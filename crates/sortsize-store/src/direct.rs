//! Session for direct, reconstructible acquisition and its non-SQLite reader.
use crate::{
    direct_tree::DirectTree,
    inventory::Result,
    scheduler::{CollectionOptions, Control, direct},
};
use sortsize_core::acquisition::Guard;
use std::{
    io,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectStatus {
    Running,
    Finished,
    Cancelled,
    Failed(Arc<str>),
}

#[cfg(feature = "test-support")]
pub type TestBatchGate = Arc<(Mutex<bool>, std::sync::Condvar)>;

/// Test-only pacing is sampled after a committed batch and after unlocking the
/// tree. Returning a gate or delay never changes acquisition observations.
#[cfg(feature = "test-support")]
pub type TestBatchPacing =
    Arc<dyn Fn() -> (std::time::Duration, Option<TestBatchGate>) + Send + Sync>;

#[cfg(feature = "test-support")]
fn pace_test_batch(pacing: &TestBatchPacing, control: &Control) -> Result<()> {
    use std::time::Duration;
    let (delay, gate) = pacing();
    let poll = Duration::from_millis(10);
    if let Some(gate) = gate {
        let (lock, changed) = &*gate;
        let mut open = lock.lock().unwrap();
        while !*open {
            if control.is_cancelled() {
                return Err(io::Error::from(io::ErrorKind::Interrupted).into());
            }
            open = changed.wait_timeout(open, poll).unwrap().0;
        }
    }
    let started = Instant::now();
    while started.elapsed() < delay {
        if control.is_cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted).into());
        }
        std::thread::sleep(delay.saturating_sub(started.elapsed()).min(poll));
    }
    Ok(())
}

/// Dropping the owning session revokes acquisition. Native calls that are
/// already running drain in the background with the workspace still alive.
pub struct DirectScan {
    // The tree's file handles close before the temporary directory is removed.
    tree: Arc<RwLock<DirectTree>>,
    scratch: Arc<tempfile::TempDir>,
    guard: Arc<Guard>,
    status: Arc<Mutex<DirectStatus>>,
    control: Control,
    cancelled: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    worker: std::thread::JoinHandle<()>,
}

