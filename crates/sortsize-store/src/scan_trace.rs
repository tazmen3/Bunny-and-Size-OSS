//! Opt-in, bounded, process-local progress diagnostics. No paths or source data.
//! One bound OS thread writes each handle; readers use atomics, never SQL or a
//! writer lock. Durations include nested work and must not be added together.
//! `wal_checkpoint` denotes an explicitly wrapped checkpoint only. A commit
//! may also include SQLite's automatic checkpoint; nested DB writes/syncs are
//! observations of I/O, not a direct measurement of that automatic checkpoint.
use std::{
    cell::RefCell,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
    },
    time::Instant,
};

const GUI_PHASES: &[&str] = &[
    "idle",
    "other",
    "service_tick",
    "preview_close",
    "preview_open",
    "preview_fold",
    "detail_order",
    "folder_tree",
    "prepare_page",
    "prepare_detail",
    "prepare_top100",
    "prepare_properties",
    "prepare_tree_rows",
    "publish_mailbox",
    "ui_poll",
    "ui_apply",
    "ui_cancel",
    "service_finalize",
    "service_publish",
    "writer_cleanup",
];
const SLOW_NS: u64 = 100_000_000;

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BUNNY_SCAN_PROGRESS_TRACE").is_some_and(|v| v == "1"))
}

fn label(id: u64) -> &'static str {
    let id = id as usize;
    GUI_PHASES
        .get(id)
        .copied()
        .or_else(|| {
            id.checked_sub(GUI_PHASES.len())
                .and_then(|i| crate::scan_metrics::LABELS.get(i).copied())
        })
        .unwrap_or("other")
}

fn phase_id(name: &str) -> u64 {
    GUI_PHASES
        .iter()
        .position(|&s| s == name)
        .or_else(|| {
            crate::scan_metrics::LABELS
                .iter()
                .position(|&s| s == name)
                .map(|i| GUI_PHASES.len() + i)
        })
        .unwrap_or(1) as u64
}

#[derive(Default)]
struct QueryCounters {
    count: AtomicU64,
    vm_steps: AtomicU64,
    fullscan_steps: AtomicU64,
    sorts: AtomicU64,
    max_ns: AtomicU64,
    max_vm_steps: AtomicU64,
    max_fullscan_steps: AtomicU64,
    max_sorts: AtomicU64,
    last_ns: AtomicU64,
    last_vm_steps: AtomicU64,
    last_fullscan_steps: AtomicU64,
    last_sorts: AtomicU64,
    saturated: AtomicU64,
}

struct State {
    origin: Instant,
    bound: AtomicBool,
    sequence: AtomicU64,
    phase: AtomicU64,
    started_ns: AtomicU64,
    depth: AtomicU64,
    completed_count: AtomicU64,
    completed_elapsed_ns: AtomicU64,
    last_completed_ns: AtomicU64,
    slow_count: AtomicU64,
    max_completed_ns: AtomicU64,
    max_completed_phase: AtomicU64,
    last_slow_phase: AtomicU64,
    last_slow_ns: AtomicU64,
    queries: [QueryCounters; 2],
}

