use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

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

pub fn snapshot() -> (usize, usize, usize) {
    std::hint::black_box(&ALLOCATOR);
    (
        CALLS.load(Ordering::Relaxed),
        LIVE.load(Ordering::Relaxed),
        PEAK.load(Ordering::Relaxed),
    )
}
