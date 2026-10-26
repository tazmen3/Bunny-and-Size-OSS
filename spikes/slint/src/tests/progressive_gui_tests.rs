use super::*;
use sortsize_core::{CancellationToken, ProgressiveInventoryOptions, ScanOptions};
use std::sync::{Arc, Mutex};

#[test]
fn progressive_navigation_finalization_and_obsolete_mailbox_are_coherent() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("child")).unwrap();
    fs::write(fixture.path().join("child/file"), [0; 41]).unwrap();
    let mailbox = Arc::new(Mutex::new(worker::Mailbox::default()));
    controller.borrow_mut().task = Some(Task {
        cancellation: Default::default(),
        mailbox: mailbox.clone(),
    });
    window.set_busy(true);
    let mut projector = tree::Projector::default();
    let result = sortsize_core::scan_inventory_progressive(
        fixture.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        ProgressiveInventoryOptions::default(),
        |_, preview| {
            mailbox.lock().unwrap().patch = Some(projector.patch(preview));
            controller.borrow_mut().poll(&window);
            assert!(window.get_busy());
            assert!(window.get_rows().row_count() > 0);
            assert!(
                window
                    .get_rows()
                    .row_data(0)
                    .unwrap()
                    .size
                    .starts_with("≥ ")
            );
            if preview.entries.len() > 1 {
                window.invoke_select_node(1);
                window.set_detail_scroll_y(-34.0);
                // Reapplying numeric changes preserves navigation and scroll.
                mailbox.lock().unwrap().patch = Some(projector.patch(preview));
                controller.borrow_mut().poll(&window);
                assert!(window.get_current_path().ends_with("child"));
                assert_eq!(window.get_detail_scroll_y(), -34.0);
            }
        },
    )
    .unwrap();
    assert_eq!(result.collected.metadata.inventory.report.logical_bytes, 41);
    window.invoke_cancel_scan();
    assert!(window.get_cancelling());
    // Replacing the Task drops its token; late responses can only reach the old mailbox.
    controller.borrow_mut().task = None;
    window.set_busy(false);
    let generation = controller.borrow().generation;
    let next = TempDir::new().unwrap();
    fs::write(next.path().join("fresh"), [0; 7]).unwrap();
    window.set_scan_path(next.path().to_str().unwrap().into());
    window.invoke_start_scan();
    mailbox.lock().unwrap().completion = Some(Err(worker::Failure::Panicked));
    wait(&window, &controller);
    assert_eq!(controller.borrow().generation, generation + 1);
    assert!(
        window
            .get_current_path()
            .starts_with(next.path().to_str().unwrap())
    );
    assert_eq!(window.get_details().row_count(), 1);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "fresh");
    assert_eq!(window.get_status(), "Analyse terminée");
}

pub(super) fn snapshot(window: &SortSizeWindow, directory: &std::path::Path, name: &str) {
    use std::io::Write;
    let pixels = window.window().take_snapshot().unwrap();
    let mut file =
        std::io::BufWriter::new(fs::File::create(directory.join(format!("{name}.ppm"))).unwrap());
    write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
    for pixel in pixels.as_slice() {
        file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
    }
    file.flush().unwrap();
}

