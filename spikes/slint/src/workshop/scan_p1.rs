//! Cadence for provisional views, and opt-in local scan counters.
pub(super) mod progress_trace;
use super::service::Prepared;
use sortsize_core::inventory_contract::{GenerationId, InventoryId};
use sortsize_core::resources::Governor;
use sortsize_store::scheduler::{State, Status};
use std::{
    io::{self, Write},
    sync::OnceLock,
    time::{Duration, Instant},
};

const ACQUIRING_QUANTUM_INTERVAL: Duration = Duration::from_millis(50);
const MIN_QUANTUM_INTERVAL: Duration = Duration::from_millis(5);
const ACQUIRING_FRAME_INTERVAL: Duration = Duration::from_millis(500);
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

pub(super) fn writer_drain_ready(status: Status) -> bool {
    status.state == State::Running && status.pending_completions >= 2
}

pub(super) fn writer_finalize_ready(status: Status, progressed: bool) -> bool {
    progressed
        && status.state == State::Idle
        && status.in_flight == 0
        && status.pending_completions == 0
}

#[derive(Default)]
pub(super) struct ProjectionCadence {
    scan: Option<(u64, GenerationId)>,
    first_total: bool,
    priority: bool,
    last_quantum: Option<Instant>,
    last_quantum_cost: Option<Duration>,
}

impl ProjectionCadence {
    pub fn bind(&mut self, scan: (u64, GenerationId)) {
        if self.scan != Some(scan) {
            *self = Self {
                scan: Some(scan),
                ..Default::default()
            };
        }
    }

    pub fn prioritize(&mut self) {
        self.priority = true;
    }

    pub fn allow_quantum(&mut self, forced: bool, ingestion_pending: bool, now: Instant) -> bool {
        // A revision remains pinned until its bounded steps finish. Pace the
        // full projection turn, not its row count: cheap cached steps must not
        // spend 50ms waiting to make another few hundred operations. Budget
        // roughly as much time between starts for acquisition as for projection,
        // without extending the previous 50ms interval for an expensive step.
        // Returned batches retain the old interval so ingestion can drain first.
        let interval = if ingestion_pending {
            ACQUIRING_QUANTUM_INTERVAL
        } else {
            self.last_quantum_cost
                .map_or(ACQUIRING_QUANTUM_INTERVAL, |cost| {
                    cost.saturating_mul(2)
                        .clamp(MIN_QUANTUM_INTERVAL, ACQUIRING_QUANTUM_INTERVAL)
                })
        };
        if self.first_total
            && !self.priority
            && !forced
            && self
                .last_quantum
                .is_some_and(|last| now.duration_since(last) < interval)
        {
            return false;
        }
        self.last_quantum = Some(now);
        true
    }

    pub fn finished_quantum(&mut self, elapsed: Duration) {
        self.last_quantum_cost = Some(elapsed);
    }

    pub fn finalizing_quantum(&self) -> bool {
        // The certified reader will build the final view. Only an explicit
        // navigation request justifies more work on the dated live snapshot.
        self.priority
    }

    pub fn frame_interval(&self, acquiring: bool) -> Duration {
        if acquiring && self.first_total && !self.priority {
            ACQUIRING_FRAME_INTERVAL
        } else {
            FRAME_INTERVAL
        }
    }

    pub fn published(&mut self, positive_total: bool, settled: bool) {
        self.first_total |= positive_total;
        if settled {
            self.priority = false;
        }
    }
}

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BUNNY_SCAN_P1_METRICS").is_some_and(|v| v == "1"))
}

#[derive(Clone, Copy, Default)]
pub(super) struct Work {
    calls: u64,
    elapsed: Duration,
}

pub(super) fn measure<T>(work: Option<&mut Work>, action: impl FnOnce() -> T) -> T {
    let Some(work) = work else {
        return action();
    };
    let start = Instant::now();
    let value = action();
    work.calls += 1;
    work.elapsed += start.elapsed();
    value
}

#[derive(Default)]
pub(super) struct PublicationWork {
    pub close_preview: Work,
    pub repository_publish: Work,
    pub open_certified: Work,
}

pub(super) struct WriterMetrics {
    pub scan: (u64, GenerationId),
    pub reported: bool,
    started: Instant,
    acquisition_finished: Option<Duration>,
    work_at_idle: Option<[Work; 6]>,
    pub tick: Work,
    pub sleep: Work,
    pub drain_yield: Work,
    pub finalize: Work,
    pub fold: Work,
    pub order: Work,
    pub tree: Work,
    pub prepare: Work,
    pub measurement: Work,
    pub publish: Work,
    pub publication: PublicationWork,
    pub projection_deferred: u64,
}