impl State {
    fn now(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
    fn begin(&self) {
        self.sequence.fetch_add(1, SeqCst);
    }
    fn end(&self) {
        self.sequence.fetch_add(1, SeqCst);
    }
}

/// A handle belongs to one writer thread at a time; clones are read-only until
/// bound. A second simultaneous binding is inert, rather than blocking work.
#[derive(Clone)]
pub struct TraceHandle(Arc<State>);

#[derive(Clone, Copy, Debug, Default)]
pub struct QuerySnapshot {
    pub count: u64,
    pub vm_steps: u64,
    pub fullscan_steps: u64,
    pub sorts: u64,
    pub max_ns: u64,
    /// VM counters from the same completed call as max_ns.
    pub max_vm_steps: u64,
    pub max_fullscan_steps: u64,
    pub max_sorts: u64,
    pub last_ns: u64,
    pub last_vm_steps: u64,
    pub last_fullscan_steps: u64,
    pub last_sorts: u64,
    /// Negative SQLite i32 counters indicate overflow; those samples are not
    /// interpreted as zero work. This counts affected counter values.
    pub saturated: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct TraceSnapshot {
    /// False if four bounded attempts raced an update. Other fields are then
    /// best-effort and must not be used as a coherent state transition.
    pub valid: bool,
    pub sequence: u64,
    pub thread_elapsed_ns: u64,
    pub phase: &'static str,
    pub phase_elapsed_ns: u64,
    pub depth: u64,
    pub completed_count: u64,
    /// Sum of inclusive durations: nested spans overlap, not writer busy time.
    pub completed_elapsed_ns: u64,
    pub last_completed_ns: u64,
    pub slow_count: u64,
    pub max_completed_ns: u64,
    pub max_completed_phase: &'static str,
    pub last_slow_phase: &'static str,
    /// Most recent completed span >=100ms. Intermediate slow spans may be
    /// overwritten between reads; slow_count and the maximum remain available.
    pub last_slow_ns: u64,
    pub pending_directories: QuerySnapshot,
    pub top100: QuerySnapshot,
}

impl Default for TraceHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceHandle {
    pub fn new() -> Self {
        Self(Arc::new(State {
            origin: Instant::now(),
            bound: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            phase: AtomicU64::new(0),
            started_ns: AtomicU64::new(0),
            depth: AtomicU64::new(0),
            completed_count: AtomicU64::new(0),
            completed_elapsed_ns: AtomicU64::new(0),
            last_completed_ns: AtomicU64::new(0),
            slow_count: AtomicU64::new(0),
            max_completed_ns: AtomicU64::new(0),
            max_completed_phase: AtomicU64::new(0),
            last_slow_phase: AtomicU64::new(0),
            last_slow_ns: AtomicU64::new(0),
            queries: std::array::from_fn(|_| QueryCounters::default()),
        }))
    }
    pub fn requested() -> Option<Self> {
        enabled().then(Self::new)
    }
    pub fn bind_current_thread(&self) -> TraceBinding {
        let acquired = enabled()
            && self
                .0
                .bound
                .compare_exchange(false, true, SeqCst, SeqCst)
                .is_ok();
        let previous = if acquired {
            CURRENT.with(|slot| slot.replace(Some(self.clone())))
        } else {
            None
        };
        TraceBinding {
            handle: acquired.then(|| self.clone()),
            previous,
            _thread: PhantomData,
        }
    }
    pub fn read(&self) -> TraceSnapshot {
        let s = &self.0;
        let query = |q: &QueryCounters| QuerySnapshot {
            count: q.count.load(SeqCst),
            vm_steps: q.vm_steps.load(SeqCst),
            fullscan_steps: q.fullscan_steps.load(SeqCst),
            sorts: q.sorts.load(SeqCst),
            max_ns: q.max_ns.load(SeqCst),
            last_ns: q.last_ns.load(SeqCst),
            max_vm_steps: q.max_vm_steps.load(SeqCst),
            max_fullscan_steps: q.max_fullscan_steps.load(SeqCst),
            max_sorts: q.max_sorts.load(SeqCst),
            last_vm_steps: q.last_vm_steps.load(SeqCst),
            last_fullscan_steps: q.last_fullscan_steps.load(SeqCst),
            last_sorts: q.last_sorts.load(SeqCst),
            saturated: q.saturated.load(SeqCst),
        };
        let mut attempts = 0;
        loop {
            let sequence = s.sequence.load(SeqCst);
            let now = s.now();
            let mut snapshot = TraceSnapshot {
                valid: false,
                sequence,
                thread_elapsed_ns: now,
                phase: label(s.phase.load(SeqCst)),
                phase_elapsed_ns: now.saturating_sub(s.started_ns.load(SeqCst)),
                depth: s.depth.load(SeqCst),
                completed_count: s.completed_count.load(SeqCst),
                completed_elapsed_ns: s.completed_elapsed_ns.load(SeqCst),
                last_completed_ns: s.last_completed_ns.load(SeqCst),
                slow_count: s.slow_count.load(SeqCst),
                max_completed_ns: s.max_completed_ns.load(SeqCst),
                max_completed_phase: label(s.max_completed_phase.load(SeqCst)),
                last_slow_phase: label(s.last_slow_phase.load(SeqCst)),
                last_slow_ns: s.last_slow_ns.load(SeqCst),
                pending_directories: query(&s.queries[0]),
                top100: query(&s.queries[1]),
            };
            snapshot.valid = sequence.is_multiple_of(2) && sequence == s.sequence.load(SeqCst);
            attempts += 1;
            if snapshot.valid || attempts == 4 {
                return snapshot;
            }
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<TraceHandle>> = const { RefCell::new(None) };
}

pub struct TraceBinding {
    handle: Option<TraceHandle>,
    previous: Option<TraceHandle>,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for TraceBinding {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = CURRENT.try_with(|slot| slot.replace(self.previous.take()));
            handle.0.bound.store(false, SeqCst);
        }
    }
}

/// LIFO scope, confined to the bound thread. Unknown names are reported as
/// `other`; the fixed label table never retains arbitrary caller strings.
pub struct TraceScope {
    inner: Option<ScopeState>,
    _thread: PhantomData<Rc<()>>,
}
struct ScopeState {
    handle: TraceHandle,
    id: u64,
    started: u64,
    previous: u64,
    previous_start: u64,
    previous_depth: u64,
}
pub fn phase(name: &'static str) -> TraceScope {
    enter(|| phase_id(name))
}
pub(crate) fn sqlite_phase(index: usize) -> TraceScope {
    enter(|| (GUI_PHASES.len() + index) as u64)
}
fn enter(id: impl FnOnce() -> u64) -> TraceScope {
    let handle = if enabled() {
        CURRENT
            .try_with(|slot| slot.borrow().clone())
            .ok()
            .flatten()
    } else {
        None
    };
    let inner = handle.map(|handle| start_scope(handle, id()));
    TraceScope {
        inner,
        _thread: PhantomData,
    }
}
fn start_scope(handle: TraceHandle, id: u64) -> ScopeState {
    let s = &handle.0;
    let now = s.now();
    s.begin();
    let previous = s.phase.swap(id, SeqCst);
    let started = s.started_ns.swap(now, SeqCst);
    let depth = s.depth.fetch_add(1, SeqCst);
    s.end();
    ScopeState {
        handle,
        id,
        started: now,
        previous,
        previous_start: started,
        previous_depth: depth,
    }
}
impl Drop for TraceScope {
    fn drop(&mut self) {
        if let Some(ScopeState {
            handle,
            id,
            started,
            previous,
            previous_start,
            previous_depth: depth,
        }) = self.inner.take()
        {
            let s = &handle.0;
            let now = s.now();
            let elapsed = now.saturating_sub(started);
            s.begin();
            s.completed_count.fetch_add(1, SeqCst);
            s.completed_elapsed_ns.store(
                s.completed_elapsed_ns.load(SeqCst).saturating_add(elapsed),
                SeqCst,
            );
            s.last_completed_ns.store(elapsed, SeqCst);
            if elapsed > s.max_completed_ns.load(SeqCst) {
                s.max_completed_ns.store(elapsed, SeqCst);
                s.max_completed_phase.store(id, SeqCst);
            }
            if elapsed >= SLOW_NS {
                s.slow_count.fetch_add(1, SeqCst);
                s.last_slow_phase.store(id, SeqCst);
                s.last_slow_ns.store(elapsed, SeqCst);
            }
            s.phase.store(previous, SeqCst);
            s.started_ns
                .store(if depth == 0 { now } else { previous_start }, SeqCst);
            s.depth.store(depth, SeqCst);
            s.end();
        }
    }
}

/// Reads SQLite's existing VM counters; does not reset them or change execution.
pub(crate) fn statement(name: &'static str, q: &rusqlite::Statement<'_>, started: Option<Instant>) {
    let Some(started) = started else {
        return;
    };
    let index = match name {
        "pending_directories" => 0,
        "top100" => 1,
        _ => return,
    };
    let _ = CURRENT.try_with(|slot| {
        let handle = slot.borrow();
        let Some(handle) = handle.as_ref() else {
            return;
        };
        use rusqlite::StatementStatus::{FullscanStep, Sort, VmStep};
        let values = [
            q.get_status(VmStep),
            q.get_status(FullscanStep),
            q.get_status(Sort),
        ];
        let counters = &handle.0.queries[index];
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        handle.0.begin();
        counters.count.fetch_add(1, SeqCst);
        counters.last_ns.store(elapsed, SeqCst);
        let maximum = elapsed > counters.max_ns.load(SeqCst);
        if maximum {
            counters.max_ns.store(elapsed, SeqCst);
        }
        for ((total, last, max), value) in [
            (
                &counters.vm_steps,
                &counters.last_vm_steps,
                &counters.max_vm_steps,
            ),
            (
                &counters.fullscan_steps,
                &counters.last_fullscan_steps,
                &counters.max_fullscan_steps,
            ),
            (&counters.sorts, &counters.last_sorts, &counters.max_sorts),
        ]
        .into_iter()
        .zip(values)
        {
            let value = u64::try_from(value).unwrap_or_else(|_| {
                counters.saturated.fetch_add(1, SeqCst);
                i32::MAX as u64
            });
            total.store(total.load(SeqCst).saturating_add(value), SeqCst);
            last.store(value, SeqCst);
            if maximum {
                max.store(value, SeqCst);
            }
        }
        handle.0.end();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(handle: &TraceHandle, name: &str) -> TraceScope {
        TraceScope {
            inner: Some(start_scope(handle.clone(), phase_id(name))),
            _thread: PhantomData,
        }
    }

    #[test]
    fn nested_phase_restores_outer_start_and_idle_without_sql() {
        let handle = TraceHandle::new();
        let outer = scope(&handle, "service_tick");
        let outer_start = handle.0.started_ns.load(SeqCst);
        let inner = scope(&handle, "wal_sync");
        let during = handle.read();
        assert!(during.valid);
        assert_eq!(during.phase, "wal_sync");
        assert_eq!(during.depth, 2);
        drop(inner);
        let restored = handle.read();
        assert!(restored.valid);
        assert_eq!(restored.phase, "service_tick");
        assert_eq!(restored.depth, 1);
        assert_eq!(handle.0.started_ns.load(SeqCst), outer_start);
        drop(outer);
        let idle = handle.read();
        assert!(idle.valid);
        assert_eq!(idle.phase, "idle");
        assert_eq!(idle.depth, 0);
        assert_eq!(idle.completed_count, 2);
        assert!(idle.completed_elapsed_ns >= idle.max_completed_ns);
    }

    #[test]
    fn interrupted_atomic_update_does_not_spin_or_claim_coherence() {
        let handle = TraceHandle::new();
        handle.0.begin();
        assert!(!handle.read().valid);
        handle.0.end();
        assert!(handle.read().valid);
        assert_eq!(label(phase_id("unknown caller string")), "other");
    }
}
