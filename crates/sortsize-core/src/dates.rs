//! Optional dates extracted from the already acquired local metadata.
use crate::{MetadataValue, ValueAccuracy};
use std::time::SystemTime;

/// Dates of one retained object at metadata acquisition time, not subtree totals.
///
/// Instants retain the native precision (including pre-1970 values) without a
/// display timezone. Known values always use `Exact`, even if listing the
/// directory later fails or the scan is cancelled. Filesystem access policies
/// such as relatime/noatime or deferred Windows atime updates still apply.
/// Ignored symlinks/special entries use `NotCollected`; they are never followed.
/// This type does not define a timestamp export format or change JSON CLI v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryDates {
    pub modified: MetadataValue<SystemTime>,
    pub accessed: MetadataValue<SystemTime>,
}

impl EntryDates {
    pub(crate) fn collect(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: date_value(metadata.modified()),
            accessed: date_value(metadata.accessed()),
        }
    }

    pub(crate) const fn not_collected() -> Self {
        Self {
            modified: MetadataValue::NotCollected,
            accessed: MetadataValue::NotCollected,
        }
    }
}

fn date_value(result: std::io::Result<SystemTime>) -> MetadataValue<SystemTime> {
    match result {
        Ok(value) => MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        },
        Err(error) => match error.kind() {
            std::io::ErrorKind::Unsupported => MetadataValue::Unsupported,
            std::io::ErrorKind::PermissionDenied => MetadataValue::AccessDenied,
            _ => MetadataValue::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn accessor_results_preserve_known_instants_and_only_proven_absence_reasons() {
        for value in [UNIX_EPOCH, UNIX_EPOCH - Duration::new(1, 123_456_700)] {
            assert_eq!(
                date_value(Ok(value)),
                MetadataValue::Known {
                    value,
                    accuracy: ValueAccuracy::Exact,
                }
            );
        }
        for (kind, expected) in [
            (ErrorKind::Unsupported, MetadataValue::Unsupported),
            (ErrorKind::PermissionDenied, MetadataValue::AccessDenied),
            (ErrorKind::NotFound, MetadataValue::Unknown),
            (ErrorKind::InvalidData, MetadataValue::Unknown),
            (ErrorKind::Other, MetadataValue::Unknown),
        ] {
            assert_eq!(date_value(Err(Error::from(kind))), expected);
        }
        assert_eq!(
            EntryDates::not_collected().modified,
            MetadataValue::NotCollected
        );
        assert_eq!(
            EntryDates::not_collected().accessed,
            MetadataValue::NotCollected
        );
    }
}
