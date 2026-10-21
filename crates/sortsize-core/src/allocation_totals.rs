//! Single-consumer, bounded aggregation. No I/O, paths or physical-block claims.
use crate::{
    Coverage, EntryAllocation, InventoryKind, InventoryReport, InventoryWithMetadata, LocalFileId,
    Metadata, MetadataSelection, MetadataValue, ValueAccuracy,
};
use std::collections::HashMap;

/// Opt-in aggregation policy, separate from legacy ScanOptions and MetadataSelection.
#[derive(Debug, Clone, Copy)]
pub struct AllocationAggregationOptions {
    /// Dates/raw per-file properties to retain. Allocation acquisition is always
    /// enabled for aggregation, even when the raw allocation table is disabled.
    pub metadata: MetadataSelection,
    /// Track identities for a separate scan-wide sum. Never a physical-space sum.
    pub deduplicate_by_identity: bool,
    /// Maximum distinct native identities, including single-link files. Zero
    /// allows an empty observed set only. Exhaustion invalidates only by_identity.
    pub max_identities: usize,
}
impl Default for AllocationAggregationOptions {
    fn default() -> Self {
        Self {
            metadata: MetadataSelection::default(),
            deduplicate_by_identity: true,
            max_identities: 65_536,
        }
    }
}

/// Diagnostics of the optional identity table, not scan coverage. Multiple
/// causes may coexist. No identity counts are claimed exhaustive after failure.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IdentityTrackingStats {
    pub limit: usize,
    pub retained_identities: usize,
    /// Actual pre-reserved HashMap capacity; no growth after initialization.
    pub capacity: usize,
    pub budget_exhausted: bool,
    pub unavailable_metadata: bool,
    pub inconsistent_observations: bool,
    pub allocation_failed: bool,
    pub arithmetic_overflow: bool,
}

#[derive(Debug)]
pub struct AllocationTotals {
    /// All observed regular file paths, including omitted inventory entries.
    pub by_path: Metadata<u64>,
    /// Each reliable native identity once, across the observed scope. Reflinks,
    /// snapshots and other extent sharing are NOT deduplicated. Unknown if the
    /// identity budget, availability, consistency or arithmetic checks fail.
    pub by_identity: Metadata<u64>,
    /// Always Unknown: neither sum proves recoverable space.
    pub potentially_reclaimable_bytes: MetadataValue<u64>,
    pub identity_tracking: IdentityTrackingStats,
}

#[derive(Debug)]
pub struct InventoryWithAllocationTotals {
    /// Unchanged tranche A result, with independently selectable raw tables.
    pub metadata: InventoryWithMetadata,
    pub totals: AllocationTotals,
    /// Exactly one record per retained node. Files describe their observation;
    /// directories sum descendant file paths, including omitted descendants.
    /// Symlinks/specials are NotCollected. No directory identity sum is assigned.
    pub entries_by_path: Vec<Metadata<u64>>,
}

fn exact(value: u64) -> MetadataValue<u64> {
    MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    }
}

/// Commutative absence propagation: same proven cause survives; mixed causes
/// become Unknown. Missing observations are never added as numeric zero.
fn add(left: MetadataValue<u64>, right: MetadataValue<u64>) -> MetadataValue<u64> {
    match (left, right) {
        (
            MetadataValue::Known {
                value: a,
                accuracy: aa,
            },
            MetadataValue::Known {
                value: b,
                accuracy: ba,
            },
        ) => match a.checked_add(b) {
            Some(value) => MetadataValue::Known {
                value,
                accuracy: if aa == ValueAccuracy::Exact && ba == ValueAccuracy::Exact {
                    ValueAccuracy::Exact
                } else {
                    ValueAccuracy::LowerBound
                },
            },
            None => MetadataValue::Unknown,
        },
        (MetadataValue::Known { .. }, missing) | (missing, MetadataValue::Known { .. }) => missing,
        (a, b) if a == b => a,
        _ => MetadataValue::Unknown,
    }
}
fn qualify(value: MetadataValue<u64>, coverage: Coverage) -> Metadata<u64> {
    let value = match value {
        MetadataValue::Known { value, .. } if coverage != Coverage::Complete => {
            MetadataValue::Known {
                value,
                accuracy: ValueAccuracy::LowerBound,
            }
        }
        other => other,
    };
    Metadata { value, coverage }
}
fn unavailable<T>(value: MetadataValue<T>) -> MetadataValue<u64> {
    match value {
        MetadataValue::AccessDenied => MetadataValue::AccessDenied,
        MetadataValue::Unsupported => MetadataValue::Unsupported,
        MetadataValue::NotCollected => MetadataValue::NotCollected,
        _ => MetadataValue::Unknown,
    }
}

