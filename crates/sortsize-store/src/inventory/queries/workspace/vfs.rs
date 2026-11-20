//! Private quota VFS (database, WAL, SHM and rollback journals). All OS operations delegate to SQLite's default
//! VFS; only growth/deletion accounting and preallocation are intercepted.
//! Never the default VFS. Owners explicitly select disposable query mode or
//! persistent canonical mode; the latter retains data and a global ledger.
use super::*;
use libsqlite3_sys as ffi;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
};

struct Context {
    parent: *mut ffi::sqlite3_vfs,
    space: Arc<Workspace>,
}
pub(in crate::inventory) struct Vfs {
    raw: Box<ffi::sqlite3_vfs>,
    _context: Box<Context>,
    name: CString,
}
// SAFETY: a registration owns stable boxed callback/context addresses. Its
// mutable ledger is mutex-protected; the native parent is SQLite's initialized
// thread-safe default VFS. Moving ownership is allowed only with its non-shared
// Connection (rusqlite Connection is Send); Vfs is deliberately not Sync.
unsafe impl Send for Vfs {}
impl Vfs {
    pub fn new(space: Arc<Workspace>) -> Result<Self> {
        // SAFETY: SQLite is initialized before accessing its static default VFS.
        unsafe {
            if ffi::sqlite3_initialize() != ffi::SQLITE_OK {
                return Err(Error::Contract("SQLite initialization failed"));
            }
            let parent = if cfg!(windows) && space.adaptive_mode() {
                ffi::sqlite3_vfs_find(c"win32-longpath".as_ptr())
            } else {
                ffi::sqlite3_vfs_find(ptr::null())
            };
            if parent.is_null() || !valid_vfs(&*parent) {
                return Err(Error::Contract("SQLite OS VFS unavailable"));
            }
            let mut context = Box::new(Context { parent, space });
            let name = CString::new(format!("bunny-query-{:p}", context.as_ref())).unwrap();
            let mut raw: Box<ffi::sqlite3_vfs> = Box::new(std::mem::zeroed());
            raw.iVersion = 1;
            raw.szOsFile = std::mem::size_of::<GuardFile>() as c_int;
            raw.mxPathname = (*parent).mxPathname;
            raw.zName = name.as_ptr();
            raw.pAppData = (&mut *context as *mut Context).cast();
            raw.xOpen = Some(open);
            raw.xDelete = Some(delete);
            raw.xAccess = Some(access);
            raw.xFullPathname = Some(full_path);
            raw.xRandomness = Some(random);
            raw.xSleep = Some(sleep);
            raw.xCurrentTime = Some(time);
            raw.xGetLastError = Some(last_error);
            // No extension loading, mmap, or unnamed temp files.
            if ffi::sqlite3_vfs_register(&mut *raw, 0) != ffi::SQLITE_OK {
                return Err(Error::Contract("query VFS registration failed"));
            }
            Ok(Self {
                raw,
                _context: context,
                name,
            })
        }
    }
    pub fn name(&self) -> &str {
        self.name.to_str().unwrap()
    }
}
impl Drop for Vfs {
    fn drop(&mut self) {
        // SAFETY: Query drops/closes its connection before this registration.
        unsafe {
            ffi::sqlite3_vfs_unregister(&mut *self.raw);
        }
    }
}
#[repr(C)]
struct GuardFile {
    base: ffi::sqlite3_file,
    handle: *mut Handle,
}
struct OwnedFilename(ffi::sqlite3_filename);
impl Drop for OwnedFilename {
    fn drop(&mut self) {
        // SAFETY: allocated by sqlite3_create_filename and retained until after
        // the native xClose; no parent pointer can outlive this owner.
        unsafe {
            ffi::sqlite3_free_filename(self.0);
        }
    }
}
struct Handle {
    real: *mut ffi::sqlite3_file,
    space: Arc<Workspace>,
    path: PathBuf,
    length: u64,
    shm_open: bool,
    wal: bool,
    sync_phase: Phase,
    owned_filename: Option<OwnedFilename>,
}
fn valid_vfs(v: &ffi::sqlite3_vfs) -> bool {
    v.iVersion >= 1
        && v.szOsFile >= std::mem::size_of::<ffi::sqlite3_file>() as c_int
        && v.mxPathname > 0
        && v.xOpen.is_some()
        && v.xDelete.is_some()
        && v.xAccess.is_some()
        && v.xFullPathname.is_some()
        && v.xRandomness.is_some()
        && v.xSleep.is_some()
        && v.xCurrentTime.is_some()
}
fn valid_methods(m: &ffi::sqlite3_io_methods, main: bool) -> bool {
    m.iVersion >= 1
        && m.xClose.is_some()
        && m.xRead.is_some()
        && m.xWrite.is_some()
        && m.xTruncate.is_some()
        && m.xSync.is_some()
        && m.xFileSize.is_some()
        && m.xLock.is_some()
        && m.xUnlock.is_some()
        && m.xCheckReservedLock.is_some()
        && m.xFileControl.is_some()
        && m.xSectorSize.is_some()
        && m.xDeviceCharacteristics.is_some()
        && (!main
            || (m.iVersion >= 2
                && m.xShmMap.is_some()
                && m.xShmLock.is_some()
                && m.xShmBarrier.is_some()
                && m.xShmUnmap.is_some()))
}
fn ordinary_file(path: &Path) -> Option<u64> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & 0x400 != 0 {
                    return None;
                }
            }
            (m.is_file() && !m.is_symlink()).then_some(m.len())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}
