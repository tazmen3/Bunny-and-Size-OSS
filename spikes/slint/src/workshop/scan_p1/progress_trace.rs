//! Opt-in, bounded observations of real work. The observer never touches SQL or
//! the service lane; only its own thread writes diagnostics. UI teardown signals
//! shutdown without joining a possibly blocked diagnostic sink.
use super::super::service::Prepared;
use sortsize_store::scan_trace::TraceHandle;
use std::{
    cell::RefCell,
    io::{self, Write},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub(in crate::workshop) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BUNNY_SCAN_PROGRESS_TRACE").is_some_and(|v| v == "1"))
}
pub(in crate::workshop) fn now_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u128::from(u64::MAX - 1)) as u64
        + 1
}

#[derive(Clone, Copy)]
pub(in crate::workshop) enum Field {
    Source,
    Generation,
    View,
    AcquiredRevision,
    PreviewRevision,
    Cursor,
    PendingEntry,
    Floor,
    FloorHolds,
    Logical,
    Files,
    Known,
    Token,
    PageRevision,
    Pages,
    LastRawChange,
    LastVisibleChange,
    LastPoll,
    Polls,
    LastApply,
    HoldReason,
    LastModelChange,
    LastStateChange,
    LastProgressChange,
    CatchupStarted,
    PreviewQuanta,
    DeferredQuanta,
    InventoryHi,
    InventoryLo,
    Ancestor,
    Applying,
    PriorityDone,
    PreviewComplete,
    PreviewLogical,
    PreviewLogicalKnown,
    SelectedFolder,
    PreviewWorkNs,
    FilesKnown,
    Busy,
    Cancelling,
    State,
    Phase,
    PublishedNs,
    TakenNs,
    TakenToken,
}
const FIELDS: usize = 45;
const LABELS: [&str; FIELDS] = [
    "source_epoch",
    "generation",
    "view_epoch",
    "acquired_revision",
    "preview_revision",
    "fold_cursor",
    "pending_entry",
    "preview_floor",
    "floor_holds",
    "logical",
    "files",
    "logical_known",
    "page_token",
    "page_revision",
    "pages",
    "raw_changed_ns",
    "visible_changed_ns",
    "ui_poll_ns",
    "ui_polls",
    "applied_ns",
    "hold_reason",
    "models_changed_ns",
    "state_changed_ns",
    "progress_text_changed_ns",
    "catchup_started_ns",
    "preview_quanta",
    "deferred_quanta",
    "inventory_hi",
    "inventory_lo",
    "fold_ancestor",
    "fold_applying",
    "priority_done",
    "preview_complete",
    "preview_logical",
    "preview_logical_known",
    "selected_folder",
    "preview_work_ns",
    "files_known",
    "busy",
    "cancelling",
    "state",
    "phase",
    "published_ns",
    "mailbox_taken_ns",
    "mailbox_taken_token",
];
pub(in crate::workshop) struct Counters {
    sequence: AtomicU64,
    values: [AtomicU64; FIELDS],
}
impl Default for Counters {
    fn default() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            values: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}
impl Counters {
    pub(in crate::workshop) fn context(
        &self,
        source: u64,
        generation: u64,
        view: u64,
        inventory: u128,
    ) {
        self.sequence.fetch_add(1, Ordering::SeqCst);
        if self.values[Field::Source as usize].load(Ordering::Relaxed) != source
            || self.values[Field::Generation as usize].load(Ordering::Relaxed) != generation
        {
            for value in &self.values {
                value.store(0, Ordering::SeqCst);
            }
        }
        for (field, value) in [
            (Field::Source, source),
            (Field::Generation, generation),
            (Field::View, view),
            (Field::InventoryHi, (inventory >> 64) as u64),
            (Field::InventoryLo, inventory as u64),
        ] {
            self.values[field as usize].store(value, Ordering::SeqCst);
        }
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }
    // Each channel has one producer. Its paired sequence fences context changes
    // so the observer never silently attributes old counters to a new source.
    pub(in crate::workshop) fn write(&self, fields: &[(Field, u64)]) {
        self.sequence.fetch_add(1, Ordering::SeqCst);
        for (field, value) in fields {
            self.values[*field as usize].store(*value, Ordering::SeqCst);
        }
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }
    pub(in crate::workshop) fn add(&self, field: Field) {
        self.add_value(field, 1);
    }
    pub(in crate::workshop) fn add_value(&self, field: Field, added: u64) {
        let value = self.values[field as usize]
            .load(Ordering::Relaxed)
            .saturating_add(added);
        self.write(&[(field, value)]);
    }
    fn read(&self) -> Option<(u64, [u64; FIELDS])> {
        for _ in 0..3 {
            let before = self.sequence.load(Ordering::SeqCst);
            if before & 1 != 0 {
                continue;
            }
            let values = std::array::from_fn(|i| self.values[i].load(Ordering::SeqCst));
            if self.sequence.load(Ordering::SeqCst) == before {
                return Some((before, values));
            }
        }
        None
    }
}
#[derive(Clone, Copy)]
pub(in crate::workshop) enum Channel {
    Writer,
    Reader,
}
thread_local! {
    static CURRENT: RefCell<Option<(Arc<Trace>, Channel)>> = const { RefCell::new(None) };
}
pub(in crate::workshop) struct Binding;
impl Drop for Binding {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = None);
    }
}
pub(in crate::workshop) fn bind(trace: Option<&Arc<Trace>>, channel: Channel) -> Binding {
    if let Some(trace) = trace {
        CURRENT.with(|current| *current.borrow_mut() = Some((trace.clone(), channel)));
    }
    Binding
}
pub(in crate::workshop) fn current(action: impl FnOnce(&Trace, &Counters)) {
    if enabled() {
        CURRENT.with(|current| {
            if let Some((trace, channel)) = current.borrow().as_ref() {
                action(
                    trace,
                    match channel {
                        Channel::Writer => &trace.writer,
                        Channel::Reader => &trace.reader,
                    },
                );
            }
        });
    }
}

