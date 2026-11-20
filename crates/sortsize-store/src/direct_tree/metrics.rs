//! Fixed-size, opt-in direct-tree timings. No source paths or names are recorded.
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Prepare,
    Encode,
    Append,
    Index,
    Propagate,
    Flush,
}

const LABELS: [&str; 6] = [
    "prepare",
    "spool_encode",
    "spool_append",
    "index",
    "propagate",
    "finish_flush",
];
const UNITS: [&str; 6] = [
    "input_rows",
    "retained_rows",
    "encoded_bytes",
    "retained_rows",
    "ancestor_visits",
    "buffered_bytes",
];

#[derive(Clone, Copy, Default)]
struct Sample {
    count: u64,
    elapsed_ns: u64,
    max_ns: u64,
    units: u64,
    failures: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Metrics {
    samples: [Sample; LABELS.len()],
}

impl Metrics {
    pub(super) fn start(metrics: Option<&mut Self>, phase: Phase) -> Timer<'_> {
        let started = metrics.is_some().then(Instant::now);
        Timer {
            sample: metrics.map(|metrics| &mut metrics.samples[phase as usize]),
            started,
            units: 0,
            failed: true,
        }
    }

    /// Durations are nested inside the session's tree_accept/tree_finish clocks.
    /// Buffered writes/flushes are userspace API durations, not durable I/O.
    pub(crate) fn report(&self, outcome: &'static str) {
        use std::io::Write;
        let mut output = std::io::stderr().lock();
        for ((label, unit), sample) in LABELS.iter().zip(UNITS).zip(self.samples) {
            if sample.count != 0 {
                let _ = writeln!(
                    output,
                    "scan_direct_tree outcome={outcome} phase={label} count={} elapsed_ns={} max_ns={} units={} unit={unit} failures={}",
                    sample.count, sample.elapsed_ns, sample.max_ns, sample.units, sample.failures,
                );
            }
        }
    }
}

pub(super) struct Timer<'a> {
    sample: Option<&'a mut Sample>,
    started: Option<Instant>,
    units: u64,
    failed: bool,
}

impl Timer<'_> {
    pub(super) fn finish(mut self, units: u64) {
        self.units = units;
        self.failed = false;
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        if let Some((sample, started)) = self.sample.as_mut().zip(self.started) {
            let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            sample.count = sample.count.saturating_add(1);
            sample.elapsed_ns = sample.elapsed_ns.saturating_add(ns);
            sample.max_ns = sample.max_ns.max(ns);
            sample.units = sample.units.saturating_add(self.units);
            sample.failures = sample.failures.saturating_add(u64::from(self.failed));
        }
    }
}