fn confirmed_missing(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}
#[cfg(windows)]
unsafe fn opened_identity(real: *mut ffi::sqlite3_file, _: &Path) -> Option<ArtifactIdentity> {
    use windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: called only on the successfully opened parent file. Its private
    // handle stays borrowed for identity collection and is never returned.
    unsafe {
        let mut native: HANDLE = ptr::null_mut();
        if (*(*real).pMethods).xFileControl.unwrap()(
            real,
            ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&raw mut native).cast(),
        ) != ffi::SQLITE_OK
        {
            return None;
        }
        ArtifactIdentity::from_handle(native).ok()
    }
}
#[cfg(not(windows))]
unsafe fn opened_identity(_: *mut ffi::sqlite3_file, path: &Path) -> Option<ArtifactIdentity> {
    ArtifactIdentity::from_file(&std::fs::File::open(path).ok()?).ok()
}

#[cfg(windows)]
unsafe fn native_quota_failure(real: *mut ffi::sqlite3_file) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_DISK_FULL, ERROR_DISK_QUOTA_EXCEEDED, ERROR_HANDLE_DISK_FULL,
    };
    // SAFETY: this is the parent VFS's live file and storage for its last errno.
    unsafe {
        let mut code: c_int = 0;
        (*(*real).pMethods).xFileControl.unwrap()(
            real,
            ffi::SQLITE_FCNTL_LAST_ERRNO,
            (&raw mut code).cast(),
        ) == ffi::SQLITE_OK
            && matches!(
                code as u32,
                ERROR_DISK_FULL | ERROR_DISK_QUOTA_EXCEEDED | ERROR_HANDLE_DISK_FULL
            )
    }
}

#[cfg(not(windows))]
unsafe fn native_quota_failure(_: *mut ffi::sqlite3_file) -> bool {
    false
}

