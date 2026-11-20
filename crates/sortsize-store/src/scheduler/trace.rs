//! Optional fixed-size progress state, readable while the writer is blocked.
//! Snapshots are approximate atomic observations, not transaction snapshots.
//! No source paths, user names, SQLite access or monitoring thread live here.
use super::{ACQUISITIONS, CURSORS};
use std::{
    cell::{Cell, RefCell},
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

macro_rules! phases {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        pub enum $name { $($variant),+ }
        impl $name {
            fn decode(value: u8) -> Self {
                const VALUES: &[$name] = &[$($name::$variant),+];
                VALUES.get(usize::from(value)).copied().unwrap_or(VALUES[0])
            }
        }
    };
}
phases!(WriterPhase {
    Idle,
    ValidateWorkspace,
    Receive,
    Ingest,
    Admission,
    Selection,
    Preparation,
    Dispatch,
    Finalize
});
phases!(WorkerPhase {
    Idle,
    Working,
    DirectoryCheck,
    Enumeration,
    Metadata,
    ResultReady,
    Enqueue,
    Panicked
});

#[derive(Debug)]
pub struct WorkerProgress {
    pub phase: WorkerPhase,
    pub since_ns: u64,
    pub quantum: u64,
    pub dispatched_ns: u64,
    pub started_ns: u64,
    pub result_ready_ns: u64,
    pub observations: u64,
}

#[derive(Debug)]
pub struct ProgressSnapshot {
    pub elapsed_ns: u64,
    pub dispatched: u64,
    pub started: u64,
    pub result_ready: u64,
    pub enqueued: u64,
    pub dequeued: u64,
    pub ingested: u64,
    pub acked: u64,
    pub abandoned: u64,
    pub observations_returned: u64,
    /// Observations in successfully committed/acknowledged batches, including
    /// a successful replay. This is not a count of unique SQLite entries.
    pub observations_ingested: u64,
    pub pending_completions: usize,
    pub in_flight: u64,
    pub global_acquisitions: usize,
    pub global_cursors: usize,
    pub writer_phase: WriterPhase,
    pub writer_phase_since_ns: u64,
    pub last_dispatch_ns: u64,
    pub last_start_ns: u64,
    pub last_result_ready_ns: u64,
    pub last_enqueue_ns: u64,
    pub last_dequeue_ns: u64,
    pub last_ingest_ns: u64,
    pub last_ack_ns: u64,
    pub workers: [WorkerProgress; 8],
}

#[derive(Default)]
struct WorkerState {
    phase: AtomicU8,
    since_ns: AtomicU64,
    quantum: AtomicU64,
    dispatched_ns: AtomicU64,
    started_ns: AtomicU64,
    result_ready_ns: AtomicU64,
    observations: AtomicU64,
}
impl WorkerState {
    fn snapshot(&self) -> WorkerProgress {
        WorkerProgress {
            phase: WorkerPhase::decode(self.phase.load(Ordering::Relaxed)),
            since_ns: self.since_ns.load(Ordering::Relaxed),
            quantum: self.quantum.load(Ordering::Relaxed),
            dispatched_ns: self.dispatched_ns.load(Ordering::Relaxed),
            started_ns: self.started_ns.load(Ordering::Relaxed),
            result_ready_ns: self.result_ready_ns.load(Ordering::Relaxed),
            observations: self.observations.load(Ordering::Relaxed),
        }
    }
}

struct TraceState {
    epoch: Instant,
    next_quantum: AtomicU64,
    dispatched: AtomicU64,
    started: AtomicU64,
    result_ready: AtomicU64,
    enqueued: AtomicU64,
    dequeued: AtomicU64,
    ingested: AtomicU64,
    acked: AtomicU64,
    abandoned: AtomicU64,
    in_flight: AtomicU64,
    observations_ingested: AtomicU64,
    returned: Arc<AtomicU64>,
    pending: Arc<AtomicUsize>,
    writer_phase: AtomicU8,
    writer_since: AtomicU64,
    last_dispatch: AtomicU64,
    last_start: AtomicU64,
    last_result_ready: AtomicU64,
    last_enqueue: AtomicU64,
    last_dequeue: AtomicU64,
    last_ingest: AtomicU64,
    last_ack: AtomicU64,
    workers: [WorkerState; 8],
}

