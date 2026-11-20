//! Bounded transactions, with a cooperative time check between windows.
//! A single slow window may exceed the budget; this is not a latency bound.
use std::time::{Duration, Instant};

pub(super) struct PublicationBatch {
    started: Instant,
    budget: Duration,
    windows: usize,
}

impl PublicationBatch {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            budget: Duration::from_millis(20),
            windows: 0,
        }
    }
    pub(super) fn next_window(&mut self) -> bool {
        if self.windows == 8 || (self.windows != 0 && self.started.elapsed() >= self.budget) {
            return false;
        }
        self.windows += 1;
        true
    }
    #[cfg(test)]
    pub(super) fn without_time_limit() -> Self {
        Self {
            budget: Duration::MAX,
            ..Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_are_bounded_and_an_expired_budget_still_admits_the_first() {
        let mut bounded = PublicationBatch::without_time_limit();
        for _ in 0..8 {
            assert!(bounded.next_window());
        }
        assert!(!bounded.next_window());
        let mut expired = PublicationBatch {
            budget: Duration::ZERO,
            ..PublicationBatch::new()
        };
        assert!(expired.next_window());
        assert!(!expired.next_window());
    }
}
