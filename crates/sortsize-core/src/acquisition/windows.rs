use super::*;
use std::{
    mem::{MaybeUninit, size_of},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Prefix,
};
use windows_sys::Win32::Storage::FileSystem::*;
mod directory;
mod metadata;
pub(super) use directory::{Directory, Entry, is_ntfs};
mod strict;
pub(super) use strict::inspect_local;
mod scope;
pub(super) use scope::{DirectoryAdmission, prepare_directory_scope};

pub(super) fn boundaries(root: &Path) -> io::Result<Vec<Boundary>> {
    profile::measure(profile::Stage::Namespace, || boundaries_impl(root))
}

fn boundaries_impl(root: &Path) -> io::Result<Vec<Boundary>> {
    let Some(Component::Prefix(prefix)) = root.components().next() else {
        return Err(denied());
    };
    let drive = match prefix.kind() {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d,
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            // Parse the explicitly supplied namespace only. No share discovery,
            // authentication or network request occurs before Guard authorizes it.
            let mut token = b"windows-unc\0".to_vec();
            for component in [server, share] {
                token.extend(component.encode_wide().flat_map(u16::to_le_bytes));
                token.extend([0, 0]);
            }
            let mut path = PathBuf::from(prefix.as_os_str());
            path.push("\\");
            return Ok(vec![Boundary {
                root: path.clone(),
                path,
                token,
                class: SourceClass::Remote,
            }]);
        }
        _ => return Err(io::ErrorKind::Unsupported.into()),
    };
    let name = [u16::from(drive), b':' as u16, 0];
    let mut target = vec![0u16; 256];
    loop {
        // Local DOS device namespace only. No GetDriveType/capacity/UNC probing.
        // Most mappings are short. Grow only when necessary, preserving the
        // complete MULTI_SZ token and the historical maximum buffer size.
        let n = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
        if n != 0 {
            target.truncate(n as usize);
            break;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error()
            != Some(windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER as i32)
            || target.len() == 32768
        {
            return Err(error);
        }
        target.resize(target.len() * 2, 0);
    }
    let class = device_class(&target);
    let mut path = PathBuf::from(prefix.as_os_str());
    path.push("\\");
    Ok(vec![Boundary {
        root: path.clone(),
        path,
        token: target.iter().flat_map(|v| v.to_le_bytes()).collect(),
        class,
    }])
}
// QueryDosDevice reports local namespace registrations, including disconnected
// drive mappings. Preserve its full target in the boundary token: a reassigned
// letter is not the same source, and UNC aliases are not assumed equivalent.
fn device_class(target: &[u16]) -> SourceClass {
    let prefix = |name: &str| {
        target.starts_with(&std::ffi::OsStr::new(name).encode_wide().collect::<Vec<_>>())
    };
    if prefix("\\Device\\HarddiskVolume") {
        SourceClass::Local
    } else if prefix("\\Device\\Mup\\")
        || prefix("\\Device\\LanmanRedirector\\")
        || prefix("\\Device\\NfsRdr\\")
    {
        SourceClass::Remote
    } else {
        SourceClass::Unknown
    }
}
pub(super) fn stamp(path: &Path, m: &fs::Metadata) -> io::Result<Stamp> {
    observe(path, m, false, false, false)
}
pub(super) fn observe(
    path: &Path,
    _: &fs::Metadata,
    detailed: bool,
    allocation: bool,
    owners: bool,
) -> io::Result<Stamp> {
    observe_with(detailed, allocation, owners, |rights| {
        metadata_open(path, rights)
    })
}

