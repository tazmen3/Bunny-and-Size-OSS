//! Public integration contracts for physical generations. No performance tests.
use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::*};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store, generation_a},
};
use std::{fs, path::Path, sync::Mutex};

static SERIAL: Mutex<()> = Mutex::new(());
const INVENTORY: InventoryId = InventoryId(1100);
const SOURCE: SourceId = SourceId(2200);
const GROWTH: u64 = 64 * 1024 * 1024;

fn context(g: u64) -> AcquisitionContext {
    AcquisitionContext {
        inventory: INVENTORY,
        source: SOURCE,
        generation: GenerationId(g),
        scan: ScanId(g),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(g),
        validation: ValidationEpoch(g),
    }
}
fn native(text: String) -> NativeName {
    #[cfg(windows)]
    {
        NativeName::Windows(text.encode_utf16().collect())
    }
    #[cfg(not(windows))]
    {
        NativeName::Posix(text.into_bytes())
    }
}
fn begin(store: &mut Store, c: AcquisitionContext, root: &Path, started: i64) -> AttemptKey {
    if store.acquisition_context(c.generation).is_err() {
        store.create_generation(c).unwrap();
    }
    store.record_acquisition(c, root, started).unwrap();
    let a = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    store.begin_attempt(a).unwrap();
    a
}
fn ingest(store: &mut Store, a: AttemptKey, sequence: u64, start: u64, end: u64) {
    let rows = (start..end)
        .map(|i| Observation {
            name: native(format!("g{}-file-{i:08}.TXT", a.task.context.generation.0)),
            kind: Kind::File,
            logical: Some(i),
            allocated: None,
            modified: Some((a.task.context.generation.0 as i64, 0)),
            metadata_known: true,
        })
        .collect::<Vec<_>>();
    store
        .ingest(
            BatchHeader {
                attempt: a,
                sequence,
                end: None,
            },
            &rows,
        )
        .unwrap();
}
fn finish(
    store: &mut Store,
    a: AttemptKey,
    sequence: u64,
    root: &Path,
    started: i64,
    measured: i64,
) {
    store
        .ingest(
            BatchHeader {
                attempt: a,
                sequence,
                end: Some(ListingEnd::Exhausted),
            },
            &[],
        )
        .unwrap();
    while !store
        .reconcile_step(a, ComparisonScope::Comparable, 128)
        .unwrap()
    {}
    while !store.consolidate_step(a.task.context, 128).unwrap() {}
    store.seal(a.task.context).unwrap();
    store
        .record_measurement(a.task.context.generation, root, started, measured)
        .unwrap();
}
fn fill(
    store: &mut Store,
    c: AcquisitionContext,
    root: &Path,
    n: u64,
    started: i64,
    measured: i64,
) {
    let a = begin(store, c, root, started);
    let mut sequence = 0;
    for start in (0..n).step_by(128) {
        ingest(store, a, sequence, start, (start + 128).min(n));
        sequence += 1;
    }
    finish(store, a, sequence, root, started, measured);
}
fn bytes(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file() && e.file_name() != "owner.lock")
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn extracting_one_generation_from_legacy_multigeneration_keeps_its_native_descriptor() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("offline-native-source");
    let mut source = Store::create(
        &tmp.path().join("legacy"),
        INVENTORY,
        SOURCE,
        Durability::Retained,
    )
    .unwrap();
    fill(&mut source, context(1), &root, 17, 100, 200);
    fill(&mut source, context(2), &root, 29, 300, 400);
    let before = bytes(source.directory());
    let out = tmp.path().join("extracted");
    generation_a::publish(
        &source,
        &out,
        GenerationId(1),
        &root,
        GROWTH,
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(bytes(source.directory()) == before);
    let inspected = Store::inspect(&out).unwrap();
    let descriptor = inspected.measurement(GenerationId(1)).unwrap();
    assert_eq!(descriptor.context, context(1));
    assert_eq!(descriptor.root, root);
    assert_eq!((descriptor.started, descriptor.measured), (100, 200));
    assert_eq!(inspected.entry_count(GenerationId(1)).unwrap(), 18);
    assert_eq!(inspected.entry_count(GenerationId(2)).unwrap(), 0);
    assert!(inspected.acquisition_context(GenerationId(2)).is_err());
    drop(inspected);
    let certified = generation_a::CertifiedGeneration::open(
        &out,
        GenerationId(1),
        &root,
        &CancellationToken::default(),
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("query")).unwrap();
    let query = certified
        .query(
            &SearchQuery::default(),
            &tmp.path().join("query"),
            GROWTH,
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(query.complete());
    assert_eq!(query.counts.matches, 18);
    for row in query.page(None).unwrap().rows {
        assert_eq!(row.key.generation, GenerationId(1));
        assert_eq!(row.key.inventory, INVENTORY);
    }
    assert!(!root.exists());
}

fn committed(outcome: generation_a::PublicationOutcome) -> generation_a::Descriptor {
    assert!(
        outcome.committed(),
        "publication must have a confirmed pointer commit"
    );
    outcome.descriptor().clone()
}
fn verify_generation(
    cap: &generation_a::CertifiedGeneration,
    g: GenerationId,
    n: u64,
    scratch: &Path,
) {
    assert_eq!(cap.generation(), g);
    fs::create_dir_all(scratch).unwrap();
    let query = cap
        .query(
            &SearchQuery::default(),
            scratch,
            GROWTH,
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(query.complete());
    assert_eq!(query.counts.matches, n + 1);
    let mut count = 0;
    let mut after = None;
    loop {
        let page = query.page(after.as_ref()).unwrap();
        assert!(page.rows.len() <= 200 && page.accounted_bytes <= 512 * 1024);
        for row in page.rows {
            assert_eq!(row.key.inventory, INVENTORY);
            assert_eq!(row.key.generation, g);
            assert!(row.path.starts_with(cap.root()));
            count += 1;
        }
        if !page.more {
            break;
        }
        after = page.next;
        assert!(after.is_some());
    }
    assert_eq!(count, n + 1);
}

#[test]
fn repository_serves_g0_while_a_different_logical_generation_is_acquired_and_published() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("never-existing-native-source");
    let cancel = CancellationToken::default();
    let mut repo =
        generation_a::Repository::open(&tmp.path().join("repository"), INVENTORY, SOURCE).unwrap();
    let first = repo.next_context(context(1)).unwrap();
    let mut first_plan = generation_a::Pipeline::plan().unwrap();
    let mut first_source = repo.create_candidate(first, &mut first_plan).unwrap();
    fill(&mut first_source, first, &root, 257, 100, 200);
    let first_descriptor = committed(
        repo.publish_with_plan(
            &first_source,
            first.generation,
            &root,
            GROWTH,
            &cancel,
            &mut first_plan,
        )
        .unwrap(),
    );
    drop(first_source);
    drop(first_plan);
    let before = bytes(&first_descriptor.directory);

    // Admission/retirement may need its own bounded certificate before worker
    // slots and the persistent reader occupy SQLite category capacity.
    let second = repo.next_context(context(2)).unwrap();
    assert_ne!(first.generation, second.generation);
    assert!(second.generation > first.generation);
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let slots = plan.remaining();
    assert!(
        slots.reader >= 1024 * 1024
            && slots.writer >= 1024 * 1024
            && slots.publication >= 1024 * 1024
    );
    let cap = repo
        .open_latest_with_plan(&mut plan, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(plan.remaining().reader, 0);
    let initial = cap.as_store().measurement(first.generation).unwrap();
    assert_eq!((initial.started, initial.measured), (100, 200));
    assert_eq!(initial.root, root);
    let (active_tx, active_rx) = mpsc::channel();
    let (read_tx, read_rx) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let reader_stop = stopped.clone();
    let scratch = tmp.path().join("g0-reader");
    let reader = std::thread::spawn(move || {
        active_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        verify_generation(&cap, first.generation, 257, &scratch);
        read_tx.send(()).unwrap();
        let mut count = 1;
        while !reader_stop.load(Ordering::Acquire) {
            verify_generation(&cap, first.generation, 257, &scratch);
            count += 1;
        }
        assert_eq!(
            cap.as_store().measurement(first.generation).unwrap(),
            initial
        );
        count
    });
    let mut second_source = repo.create_candidate(second, &mut plan).unwrap();
    let a = begin(&mut second_source, second, &root, 300);
    ingest(&mut second_source, a, 0, 0, 128);
    assert!(
        !second_source
            .totals(second.generation)
            .unwrap()
            .fold_complete
    );
    active_tx.send(()).unwrap();
    read_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    ingest(&mut second_source, a, 1, 128, 385);
    finish(&mut second_source, a, 2, &root, 300, 400);
    let descriptor = committed(
        repo.publish_with_plan(
            &second_source,
            second.generation,
            &root,
            GROWTH,
            &cancel,
            &mut plan,
        )
        .unwrap(),
    );
    assert_eq!(plan.remaining().publication, 0);
    drop(second_source);
    drop(plan);
    assert_eq!((descriptor.started, descriptor.measured), (300, Some(400)));
    assert_eq!(descriptor.root, root);
    // The currently leased old generation must not be deleted or reopened RW.
    assert!(repo.purge(first.generation, &cancel).is_err());
    assert!(bytes(&first_descriptor.directory) == before);
    stopped.store(true, Ordering::Release);
    assert!(reader.join().unwrap() >= 1);
    let latest = repo.open_latest(&cancel).unwrap().unwrap();
    verify_generation(
        &latest,
        second.generation,
        385,
        &tmp.path().join("g1-reader"),
    );
    assert!(!root.exists());
}

#[test]
fn pointer_failure_after_real_generation_publication_is_typed_and_recoverable() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let location = tmp.path().join("repository");
    let root = tmp.path().join("offline");
    let cancel = CancellationToken::default();
    let mut repo = generation_a::Repository::open(&location, INVENTORY, SOURCE).unwrap();
    let first = repo.next_context(context(1)).unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(first, &mut plan).unwrap();
    fill(&mut source, first, &root, 9, 100, 200);
    let old = committed(
        repo.publish_with_plan(&source, first.generation, &root, GROWTH, &cancel, &mut plan)
            .unwrap(),
    );
    drop(source);
    drop(plan);
    let old_bytes = bytes(&old.directory);
    let second = repo.next_context(context(2)).unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(second, &mut plan).unwrap();
    fill(&mut source, second, &root, 13, 300, 400);
    let pointer = location.join("CURRENT.json");
    fs::remove_file(&pointer).unwrap();
    fs::create_dir(&pointer).unwrap();
    let outcome = repo
        .publish_with_plan(
            &source,
            second.generation,
            &root,
            GROWTH,
            &cancel,
            &mut plan,
        )
        .unwrap();
    assert!(matches!(
        &outcome,
        generation_a::PublicationOutcome::PublishedUncommitted { .. }
    ));
    assert!(!outcome.committed());
    assert!(
        outcome
            .descriptor()
            .directory
            .join("inventory.db")
            .is_file()
    );
    assert!(bytes(&old.directory) == old_bytes);
    assert_eq!(
        repo.open_latest(&cancel).unwrap().unwrap().generation(),
        first.generation
    );
    drop(outcome);
    drop(source);
    drop(plan);
    drop(repo);
    assert!(generation_a::Repository::open_existing(&location).is_err());
    assert!(
        pointer.is_dir(),
        "opening a corrupt catalogue must not repair or delete it"
    );
    // Remove only this test's deliberately created empty directory. Recovery
    // must never delete a caller's foreign directory implicitly.
    fs::remove_dir(&pointer).unwrap();
    let mut recovered =
        generation_a::Repository::open_for_recovery(&location, INVENTORY, SOURCE).unwrap();
    recovered.recover_catalogue(&cancel).unwrap();
    let latest = recovered.open_latest(&cancel).unwrap().unwrap();
    verify_generation(
        &latest,
        second.generation,
        13,
        &tmp.path().join("recovered-query"),
    );
    assert_eq!(
        latest
            .as_store()
            .measurement(second.generation)
            .unwrap()
            .measured,
        400
    );
    assert!(bytes(&old.directory) == old_bytes);
}

#[test]
fn cancelled_publication_does_not_reuse_its_allocated_generation_after_restart() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let location = tmp.path().join("repository");
    let root = tmp.path().join("offline");
    let mut repo = generation_a::Repository::open(&location, INVENTORY, SOURCE).unwrap();
    let first = repo.next_context(context(1)).unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(first, &mut plan).unwrap();
    fill(&mut source, first, &root, 9, 100, 200);
    let stop = CancellationToken::default();
    stop.cancel();
    assert!(
        repo.publish_with_plan(&source, first.generation, &root, GROWTH, &stop, &mut plan)
            .is_err()
    );
    assert!(
        repo.open_latest(&CancellationToken::default())
            .unwrap()
            .is_none()
    );
    assert!(repo.published(first.generation).unwrap().is_none());
    drop(source);
    drop(plan);
    drop(repo);
    let mut restarted = generation_a::Repository::open_existing(&location).unwrap();
    let next = restarted.next_context(context(2)).unwrap();
    assert!(next.generation > first.generation);
    assert!(
        restarted
            .candidate_path(first.generation)
            .join("inventory.db")
            .is_file()
    );
}