pub(in crate::workshop) struct PreviewWork(Option<Instant>);
impl PreviewWork {
    pub(in crate::workshop) fn start() -> Self {
        Self(enabled().then(Instant::now))
    }
}
impl Drop for PreviewWork {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            current(|_, counters| {
                counters.add_value(
                    Field::PreviewWorkNs,
                    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                )
            });
        }
    }
}

pub(in crate::workshop) struct Trace {
    stop: AtomicBool,
    pub writer_sql: TraceHandle,
    pub reader_sql: TraceHandle,
    pub ui_sql: TraceHandle,
    writer: Counters,
    reader: Counters,
    pub ui: Counters,
    // This lock protects only replacement of an observer handle, never work.
    // The watchdog uses try_lock and holds it only long enough to clone it.
    native: Mutex<Option<(u64, u64, sortsize_store::scheduler::ProgressTrace)>>,
    pub cancel_requested: Event,
    pub cancel_control_sent: Event,
    pub cancel_observed: Event,
    pub cancel_terminal: Event,
    pub cancel_ui_ack: Event,
    pub cancel_applied: Event,
    visible_hash: [AtomicU64; 3],
}
impl Trace {
    pub(in crate::workshop) fn start() -> Option<Arc<Self>> {
        if !enabled() {
            return None;
        }
        let trace = Arc::new(Self {
            stop: AtomicBool::new(false),
            writer_sql: TraceHandle::new(),
            reader_sql: TraceHandle::new(),
            ui_sql: TraceHandle::new(),
            writer: Counters::default(),
            reader: Counters::default(),
            ui: Counters::default(),
            native: Mutex::new(None),
            cancel_requested: Event::default(),
            cancel_control_sent: Event::default(),
            cancel_observed: Event::default(),
            cancel_terminal: Event::default(),
            cancel_ui_ack: Event::default(),
            cancel_applied: Event::default(),
            visible_hash: std::array::from_fn(|_| AtomicU64::new(0)),
        });
        let observer = trace.clone();
        // Instrumentation failure must not prevent starting the application.
        std::thread::Builder::new()
            .name("scan-progress-observer".into())
            .spawn(move || observer.observe())
            .ok()?;
        Some(trace)
    }
    pub(in crate::workshop) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub(in crate::workshop) fn mark_first(&self, target: &Event) {
        self.mark_for_source(
            self.ui.values[Field::Source as usize].load(Ordering::Relaxed),
            target,
        );
    }
    pub(in crate::workshop) fn mark_for_source(&self, source: u64, target: &Event) {
        if target.source.load(Ordering::SeqCst) == source && target.time.load(Ordering::SeqCst) != 0
        {
            return;
        }
        target.sequence.fetch_add(1, Ordering::SeqCst);
        target.source.store(source, Ordering::SeqCst);
        target.time.store(now_ns(), Ordering::SeqCst);
        target.sequence.fetch_add(1, Ordering::SeqCst);
    }
    pub(in crate::workshop) fn start_source(&self, source: u64) {
        self.ui.context(source, 0, source, 0);
        for hash in &self.visible_hash {
            hash.store(0, Ordering::Relaxed);
        }
    }
    pub(in crate::workshop) fn native(
        &self,
        source: u64,
        generation: u64,
        session: &sortsize_store::scheduler::Session,
    ) {
        if let Ok(mut native) = self.native.try_lock()
            && native
                .as_ref()
                .is_none_or(|(epoch, id, _)| *epoch != source || *id != generation)
        {
            *native = session
                .progress_trace()
                .map(|handle| (source, generation, handle));
        }
    }
    pub(in crate::workshop) fn ui_poll(&self) {
        self.ui.add(Field::Polls);
        self.ui.write(&[(Field::LastPoll, now_ns())]);
    }
    pub(in crate::workshop) fn applied(
        &self,
        page: &Prepared,
        raw_changed: bool,
        models_changed: bool,
    ) {
        let now = now_ns();
        self.ui.add(Field::Pages);
        self.ui.write(&[
            (Field::Generation, page.frame.stamp.context.generation.0),
            (Field::View, page.epoch),
            (
                Field::InventoryHi,
                (page.frame.stamp.context.inventory.0 >> 64) as u64,
            ),
            (
                Field::InventoryLo,
                page.frame.stamp.context.inventory.0 as u64,
            ),
            (Field::Token, page.token as u64),
            (Field::PageRevision, page.frame.stamp.revision.0),
            (
                Field::Logical,
                page.root_measure.and_then(|m| m.logical).unwrap_or(0),
            ),
            (Field::Files, page.root_measure.map_or(0, |m| m.files)),
            (Field::FilesKnown, u64::from(page.root_measure.is_some())),
            (
                Field::Known,
                u64::from(page.root_measure.and_then(|m| m.logical).is_some()),
            ),
            (Field::LastApply, now),
        ]);
        if raw_changed {
            self.ui.write(&[(Field::LastRawChange, now)]);
        }
        if models_changed {
            self.ui.write(&[(Field::LastModelChange, now)]);
        }
    }
    pub(in crate::workshop) fn visible(&self, window: &crate::SortSizeWindow) {
        self.ui.write(&[
            (Field::Busy, u64::from(window.get_busy())),
            (Field::Cancelling, u64::from(window.get_cancelling())),
        ]);
        use std::hash::{Hash, Hasher};
        fn fingerprint(value: impl Hash) -> u64 {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            value.hash(&mut hash);
            hash.finish()
        }
        let values = [
            (
                Field::LastVisibleChange,
                fingerprint((
                    window.get_summary().as_str(),
                    window.get_current_folder_summary().as_str(),
                )),
            ),
            (
                Field::LastStateChange,
                fingerprint((
                    window.get_status().as_str(),
                    window.get_busy(),
                    window.get_cancelling(),
                )),
            ),
            (
                Field::LastProgressChange,
                fingerprint(window.get_scan_progress().as_str()),
            ),
        ];
        for (index, (field, hash)) in values.into_iter().enumerate() {
            if self.visible_hash[index].swap(hash, Ordering::Relaxed) != hash {
                self.ui.write(&[(field, now_ns())]);
            }
        }
    }
    fn observe(&self) {
        let mut sample = 0u64;
        while !self.stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(500));
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            sample += 1;
            let time = now_ns();
            let native = self
                .native
                .try_lock()
                .ok()
                .and_then(|native| native.clone());
            let mut line = format!("P2_WATCH sample={sample} monotonic_ns={time}");
            use std::fmt::Write as _;
            for (name, counters, sql) in [
                ("writer", &self.writer, &self.writer_sql),
                ("reader", &self.reader, &self.reader_sql),
                ("ui", &self.ui, &self.ui_sql),
            ] {
                if let Some((sequence, values)) = counters.read() {
                    let _ = write!(line, " {name}_coherent=true {name}_seq={sequence}");
                    for (key, value) in LABELS.iter().zip(values) {
                        let _ = write!(line, " {name}_{key}={value}");
                    }
                } else {
                    let _ = write!(line, " {name}_coherent=false");
                }
                let state = sql.read();
                let _ = write!(
                    line,
                    " {name}_work_valid={} {name}_work_seq={} {name}_work_phase={} {name}_work_phase_age_ns={} {name}_phase_depth={} {name}_completed={} {name}_inclusive_ns={} {name}_last_completed_ns={} {name}_slow={} {name}_max_ns={} {name}_max_phase={} {name}_last_slow_phase={} {name}_last_slow_ns={}",
                    state.valid,
                    state.sequence,
                    state.phase,
                    state.phase_elapsed_ns,
                    state.depth,
                    state.completed_count,
                    state.completed_elapsed_ns,
                    state.last_completed_ns,
                    state.slow_count,
                    state.max_completed_ns,
                    state.max_completed_phase,
                    state.last_slow_phase,
                    state.last_slow_ns
                );
                for (query, q) in [
                    ("pending_directories", state.pending_directories),
                    ("top100", state.top100),
                ] {
                    let _ = write!(
                        line,
                        " {name}_{query}_count={} {name}_{query}_vm={} {name}_{query}_fullscan={} {name}_{query}_sorts={} {name}_{query}_max_ns={} {name}_{query}_max_vm={} {name}_{query}_max_fullscan={} {name}_{query}_max_sorts={} {name}_{query}_last_ns={} {name}_{query}_saturated={}",
                        q.count,
                        q.vm_steps,
                        q.fullscan_steps,
                        q.sorts,
                        q.max_ns,
                        q.max_vm_steps,
                        q.max_fullscan_steps,
                        q.max_sorts,
                        q.last_ns,
                        q.saturated
                    );
                }
            }
            for (name, value) in [
                ("cancel_requested_ns", &self.cancel_requested),
                ("cancel_control_sent_ns", &self.cancel_control_sent),
                ("cancel_observed_ns", &self.cancel_observed),
                ("cancel_terminal_ns", &self.cancel_terminal),
                ("cancel_ui_ack_ns", &self.cancel_ui_ack),
                ("cancel_applied_ns", &self.cancel_applied),
            ] {
                let sequence = value.sequence.load(Ordering::SeqCst);
                let source = value.source.load(Ordering::SeqCst);
                let time = value.time.load(Ordering::SeqCst);
                let coherent =
                    sequence & 1 == 0 && sequence == value.sequence.load(Ordering::SeqCst);
                let _ = write!(
                    line,
                    " {name}={time} {name}_source={source} {name}_coherent={coherent}"
                );
            }
            let _ = writeln!(io::stderr().lock(), "{line}");
            if let Some((source, generation, native)) = native {
                let state = native.snapshot();
                let mut line = format!(
                    "P2_NATIVE sample={sample} monotonic_ns={time} approximate=true source_epoch={source} generation={generation} native_elapsed_ns={} dispatched={} started={} result_ready={} enqueued={} dequeued={} ingested={} acked={} abandoned={} observations_returned={} observations_ingested={} pending_completions={} in_flight={} global_acquisitions={} global_cursors={} writer_phase={:?} writer_phase_age_ns={}",
                    state.elapsed_ns,
                    state.dispatched,
                    state.started,
                    state.result_ready,
                    state.enqueued,
                    state.dequeued,
                    state.ingested,
                    state.acked,
                    state.abandoned,
                    state.observations_returned,
                    state.observations_ingested,
                    state.pending_completions,
                    state.in_flight,
                    state.global_acquisitions,
                    state.global_cursors,
                    state.writer_phase,
                    state.elapsed_ns.saturating_sub(state.writer_phase_since_ns)
                );
                for (name, stamp) in [
                    ("dispatch", state.last_dispatch_ns),
                    ("start", state.last_start_ns),
                    ("result_ready", state.last_result_ready_ns),
                    ("enqueue", state.last_enqueue_ns),
                    ("dequeue", state.last_dequeue_ns),
                    ("ingest", state.last_ingest_ns),
                    ("ack", state.last_ack_ns),
                ] {
                    let _ = write!(
                        line,
                        " last_{name}_known={} last_{name}_age_ns={}",
                        stamp != 0,
                        state.elapsed_ns.saturating_sub(stamp)
                    );
                }
                for (index, worker) in state.workers.iter().enumerate() {
                    let _ = write!(
                        line,
                        " worker{index}_phase={:?} worker{index}_phase_age_ns={} worker{index}_quantum={} worker{index}_observations={} worker{index}_dispatch_age_ns={} worker{index}_start_age_ns={} worker{index}_ready_age_ns={}",
                        worker.phase,
                        state.elapsed_ns.saturating_sub(worker.since_ns),
                        worker.quantum,
                        worker.observations,
                        state.elapsed_ns.saturating_sub(worker.dispatched_ns),
                        state.elapsed_ns.saturating_sub(worker.started_ns),
                        state.elapsed_ns.saturating_sub(worker.result_ready_ns)
                    );
                }
                let _ = writeln!(io::stderr().lock(), "{line}");
            }
        }
    }
}

#[derive(Default)]
pub(in crate::workshop) struct Event {
    sequence: AtomicU64,
    source: AtomicU64,
    time: AtomicU64,
}

pub(in crate::workshop) struct UiPoll<'a>(pub &'a Trace, pub &'a crate::SortSizeWindow);
impl Drop for UiPoll<'_> {
    fn drop(&mut self) {
        self.0.visible(self.1);
    }
}
