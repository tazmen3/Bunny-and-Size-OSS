//! Stable preferences schema. Scan data and network consent stay separate.
use crate::{locale::LanguagePreference, units::Units};
use sortsize_store::inventory::queries::duplicates::DuplicateFileProfile;

pub(crate) fn profile_index(profile: DuplicateFileProfile) -> i32 {
    match profile {
        DuplicateFileProfile::General => 0,
        DuplicateFileProfile::Optimized => 1,
        DuplicateFileProfile::LargeFiles | DuplicateFileProfile::Exhaustive => 2,
    }
}

fn normal_profile(index: i32) -> DuplicateFileProfile {
    match index {
        0 => DuplicateFileProfile::General,
        1 => DuplicateFileProfile::Optimized,
        _ => DuplicateFileProfile::LargeFiles,
    }
}

pub(crate) fn effective_profile(index: i32, exhaustive: bool) -> DuplicateFileProfile {
    if exhaustive {
        DuplicateFileProfile::Exhaustive
    } else {
        normal_profile(index)
    }
}

impl crate::Controller {
    pub(crate) fn change_duplicate_profile(&mut self, window: &crate::SortSizeWindow, index: i32) {
        let profile = normal_profile(index);
        if profile != self.preferences.duplicate_profile {
            self.preferences.duplicate_profile = profile;
            window.set_duplicate_profile_choice(profile_index(profile));
            self.save_preferences(window);
        }
    }
}

pub(crate) fn profile_counts(
    profile: DuplicateFileProfile,
    examined: u128,
    below: u128,
    zero: u128,
) -> String {
    use crate::number_format::integer;
    let profile = match profile {
        DuplicateFileProfile::General => tr!("General search (256 KiB)"),
        DuplicateFileProfile::Optimized => tr!("Optimized search (1 MiB)"),
        DuplicateFileProfile::LargeFiles => tr!("Large files only (10 MiB)"),
        DuplicateFileProfile::Exhaustive => tr!("Exhaustive search"),
    };
    tr_format!(
        "{} · Files examined: {} · Below threshold: {} · Empty files excluded: {}.",
        profile,
        integer(examined),
        integer(below),
        integer(zero),
    )
}

#[cfg(any(target_os = "linux", windows))]
pub(crate) const MIB: u64 = 1024 * 1024;

#[cfg(any(target_os = "linux", windows))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DuplicateResources {
    pub memory_limit: Option<u64>,
    pub temporary_limit: Option<u64>,
    pub temporary_parent: Option<std::path::PathBuf>,
}

#[cfg(any(target_os = "linux", windows))]
impl DuplicateResources {
    fn encode(&self) -> String {
        use std::fmt::Write;
        #[cfg(windows)]
        use std::os::windows::ffi::OsStrExt;

        let mut value = format!(
            "duplicate_memory_bytes={}\nduplicate_temporary_bytes={}\n{}=",
            self.memory_limit.unwrap_or(0),
            self.temporary_limit.unwrap_or(0),
            Self::PARENT_KEY,
        );
        if let Some(parent) = &self.temporary_parent {
            #[cfg(target_os = "linux")]
            for byte in parent.as_os_str().as_encoded_bytes() {
                write!(value, "{byte:02x}").expect("writing to a String");
            }
            #[cfg(windows)]
            for unit in parent.as_os_str().encode_wide() {
                write!(value, "{unit:04x}").expect("writing to a String");
            }
        }
        value.push('\n');
        value
    }

    #[cfg(windows)]
    const PARENT_KEY: &'static str = "duplicate_temporary_parent_utf16";
    #[cfg(target_os = "linux")]
    const PARENT_KEY: &'static str = "duplicate_temporary_parent_unix_hex";

    #[cfg(target_os = "linux")]
    fn decode_parent(value: &str) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;

