//! OS-specific read-only content handles. Never resolve a final symlink.
use crate::LocalFileId;
use std::{
    fs::{File, Metadata},
    io,
    path::Path,
    time::SystemTime,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub identity: ContentId,
    pub length: u64,
    pub modified: SystemTime,
    pub change: (i64, i64),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ContentId {
    Native(LocalFileId),
    // Keep the legacy and 128-bit namespaces distinct. These are observations
    // within one operation, never persistent or physical-block identifiers.
    #[cfg(windows)]
    WindowsLegacy {
        volume: u32,
        index: u64,
    },
}
pub(crate) fn stamp(file: &File) -> io::Result<Stamp> {
    let m = file.metadata()?;
    if !m.is_file() {
        return Err(io::Error::other("Not a regular file"));
    }
    platform_stamp(file, &m)
}
#[cfg(target_os = "linux")]
fn platform_stamp(_file: &File, m: &Metadata) -> io::Result<Stamp> {
    use std::os::unix::fs::MetadataExt;
    Ok(Stamp {
        identity: ContentId::Native(LocalFileId::Unix {
            device: m.dev(),
            inode: m.ino(),
        }),
        length: m.len(),
        modified: m.modified()?,
        change: (m.ctime(), m.ctime_nsec()),
    })
}
#[cfg(windows)]
fn platform_stamp(file: &File, m: &Metadata) -> io::Result<Stamp> {
    use std::{
        mem::{MaybeUninit, size_of},
        os::windows::{fs::MetadataExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
    if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("Reparse point rejected"));
    }
    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: output matches FileIdInfo and is read only after API success.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    let identity = if ok != 0 {
        // SAFETY: successful API initialized the complete output.
        let info = unsafe { info.assume_init() };
        ContentId::Native(LocalFileId::Windows {
            volume_serial: info.VolumeSerialNumber,
            file_id: info.FileId.Identifier,
        })
    } else {
        let error = io::Error::last_os_error();
        // FileIdInfo is not implemented by some SMB redirector/server pairs.
        // The read-only handle API for 64-bit file indices is supported there;
        // do not retry permission or arbitrary I/O failures through another API.
        if !matches!(error.raw_os_error(), Some(1 | 50 | 87)) {
            return Err(error);
        }
        let mut legacy = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: valid live handle and correctly sized output, checked below.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), legacy.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful API initialized the complete output.
        let legacy = unsafe { legacy.assume_init() };
        let index = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
        let length = (u64::from(legacy.nFileSizeHigh) << 32) | u64::from(legacy.nFileSizeLow);
        if index == 0
            || length != m.len()
            || legacy.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "No reliable file identity",
            ));
        }
        ContentId::WindowsLegacy {
            volume: legacy.dwVolumeSerialNumber,
            index,
        }
    };
    Ok(Stamp {
        identity,
        length: m.len(),
        modified: m.modified()?,
        change: (0, 0),
    })
}
#[cfg(not(any(target_os = "linux", windows)))]
fn platform_stamp(_file: &File, _m: &Metadata) -> io::Result<Stamp> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(target_os = "linux")]
pub(crate) fn open(path: &Path) -> io::Result<File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };
    if !path.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut current = File::open("/")?;
    let components = path
        .components()
        .filter(|c| !matches!(c, Component::RootDir))
        .collect::<Vec<_>>();
    for (i, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: valid directory descriptor, NUL-terminated component, no creation.
        let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a new, uniquely owned descriptor.
        current = unsafe { File::from_raw_fd(fd) };
    }
    Ok(current)
}
#[cfg(windows)]
pub(crate) fn open(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
    // Refuse write/delete sharing while this content handle is held.
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}
#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn open(_path: &Path) -> io::Result<File> {
    Err(io::ErrorKind::Unsupported.into())
}
