use super::*;
use crate::locale::{Language, LanguagePreference};

// Keep the observed renderer untouched by reference renders for the ENTIRE
// sequence. A full render between frames resets its partial-rendering cache and
// can hide an accumulating invalidation fault (the older oracle below does so).
#[test]
fn footer_seven_languages_preserves_cache_until_all_observations_are_recorded() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("oracle.txt"), [1; 1024]).unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    window.invoke_set_column(3, false);
    window.show().unwrap();
    let (width, height) = (1600, 1000);
    adapter.set_size(slint::PhysicalSize::new(width, height));
    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, width as usize);
    });
    let footer = (width * (height - 85)) as usize..pixels.len();
    let choices = [1, 6, 7, 3, 4, 5, 2];
    let nouns = [
        "", "file", "fichier", "Datei", "archivo", "ficheiro", "file", "bestand",
    ];
    let mut records = Vec::new();
    for panel in [true, false, true, false] {
        window.set_preferences_open(panel);
        for (position, &choice) in choices.iter().enumerate() {
            // Two publications before the next frame, including IT/NL.
            window.invoke_set_language(choices[(position + 1) % choices.len()]);
            window.invoke_set_language(choice);
            let semantic = window.get_summary();
            assert!(semantic.contains(nouns[choice as usize]), "{semantic}");
            slint::platform::update_timers_and_animations();
            assert!(adapter.draw_if_needed(|r| {
                r.render(&mut pixels, width as usize);
            }));
            records.push((panel, choice, semantic, pixels[footer.clone()].to_vec()));
        }
    }
    // Only now reset the cache to construct independent full-render references.
    let mut discriminating = 0;
    for (index, (panel, choice, semantic, observed)) in records.iter().enumerate() {
        window.set_preferences_open(*panel);
        window.invoke_set_language(*choice);
        assert_eq!(&window.get_summary(), semantic, "replay state differs");
        slint::platform::update_timers_and_animations();
        assert!(adapter.draw_if_needed(|r| {
            r.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            r.render(&mut pixels, width as usize);
        }));
        assert_eq!(
            &pixels[footer.clone()],
            observed,
            "D1 replay {index}, language {choice}"
        );
        if index > 0 && records[index - 1].3 != pixels[footer.clone()] {
            discriminating += 1;
        }
    }
    assert_eq!(records.len(), 28);
    assert!(discriminating >= 20, "old-language pixels must be rejected");
    window.hide().unwrap();
}

// D1: getters can be correct while a retained framebuffer still contains the
// previous language. Compare the incremental pixels with a full render of the
// SAME state and geometry, after recording the incremental result first.
#[test]
fn footer_incremental_pixels_match_full_render_after_language_transactions() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("oracle.txt"), [1; 1024]).unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    window.show().unwrap();
    let (width, height) = (1600, 1000);
    adapter.set_size(slint::PhysicalSize::new(width, height));
    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, width as usize);
    });
    let mut discriminating_transitions = 0;
    for panel in [true, false, true, false] {
        window.set_preferences_open(panel);
        for index in [1, 3, 4, 5, 2, 1, 2] {
            let before = pixels.clone();
            window.invoke_set_language(index);
            let semantic = window.get_summary();
            let noun = ["", "file", "fichier", "Datei", "archivo", "ficheiro"][index as usize];
            assert!(
                semantic.contains(noun),
                "wrong semantic language: {semantic}"
            );
            slint::platform::update_timers_and_animations();
            assert!(adapter.draw_if_needed(|r| {
                r.render(&mut pixels, width as usize);
                let mut reference = vec![slint::Rgb8Pixel::default(); pixels.len()];
                r.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
                r.render(&mut reference, width as usize);
                // Footer only: animated/focused widgets elsewhere are excluded.
                let footer = (width * (height - 85)) as usize..pixels.len();
                if before[footer.clone()] != reference[footer.clone()] {
                    discriminating_transitions += 1;
                }
                assert_eq!(
                    pixels[footer.clone()],
                    reference[footer],
                    "D1: semantic={semantic}, language={index}, preferences={panel}"
                );
                r.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
            }));
        }
    }
    // Negative control: retaining the old pixels must fail this oracle even
    // though all semantic getters already expose the new language.
    assert!(discriminating_transitions >= 20);
    window.hide().unwrap();
}

