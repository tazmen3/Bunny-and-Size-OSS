use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

/// Number of entry observations returned by native workers in this run.
/// Retries can observe an entry again. This is separate from the inventory total.
pub(super) struct Progress {
    pub source_epoch: u64,
    pub processed_entries: Arc<AtomicU64>,
}

impl Progress {
    pub fn label(&self, phase: super::service::Phase) -> String {
        let count = self.processed_entries.load(Ordering::Relaxed);
        format!(
            "{} · {}",
            phase.label(),
            tr_count!("{n} entry processed", "{n} entries processed", count)
        )
    }
}

pub(super) const DISPLAY_INTERVAL: Duration = Duration::from_millis(200);
