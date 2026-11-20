//! Local metadata acquisition relative to an authorized directory descriptor.
use super::{Guard, ScanEntry, SourceClass, Stamp, denied, observe, stamp};
use std::{
    ffi::{CStr, CString, OsStr, OsString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
    ptr::NonNull,
};

const RELATIVE_SCOPE: u64 =
    libc::RESOLVE_NO_XDEV | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_BENEATH;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn open_at(parent: RawFd, path: &OsStr, flags: libc::c_int, resolve: u64) -> io::Result<File> {
    let path = CString::new(path.as_bytes())?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: openat2 receives a live C string and the three-field UAPI struct.
    // A successful call returns a new descriptor owned by this function.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent,
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::EXDEV | libc::ELOOP) => denied(),
            _ => error,
        });
    }
    Ok(unsafe { File::from_raw_fd(fd as RawFd) })
}

fn mount_id(file: &File) -> io::Result<u64> {
    let mut information = std::mem::MaybeUninit::<libc::statx>::uninit();
    // SAFETY: statx consumes an empty C string with AT_EMPTY_PATH and writes
    // a statx structure on success. The file remains open during the call.
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
            libc::STATX_MNT_ID,
            information.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    if information.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(information.stx_mnt_id)
}

pub(in crate::acquisition) struct Directory {
    stream: NonNull<libc::DIR>,
    fd: RawFd,
    path: PathBuf,
    stamp: Stamp,
}

// SAFETY: the stream and descriptor are exclusively owned by Directory.
// Iteration requires &mut self and the lifetime ends with closedir.
unsafe impl Send for Directory {}

impl Directory {
    pub(in crate::acquisition) fn open(
        guard: &Guard,
        path: &Path,
        expected: &Stamp,
    ) -> io::Result<Option<Self>> {
        let boundary = guard.evidence().0;
        if boundary.class != SourceClass::Local || !guard.policy().same_volume {
            return Ok(None);
        }
        let expected_mount = boundary
            .token
            .split(|byte| *byte == b' ')
            .next()
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<u64>().ok())
            .ok_or_else(denied)?;
        let mount = match open_at(
            libc::AT_FDCWD,
            boundary.path.as_os_str(),
            libc::O_PATH | libc::O_DIRECTORY,
            libc::RESOLVE_NO_SYMLINKS,
        ) {
            Ok(mount) => mount,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match mount_id(&mount) {
            Ok(id) if id == expected_mount => {}
            Err(error) if error.kind() == io::ErrorKind::Unsupported => return Ok(None),
            Ok(_) => return Err(denied()),
            Err(error) => return Err(error),
        }

        let relative = path.strip_prefix(&boundary.path).map_err(|_| denied())?;
        let relative = if relative.as_os_str().is_empty() {
            OsStr::new(".")
        } else {
            relative.as_os_str()
        };
        let handle = open_at(
            mount.as_raw_fd(),
            relative,
            libc::O_PATH | libc::O_DIRECTORY,
            RELATIVE_SCOPE,
        )?;
        let opened = stamp(path, &handle.metadata()?)?;
        super::super::validate_root_attachment(expected, &opened)?;
        guard.authorize_path(path)?;

        // Open for listing only after checking the anchored directory identity.
        let fd = open_at(
            handle.as_raw_fd(),
            OsStr::new("."),
            libc::O_RDONLY | libc::O_DIRECTORY,
            RELATIVE_SCOPE,
        )?
        .into_raw_fd();
        // SAFETY: fd is an owned directory descriptor. On success fdopendir
        // takes ownership; on failure it leaves the descriptor to the caller.
        let Some(stream) = NonNull::new(unsafe { libc::fdopendir(fd) }) else {
            let error = io::Error::last_os_error();
            drop(unsafe { File::from_raw_fd(fd) });
            return Err(error);
        };
        Ok(Some(Self {
            stream,
            fd,
            path: path.to_owned(),
            stamp: opened,
        }))
    }

    pub(in crate::acquisition) fn validate(&self, current: &Stamp) -> io::Result<()> {
        super::super::validate_root_attachment(&self.stamp, current)
    }

