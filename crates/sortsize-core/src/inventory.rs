//! Optional, bounded inventory built during the same traversal as the CLI summary.
use crate::allocation_totals::AllocationBuilder;
use crate::local::{CollectFields, Observation};
use crate::scan::scan_observed;
use crate::top_files::{TopFiles, TopFilesBuilder};
use crate::{
    AllocationAggregationOptions, AllocationTotals, InventoryWithAllocationTotals, Metadata,
};
use crate::{
    CancellationToken, Coverage, EntryAllocation, EntryDates, EntryOwner, Progress, ScanFailure,
    ScanOptions, ScanReport,
};
use std::collections::{HashMap, hash_map::RandomState};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryKind {
    Directory,
    File,
    Symlink,
    Special,
}

/// Counts of observed file paths and real directories in the scan scope.
/// Recursive counts include direct children, but never the directory itself.
/// Hardlink paths count separately; excluded, symbolic and special entries do not.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryCounts {
    pub direct_files: u64,
    pub direct_directories: u64,
    pub recursive_files: u64,
    pub recursive_directories: u64,
}

#[derive(Debug)]
pub struct InventoryEntry {
    /// Native path, never reconstructed from a display string.
    pub path: PathBuf,
    /// Index of a retained ancestor. The root has no parent.
    pub parent: Option<usize>,
    pub kind: InventoryKind,
    /// Observed logical bytes; None for ignored links and special entries.
    pub logical_bytes: Option<u64>,
    /// Errors, depth limits or cancellation make a directory's size a lower bound.
    pub incomplete: bool,
    /// None for non-directories. Values are lower bounds when `incomplete` is
    /// true; inventory retention limits alone do not make counts partial.
    pub counts: Option<DirectoryCounts>,
}

#[derive(Debug)]
pub struct InventoryReport {
    /// Global observed Top 100, independent of inventory retention limits.
    pub top_files: TopFiles,
    pub report: ScanReport,
    pub entries: Vec<InventoryEntry>,
    /// Omitted tree nodes, not unscanned objects. Totals continue to accumulate.
    pub entries_omitted: u64,
    /// Encoded native path bytes retained in entries, excluding allocation overhead.
    pub retained_path_bytes: usize,
    /// Retention stopped because the next path would exceed the byte budget.
    pub path_bytes_limit_reached: bool,
}

/// Optional dates indexed exactly like `inventory.entries` on return.
/// No native path is copied into the date table. Public legacy structs stay unchanged.
#[derive(Debug)]
pub struct InventoryWithDates {
    pub inventory: InventoryReport,
    /// One record per retained entry, including NotCollected for ignored kinds.
    /// This table never includes omitted nodes. Keep it paired with its inventory.
    pub dates: Vec<EntryDates>,
}

/// Properties requested during the single traversal. Both are opt-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataSelection {
    pub dates: bool,
    pub allocation: bool,
}

/// Optional tables paired with exactly this inventory, never with another scan.
/// A disabled table is empty; an enabled table has one record per retained node.
/// Allocation records describe regular files only, without any physical aggregate.
#[derive(Debug)]
pub struct InventoryWithMetadata {
    pub inventory: InventoryReport,
    pub dates: Vec<EntryDates>,
    pub allocations: Vec<EntryAllocation>,
}

/// Additive selection; existing `MetadataSelection` literals remain valid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OwnerSelection {
    pub metadata: MetadataSelection,
    pub owner: bool,
}
impl From<MetadataSelection> for OwnerSelection {
    fn from(metadata: MetadataSelection) -> Self {
        Self {
            metadata,
            owner: false,
        }
    }
}

/// Owner table aligned with `metadata.inventory.entries`. Disabled means empty;
/// ignored kinds are NotCollected; omitted nodes have no owner record.
#[derive(Debug)]
pub struct InventoryWithOwners {
    pub metadata: InventoryWithMetadata,
    pub owners: Vec<EntryOwner>,
}

/// Native owners and recursive allocation totals from the SAME traversal.
/// All retained tables use exactly `allocated.metadata.inventory.entries` IDs.
/// Owners are empty when disabled; no owner acquisition is then requested.
#[derive(Debug)]
pub struct InventoryWithOwnersAndAllocationTotals {
    pub allocated: InventoryWithAllocationTotals,
    pub owners: Vec<EntryOwner>,
}