// Multi-window tests need a distinct adapter per component; sharing a single
// MinimalSoftwareWindow makes constructing a bare window replace the first
// window's adapter state (including its viewport).
struct IndependentWindows;
impl slint::platform::Platform for IndependentWindows {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
    }
}

#[test]
fn startup_resolution_and_fallback_precede_first_frame_without_writing_preferences() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    for (system, expected) in [
        ("fr_FR", Language::French),
        ("fr_CA", Language::French),
        ("en_US", Language::English),
        ("en_GB", Language::English),
        ("de_DE", Language::German),
        ("de_CH", Language::German),
        ("es_ES", Language::Spanish),
        ("es-419", Language::Spanish),
        ("pt_PT", Language::Portuguese),
        ("pt_BR", Language::Portuguese),
        ("pt-BR", Language::Portuguese),
        ("ja_JP", Language::English),
        ("fr-???", Language::English),
    ] {
        let (window, state) = new_session(Default::default(), Some(system.into())).unwrap();
        assert!(!window.window().is_visible());
        assert_eq!(state.borrow().presentation.language, expected);
        assert_eq!(window.get_language_choice(), 0);
        assert_eq!(window.get_detected_language(), system);
        assert_eq!(
            window.get_effective_language(),
            Language::ALL.iter().position(|l| *l == expected).unwrap() as i32
        );
        assert!(state.borrow().preference_store.is_none());
    }
    for language in Language::ALL {
        let preferences = display_preferences::Preferences {
            language: LanguagePreference::Explicit(language),
            ..Default::default()
        };
        let (window, state) = new_session(preferences.clone(), Some("fr_FR".into())).unwrap();
        assert_eq!(state.borrow().preferences, preferences);
        assert_eq!(state.borrow().presentation.language, language);
        assert!(!window.get_language_fallback());
        assert!(!window.get_language_failed());
    }
}

#[test]
fn explicit_to_auto_same_effective_language_is_flushed_and_reopened() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("settings/display-v1.conf");
    let preferences = display_preferences::Preferences {
        language: LanguagePreference::Explicit(Language::French),
        ..Default::default()
    };
    crate::preferences_store::save(&path, &preferences).unwrap();
    let (window, state) = new_session(preferences, Some("fr_FR".into())).unwrap();
    state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(path.clone()).unwrap());
    window.invoke_set_language(0);
    assert_eq!(state.borrow().presentation.language, Language::French);
    assert_eq!(window.get_preference_state(), 1);
    state.borrow_mut().preference_store = None; // flush on application close
    let (preferences, status) = crate::preferences_store::load(&path);
    assert_eq!(status, 0);
    assert_eq!(preferences.language, LanguagePreference::Automatic);
    drop(state);
    drop(window);
    let (window, state) = new_session(preferences, Some("en_GB".into())).unwrap();
    assert_eq!(state.borrow().presentation.language, Language::English);
    assert_eq!(window.get_language_choice(), 0);
    for index in [2, 1, 3, 4, 5, 2, 0] {
        state.borrow_mut().preference_store =
            Some(crate::preferences_store::Store::start(path.clone()).unwrap());
        window.invoke_set_language(index);
        state.borrow_mut().preference_store = None;
        assert_eq!(
            crate::preferences_store::load(&path).0,
            state.borrow().preferences
        );
    }
}

