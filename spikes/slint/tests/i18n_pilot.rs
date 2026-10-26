//! Separate test executable: Slint's first component owns the bundled catalog.
use slint::ComponentHandle;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use sortsize_slint_spike::locale::{Language, LanguagePreference, LocaleState, activate_bundled};
use std::rc::Rc;
include!(concat!(env!("OUT_DIR"), "/i18n_pilot.rs"));
struct Platform;
impl slint::platform::Platform for Platform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer))
    }
}
#[test]
fn bundled_hot_switch_missing_catalog_message_plural_and_auto() {
    assert!(matches!(
        slint::select_bundled_translation("fr"),
        Err(slint::SelectBundledTranslationError::NoTranslationsBundled)
    ));
    slint::platform::set_platform(Box::new(Platform)).unwrap();
    let window = I18nPilot::new().unwrap();
    // A second already-open component must also be invalidated.
    let dialog = I18nPilot::new().unwrap();
    let rust_label = |language| match language {
        Language::French => "Prêt",
        _ => "Ready",
    };
    let mut state = LocaleState::new(
        LanguagePreference::Explicit(Language::English),
        Some("fr_FR"),
    );
    activate_bundled(state.effective).unwrap();
    window.set_rust_label(rust_label(state.effective).into());
    dialog.set_rust_label(rust_label(state.effective).into());
    assert_eq!(window.get_rust_label(), "Ready");
    assert_eq!(window.get_greeting(), "Hello, Ada!");
    assert_eq!(window.get_default_context(), "Default context");
    state
        .update(LanguagePreference::Automatic, Some("fr_FR"))
        .unwrap();
    assert_eq!(
        activate_bundled(state.effective)
            .unwrap()
            .unavailable_catalog,
        None
    );
    // Slint does not translate strings assigned from Rust. A real locale
    // transaction must explicitly rebuild these for every open component.
    assert_eq!(window.get_rust_label(), "Ready");
    window.set_rust_label(rust_label(state.effective).into());
    dialog.set_rust_label(rust_label(state.effective).into());
    assert_eq!(window.get_rust_label(), "Prêt");
    assert_eq!(dialog.get_rust_label(), "Prêt");
    assert_eq!(window.get_greeting(), "Bonjour, Ada !");
    assert_eq!(dialog.get_greeting(), "Bonjour, Ada !");
    assert_eq!(window.get_default_context(), "Contexte par défaut");
    assert_eq!(window.get_missing(), "Source fallback");
    window.set_person("Éva".into());
    assert_eq!(window.get_greeting(), "Bonjour, Éva !");
    for (n, expected) in [
        (0, "0 fichier"),
        (1, "1 fichier"),
        (2, "2 fichiers"),
        (7, "7 fichiers"),
    ] {
        window.set_count(n);
        assert_eq!(window.get_files(), expected);
    }
    // Failed raw selection leaves the previous catalog active in Slint 1.18.1.
    assert!(matches!(
        slint::select_bundled_translation("de"),
        Err(slint::SelectBundledTranslationError::LanguageNotFound { .. })
    ));
    assert_eq!(window.get_greeting(), "Bonjour, Éva !");
    let activation = activate_bundled(Language::German).unwrap();
    assert_eq!(activation.effective, Language::English);
    assert_eq!(activation.unavailable_catalog, Some(Language::German));
    assert_eq!(window.get_greeting(), "Hello, Éva!");
    assert_eq!(dialog.get_greeting(), "Hello, Ada!");
    for (n, expected) in [
        (0, "0 files"),
        (1, "1 file"),
        (2, "2 files"),
        (7, "7 files"),
    ] {
        window.set_count(n);
        assert_eq!(window.get_files(), expected);
    }
    // Auto is resolved again by our policy; Slint's empty string selects source,
    // not automatic system detection.
    state.update(
        LanguagePreference::Explicit(Language::Spanish),
        Some("fr_FR"),
    );
    state
        .update(LanguagePreference::Automatic, Some("fr_FR"))
        .unwrap();
    activate_bundled(state.effective).unwrap();
    assert_eq!(window.get_greeting(), "Bonjour, Éva !");
    slint::select_bundled_translation("").unwrap();
    assert_eq!(window.get_greeting(), "Hello, Éva!");
    window.hide().unwrap();
}
