//! Read-only scanning engine. OS acquisition is isolated in the local adapter.

mod allocation;
mod allocation_totals;
mod dates;
mod directory_scope;
pub mod entries;
mod inventory;
pub use directory_scope::validate_directory_scope;
mod local;
mod metadata;
mod model;
mod owner;
#[cfg(all(windows, feature = "owner-profiling"))]
#[doc(hidden)]
pub mod owner_profile;
mod pool;
mod scan;
mod top_files;

pub use top_files::{TOP_FILE_MAX_PATH_BYTES, TOP_FILES_COUNT, TopFile, TopFiles};

pub use allocation::{EntryAllocation, LocalFileId};
pub use allocation_totals::{
    AllocationAggregationOptions, AllocationTotals, IdentityTrackingStats,
    InventoryWithAllocationTotals,
};
pub use dates::EntryDates;
#[cfg(windows)]
pub use owner::WindowsSid;
pub use owner::{EntryOwner, NativeOwnerId};

pub use inventory::{
    DirectoryCounts, InventoryEntry, InventoryKind, InventoryLimits, InventoryPreview,
    InventoryReport, InventoryWithDates, InventoryWithMetadata, InventoryWithOwners,
    InventoryWithOwnersAndAllocationTotals, MetadataSelection, OwnerSelection, PreviewAggregate,
    ProgressiveInventory, ProgressiveInventoryOptions, scan_inventory, scan_inventory_progressive,
    scan_inventory_with_allocation_totals, scan_inventory_with_dates, scan_inventory_with_limits,
    scan_inventory_with_metadata, scan_inventory_with_owners,
    scan_inventory_with_owners_and_allocation_totals,
};
pub use metadata::{Metadata, MetadataValue, ValueAccuracy};

pub use model::{
    CancellationToken, ConcurrencyOptions, Coverage, NativePath, Progress, RawPath, ScanIssue,
    ScanOptions, ScanReport, TraversalLimits, TraversalStats,
};
pub use scan::{ScanFailure, scan};

/// Desktop volume discovery; never substitutes POSIX mounts for Android SAF.
#[cfg(any(target_os = "linux", windows))]
pub mod volumes;