#[test]
fn fifty_thousand_nodes_all_orders_two_windows_legal_and_state_survive_real_transaction() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let (second, second_state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let legal = LegalWindow::new().unwrap();
    state.borrow_mut().legal = Some(legal.as_weak());
    legal.set_full_license(true);
    legal.set_license_text(include_str!("../../../../LICENSE").into());
    let license = legal.get_license_text();
    state.borrow_mut().tree = Some(owner_gui_tests::synthetic_tree(50_000));
    state.borrow_mut().show_rows(&window);
    // This synthetic inventory has no Top100 entries; the real-scan test below does.
    let mut timings = Vec::new();
    for sort in 0..12 {
        {
            let mut s = state.borrow_mut();
            let tree = s.tree.as_mut().unwrap();
            match sort {
                0..=3 => {
                    tree.sort_by(sort >= 2);
                    if tree.sort() != sort {
                        tree.sort_by(sort >= 2);
                    }
                }
                4 | 5 => {
                    tree.sort_allocated();
                    if tree.sort() != sort {
                        tree.sort_allocated();
                    }
                }
                6 | 7 => tree.sort_owner(None, sort - 6),
                _ => {
                    tree.sort_dates(sort >= 10);
                    if tree.sort() != sort {
                        tree.sort_dates(sort >= 10);
                    }
                }
            }
            s.show_rows(&window);
        }
        window.invoke_change_page(true, true);
        let node = window.get_details().row_data(32).unwrap().node_id;
        window.invoke_inspect_entry(node);
        window.invoke_set_column(2, false);
        window.invoke_set_units(true);
        window.set_detail_scroll_y(-102.0);
        window.set_detail_scroll_x(-700.0);
        window.set_top_scroll_y(-34.0);
        window.set_preferences_open(true);
        let before = state.borrow().tree.as_ref().unwrap().raw_fingerprint();
        let key = state.borrow().tree.as_ref().unwrap().owner_page_key();
        let generation = state.borrow().generation;
        let owner_page = state.borrow().owner_page;
        let serial = state.borrow().owner_sort_serial;
        let ids: Vec<_> = window.get_details().iter().map(|r| r.node_id).collect();
        let path = window.get_entry_path();
        let rows = window.get_rows();
        let details = window.get_details();
        let units = state.borrow().preferences.units;
        let columns = state.borrow().preferences.visible;
        for index in [2, 1, 3, 4, 5, 2] {
            let start = Instant::now();
            window.invoke_set_language(index);
            timings.push(start.elapsed().as_secs_f64() * 1000.0);
            let s = state.borrow();
            assert_eq!(s.tree.as_ref().unwrap().raw_fingerprint(), before);
            assert_eq!(s.tree.as_ref().unwrap().owner_page_key(), key);
            assert_eq!(s.generation, generation);
            assert_eq!(s.owner_page, owner_page);
            assert_eq!(s.owner_sort_serial, serial);
            assert!(s.task.is_none() && s.owner_task.is_none());
            assert!(!s.sources.loading());
            assert_eq!(s.preferences.units, units);
            assert_eq!(s.preferences.visible, columns);
            assert_eq!(window.get_details(), details);
            assert_eq!(window.get_rows(), rows);
            assert_eq!(
                window
                    .get_details()
                    .iter()
                    .map(|r| r.node_id)
                    .collect::<Vec<_>>(),
                ids
            );
            assert_eq!(window.get_entry_path(), path);
            assert_eq!(window.get_inspected_node(), node);
            assert_eq!(window.get_detail_scroll_y(), -102.0);
            assert_eq!(window.get_detail_scroll_x(), -700.0);
            assert_eq!(window.get_top_scroll_y(), -34.0);
            assert!(window.get_preferences_open());
            assert_eq!(window.get_sort_order(), sort as i32);
            assert_eq!(
                second_state.borrow().presentation.language,
                s.presentation.language
            );
            assert_eq!(second.get_effective_language(), index - 1);
            assert!(legal.get_full_license());
            assert_eq!(legal.get_license_text(), license);
            let (notice, file) = match index {
                1 => ("free software", "File"),
                2 => ("logiciel libre", "Fichier"),
                3 => ("freie Software", "Datei"),
                4 => ("software libre", "Archivo"),
                5 => ("software livre", "Ficheiro"),
                _ => unreachable!(),
            };
            assert!(legal.get_notices().contains(notice));
            assert_eq!(window.get_details().row_data(0).unwrap().note, file);
        }
    }
    timings.sort_by(f64::total_cmp);
    eprintln!(
        "B2_TRANSACTION nodes=50000 page=2000 samples={} median_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        timings.len(),
        timings[timings.len() / 2],
        timings[timings.len() * 95 / 100],
        timings.last().unwrap()
    );
}

