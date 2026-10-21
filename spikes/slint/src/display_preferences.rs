//! Stable presentation schema. No paths, scan data or network consent.
use crate::units::Units;

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
    pub visible: [bool; 11],
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            units: Units::Binary,
            visible: [true; 11],
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
            "version=1\nunits={}\nhidden_columns={hidden}\n",
            if self.units == Units::Binary {
                "binary"
            } else {
                "decimal"
            }
        )
    }
    pub fn decode(text: &str) -> Option<Self> {
        let mut result = Self::default();
        let mut version = None;
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
        (version == Some("1")).then_some(result)
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
