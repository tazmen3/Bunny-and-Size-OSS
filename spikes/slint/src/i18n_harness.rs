//! Explicit laboratory executable, available only with `benchmarks`.
//! Uses the real controller/backend; never reads or writes user preferences.
use crate::{
    Controller, LegalWindow, SortSizeWindow, connect, locale::Language, presentation::Context,
};
use slint::{ComponentHandle, Model, Timer, TimerMode};
use std::{
    cell::RefCell,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

fn capture(component: &impl ComponentHandle, directory: &Path, name: &str) {
    let pixels = component.window().take_snapshot().expect("native snapshot");
    let mut file = std::fs::File::create(directory.join(format!("{name}.ppm"))).unwrap();
    write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
    let bytes: Vec<_> = pixels
        .as_slice()
        .iter()
        .flat_map(|p| [p.r, p.g, p.b])
        .collect();
    file.write_all(&bytes).unwrap();
}
// Rust Debug emits \u{202f}, which is not JSON. Keep human Unicode intact
// and escape JSON control characters in laboratory reports as well.
fn json_string(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '\"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c <= '\u{001f}' => result.push_str(&format!("\\u{:04x}", c as u32)),
            c => result.push(c),
        }
    }
    result.push('\"');
    result
}

fn record(
    controller: &Controller,
    window: &SortSizeWindow,
    out: &Path,
    startup_ms: f64,
    samples: &[f64],
) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    assert!(controller.task.is_none());
    std::fs::write(out.join("native.json"), format!(
        "{{\"language\":\"{}\",\"startup_ms\":{},\"callback_p95_ms\":{},\"callback_max_ms\":{},\"callbacks\":{},\"details\":{},\"top\":{},\"status\":{},\"summary\":{},\"scan_generation\":{},\"owner_resolution_enabled\":false,\"preferences_used\":false}}\n",
        controller.presentation.language.as_str(), startup_ms, p95, sorted.last().unwrap(), sorted.len(),
        window.get_details().row_count(), window.get_top_rows().row_count(), json_string(window.get_status().as_str()),
        json_string(window.get_summary().as_str()), controller.generation)).unwrap();
}
fn fixture_volumes(controller: &Rc<RefCell<Controller>>, window: &SortSizeWindow) {
    #[cfg(any(target_os = "linux", windows))]
    {
        use sortsize_core::{MetadataValue, ValueAccuracy, volumes::*};
        let known = |value| MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        };
        let path = PathBuf::from("public-volume");
        controller.borrow_mut().sources.current =
            Some(crate::volume_view::Prepared::new(VolumeDiscovery {
                volumes: vec![Volume {
                    identity: VolumeIdentity::WindowsPath(path.clone()),
                    label: MetadataValue::Known {
                        value: "Public fixture".into(),
                        accuracy: ValueAccuracy::Exact,
                    },
                    filesystem: MetadataValue::Unknown,
                    kind: SourceKind::Local,
                    mounts: vec![VolumeMount {
                        path,
                        mount_id: None,
                        root: None,
                    }],
                    capacity: VolumeCapacity::from_bytes(
                        known(1_000_000),
                        known(500_000),
                        known(500_000),
                    ),
                    availability: VolumeAvailability::Available,
                }],
                enumeration_time: Duration::ZERO,
                capacity_time: Duration::ZERO,
                capacity_calls: 0,
            }));
        controller.borrow().show_sources(window);
    }
}

