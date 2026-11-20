use sortsize_core::{
    acquisition::{Attachment, Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{Durability, inventory::Store, scheduler::Session};
use std::{fs, path::Path};

fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(951),
        source: SourceId(952),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn reference(root: &Path, directory: &Path) -> Store {
    let c = context();
    let session = Session::new(
        Store::create(directory, c.inventory, c.source, Durability::Retained).unwrap(),
        c,
        Guard::open(root, Policy::default()).unwrap(),
    )
    .unwrap();
    drop(session);
    Store::reopen(directory, Durability::Retained).unwrap()
}

#[test]
fn source_attachment_known_positive_and_replaced_root_keep_strict_matches() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    fs::create_dir(&root).unwrap();
    let store = reference(&root, &t.path().join("store"));
    let guard = Guard::open(&root, Policy::default()).unwrap();
    assert!(
        guard.evidence().1.created.is_some(),
        "native fixture needs a birth time"
    );
    assert_eq!(
        Session::source_attachment(&store, GenerationId(1), &guard).unwrap(),
        Attachment::Compatible
    );
    assert!(Session::source_matches(&store, GenerationId(1), &guard).unwrap());
    fs::rename(&root, t.path().join("previous-source")).unwrap();
    fs::create_dir(&root).unwrap();
    let replacement = Guard::open(&root, Policy::default()).unwrap();
    assert_eq!(
        Session::source_attachment(&store, GenerationId(1), &replacement).unwrap(),
        Attachment::Different
    );
    assert!(!Session::source_matches(&store, GenerationId(1), &replacement).unwrap());
}

#[test]
fn source_attachment_absent_historical_evidence_is_uncertain_without_creating_it() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    fs::create_dir(&root).unwrap();
    let c = context();
    let mut store = Store::create(
        &t.path().join("store"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    store.create_generation(c).unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    assert_eq!(
        Session::source_attachment(&store, c.generation, &guard).unwrap(),
        Attachment::Uncertain
    );
    assert!(!Session::source_matches(&store, c.generation, &guard).unwrap());
    let db = rusqlite::Connection::open_with_flags(
        store.directory().join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM recovery_guard", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn source_attachment_legacy_missing_table_stays_uncertain_without_migration() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    let directory = t.path().join("store");
    fs::create_dir(&root).unwrap();
    drop(reference(&root, &directory));
    {
        let db = rusqlite::Connection::open(directory.join("inventory.db")).unwrap();
        db.execute_batch("DROP TABLE recovery_guard; PRAGMA user_version=1;")
            .unwrap();
    }
    let store = Store::reopen(&directory, Durability::Retained).unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    assert_eq!(
        Session::source_attachment(&store, GenerationId(1), &guard).unwrap(),
        Attachment::Uncertain
    );
    assert!(!Session::source_matches(&store, GenerationId(1), &guard).unwrap());
    let db = rusqlite::Connection::open_with_flags(
        directory.join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='recovery_guard'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn source_attachment_oversized_or_malformed_evidence_is_uncertain() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    let directory = t.path().join("store");
    fs::create_dir(&root).unwrap();
    drop(reference(&root, &directory));
    let guard = Guard::open(&root, Policy::default()).unwrap();
    for sql in [
        "UPDATE recovery_guard SET proof=zeroblob(131073)",
        "UPDATE recovery_guard SET proof=x'01'",
    ] {
        {
            let db = rusqlite::Connection::open(directory.join("inventory.db")).unwrap();
            db.execute_batch(sql).unwrap();
        }
        let store = Store::reopen(&directory, Durability::Retained).unwrap();
        assert_eq!(
            Session::source_attachment(&store, GenerationId(1), &guard).unwrap(),
            Attachment::Uncertain
        );
        assert!(!Session::source_matches(&store, GenerationId(1), &guard).unwrap());
    }
}