#[test]
fn running_scan_late_completion_errors_cancellation_and_top100_use_current_language() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for i in 0..2_007 {
        fs::write(fixture.path().join(format!("été-{i:04}")), [1; 4096]).unwrap();
    }
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.set_collect_allocation(true);
    window.set_collect_owner(true);
    window.invoke_start_scan();
    let mailbox = state.borrow().task.as_ref().unwrap().mailbox.clone();
    let generation = state.borrow().generation;
    window.invoke_set_language(1);
    assert_eq!(window.get_status(), "Scan in progress…");
    assert_eq!(state.borrow().generation, generation);
    assert!(std::sync::Arc::ptr_eq(
        &mailbox,
        &state.borrow().task.as_ref().unwrap().mailbox
    ));
    wait(&window, &state);
    assert_eq!(window.get_status(), "Scan complete");
    assert!(window.get_summary().contains("2,007 files"));
    assert_eq!(window.get_top_rows().row_count(), 100);
    window.invoke_select_top(7);
    window.set_top_scroll_y(-34.0);
    let top = window.get_top_rows();
    let top_path = window.get_top_path();
    fixture.close().unwrap(); // A language change cannot read the former source.
    window.invoke_set_language(2);
    assert!(window.get_summary().contains("2\u{202f}007 fichiers"));
    for (choice, summary) in [
        (3, "2.007 Dateien"),
        (4, "2.007 archivos"),
        (5, "2\u{a0}007 ficheiros"),
        (2, "2\u{202f}007 fichiers"),
    ] {
        window.invoke_set_language(choice);
        assert!(window.get_summary().contains(summary));
        assert_eq!(state.borrow().generation, generation);
        assert_eq!(window.get_top_rows(), top);
        assert_eq!(window.get_top_path(), top_path);
    }
    assert_eq!(window.get_top_rows(), top);
    assert_eq!(window.get_top_selected(), 7);
    assert_eq!(window.get_top_path(), top_path);
    assert_eq!(window.get_top_scroll_y(), -34.0);
    assert!(
        window
            .get_top_rows()
            .row_data(0)
            .unwrap()
            .size
            .contains(',')
    );
    window.invoke_start_scan();
    wait(&window, &state);
    assert_eq!(window.get_status(), "Analyse impossible.");
    window.invoke_set_language(1);
    assert_eq!(window.get_status(), "Scan failed.");
    assert!(window.get_errors().starts_with("Could not access "));
    assert_eq!(window.get_empty_message(), "No inventory available.");
    for (choice, failed, prefix) in [
        (3, "Analyse fehlgeschlagen.", "Zugriff auf "),
        (4, "El análisis falló.", "No se pudo acceder a "),
        (5, "A análise falhou.", "Não foi possível aceder a "),
        (2, "Analyse impossible.", "Impossible d'accéder à "),
    ] {
        let before = state.borrow().generation;
        window.invoke_set_language(choice);
        assert_eq!(window.get_status(), failed);
        assert!(window.get_errors().starts_with(prefix));
        assert_eq!(state.borrow().generation, before);
    }

    let mailbox = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox {
        directory: None,
        patch: None,
        progress: Some(worker::DisplayLabels::Progress {
            files: 1234,
            directories: 2,
            bytes: 2048,
            errors: 1,
        }),
        completion: None,
    }));
    state.borrow_mut().task = Some(Task {
        cancellation: Default::default(),
        mailbox,
    });
    state
        .borrow_mut()
        .status(&window, presentation::Status::Scanning);
    window.invoke_set_language(2);
    state.borrow_mut().poll(&window);
    assert!(window.get_summary().starts_with("1\u{202f}234 fichiers"));
    window.invoke_cancel_scan();
    window.invoke_set_language(1);
    assert!(window.get_cancelling());
    assert_eq!(window.get_status(), "Cancellation requested…");
    assert!(
        state
            .borrow()
            .task
            .as_ref()
            .unwrap()
            .cancellation
            .is_cancelled()
    );
}

