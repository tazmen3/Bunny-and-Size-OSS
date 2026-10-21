//! Win32 mount manager and logical drive adapters. All strings remain UTF-16.
use super::*;
use std::{
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
    System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
};
const MAX_WIDE: usize = 65_536;
pub struct NativeVolumes;
fn wide(path: &std::ffi::OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = path.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    value.push(0);
    Ok(value)
}
fn string(value: &[u16]) -> OsString {
    OsString::from_wide(&value[..value.iter().position(|c| *c == 0).unwrap_or(value.len())])
}
fn multi(value: &[u16]) -> Vec<PathBuf> {
    value
        .split(|c| *c == 0)
        .take_while(|s| !s.is_empty())
        .map(|s| PathBuf::from(OsString::from_wide(s)))
        .collect()
}
fn kind(value: u32) -> SourceKind {
    match value {
        2 => SourceKind::Removable,
        3 | 6 => SourceKind::Local,
        4 => SourceKind::Remote,
        5 => SourceKind::Optical,
        _ => SourceKind::Unknown,
    }
}
fn blank(identity: VolumeIdentity, paths: Vec<PathBuf>, kind: SourceKind) -> Volume {
    Volume {
        identity,
        label: MetadataValue::Unknown,
        filesystem: MetadataValue::Unknown,
        kind,
        mounts: paths
            .into_iter()
            .map(|path| VolumeMount {
                path,
                mount_id: None,
                root: None,
            })
            .collect(),
        capacity: VolumeCapacity::absent(MetadataValue::NotCollected),
        availability: VolumeAvailability::NotProbed,
    }
}
struct Search(windows_sys::Win32::Foundation::HANDLE);
impl Drop for Search {
    fn drop(&mut self) {
        unsafe {
            FindVolumeClose(self.0);
        }
    }
}
fn mount_paths(name: &[u16]) -> io::Result<Vec<PathBuf>> {
    let mut value = vec![0; 512];
    let mut needed = 0;
    loop {
        // SAFETY: terminated GUID and correctly sized writable buffer.
        if unsafe {
            GetVolumePathNamesForVolumeNameW(
                name.as_ptr(),
                value.as_mut_ptr(),
                value.len() as u32,
                &mut needed,
            )
        } != 0
        {
            return Ok(multi(&value));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_MORE_DATA as i32)
            || needed as usize > MAX_WIDE
            || needed as usize <= value.len()
        {
            return Err(e);
        }
        value.resize(needed as usize, 0);
    }
}
impl VolumeProvider for NativeVolumes {
    fn enumerate(&self) -> io::Result<Vec<Volume>> {
        let mut result = Vec::new();
        let mut name = vec![0u16; 1024];
        // Mount-manager enumeration does not query volume capacity or network.
        let handle = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
        if handle != INVALID_HANDLE_VALUE {
            let _search = Search(handle);
            loop {
                let guid = string(&name);
                let terminated = wide(&guid)?;
                if let Ok(mut paths) = mount_paths(&terminated) {
                    paths.sort_by(|a, b| {
                        (a.components().count(), a).cmp(&(b.components().count(), b))
                    });
                    if let Some(path) = paths.first() {
                        let path = wide(path.as_os_str())?;
                        let drive_kind = kind(unsafe { GetDriveTypeW(path.as_ptr()) });
                        result.push(blank(VolumeIdentity::WindowsGuid(guid), paths, drive_kind));
                    }
                }
                if unsafe { FindNextVolumeW(handle, name.as_mut_ptr(), name.len() as u32) } == 0 {
                    let error = unsafe { GetLastError() };
                    if error != ERROR_NO_MORE_FILES {
                        return Err(io::Error::from_raw_os_error(error as i32));
                    }
                    break;
                }
            }
        } else if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
            return Err(io::Error::last_os_error());
        }
        // Mapped network and empty removable drives need not have a volume GUID.
        let mut drives = vec![0u16; 512];
        let size = unsafe { GetLogicalDriveStringsW(drives.len() as u32, drives.as_mut_ptr()) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        if size as usize >= drives.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        for path in multi(&drives) {
            if result
                .iter()
                .any(|v| v.mounts.iter().any(|m| m.path == path))
            {
                continue;
            }
            let w = wide(path.as_os_str())?;
            result.push(blank(
                VolumeIdentity::WindowsPath(path.clone()),
                vec![path],
                kind(unsafe { GetDriveTypeW(w.as_ptr()) }),
            ));
        }
        result.sort_by(|a, b| a.mounts[0].path.cmp(&b.mounts[0].path));
        Ok(result)
    }
    fn probe(&self, path: &Path) -> VolumeProbe {
        let path = match wide(path.as_os_str()) {
            Ok(p) => p,
            Err(e) => return VolumeProbe::error(&e),
        };
        let (mut available, mut caller_total, mut free_total) = (0, 0, 0);
        // SAFETY: terminated native UTF-16; three writable u64 outputs.
        if unsafe {
            GetDiskFreeSpaceExW(
                path.as_ptr(),
                &mut available,
                &mut caller_total,
                &mut free_total,
            )
        } == 0
        {
            return VolumeProbe::error(&io::Error::last_os_error());
        }
        // GetDiskFreeSpaceExW total is quota-sensitive. Obtain a whole-volume
        // total separately; never subtract physical free from a quota total.
        // Dynamic lookup preserves startup on Windows lacking the newer API.
        let mut total = MetadataValue::Unsupported;
        unsafe {
            let module = GetModuleHandleW(windows_sys::core::w!("kernel32.dll"));
            if !module.is_null()
                && let Some(function) =
                    GetProcAddress(module, windows_sys::core::s!("GetDiskSpaceInformationW"))
            {
                type Query =
                    unsafe extern "system" fn(*const u16, *mut DISK_SPACE_INFORMATION) -> i32;
                // SAFETY: signature from the Win32 header for this exact symbol.
                let query: Query = std::mem::transmute(function);
                let mut info = DISK_SPACE_INFORMATION::default();
                let hr = query(path.as_ptr(), &mut info);
                total = if hr >= 0 {
                    (info.SectorsPerAllocationUnit as u64)
                        .checked_mul(info.BytesPerSector as u64)
                        .filter(|u| *u > 0)
                        .and_then(|u| info.ActualTotalAllocationUnits.checked_mul(u))
                        .map(known)
                        .unwrap_or(MetadataValue::Unknown)
                } else {
                    MetadataValue::Unknown
                };
            }
        }
        let mut capacity = VolumeCapacity::from_bytes(total, known(free_total), known(available));
        capacity.caller_total = known(caller_total);
        let mut label = vec![0; 261];
        let mut fs = vec![0; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                path.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        VolumeProbe {
            capacity,
            availability: VolumeAvailability::Available,
            label: if ok != 0 {
                known(string(&label))
            } else {
                MetadataValue::Unknown
            },
            filesystem: if ok != 0 {
                known(string(&fs))
            } else {
                MetadataValue::Unknown
            },
        }
    }
}
/// Explicit UNC source only; no network enumeration or implicit query.
/// Invoke the provider probe on a worker only after user authorization.
pub fn unc_source(path: &Path) -> io::Result<Volume> {
    use std::path::{Component, Prefix};
    if !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    wide(path.as_os_str())?;
    Ok(blank(
        VolumeIdentity::WindowsPath(path.to_owned()),
        vec![path.to_owned()],
        SourceKind::Remote,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drive_types_letters_multisz_and_unpaired_utf16() {
        assert_eq!(kind(4), SourceKind::Remote);
        assert_eq!(kind(2), SourceKind::Removable);
        assert_eq!(kind(1), SourceKind::Unknown);
        assert_eq!(kind(3), SourceKind::Local);
        let paths = multi(&[67, 58, 92, 0, 68, 58, 92, 0, 0]);
        assert_eq!(paths, [PathBuf::from("C:\\"), PathBuf::from("D:\\")]);
        let native = OsString::from_wide(&[67, 58, 92, 0xd800]);
        assert_eq!(&wide(&native).unwrap()[..4], &[67, 58, 92, 0xd800]);
        assert_eq!(string(&[0xd800, 0]), OsString::from_wide(&[0xd800]));
    }
    #[test]
    fn explicit_unc_does_not_probe_and_missing_drive_is_unavailable() {
        let v = unc_source(Path::new("\\\\controlled-host\\share\\")).unwrap();
        assert_eq!(v.kind, SourceKind::Remote);
        assert_eq!(v.availability, VolumeAvailability::NotProbed);
        assert!(unc_source(Path::new("C:\\folder")).is_err());
        let p = VolumeProbe::error(&io::Error::from_raw_os_error(21)); // no media
        assert_ne!(p.availability, VolumeAvailability::Available);
        assert_eq!(p.capacity.total, MetadataValue::Unknown);
        let denied = VolumeProbe::error(&io::Error::from_raw_os_error(5));
        assert_eq!(denied.capacity.available, MetadataValue::AccessDenied);
    }
}
