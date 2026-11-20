use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, observation, reconcile};
fn measured(s: &mut Store, g: u64, complete: bool) {
    s.create_generation(context_id(g)).unwrap();
    let a = attempt_id(g, 0, 1);
    s.begin_attempt(a).unwrap();
    batch(
        s,
        a,
        0,
        Some(if complete {
            ListingEnd::Exhausted
        } else {
            ListingEnd::Incomplete
        }),
        &[observation(10 + g, Kind::File)],
    )
    .unwrap();
    if complete {
        reconcile(s, a);
    }
    fold(s, g);
    s.seal(context_id(g)).unwrap();
    #[cfg(unix)]
    let root = Path::new("/offline/never-probe");
    #[cfg(windows)]
    let root = Path::new(r"\\offline.invalid\share\never-probe");
    s.record_measurement(GenerationId(g), root, 100, 200 + g as i64)
        .unwrap();
}
fn fixture() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let s = Store::create(
        &t.path().join("store"),
        InventoryId(u128::MAX),
        SourceId(9),
        Durability::Reconstructible,
    )
    .unwrap();
    (t, s)
}
#[test]
fn retention_offline_copy_full_expiry_and_forgetting_are_local() {
    let (t, mut s) = fixture();
    assert!(!s.retention().unwrap().enabled);
    measured(&mut s, 0, true);
    let parent = t.path().join("retained");
    private_directory(&parent).unwrap();
    let mut copy = s.retain_copy(&parent.join("owned"), 30).unwrap();
    assert_eq!(
        copy.db
            .pragma_query_value::<i64, _>(None, "synchronous", |r| r.get(0))
            .unwrap(),
        2
    );
    assert_eq!(
        copy.measurement(GenerationId(0)).unwrap(),
        s.measurement(GenerationId(0)).unwrap()
    );
    let lease = copy.lease(GenerationId(0)).unwrap();
    assert_eq!(lease.measurement.measured, 200);
    assert_eq!(lease.measurement.logical, Some(10));
    drop(lease);
    assert!(!copy.expired(200 + 30 * 86400 - 1).unwrap());
    assert!(copy.expired(200 + 30 * 86400).unwrap());
    // Reads did not refresh the clock; disable is immediate and visible.
    assert!(copy.set_retention(false, 30).unwrap().pending_purge);
    assert!(copy.expired(201).unwrap());
    let path = copy.directory().to_owned();
    copy.forget().unwrap();
    assert!(!path.join("inventory.db").exists());
    assert!(path.join("owner.lock").exists());
    assert_eq!(s.entry_count(GenerationId(0)).unwrap(), 2);
}
#[test]
fn protected_complete_and_latest_partial_survive_third_and_retirement() {
    let (_t, mut s) = fixture();
    measured(&mut s, 0, true);
    measured(&mut s, 1, false);
    s.create_generation(context_id(2)).unwrap();
    assert!(s.create_generation(context_id(3)).is_err());
    assert!(s.retire_generation(GenerationId(0)).is_err());
    assert!(s.retire_generation(GenerationId(1)).is_err());
    s.retire_generation(GenerationId(2)).unwrap();
    measured(&mut s, 2, true);
    s.retire_generation(GenerationId(0)).unwrap();
    assert_eq!(s.measurements().unwrap().len(), 2);
    assert_eq!(
        s.db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap(),
        "ok"
    );
}
#[test]
fn future_semantics_and_unknown_artifacts_are_preserved() {
    let (t, mut s) = fixture();
    measured(&mut s, 0, true);
    s.db.execute("UPDATE retention SET semantic=999", [])
        .unwrap();
    assert!(s.set_retention(true, 30).is_err());
    assert!(s.forget().is_err());
    assert!(t.path().join("store/inventory.db").exists());
    assert!(Store::reopen(&t.path().join("store"), Durability::Retained).is_err());
    let (t, s) = fixture();
    std::fs::write(s.directory().join("foreign.txt"), "keep").unwrap();
    assert!(s.forget().is_err());
    assert!(t.path().join("store/foreign.txt").exists());
}
#[test]
fn interrupted_additive_migration_rolls_back_and_copy_stage_is_not_published() {
    let (t, mut s) = fixture();
    {
        let tx = s.db.transaction().unwrap();
        tx.execute_batch(MIGRATION).unwrap();
    }
    assert_eq!(
        s.db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        2
    );
    measured(&mut s, 0, true);
    let parent = t.path().join("retained");
    private_directory(&parent).unwrap();
    let stage = parent.join(format!(".stage-{:032x}", u128::MAX));
    private_directory(&stage).unwrap();
    std::fs::write(stage.join("inventory.db"), "incomplete old copy").unwrap();
    assert!(s.retain_copy(&parent.join("published"), 30).is_err());
    assert!(!parent.join("published").exists());
    assert_eq!(
        std::fs::read(stage.join("inventory.db")).unwrap(),
        b"incomplete old copy"
    );
}
#[test]
fn corrupt_group_is_retained_and_native_owner_is_not_stolen() {
    let (t, mut s) = fixture();
    measured(&mut s, 0, true);
    assert!(Store::reopen(s.directory(), Durability::Retained).is_err());
    let path = s.directory().to_owned();
    drop(s);
    std::fs::write(path.join("inventory.db"), b"corrupt!").unwrap();
    let before = std::fs::read(path.join("inventory.db")).unwrap();
    assert!(Store::reopen(&path, Durability::Retained).is_err());
    assert_eq!(
        std::fs::read(t.path().join("store/inventory.db")).unwrap(),
        before
    );
}
#[cfg(unix)]
#[test]
fn private_directory_and_links_are_enforced() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (t, mut s) = fixture();
    measured(&mut s, 0, true);
    let parent = t.path().join("retained");
    private_directory(&parent).unwrap();
    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    symlink(
        t.path().join("source"),
        s.directory().join("inventory.db-wal-evil"),
    )
    .unwrap();
    assert!(s.retain_copy(&parent.join("published"), 30).is_err());
}