#[test]
fn delayed_volume_and_owner_results_render_in_current_language_without_new_requests() {
    use sortsize_core::NativeOwnerId;
    use sortsize_owner_resolution::{AccountName, LookupLimits, OwnerLookup, OwnerNameResolution};
    use std::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    struct Delayed {
        calls: AtomicUsize,
        gate: (Mutex<bool>, Condvar),
    }
    impl OwnerLookup for Delayed {
        fn lookup(&self, _: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (lock, cv) = &self.gate;
            let _guard = cv
                .wait_while(lock.lock().unwrap(), |ready| !*ready)
                .unwrap();
            OwnerNameResolution::Resolved(AccountName::UnixBytes(
                b"opaque-{n}-{}-UID".to_vec().into(),
            ))
        }
    }
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    state.borrow_mut().tree = Some(owner_gui_tests::synthetic_tree(100));
    state.borrow_mut().show_rows(&window);
    let provider = Arc::new(Delayed {
        calls: AtomicUsize::new(0),
        gate: (Mutex::new(false), Condvar::new()),
    });
    state.borrow_mut().owner_task = Some(owner_view::Task::start(provider.clone()).unwrap());
    window.set_resolve_owner(true);
    window.invoke_owner_resolution_changed();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let gated = gate.clone();
    state.borrow_mut().sources = crate::volume_view::Sources::with_provider(Arc::new(move |_| {
        called.fetch_add(1, Ordering::SeqCst);
        let (lock, cv) = &*gated;
        let _guard = cv
            .wait_while(lock.lock().unwrap(), |ready| !*ready)
            .unwrap();
        Ok(volume_gui_tests::discovered(&["native-volume".into()]))
    }));
    window.invoke_refresh_volumes(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while calls.load(Ordering::SeqCst) == 0 || provider.calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let generation = state.borrow().generation;
    let owner_page = state.borrow().owner_page;
    for choice in [2, 1, 3, 4, 5, 2, 1] {
        window.invoke_set_language(choice);
    }
    assert_eq!(window.get_storage_status(), "Discovering…");
    assert_eq!(window.get_owner_status(), "Resolving page names…");
    assert_eq!(state.borrow().generation, generation);
    assert_eq!(state.borrow().owner_page, owner_page);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    *provider.gate.0.lock().unwrap() = true;
    provider.gate.1.notify_all();
    while window.get_storage_loading()
        || !window
            .get_details()
            .row_data(0)
            .unwrap()
            .owner
            .starts_with("opaque-")
    {
        state.borrow_mut().poll(&window);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    while window.get_owner_status()
        != "Page names updated; native identifiers retained as fallback."
    {
        state.borrow_mut().poll(&window);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let looked_up = provider.calls.load(Ordering::SeqCst);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        window
            .get_volumes()
            .row_data(0)
            .unwrap()
            .state
            .starts_with("Local · Available")
    );
    let volumes = window.get_volumes();
    window.invoke_set_language(2);
    assert_eq!(window.get_volumes(), volumes);
    assert!(
        window
            .get_volumes()
            .row_data(0)
            .unwrap()
            .state
            .starts_with("Local · Disponible")
    );
    assert!(
        window
            .get_details()
            .row_data(0)
            .unwrap()
            .owner
            .starts_with("opaque-{n}-{}-UID")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), looked_up);
}

#[test]
fn language_selector_keyboard_changes_the_catalog_through_the_actual_widget() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 760));
    window.set_preferences_open(true);
    window.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1200);
    });
    let position = slint::LogicalPosition::new(500.0, 156.0);
    for event in [
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        },
        WindowEvent::KeyPressed {
            text: Key::DownArrow.into(),
        },
        WindowEvent::KeyReleased {
            text: Key::DownArrow.into(),
        },
        WindowEvent::KeyPressed {
            text: Key::Return.into(),
        },
        WindowEvent::KeyReleased {
            text: Key::Return.into(),
        },
    ] {
        window.window().dispatch_event(event);
    }
    assert_eq!(
        state.borrow().preferences.language,
        LanguagePreference::Explicit(Language::English)
    );
    assert_eq!(state.borrow().presentation.language, Language::English);
    assert_eq!(window.get_language_choice(), 1);
    assert_eq!(window.get_status(), "Choose a folder to scan.");
    window.hide().unwrap();
}

