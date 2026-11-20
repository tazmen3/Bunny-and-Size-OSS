use super::*;
use sortsize_core::acquisition::Policy;

fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(193),
        source: SourceId(193),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}

#[test]
fn completed_root_listing_records_freshness_in_the_existing_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("member"), b"content").unwrap();
    let context = context();
    let store = Store::create(
        &temp.path().join("inventory"),
        context.inventory,
        context.source,
        crate::Durability::Reconstructible,
    )
    .unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    let stamp = guard.inspect(&root).unwrap();
    let mut session = Session::new(store, context, guard).unwrap();
    let start = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    let db = rusqlite::Connection::open_with_flags(
        session.store().directory().join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (seconds, nanos, proof): (Option<i64>, Option<u32>, Vec<u8>) = db.query_row(
        "SELECT e.modified_s,e.modified_ns,p.proof FROM entries e JOIN entry_evidence p ON p.generation=e.generation AND p.id=e.id WHERE e.id=zeroblob(8)",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(seconds.zip(nanos), stamp.modified.and_then(timestamp));
    assert_eq!(
        proof,
        crate::inventory::differential::NativeEvidence::from_stamp(&stamp).encode()
    );
    assert!(
        !proof.is_empty(),
        "native local fixture must have identity and creation evidence"
    );
}

#[test]
fn cached_eof_after_directory_mutation_is_incomplete() {
    let temp = tempfile::tempdir().unwrap();
    let guard = Guard::open(temp.path(), Policy::default()).unwrap();
    let before = guard.inspect(temp.path()).unwrap();
    let mut read = guard.scan_directory(temp.path()).unwrap();
    assert!(read.next().is_none());
    let start = Instant::now();
    loop {
        fs::write(temp.path().join("added-after-cached-eof"), b"new member").unwrap();
        if guard.inspect(temp.path()).unwrap().modified != before.modified {
            break;
        }
        fs::remove_file(temp.path().join("added-after-cached-eof")).unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(5));
    }
    let cursor = Cursor {
        attempt: AttemptKey {
            task: TaskKey {
                context: context(),
                directory: EntryId(0),
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(1),
            owner: OwnerEpoch(1),
            branch: BranchRevision(1),
        },
        path: temp.path().to_owned(),
        read: Some(read),
        snapshot: Some(before),
        sequence: 0,
        _lease: cursor_lease(true).unwrap(),
    };
    let done = quantum(
        &guard,
        cursor,
        acquisition_lease(true, false).unwrap(),
        Default::default(),
        &Control::default(),
        None,
    );
    assert_eq!(done.end, Some(ListingEnd::Incomplete));
    assert!(done.directory.is_none());
    assert!(done.errors.count > 0);
    assert_eq!(
        done.errors.last.unwrap().operation,
        AcquisitionOperation::ValidateDirectory
    );
}

#[test]
fn replayed_eof_cannot_replace_the_recorded_directory_freshness() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let stamp = Guard::open(&root, Policy::default())
        .unwrap()
        .inspect(&root)
        .unwrap();
    let context = context();
    let mut store = Store::create(
        &temp.path().join("inventory"),
        context.inventory,
        context.source,
        crate::Durability::Reconstructible,
    )
    .unwrap();
    store.enable_properties().unwrap();
    store.create_generation(context).unwrap();
    let attempt = store.next_attempt(context, EntryId(0)).unwrap();
    store.begin_attempt(attempt).unwrap();
    let header = BatchHeader {
        attempt,
        sequence: 0,
        end: Some(ListingEnd::Exhausted),
    };
    store
        .ingest_directory_properties(header, &[], &[], &[], Some(&stamp))
        .unwrap();
    let revision = store.revision(context.generation).unwrap();
    let mut different = stamp.clone();
    different.modified = Some(std::time::UNIX_EPOCH);
    assert!(matches!(
        store
            .ingest_directory_properties(header, &[], &[], &[], Some(&different))
            .unwrap(),
        crate::inventory::Ack::Replay
    ));
    assert_eq!(revision, store.revision(context.generation).unwrap());
    let db = rusqlite::Connection::open_with_flags(
        store.directory().join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let actual: (Option<i64>, Option<u32>) = db
        .query_row(
            "SELECT modified_s,modified_ns FROM entries WHERE id=zeroblob(8)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(actual.0.zip(actual.1), stamp.modified.and_then(timestamp));
}
