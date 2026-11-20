//! Native readings and actual SQLite opens, plus explicitly simulated admission bounds.
use sortsize_core::{
    inventory_contract::*,
    resources::{Category, Governor, MemoryReading, native_memory},
};
use sortsize_store::{Durability, inventory::Store};
use std::time::Instant;
#[cfg(target_os = "linux")]
fn memory() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|s| s.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024
}
#[cfg(windows)]
fn memory() -> u64 {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    // SAFETY: valid output struct and current-process handle.
    unsafe {
        let mut m: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        assert_ne!(
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut m,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32
            ),
            0
        );
        m.PeakWorkingSetSize as u64
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
fn memory() -> u64 {
    0
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for i in 0..3 {
        let reading = native_memory();
        let old_budget = sortsize_core::resources::Limits::calibrate(MemoryMode::Auto, reading);
        let t = tempfile::tempdir()?;
        let begin = Instant::now();
        let s = Store::create(
            &t.path().join("inventory"),
            InventoryId(i),
            SourceId(i),
            Durability::Retained,
        )?;
        println!(
            "{}",
            serde_json::json!({"case":"native_open","iteration":i,"native_total":reading.map(|r|r.total),"native_available_lower_bound":reading.map(|r|r.available),"old_fixed_8m_admissible":old_budget.categories[Category::Sqlite as usize]>=8*1024*1024,"sqlite_cache_bytes":s.sqlite_cache_bytes()?,"open_ms":begin.elapsed().as_secs_f64()*1000.,"process_peak_rss_bytes":memory()})
        );
    }
    for available_mib in [32, 64, 128, 256, 512] {
        let g = Governor::new(
            MemoryMode::Auto,
            Some(MemoryReading {
                total: 32 * 1024 * 1024 * 1024,
                available: available_mib * 1024 * 1024,
            }),
            0,
        );
        let old = g.reserve(Category::Sqlite, 8 * 1024 * 1024, None).is_some();
        let admitted = g.reserve_up_to(Category::Sqlite, 1024 * 1024, 8 * 1024 * 1024);
        println!(
            "{}",
            serde_json::json!({"case":"simulated_available_memory","available_mib":available_mib,"old_admitted":old,"new_reserved":admitted.as_ref().map(|r|r.bytes()),"total_budget":g.usage().limits.bytes})
        );
    }
    Ok(())
}
