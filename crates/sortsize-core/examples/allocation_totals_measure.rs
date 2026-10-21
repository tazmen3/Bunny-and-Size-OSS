//! Opt-in local validation harness; not linked by CLI/GUI. No telemetry.
use sortsize_core::{CancellationToken, InventoryLimits, ScanOptions, ScanReport};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static ALLOCATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static REALLOCATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Counting = Counting;
fn add(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
// SAFETY: exact pointer/layout forwarding to System. Counters do not allocate
// or change allocation ownership, alignment or allocation failure semantics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            add(layout.size());
        }
        result
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc_zeroed(layout) };
        if !result.is_null() {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            add(layout.size());
        }
        result
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            REALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        result
    }
}
type AggregateSample = (
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    bool,
    u64,
    Option<u64>,
);
#[derive(Default)]
struct Tables {
    retained: usize,
    omitted: u64,
    paths: usize,
    dates: Option<(usize, usize)>,
    raw: Option<(usize, usize)>,
    aggregates: Option<AggregateSample>,
}
fn emit(report: &ScanReport, tables: Tables, start: Instant) {
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let live = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    let allocation_calls = ALLOCATION_CALLS.load(Ordering::Relaxed);
    let reallocation_calls = REALLOCATION_CALLS.load(Ordering::Relaxed);
    println!(
        "{}",
        serde_json::json!({
            "wall_ms":elapsed,"files":report.files,"directories":report.directories,
            "logical_bytes":report.logical_bytes,"coverage":report.coverage,"errors":report.errors_count,
            "workers":report.concurrency.workers,"retained":tables.retained,"omitted":tables.omitted,
            "retained_path_bytes":tables.paths,"dates":tables.dates,"raw":tables.raw,
            "aggregates":tables.aggregates.map(|(len,cap,bytes,limit,tracked,table_capacity,exhausted,path,id)| serde_json::json!({
                "len":len,"capacity":cap,"record_bytes":bytes,"identity_limit":limit,
                "tracked":tracked,"table_capacity":table_capacity,"budget_exhausted":exhausted,"by_path":path,"by_identity":id
            })),
            "live_requested_bytes":live,"peak_requested_bytes":peak,
            "allocation_calls":allocation_calls,"reallocation_calls":reallocation_calls,
        })
    );
}
fn tables(inventory: &sortsize_core::InventoryReport) -> Tables {
    Tables {
        retained: inventory.entries.len(),
        omitted: inventory.entries_omitted,
        paths: inventory.retained_path_bytes,
        ..Default::default()
    }
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args[1].to_str().unwrap();
    let root = std::path::Path::new(&args[2]);
    let mut options = ScanOptions::default();
    options.concurrency.workers = args[3].to_str().unwrap().parse().unwrap();
    let token = CancellationToken::default();
    let start = Instant::now();
    // TOTALS_BEGIN: removed only from the exact tranche A baseline harness.
    fn known(value: sortsize_core::MetadataValue<u64>) -> Option<u64> {
        match value {
            sortsize_core::MetadataValue::Known { value, .. } => Some(value),
            _ => None,
        }
    }
    if matches!(mode, "paths" | "identities" | "budget") {
        let result = sortsize_core::scan_inventory_with_allocation_totals(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            sortsize_core::AllocationAggregationOptions {
                metadata: sortsize_core::MetadataSelection {
                    dates: true,
                    allocation: true,
                },
                deduplicate_by_identity: mode != "paths",
                max_identities: if mode == "budget" { 128 } else { 65_536 },
            },
            |_| {},
        )
        .unwrap();
        let inventory = &result.metadata.inventory;
        let mut t = tables(inventory);
        t.dates = Some((
            result.metadata.dates.len(),
            result.metadata.dates.capacity(),
        ));
        t.raw = Some((
            result.metadata.allocations.len(),
            result.metadata.allocations.capacity(),
        ));
        let stats = result.totals.identity_tracking;
        assert!(
            !stats.unavailable_metadata
                && !stats.inconsistent_observations
                && !stats.allocation_failed
        );
        t.aggregates = Some((
            result.entries_by_path.len(),
            result.entries_by_path.capacity(),
            std::mem::size_of::<sortsize_core::Metadata<u64>>(),
            stats.limit,
            stats.retained_identities,
            stats.capacity,
            stats.budget_exhausted,
            known(result.totals.by_path.value).unwrap(),
            known(result.totals.by_identity.value),
        ));
        emit(&inventory.report, t, start);
        std::hint::black_box(result);
        return;
    }
    // TOTALS_END
    if mode == "summary" {
        let report = sortsize_core::scan(root, &options, &token, |_| {}).unwrap();
        emit(&report, Tables::default(), start);
        std::hint::black_box(report);
    } else if mode == "inventory" {
        let result = sortsize_core::scan_inventory_with_limits(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        emit(&result.report, tables(&result), start);
        std::hint::black_box(result);
    } else {
        assert!(matches!(mode, "dates" | "both"));
        let result = sortsize_core::scan_inventory_with_metadata(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            sortsize_core::MetadataSelection {
                dates: true,
                allocation: mode == "both",
            },
            |_| {},
        )
        .unwrap();
        let mut t = tables(&result.inventory);
        t.dates = Some((result.dates.len(), result.dates.capacity()));
        if mode == "both" {
            t.raw = Some((result.allocations.len(), result.allocations.capacity()));
        }
        emit(&result.inventory.report, t, start);
        std::hint::black_box(result);
    }
}
