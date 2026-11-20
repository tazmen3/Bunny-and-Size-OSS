//! A presentation of the already calculated recursive measure of one folder.
//! No source access, inventory query, or summation of a paginated view occurs here.
use crate::units::Units;

/// `None` means unavailable, including a subtotal not prepared yet. In particular,
/// the live file-only projection cannot supply a directory count. A partial
/// measure is a lower bound even when its current known value happens to be zero.
pub(crate) fn render(
    logical: Option<u64>,
    files: Option<u64>,
    directories: Option<u64>,
    partial: bool,
    units: Units,
) -> String {
    let prefix = if partial { "≥ " } else { "" };
    let mut parts = Vec::with_capacity(4);
    parts.push(logical.map_or_else(
        || tr!("Unknown").to_owned(),
        |bytes| format!("{prefix}{}", units.format(bytes)),
    ));
    if let Some(files) = files {
        parts.push(format!("{prefix}{}", crate::presentation::files(files)));
    }
    if let Some(directories) = directories {
        parts.push(format!(
            "{prefix}{}",
            crate::presentation::folders(directories)
        ));
    }
    parts.push(
        if partial || logical.is_none() || files.is_none() {
            tr!("Partial")
        } else {
            tr!("Complete")
        }
        .to_owned(),
    );
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{locale::Language, presentation::Context};

    fn english() -> crate::presentation::Scope {
        Context {
            language: Language::English,
            ..Default::default()
        }
        .enter()
    }

    #[test]
    fn recursive_counts_use_the_supplied_folder_measure_without_page_limits() {
        let _scope = english();
        assert_eq!(
            render(Some(1 << 30), Some(2650), Some(297), false, Units::Binary),
            "1.0 GiB · 2,650 files · 297 folders · Complete"
        );
        assert_eq!(
            render(Some(12), Some(1), Some(0), false, Units::Binary),
            "12 bytes · 1 file · 0 folders · Complete"
        );
    }

    #[test]
    fn unknown_values_are_not_zero_and_live_directory_counts_are_omitted() {
        let _scope = english();
        assert_eq!(
            render(None, None, None, true, Units::Binary),
            "Unknown · Partial"
        );
        assert_eq!(
            render(None, Some(2), Some(1), true, Units::Binary),
            "Unknown · ≥ 2 files · ≥ 1 folder · Partial"
        );
        assert_eq!(
            render(Some(12), Some(2), None, true, Units::Binary),
            "≥ 12 bytes · ≥ 2 files · Partial"
        );
        assert_eq!(
            render(Some(0), Some(0), None, true, Units::Binary),
            "≥ 0 bytes · ≥ 0 files · Partial"
        );
        assert_eq!(
            render(Some(0), Some(0), Some(0), false, Units::Binary),
            "0 bytes · 0 files · 0 folders · Complete"
        );
        assert_eq!(
            render(None, Some(2), Some(1), false, Units::Binary),
            "Unknown · 2 files · 1 folder · Partial"
        );
    }

    #[test]
    fn configured_units_and_all_seven_languages_reuse_product_catalogues() {
        let _scope = english();
        assert_eq!(
            render(
                Some(1_000_000_000),
                Some(3),
                Some(1),
                false,
                Units::DecimalFixed(3)
            ),
            "1.0 GB · 3 files · 1 folder · Complete"
        );
        assert_eq!(
            render(Some(1), Some(3), Some(1), true, Units::BinaryFixed(3)),
            "≥ 0.0 GiB · ≥ 3 files · ≥ 1 folder · Partial"
        );
        for (language, expected) in [
            (Language::English, "1.0 GiB · 2 files · 1 folder · Complete"),
            (
                Language::French,
                "1,0 GiB · 2 fichiers · 1 dossier · Complet",
            ),
            (
                Language::German,
                "1,0 GiB · 2 Dateien · 1 Ordner · Vollständig",
            ),
            (
                Language::Spanish,
                "1,0 GiB · 2 archivos · 1 carpeta · Completo",
            ),
            (
                Language::Portuguese,
                "1,0 GiB · 2 ficheiros · 1 pasta · Completo",
            ),
            (
                Language::Italian,
                "1,0 GiB · 2 file · 1 cartella · Completo",
            ),
            (Language::Dutch, "1,0 GiB · 2 bestanden · 1 map · Volledig"),
        ] {
            let _scope = Context {
                language,
                ..Default::default()
            }
            .enter();
            assert_eq!(
                render(Some(1 << 30), Some(2), Some(1), false, Units::Binary),
                expected
            );
        }
    }
}
