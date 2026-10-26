//! Desktop presentation only. Integer arithmetic keeps rounding exact up to u64::MAX.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Units {
    #[default]
    Binary,
    Decimal,
}
impl Units {
    pub fn format(self, bytes: u64) -> String {
        let (base, labels) = match self {
            Self::Binary => (1024u128, ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]),
            Self::Decimal => (1000u128, ["kB", "MB", "GB", "TB", "PB", "EB"]),
        };
        let bytes = u128::from(bytes);
        if bytes < base {
            return tr_count!("{n} byte", "{n} bytes", bytes as u64);
        }
        let mut divisor = base;
        let mut unit = 0;
        while bytes >= divisor * base && unit + 1 < labels.len() {
            divisor *= base;
            unit += 1;
        }
        let tenths = (bytes * 10 + divisor / 2) / divisor;
        format!("{} {}", crate::number_format::tenths(tenths), labels[unit])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
