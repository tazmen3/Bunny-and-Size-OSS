//! Optional diagnostics of live requested Rust allocation payload, not RSS.
//! No counters or allocator replacement in the production/default benchmark.
#[cfg(feature = "allocation-profiling")]
mod enabled {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub(super) static LIVE: AtomicUsize = AtomicUsize::new(0);
    pub(super) static PEAK: AtomicUsize = AtomicUsize::new(0);
    fn add(bytes: usize) {
        let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
    struct Counting;
    #[global_allocator]
    static ALLOCATOR: Counting = Counting;
    // SAFETY: forwards the exact pointer/layout to System; counters allocate
    // nothing and do not change allocation ownership, alignment or failure.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                add(layout.size());
            }
            pointer
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                add(layout.size());
            }
            pointer
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            unsafe { System.dealloc(pointer, layout) };
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let result = unsafe { System.realloc(pointer, layout, new_size) };
            if !result.is_null() {
                if new_size >= layout.size() {
                    add(new_size - layout.size());
                } else {
                    LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
                }
            }
            result
        }
    }
}
pub(super) fn json() -> String {
    #[cfg(feature = "allocation-profiling")]
    {
        use std::sync::atomic::Ordering;
        format!(
            "{{\"live_requested_bytes\":{},\"peak_requested_bytes\":{}}}",
            enabled::LIVE.load(Ordering::Relaxed),
            enabled::PEAK.load(Ordering::Relaxed)
        )
    }
    #[cfg(not(feature = "allocation-profiling"))]
    {
        "null".into()
    }
}