pub fn scan_inventory_with_owners_and_allocation_totals(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    aggregation: AllocationAggregationOptions,
    collect_owner: bool,
    progress: impl FnMut(&Progress),
) -> Result<InventoryWithOwnersAndAllocationTotals, ScanFailure> {
    let (collected, aggregates) = scan_inventory_collect(
        root,
        options,
        cancellation,
        limits,
        OwnerSelection {
            metadata: aggregation.metadata,
            owner: collect_owner,
        },
        Some(aggregation),
        progress,
    )?;
    let (totals, entries_by_path) = aggregates.expect("aggregation requested");
    Ok(InventoryWithOwnersAndAllocationTotals {
        allocated: InventoryWithAllocationTotals {
            metadata: collected.metadata,
            totals,
            entries_by_path,
        },
        owners: collected.owners,
    })
}

/// Collect native owners, dates and/or allocation in one existing traversal.
/// Linux UID acquisition reuses metadata, without additional system calls.
/// Windows uses a read-only native security handle and owns the binary SID.
/// Other providers return Unsupported for requested file/directory owners.
pub fn scan_inventory_with_owners(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    selection: OwnerSelection,
    progress: impl FnMut(&Progress),
) -> Result<InventoryWithOwners, ScanFailure> {
    scan_inventory_collect(
        root,
        options,
        cancellation,
        limits,
        selection,
        None,
        progress,
    )
    .map(|(result, _)| result)
}