        if value.is_empty() || !value.is_ascii() || !value.len().is_multiple_of(2) {
            return None;
        }
        let bytes = (0..value.len())
            .step_by(2)
            .map(|first| u8::from_str_radix(&value[first..first + 2], 16).ok())
            .collect::<Option<Vec<_>>>()?;
        if bytes.contains(&0) {
            return None;
        }
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
        path.is_absolute().then_some(path)
    }

    #[cfg(windows)]
    fn decode_parent(value: &str) -> Option<std::path::PathBuf> {
        use std::os::windows::ffi::OsStringExt;

        if value.is_empty() || !value.is_ascii() || !value.len().is_multiple_of(4) {
            return None;
        }
        let units = (0..value.len())
            .step_by(4)
            .map(|first| u16::from_str_radix(&value[first..first + 4], 16).ok())
            .collect::<Option<Vec<_>>>()?;
        if units.contains(&0) {
            return None;
        }
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&units));
        path.is_absolute().then_some(path)
    }
}

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
// Two complete 200-row publications and Top100 must fit at the GUI floor.
pub(crate) const MIN_MEMORY_MIB: u64 = 128;
pub(crate) fn memory_limit_bytes(mib: u64) -> Option<u64> {
    (mib >= MIN_MEMORY_MIB)
        .then_some(mib)?
        .checked_mul(1024 * 1024)
}

pub(crate) const WIDTHS: [u16; 11] = [200, 104, 168, 220, 86, 64, 64, 92, 270, 270, 346];
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preferences {
    pub units: Units,
    pub language: LanguagePreference,
    pub visible: [bool; 11],
    pub sidebar_ratio: u16,
    pub theme: u8,
    pub sidebar_width: u16,
    pub sidebar_collapsed: bool,
    pub treemap_mode: u8,
    pub treemap_height: u16,
    pub detail_sort: u8,
    pub dense: bool,
    pub memory: sortsize_core::inventory_contract::MemoryMode,
    pub memory_bytes: Option<u64>,
    pub duplicate_profile: DuplicateFileProfile,
    #[cfg(any(target_os = "linux", windows))]
    pub duplicate_resources: DuplicateResources,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            units: Units::Binary,
            language: LanguagePreference::Automatic,
            visible: [true; 11],
            sidebar_ratio: 550,
            theme: 0,
            sidebar_width: 300,
            sidebar_collapsed: false,
            treemap_mode: 0,
            treemap_height: 0,
            detail_sort: 0,
            dense: false,
            memory: Default::default(),
            memory_bytes: None,
            duplicate_profile: DuplicateFileProfile::default(),
            #[cfg(any(target_os = "linux", windows))]
            duplicate_resources: DuplicateResources::default(),
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
        let text = format!(
            "version=2\nunits={}\nhidden_columns={hidden}\nlanguage={}\nsidebar_ratio={}\ntheme={}\ndensity={}\nmemory_mode={}\nsidebar_width={}\nsidebar_collapsed={}\ntreemap_mode={}\ntreemap_height={}\ndetail_sort={}\nsize_unit={}\nmemory_bytes={}\nduplicate_profile={}\n",
            if !self.units.is_decimal() {
                "binary"
            } else {
                "decimal"
            },
            self.language.as_str(),
            self.sidebar_ratio.clamp(100, 900),
            ["system", "light", "dark"][usize::from(self.theme.min(2))],
            if self.dense { "compact" } else { "normal" },
            self.memory.key(),
            self.sidebar_width.clamp(220, 600),
            self.sidebar_collapsed,
            if self.treemap_mode == 1 {
                "bands"
            } else {
                "mosaic"
            },
            self.treemap_height,
            if self.detail_sort < 12 {
                self.detail_sort
            } else {
                0
            },
            ["auto", "kilo", "mega", "giga", "tera"][usize::from(self.units.size_unit())],
            self.memory_bytes.unwrap_or(0),
            ["general", "optimized", "large"][profile_index(self.duplicate_profile) as usize]
        );
        #[cfg(any(target_os = "linux", windows))]
        let text = text + &self.duplicate_resources.encode();
        text
    }
    pub fn decode(text: &str) -> Option<Self> {
        let mut result = Self::default();
        let mut version = None;
        let mut language = None;
        let mut size_unit = 0;
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
                "size_unit" => {
                    size_unit = match value {
                        "kilo" => 1,
                        "mega" => 2,
                        "giga" => 3,
                        "tera" => 4,
                        _ => 0,
                    };
                }
                "language" => language = Some(value),
                "theme" => {
                    result.theme = match value {
                        "light" => 1,
                        "dark" => 2,
                        _ => 0,
                    }
                }
                "duplicate_profile" => {
                    result.duplicate_profile = match value {
                        "general" => DuplicateFileProfile::General,
                        "optimized" => DuplicateFileProfile::Optimized,
                        _ => DuplicateFileProfile::LargeFiles,
                    }
                }
                "density" => result.dense = value == "compact",
                "memory_bytes" => {
                    result.memory_bytes = value
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n >= MIN_MEMORY_MIB * 1024 * 1024);
                }
                #[cfg(any(target_os = "linux", windows))]
                "duplicate_memory_bytes" => {
                    result.duplicate_resources.memory_limit = duplicate_limit(value);
                }
                #[cfg(any(target_os = "linux", windows))]
                "duplicate_temporary_bytes" => {
                    result.duplicate_resources.temporary_limit = duplicate_limit(value);
                }
                #[cfg(any(target_os = "linux", windows))]
                DuplicateResources::PARENT_KEY => {
                    result.duplicate_resources.temporary_parent =
                        DuplicateResources::decode_parent(value);
                }
                "memory_mode" => {
                    result.memory = sortsize_core::inventory_contract::MemoryMode::from_key(value)
                        .unwrap_or_default()
                }
                "sidebar_width" => {
                    result.sidebar_width = value
                        .parse::<u16>()
                        .ok()
                        .filter(|v| (220..=600).contains(v))
                        .unwrap_or(300)
                }
                "sidebar_collapsed" => result.sidebar_collapsed = value == "true",
                "treemap_mode" => result.treemap_mode = u8::from(value == "bands"),
                "treemap_height" => {
                    result.treemap_height = value
                        .parse()
                        .ok()
                        .filter(|height| *height >= 80)
                        .unwrap_or(0);
                }
                "detail_sort" => {
                    result.detail_sort = value
                        .parse::<u8>()
                        .ok()
                        .filter(|index| *index < 12)
                        .unwrap_or(0);
                }
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
        result.units = result.units.with_size_unit(size_unit);
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

