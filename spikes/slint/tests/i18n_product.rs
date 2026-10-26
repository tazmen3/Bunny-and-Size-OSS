//! Isolated product process: the first component installs the real product bundle.
use slint::Model;
use sortsize_slint_spike::{
    LegalWindow, SortSizeWindow,
    locale::{Language, activate_bundled},
};
use std::rc::Rc;
struct Platform;
impl slint::platform::Platform for Platform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(
            slint::platform::software_renderer::MinimalSoftwareWindow::new(
                slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
            ),
        )
    }
}
#[test]
fn product_bundle_source_french_multiple_windows_and_missing_catalog() {
    assert!(matches!(
        activate_bundled(Language::French),
        Err(slint::SelectBundledTranslationError::NoTranslationsBundled)
    ));
    slint::platform::set_platform(Box::new(Platform)).unwrap();
    // B1 product constructor pins French without reading/mutating preferences.
    let first = sortsize_slint_spike::new_window().unwrap();
    let second = SortSizeWindow::new().unwrap();
    let legal = LegalWindow::new().unwrap();
    assert_eq!(first.get_status(), "Choisis un dossier à analyser.");
    assert_eq!(second.get_status(), "Choisis un dossier à analyser.");
    assert_eq!(
        legal.get_localized_title(),
        "Bunny and Size — À propos / Licences"
    );
    activate_bundled(Language::English).unwrap();
    assert_eq!(first.get_status(), "Choose a folder to scan.");
    assert_eq!(second.get_status(), "Choose a folder to scan.");
    assert_eq!(first.get_column_labels().row_data(3).unwrap(), "Owner");
    assert_eq!(
        legal.get_localized_title(),
        "Bunny and Size — About / Licenses"
    );
    activate_bundled(Language::French).unwrap();
    for (language, status, owner) in [
        (
            Language::German,
            "Ordner zum Analysieren auswählen.",
            "Eigentümer",
        ),
        (
            Language::Spanish,
            "Elige una carpeta para analizar.",
            "Propietario",
        ),
        (
            Language::Portuguese,
            "Escolhe uma pasta para analisar.",
            "Proprietário",
        ),
    ] {
        let activation = activate_bundled(language).unwrap();
        assert_eq!(activation.effective, language);
        assert_eq!(activation.unavailable_catalog, None);
        assert_eq!(first.get_status(), status);
        assert_eq!(second.get_status(), status);
        assert_eq!(first.get_column_labels().row_data(3).unwrap(), owner);
    }
    activate_bundled(Language::French).unwrap();
    assert_eq!(first.get_status(), "Choisis un dossier à analyser.");
    assert_eq!(second.get_status(), "Choisis un dossier à analyser.");
}
