//! Passive, bounded event-loop diagnostics, development feature only.
use i_slint_backend_winit::{CustomApplicationHandler, EventResult, winit};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{cell::RefCell, rc::Rc, time::Instant};
static CPU_PROBE_MAX_NS: AtomicU64 = AtomicU64::new(0);

fn cpu_ms() -> Option<f64> {
    let started = Instant::now();
    let value = platform_cpu_ms();
    CPU_PROBE_MAX_NS.fetch_max(
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    value
}
fn platform_cpu_ms() -> Option<f64> {
    #[cfg(windows)]
    {
        super::windows::thread_cpu_ms().ok()
    }
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Time {
            seconds: std::ffi::c_long,
            nanos: std::ffi::c_long,
        }
        unsafe extern "C" {
            fn clock_gettime(clock: i32, time: *mut Time) -> i32;
        }
        let mut time = Time {
            seconds: 0,
            nanos: 0,
        };
        // SAFETY: writable C timespec, Linux CLOCK_THREAD_CPUTIME_ID = 3.
        if unsafe { clock_gettime(3, &mut time) } == 0 {
            Some(time.seconds as f64 * 1000.0 + time.nanos as f64 / 1_000_000.0)
        } else {
            None
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

struct Span {
    started: Instant,
    cpu: Option<f64>,
    phase: &'static str,
    kind: &'static str,
}
impl Span {
    fn new(phase: &'static str, kind: &'static str) -> Self {
        Self {
            started: Instant::now(),
            cpu: cpu_ms(),
            phase,
            kind,
        }
    }
}
struct SlowSpan {
    at_ms: f64,
    wall_ms: f64,
    cpu_ms: Option<f64>,
    phase: &'static str,
    kind: &'static str,
}
pub(super) struct Trace {
    origin: Instant,
    pub(super) phase: &'static str,
    dispatch: Option<Span>,
    event: Option<Span>,
    idle_cpu: Option<f64>,
    wake_max_ms: f64,
    dispatch_max_ms: f64,
    redraw_max_ms: f64,
    redraw_samples: Vec<f64>,
    spans: Vec<SlowSpan>,
    omitted: usize,
}
impl Default for Trace {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            phase: "startup",
            dispatch: None,
            event: None,
            idle_cpu: None,
            wake_max_ms: 0.0,
            dispatch_max_ms: 0.0,
            redraw_max_ms: 0.0,
            redraw_samples: Vec::new(),
            spans: Vec::new(),
            omitted: 0,
        }
    }
}
impl Trace {
    fn record(&mut self, kind: &'static str, phase: &'static str, wall_ms: f64, cpu: Option<f64>) {
        if wall_ms < 50.0 {
            return;
        }
        if self.spans.len() < 128 {
            self.spans.push(SlowSpan {
                at_ms: self.origin.elapsed().as_secs_f64() * 1000.0,
                wall_ms,
                cpu_ms: cpu,
                phase,
                kind,
            });
        } else {
            self.omitted += 1;
        }
    }
    fn finish_event(&mut self) {
        if let Some(span) = self.event.take() {
            let ms = span.started.elapsed().as_secs_f64() * 1000.0;
            if span.kind == "redraw" {
                self.redraw_max_ms = self.redraw_max_ms.max(ms);
                if self.redraw_samples.len() < 4096 {
                    self.redraw_samples.push(ms);
                }
            }
            self.record(
                span.kind,
                span.phase,
                ms,
                cpu_ms()
                    .zip(span.cpu)
                    .map(|(end, start)| (end - start).max(0.0)),
            );
        }
    }
    pub(super) fn json(&self) -> String {
        let mut redraw_samples = self.redraw_samples.clone();
        redraw_samples.sort_by(f64::total_cmp);
        let redraw_p95 = redraw_samples
            .get(redraw_samples.len().saturating_sub(1) * 95 / 100)
            .copied()
            .unwrap_or(0.0);
        let spans: Vec<_> = self.spans.iter().map(|s| format!(
            "{{\"at_ms\":{:.3},\"wall_ms\":{:.3},\"cpu_ms\":{},\"phase\":\"{}\",\"kind\":\"{}\"}}",
            s.at_ms, s.wall_ms, s.cpu_ms.map_or("null".into(), |v| format!("{v:.3}")),
            s.phase, s.kind)).collect();
        format!(
            "{{\"redraw_samples\":{},\"redraw_p95_ms\":{:.3},\"wake_lateness_max_ms\":{:.3},\"dispatch_max_ms\":{:.3},\"redraw_max_ms\":{:.3},\"cpu_probe_max_ms\":{:.3},\"slow_spans\":[{}],\"spans_omitted\":{}}}",
            redraw_samples.len(),
            redraw_p95,
            self.wake_max_ms,
            self.dispatch_max_ms,
            self.redraw_max_ms,
            CPU_PROBE_MAX_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            spans.join(","),
            self.omitted
        )
    }
}
pub(super) struct Handler(pub(super) Rc<RefCell<Trace>>);
impl CustomApplicationHandler for Handler {
    fn new_events(
        &mut self,
        _: &winit::event_loop::ActiveEventLoop,
        cause: winit::event::StartCause,
    ) -> EventResult {
        let mut trace = self.0.borrow_mut();
        let requested = match cause {
            winit::event::StartCause::ResumeTimeReached {
                requested_resume, ..
            } => Some(requested_resume),
            winit::event::StartCause::WaitCancelled {
                requested_resume, ..
            } => requested_resume,
            _ => None,
        };
        if let Some(requested) = requested {
            let late = Instant::now()
                .saturating_duration_since(requested)
                .as_secs_f64()
                * 1000.0;
            trace.wake_max_ms = trace.wake_max_ms.max(late);
            let cpu = cpu_ms()
                .zip(trace.idle_cpu)
                .map(|(end, start)| (end - start).max(0.0));
            let phase = trace.phase;
            trace.record("wake-late", phase, late, cpu);
        }
        trace.dispatch = Some(Span::new(trace.phase, "dispatch"));
        EventResult::Propagate
    }
    fn window_event(
        &mut self,
        _: &winit::event_loop::ActiveEventLoop,
        _: winit::window::WindowId,
        _: Option<&winit::window::Window>,
        _: Option<&slint::Window>,
        event: &winit::event::WindowEvent,
    ) -> EventResult {
        let mut trace = self.0.borrow_mut();
        trace.finish_event();
        let kind = if matches!(event, winit::event::WindowEvent::RedrawRequested) {
            "redraw"
        } else {
            "window-event"
        };
        trace.event = Some(Span::new(trace.phase, kind));
        EventResult::Propagate
    }
    fn about_to_wait(&mut self, _: &winit::event_loop::ActiveEventLoop) -> EventResult {
        let mut trace = self.0.borrow_mut();
        trace.finish_event();
        if let Some(span) = trace.dispatch.take() {
            let ms = span.started.elapsed().as_secs_f64() * 1000.0;
            trace.dispatch_max_ms = trace.dispatch_max_ms.max(ms);
            trace.record(
                "dispatch",
                span.phase,
                ms,
                cpu_ms()
                    .zip(span.cpu)
                    .map(|(end, start)| (end - start).max(0.0)),
            );
        }
        trace.idle_cpu = cpu_ms();
        EventResult::Propagate
    }
}
