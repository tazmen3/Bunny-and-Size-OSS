use super::*;
use crate::Durability;
use sortsize_core::acquisition::Policy;
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
#[test]
fn listing_provenance_is_required_and_restored_after_cancellation() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    let path = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), b"data").unwrap();
    let create = |path: &std::path::Path| {
        Store::create(
            path,
            context().inventory,
            context().source,
            Durability::Retained,
        )
        .unwrap()
    };
    let listing_guard = || {
        Guard::open(&root, Policy::default())
            .unwrap()
            .with_scan_observation(ScanObservation::DirectoryListing)
    };
    assert!(matches!(
        Session::new(
            create(&temp.path().join("mismatch")),
            context(),
            listing_guard()
        ),
        Err(Error::Contract(_))
    ));
    let c = AcquisitionContext {
        policy: DIRECTORY_LISTING_POLICY,
        ..context()
    };
    let mut session = Session::new(create(&path), c, listing_guard()).unwrap();
    session.store_mut().record_acquisition(c, &root, 1).unwrap();
    session.control().cancel();
    assert_eq!(session.tick().unwrap().state, State::Cancelled);
    drop(session);
    let mut resumed = Session::resume(
        Store::reopen(&path, Durability::Retained).unwrap(),
        c.generation,
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        resumed.guard.scan_observation(),
        ScanObservation::DirectoryListing
    );
    assert_eq!(resumed.context().policy, DIRECTORY_LISTING_POLICY);
    let started = Instant::now();
    while resumed.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    while resumed.finalize_step().unwrap().is_none() {}
    resumed
        .store_mut()
        .record_measurement(c.generation, &root, 1, 2)
        .unwrap();
    drop(resumed);
    let saved = Store::reopen(&path, Durability::Retained).unwrap();
    assert_eq!(
        saved.acquisition_context(c.generation).unwrap().policy,
        DIRECTORY_LISTING_POLICY
    );
    let strict = Session::verify(saved, c.generation, listing_guard()).unwrap();
    assert_eq!(strict.guard.scan_observation(), ScanObservation::Strict);
    assert_eq!(
        strict.context().policy,
        observation_policy(ScanObservation::Strict)
    );
}