impl DirectScan {
    pub fn start(
        guard: Guard,
        options: CollectionOptions,
        scratch: tempfile::TempDir,
        disk_limit: u64,
    ) -> Result<Self> {
        Self::start_inner(
            guard,
            options,
            scratch,
            disk_limit,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    #[cfg(feature = "test-support")]
    pub fn start_with_test_pacing(
        guard: Guard,
        options: CollectionOptions,
        scratch: tempfile::TempDir,
        disk_limit: u64,
        pacing: TestBatchPacing,
    ) -> Result<Self> {
        Self::start_inner(guard, options, scratch, disk_limit, Some(pacing))
    }

    fn start_inner(
        guard: Guard,
        options: CollectionOptions,
        scratch: tempfile::TempDir,
        disk_limit: u64,
        #[cfg(feature = "test-support")] pacing: Option<TestBatchPacing>,
    ) -> Result<Self> {
        if !guard.work_path_allowed(scratch.path())? {
            return Err(crate::inventory::Error::Contract(
                "direct scan scratch must be outside the scan or in the verified workspace",
            ));
        }
        let tree = Arc::new(RwLock::new(DirectTree::new(
            guard.root().to_owned(),
            scratch.path(),
            disk_limit,
        )?));
        let disk = tree.read().unwrap().disk_budget();
        let scratch = Arc::new(scratch);
        let guard = Arc::new(guard);
        let status = Arc::new(Mutex::new(DirectStatus::Running));
        let control = Control::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));

        let worker_scratch = Arc::clone(&scratch);
        let worker_tree = Arc::clone(&tree);
        let worker_guard = Arc::clone(&guard);
        let worker_status = Arc::clone(&status);
        let worker_control = control.clone();
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_progress = Arc::clone(&progress);
        let worker = std::thread::Builder::new()
            .name("bunny-direct-scan".into())
            .spawn(move || {
                // Drop in this order even if unwinding from a native worker fault.
                let _scratch = worker_scratch;
                let tree = worker_tree;
                let trace = crate::scan_metrics::enabled();
                let started = trace.then(Instant::now);
                let mut ntfs = sortsize_core::acquisition::NtfsListingStats::default();
                let mut frontier_bytes = 0;
                let mut quanta = 0u64;
                let mut write_wait_ns = 0u128;
                let mut write_wait_max_ns = 0u128;
                let mut accept_ns = 0u128;
                let mut accept_max_ns = 0u128;
                let mut finish_ns = 0u128;
                let outcome = direct::run_with_spill(worker_guard, options, &worker_control, _scratch.path(), disk, |batch| {
                    worker_progress.fetch_add(
                        batch.batch.observations.len() as u64,
                        Ordering::Relaxed,
                    );
                    quanta = quanta.saturating_add(1);
                    ntfs.batch_calls = ntfs.batch_calls.saturating_add(batch.ntfs.batch_calls);
                    ntfs.relative_opens =
                        ntfs.relative_opens.saturating_add(batch.ntfs.relative_opens);
                    ntfs.stale_rows = ntfs.stale_rows.saturating_add(batch.ntfs.stale_rows);
                    frontier_bytes = frontier_bytes.max(batch.frontier_bytes);
                    let waiting = trace.then(Instant::now);
                    let mut tree = tree.write().unwrap();
                    let acquired = trace.then(Instant::now);
                    if let Some((waiting, acquired)) = waiting.zip(acquired) {
                        let elapsed = acquired.duration_since(waiting).as_nanos();
                        write_wait_ns += elapsed;
                        write_wait_max_ns = write_wait_max_ns.max(elapsed);
                    }
                    let result = tree.accept(batch.batch);
                    if let Some(acquired) = acquired {
                        let elapsed = acquired.elapsed().as_nanos();
                        accept_ns += elapsed;
                        accept_max_ns = accept_max_ns.max(elapsed);
                    }
                    drop(tree);
                    #[cfg(feature = "test-support")]
                    if result.is_ok() && let Some(pacing) = &pacing {
                        pace_test_batch(pacing, &worker_control)?;
                    }
                    result
                })
                .and_then(|()| {
                    let waiting = trace.then(Instant::now);
                    let mut tree = tree.write().unwrap();
                    let acquired = trace.then(Instant::now);
                    if let Some((waiting, acquired)) = waiting.zip(acquired) {
                        let elapsed = acquired.duration_since(waiting).as_nanos();
                        write_wait_ns += elapsed;
                        write_wait_max_ns = write_wait_max_ns.max(elapsed);
                    }
                    let result = tree.finish();
                    if let Some(acquired) = acquired {
                        finish_ns = acquired.elapsed().as_nanos();
                    }
                    result
                });
                let next = match outcome {
                    _ if worker_cancelled.load(Ordering::Acquire) => DirectStatus::Cancelled,
                    Ok(()) => DirectStatus::Finished,
                    Err(crate::inventory::Error::Io(ref e))
                        if e.kind() == io::ErrorKind::Interrupted =>
                    {
                        DirectStatus::Cancelled
                    }
                    Err(e) => DirectStatus::Failed(e.to_string().into()),
                };
                if let Some(started) = started {
                    // The outcome clock excludes diagnostic summary/formatting.
                    let wall_ns = started.elapsed().as_nanos();
                    let (summary, tree_metrics) = {
                        let tree = tree.read().unwrap();
                        (tree.summary(), tree.metrics())
                    };
                    let measure = summary.measure;
                    let outcome = match next {
                        DirectStatus::Running => "running",
                        DirectStatus::Finished => "finished",
                        DirectStatus::Cancelled => "cancelled",
                        DirectStatus::Failed(_) => "failed",
                    };
                    use std::io::Write;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "scan_direct outcome={outcome} wall_ns={wall_ns} tree_write_wait_ns={write_wait_ns} tree_write_wait_max_ns={write_wait_max_ns} tree_accept_ns={accept_ns} tree_accept_max_ns={accept_max_ns} tree_finish_ns={finish_ns} observations={} retained_observations={} quanta={quanta} errors={} access_denied={} files={} directories={} logical={:?} logical_unknown={} allocated={:?} allocation_unknown={} coverage={:?} pending_directories={} incomplete_directories={} detached_directories={} duplicate_observations={} temporary_bytes={} frontier_peak_sampled_bytes={frontier_bytes} ntfs_batch_calls={} ntfs_relative_opens={} ntfs_stale_rows={}",
                        worker_progress.load(Ordering::Relaxed),
                        summary.entries,
                        summary.errors,
                        summary.denied,
                        measure.files,
                        measure.directories,
                        measure.logical,
                        measure.unknown,
                        measure.allocated,
                        measure.allocation_unknown,
                        summary.coverage,
                        summary.pending_directories,
                        summary.incomplete_directories,
                        summary.detached_directories,
                        summary.duplicate_observations,
                        summary.temporary_bytes,
                        ntfs.batch_calls,
                        ntfs.relative_opens,
                        ntfs.stale_rows,
                    );
                    if let Some(metrics) = tree_metrics {
                        metrics.report(outcome);
                    }
                }
                *worker_status.lock().unwrap() = next;
            })?;
        Ok(Self {
            tree,
            scratch,
            guard,
            status,
            control,
            cancelled,
            progress,
            worker,
        })
    }

    pub fn tree(&self) -> &Arc<RwLock<DirectTree>> {
        &self.tree
    }

    pub fn scratch(&self) -> Arc<tempfile::TempDir> {
        Arc::clone(&self.scratch)
    }

    pub fn guard(&self) -> Arc<Guard> {
        Arc::clone(&self.guard)
    }

    pub fn status(&self) -> DirectStatus {
        let status = self.status.lock().unwrap();
        if *status == DirectStatus::Running && self.worker.is_finished() {
            return DirectStatus::Failed("direct acquisition worker terminated".into());
        }
        status.clone()
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    pub fn progress_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.progress)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.control.cancel();
    }
}

