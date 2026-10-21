//! Allocation presentation only. Called on the scan worker, never by row rendering.
#[cfg(test)]
use sortsize_core::Metadata;
use sortsize_core::{MetadataValue, ValueAccuracy};

#[cfg(test)]
pub(crate) fn format_allocation(metric: Metadata<u64>) -> String {
    format_value(metric.value, crate::units::Units::Binary)
}

pub(crate) fn format_value(value: MetadataValue<u64>, units: crate::units::Units) -> String {
    // Coverage describes the observation scope. It does not prove a numerical
    // bound: only the supplied accuracy may add ≥, even for Partial/Cancelled.
    match value {
        MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        } => units.format(value),
        MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::LowerBound,
        } => format!("≥ {}", units.format(value)),
        MetadataValue::Unknown => "Inconnue".into(),
        MetadataValue::Unsupported => "Non prise en charge".into(),
        MetadataValue::AccessDenied => "Accès refusé".into(),
        MetadataValue::NotCollected => "Non collectée".into(),
        _ => "Inconnue".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::Coverage;
    #[test]
    fn zero_absence_and_bounds_never_depend_on_coverage_alone() {
        eprintln!(
            "SharedString={} DetailRow={}",
            std::mem::size_of::<slint::SharedString>(),
            std::mem::size_of::<crate::DetailRow>()
        );
        for coverage in [Coverage::Complete, Coverage::Partial, Coverage::Cancelled] {
            for (value, expected) in [
                (
                    MetadataValue::Known {
                        value: 0,
                        accuracy: ValueAccuracy::Exact,
                    },
                    "0 octets",
                ),
                (
                    MetadataValue::Known {
                        value: 4096,
                        accuracy: ValueAccuracy::Exact,
                    },
                    "4.0 KiB",
                ),
                (
                    MetadataValue::Known {
                        value: 4096,
                        accuracy: ValueAccuracy::LowerBound,
                    },
                    "≥ 4.0 KiB",
                ),
                (MetadataValue::Unknown, "Inconnue"),
                (MetadataValue::Unsupported, "Non prise en charge"),
                (MetadataValue::AccessDenied, "Accès refusé"),
                (MetadataValue::NotCollected, "Non collectée"),
            ] {
                assert_eq!(format_allocation(Metadata { value, coverage }), expected);
                let decimal = expected.replace("4.0 KiB", "4.1 kB");
                assert_eq!(format_value(value, crate::units::Units::Decimal), decimal);
            }
        }
    }
}