#[derive(Debug, Clone, Copy)]
struct IdentitySample {
    allocated: u64,
    logical: u64,
    links: u64,
    paths_seen: u64,
}

pub(crate) struct AllocationBuilder {
    options: AllocationAggregationOptions,
    by_path: MetadataValue<u64>,
    by_identity: MetadataValue<u64>,
    entries: Vec<Metadata<u64>>,
    identities: HashMap<LocalFileId, IdentitySample>,
    stats: IdentityTrackingStats,
}
impl AllocationBuilder {
    pub(crate) fn new(options: AllocationAggregationOptions) -> Self {
        Self {
            options,
            by_path: exact(0),
            by_identity: if options.deduplicate_by_identity {
                exact(0)
            } else {
                MetadataValue::NotCollected
            },
            entries: Vec::new(),
            identities: HashMap::new(),
            stats: IdentityTrackingStats {
                limit: options.max_identities,
                ..Default::default()
            },
        }
    }
    pub(crate) fn observe_file(
        &mut self,
        parent: Option<usize>,
        logical: u64,
        allocation: Option<EntryAllocation>,
    ) {
        let allocation = allocation.unwrap_or_else(EntryAllocation::not_collected);
        self.by_path = add(self.by_path, allocation.allocated_bytes);
        if let Some(parent) = parent {
            self.entries[parent].value =
                add(self.entries[parent].value, allocation.allocated_bytes);
        }
        if self.options.deduplicate_by_identity {
            self.observe_identity(logical, allocation);
        }
    }
    fn observe_identity(&mut self, logical: u64, allocation: EntryAllocation) {
        let mut absent = exact(0);
        // Only exact native properties permit deduplication. Missing link counts
        // or a changed count cannot justify counting a path as a new object.
        let bytes = match allocation.allocated_bytes {
            MetadataValue::Known {
                value,
                accuracy: ValueAccuracy::Exact,
            } => Some(value),
            other => {
                absent = add(absent, unavailable(other));
                None
            }
        };
        let id = match allocation.identity {
            MetadataValue::Known {
                value,
                accuracy: ValueAccuracy::Exact,
            } => Some(value),
            other => {
                absent = add(absent, unavailable(other));
                None
            }
        };
        let links = match allocation.link_count {
            MetadataValue::Known {
                value,
                accuracy: ValueAccuracy::Exact,
            } if value > 0 => Some(value),
            other => {
                absent = add(absent, unavailable(other));
                None
            }
        };
        let (Some(bytes), Some(id), Some(links)) = (bytes, id, links) else {
            self.stats.unavailable_metadata = true;
            self.by_identity = add(self.by_identity, absent);
            return;
        };
        if let Some(sample) = self.identities.get_mut(&id) {
            if sample.allocated != bytes
                || sample.logical != logical
                || sample.links != links
                || sample.paths_seen >= links
            {
                self.stats.inconsistent_observations = true;
                self.by_identity = MetadataValue::Unknown;
            } else {
                sample.paths_seen += 1;
            }
            return;
        }
        if self.identities.len() == self.options.max_identities {
            self.stats.budget_exhausted = true;
            self.by_identity = MetadataValue::Unknown;
            return;
        }
        if self.stats.allocation_failed {
            return;
        }
        if self.identities.capacity() == 0 {
            // Reserve the ENTIRE configured table once; insertions never grow it.
            // try_reserve also handles capacity overflow without panicking.
            if self
                .identities
                .try_reserve(self.options.max_identities)
                .is_err()
            {
                self.stats.allocation_failed = true;
                self.by_identity = MetadataValue::Unknown;
                return;
            }
            self.stats.capacity = self.identities.capacity();
        }
        self.identities.insert(
            id,
            IdentitySample {
                allocated: bytes,
                logical,
                links,
                paths_seen: 1,
            },
        );
        if let MetadataValue::Known { value, .. } = self.by_identity
            && value.checked_add(bytes).is_none()
        {
            self.stats.arithmetic_overflow = true;
        }
        self.by_identity = add(self.by_identity, exact(bytes));
    }
    pub(crate) fn retain(
        &mut self,
        kind: InventoryKind,
        allocation: Option<EntryAllocation>,
        max_entries: usize,
    ) {
        super::inventory::reserve_record(&mut self.entries, max_entries);
        let value = match kind {
            InventoryKind::Directory => exact(0),
            InventoryKind::File => {
                allocation
                    .unwrap_or_else(EntryAllocation::not_collected)
                    .allocated_bytes
            }
            _ => MetadataValue::NotCollected,
        };
        self.entries.push(Metadata {
            value,
            coverage: Coverage::Complete,
        });
    }
    pub(crate) fn finish(
        mut self,
        inventory: &InventoryReport,
    ) -> (AllocationTotals, Vec<Metadata<u64>>) {
        for index in (0..inventory.entries.len()).rev() {
            let entry = &inventory.entries[index];
            if entry.kind != InventoryKind::Directory {
                continue;
            }
            if let Some(parent) = entry.parent {
                self.entries[parent].value =
                    add(self.entries[parent].value, self.entries[index].value);
            }
            let coverage = if !entry.incomplete {
                Coverage::Complete
            } else if inventory.report.coverage == Coverage::Cancelled {
                Coverage::Cancelled
            } else {
                Coverage::Partial
            };
            self.entries[index] = qualify(self.entries[index].value, coverage);
        }
        self.stats.retained_identities = self.identities.len();
        (
            AllocationTotals {
                by_path: qualify(self.by_path, inventory.report.coverage),
                by_identity: qualify(self.by_identity, inventory.report.coverage),
                potentially_reclaimable_bytes: MetadataValue::Unknown,
                identity_tracking: self.stats,
            },
            self.entries,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn allocation(id: u64, bytes: u64, links: u64) -> EntryAllocation {
        EntryAllocation {
            allocated_bytes: exact(bytes),
            identity: MetadataValue::Known {
                value: LocalFileId::Unix {
                    device: 1,
                    inode: id,
                },
                accuracy: ValueAccuracy::Exact,
            },
            link_count: exact(links),
        }
    }
    fn builder(limit: usize) -> AllocationBuilder {
        AllocationBuilder::new(AllocationAggregationOptions {
            max_identities: limit,
            ..Default::default()
        })
    }
    #[test]
    fn unavailable_properties_never_become_zero_and_only_affect_their_metric() {
        for missing in [
            MetadataValue::Unknown,
            MetadataValue::Unsupported,
            MetadataValue::AccessDenied,
            MetadataValue::NotCollected,
        ] {
            let mut b = builder(8);
            let mut a = allocation(1, 4096, 1);
            a.allocated_bytes = missing;
            b.observe_file(None, 1, Some(a));
            assert_eq!(b.by_path, missing);
            assert_eq!(b.by_identity, missing);
            assert!(b.stats.unavailable_metadata);
        }
        let mut b = builder(8);
        let mut a = allocation(1, 4096, 1);
        a.identity = MetadataValue::AccessDenied;
        b.observe_file(None, 1, Some(a));
        assert_eq!(b.by_path, exact(4096));
        assert_eq!(b.by_identity, MetadataValue::AccessDenied);
        a.identity = MetadataValue::Unknown;
        b.observe_file(None, 1, Some(a));
        assert_eq!(b.by_path, exact(8192));
        assert_eq!(b.by_identity, MetadataValue::Unknown);
        for link_count in [MetadataValue::Unknown, MetadataValue::Unsupported, exact(0)] {
            let mut b = builder(8);
            b.observe_file(
                None,
                1,
                Some(EntryAllocation {
                    link_count,
                    ..allocation(1, 4096, 1)
                }),
            );
            assert_eq!(b.by_path, exact(4096));
            assert!(!matches!(b.by_identity, MetadataValue::Known { .. }));
        }
    }
    #[test]
    fn contradictions_in_any_order_invalidate_identity_only() {
        // Includes one -> many link transitions: single-link identities must
        // remain tracked to reject this irrespective of worker ordering.
        for samples in [
            [(1, 4096, 1), (1, 4096, 2)],
            [(1, 4096, 2), (1, 8192, 2)],
            [(1, 4096, 2), (2, 4096, 2)],
            [(1, 4096, 1), (1, 4096, 1)],
        ] {
            for reverse in [false, true] {
                let mut b = builder(8);
                let mut samples = samples;
                if reverse {
                    samples.reverse();
                }
                let mut sum = 0;
                for (logical, bytes, links) in samples {
                    b.observe_file(None, logical, Some(allocation(1, bytes, links)));
                    sum += bytes;
                }
                assert_eq!(b.by_path, exact(sum));
                assert_eq!(b.by_identity, MetadataValue::Unknown);
                assert!(b.stats.inconsistent_observations);
            }
        }
    }
    #[test]
    fn limit_checks_distinct_identities_not_paths_and_never_grows_table() {
        eprintln!(
            "Metadata<u64>={} IdentityRecord={} Observation unchanged by tranche B",
            std::mem::size_of::<Metadata<u64>>(),
            std::mem::size_of::<(LocalFileId, IdentitySample)>()
        );
        for limit in [0, 1, 3] {
            let mut b = builder(limit);
            for id in 0..limit {
                b.observe_file(None, 1, Some(allocation(id as u64, 4096, 4)));
            }
            let capacity = b.identities.capacity();
            for _ in 0..3 {
                for id in 0..limit {
                    b.observe_file(None, 1, Some(allocation(id as u64, 4096, 4)));
                }
            }
            assert_eq!(b.by_identity, exact(limit as u64 * 4096));
            assert!(!b.stats.budget_exhausted);
            b.observe_file(None, 1, Some(allocation(limit as u64, 4096, 1)));
            assert_eq!(b.by_identity, MetadataValue::Unknown);
            assert!(b.stats.budget_exhausted);
            assert_eq!(b.identities.len(), limit);
            assert_eq!(b.identities.capacity(), capacity);
        }
    }
    #[test]
    fn overflow_reservation_failure_and_disabled_tracking_are_explicit() {
        let mut b = builder(8);
        b.observe_file(None, 1, Some(allocation(1, u64::MAX, 2)));
        b.observe_file(None, 1, Some(allocation(1, u64::MAX, 2)));
        assert_eq!(b.by_path, MetadataValue::Unknown);
        assert_eq!(b.by_identity, exact(u64::MAX));
        b.observe_file(None, 1, Some(allocation(2, 1, 1)));
        assert_eq!(b.by_identity, MetadataValue::Unknown);
        assert!(b.stats.arithmetic_overflow);
        let mut b = builder(usize::MAX);
        b.observe_file(None, 1, Some(allocation(1, 4096, 1)));
        assert!(b.stats.allocation_failed);
        assert_eq!(b.by_path, exact(4096));
        assert_eq!(b.by_identity, MetadataValue::Unknown);
        assert_eq!(b.identities.capacity(), 0);
        let mut b = AllocationBuilder::new(AllocationAggregationOptions {
            deduplicate_by_identity: false,
            ..Default::default()
        });
        b.observe_file(None, 1, Some(allocation(1, 4096, 1)));
        assert_eq!(b.by_identity, MetadataValue::NotCollected);
        assert_eq!(b.identities.capacity(), 0);
    }
    #[test]
    fn mixed_missing_states_and_overflow_propagate_only_to_affected_ancestors() {
        let mut b = builder(8);
        b.retain(InventoryKind::Directory, None, 8); // root
        b.retain(InventoryKind::Directory, None, 8); // child
        b.observe_file(
            Some(1),
            1,
            Some(EntryAllocation {
                allocated_bytes: MetadataValue::AccessDenied,
                ..allocation(1, 1, 1)
            }),
        );
        b.observe_file(Some(0), 1, Some(allocation(2, 4096, 1)));
        assert_eq!(b.entries[0].value, exact(4096));
        assert_eq!(b.entries[1].value, MetadataValue::AccessDenied);
        assert_eq!(
            add(b.entries[0].value, b.entries[1].value),
            MetadataValue::AccessDenied
        );
        assert_eq!(
            add(MetadataValue::Unsupported, MetadataValue::AccessDenied),
            MetadataValue::Unknown
        );
        assert_eq!(add(exact(u64::MAX), exact(1)), MetadataValue::Unknown);
    }
}
