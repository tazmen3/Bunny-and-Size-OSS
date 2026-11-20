//! Native callback boundary tests. Each raw handle borrows its registration;
//! no global VFS/default changes or canonical writes are used by these probes.
use super::*;
use crate::inventory::queries::tests::fixture;

struct Raw<'a> {
    file: Box<GuardFile>,
    _vfs: &'a Vfs,
    filename: ffi::sqlite3_filename,
}
impl<'a> Raw<'a> {
    fn new(vfs: &'a Vfs, filename: &Path) -> Self {
        let mut file = Box::new(GuardFile {
            base: ffi::sqlite3_file {
                pMethods: ptr::null(),
            },
            handle: ptr::null_mut(),
        });
        let name = CString::new(filename.to_str().unwrap()).unwrap();
        // sqlite3_create_filename supplies SQLite's filename metadata suffix,
        // which native xOpen may read beyond the first NUL.
        let filename = unsafe {
            let filename = ffi::sqlite3_create_filename(
                name.as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                0,
                ptr::null_mut(),
            );
            let rc = open(
                vfs.raw.as_ref() as *const _ as *mut _,
                filename,
                &mut file.base,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_MAIN_DB,
                ptr::null_mut(),
            );
            // Parent keeps zPath for the lifetime of this raw handle. Keep the
            // SQLite filename alive too, released in Drop after xClose.
            assert_eq!(rc, ffi::SQLITE_OK);
            filename
        };
        Self {
            file,
            _vfs: vfs,
            filename,
        }
    }
    fn ptr(&mut self) -> *mut ffi::sqlite3_file {
        &mut self.file.base
    }
}
impl Drop for Raw<'_> {
    fn drop(&mut self) {
        unsafe {
            if !self.file.base.pMethods.is_null() {
                close(self.ptr());
            }
            ffi::sqlite3_free_filename(self.filename);
        }
    }
}
fn del(vfs: &Vfs, path: &Path) -> c_int {
    let p = CString::new(path.to_str().unwrap()).unwrap();
    unsafe { delete(vfs.raw.as_ref() as *const _ as *mut _, p.as_ptr(), 0) }
}

#[test]
fn controls_are_default_denied_without_handle_escape_or_native_side_effects() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 1024 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let mut raw = Raw::new(&vfs, artifact.path());
    let used = space.usage().charged;
    for op in [
        ffi::SQLITE_FCNTL_SIZE_HINT,
        ffi::SQLITE_FCNTL_CHUNK_SIZE,
        ffi::SQLITE_FCNTL_FILE_POINTER,
        ffi::SQLITE_FCNTL_VFS_POINTER,
        ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
        ffi::SQLITE_FCNTL_NULL_IO,
        ffi::SQLITE_FCNTL_MMAP_SIZE,
        ffi::SQLITE_FCNTL_BEGIN_ATOMIC_WRITE,
        ffi::SQLITE_FCNTL_TEMPFILENAME,
        0x7fff_ffff,
    ] {
        let mut sentinel = 0x0123_4567_89ab_cdef_i64;
        assert_eq!(
            unsafe { control(raw.ptr(), op, (&mut sentinel as *mut i64).cast()) },
            ffi::SQLITE_NOTFOUND
        );
        assert_eq!(sentinel, 0x0123_4567_89ab_cdef_i64);
    }
    assert_eq!(space.usage().charged, used);
    assert_eq!(artifact.file().metadata().unwrap().len(), 0);
    let bytes = [0xa5u8; 4096];
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), bytes.len() as c_int, 0) },
        ffi::SQLITE_OK
    );
    let mut got = [0u8; 4096];
    assert_eq!(
        unsafe { read(raw.ptr(), got.as_mut_ptr().cast(), 4096, 0) },
        ffi::SQLITE_OK
    );
    assert_eq!(got, bytes); // NULL_IO did not close/divert the real file.
    assert_eq!(
        unsafe { characteristics(raw.ptr()) } & ffi::SQLITE_IOCAP_BATCH_ATOMIC,
        0
    );
}

