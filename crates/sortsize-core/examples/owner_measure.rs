//! Opt-in local validation harness; not linked by CLI/GUI. No telemetry.
use sortsize_core::{CancellationToken, InventoryLimits, ScanOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;
static CALLS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
#[cfg_attr(feature = "owner-heap-profile", global_allocator)]
static ALLOCATOR: Counting = Counting;
fn add(bytes: usize) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
// SAFETY: exact pointer/layout forwarding to System. Counters do not allocate
// or change allocation ownership, alignment or allocation failure semantics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            if ENABLED.load(Ordering::Relaxed) {
                CALLS.fetch_add(1, Ordering::Relaxed);
            }
            add(layout.size());
        }
        result
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc_zeroed(layout) };
        if !result.is_null() {
            if ENABLED.load(Ordering::Relaxed) {
                CALLS.fetch_add(1, Ordering::Relaxed);
            }
            add(layout.size());
        }
        result
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if ENABLED.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            if ENABLED.load(Ordering::Relaxed) {
                CALLS.fetch_add(1, Ordering::Relaxed);
            }
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                if ENABLED.load(Ordering::Relaxed) {
                    LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
                }
            }
        }
        result
    }
}
fn main() {
    std::hint::black_box(&ALLOCATOR);
    ENABLED.store(
        std::env::var_os("SORTSIZE_NO_HEAP_COUNTERS").is_none(),
        Ordering::Relaxed,
    );
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args[1].to_str().unwrap();
    let root = std::path::Path::new(&args[2]);
    let mut options = ScanOptions::default();
    options.concurrency.workers = args[3].to_str().unwrap().parse().unwrap();
    let token = CancellationToken::default();
    let start = Instant::now();
    let metadata;
    // OWNER_BEGIN: removed by validation script for the exact base.
    let mut owner_records = None;
    if mode.contains("owner") {
        let result = sortsize_core::scan_inventory_with_owners(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            sortsize_core::OwnerSelection {
                owner: true,
                metadata: sortsize_core::MetadataSelection {
                    dates: mode.contains("dates"),
                    allocation: mode.contains("allocation"),
                },
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(result.owners.len(), result.metadata.inventory.entries.len());
        assert!(result.owners.capacity() <= InventoryLimits::default().max_entries);
        owner_records = Some(result.owners);
        metadata = result.metadata;
    } else
    // OWNER_END
    {
        metadata = sortsize_core::scan_inventory_with_metadata(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            sortsize_core::MetadataSelection {
                dates: mode.contains("dates"),
                allocation: mode.contains("allocation"),
            },
            |_| {},
        )
        .unwrap();
    }
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
    // Read counters before JSON allocations; keep all returned tables live.
    let live = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    let calls = CALLS.load(Ordering::Relaxed);

    // OWNER_STATS_BEGIN
    let owner_capacity_bytes = owner_records.as_ref().map_or(0, |o| {
        o.capacity() * std::mem::size_of::<sortsize_core::EntryOwner>()
    });
    // OWNER_STATS_END
    let inventory = &metadata.inventory;
    let mut owner_states = [0usize; 5];
    if let Some(owners) = &owner_records {
        for owner in owners {
            use sortsize_core::MetadataValue::*;
            owner_states[match owner.id {
                Known { .. } => 0,
                Unknown => 1,
                Unsupported => 2,
                AccessDenied => 3,
                NotCollected => 4,
                _ => 1,
            }] += 1;
        }
    }
    #[cfg(all(windows, feature = "owner-profiling"))]
    let native_counters = sortsize_core::owner_profile::snapshot();
    #[cfg(not(all(windows, feature = "owner-profiling")))]
    let native_counters: Option<[usize; 5]> = None;
    println!(
        "{}",
        serde_json::json!({
            "wall_ms": elapsed_ms, "files": inventory.report.files,
            "native_counters":native_counters,
            "heap_counters":cfg!(feature = "owner-heap-profile"),
            "owner_states":owner_states,
            "directories": inventory.report.directories, "logical_bytes":inventory.report.logical_bytes,
            "coverage": inventory.report.coverage, "errors":inventory.report.errors_count,
            "retained":inventory.entries.len(), "omitted":inventory.entries_omitted,
            "owner_capacity_bytes":owner_capacity_bytes,
            "live_requested_bytes":live, "peak_requested_bytes":peak, "allocation_calls":calls,
            "entry_allocation_bytes":std::mem::size_of::<sortsize_core::EntryAllocation>(),
        })
    );
    std::hint::black_box(&metadata);
    // OWNER_KEEP_BEGIN
    std::hint::black_box(owner_records);
    // OWNER_KEEP_END
}