    pub(in crate::acquisition) fn observe(
        &self,
        guard: &Guard,
        entry: &ScanEntry,
        allocation: bool,
        owners: bool,
    ) -> io::Result<Stamp> {
        if entry.path().parent() != Some(&self.path) || !self.path.starts_with(guard.root()) {
            return Err(denied());
        }
        let file = open_at(
            self.fd,
            entry.file_name(),
            libc::O_PATH | libc::O_NOFOLLOW,
            RELATIVE_SCOPE,
        )?;
        let metadata = file.metadata()?;
        if metadata.is_symlink() {
            return Err(denied());
        }
        observe(entry.path(), &metadata, true, allocation, owners)
    }

    pub(in crate::acquisition) fn next_entry(&mut self) -> Option<io::Result<ScanEntry>> {
        loop {
            // SAFETY: this thread owns the stream and reads d_name before the
            // next readdir. errno is thread-local and readdir leaves it set to
            // zero on EOF. The kernel's name is NUL-terminated.
            let name = unsafe {
                *libc::__errno_location() = 0;
                let entry = libc::readdir(self.stream.as_ptr());
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    return (error.raw_os_error() != Some(0)).then_some(Err(error));
                }
                CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes()
            };
            if matches!(name, b"." | b"..") {
                continue;
            }
            let name = OsString::from_vec(name.to_vec());
            return Some(Ok(ScanEntry {
                path: self.path.join(&name),
                name,
            }));
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: the uniquely owned stream closes its descriptor once.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquisition::Policy;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn relative_scan_preserves_native_metadata_and_refuses_links() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let native_name = OsStr::from_bytes(b"native-\xff");
        let source = root.join(native_name);
        fs::write(&source, b"payload").unwrap();
        fs::hard_link(&source, root.join("alias")).unwrap();
        symlink(&source, root.join("link")).unwrap();
        let guard = Guard::open(root, Policy::default()).unwrap();
        let expected = guard.observe(&source, true).unwrap();
        let mut entries = guard.scan_directory(root).unwrap();
        let linux_supported = entries.has_native_directory_handle();
        let mut seen = 0;
        while let Some(entry) = entries.next() {
            let entry = entry.unwrap();
            seen += 1;
            let observed = entries.observe(&guard, &entry, true, true);
            if entry.file_name() == "link" {
                assert_eq!(
                    observed.unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
            } else {
                assert_eq!(observed.unwrap(), expected);
                assert!(entry.file_name() == native_name || entry.file_name() == "alias");
            }
        }
        assert_eq!(seen, 3);
        if linux_supported {
            let alternate = Guard::open(
                root,
                Policy {
                    same_volume: false,
                    ..Policy::default()
                },
            )
            .unwrap();
            assert!(
                !alternate
                    .scan_directory(root)
                    .unwrap()
                    .has_native_directory_handle()
            );
        }
    }

    #[test]
    fn relative_open_cannot_cross_a_mount() {
        let root = File::open("/").unwrap();
        if mount_id(&root).ok() == File::open("/proc").and_then(|f| mount_id(&f)).ok() {
            return;
        }
        let result = open_at(
            root.as_raw_fd(),
            OsStr::new("proc"),
            libc::O_PATH,
            RELATIVE_SCOPE,
        );
        if let Err(error) = &result
            && matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL))
        {
            return;
        }
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn replaced_directory_does_not_inherit_the_original_cursor() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        let guard = Guard::open(root, Policy::default()).unwrap();
        let mut cursor = guard.scan_directory(&child).unwrap();
        if !cursor.has_native_directory_handle() {
            return;
        }

        fs::rename(&child, root.join("moved")).unwrap();
        fs::create_dir(&child).unwrap();
        fs::write(child.join("replacement"), b"must not be listed").unwrap();
        assert!(cursor.next().is_none());
        assert!(cursor.validate(&guard.inspect(&child).unwrap()).is_err());

        let swapped = guard.scan_directory_with(&child, |_| {
            fs::rename(&child, root.join("other")).unwrap();
            fs::create_dir(&child)
        });
        assert!(swapped.is_err());
    }
}
