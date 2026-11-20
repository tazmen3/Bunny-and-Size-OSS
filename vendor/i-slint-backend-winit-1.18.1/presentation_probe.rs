// SPDX-License-Identifier: GPL-3.0-only
//! Opt-in local benchmark hook. No file/network I/O and no production feature.
//! Captures successful software-buffer submissions, not compositor/display scanout.
use std::{cell::RefCell, time::Instant};
#[derive(Default)]
struct Probe {
    start: Option<Instant>,
    phase: u8,
    frames: u64,
    samples: Vec<(u8, f64, u64)>,
}
thread_local! { static PROBE: RefCell<Probe> = RefCell::new(Probe::default()); }
/// Arm immediately before dispatching the measured native-window input event.
pub fn arm() {
    PROBE.with(|p| {
        *p.borrow_mut() = Probe {
            start: Some(Instant::now()),
            samples: Vec::with_capacity(8),
            ..Default::default()
        }
    });
}
/// Set only after the product model carrying that state has been applied.
pub fn phase(phase: u8) {
    PROBE.with(|p| p.borrow_mut().phase = phase);
}
/// Read bounded first-presentation samples: phase, milliseconds, presented frame.
pub fn samples() -> Vec<(u8, f64, u64)> {
    PROBE.with(|p| p.borrow().samples.clone())
}
/// Stop collecting samples without changing the last bounded result.
pub fn disarm() {
    PROBE.with(|p| p.borrow_mut().start = None);
}
pub(crate) fn presented() {
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(start) = p.start {
            p.frames += 1;
            if p.phase > 0 && p.samples.len() < 8 && !p.samples.iter().any(|s| s.0 == p.phase) {
                let phase = p.phase;
                let frames = p.frames;
                p.samples
                    .push((phase, start.elapsed().as_secs_f64() * 1000., frames));
            }
        }
    });
}
