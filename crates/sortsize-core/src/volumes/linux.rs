use super::*;
use std::{
    collections::HashMap,
    ffi::{CString, OsStr},
    os::unix::ffi::{OsStrExt, OsStringExt},
};

pub struct NativeVolumes;
impl VolumeProvider for NativeVolumes {
    fn enumerate(&self) -> io::Result<Vec<Volume>> {
        // Kernel mount table only; never used by the file scanner.
        use std::io::Read;
        const MAX_TABLE: u64 = 16 * 1024 * 1024;
        let mut data = Vec::new();
        std::fs::File::open("/proc/self/mountinfo")?
            .take(MAX_TABLE + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > MAX_TABLE {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut volumes = parse_mountinfo(&data)?;
        // Filter file bind mounts only on known local filesystems. Never stat
        // a remote/unknown mount implicitly. Keep denied/disappeared locations.
        for v in &mut volumes {
            if !v.kind.may_contact_network() {
                v.mounts
                    .retain(|m| !matches!(std::fs::metadata(&m.path), Ok(meta) if !meta.is_dir()));
            }
        }
        volumes.retain(|v| !v.mounts.is_empty());
        Ok(volumes)
    }
    fn probe(&self, path: &Path) -> VolumeProbe {
        let result = (|| {
            // File bind mounts are excluded during local enumeration too.
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // SAFETY: NUL-terminated native bytes, writable statvfs storage.
            if unsafe { libc::statvfs(path.as_ptr(), value.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: success initialized all fields.
            let value = unsafe { value.assume_init() };
            Ok(VolumeProbe {
                capacity: blocks(
                    value.f_blocks,
                    value.f_bfree,
                    value.f_bavail,
                    value.f_frsize,
                ),
                availability: VolumeAvailability::Available,
                label: MetadataValue::Unknown,
                filesystem: MetadataValue::Unknown,
            })
        })();
        result.unwrap_or_else(|e| VolumeProbe::error(&e))
    }
}
fn blocks(total: u64, free: u64, available: u64, unit: u64) -> VolumeCapacity {
    if unit == 0 {
        return VolumeCapacity::absent(MetadataValue::Unknown);
    }
    let bytes = |n: u64| {
        n.checked_mul(unit)
            .map(known)
            .unwrap_or(MetadataValue::Unknown)
    };
    VolumeCapacity::from_bytes(bytes(total), bytes(free), bytes(available))
}
fn unescape(input: &[u8]) -> io::Result<OsString> {
    let mut result = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'\\' {
            let digits = input.get(i + 1..i + 4).ok_or(io::ErrorKind::InvalidData)?;
            if !digits.iter().all(|c| (b'0'..=b'7').contains(c)) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let n = (digits[0] - b'0') as u16 * 64
                + (digits[1] - b'0') as u16 * 8
                + (digits[2] - b'0') as u16;
            if n == 0 || n > 255 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            result.push(n as u8);
            i += 4;
        } else {
            result.push(input[i]);
            i += 1;
        }
    }
    Ok(OsString::from_vec(result))
}
fn classify(fs: &[u8]) -> SourceKind {
    match fs {
        b"nfs" | b"nfs4" | b"cifs" | b"smb3" | b"smbfs" | b"9p" | b"ceph" | b"afs"
        | b"fuse.sshfs" | b"fuse.rclone" | b"fuse.glusterfs" | b"fuse.davfs" => SourceKind::Remote,
        b"ext2" | b"ext3" | b"ext4" | b"xfs" | b"btrfs" | b"f2fs" | b"vfat" | b"exfat"
        | b"ntfs" | b"ntfs3" | b"fuseblk" | b"zfs" | b"jfs" | b"reiserfs" => SourceKind::Local,
        b"iso9660" | b"udf" => SourceKind::Optical,
        _ => SourceKind::Unknown,
    }
}
fn virtual_fs(fs: &[u8]) -> bool {
    matches!(
        fs,
        b"proc"
            | b"sysfs"
            | b"devtmpfs"
            | b"devpts"
            | b"tmpfs"
            | b"cgroup"
            | b"cgroup2"
            | b"securityfs"
            | b"debugfs"
            | b"tracefs"
            | b"configfs"
            | b"pstore"
            | b"efivarfs"
            | b"hugetlbfs"
            | b"mqueue"
            | b"bpf"
            | b"fusectl"
            | b"autofs"
            | b"rpc_pipefs"
            | b"binfmt_misc"
            | b"nsfs"
            | b"squashfs"
            | b"overlay"
            | b"fuse.portal"
    )
}
fn number<T: std::str::FromStr>(bytes: &[u8]) -> io::Result<T> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(io::ErrorKind::InvalidData.into())
}
fn parse_mountinfo(data: &[u8]) -> io::Result<Vec<Volume>> {
    let mut volumes: Vec<Volume> = Vec::new();
    let mut indices = HashMap::new();
    let mut paths: HashMap<PathBuf, (usize, u64)> = HashMap::new();
    // First validate every record; don't silently claim complete enumeration
    // after corrupt/truncated kernel input. No path UTF-8 conversion.
    for line in data.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let fields: Vec<_> = line.split(|b| *b == b' ').collect();
        let sep = fields
            .iter()
            .position(|f| *f == b"-")
            .ok_or(io::ErrorKind::InvalidData)?;
        if sep < 6 || fields.len() < sep + 4 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let id = number::<u64>(fields[0])?;
        let dev: Vec<_> = fields[2].split(|b| *b == b':').collect();
        if dev.len() != 2 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let major = number::<u32>(dev[0])?;
        let minor = number::<u32>(dev[1])?;
        let path = PathBuf::from(unescape(fields[4])?);
        let root = PathBuf::from(unescape(fields[3])?);
        let fs = fields[sep + 1];
        if !path.is_absolute() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if path != Path::new("/")
            && (virtual_fs(fs)
                || path.starts_with("/proc")
                || path.starts_with("/sys")
                || path.starts_with("/dev"))
        {
            // A hidden pseudo mount can cover an earlier useful mount too.
            if let Some((old, mount)) = paths.remove(&path) {
                volumes[old].mounts.retain(|m| m.mount_id != Some(mount));
            }
            continue;
        }
        if !root.is_absolute() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let source = unescape(fields[sep + 2])?;
        let identity = VolumeIdentity::Linux {
            major,
            minor,
            filesystem: OsStr::from_bytes(fs).to_owned(),
            source: source.clone(),
        };
        let index = *indices.entry(identity.clone()).or_insert_with(|| {
            let i = volumes.len();
            volumes.push(Volume {
                identity,
                label: known(source),
                filesystem: known(OsStr::from_bytes(fs).to_owned()),
                kind: classify(fs),
                mounts: Vec::new(),
                capacity: VolumeCapacity::absent(MetadataValue::NotCollected),
                availability: VolumeAvailability::NotProbed,
            });
            i
        });
        // Last mount at a pathname hides earlier records; no capacity sum.
        if let Some((old, mount)) = paths.insert(path.clone(), (index, id)) {
            volumes[old].mounts.retain(|m| m.mount_id != Some(mount));
        }
        volumes[index].mounts.push(VolumeMount {
            path,
            mount_id: Some(id),
            root: Some(root),
        });
    }
    volumes.retain(|v| !v.mounts.is_empty());
    for v in &mut volumes {
        // Prefer a filesystem root over a bind subtree; deterministic native order.
        v.mounts.sort_by(|a, b| {
            (
                a.root.as_deref() != Some(Path::new("/")),
                a.path.components().count(),
                &a.path,
            )
                .cmp(&(
                    b.root.as_deref() != Some(Path::new("/")),
                    b.path.components().count(),
                    &b.path,
                ))
        });
    }
    volumes.sort_by(|a, b| a.mounts[0].path.cmp(&b.mounts[0].path));
    Ok(volumes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mountinfo_native_bytes_escapes_aliases_and_virtuals() {
        let data = b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 8:1 /dir /mnt/bind\\040space rw shared:1 - ext4 /dev/a rw\n3 1 8:2 / /mnt/a\\134b\\011c\\012d\xff rw - xfs /dev/b rw\n4 1 0:1 / /proc rw - proc proc rw\n5 1 0:2 / /run rw - tmpfs tmpfs rw\n6 1 0:3 / /mnt/net rw - cifs //host/share rw\n7 1 0:4 / /mnt/fuse rw - fuse.custom custom rw\n";
        let v = parse_mountinfo(data).unwrap();
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].mounts.len(), 2);
        assert_eq!(v[0].mounts[1].path, Path::new("/mnt/bind space"));
        assert_eq!(
            v[1].mounts[0].path.as_os_str().as_bytes(),
            b"/mnt/a\\b\tc\nd\xff"
        );
        assert_eq!(v[2].kind, SourceKind::Unknown);
        assert_eq!(v[3].kind, SourceKind::Remote);
        assert_eq!(v[0].kind, SourceKind::Local);
    }
    #[test]
    fn empty_malformed_and_overmounted_paths() {
        assert!(parse_mountinfo(b"").unwrap().is_empty());
        for data in [
            b"broken\n".as_slice(),
            b"1 0 8:1 / /bad\\999 rw - ext4 /dev/a rw\n",
            b"1 0 8:1 / /bad\\000 rw - ext4 /dev/a rw\n",
        ] {
            assert!(parse_mountinfo(data).is_err());
        }
        let v = parse_mountinfo(
            b"1 0 8:1 / /mnt rw - ext4 /dev/a rw\n2 0 8:2 / /mnt rw - xfs /dev/b rw\n3 0 0:5 mnt:[4026533379] /run/snapd/ns/test.mnt rw - nsfs nsfs rw\n",
        )
        .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].mounts[0].mount_id, Some(2));
    }
    #[test]
    fn statvfs_checked_units_and_reservations() {
        let c = blocks(100, 20, 10, 4096);
        assert_eq!(c.used, known(80 * 4096));
        assert_eq!(c.available, known(10 * 4096));
        assert_eq!(blocks(u64::MAX, 0, 0, 2).total, MetadataValue::Unknown);
        assert_eq!(blocks(1, 0, 0, 0).total, MetadataValue::Unknown);
        assert_eq!(blocks(1, 2, 0, 1).used, MetadataValue::Unknown);
    }
}