#[test]
fn future_repository_identity_and_corrupt_published_database_are_preserved() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let location = tmp.path().join("repository");
    let root = tmp.path().join("offline");
    let cancel = CancellationToken::default();
    let mut repo = generation_a::Repository::open(&location, INVENTORY, SOURCE).unwrap();
    let c = repo.next_context(context(1)).unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(c, &mut plan).unwrap();
    fill(&mut source, c, &root, 17, 100, 200);
    let descriptor = committed(
        repo.publish_with_plan(&source, c.generation, &root, GROWTH, &cancel, &mut plan)
            .unwrap(),
    );
    drop(source);
    drop(plan);
    let db = rusqlite::Connection::open(descriptor.directory.join("inventory.db")).unwrap();
    db.execute_batch("PRAGMA user_version=999;").unwrap();
    drop(db);
    let group_before = bytes(&descriptor.directory);
    assert!(repo.open_latest(&cancel).is_err());
    assert!(bytes(&descriptor.directory) == group_before);
    drop(repo);
    let identity = location.join("IDENTITY.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&fs::read(&identity).unwrap()).unwrap();
    data["version"] = serde_json::json!(999);
    fs::write(&identity, serde_json::to_vec(&data).unwrap()).unwrap();
    let before = fs::read(&identity).unwrap();
    assert!(generation_a::Repository::open_existing(&location).is_err());
    assert_eq!(fs::read(&identity).unwrap(), before);
    assert!(bytes(&descriptor.directory) == group_before);
}

