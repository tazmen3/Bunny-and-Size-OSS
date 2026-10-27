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

/// Borrowed inventory during a scan. Paths and optional tables are never copied
/// for a notification. Directory values in `entries` are local accumulators;
/// use `aggregate_into` for recursive lower bounds. All directories are provisional.
#[derive(Clone, Copy)]
pub struct InventoryPreview<'a> {
    pub entries: &'a [InventoryEntry],
    pub dates: &'a [EntryDates],
    pub owners: &'a [EntryOwner],
    pub entries_omitted: u64,
    pub direct_children_seen: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewAggregate {
    pub logical_bytes: Option<u64>,
    pub counts: Option<DirectoryCounts>,
}

impl InventoryPreview<'_> {
    /// Reuses caller-owned scratch space, at most one fixed-size record per
    /// retained entry. Does not mutate or consolidate the live inventory twice.
    pub fn aggregate_into(&self, output: &mut Vec<PreviewAggregate>) {
        output.clear();
        output.extend(self.entries.iter().map(|entry| PreviewAggregate {
            logical_bytes: entry.logical_bytes,
            counts: entry.counts,
        }));
        for index in (0..self.entries.len()).rev() {
            if self.entries[index].kind != InventoryKind::Directory {
                continue;
            }
            if let Some(parent) = self.entries[index].parent {
                let child = output[index];
                let ancestor = &mut output[parent];
                if let (Some(total), Some(bytes)) =
                    (&mut ancestor.logical_bytes, child.logical_bytes)
                {
                    *total = total.saturating_add(bytes);
                }
                if let (Some(total), Some(counts)) = (&mut ancestor.counts, child.counts) {
                    total.recursive_files += counts.recursive_files;
                    total.recursive_directories += counts.recursive_directories;
                }
            }
        }
    }
}

/// Additive options for the progressive inventory API. Aggregation remains
/// optional and follows the same single traversal and hardlink policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProgressiveInventoryOptions {
    /// Number of direct children skipped before retention. Valid only for a
    /// one-level listing or direct_children_only retention; root/totals stay present.
    pub child_offset: u64,
    /// Retain only the root and a window of its direct children, while still
    /// traversing descendants for exact recursive aggregates within this scope.
    pub direct_children_only: bool,
    pub limits: InventoryLimits,
    pub selection: OwnerSelection,
    pub aggregation: Option<AllocationAggregationOptions>,
}

pub struct ProgressiveInventory {
    pub collected: InventoryWithOwners,
    pub aggregates: Option<(AllocationTotals, Vec<Metadata<u64>>)>,
}