#[test]
fn new_session_republishes_global_bundle_and_repairs_stale_model_lengths() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (first, state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    state.borrow_mut().tree = Some(owner_gui_tests::synthetic_tree(2500));
    state.borrow_mut().show_rows(&first);
    let legal = LegalWindow::new().unwrap();
    state.borrow_mut().legal = Some(legal.as_weak());
    let before = state.borrow().tree.as_ref().unwrap().raw_fingerprint();
    let generation = state.borrow().generation;
    // Simulate a stale UI page from a preceding publication. Both shorter and
    // longer models must converge to the authoritative raw page without panic.
    for count in [0, 2200] {
        first.set_details(Rc::new(slint::VecModel::from(vec![DetailRow::default(); count])).into());
        let (second, second_state) = new_session(Default::default(), Some("de_DE".into())).unwrap();
        assert_eq!(first.get_details().row_count(), 2000);
        assert_eq!(state.borrow().presentation.language, Language::German);
        assert_eq!(
            second_state.borrow().presentation.language,
            Language::German
        );
        assert_eq!(first.get_status(), "Ordner zum Analysieren auswählen.");
        assert_eq!(second.get_status(), first.get_status());
        assert_eq!(
            legal.get_localized_title(),
            "Bunny and Size — Über / Lizenzen"
        );
        assert!(legal.get_notices().contains("freie Software"));
        assert_eq!(
            state.borrow().tree.as_ref().unwrap().raw_fingerprint(),
            before
        );
        assert_eq!(state.borrow().generation, generation);
        assert!(state.borrow().task.is_none());
        first.invoke_set_language(5);
        assert_eq!(
            second_state.borrow().presentation.language,
            Language::Portuguese
        );
        assert_eq!(first.get_details().row_data(0).unwrap().note, "Ficheiro");
    }
}

#[test]
fn automatic_preference_survives_new_window_without_publication_or_save() {
    slint::platform::set_platform(Box::new(IndependentWindows)).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("display-v1.conf");
    let preferences = display_preferences::Preferences {
        language: LanguagePreference::Explicit(Language::French),
        ..Default::default()
    };
    let (window, state) = new_session(preferences, Some("fr_FR".into())).unwrap();
    state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(path.clone()).unwrap());
    window.invoke_set_language(0);
    state.borrow_mut().preference_store = None;
    let saved = fs::read(&path).unwrap();
    state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(path.clone()).unwrap());
    window.set_preference_state(0);
    let generation = state.borrow().presentation.generation;
    let _scope = state.borrow().presentation.enter();
    let bare = new_window().unwrap();
    assert_eq!(
        state.borrow().preferences.language,
        LanguagePreference::Automatic
    );
    assert_eq!(state.borrow().presentation.language, Language::French);
    assert_eq!(state.borrow().presentation.generation, generation);
    assert_eq!(window.get_language_choice(), 0);
    assert_eq!(window.get_effective_language(), 1);
    assert_eq!(window.get_status(), "Choisis un dossier à analyser.");
    assert_eq!(bare.get_status(), window.get_status());
    assert_eq!(window.get_preference_state(), 0);
    assert_eq!(
        state
            .borrow()
            .preference_store
            .as_ref()
            .unwrap()
            .submissions(),
        0
    );
    state.borrow_mut().preference_store = None;
    assert_eq!(fs::read(&path).unwrap(), saved);
    assert_eq!(
        crate::preferences_store::load(&path).0.language,
        LanguagePreference::Automatic
    );
}

