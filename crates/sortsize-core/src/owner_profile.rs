//! Opt-in validation counters, absent from ordinary builds. Process-local;
//! snapshot only after workers have joined. No paths, identities or SID bytes.
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
static OPEN_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static OPENS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SECURITY: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn attempt() {
    OPEN_ATTEMPTS.fetch_add(1, Relaxed);
}
pub(crate) fn security() {
    SECURITY.fetch_add(1, Relaxed);
}
pub(crate) struct Handle;
impl Handle {
    pub(crate) fn opened() -> Self {
        OPENS.fetch_add(1, Relaxed);
        PEAK.fetch_max(LIVE.fetch_add(1, Relaxed) + 1, Relaxed);
        Self
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Relaxed);
    }
}
/// open attempts, successful opens, security calls, peak live adapter handles,
/// live adapter handles. Lifetime counters, with no reset races.
pub fn snapshot() -> [usize; 5] {
    [
        OPEN_ATTEMPTS.load(Relaxed),
        OPENS.load(Relaxed),
        SECURITY.load(Relaxed),
        PEAK.load(Relaxed),
        LIVE.load(Relaxed),
    ]
}