#[cfg(any(target_os = "linux", windows))]
fn duplicate_limit(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n >= MIB && n.is_multiple_of(MIB))
}

#[cfg(test)]
mod finish_tests {
    use super::*;
    #[test]
    fn optional_treemap_height_preserves_legacy_and_rejects_invalid_values() {
        let legacy = "version=1\nunits=binary\nhidden_columns=\n";
        let v2 = "version=2\nunits=binary\nhidden_columns=\nlanguage=fr\n";
        for text in [legacy, v2] {
            assert_eq!(Preferences::decode(text).unwrap().treemap_height, 0);
        }
        for value in ["", "auto", "-1", "79", "65536"] {
            let text = format!("{v2}treemap_height={value}\n");
            assert_eq!(Preferences::decode(&text).unwrap().treemap_height, 0);
        }
        let preferences = Preferences {
            treemap_height: 360,
            ..Default::default()
        };
        assert_eq!(
            Preferences::decode(&preferences.encode()),
            Some(preferences)
        );
    }

    #[test]
    fn additive_layout_keys_keep_legacy_defaults_and_roundtrip_independently() {
        let old = "version=2\nunits=binary\nhidden_columns=owner\nlanguage=fr\nsidebar_ratio=375\n";
        let p = Preferences::decode(old).unwrap();
        assert_eq!(
            (p.sidebar_width, p.sidebar_collapsed, p.treemap_mode),
            (300, false, 0)
        );
        for width in [220, 300, 600] {
            for mode in [0, 1] {
                for collapsed in [false, true] {
                    let mut changed = p.clone();
                    changed.sidebar_width = width;
                    changed.treemap_mode = mode;
                    changed.sidebar_collapsed = collapsed;
                    assert_eq!(Preferences::decode(&changed.encode()), Some(changed));
                }
            }
        }
        let invalid = Preferences::decode(
            &(old.to_owned()
                + "sidebar_width=65535\ntreemap_mode=future\nsidebar_collapsed=invalid\n"),
        )
        .unwrap();
        assert_eq!(
            (
                invalid.sidebar_width,
                invalid.sidebar_collapsed,
                invalid.treemap_mode
            ),
            (300, false, 0)
        );
        assert_eq!(invalid.sidebar_ratio, 375);
    }
}
#[cfg(test)]
mod detail_sort_tests {
    use super::*;