#[test]
fn distinct_session_preferences_survive_initialization_close_reopen_and_explicit_choice() {
    slint::platform::set_platform(Box::new(IndependentWindows)).unwrap();
    let directory = TempDir::new().unwrap();
    let first_path = directory.path().join("first.conf");
    let second_path = directory.path().join("second.conf");
    let first_preferences = display_preferences::Preferences::default();
    let second_preferences = display_preferences::Preferences {
        language: LanguagePreference::Explicit(Language::German),
        ..Default::default()
    };
    crate::preferences_store::save(&first_path, &first_preferences).unwrap();
    crate::preferences_store::save(&second_path, &second_preferences).unwrap();
    let (first, state) = new_session(first_preferences.clone(), Some("fr_FR".into())).unwrap();
    state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(first_path.clone()).unwrap());
    let (second, second_state) =
        new_session(second_preferences.clone(), Some("en_GB".into())).unwrap();
    second_state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(second_path.clone()).unwrap());
    assert_eq!(state.borrow().preferences, first_preferences);
    assert_eq!(second_state.borrow().preferences, second_preferences);
    for (window, controller) in [(&first, &state), (&second, &second_state)] {
        assert_eq!(controller.borrow().presentation.language, Language::German);
        assert_eq!(window.get_status(), "Ordner zum Analysieren auswählen.");
        assert_eq!(window.get_effective_language(), 2);
        assert_eq!(window.get_preference_state(), 0);
    }
    assert_eq!(first.get_language_choice(), 0);
    assert_eq!(second.get_language_choice(), 3);
    let _english = presentation::Context {
        language: Language::English,
        generation: 0,
    }
    .enter();
    let bare = new_window().unwrap();
    assert_eq!(bare.get_status(), "Choose a folder to scan.");
    for window in [&first, &second] {
        assert_eq!(window.get_status(), bare.get_status());
        assert_eq!(window.get_effective_language(), 0);
    }
    assert_eq!(state.borrow().preferences, first_preferences);
    assert_eq!(second_state.borrow().preferences, second_preferences);
    for controller in [&state, &second_state] {
        assert_eq!(
            controller
                .borrow()
                .preference_store
                .as_ref()
                .unwrap()
                .submissions(),
            0
        );
    }
    state.borrow_mut().preference_store = None;
    second_state.borrow_mut().preference_store = None;
    assert_eq!(
        crate::preferences_store::load(&first_path).0,
        first_preferences
    );
    assert_eq!(
        crate::preferences_store::load(&second_path).0,
        second_preferences
    );
    drop(second_state);
    drop(second);
    let (second, second_state) = new_session(
        crate::preferences_store::load(&second_path).0,
        Some("en_GB".into()),
    )
    .unwrap();
    assert_eq!(first.get_status(), "Ordner zum Analysieren auswählen.");
    assert_eq!(state.borrow().preferences, first_preferences);
    for (controller, path) in [(&state, &first_path), (&second_state, &second_path)] {
        controller.borrow_mut().preference_store =
            Some(crate::preferences_store::Store::start(path.clone()).unwrap());
    }
    second.invoke_set_language(5);
    for (window, controller) in [(&first, &state), (&second, &second_state)] {
        assert_eq!(
            controller.borrow().preferences.language,
            LanguagePreference::Explicit(Language::Portuguese)
        );
        assert_eq!(window.get_language_choice(), 5);
        assert_eq!(window.get_effective_language(), 4);
        assert_eq!(window.get_status(), "Escolhe uma pasta para analisar.");
        assert_eq!(window.get_preference_state(), 1);
        controller.borrow_mut().preference_store = None;
    }
    for path in [&first_path, &second_path] {
        let saved = crate::preferences_store::load(path).0;
        assert_eq!(
            saved.language,
            LanguagePreference::Explicit(Language::Portuguese)
        );
        let (reopened, controller) = new_session(saved, Some("fr_FR".into())).unwrap();
        assert_eq!(reopened.get_language_choice(), 5);
        assert_eq!(
            controller.borrow().presentation.language,
            Language::Portuguese
        );
    }
}