#[test]
fn weighted_rotation_never_starves_background_or_enrichment() {
    let mut f = Fairness::default();
    let mut trace = Vec::new();
    for _ in 0..8000 {
        trace.push(f.choose([true; 4]).unwrap());
    }
    for window in trace.windows(8) {
        assert!(window.contains(&2));
        assert!(window.contains(&3));
    }
    assert_eq!(
        (0..4)
            .map(|p| trace.iter().filter(|v| **v == p).count())
            .collect::<Vec<_>>(),
        vec![4000, 2000, 1000, 1000]
    );
    for _ in 0..100 {
        assert_eq!(f.choose([false, false, false, true]), Some(3));
    }
    assert_eq!(f.choose([false; 4]), None);
}
#[test]
fn blocked_calls_keep_all_eight_permits_and_cursor_reservations() {
    // Independent counters avoid interference with parallel native tests.
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let held: Vec<_> = (0..8).map(|_| reserve(&COUNT, 8).unwrap()).collect();
    assert!(reserve(&COUNT, 8).is_none());
    assert_eq!(COUNT.load(Ordering::SeqCst), 8);
    drop(held);
    assert_eq!(COUNT.load(Ordering::SeqCst), 0);
}
#[test]
fn native_session_commits_children_once_pauses_and_does_not_rescan_completed_directory() {
    let _serial = TEST_LOCK.lock().unwrap();
    let f = tempfile::tempdir().unwrap();
    let root = f.path().join("source");
    fs::create_dir(&root).unwrap();
    for i in 0..12 {
        let dir = root.join(format!("dir{i}"));
        fs::create_dir(&dir).unwrap();
        for n in 0..40 {
            fs::write(dir.join(format!("f{n}")), b"abc").unwrap();
        }
    }
    let store = Store::create(
        &f.path().join("db"),
        InventoryId(1),
        SourceId(1),
        Durability::Retained,
    )
    .unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    let mut s = Session::new(store, context(), guard).unwrap();
    let control = s.control();
    control.pause(true);
    assert_eq!(s.tick().unwrap().state, State::Paused);
    assert_eq!(s.store().entry_count(GenerationId(1)).unwrap(), 1);
    control.navigate(EntryId(99));
    control.navigate(EntryId(88));
    control.navigate(EntryId(0));
    assert_eq!(control.0.lock().unwrap().navigation, Some(EntryId(0)));
    control.pause(false);
    let start = Instant::now();
    loop {
        let status = s.tick().unwrap();
        assert!(
            status.in_flight <= 8 && status.global_acquisitions <= 8 && status.global_cursors <= 16
        );
        if status.state == State::Idle {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(s.store().entry_count(GenerationId(1)).unwrap(), 493);
    let n = s.quanta;
    control.navigate(EntryId(0));
    for _ in 0..10 {
        s.tick().unwrap();
    }
    assert_eq!(s.quanta, n);
    while s.finalize_step().unwrap().is_none() {}
    assert_eq!(s.store().latest_complete().unwrap(), Some(GenerationId(1)));
    assert_eq!(s.tick().unwrap().state, State::Sealed);
}

pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

fn cancellation_failure_session(temp: &tempfile::TempDir) -> Session {
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    for n in 0..8 {
        let directory = root.join(format!("child-{n:02}"));
        fs::create_dir(&directory).unwrap();
        for file in 0..64 {
            fs::write(directory.join(format!("file-{file:02}")), b"abc").unwrap();
        }
    }
    let store = Store::create(
        &temp.path().join("db"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    // Commit only the root. Children remain durably pending, with no lost data.
    session.tick().unwrap();
    session.control().pause(true);
    let started = Instant::now();
    while session.status().in_flight != 0 {
        session.tick().unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    session
}

#[test]
fn grouped_admission_failure_rolls_back_before_dispatch_and_reselects() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    session.batch_admissions = true;
    let before = session.store().pending_directories(context(), 16).unwrap();
    assert_eq!(before.len(), 8);
    let revision = session.store().revision(context().generation).unwrap();
    let fair = session.fair.next;
    let db = rusqlite::Connection::open(session.store().directory().join("inventory.db")).unwrap();
    // Root has a key. Fail the second attempt only, after the first update
    // has already become visible to the same transaction.
    db.execute_batch(
        "CREATE TRIGGER fail_second_admission BEFORE UPDATE OF key ON tasks
         WHEN (SELECT count(*) FROM tasks WHERE key IS NOT NULL) = 2
         BEGIN SELECT RAISE(ABORT,'second scheduler admission'); END;",
    )
    .unwrap();
    session.control().pause(false);
    assert!(session.tick().is_err());
    let status = session.status();
    assert_eq!(status.state, State::StoreError);
    assert_eq!(status.in_flight, 0);
    assert_eq!(status.pending_completions, 0);
    assert_eq!(status.parked, 0);
    assert_eq!(status.global_acquisitions, 0);
    assert_eq!(status.global_cursors, 0);
    assert_eq!(session.fair.next, fair);
    assert_eq!(
        session.store().revision(context().generation).unwrap(),
        revision
    );
    assert_eq!(
        session.store().pending_directories(context(), 16).unwrap(),
        before
    );
    db.execute_batch("DROP TRIGGER fail_second_admission")
        .unwrap();
    drop(db);
    session.retry_store();
    finish_grouped_admission_scan(&mut session);
}

#[test]
fn selected_background_candidates_yield_to_new_navigation_before_commit() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    let candidates = session.store().pending_directories(context(), 16).unwrap();
    let revision = session.store().revision(context().generation).unwrap();
    let control = session.control();
    for relative in [false, true] {
        *control.0.lock().unwrap() = Intent::default();
        if relative {
            control
                .navigate_relative(PathBuf::from("child-07"))
                .unwrap();
        } else {
            control.navigate(*candidates.last().unwrap());
        }
        // Selection happened before the new intention; the saved IDs must
        // confer no authority to commit or dispatch background work now.
        let permit = acquisition_lease(false, false).unwrap();
        assert!(
            session
                .prepare_admissions(candidates[0], cursor_lease(false).unwrap(), 7, &candidates)
                .unwrap()
                .is_none()
        );
        drop(permit);
        assert_eq!(
            session.store().revision(context().generation).unwrap(),
            revision
        );
        assert_eq!(
            session.store().pending_directories(context(), 16).unwrap(),
            candidates
        );
        assert_eq!(session.status().global_acquisitions, 0);
        assert_eq!(session.status().global_cursors, 0);
        assert!(session.running.is_empty());
    }
    *control.0.lock().unwrap() = Intent::default();
    session.batch_admissions = true;
    finish_grouped_admission_scan(&mut session);
}

#[test]
fn selected_background_candidates_revalidate_parent_attempt_before_atomic_commit() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    let candidates = session.store().pending_directories(context(), 16).unwrap();
    let revision = session.store().revision(context().generation).unwrap();
    let db = rusqlite::Connection::open(session.store().directory().join("inventory.db")).unwrap();
    let generation = crate::codec::unsigned(context().generation.0);
    let stale_id = crate::codec::unsigned(candidates[1].0);
    let seen: Vec<u8> = db
        .query_row(
            "SELECT attempt FROM entries WHERE generation=?1 AND id=?2",
            rusqlite::params![generation, stale_id],
            |row| row.get(0),
        )
        .unwrap();
    // The native path and task still exist, but this child's parent proof no
    // longer matches. A list selected earlier must not bypass validate_attempt.
    db.execute(
        "UPDATE entries SET attempt=?3 WHERE generation=?1 AND id=?2",
        rusqlite::params![generation, stale_id, crate::codec::unsigned(u64::MAX)],
    )
    .unwrap();
    session.control().pause(false);
    let permit = acquisition_lease(false, false).unwrap();
    assert!(
        session
            .prepare_admissions(candidates[0], cursor_lease(false).unwrap(), 7, &candidates)
            .is_err()
    );
    drop(permit);
    assert_eq!(session.state, State::StoreError);
    assert_eq!(
        session.store().revision(context().generation).unwrap(),
        revision
    );
    let opened: i64 = db
        .query_row(
            "SELECT count(*) FROM tasks WHERE directory != zeroblob(8) AND key IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(opened, 0);
    assert!(session.running.is_empty());
    assert_eq!(session.status().global_acquisitions, 0);
    assert_eq!(session.status().global_cursors, 0);
    db.execute(
        "UPDATE entries SET attempt=?3 WHERE generation=?1 AND id=?2",
        rusqlite::params![generation, stale_id, seen],
    )
    .unwrap();
    drop(db);
    session.batch_admissions = true;
    session.retry_store();
    finish_grouped_admission_scan(&mut session);
}

#[test]
fn committed_eight_attempts_park_before_dispatch_and_can_be_replayed_after_stop() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    let candidates = session.store().pending_directories(context(), 16).unwrap();
    let id = candidates[0];
    session.control().pause(false);
    let permit = acquisition_lease(false, false).unwrap();
    let (cursor, additional) = session
        .prepare_admissions(id, cursor_lease(false).unwrap(), 7, &candidates)
        .unwrap()
        .unwrap();
    assert_eq!(additional.len(), 7);
    let mut group = vec![Admission { cursor, permit }];
    group.extend(additional);
    assert_eq!(
        session
            .store()
            .pending_directories(context(), 16)
            .unwrap()
            .len(),
        0
    );
    session.control().pause(true);
    assert!(!session.dispatch_background_group(group).unwrap());
    assert_eq!(session.tick().unwrap().state, State::Paused);
    assert_eq!(session.status().in_flight, 0);
    assert_eq!(session.status().parked, 8);
    assert_eq!(session.status().global_acquisitions, 0);
    session.control().cancel();
    assert_eq!(session.tick().unwrap().state, State::Cancelled);
    assert_eq!(session.status().parked, 0);
    assert_eq!(session.status().global_cursors, 0);
    assert_eq!(session.store().latest_complete().unwrap(), None);

    let store_path = session.store().directory().to_owned();
    drop(session);
    let mut resumed = Session::resume(
        Store::reopen(&store_path, Durability::Retained).unwrap(),
        context().generation,
        Guard::open(&temp.path().join("source"), Policy::default()).unwrap(),
    )
    .unwrap();
    assert!(resumed.context().run > context().run);
    resumed.batch_admissions = true;
    finish_grouped_admission_scan(&mut resumed);
}

fn finish_grouped_admission_scan(session: &mut Session) {
    let start = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    assert_eq!(
        session.store().entry_count(context().generation).unwrap(),
        521
    );
    let coverage = loop {
        if let Some(coverage) = session.finalize_step().unwrap() {
            break coverage;
        }
    };
    assert_eq!(coverage, ScopeCoverage::Complete);
    assert_eq!(
        session.store().latest_complete().unwrap(),
        Some(context().generation)
    );
}

fn fail_scheduler_ingestion(session: &Session) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(session.store().directory().join("inventory.db")).unwrap();
    // A real SQLite transaction abort, rather than a manually assigned state.
    db.execute_batch("CREATE TRIGGER scheduler_ingest_failure BEFORE INSERT ON entries BEGIN SELECT RAISE(ABORT,'scheduler ingestion fault'); END;")
        .unwrap();
    db
}

#[test]
fn cancellation_after_ingestion_failure_drains_retained_and_queued_completions() {
    check_cancellation_after_ingestion_failure(false);
}

#[test]
fn cancellation_after_grouped_ingestion_failure_drains_eight_retained_completions() {
    check_cancellation_after_ingestion_failure(true);
}

fn check_cancellation_after_ingestion_failure(grouped: bool) {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    session.batch_completions = grouped;
    let root = session.guard.root().to_owned();
    let db = fail_scheduler_ingestion(&session);
    session.control().pause(false);
    assert_eq!(session.tick().unwrap().in_flight, 8);
    let started = Instant::now();
    // A pending lease is counted just before the channel send. Wait for all
    // sends explicitly so the next tick always has enough work to pair.
    let returned: Vec<_> = (0..8)
        .map(|_| {
            session
                .receive
                .recv_timeout(Duration::from_secs(30))
                .unwrap()
        })
        .collect();
    for done in returned {
        assert!(session.send.send(done).is_ok());
    }
    let error = session.tick().unwrap_err();
    assert!(error.to_string().contains("scheduler ingestion fault"));
    assert_eq!(session.status().state, State::StoreError);
    assert_eq!(session.retry.len(), if grouped { 8 } else { 1 });
    assert_eq!(session.status().acquisitions_pending, 0);
    assert_eq!(session.status().pending_completions, 8);
    assert_eq!(session.status().in_flight, 8);
    assert_eq!(session.tick().unwrap().in_flight, 8);
    assert_eq!(
        session.store().entry_count(context().generation).unwrap(),
        9
    );
    session.control().cancel();
    let committed = session.status().quanta;
    let mut drains = 0;
    while session.status().in_flight != 0 {
        let before = session.status().in_flight;
        session.tick().unwrap();
        assert!(before - session.status().in_flight <= COMMIT_QUANTA);
        drains += 1;
        assert!(drains <= 8, "cancellation remained stuck behind StoreError");
    }
    let status = session.status();
    assert_eq!(status.state, State::CancelledStoreError);
    assert_eq!(status.pending_completions, 0);
    assert_eq!(status.acquisitions_pending, 0);
    assert_eq!(status.abandoned_quanta, 8);
    assert_eq!(status.quanta, committed);
    assert_eq!(status.global_acquisitions, 0);
    assert_eq!(status.global_cursors, 0);
    assert!(session.active.is_none());
    assert!(session.finalize_step().is_err());
    assert_eq!(session.store().latest_complete().unwrap(), None);
    assert_eq!(session.store().latest_sealed().unwrap(), None);
    // Repairing the storage fault cannot restart an explicitly cancelled job.
    db.execute_batch("DROP TRIGGER scheduler_ingest_failure;")
        .unwrap();
    drop(db);
    session.retry_store();
    assert_eq!(session.tick().unwrap().state, State::CancelledStoreError);
    assert_eq!(
        session.store().entry_count(context().generation).unwrap(),
        9
    );
    drop(session);
    let preserved = Store::reopen(&temp.path().join("db"), Durability::Retained).unwrap();
    assert_eq!(preserved.latest_sealed().unwrap(), None);
    assert_eq!(preserved.entry_count(context().generation).unwrap(), 9);
    drop(preserved);
    // The root lease and global budgets permit a new deliberate acquisition.
    let mut next = Session::new(
        Store::create(
            &temp.path().join("next"),
            context().inventory,
            context().source,
            Durability::Retained,
        )
        .unwrap(),
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    while next.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    while next.finalize_step().unwrap().is_none() {}
    assert_eq!(next.store().entry_count(context().generation).unwrap(), 521);
    assert_eq!(
        next.store().latest_complete().unwrap(),
        Some(context().generation)
    );
}

#[test]
fn failed_second_grouped_completion_rolls_back_both_and_retries_in_order() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    session.batch_completions = true;
    let generation = context().generation;
    session.control().pause(false);
    assert_eq!(session.tick().unwrap().in_flight, 8);
    session.control().pause(true);

    // Fix dequeue order so the trigger fails in the second batch after the
    // first batch has written to the transaction. Keep both permits held.
    let first = session
        .receive
        .recv_timeout(Duration::from_secs(30))
        .unwrap();
    let second = session
        .receive
        .recv_timeout(Duration::from_secs(30))
        .unwrap();
    assert!(!first.observations.is_empty());
    assert!(!second.observations.is_empty());
    let first_header = first.batch().header;
    let second_header = second.batch().header;
    let rejected_parent = second_header.attempt.task.directory.0;
    let batch_size = (first.observations.len() + second.observations.len()) as u64;
    session.retry.extend([first, second]);
    let db = rusqlite::Connection::open(session.store().directory().join("inventory.db")).unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER scheduler_second_failure BEFORE INSERT ON entries
         WHEN NEW.parent = x'{rejected_parent:016x}'
         BEGIN SELECT RAISE(ABORT,'scheduler second completion fault'); END;"
    ))
    .unwrap();

    let revision = session.store().revision(generation).unwrap();
    let quanta = session.status().quanta;
    let entries = session.store().entry_count(generation).unwrap();
    let error = session.tick().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("scheduler second completion fault")
    );
    assert_eq!(session.status().state, State::StoreError);
    assert_eq!(session.status().quanta, quanta);
    assert_eq!(session.status().in_flight, 8);
    assert_eq!(session.store().revision(generation).unwrap(), revision);
    assert_eq!(session.store().entry_count(generation).unwrap(), entries);
    assert_eq!(session.retry.len(), 2);
    assert_eq!(session.retry[0].batch().header, first_header);
    assert_eq!(session.retry[1].batch().header, second_header);

    db.execute_batch("DROP TRIGGER scheduler_second_failure;")
        .unwrap();
    drop(db);
    session.retry_store();
    let status = session.tick().unwrap();
    assert_eq!(status.state, State::Paused);
    assert_eq!(status.quanta, quanta + 2);
    assert_eq!(status.in_flight, 6);
    assert!(session.retry.is_empty());
    assert_eq!(
        session.store().entry_count(generation).unwrap(),
        entries + batch_size
    );

    session.control().pause(false);
    let started = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    while session.finalize_step().unwrap().is_none() {}
    assert_eq!(session.store().entry_count(generation).unwrap(), 521);
    assert_eq!(session.store().latest_complete().unwrap(), Some(generation));
}