#[test]
fn growth_is_charged_before_write_truncate_and_shm_and_never_released_on_failure() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 256 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let mut raw = Raw::new(&vfs, artifact.path());
    let bytes = [0xabu8; 4096];
    for (offset, n) in [(256 * 1024, 4096), (i64::MAX, 4096), (-1, 4096), (0, -1)] {
        assert_eq!(
            unsafe { write(raw.ptr(), bytes.as_ptr().cast(), n, offset) },
            ffi::SQLITE_FULL
        );
        assert_eq!(artifact.file().metadata().unwrap().len(), 0);
    }
    assert_eq!(unsafe { truncate(raw.ptr(), 256 * 1024) }, ffi::SQLITE_FULL);
    assert_eq!(unsafe { truncate(raw.ptr(), -1) }, ffi::SQLITE_FULL);
    let mut mapped = ptr::dangling_mut::<c_void>();
    assert_eq!(
        unsafe { shm_map(raw.ptr(), 1024, 32768, 1, &mut mapped) },
        ffi::SQLITE_FULL
    );
    assert!(mapped.is_null());
    assert!(!shm_path(artifact.path()).exists());
    space.inject("vfs_write");
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_IOERR_WRITE
    );
    assert_eq!(artifact.file().metadata().unwrap().len(), 2048);
    let retained = space.usage().charged;
    assert!(retained >= 8192);
    space.inject("vfs_truncate");
    assert_eq!(
        unsafe { truncate(raw.ptr(), 16 * 1024) },
        ffi::SQLITE_IOERR_TRUNCATE
    );
    assert!(space.usage().charged > retained);
    let retained = space.usage().charged;
    assert_eq!(unsafe { truncate(raw.ptr(), 0) }, ffi::SQLITE_OK);
    assert_eq!(space.usage().charged, retained);
    space.inject("vfs_sync");
    assert_eq!(
        unsafe { sync(raw.ptr(), ffi::SQLITE_SYNC_FULL) },
        ffi::SQLITE_IOERR_FSYNC
    );
    space.inject("vfs_read");
    let mut got = [255u8; 8];
    assert_eq!(
        unsafe { read(raw.ptr(), got.as_mut_ptr().cast(), 8, 0) },
        ffi::SQLITE_IOERR_READ
    );
    assert_eq!(got, [0u8; 8]);
    // Native short-read contract: zero-fill missing tail, not uninitialized data.
    got.fill(255);
    assert_eq!(
        unsafe { read(raw.ptr(), got.as_mut_ptr().cast(), 8, 0) },
        ffi::SQLITE_IOERR_SHORT_READ
    );
    assert_eq!(got, [0u8; 8]);
    space.inject("vfs_shm");
    assert_eq!(
        unsafe { shm_map(raw.ptr(), 0, 32768, 1, &mut mapped) },
        ffi::SQLITE_IOERR_SHMSIZE
    );
    assert!(mapped.is_null());
    assert!(space.usage().peak <= 256 * 1024);
}

#[test]
fn deletion_is_denied_while_any_native_handle_is_open_and_charged_until_success() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 1024 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let first = Raw::new(&vfs, artifact.path());
    let second = Raw::new(&vfs, artifact.path());
    let used = space.usage().charged;
    assert_eq!(del(&vfs, artifact.path()), ffi::SQLITE_IOERR_DELETE);
    drop(first);
    assert_eq!(del(&vfs, artifact.path()), ffi::SQLITE_IOERR_DELETE);
    assert_eq!(space.usage().charged, used);
    drop(second);
    space.inject("vfs_delete");
    assert_eq!(del(&vfs, artifact.path()), ffi::SQLITE_IOERR_DELETE);
    assert!(artifact.path().exists());
    assert_eq!(space.usage().charged, used);
    assert_eq!(del(&vfs, artifact.path()), ffi::SQLITE_OK);
    assert_eq!(space.usage().charged, 0);
}

#[test]
fn wal_shm_two_connections_share_charges_and_reject_deletion_until_unmapped() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 2 * 1024 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let connect = || {
        Connection::open_with_flags_and_vfs(
            artifact.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            vfs.name(),
        )
        .unwrap()
    };
    let first = connect();
    first
        .execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE t(x);INSERT INTO t VALUES(1);")
        .unwrap();
    let second = connect();
    assert_eq!(
        second
            .query_row::<i64, _, _>("SELECT sum(x) FROM t", [], |r| r.get(0))
            .unwrap(),
        1
    );
    let shm = shm_path(artifact.path());
    assert_eq!(del(&vfs, &shm), ffi::SQLITE_IOERR_DELETE);
    drop(first);
    assert!(shm.exists());
    assert_eq!(del(&vfs, &shm), ffi::SQLITE_IOERR_DELETE);
    assert_eq!(
        second
            .query_row::<i64, _, _>("SELECT sum(x) FROM t", [], |r| r.get(0))
            .unwrap(),
        1
    );
    drop(second);
    assert!(space.may_delete(&shm));
    assert!(space.usage().peak <= 2 * 1024 * 1024);
}

