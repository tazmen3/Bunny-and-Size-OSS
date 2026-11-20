//! Additional enriched-inventory evidence. Does not replace frozen L17 gates.
use sortsize_core::{analysis::SearchQuery, inventory_contract::*, resources::Governor};
use sortsize_store::inventory::{Kind, preview::Unverified};
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
    let mut args = std::env::args().skip(1).peekable();
    let dir = PathBuf::from(args.next().ok_or("store directory")?);
    if args.peek().map(|s| s.as_str()) == Some("catalogue") {
        let catalogue = dir.join("catalogue");
        sortsize_store::inventory::lifecycle::ensure_root(&catalogue)?;
        std::fs::rename(dir.join("inventory"), catalogue.join("inv-recorded"))?;
        return Ok(());
    }
    let n = args.next().ok_or("file count")?.parse::<u64>()?;
    let scenario = args.next().ok_or("rare|absent|broad")?;
    let quota = args.next().ok_or("512|2048 MiB")?.parse::<u64>()?;
    if ![512, 2048].contains(&quota) {
        return Err("fixture quota must be explicitly 512 or 2048 MiB".into());
    }
    let scratch = tempfile::tempdir_in(args.next().ok_or("existing scratch parent")?)?;
    Governor::global().configure(MemoryMode::Auto, sortsize_core::resources::native_memory());
    let start = Instant::now();
    let preview = Unverified::open(&dir, GenerationId(1))?;
    let prefix = preview.prefix(&Default::default())?;
    let prefix_ms = start.elapsed().as_secs_f64() * 1000.;
    let root = prefix.saved.root.clone();
    assert!(prefix.rows.len() <= 200);
    drop(prefix);
    let store = preview.verify(&Default::default())?;
    let certified_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut search = SearchQuery::default();
    match scenario.as_str() {
        "rare" => search.name = Some(format!("file-{:010}.txt", n - 1)),
        "absent" => search.name = Some("absent-from-this-corpus".into()),
        "broad" | "broad-size" => {}
        _ => return Err("unknown scenario".into()),
    };
    let query = store.query(
        GenerationId(1),
        &root,
        &search,
        scratch.path(),
        quota * 1024 * 1024,
        &Default::default(),
    );
    match query {
        Err(error) => println!(
            "{}",
            serde_json::json!({"synthetic":true,"schema_enriched":true,"files":n,"scenario":scenario,"quota_mib":quota,"status":"REFUSED","diagnostic":error.to_string(),"prefix_ready_ms":prefix_ms,"certified_ms":certified_ms,"peak_process_bytes":memory(),"source_acquisition":false})
        ),
        Ok(query) => {
            let query_ready_ms = start.elapsed().as_secs_f64() * 1000.;
            let ordered = if scenario == "broad-size" {
                let result = (|| {
                    let mut order =
                        query.order_by_size(true, scratch.path(), quota * 1024 * 1024)?;
                    while !order.advance(&Default::default())? {}
                    Ok::<_, sortsize_store::inventory::Error>(order)
                })();
                match result {
                    Ok(order) => Some(order),
                    Err(error) => {
                        println!(
                            "{}",
                            serde_json::json!({"status":"REFUSED","phase":"size-order","files":n,"scenario":scenario,"quota_mib":quota,"diagnostic":error.to_string(),"query_ready_ms":query_ready_ms,"certified_ms":certified_ms,"peak_process_bytes":memory(),"synthetic":true,"source_acquisition":false})
                        );
                        return Ok(());
                    }
                }
            } else {
                None
            };
            let read = |cursor: Option<&sortsize_store::inventory::queries::Cursor>| match &ordered
            {
                Some(order) => order.page(cursor, false),
                None => query.page(cursor),
            };
            let first_ms = start.elapsed().as_secs_f64() * 1000.;
            let expected = match scenario.as_str() {
                "rare" => 1,
                "absent" => 0,
                _ => n + 1,
            };
            assert_eq!(query.counts.matches, expected);
            assert!(query.complete());
            let mut page = read(None)?;
            let fetched_ms = start.elapsed().as_secs_f64() * 1000.;
            let scratch_files = std::fs::read_dir(scratch.path())?.map(|entry| {
                let entry = entry?;
                Ok(serde_json::json!({"name":entry.file_name().to_string_lossy(),"bytes":entry.metadata()?.len()}))
            }).collect::<Result<Vec<_>, std::io::Error>>()?;
            let mut count = 0;
            let mut max_page_ms = 0_f64;
            loop {
                for row in &page.rows {
                    if scenario == "rare" {
                        assert_eq!(row.key.entry, EntryId(n));
                        assert_eq!(row.kind, Kind::File);
                        assert_eq!(row.path, root.join(format!("file-{:010}.txt", n - 1)));
                        assert_eq!(row.logical, Some(n - 1));
                    }
                    if scenario == "broad" {
                        assert_eq!(row.key.entry.0, count);
                        if count == 0 {
                            assert_eq!(row.kind, Kind::Directory);
                            assert_eq!(row.path, root);
                        } else {
                            let i = count - 1;
                            assert_eq!(row.path, root.join(format!("file-{i:010}.txt")));
                            assert_eq!(row.logical, Some(i));
                        }
                    }
                    if scenario == "broad-size" {
                        if count == n {
                            assert_eq!(row.key.entry, EntryId(0));
                            assert_eq!(row.kind, Kind::Directory);
                            assert_eq!(row.path, root);
                            assert_eq!(row.logical, None);
                        } else {
                            let i = n - count - 1;
                            assert_eq!(row.key.entry, EntryId(i + 1));
                            assert_eq!(row.kind, Kind::File);
                            assert_eq!(row.path, root.join(format!("file-{i:010}.txt")));
                            assert_eq!(row.logical, Some(i));
                        }
                    }
                    count += 1;
                }
                if !page.more {
                    break;
                }
                let at = Instant::now();
                page = read(page.next.as_ref())?;
                max_page_ms = max_page_ms.max(at.elapsed().as_secs_f64() * 1000.);
            }
            assert_eq!(count, expected);
            println!(
                "{}",
                serde_json::json!({"synthetic":true,"schema_enriched":true,"files":n,"scenario":scenario,"quota_mib":quota,"status":"EXACT","rows":count,"prefix_ready_ms":prefix_ms,"certified_ms":certified_ms,"first_exact_from_open_ms":first_ms,"first_fetched_page_from_open_ms":fetched_ms,"query_ready_ms":query_ready_ms,"evaluation_ms":query_ready_ms-certified_ms,"order_ms":first_ms-query_ready_ms,"scratch_after_evaluation":scratch_files,"frozen_first_page_target_pass":first_ms<100.,"max_page_ms":max_page_ms,"peak_process_bytes":memory(),"source_acquisition":false})
            );
        }
    }
    Ok(())
}