#[test]
fn unchanged_effective_language_preserves_models_and_prunes_closed_windows() {
    slint::platform::set_platform(Box::new(IndependentWindows)).unwrap();
    let (window, state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    state.borrow_mut().tree = Some(owner_gui_tests::synthetic_tree(2500));
    state.borrow_mut().show_rows(&window);
    let context = state.borrow().presentation;
    let rows = window.get_rows();
    let details = window.get_details();
    let columns = window.get_column_settings();
    let raw = state.borrow().tree.as_ref().unwrap().raw_fingerprint();
    let count = crate::language_controller::participant_count();
    let _scope = context.enter();
    for _ in 0..32 {
        let bare = new_window().unwrap();
        let (other, other_state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
        assert_eq!(state.borrow().presentation, context);
        assert_eq!(window.get_rows(), rows);
        assert_eq!(window.get_details(), details);
        assert_eq!(window.get_column_settings(), columns);
        assert_eq!(state.borrow().tree.as_ref().unwrap().raw_fingerprint(), raw);
        assert_eq!(crate::language_controller::participant_count(), count + 1);
        drop(other_state);
        drop(other);
        drop(bare);
        assert_eq!(crate::language_controller::participant_count(), count);
    }
}

#[test]
fn unavailable_activation_preserves_distinct_preferences_and_does_not_save() {
    slint::platform::set_platform(Box::new(IndependentWindows)).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("no-unsolicited-write.conf");
    let preferences = display_preferences::Preferences {
        language: LanguagePreference::Explicit(Language::German),
        ..Default::default()
    };
    let (first, state) = new_session(preferences.clone(), Some("fr_FR".into())).unwrap();
    state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(path.clone()).unwrap());
    let (second, second_state) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    // Inject the activation result of a build without DE. The real bundled
    // fallback is additionally exercised in the separate native negative build.
    crate::locale::activate_bundled(Language::English).unwrap();
    crate::language_controller::publish_effective_language(crate::locale::Activation {
        effective: Language::English,
        unavailable_catalog: Some(Language::German),
    });
    for window in [&first, &second] {
        assert_eq!(window.get_status(), "Choose a folder to scan.");
        assert_eq!(window.get_effective_language(), 0);
        assert!(window.get_language_fallback());
        assert_eq!(window.get_unavailable_language(), 2);
    }
    assert_eq!(state.borrow().preferences, preferences);
    assert_eq!(
        second_state.borrow().preferences.language,
        LanguagePreference::Automatic
    );
    assert_eq!(
        state
            .borrow()
            .preference_store
            .as_ref()
            .unwrap()
            .submissions(),
        0
    );
    let context = state.borrow().presentation;
    crate::language_controller::publish_effective_language(crate::locale::Activation {
        effective: Language::English,
        unavailable_catalog: None,
    });
    assert!(!first.get_language_fallback());
    assert_eq!(state.borrow().presentation, context);
    state.borrow_mut().preference_store = None;
    assert!(!path.exists());
}

#[test]
fn legacy_files_survive_three_complete_startup_shutdown_sessions_without_any_write() {
    slint::platform::set_platform(Box::new(IndependentWindows)).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("sortsize/display-v1.conf");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for version in [1, 2] {
        for language in Language::ALL {
            for units in ["binary", "decimal"] {
                let content = format!(
                    "version={version}\nunits={units}\nhidden_columns=logical_size,allocated_size,owner,folder_share,recursive_files,recursive_directories,kind,modified,accessed,full_path\nlanguage={}\nfuture=preserve original formatting\n",
                    language.as_str()
                );
                fs::write(&path, &content).unwrap();
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                for _ in 0..3 {
                    let (preferences, status) = crate::preferences_store::load(&path);
                    assert_eq!(status, 0);
                    assert!(preferences.visible[0]);
                    assert!(preferences.visible[1..].iter().all(|visible| !visible));
                    assert_eq!(
                        preferences.language,
                        if version == 1 {
                            LanguagePreference::Automatic
                        } else {
                            LanguagePreference::Explicit(language)
                        }
                    );
                    let (window, state) =
                        new_session(preferences.clone(), Some("en_GB".into())).unwrap();
                    state.borrow_mut().preference_store =
                        Some(crate::preferences_store::Store::start(path.clone()).unwrap());
                    assert_eq!(state.borrow().preferences, preferences);
                    drop(state);
                    drop(window);
                    assert_eq!(fs::read_to_string(&path).unwrap(), content);
                    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
                }
            }
        }
    }
    assert!(!directory.path().join("bunny-and-size").exists());
}