pub(super) fn copy_hook(point: &str) {
    if std::env::var("BUNNY_COPY_KILL_POINT").ok().as_deref() == Some(point) {
        std::fs::write(std::env::var_os("BUNNY_COPY_MARKER").unwrap(), point).unwrap();
        loop {
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
}
#[test]
#[ignore = "helper killed by the parent at an actual copy boundary"]
fn copy_process_child() {
    let root = std::path::PathBuf::from(std::env::var_os("BUNNY_COPY_ROOT").unwrap());
    let mut s = Store::create(
        &root.join("store"),
        InventoryId(u128::MAX),
        SourceId(9),
        Durability::Retained,
    )
    .unwrap();
    measured(&mut s, 0, true);
    let cache = root.join("cache");
    private_directory(&cache).unwrap();
    let _ = s.retain_copy(&cache.join("inv-published"), 30).unwrap();
}
#[test]
fn killed_copy_never_publishes_partial_and_complete_stage_recovers() {
    for point in ["before_backup", "before_publish"] {
        let t = tempfile::tempdir().unwrap();
        let marker = t.path().join("marker");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "inventory::lifecycle::tests::copy_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("BUNNY_COPY_KILL_POINT", point)
            .env("BUNNY_COPY_ROOT", t.path())
            .env("BUNNY_COPY_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !marker.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "child failed before copy boundary"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "copy boundary timeout"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        let cache = t.path().join("cache");
        let destination = cache.join("inv-published");
        assert!(!destination.exists());
        let original = Store::reopen(&t.path().join("store"), Durability::Retained).unwrap();
        assert_eq!(
            original.measurement(GenerationId(0)).unwrap().logical,
            Some(10)
        );
        drop(original);
        let recovered = Store::recover_copy(&cache, InventoryId(u128::MAX), &destination);
        if point == "before_backup" {
            assert!(recovered.is_err());
            assert!(!destination.exists());
        } else {
            let recovered = recovered.unwrap();
            assert_eq!(
                recovered.measurement(GenerationId(0)).unwrap().logical,
                Some(10)
            );
        }
        eprintln!(
            "process killed at {point}: source reference intact, publication/recovery qualified"
        );
    }
}
#[test]
#[ignore = "separate-process native lock probe, invoked twice by parent"]
fn owner_process_child() {
    let path = std::path::PathBuf::from(std::env::var_os("BUNNY_OWNER_ROOT").unwrap());
    let expected = std::env::var("BUNNY_OWNER_OPEN").unwrap() == "yes";
    assert_eq!(Store::reopen(&path, Durability::Retained).is_ok(), expected);
}
#[test]
fn independent_process_cannot_steal_owner_and_can_open_after_release() {
    let (t, s) = fixture();
    let path = s.directory().to_owned();
    let probe = |open| {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "inventory::lifecycle::tests::owner_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("BUNNY_OWNER_ROOT", &path)
            .env("BUNNY_OWNER_OPEN", open)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    };
    probe("no");
    drop(s);
    probe("yes");
    assert!(t.path().join("store/inventory.db").exists());
}

#[test]
#[ignore = "malformed DB plus live WAL fixture; parent terminates the owner"]
fn corruption_process_child() {
    use std::io::{Seek, SeekFrom, Write};
    let root = std::path::PathBuf::from(std::env::var_os("BUNNY_CORRUPT_ROOT").unwrap());
    let cache = root.join("cache");
    private_directory(&cache).unwrap();
    let mut s = Store::create(
        &cache.join("inv-corrupt"),
        InventoryId(u128::MAX),
        SourceId(9),
        Durability::Retained,
    )
    .unwrap();
    measured(&mut s, 0, true);
    s.enable_catalogue().unwrap();
    s.db.execute_batch("PRAGMA wal_autocheckpoint=0;PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    s.set_retention(true, 30).unwrap();
    let page: i64 =
        s.db.query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='entries'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let size: i64 =
        s.db.pragma_query_value(None, "page_size", |r| r.get(0))
            .unwrap();
    let mut f = OpenOptions::new()
        .write(true)
        .open(s.directory().join("inventory.db"))
        .unwrap();
    f.seek(SeekFrom::Start(((page - 1) * size) as u64)).unwrap();
    f.write_all(&[0]).unwrap();
    f.sync_all().unwrap();
    std::fs::write(root.join("marker"), b"corrupt base, committed WAL").unwrap();
    loop {
        std::thread::park_timeout(Duration::from_millis(20));
    }
}
#[test]
fn corrupt_base_and_wal_are_preserved_before_any_rw_recovery() {
    let t = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inventory::lifecycle::tests::corruption_process_child",
            "--ignored",
            "--nocapture",
        ])
        .env("BUNNY_CORRUPT_ROOT", t.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !t.path().join("marker").exists() {
        assert!(child.try_wait().unwrap().is_none());
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let path = t.path().join("cache/inv-corrupt");
    let base = std::fs::read(path.join("inventory.db")).unwrap();
    let wal = std::fs::read(path.join("inventory.db-wal")).unwrap();
    assert!(!wal.is_empty());
    assert!(Store::reopen(&path, Durability::Retained).is_err());
    assert_eq!(std::fs::read(path.join("inventory.db")).unwrap(), base);
    assert_eq!(std::fs::read(path.join("inventory.db-wal")).unwrap(), wal);
    assert!(account_root(&t.path().join("cache")).unwrap() >= (base.len() + wal.len()) as u64);
}

#[test]
fn full_open_check_rejects_valid_btrees_with_mismatched_secondary_indexes() {
    let (t, mut s) = fixture();
    measured(&mut s, 0, true);
    let path = s.directory().join("inventory.db");
    drop(s);
    let db = Connection::open(&path).unwrap();
    let parent: i64 = db
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='entries_parent_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let size: i64 = db
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='entries_size'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute_batch("PRAGMA writable_schema=ON;").unwrap();
    db.execute("UPDATE sqlite_schema SET rootpage=CASE name WHEN 'entries_parent_id' THEN ?1 ELSE ?2 END WHERE name IN ('entries_parent_id','entries_size')",params![size,parent]).unwrap();
    drop(db);
    let check =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        check
            .query_row::<String, _, _>("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap(),
        "ok"
    );
    assert_ne!(
        check
            .query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap(),
        "ok"
    );
    drop(check);
    let before = std::fs::read(&path).unwrap();
    assert!(Store::reopen(&t.path().join("store"), Durability::Retained).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