/// Bounds retained inventory payload, not process RSS or the walker's buffers.
#[derive(Debug, Clone, Copy)]
pub struct InventoryLimits {
    pub max_entries: usize,
    /// Sum of `OsStr::as_encoded_bytes().len()` over retained paths. Native
    /// encoding is preserved; this does not require UTF-8 or POSIX paths.
    pub max_path_bytes: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_path_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Retain at most `max_entries` nodes while scanning every accessible entry.
/// Directory totals include files whose nodes could not be retained. A truncated
/// display does not change the summary coverage. Uses the default native path
/// byte budget; see `scan_inventory_with_limits` to configure both bounds.
pub fn scan_inventory(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    max_entries: usize,
    progress: impl FnMut(&Progress),
) -> Result<InventoryReport, ScanFailure> {
    scan_inventory_with_limits(
        root,
        options,
        cancellation,
        InventoryLimits {
            max_entries,
            ..InventoryLimits::default()
        },
        progress,
    )
}

/// Scan every accessible entry while retaining only a bounded prefix of nodes.
/// Directory totals include omitted descendants. Once either bound is reached,
/// retention stays stopped: later short paths must not create holes or change
/// parent relationships. The directory lookup indexes paths owned by entries;
/// hash collisions always compare the complete native path. Traversal, errors
/// and GUI projections have separate costs.
pub fn scan_inventory_with_limits(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    progress: impl FnMut(&Progress),
) -> Result<InventoryReport, ScanFailure> {
    scan_inventory_with_metadata(
        root,
        options,
        cancellation,
        limits,
        MetadataSelection::default(),
        progress,
    )
    .map(|result| result.inventory)
}

/// Explicitly collect modification/access dates during the same local traversal.
/// Reuses `symlink_metadata`; does not read file contents or follow symlinks.
/// Only retained entries own date records, under the existing inventory limits.
/// Known dates stay exact observations, independent of incomplete subtree totals.
pub fn scan_inventory_with_dates(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    progress: impl FnMut(&Progress),
) -> Result<InventoryWithDates, ScanFailure> {
    scan_inventory_with_metadata(
        root,
        options,
        cancellation,
        limits,
        MetadataSelection {
            dates: true,
            allocation: false,
        },
        progress,
    )
    .map(|result| InventoryWithDates {
        inventory: result.inventory,
        dates: result.dates,
    })
}

/// Collect requested dates and/or native allocation in the same bounded scan.
/// No second enumeration, content reads or post-scan metadata lookups occur.
/// Logical totals and Top 100 include omitted nodes. Allocation is per file path,
/// not unique physical space or reclaimable bytes; directory records are
/// NotCollected. Identity and link count allow best-effort hardlink recognition
/// within this scan, without retaining an unbounded set of identities.
pub fn scan_inventory_with_metadata(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    selection: MetadataSelection,
    progress: impl FnMut(&Progress),
) -> Result<InventoryWithMetadata, ScanFailure> {
    scan_inventory_collect(
        root,
        options,
        cancellation,
        limits,
        selection.into(),
        None,
        progress,
    )
    .map(|(result, _)| result.metadata)
}

/// Aggregate every observed regular file, independently of inventory retention.
/// Always acquires allocation in the existing traversal; raw date/allocation
/// retention is selected separately. Identity deduplication is scan-wide only,
/// bounded by max_identities, and never represents unique physical blocks.
pub fn scan_inventory_with_allocation_totals(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    aggregation: AllocationAggregationOptions,
    progress: impl FnMut(&Progress),
) -> Result<InventoryWithAllocationTotals, ScanFailure> {
    let (metadata, aggregates) = scan_inventory_collect(
        root,
        options,
        cancellation,
        limits,
        aggregation.metadata.into(),
        Some(aggregation),
        progress,
    )?;
    let metadata = metadata.metadata;
    let (totals, entries_by_path) = aggregates.expect("aggregation requested");
    Ok(InventoryWithAllocationTotals {
        metadata,
        totals,
        entries_by_path,
    })
}

type CollectedInventory = (
    InventoryWithOwners,
    Option<(AllocationTotals, Vec<Metadata<u64>>)>,
);
fn scan_inventory_collect(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    selection: OwnerSelection,
    aggregation: Option<AllocationAggregationOptions>,
    progress: impl FnMut(&Progress),
) -> Result<CollectedInventory, ScanFailure> {
    if limits.max_entries == 0 || limits.max_path_bytes == 0 {
        return Err(ScanFailure::InvalidOptions(
            "max_entries et max_path_bytes doivent être supérieurs à zéro".into(),
        ));
    }
    let mut builder = InventoryBuilder::new(limits);
    builder.aggregates = aggregation.map(AllocationBuilder::new);
    builder.owners = selection.owner.then(Vec::new);
    builder.dates = selection.metadata.dates.then(Vec::new);
    builder.allocations = selection.metadata.allocation.then(Vec::new);
    let report = scan_observed(
        root,
        options,
        cancellation,
        progress,
        CollectFields {
            owner: selection.owner,
            dates: selection.metadata.dates,
            allocation: selection.metadata.allocation || aggregation.is_some(),
            ..Default::default()
        },
        |path, observation| {
            builder.observe(path, observation);
        },
    )?;
    let owners = builder.owners.take().unwrap_or_default();
    let dates = builder.dates.take().unwrap_or_default();
    let allocations = builder.allocations.take().unwrap_or_default();
    let aggregate_builder = builder.aggregates.take();
    let inventory = builder.finish(report);
    let aggregates = aggregate_builder.map(|builder| builder.finish(&inventory));
    Ok((
        InventoryWithOwners {
            metadata: InventoryWithMetadata {
                inventory,
                dates,
                allocations,
            },
            owners,
        },
        aggregates,
    ))
}

// Keep optional fixed-size tables' requested capacity within the entry budget.
pub(crate) fn reserve_record<T>(table: &mut Vec<T>, max_entries: usize) {
    if table.len() == table.capacity() {
        let capacity = table.capacity().max(4).saturating_mul(2).min(max_entries);
        table.reserve_exact(capacity - table.len());
    }
}

struct InventoryBuilder {
    entries: Vec<InventoryEntry>,
    owners: Option<Vec<EntryOwner>>,
    dates: Option<Vec<EntryDates>>,
    allocations: Option<Vec<EntryAllocation>>,
    aggregates: Option<AllocationBuilder>,
    top_files: TopFilesBuilder,
    directories: DirectoryLookup,
    limits: InventoryLimits,
    omitted: u64,
    path_bytes: usize,
    path_limit_reached: bool,
}

/// Index paths already owned by entries. Hashes only choose a bucket: native
/// equality remains authoritative, including the (rare) collision chain.
#[derive(Default)]
struct DirectoryLookup {
    hasher: RandomState,
    heads: HashMap<u64, usize>,
    collisions: HashMap<usize, usize>,
}
impl DirectoryLookup {
    fn insert(&mut self, path: &Path, index: usize) {
        self.insert_key(self.hasher.hash_one(path), index);
    }
    fn insert_key(&mut self, key: u64, index: usize) {
        if let Some(previous) = self.heads.insert(key, index) {
            self.collisions.insert(index, previous);
        }
    }
    fn get(&self, path: &Path, entries: &[InventoryEntry]) -> Option<usize> {
        self.get_key(self.hasher.hash_one(path), path, entries)
    }
    fn get_key(&self, key: u64, path: &Path, entries: &[InventoryEntry]) -> Option<usize> {
        let mut index = *self.heads.get(&key)?;
        loop {
            if entries[index].path == path {
                return Some(index);
            }
            index = *self.collisions.get(&index)?;
        }
    }
}

impl InventoryBuilder {
    fn new(limits: InventoryLimits) -> Self {
        Self {
            entries: Vec::new(),
            owners: None,
            dates: None,
            allocations: None,
            aggregates: None,
            top_files: TopFilesBuilder::default(),
            directories: DirectoryLookup::default(),
            limits,
            omitted: 0,
            path_bytes: 0,
            path_limit_reached: false,
        }
    }

