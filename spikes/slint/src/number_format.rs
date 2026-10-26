//! Human presentation only: canonical conventions for the five product languages. No I/O.
use crate::{locale::Language, presentation::Context};

pub(crate) fn integer(value: u128) -> String {
    let digits = value.to_string();
    if digits.len() <= 3 {
        return digits;
    }
    let separator = match Context::current().language {
        Language::French => '\u{202f}',
        Language::German | Language::Spanish => '.',
        Language::Portuguese => '\u{a0}',
        Language::English => ',',
    };
    let mut result = String::with_capacity(digits.len() + digits.len() / 3 * separator.len_utf8());
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(separator);
        }
        result.push(digit);
    }
    result
}
pub(crate) fn decimal_separator() -> char {
    if Context::current().language != Language::English {
        ','
    } else {
        '.'
    }
}
pub(crate) fn tenths(value: u128) -> String {
    format!(
        "{}{}{}",
        integer(value / 10),
        decimal_separator(),
        value % 10
    )
}
pub(crate) fn decimal(value: f64) -> String {
    // Percentages only; no conversion back to a native number or sort key.
    let value = format!("{value:.1}");
    let (whole, fractional) = value.split_once('.').unwrap_or((&value, "0"));
    let whole = whole
        .parse::<u128>()
        .map(integer)
        .unwrap_or_else(|_| whole.into());
    format!("{whole}{}{fractional}", decimal_separator())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_range_grouping_decimals_and_no_global_locale_mutation() {
        for (language, grouped, fractional) in [
            (Language::English, "18,446,744,073,709,551,615", "1,234.5"),
            (Language::German, "18.446.744.073.709.551.615", "1.234,5"),
            (Language::Spanish, "18.446.744.073.709.551.615", "1.234,5"),
            (
                Language::Portuguese,
                "18\u{a0}446\u{a0}744\u{a0}073\u{a0}709\u{a0}551\u{a0}615",
                "1\u{a0}234,5",
            ),
            (
                Language::French,
                "18\u{202f}446\u{202f}744\u{202f}073\u{202f}709\u{202f}551\u{202f}615",
                "1\u{202f}234,5",
            ),
        ] {
            let _scope = Context {
                language,
                generation: 1,
            }
            .enter();
            assert_eq!(integer(u64::MAX.into()), grouped);
            assert_eq!(tenths(12345), fractional);
            assert_eq!(decimal(1234.5), fractional);
            assert_eq!(integer(0), "0");
            assert_eq!(integer(999), "999");
        }
    }
}