#[test]
#[ignore = "native Winit desktop, explicit SORTSIZE_PROGRESSIVE_FIXTURE and SORTSIZE_PROGRESSIVE_OUTPUT required"]
fn native_progressive_scan_cancel_source_error_and_restore() {
    let fixture =
        std::path::PathBuf::from(std::env::var_os("SORTSIZE_PROGRESSIVE_FIXTURE").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("SORTSIZE_PROGRESSIVE_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window
        .window()
        .set_size(slint::LogicalSize::new(1200., 760.));
    window.set_worker_choice(
        if std::env::var("SORTSIZE_PROGRESSIVE_WORKERS").as_deref() == Ok("8") {
            3
        } else {
            0
        },
    );
    window.set_scan_path(fixture.to_str().unwrap().into());
    window.show().unwrap();
    window.invoke_start_scan();
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    let started = Instant::now();
    let mut last = Instant::now();
    let mut heartbeat_max = 0f64;
    let mut poll_max = 0f64;
    let mut intermediate = false;
    let mut updates = 0;
    let mut previous_summary = slint::SharedString::default();
    let mut phase = 0;
    let mut phase_started = Instant::now();
    let small = TempDir::new().unwrap();
    fs::write(small.path().join("new-source"), [0; 17]).unwrap();
    timer.start(TimerMode::Repeated, Duration::from_millis(20), move || {
        assert!(started.elapsed() < Duration::from_secs(240), "native test timeout phase {phase}");
        let window = weak.upgrade().unwrap();
        heartbeat_max = heartbeat_max.max(last.elapsed().as_secs_f64() * 1000.);
        let tick = Instant::now();
        state.borrow_mut().poll(&window);
        poll_max = poll_max.max(tick.elapsed().as_secs_f64() * 1000.);
        if window.get_summary() != previous_summary {
            updates += 1;
            previous_summary = window.get_summary();
        }
        match phase {
            0 => {
                if window.get_busy() && window.get_details().row_count() > 0 && !intermediate {
                    assert!(window.get_selection_summary().contains("≥"));
                    assert!(window.get_tiles().row_count() > 0);
                    snapshot(&window, &output, "intermediate");
                    if let Some(row) = window.get_details().iter().find(|row| row.directory) {
                        window.invoke_select_node(row.node_id);
                    }
                    assert!(window.get_busy());
                    intermediate = true;
                    window.window().set_minimized(true);
                    phase_started = Instant::now();
                    phase = 1;
                } else if !window.get_busy() { panic!("fixture finished before an intermediate result was observed"); }
            }
            1 => {
                if phase_started.elapsed() >= Duration::from_millis(100) {
                    window.window().set_minimized(false);
                    window.window().request_redraw();
                    phase = 2;
                }
            }
            2 if !window.get_busy() => {
                snapshot(&window, &output, "final");
                assert!(!window.get_selection_summary().contains("≥"));
                assert_eq!(window.get_status(), "Analyse terminée");
                window.set_scan_path(fixture.to_str().unwrap().into());
                window.invoke_start_scan();
                phase_started = Instant::now();
                phase = 3;
            }
            3 if window.get_busy() && phase_started.elapsed() >= Duration::from_millis(750) => {
                window.invoke_cancel_scan();
                phase_started = Instant::now();
                phase = 4;
            }
            4 if !window.get_busy() => {
                let cancel_latency_ms = phase_started.elapsed().as_secs_f64()*1000.;
                assert!(window.get_status().contains("annul"));
                assert!(window.get_selection_summary().contains("≥"));
                snapshot(&window, &output, "cancelled");
                eprintln!("cancel_latency_ms={cancel_latency_ms:.3}");
                window.set_scan_path(small.path().to_str().unwrap().into());
                window.invoke_start_scan();
                phase = 5;
            }
            5 if !window.get_busy() => {
                assert_eq!(window.get_details().row_count(), 1);
                assert_eq!(window.get_details().row_data(0).unwrap().name, "new-source");
                window.set_scan_path(small.path().join("missing").to_str().unwrap().into());
                window.invoke_start_scan();
                phase = 6;
            }
            6 if !window.get_busy() => {
                assert_eq!(window.get_status(), "Analyse impossible.");
                assert_eq!(window.get_rows().row_count(), 0);
                snapshot(&window, &output, "error");
                fs::write(output.join("metrics.json"), format!("{{\"intermediate\":{intermediate},\"summary_updates\":{updates},\"poll_max_ms\":{poll_max:.3},\"heartbeat_max_ms\":{heartbeat_max:.3},\"duration_ms\":{}}}", started.elapsed().as_millis())).unwrap();
                window.hide().unwrap();
                slint::quit_event_loop().unwrap();
            }
            _ => {}
        }
        // Snapshot rendering is measured separately from timer responsiveness.
        last = Instant::now();
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
}