    fn directory_ancestor(&self, path: &Path) -> Option<usize> {
        path.ancestors()
            .find_map(|p| self.directories.get(p, &self.entries))
    }

    fn observe(&mut self, path: &Path, observation: &Observation) {
        let parent = path.parent().and_then(|p| self.directory_ancestor(p));
        let (kind, logical_bytes, incomplete) = match observation {
            Observation::File {
                bytes, allocation, ..
            } => {
                if let Some(aggregates) = &mut self.aggregates {
                    aggregates.observe_file(parent, *bytes, *allocation);
                }
                if let Some(parent) = parent {
                    // The shared summary rejects total overflow before returning
                    // an inventory. No successful subtree sum can overflow either.
                    let total = self.entries[parent].logical_bytes.as_mut().unwrap();
                    *total = total.saturating_add(*bytes);
                }
                (InventoryKind::File, Some(*bytes), false)
            }
            Observation::Directory {
                depth_limited,
                resource_limited,
                ..
            } => (
                InventoryKind::Directory,
                Some(0),
                *depth_limited || *resource_limited,
            ),
            Observation::Symlink => (InventoryKind::Symlink, None, false),
            Observation::Special => (InventoryKind::Special, None, false),
            Observation::Error(_) => {
                if let Some(index) = self.directory_ancestor(path) {
                    self.entries[index].incomplete = true;
                }
                return;
            }
            Observation::Excluded => return,
        };
        if let Some(parent) = parent {
            let direct = path.parent() == Some(self.entries[parent].path.as_path());
            let counts = self.entries[parent].counts.as_mut().unwrap();
            match kind {
                InventoryKind::File => {
                    counts.recursive_files += 1;
                    counts.direct_files += u64::from(direct);
                }
                InventoryKind::Directory => {
                    counts.recursive_directories += 1;
                    counts.direct_directories += u64::from(direct);
                }
                _ => {}
            }
        }
        let path_bytes = path.as_os_str().as_encoded_bytes().len();
        if self.entries.len() < self.limits.max_entries
            && path_bytes > self.limits.max_path_bytes - self.path_bytes
        {
            self.path_limit_reached = true;
        }
        let retain = self.entries.len() < self.limits.max_entries && !self.path_limit_reached;
        // Every file reaches the global ranking before any retention return.
        // The optional index is only a navigation hint, never a ranking input.
        if kind == InventoryKind::File {
            self.top_files.observe(
                path,
                logical_bytes.unwrap(),
                retain.then_some(self.entries.len()),
            );
        }
        if !retain {
            if incomplete && let Some(parent) = parent {
                self.entries[parent].incomplete = true;
            }
            self.omitted += 1;
            return;
        }
        self.path_bytes += path_bytes;
        if kind == InventoryKind::Directory {
            self.directories.insert(path, self.entries.len());
        }
        if let Some(owners) = &mut self.owners {
            reserve_record(owners, self.limits.max_entries);
            let value = match observation {
                Observation::File { owner, .. } | Observation::Directory { owner, .. } => {
                    owner.as_ref().map(crate::owner::retained)
                }
                _ => None,
            };
            owners.push(value.unwrap_or_else(EntryOwner::not_collected));
        }
        if let Some(dates) = &mut self.dates {
            reserve_record(dates, self.limits.max_entries);
            let value = match observation {
                Observation::File { dates, .. } | Observation::Directory { dates, .. } => *dates,
                _ => None,
            };
            dates.push(value.unwrap_or_else(EntryDates::not_collected));
        }
        if let Some(allocations) = &mut self.allocations {
            reserve_record(allocations, self.limits.max_entries);
            let value = match observation {
                Observation::File { allocation, .. } => *allocation,
                _ => None,
            };
            allocations.push(value.unwrap_or_else(EntryAllocation::not_collected));
        }
        if let Some(aggregates) = &mut self.aggregates {
            let allocation = match observation {
                Observation::File { allocation, .. } => *allocation,
                _ => None,
            };
            aggregates.retain(kind, allocation, self.limits.max_entries);
        }
        self.entries.push(InventoryEntry {
            path: path.to_owned(),
            parent,
            kind,
            logical_bytes,
            incomplete,
            counts: (kind == InventoryKind::Directory).then(DirectoryCounts::default),
        });
    }

