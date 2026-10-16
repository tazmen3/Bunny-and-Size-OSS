//! Read-only scanning engine. OS acquisition is isolated in the local adapter.

mod inventory;
mod local;
mod model;
mod pool;
mod scan;

pub use inventory::{
    InventoryEntry, InventoryKind, InventoryLimits, InventoryReport, scan_inventory,
    scan_inventory_with_limits,
};
pub use model::{
    CancellationToken, ConcurrencyOptions, Coverage, NativePath, Progress, RawPath, ScanIssue,
    ScanOptions, ScanReport, TraversalLimits, TraversalStats,
};
pub use scan::{ScanFailure, scan};
