use super::*;
use std::{
    collections::{HashMap, hash_map::Entry},
    ffi::OsString,
    io::Read,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
};
mod directory;
pub(super) use directory::Directory;

fn decode(bytes: &[u8]) -> io::Result<PathBuf> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let d = bytes.get(i + 1..i + 4).ok_or(io::ErrorKind::InvalidData)?;
            if !d.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let n =
                u16::from(d[0] - b'0') * 64 + u16::from(d[1] - b'0') * 8 + u16::from(d[2] - b'0');
            if n == 0 || n > 255 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            result.push(n as u8);
            i += 4;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    Ok(OsString::from_vec(result).into())
}
pub(super) fn parse(data: &[u8]) -> io::Result<Vec<Boundary>> {
    // Each read is fresh. Only decoding/deduplication of that single snapshot
    // is shared: no mount evidence survives into the next authorization.
    let mut mounts = HashMap::new();
    for (index, line) in data
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let mut fields = line.split(|b| *b == b' ');
        let mut fixed = [&b""[..]; 6];
        for field in &mut fixed {
            *field = fields.next().ok_or(io::ErrorKind::InvalidData)?;
            if *field == b"-" {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        if !fields.any(|f| f == b"-") {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let filesystem = fields.next().ok_or(io::ErrorKind::InvalidData)?;
        // Preserve the old grammar, including required source and super options.
        fields.next().ok_or(io::ErrorKind::InvalidData)?;
        fields.next().ok_or(io::ErrorKind::InvalidData)?;
        let path = decode(fixed[4])?;
        lexical(&path)?;
        let class = match filesystem {
            b"ext2" | b"ext3" | b"ext4" | b"xfs" | b"btrfs" | b"f2fs" | b"vfat" | b"exfat"
            | b"ntfs" | b"ntfs3" | b"zfs" | b"tmpfs" => SourceClass::Local,
            b"nfs" | b"nfs4" | b"cifs" | b"smb3" | b"9p" | b"fuse.sshfs" | b"fuse.gvfsd-fuse" => {
                SourceClass::Remote
            }
            _ => SourceClass::Unknown,
        };
        // Last occurrence wins, in last-occurrence order, exactly as retain +
        // push did. Path keys own their native bytes; no lossy normalization.
        // Keep every token field, including mount ID, parent and options.
        let value = (index, decode(fixed[3])?, line.to_vec(), class);
        match mounts.entry(path) {
            Entry::Vacant(entry) => {
                entry.insert(value);
            }
            Entry::Occupied(entry) => {
                // Path equality normalizes redundant separators. HashMap's
                // insert alone would keep the first key's native spelling.
                entry.remove_entry();
                mounts.insert(decode(fixed[4])?, value);
            }
        }
    }
    if mounts.is_empty() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut out: Vec<_> = mounts.into_iter().collect();
    out.sort_unstable_by_key(|(_, (index, ..))| *index);
    Ok(out
        .into_iter()
        .map(|(path, (_, root, token, class))| Boundary {
            path,
            root,
            token,
            class,
        })
        .collect())
}

/// GVfs represents all desktop shares with one kernel mount. Explicit consent
/// to its global root must not grant access to all its sibling SMB shares.
/// Validate the first provider component before any native metadata operation.
fn gvfs_scope(table: &[Boundary], path: &Path) -> io::Result<()> {
    let Some(mount) = table
        .iter()
        .filter(|m| path.starts_with(&m.path))
        .max_by_key(|m| m.path.components().count())
    else {
        return Ok(());
    };
    let fields: Vec<_> = mount.token.split(|b| *b == b' ').collect();
    let is_gvfs = fields
        .iter()
        .position(|f| *f == b"-")
        .and_then(|i| fields.get(i + 1))
        .is_some_and(|fs| *fs == b"fuse.gvfsd-fuse");
    if !is_gvfs {
        return Ok(());
    }
    let first = path
        .strip_prefix(&mount.path)
        .map_err(|_| denied())?
        .components()
        .next();
    match first {
        Some(Component::Normal(name)) if crate::volumes::smb::gvfs_target(name).is_some() => Ok(()),
        _ => Err(denied()),
    }
}
pub(super) fn boundaries(path: &Path) -> io::Result<Vec<Boundary>> {
    let mut data = Vec::new();
    fs::File::open("/proc/self/mountinfo")?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    if data.len() > 16 * 1024 * 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let table = parse(&data)?;
    gvfs_scope(&table, path)?;
    Ok(table)
}
pub(super) fn stamp(_: &Path, m: &fs::Metadata) -> io::Result<Stamp> {
    Ok(Stamp {
        properties: NodeProperties::default(),
        identity: Some(LocalFileId::Unix {
            device: m.dev(),
            inode: m.ino(),
        }),
        created: m.created().ok(),
        modified: m.modified().ok(),
        length: m.len(),
        directory: m.is_dir(),
        regular: m.is_file(),
    })
}

pub(super) fn observe(
    path: &Path,
    m: &fs::Metadata,
    detailed: bool,
    allocation: bool,
    owners: bool,
) -> io::Result<Stamp> {
    let mut s = stamp(path, m)?;
    if detailed {
        use crate::{MetadataValue, ValueAccuracy};
        s.properties.allocated = if allocation && m.is_file() {
            m.blocks()
                .checked_mul(512)
                .map_or(MetadataValue::Unknown, |value| MetadataValue::Known {
                    value,
                    accuracy: ValueAccuracy::Exact,
                })
        } else {
            MetadataValue::NotCollected
        };
        s.properties.accessed =
            m.accessed()
                .map_or(MetadataValue::Unknown, |value| MetadataValue::Known {
                    value,
                    accuracy: ValueAccuracy::Exact,
                });
        if owners {
            s.properties.owner = crate::EntryOwner::collect(m);
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_gvfs_root_cannot_authorize_sibling_shares_even_explicitly() {
        let table = parse(b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 0:9 / /run/user/1000/gvfs rw - fuse.gvfsd-fuse gvfsd-fuse rw\n").unwrap();
        let global = Path::new("/run/user/1000/gvfs");
        let alpha = global.join("smb-share:server=nas,share=alpha");
        // The generic mount policy alone cannot distinguish sibling shares.
        assert!(
            authorize(
                &table,
                global,
                global,
                &table[1],
                Policy {
                    same_volume: true,
                    explicit_root: true
                }
            )
            .is_ok()
        );
        // Native preflight rejects this root before a stat/read_dir is possible.
        assert_eq!(
            gvfs_scope(&table, global).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(gvfs_scope(&table, &global.join("smb-server:server=nas")).is_err());
        assert!(gvfs_scope(&table, &global.join("smb-share:server=nas,share=..")).is_err());
        assert!(gvfs_scope(&table, &alpha).is_ok());
        assert!(gvfs_scope(&table, &alpha.join("subdirectory")).is_ok());
        // A grant rooted at alpha cannot reach the sibling beta.
        assert!(
            authorize(
                &table,
                &alpha,
                &global.join("smb-share:server=nas,share=beta"),
                &table[1],
                Policy {
                    same_volume: true,
                    explicit_root: true
                }
            )
            .is_err()
        );
        // Existing ordinary local/native mounts are not widened or blocked.
        assert!(gvfs_scope(&table, Path::new("/tmp")).is_ok());
    }
    #[test]
    fn desktop_smb_mount_never_inherits_a_local_automatic_grant() {
        let table = parse(b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 0:9 / /run/user/1000/gvfs rw - fuse.gvfsd-fuse gvfsd-fuse rw\n").unwrap();
        let root = Path::new("/run/user/1000/gvfs/smb-share:server=nas,share=data");
        assert_eq!(table[1].class, SourceClass::Remote);
        assert!(
            authorize(
                &table,
                Path::new("/"),
                root,
                &table[0],
                Policy {
                    same_volume: false,
                    explicit_root: false
                }
            )
            .is_err()
        );
        assert!(authorize(&table, root, root, &table[1], Policy::default()).is_err());
        assert!(
            authorize(
                &table,
                root,
                root,
                &table[1],
                Policy {
                    same_volume: true,
                    explicit_root: true
                }
            )
            .is_ok()
        );
    }
    #[test]
    fn equal_mount_paths_keep_the_last_native_representation() {
        for (first, last) in [("/a//b", "/a/b"), ("/a/b", "/a//b"), ("/", "//")] {
            let input = format!(
                "1 0 8:1 / {first} rw - ext4 /dev/a rw\n2 0 8:1 /new {last} rw - ext4 /dev/a ro\n"
            );
            let table = parse(input.as_bytes()).unwrap();
            assert_eq!(table.len(), 1);
            // Path equality normalizes separators, so check the actual native bytes.
            assert_eq!(
                table[0].path.as_os_str().as_encoded_bytes(),
                last.as_bytes()
            );
            assert_eq!(table[0].root, Path::new("/new"));
            assert!(table[0].token.ends_with(b"ext4 /dev/a ro"));
        }
    }
}
