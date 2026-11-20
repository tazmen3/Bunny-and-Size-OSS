//! Opt-in writer diagnostics. Fixed-size counters are isolated by OS thread.
use rusqlite::Transaction;
use std::{cell::RefCell, sync::OnceLock, time::Instant};

macro_rules! phases {
    ($($phase:ident => $label:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub(crate) enum Phase {
            $($phase),+
        }

        pub(crate) const LABELS: &[&str] = &[$($label),+];
    };
}

phases! {
    DiskAdmission => "disk_admission",
    PendingDirectories => "pending_directories",
    NavigationDirectory => "navigation_directory",
    DirectoryPending => "directory_pending",
    RelativeDirectory => "relative_directory",
    NextAttempt => "next_attempt",
    Attempt => "attempt",
    AttemptGroup => "attempt_group",
    AttemptValidate => "attempt_validate",
    AttemptWrite => "attempt_write",
    AttemptBegin => "attempt_begin",
    AttemptTaskLookup => "attempt_task_lookup",
    AttemptReplayCheck => "attempt_replay_check",
    AttemptMutate => "attempt_mutate",
    AttemptCommit => "attempt_commit",
    Ingest => "ingest",
    IngestWrite => "ingest_write",
    IngestCommit => "ingest_commit",
    ReconcileInline => "reconcile_inline",
    ReconcileDeferred => "reconcile_deferred",
    ReconcileCommit => "reconcile_commit",
    Consolidate => "consolidate",
    ConsolidateCommit => "consolidate_commit",
    Seal => "seal",
    SealCommit => "seal_commit",
    Publish => "publish",
    CreateStage => "create_stage",
    Extract => "extract",
    ExtractCommit => "extract_commit",
    CompareObservation => "compare_observation",
    CreateModel => "create_model",
    BuildKeys => "build_keys",
    BuildKeysCommit => "build_keys_commit",
    BuildRowsAndIndexes => "build_rows_and_indexes",
    BuildRowsAndIndexesCommit => "build_rows_and_indexes_commit",
    IntegrityCheck => "integrity_check",
    IntegrityExtracted => "integrity_extracted",
    IntegrityModel => "integrity_model",
    WalCheckpoint => "wal_checkpoint",
    StandaloneJournal => "standalone_journal",
    SyncStage => "sync_stage",
    Certify => "certify",
    CertifyInspect => "certify_inspect",
    CertifySemantics => "certify_semantics",
    CertifyStage => "cert_stage",
    CertifyDestination => "cert_destination",
    CertifyReader => "cert_reader",
    CertifyCleanup => "cert_cleanup",
    CertifySemanticRows => "cert_semantic_rows",
    CertifyRankOrder => "cert_rank_order",
    CertifyPopulationCounts => "cert_population_counts",
    CleanupCandidateIntegrity => "cleanup_candidate_integrity",
    CleanupCompareObservation => "cleanup_compare_observation",
    PublishRename => "publish_rename",
    CatalogueCommit => "catalogue_commit",
    WalWrite => "wal_write",
    DbWrite => "db_write",
    OtherWrite => "other_write",
    NativeClose => "native_close",
    WalSync => "wal_sync",
    DbSync => "db_sync",
    OtherSync => "other_sync",
}

#[derive(Clone, Copy, Default)]
struct Sample {
    count: u64,
    elapsed_ns: u64,
    bytes: u64,
    failures: u64,
    sync_count: u64,
    sync_ns: u64,
    max_ns: u64,
    slow_count: u64,
}

thread_local! {
    static SAMPLES: RefCell<[Sample; LABELS.len()]> = const {
        RefCell::new([Sample {
            count: 0, elapsed_ns: 0, bytes: 0, failures: 0, sync_count: 0, sync_ns: 0,
            max_ns: 0, slow_count: 0,
        }; LABELS.len()])
    };
}

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BUNNY_SCAN_P1_METRICS").is_some_and(|v| v == "1"))
}

pub(crate) struct Timer {
    phase: Phase,
    started: Option<Instant>,
    _trace: crate::scan_trace::TraceScope,
}

impl Phase {
    #[inline]
    pub(crate) fn start(self) -> Timer {
        Timer {
            phase: self,
            started: enabled().then(Instant::now),
            _trace: crate::scan_trace::sqlite_phase(self as usize),
        }
    }
}

impl Timer {
    pub(crate) fn finish_io(mut self, bytes: u64, failed: bool) {
        self.record(bytes, failed, (0, 0));
    }

    fn record(&mut self, bytes: u64, failed: bool, (sync_count, sync_ns): (u64, u64)) {
        if let Some(started) = self.started.take() {
            let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            // A native xSync can occur during TLS destruction as connections
            // close. Diagnostics never interfere with the native callback.
            let _ = SAMPLES.try_with(|samples| {
                let mut samples = samples.borrow_mut();
                let sample = &mut samples[self.phase as usize];
                sample.count = sample.count.saturating_add(1);
                sample.elapsed_ns = sample.elapsed_ns.saturating_add(ns);
                sample.bytes = sample.bytes.saturating_add(bytes);
                sample.failures = sample.failures.saturating_add(u64::from(failed));
                sample.sync_count = sample.sync_count.saturating_add(sync_count);
                sample.sync_ns = sample.sync_ns.saturating_add(sync_ns);
                sample.max_ns = sample.max_ns.max(ns);
                sample.slow_count = sample
                    .slow_count
                    .saturating_add(u64::from(ns >= 100_000_000));
            });
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.record(0, false, (0, 0));
    }
}

fn sync_totals() -> (u64, u64) {
    SAMPLES
        .try_with(|samples| {
            let samples = samples.borrow();
            [Phase::WalSync, Phase::DbSync, Phase::OtherSync]
                .iter()
                .fold((0_u64, 0_u64), |(count, ns), &phase| {
                    let sample = samples[phase as usize];
                    (
                        count.saturating_add(sample.count),
                        ns.saturating_add(sample.elapsed_ns),
                    )
                })
        })
        .unwrap_or_default()
}

pub(crate) fn commit(tx: Transaction<'_>, phase: Phase) -> rusqlite::Result<()> {
    if !enabled() && !crate::scan_trace::enabled() {
        return tx.commit();
    }
    let before = sync_totals();
    let mut timer = phase.start();
    let result = tx.commit();
    let after = sync_totals();
    timer.record(
        0,
        result.is_err(),
        (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
        ),
    );
    result
}

/// Drain the current writer thread's opt-in aggregates at a static scan
/// boundary. Phase durations may nest and are not an additive wall clock.
/// Call from the writer, between store operations. No source data is emitted.
pub fn report_scan_metrics(boundary: &'static str) {
    if !enabled() {
        return;
    }
    let _ = SAMPLES.try_with(|samples| {
        let mut samples = samples.borrow_mut();
        for (phase, sample) in LABELS.iter().zip(samples.iter_mut()) {
            let Sample {
                count,
                elapsed_ns,
                bytes,
                failures,
                sync_count,
                sync_ns,
                max_ns,
                slow_count,
            } = std::mem::take(sample);
            if count != 0 {
                // Diagnostics must not turn a closed stderr into a scan error.
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "bunny-scan-p1-sqlite boundary={boundary} phase={phase} count={count} elapsed_ns={elapsed_ns} bytes={bytes} failures={failures} sync_count={sync_count} sync_ns={sync_ns} max_ns={max_ns} slow_count={slow_count}"
                );
            }
        }
    });
}