#[test]
fn repository_preserves_an_opaque_native_root_without_text_conversion() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let root = {
        use std::os::unix::ffi::OsStringExt;
        tmp.path()
            .join(std::ffi::OsString::from_vec(vec![b'r', 0xff]))
    };
    #[cfg(windows)]
    let root = {
        use std::os::windows::ffi::OsStringExt;
        tmp.path()
            .join(std::ffi::OsString::from_wide(&[0x72, 0xd800]))
    };
    #[cfg(not(any(unix, windows)))]
    let root = tmp.path().join("opaque-root");
    let mut repo =
        generation_a::Repository::open(&tmp.path().join("repository"), INVENTORY, SOURCE).unwrap();
    let c = repo.next_context(context(1)).unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(c, &mut plan).unwrap();
    fill(&mut source, c, &root, 17, 100, 200);
    let descriptor = committed(
        repo.publish_with_plan(
            &source,
            c.generation,
            &root,
            GROWTH,
            &CancellationToken::default(),
            &mut plan,
        )
        .unwrap(),
    );
    assert_eq!(descriptor.root, root);
    drop(source);
    drop(plan);
    drop(repo);
    let restarted =
        generation_a::Repository::open_existing(&tmp.path().join("repository")).unwrap();
    let cap = restarted
        .open_latest(&CancellationToken::default())
        .unwrap()
        .unwrap();
    assert_eq!(cap.root(), root);
    assert_eq!(cap.as_store().measurement(c.generation).unwrap().root, root);
    verify_generation(&cap, c.generation, 17, &tmp.path().join("native-query"));
    assert!(!root.exists());
}