impl Drop for DirectScan {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(all(test, feature = "test-support"))]
mod pacing_tests {
    use super::*;
    use sortsize_core::acquisition::Policy;
    use std::time::{Duration, Instant};

    #[test]
    fn observed_batch_remains_readable_while_gate_waits_and_cancel_releases_it() {
        let source = tempfile::tempdir().unwrap();
        for number in 0..128 {
            std::fs::write(source.path().join(format!("f-{number}")), b"x").unwrap();
        }
        let scratch = tempfile::tempdir().unwrap();
        let guard = Guard::open(source.path(), Policy::default()).unwrap();
        let gate: TestBatchGate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let hook_gate = gate.clone();
        let scan = DirectScan::start_with_test_pacing(
            guard,
            CollectionOptions::default(),
            scratch,
            1024 * 1024,
            Arc::new(move || (Duration::ZERO, Some(hook_gate.clone()))),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while scan.progress_counter().load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "no observed batch");
            std::thread::sleep(Duration::from_millis(1));
        }
        // The progress counter advances immediately before acceptance. Retry a
        // contended read, but require a coherent published batch while gate stays shut.
        loop {
            if let Ok(tree) = scan.tree().try_read()
                && tree.summary().entries != 0
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "gate retained the tree write lock"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(scan.status(), DirectStatus::Running);
        scan.cancel();
        while scan.status() == DirectStatus::Running {
            assert!(
                Instant::now() < deadline,
                "cancellation did not release test gate"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(scan.status(), DirectStatus::Cancelled);
    }
}
