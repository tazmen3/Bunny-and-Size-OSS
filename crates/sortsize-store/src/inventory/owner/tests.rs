use super::*;

fn group(parent: &Path, name: &str) -> PathBuf {
    let p = parent.join(name);
    std::fs::create_dir(&p).unwrap();
    std::fs::write(parent.join("IDENTITY.json"), b"{}").unwrap();
    p
}
#[test]
fn opening_is_anchor_readonly_and_both_aliases_share_the_native_lock() {
    let t = tempfile::tempdir().unwrap();
    let p = group(t.path(), "group");
    let initial = acquire(&p, false, true).unwrap();
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    install(&p, &initial).unwrap();
    assert!(acquire(&p, true, false).is_err());
    drop(initial);
    let first = acquire(&p, true, false).unwrap();
    let second = acquire(&p, true, false).unwrap();
    assert!(acquire(&p, false, false).is_err());
    let inside = open_existing(&p.join("owner.lock")).unwrap();
    assert!(inside.try_lock().is_err());
    drop(inside);
    drop(first);
    drop(second);
    let external = acquire(&p, false, false).unwrap();
    assert!(acquire(&p, true, false).is_err());
    drop(external);
    let count = std::fs::read_dir(t.path()).unwrap().count();
    drop(acquire(&p, true, false).unwrap());
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), count);
}
#[test]
fn external_lease_survives_write_through_directory_move_and_fences_the_new_name() {
    let t = tempfile::tempdir().unwrap();
    let p = group(t.path(), "old");
    let initial = acquire(&p, false, true).unwrap();
    let id = identity(&initial).unwrap();
    install(&p, &initial).unwrap();
    drop(initial);
    let lease = acquire(&p, false, false).unwrap();
    let moved = t.path().join("quarantine");
    crate::inventory::generation_a::publish_directory(&p, &moved).unwrap();
    assert_eq!(
        identity(&lease).unwrap(),
        identity_at(&moved.join("owner.lock")).unwrap()
    );
    assert!(acquire(&moved, true, false).is_err());
    assert!(acquire(&moved, false, false).is_err());
    // Recreating the original pathname has a different physical owner and
    // cannot inherit, replace or clear the quarantined generation's lease.
    std::fs::create_dir(&p).unwrap();
    let replacement = acquire(&p, false, true).unwrap();
    assert_ne!(identity(&replacement).unwrap(), id);
    assert!(acquire(&moved, true, false).is_err());
    drop(replacement);
    std::fs::remove_file(moved.join("owner.lock")).unwrap();
    std::fs::remove_dir(&moved).unwrap();
    remove_after_group(&moved, &lease).unwrap();
    assert!(!anchor_path(&moved, id).unwrap().exists());
}
#[test]
fn substituted_anchor_or_owner_and_anchor_symlinks_are_preserved() {
    let t = tempfile::tempdir().unwrap();
    let p = group(t.path(), "group");
    let initial = acquire(&p, false, true).unwrap();
    install(&p, &initial).unwrap();
    let anchor = anchor_path(&p, identity(&initial).unwrap()).unwrap();
    drop(initial);
    std::fs::remove_file(&anchor).unwrap();
    std::fs::write(&anchor, []).unwrap();
    assert!(acquire(&p, true, false).is_err());
    assert!(validate_anchor(&anchor).is_err());
    std::fs::remove_file(&anchor).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(p.join("owner.lock"), &anchor).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(p.join("owner.lock"), &anchor).unwrap();
    assert!(acquire(&p, false, false).is_err());
    assert!(std::fs::symlink_metadata(&anchor).unwrap().is_symlink());
    std::fs::remove_file(&anchor).unwrap();
    let initial = acquire(&p, false, false).unwrap();
    install(&p, &initial).unwrap();
    drop(initial);
    // Replace owner.lock, leaving the old anchor intact. It is never selected
    // for the new owner: physical identity, not a generation filename, is key.
    std::fs::remove_file(p.join("owner.lock")).unwrap();
    std::fs::write(p.join("owner.lock"), []).unwrap();
    let current = acquire(&p, false, false).unwrap();
    assert_ne!(identity(&current).unwrap(), identity_at(&anchor).unwrap());
    require_same(&p.join("owner.lock"), identity_at(&anchor).unwrap()).unwrap_err();
}
#[test]
fn anchor_creation_is_not_a_side_effect_of_missing_or_future_style_open() {
    let t = tempfile::tempdir().unwrap();
    let p = group(t.path(), "future");
    assert!(acquire(&p, true, false).is_err());
    assert!(acquire(&p, false, false).is_err());
    assert_eq!(std::fs::read_dir(&p).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    std::fs::write(p.join("owner.lock"), []).unwrap();
    drop(acquire(&p, true, false).unwrap());
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    assert!(!is_anchor_name(std::ffi::OsStr::new(
        ".owner-anchor-v1-future.lock"
    )));
}

#[test]
fn historical_single_link_owners_do_not_require_the_new_native_id_api() {
    let t = tempfile::tempdir().unwrap();
    let p = group(t.path(), "legacy");
    std::fs::write(p.join("owner.lock"), []).unwrap();
    NATIVE_ID_UNAVAILABLE.with(|flag| flag.set(true));
    let opened = acquire(&p, true, false);
    NATIVE_ID_UNAVAILABLE.with(|flag| flag.set(false));
    drop(opened.unwrap());
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    let initial = acquire(&p, false, false).unwrap();
    install(&p, &initial).unwrap();
    drop(initial);
    NATIVE_ID_UNAVAILABLE.with(|flag| flag.set(true));
    let anchored = acquire(&p, true, false);
    NATIVE_ID_UNAVAILABLE.with(|flag| flag.set(false));
    assert!(anchored.is_err());
    // Restoring the identity API needs no repair and ordinary locking works.
    drop(acquire(&p, true, false).unwrap());
}

#[test]
fn future_corrupt_and_nonquiescent_ownerless_copies_are_preserved_before_rw_open() {
    use crate::Durability;
    use crate::inventory::Store;
    use sortsize_core::inventory_contract::{InventoryId, SourceId};
    use std::collections::BTreeMap;
    fn snapshot(p: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
        std::fs::read_dir(p)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name(), std::fs::read(e.path()).unwrap())
            })
            .collect()
    }
    for case in ["future", "corrupt", "wal", "shm", "journal", "compatible"] {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("copy");
        let store = Store::create(&p, InventoryId(99), SourceId(77), Durability::Retained).unwrap();
        if case == "future" {
            store.db.pragma_update(None, "user_version", 499).unwrap();
        }
        drop(store);
        std::fs::remove_file(p.join("owner.lock")).unwrap();
        if case == "corrupt" {
            std::fs::write(p.join("inventory.db"), b"broken SQLite header").unwrap();
        }
        if matches!(case, "wal" | "shm" | "journal") {
            std::fs::write(
                p.join(format!("inventory.db-{case}")),
                b"retained native recovery payload",
            )
            .unwrap();
        }
        let before = snapshot(&p);
        let reopened = Store::reopen(&p, Durability::Retained);
        if case == "compatible" {
            drop(reopened.unwrap());
            assert!(std::fs::read(p.join("owner.lock")).unwrap().is_empty());
            assert_eq!(
                std::fs::read(p.join("inventory.db")).unwrap(),
                before[std::ffi::OsStr::new("inventory.db")]
            );
        } else {
            assert!(reopened.is_err(), "{case}");
            assert_eq!(snapshot(&p), before, "{case}");
        }
    }
}

#[test]
fn compatible_header_corrupt_btree_keeps_db_bytes_and_the_explicit_new_owner() {
    use crate::Durability;
    use crate::inventory::Store;
    use sortsize_core::inventory_contract::{InventoryId, SourceId};
    use std::io::{Seek, SeekFrom, Write};
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("copy");
    let store = Store::create(&path, InventoryId(51), SourceId(19), Durability::Retained).unwrap();
    store.db.execute_batch("CREATE TABLE unused_corruption(payload BLOB); INSERT INTO unused_corruption VALUES(zeroblob(8192));").unwrap();
    let page: u32 = store
        .db
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='unused_corruption'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let page_size: u32 = store
        .db
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .unwrap();
    store
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    drop(store);
    std::fs::remove_file(path.join("owner.lock")).unwrap();
    let db_path = path.join("inventory.db");
    let mut file = OpenOptions::new().write(true).open(&db_path).unwrap();
    file.seek(SeekFrom::Start(
        (u64::from(page) - 1) * u64::from(page_size),
    ))
    .unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let before = std::fs::read(&db_path).unwrap();
    assert!(Store::reopen(&path, Durability::Retained).is_err());
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
    assert!(std::fs::read(path.join("owner.lock")).unwrap().is_empty());
}
