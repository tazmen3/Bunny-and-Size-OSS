//! Deterministic contract fixtures; not a SQLite/crash/native-I/O qualification.
use sortsize_core::inventory_contract::*;

fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(1),
        generation: GenerationId(2),
        source: SourceId(3),
        scan: ScanId(4),
        policy: PolicyId(5),
        mount: MountEpoch(6),
        run: RunEpoch(7),
        validation: ValidationEpoch(8),
    }
}

fn attempt() -> AttemptKey {
    AttemptKey {
        task: TaskKey {
            context: context(),
            directory: EntryId(9),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(10),
        owner: OwnerEpoch(11),
        branch: BranchRevision(12),
    }
}

fn batch(sequence: u64, end: Option<ListingEnd>) -> BatchHeader {
    BatchHeader {
        attempt: attempt(),
        sequence,
        end,
    }
}

fn commit(cursor: &mut BatchCursor, header: BatchHeader) {
    let BatchDecision::Apply { next } = cursor.prepare(header).unwrap() else {
        panic!("new batch expected");
    };
    *cursor = next;
}

fn complete_directory() -> DirectoryEvidence {
    DirectoryEvidence {
        listing: ListingState::Exhausted,
        reconciliation: Completion::Complete,
        descendants: ScopeCoverage::Complete,
        required_metadata: ScopeCoverage::Complete,
        aggregates: Completion::Complete,
    }
}

fn complete_query() -> QueryEvidence {
    QueryEvidence {
        source: ScopeCoverage::Complete,
        recording: RecordingCoverage::Complete,
        generation: GenerationState::Sealed,
        evaluation: EvaluationState::Complete,
        unevaluable: 0,
        results: ResultAvailability::AllAccessible,
    }
}

fn view() -> ViewContext {
    let c = context();
    ViewContext {
        inventory: c.inventory,
        generation: c.generation,
        source: c.source,
        scan: c.scan,
        validation: c.validation,
        view: ViewId(1),
        epoch: ViewEpoch(1),
        query: Some(QueryId(1)),
        presentation: PresentationEpoch(1),
    }
}

#[test]
fn entry_numbers_do_not_transfer_selection_between_generations_or_inventories() {
    let key = EntryKey {
        inventory: InventoryId(1),
        generation: GenerationId(1),
        entry: EntryId(u64::MAX),
    };
    assert_ne!(
        key,
        EntryKey {
            generation: GenerationId(2),
            ..key
        }
    );
    assert_ne!(
        key,
        EntryKey {
            inventory: InventoryId(2),
            ..key
        }
    );
    assert_ne!(
        key,
        EntryKey {
            entry: EntryId(7),
            ..key
        }
    );
}

#[test]
fn rollback_does_not_advance_sequence_or_acknowledge_data() {
    let mut committed = BatchCursor::start(attempt());
    let prepared = committed.prepare(batch(0, None)).unwrap();
    // A failed transaction discards its candidate; there is no UI acknowledgement.
    assert_eq!(committed.next_sequence(), 0);
    assert_eq!(committed.prepare(batch(0, None)).unwrap(), prepared);
    commit(&mut committed, batch(0, None));
    assert_eq!(committed.next_sequence(), 1);
}

#[test]
fn lost_ack_and_reordered_batches_do_not_duplicate_the_oracle_total() {
    let mut cursor = BatchCursor::start(attempt());
    let mut total = 0_u64;
    // Independent input oracle: two distinct observations of 7 and 23 bytes.
    // The transcript includes a gap, lost acks, retries, EOF replay and a late batch.
    let transcript = [
        (1, 23, None),
        (0, 7, None),
        (0, 7, None),
        (1, 23, Some(ListingEnd::Exhausted)),
        (1, 23, Some(ListingEnd::Exhausted)),
        (0, 7, None),
        (2, 99, None),
    ];
    let mut rejected = 0;
    for (seq, size, end) in transcript {
        match cursor.prepare(batch(seq, end)) {
            Ok(BatchDecision::Apply { next }) => {
                total += size;
                cursor = next;
            }
            Ok(BatchDecision::Replay) => {}
            Err(_) => rejected += 1,
        }
    }
    assert_eq!(total, 7 + 23);
    assert_eq!(rejected, 2);
    assert_eq!(cursor.next_sequence(), 2);
    assert_eq!(cursor.listing_end(), Some(ListingEnd::Exhausted));
}

#[test]
fn every_acquisition_identity_dimension_fences_late_data_even_on_replay() {
    let c = context();
    let contexts = [
        AcquisitionContext {
            inventory: InventoryId(99),
            ..c
        },
        AcquisitionContext {
            generation: GenerationId(99),
            ..c
        },
        AcquisitionContext {
            source: SourceId(99),
            ..c
        },
        AcquisitionContext {
            scan: ScanId(99),
            ..c
        },
        AcquisitionContext {
            policy: PolicyId(99),
            ..c
        },
        AcquisitionContext {
            mount: MountEpoch(99),
            ..c
        },
        AcquisitionContext {
            run: RunEpoch(99),
            ..c
        },
        AcquisitionContext {
            validation: ValidationEpoch(99),
            ..c
        },
    ];
    let mut cursor = BatchCursor::start(attempt());
    commit(&mut cursor, batch(0, None));
    for context in contexts {
        let mut header = batch(0, None);
        header.attempt.task.context = context;
        assert_eq!(cursor.prepare(header), Err(BatchRejection::StaleAttempt));
    }
    let a = attempt();
    for altered in [
        AttemptKey {
            attempt: AttemptId(99),
            ..a
        },
        AttemptKey {
            owner: OwnerEpoch(99),
            ..a
        },
        AttemptKey {
            branch: BranchRevision(99),
            ..a
        },
        AttemptKey {
            task: TaskKey {
                directory: EntryId(99),
                ..a.task
            },
            ..a
        },
        AttemptKey {
            task: TaskKey {
                kind: TaskKind::Reconcile,
                ..a.task
            },
            ..a
        },
    ] {
        assert_eq!(
            cursor.prepare(BatchHeader {
                attempt: altered,
                ..batch(0, None)
            }),
            Err(BatchRejection::StaleAttempt)
        );
    }
}

#[test]
fn eof_cannot_skip_a_batch_and_no_new_batch_follows_eof() {
    let mut cursor = BatchCursor::start(attempt());
    assert_eq!(
        cursor.prepare(batch(1, Some(ListingEnd::Exhausted))),
        Err(BatchRejection::SequenceGap)
    );
    commit(&mut cursor, batch(0, Some(ListingEnd::Exhausted)));
    assert_eq!(
        cursor.prepare(batch(0, Some(ListingEnd::Exhausted))),
        Ok(BatchDecision::Replay)
    );
    assert_eq!(
        cursor.prepare(batch(1, None)),
        Err(BatchRejection::ListingAlreadyEnded)
    );
}

#[test]
fn a_new_listing_attempt_does_not_inherit_previous_seen_set_or_watermark() {
    let mut old = BatchCursor::start(attempt());
    commit(&mut old, batch(0, Some(ListingEnd::Incomplete)));
    let next_attempt = AttemptKey {
        attempt: AttemptId(20),
        ..attempt()
    };
    let new = BatchCursor::start(next_attempt);
    assert_eq!(new.next_sequence(), 0);
    assert_eq!(new.listing_end(), None);
    assert_eq!(
        new.prepare(batch(0, None)),
        Err(BatchRejection::StaleAttempt)
    );
    assert!(matches!(
        new.prepare(BatchHeader {
            attempt: next_attempt,
            ..batch(0, None)
        }),
        Ok(BatchDecision::Apply { .. })
    ));
}

#[test]
fn eof_reconciliation_descendants_fields_and_totals_are_independent() {
    let full = complete_directory();
    assert_eq!(full.coverage(), ScopeCoverage::Complete);
    for partial in [
        DirectoryEvidence {
            listing: ListingState::InProgress,
            ..full
        },
        DirectoryEvidence {
            listing: ListingState::Incomplete,
            ..full
        },
        DirectoryEvidence {
            reconciliation: Completion::Pending,
            ..full
        },
        DirectoryEvidence {
            descendants: ScopeCoverage::Partial,
            ..full
        },
        DirectoryEvidence {
            required_metadata: ScopeCoverage::Unknown,
            ..full
        },
        DirectoryEvidence {
            aggregates: Completion::Pending,
            ..full
        },
    ] {
        assert_eq!(partial.coverage(), ScopeCoverage::Partial);
    }
    assert_eq!(
        DirectoryEvidence {
            listing: ListingState::NotStarted,
            ..full
        }
        .coverage(),
        ScopeCoverage::Unknown
    );
}

#[test]
fn absence_requires_native_eof_and_comparable_scope_not_successful_stat() {
    let full = complete_directory();
    // Presence with a refused property still proves the name was listed.
    let stat_denied = DirectoryEvidence {
        required_metadata: ScopeCoverage::Partial,
        ..full
    };
    assert!(stat_denied.permits_absence_inference(ComparisonScope::Comparable));
    assert_eq!(stat_denied.coverage(), ScopeCoverage::Partial);
    for listing in [
        ListingState::NotStarted,
        ListingState::InProgress,
        ListingState::Incomplete,
    ] {
        assert!(
            !DirectoryEvidence { listing, ..full }
                .permits_absence_inference(ComparisonScope::Comparable)
        );
    }
    assert!(!full.permits_absence_inference(ComparisonScope::Uncertain));
    assert!(!full.permits_absence_inference(ComparisonScope::Incompatible));
}

#[test]
fn sealing_a_partial_generation_does_not_make_a_query_complete() {
    let full = complete_query();
    assert_eq!(full.completeness(), QueryCompleteness::Complete);
    for partial in [
        QueryEvidence {
            source: ScopeCoverage::Unknown,
            ..full
        },
        QueryEvidence {
            source: ScopeCoverage::Partial,
            ..full
        },
        QueryEvidence {
            recording: RecordingCoverage::Partial,
            ..full
        },
        QueryEvidence {
            generation: GenerationState::Live,
            ..full
        },
        QueryEvidence {
            evaluation: EvaluationState::InProgress,
            ..full
        },
        QueryEvidence {
            evaluation: EvaluationState::Cancelled,
            ..full
        },
        QueryEvidence {
            evaluation: EvaluationState::Failed,
            ..full
        },
        QueryEvidence {
            unevaluable: 1,
            ..full
        },
        QueryEvidence {
            results: ResultAvailability::Truncated,
            ..full
        },
    ] {
        assert_eq!(partial.completeness(), QueryCompleteness::Partial);
    }
}

#[test]
fn no_usable_recording_or_results_is_unavailable_even_if_source_is_complete() {
    let full = complete_query();
    assert_eq!(
        QueryEvidence {
            recording: RecordingCoverage::Unavailable,
            ..full
        }
        .completeness(),
        QueryCompleteness::Unavailable
    );
    assert_eq!(
        QueryEvidence {
            results: ResultAvailability::Unavailable,
            ..full
        }
        .completeness(),
        QueryCompleteness::Unavailable
    );
}

#[test]
fn all_six_modes_have_stable_keys_and_a_byte_guard() {
    let modes = [
        MemoryMode::Auto,
        MemoryMode::Entries50k,
        MemoryMode::Entries250k,
        MemoryMode::Entries1m,
        MemoryMode::Entries5m,
        MemoryMode::UnlimitedCount,
    ];
    for mode in modes {
        assert_eq!(MemoryMode::from_key(mode.key()), Some(mode));
        let budget = CacheBudget {
            entries: mode.entry_limit(0),
            bytes: 0,
        };
        assert!(!budget.admits(0, 0, 1, 1));
    }
    assert_eq!(
        MemoryMode::from_key("Sans limite").unwrap_or_default(),
        MemoryMode::Auto
    );
}

#[test]
fn both_memory_limits_and_overflow_are_enforced_even_without_count_limit() {
    let budget = CacheBudget {
        entries: Some(2),
        bytes: 100,
    };
    assert!(budget.admits(1, 50, 1, 50));
    assert!(!budget.admits(1, 50, 2, 0));
    assert!(!budget.admits(1, 50, 1, 51));
    let unlimited = CacheBudget {
        entries: None,
        bytes: 100,
    };
    assert!(unlimited.admits(5_000_000, 50, 1, 50));
    assert!(!unlimited.admits(0, 100, 1, 1));
    assert!(
        !CacheBudget {
            entries: None,
            bytes: u64::MAX
        }
        .admits(0, u64::MAX, 0, 1)
    );
    assert!(!unlimited.admits(u64::MAX, 0, 1, 0));
}

#[test]
fn every_view_dimension_rejects_late_publications_without_changing_current_revision() {
    let v = view();
    let contexts = [
        ViewContext {
            inventory: InventoryId(99),
            ..v
        },
        ViewContext {
            generation: GenerationId(99),
            ..v
        },
        ViewContext {
            source: SourceId(99),
            ..v
        },
        ViewContext {
            scan: ScanId(99),
            ..v
        },
        ViewContext {
            validation: ValidationEpoch(99),
            ..v
        },
        ViewContext {
            view: ViewId(99),
            ..v
        },
        ViewContext {
            epoch: ViewEpoch(99),
            ..v
        },
        ViewContext {
            query: Some(QueryId(99)),
            ..v
        },
        ViewContext { query: None, ..v },
        ViewContext {
            presentation: PresentationEpoch(99),
            ..v
        },
    ];
    let mut guard = ViewGuard::new(v);
    assert!(guard.accept_snapshot(ViewStamp {
        context: v,
        revision: Revision(1)
    }));
    for context in contexts {
        assert!(!guard.accept_snapshot(ViewStamp {
            context,
            revision: Revision(99)
        }));
        assert_eq!(guard.applied(), Some(Revision(1)));
    }
}

#[test]
fn a_lost_or_reordered_delta_requires_replacement_without_partial_application() {
    let mut guard = ViewGuard::new(view());
    let stamp = |revision| ViewStamp {
        context: view(),
        revision: Revision(revision),
    };
    assert!(!guard.accept_delta(Revision(0), stamp(1)));
    assert!(guard.accept_snapshot(stamp(0)));
    assert!(!guard.accept_snapshot(stamp(0)));
    assert!(!guard.accept_delta(Revision(1), stamp(2)));
    assert_eq!(guard.applied(), Some(Revision(0)));
    assert!(guard.accept_snapshot(stamp(3)));
    assert!(!guard.accept_snapshot(stamp(2)));
    assert!(!guard.accept_delta(Revision(3), stamp(3)));
    assert!(guard.accept_delta(Revision(3), stamp(4)));
    assert_eq!(guard.applied(), Some(Revision(4)));
}