#[test]
fn native_close_failure_preserves_name_and_residual_charge_instead_of_unlinking() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 1024 * 1024).unwrap();
    let directory = space.path().to_owned();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let path = artifact.path().to_owned();
    let mut raw = Raw::new(&vfs, &path);
    space.inject("vfs_close");
    assert_eq!(unsafe { close(raw.ptr()) }, ffi::SQLITE_IOERR_CLOSE);
    drop(raw);
    assert_eq!(del(&vfs, &path), ffi::SQLITE_IOERR_DELETE);
    drop(artifact);
    assert!(path.exists());
    assert!(space.usage().charged > 0);
    drop(vfs);
    drop(space);
    assert!(path.exists());
    // The injected close did close the OS handle. Only this test's residue is
    // now explicitly deleted, with the registry released after actual deletion.
    std::fs::remove_dir_all(&directory).unwrap();
    Governor::global().forget_deleted_artifact_group(&directory);
}

#[test]
fn open_failures_boundaries_links_and_vfs_registration_lifetime() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 1024 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let registration = vfs.name.clone();
    let artifact = space.file().unwrap();
    let mut file = GuardFile {
        base: ffi::sqlite3_file {
            pMethods: ptr::null(),
        },
        handle: ptr::null_mut(),
    };
    let flags = ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_MAIN_DB;
    let attempt = |p: &Path, flags, file: &mut GuardFile| {
        let name = CString::new(p.to_str().unwrap()).unwrap();
        unsafe {
            let name = ffi::sqlite3_create_filename(
                name.as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                0,
                ptr::null_mut(),
            );
            let rc = open(
                vfs.raw.as_ref() as *const _ as *mut _,
                name,
                &mut file.base,
                flags,
                ptr::null_mut(),
            );
            assert!(file.base.pMethods.is_null());
            ffi::sqlite3_free_filename(name);
            rc
        }
    };
    space.inject("vfs_open");
    assert_eq!(
        attempt(artifact.path(), flags, &mut file),
        ffi::SQLITE_CANTOPEN
    );
    assert_eq!(
        attempt(
            artifact.path(),
            flags | ffi::SQLITE_OPEN_DELETEONCLOSE,
            &mut file
        ),
        ffi::SQLITE_CANTOPEN
    );
    let outside = tmp.path().join("outside");
    std::fs::write(&outside, b"unchanged").unwrap();
    assert_eq!(attempt(&outside, flags, &mut file), ffi::SQLITE_FULL);
    assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
    let link = space.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside, &link).unwrap();
    assert_eq!(attempt(&link, flags, &mut file), ffi::SQLITE_CANTOPEN);
    std::fs::remove_file(link).unwrap();
    let path = artifact.path().to_owned();
    let directory = space.path().to_owned();
    space.inject("vfs_open_after_native");
    assert_eq!(attempt(&path, flags, &mut file), ffi::SQLITE_CANTOPEN);
    assert!(!space.may_delete(&path));
    assert!(space.usage().charged > 0);
    drop(artifact);
    assert!(path.exists());
    assert!(!unsafe { ffi::sqlite3_vfs_find(registration.as_ptr()) }.is_null());
    drop(vfs);
    assert!(unsafe { ffi::sqlite3_vfs_find(registration.as_ptr()) }.is_null());
    drop(space);
    assert!(path.exists());
    std::fs::remove_dir_all(&directory).unwrap();
    Governor::global().forget_deleted_artifact_group(&directory);
}

#[test]
fn absence_checks_do_not_confuse_links_or_access_errors_with_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    assert!(confirmed_missing(&missing));
    let link = tmp.path().join("dangling-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&missing, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&missing, &link).unwrap();
    assert!(!confirmed_missing(&link));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("shm"), b"retained").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o0)).unwrap();
        let result = confirmed_missing(&blocked.join("shm"));
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!result);
    }
}

