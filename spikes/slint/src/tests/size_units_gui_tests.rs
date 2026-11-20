//! VIEW-009: forced display units persist independently of the byte base.
use super::*;

#[test]
fn win02_french_startup_renders_automatic_or_recorded_giga_without_rewriting() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("display-v1.conf");
    for (recorded, expected) in [
        (None, 0),
        (Some("version=1\nunits=binary\nhidden_columns=\n"), 0),
        (
            Some("version=2\nunits=binary\nhidden_columns=\nlanguage=fr\n"),
            0,
        ),
        (
            Some("version=2\nunits=binary\nhidden_columns=\nlanguage=fr\nsize_unit=giga\n"),
            3,
        ),
    ] {
        if let Some(text) = recorded {
            fs::write(&path, text).unwrap();
        }
        let before = fs::read(&path).ok();
        let (preferences, status) = preferences_store::load(&path);
        assert_eq!(status, 0);
        let (window, controller) = new_session(preferences, Some("fr_FR".into())).unwrap();
        controller.borrow_mut().preference_store =
            Some(preferences_store::Store::start(path.clone()).unwrap());
        window.invoke_activate_ribbon(2);
        window.show().unwrap();
        adapter.set_size(slint::PhysicalSize::new(1200, 760));
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1200);
        }));
        assert!(window.get_display_controls_visible());
        assert_eq!(window.get_size_unit_choice(), expected);
        assert_eq!(window.get_size_unit_selector_index(), expected);
        assert_eq!(
            controller.borrow().presentation.language,
            locale::Language::French
        );
        assert_eq!(
            controller
                .borrow()
                .preference_store
                .as_ref()
                .unwrap()
                .submissions(),
            0,
            "opening Display must not save an implicit unit choice"
        );
        window.hide().unwrap();
        controller.borrow_mut().preference_store = None;
        assert_eq!(fs::read(&path).ok(), before);
    }
}

#[test]
fn forced_size_unit_choice_persists_reopens_and_survives_base_changes() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("display-v1.conf");
    let (window, controller) = new_session(Default::default(), Some("en".into())).unwrap();
    assert_eq!(window.get_size_unit_choice(), 0);
    controller.borrow_mut().preference_store =
        Some(preferences_store::Store::start(path.clone()).unwrap());
    for choice in 0..=4 {
        window.invoke_set_size_unit(choice);
        assert_eq!(window.get_size_unit_choice(), choice);
        for decimal in [true, false] {
            window.invoke_set_units(decimal);
            assert_eq!(window.get_size_unit_choice(), choice);
            assert_eq!(window.get_decimal_units(), decimal);
            assert_eq!(
                controller.borrow().preferences.units.size_unit(),
                choice as u8
            );
        }
    }
    for invalid in [-1, 5, 256] {
        window.invoke_set_size_unit(invalid);
        assert_eq!(window.get_size_unit_choice(), 4);
    }
    controller.borrow_mut().preference_store = None; // Flush worker before reopening.
    let (loaded, status) = preferences_store::load(&path);
    assert_eq!(status, 0);
    assert_eq!(loaded.units, units::Units::BinaryFixed(4));
    let bytes = fs::read(&path).unwrap();
    let (restart, state) = new_session(loaded, Some("en".into())).unwrap();
    assert_eq!(restart.get_size_unit_choice(), 4);
    assert!(!restart.get_decimal_units());
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "startup must not rewrite preferences"
    );
    restart.invoke_reset_display();
    assert_eq!(restart.get_size_unit_choice(), 0);
    assert_eq!(state.borrow().preferences.units, units::Units::Binary);
}
