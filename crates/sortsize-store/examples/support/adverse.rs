#[cfg(target_os = "linux")]
pub fn memory() -> u64 {
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
pub fn memory() -> u64 {
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
pub fn memory() -> u64 {
    0
}

/// Fixed seed and SplitMix64's bijection: random-looking full-width u64 values.
/// The run oracle checks monotonicity and a membership bitset, not an inverse
/// of this generator or a copy of the engine's ordering algorithm.
pub fn value(pattern: &str, i: u64, n: u64) -> Option<u64> {
    match pattern {
        "random" => {
            let mut z = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            Some(z ^ (z >> 31))
        }
        "reverse" => Some(n - i - 1),
        "equal" => Some(42),
        "unknown" => None,
        "extreme" => match i % 7 {
            0 => None,
            1 => Some(0),
            2 => Some(1),
            3 => Some(i64::MAX as u64),
            4 => Some(1u64 << 63),
            5 => Some(u64::MAX - 1),
            _ => Some(u64::MAX),
        },
        _ => panic!("unknown fixture pattern"),
    }
}
pub fn pattern_ok(pattern: &str) -> bool {
    matches!(
        pattern,
        "random" | "reverse" | "equal" | "unknown" | "extreme"
    )
}
