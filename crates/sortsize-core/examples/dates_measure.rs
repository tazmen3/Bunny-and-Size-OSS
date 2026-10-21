//! Opt-in local validation harness; not linked by CLI/GUI. No telemetry.
use sortsize_core::{CancellationToken, InventoryLimits, ScanOptions, ScanReport};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
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
            add(layout.size());
        }
        result
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc_zeroed(layout) };
        if !result.is_null() {
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
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        result
    }
}
fn emit(
    report: &ScanReport,
    retained: usize,
    omitted: u64,
    paths: usize,
    dates: Option<(usize, usize, usize)>,
    start: Instant,
) {
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let live = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    println!(
        "{}",
        serde_json::json!({
            "wall_ms":elapsed, "files":report.files,"directories":report.directories,
            "logical_bytes":report.logical_bytes,"coverage":report.coverage,"errors":report.errors_count,
            "workers":report.concurrency.workers,"retained":retained,"omitted":omitted,"retained_path_bytes":paths,
            "dates":dates.map(|(len,capacity,record_bytes)| serde_json::json!({"len":len,"capacity":capacity,"record_bytes":record_bytes,"capacity_bytes":capacity*record_bytes})),
            "live_requested_bytes":live,"peak_requested_bytes":peak,
        })
    );
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args[1].to_str().unwrap();
    let root = std::path::Path::new(&args[2]);
    let mut options = ScanOptions::default();
    options.concurrency.workers = args[3].to_str().unwrap().parse().unwrap();
    let token = CancellationToken::default();
    let start = Instant::now();
    // DATES_BEGIN: removed only from the exact-base copy of this harness.
    if mode == "dates" {
        let result = sortsize_core::scan_inventory_with_dates(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        let inventory = &result.inventory;
        emit(
            &inventory.report,
            inventory.entries.len(),
            inventory.entries_omitted,
            inventory.retained_path_bytes,
            Some((
                result.dates.len(),
                result.dates.capacity(),
                std::mem::size_of::<sortsize_core::EntryDates>(),
            )),
            start,
        );
        std::hint::black_box(result);
        return;
    }
    // DATES_END
    if mode == "summary" {
        let report = sortsize_core::scan(root, &options, &token, |_| {}).unwrap();
        emit(&report, 0, 0, 0, None, start);
        std::hint::black_box(report);
    } else {
        assert_eq!(mode, "inventory");
        let result = sortsize_core::scan_inventory_with_limits(
            root,
            &options,
            &token,
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        emit(
            &result.report,
            result.entries.len(),
            result.entries_omitted,
            result.retained_path_bytes,
            None,
            start,
        );
        std::hint::black_box(result);
    }
}
