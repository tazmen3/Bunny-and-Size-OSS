use super::*;
use std::time::Instant;
pub(super) fn context_id(g: u64) -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(u128::MAX),
        source: SourceId(9),
        generation: GenerationId(g),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
pub(super) fn attempt_id(g: u64, d: u64, n: u64) -> AttemptKey {
    AttemptKey {
        task: TaskKey {
            context: context_id(g),
            directory: EntryId(d),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(n),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    }
}
pub(super) fn observation(i: u64, kind: Kind) -> Observation {
    Observation {
        name: NativeName::Posix(format!("n-{i:08}").into_bytes()),
        kind,
        logical: Some(i),
        allocated: None,
        modified: Some((-1, 999_999_999)),
        metadata_known: true,
    }
}
fn setup() -> (tempfile::TempDir, Store) {
    setup_with_durability(Durability::Retained)
}
fn setup_with_durability(durability: Durability) -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &temp.path().join("store"),
        InventoryId(u128::MAX),
        SourceId(9),
        durability,
    )
    .unwrap();
    s.create_generation(context_id(0)).unwrap();
    s.begin_attempt(attempt_id(0, 0, 1)).unwrap();
    (temp, s)
}
pub(super) fn batch(
    s: &mut Store,
    a: AttemptKey,
    seq: u64,
    end: Option<ListingEnd>,
    obs: &[Observation],
) -> Result<Ack> {
    s.ingest(
        BatchHeader {
            attempt: a,
            sequence: seq,
            end,
        },
        obs,
    )
}
pub(super) fn reconcile(s: &mut Store, a: AttemptKey) {
    while !s
        .reconcile_step(a, ComparisonScope::Comparable, 512)
        .unwrap()
    {}
}
pub(super) fn fold(s: &mut Store, g: u64) {
    while !s.consolidate_step(context_id(g), 512).unwrap() {}
}

