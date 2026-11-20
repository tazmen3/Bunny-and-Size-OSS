//! A locally authorized volume identity, independent of its mount spelling.
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskVolume(#[cfg(windows)] [u16; 50], #[cfg(not(windows))] u64);
#[cfg(test)]
impl DiskVolume {
    pub(super) fn fixture(serial: u16) -> Self {
        #[cfg(windows)]
        let key = [serial; 50];
        #[cfg(not(windows))]
        let key = u64::from(serial);
        Self(key)
    }
}

#[cfg(windows)]
pub fn native_disk_volume(path: &Path) -> Option<DiskVolume> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if path[..path.len() - 1].contains(&0) {
        return None;
    }
    let mut mount = vec![0; 32_768];
    let mut guid = [0; 50];
    // SAFETY: the source path is NUL terminated and both output buffers
    // remain writable for the declared length of their respective calls.
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) == 0
            || GetVolumeNameForVolumeMountPointW(
                mount.as_ptr(),
                guid.as_mut_ptr(),
                guid.len() as u32,
            ) == 0
        {
            return None;
        }
    }
    for unit in &mut guid {
        if (*unit >= u16::from(b'a')) && (*unit <= u16::from(b'z')) {
            *unit -= u16::from(b'a' - b'A');
        }
    }
    Some(DiskVolume(guid))
}

#[cfg(target_os = "linux")]
pub fn native_disk_volume(path: &Path) -> Option<DiskVolume> {
    use std::os::unix::fs::MetadataExt;
    Some(DiskVolume(path.metadata().ok()?.dev()))
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn native_disk_volume(_: &Path) -> Option<DiskVolume> {
    None
}