#[test]
fn sqlite_transaction_faults_preserve_committed_projection_and_canonical_inventory() {
    let (tmp, store) = fixture(900);
    let old = store
        .query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &Default::default(),
            tmp.path(),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let mut order = old
        .order_by_size(false, tmp.path(), 8 * 1024 * 1024)
        .unwrap();
    while !order.advance(&Default::default()).unwrap() {}
    let collect = || {
        let mut ids = Vec::new();
        let mut p = order.page(None, false).unwrap();
        loop {
            ids.extend(p.rows.iter().map(|r| r.key.entry));
            if !p.more {
                break;
            }
            p = order.page(p.next.as_ref(), false).unwrap();
        }
        ids
    };
    let expected = collect();
    let snapshot = || {
        ["inventory.db", "inventory.db-wal", "owner.lock"]
            .map(|name| std::fs::read(tmp.path().join("store").join(name)).ok())
    };
    let canonical = snapshot();
    for mode in ["DELETE", "WAL"] {
        for fault in ["vfs_write", "vfs_sync"] {
            let space = Workspace::new(&store, tmp.path(), 2 * 1024 * 1024).unwrap();
            let vfs = Vfs::new(space.clone()).unwrap();
            let artifact = space.file().unwrap();
            let db = Connection::open_with_flags_and_vfs(
                artifact.path(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                vfs.name(),
            )
            .unwrap();
            db.execute_batch(&format!("PRAGMA journal_mode={mode};PRAGMA synchronous=FULL;CREATE TABLE t(x);INSERT INTO t VALUES(7);")).unwrap();
            space.inject(fault);
            assert!(
                db.execute_batch("BEGIN;INSERT INTO t VALUES(9);COMMIT;")
                    .is_err()
            );
            // SQLite can leave the transaction active on COMMIT error.
            if !db.is_autocommit() {
                db.execute_batch("ROLLBACK;").unwrap();
            }
            assert_eq!(
                db.query_row::<i64, _, _>("SELECT sum(x) FROM t", [], |r| r.get(0))
                    .unwrap(),
                7
            );
            assert_eq!(
                db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                    .unwrap(),
                "ok"
            );
            assert_eq!(collect(), expected);
            assert_eq!(snapshot(), canonical);
            assert!(space.usage().peak <= 2 * 1024 * 1024);
        }
    }
}

#[test]
fn sqlite_interrupt_rolls_back_candidate_and_keeps_previous_snapshot() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 2 * 1024 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let mut db = Connection::open_with_flags_and_vfs(
        artifact.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        vfs.name(),
    )
    .unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE t(x);INSERT INTO t VALUES(7);")
        .unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    db.progress_handler(
        100,
        Some(move || observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 100),
    )
    .unwrap();
    assert!(db.execute_batch("INSERT INTO t WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT x FROM n;").is_err());
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    assert!(calls.load(std::sync::atomic::Ordering::Relaxed) > 100);
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT sum(x) FROM t", [], |r| r.get(0))
            .unwrap(),
        7
    );
    assert_eq!(store.entry_count(GenerationId(1)).unwrap(), 2);
}

#[cfg(windows)]
#[test]
fn windows_native_sharing_violation_keeps_charge_until_handle_released() {
    use std::os::windows::fs::OpenOptionsExt;
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 1024 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(artifact.path())
        .unwrap();
    let used = space.usage().charged;
    assert_eq!(del(&vfs, artifact.path()), ffi::SQLITE_IOERR_DELETE);
    assert_eq!(space.usage().charged, used);
    assert!(artifact.path().exists());
    drop(held);
    assert_eq!(del(&vfs, artifact.path()), ffi::SQLITE_OK);
    assert_eq!(space.usage().charged, 0);
}

#[test]
fn quota_full_rolls_back_real_sqlite_transactions_in_delete_and_wal_modes() {
    let (tmp, store) = fixture(1);
    let snapshot = || {
        ["inventory.db", "inventory.db-wal", "owner.lock"]
            .map(|name| std::fs::read(tmp.path().join("store").join(name)).ok())
    };
    let canonical = snapshot();
    for mode in ["DELETE", "WAL"] {
        let space = Workspace::new(&store, tmp.path(), 256 * 1024).unwrap();
        let directory = space.path().to_owned();
        let vfs = Vfs::new(space.clone()).unwrap();
        let artifact = space.file().unwrap();
        let db = Connection::open_with_flags_and_vfs(
            artifact.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            vfs.name(),
        )
        .unwrap();
        db.execute_batch(&format!("PRAGMA journal_mode={mode};PRAGMA synchronous=FULL;PRAGMA cache_size=8;CREATE TABLE t(x);INSERT INTO t VALUES(7);")).unwrap();
        let error = db
            .execute_batch("BEGIN;INSERT INTO t VALUES(zeroblob(524288));COMMIT;")
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DiskFull)
        );
        if !db.is_autocommit() {
            db.execute_batch("ROLLBACK;").unwrap();
        }
        assert_eq!(
            db.query_row::<(i64, i64), _, _>("SELECT count(*),sum(x) FROM t", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap(),
            (1, 7)
        );
        assert_eq!(
            db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap(),
            "ok"
        );
        assert!(space.usage().peak <= 256 * 1024);
        assert_eq!(snapshot(), canonical);
        drop(db);
        drop(artifact);
        drop(vfs);
        drop(space);
        assert!(!directory.exists());
    }
}