#[test]
fn cancellation_after_storage_error_keeps_blocked_worker_budgets_until_real_return() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = cancellation_failure_session(&temp);
    let ids = session.store().pending_directories(context(), 8).unwrap();
    let mut release = Vec::new();
    let (started_send, started_receive) = mpsc::channel();
    // Controlled waits stand for outstanding worker calls, with the very same
    // leases and completion path as the real scheduler. No sleeps or guessed
    // native-call timing is needed for this regression.
    for (index, id) in ids.into_iter().enumerate() {
        let cursor = Cursor {
            attempt: session.store().next_attempt(context(), id).unwrap(),
            path: session
                .guard
                .root()
                .join(session.store().relative_directory(context(), id).unwrap()),
            read: None,
            snapshot: None,
            sequence: 0,
            _lease: cursor_lease(false).unwrap(),
        };
        session.store_mut().begin_attempt(cursor.attempt).unwrap();
        let permit = acquisition_lease(false, false).unwrap();
        session.running.push(id);
        let reply = session.send.clone();
        let guard = session.guard.clone();
        let control = session.control();
        let options = session.options;
        let memory = session._memory.clone();
        let active = session.active.clone();
        let pending = session.pending_completions.clone();
        let started = started_send.clone();
        let (release_send, release_receive) = mpsc::channel();
        if index != 0 {
            release.push(release_send);
        } else {
            drop(release_send);
        }
        pool()
            .send
            .send(Box::new(move || {
                started.send(()).unwrap();
                let _ = release_receive.recv();
                let mut completed = quantum(&guard, cursor, permit, options, &control, None);
                completed._pending = Some(CompletionLease::new(pending));
                completed._session = Some(SessionLease {
                    _memory: memory,
                    _active: active,
                });
                let _ = reply.send(completed);
            }))
            .unwrap();
    }
    for _ in 0..8 {
        started_receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
    let db = fail_scheduler_ingestion(&session);
    let started = Instant::now();
    while session.status().pending_completions == 0 {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    assert!(session.tick().is_err());
    session.control().cancel();
    let status = session.tick().unwrap();
    assert_eq!(status.state, State::StoreError);
    assert_eq!(status.in_flight, 7);
    assert_eq!(status.acquisitions_pending, 7);
    assert_eq!(status.pending_completions, 0);
    assert_eq!(status.global_acquisitions, 7);
    assert_eq!(status.global_cursors, 7);
    assert!(session.active.is_some());
    assert!(active::acquire(&session.guard).is_err());
    assert!(session.finalize_step().is_err());
    session.retry_store();
    assert_eq!(session.status().state, State::StoreError);
    for sender in release {
        sender.send(()).unwrap();
    }
    while session.status().in_flight != 0 {
        session.tick().unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    assert_eq!(session.status().state, State::CancelledStoreError);
    assert_eq!(session.status().global_acquisitions, 0);
    assert_eq!(session.status().global_cursors, 0);
    assert_eq!(session.status().pending_completions, 0);
    assert_eq!(session.status().abandoned_quanta, 8);
    assert!(active::acquire(&session.guard).is_ok());
    assert_eq!(session.store().latest_sealed().unwrap(), None);
    db.execute_batch("DROP TRIGGER scheduler_ingest_failure;")
        .unwrap();
}

#[test]
fn background_admission_keeps_two_native_slots_for_navigation() {
    let _serial = TEST_LOCK.lock().unwrap();
    let background: Vec<_> = (0..6)
        .map(|_| acquisition_lease(false, true).unwrap())
        .collect();
    assert!(acquisition_lease(false, true).is_none());
    let navigation = acquisition_lease(true, true).unwrap();
    let visible = acquisition_lease(true, true).unwrap();
    assert_eq!(ACQUISITIONS.load(Ordering::SeqCst), 8);
    assert!(acquisition_lease(true, true).is_none());
    drop((background, navigation, visible));
    assert_eq!(ACQUISITIONS.load(Ordering::SeqCst), 0);
    assert_eq!(BACKGROUND_ACQUISITIONS.load(Ordering::SeqCst), 0);
    let resumed: Vec<_> = (0..8)
        .map(|_| acquisition_lease(false, false).unwrap())
        .collect();
    assert!(acquisition_lease(false, false).is_none());
    drop(resumed);
}

#[test]
fn latest_navigation_uses_next_foreground_quantum_and_completed_commits_yield() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    for n in 0..12 {
        let child = root.join(format!("child-{n:02}"));
        fs::create_dir(&child).unwrap();
        fs::write(child.join("file"), b"abc").unwrap();
    }
    let store = Store::create(
        &temp.path().join("db"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    let control = session.control();
    session.tick().unwrap();
    control.pause(true);
    let started = Instant::now();
    while !session.running.is_empty() {
        session.tick().unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    let pending = session.store().pending_directories(context(), 16).unwrap();
    assert_eq!(pending.len(), 12);
    let last = *pending.last().unwrap();
    control.navigate(pending[0]);
    control.navigate(pending[1]);
    control.navigate(last);
    // A background turn remains payable even when another click arrives. The
    // next foreground quantum uses the latest selection, not the older clicks.
    session.fair.next = 4;
    control.pause(false);
    session.tick().unwrap();
    assert_eq!(session.running[1], last);
    assert_eq!(session.context(), context());
    control.pause(true);
    let started = Instant::now();
    while !session.running.is_empty() {
        let before = session.quanta;
        session.tick().unwrap();
        assert!(session.quanta - before <= COMMIT_QUANTA as u64);
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    control.pause(false);
    while session.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    assert_eq!(
        session.store().entry_count(context().generation).unwrap(),
        25
    );
    while session.finalize_step().unwrap().is_none() {}
    let quanta = session.quanta;
    control.navigate(last);
    session.tick().unwrap();
    assert_eq!(session.quanta, quanta);
}

#[test]
fn rapid_navigation_and_failed_capacity_do_not_consume_background_or_enrichment_turns() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    for n in 0..20 {
        let child = root.join(format!("child-{n:02}"));
        fs::create_dir(&child).unwrap();
        // Keep this task eligible for four complete service turns regardless
        // of the native quantum size; the assertion below tests fairness.
        for file in 0..4 * QUANTUM {
            fs::write(child.join(format!("file-{file:03}")), b"abc").unwrap();
        }
    }
    let store = Store::create(
        &temp.path().join("db"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new_with_options(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
        CollectionOptions {
            allocation: false,
            owners: false,
        },
    )
    .unwrap();
    let control = session.control();
    // Exactly one native slot can be admitted. Each tick then encounters a
    // real capacity refusal before returning to the service control lane.
    let held: Vec<_> = (0..7).map(|_| reserve(&ACQUISITIONS, 8).unwrap()).collect();
    let started = Instant::now();
    let drain = |session: &mut Session| {
        control.pause(true);
        while !session.running.is_empty() {
            session.tick().unwrap();
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::yield_now();
        }
    };
    session.tick().unwrap();
    drain(&mut session);
    let directories = session.store().pending_directories(context(), 16).unwrap();
    assert_eq!(directories.len(), 16);
    let visible = directories[14];
    let enrichment = directories[15];
    control.visible(Some(visible));
    control.enrich(Some(enrichment));
    session.fair = Fairness::default();
    let mut trace = Vec::new();
    for change in 0..16 {
        let latest = directories[change % 14];
        control.navigate(directories[(change + 1) % 14]);
        control.navigate(latest);
        control.pause(false);
        session.tick().unwrap();
        assert_eq!(session.running.len(), 1);
        let admitted = session.running[0];
        trace.push(if admitted == latest {
            0
        } else if admitted == visible {
            1
        } else if admitted == enrichment {
            3
        } else {
            2
        });
        drain(&mut session);
    }
    for window in trace.windows(8) {
        assert!(window.contains(&2), "background starved: {trace:?}");
        assert!(window.contains(&3), "enrichment starved: {trace:?}");
    }
    assert_eq!(trace, ROTATION.repeat(2));
    control.cancel();
    assert_eq!(session.tick().unwrap().state, State::Cancelled);
    drop(held);
}

#[test]
fn revisiting_a_listed_folder_keeps_its_attempt_and_resumes_pending_descendants() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    let descendant_path = root.join("selected").join("descendant");
    fs::create_dir_all(&descendant_path).unwrap();
    fs::write(descendant_path.join("file"), b"abc").unwrap();
    let store = Store::create(
        &temp.path().join("db"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    let control = session.control();
    let started = Instant::now();
    let drain = |session: &mut Session| {
        control.pause(true);
        while !session.running.is_empty() {
            session.tick().unwrap();
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::yield_now();
        }
    };
    session.tick().unwrap();
    drain(&mut session);
    let selected = session.store().pending_directories(context(), 16).unwrap()[0];
    control.navigate(selected);
    control.pause(false);
    session.tick().unwrap();
    assert_eq!(session.running, vec![selected]);
    drain(&mut session);
    assert!(
        !session
            .store()
            .directory_pending(context(), selected)
            .unwrap()
    );
    let descendant = session.store().pending_directories(context(), 16).unwrap()[0];
    assert_ne!(selected, descendant);
    control.navigate(selected);
    control.pause(false);
    session.tick().unwrap();
    assert_eq!(session.running, vec![descendant]);
    assert!(!session.parked.contains_key(&selected));
    assert_eq!(session.context(), context());
    drain(&mut session);
    control.pause(false);
    assert_eq!(session.tick().unwrap().state, State::Idle);
    assert_eq!(session.quanta, 3);
    assert_eq!(
        session.store().entry_count(context().generation).unwrap(),
        4
    );
    while session.finalize_step().unwrap().is_none() {}
}

#[test]
fn relative_navigation_maps_generation_ids_and_advances_known_ancestors_without_source_io() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0x66, 0xd800])
    };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![b'f', 0xff])
    };
    #[cfg(not(any(windows, unix)))]
    let name = OsString::from("selected");
    let target = PathBuf::from(&name).join("descendant");
    let observation = |name: &OsStr, kind| Observation {
        name: encode_name(name),
        kind,
        logical: None,
        allocated: None,
        modified: None,
        metadata_known: true,
    };
    let put = |store: &mut Store, c, parent, observations: &[Observation]| {
        let attempt = store.next_attempt(c, parent).unwrap();
        store.begin_attempt(attempt).unwrap();
        store
            .ingest(
                BatchHeader {
                    attempt,
                    sequence: 0,
                    end: Some(ListingEnd::Exhausted),
                },
                observations,
            )
            .unwrap();
    };
    let c0 = context();
    let mut c1 = c0;
    c1.generation = GenerationId(2);
    let make = |path: &str, c: AcquisitionContext| {
        let mut store = Store::create(
            &temp.path().join(path),
            c.inventory,
            c.source,
            Durability::Retained,
        )
        .unwrap();
        store.create_generation(c).unwrap();
        store
    };
    let mut old = make("g0", c0);
    put(
        &mut old,
        c0,
        EntryId(0),
        &[observation(&name, Kind::Directory)],
    );
    put(
        &mut old,
        c0,
        EntryId(1),
        &[observation(OsStr::new("descendant"), Kind::Directory)],
    );
    let old_target = old.navigation_directory(c0, &target).unwrap();
    assert_eq!(old_target, EntryId(2));
    let mut candidate = make("g1", c1);
    assert_eq!(
        candidate.navigation_directory(c1, &target).unwrap(),
        EntryId(0)
    );
    put(
        &mut candidate,
        c1,
        EntryId(0),
        &[
            observation(OsStr::new("unrelated"), Kind::File),
            observation(&name, Kind::Directory),
        ],
    );
    assert_eq!(
        candidate.navigation_directory(c1, &target).unwrap(),
        EntryId(2)
    );
    put(
        &mut candidate,
        c1,
        EntryId(2),
        &[observation(OsStr::new("descendant"), Kind::Directory)],
    );
    let candidate_target = candidate.navigation_directory(c1, &target).unwrap();
    assert_eq!(candidate_target, EntryId(3));
    assert_ne!(candidate_target, old_target);
    let before = candidate.entry_count(c1.generation).unwrap();
    assert_eq!(
        candidate.navigation_directory(c1, &target).unwrap(),
        candidate_target
    );
    assert_eq!(candidate.entry_count(c1.generation).unwrap(), before);
    assert!(candidate.navigation_directory(c0, &target).is_err());
    while !candidate.consolidate_step(c1, 512).unwrap() {}
    candidate.seal(c1).unwrap();
    assert_eq!(
        candidate.navigation_directory(c1, &target).unwrap(),
        candidate_target
    );
    // A readable sealed generation never becomes a writable acquisition.
    assert!(candidate.next_attempt(c1, candidate_target).is_err());
    let mut foreign = c1;
    foreign.source = SourceId(c1.source.0 + 1);
    assert!(candidate.navigation_directory(foreign, &target).is_err());
    foreign = c1;
    foreign.scan = ScanId(c1.scan.0 + 1);
    assert!(candidate.navigation_directory(foreign, &target).is_err());
    foreign = c1;
    foreign.run = RunEpoch(c1.run.0 + 1);
    assert!(candidate.navigation_directory(foreign, &target).is_err());
    assert!(
        candidate
            .navigation_directory(c1, std::path::Path::new("../outside"))
            .is_err()
    );
    let control = Control::default();
    control.navigate(EntryId(99));
    assert!(
        control
            .navigate_relative(PathBuf::from("../outside"))
            .is_err()
    );
    assert_eq!(control.0.lock().unwrap().navigation, Some(EntryId(99)));
    control.navigate_relative(target.clone()).unwrap();
    assert_eq!(control.0.lock().unwrap().navigation_relative, Some(target));
    control.navigate(EntryId(3));
    assert!(control.0.lock().unwrap().navigation_relative.is_none());
}

#[test]
fn native_measured_sizes_grow_before_seal_and_discovered_folders_remain_navigable() {
    use crate::inventory::navigation::Mode;
    const FILES: usize = 3 * QUANTUM;
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    for folder in ["foreground", "background"] {
        let path = root.join(folder);
        fs::create_dir(&path).unwrap();
        for n in 0..FILES {
            fs::write(path.join(format!("file-{n:03}")), vec![0u8; n + 1]).unwrap();
        }
    }
    let store = Store::create(
        &temp.path().join("db"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    let control = session.control();
    let view = ViewContext {
        inventory: context().inventory,
        source: context().source,
        generation: context().generation,
        scan: context().scan,
        validation: context().validation,
        view: ViewId(1),
        epoch: ViewEpoch(1),
        query: None,
        presentation: PresentationEpoch(1),
    };
    let key = |entry| EntryKey {
        inventory: context().inventory,
        generation: context().generation,
        entry,
    };
    let frame = |session: &Session, parent| {
        session
            .store()
            .navigation_frame(
                context(),
                view,
                Mode::Folder {
                    parent: key(parent),
                    after: None,
                },
                &root,
            )
            .unwrap()
    };
    // Admit one quantum per directory, then pause before any next quantum.
    // Native workers complete normally; the pauses make intermediate states
    // deterministic without adding a production slowdown or rescanning.
    let round = |session: &mut Session| {
        control.pause(false);
        session.tick().unwrap();
        control.pause(true);
        let started = Instant::now();
        loop {
            let status = session.tick().unwrap();
            if status.in_flight == 0 {
                assert_eq!(status.state, State::Paused);
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    round(&mut session);
    let discovered = frame(&session, EntryId(0));
    assert!(discovered.provisional);
    assert_eq!(discovered.rows.len(), 2);
    let foreground = discovered
        .rows
        .iter()
        .find(|row| row.path.file_name().unwrap() == "foreground")
        .unwrap()
        .key;
    assert!(
        session
            .store()
            .navigation_target(context(), view, foreground)
            .unwrap()
            .is_some()
    );
    control.navigate(foreground.entry);
    assert_eq!(control.0.lock().unwrap().navigation, Some(foreground.entry));
    round(&mut session);
    let first = frame(&session, foreground.entry);
    assert!(first.provisional);
    assert!(!first.rows.is_empty() && first.rows.len() <= QUANTUM);
    let known_bytes = |rows: &[crate::inventory::queries::Row]| {
        rows.iter()
            .map(|row| {
                assert_eq!(row.kind, Kind::File);
                let actual = fs::metadata(&row.path).unwrap().len();
                assert_eq!(row.logical, Some(actual));
                actual
            })
            .sum::<u64>()
    };
    let first_bytes = known_bytes(&first.rows);
    assert!(first_bytes > 0);
    assert_eq!(session.store().latest_complete().unwrap(), None);
    round(&mut session);
    let second = frame(&session, foreground.entry);
    assert!(second.provisional);
    assert!(second.rows.len() > first.rows.len() && second.rows.len() < FILES);
    assert!(known_bytes(&second.rows) > first_bytes);
    control.pause(false);
    let started = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    let finished = frame(&session, foreground.entry);
    assert!(!finished.provisional);
    assert_eq!(finished.coverage, ScopeCoverage::Complete);
    assert_eq!(finished.rows.len(), FILES);
    assert_eq!(
        known_bytes(&finished.rows),
        (FILES * (FILES + 1) / 2) as u64
    );
    assert_eq!(
        session.store().entry_count(context().generation).unwrap(),
        (2 * FILES + 3) as u64
    );
}

#[cfg(windows)]
#[test]
fn full_windows_names_cross_quanta_without_loss_or_payload_overflow() {
    use std::collections::BTreeSet;
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let mut expected = BTreeSet::new();
    for id in 0..(2 * QUANTUM + 7) {
        let name = format!("{id:04}{}", "é".repeat(251));
        fs::write(root.join(&name), b"abc").unwrap();
        expected.insert(encode_name(OsStr::new(&name)));
    }
    let guard = Guard::open(&root, Policy::default()).unwrap();
    let mut cursor = Cursor {
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
        path: root,
        read: None,
        snapshot: None,
        sequence: 0,
        _lease: cursor_lease(true).unwrap(),
    };
    let mut actual = BTreeSet::new();
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(30));
        let mut completed = quantum(
            &guard,
            cursor,
            acquisition_lease(true, false).unwrap(),
            Default::default(),
            &Control::default(),
            None,
        );
        assert!(completed.observations.len() <= QUANTUM);
        let bytes: usize = completed
            .observations
            .iter()
            .map(|row| row.name.encode().len() + 384)
            .sum();
        assert!(bytes <= BATCH_BYTES);
        assert_eq!(completed.errors.count, 0);
        for row in &completed.observations {
            assert_eq!(row.logical, Some(3));
            assert!(actual.insert(row.name.clone()), "duplicate native entry");
        }
        if let Some(end) = completed.end {
            assert_eq!(end, ListingEnd::Exhausted);
            break;
        }
        completed.cursor.sequence = completed.next_sequence().unwrap();
        cursor = completed.cursor;
    }
    assert_eq!(actual, expected);
}

#[test]
fn eight_actually_blocked_workers_are_not_replaced_by_source_switches() {
    let _serial = TEST_LOCK.lock().unwrap();
    let (started_tx, started) = mpsc::channel();
    let (finished_tx, finished) = mpsc::channel();
    let mut release = Vec::new();
    for _ in 0..8 {
        let permit = reserve(&ACQUISITIONS, 8).unwrap();
        let (tx, rx) = mpsc::channel();
        release.push(tx);
        let ready = started_tx.clone();
        let done = finished_tx.clone();
        pool()
            .send
            .send(Box::new(move || {
                ready.send(()).unwrap();
                rx.recv().unwrap();
                drop(permit);
                done.send(()).unwrap();
            }))
            .unwrap();
    }
    for _ in 0..8 {
        started.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let f = tempfile::tempdir().unwrap();
    let source = f.path().join("source");
    fs::create_dir(&source).unwrap();
    let store = Store::create(
        &f.path().join("db"),
        InventoryId(1),
        SourceId(1),
        Durability::Retained,
    )
    .unwrap();
    let mut s = Session::new(
        store,
        context(),
        Guard::open(&source, Policy::default()).unwrap(),
    )
    .unwrap();
    for _ in 0..16 {
        let state = s.tick().unwrap();
        assert_eq!(state.global_acquisitions, 8);
        assert_eq!(state.in_flight, 0);
    }
    for tx in release {
        tx.send(()).unwrap();
    }
    for _ in 0..8 {
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(ACQUISITIONS.load(Ordering::SeqCst), 0);
}

#[test]
fn integration_same_or_overlapping_root_is_serialized_and_job_keeps_lease() {
    let _serial = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    let child = root.join("child");
    std::fs::create_dir(&child).unwrap();
    let first = active::acquire(&Guard::open(&root, Policy::default()).unwrap()).unwrap();
    let held_by_job = first.clone();
    drop(first);
    assert!(active::acquire(&Guard::open(&root, Policy::default()).unwrap()).is_err());
    assert!(active::acquire(&Guard::open(&child, Policy::default()).unwrap()).is_err());
    drop(held_by_job);
    let replacement = active::acquire(&Guard::open(&root, Policy::default()).unwrap()).unwrap();
    drop(replacement);
    let store = Store::create(
        &tmp.path().join("db"),
        InventoryId(1),
        SourceId(1),
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    match Store::create(
        &tmp.path().join("other"),
        InventoryId(1),
        SourceId(1),
        Durability::Retained,
    ) {
        Ok(other) => assert!(
            Session::new(
                other,
                context(),
                Guard::open(&root, Policy::default()).unwrap()
            )
            .is_err()
        ),
        // Native Auto may admit only one SQLite working set under pressure.
        // This is another valid refusal, not permission to overlap the source.
        Err(crate::inventory::Error::MemoryAdmission { .. }) => {
            assert!(active::acquire(&Guard::open(&root, Policy::default()).unwrap()).is_err());
        }
        Err(e) => panic!("unexpected second-store failure: {e:?}"),
    }
    session.control().cancel();
    assert_eq!(session.tick().unwrap().state, State::Cancelled);
    assert!(active::acquire(&Guard::open(&root, Policy::default()).unwrap()).is_ok());
}

#[test]
fn lost_source_is_partial_with_bounded_native_diagnostics_and_no_retry() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), b"source").unwrap();
    let store = Store::create(
        &temp.path().join("db"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    let mut session = Session::new(
        store,
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    fs::rename(&root, temp.path().join("detached")).unwrap();
    let started = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::yield_now();
    }
    assert_eq!(session.acquisition_error_count(), 1);
    let failure = session.last_acquisition_error().unwrap();
    assert_eq!(failure.path, root);
    assert_eq!(failure.kind, io::ErrorKind::NotFound);
    assert_eq!(failure.operation, AcquisitionOperation::OpenDirectory);
    let quanta = session.status().quanta;
    // Restoration does not grant a new scan or retry an incomplete directory.
    fs::rename(temp.path().join("detached"), &root).unwrap();
    for _ in 0..3 {
        assert_eq!(session.tick().unwrap().state, State::Idle);
    }
    assert_eq!(session.status().quanta, quanta);
    let coverage = loop {
        if let Some(coverage) = session.finalize_step().unwrap() {
            break coverage;
        }
    };
    assert_eq!(coverage, ScopeCoverage::Partial);
    assert_eq!(session.store().latest_complete().unwrap(), None);
}

#[cfg(unix)]
#[test]
fn cached_directory_eof_cannot_certify_a_detached_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    let mut cursor = guard.read_dir(&root).unwrap();
    assert!(cursor.next().is_none());
    assert!(validate_directory_end(&guard, &root).is_ok());
    fs::rename(&root, temp.path().join("old")).unwrap();
    assert!(cursor.next().is_none());
    assert!(validate_directory_end(&guard, &root).is_err());
    fs::create_dir(&root).unwrap();
    assert!(validate_directory_end(&guard, &root).is_err());
}

#[test]
fn revoked_quantum_does_not_open_a_pending_native_directory() {
    let _serial = TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let guard = Guard::open(temp.path(), Policy::default()).unwrap();
    let control = Control::default();
    control.cancel();
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
        path: temp.path().join("unavailable-do-not-open"),
        read: None,
        snapshot: None,
        sequence: 0,
        _lease: cursor_lease(true).unwrap(),
    };
    let done = quantum(
        &guard,
        cursor,
        acquisition_lease(true, false).unwrap(),
        Default::default(),
        &control,
        None,
    );
    assert!(done.observations.is_empty());
    assert!(done.cursor.read.is_none());
    assert_eq!(done.end, Some(ListingEnd::Incomplete));
    // Opening the nonexistent directory would have recorded NotFound instead.
    assert_eq!(done.errors.count, 0);
}
