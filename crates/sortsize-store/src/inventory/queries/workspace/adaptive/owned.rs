//! Native artifact ownership for the disposable workspace.
use std::{fs::File, io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::inventory::queries::workspace) struct ArtifactIdentity {
    device: u64,
    inode: u64,
    #[cfg(windows)]
    created: u64,
}

impl ArtifactIdentity {
    #[cfg(windows)]
    pub(in crate::inventory::queries::workspace) fn from_file(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the caller keeps the File alive for the query.
        unsafe { Self::from_handle(file.as_raw_handle()) }
    }

    #[cfg(windows)]
    pub(in crate::inventory::queries::workspace) unsafe fn from_handle(
        handle: windows_sys::Win32::Foundation::HANDLE,
    ) -> io::Result<Self> {
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            GetFileInformationByHandle,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: caller passes the live native I/O handle and info is writable.
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || info.nNumberOfLinks != 1
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let combine = |high, low| u64::from(high) << 32 | u64::from(low);
        Ok(Self {
            device: u64::from(info.dwVolumeSerialNumber),
            inode: combine(info.nFileIndexHigh, info.nFileIndexLow),
            created: combine(
                info.ftCreationTime.dwHighDateTime,
                info.ftCreationTime.dwLowDateTime,
            ),
        })
    }

    #[cfg(unix)]
    pub(in crate::inventory::queries::workspace) fn from_file(file: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub(in crate::inventory::queries::workspace) fn from_file(_: &File) -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    pub(in crate::inventory::queries::workspace) fn remove(self, path: &Path) -> io::Result<()> {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        let handle = OpenOptions::new()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if Self::from_file(&handle)? != self {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        mark_for_removal(&handle)
    }

    #[cfg(not(windows))]
    pub(in crate::inventory::queries::workspace) fn remove(self, path: &Path) -> io::Result<()> {
        if std::fs::symlink_metadata(path)?.is_symlink()
            || Self::from_file(&File::open(path)?)? != self
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        std::fs::remove_file(path)
    }
}

#[cfg(windows)]
pub(super) fn mark_for_removal(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let deletion = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle was opened with DELETE and refers to the object the
    // caller has identified. The input is the corresponding Win32 struct.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const deletion).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