unsafe fn close_failed_open(real: *mut ffi::sqlite3_file, space: &Workspace, path: &Path) -> bool {
    // SAFETY: parent xOpen initialized real; SQLite requires cleanup whenever
    // pMethods is non-null, including on failure. A failed cleanup keeps a
    // sticky reference because the native handle may still own disk space.
    unsafe {
        let mut closed = true;
        if !(*real).pMethods.is_null() {
            let result = (*(*real).pMethods)
                .xClose
                .map_or(ffi::SQLITE_IOERR_CLOSE, |close| close(real));
            #[cfg(test)]
            let result = if space.take_fault("vfs_open_cleanup_close") {
                ffi::SQLITE_IOERR_CLOSE
            } else {
                result
            };
            if result != ffi::SQLITE_OK {
                space.vfs_opened(path);
                closed = false;
            }
        }
        ffi::sqlite3_free(real.cast());
        closed
    }
}
unsafe fn context<'a>(v: *mut ffi::sqlite3_vfs) -> &'a Context {
    // SAFETY: pAppData points to the live boxed Context owned by Vfs.
    unsafe { &*((*v).pAppData.cast::<Context>()) }
}
unsafe fn handle<'a>(f: *mut ffi::sqlite3_file) -> &'a mut Handle {
    // SAFETY: SQLite passes only GuardFiles successfully opened by this VFS.
    unsafe { &mut *(*(f.cast::<GuardFile>())).handle }
}
unsafe fn path(name: *const c_char) -> Option<PathBuf> {
    if name.is_null() {
        return None;
    }
    // SAFETY: SQLite's filename argument is NUL terminated for this callback.
    let s = unsafe { CStr::from_ptr(name) };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(s.to_bytes())))
    }
    #[cfg(not(unix))]
    {
        s.to_str().ok().map(PathBuf::from)
    }
}
unsafe extern "C" fn open(
    v: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite owns file storage of szOsFile bytes. Parent storage uses
    // sqlite3_malloc64's native alignment and is freed after parent xClose.
    unsafe {
        (*file).pMethods = ptr::null();
        let c = context(v);
        let transient = name.is_null() || flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0;
        let mut owned_filename = None;
        let mut actual_name = name;
        let mut actual_flags = flags;
        let path = if transient {
            let transient_types = ffi::SQLITE_OPEN_SUBJOURNAL
                | ffi::SQLITE_OPEN_TEMP_DB
                | ffi::SQLITE_OPEN_TEMP_JOURNAL
                | ffi::SQLITE_OPEN_TRANSIENT_DB;
            let all_types = transient_types
                | ffi::SQLITE_OPEN_MAIN_DB
                | ffi::SQLITE_OPEN_MAIN_JOURNAL
                | ffi::SQLITE_OPEN_WAL
                | ffi::SQLITE_OPEN_SUPER_JOURNAL;
            let kind = flags & all_types;
            if !c.space.allows_transients()
                || !matches!(
                    kind,
                    ffi::SQLITE_OPEN_SUBJOURNAL
                        | ffi::SQLITE_OPEN_TEMP_DB
                        | ffi::SQLITE_OPEN_TEMP_JOURNAL
                        | ffi::SQLITE_OPEN_TRANSIENT_DB
                )
            {
                return ffi::SQLITE_CANTOPEN;
            }
            let Ok(path) = c.space.transient_path() else {
                return ffi::SQLITE_FULL;
            };
            #[cfg(unix)]
            let text = {
                use std::os::unix::ffi::OsStrExt;
                CString::new(path.as_os_str().as_bytes())
            };
            #[cfg(not(unix))]
            let text = path
                .to_str()
                .ok_or(())
                .and_then(|text| CString::new(text).map_err(|_| ()));
            let Ok(text) = text else {
                return ffi::SQLITE_CANTOPEN;
            };
            let filename = ffi::sqlite3_create_filename(
                text.as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                0,
                ptr::null_mut(),
            );
            if filename.is_null() {
                return ffi::SQLITE_NOMEM;
            }
            owned_filename = Some(OwnedFilename(filename));
            actual_name = filename;
            actual_flags |= ffi::SQLITE_OPEN_EXCLUSIVE
                | ffi::SQLITE_OPEN_CREATE
                | ffi::SQLITE_OPEN_READWRITE
                | ffi::SQLITE_OPEN_DELETEONCLOSE;
            path
        } else {
            let Some(path) = path(name) else {
                return ffi::SQLITE_CANTOPEN;
            };
            path
        };
        let Some(length) = ordinary_file(&path) else {
            return ffi::SQLITE_CANTOPEN;
        };
        let adaptive = c.space.adaptive_mode();
        if adaptive {
            let supported = transient
                || (flags & ffi::SQLITE_OPEN_MAIN_DB != 0
                    && path.file_name().is_some_and(|name| name == "duplicates.db"))
                || (flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0
                    && path
                        .file_name()
                        .is_some_and(|name| name == "duplicates.db-journal"));
            if !supported {
                return ffi::SQLITE_CANTOPEN;
            }
            let missing = confirmed_missing(&path);
            if missing == c.space.owns_adaptive_file(&path) {
                // Existing foreign file, or an owned file has been removed
                // unexpectedly. No native open or deletion is authorized.
                return ffi::SQLITE_CANTOPEN;
            }
            if missing {
                if actual_flags & ffi::SQLITE_OPEN_CREATE == 0 {
                    return ffi::SQLITE_CANTOPEN;
                }
                actual_flags |= ffi::SQLITE_OPEN_EXCLUSIVE;
            }
        }
        let Some(admission) = c.space.admit_growth(&path, length) else {
            return ffi::SQLITE_FULL;
        };
        #[cfg(test)]
        if c.space.take_fault("vfs_open") {
            return ffi::SQLITE_CANTOPEN;
        }
        let real = ffi::sqlite3_malloc64((*c.parent).szOsFile as u64).cast::<ffi::sqlite3_file>();
        if real.is_null() {
            return ffi::SQLITE_NOMEM;
        }
        ptr::write_bytes(real.cast::<u8>(), 0, (*c.parent).szOsFile as usize);
        let result = (*c.parent).xOpen.unwrap()(c.parent, actual_name, real, actual_flags, out);
        drop(admission);
        #[cfg(test)]
        let result = if c.space.take_fault("vfs_open_after_native") {
            c.space.inject("vfs_open_cleanup_close");
            ffi::SQLITE_CANTOPEN
        } else {
            result
        };
        if result != ffi::SQLITE_OK {
            if !close_failed_open(real, &c.space, &path) {
                std::mem::forget(owned_filename.take());
            }
            return result;
        }
        let main = flags & ffi::SQLITE_OPEN_MAIN_DB != 0;
        if (*real).pMethods.is_null()
            || !valid_methods(&*(*real).pMethods, main)
            || (adaptive
                && !transient
                && !opened_identity(real, &path)
                    .is_some_and(|identity| c.space.claim_adaptive_file(&path, identity)))
            || !c.space.vfs_opened(&path)
        {
            if !close_failed_open(real, &c.space, &path) {
                std::mem::forget(owned_filename.take());
            }
            return ffi::SQLITE_CANTOPEN;
        }
        let wal = flags & ffi::SQLITE_OPEN_WAL != 0;
        let sync_phase = if wal {
            Phase::WalSync
        } else if main {
            Phase::DbSync
        } else {
            Phase::OtherSync
        };
        let h = Box::new(Handle {
            real,
            space: c.space.clone(),
            path,
            length,
            shm_open: false,
            wal,
            sync_phase,
            owned_filename,
        });
        ptr::write(
            file.cast::<GuardFile>(),
            GuardFile {
                base: ffi::sqlite3_file {
                    pMethods: if main { &METHODS } else { &JOURNAL_METHODS },
                },
                handle: Box::into_raw(h),
            },
        );
        ffi::SQLITE_OK
    }
}
unsafe extern "C" fn close(f: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: each successful xOpen is paired with one xClose by SQLite.
    unsafe {
        let mut h = Box::from_raw((*(f.cast::<GuardFile>())).handle);
        let timer = Phase::NativeClose.start();
        let result = ((*(*h.real).pMethods).xClose.unwrap())(h.real);
        timer.finish_io(0, result != ffi::SQLITE_OK);
        ffi::sqlite3_free(h.real.cast());
        (*f).pMethods = ptr::null();
        (*(f.cast::<GuardFile>())).handle = ptr::null_mut();
        #[cfg(test)]
        let result = if h.space.take_fault("vfs_close") {
            ffi::SQLITE_IOERR_CLOSE
        } else {
            result
        };
        if result == ffi::SQLITE_OK {
            h.space.vfs_closed(&h.path);
            if h.owned_filename.is_some() && confirmed_missing(&h.path) {
                h.space.forget_deleted(&h.path);
            }
            if h.shm_open {
                let shm = shm_path(&h.path);
                h.space.vfs_closed(&shm);
                if confirmed_missing(&shm) {
                    h.space.forget_deleted(&shm);
                }
            }
        } else {
            // A failed native close may retain references to its filename.
            // Keep that bounded allocation alongside the sticky disk charge.
            std::mem::forget(h.owned_filename.take());
        }
        result
    }
}
unsafe extern "C" fn write(
    f: *mut ffi::sqlite3_file,
    buf: *const c_void,
    n: c_int,
    offset: i64,
) -> c_int {
    // SAFETY: SQLite provides a readable buffer and nonnegative offsets. Check
    // arithmetic and reserve before calling the native write operation.
    unsafe {
        let h = handle(f);
        let Some(end) = u64::try_from(offset)
            .ok()
            .and_then(|o| u64::try_from(n).ok().and_then(|n| o.checked_add(n)))
        else {
            return ffi::SQLITE_FULL;
        };
        let _admission = if end > h.length {
            let Some(grant) = h.space.admit_extent(&h.path, end, Some(h.length)) else {
                return ffi::SQLITE_FULL;
            };
            Some(grant)
        } else {
            None
        };
        #[cfg(test)]
        if h.space.take_fault("vfs_full") {
            return ffi::SQLITE_FULL;
        }
        #[cfg(test)]
        if h.space.take_fault("vfs_write") {
            if n > 1 {
                ((*(*h.real).pMethods).xWrite.unwrap())(h.real, buf, n / 2, offset);
            }
            return ffi::SQLITE_IOERR_WRITE;
        }
        let timer = if h.wal {
            Phase::WalWrite
        } else if matches!(h.sync_phase, Phase::DbSync) {
            Phase::DbWrite
        } else {
            Phase::OtherWrite
        }
        .start();
        let result = ((*(*h.real).pMethods).xWrite.unwrap())(h.real, buf, n, offset);
        let result = if result == ffi::SQLITE_IOERR_WRITE
            && h.space.adaptive_mode()
            && native_quota_failure(h.real)
        {
            ffi::SQLITE_FULL
        } else {
            result
        };
        if result == ffi::SQLITE_FULL {
            h.space.note_native_space_error();
        }
        let bytes = if result == ffi::SQLITE_OK {
            h.length = h.length.max(end);
            n as u64
        } else {
            0
        };
        timer.finish_io(bytes, result != ffi::SQLITE_OK);
        result
    }
}
unsafe extern "C" fn truncate(f: *mut ffi::sqlite3_file, n: i64) -> c_int {
    // SAFETY: the parent validates the truncate; a failed operation retains the
    // conservative charge. Shrinks are only uncharged when the file is deleted.
    unsafe {
        let h = handle(f);
        let Ok(nu) = u64::try_from(n) else {
            return ffi::SQLITE_FULL;
        };
        let _admission = if nu > h.length {
            let Some(grant) = h.space.admit_extent(&h.path, nu, Some(h.length)) else {
                return ffi::SQLITE_FULL;
            };
            Some(grant)
        } else {
            None
        };
        #[cfg(test)]
        if h.space.take_fault("vfs_truncate") {
            return ffi::SQLITE_IOERR_TRUNCATE;
        }
        let result = ((*(*h.real).pMethods).xTruncate.unwrap())(h.real, n);
        if result == ffi::SQLITE_FULL {
            h.space.note_native_space_error();
        }
        if result == ffi::SQLITE_OK {
            h.length = if h.space.adaptive_mode() {
                nu
            } else {
                h.length.max(nu)
            };
        }
        result
    }
}
unsafe extern "C" fn control(f: *mut ffi::sqlite3_file, op: c_int, arg: *mut c_void) -> c_int {
    // Default deny, including future controls. In particular WIN32_GET_HANDLE,
    // NULL_IO, mmap, atomic writes, and size hints must not bypass this wrapper.
    if !matches!(
        op,
        ffi::SQLITE_FCNTL_LOCKSTATE
            | ffi::SQLITE_FCNTL_LAST_ERRNO
            | ffi::SQLITE_FCNTL_HAS_MOVED
            | ffi::SQLITE_FCNTL_VFSNAME
            | ffi::SQLITE_FCNTL_EXTERNAL_READER
    ) {
        return ffi::SQLITE_NOTFOUND;
    }
    // SAFETY: the five audited introspection controls keep SQLite's arguments.
    unsafe {
        let h = handle(f);
        ((*(*h.real).pMethods).xFileControl.unwrap())(h.real, op, arg)
    }
}
unsafe extern "C" fn delete(v: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    // SAFETY: forward the filename unchanged, and only uncharge after success.
    unsafe {
        let c = context(v);
        let Some(p) = path(name) else {
            return ffi::SQLITE_IOERR_DELETE;
        };
        if c.space.adaptive_mode() {
            #[cfg(test)]
            if c.space.take_fault("vfs_delete") {
                return ffi::SQLITE_IOERR_DELETE;
            }
            let result = c.space.remove_adaptive_file(&p);
            return match result {
                Ok(()) => {
                    c.space.forget_deleted(&p);
                    ffi::SQLITE_OK
                }
                Err(_) if p.parent() == Some(c.space.path()) && confirmed_missing(&p) => {
                    c.space.forget_deleted(&p);
                    ffi::SQLITE_IOERR_DELETE_NOENT
                }
                Err(_) => ffi::SQLITE_IOERR_DELETE,
            };
        }
        if !c.space.may_delete(&p) {
            return ffi::SQLITE_IOERR_DELETE;
        }
        #[cfg(test)]
        if c.space.take_fault("vfs_delete") {
            return ffi::SQLITE_IOERR_DELETE;
        }
        let rc = (*c.parent).xDelete.unwrap()(c.parent, name, sync);
        if rc == ffi::SQLITE_OK {
            c.space.forget_deleted(&p);
        }
        rc
    }
}
fn shm_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-shm");
    name.into()
}
unsafe extern "C" fn shm_map(
    f: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    // SAFETY: WAL shared memory can grow only via this callback. Reserve the
    // entire mapping before the native VFS allocates/maps the SHM file.
    unsafe {
        *out = ptr::null_mut();
        let h = handle(f);
        let Some(end) = u64::try_from(page)
            .ok()
            .and_then(|n| n.checked_add(1))
            .and_then(|n| u64::try_from(size).ok().and_then(|s| n.checked_mul(s)))
        else {
            return ffi::SQLITE_FULL;
        };
        // unixShmMap rounds to OS mapping pages, which can exceed SQLite's
        // 32 KiB region. Charge before that native growth too.
        let unit = h.space.mapping_unit;
        let Some(end) = end.checked_add(unit - 1).map(|n| n / unit * unit) else {
            return ffi::SQLITE_FULL;
        };
        let shm = shm_path(&h.path);
        if ordinary_file(&shm).is_none() || !h.space.charge(&shm, end) {
            return ffi::SQLITE_FULL;
        }
        #[cfg(test)]
        if h.space.take_fault("vfs_shm") {
            return ffi::SQLITE_IOERR_SHMSIZE;
        }
        if !h.shm_open {
            if !h.space.vfs_opened(&shm) {
                return ffi::SQLITE_FULL;
            }
            h.shm_open = true;
        }
        match (*(*h.real).pMethods).xShmMap {
            Some(map) => map(h.real, page, size, extend, out),
            None => ffi::SQLITE_IOERR_SHMMAP,
        }
    }
}
unsafe extern "C" fn shm_lock(
    f: *mut ffi::sqlite3_file,
    offset: c_int,
    n: c_int,
    flags: c_int,
) -> c_int {
    // SAFETY: unchanged native WAL locking arguments.
    unsafe {
        let h = handle(f);
        ((*(*h.real).pMethods).xShmLock.unwrap())(h.real, offset, n, flags)
    }
}
unsafe extern "C" fn shm_barrier(f: *mut ffi::sqlite3_file) {
    // SAFETY: unchanged native WAL memory barrier.
    unsafe {
        let h = handle(f);
        ((*(*h.real).pMethods).xShmBarrier.unwrap())(h.real)
    }
}
unsafe extern "C" fn shm_unmap(f: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    // SAFETY: native unmap completes before any successfully deleted file is
    // uncharged. Failed deletion remains conservatively accounted.
    unsafe {
        let h = handle(f);
        let rc = ((*(*h.real).pMethods).xShmUnmap.unwrap())(h.real, delete);
        let path = shm_path(&h.path);
        if rc == ffi::SQLITE_OK {
            if h.shm_open {
                h.space.vfs_closed(&path);
                h.shm_open = false;
            }
            if confirmed_missing(&path) {
                h.space.forget_deleted(&path);
            }
        }
        rc
    }
}
macro_rules! file_forward {
    ($name:ident,$method:ident,($($arg:ident:$ty:ty),*)) => {
        unsafe extern "C" fn $name(f:*mut ffi::sqlite3_file,$($arg:$ty),*)->c_int {
            // SAFETY: forward to the parent's live file with unchanged arguments.
            unsafe {let h=handle(f);((*(*h.real).pMethods).$method.unwrap())(h.real,$($arg),*)}
        }
    };
}
unsafe extern "C" fn read(
    f: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    n: c_int,
    offset: i64,
) -> c_int {
    // SAFETY: SQLite provides a writable buffer of n bytes. The parent retains
    // responsibility for zero-filling the tail of SQLITE_IOERR_SHORT_READ.
    unsafe {
        let h = handle(f);
        if n < 0 || offset < 0 {
            return ffi::SQLITE_IOERR_READ;
        }
        #[cfg(test)]
        if h.space.take_fault("vfs_read") {
            ptr::write_bytes(buf.cast::<u8>(), 0, n as usize);
            return ffi::SQLITE_IOERR_READ;
        }
        ((*(*h.real).pMethods).xRead.unwrap())(h.real, buf, n, offset)
    }
}
unsafe extern "C" fn sync(f: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    // SAFETY: unchanged native SQLite sync flags; no alternate write path.
    unsafe {
        let h = handle(f);
        #[cfg(test)]
        if h.space.take_fault("vfs_sync") {
            return ffi::SQLITE_IOERR_FSYNC;
        }
        let timer = h.sync_phase.start();
        let result = ((*(*h.real).pMethods).xSync.unwrap())(h.real, flags);
        timer.finish_io(0, result != ffi::SQLITE_OK);
        result
    }
}
file_forward!(size,xFileSize,(out:*mut i64));
file_forward!(lock,xLock,(kind:c_int));
file_forward!(unlock,xUnlock,(kind:c_int));
file_forward!(reserved,xCheckReservedLock,(out:*mut c_int));
file_forward!(sector, xSectorSize, ());
unsafe extern "C" fn characteristics(f: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: parent capability query. Atomic-write file-controls are denied.
    unsafe {
        let h = handle(f);
        ((*(*h.real).pMethods).xDeviceCharacteristics.unwrap())(h.real)
            & !ffi::SQLITE_IOCAP_BATCH_ATOMIC
    }
}
macro_rules! vfs_forward {
    ($name:ident,$method:ident,($($arg:ident:$ty:ty),*)) => {
        unsafe extern "C" fn $name(v:*mut ffi::sqlite3_vfs,$($arg:$ty),*)->c_int {
            // SAFETY: parent callbacks receive their own VFS/context unchanged.
            unsafe {let p=context(v).parent;((*p).$method.unwrap())(p,$($arg),*)}
        }
    };
}
vfs_forward!(access,xAccess,(name:*const c_char,flags:c_int,out:*mut c_int));
vfs_forward!(full_path,xFullPathname,(name:*const c_char,n:c_int,out:*mut c_char));
vfs_forward!(random,xRandomness,(n:c_int,out:*mut c_char));
vfs_forward!(sleep,xSleep,(n:c_int));
vfs_forward!(time,xCurrentTime,(out:*mut f64));
unsafe extern "C" fn last_error(v: *mut ffi::sqlite3_vfs, n: c_int, out: *mut c_char) -> c_int {
    // SAFETY: xGetLastError is optional in the SQLite VFS contract.
    unsafe {
        let p = context(v).parent;
        match (*p).xGetLastError {
            Some(get) => get(p, n, out),
            None => {
                if n > 0 {
                    *out = 0;
                }
                0
            }
        }
    }
}
static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: None,
    xUnfetch: None,
};
// Native journal/WAL handles can use version-1 methods without SHM support.
static JOURNAL_METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    ..METHODS
};

#[cfg(test)]
mod tests;
