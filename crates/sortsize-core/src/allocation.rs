//! Collecte native optionnelle; une taille allouée n'est PAS un gain récupérable.
use crate::MetadataValue;
use std::path::Path;

/// Best-effort identity within a scan, never a persistent global identifier.
/// Equal identities recognize hardlinks; distinct identities may still share
/// extents (reflinks, snapshots, deduplication). No reclaimable space is implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LocalFileId {
    Unix {
        device: u64,
        inode: u64,
    },
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
    },
}

/// Native regular-file properties observed together. Not a physical-space total.
/// Directories, symlinks and special entries use NotCollected for all fields.
/// Known zero is a real observation. Missing properties never become zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryAllocation {
    pub allocated_bytes: MetadataValue<u64>,
    pub identity: MetadataValue<LocalFileId>,
    pub link_count: MetadataValue<u64>,
}

impl EntryAllocation {
    pub(crate) const fn not_collected() -> Self {
        Self {
            allocated_bytes: MetadataValue::NotCollected,
            identity: MetadataValue::NotCollected,
            link_count: MetadataValue::NotCollected,
        }
    }
}

/// On Windows use the same handle for length/dates/allocation/identity so a
/// same-length replacement between path metadata and open cannot mix objects.
pub(crate) fn collect_file(
    path: &Path,
    metadata: std::fs::Metadata,
) -> (std::fs::Metadata, EntryAllocation) {
    #[cfg(windows)]
    {
        let (metadata, allocation, _) = platform::collect_node(path, metadata, true, false);
        (metadata, allocation)
    }
    #[cfg(not(windows))]
    platform::collect(path, metadata)
}

