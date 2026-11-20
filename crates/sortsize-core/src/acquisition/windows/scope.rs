//! One fresh, bounded directory check; never a temporal namespace cache.
//! Only identical DOS namespaces may share a volume handle. The admission
//! retains READ_ATTRIBUTES only; LIST_DIRECTORY is requested after the callback.
use super::{directory::open_relative, metadata::observe_file, strict::LocalPath, *};

pub(in crate::acquisition) struct DirectoryAdmission {
    volume: fs::File,
    target: LocalPath,
}

impl DirectoryAdmission {
    pub(super) fn open_listing(&self) -> io::Result<fs::File> {
        let rights = FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY;
        if self.target.relative.is_empty() {
            // A drive root has no intermediate components. The subsequently
            // observed handle must still match the prepared root's identity.
            metadata_open(&self.target.volume, rights)
        } else {
            // Complete path from the freshly opened volume, not from a cached
            // parent. Reject intermediate reparses introduced by the callback.
            open_relative(&self.volume, &self.target.relative, rights)
        }
    }
}

/// None selects the historical guard before any native operation. Once the
/// optimized check starts, access, identity and I/O errors must propagate.
pub(in crate::acquisition) fn prepare_directory_scope(
    guard: &Guard,
    path: &Path,
) -> Option<io::Result<(Stamp, DirectoryAdmission)>> {
    let root = LocalPath::parse(guard.root())?;
    let target = LocalPath::parse(path)?;
    if root.drive != target.drive {
        return None;
    }
    let state = match guard.internal_state() {
        Some(state) => {
            if !state.ntfs_directory
                || state.boundary.token != guard.boundary.token
                || state.boundary.class != guard.boundary.class
            {
                return None;
            }
            let local = LocalPath::parse(state.root())?;
            if local.drive != root.drive {
                return None;
            }
            Some((state, local))
        }
        None => None,
    };
    Some((|| {
        let table = boundaries(guard.root())?;
        authorize(&table, guard.root(), path, &guard.boundary, guard.policy)?;
        if let Some((state, _)) = &state
            && (boundary(&table, guard.root())?.token != state.boundary.token
                || boundary(&table, guard.root())?.class != state.boundary.class)
        {
            return Err(denied());
        }
        let (volume, root_stamp) = profile::measure(profile::Stage::Root, || {
            let volume = metadata_open(&root.volume, FILE_READ_ATTRIBUTES)?;
            let volume_stamp = observe_file(&volume, false, false, false, None)?;
            if !volume_stamp.directory {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            let stamp = if root.relative.is_empty() {
                volume_stamp
            } else {
                let file = open_relative(&volume, &root.relative, FILE_READ_ATTRIBUTES)?;
                observe_file(&file, false, false, false, None)?
            };
            validate_root_attachment(&guard.root_stamp, &stamp)?;
            Ok((volume, stamp))
        })?;
        if let Some((state, local)) = &state {
            profile::measure(profile::Stage::Workspace, || {
                // with_internal_state excludes an ancestor of the source, so
                // an admitted internal directory cannot be this volume root.
                let file = open_relative(&volume, &local.relative, FILE_READ_ATTRIBUTES)?;
                let current = observe_file(&file, false, false, false, None)?;
                if state.root_stamp.attachment(&current) != Attachment::Compatible {
                    return Err(denied());
                }
                Ok(())
            })?;
        }
        let stamp = if root.relative == target.relative {
            root_stamp
        } else {
            profile::measure(profile::Stage::Target, || {
                let file = open_relative(&volume, &target.relative, FILE_READ_ATTRIBUTES)?;
                observe_file(&file, false, false, false, None)
            })?
        };
        let after = boundaries(guard.root())?;
        authorize(&after, guard.root(), path, &guard.boundary, guard.policy)?;
        if let Some((state, _)) = &state
            && (boundary(&after, guard.root())?.token != state.boundary.token
                || boundary(&after, guard.root())?.class != state.boundary.class)
        {
            return Err(denied());
        }
        Ok((stamp, DirectoryAdmission { volume, target }))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_drive_verbatim_workspace_is_shared_but_distinct_tokens_and_aliases_are_not() {
        let fixture = tempfile::tempdir().unwrap();
        if !is_ntfs(fixture.path()) {
            return;
        }
        let child = fixture.path().join("child");
        let state = fixture.path().join("state");
        fs::create_dir(&child).unwrap();
        fs::create_dir(&state).unwrap();
        let state = state.canonicalize().unwrap();
        let mut guard = Guard::open(fixture.path(), Policy::default())
            .unwrap()
            .with_internal_state(Guard::open(&state, Policy::default()).unwrap())
            .unwrap()
            .with_scan_observation(ScanObservation::DirectoryListing);
        guard.ntfs_directory = true;
        // Both spellings query the same DOS drive registration. Components
        // have already passed the ambiguity filter and retain their UTF-16.
        assert_ne!(
            LocalPath::parse(guard.root()).unwrap().volume,
            LocalPath::parse(&state).unwrap().volume
        );
        assert!(prepare_directory_scope(&guard, &child).unwrap().is_ok());
        assert!(guard.scan_directory(&child).is_ok());
        assert!(prepare_directory_scope(&guard, &fixture.path().join("ambiguous.")).is_none());
        let state = guard.internal.as_mut().unwrap();
        state.boundary.token.push(0);
        assert!(prepare_directory_scope(&guard, &child).is_none());
        guard.internal.as_mut().unwrap().boundary.token.pop();
        // An alternate letter is not admitted merely because a captured token
        // and file ID happen to match. This refusal occurs before native I/O.
        let other_drive = if LocalPath::parse(guard.root()).unwrap().drive == b'Z' {
            "Y:\\state"
        } else {
            "Z:\\state"
        };
        guard.internal.as_mut().unwrap().root = other_drive.into();
        assert!(prepare_directory_scope(&guard, &child).is_none());
    }
}
