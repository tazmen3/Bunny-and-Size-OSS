use super::*;
use crate::inventory::{
    differential::NativeEvidence,
    tests::{attempt_id, context_id, fold, reconcile},
};
use sortsize_core::{
    EntryOwner, LocalFileId, MetadataValue, ValueAccuracy, acquisition::NodeProperties,
};
use std::sync::Mutex;
static SERIAL: Mutex<()> = Mutex::new(());

fn native_fixture(path: &Path, root: &Path, rows: u64, long_names: bool) -> Store {
    let c = context_id(1);
    let mut source = Store::create(path, c.inventory, c.source, Durability::Retained).unwrap();
    source.enable_properties().unwrap();
    source.create_generation(c).unwrap();
    source.set_collection_options(c, true, false).unwrap();
    source.record_acquisition(c, root, 100).unwrap();
    let attempt = attempt_id(1, 0, 1);
    source.begin_attempt(attempt).unwrap();
    let mut sequence = 0;
    for start in (0..rows).step_by(128) {
        let mut observations = Vec::new();
        let mut evidence = Vec::new();
        let mut properties = Vec::new();
        for i in start..(start + 128).min(rows) {
            let lexical = (i * 4051) % rows;
            let name = if long_names {
                format!("{}-{lexical:08}.DAT", "n".repeat(28 * 1024))
            } else {
                format!("native-{lexical:08}.type-{:02}", lexical % 32)
            };
            observations.push(Observation {
                name: queries::native_path(Path::new(&name)),
                kind: Kind::File,
                logical: Some((lexical * 7919) % 65536),
                allocated: Some(4096),
                modified: Some((-1, 999999999)),
                metadata_known: true,
            });
            #[cfg(not(windows))]
            let identity = LocalFileId::Unix {
                device: 17,
                inode: i + 1,
            };
            #[cfg(windows)]
            let identity = LocalFileId::Windows {
                volume_serial: 17,
                file_id: u128::from(i + 1).to_be_bytes(),
            };
            evidence.push(Some(NativeEvidence {
                identity: Some(identity),
                created: Some((-1, 999999999)),
            }));
            properties.push(NodeProperties {
                allocated: MetadataValue::Known {
                    value: 4096,
                    accuracy: ValueAccuracy::Exact,
                },
                accessed: MetadataValue::Known {
                    value: std::time::UNIX_EPOCH,
                    accuracy: ValueAccuracy::Exact,
                },
                owner: EntryOwner {
                    id: MetadataValue::NotCollected,
                },
            });
        }
        source
            .ingest_properties(
                BatchHeader {
                    attempt,
                    sequence,
                    end: None,
                },
                &observations,
                &evidence,
                &properties,
            )
            .unwrap();
        sequence += 1;
    }
    source
        .ingest(
            BatchHeader {
                attempt,
                sequence,
                end: Some(ListingEnd::Exhausted),
            },
            &[],
        )
        .unwrap();
    reconcile(&mut source, attempt);
    fold(&mut source, 1);
    source.seal(c).unwrap();
    source
        .record_measurement(c.generation, root, 100, 101)
        .unwrap();
    source
}

#[test]
fn permuted_native_ids_and_properties_publish_without_pinning_target_wal() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("offline");
    let source = native_fixture(&temp.path().join("source"), &root, 16384, false);
    let destination = temp.path().join("published");
    let memory = Governor::global()
        .reserve(Category::Sqlite, FRAME, None)
        .unwrap();
    // Both old and new algorithms receive exactly this envelope and cache.
    // It comfortably covers the final model; it must not fund an unbounded WAL
    // pinned by a SELECT through all write transactions.
    OBSERVED_GROWTH.store(0, std::sync::atomic::Ordering::Relaxed);
    let result = super::super::publish_admitted(
        &source,
        &destination,
        GenerationId(1),
        &root,
        PublicationGrowth::Standard(32 * FRAME),
        &Default::default(),
        Some(memory),
    );
    eprintln!(
        "native_permutation_growth_proof budget={} observed_growth={} rows=16384 target_cache={}",
        32 * FRAME,
        OBSERVED_GROWTH.load(std::sync::atomic::Ordering::Relaxed),
        FRAME
    );
    let report = result.unwrap();
    assert_eq!(report.entries, 16385);
    assert_eq!(report.oversized_paths, 0);
    let cap = CertifiedGeneration::open(&destination, GenerationId(1), &root, &Default::default())
        .unwrap();
    assert!(
        source
            .same_generation_observation(
                cap.as_store(),
                GenerationId(1),
                GenerationId(1),
                &Default::default()
            )
            .unwrap()
    );
    let ids = cap
        .as_store()
        .db
        .prepare("SELECT id FROM generation_a_rows WHERE rank>0 ORDER BY rank LIMIT 512")
        .unwrap()
        .query_map([], |r| number(r, 0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(ids.windows(2).any(|w| w[1] < w[0]));
    assert!(ids.windows(2).any(|w| w[1] > w[0]));
}

#[test]
fn direct_model_consumes_pending_keys_and_certifies_the_same_observation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("offline");
    let source = native_fixture(&temp.path().join("source"), &root, 1024, false);
    let destination = temp.path().join("published");
    let budget = PublicationGrowth::DirectReplay(32 * FRAME);
    assert_eq!(budget.model_page_bytes(), budget.bytes());
    let report = super::super::publish_admitted(
        &source,
        &destination,
        GenerationId(1),
        &root,
        budget,
        &Default::default(),
        None,
    )
    .unwrap();
    assert_eq!(report.entries, 1025);
    let cap = CertifiedGeneration::open(&destination, GenerationId(1), &root, &Default::default())
        .unwrap();
    assert!(
        source
            .same_generation_observation(
                cap.as_store(),
                GenerationId(1),
                GenerationId(1),
                &Default::default(),
            )
            .unwrap()
    );
}

