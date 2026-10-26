//! Language policy for presentation. No scan, path, timestamp or export state.
//! Five complete product catalogs; Portuguese deliberately uses pt-PT.

/// Product message languages; source English and four bundled catalogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    French,
    German,
    Spanish,
    /// Initial Portuguese catalog: European Portuguese (pt-PT).
    Portuguese,
}
impl Language {
    pub const ALL: [Self; 5] = [
        Self::English,
        Self::French,
        Self::German,
        Self::Spanish,
        Self::Portuguese,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::French => "fr",
            Self::German => "de",
            Self::Spanish => "es",
            Self::Portuguese => "pt",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LanguagePreference {
    #[default]
    Automatic,
    Explicit(Language),
}
impl LanguagePreference {
    /// Stable configuration tokens. These are not display labels or OS locales.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Automatic),
            value => Language::ALL
                .into_iter()
                .find(|language| language.as_str() == value)
                .map(Self::Explicit),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "auto",
            Self::Explicit(language) => language.as_str(),
        }
    }
    pub fn resolve(self, system_locale: Option<&str>) -> Language {
        match self {
            Self::Explicit(language) => language,
            Self::Automatic => system_locale
                .and_then(normalize)
                .unwrap_or(Language::English),
        }
    }
    /// Offline OS query, injectable through `resolve` in tests. No environment mutation.
    pub fn resolve_system(self) -> Language {
        self.resolve(sys_locale::get_locale().as_deref())
    }
}

/// Recognize the five base languages, preserving no formatting policy.
/// Accept POSIX encoding/modifier suffixes and common BCP-47 regional spellings.
/// All Portuguese regions deliberately resolve to the single pt-PT catalog.
pub fn normalize(locale: &str) -> Option<Language> {
    let locale = locale.trim();
    let (tag, suffix) = locale.split_once(['.', '@']).unwrap_or((locale, ""));
    if locale.len() != tag.len()
        && (suffix.is_empty()
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_@".contains(&b)))
    {
        return None;
    }
    let mut parts = tag.split(['-', '_']);
    let base = parts.next()?;
    if base.len() != 2 || !base.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    // Validate the common script/region/variant grammar rather than accepting
    // arbitrary text after a known prefix (e.g. fr-??? or en--US).
    let mut script_seen = false;
    let mut region_seen = false;
    let mut variant_seen = false;
    for part in parts {
        let alpha = part.bytes().all(|b| b.is_ascii_alphabetic());
        let digits = part.bytes().all(|b| b.is_ascii_digit());
        if !script_seen && !region_seen && !variant_seen && part.len() == 4 && alpha {
            script_seen = true;
        } else if !region_seen
            && !variant_seen
            && ((part.len() == 2 && alpha) || (part.len() == 3 && digits))
        {
            region_seen = true;
        } else if ((5..=8).contains(&part.len())
            || (part.len() == 4 && part.as_bytes()[0].is_ascii_digit()))
            && part.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            variant_seen = true;
        } else {
            return None;
        }
    }
    Language::ALL
        .into_iter()
        .find(|language| base.eq_ignore_ascii_case(language.as_str()))
}

/// Future UI notification payload. Apply on the UI thread; consumers must
/// retranslate Rust models and open dialogs together with Slint bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocaleChange {
    pub previous: Language,
    pub effective: Language,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocaleState {
    pub preference: LanguagePreference,
    pub effective: Language,
}
impl LocaleState {
    pub fn new(preference: LanguagePreference, system_locale: Option<&str>) -> Self {
        Self {
            preference,
            effective: preference.resolve(system_locale),
        }
    }
    /// Pure notification preparation. Does not activate catalogs or schedule work.
    pub fn update(
        &mut self,
        preference: LanguagePreference,
        system_locale: Option<&str>,
    ) -> Option<LocaleChange> {
        let previous = self.effective;
        *self = Self::new(preference, system_locale);
        (previous != self.effective).then_some(LocaleChange {
            previous,
            effective: self.effective,
        })
    }
}

/// The actual activated catalog can differ from the requested language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    pub effective: Language,
    pub unavailable_catalog: Option<Language>,
}
/// Call only on the Slint UI thread, after instantiating the first *bundled*
/// component. An absent bundle is an error, not proof of an English UI.
/// A missing message inside an available catalog uses Slint's source fallback
/// and does not cause LanguageNotFound (proved by the isolated pilot).
pub fn activate_bundled(
    language: Language,
) -> Result<Activation, slint::SelectBundledTranslationError> {
    match slint::select_bundled_translation(language.as_str()) {
        Ok(()) => Ok(Activation {
            effective: language,
            unavailable_catalog: None,
        }),
        Err(slint::SelectBundledTranslationError::LanguageNotFound { .. }) => {
            slint::select_bundled_translation("en")?;
            Ok(Activation {
                effective: Language::English,
                unavailable_catalog: Some(language),
            })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn regional_posix_and_invalid_locales() {
        for (text, language) in [
            ("fr_FR", Language::French),
            ("fr-FR", Language::French),
            ("fr_FR.UTF-8@euro", Language::French),
            ("FR-ca", Language::French),
            ("pt_PT", Language::Portuguese),
            ("pt-BR", Language::Portuguese),
            ("pt_BR", Language::Portuguese),
            ("en_US", Language::English),
            ("de_DE", Language::German),
            ("es_ES", Language::Spanish),
            ("es-419", Language::Spanish),
            ("en-Latn-GB", Language::English),
        ] {
            assert_eq!(normalize(text), Some(language), "{text}");
        }
        for invalid in [
            "",
            "C",
            "C.UTF-8",
            "POSIX",
            "it_IT",
            "zh-Hant-TW",
            "fr-???",
            "en--US",
            "en-",
            "english",
            "fr_1234garbage",
            "fr.",
            "fr@",
            "fr.utf 8",
            "fr_US/../../",
            "éé-FR",
        ] {
            assert_eq!(normalize(invalid), None, "{invalid}");
            assert_eq!(
                LanguagePreference::Automatic.resolve(Some(invalid)),
                Language::English
            );
        }
        assert_eq!(
            LanguagePreference::Automatic.resolve(None),
            Language::English
        );
    }
    #[test]
    fn explicit_precedence_return_to_auto_and_notifications() {
        for language in Language::ALL {
            let preference = LanguagePreference::Explicit(language);
            assert_eq!(
                LanguagePreference::parse(preference.as_str()),
                Some(preference)
            );
            for system in [None, Some("fr_FR"), Some("bad")] {
                assert_eq!(preference.resolve(system), language);
            }
        }
        for invalid in ["Automatic", "FR", "fr-FR", "", "xx"] {
            assert_eq!(LanguagePreference::parse(invalid), None);
        }
        let mut state = LocaleState::new(
            LanguagePreference::Explicit(Language::German),
            Some("fr_FR"),
        );
        let change = state
            .update(LanguagePreference::Automatic, Some("fr_FR"))
            .unwrap();
        assert_eq!(
            change,
            LocaleChange {
                previous: Language::German,
                effective: Language::French
            }
        );
        assert_eq!(state.preference, LanguagePreference::Automatic);
        assert!(
            state
                .update(LanguagePreference::Automatic, Some("fr-CA"))
                .is_none()
        );
        assert_eq!(
            state
                .update(LanguagePreference::Automatic, Some("ja_JP"))
                .unwrap()
                .effective,
            Language::English
        );
    }
}
