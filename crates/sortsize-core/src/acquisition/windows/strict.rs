//! Fresh local-volume opens with a native no-reparse walk of the complete path.
//!
//! Do not retain a volume handle across inspections: reopening it and retaining
//! the caller's DOS-device checks also detects a replaced mapping.
//! OBJ_DONT_REPARSE rejects reparses in intermediate components; the final
//! component is additionally checked by observe_file on the opened handle.
use super::{
    directory::open_relative,
    metadata::{observe_file, observe_with},
    metadata_open, *,
};

pub(super) struct LocalPath {
    pub(super) drive: u8,
    pub(super) volume: PathBuf,
    pub(super) relative: Vec<u16>,
}

impl LocalPath {
    pub(super) fn parse(path: &Path) -> Option<Self> {
        let mut components = path.components();
        let Component::Prefix(prefix) = components.next()? else {
            return None;
        };
        let drive = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive.to_ascii_uppercase(),
            _ => return None,
        };
        if components.next()? != Component::RootDir {
            return None;
        }
        let mut volume = PathBuf::from(prefix.as_os_str());
        volume.push("\\");
        let mut relative = Vec::new();
        for component in components {
            let Component::Normal(name) = component else {
                return None;
            };
            let name: Vec<u16> = name.encode_wide().collect();
            // Preserve historical Win32 interpretation for ambiguous names.
            if name.is_empty()
                || matches!(name.last(), Some(32 | 46))
                || name.iter().any(|c| matches!(c, 0 | 47 | 58 | 92))
                || reserved_dos_name(&name)
            {
                return None;
            }
            if !relative.is_empty() {
                relative.push(92);
            }
            relative.extend(name);
        }
        if relative.len() > usize::from(u16::MAX) / size_of::<u16>() {
            return None;
        }
        Some(Self {
            drive,
            volume,
            relative,
        })
    }
}

fn reserved_dos_name(name: &[u16]) -> bool {
    let stem: Vec<u16> = name
        .iter()
        .take_while(|&&c| c != 46)
        .map(|&c| if (97..=122).contains(&c) { c - 32 } else { c })
        .collect();
    matches!(
        stem.as_slice(),
        [67, 79, 78] | [80, 82, 78] | [65, 85, 88] | [78, 85, 76]
    ) || matches!(stem.as_slice(), [67, 79, 77, n] | [76, 80, 84, n]
            if (49..=57).contains(n) || matches!(n, 0xb9 | 0xb2 | 0xb3))
}

/// None means a pathname needs the existing portable/path-based implementation.
/// An attempted native open's error is never retried through a weaker traversal.
pub(in crate::acquisition) fn inspect_local(
    root: &Path,
    path: &Path,
    expected_root: &Stamp,
    detailed: bool,
    allocation: bool,
    owners: bool,
) -> Option<io::Result<(Stamp, Stamp)>> {
    let root = LocalPath::parse(root)?;
    let target = LocalPath::parse(path)?;
    if root.volume != target.volume {
        return None;
    }
    Some((|| {
        let (volume, root_stamp) = profile::measure(profile::Stage::Root, || {
            let volume = metadata_open(&root.volume, FILE_READ_ATTRIBUTES)?;
            let volume_stamp = observe_file(&volume, false, false, false, None)?;
            if !volume_stamp.directory {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            let root_stamp = if root.relative.is_empty() {
                volume_stamp
            } else {
                let file = open_relative(&volume, &root.relative, FILE_READ_ATTRIBUTES)?;
                observe_file(&file, false, false, false, None)?
            };
            validate_root_attachment(expected_root, &root_stamp)?;
            Ok((volume, root_stamp))
        })?;
        // Directory validation asks for the root frequently. Reuse this exact
        // fresh observation instead of reopening the same path a second time.
        if root.relative == target.relative && !detailed {
            return Ok((root_stamp.clone(), root_stamp));
        }
        let stamp = profile::measure(profile::Stage::Target, || {
            if target.relative.is_empty() {
                observe_with(detailed, allocation, owners, |rights| {
                    metadata_open(&target.volume, rights)
                })
            } else {
                observe_with(detailed, allocation, owners, |rights| {
                    open_relative(&volume, &target.relative, rights)
                })
            }
        })?;
        Ok((root_stamp, stamp))
    })())
}

#[cfg(test)]
mod tests;
