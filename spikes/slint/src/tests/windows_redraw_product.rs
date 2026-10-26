//! Bounded native driver of the real product/controller. Never compiled into the application.
use super::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, ffi::c_void, path::PathBuf};

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> i32;
    fn IsIconic(hwnd: *mut c_void) -> i32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetPixel(dc: *mut c_void, x: i32, y: i32) -> u32;
}

fn handle(window: &SortSizeWindow) -> *mut c_void {
    let owner = window.window().window_handle();
    let raw = owner.window_handle().unwrap();
    let RawWindowHandle::Win32(raw) = raw.as_raw() else {
        panic!("Win32 required")
    };
    raw.hwnd.get() as *mut c_void
}

fn background_samples(window: &SortSizeWindow) -> Vec<u32> {
    let hwnd = handle(window);
    // SAFETY: the test retains the component/HWND and pairs GetDC with ReleaseDC.
    let dc = unsafe { GetDC(hwnd) };
    assert!(!dc.is_null());
    let size = window.window().size();
    let colors = [2, size.width as i32 - 3]
        .into_iter()
        .flat_map(|x| {
            [
                size.height as i32 / 3,
                size.height as i32 / 2,
                size.height as i32 - 3,
            ]
            .map(|y| unsafe { GetPixel(dc, x, y) })
        })
        .collect();
    unsafe {
        ReleaseDC(hwnd, dc);
    }
    colors
}

fn path_focus(window: &SortSizeWindow, active: bool) {
    use slint::platform::{PointerEventButton, WindowEvent};
    // Input through Slint's public event API into this test-owned component.
    let position = slint::LogicalPosition::new(160.0, 58.0);
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    if !active {
        for event in [
            WindowEvent::KeyPressed {
                text: slint::platform::Key::Tab.into(),
            },
            WindowEvent::KeyReleased {
                text: slint::platform::Key::Tab.into(),
            },
        ] {
            window.window().dispatch_event(event);
        }
    }
}

#[test]
#[ignore = "interactive Windows desktop; set SORTSIZE_REDRAW_SMALL and SORTSIZE_REDRAW_LARGE to synthetic fixtures"]
fn native_product_hundred_cycles_with_real_scans() {
    let small = PathBuf::from(std::env::var_os("SORTSIZE_REDRAW_SMALL").expect("small fixture"));
    let large = PathBuf::from(std::env::var_os("SORTSIZE_REDRAW_LARGE").expect("large fixture"));
    assert!(small.is_dir() && large.is_dir());
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let (second, second_controller) =
        new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let legal = LegalWindow::new().unwrap();
    legal.set_license_text(include_str!("../../../../LICENSE").into());
    controller.borrow_mut().legal = Some(legal.as_weak());
    for component in [&window, &second] {
        component
            .window()
            .set_size(slint::LogicalSize::new(1000.0, 650.0));
    }
    window
        .window()
        .set_position(slint::LogicalPosition::new(20.0, 20.0));
    second
        .window()
        .set_position(slint::LogicalPosition::new(1060.0, 20.0));
    window.show().unwrap();
    second.show().unwrap();
    window.set_scan_path(small.to_str().unwrap().into());
    window.invoke_start_scan();
    let shared = controller.clone();
    let weak = window.as_weak();
    let completed = Rc::new(Cell::new(0));
    let done = completed.clone();
    let timer = Timer::default();
    let mut cycle = 0;
    let mut phase = 0;
    let mut reference = Vec::new();
    let mut initial_paint_retries = 0;
    let mut size = slint::PhysicalSize::default();
    let deadline = Instant::now() + Duration::from_secs(300);
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        assert!(
            Instant::now() < deadline,
            "native campaign timeout: cycle {cycle}, phase {phase}"
        );
        let window = weak.upgrade().unwrap();
        shared.borrow_mut().poll(&window);
        match phase {
            0 => {
                if window.get_busy() {
                    return;
                }
                if cycle == 0 {
                    let samples = background_samples(&window);
                    // GetDC can still expose the desktop before the first Winit
                    // paint, especially when a one-file scan finishes at once.
                    // main.slint uses opaque #edf2f7 (COLORREF 0x00f7f2ed).
                    // Establish a painted reference before testing restoration;
                    // the existing campaign deadline also bounds this wait.
                    if samples.iter().any(|&color| color != 0x00f7f2ed) {
                        initial_paint_retries += 1;
                        return;
                    }
                    eprintln!("initial_native_background_ready retries={initial_paint_retries}");
                    reference = samples;
                    size = window.window().size();
                }
                if cycle == 100 {
                    window.hide().unwrap();
                    slint::quit_event_loop().unwrap();
                    return;
                }
                if (60..80).contains(&cycle) {
                    window.set_scan_path(large.to_str().unwrap().into());
                    window.invoke_start_scan();
                    assert!(window.get_busy(), "scan did not start");
                } else if cycle == 80 {
                    window.set_scan_path(small.to_str().unwrap().into());
                    window.invoke_start_scan();
                    phase = 5;
                    return;
                }
                phase = 1;
            }
            1 => {
                path_focus(&window, !(30..60).contains(&cycle));
                if (60..80).contains(&cycle) {
                    assert!(window.get_busy(), "scan finished before minimize");
                }
                window.window().set_minimized(true);
                phase = 2;
            }
            2 => {
                assert_ne!(unsafe { IsIconic(handle(&window)) }, 0);
                window.window().request_redraw();
                phase = 3;
            }
            3 => {
                window.window().set_minimized(false);
                phase = 4;
            }
            4 => {
                assert_eq!(unsafe { IsIconic(handle(&window)) }, 0);
                assert_eq!(window.window().size(), size);
                assert_eq!(
                    background_samples(&window),
                    reference,
                    "native surface lost, cycle {cycle}"
                );
                eprintln!(
                    "product cycle={} scenario={} busy_after_restore={} native_background=PASS",
                    cycle + 1,
                    if cycle < 30 {
                        "path-active"
                    } else if cycle < 60 {
                        "path-inactive"
                    } else if cycle < 80 {
                        "scan-active-at-minimize"
                    } else {
                        "scan-complete"
                    },
                    window.get_busy()
                );
                if window.get_busy() {
                    window.invoke_cancel_scan();
                }
                cycle += 1;
                done.set(cycle);
                phase = 0;
            }
            5 => {
                if !window.get_busy() {
                    phase = 1;
                }
            }
            _ => unreachable!(),
        }
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
    second.hide().unwrap();
    legal.hide().unwrap();
    drop(second_controller);
    assert_eq!(completed.get(), 100);
}