    fn finish(mut self, report: ScanReport) -> InventoryReport {
        // Parents precede children, so reverse propagation is linear and avoids
        // recursive calls even for very deep directory trees.
        for index in (0..self.entries.len()).rev() {
            if self.entries[index].kind != InventoryKind::Directory {
                continue;
            }
            if report.coverage == Coverage::Cancelled {
                self.entries[index].incomplete = true;
            }
            if let Some(parent) = self.entries[index].parent {
                let bytes = self.entries[index].logical_bytes.unwrap_or(0);
                let incomplete = self.entries[index].incomplete;
                let counts = self.entries[index].counts.unwrap();
                let ancestor = &mut self.entries[parent];
                let ancestor_counts = ancestor.counts.as_mut().unwrap();
                ancestor_counts.recursive_files += counts.recursive_files;
                ancestor_counts.recursive_directories += counts.recursive_directories;
                let total = ancestor.logical_bytes.as_mut().unwrap();
                *total = total.saturating_add(bytes);
                ancestor.incomplete |= incomplete;
            }
        }
        if report.coverage == Coverage::Partial
            && let Some(root) = self.entries.first_mut()
        {
            root.incomplete = true;
        }
        InventoryReport {
            top_files: self.top_files.finish(),
            report,
            entries: self.entries,
            entries_omitted: self.omitted,
            retained_path_bytes: self.path_bytes,
            path_bytes_limit_reached: self.path_limit_reached,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScanIssue;
    use std::io;

    fn report(coverage: Coverage) -> ScanReport {
        let fixture = tempfile::TempDir::new().unwrap();
        let mut report = crate::scan(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            |_| {},
        )
        .unwrap();
        report.coverage = coverage;
        report
    }

    #[test]
    fn injected_owner_states_and_metadata_failures_preserve_alignment() {
        use crate::{MetadataValue, NativeOwnerId, ValueAccuracy};
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 10,
            ..Default::default()
        });
        builder.owners = Some(Vec::new());
        directory(&mut builder, Path::new("root"));
        for state in [
            MetadataValue::Known {
                value: NativeOwnerId::UnixUid(0),
                accuracy: ValueAccuracy::Exact,
            },
            MetadataValue::Unknown,
            MetadataValue::Unsupported,
            MetadataValue::AccessDenied,
            MetadataValue::NotCollected,
        ] {
            let path = Path::new("root").join(builder.entries.len().to_string());
            builder.observe(
                &path,
                &Observation::File {
                    bytes: 0,
                    hardlinked: None,
                    dates: None,
                    allocation: None,
                    owner: Some(crate::owner::observed(EntryOwner { id: state })),
                },
            );
            assert_eq!(builder.owners.as_ref().unwrap().last().unwrap().id, state);
        }
        let count = builder.entries.len();
        // Injected failures, not claims of native access denial/deletion.
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
            builder.observe(
                Path::new("root/gone"),
                &Observation::Error(ScanIssue::io(
                    Path::new("root/gone"),
                    "metadata",
                    &io::Error::from(kind),
                )),
            );
        }
        assert_eq!(builder.entries.len(), count);
        assert_eq!(builder.owners.as_ref().unwrap().len(), count);
    }

