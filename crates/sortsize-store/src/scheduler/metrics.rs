use super::{COMMIT_QUANTA, State};
use sortsize_core::{
    acquisition::{NtfsListingStats, profile::DirectoryMetrics},
    resources::{Governor, Usage},
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Timing {
    calls: u64,
    total: Duration,
    max: Duration,
}

impl Timing {
    pub(super) fn add(&mut self, elapsed: Duration) {
        self.calls += 1;
        self.total += elapsed;
        self.max = self.max.max(elapsed);
    }

    pub(super) fn measure<T>(&mut self, enabled: bool, f: impl FnOnce() -> T) -> T {
        let start = enabled.then(Instant::now);
        let result = f();
        if let Some(start) = start {
            self.add(start.elapsed());
        }
        result
    }

    fn merge(&mut self, other: Self) {
        self.merge_ref(&other);
    }

    pub(super) fn merge_ref(&mut self, other: &Self) {
        self.calls += other.calls;
        self.total += other.total;
        self.max = self.max.max(other.max);
    }

    pub(super) fn write(&self, out: &mut impl Write, key: &str) -> io::Result<()> {
        write!(
            out,
            " {key}_calls={} {key}_ns={} {key}_max_ns={}",
            self.calls,
            self.total.as_nanos(),
            self.max.as_nanos(),
        )
    }
}

pub(super) struct WorkerSample {
    pub(super) dispatched: Instant,
    pub(super) started: Instant,
    pub(super) finished: Instant,
    pub(super) checks: Timing,
    pub(super) enumeration: Timing,
    pub(super) metadata: Timing,
    pub(super) acquisition: DirectoryMetrics,
    pub(super) observation_budget_bytes: usize,
    pub(super) ntfs: NtfsListingStats,
}

#[derive(Default)]
pub(super) struct Metrics {
    pub(super) dispatched: u64,
    pub(super) observations_returned: usize,
    pub(super) receive: Timing,
    pub(super) ingest: Timing,
    pub(super) grouped_ingestions: u64,
    pub(super) admission: Timing,
    pub(super) selection: Timing,
    pub(super) preparation: Timing,
    pub(super) dispatch: Timing,
    pub(super) new_attempts: u64,
    pub(super) grouped_admissions: u64,
    pub(super) grouped_admission_failures: u64,
    pub(super) parked_dispatches: u64,
    pub(super) acquisition_permit_denials: u64,
    pub(super) cursor_permit_denials: u64,
    new_attempts_per_tick: [u64; 9],
    new_attempts_at_tick_start: u64,
    memory: Option<Usage>,
    governor_samples: u64,
    governor_peak_bytes: u64,
    governor_peak_categories: [u64; 6],
    governor_peak_entries: usize,
    session_transport_reserved_bytes: u64,
    transport_observation_bytes_total: u64,
    transport_observation_bytes_max: usize,
    first_tick: Option<Instant>,
    last_tick_end: Option<(Instant, usize)>,
    between_ticks: Timing,
    ready_between_ticks: Timing,
    tick: Timing,
    pending_at_tick_start: [u64; 9],
    capped_drains: u64,
    worker_started: Option<Instant>,
    worker_finished: Option<Instant>,
    dispatch_wait: Timing,
    completion_wait: Timing,
    worker: Timing,
    checks: Timing,
    enumeration: Timing,
    metadata: Timing,
    acquisition: DirectoryMetrics,
    ntfs: NtfsListingStats,
}

impl Metrics {
    pub(super) fn requested(session_transport_reserved_bytes: u64) -> Option<Self> {
        std::env::var_os("BUNNY_SCAN_P1_METRICS")
            .is_some_and(|value| value == "1")
            .then(|| Self {
                session_transport_reserved_bytes,
                ..Self::default()
            })
    }

    fn sample_memory(&mut self) {
        let usage = Governor::global().usage();
        self.governor_samples += 1;
        self.governor_peak_bytes = self.governor_peak_bytes.max(usage.categories.iter().sum());
        self.governor_peak_entries = self.governor_peak_entries.max(usage.distinct_entries);
        for (peak, used) in self
            .governor_peak_categories
            .iter_mut()
            .zip(usage.categories)
        {
            *peak = (*peak).max(used);
        }
        self.memory = Some(usage);
    }

    pub(super) fn start_tick(&mut self, pending: usize) -> Instant {
        let start = Instant::now();
        self.first_tick.get_or_insert(start);
        if let Some((previous, pending_at_end)) = self.last_tick_end {
            let elapsed = start.duration_since(previous);
            self.between_ticks.add(elapsed);
            // The same writer consumes all completions, so a ready quantum
            // at the end of a tick remains queued until the following tick.
            if pending_at_end != 0 {
                self.ready_between_ticks.add(elapsed);
            }
        }
        self.pending_at_tick_start[pending.min(8)] += 1;
        self.new_attempts_at_tick_start = self.new_attempts;
        start
    }

    pub(super) fn end_tick(&mut self, start: Instant, pending: usize) {
        let end = Instant::now();
        self.tick.add(end.duration_since(start));
        self.last_tick_end = Some((end, pending));
        let attempts = (self.new_attempts - self.new_attempts_at_tick_start).min(8);
        self.new_attempts_per_tick[attempts as usize] += 1;
        self.sample_memory();
    }

    pub(super) fn capped_drain(&mut self) {
        self.capped_drains += 1;
    }

    pub(super) fn completion(&mut self, sample: WorkerSample) {
        self.worker_started = Some(
            self.worker_started
                .map_or(sample.started, |s| s.min(sample.started)),
        );
        self.worker_finished = Some(
            self.worker_finished
                .map_or(sample.finished, |s| s.max(sample.finished)),
        );
        self.dispatch_wait
            .add(sample.started.duration_since(sample.dispatched));
        self.completion_wait.add(sample.finished.elapsed());
        self.worker
            .add(sample.finished.duration_since(sample.started));
        self.checks.merge(sample.checks);
        self.enumeration.merge(sample.enumeration);
        self.metadata.merge(sample.metadata);
        self.acquisition.merge(&sample.acquisition);
        self.ntfs.batch_calls += sample.ntfs.batch_calls;
        self.ntfs.relative_opens += sample.ntfs.relative_opens;
        self.ntfs.stale_rows += sample.ntfs.stale_rows;
        self.transport_observation_bytes_total = self
            .transport_observation_bytes_total
            .saturating_add(sample.observation_budget_bytes as u64);
        self.transport_observation_bytes_max = self
            .transport_observation_bytes_max
            .max(sample.observation_budget_bytes);
    }

    pub(super) fn report(
        self,
        state: State,
        committed: u64,
        abandoned: u64,
        acquisition_errors: u64,
    ) {
        // Best effort on redirected stderr. Never turn a diagnostic write
        // failure into a failed acquisition or log native names/paths.
        let _ = self.write(
            &mut io::stderr().lock(),
            state,
            committed,
            abandoned,
            acquisition_errors,
        );
    }

    fn write(
        &self,
        out: &mut impl Write,
        state: State,
        committed: u64,
        abandoned: u64,
        acquisition_errors: u64,
    ) -> io::Result<()> {
        let drain_wall = self
            .first_tick
            .zip(self.last_tick_end)
            .map_or(Duration::ZERO, |(a, (b, _))| b.duration_since(a));
        let worker_wall = self
            .worker_started
            .zip(self.worker_finished)
            .map_or(Duration::ZERO, |(a, b)| b.duration_since(a));
        let last_worker = self
            .first_tick
            .zip(self.worker_finished)
            .map_or(Duration::ZERO, |(a, b)| b.duration_since(a));
        write!(
            out,
            "scan_p1_scheduler state={state:?} commit_quanta={COMMIT_QUANTA} dispatched={} committed={committed} abandoned={abandoned} acquisition_errors={acquisition_errors} observations_returned={} drain_wall_ns={} worker_span_ns={} last_worker_since_first_tick_ns={} capped_drains={} grouped_ingest_calls={} pending_at_tick_start=",
            self.dispatched,
            self.observations_returned,
            drain_wall.as_nanos(),
            worker_wall.as_nanos(),
            last_worker.as_nanos(),
            self.capped_drains,
            self.grouped_ingestions,
        )?;
        write_counts(out, &self.pending_at_tick_start)?;
        write!(
            out,
            " new_attempts={} grouped_admissions={} grouped_admission_failures={} parked_dispatches={} acquisition_permit_denials={} cursor_permit_denials={} new_attempts_per_tick=",
            self.new_attempts,
            self.grouped_admissions,
            self.grouped_admission_failures,
            self.parked_dispatches,
            self.acquisition_permit_denials,
            self.cursor_permit_denials,
        )?;
        write_counts(out, &self.new_attempts_per_tick)?;
        write!(
            out,
            " session_transport_reserved_bytes={} transport_observation_bytes_total={} transport_observation_bytes_max={}",
            self.session_transport_reserved_bytes,
            self.transport_observation_bytes_total,
            self.transport_observation_bytes_max,
        )?;
        if let Some(usage) = self.memory {
            // The latest limits describe the same sampled window as the peaks.
            // Independent category peaks need not occur in the same tick.
            write!(
                out,
                " governor_samples={} governor_byte_limit={} governor_entry_limit={} governor_native_fallback={} governor_peak_bytes={} governor_peak_entries={} governor_category_limits=",
                self.governor_samples,
                usage.limits.bytes,
                usage.limits.entries.unwrap_or(u64::MAX),
                u8::from(usage.limits.fallback),
                self.governor_peak_bytes,
                self.governor_peak_entries,
            )?;
            write_counts(out, &usage.limits.categories)?;
            write!(out, " governor_category_peaks=")?;
            write_counts(out, &self.governor_peak_categories)?;
        }
        write!(
            out,
            " ntfs_batch_calls={} ntfs_relative_opens={} ntfs_stale_rows={}",
            self.ntfs.batch_calls, self.ntfs.relative_opens, self.ntfs.stale_rows
        )?;
        for (key, timing) in [
            ("tick", &self.tick),
            ("between_ticks", &self.between_ticks),
            ("ready_between_ticks", &self.ready_between_ticks),
            ("admission", &self.admission),
            ("selection", &self.selection),
            ("preparation", &self.preparation),
            ("dispatch", &self.dispatch),
            ("receive", &self.receive),
            ("ingest", &self.ingest),
            ("dispatch_wait", &self.dispatch_wait),
            ("completion_wait", &self.completion_wait),
            ("worker", &self.worker),
            ("worker_directory_checks", &self.checks),
            ("worker_enumeration", &self.enumeration),
            ("worker_metadata", &self.metadata),
        ] {
            timing.write(out, key)?;
        }
        self.acquisition.write(out)?;
        writeln!(out)
    }
}

fn write_counts(out: &mut impl Write, counts: &[u64]) -> io::Result<()> {
    for (i, count) in counts.iter().enumerate() {
        if i != 0 {
            write!(out, ",")?;
        }
        write!(out, "{count}")?;
    }
    Ok(())
}