pub fn run(
    language: Language,
    fixture: PathBuf,
    out: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    assert!(matches!(language, Language::English | Language::French));
    std::fs::create_dir_all(&out)?;
    let context = Context {
        language,
        generation: 1,
    };
    let _scope = context.enter();
    let backend = i_slint_backend_winit::Backend::builder()
        .with_renderer_name("software")
        .build()?;
    slint::platform::set_platform(Box::new(backend))?;
    let start = Instant::now();
    let window = crate::new_window()?;
    let legal = LegalWindow::new()?;
    let startup_ms = start.elapsed().as_secs_f64() * 1000.0;
    window.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_notices(
        tr!(include_str!("legal-notice-en.txt"))
            .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
            .into(),
    );
    legal.set_license_text(include_str!("../../../LICENSE").into());
    window
        .window()
        .set_size(slint::LogicalSize::new(1200.0, 760.0));
    let controller = connect(&window);
    fixture_volumes(&controller, &window);
    window.set_collect_allocation(true);
    window.set_collect_owner(true);
    window.set_scan_path(
        fixture
            .to_str()
            .ok_or("harness fixture must be Unicode")?
            .into(),
    );
    let weak = window.as_weak();
    let weak_legal = legal.as_weak();
    let state = controller.clone();
    let samples = Rc::new(RefCell::new(Vec::new()));
    let measured = samples.clone();
    let mut phase = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        let window = weak.upgrade().unwrap();
        let legal = weak_legal.upgrade().unwrap();
        assert!(
            Instant::now() < deadline,
            "native harness timeout at phase {phase}"
        );
        let start = Instant::now();
        state.borrow_mut().poll(&window);
        measured
            .borrow_mut()
            .push(start.elapsed().as_secs_f64() * 1000.0);
        match phase {
            0 => window.invoke_start_scan(),
            1 if window.get_busy() => return,
            1 => {
                assert_eq!(
                    window.get_status(),
                    if language == Language::French {
                        "Analyse terminée"
                    } else {
                        "Scan complete"
                    }
                );
            }
            2 => capture(&window, &out, "main-1200"),
            3 => window
                .window()
                .set_size(slint::LogicalSize::new(960.0, 600.0)),
            4 => capture(&window, &out, "main-960"),
            5 => window.invoke_change_page(true, true),
            6 => capture(&window, &out, "page-2"),
            7 => {
                window.invoke_change_page(true, false);
                window.set_top_visible(true);
            }
            8 => capture(&window, &out, "top-960"),
            9 => {
                window.set_top_visible(false);
                window.set_preferences_open(true);
            }
            10 => capture(&window, &out, "preferences-960"),
            11 => {
                window.set_preferences_open(false);
                legal.show().unwrap();
            }
            12 => capture(&legal, &out, "legal"),
            13 => legal.set_full_license(true),
            14 => capture(&legal, &out, "gpl"),
            15 => {
                legal.hide().unwrap();
                window.set_scan_path(fixture.join("missing").to_str().unwrap().into());
                window.invoke_start_scan();
            }
            16 if window.get_busy() => return,
            16 => {
                assert_eq!(
                    window.get_status(),
                    if language == Language::French {
                        "Analyse impossible."
                    } else {
                        "Scan failed."
                    }
                );
            }
            17 => {
                capture(&window, &out, "error-960");
                window.set_scan_path(fixture.to_str().unwrap().into());
                window.invoke_start_scan();
            }
            18 if window.get_busy() => return,
            18 => {
                assert_eq!(
                    window.get_status(),
                    if language == Language::French {
                        "Analyse terminée"
                    } else {
                        "Scan complete"
                    }
                );
            }
            19 => {
                capture(&window, &out, "recovered-960");
                record(
                    &state.borrow(),
                    &window,
                    &out,
                    startup_ms,
                    &measured.borrow(),
                );
                window.invoke_start_scan();
                window.invoke_cancel_scan();
            }
            20 if window.get_busy() => return,
            20 => {
                assert!(
                    window
                        .get_status()
                        .starts_with(if language == Language::French {
                            "Analyse annulée"
                        } else {
                            "Scan cancelled"
                        })
                );
            }
            21 => {
                capture(&window, &out, "cancelled-960");
                std::fs::write(
                    out.join("cancellation.json"),
                    format!(
                        "{{\"language\":\"{}\",\"cancelled\":true,\"busy\":{},\"status\":{}}}\n",
                        language.as_str(),
                        window.get_busy(),
                        json_string(window.get_status().as_str())
                    ),
                )
                .unwrap();
                window.hide().unwrap();
                slint::quit_event_loop().unwrap();
            }
            _ => {}
        }
        phase += 1;
    });
    window.run()?;
    timer.stop();
    Ok(())
}