impl WriterMetrics {
    pub fn new(scan: (u64, GenerationId)) -> Self {
        Self {
            scan,
            reported: false,
            started: Instant::now(),
            acquisition_finished: None,
            work_at_idle: None,
            tick: Work::default(),
            sleep: Work::default(),
            drain_yield: Work::default(),
            finalize: Work::default(),
            fold: Work::default(),
            order: Work::default(),
            tree: Work::default(),
            prepare: Work::default(),
            measurement: Work::default(),
            publish: Work::default(),
            publication: PublicationWork::default(),
            projection_deferred: 0,
        }
    }

    fn continuing_work(&self) -> [(&'static str, Work); 6] {
        [
            ("tick", self.tick),
            ("sleep", self.sleep),
            ("fold", self.fold),
            ("order", self.order),
            ("tree", self.tree),
            ("prepare", self.prepare),
        ]
    }

    pub fn acquisition_finished(&mut self) {
        if self.acquisition_finished.is_none() {
            self.acquisition_finished = Some(self.started.elapsed());
            self.work_at_idle = Some(self.continuing_work().map(|(_, work)| work));
            sortsize_store::report_scan_metrics("idle");
        }
    }

    pub fn report(&mut self, outcome: &'static str) {
        if self.reported {
            return;
        }
        self.reported = true;
        let (epoch, generation) = self.scan;
        let wall = self.started.elapsed();
        let mut line = format!(
            "P1_SERVICE outcome={outcome} source_epoch={epoch} generation={} wall_us={} acquisition_wall_us={} after_acquisition_wall_us={} projection_deferred={}",
            generation.0,
            wall.as_micros(),
            optional_us(self.acquisition_finished),
            optional_us(self.acquisition_finished.map(|t| wall.saturating_sub(t))),
            self.projection_deferred,
        );
        append_work(
            &mut line,
            [
                ("tick", &self.tick),
                ("sleep", &self.sleep),
                ("drain_yield", &self.drain_yield),
                ("finalize", &self.finalize),
                ("fold", &self.fold),
                ("order", &self.order),
                ("tree", &self.tree),
                ("prepare", &self.prepare),
                ("measurement", &self.measurement),
                ("publish", &self.publish),
                ("close_preview", &self.publication.close_preview),
                ("repository_publish", &self.publication.repository_publish),
                ("open_certified", &self.publication.open_certified),
            ],
        );
        if let Some(before_idle) = self.work_at_idle {
            use std::fmt::Write;
            for ((name, total), before) in self.continuing_work().into_iter().zip(before_idle) {
                let _ = write!(
                    line,
                    " after_idle_{name}_calls={} after_idle_{name}_us={}",
                    total.calls - before.calls,
                    (total.elapsed - before.elapsed).as_micros(),
                );
            }
        }
        if outcome == "published" {
            append_handoff_memory(&mut line);
        }
        let _ = writeln!(io::stderr().lock(), "{line}");
        sortsize_store::report_scan_metrics(outcome);
    }
}

fn append_handoff_memory(line: &mut String) {
    use std::fmt::Write;
    let usage = Governor::global().usage();
    let _ = write!(
        line,
        " handoff_reserved_bytes={} handoff_entries={} handoff_limit_bytes={}",
        usage.categories.iter().sum::<u64>(),
        usage.distinct_entries,
        usage.limits.bytes,
    );
    if let Some(limit) = usage.limits.entries {
        let _ = write!(line, " handoff_limit_entries={limit}");
    }
    for (name, [a, b, c, d, e, f]) in [
        ("handoff_reserved_by_category", usage.categories),
        ("handoff_limit_by_category", usage.limits.categories),
    ] {
        let _ = write!(line, " {name}={a},{b},{c},{d},{e},{f}");
    }
}

fn optional_us(elapsed: Option<Duration>) -> String {
    elapsed.map_or_else(|| "none".to_owned(), |t| t.as_micros().to_string())
}

fn append_work<'a>(line: &mut String, items: impl IntoIterator<Item = (&'a str, &'a Work)>) {
    use std::fmt::Write;
    for (name, work) in items {
        let _ = write!(
            line,
            " {name}_calls={} {name}_us={}",
            work.calls,
            work.elapsed.as_micros()
        );
    }
}

pub(super) struct ReaderMetrics {
    handoff: Instant,
    pub fold: Work,
    pub tree: Work,
    pub order: Work,
    pub prepare: Work,
}

