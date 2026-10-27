//! Bounded native driver of the real product/controller. Never compiled into the application.
use super::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, ffi::c_void, path::PathBuf};

#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct BitmapInfo {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    colors_used: u32,
    colors_important: u32,
    colors: [u32; 3],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> i32;
    fn IsIconic(hwnd: *mut c_void) -> i32;
    fn ClientToScreen(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn GetForegroundWindow() -> *mut c_void;
    fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetPixel(dc: *mut c_void, x: i32, y: i32) -> u32;
    fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
    fn CreateDIBSection(
        dc: *mut c_void,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    fn SelectObject(dc: *mut c_void, object: *mut c_void) -> *mut c_void;
    fn BitBlt(
        dst: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        src: *mut c_void,
        src_x: i32,
        src_y: i32,
        operation: u32,
    ) -> i32;
    fn DeleteObject(object: *mut c_void) -> i32;
    fn DeleteDC(dc: *mut c_void) -> i32;
    fn GdiFlush() -> i32;
}

// One screen readback per footer. Thousands of GetPixel(screen) calls each
// synchronize with the display driver and can stall an interactive test.
fn desktop_footer(window: &SortSizeWindow, top: u32) -> Vec<u32> {
    let hwnd = handle(window);
    let size = window.window().size();
    let height = size.height - top;
    let mut origin = Point::default();
    assert_eq!(
        unsafe { GetForegroundWindow() },
        hwnd,
        "foreground precondition lost"
    );
    assert_ne!(unsafe { ClientToScreen(hwnd, &mut origin) }, 0);
    let info = BitmapInfo {
        size: 40,
        width: size.width as i32,
        height: -(height as i32),
        planes: 1,
        bit_count: 32,
        compression: 0,
        size_image: 0,
        x_ppm: 0,
        y_ppm: 0,
        colors_used: 0,
        colors_important: 0,
        colors: [0; 3],
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: all GDI resources are test-owned and released below. The top-down
    // 32-bit DIB has width*height pixels; copy them before deleting the bitmap.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        assert!(!screen.is_null());
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        if memory.is_null() || bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !memory.is_null() {
                DeleteDC(memory);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
            panic!("screen readback allocation failed");
        }
        let old = SelectObject(memory, bitmap);
        let copied = BitBlt(
            memory,
            0,
            0,
            size.width as i32,
            height as i32,
            screen,
            origin.x,
            origin.y + top as i32,
            0x40cc_0020,
        );
        let flushed = GdiFlush();
        let result = if copied != 0 && flushed != 0 {
            std::slice::from_raw_parts(bits.cast::<u32>(), (size.width * height) as usize).to_vec()
        } else {
            Vec::new()
        };
        SelectObject(memory, old);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
        assert_ne!(copied, 0, "screen readback failed");
        assert_ne!(flushed, 0, "screen readback synchronization failed");
        assert_eq!(
            GetForegroundWindow(),
            hwnd,
            "foreground changed during readback"
        );
        result
    }
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

// D1 oracle: read the native HWND BEFORE asking Slint for a full snapshot.
// A correct getter or an offscreen snapshot alone cannot pass this test.
#[test]
#[ignore = "requires an unlocked, unobscured Windows desktop; opens a maximized product window"]
fn native_footer_pixels_match_semantic_state_without_resize() {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("oracle.txt"), [1; 1024]).unwrap();
    let source = std::env::var_os("SORTSIZE_D1_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| fixture.path().to_owned());
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window.set_collect_allocation(true);
    window.invoke_set_column(3, false);
    window.set_scan_path(source.to_str().unwrap().into());
    window.invoke_start_scan();
    window.show().unwrap();
    let weak = window.as_weak();
    let done = Rc::new(Cell::new(0));
    let completed = done.clone();
    let timer = Timer::default();
    let mut phase = 0;
    let mut prepared = false;
    let mut index = 0;
    let mut size = slint::PhysicalSize::default();
    let choices = [1, 3, 4, 5, 2, 1, 2];
    let mut last_native = Vec::new();
    let mut discriminating = 0;
    let failure = Rc::new(RefCell::new(None));
    let failed = failure.clone();
    let deadline = Instant::now() + Duration::from_secs(180);
    timer.start(TimerMode::Repeated, Duration::from_millis(400), move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(Instant::now() < deadline, "D1 native test timeout");
            let window = weak.upgrade().unwrap();
            controller.borrow_mut().poll(&window);
            if window.get_busy() {
                return;
            }
            if phase == 0 {
                // Windows may refuse foreground activation to a process started by
                // Cargo. Allow the operator to focus it before establishing size.
                if unsafe { GetForegroundWindow() } != handle(&window) {
                    return;
                }
                if !prepared {
                    prepared = true;
                    let branch = window
                        .get_rows()
                        .iter()
                        .find(|row| row.name == "branch-000");
                    if let Some(branch) = branch {
                        window.invoke_select_node(branch.node_id);
                    }
                    // Wait for initial placement before maximizing; startup sizing
                    // can otherwise overwrite the requested maximized client size.
                    window.window().set_maximized(true);
                    return;
                }
                assert!(background_samples(&window).iter().all(|&p| p == 0x00f7f2ed));
                size = window.window().size();
                assert!(window.window().is_maximized());
                eprintln!(
                    "D1 native geometry={}x{} scale={}",
                    size.width,
                    size.height,
                    window.window().scale_factor()
                );
                phase = 1;
                return;
            }
            assert_eq!(
                size,
                window.window().size(),
                "no resize permitted during D1 test"
            );
            if phase == 1 {
                if index == 28 {
                    assert!(
                        discriminating >= 20,
                        "oracle must reject stale previous-language pixels"
                    );
                    window.hide().unwrap();
                    slint::quit_event_loop().unwrap();
                    return;
                }
                window.set_preferences_open((index / 7) % 2 == 0);
                // Coalesced transitions exercise rapid state changes before a frame.
                window.invoke_set_language(choices[(index + 1) % choices.len()]);
                window.invoke_set_language(choices[index % choices.len()]);
                phase = 2;
                return;
            }
            let semantic = window.get_summary();
            assert_eq!(window.get_language_choice(), choices[index % choices.len()]);
            let noun = ["", "file", "fichier", "Datei", "archivo", "ficheiro"]
                [choices[index % choices.len()] as usize];
            assert!(
                semantic.contains(noun),
                "wrong semantic language: {semantic}"
            );
            let top = size
                .height
                .saturating_sub((72.0 * window.window().scale_factor()).ceil() as u32);
            let positions: Vec<_> = (top..size.height.saturating_sub(4))
                .step_by(2)
                .flat_map(|y| {
                    (4..size.width.saturating_sub(4))
                        .step_by(3)
                        .map(move |x| (x, y))
                })
                .collect();
            let desktop = desktop_footer(&window, top);
            let actual: Vec<_> = positions
                .iter()
                .map(|&(x, y)| desktop[((y - top) * size.width + x) as usize] & 0x00ff_ffff)
                .collect();
            // This diagnostic resets the software-renderer cache AFTER observation.
            // It never resizes or presents the reference to the native window.
            let snapshot = window.window().take_snapshot().unwrap();
            assert_eq!(
                (snapshot.width(), snapshot.height()),
                (size.width, size.height)
            );
            let expected: Vec<_> = positions
                .iter()
                .map(|&(x, y)| {
                    let p = snapshot.as_slice()[(y * size.width + x) as usize];
                    (u32::from(p.r) << 16) | (u32::from(p.g) << 8) | u32::from(p.b)
                })
                .collect();
            let differences = actual.iter().zip(&expected).filter(|(a, b)| a != b).count();
            assert_eq!(
                differences,
                0,
                "D1 native stale footer: choice={}, semantic={semantic}",
                choices[index % choices.len()]
            );
            if !last_native.is_empty() && last_native != expected {
                discriminating += 1;
            }
            last_native = actual;
            eprintln!(
                "D1 native transition={} choice={} samples={} different={} semantic={semantic}",
                index + 1,
                choices[index % choices.len()],
                positions.len(),
                differences
            );
            index += 1;
            completed.set(index);
            phase = 1;
        }));
        if let Err(panic) = outcome {
            *failed.borrow_mut() = Some(panic);
            slint::quit_event_loop().unwrap();
        }
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
    window.hide().unwrap();
    if let Some(panic) = failure.borrow_mut().take() {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(done.get(), 28);
}

// Record ALL desktop observations before take_snapshot can reset the renderer.
// This complements the per-frame oracle above, whose reference renders can
// prevent cache faults from accumulating. No production redraw is added.
#[test]
#[ignore = "unlocked unobscured Windows desktop; requires SORTSIZE_D1_OUTPUT; maximizes a test window"]
fn native_footer_sequence_records_before_any_reference_render() {
    use std::io::Write;
    let output = PathBuf::from(std::env::var_os("SORTSIZE_D1_OUTPUT").expect("evidence directory"));
    fs::create_dir_all(&output).unwrap();
    let mut journal = fs::File::create(output.join("observations.jsonl")).unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("oracle.txt"), [1; 1024]).unwrap();
    let source = std::env::var_os("SORTSIZE_D1_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| fixture.path().to_owned());
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window.set_collect_allocation(true);
    window.invoke_set_column(3, false);
    window.set_scan_path(source.to_str().unwrap().into());
    window.invoke_start_scan();
    window.show().unwrap();
    let weak = window.as_weak();
    let timer = Timer::default();
    let failure = Rc::new(RefCell::new(None));
    let failed = failure.clone();
    let finished = Rc::new(Cell::new(false));
    let done = finished.clone();
    let mut phase = 0;
    let mut prepared = false;
    let mut index = 0;
    let mut size = slint::PhysicalSize::default();
    let mut scale = 0.0;
    let choices = [1, 6, 7, 3, 4, 5, 2];
    let nouns = [
        "", "file", "fichier", "Datei", "archivo", "ficheiro", "file", "bestand",
    ];
    let mut records = Vec::new();
    let mut discriminating = 0;
    let deadline = Instant::now() + Duration::from_secs(180);
    timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(Instant::now() < deadline, "D1 sequence timeout at phase {phase}, index {index}");
            let window = weak.upgrade().unwrap();
            controller.borrow_mut().poll(&window);
            if window.get_busy() { return; }
            if phase == 0 {
                if unsafe { GetForegroundWindow() } != handle(&window) { return; }
                if !prepared { prepared = true; window.window().set_maximized(true); return; }
                assert!(window.window().is_maximized());
                size = window.window().size();
                scale = window.window().scale_factor();
                phase = 1;
                return;
            }
            assert_eq!(window.window().size(), size, "geometry changed");
            assert_eq!(window.window().scale_factor(), scale, "DPI changed");
            let top = size.height.saturating_sub((85.0 * scale).ceil() as u32);
            if phase == 1 || phase == 3 {
                if index == 28 {
                    if phase == 1 { phase = 3; index = 0; return; }
                    assert!(discriminating >= 20, "oracle must reject old-language pixels");
                    writeln!(journal, "{}", serde_json::json!({"event":"complete", "observations":records.len(), "discriminating":discriminating})).unwrap();
                    journal.flush().unwrap();
                    done.set(true);
                    window.hide().unwrap();
                    slint::quit_event_loop().unwrap();
                    return;
                }
                window.set_preferences_open((index / 7) % 2 == 0);
                window.invoke_set_language(choices[(index + 1) % choices.len()]);
                window.invoke_set_language(choices[index % choices.len()]);
                phase += 1;
                return;
            }
            let semantic = (window.get_summary(), window.get_status(), window.get_inventory_warning());
            let choice = choices[index % choices.len()];
            assert_eq!(window.get_language_choice(), choice);
            assert!(semantic.0.contains(nouns[choice as usize]), "wrong language: {}", semantic.0);
            if phase == 2 {
                let desktop = desktop_footer(&window, top);
                // Raw complete footer, captured before ANY full reference render.
                write_footer_ppm(&output.join(format!("observed-{index:02}.ppm")), size.width, &desktop);
                let mut origin = Point::default();
                assert_ne!(unsafe { ClientToScreen(handle(&window), &mut origin) }, 0);
                let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
                writeln!(journal, "{}", serde_json::json!({
                    "event":"observation", "index":index, "unix_ms":time,
                    "revision":std::env::var("SORTSIZE_TEST_REVISION").ok(),
                    "hwnd":handle(&window) as usize, "client":[size.width,size.height],
                    "origin":[origin.x,origin.y], "footer_top":top, "scale":scale,
                    "dpi":unsafe { GetDpiForWindow(handle(&window)) },
                    "remote_session":unsafe { GetSystemMetrics(0x1000) } != 0,
                    "language_choice":choice, "preferences_open":window.get_preferences_open(),
                    "summary":semantic.0.as_str(), "status":semantic.1.as_str(), "warning":semantic.2.as_str(),
                    "scan_busy":window.get_busy(), "input":"coalesced controller callbacks", "reference_rendered":false
                })).unwrap();
                journal.flush().unwrap();
                records.push((semantic, desktop));
                index += 1;
                phase = 1;
                return;
            }
            assert_eq!(phase, 4);
            assert_eq!(semantic, records[index].0, "reference semantic state differs");
            let snapshot = window.window().take_snapshot().unwrap();
            assert_eq!((snapshot.width(), snapshot.height()), (size.width, size.height));
            let expected: Vec<_> = snapshot.as_slice()[(top * size.width) as usize..].iter().map(|p| (u32::from(p.r) << 16) | (u32::from(p.g) << 8) | u32::from(p.b)).collect();
            write_footer_ppm(&output.join(format!("expected-{index:02}.ppm")), size.width, &expected);
            let actual = &records[index].1;
            // Exclude only the outer four pixels; compare every interior pixel.
            let positions: Vec<_> = (0..(size.height-top).saturating_sub(4)).flat_map(|y| (4..size.width.saturating_sub(4)).map(move |x| (y * size.width+x) as usize)).collect();
            let differences = positions.iter().filter(|&&p| actual[p] & 0x00ff_ffff != expected[p]).count();
            if index > 0 && positions.iter().any(|&p| records[index-1].1[p] & 0x00ff_ffff != expected[p]) { discriminating += 1; }
            writeln!(journal, "{}", serde_json::json!({"event":"comparison", "index":index, "pixels":positions.len(), "different":differences})).unwrap();
            journal.flush().unwrap();
            assert_eq!(differences, 0, "D1 stale footer at transition {index}, language {choice}");
            index += 1;
            phase = 3;
        }));
        if let Err(panic) = outcome { *failed.borrow_mut() = Some(panic); slint::quit_event_loop().unwrap(); }
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
    window.hide().unwrap();
    if let Some(panic) = failure.borrow_mut().take() {
        std::panic::resume_unwind(panic);
    }
    assert!(finished.get());
}

fn write_footer_ppm(path: &std::path::Path, width: u32, pixels: &[u32]) {
    let mut bytes = format!("P6\n{} {}\n255\n", width, pixels.len() / width as usize).into_bytes();
    for &pixel in pixels {
        bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
    }
    fs::write(path, bytes).unwrap();
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