/// Native-backend regression for initialization versus explicit user choice.
/// Locales and actions are injected; the separate product recipe uses OS input.
fn check_initialization_coherence(
    first: &SortSizeWindow,
    state: &Rc<RefCell<Controller>>,
    second: &SortSizeWindow,
    second_state: &Rc<RefCell<Controller>>,
    system: &str,
    out: &Path,
) {
    use crate::{display_preferences::Preferences, locale::LanguagePreference, preferences_store};
    let original = state.borrow().presentation;
    {
        let _scope = original.enter();
        let bare = crate::new_window().unwrap();
        assert_eq!(bare.get_status(), first.get_status());
        assert_eq!(
            state.borrow().preferences.language,
            LanguagePreference::Automatic
        );
        assert_eq!(state.borrow().presentation, original);
    }
    let path = out.join("coherence-german.conf");
    let preferences = Preferences {
        language: LanguagePreference::Explicit(Language::German),
        ..Default::default()
    };
    preferences_store::save(&path, &preferences).unwrap();
    let saved = std::fs::read(&path).unwrap();
    let (other, other_state) =
        crate::new_session(preferences.clone(), Some("en_GB".into())).unwrap();
    other_state.borrow_mut().preference_store =
        Some(preferences_store::Store::start(path.clone()).unwrap());
    other
        .window()
        .set_size(slint::LogicalSize::new(1200.0, 760.0));
    other.set_preferences_open(true);
    first.set_preferences_open(true);
    other.show().unwrap();
    for (window, controller) in [
        (first, state),
        (second, second_state),
        (&other, &other_state),
    ] {
        assert_eq!(controller.borrow().presentation.language, Language::German);
        assert_eq!(window.get_effective_language(), 2);
        assert_eq!(window.get_status(), "Ordner zum Analysieren auswählen.");
        assert_eq!(window.get_preference_state(), 0);
    }
    assert_eq!(first.get_language_choice(), 0);
    assert_eq!(other.get_language_choice(), 3);
    assert_eq!(
        state.borrow().preferences.language,
        LanguagePreference::Automatic
    );
    assert_eq!(other_state.borrow().preferences, preferences);
    capture(first, out, "coherence-auto-effective-de");
    capture(&other, out, "coherence-explicit-de");
    other_state.borrow_mut().preference_store = None;
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    other.hide().unwrap();
    drop(other_state);
    drop(other);
    let (reopened, reopened_state) =
        crate::new_session(preferences_store::load(&path).0, Some("fr_FR".into())).unwrap();
    assert_eq!(reopened.get_language_choice(), 3);
    assert_eq!(
        state.borrow().preferences.language,
        LanguagePreference::Automatic
    );
    let first_path = out.join("config/display-v1.conf");
    state.borrow_mut().preference_store = None;
    assert!(!first_path.exists(), "initialization submitted preferences");
    state.borrow_mut().preference_store =
        Some(preferences_store::Store::start(first_path.clone()).unwrap());
    reopened_state.borrow_mut().preference_store =
        Some(preferences_store::Store::start(path.clone()).unwrap());
    second.invoke_set_language(5);
    for controller in [state, second_state, &reopened_state] {
        assert_eq!(
            controller.borrow().preferences.language,
            LanguagePreference::Explicit(Language::Portuguese)
        );
        assert_eq!(
            controller.borrow().presentation.language,
            Language::Portuguese
        );
    }
    state.borrow_mut().preference_store = None;
    reopened_state.borrow_mut().preference_store = None;
    for config in [&first_path, &path] {
        assert_eq!(
            preferences_store::load(config).0.language,
            LanguagePreference::Explicit(Language::Portuguese)
        );
    }
    drop(reopened_state);
    drop(reopened);
    state.borrow_mut().preference_store =
        Some(preferences_store::Store::start(first_path).unwrap());
    first.invoke_set_language(0);
    assert_eq!(
        state.borrow().preferences.language,
        LanguagePreference::Automatic
    );
    assert_eq!(
        state.borrow().presentation.language,
        LanguagePreference::Automatic.resolve(Some(system))
    );
    first.set_preferences_open(false);
    std::fs::write(out.join("initialization-coherence.json"),
        "{\"automatic_preserved\":true,\"distinct_preferences_preserved\":true,\"global_catalog_coherent\":true,\"initialization_writes\":0,\"explicit_choice_persisted_two_stores\":true,\"closed_and_reopened\":true,\"native_backend\":true,\"actions\":\"injected controller APIs\"}\n").unwrap();
}

