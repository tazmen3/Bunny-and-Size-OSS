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
    let action = args.next().ok_or("prepare|run")?;
    let dir = PathBuf::from(args.next().ok_or("directory")?);
    let n = args.next().ok_or("entry count")?.parse::<u64>()?;
    let mode = args.next().unwrap_or_else(|| "auto".into());
    let memory_mode = sortsize_core::inventory_contract::MemoryMode::from_key(&mode)
        .ok_or("invalid memory mode")?;
    let governor = sortsize_core::resources::Governor::global();
    governor.configure(memory_mode, sortsize_core::resources::native_memory());
    if action == "prepare" || action == "prepare-enriched" {
        let enriched = action == "prepare-enriched";
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
        if enriched {
            store.enable_properties()?;
        }
        store.create_generation(c)?;
        if enriched {
            store.set_collection_options(c, true, true)?;
            store.record_acquisition(c, &dir.join("unmounted-source"), 100)?;
        }
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
                    allocated: enriched.then_some(i.div_ceil(4096) * 4096),
                    modified: enriched.then_some((i as i64, 0)),
                    metadata_known: true,
                })
                .collect::<Vec<_>>();
            let header = BatchHeader {
                attempt: a,
                sequence: seq,
                end: None,
            };
            if enriched {
                use sortsize_core::{
                    EntryOwner, MetadataValue, NativeOwnerId, ValueAccuracy,
                    acquisition::NodeProperties,
                };
                let exact = |v| MetadataValue::Known {
                    value: v,
                    accuracy: ValueAccuracy::Exact,
                };
                let properties = rows
                    .iter()
                    .enumerate()
                    .map(|(j, o)| NodeProperties {
                        allocated: exact(o.allocated.unwrap()),
                        accessed: MetadataValue::Known {
                            value: std::time::UNIX_EPOCH
                                + std::time::Duration::from_secs(first + j as u64),
                            accuracy: ValueAccuracy::Exact,
                        },
                        owner: EntryOwner {
                            id: MetadataValue::Known {
                                value: NativeOwnerId::UnixUid((j % 17) as u32),
                                accuracy: ValueAccuracy::Exact,
                            },
                        },
                    })
                    .collect::<Vec<_>>();
                store.ingest_properties(header, &rows, &[], &properties)?;
            } else {
                store.ingest(
                    BatchHeader {
                        attempt: a,
                        sequence: seq,
                        end: None,
                    },
                    &rows,
                )?;
            }
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
        assert_eq!(store.entry_count(c.generation)?, n + 1);
        let totals = store.totals(c.generation)?;
        assert_eq!(totals.logical, Some(n * (n - 1) / 2));
        assert_eq!(totals.unknown, 0);
        let ingestion_ms = start.elapsed().as_secs_f64() * 1000.;
        if enriched {
            store.record_measurement(c.generation, &dir.join("unmounted-source"), 100, 200)?;
        }
        store.set_retention(true, 30)?;
        println!(
            "{}",
            serde_json::json!({"phase":"prepare","synthetic":true,"entries":n+1,"ingestion_and_seal_ms":ingestion_ms,"peak_process_bytes":memory(),"source_io":0})
        );
        return Ok(());
    }
    if action != "run" {
        return Err("prepare|run".into());
    }
    let scenario = args.next().ok_or("rare|absent|broad")?;
    let broad = scenario == "broad";
    let absent = scenario == "absent";
    if !matches!(scenario.as_str(), "rare" | "absent" | "broad") {
        return Err("rare|absent|broad".into());
    }
    let scratch = PathBuf::from(args.next().ok_or("new result scratch directory")?);
    std::fs::create_dir(&scratch)?;
    let all = Instant::now();
    let store = Store::reopen(&dir.join("inventory"), Durability::Retained)?;
    let open_ms = all.elapsed().as_secs_f64() * 1000.;
    let root = PathBuf::from(if cfg!(windows) { "Q:/q" } else { "/q" });
    let mut query = SearchQuery::default();
    if absent {
        query.name = Some("absent-from-this-corpus".into());
    } else if !broad {
        query.name = Some(format!("file-{:010}.txt", n - 1));
    }
    let start = Instant::now();
    let q = store.query(
        GenerationId(1),
        &root,
        &query,
        &scratch,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::default(),
    )?;
    let materialize_ms = start.elapsed().as_secs_f64() * 1000.;
    assert!(q.complete());
    let mut cursor = None;
    let mut count = 0u64;
    let mut page_ms = Vec::new();
    let mut first_ms = None;
    let mut previous_id = None;
    loop {
        let page_start = Instant::now();
        let page = q.page(cursor.as_ref())?;
        page_ms.push(page_start.elapsed().as_secs_f64() * 1000.);
        if first_ms.is_none() {
            first_ms = Some(all.elapsed().as_secs_f64() * 1000.);
        }
        for row in &page.rows {
            assert!(previous_id.is_none_or(|id| row.key.entry.0 > id));
            previous_id = Some(row.key.entry.0);
            assert!(row.key.entry.0 <= n);
            let expected_path = if row.key.entry.0 == 0 {
                root.clone()
            } else {
                root.join(format!("file-{:010}.txt", row.key.entry.0 - 1))
            };
            assert_eq!(row.path, expected_path);
            if !broad {
                assert!(
                    row.path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(&format!("file-{:010}", n - 1))
                );
            }
        }
        count += page.rows.len() as u64;
        if !page.more {
            break;
        }
        cursor = page.next;
    }
    assert_eq!(
        count,
        if broad {
            n + 1
        } else if absent {
            0
        } else {
            1
        }
    );
    assert_eq!(q.counts.evaluated, n + 1);
    // L01 calls for at least 30 page observations, even for a rare result.
    while page_ms.len() < 30 {
        let start = Instant::now();
        let page = q.page(None)?;
        assert_eq!(page.rows.is_empty(), absent);
        page_ms.push(start.elapsed().as_secs_f64() * 1000.);
    }
    page_ms.sort_by(f64::total_cmp);
    // The workspace is a private subdirectory: its directory inode length is
    // not the byte size of the result files. Measure the owned files instead.
    let mut artifacts = 0u64;
    for workspace in std::fs::read_dir(&scratch)? {
        for file in std::fs::read_dir(workspace?.path())? {
            artifacts += file?.metadata()?.len();
        }
    }
    let temporary = q.temporary_usage();
    let usage = governor.usage();
    println!(
        "{}",
        serde_json::json!({"phase":"run","synthetic":true,"entries":n+1,"requested_mode":mode,"effective_limit_bytes":usage.limits.bytes,"effective_entry_limit":usage.limits.entries,"fallback":usage.limits.fallback,"accounted_bytes":usage.categories.iter().sum::<u64>(),"scenario":if broad{"broad"}else if absent{"absent"}else{"late-rare"},"open_integrity_ms":open_ms,"materialize_ms":materialize_ms,"first_exact_page_from_open_ms":first_ms,"page_samples":page_ms.len(),"page_p95_ms":page_ms[page_ms.len()*95/100],"peak_process_bytes":memory(),"result_artifacts_bytes":artifacts,"temporary_usage":temporary,"matches":count,"source_io":0,"exactness":"PASS","first_page_target_pass":first_ms.unwrap()<100.,"page_p95_target_pass":page_ms[page_ms.len()*95/100]<100.})
    );
    Ok(())
}