/// The callback runs synchronously on the caller's scan thread after an
/// observation. Consumers choose their cadence and must not retain borrowed data.
/// Final coverage is exclusively that of the returned report.
pub fn scan_inventory_progressive(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    collection: ProgressiveInventoryOptions,
    progress: impl FnMut(&Progress, InventoryPreview<'_>),
) -> Result<ProgressiveInventory, ScanFailure> {
    let (collected, aggregates) =
        collect_progressive(root, options, cancellation, collection, progress)?;
    Ok(ProgressiveInventory {
        collected,
        aggregates,
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
    let mut progress = progress;
    collect_progressive(
        root,
        options,
        cancellation,
        ProgressiveInventoryOptions {
            limits,
            selection,
            aggregation,
            child_offset: 0,
            direct_children_only: false,
        },
        |p, _| progress(p),
    )
}

fn collect_progressive(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    collection: ProgressiveInventoryOptions,
    mut progress: impl FnMut(&Progress, InventoryPreview<'_>),
) -> Result<CollectedInventory, ScanFailure> {
    let ProgressiveInventoryOptions {
        limits,
        selection,
        aggregation,
        child_offset,
        direct_children_only,
    } = collection;
    if child_offset > 0 && options.max_depth != Some(1) && !direct_children_only {
        return Err(ScanFailure::InvalidOptions(
            "child_offset requires a one-level listing".into(),
        ));
    }
    if limits.max_entries == 0 || limits.max_path_bytes == 0 {
        return Err(ScanFailure::InvalidOptions(
            "max_entries et max_path_bytes doivent être supérieurs à zéro".into(),
        ));
    }
    let mut builder = InventoryBuilder::new(limits);
    builder.skip_children = child_offset;
    builder.direct_children_only = direct_children_only;
    builder.aggregates = aggregation.map(AllocationBuilder::new);
    builder.owners = selection.owner.then(Vec::new);
    builder.dates = selection.metadata.dates.then(Vec::new);
    builder.allocations = selection.metadata.allocation.then(Vec::new);
    let builder = std::cell::RefCell::new(builder);
    let report = scan_observed(
        root,
        options,
        cancellation,
        |p| {
            let builder = builder.borrow();
            progress(
                p,
                InventoryPreview {
                    entries: &builder.entries,
                    dates: builder.dates.as_deref().unwrap_or_default(),
                    owners: builder.owners.as_deref().unwrap_or_default(),
                    entries_omitted: builder.omitted,
                    direct_children_seen: builder.direct_children_seen,
                },
            );
        },
        CollectFields {
            owner: selection.owner,
            dates: selection.metadata.dates,
            allocation: selection.metadata.allocation || aggregation.is_some(),
            ..Default::default()
        },
        |path, observation| {
            builder.borrow_mut().observe(path, observation);
        },
    )?;
    let mut builder = builder.into_inner();
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
    skip_children: u64,
    direct_children_only: bool,
    direct_children_seen: u64,
}

/// Index paths already owned by entries. Hashes only choose a bucket: native
/// equality remains authoritative, including the (rare) collision chain.
#[derive(Default)]
struct DirectoryLookup {
    hasher: RandomState,
    heads: HashMap<u64, usize>,
    collisions: HashMap<usize, usize>,
}
// Canonical UNC share roots may omit the final separator, while parent()
// of a direct child includes it. Use the same borrowed key for those two root
// spellings only. Retained/exported paths, drive-relative C:, case, and all
// non-root components remain native and unchanged. No filesystem access.
fn directory_key(path: &Path) -> &Path {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next()
            && matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
            && match components.next() {
                None => true,
                Some(Component::RootDir) => components.next().is_none(),
                _ => false,
            }
        {
            return Path::new(prefix.as_os_str());
        }
    }
    path
}
impl DirectoryLookup {
    fn insert(&mut self, path: &Path, index: usize) {
        self.insert_key(self.hasher.hash_one(directory_key(path)), index);
    }
    fn insert_key(&mut self, key: u64, index: usize) {
        if let Some(previous) = self.heads.insert(key, index) {
            self.collisions.insert(index, previous);
        }
    }
    fn get(&self, path: &Path, entries: &[InventoryEntry]) -> Option<usize> {
        self.get_key(self.hasher.hash_one(directory_key(path)), path, entries)
    }
    fn get_key(&self, key: u64, path: &Path, entries: &[InventoryEntry]) -> Option<usize> {
        let mut index = *self.heads.get(&key)?;
        loop {
            if directory_key(&entries[index].path) == directory_key(path) {
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
            skip_children: 0,
            direct_children_only: false,
            direct_children_seen: 0,
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
        let direct = parent.is_some_and(|parent| {
            path.parent().is_some_and(|path| {
                directory_key(path) == directory_key(&self.entries[parent].path)
            })
        });
        if let Some(parent) = parent {
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
        let direct_child = parent == Some(0) && direct;
        if direct_child {
            self.direct_children_seen += 1;
        }
        let skip = direct_child && self.skip_children > 0;
        let descendant = self.direct_children_only && parent.is_some() && !direct_child;
        let path_bytes = path.as_os_str().as_encoded_bytes().len();
        if !skip
            && !descendant
            && self.entries.len() < self.limits.max_entries
            && path_bytes > self.limits.max_path_bytes - self.path_bytes
        {
            self.path_limit_reached = true;
        }
        let retain = !skip
            && !descendant
            && self.entries.len() < self.limits.max_entries
            && !self.path_limit_reached;
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
            if skip {
                self.skip_children -= 1;
            }
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

#[cfg(test)]
mod progressive_tests {
    use super::*;

    #[test]
    fn independent_prefix_oracle_checks_every_retained_subtree_at_one_and_eight_workers() {
        use std::collections::{HashMap, HashSet};
        let fixture = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let mut files = HashMap::new();
        let mut directories = HashSet::from([root.clone()]);
        for i in 0..9 {
            let folder = root.join(format!("d{i}")).join("deep");
            std::fs::create_dir_all(&folder).unwrap();
            directories.insert(folder.parent().unwrap().to_owned());
            directories.insert(folder.clone());
            for j in 0..7 {
                let path = folder.join(format!("f{j}"));
                let size = (i * 7 + j + 1) as u64;
                std::fs::write(&path, vec![0; size as usize]).unwrap();
                files.insert(path, size);
            }
        }
        let linked = root.join("hardlink");
        std::fs::hard_link(root.join("d0/deep/f0"), &linked).unwrap();
        files.insert(linked, 1); // Logical totals are by path, including hardlinks.
        let expected_bytes: u64 = files.values().sum();
        for workers in [1, 8] {
            for max_entries in [1, 8, 50_000] {
                for cancel in [false, true] {
                    let token = CancellationToken::default();
                    let mut observed_files = HashMap::new();
                    let mut observed_directories = HashSet::new();
                    let mut scratch = Vec::new();
                    let result = scan_inventory_progressive(
                        &root,
                        &ScanOptions {
                            concurrency: crate::ConcurrencyOptions {
                                workers,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        &token,
                        ProgressiveInventoryOptions {
                            limits: InventoryLimits {
                                max_entries,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        |progress, preview| {
                            // This oracle never uses inventory parent indices or
                            // its accumulators: only known fixture paths observed so far.
                            if let Some(&size) = files.get(&progress.current_path) {
                                assert!(
                                    observed_files
                                        .insert(progress.current_path.clone(), size)
                                        .is_none()
                                );
                            } else if directories.contains(&progress.current_path) {
                                assert!(observed_directories.insert(progress.current_path.clone()));
                            }
                            preview.aggregate_into(&mut scratch);
                            for (entry, aggregate) in preview.entries.iter().zip(&scratch) {
                                if entry.kind != InventoryKind::Directory {
                                    continue;
                                }
                                let expected: u64 = observed_files
                                    .iter()
                                    .filter(|(path, _)| path.starts_with(&entry.path))
                                    .map(|(_, size)| size)
                                    .sum();
                                let counts = aggregate.counts.unwrap();
                                assert_eq!(aggregate.logical_bytes, Some(expected));
                                assert_eq!(
                                    counts.recursive_files,
                                    observed_files
                                        .keys()
                                        .filter(|path| path.starts_with(&entry.path))
                                        .count() as u64
                                );
                                assert_eq!(
                                    counts.recursive_directories,
                                    observed_directories
                                        .iter()
                                        .filter(|path| *path != &entry.path
                                            && path.starts_with(&entry.path))
                                        .count() as u64
                                );
                            }
                            if cancel && progress.files >= 25 {
                                token.cancel();
                            }
                        },
                    )
                    .unwrap();
                    let inventory = result.collected.metadata.inventory;
                    assert_eq!(
                        inventory.entries[0].logical_bytes,
                        Some(observed_files.values().sum())
                    );
                    assert_eq!(inventory.report.files, observed_files.len() as u64);
                    assert_eq!(
                        inventory.report.directories,
                        observed_directories.len() as u64
                    );
                    if cancel {
                        assert_eq!(inventory.report.coverage, Coverage::Cancelled);
                        assert!(inventory.entries[0].incomplete);
                    } else {
                        assert_eq!(inventory.report.coverage, Coverage::Complete);
                        assert_eq!(inventory.report.logical_bytes, expected_bytes);
                        assert_eq!(inventory.report.files, files.len() as u64);
                    }
                }
            }
        }
    }

    #[test]
    fn borrowed_progress_has_recursive_totals_without_mutating_final_consolidation() {
        let fixture = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(fixture.path().join("a/b")).unwrap();
        for (path, bytes) in [("a/x", 7), ("a/b/y", 13), ("z", 23)] {
            std::fs::write(fixture.path().join(path), vec![0; bytes]).unwrap();
        }
        for workers in [1, 8] {
            for limit in [1, 2, 50_000] {
                let mut aggregates = Vec::new();
                let mut last_bytes = 0;
                let mut callbacks = 0;
                let result = scan_inventory_progressive(
                    fixture.path(),
                    &ScanOptions {
                        concurrency: crate::ConcurrencyOptions {
                            workers,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    &CancellationToken::default(),
                    ProgressiveInventoryOptions {
                        child_offset: 0,
                        direct_children_only: false,
                        limits: InventoryLimits {
                            max_entries: limit,
                            ..Default::default()
                        },
                        selection: OwnerSelection {
                            metadata: MetadataSelection {
                                dates: true,
                                allocation: false,
                            },
                            owner: true,
                        },
                        aggregation: None,
                    },
                    |p, preview| {
                        callbacks += 1;
                        preview.aggregate_into(&mut aggregates);
                        assert_eq!(aggregates[0].logical_bytes, Some(p.logical_bytes));
                        assert_eq!(aggregates[0].counts.unwrap().recursive_files, p.files);
                        assert_eq!(
                            aggregates[0].counts.unwrap().recursive_directories,
                            p.directories - 1
                        );
                        assert_eq!(preview.entries.len(), preview.dates.len());
                        assert_eq!(preview.entries.len(), preview.owners.len());
                        assert!(p.logical_bytes >= last_bytes);
                        last_bytes = p.logical_bytes;
                    },
                )
                .unwrap();
                let final_inventory = result.collected.metadata.inventory;
                assert_eq!(callbacks, 6);
                assert_eq!(last_bytes, 43);
                assert_eq!(final_inventory.entries[0].logical_bytes, Some(43));
                assert_eq!(final_inventory.report.coverage, Coverage::Complete);
                assert_eq!(final_inventory.entries.len(), limit.min(6));
                assert_eq!(final_inventory.entries_omitted, (6 - limit.min(6)) as u64);
            }
        }
    }

    #[test]
    fn cancellation_and_path_limits_keep_preview_and_final_lower_bounds() {
        let fixture = tempfile::TempDir::new().unwrap();
        std::fs::write(fixture.path().join("one"), [0; 17]).unwrap();
        std::fs::write(fixture.path().join("two"), [0; 19]).unwrap();
        let token = CancellationToken::default();
        let result = scan_inventory_progressive(
            fixture.path(),
            &ScanOptions::default(),
            &token,
            ProgressiveInventoryOptions {
                limits: InventoryLimits {
                    max_entries: 50_000,
                    max_path_bytes: std::fs::canonicalize(fixture.path())
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .len(),
                },
                ..Default::default()
            },
            |p, preview| {
                assert_eq!(preview.entries.len(), 1);
                if p.files == 1 {
                    token.cancel();
                }
            },
        )
        .unwrap();
        let inventory = result.collected.metadata.inventory;
        assert_eq!(inventory.report.coverage, Coverage::Cancelled);
        assert_eq!(inventory.report.files, 1);
        assert!(inventory.entries[0].incomplete);
        assert!(inventory.path_bytes_limit_reached);
        assert_eq!(
            inventory.entries[0].logical_bytes,
            Some(inventory.report.logical_bytes)
        );
    }
}

#[cfg(test)]
mod child_window_tests {
    use super::*;
    #[test]
    fn child_windows_cover_every_direct_entry_without_double_consolidation() {
        let fixture = tempfile::TempDir::new().unwrap();
        for i in 0..29 {
            std::fs::write(fixture.path().join(format!("f-{i:02}")), [0; 3]).unwrap();
        }
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(fixture.path().join("child/deep"), [0; 91]).unwrap();
        let initial = scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            1,
            |_| {},
        )
        .unwrap();
        assert_eq!(initial.report.logical_bytes, 178);
        let mut seen = std::collections::HashSet::new();
        for offset in (0..30).step_by(4) {
            let result = scan_inventory_progressive(
                fixture.path(),
                &ScanOptions {
                    max_depth: Some(1),
                    ..Default::default()
                },
                &CancellationToken::default(),
                ProgressiveInventoryOptions {
                    child_offset: offset,
                    limits: InventoryLimits {
                        max_entries: 5,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                |_, _| {},
            )
            .unwrap();
            let inventory = result.collected.metadata.inventory;
            assert_eq!(inventory.report.logical_bytes, 87);
            assert_eq!(inventory.entries[0].logical_bytes, Some(87));
            assert!(inventory.entries[0].incomplete);
            for entry in inventory.entries.into_iter().skip(1) {
                assert!(seen.insert(entry.path));
            }
        }
        assert_eq!(seen.len(), 30);
        assert_eq!(initial.entries[0].logical_bytes, Some(178));
        assert!(
            scan_inventory_progressive(
                fixture.path(),
                &ScanOptions::default(),
                &CancellationToken::default(),
                ProgressiveInventoryOptions {
                    child_offset: 1,
                    ..Default::default()
                },
                |_, _| {}
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod recursive_child_window_tests {
    use super::*;
    #[test]
    fn recursive_totals_cover_omitted_descendants_while_only_direct_children_are_retained() {
        let fixture = tempfile::TempDir::new().unwrap();
        for i in 0..12 {
            let folder = fixture.path().join(format!("d{i}"));
            std::fs::create_dir_all(folder.join("deep")).unwrap();
            std::fs::write(folder.join("file"), [0; 3]).unwrap();
            std::fs::write(folder.join("deep/leaf"), [0; 7]).unwrap();
        }
        let mut paths = std::collections::HashSet::new();
        for offset in (0..12).step_by(4) {
            let result = scan_inventory_progressive(
                fixture.path(),
                &ScanOptions::default(),
                &CancellationToken::default(),
                ProgressiveInventoryOptions {
                    child_offset: offset,
                    direct_children_only: true,
                    limits: InventoryLimits {
                        max_entries: 5,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                |_, preview| {
                    assert!(preview.direct_children_seen <= 12);
                },
            )
            .unwrap();
            let inventory = result.collected.metadata.inventory;
            assert_eq!(inventory.entries.len(), 5);
            assert_eq!(inventory.entries[0].logical_bytes, Some(120));
            assert_eq!(inventory.entries[0].counts.unwrap().recursive_files, 24);
            assert_eq!(inventory.entries[0].counts.unwrap().direct_directories, 12);
            assert!(!inventory.entries[0].incomplete);
            for entry in inventory.entries.into_iter().skip(1) {
                assert_eq!(entry.parent, Some(0));
                assert_eq!(entry.logical_bytes, Some(10));
                assert_eq!(entry.counts.unwrap().recursive_files, 2);
                assert!(paths.insert(entry.path));
            }
        }
        assert_eq!(paths.len(), 12);
    }
}

#[cfg(all(test, windows))]
mod unc_parent_tests {
    use super::*;
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
    fn file(builder: &mut InventoryBuilder, path: &Path, bytes: u64) {
        builder.observe(
            path,
            &Observation::File {
                bytes,
                hardlinked: None,
                dates: None,
                allocation: None,
                owner: None,
            },
        );
    }
    #[test]
    fn share_root_parent_links_totals_and_direct_counts_survive_both_spellings() {
        for root in [
            r"\\server\share",
            r"\\server\share\",
            r"\\?\UNC\server\été 日本語",
            r"\\?\UNC\server\été 日本語\",
        ] {
            for max_entries in [1, 10] {
                let root = Path::new(root);
                let mut builder = InventoryBuilder::new(InventoryLimits {
                    max_entries,
                    ..Default::default()
                });
                directory(&mut builder, root);
                directory(&mut builder, &root.join("folder"));
                file(&mut builder, &root.join("copy.txt"), 4);
                file(&mut builder, &root.join("folder/nested.txt"), 6);
                if max_entries > 1 {
                    assert_eq!(builder.entries[1].parent, Some(0));
                    assert_eq!(builder.entries[2].parent, Some(0));
                    assert_eq!(builder.entries[3].parent, Some(1));
                }
                let fixture = tempfile::tempdir().unwrap();
                let report = crate::scan(
                    fixture.path(),
                    &Default::default(),
                    &Default::default(),
                    |_| {},
                )
                .unwrap();
                let result = builder.finish(report);
                assert_eq!(result.entries[0].logical_bytes, Some(10));
                let counts = result.entries[0].counts.unwrap();
                assert_eq!(counts.direct_files, 1);
                assert_eq!(counts.direct_directories, 1);
                assert_eq!(counts.recursive_files, 2);
                assert_eq!(counts.recursive_directories, 1);
            }
        }
    }
    #[test]
    fn share_root_child_offsets_skip_the_requested_direct_child() {
        let root = Path::new(r"\\?\UNC\server\share");
        let mut builder = InventoryBuilder::new(Default::default());
        builder.direct_children_only = true;
        builder.skip_children = 1;
        directory(&mut builder, root);
        file(&mut builder, &root.join("a.txt"), 4);
        file(&mut builder, &root.join("b.txt"), 6);
        assert_eq!(builder.direct_children_seen, 2);
        assert_eq!(builder.entries.len(), 2);
        assert!(builder.entries[1].path.ends_with("b.txt"));
        assert_eq!(builder.entries[1].parent, Some(0));
    }
}

#[cfg(all(test, windows))]
#[test]
fn inventory_share_keys_do_not_merge_drives_other_shares_or_descendants() {
    for (a, b) in [
        (r"C:", r"C:\"),
        (r"\\?\C:", r"\\?\C:\"),
        (r"\\s\a", r"\\s\b"),
        (r"\\s\share", r"\\s\share\child"),
        (r"\\s\share", r"\\other\share"),
    ] {
        assert_ne!(directory_key(Path::new(a)), directory_key(Path::new(b)));
    }
    for (a, b) in [
        (r"\\s\share", r"\\s\share\"),
        (r"\\?\UNC\s\share", r"\\?\UNC\s\share\"),
    ] {
        assert_eq!(directory_key(Path::new(a)), directory_key(Path::new(b)));
    }
}
