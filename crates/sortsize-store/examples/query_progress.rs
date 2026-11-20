//! Reuse sealed SYNTHETIC metadata. Does not scan the recorded source.
use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::*};
use sortsize_store::{Durability, inventory::Store};
use std::{path::PathBuf, time::Instant};
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
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().ok_or("inventory directory")?);
    let repeats = args.next().unwrap_or("3".into()).parse::<usize>()?;
    let store = Store::reopen(&dir, Durability::Retained)?;
    let n = store.entry_count(GenerationId(1))?;
    let root = dir.parent().unwrap().join("offline-not-created");
    let cancel = CancellationToken::default();
    for (case, search, expected) in [
        ("all", SearchQuery::default(), n),
        (
            "late",
            SearchQuery {
                name: Some(format!("file-{:010}.txt", n - 2)),
                ..Default::default()
            },
            1,
        ),
        (
            "absent",
            SearchQuery {
                name: Some("__no_matching_name__".into()),
                ..Default::default()
            },
            0,
        ),
    ] {
        for repetition in 0..repeats {
            let start = Instant::now();
            let mut q = store.start_query(
                GenerationId(1),
                &root,
                &search,
                dir.parent().unwrap(),
                2 * 1024 * 1024 * 1024,
            )?;
            let initialization_ms = start.elapsed().as_secs_f64() * 1000.;
            let mut first_result_ms = None;
            let mut first_preview_ms = None;
            let mut steps = Vec::new();
            loop {
                let step = Instant::now();
                let done = q.advance(&cancel)?;
                steps.push(step.elapsed().as_secs_f64() * 1000.);
                if first_preview_ms.is_none() {
                    let p = q.preview()?;
                    first_preview_ms = Some(start.elapsed().as_secs_f64() * 1000.);
                    assert!(!p.complete);
                }
                if first_result_ms.is_none() && q.counts.matches > 0 {
                    let p = q.preview()?;
                    assert!(!p.rows.is_empty());
                    first_result_ms = Some(start.elapsed().as_secs_f64() * 1000.);
                }
                if done {
                    break;
                }
            }
            let materialize_ms = start.elapsed().as_secs_f64() * 1000.;
            assert!(q.complete());
            assert_eq!(q.counts.matches, expected);
            let mut cursor = None;
            let mut count = 0u64;
            let mut sum = 0u128;
            let mut pages = Vec::new();
            let mut last_path = None;
            loop {
                let begin = Instant::now();
                let page = q.page(cursor.as_ref())?;
                pages.push(begin.elapsed().as_secs_f64() * 1000.);
                for row in &page.rows {
                    if let Some(last) = last_path.take() {
                        assert!(row.path > last);
                    }
                    last_path = Some(row.path.clone());
                    sum += u128::from(row.key.entry.0);
                }
                count += page.rows.len() as u64;
                if !page.more {
                    break;
                }
                cursor = page.next;
            }
            assert_eq!(count, expected);
            if case == "all" {
                assert_eq!(sum, u128::from(n) * u128::from(n - 1) / 2);
            }
            steps.sort_by(f64::total_cmp);
            pages.sort_by(f64::total_cmp);
            println!(
                "{}",
                serde_json::json!({"synthetic":true,"source_io":0,"case":case,"repetition":repetition,"entries":n,"initialization_ms":initialization_ms,"first_preview_ms":first_preview_ms,"first_result_ms":first_result_ms,"materialize_ms":materialize_ms,"step_p95_ms":steps[steps.len()*95/100],"step_max_ms":steps.last(),"page_p95_ms":pages[pages.len()*95/100],"page_max_ms":pages.last(),"matches":count,"peak_rss_bytes":memory(),"exactness":"PASS","inventory_durability":"FULL","temporary_result":"WAL/NORMAL"})
            );
        }
    }
    Ok(())
}
