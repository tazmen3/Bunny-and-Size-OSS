//! Synthetic stored metadata; no source disk is scanned.
use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::*};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store},
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

fn name(i: u64) -> NativeName {
    let s = format!("file-{i:010}.txt");
    #[cfg(unix)]
    {
        NativeName::Posix(s.into_bytes())
    }
    #[cfg(windows)]
    {
        NativeName::Windows(s.encode_utf16().collect())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().ok_or("new directory")?);
    let n = args.next().unwrap_or("1000000".into()).parse::<u64>()?;
    std::fs::create_dir(&dir)?;
    let c = AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut store = Store::create(
        &dir.join("inventory"),
        c.inventory,
        c.source,
        Durability::Retained,
    )?;
    store.create_generation(c)?;
    let a = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    store.begin_attempt(a)?;
    let start = Instant::now();
    let mut seq = 0;
    for first in (0..n).step_by(512) {
        let rows = (first..(first + 512).min(n))
            .map(|i| Observation {
                name: name(i),
                kind: Kind::File,
                logical: Some(i),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        store.ingest(
            BatchHeader {
                attempt: a,
                sequence: seq,
                end: None,
            },
            &rows,
        )?;
        seq += 1;
    }
    store.ingest(
        BatchHeader {
            attempt: a,
            sequence: seq,
            end: Some(ListingEnd::Exhausted),
        },
        &[],
    )?;
    while !store.reconcile_step(a, ComparisonScope::Comparable, 512)? {}
    while !store.consolidate_step(c, 512)? {}
    store.seal(c)?;
    let ingestion_ms = start.elapsed().as_secs_f64() * 1000.;
    let root = dir.join("offline-not-created");
    let start = Instant::now();
    let q = store.query(
        c.generation,
        &root,
        &SearchQuery::default(),
        &dir,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::default(),
    )?;
    let materialize_ms = start.elapsed().as_secs_f64() * 1000.;
    assert!(q.complete());
    let mut cursor = None;
    let mut count = 0;
    let mut page_ms = Vec::new();
    loop {
        let start = Instant::now();
        let p = q.page(cursor.as_ref())?;
        page_ms.push(start.elapsed().as_secs_f64() * 1000.);
        count += p.rows.len() as u64;
        if !p.more {
            break;
        }
        cursor = p.next;
    }
    assert_eq!(count, n + 1);
    page_ms.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"synthetic":true,"kind":"stored_metadata_query","entries":n+1,"ingestion_and_seal_ms":ingestion_ms,"materialize_ms":materialize_ms,"pages":page_ms.len(),"page_p95_ms":page_ms[page_ms.len()*95/100],"page_max_ms":page_ms.last(),"peak_rss_bytes":memory(),"exactness":"PASS","source_io":0})
    );
    Ok(())
}
