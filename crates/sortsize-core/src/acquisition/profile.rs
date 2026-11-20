//! Opt-in, path-free acquisition timings for one scheduler quantum.
//! EOF is inclusive. Historical workspace checks also include root/namespace
//! suboperations; a grouped check measures only the workspace observation in
//! Workspace, while the shared volume belongs to Root. Do not compare these
//! subcategory percentages across methods or add inclusive timings together.
//! No clock is read unless the caller explicitly starts an enabled profile.
use std::{
    cell::RefCell,
    io,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Workspace,
    Namespace,
    Root,
    Target,
    Cursor,
    Eof,
    Owner,
}
const KEYS: [&str; 7] = [
    "acq_workspace",
    "acq_namespace",
    "acq_root",
    "acq_target",
    "acq_cursor",
    "acq_eof",
    "acq_owner",
];

#[derive(Clone, Copy, Debug, Default)]
struct Timing {
    calls: u64,
    total: Duration,
    max: Duration,
}

/// Fixed-size counters; no paths, names, per-entry records or allocations.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectoryMetrics {
    stages: [Timing; 7],
}
impl DirectoryMetrics {
    pub fn merge(&mut self, other: &Self) {
        for (current, incoming) in self.stages.iter_mut().zip(other.stages) {
            current.calls += incoming.calls;
            current.total += incoming.total;
            current.max = current.max.max(incoming.max);
        }
    }
    pub fn write(&self, out: &mut impl io::Write) -> io::Result<()> {
        for (key, timing) in KEYS.iter().zip(self.stages) {
            write!(
                out,
                " {key}_calls={} {key}_ns={} {key}_max_ns={}",
                timing.calls,
                timing.total.as_nanos(),
                timing.max.as_nanos()
            )?;
        }
        Ok(())
    }
}

thread_local! {
    static CURRENT: RefCell<Option<DirectoryMetrics>> = const { RefCell::new(None) };
}

/// Lives on its originating worker thread; panic/drop discards its sample.
pub struct Profile {
    enabled: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Profile {
    pub fn start(enabled: bool) -> Self {
        if enabled {
            CURRENT.with(|current| *current.borrow_mut() = Some(DirectoryMetrics::default()));
        }
        Self {
            enabled,
            _thread: PhantomData,
        }
    }
    pub fn finish(mut self) -> DirectoryMetrics {
        let result = if self.enabled {
            CURRENT.with(|current| current.borrow_mut().take().unwrap_or_default())
        } else {
            DirectoryMetrics::default()
        };
        self.enabled = false;
        result
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        if self.enabled {
            CURRENT.with(|current| *current.borrow_mut() = None);
        }
    }
}

pub fn measure<T>(stage: Stage, operation: impl FnOnce() -> T) -> T {
    let start = CURRENT
        .with(|current| current.borrow().is_some())
        .then(Instant::now);
    let result = operation();
    if let Some(start) = start {
        let elapsed = start.elapsed();
        CURRENT.with(|current| {
            if let Some(metrics) = current.borrow_mut().as_mut() {
                let timing = &mut metrics.stages[stage as usize];
                timing.calls += 1;
                timing.total += elapsed;
                timing.max = timing.max.max(elapsed);
            }
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_and_finished_profiles_do_not_record_and_merge_is_additive() {
        let disabled = Profile::start(false);
        assert_eq!(measure(Stage::Target, || 42), 42);
        assert_eq!(disabled.finish().stages[Stage::Target as usize].calls, 0);
        let enabled = Profile::start(true);
        measure(Stage::Workspace, || measure(Stage::Target, || ()));
        let sample = enabled.finish();
        assert_eq!(sample.stages[Stage::Workspace as usize].calls, 1);
        assert_eq!(sample.stages[Stage::Target as usize].calls, 1);
        let mut aggregate = sample;
        aggregate.merge(&sample);
        assert_eq!(aggregate.stages[Stage::Target as usize].calls, 2);
        assert_eq!(
            aggregate.stages[Stage::Target as usize].total,
            sample.stages[Stage::Target as usize].total * 2
        );
        assert!(CURRENT.with(|current| current.borrow().is_none()));
    }
}