#[cfg(any(target_os = "linux", windows, test))]
fn known<T>(value: T) -> MetadataValue<T> {
    MetadataValue::Known {
        value,
        accuracy: crate::ValueAccuracy::Exact,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    pub(super) fn collect(
        _path: &Path,
        metadata: std::fs::Metadata,
    ) -> (std::fs::Metadata, EntryAllocation) {
        let allocation = EntryAllocation {
            allocated_bytes: from_blocks(metadata.blocks()),
            identity: known(LocalFileId::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
            link_count: known(metadata.nlink()),
        };
        (metadata, allocation)
    }
}

#[cfg(any(target_os = "linux", test))]
fn from_blocks(blocks: u64) -> MetadataValue<u64> {
    match blocks.checked_mul(512) {
        Some(bytes) => known(bytes),
        None => MetadataValue::Unknown,
    }
}

#[cfg(windows)]
pub(crate) mod platform {
    use super::*;
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    use std::{
        io,
        mem::{MaybeUninit, size_of},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FileIdInfo, FileStandardInfo, GetFileInformationByHandleEx,
        READ_CONTROL,
    };

    pub(super) fn missing<T>(error: &io::Error) -> MetadataValue<T> {
        match error.kind() {
            io::ErrorKind::PermissionDenied => MetadataValue::AccessDenied,
            io::ErrorKind::Unsupported => MetadataValue::Unsupported,
            _ => MetadataValue::Unknown,
        }
    }

    unsafe fn query<T>(file: &std::fs::File, class: i32) -> io::Result<T> {
        let mut output = MaybeUninit::<T>::uninit();
        // SAFETY: only used with FILE_STANDARD_INFO / FILE_ID_INFO and their
        // matching FileInformationClass, writing the complete initialized result.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                output.as_mut_ptr().cast(),
                size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success means the API initialized the requested output struct.
        Ok(unsafe { output.assume_init() })
    }

    pub(crate) fn collect_node(
        path: &Path,
        metadata: std::fs::Metadata,
        allocation_requested: bool,
        owner_requested: bool,
    ) -> (
        std::fs::Metadata,
        EntryAllocation,
        Option<crate::EntryOwner>,
    ) {
        // Explicit attribute/security rights; never read data or enable a
        // privilege. OPEN_REPARSE_POINT protects the final component. Directory
        // handles require BACKUP_SEMANTICS (this does not enable backup privilege).
        let open = |access| {
            #[cfg(feature = "owner-profiling")]
            crate::owner_profile::attempt();
            std::fs::OpenOptions::new()
                .access_mode(access)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
        };
        let mut owner_failure = None;
        let handle = if owner_requested {
            match open(FILE_READ_ATTRIBUTES | READ_CONTROL) {
                Ok(file) => Ok(file),
                Err(error) => {
                    owner_failure = Some(crate::owner::windows::missing(
                        error.raw_os_error().unwrap_or(0) as u32,
                    ));
                    // Owner permission failure must not suppress previously
                    // available allocation, logical bytes or dates.
                    open(FILE_READ_ATTRIBUTES)
                }
            }
        } else {
            open(FILE_READ_ATTRIBUTES)
        };
        // Declared before file so the counter is dropped after CloseHandle.
        #[cfg(feature = "owner-profiling")]
        let _profile_handle = handle
            .as_ref()
            .ok()
            .map(|_| crate::owner_profile::Handle::opened());
        let file = match handle {
            Ok(value) => value,
            Err(error) => {
                return (
                    metadata,
                    EntryAllocation {
                        allocated_bytes: missing(&error),
                        identity: missing(&error),
                        link_count: missing(&error),
                    },
                    owner_requested.then(|| {
                        owner_failure.unwrap_or_else(|| {
                            crate::owner::windows::missing(error.raw_os_error().unwrap_or(0) as u32)
                        })
                    }),
                );
            }
        };
        let current = match file.metadata() {
            Ok(current) => current,
            Err(error) => {
                return (
                    metadata,
                    EntryAllocation {
                        allocated_bytes: missing(&error),
                        identity: missing(&error),
                        link_count: missing(&error),
                    },
                    owner_requested.then_some(crate::EntryOwner {
                        id: MetadataValue::Unknown,
                    }),
                );
            }
        };
        if current.is_file() != metadata.is_file()
            || current.is_dir() != metadata.is_dir()
            || current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return (
                metadata,
                EntryAllocation {
                    allocated_bytes: MetadataValue::Unknown,
                    identity: MetadataValue::Unknown,
                    link_count: MetadataValue::Unknown,
                },
                owner_requested.then_some(crate::EntryOwner {
                    id: MetadataValue::Unknown,
                }),
            );
        }
        let owner = owner_requested
            .then(|| owner_failure.unwrap_or_else(|| crate::owner::windows::collect(&file)));
        if !allocation_requested {
            return (current, EntryAllocation::not_collected(), owner);
        }
        // SAFETY: FILE_STANDARD_INFO matches FileStandardInfo.
        let (allocated_bytes, link_count) =
            match unsafe { query::<FILE_STANDARD_INFO>(&file, FileStandardInfo) } {
                Ok(info)
                    if !info.Directory
                        && u64::try_from(info.EndOfFile) == Ok(current.len())
                        && info.NumberOfLinks > 0 =>
                {
                    (
                        match u64::try_from(info.AllocationSize) {
                            Ok(bytes) => known(bytes),
                            Err(_) => MetadataValue::Unknown,
                        },
                        known(u64::from(info.NumberOfLinks)),
                    )
                }
                Ok(_) => (MetadataValue::Unknown, MetadataValue::Unknown),
                Err(error) => (missing(&error), missing(&error)),
            };
        // SAFETY: FILE_ID_INFO matches FileIdInfo.
        let identity = match unsafe { query::<FILE_ID_INFO>(&file, FileIdInfo) } {
            Ok(info) => known(LocalFileId::Windows {
                volume_serial: info.VolumeSerialNumber,
                file_id: info.FileId.Identifier,
            }),
            Err(error) => missing(&error),
        };
        (
            current,
            EntryAllocation {
                allocated_bytes,
                identity,
                link_count,
            },
            owner,
        )
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;
    pub(super) fn collect(
        _path: &Path,
        metadata: std::fs::Metadata,
    ) -> (std::fs::Metadata, EntryAllocation) {
        (
            metadata,
            EntryAllocation {
                allocated_bytes: MetadataValue::Unsupported,
                identity: MetadataValue::Unsupported,
                link_count: MetadataValue::Unsupported,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn block_conversion_preserves_zero_and_rejects_overflow() {
        assert_eq!(from_blocks(0), known(0));
        assert_eq!(from_blocks(8), known(4096));
        assert_eq!(from_blocks(u64::MAX / 512), known((u64::MAX / 512) * 512));
        assert_eq!(from_blocks(u64::MAX / 512 + 1), MetadataValue::Unknown);
    }

    #[cfg(windows)]
    #[test]
    fn windows_failure_reasons_are_not_zero() {
        use std::io::{Error, ErrorKind};
        for (kind, expected) in [
            (ErrorKind::PermissionDenied, MetadataValue::AccessDenied),
            (ErrorKind::Unsupported, MetadataValue::Unsupported),
            (ErrorKind::NotFound, MetadataValue::Unknown),
        ] {
            assert_eq!(platform::missing::<u64>(&Error::from(kind)), expected);
        }
        let fixture = tempfile::TempDir::new().unwrap();
        let path = fixture.path().join("gone");
        std::fs::write(&path, b"data").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let (_, allocation) = collect_file(&path, metadata.clone());
        assert_eq!(allocation.allocated_bytes, MetadataValue::Unknown);
        assert_eq!(allocation.identity, MetadataValue::Unknown);
        assert_eq!(allocation.link_count, MetadataValue::Unknown);
        let (_, _, owner) = platform::collect_node(&path, metadata, true, true);
        assert_eq!(owner.unwrap().id, MetadataValue::Unknown);
    }

    #[cfg(windows)]
    #[test]
    fn replacement_with_directory_is_unknown_instead_of_mixing_types() {
        let fixture = tempfile::TempDir::new().unwrap();
        let path = fixture.path().join("replaced");
        std::fs::write(&path, []).unwrap();
        let stale = std::fs::symlink_metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let (metadata, allocation, owner) = platform::collect_node(&path, stale, true, true);
        assert!(metadata.is_file());
        assert_eq!(allocation.allocated_bytes, MetadataValue::Unknown);
        assert_eq!(owner.unwrap().id, MetadataValue::Unknown);
    }

    #[cfg(windows)]
    #[test]
    fn same_length_replacement_uses_one_handle_for_all_properties() {
        use std::fs::{self, File, FileTimes};
        use std::time::{Duration, UNIX_EPOCH};
        let fixture = tempfile::TempDir::new().unwrap();
        let path = fixture.path().join("replaced");
        fs::write(&path, b"old!").unwrap();
        let stale = fs::symlink_metadata(&path).unwrap();
        fs::rename(&path, fixture.path().join("original")).unwrap();
        fs::write(&path, b"new!").unwrap();
        let time = UNIX_EPOCH + Duration::from_secs(123456789);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap();
        let (current, allocation) = collect_file(&path, stale);
        assert_eq!(current.modified().unwrap(), time);
        let (_, original) = collect_file(
            &fixture.path().join("original"),
            fs::symlink_metadata(fixture.path().join("original")).unwrap(),
        );
        assert_ne!(allocation.identity, original.identity);
        assert!(matches!(allocation.identity, MetadataValue::Known { .. }));
        let (current, combined, owner) = platform::collect_node(&path, current, true, true);
        assert_eq!(current.modified().unwrap(), time);
        assert_eq!(combined.identity, allocation.identity);
        assert!(matches!(
            owner.unwrap().id,
            MetadataValue::Known {
                value: crate::NativeOwnerId::WindowsSid(_),
                ..
            }
        ));
    }

    #[test]
    fn absent_is_not_zero() {
        let missing = EntryAllocation::not_collected();
        assert_eq!(missing.allocated_bytes, MetadataValue::NotCollected);
        assert_eq!(missing.identity, MetadataValue::NotCollected);
        assert_eq!(missing.link_count, MetadataValue::NotCollected);
        assert!(!std::mem::needs_drop::<EntryAllocation>());
    }
}