#[test]
fn shared_native_handles_retain_one_high_water_charge_across_shrink_and_owner_drop() {
    let (tmp, store) = fixture(1);
    let space = Workspace::new(&store, tmp.path(), 128 * 1024).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let path = artifact.path().to_owned();
    let mut first = Raw::new(&vfs, &path);
    let mut second = Raw::new(&vfs, &path);
    let bytes = [0x5au8; 8192];
    assert_eq!(
        unsafe { write(first.ptr(), bytes.as_ptr().cast(), 8192, 0) },
        ffi::SQLITE_OK
    );
    let charged = space.usage().charged;
    assert_eq!(
        unsafe { write(second.ptr(), bytes.as_ptr().cast(), 4096, 4096) },
        ffi::SQLITE_OK
    );
    assert_eq!(space.usage().charged, charged);
    assert_eq!(unsafe { truncate(first.ptr(), 0) }, ffi::SQLITE_OK);
    assert_eq!(
        unsafe { write(second.ptr(), bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_OK
    );
    assert_eq!(space.usage().charged, charged);
    assert_eq!(
        unsafe { write(first.ptr(), bytes.as_ptr().cast(), 4096, 128 * 1024) },
        ffi::SQLITE_FULL
    );
    assert_eq!(artifact.file().metadata().unwrap().len(), 4096);
    drop(artifact);
    drop(first);
    assert!(path.exists());
    assert_eq!(del(&vfs, &path), ffi::SQLITE_IOERR_DELETE);
    assert_eq!(space.usage().charged, charged);
    drop(second);
    assert_eq!(del(&vfs, &path), ffi::SQLITE_OK);
    assert_eq!(space.usage().charged, 0);
}

#[test]
fn full_residue_registry_keeps_admission_on_close_failure() {
    const CHILD: &str = "BUNNY_VFS_REGISTRY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let out=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","inventory::queries::workspace::vfs::tests::full_residue_registry_keeps_admission_on_close_failure","--nocapture"])
            .env(CHILD,"1").output().unwrap();
        assert!(
            out.status.success(),
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let (tmp, store) = fixture(1);
    let governor = Governor::global();
    let before = governor.usage().disk_reserved;
    let space = Workspace::new(&store, tmp.path(), 1024 * 1024).unwrap();
    let directory = space.path().to_owned();
    let vfs = Vfs::new(space.clone()).unwrap();
    let artifact = space.file().unwrap();
    let mut raw = Raw::new(&vfs, artifact.path());
    space.inject("vfs_close");
    assert_eq!(unsafe { close(raw.ptr()) }, ffi::SQLITE_IOERR_CLOSE);
    drop(raw);
    drop(artifact);
    drop(vfs);
    let mut full = false;
    for i in 0..=1024 {
        if !governor.observe_artifact_group(
            &tmp.path().join(format!("registered-{i}")),
            16 * 1024 * 1024 * 1024,
            0,
        ) {
            full = true;
            break;
        }
    }
    assert!(full);
    drop(space);
    assert_eq!(governor.usage().disk_reserved, before + 1024 * 1024);
    assert!(directory.exists());
    std::fs::remove_dir_all(directory).unwrap();
    // This isolated child exits: the deliberately retained reservation cannot
    // pollute later tests or silently admit more scratch growth.
}

#[test]
fn persistent_ledger_refuses_growth_and_foreign_deletion_before_native_io() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let space = Workspace::persistent(&directory, 128 * 1024, None).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let db = directory.join("inventory.db");
    let mut raw = Raw::new(&vfs, &db);
    let bytes = [42u8; 4096];
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 256 * 1024) },
        ffi::SQLITE_FULL
    );
    assert_eq!(std::fs::metadata(&db).unwrap().len(), 0);
    assert_eq!(unsafe { truncate(raw.ptr(), 256 * 1024) }, ffi::SQLITE_FULL);
    assert_eq!(std::fs::metadata(&db).unwrap().len(), 0);
    let mut mapped = ptr::null_mut();
    assert_eq!(
        unsafe { shm_map(raw.ptr(), 32, 32768, 1, &mut mapped) },
        ffi::SQLITE_FULL
    );
    assert!(mapped.is_null());
    assert!(!directory.join("inventory.db-shm").exists());
    let governor = Governor::global();
    let free = sortsize_core::resources::native_free_disk(&directory).unwrap();
    governor.observe_disk(512 * 1024 * 1024, 0);
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_FULL
    );
    assert_eq!(std::fs::metadata(&db).unwrap().len(), 0);
    governor.observe_disk(free, 0);
    let foreign = directory.join("foreign-user-file");
    std::fs::write(&foreign, b"preserve").unwrap();
    assert_eq!(del(&vfs, &foreign), ffi::SQLITE_IOERR_DELETE);
    assert_eq!(std::fs::read(&foreign).unwrap(), b"preserve");
    std::fs::remove_file(foreign).unwrap();
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_OK
    );
    drop(raw);
    drop(vfs);
    drop(space);
    assert_eq!(std::fs::metadata(db).unwrap().len(), 4096);
}
#[test]
fn persistent_close_failure_keeps_global_floor_sticky_without_deleting_data() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let space = Workspace::persistent(&directory, 128 * 1024, None).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let db = directory.join("inventory.db");
    let mut raw = Raw::new(&vfs, &db);
    let bytes = [42u8; 4096];
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_OK
    );
    let charged = space.usage().charged;
    space.inject("vfs_close");
    drop(raw);
    drop(vfs);
    drop(space);
    let governor = Governor::global();
    let free = sortsize_core::resources::native_free_disk(&directory).unwrap();
    governor.observe_artifact_group(&directory, free, 0);
    governor.observe_disk(free, 0);
    governor.forget_deleted_artifact_group(&directory);
    assert!(governor.usage().disk_used >= charged);
    assert!(governor.guard_artifact_group(&directory, free, 0).is_none());
    assert_eq!(std::fs::metadata(db).unwrap().len(), 4096);
}

