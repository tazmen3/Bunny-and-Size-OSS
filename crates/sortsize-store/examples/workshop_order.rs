//! Additional L17 evidence on the existing sealed synthetic corpora.
//! This does not acquire files or replace the frozen exact-first-page threshold.
use sortsize_core::{inventory_contract::*, resources::Governor};
use sortsize_store::{
    Durability,
    inventory::{Store, folder_totals::FolderFold, ordered::Sort},
};
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
    let corpus = PathBuf::from(args.next().ok_or("corpus")?);
    let n = args.next().ok_or("count")?.parse::<u64>()?;
    let mode = args.next().unwrap_or_else(|| "auto".into());
    let quota_mib = args
        .next()
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(512);
    if ![512, 2048].contains(&quota_mib) {
        return Err("512|2048 MiB quota".into());
    }
    let enriched = args.next().as_deref() == Some("enriched");
    if enriched && n > 1_000_000 {
        return Err("enriched sort oracle is bounded to 1M".into());
    }
    let governor = Governor::global();
    governor.configure(
        MemoryMode::from_key(&mode).ok_or("mode")?,
        sortsize_core::resources::native_memory(),
    );
    let start = Instant::now();
    let store = Store::reopen(&corpus.join("inventory"), Durability::Retained)?;
    let open_ms = start.elapsed().as_secs_f64() * 1000.;
    let c = store.acquisition_context(GenerationId(1))?;
    assert_eq!(store.entry_count(c.generation)?, n + 1);
    let root = if enriched {
        store.saved(c.generation)?.root
    } else {
        std::fs::canonicalize(&corpus)?
    };
    let scratch = tempfile::tempdir()?;
    let mut fold = FolderFold::default();
    let started = Instant::now();
    while !store.folder_fold_step(c.generation, &mut fold)? {}
    let fold_ms = started.elapsed().as_secs_f64() * 1000.;
    let mut data = Vec::new();
    for index in 0..12 {
        let _oracle_memory = if enriched {
            Some(
                governor
                    .reserve(sortsize_core::resources::Category::Queries, n * 8, None)
                    .ok_or("oracle memory budget")?,
            )
        } else {
            None
        };
        let expected = if enriched {
            let mut rows = (0..n).collect::<Vec<_>>();
            rows.sort_unstable_by(|a, b| match index {
                0 | 3 | 8 | 10 => b.cmp(a),
                4 => b.div_ceil(4096).cmp(&a.div_ceil(4096)).then(a.cmp(b)),
                6 => (a % 512 % 17).cmp(&(b % 512 % 17)).then(a.cmp(b)),
                7 => (b % 512 % 17).cmp(&(a % 512 % 17)).then(a.cmp(b)),
                _ => a.cmp(b),
            });
            Some(rows)
        } else {
            None
        };
        let started = Instant::now();
        let mut order = store.start_ordered_folder(
            c,
            EntryId(0),
            &root,
            Sort::from_index(index).unwrap(),
            scratch.path(),
            quota_mib * 1024 * 1024,
        )?;
        let mut max_step_ms = 0.0f64;
        let mut steps = 0u64;
        loop {
            let s = Instant::now();
            let done = order.step(&store, &fold)?;
            max_step_ms = max_step_ms.max(s.elapsed().as_secs_f64() * 1000.);
            steps += 1;
            if done {
                break;
            }
        }
        let mut p = order.page(None)?;
        let materialize_ms = started.elapsed().as_secs_f64() * 1000.;
        let mut count = 0u64;
        let mut pages = 0u64;
        let mut max_page_ms = 0.0f64;
        loop {
            pages += 1;
            for r in &p.rows {
                let i = if let Some(expected) = &expected {
                    expected[count as usize]
                } else if matches!(index, 0 | 3) {
                    n - 1 - count
                } else {
                    count
                };
                assert_eq!(r.key.entry, EntryId(i + 1));
                assert_eq!(r.path, root.join(format!("file-{i:010}.txt")));
                assert_eq!(r.logical, Some(i));
                count += 1;
            }
            if !p.more {
                break;
            }
            let s = Instant::now();
            p = order.page(p.next.as_ref())?;
            max_page_ms = max_page_ms.max(s.elapsed().as_secs_f64() * 1000.);
        }
        assert_eq!(count, n);
        eprintln!(
            "sort={index} rows={count} materialize_ms={materialize_ms:.3} max_step_ms={max_step_ms:.3}"
        );
        data.push(serde_json::json!({"sort":index,"materialize_and_first_page_ms":materialize_ms,"steps":steps,"max_step_ms":max_step_ms,"pages":pages,"max_page_ms":max_page_ms,"exact_rows":count,"startup_plus_this_sort_ms":open_ms+fold_ms+materialize_ms,"first_page_target_pass":open_ms+fold_ms+materialize_ms<100.}));
    }
    println!(
        "{}",
        serde_json::json!({"entries":n,"mode":mode,"quota_mib":quota_mib,"enriched":enriched,"synthetic":true,"native_acquisition":false,"open_ms":open_ms,"folder_fold_ms":fold_ms,"sorts":data,"accounted_after_sorts":governor.usage().categories.iter().sum::<u64>(),"peak_process_bytes":memory(),"source_acquisition":false})
    );
    Ok(())
}
