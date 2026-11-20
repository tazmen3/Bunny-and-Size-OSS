//! Measures the real L02 API, not the representative L01 schema.
use serde_json::json;
use sortsize_core::inventory_contract::*;
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store},
};
use std::{error::Error, path::Path, time::Instant};
fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}
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
    let t = format!("item-{i:016x}");
    if i.is_multiple_of(2) {
        let mut b = t.into_bytes();
        b.push(255);
        NativeName::Posix(b)
    } else {
        let mut w: Vec<_> = t.encode_utf16().collect();
        w.push(0xd800);
        NativeName::Windows(w)
    }
}
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: inventory_qualification NEW_DIRECTORY ROWS (>=70000)".into());
    }
    let dir = Path::new(&args[1]);
    let count = args[2].parse::<u64>()?;
    if count < 70000 {
        return Err("need at least 70000 entries".into());
    }
    let mut s = Store::create(dir, InventoryId(1), SourceId(2), Durability::Retained)?;
    let mut metrics = vec![];
    for generation in 0..2 {
        let c = AcquisitionContext {
            inventory: InventoryId(1),
            source: SourceId(2),
            generation: GenerationId(generation),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
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
        let t = Instant::now();
        for (seq, lo) in (0..count).step_by(512).enumerate() {
            let hi = (lo + 512).min(count);
            let rows: Vec<_> = (lo..hi)
                .map(|i| Observation {
                    name: name(i),
                    kind: if i < 70000 {
                        Kind::Directory
                    } else {
                        Kind::File
                    },
                    logical: if i < 70000 { None } else { Some(i) },
                    allocated: None,
                    modified: Some((-1, 999999999)),
                    metadata_known: true,
                })
                .collect();
            s.ingest(
                BatchHeader {
                    attempt: a,
                    sequence: seq as u64,
                    end: if hi == count {
                        Some(ListingEnd::Exhausted)
                    } else {
                        None
                    },
                },
                &rows,
            )?;
        }
        let ingest_ms = ms(t);
        let t = Instant::now();
        while !s.reconcile_step(a, ComparisonScope::Comparable, 512)? {}
        let sweep_ms = ms(t);
        let t = Instant::now();
        while !s.consolidate_step(c, 512)? {}
        let fold_ms = ms(t);
        let expected =
            u64::try_from((70_000u128 + u128::from(count) - 1) * u128::from(count - 70_000) / 2)?;
        assert_eq!(s.totals(c.generation)?.logical, Some(expected));
        let t = Instant::now();
        assert_eq!(s.seal(c)?, ScopeCoverage::Partial);
        let seal_ms = ms(t); // Empty child directories not enumerated: partial is required.
        assert_eq!(s.task_count(c.generation)?, 70001);
        assert_eq!(s.entry_count(c.generation)?, count + 1);
        let t = Instant::now();
        let late = s
            .lookup(c.generation, EntryId(0), &name(count - 1))?
            .unwrap();
        assert_eq!(late.key.entry, EntryId(count));
        let lookup_ms = ms(t);
        metrics.push(json!({"generation":generation,"ingest_ms":ingest_ms,"rows_per_second":count as f64/(ingest_ms/1000.0),"sweep_ms":sweep_ms,"fold_ms":fold_ms,"seal_ms":seal_ms,"late_lookup_ms":lookup_ms,"known_logical":expected,"entries":count+1,"tasks":70001}));
    }
    drop(s);
    let s = Store::reopen(dir, Durability::Retained)?;
    assert_eq!(s.latest_sealed()?, Some(GenerationId(1)));
    assert_eq!(s.latest_complete()?, None);
    assert_eq!(s.entry_count(GenerationId(0))?, count + 1);
    let disk: u64 = std::fs::read_dir(dir)?
        .map(|p| p.unwrap().metadata().unwrap().len())
        .sum();
    println!(
        "{}",
        json!({"rows_per_generation":count,"generations":metrics,"peak_rss_bytes":memory(),"retained_bytes":disk,"exactness":"PASS","coverage":"Partial: child directories deliberately unenumerated"})
    );
    Ok(())
}