impl ReaderMetrics {
    pub fn new(handoff: Instant) -> Self {
        Self {
            handoff,
            fold: Work::default(),
            tree: Work::default(),
            order: Work::default(),
            prepare: Work::default(),
        }
    }

    pub fn report(self, source_epoch: u64, generation: GenerationId) {
        let mut line = format!(
            "P1_READER source_epoch={source_epoch} generation={} handoff_to_frame_wall_us={}",
            generation.0,
            self.handoff.elapsed().as_micros(),
        );
        append_work(
            &mut line,
            [
                ("fold", &self.fold),
                ("tree", &self.tree),
                ("order", &self.order),
                ("prepare", &self.prepare),
            ],
        );
        let _ = writeln!(io::stderr().lock(), "{line}");
    }
}

pub(super) struct UiMetrics {
    pub source_epoch: u64,
    retained: Option<(InventoryId, GenerationId)>,
    started: Instant,
    first_total: Option<Duration>,
    last_total: Option<Duration>,
    last_summary: String,
    max_total_gap: Duration,
    pages: u64,
    apply: Duration,
}

impl UiMetrics {
    pub fn new(source_epoch: u64, retained: Option<(InventoryId, GenerationId)>) -> Self {
        Self {
            source_epoch,
            retained,
            started: Instant::now(),
            first_total: None,
            last_total: None,
            last_summary: String::new(),
            max_total_gap: Duration::ZERO,
            pages: 0,
            apply: Duration::ZERO,
        }
    }

