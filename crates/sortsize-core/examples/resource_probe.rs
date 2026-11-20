//! Synthetic cache pressure, not a disk scan or a 10M-entry resident cache.
use sortsize_core::{
    inventory_contract::*,
    resources::{Cache, Governor, MemoryReading},
};
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

fn main() {
    let start = Instant::now();
    let g = Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 32768,
            available: 8192,
        }),
        0,
    );
    let mut cache = Cache::new(g.clone());
    let mut rss1 = 0;
    let mut bytes1 = 0;
    for i in 0..10_000_000 {
        let k = EntryKey {
            inventory: InventoryId(1),
            generation: GenerationId(1),
            entry: EntryId(i),
        };
        let _ = cache.insert(k, i, 8, false);
        if i == 999_999 {
            rss1 = memory();
            bytes1 = g.usage().categories[0];
        }
    }
    println!(
        "{}",
        serde_json::json!({"synthetic":true,"admissions":10000000,"resident_entries":cache.len(),"accounted_at_1m":bytes1,"accounted_at_10m":g.usage().categories[0],"peak_rss_at_1m":rss1,"peak_rss_at_10m":memory(),"elapsed_ms":start.elapsed().as_secs_f64()*1000.})
    );
}