    #[test]
    fn additive_detail_sort_defaults_legacy_files_and_roundtrips_every_choice() {
        let legacy = "version=1\nunits=decimal\nhidden_columns=owner\n";
        let v2 = "version=2\nunits=decimal\nhidden_columns=owner\nlanguage=fr\n";
        assert_eq!(Preferences::default().detail_sort, 0);
        for text in [legacy, v2] {
            assert_eq!(Preferences::decode(text).unwrap().detail_sort, 0);
            for index in 0..12 {
                let changed = Preferences::decode(&format!("{text}detail_sort={index}\n")).unwrap();
                assert_eq!(changed.detail_sort, index);
                assert_eq!(changed.units, Units::Decimal);
                assert!(!changed.visible[3]);
                assert_eq!(Preferences::decode(&changed.encode()), Some(changed));
            }
            for value in ["12", "255", "256", "-1", "future", ""] {
                let decoded = Preferences::decode(&format!("{text}detail_sort={value}\n")).unwrap();
                assert_eq!(decoded.detail_sort, 0);
                assert_eq!(decoded.units, Units::Decimal);
                assert!(!decoded.visible[3]);
            }
        }
        let invalid = Preferences {
            detail_sort: 255,
            ..Default::default()
        };
        assert_eq!(
            Preferences::decode(&invalid.encode()).unwrap().detail_sort,
            0
        );
        assert!(Preferences::decode(&format!("{v2}detail_sort=2\ndetail_sort=3\n")).is_none());
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

#[cfg(test)]
mod workshop_tests {
    use super::*;
    #[test]
    fn additive_theme_density_keep_version_two_and_legacy_defaults() {
        for theme in 0..3 {
            for dense in [false, true] {
                let p = Preferences {
                    theme,
                    dense,
                    ..Default::default()
                };
                assert!(p.encode().starts_with("version=2\n"));
                assert_eq!(Preferences::decode(&p.encode()), Some(p));
            }
        }
        let old = Preferences::decode("version=2\nunits=binary\nhidden_columns=\nlanguage=auto\n")
            .unwrap();
        assert_eq!((old.theme, old.dense), (0, false));
        let invalid=Preferences::decode("version=2\nunits=binary\nhidden_columns=\nlanguage=auto\ntheme=invalid\ndensity=invalid\n").unwrap();
        assert_eq!((invalid.theme, invalid.dense), (0, false));
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use sortsize_core::inventory_contract::MemoryMode;
    #[test]
    fn gui_memory_floor_rejects_127_accepts_128_and_roundtrips() {
        assert_eq!(memory_limit_bytes(127), None);
        assert_eq!(memory_limit_bytes(128), Some(128 * 1024 * 1024));
        assert_eq!(memory_limit_bytes(u64::MAX), None);
        let mut preferences = Preferences {
            memory_bytes: memory_limit_bytes(128),
            ..Default::default()
        };
        assert_eq!(
            Preferences::decode(&preferences.encode()),
            Some(preferences.clone())
        );
        let below = preferences
            .encode()
            .replace("memory_bytes=134217728", "memory_bytes=133169152");
        preferences.memory_bytes = None;
        assert_eq!(Preferences::decode(&below), Some(preferences));
    }

    #[test]
    fn additive_memory_keys_keep_v1_v2_settings_and_invalid_falls_back() {
        for key in [
            "auto",
            "50000",
            "250000",
            "1000000",
            "5000000",
            "unlimited-count",
        ] {
            let mut p = Preferences::default();
            p.set_column(3, false);
            p.sidebar_ratio = 360;
            p.theme = 2;
            p.dense = true;
            p.units = Units::Decimal;
            p.memory = MemoryMode::from_key(key).unwrap();
            assert_eq!(Preferences::decode(&p.encode()), Some(p.clone()));
            let invalid = p
                .encode()
                .replace(&format!("memory_mode={key}"), "memory_mode=bad-key");
            p.memory = MemoryMode::Auto;
            assert_eq!(Preferences::decode(&invalid), Some(p));
        }
        assert_eq!(
            Preferences::decode("version=1\nunits=decimal\nhidden_columns=owner\n")
                .unwrap()
                .memory,
            MemoryMode::Auto
        );
        assert_eq!(
            Preferences::decode(
                "version=2\nunits=decimal\nhidden_columns=owner\nlanguage=auto\nfuture=x\n"
            )
            .unwrap()
            .memory,
            MemoryMode::Auto
        );
    }
}

#[cfg(test)]
mod size_unit_tests {
    use super::*;

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn win02_missing_profile_starts_automatic_without_creating_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new-profile/display-v1.conf");
        let (preferences, status) = crate::preferences_store::load(&path);
        assert_eq!(status, 0);
        assert_eq!(preferences.units.size_unit(), 0);
        assert!(!path.parent().unwrap().exists());
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn win02_legacy_profiles_without_size_unit_stay_automatic_and_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-v1.conf");
        for version in [1, 2] {
            for base in ["binary", "decimal"] {
                let text =
                    format!("version={version}\nunits={base}\nhidden_columns=owner\nlanguage=fr\n");
                std::fs::write(&path, &text).unwrap();
                let (preferences, status) = crate::preferences_store::load(&path);
                assert_eq!(status, 0);
                assert_eq!(preferences.units.size_unit(), 0);
                assert_eq!(preferences.units.is_decimal(), base == "decimal");
                assert!(!preferences.visible[3]);
                assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
            }
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn win02_explicit_units_survive_loading_and_saving_including_giga() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-v1.conf");
        // Historical files have no provenance marker for size_unit. Preserve
        // recorded choices rather than guessing that a giga choice was implicit.
        for version in [1, 2] {
            for base in ["binary", "decimal"] {
                for (choice, unit) in ["auto", "kilo", "mega", "giga", "tera"].iter().enumerate() {
                    let text = format!(
                        "version={version}\nunits={base}\nhidden_columns=owner\nlanguage=fr\nsize_unit={unit}\n"
                    );
                    std::fs::write(&path, &text).unwrap();
                    let (preferences, status) = crate::preferences_store::load(&path);
                    assert_eq!(status, 0);
                    assert_eq!(preferences.units.size_unit(), choice as u8);
                    assert_eq!(preferences.units.is_decimal(), base == "decimal");
                    assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
                    crate::preferences_store::save(&path, &preferences).unwrap();
                    assert_eq!(crate::preferences_store::load(&path), (preferences, 0));
                }
            }
        }
    }

    #[test]
    fn additive_size_unit_roundtrips_every_base_and_legacy_defaults() {
        for base in [Units::Binary, Units::Decimal] {
            for mode in 0..=4 {
                let expected = Preferences {
                    units: base.with_size_unit(mode),
                    ..Default::default()
                };
                assert_eq!(Preferences::decode(&expected.encode()), Some(expected));
            }
        }
        for text in [
            "version=1\nunits=decimal\nhidden_columns=owner\n",
            "version=2\nunits=decimal\nhidden_columns=owner\nlanguage=fr\n",
        ] {
            assert_eq!(Preferences::decode(text).unwrap().units, Units::Decimal);
            assert_eq!(
                Preferences::decode(&format!("size_unit=giga\n{text}"))
                    .unwrap()
                    .units,
                Units::DecimalFixed(3)
            );
            assert_eq!(
                Preferences::decode(&format!("{text}size_unit=future\n"))
                    .unwrap()
                    .units,
                Units::Decimal
            );
            assert!(
                Preferences::decode(&format!("{text}size_unit=kilo\nsize_unit=mega\n")).is_none()
            );
        }
    }
}

#[cfg(test)]
#[test]
fn duplicate_profile_defaults_migrates_and_round_trips_only_normal_modes() {
    let old = "version=2\nunits=binary\nhidden_columns=\nlanguage=auto\n";
    assert_eq!(
        Preferences::decode(old).unwrap().duplicate_profile,
        DuplicateFileProfile::LargeFiles
    );
    for (index, key, expected) in [
        (0, "general", DuplicateFileProfile::General),
        (1, "optimized", DuplicateFileProfile::Optimized),
        (2, "large", DuplicateFileProfile::LargeFiles),
    ] {
        let preferences = Preferences {
            duplicate_profile: expected,
            ..Default::default()
        };
        assert!(
            preferences
                .encode()
                .contains(&format!("duplicate_profile={key}\n"))
        );
        assert_eq!(
            Preferences::decode(&preferences.encode()).unwrap(),
            preferences
        );
        assert_eq!(
            effective_profile(index, true),
            DuplicateFileProfile::Exhaustive
        );
        assert_eq!(effective_profile(index, false), expected);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn duplicate_temporary_preference_round_trips_native_bytes() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

    let preferences = Preferences {
        duplicate_resources: DuplicateResources {
            memory_limit: Some(64 * MIB),
            temporary_limit: Some(4096 * MIB),
            temporary_parent: Some(PathBuf::from(OsStr::from_bytes(b"/lab/tmp-\xff\n"))),
        },
        ..Default::default()
    };
    let encoded = preferences.encode();
    assert_eq!(Preferences::decode(&encoded), Some(preferences));
    for invalid in ["", "f", "00", "2f00", "6e6f742d6162736f6c757465", "2z", "é"] {
        assert!(DuplicateResources::decode_parent(invalid).is_none());
    }
}