/// A read-only handle for the GUI watchdog. Clones never own a connection or
/// writer/control mutex. All timestamps use this handle's monotonic epoch.
#[derive(Clone)]
pub struct ProgressTrace(Arc<TraceState>);
impl ProgressTrace {
    pub(super) fn requested(pending: &Arc<AtomicUsize>, returned: &Arc<AtomicU64>) -> Option<Self> {
        if !std::env::var_os("BUNNY_SCAN_PROGRESS_TRACE").is_some_and(|v| v == "1") {
            return None;
        }
        Some(Self::new(pending, returned))
    }
    fn new(pending: &Arc<AtomicUsize>, returned: &Arc<AtomicU64>) -> Self {
        Self(Arc::new(TraceState {
            epoch: Instant::now(),
            next_quantum: AtomicU64::new(1),
            dispatched: AtomicU64::new(0),
            started: AtomicU64::new(0),
            result_ready: AtomicU64::new(0),
            enqueued: AtomicU64::new(0),
            dequeued: AtomicU64::new(0),
            ingested: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            abandoned: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            observations_ingested: AtomicU64::new(0),
            returned: returned.clone(),
            pending: pending.clone(),
            writer_phase: AtomicU8::new(0),
            writer_since: AtomicU64::new(0),
            last_dispatch: AtomicU64::new(0),
            last_start: AtomicU64::new(0),
            last_result_ready: AtomicU64::new(0),
            last_enqueue: AtomicU64::new(0),
            last_dequeue: AtomicU64::new(0),
            last_ingest: AtomicU64::new(0),
            last_ack: AtomicU64::new(0),
            workers: std::array::from_fn(|_| WorkerState::default()),
        }))
    }
    fn now(&self) -> u64 {
        u64::try_from(self.0.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
    pub fn snapshot(&self) -> ProgressSnapshot {
        let s = &self.0;
        let dispatched = s.dispatched.load(Ordering::Relaxed);
        let acked = s.acked.load(Ordering::Relaxed);
        let abandoned = s.abandoned.load(Ordering::Relaxed);
        ProgressSnapshot {
            elapsed_ns: self.now(),
            dispatched,
            acked,
            abandoned,
            started: s.started.load(Ordering::Relaxed),
            result_ready: s.result_ready.load(Ordering::Relaxed),
            enqueued: s.enqueued.load(Ordering::Relaxed),
            dequeued: s.dequeued.load(Ordering::Relaxed),
            ingested: s.ingested.load(Ordering::Relaxed),
            observations_returned: s.returned.load(Ordering::Relaxed),
            observations_ingested: s.observations_ingested.load(Ordering::Relaxed),
            pending_completions: s.pending.load(Ordering::Relaxed),
            in_flight: s.in_flight.load(Ordering::Relaxed),
            global_acquisitions: ACQUISITIONS.load(Ordering::Relaxed),
            global_cursors: CURSORS.load(Ordering::Relaxed),
            writer_phase: WriterPhase::decode(s.writer_phase.load(Ordering::Relaxed)),
            writer_phase_since_ns: s.writer_since.load(Ordering::Relaxed),
            last_dispatch_ns: s.last_dispatch.load(Ordering::Relaxed),
            last_start_ns: s.last_start.load(Ordering::Relaxed),
            last_result_ready_ns: s.last_result_ready.load(Ordering::Relaxed),
            last_enqueue_ns: s.last_enqueue.load(Ordering::Relaxed),
            last_dequeue_ns: s.last_dequeue.load(Ordering::Relaxed),
            last_ingest_ns: s.last_ingest.load(Ordering::Relaxed),
            last_ack_ns: s.last_ack.load(Ordering::Relaxed),
            workers: std::array::from_fn(|i| s.workers[i].snapshot()),
        }
    }
    pub(super) fn ticket(&self) -> (u64, u64) {
        (
            self.0.next_quantum.fetch_add(1, Ordering::Relaxed),
            self.now(),
        )
    }
    pub(super) fn job(&self) -> JobTrace {
        self.0.in_flight.fetch_add(1, Ordering::Relaxed);
        JobTrace(self.clone())
    }
    pub(super) fn dispatched(&self, when: u64) {
        self.0.dispatched.fetch_add(1, Ordering::Relaxed);
        self.0.last_dispatch.store(when, Ordering::Relaxed);
    }
    pub(super) fn dequeue(&self) {
        self.0.dequeued.fetch_add(1, Ordering::Relaxed);
        self.0.last_dequeue.store(self.now(), Ordering::Relaxed);
    }
    pub(super) fn ingested(&self, quanta: u64, observations: u64) {
        self.0.ingested.fetch_add(quanta, Ordering::Relaxed);
        self.0
            .observations_ingested
            .fetch_add(observations, Ordering::Relaxed);
        self.0.last_ingest.store(self.now(), Ordering::Relaxed);
    }
    pub(super) fn ack(&self) {
        self.0.acked.fetch_add(1, Ordering::Relaxed);
        self.0.last_ack.store(self.now(), Ordering::Relaxed);
    }
    pub(super) fn abandon(&self) {
        self.0.abandoned.fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn writer(&self, phase: WriterPhase) -> WriterMarker {
        let previous = (
            self.0.writer_phase.load(Ordering::Relaxed),
            self.0.writer_since.load(Ordering::Relaxed),
        );
        self.0.writer_since.store(self.now(), Ordering::Relaxed);
        self.0.writer_phase.store(phase as u8, Ordering::Relaxed);
        WriterMarker {
            trace: self.clone(),
            previous,
        }
    }
    pub(super) fn start_worker(&self, ticket: (u64, u64)) -> WorkerRun {
        let slot = SLOT.with(Cell::get);
        let context = WorkerContext {
            trace: self.clone(),
            slot,
        };
        let state = &self.0.workers[slot];
        let now = self.now();
        state.quantum.store(ticket.0, Ordering::Relaxed);
        state.dispatched_ns.store(ticket.1, Ordering::Relaxed);
        state.started_ns.store(now, Ordering::Relaxed);
        state.result_ready_ns.store(0, Ordering::Relaxed);
        state.observations.store(0, Ordering::Relaxed);
        self.0.started.fetch_add(1, Ordering::Relaxed);
        self.0.last_start.store(now, Ordering::Relaxed);
        context.phase(WorkerPhase::Working);
        CURRENT.with(|current| *current.borrow_mut() = Some(context.clone()));
        WorkerRun(context)
    }
}

/// Follows the same job/result lifetime as its acquisition permit, including
/// failed sends and receiver destruction. It never owns that permit itself.
pub(super) struct JobTrace(ProgressTrace);
impl Drop for JobTrace {
    fn drop(&mut self) {
        self.0.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(super) struct WriterMarker {
    trace: ProgressTrace,
    previous: (u8, u64),
}
impl Drop for WriterMarker {
    fn drop(&mut self) {
        self.trace
            .0
            .writer_since
            .store(self.previous.1, Ordering::Relaxed);
        self.trace
            .0
            .writer_phase
            .store(self.previous.0, Ordering::Relaxed);
    }
}

thread_local! {
    static SLOT: Cell<usize> = const { Cell::new(0) };
    static CURRENT: RefCell<Option<WorkerContext>> = const { RefCell::new(None) };
}
pub(super) fn set_slot(slot: usize) {
    SLOT.with(|current| current.set(slot));
}
pub(super) fn current_worker() -> Option<WorkerContext> {
    CURRENT.with(|current| current.borrow().clone())
}

#[derive(Clone)]
pub(super) struct WorkerContext {
    trace: ProgressTrace,
    slot: usize,
}
impl WorkerContext {
    fn phase(&self, phase: WorkerPhase) {
        let state = &self.trace.0.workers[self.slot];
        state.since_ns.store(self.trace.now(), Ordering::Relaxed);
        state.phase.store(phase as u8, Ordering::Relaxed);
    }
    pub(super) fn measure<T>(&self, phase: WorkerPhase, operation: impl FnOnce() -> T) -> T {
        self.phase(phase);
        let result = operation();
        self.phase(WorkerPhase::Working);
        result
    }
}
pub(super) fn measure<T>(
    worker: Option<&WorkerContext>,
    phase: WorkerPhase,
    operation: impl FnOnce() -> T,
) -> T {
    match worker {
        Some(worker) => worker.measure(phase, operation),
        None => operation(),
    }
}
pub(super) struct WorkerRun(WorkerContext);
impl WorkerRun {
    pub(super) fn ready(&self, observations: u64) {
        let state = &self.0.trace.0;
        let now = self.0.trace.now();
        state.result_ready.fetch_add(1, Ordering::Relaxed);
        state.last_result_ready.store(now, Ordering::Relaxed);
        state.workers[self.0.slot]
            .result_ready_ns
            .store(now, Ordering::Relaxed);
        state.workers[self.0.slot]
            .observations
            .store(observations, Ordering::Relaxed);
        self.0.phase(WorkerPhase::ResultReady);
    }
    pub(super) fn enqueue_begin(&self) {
        self.0.phase(WorkerPhase::Enqueue);
    }
    pub(super) fn enqueued(&self) {
        self.0.trace.0.enqueued.fetch_add(1, Ordering::Relaxed);
        self.0
            .trace
            .0
            .last_enqueue
            .store(self.0.trace.now(), Ordering::Relaxed);
    }
}
impl Drop for WorkerRun {
    fn drop(&mut self) {
        self.0.phase(if std::thread::panicking() {
            WorkerPhase::Panicked
        } else {
            WorkerPhase::Idle
        });
        CURRENT.with(|current| *current.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_observes_queued_uncommitted_work_and_releases_dropped_results() {
        let pending = Arc::new(AtomicUsize::new(0));
        let returned = Arc::new(AtomicU64::new(0));
        let trace = ProgressTrace::new(&pending, &returned);
        let job = trace.job();
        let ticket = trace.ticket();
        trace.dispatched(ticket.1);
        let run = trace.start_worker(ticket);
        let worker = current_worker().unwrap();
        worker.measure(WorkerPhase::Metadata, || {
            assert_eq!(trace.snapshot().workers[0].phase, WorkerPhase::Metadata);
        });
        returned.store(32, Ordering::Relaxed);
        pending.store(1, Ordering::Relaxed);
        run.ready(32);
        run.enqueue_begin();
        run.enqueued();
        drop(run);
        let queued = trace.snapshot();
        assert_eq!(
            (
                queued.dispatched,
                queued.started,
                queued.result_ready,
                queued.enqueued
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(
            (
                queued.pending_completions,
                queued.in_flight,
                queued.observations_returned
            ),
            (1, 1, 32)
        );
        trace.dequeue();
        // An ingestion failure/retry must not report a commit or release its
        // job. The same dequeue is not counted again when retried internally.
        assert_eq!(trace.snapshot().ingested, 0);
        assert_eq!(trace.snapshot().in_flight, 1);
        trace.ingested(1, 32);
        trace.ack();
        pending.store(0, Ordering::Relaxed);
        drop(job);
        let committed = trace.snapshot();
        assert_eq!(
            (
                committed.dequeued,
                committed.ingested,
                committed.acked,
                committed.observations_ingested
            ),
            (1, 1, 1, 32)
        );
        assert_eq!(committed.in_flight, 0);
        let dropped_delivery = trace.job();
        assert_eq!(trace.snapshot().in_flight, 1);
        drop(dropped_delivery);
        assert_eq!(trace.snapshot().in_flight, 0);
        assert!(current_worker().is_none());
    }

    #[test]
    fn watchdog_handle_is_send_sync_and_nested_writer_phases_restore_the_parent() {
        fn requires_send_sync<T: Send + Sync>() {}
        requires_send_sync::<ProgressTrace>();
        let trace =
            ProgressTrace::new(&Arc::new(AtomicUsize::new(0)), &Arc::new(AtomicU64::new(0)));
        let admission = trace.writer(WriterPhase::Admission);
        let began = trace.snapshot().writer_phase_since_ns;
        {
            let _selection = trace.writer(WriterPhase::Selection);
            let independent = trace.clone();
            assert_eq!(independent.snapshot().writer_phase, WriterPhase::Selection);
        }
        assert_eq!(trace.snapshot().writer_phase, WriterPhase::Admission);
        assert_eq!(trace.snapshot().writer_phase_since_ns, began);
        drop(admission);
        assert_eq!(trace.snapshot().writer_phase, WriterPhase::Idle);
    }
}
