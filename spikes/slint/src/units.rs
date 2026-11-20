//! Desktop presentation only. Integer arithmetic keeps rounding exact up to u64::MAX.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Units {
    #[default]
    Binary,
    Decimal,
    /// Presentation exponent only; scan and stored byte values remain exact.
    BinaryFixed(u8),
    DecimalFixed(u8),
}
impl Units {
    pub fn is_decimal(self) -> bool {
        matches!(self, Self::Decimal | Self::DecimalFixed(_))
    }
    /// 0 is automatic; 1..=4 select kilo, mega, giga and tera.
    pub fn size_unit(self) -> u8 {
        match self {
            Self::Binary | Self::Decimal => 0,
            Self::BinaryFixed(unit) | Self::DecimalFixed(unit) => unit.clamp(1, 4),
        }
    }
    pub fn with_size_unit(self, unit: u8) -> Self {
        match (self.is_decimal(), unit) {
            (false, 1..=4) => Self::BinaryFixed(unit),
            (true, 1..=4) => Self::DecimalFixed(unit),
            (false, _) => Self::Binary,
            (true, _) => Self::Decimal,
        }
    }
    pub fn with_decimal(self, decimal: bool) -> Self {
        if decimal { Self::Decimal } else { Self::Binary }.with_size_unit(self.size_unit())
    }
    pub fn format(self, bytes: u64) -> String {
        self.format_size(bytes, false)
    }
    /// Exact file sizes must distinguish a nonempty member from an empty file,
    /// even when an explicitly chosen unit is much larger than the file.
    pub fn format_file(self, bytes: u64) -> String {
        self.format_size(bytes, true)
    }
    fn format_size(self, bytes: u64, distinguish_nonzero: bool) -> String {
        let (base, labels) = match self.is_decimal() {
            false => (1024u128, ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]),
            true => (1000u128, ["kB", "MB", "GB", "TB", "PB", "EB"]),
        };
        let bytes = u128::from(bytes);
        if self.size_unit() == 0 && bytes < base {
            return tr_count!("{n} byte", "{n} bytes", bytes as u64);
        }
        let mut unit = usize::from(self.size_unit().saturating_sub(1));
        let mut divisor = base.pow(unit as u32 + 1);
        while self.size_unit() == 0 && bytes >= divisor * base && unit + 1 < labels.len() {
            divisor *= base;
            unit += 1;
        }
        let tenths = (bytes * 10 + divisor / 2) / divisor;
        if distinguish_nonzero && bytes > 0 && tenths == 0 {
            return format!("< {} {}", crate::number_format::tenths(1), labels[unit]);
        }
        format!("{} {}", crate::number_format::tenths(tenths), labels[unit])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forced_units_keep_one_exponent_for_zero_small_and_large_values() {
        let _scope = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        for (base, binary, labels) in [
            (1024u64, Units::Binary, ["KiB", "MiB", "GiB", "TiB"]),
            (1000u64, Units::Decimal, ["kB", "MB", "GB", "TB"]),
        ] {
            for choice in 1..=4 {
                let units = binary.with_size_unit(choice);
                let label = labels[usize::from(choice - 1)];
                assert_eq!(units.format(0), format!("0.0 {label}"));
                assert_eq!(units.format(1), format!("0.0 {label}"));
                assert_eq!(
                    units.format(base.pow(u32::from(choice))),
                    format!("1.0 {label}")
                );
                assert!(units.format(u64::MAX).ends_with(label));
                assert_eq!(units.with_decimal(!units.is_decimal()).size_unit(), choice);
                assert_eq!(units.with_size_unit(0), binary);
            }
        }
        assert_eq!(Units::Binary.with_size_unit(99), Units::Binary);
        assert_eq!(Units::Decimal.with_size_unit(99), Units::Decimal);
    }

    #[test]
    fn small_nonempty_files_never_round_to_zero_in_a_chosen_unit() {
        let _scope = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        for base in [Units::Binary, Units::Decimal] {
            assert_eq!(base.format_file(1), "1 byte");
            assert_eq!(base.format_file(9), "9 bytes");
            assert_eq!(base.format_file(53), "53 bytes");
            for unit in 1..=4 {
                let chosen = base.with_size_unit(unit);
                for bytes in [1, 9, 49] {
                    let display = chosen.format_file(bytes);
                    assert!(!display.starts_with("0.0"), "{bytes}: {display}");
                    assert!(display.starts_with("< 0.1"), "{bytes}: {display}");
                }
            }
        }
    }

    #[test]
    fn forced_units_preserve_unknown_allocations_and_lower_bounds() {
        use sortsize_core::{MetadataValue, ValueAccuracy};
        let _scope = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        let units = Units::Binary.with_size_unit(3);
        assert_eq!(
            crate::allocation_label::format_value(MetadataValue::Unknown, units),
            "Unknown"
        );
        assert_eq!(
            crate::allocation_label::format_value(
                MetadataValue::Known {
                    value: 0,
                    accuracy: ValueAccuracy::Exact,
                },
                units
            ),
            "0.0 GiB"
        );
        assert_eq!(
            crate::allocation_label::format_value(
                MetadataValue::Known {
                    value: 1 << 30,
                    accuracy: ValueAccuracy::LowerBound,
                },
                units
            ),
            "≥ 1.0 GiB"
        );
    }

    #[test]
    fn boundaries_rounding_and_full_u64_range() {
        for (n, binary, decimal) in [
            (0, "0 octet", "0 octet"),
            (1, "1 octet", "1 octet"),
            (999, "999 octets", "999 octets"),
            (1000, "1\u{202f}000 octets", "1,0 kB"),
            (1023, "1\u{202f}023 octets", "1,0 kB"),
            (1024, "1,0 KiB", "1,0 kB"),
            (1050, "1,0 KiB", "1,1 kB"),
            (1_048_576, "1,0 MiB", "1,0 MB"),
            (1 << 40, "1,0 TiB", "1,1 TB"),
            (u64::MAX, "16,0 EiB", "18,4 EB"),
        ] {
            assert_eq!(Units::Binary.format(n), binary);
            assert_eq!(Units::Decimal.format(n), decimal);
        }
        for shift in [10, 20, 30, 40, 50, 60] {
            assert!(Units::Binary.format(1 << shift).starts_with("1,0 "));
        }
        for power in 1..=6 {
            assert!(
                Units::Decimal
                    .format(1000u64.pow(power))
                    .starts_with("1,0 ")
            );
        }
    }
}