#[test]
fn late_entries_and_more_than_65536_directory_tasks_survive_reopen() {
    let start = Instant::now();
    let (tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let count = 70_000u64;
    for (seq, lo) in (0..count).step_by(512).enumerate() {
        let rows: Vec<_> = (lo..(lo + 512).min(count))
            .map(|i| observation(i, Kind::Directory))
            .collect();
        batch(&mut s, a, seq as u64, None, &rows).unwrap();
    }
    assert_eq!(s.entry_count(GenerationId(0)).unwrap(), count + 1);
    assert_eq!(s.task_count(GenerationId(0)).unwrap(), count + 1);
    let last = s
        .lookup(
            GenerationId(0),
            EntryId(0),
            &observation(count - 1, Kind::Directory).name,
        )
        .unwrap()
        .unwrap();
    assert_eq!(last.key.entry.0, count);
    drop(s);
    let s = Store::reopen(&tmp.path().join("store"), Durability::Retained).unwrap();
    assert_eq!(s.task_count(GenerationId(0)).unwrap(), count + 1);
    assert!(
        s.lookup(
            GenerationId(0),
            EntryId(0),
            &observation(69_999, Kind::Directory).name
        )
        .unwrap()
        .unwrap()
        .present
    );
    eprintln!(
        "70k directory entries/tasks committed and reopened: {:?}",
        start.elapsed()
    );
}
#[test]
fn replay_is_idempotent_and_stale_attempt_is_fenced_before_replay() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    batch(&mut s, a, 0, None, &[observation(4, Kind::File)]).unwrap();
    let rev = s.revision(GenerationId(0)).unwrap();
    assert_eq!(
        batch(&mut s, a, 0, None, &vec![observation(7, Kind::File); 513]).unwrap(),
        Ack::Replay
    );
    assert_eq!(s.revision(GenerationId(0)).unwrap(), rev);
    assert!(matches!(
        batch(&mut s, a, 2, None, &[]),
        Err(Error::Batch(BatchRejection::SequenceGap))
    ));
    s.begin_attempt(attempt_id(0, 0, 2)).unwrap();
    assert!(matches!(
        batch(&mut s, a, 0, None, &[]),
        Err(Error::Batch(BatchRejection::StaleAttempt))
    ));
    assert_eq!(s.entry_count(GenerationId(0)).unwrap(), 2);
}
#[test]
fn eight_ingestions_roll_back_all_batches_on_statement_or_commit_failure() {
    let (temp, mut s) = setup();
    let generation = GenerationId(0);
    let root = attempt_id(0, 0, 1);
    batch(
        &mut s,
        root,
        0,
        Some(ListingEnd::Exhausted),
        &(1..=8)
            .map(|id| observation(id, Kind::Directory))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let attempts: [_; 8] = std::array::from_fn(|index| attempt_id(0, index as u64 + 1, 1));
    for attempt in attempts {
        s.begin_attempt(attempt).unwrap();
    }
    let revision = s.revision(generation).unwrap();
    let observations: [_; 8] =
        std::array::from_fn(|index| [observation(index as u64 + 9, Kind::File)]);
    let batches = || {
        std::array::from_fn::<_, 8, _>(|index| DirectoryBatch {
            header: BatchHeader {
                attempt: attempts[index],
                sequence: 0,
                end: Some(ListingEnd::Exhausted),
            },
            observations: &observations[index],
            evidence: &[],
            properties: &[],
            directory: None,
        })
    };
    let assert_rolled_back = |s: &Store| {
        assert!(s.db.is_autocommit());
        assert_eq!(s.entry_count(generation).unwrap(), 9);
        assert_eq!(s.revision(generation).unwrap(), revision);
        for attempt in attempts {
            let cursor = active(&s.db, attempt).unwrap();
            assert_eq!(cursor.next_sequence(), 0);
            assert_eq!(cursor.listing_end(), None);
        }
    };

    // Seven completions write before the last aborts the entire group.
    s.db.execute_batch(
        "CREATE TEMP TRIGGER fault_second BEFORE INSERT ON main.entries
         WHEN NEW.parent = X'0000000000000008'
         BEGIN SELECT RAISE(ABORT,'second completion fault'); END;",
    )
    .unwrap();
    let error = s.ingest_directory_group(&batches()).unwrap_err();
    assert!(error.to_string().contains("second completion fault"));
    assert_rolled_back(&s);
    s.db.execute_batch("DROP TRIGGER fault_second;").unwrap();

    // All eight mutations now succeed, but SQLite refuses the final commit.
    s.db.commit_hook(Some(|| true)).unwrap();
    assert!(matches!(
        s.ingest_directory_group(&batches()),
        Err(Error::Sql(_))
    ));
    s.db.commit_hook(None::<fn() -> bool>).unwrap();
    assert_rolled_back(&s);

    s.ingest_directory_group(&batches()).unwrap();
    assert_eq!(s.entry_count(generation).unwrap(), 17);
    let committed = s.revision(generation).unwrap();
    assert_eq!(committed.0, revision.0 + 8);
    for attempt in attempts {
        let cursor = active(&s.db, attempt).unwrap();
        assert_eq!(cursor.next_sequence(), 1);
        assert_eq!(cursor.listing_end(), Some(ListingEnd::Exhausted));
    }
    s.ingest_directory_group(&batches()).unwrap();
    assert_eq!(s.revision(generation).unwrap(), committed);
    drop(s);

    let s = Store::reopen(&temp.path().join("store"), Durability::Retained).unwrap();
    assert_eq!(s.entry_count(generation).unwrap(), 17);
    assert_eq!(s.revision(generation).unwrap(), committed);
}

#[test]
fn eight_attempt_admissions_roll_back_and_replays_durably() {
    let (temp, mut s) = setup();
    let context = context_id(0);
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        0,
        Some(ListingEnd::Exhausted),
        &(1..=8)
            .map(|id| observation(id, Kind::Directory))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let attempts: [_; 8] = std::array::from_fn(|index| attempt_id(0, 8 - index as u64, 1));
    let revision = s.revision(context.generation).unwrap();
    let synchronous: i64 =
        s.db.pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
    assert_eq!(synchronous, 2);
    let assert_rolled_back = |s: &Store| {
        assert!(s.db.is_autocommit());
        assert_eq!(s.revision(context.generation).unwrap(), revision);
        for attempt in attempts {
            assert!(
                s.directory_pending(context, attempt.task.directory)
                    .unwrap()
            );
            assert!(matches!(
                active(&s.db, attempt),
                Err(Error::Batch(BatchRejection::StaleAttempt))
            ));
        }
    };

    // Seven updates succeed; the last aborts the whole group.
    s.db.execute_batch(
        "CREATE TEMP TRIGGER fault_attempt BEFORE UPDATE OF key ON main.tasks
         WHEN NEW.directory = X'0000000000000008'
         BEGIN SELECT RAISE(ABORT,'second attempt fault'); END;",
    )
    .unwrap();
    let error = s.begin_attempt_group(&attempts).unwrap_err();
    assert!(error.to_string().contains("second attempt fault"));
    assert_rolled_back(&s);
    s.db.execute_batch("DROP TRIGGER fault_attempt;").unwrap();

    s.db.commit_hook(Some(|| true)).unwrap();
    assert!(matches!(
        s.begin_attempt_group(&attempts),
        Err(Error::Sql(_))
    ));
    s.db.commit_hook(None::<fn() -> bool>).unwrap();
    assert_rolled_back(&s);

    s.begin_attempt_group(&attempts).unwrap();
    let committed = s.revision(context.generation).unwrap();
    assert_eq!(committed.0, revision.0 + 8);
    drop(s);
    let mut s = Store::reopen(&temp.path().join("store"), Durability::Retained).unwrap();
    s.begin_attempt_group(&attempts).unwrap();
    assert_eq!(s.revision(context.generation).unwrap(), committed);
    for attempt in attempts {
        let cursor = active(&s.db, attempt).unwrap();
        assert_eq!(cursor.next_sequence(), 0);
        assert_eq!(cursor.listing_end(), None);
    }
}

#[test]
fn paired_attempt_admission_keeps_context_and_ancestor_fences() {
    let (_temp, mut s) = setup();
    let root = attempt_id(0, 0, 1);
    let replacement = attempt_id(0, 0, 2);
    let child = attempt_id(0, 1, 1);
    batch(&mut s, root, 0, None, &[observation(1, Kind::Directory)]).unwrap();
    let revision = s.revision(root.task.context.generation).unwrap();

    // Applying the child first would accept it under the old root attempt.
    assert!(matches!(
        s.begin_attempt_group(&[child, replacement]),
        Err(Error::Batch(BatchRejection::StaleAttempt))
    ));
    let mut foreign = child;
    foreign.task.context.validation = ValidationEpoch(2);
    for invalid in [&[child][..], &[child, child], &[child, foreign]] {
        assert!(matches!(
            s.begin_attempt_group(invalid),
            Err(Error::Contract(_))
        ));
    }
    assert_eq!(s.revision(root.task.context.generation).unwrap(), revision);
    assert!(
        s.directory_pending(root.task.context, child.task.directory)
            .unwrap()
    );
    assert_eq!(active(&s.db, root).unwrap().next_sequence(), 1);
}

#[test]
fn paired_attempt_admission_requires_retained_writer() {
    let (_temp, mut s) = setup_with_durability(Durability::Reconstructible);
    let root = attempt_id(0, 0, 1);
    batch(
        &mut s,
        root,
        0,
        Some(ListingEnd::Exhausted),
        &[
            observation(1, Kind::Directory),
            observation(2, Kind::Directory),
        ],
    )
    .unwrap();
    let attempts = [attempt_id(0, 1, 1), attempt_id(0, 2, 1)];
    assert!(!s.supports_grouped_attempts());
    assert!(matches!(
        s.begin_attempt_group(&attempts),
        Err(Error::Contract(
            "attempt group requires an admitted candidate writer"
        ))
    ));
    for attempt in attempts {
        assert!(
            s.directory_pending(root.task.context, attempt.task.directory)
                .unwrap()
        );
        s.begin_attempt(attempt).unwrap();
    }
}

#[test]
fn stable_single_batch_eof_reconciles_only_when_all_children_are_current() {
    let (_tmp, mut s) = setup();
    s.enable_evidence().unwrap();
    let directory = sortsize_core::acquisition::Stamp {
        properties: Default::default(),
        identity: None,
        created: None,
        modified: None,
        length: 0,
        directory: true,
        regular: false,
    };
    let retained = observation(2, Kind::File);
    let omitted = observation(9, Kind::File);
    let eof = |attempt| BatchHeader {
        attempt,
        sequence: 0,
        end: Some(ListingEnd::Exhausted),
    };
    s.ingest_directory_properties(
        eof(attempt_id(0, 0, 1)),
        &[retained.clone(), omitted.clone()],
        &[],
        &[],
        Some(&directory),
    )
    .unwrap();
    assert!(s.reconcile_next(context_id(0)).unwrap());

    let next = attempt_id(0, 0, 2);
    s.begin_attempt(next).unwrap();
    // A duplicate in the batch cannot mask an omitted old child.
    s.ingest_directory_properties(
        eof(next),
        &[retained.clone(), retained],
        &[],
        &[],
        Some(&directory),
    )
    .unwrap();
    let present = |s: &Store| {
        s.lookup(GenerationId(0), EntryId(0), &omitted.name)
            .unwrap()
            .unwrap()
            .present
    };
    assert!(present(&s));
    assert!(!s.reconcile_next(context_id(0)).unwrap());
    assert!(!present(&s));
    assert!(s.reconcile_next(context_id(0)).unwrap());
}

#[test]
fn eof_alone_never_removes_and_bounded_sweep_resumes_after_reopen() {
    let (tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let rows: Vec<_> = (0..400).map(|i| observation(i, Kind::File)).collect();
    batch(&mut s, a, 0, Some(ListingEnd::Exhausted), &rows).unwrap();
    reconcile(&mut s, a);
    let b = attempt_id(0, 0, 2);
    s.begin_attempt(b).unwrap();
    batch(
        &mut s,
        b,
        0,
        Some(ListingEnd::Exhausted),
        &[observation(399, Kind::File)],
    )
    .unwrap();
    assert!(
        s.lookup(GenerationId(0), EntryId(0), &rows[0].name)
            .unwrap()
            .unwrap()
            .present
    );
    assert!(
        s.reconcile_step(b, ComparisonScope::Uncertain, 100)
            .is_err()
    );
    assert!(
        !s.reconcile_step(b, ComparisonScope::Comparable, 100)
            .unwrap()
    );
    drop(s);
    let mut s = Store::reopen(&tmp.path().join("store"), Durability::Retained).unwrap();
    assert!(
        !s.lookup(GenerationId(0), EntryId(0), &rows[0].name)
            .unwrap()
            .unwrap()
            .present
    );
    reconcile(&mut s, b);
    assert!(
        s.lookup(GenerationId(0), EntryId(0), &rows[399].name)
            .unwrap()
            .unwrap()
            .present
    );
    assert!(
        !s.lookup(GenerationId(0), EntryId(0), &rows[398].name)
            .unwrap()
            .unwrap()
            .present
    );
    fold(&mut s, 0);
    assert_eq!(s.totals(GenerationId(0)).unwrap().logical, Some(399));
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Complete);
}
#[test]
fn refused_stat_preserves_presence_and_partial_listing_cannot_sweep() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let o = observation(9, Kind::File);
    batch(&mut s, a, 0, None, std::slice::from_ref(&o)).unwrap();
    let b = attempt_id(0, 0, 2);
    s.begin_attempt(b).unwrap();
    let mut refused = o.clone();
    refused.metadata_known = false;
    refused.kind = Kind::Unknown;
    batch(&mut s, b, 0, Some(ListingEnd::Incomplete), &[refused]).unwrap();
    assert!(
        s.reconcile_step(b, ComparisonScope::Comparable, 200)
            .is_err()
    );
    let got = s
        .lookup(GenerationId(0), EntryId(0), &o.name)
        .unwrap()
        .unwrap();
    assert!(got.present);
    assert!(!got.metadata_known);
    assert_eq!(got.logical, None);
    fold(&mut s, 0);
    assert_eq!(s.totals(GenerationId(0)).unwrap().unknown, 1);
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Partial);
}
#[test]
fn generations_are_independent_and_partial_seal_publishes_atomically() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    batch(
        &mut s,
        a,
        0,
        Some(ListingEnd::Exhausted),
        &[observation(5, Kind::File)],
    )
    .unwrap();
    reconcile(&mut s, a);
    fold(&mut s, 0);
    assert_eq!(s.latest_sealed().unwrap(), None);
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Complete);
    let old = s.revision(GenerationId(0)).unwrap();
    s.create_generation(context_id(1)).unwrap();
    let b = attempt_id(1, 0, 1);
    s.begin_attempt(b).unwrap();
    batch(
        &mut s,
        b,
        0,
        Some(ListingEnd::Incomplete),
        &[observation(2, Kind::File)],
    )
    .unwrap();
    assert_eq!(s.latest_sealed().unwrap(), Some(GenerationId(0)));
    assert!(s.seal(context_id(1)).is_err());
    fold(&mut s, 1);
    assert_eq!(s.seal(context_id(1)).unwrap(), ScopeCoverage::Partial);
    assert_eq!(s.latest_sealed().unwrap(), Some(GenerationId(1)));
    assert_eq!(s.totals(GenerationId(0)).unwrap().logical, Some(5));
    assert_eq!(s.revision(GenerationId(0)).unwrap(), old);
    assert!(batch(&mut s, a, 0, None, &[]).is_err());
    assert!(
        s.db.execute(
            "UPDATE entries SET logical=?1 WHERE generation=?2",
            params![unsigned(1), unsigned(0)]
        )
        .is_err()
    );
}
#[test]
fn disk_full_rolls_back_entries_tasks_ack_and_contributions() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let revision = s.revision(GenerationId(0)).unwrap();
    let pages: i64 =
        s.db.query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
    s.db.pragma_update(None, "max_page_count", pages + 2)
        .unwrap();
    let mut o = observation(5, Kind::Directory);
    o.name = NativeName::Posix(vec![b'x'; 60_000]);
    let error = batch(&mut s, a, 0, None, &[o]).unwrap_err();
    assert!(
        matches!(error,Error::Sql(ref e) if e.sqlite_error_code()==Some(rusqlite::ErrorCode::DiskFull))
    );
    assert_eq!(s.entry_count(GenerationId(0)).unwrap(), 1);
    assert_eq!(s.task_count(GenerationId(0)).unwrap(), 1);
    assert_eq!(s.revision(GenerationId(0)).unwrap(), revision);
    assert_eq!(active(&s.db, a).unwrap().next_sequence(), 0);
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM contributions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    s.db.pragma_update(None, "max_page_count", 1_000_000)
        .unwrap();
    batch(&mut s, a, 0, None, &[observation(8, Kind::Directory)]).unwrap();
    assert_eq!(s.task_count(GenerationId(0)).unwrap(), 2);
}
#[test]
fn overflow_never_wraps_and_mutation_invalidates_partial_fold() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let mut o = observation(1, Kind::File);
    o.logical = Some(u64::MAX);
    batch(&mut s, a, 0, None, &[o]).unwrap();
    assert!(!s.consolidate_step(context_id(0), 1).unwrap());
    batch(
        &mut s,
        a,
        1,
        Some(ListingEnd::Exhausted),
        &[observation(2, Kind::File)],
    )
    .unwrap();
    reconcile(&mut s, a);
    fold(&mut s, 0);
    let totals = s.totals(GenerationId(0)).unwrap();
    assert!(totals.overflow);
    assert_eq!(totals.logical, None);
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Partial);
}
#[test]
fn native_lock_and_future_format_are_not_bypassed() {
    let (tmp, s) = setup();
    assert!(Store::reopen(&tmp.path().join("store"), Durability::Retained).is_err());
    s.db.pragma_update(None, "user_version", 99).unwrap();
    s.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(s);
    let path = tmp.path().join("store/inventory.db");
    let before = std::fs::read(&path).unwrap();
    assert!(Store::reopen(&tmp.path().join("store"), Durability::Retained).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}
#[test]
fn parent_attempt_change_fences_descendants_and_old_contributions() {
    let (_tmp, mut s) = setup();
    let root = attempt_id(0, 0, 1);
    let dir = observation(1, Kind::Directory);
    batch(&mut s, root, 0, None, std::slice::from_ref(&dir)).unwrap();
    let child = attempt_id(0, 1, 1);
    s.begin_attempt(child).unwrap();
    batch(&mut s, child, 0, None, &[observation(10, Kind::File)]).unwrap();
    fold(&mut s, 0);
    assert_eq!(s.totals(GenerationId(0)).unwrap().logical, Some(10));
    let root2 = attempt_id(0, 0, 2);
    s.begin_attempt(root2).unwrap();
    assert!(batch(&mut s, child, 1, None, &[]).is_err());
    fold(&mut s, 0);
    assert_eq!(s.totals(GenerationId(0)).unwrap().logical, Some(0));
    batch(&mut s, root2, 0, None, &[dir]).unwrap();
    assert!(batch(&mut s, child, 1, None, &[]).is_err());
    assert!(s.begin_attempt(child).is_err());
    let child2 = attempt_id(0, 1, 2);
    s.begin_attempt(child2).unwrap();
    batch(&mut s, child2, 0, None, &[observation(7, Kind::File)]).unwrap();
    fold(&mut s, 0);
    assert_eq!(s.totals(GenerationId(0)).unwrap().logical, Some(7));
}
#[test]
fn batch_bounds_and_native_metadata_are_exact() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let mut o = observation(1, Kind::File);
    o.name = NativeName::Windows(vec![0x61, 0xd800, 0x62]);
    o.logical = Some(u64::MAX);
    batch(&mut s, a, 0, None, std::slice::from_ref(&o)).unwrap();
    assert_eq!(
        s.lookup(GenerationId(0), EntryId(0), &o.name)
            .unwrap()
            .unwrap()
            .logical,
        Some(u64::MAX)
    );
    assert!(batch(&mut s, a, 1, None, &vec![o.clone(); 513]).is_err());
    o.name = NativeName::Opaque(vec![1; MAX_BYTES]);
    assert!(batch(&mut s, a, 1, None, &[o]).is_err());
    assert_eq!(active(&s.db, a).unwrap().next_sequence(), 1);
    let date: (i64, i64) =
        s.db.query_row(
            "SELECT modified_s,modified_ns FROM entries WHERE id=?1",
            [unsigned(1)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(date, (-1, 999_999_999));
}

#[test]
fn completed_child_does_not_certify_new_parent_attempt() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let dir = observation(1, Kind::Directory);
    batch(
        &mut s,
        a,
        0,
        Some(ListingEnd::Exhausted),
        std::slice::from_ref(&dir),
    )
    .unwrap();
    reconcile(&mut s, a);
    let child = attempt_id(0, 1, 1);
    s.begin_attempt(child).unwrap();
    batch(
        &mut s,
        child,
        0,
        Some(ListingEnd::Exhausted),
        &[observation(10, Kind::File)],
    )
    .unwrap();
    reconcile(&mut s, child);
    fold(&mut s, 0);
    let b = attempt_id(0, 0, 2);
    s.begin_attempt(b).unwrap();
    batch(&mut s, b, 0, Some(ListingEnd::Exhausted), &[dir]).unwrap();
    reconcile(&mut s, b);
    fold(&mut s, 0);
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Partial);
    assert_eq!(s.latest_complete().unwrap(), None);
}
#[test]
fn removed_subtree_cannot_keep_a_complete_generation_pending() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    batch(&mut s, a, 0, None, &[observation(1, Kind::Directory)]).unwrap();
    let child = attempt_id(0, 1, 1);
    s.begin_attempt(child).unwrap();
    batch(&mut s, child, 0, None, &[observation(2, Kind::Directory)]).unwrap();
    let b = attempt_id(0, 0, 2);
    s.begin_attempt(b).unwrap();
    batch(&mut s, b, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut s, b);
    fold(&mut s, 0);
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Complete);
    assert_eq!(s.latest_complete().unwrap(), Some(GenerationId(0)));
}
#[test]
fn materialized_generation_budget_and_complete_reference_are_preserved() {
    let (_tmp, mut s) = setup();
    assert!(s.create_generation(context_id(1)).is_err());
    let a = attempt_id(0, 0, 1);
    batch(&mut s, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut s, a);
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    for g in 1..3 {
        s.create_generation(context_id(g)).unwrap();
        s.begin_attempt(attempt_id(g, 0, 1)).unwrap();
        fold(&mut s, g);
        assert_eq!(s.seal(context_id(g)).unwrap(), ScopeCoverage::Partial);
    }
    assert!(s.create_generation(context_id(3)).is_err());
    assert_eq!(s.latest_complete().unwrap(), Some(GenerationId(0)));
    assert_eq!(s.latest_sealed().unwrap(), Some(GenerationId(2)));
}

#[test]
fn symbolic_and_special_entries_do_not_create_tasks_or_regular_file_totals() {
    let (_tmp, mut s) = setup();
    let a = attempt_id(0, 0, 1);
    let link = observation(1, Kind::Symlink);
    let special = observation(2, Kind::Special);
    batch(
        &mut s,
        a,
        0,
        Some(ListingEnd::Exhausted),
        &[link.clone(), special],
    )
    .unwrap();
    reconcile(&mut s, a);
    fold(&mut s, 0);
    assert_eq!(s.task_count(GenerationId(0)).unwrap(), 1);
    assert_eq!(s.totals(GenerationId(0)).unwrap().logical, Some(0));
    let got = s
        .lookup(GenerationId(0), EntryId(0), &link.name)
        .unwrap()
        .unwrap();
    assert_eq!(got.kind, Kind::Symlink);
    assert_eq!(got.modified, link.modified);
    assert_eq!(s.seal(context_id(0)).unwrap(), ScopeCoverage::Complete);
}
