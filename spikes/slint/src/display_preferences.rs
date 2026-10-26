//! Stable presentation schema. No paths, scan data or network consent.
use crate::{locale::LanguagePreference, units::Units};

pub(crate) const COLUMN_IDS: [&str; 11] = [
    "name",
    "logical_size",
    "allocated_size",
    "owner",
    "folder_share",
    "recursive_files",
    "recursive_directories",
    "kind",
    "modified",
    "accessed",
    "full_path",
];
pub(crate) const WIDTHS: [u16; 11] = [200, 104, 168, 220, 86, 64, 64, 92, 270, 270, 346];
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preferences {
    pub units: Units,
    pub language: LanguagePreference,
    pub visible: [bool; 11],
    pub sidebar_ratio: u16,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            units: Units::Binary,
            language: LanguagePreference::Automatic,
            visible: [true; 11],
            sidebar_ratio: 550,
        }
    }
}
impl Preferences {
    pub fn set_column(&mut self, index: usize, visible: bool) {
        if index > 0 && index < self.visible.len() {
            self.visible[index] = visible;
        }
    }
    pub fn width(&self) -> u16 {
        let mut width = 2;
        let mut count = 0;
        for (index, size) in WIDTHS.iter().enumerate() {
            if index == 0 || self.visible[index] {
                width += size;
                count += 1;
            }
        }
        width + (count - 1) * 6
    }
    pub fn encode(&self) -> String {
        let hidden = COLUMN_IDS
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && !self.visible[*i])
            .map(|(_, id)| *id)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "version=2\nunits={}\nhidden_columns={hidden}\nlanguage={}\nsidebar_ratio={}\n",
            if self.units == Units::Binary {
                "binary"
            } else {
                "decimal"
            },
            self.language.as_str(),
            self.sidebar_ratio.clamp(100, 900)
        )
    }
    pub fn decode(text: &str) -> Option<Self> {
        let mut result = Self::default();
        let mut version = None;
        let mut language = None;
        let mut seen = std::collections::HashSet::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let (key, value) = line.split_once('=')?;
            if !seen.insert(key) {
                return None;
            }
            match key {
                "version" => version = Some(value),
                "units" => {
                    result.units = if value == "decimal" {
                        Units::Decimal
                    } else {
                        Units::Binary
                    }
                }
                "language" => language = Some(value),
                "sidebar_ratio" => {
                    // Additive v2 key: missing/invalid values keep the historic layout default.
                    result.sidebar_ratio = value
                        .parse::<u16>()
                        .ok()
                        .filter(|n| (100..=900).contains(n))
                        .unwrap_or(550);
                }
                "hidden_columns" => {
                    for id in value.split(',') {
                        if let Some(i) = COLUMN_IDS.iter().position(|known| *known == id) {
                            result.set_column(i, false);
                        }
                    }
                }
                _ => {}
            }
        }
        match version {
            Some("1") => {
                result.sidebar_ratio = 550;
                Some(result)
            }
            Some("2")
                if ["units", "hidden_columns", "language"]
                    .iter()
                    .all(|key| seen.contains(key)) =>
            {
                result.language = language
                    .and_then(LanguagePreference::parse)
                    .unwrap_or_default();
                Some(result)
            }
            _ => None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_visibility_width_reset_and_schema_evolution() {
        let mut p = Preferences::default();
        assert_eq!(p.width(), 1946);
        for i in 0..11 {
            p.set_column(i, false);
        }
        assert!(p.visible[0]);
        assert_eq!(p.width(), 202);
        p.set_column(1, true);
        assert_eq!(p.width(), 312);
        p.units = Units::Decimal;
        assert_eq!(Preferences::decode(&p.encode()), Some(p));
        let old = Preferences::decode(
            "version=1\nunits=unknown\nhidden_columns=name,deleted,owner\nfuture=value\n",
        )
        .unwrap();
        assert!(old.visible[0]);
        assert!(!old.visible[3]);
        assert!(old.visible[10]);
        assert_eq!(old.units, Units::Binary);
        for bad in [
            "broken",
            "version=2",
            "version=1\nversion=1",
            "units=binary",
        ] {
            assert!(Preferences::decode(bad).is_none());
        }
        assert_eq!(Preferences::default().width(), 1946);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::locale::Language;

    #[test]
    fn every_v1_column_combination_and_unit_survives_v2_and_all_languages() {
        // Name was always forced visible; exhaust all 10 mutable flags.
        for units in [Units::Binary, Units::Decimal] {
            for mask in 0..1024 {
                let mut expected = Preferences {
                    units,
                    ..Default::default()
                };
                for index in 1..11 {
                    expected.set_column(index, mask & (1 << (index - 1)) != 0);
                }
                let encoded = expected.encode();
                let legacy = encoded
                    .replace("version=2", "version=1")
                    .replace("language=auto\n", "");
                let migrated = Preferences::decode(&legacy).unwrap();
                assert_eq!(migrated, expected);
                for choice in std::iter::once(LanguagePreference::Automatic)
                    .chain(Language::ALL.map(LanguagePreference::Explicit))
                {
                    let selected = Preferences {
                        language: choice,
                        ..migrated.clone()
                    };
                    assert_eq!(Preferences::decode(&selected.encode()), Some(selected));
                }
            }
        }
    }
    #[test]
    fn invalid_choices_future_versions_duplicates_and_truncated_files() {
        let valid =
            "version=2\nunits=decimal\nhidden_columns=owner,modified,full_path\nlanguage=de\n";
        for invalid in ["FR", "fr_FR", "unknown", ""] {
            let p =
                Preferences::decode(&valid.replace("language=de", &format!("language={invalid}")))
                    .unwrap();
            assert_eq!(p.language, LanguagePreference::Automatic);
            assert_eq!(p.units, Units::Decimal);
            assert!(!p.visible[3] && !p.visible[8] && !p.visible[10]);
        }
        for invalid in [
            valid.replace("version=2", "version=3"),
            valid.replace("version=2", "version=999"),
            valid.replace("language=de\n", ""),
            valid.replace("hidden_columns=owner,modified,full_path\n", ""),
            valid.replace("units=decimal\n", ""),
            format!("{valid}language=fr\n"),
            format!("{valid}units=binary\n"),
            format!("{valid}hidden_columns=\n"),
            format!("{valid}version=2\n"),
            format!("{valid}future=x\nfuture=y\n"),
            format!("{valid}broken"),
        ] {
            assert!(Preferences::decode(&invalid).is_none(), "{invalid}");
        }
        let crlf = Preferences::decode(&valid.replace('\n', "\r\n")).unwrap();
        assert_eq!(
            crlf.language,
            LanguagePreference::Explicit(Language::German)
        );
        assert!(Preferences::decode(&(valid.to_owned() + "future=value\n")).is_some());
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;
    #[test]
    fn additive_v2_ratio_preserves_v1_v2_and_rejects_invalid_values() {
        let legacy = "version=1\nunits=decimal\nhidden_columns=owner\n";
        let v2 = "version=2\nunits=decimal\nhidden_columns=owner\nlanguage=de\n";
        for text in [legacy, v2] {
            assert_eq!(Preferences::decode(text).unwrap().sidebar_ratio, 550);
        }
        for value in ["0", "99", "901", "65536", "-1", "NaN", ""] {
            let decoded = Preferences::decode(&format!("{v2}sidebar_ratio={value}\n")).unwrap();
            assert_eq!(decoded.sidebar_ratio, 550);
            assert_eq!(decoded.units, Units::Decimal);
            assert!(!decoded.visible[3]);
        }
        for ratio in [100, 375, 550, 900] {
            let p = Preferences {
                sidebar_ratio: ratio,
                ..Default::default()
            };
            assert_eq!(Preferences::decode(&p.encode()), Some(p));
        }
    }
}
