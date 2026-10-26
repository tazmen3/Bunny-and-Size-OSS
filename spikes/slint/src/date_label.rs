//! Presentation only: UTC Gregorian dates, no timezone database or filesystem I/O.
use sortsize_core::MetadataValue;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn format_date(date: MetadataValue<SystemTime>) -> String {
    let value = match date {
        MetadataValue::Known { value, .. } => value,
        MetadataValue::Unknown => return tr!("Unknown").into(),
        MetadataValue::Unsupported => return tr!("Unsupported").into(),
        MetadataValue::AccessDenied => return tr!("Access denied").into(),
        MetadataValue::NotCollected => return tr!("Not collected").into(),
        // Future availability states must never invent an instant.
        _ => return tr!("Unknown").into(),
    };
    // Signed nanoseconds plus Euclidean division preserve fractional instants
    // before 1970 (e.g. epoch - 1 ns is in the preceding calendar second).
    let ns = match value.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    let seconds = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000);
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date(days);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+07}")
    };
    // Localize only the selected month for each visible timestamp.
    let month = match month {
        1 => tr!("Jan"),
        2 => tr!("Feb"),
        3 => tr!("Mar"),
        4 => tr!("Apr"),
        5 => tr!("May"),
        6 => tr!("Jun"),
        7 => tr!("Jul"),
        8 => tr!("Aug"),
        9 => tr!("Sep"),
        10 => tr!("Oct"),
        11 => tr!("Nov"),
        12 => tr!("Dec"),
        _ => unreachable!("Gregorian month is in 1..=12"),
    };
    let decimal = crate::number_format::decimal_separator();
    format!(
        "{day:02} {month} {year} {:02}:{:02}:{:02}{decimal}{nanos:09} UTC",
        time / 3600,
        time / 60 % 60,
        time % 60,
    )
}

// Proleptic Gregorian calendar: March-based years grouped in 400-year eras.
// All intermediates fit i128 for SystemTime's u64 duration range. No unsigned
// epoch conversion, local timezone, locale state or date-range fallback to zero.
fn civil_date(days_since_epoch: i128) -> (i128, i128, i128) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = march_month + if march_month < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::ValueAccuracy;
    use std::time::Duration;

    fn label(value: SystemTime) -> String {
        format_date(MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        })
    }

    #[test]
    fn dates_keep_pre_epoch_fraction_precision_leap_centuries_and_utc() {
        assert_eq!(label(UNIX_EPOCH), "01 janv. 1970 00:00:00,000000000 UTC");
        assert_eq!(
            label(UNIX_EPOCH - Duration::from_nanos(100)),
            "31 déc. 1969 23:59:59,999999900 UTC"
        );
        #[cfg(unix)]
        assert_eq!(
            label(UNIX_EPOCH - Duration::from_nanos(1)),
            "31 déc. 1969 23:59:59,999999999 UTC"
        );
        assert_eq!(
            label(
                UNIX_EPOCH - Duration::from_secs(315_619_200) + Duration::from_nanos(123_456_700)
            ),
            "01 janv. 1960 00:00:00,123456700 UTC"
        );
        assert_eq!(
            label(UNIX_EPOCH + Duration::new(951_827_696, 765_432_100)),
            "29 févr. 2000 12:34:56,765432100 UTC"
        );
        for (days, expected) in [
            (-719_528, (0, 1, 1)),
            (-25_567, (1900, 1, 1)),
            (47_541, (2100, 3, 1)),
            (2_932_897, (10_000, 1, 1)),
            (-719_529, (-1, 12, 31)),
        ] {
            assert_eq!(civil_date(days), expected);
        }
    }

    #[test]
    fn effective_locale_policy_is_unambiguous_for_fr_us_and_gb() {
        use crate::{locale::LanguagePreference, presentation::Context};
        let value = UNIX_EPOCH + Duration::new(951_827_696, 765_432_100);
        for (system, expected) in [
            ("fr_FR", "29 févr. 2000 12:34:56,765432100 UTC"),
            ("fr_CA", "29 févr. 2000 12:34:56,765432100 UTC"),
            ("en_US", "29 Feb 2000 12:34:56.765432100 UTC"),
            ("en_GB", "29 Feb 2000 12:34:56.765432100 UTC"),
            ("de_DE", "29 Feb. 2000 12:34:56,765432100 UTC"),
            ("de_CH", "29 Feb. 2000 12:34:56,765432100 UTC"),
            ("es_ES", "29 feb 2000 12:34:56,765432100 UTC"),
            ("es_MX", "29 feb 2000 12:34:56,765432100 UTC"),
            ("pt_PT", "29 fev. 2000 12:34:56,765432100 UTC"),
            ("pt_BR", "29 fev. 2000 12:34:56,765432100 UTC"),
            ("ja_JP", "29 Feb 2000 12:34:56.765432100 UTC"),
        ] {
            let language = LanguagePreference::Automatic.resolve(Some(system));

            let _scope = Context {
                language,
                generation: 1,
            }
            .enter();
            assert_eq!(label(value), expected);
            assert_eq!(
                value.duration_since(UNIX_EPOCH).unwrap(),
                Duration::new(951_827_696, 765_432_100)
            );
        }
    }

    #[test]
    fn unavailable_dates_are_explicit_and_never_epoch_substitutes() {
        for (value, text) in [
            (MetadataValue::Unknown, "Inconnue"),
            (MetadataValue::Unsupported, "Non prise en charge"),
            (MetadataValue::AccessDenied, "Accès refusé"),
            (MetadataValue::NotCollected, "Non collectée"),
        ] {
            assert_eq!(format_date(value), text);
        }
    }
}
