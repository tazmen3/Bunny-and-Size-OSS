//! Additive metadata contract, independent of acquisition and presentation.
//!
//! Availability and accuracy describe one property; [`Coverage`] describes the
//! observation scope. None of these states implies a filesystem snapshot.
//! Existing reports and their JSON v2 representation are deliberately unchanged.

use crate::Coverage;
use serde::Serialize;

/// Meaning of a known value within its declared scope and measurement method.
///
/// This is independent of traversal coverage: an observed file length may be
/// exact even when other files could not be visited. Conversely, a cumulative
/// nonnegative size can be a lower bound when descendants were not observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ValueAccuracy {
    /// The value is exact for the stated scope, method and observation time.
    Exact,
    /// The value is at most the true value (for an appropriately ordered metric).
    /// Only use when the acquisition/aggregation method proves this bound.
    LowerBound,
}

/// Availability of one metadata property for one object or aggregate.
///
/// No default, numeric ordering or conversion from `Option<T>` is provided:
/// `None` alone cannot identify why a property is missing. In particular, a
/// known zero is not unknown, and denial is not lack of provider support.
/// Accuracy is present only when a value exists.
///
/// Support is property-, source- and API-specific, never inferred solely from
/// the OS name. A supported property can still be denied, not collected or
/// unknown for a particular object. This type does not declare provider APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MetadataValue<T> {
    Known {
        value: T,
        accuracy: ValueAccuracy,
    },
    /// No usable value is known, and no more specific absence reason is proven.
    Unknown,
    /// The source/API does not support this property in the stated context.
    Unsupported,
    /// Collection was refused by the applicable access policy or permissions.
    AccessDenied,
    /// Collection was not performed (e.g. not requested or not reached).
    NotCollected,
}

/// A property's value state and the coverage of its declared observation scope.
///
/// `Complete` means the intended traversal finished, not that every property was
/// collected. `Partial` and `Cancelled` do not turn an exact observed property
/// into a lower bound automatically. Callers must keep the scope (object,
/// directory or scan) explicit; no coverage/accuracy conversion is implicit.
///
/// This record has no path or provider key: those identities stay in their
/// original native/opaque model. `T` may be borrowed, avoiding payload copies.
/// Serialization delegates to `T`; native path exports must use the existing
/// lossless [`crate::NativePath`] contract explicitly, not a lossy string.
/// These types are not added to `ScanReport` or JSON CLI v2 in this tranche.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Metadata<T> {
    pub value: MetadataValue<T>,
    pub coverage: Coverage,
}