#[test]
fn canonical_unnamed_subjournal_has_owned_sqlite_filename_and_precharged_growth() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let space = Workspace::persistent(&directory, 128 * 1024, None).unwrap();
    let vfs = Vfs::new(space.clone()).unwrap();
    let file = Box::new(GuardFile {
        base: ffi::sqlite3_file {
            pMethods: ptr::null(),
        },
        handle: ptr::null_mut(),
    });
    let mut raw = Raw {
        file,
        _vfs: &vfs,
        filename: ptr::null(),
    };
    let flags = ffi::SQLITE_OPEN_SUBJOURNAL
        | ffi::SQLITE_OPEN_READWRITE
        | ffi::SQLITE_OPEN_CREATE
        | ffi::SQLITE_OPEN_EXCLUSIVE
        | ffi::SQLITE_OPEN_DELETEONCLOSE;
    assert_eq!(
        unsafe {
            open(
                vfs.raw.as_ref() as *const _ as *mut _,
                ptr::null(),
                raw.ptr(),
                flags,
                ptr::null_mut(),
            )
        },
        ffi::SQLITE_OK
    );
    let path = unsafe { handle(raw.ptr()).path.clone() };
    assert_eq!(path.parent(), Some(directory.as_path()));
    assert!(
        crate::inventory::generation_a::canonical::is_temporary_name(path.file_name().unwrap())
    );
    let filename = unsafe { handle(raw.ptr()).owned_filename.as_ref().unwrap().0 };
    // open()'s temporary CString has already been dropped. Parent URI helpers
    // must still be able to read SQLite's allocated filename header safely.
    assert_eq!(
        unsafe { ffi::sqlite3_uri_boolean(filename, c"exclusive".as_ptr(), 0) },
        0
    );
    assert!(!unsafe { ffi::sqlite3_filename_database(filename) }.is_null());
    let bytes = [5u8; 4096];
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 0) },
        ffi::SQLITE_OK
    );
    let mut length = 0;
    assert_eq!(unsafe { size(raw.ptr(), &mut length) }, ffi::SQLITE_OK);
    assert_eq!(length, 4096);
    let before = space.usage().charged;
    assert_eq!(
        unsafe { write(raw.ptr(), bytes.as_ptr().cast(), 4096, 256 * 1024) },
        ffi::SQLITE_FULL
    );
    assert_eq!(space.usage().charged, before);
    assert_eq!(unsafe { size(raw.ptr(), &mut length) }, ffi::SQLITE_OK);
    assert_eq!(length, 4096);
    drop(raw);
    assert!(!path.exists());
    assert_eq!(space.usage().charged, 0);
}