/// B2 native event-loop recipe. All configuration paths and corpus data are
/// explicitly supplied laboratory artifacts, never the operator's preferences.
pub fn run_b2(
    language: Language,
    fixture: PathBuf,
    out: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::{display_preferences::Preferences, locale::LanguagePreference};
    std::fs::create_dir_all(&out)?;
    let backend = i_slint_backend_winit::Backend::builder()
        .with_renderer_name("software")
        .build()?;
    slint::platform::set_platform(Box::new(backend))?;
    let system = match language {
        Language::English => "en_GB",
        Language::French => "fr_FR",
        Language::German => "de_DE",
        Language::Spanish => "es_ES",
        Language::Italian => "it_IT",
        Language::Dutch => "nl_NL",
        Language::Portuguese => "pt_BR", // documented pt-PT fallback, injected
    };
    let start = Instant::now();
    let (window, state) = crate::new_session(Preferences::default(), Some(system.into()))?;
    let startup_ms = start.elapsed().as_secs_f64() * 1000.0;
    let (second, second_state) = crate::new_session(Preferences::default(), Some(system.into()))?;
    let legal = LegalWindow::new()?;
    state.borrow_mut().legal = Some(legal.as_weak());
    {
        let _scope = state.borrow().presentation.enter();
        crate::language_controller::legal_notices(&legal);
    }
    for component in [&window, &second] {
        component.set_product_version(env!("CARGO_PKG_VERSION").into());
        component
            .window()
            .set_size(slint::LogicalSize::new(1200.0, 760.0));
    }
    legal.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_license_text(include_str!("../../../LICENSE").into());
    fixture_volumes(&state, &window);
    let config = out.join("config/display-v1.conf");
    state.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(config.clone())?);
    window.set_collect_allocation(true);
    window.set_collect_owner(true);
    window.set_scan_path(fixture.to_str().ok_or("Unicode lab path required")?.into());
    let fixture_path = fixture.clone();
    // Record the effective bundle activation separately, after its mandatory
    // first component installation. Startup above includes construction + models.
    let start = Instant::now();
    crate::locale::activate_bundled(language)?;
    let activation_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut phase = 0;
    let mut transactions = Vec::new();
    let mut callbacks = Vec::new();
    let mut projections = Vec::new();
    let mut visible_pages = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(180);
    let timer = Timer::default();
    let weak = window.as_weak();
    let weak_second = second.as_weak();
    let weak_legal = legal.as_weak();
    let shared = state.clone();
    let second_shared = second_state.clone();
    let initial_choice = Language::ALL.iter().position(|l| *l == language).unwrap() as i32 + 1;
    let opposite_choice = if initial_choice == 2 { 1 } else { 2 };
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        let window = weak.upgrade().unwrap(); let second = weak_second.upgrade().unwrap();
        let legal = weak_legal.upgrade().unwrap();
        assert!(Instant::now() < deadline, "B2 harness timeout phase {phase}");
        let start = Instant::now(); shared.borrow_mut().poll(&window);
        callbacks.push(start.elapsed().as_secs_f64()*1000.0);
        match phase {
            0 => {
                check_initialization_coherence(&window, &shared, &second, &second_shared, system, &out);
                capture(&window, &out, "startup-1200");
            },
            1 => {
                window.invoke_start_scan();
                let generation = shared.borrow().generation;
                for choice in [2, 1, 3, 4, 5, 2] { window.invoke_set_language(choice); }
                window.invoke_set_language(opposite_choice);
                assert_eq!(shared.borrow().generation, generation);
                capture(&window, &out, "running-switched-1200");
            }
            2 if window.get_busy() => return,
            2 => {
                assert_eq!(window.get_details().row_count(), 2000);
                assert_eq!(window.get_top_rows().row_count(), 100);
                window.invoke_set_language(initial_choice);
                capture(&window, &out, "completed-initial-1200");
                window.invoke_change_page(true, true);
                window.invoke_inspect_entry(window.get_details().row_data(17).unwrap().node_id);
                window.set_detail_scroll_x(-700.0); window.set_detail_scroll_y(-102.0);
                let raw = shared.borrow().tree.as_ref().unwrap().raw_fingerprint();
                let generation = shared.borrow().generation;
                let details = window.get_details();
                let path = window.get_entry_path();
                for iteration in 0..60 {
                    let start = Instant::now();
                    window.invoke_set_language([2, 1, 3, 4, 5, 2][iteration % 6]);
                    transactions.push(start.elapsed().as_secs_f64()*1000.0);
                    assert_eq!(window.get_details(), details);
                    assert_eq!(window.get_entry_path(), path);
                    assert_eq!(window.get_detail_scroll_x(), -700.0); assert_eq!(window.get_detail_scroll_y(), -102.0);
                    assert_eq!(shared.borrow().generation, generation);
                    assert_eq!(second_shared.borrow().presentation.language, shared.borrow().presentation.language);
                }
                assert_eq!(shared.borrow().tree.as_ref().unwrap().raw_fingerprint(), raw);
                // Separate scopes quantify Rust preparation and bounded page
                // construction. They are probes, not additive transaction parts.
                for iteration in 0..40 {
                    let context = Context { language: Language::ALL[iteration as usize % 5], generation: iteration };
                    let mut s = shared.borrow_mut();
                    let tree = s.tree.as_mut().unwrap();
                    let start = Instant::now(); tree.prepare_presentation(context);
                    projections.push(start.elapsed().as_secs_f64()*1000.0);
                    let start = Instant::now(); let _page = tree.details();
                    visible_pages.push(start.elapsed().as_secs_f64()*1000.0);
                }
                window.invoke_set_language(initial_choice);
                window.window().set_size(slint::LogicalSize::new(960.0, 600.0));
                second.window().set_size(slint::LogicalSize::new(960.0, 600.0));
                second.show().unwrap(); legal.show().unwrap();
                window.set_preferences_open(true);
            }
            3 => {
                capture(&window, &out, "preferences-initial-960");
                capture(&second, &out, "second-initial-960"); capture(&legal, &out, "legal-initial");
                window.invoke_set_language(opposite_choice);
            }
            4 => {
                capture(&window, &out, "preferences-switched-960");
                capture(&second, &out, "second-switched-960"); capture(&legal, &out, "legal-switched");
                legal.set_full_license(true); window.set_preferences_open(false); window.set_top_visible(true);
            }
            5 => { capture(&window, &out, "top-switched-960"); capture(&legal, &out, "gpl");
                window.invoke_set_language(initial_choice); }
            6 => { capture(&window, &out, "top-initial-960"); window.set_top_visible(false);
                window.invoke_change_page(true, false); }
            7 => { capture(&window, &out, "table-initial-960"); window.invoke_set_language(opposite_choice); }
            8 => {
                capture(&window, &out, "table-switched-960");
                legal.hide().unwrap(); second.hide().unwrap();
                window.set_scan_path(fixture_path.join("missing").to_str().unwrap().into());
                window.invoke_start_scan();
            }
            9 if window.get_busy() => return,
            9 => { capture(&window, &out, "error-switched-960"); window.invoke_set_language(initial_choice); }
            10 => {
                capture(&window, &out, "error-initial-960");
                window.set_scan_path(fixture_path.to_str().unwrap().into()); window.invoke_start_scan(); window.invoke_cancel_scan();
                window.invoke_set_language(opposite_choice);
                assert!(window.get_cancelling());
            }
            11 if window.get_busy() => return,
            11 => { capture(&window, &out, "cancelled-switched-960"); window.invoke_set_language(initial_choice); }
            12 => {
                capture(&window, &out, "cancelled-initial-960");
                // Explicit -> Auto keeps the effective language, still persists.
                window.invoke_set_language(0);
                shared.borrow_mut().preference_store = None;
                let (preferences, status) = crate::preferences_store::load(&config);
                assert_eq!(status, 0); assert_eq!(preferences.language, LanguagePreference::Automatic);
                let (reopened, reopened_state) = crate::new_session(preferences, Some(system.into())).unwrap();
                assert_eq!(reopened_state.borrow().presentation.language, language);
                assert_eq!(reopened.get_language_choice(), 0);
                let metrics = |values: &mut Vec<f64>| {
                    values.sort_by(f64::total_cmp);
                    format!("{{\"samples\":{},\"median_ms\":{},\"p95_ms\":{},\"max_ms\":{}}}", values.len(), values[values.len()/2], values[values.len()*95/100], values.last().unwrap())
                };
                std::fs::write(out.join("b2-metrics.json"), format!(
                    "{{\"initial_language\":\"{}\",\"startup_ms\":{},\"activation_ms\":{},\"transactions\":{},\"rust_preparation\":{},\"visible_page\":{},\"callbacks\":{},\"two_windows\":true,\"legal\":true,\"reopened_auto\":true,\"source_preferences_used\":false,\"fixture\":\"50000 retained / 2000 page / Top100\"}}\n",
                    language.as_str(), startup_ms, activation_ms, metrics(&mut transactions), metrics(&mut projections), metrics(&mut visible_pages), metrics(&mut callbacks))).unwrap();
                window.hide().unwrap(); slint::quit_event_loop().unwrap();
            }
            _ => {}
        }
        phase += 1;
    });
    second.show()?;
    legal.show()?;
    window.run()?;
    timer.stop();
    Ok(())
}
