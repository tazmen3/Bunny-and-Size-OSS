//! Synthetic frozen metadata, not a source scan. One distinct extension per file.
use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::*};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store},
};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    time::Instant,
};
fn name(s: &str) -> NativeName {
    #[cfg(unix)]
    {
        NativeName::Posix(s.as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        NativeName::Windows(s.encode_utf16().collect())
    }
    #[cfg(not(any(unix, windows)))]
    {
        NativeName::Opaque(s.as_bytes().to_vec())
    }
}
#[cfg(target_os = "linux")]
fn peak() -> u64 {
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
fn peak() -> u64 {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
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
fn peak() -> u64 {
    0
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().ok_or("output directory")?);
    let n = args.next().ok_or("entry count")?.parse::<u64>()?;
    if n == 0 {
        return Err("entry count must be positive".into());
    }
    std::fs::create_dir_all(&out)?;
    let dir = out.join("inventory");
    let start = Instant::now();
    let initial_memory = sortsize_core::resources::native_memory();
    let c = AcquisitionContext {
        inventory: InventoryId(909),
        source: SourceId(909),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = if dir.exists() {
        Store::reopen(&dir, Durability::Reconstructible)?
    } else {
        Store::create(&dir, c.inventory, c.source, Durability::Reconstructible)?
    };
    if s.entry_count(c.generation)? == 0 {
        s.create_generation(c)?;
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
        s.begin_attempt(a)?;
        for (sequence, start) in (0..n).step_by(512).enumerate() {
            let end = (start + 512).min(n);
            let rows = (start..end)
                .map(|i| Observation {
                    name: name(&format!("f{i:07}.e{i:07}")),
                    kind: Kind::File,
                    logical: Some(i),
                    allocated: None,
                    modified: None,
                    metadata_known: true,
                })
                .collect::<Vec<_>>();
            s.ingest(
                BatchHeader {
                    attempt: a,
                    sequence: sequence as u64,
                    end: if end == n {
                        Some(ListingEnd::Exhausted)
                    } else {
                        None
                    },
                },
                &rows,
            )?;
        }
        while !s.reconcile_step(a, ComparisonScope::Comparable, 512)? {}
        while !s.consolidate_step(c, 512)? {}
        s.seal(c)?;
    }
    assert_eq!(s.entry_count(c.generation)?, n + 1);
    let build_ms = start.elapsed().as_secs_f64() * 1000.;
    let cancel = CancellationToken::default();
    let root = if cfg!(windows) {
        PathBuf::from(r"C:\f")
    } else {
        PathBuf::from("/f")
    };
    let start = Instant::now();
    let q = s.query(
        c.generation,
        &root,
        &SearchQuery::default(),
        &out,
        2 * 1024 * 1024 * 1024,
        &cancel,
    )?;
    let query_ms = start.elapsed().as_secs_f64() * 1000.;
    assert!(q.complete());
    assert_eq!(q.counts.matches, n + 1);
    let start = Instant::now();
    let stats = q.statistics(&out, 1024 * 1024 * 1024, &cancel)?;
    let stats_ms = start.elapsed().as_secs_f64() * 1000.;
    assert!(stats.complete);
    assert_eq!(stats.totals.files, n);
    assert_eq!(
        stats.totals.known_bytes,
        u128::from(n) * u128::from(n - 1) / 2
    );
    assert_eq!(stats.extensions.len(), n.min(32) as usize);
    assert_eq!(stats.extensions[0].counts.known_bytes, u128::from(n - 1));
    assert_eq!(stats.others.files, n.saturating_sub(32));
    assert_eq!(
        stats.others.known_bytes
            + stats
                .extensions
                .iter()
                .map(|e| e.counts.known_bytes)
                .sum::<u128>(),
        stats.totals.known_bytes
    );
    std::fs::write(
        out.join("statistics.json"),
        serde_json::to_vec_pretty(&stats.json())?,
    )?;
    drop(stats);
    let output = out.join("inventory.jsonl");
    let start = Instant::now();
    let export = q.export_jsonl(&output, 2 * 1024 * 1024 * 1024, &cancel)?;
    let export_ms = start.elapsed().as_secs_f64() * 1000.;
    assert!(export.complete);
    assert_eq!(export.records, n + 1);
    let start = Instant::now();
    let mut rows = 0_u64;
    let mut bytes = 0_u128;
    let mut end = false;
    for line in BufReader::new(std::fs::File::open(&output)?).lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        assert!(!end);
        match v["record"].as_str().unwrap() {
            "entry" => {
                let entry = v["entry"].as_str().unwrap().parse::<u64>()?;
                assert_eq!(
                    entry, rows,
                    "every expected identity exactly once in native order"
                );
                if entry > 0 {
                    let expected = root.join(format!("f{:07}.e{:07}", entry - 1, entry - 1));
                    assert_eq!(v["path"]["display"], expected.to_string_lossy().as_ref());
                    assert_eq!(v["logical_bytes"], (entry - 1).to_string());
                }
                rows += 1;
                if v["kind"] == "file" {
                    bytes += v["logical_bytes"].as_str().unwrap().parse::<u128>()?;
                }
            }
            "end" => {
                end = true;
                assert_eq!(v["coverage"], "complete");
                assert_eq!(v["records"], (n + 1).to_string());
            }
            "header" => {}
            _ => panic!("unknown record"),
        }
    }
    assert!(end);
    assert_eq!(rows, n + 1);
    assert_eq!(bytes, u128::from(n) * u128::from(n - 1) / 2);
    let report = serde_json::json!({"synthetic_metadata":true,"initial_native_available_bytes":initial_memory.map(|m|m.available),"retained_query_bytes":sortsize_core::resources::Governor::global().usage().categories[sortsize_core::resources::Category::Queries as usize],"source_io":false,"entries":n+1,"unique_extensions":n,"build_ms":build_ms,"query_ms":query_ms,"statistics_ms":stats_ms,"export_ms":export_ms,"readback_ms":start.elapsed().as_secs_f64()*1000.,"export_bytes":std::fs::metadata(&output)?.len(),"peak_process_bytes":peak(),"records_read_back":rows,"logical_sum":bytes.to_string(),"all_oracles_pass":true});
    println!("{report}");
    std::fs::write(
        out.join("qualification.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
