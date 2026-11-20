use super::*;
use std::{
    cell::Cell,
    sync::Condvar,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
enum Intervention {
    Cancel,
    EditScope,
}

#[derive(Debug)]
enum Step {
    FindFolder,
    BlockWriter,
    SelectMenuCommand,
    Intervene,
    Drain,
    ObserveStopped(Instant),
}

#[derive(Default)]
struct WriterGate(Arc<(Mutex<bool>, Condvar)>);

impl Drop for WriterGate {
    fn drop(&mut self) {
        let (released, ready) = &*self.0;
        *released.lock().unwrap() = true;
        ready.notify_all();
    }
}

#[test]
#[ignore = "interactive Winit window on a disposable SUBST drive; see the Windows validation"]
fn native_folder_duplicates_intervention_while_global_scan_drains() {
    let source = PathBuf::from(std::env::var_os("BUNNY_PRIORITY_FIXTURE_DRIVE").unwrap());
    assert!(source.join("tmp/1/enfant-a/copie-a.bin").is_file());
    assert!(source.join("hors/copie-exterieure.bin").is_file());
    let output = PathBuf::from(std::env::var_os("BUNNY_PRIORITY_EVIDENCE").unwrap());
    let intervention = match std::env::var("BUNNY_PRIORITY_INTERVENTION").as_deref() {
        Ok("cancel") => Intervention::Cancel,
        Ok("edit") => Intervention::EditScope,
        _ => panic!("BUNNY_PRIORITY_INTERVENTION must be cancel or edit"),
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let preferences = crate::display_preferences::Preferences {
        memory: MemoryMode::Entries50k,
        ..Default::default()
    };
    let (window, controller) = crate::new_session(preferences, Some("fr_FR".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    let _visibility = TestWindowVisibility::new(&window);
    window.show().unwrap();
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(source.clone())
            .into(),
    );
    window.invoke_start_scan();

    let lane = host.borrow().service.lane.clone();
    let initial_epoch = lane.lock().unwrap().source_epoch;
    let started = Instant::now();
    let completed = Rc::new(Cell::new(false));
    let complete = completed.clone();
    let timer = slint::Timer::default();
    let mut gate = Some(WriterGate::default());
    let mut step = Step::FindFolder;
    let clicked = label(&source.join("tmp"));
    let changed = label(&source.join("hors"));
    let weak = window.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(40),
        move || {
            let window = weak.upgrade().unwrap();
            controller.borrow_mut().poll(&window);
            window.invoke_workshop_ready();
            assert!(started.elapsed() < Duration::from_secs(120), "{step:?}");
            assert!(window.get_errors().is_empty(), "{}", window.get_errors());
            match step {
                Step::FindFolder => {
                    assert!(
                        window.get_busy(),
                        "the global inventory completed before the folder appeared"
                    );
                    if window.get_rows().iter().any(|row| row.path == clicked) {
                        let mut lane = lane.lock().unwrap();
                        assert!(lane.acquisition_in_progress());
                        lane.tick_gate = Some(gate.as_ref().unwrap().0.clone());
                        step = Step::BlockWriter;
                    }
                }
                Step::BlockWriter if lane.lock().unwrap().tick_blocked => {
                    assert!(window.get_busy());
                    assert!(!window.get_search_visible());
                    std::fs::write(
                        output.join("01-global-actif.txt"),
                        window.get_status().as_str(),
                    )
                    .unwrap();
                    step = Step::SelectMenuCommand;
                }
                Step::SelectMenuCommand if window.get_duplicate_waiting_for_scan() => {
                    assert_eq!(window.get_duplicate_single_root(), clicked);
                    assert!(window.get_search_busy());
                    assert!(!window.get_search_complete());
                    assert_eq!(window.get_duplicate_rows().row_count(), 0);
                    let lane = lane.lock().unwrap();
                    assert_eq!(lane.source_epoch, initial_epoch);
                    assert!(lane.cancellation_pending());
                    std::fs::write(
                        output.join("02-transition.txt"),
                        format!(
                            "epoch={initial_epoch}\n{}\n{}",
                            window.get_duplicate_single_root(),
                            window.get_duplicate_summary()
                        ),
                    )
                    .unwrap();
                    step = Step::Intervene;
                }
                Step::Intervene if !window.get_duplicate_waiting_for_scan() => {
                    assert!(!window.get_search_busy());
                    assert!(!window.get_search_complete());
                    assert_eq!(window.get_duplicate_rows().row_count(), 0);
                    match intervention {
                        Intervention::Cancel => {
                            assert_eq!(window.get_duplicate_single_root(), clicked);
                            assert_eq!(window.get_duplicate_warning(), tr!("Cancelled"));
                        }
                        Intervention::EditScope => {
                            // Text input may generate more than one edited event.
                            if window.get_duplicate_single_root() != changed {
                                return;
                            }
                        }
                    }
                    {
                        let mut lane = lane.lock().unwrap();
                        assert_eq!(lane.source_epoch, initial_epoch);
                        assert!(lane.cancellation_pending());
                        lane.tick_gate = None;
                    }
                    drop(gate.take());
                    step = Step::Drain;
                }
                Step::Drain if !window.get_busy() && !window.get_cancelling() => {
                    let lane = lane.lock().unwrap();
                    assert_eq!(lane.cancelled_unpublished, Some(initial_epoch));
                    assert!(!lane.acquisition_in_progress());
                    step = Step::ObserveStopped(Instant::now());
                }
                Step::ObserveStopped(since) => {
                    let lane = lane.lock().unwrap();
                    assert_eq!(lane.source_epoch, initial_epoch);
                    assert_eq!(lane.cancelled_unpublished, Some(initial_epoch));
                    assert!(!lane.acquisition_in_progress());
                    assert!(!window.get_search_busy());
                    assert!(!window.get_search_complete());
                    assert_eq!(window.get_duplicate_rows().row_count(), 0);
                    if since.elapsed() >= Duration::from_secs(2) {
                        std::fs::write(
                            output.join("03-arret-confirme.txt"),
                            format!(
                                "{intervention:?}\nepoch={initial_epoch}\n{}\n{}\n{}",
                                window.get_duplicate_single_root(),
                                window.get_duplicate_warning(),
                                window.get_status()
                            ),
                        )
                        .unwrap();
                        complete.set(true);
                        slint::quit_event_loop().unwrap();
                    }
                }
                _ => {}
            }
        },
    );
    slint::run_event_loop().unwrap();
    timer.stop();
    assert!(
        completed.get(),
        "native window closed before the recipe completed"
    );
}

#[cfg(feature = "benchmarks")]
#[test]
#[ignore = "native Winit timing on the supplied duplicate corpus"]
fn native_duplicate_profile_first_frame() {
    use i_slint_backend_winit::presentation_probe as paint;
    use serde_json::json;
    use slint::platform::{PointerEventButton, WindowEvent};

    let source = PathBuf::from(std::env::var_os("BUNNY_DUPLICATE_PROFILE_FIXTURE").unwrap());
    let evidence = PathBuf::from(std::env::var_os("BUNNY_DUPLICATE_PROFILE_EVIDENCE").unwrap());
    let profile = std::env::var("BUNNY_DUPLICATE_PROFILE").unwrap_or_else(|_| "large".into());
    let appearance = std::env::var("BUNNY_DUPLICATE_THEME").unwrap_or_else(|_| "light".into());
    let locale = std::env::var("BUNNY_DUPLICATE_LANGUAGE").unwrap_or_else(|_| "en".into());
    let (index, exhaustive) = match profile.as_str() {
        "general" => (0, false),
        "optimized" => (1, false),
        "large" => (2, false),
        "exhaustive" => (2, true),
        _ => panic!("invalid BUNNY_DUPLICATE_PROFILE"),
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let preferences = crate::display_preferences::Preferences {
        theme: if appearance == "dark" { 2 } else { 1 },
        ..Default::default()
    };
    let (w, c) = crate::new_session(preferences, Some(locale.clone())).unwrap();
    let h = connect(&w, &c).unwrap();
    let _visibility = TestWindowVisibility::new(&w);
    w.window().set_size(slint::LogicalSize::new(780.0, 520.0));
    w.show().unwrap();
    w.invoke_activate_ribbon(1);
    w.invoke_duplicate_mode_changed(3);
    w.invoke_show_analysis();
    w.set_duplicate_single_root(source.to_str().expect("qualification path").into());
    w.invoke_duplicate_scope_edited(3);
    w.invoke_duplicate_profile_changed(index);
    w.set_duplicate_exhaustive(exhaustive);
    let timer = slint::Timer::default();
    let weak = w.as_weak();
    let begun = Instant::now();
    let mut clicked = None;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            let w = weak.upgrade().unwrap();
            c.borrow_mut().poll(&w);
            w.invoke_workshop_ready();
            assert!(
                begun.elapsed() < Duration::from_secs(180),
                "{} / {}",
                w.get_duplicate_summary(),
                w.get_duplicate_warning()
            );
            if clicked.is_none()
                && w.get_duplicate_ready()
                && begun.elapsed() > Duration::from_millis(300)
            {
                let position = slint::LogicalPosition::new(
                    w.get_search_duplicates_x() + 12.0,
                    w.get_search_duplicates_y() + 9.0,
                );
                clicked = Some(Instant::now());
                paint::arm();
                w.window().dispatch_event(WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                });
                w.window().dispatch_event(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
                assert!(w.get_search_busy(), "the search action was not activated");
            }
            if let Some(start) = clicked
                && let Some((_, first_frame_ms, frame)) =
                    paint::samples().into_iter().find(|s| s.0 == 4)
            {
                let applied = h.borrow().duplicate_applied_at.unwrap();
                assert!(w.get_search_complete(), "{}", w.get_duplicate_warning());
                assert!(w.get_duplicate_rows().row_count() > 0);
                assert!(w.get_duplicate_results_height() >= 34.0);
                let result = json!({
                    "profile": profile,
                    "locale": locale,
                    "theme": appearance,
                    "first_applied_ms": (applied - start).as_secs_f64() * 1000.0,
                    "first_presented_ms": first_frame_ms,
                    "first_presented_frame": frame,
                    "first_page_rows": w.get_duplicate_rows().row_count(),
                    "results_height_logical": w.get_duplicate_results_height(),
                    "groups": w.get_search_matches().as_str(),
                    "summary": w.get_duplicate_summary().as_str(),
                });
                std::fs::write(&evidence, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
                paint::disarm();
                slint::quit_event_loop().unwrap();
            }
        },
    );
    slint::run_event_loop().unwrap();
}
