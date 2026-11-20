use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, observation, reconcile};
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("inventory");
    let c = context_id(0);
    let mut s = Store::create(&path, c.inventory, c.source, Durability::Retained).unwrap();
    s.create_generation(c).unwrap();
    s.begin_attempt(attempt_id(0, 0, 1)).unwrap();
    let mut o = observation(9, Kind::File);
    #[cfg(windows)]
    {
        o.name = NativeName::Windows("native-file".encode_utf16().collect());
    }
    #[cfg(not(windows))]
    {
        o.name = NativeName::Posix(b"native-file".to_vec());
    }
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        0,
        Some(ListingEnd::Exhausted),
        &[o],
    )
    .unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    fold(&mut s, 0);
    s.seal(c).unwrap();
    #[cfg(windows)]
    let root = Path::new(r"Z:\offline-preview");
    #[cfg(not(windows))]
    let root = Path::new("/offline-preview");
    s.record_measurement(GenerationId(0), root, 100, 200)
        .unwrap();
    drop(s);
    (t, path)
}
fn artifacts(path: &Path) -> Vec<(String, Vec<u8>)> {
    ["inventory.db", "inventory.db-wal"]
        .into_iter()
        .filter_map(|n| {
            std::fs::read(path.join(n))
                .ok()
                .filter(|b| !b.is_empty())
                .map(|b| (n.to_owned(), b))
        })
        .collect()
}
#[test]
fn unverified_capability_is_bounded_and_only_same_snapshot_can_be_certified() {
    let (_t, path) = fixture();
    let before = artifacts(&path);
    let p = Unverified::open(&path, GenerationId(0)).unwrap();
    assert!(p.store.require_integrity().is_err());
    assert!(Store::inspect(&path).is_err());
    let prefix = p.prefix(&Default::default()).unwrap();
    assert_eq!(prefix.rows.len(), 1);
    assert!(prefix.accounted_bytes <= 512 * 1024);
    assert_eq!(prefix.rows[0].logical, Some(9));
    assert_eq!(prefix.saved.measured, Some(200));
    let s = p.verify(&Default::default()).unwrap();
    assert!(s.require_integrity().is_ok());
    assert!(!s.db.is_autocommit());
    assert!(s.db.is_readonly("main").unwrap());
    assert_eq!(
        s.db.query_row("PRAGMA mmap_size", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let mut fold = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut fold).unwrap() {}
    assert_eq!(
        s.folder_measure(GenerationId(0), EntryId(0), &fold)
            .unwrap()
            .unwrap()
            .logical,
        Some(9)
    );
    assert!(s.db.execute("DELETE FROM retention", []).is_err());
    drop(s);
    assert!(artifacts(&path) == before);
}
#[test]
fn cancellation_closes_snapshot_without_rw_and_allows_retry() {
    let (_t, path) = fixture();
    let before = artifacts(&path);
    let p = Unverified::open(&path, GenerationId(0)).unwrap();
    let c = CancellationToken::default();
    c.cancel();
    assert!(p.verify(&c).is_err());
    assert!(artifacts(&path) == before);
    assert!(
        Unverified::open(&path, GenerationId(0))
            .unwrap()
            .verify(&Default::default())
            .is_ok()
    );
}
#[test]
fn corrupt_index_invalidates_previously_readable_preview_and_preserves_bytes() {
    let (_t, path) = fixture();
    let db = Connection::open(path.join("inventory.db")).unwrap();
    db.execute_batch("PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql,'entries(generation,logical,id)','entries(generation,allocated,id)') WHERE name='entries_size'; PRAGMA writable_schema=OFF;").unwrap();
    drop(db);
    let before = artifacts(&path);
    let p = Unverified::open(&path, GenerationId(0)).unwrap();
    assert_eq!(p.prefix(&Default::default()).unwrap().rows.len(), 1);
    assert!(p.verify(&Default::default()).is_err());
    assert!(artifacts(&path) == before);
    assert!(Store::reopen(&path, Durability::Retained).is_err());
    assert!(artifacts(&path) == before);
}
#[test]
fn certification_keeps_the_preview_snapshot_even_with_an_external_wal_writer() {
    let (_t, path) = fixture();
    let p = Unverified::open(&path, GenerationId(0)).unwrap();
    let db = Connection::open(path.join("inventory.db")).unwrap();
    db.execute("UPDATE retention SET days=31", []).unwrap();
    drop(db);
    let s = p.verify(&Default::default()).unwrap();
    assert_eq!(s.retention().unwrap().days, 30);
    assert_eq!(
        s.db.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(s);
    assert_eq!(
        Store::reopen(&path, Durability::Retained)
            .unwrap()
            .retention()
            .unwrap()
            .days,
        31
    );
}
#[test]
fn failed_projection_savepoint_rolls_back_and_keeps_the_read_snapshot() {
    let (_t, path) = fixture();
    let s = Unverified::open(&path, GenerationId(0))
        .unwrap()
        .verify(&Default::default())
        .unwrap();
    s.db.execute_batch("CREATE TEMP TABLE marker(n INTEGER);")
        .unwrap();
    {
        let tx = ProjectionStep::begin(&s.db).unwrap();
        tx.execute("INSERT INTO temp.marker VALUES(1)", []).unwrap();
    }
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM temp.marker", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(!s.db.is_autocommit());
    assert!(s.require_integrity().is_ok());
}
#[test]
fn real_publication_pressure_refuses_prefix_and_retry_preserves_certificate_boundary() {
    let (_t, path) = fixture();
    let p = Unverified::open(&path, GenerationId(0)).unwrap();
    let governor = Governor::global();
    let usage = governor.usage();
    let cat = Category::Publications;
    let spare =
        usage.limits.categories[cat as usize].saturating_sub(usage.categories[cat as usize]);
    let pressure = governor.reserve(cat, spare, None).unwrap();
    assert!(p.prefix(&Default::default()).is_err());
    assert!(p.store.require_integrity().is_err());
    drop(pressure);
    assert_eq!(p.prefix(&Default::default()).unwrap().rows.len(), 1);
    assert!(p.verify(&Default::default()).is_ok());
}

#[test]
fn unverified_connection_bounds_oversized_values_before_publication() {
    let (_t, path) = fixture();
    let p = Unverified::open(&path, GenerationId(0)).unwrap();
    assert_eq!(
        p.store
            .db
            .limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH)
            .unwrap(),
        512 * 1024
    );
    assert!(
        p.store
            .db
            .query_row("SELECT zeroblob(524289)", [], |r| r.get::<_, Vec<u8>>(0))
            .is_err()
    );
    assert!(p.store.require_integrity().is_err());
    assert_eq!(p.prefix(&Default::default()).unwrap().rows.len(), 1);
}
#[test]
fn original_request_cancellation_cannot_be_replaced_to_promote_an_old_preview() {
    let (_t, path) = fixture();
    let stop = CancellationToken::default();
    let p = Unverified::open_cancellable(&path, GenerationId(0), &stop).unwrap();
    assert_eq!(p.prefix(&Default::default()).unwrap().rows.len(), 1);
    stop.cancel();
    assert!(p.verify(&Default::default()).is_err());
    assert!(Unverified::open_cancellable(&path, GenerationId(0), &stop).is_err());
    assert!(
        Unverified::open(&path, GenerationId(0))
            .unwrap()
            .verify(&Default::default())
            .is_ok()
    );
}
#[test]
fn readable_noncanonical_views_are_refused_before_loading_their_rows() {
    let (_t, path) = fixture();
    let db = Connection::open(path.join("inventory.db")).unwrap();
    db.execute_batch("ALTER TABLE measurements RENAME TO original_measurements; CREATE VIEW measurements AS SELECT * FROM original_measurements;").unwrap();
    drop(db);
    let before = artifacts(&path);
    assert!(Unverified::open(&path, GenerationId(0)).is_err());
    assert!(artifacts(&path) == before);
}

#[test]
fn metadata_views_are_refused_before_evaluating_their_expressions() {
    for (name, definition) in [
        (
            "identity",
            "SELECT abs(-9223372036854775808) AS inventory, source FROM original_identity",
        ),
        (
            "retention",
            "SELECT singleton, abs(-9223372036854775808) AS semantic FROM original_retention",
        ),
    ] {
        let (_t, path) = fixture();
        let db = Connection::open(path.join("inventory.db")).unwrap();
        db.execute_batch(&format!(
            "ALTER TABLE {name} RENAME TO original_{name}; CREATE VIEW {name} AS {definition};"
        ))
        .unwrap();
        // This body throws integer overflow if evaluated. The format guard must
        // reject the view, rather than discovering it after a metadata SELECT.
        assert!(
            db.query_row(&format!("SELECT * FROM {name}"), [], |_| Ok(()))
                .is_err()
        );
        drop(db);
        let before = artifacts(&path);
        assert!(matches!(
            Store::inspect(&path),
            Err(Error::Contract("preview requires canonical tables"))
        ));
        assert!(matches!(
            Unverified::open(&path, GenerationId(0)),
            Err(Error::Contract("preview requires canonical tables"))
        ));
        assert!(matches!(
            Store::reopen(&path, Durability::Retained),
            Err(Error::Contract("preview requires canonical tables"))
        ));
        assert!(artifacts(&path) == before);
    }
}
