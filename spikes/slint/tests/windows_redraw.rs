//! Opt-in native regression: inspect actual HWND pixels, not a Slint offscreen snapshot.
//! Run on an unlocked Windows desktop with `--test windows_redraw -- --ignored`.
#![cfg(windows)]

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::{cell::Cell, ffi::c_void, rc::Rc, time::Duration};

slint::slint! {
    export component RedrawProbe inherits Window {
        width: 600px;
        height: 400px;
        background: #123456;
        in property <bool> pulse;
        public function focus-path() { path.focus(); }
        public function leave-path() { other.focus(); }
        path := TextInput { x: 10px; y: 10px; width: 300px; height: 30px; text: "synthetic-path"; }
        other := FocusScope { }
        Rectangle { x: 350px; y: 10px; width: 20px; height: 20px; background: pulse ? red : green; }
    }
}

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> i32;
    fn GetClientRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    fn InvalidateRect(hwnd: *mut c_void, rect: *const Rect, erase: i32) -> i32;
    fn IsIconic(hwnd: *mut c_void) -> i32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetPixel(dc: *mut c_void, x: i32, y: i32) -> u32;
    fn SetPixel(dc: *mut c_void, x: i32, y: i32, color: u32) -> u32;
}

fn hwnd(probe: &RedrawProbe) -> *mut c_void {
    let owner = probe.window().window_handle();
    let handle = owner.window_handle().unwrap();
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        panic!("Win32 required")
    };
    handle.hwnd.get() as *mut c_void
}

fn client_size(probe: &RedrawProbe) -> (i32, i32) {
    let mut rect = Rect::default();
    // SAFETY: the component owns a live HWND, and rect is writable.
    assert_ne!(unsafe { GetClientRect(hwnd(probe), &mut rect) }, 0);
    (rect.right, rect.bottom)
}

fn check_pixels(probe: &RedrawProbe, cycle: usize) {
    let handle = hwnd(probe);
    // SAFETY: GetDC/ReleaseDC are paired for this live, test-owned HWND.
    let dc = unsafe { GetDC(handle) };
    assert!(!dc.is_null());
    let size = client_size(probe);
    let mut colors = Vec::new();
    for x in [20, size.0 / 2, size.0 - 20] {
        for y in [size.1 / 3, size.1 / 2, size.1 - 20] {
            colors.push(unsafe { GetPixel(dc, x, y) });
        }
    }
    unsafe { ReleaseDC(handle, dc) };
    assert_eq!(colors, vec![0x0056_3412; 9], "native pixels, cycle {cycle}");
}

#[test]
#[ignore = "requires an unlocked interactive Windows desktop; opens two native windows"]
fn native_same_size_restore_and_system_invalidation_two_windows() {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let probes = [RedrawProbe::new().unwrap(), RedrawProbe::new().unwrap()];
    for (index, probe) in probes.iter().enumerate() {
        probe.window().set_position(slint::LogicalPosition::new(
            30.0 + index as f32 * 650.0,
            30.0,
        ));
        probe.show().unwrap();
    }
    let probes = Rc::new(probes);
    let completed = Rc::new(Cell::new(0));
    let done = completed.clone();
    let shared = probes.clone();
    let timer = slint::Timer::default();
    let mut phase = 0;
    let mut initial_sizes = [(0, 0); 2];
    // This timer is a bounded test driver; it is absent from the product binary.
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            if phase == 0 {
                for (i, probe) in shared.iter().enumerate() {
                    initial_sizes[i] = client_size(probe);
                    check_pixels(probe, 0);
                }
            } else if phase <= 480 {
                let cycle = (phase - 1) / 4;
                let index = cycle % 2;
                let probe = &shared[index];
                match (phase - 1) % 4 {
                    0 => {
                        if cycle < 60 {
                            probe.invoke_focus_path();
                        } else {
                            probe.invoke_leave_path();
                        }
                        probe.window().set_minimized(true);
                    }
                    1 => {
                        assert_ne!(unsafe { IsIconic(hwnd(probe)) }, 0);
                        // Force a pending small invalidation while minimized.
                        probe.set_pulse(!probe.get_pulse());
                    }
                    2 => probe.window().set_minimized(false),
                    _ => {
                        assert_eq!(unsafe { IsIconic(hwnd(probe)) }, 0);
                        assert_eq!(client_size(probe), initial_sizes[index]);
                        check_pixels(probe, cycle + 1);
                        done.set(cycle + 1);
                    }
                }
            } else if phase == 481 {
                // Damage a native pixel without changing the scene, then ask Windows to repaint it.
                // An internal redraw merged with this invalidation must not lose the update region.
                for probe in shared.iter() {
                    let handle = hwnd(probe);
                    let dc = unsafe { GetDC(handle) };
                    assert!(!dc.is_null());
                    unsafe {
                        SetPixel(dc, 20, 200, 0x00ff_00ff);
                        ReleaseDC(handle, dc);
                    }
                    let rect = Rect {
                        left: 19,
                        top: 199,
                        right: 22,
                        bottom: 202,
                    };
                    assert_ne!(unsafe { InvalidateRect(handle, &rect, 0) }, 0);
                    probe.window().request_redraw();
                }
            } else {
                for probe in shared.iter() {
                    let handle = hwnd(probe);
                    let dc = unsafe { GetDC(handle) };
                    assert!(!dc.is_null());
                    let color = unsafe { GetPixel(dc, 20, 200) };
                    unsafe {
                        ReleaseDC(handle, dc);
                    }
                    assert_eq!(color, 0x0056_3412, "system invalidation was lost");
                    probe.hide().unwrap();
                }
                slint::quit_event_loop().unwrap();
            }
            phase += 1;
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
    assert_eq!(completed.get(), 120);
    eprintln!(
        "120 same-size restores, two HWNDs, 9 native pixels per restore, merged system/internal invalidation: PASS"
    );
}