#[test]
fn direct_model_pending_window_delete_rolls_back_and_keeps_key_ties() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE generation_a_pending(sort BLOB NOT NULL,id BLOB NOT NULL,PRIMARY KEY(sort,id)) STRICT,WITHOUT ROWID;").unwrap();
    for id in (0..=256).chain(Some(u64::MAX)) {
        db.execute(
            "INSERT INTO generation_a_pending VALUES(?1,?2)",
            params![Vec::<u8>::new(), unsigned(id)],
        )
        .unwrap();
    }
    let window = pending_window(&db, None, &Default::default()).unwrap();
    let tx = db.transaction().unwrap();
    consume_pending(&tx, &window).unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        pending_window(&db, None, &Default::default()).unwrap().ids,
        window.ids
    );
    let tx = db.transaction().unwrap();
    consume_pending(&tx, &window).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        pending_window(&db, None, &Default::default()).unwrap().ids,
        vec![EntryId(256), EntryId(u64::MAX)]
    );
}

#[test]
fn key_windows_close_read_transactions_and_keep_empty_keys_ties_and_u64() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE generation_a_pending(sort BLOB NOT NULL,id BLOB NOT NULL,PRIMARY KEY(sort,id)) STRICT,WITHOUT ROWID;").unwrap();
    for id in (0..=256).chain(Some(u64::MAX)) {
        db.execute(
            "INSERT INTO generation_a_pending VALUES(?1,?2)",
            params![Vec::<u8>::new(), unsigned(id)],
        )
        .unwrap();
    }
    let first = pending_window(&db, None, &Default::default()).unwrap();
    assert_eq!(first.ids.len(), 256);
    assert_eq!(
        db.transaction_state(Some("main")).unwrap(),
        rusqlite::TransactionState::None
    );
    assert!(first.key.is_empty());
    assert_eq!(first.ids.first(), Some(&EntryId(0)));
    let after = Some((first.key, *first.ids.last().unwrap()));
    let second = pending_window(&db, after, &Default::default()).unwrap();
    assert_eq!(second.ids, vec![EntryId(256), EntryId(u64::MAX)]);
    let after = Some((second.key, *second.ids.last().unwrap()));
    let third = pending_window(&db, after, &Default::default()).unwrap();
    assert!(third.ids.is_empty());
    assert_eq!(
        db.transaction_state(Some("main")).unwrap(),
        rusqlite::TransactionState::None
    );
}

#[test]
fn key_window_does_not_reduce_existing_large_blob_admissibility_or_frame() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let db = Connection::open_in_memory().unwrap();
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 512 * 1024)
        .unwrap();
    db.execute_batch("CREATE TABLE generation_a_pending(sort BLOB NOT NULL,id BLOB NOT NULL,PRIMARY KEY(sort,id)) STRICT,WITHOUT ROWID;").unwrap();
    let key = vec![7u8; 500 * 1024];
    db.execute(
        "INSERT INTO generation_a_pending VALUES(?1,?2)",
        params![&key, unsigned(1)],
    )
    .unwrap();
    let first = pending_window(&db, None, &Default::default()).unwrap();
    assert_eq!(first.key, key);
    let footprint =
        first.key.capacity() + first.ids.capacity() * std::mem::size_of::<EntryId>() + 256 * 1024;
    assert!(footprint <= FRAME as usize);
    let end = pending_window(&db, Some((first.key, EntryId(1))), &Default::default()).unwrap();
    assert!(end.ids.is_empty());
    assert_eq!(
        db.transaction_state(Some("main")).unwrap(),
        rusqlite::TransactionState::None
    );
}

#[test]
fn keyset_publication_preserves_long_native_paths_and_cancellation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("offline");
    let source = native_fixture(&temp.path().join("source"), &root, 3, true);
    let destination = temp.path().join("published");
    let report = publish(
        &source,
        &destination,
        GenerationId(1),
        &root,
        32 * FRAME,
        &Default::default(),
    )
    .unwrap();
    assert_eq!((report.entries, report.oversized_paths), (4, 0));
    let cap = CertifiedGeneration::open(&destination, GenerationId(1), &root, &Default::default())
        .unwrap();
    let longest: i64 = cap
        .as_store()
        .db
        .query_row("SELECT max(length(path)) FROM generation_a_rows", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(longest > 28 * 1024);
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE generation_a_pending(sort BLOB NOT NULL,id BLOB NOT NULL,PRIMARY KEY(sort,id)) STRICT,WITHOUT ROWID;").unwrap();
    db.execute(
        "INSERT INTO generation_a_pending VALUES(?1,?2)",
        params![b"key", unsigned(1)],
    )
    .unwrap();
    let cancel = CancellationToken::default();
    cancel.cancel();
    assert!(pending_window(&db, None, &cancel).is_err());
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM generation_a_pending", [], |r| r
            .get(0))
            .unwrap(),
        1
    );
}