#[test]
fn admitted_growth_failure_keeps_the_candidate_and_stage_unselected_after_restart() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let location = tmp.path().join("repository");
    let root = tmp.path().join("offline");
    let cancel = CancellationToken::default();
    let mut repo = generation_a::Repository::open(&location, INVENTORY, SOURCE).unwrap();
    let c = repo.next_context(context(1)).unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(c, &mut plan).unwrap();
    fill(&mut source, c, &root, 20000, 100, 200);
    let before = bytes(source.directory());
    assert!(
        repo.publish_with_plan(
            &source,
            c.generation,
            &root,
            1024 * 1024,
            &cancel,
            &mut plan
        )
        .is_err()
    );
    assert!(bytes(source.directory()) == before);
    assert!(!repo.generation_path(c.generation).exists());
    assert!(repo.published(c.generation).unwrap().is_none());
    assert!(fs::read_dir(&location).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".stage-a-")
    }));
    assert!(repo.account().unwrap() > 0);
    drop(source);
    drop(plan);
    drop(repo);
    let restarted = generation_a::Repository::open_existing(&location).unwrap();
    assert!(restarted.open_latest(&cancel).unwrap().is_none());
    assert!(
        restarted
            .candidate_path(c.generation)
            .join("inventory.db")
            .is_file()
    );
}