    fn directory(builder: &mut InventoryBuilder, path: &Path) {
        builder.observe(
            path,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
                dates: None,
                owner: None,
            },
        );
    }

    #[test]
    fn omitted_unknown_allocation_invalidates_only_its_ancestors() {
        use crate::{LocalFileId, MetadataValue, ValueAccuracy};
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 3,
            ..Default::default()
        });
        builder.aggregates = Some(AllocationBuilder::new(AllocationAggregationOptions {
            max_identities: 4,
            ..Default::default()
        }));
        for path in ["root", "root/bad", "root/good"] {
            directory(&mut builder, Path::new(path));
        }
        for (i, path) in ["root/bad/f", "root/good/f"].into_iter().enumerate() {
            builder.observe(
                Path::new(path),
                &Observation::File {
                    bytes: 1,
                    hardlinked: None,
                    dates: None,
                    owner: None,
                    allocation: Some(EntryAllocation {
                        allocated_bytes: if i == 0 {
                            MetadataValue::AccessDenied
                        } else {
                            MetadataValue::Known {
                                value: 8192,
                                accuracy: ValueAccuracy::Exact,
                            }
                        },
                        identity: MetadataValue::Known {
                            value: LocalFileId::Unix {
                                device: 1,
                                inode: i as u64,
                            },
                            accuracy: ValueAccuracy::Exact,
                        },
                        link_count: MetadataValue::Known {
                            value: 1,
                            accuracy: ValueAccuracy::Exact,
                        },
                    }),
                },
            );
        }
        let aggregate_builder = builder.aggregates.take().unwrap();
        let inventory = builder.finish(report(Coverage::Complete));
        let (totals, entries) = aggregate_builder.finish(&inventory);
        assert_eq!(inventory.entries_omitted, 2);
        assert_eq!(inventory.entries[0].logical_bytes, Some(2));
        assert_eq!(entries[0].value, MetadataValue::AccessDenied);
        assert_eq!(entries[1].value, MetadataValue::AccessDenied);
        assert_eq!(
            entries[2].value,
            MetadataValue::Known {
                value: 8192,
                accuracy: ValueAccuracy::Exact
            }
        );
        assert_eq!(entries[0].coverage, Coverage::Complete);
        assert_eq!(totals.by_path.value, MetadataValue::AccessDenied);
        assert_eq!(totals.by_identity.value, MetadataValue::AccessDenied);
    }

    #[test]
    fn unavailable_allocations_survive_retention_without_becoming_zero() {
        use crate::MetadataValue;
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 5,
            ..Default::default()
        });
        builder.allocations = Some(Vec::new());
        directory(&mut builder, Path::new("root"));
        for (i, state) in [
            MetadataValue::Unknown,
            MetadataValue::Unsupported,
            MetadataValue::AccessDenied,
            MetadataValue::NotCollected,
        ]
        .into_iter()
        .enumerate()
        {
            builder.observe(
                &Path::new("root").join(i.to_string()),
                &Observation::File {
                    bytes: 0,
                    hardlinked: None,
                    dates: None,
                    owner: None,
                    allocation: Some(EntryAllocation {
                        allocated_bytes: state,
                        identity: MetadataValue::Unknown,
                        link_count: MetadataValue::Unknown,
                    }),
                },
            );
            assert_eq!(
                builder.allocations.as_ref().unwrap()[i + 1].allocated_bytes,
                state
            );
        }
        assert_eq!(
            builder.allocations.as_ref().unwrap().len(),
            builder.entries.len()
        );
    }

    #[test]
    fn deep_counts_propagate_iteratively_with_and_without_retention() {
        for max_entries in [1, 129, 1025] {
            let mut builder = InventoryBuilder::new(InventoryLimits {
                max_entries,
                ..Default::default()
            });
            let mut path = PathBuf::from("root");
            directory(&mut builder, &path);
            for _ in 0..512 {
                path.push("d");
                directory(&mut builder, &path);
                builder.observe(
                    &path.join("f"),
                    &Observation::File {
                        bytes: 1,
                        hardlinked: None,
                        dates: None,
                        owner: None,
                        allocation: None,
                    },
                );
            }
            let result = builder.finish(report(Coverage::Complete));
            assert_eq!(result.entries[0].counts.unwrap().recursive_files, 512);
            assert_eq!(result.entries[0].counts.unwrap().recursive_directories, 512);
            for entry in result
                .entries
                .iter()
                .filter(|e| e.kind == InventoryKind::Directory)
            {
                let depth = entry.path.components().count() as u64 - 1;
                let c = entry.counts.unwrap();
                assert_eq!(c.direct_files, u64::from(depth > 0));
                assert_eq!(c.direct_directories, u64::from(depth < 512));
                assert_eq!(c.recursive_files, 512 - depth + u64::from(depth > 0));
                assert_eq!(c.recursive_directories, 512 - depth);
                assert!(!entry.incomplete);
            }
        }
    }

    #[test]
    fn access_errors_propagate_counts_and_partial_state_from_omitted_subtrees() {
        for max_entries in [1, 10] {
            let mut builder = InventoryBuilder::new(InventoryLimits {
                max_entries,
                ..Default::default()
            });
            let root = Path::new("root");
            let child = root.join("child");
            directory(&mut builder, root);
            directory(&mut builder, &child);
            builder.observe(
                &child.join("seen"),
                &Observation::File {
                    bytes: 0,
                    hardlinked: None,
                    dates: None,
                    owner: None,
                    allocation: None,
                },
            );
            builder.observe(
                &child.join("unreadable"),
                &Observation::Error(ScanIssue::io(
                    &child,
                    "metadata",
                    &io::Error::from(io::ErrorKind::PermissionDenied),
                )),
            );
            let result = builder.finish(report(Coverage::Partial));
            let c = result.entries[0].counts.unwrap();
            assert_eq!(
                c,
                DirectoryCounts {
                    direct_files: 0,
                    direct_directories: 1,
                    recursive_files: 1,
                    recursive_directories: 1
                }
            );
            assert!(
                result
                    .entries
                    .iter()
                    .filter(|e| e.kind == InventoryKind::Directory)
                    .all(|e| e.incomplete)
            );
        }
    }

    #[test]
    fn directory_hash_collisions_preserve_exact_native_identity() {
        let entries: Vec<_> = ["fixture", "fixture/été", "fixture/別"]
            .into_iter()
            .map(|path| InventoryEntry {
                path: path.into(),
                parent: None,
                kind: InventoryKind::Directory,
                logical_bytes: Some(0),
                incomplete: false,
                counts: Some(DirectoryCounts::default()),
            })
            .collect();
        let mut lookup = DirectoryLookup::default();
        for index in 0..entries.len() {
            lookup.insert_key(7, index);
        }
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(lookup.get_key(7, &entry.path, &entries), Some(index));
        }
        assert_eq!(
            lookup.get_key(7, Path::new("fixture/missing"), &entries),
            None
        );
        assert_eq!(lookup.get_key(8, &entries[0].path, &entries), None);
    }

    #[test]
    fn inaccessible_directory_marks_its_ancestors_incomplete() {
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 10,
            ..InventoryLimits::default()
        });
        let root = Path::new("fixture");
        let child = root.join("blocked");
        builder.observe(
            root,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
                dates: None,
                owner: None,
            },
        );
        builder.observe(
            &child,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
                dates: None,
                owner: None,
            },
        );
        builder.observe(
            &child,
            &Observation::Error(ScanIssue::io(
                &child,
                "list",
                &io::Error::from(io::ErrorKind::PermissionDenied),
            )),
        );
        assert!(builder.entries[1].incomplete);
        assert_eq!(builder.entries[1].logical_bytes, Some(0));
    }

    #[test]
    fn path_budget_stops_retention_even_if_later_paths_are_shorter() {
        let root = Path::new("r");
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 100,
            max_path_bytes: 8,
        });
        builder.observe(
            root,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
                dates: None,
                owner: None,
            },
        );
        builder.observe(
            Path::new("r/long-name"),
            &Observation::File {
                bytes: 3,
                hardlinked: None,
                dates: None,
                owner: None,
                allocation: None,
            },
        );
        builder.observe(
            Path::new("r/a"),
            &Observation::File {
                bytes: 5,
                hardlinked: None,
                dates: None,
                owner: None,
                allocation: None,
            },
        );
        assert_eq!(builder.entries.len(), 1);
        assert_eq!(builder.entries[0].logical_bytes, Some(8));
        assert_eq!(builder.omitted, 2);
        assert!(builder.path_limit_reached);
    }
}