    pub fn applied(&mut self, page: &Prepared, ready: bool, apply: Duration) -> bool {
        let context = page.frame.stamp.context;
        if self.retained == Some((context.inventory, context.generation)) {
            return false;
        }
        self.pages += 1;
        self.apply += apply;
        let elapsed = self.started.elapsed();
        if page
            .root_measure
            .and_then(|measure| measure.logical)
            .is_some_and(|logical| logical > 0)
            && self.last_summary != page.summary
        {
            self.first_total.get_or_insert(elapsed);
            self.max_total_gap = self.max_total_gap.max(
                self.last_total
                    .map_or(Duration::ZERO, |last| elapsed.saturating_sub(last)),
            );
            self.last_total = Some(elapsed);
            self.last_summary.clone_from(&page.summary);
        }
        if ready {
            let _ = writeln!(
                io::stderr().lock(),
                "P1_UI source_epoch={} generation={} first_total_wall_us={} ready_wall_us={} max_changed_total_gap_us={} pages={} apply_us={} final_apply_us={} frame_to_apply_end_us={}",
                self.source_epoch,
                context.generation.0,
                optional_us(self.first_total),
                elapsed.as_micros(),
                self.max_total_gap.as_micros(),
                self.pages,
                self.apply.as_micros(),
                apply.as_micros(),
                optional_us(page.p1_frame_ready_at.map(|t| t.elapsed())),
            );
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_drain_requires_two_ready_completions_and_a_running_writer() {
        let mut status = Status {
            state: State::Running,
            in_flight: 8,
            pending_completions: 1,
            acquisitions_pending: 7,
            abandoned_quanta: 0,
            parked: 0,
            global_acquisitions: 8,
            global_cursors: 8,
            quanta: 1,
            processed_entries: 0,
        };
        assert!(!writer_drain_ready(status));

        status.pending_completions = 2;
        status.acquisitions_pending = 6;
        assert!(writer_drain_ready(status));
        for state in [State::Paused, State::Cancelled, State::StoreError] {
            status.state = state;
            assert!(!writer_drain_ready(status));
        }
    }

    #[test]
    fn productive_finalization_yields_only_until_settled_work_or_state_changes() {
        let mut status = Status {
            state: State::Idle,
            in_flight: 0,
            pending_completions: 0,
            acquisitions_pending: 0,
            abandoned_quanta: 0,
            parked: 0,
            global_acquisitions: 0,
            global_cursors: 0,
            quanta: 10,
            processed_entries: 512,
        };
        // Idle alone must keep its normal sleep. Progress is local to this turn,
        // so an earlier successful quantum cannot sustain an idle spin.
        assert!(!writer_finalize_ready(status, false));
        assert!(writer_finalize_ready(status, true));
        assert!(!writer_drain_ready(status));
        status.in_flight = 1;
        assert!(!writer_finalize_ready(status, true));
        status.in_flight = 0;
        status.pending_completions = 1;
        assert!(!writer_finalize_ready(status, true));
        status.pending_completions = 0;
        for state in [
            State::Running,
            State::Paused,
            State::Cancelled,
            State::StoreError,
            State::CancelledStoreError,
            State::Sealed,
        ] {
            status.state = state;
            assert!(!writer_finalize_ready(status, true));
        }
    }

    #[test]
    fn deferral_is_bounded_and_yields_to_first_total_and_navigation() {
        let mut cadence = ProjectionCadence::default();
        let now = Instant::now();
        cadence.bind((1, GenerationId(1)));
        assert!(cadence.allow_quantum(false, false, now));
        assert!(cadence.allow_quantum(false, true, now));
        assert_eq!(cadence.frame_interval(true), FRAME_INTERVAL);
        cadence.published(true, true);
        assert!(!cadence.allow_quantum(false, false, now));
        assert!(cadence.allow_quantum(false, false, now + ACQUIRING_QUANTUM_INTERVAL));
        assert_eq!(cadence.frame_interval(true), ACQUIRING_FRAME_INTERVAL);
        assert!(!cadence.finalizing_quantum());

        cadence.prioritize();
        cadence.published(true, false);
        assert!(cadence.allow_quantum(false, true, now + ACQUIRING_QUANTUM_INTERVAL));
        assert!(cadence.finalizing_quantum());
        assert_eq!(cadence.frame_interval(true), FRAME_INTERVAL);
        cadence.published(true, true);
        assert!(!cadence.allow_quantum(false, false, now + ACQUIRING_QUANTUM_INTERVAL));
        assert!(!cadence.finalizing_quantum());

        cadence.bind((2, GenerationId(1)));
        assert!(cadence.allow_quantum(false, true, now + ACQUIRING_QUANTUM_INTERVAL));
    }

    #[test]
    fn cheap_catchup_advances_without_increasing_frame_frequency_or_bypassing_ingestion() {
        let now = Instant::now();
        let mut cadence = ProjectionCadence::default();
        cadence.bind((1, GenerationId(1)));
        assert!(cadence.allow_quantum(false, false, now));
        cadence.finished_quantum(Duration::from_millis(1));
        cadence.published(true, true);
        assert!(!cadence.allow_quantum(false, false, now + Duration::from_millis(4)));
        assert!(cadence.allow_quantum(false, false, now + Duration::from_millis(5)));
        assert_eq!(cadence.frame_interval(true), Duration::from_millis(500));
        cadence.finished_quantum(Duration::ZERO);
        assert!(!cadence.allow_quantum(false, false, now + Duration::from_millis(9)));
        // A returned-work backlog restores the old interval despite cheap work.
        assert!(!cadence.allow_quantum(false, true, now + Duration::from_millis(10)));
        assert!(cadence.allow_quantum(false, true, now + Duration::from_millis(55)));
        // Explicit navigation and forced refresh still take the next turn.
        assert!(cadence.allow_quantum(true, true, now + Duration::from_millis(55)));
        cadence.prioritize();
        assert!(cadence.allow_quantum(false, true, now + Duration::from_millis(55)));
        assert_eq!(cadence.frame_interval(true), FRAME_INTERVAL);
    }

    #[test]
    fn expensive_projection_and_new_source_do_not_inherit_an_aggressive_budget() {
        let now = Instant::now();
        let mut cadence = ProjectionCadence::default();
        cadence.bind((1, GenerationId(1)));
        assert!(cadence.allow_quantum(false, false, now));
        cadence.finished_quantum(Duration::from_millis(20));
        cadence.published(true, true);
        assert!(!cadence.allow_quantum(false, false, now + Duration::from_millis(39)));
        assert!(cadence.allow_quantum(false, false, now + Duration::from_millis(40)));
        cadence.finished_quantum(Duration::from_millis(30));
        assert!(!cadence.allow_quantum(false, false, now + Duration::from_millis(89)));
        assert!(cadence.allow_quantum(false, false, now + Duration::from_millis(90)));
        cadence.finished_quantum(Duration::from_millis(150));
        // A native/SQLite overrun must not add another delay after completion.
        assert!(cadence.allow_quantum(false, false, now + Duration::from_millis(240)));
        cadence.finished_quantum(Duration::from_millis(1));
        cadence.bind((2, GenerationId(1)));
        assert!(cadence.allow_quantum(false, true, now + Duration::from_millis(240)));
        cadence.published(true, true);
        assert!(!cadence.allow_quantum(false, false, now + Duration::from_millis(245)));
        assert!(cadence.allow_quantum(false, false, now + Duration::from_millis(290)));
    }
}