fn metadata_open(path: &Path, rights: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .access_mode(rights)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn observe_with(
    detailed: bool,
    allocation: bool,
    owners: bool,
    open: impl FnMut(u32) -> io::Result<fs::File>,
) -> io::Result<Stamp> {
    let (file, owner_error) = open_for_observation(detailed, owners, open)?;
    observe_file(&file, detailed, allocation, owners, owner_error)
}

fn open_for_observation(
    detailed: bool,
    owners: bool,
    mut open: impl FnMut(u32) -> io::Result<fs::File>,
) -> io::Result<(fs::File, Option<u32>)> {
    Ok(if detailed && owners {
        match open(FILE_READ_ATTRIBUTES | READ_CONTROL) {
            Ok(f) => (f, None),
            Err(e) => (
                open(FILE_READ_ATTRIBUTES)?,
                Some(e.raw_os_error().unwrap_or(0) as u32),
            ),
        }
    } else {
        (open(FILE_READ_ATTRIBUTES)?, None)
    })
}

fn observe_file(
    file: &fs::File,
    detailed: bool,
    allocation: bool,
    owners: bool,
    owner_error: Option<u32>,
) -> io::Result<Stamp> {
    let m = file.metadata()?;
    use std::os::windows::fs::MetadataExt;
    if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(denied());
    }
    let identity = handle_identity(file, m.len(), m.is_dir())?;
    let mut properties = NodeProperties::default();
    if detailed {
        use crate::{MetadataValue, ValueAccuracy};
        if allocation && m.is_file() {
            let mut info = MaybeUninit::<FILE_STANDARD_INFO>::uninit();
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    info.as_mut_ptr().cast(),
                    size_of::<FILE_STANDARD_INFO>() as u32,
                )
            };
            properties.allocated = if ok != 0 {
                allocation_value(unsafe { info.assume_init() }, m.len())
            } else {
                MetadataValue::Unknown
            };
        }
        properties.accessed =
            m.accessed()
                .map_or(MetadataValue::Unknown, |value| MetadataValue::Known {
                    value,
                    accuracy: ValueAccuracy::Exact,
                });
        if owners {
            properties.owner = if let Some(code) = owner_error {
                crate::owner::windows::missing(code)
            } else {
                profile::measure(profile::Stage::Owner, || {
                    crate::owner::windows::collect(file)
                })
            };
        }
    }
    Ok(Stamp {
        properties,
        identity,
        created: m.created().ok(),
        modified: m.modified().ok(),
        length: m.len(),
        directory: m.is_dir(),
        regular: m.is_file(),
    })
}

fn handle_identity(
    file: &fs::File,
    length: u64,
    directory: bool,
) -> io::Result<Option<LocalFileId>> {
    let mut id = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: matching FILE_ID_INFO class, live handle and writable buffer.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            id.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    Ok(if ok != 0 {
        let id = unsafe { id.assume_init() };
        Some(LocalFileId::Windows {
            volume_serial: id.VolumeSerialNumber,
            file_id: id.FileId.Identifier,
        })
    } else {
        let error = io::Error::last_os_error();
        // Some native SMB redirector/server pairs expose only the 64-bit
        // handle identity. Match the content reader's restricted fallback;
        // access denial and arbitrary network errors must not be retried.
        if !matches!(error.raw_os_error(), Some(1 | 50 | 87)) {
            return Err(error);
        }
        let mut legacy = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: live read-only metadata handle and matching writable buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), legacy.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful API initialized the complete output.
        legacy_identity(unsafe { legacy.assume_init() }, length, directory)
    })
}

// Keep the legacy identity in a separate domain from FILE_ID_INFO. A server
// changing which API it exposes cannot silently prove source continuity.
fn legacy_identity(
    info: BY_HANDLE_FILE_INFORMATION,
    length: u64,
    directory: bool,
) -> Option<LocalFileId> {
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let observed_length = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
    if index == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || observed_length != length
    {
        return None;
    }
    Some(LocalFileId::WindowsLegacy {
        volume: info.dwVolumeSerialNumber,
        index,
    })
}

// Match the historical collector's same-handle consistency checks. A mutation
// between metadata and FILE_STANDARD_INFO is unavailable, never an exact mix.
fn allocation_value(info: FILE_STANDARD_INFO, length: u64) -> crate::MetadataValue<u64> {
    use crate::{MetadataValue as M, ValueAccuracy};
    if info.Directory || info.NumberOfLinks == 0 || u64::try_from(info.EndOfFile) != Ok(length) {
        return M::Unknown;
    }
    u64::try_from(info.AllocationSize).map_or(M::Unknown, |value| M::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MetadataValue as M, ValueAccuracy as A};
    #[test]
    fn unc_boundaries_are_remote_without_contacting_the_server() {
        for path in [
            r"\\offline.invalid\share\folder",
            r"\\?\UNC\offline.invalid\share\folder",
        ] {
            let table = boundaries(Path::new(path)).unwrap();
            assert_eq!(table.len(), 1);
            assert_eq!(table[0].class, SourceClass::Remote);
            assert!(Path::new(path).starts_with(&table[0].path));
            assert!(Guard::open(Path::new(path), Policy::default()).is_err());
        }
        let a = boundaries(Path::new(r"\\offline.invalid\share\a")).unwrap();
        let b = boundaries(Path::new(r"\\offline.invalid\share\b")).unwrap();
        let c = boundaries(Path::new(r"\\offline.invalid\other\a")).unwrap();
        assert_eq!(a[0], b[0]);
        assert_ne!(a[0].token, c[0].token);
    }

    #[test]
    fn redirector_mappings_are_remote_and_unknown_devices_stay_unknown() {
        let classify = |value: &str| {
            device_class(
                &std::ffi::OsStr::new(value)
                    .encode_wide()
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(classify(r"\Device\HarddiskVolume3"), SourceClass::Local);
        assert_eq!(
            classify(r"\Device\Mup\;X:session\server\share"),
            SourceClass::Remote
        );
        assert_eq!(
            classify(r"\Device\LanmanRedirector\;X:session\server\share"),
            SourceClass::Remote
        );
        assert_eq!(
            classify(r"\Device\NfsRdr\server\export"),
            SourceClass::Remote
        );
        assert_eq!(classify(r"\??\C:\alias"), SourceClass::Unknown);
    }

    #[test]
    fn legacy_handle_identity_rejects_mutation_links_and_zero_indices() {
        let mut info = BY_HANDLE_FILE_INFORMATION {
            dwVolumeSerialNumber: 17,
            nFileIndexLow: 42,
            nFileSizeLow: 7,
            ..Default::default()
        };
        assert_eq!(
            legacy_identity(info, 7, false),
            Some(LocalFileId::WindowsLegacy {
                volume: 17,
                index: 42,
            })
        );
        assert_eq!(legacy_identity(info, 8, false), None);
        assert_eq!(legacy_identity(info, 7, true), None);
        info.dwFileAttributes = FILE_ATTRIBUTE_REPARSE_POINT;
        assert_eq!(legacy_identity(info, 7, false), None);
        info.dwFileAttributes = FILE_ATTRIBUTE_DIRECTORY;
        assert!(legacy_identity(info, 7, true).is_some());
        info.nFileIndexLow = 0;
        assert_eq!(legacy_identity(info, 7, true), None);
    }

    #[test]
    fn allocation_rejects_inconsistent_same_handle_metadata() {
        let mut info = FILE_STANDARD_INFO {
            AllocationSize: 0,
            EndOfFile: 7,
            NumberOfLinks: 1,
            DeletePending: false,
            Directory: false,
        };
        assert_eq!(
            allocation_value(info, 7),
            M::Known {
                value: 0,
                accuracy: A::Exact
            }
        );
        info.AllocationSize = 4096;
        assert_eq!(
            allocation_value(info, 7),
            M::Known {
                value: 4096,
                accuracy: A::Exact
            }
        );
        assert_eq!(allocation_value(info, 8), M::Unknown);
        info.AllocationSize = -1;
        assert_eq!(allocation_value(info, 7), M::Unknown);
        info.AllocationSize = 4096;
        info.Directory = true;
        assert_eq!(allocation_value(info, 7), M::Unknown);
        info.Directory = false;
        info.NumberOfLinks = 0;
        assert_eq!(allocation_value(info, 7), M::Unknown);
    }
}
