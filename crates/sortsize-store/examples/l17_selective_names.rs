//! Recheck the L17 rare and absent names through the production PageQuery.
use serde_json::json;
use sortsize_core::{
    CancellationToken, InventoryKind,
    analysis::SearchQuery,
    inventory_contract::MemoryMode,
    resources::{Governor, disk_limit, native_free_disk, native_memory},
};
use sortsize_store::{
    direct_snapshot,
    direct_tree::{DirectTree, query::PageQuery},
};
use std::{path::PathBuf, time::Instant};

fn verify_queries(
    tree: &DirectTree,
    files: u64,
    trials: u32,
    cancel: &CancellationToken,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let mut measurements = Vec::new();
    let late = format!("f{:08}", files - 1);
    for kind in [Some(InventoryKind::File), None] {
        for (scenario, needle) in [("late", late.as_str()), ("absent", "no-such-entry-l17")] {
            let query = SearchQuery {
                name: Some(needle.to_owned()),
                kind,
                ..SearchQuery::default()
            };
            for trial in 1..=trials {
                let start = Instant::now();
                let mut page = PageQuery::new(tree, None, Some(&query), 2, None, 200)?;
                let mut turns = 1;
                while !page.advance(tree, cancel)? {
                    turns += 1;
                }
                let rows = page.rows(tree)?;
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                let expected = usize::from(scenario == "late");
                assert_eq!(rows.len(), expected);
                assert_eq!(page.matches, expected as u64);
                assert_eq!(page.evaluated, tree.len() - 1);
                assert_eq!(page.gaps, 0);
                assert!(page.next().is_none());
                if expected == 1 {
                    let position = files - 1;
                    let length = if position.is_multiple_of(64) {
                        (position / 64) % 4096 + 1
                    } else {
                        0
                    };
                    assert_eq!(rows[0].path.file_name().unwrap(), late.as_str());
                    assert_eq!(rows[0].logical, Some(length));
                    assert_eq!(page.logical, u128::from(length));
                    assert_eq!(page.files, 1);
                }
                measurements.push(json!({
                    "trial": trial,
                    "scope": if kind.is_some() { "file" } else { "all_kinds" },
                    "scenario": scenario, "matches": page.matches,
                    "first_page_ms": elapsed, "cooperative_turns": turns,
                }));
            }
        }
    }
    Ok(measurements)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let snapshot = PathBuf::from(args.next().ok_or("missing SQLite snapshot")?);
    let scratch = PathBuf::from(args.next().ok_or("missing fresh scratch directory")?);
    let files: u64 = args
        .next()
        .ok_or("missing file count")?
        .to_str()
        .ok_or("invalid file count")?
        .parse()?;
    let memory = args
        .next()
        .map(|value| -> Result<u64, Box<dyn std::error::Error>> {
            Ok(value.to_str().ok_or("invalid MiB")?.parse()?)
        })
        .transpose()?;
    let source = direct_snapshot::inspect(&snapshot)?.root;
    if source.try_exists()? {
        return Err("fixture source must be unavailable".into());
    }
    std::fs::create_dir(&scratch)?;
    let memory_ceiling = memory
        .map(|mib| mib.checked_mul(1 << 20).ok_or("invalid MiB"))
        .transpose()?;
    let governor = Governor::global();
    governor.configure(MemoryMode::Auto, native_memory());
    governor.set_byte_limit(memory_ceiling);
    let quota = disk_limit(native_free_disk(&scratch).ok_or("scratch capacity unavailable")?);
    let cancel = CancellationToken::default();
    let start = Instant::now();
    let tree = direct_snapshot::load(&snapshot, &scratch, quota, &cancel)?.tree;
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(tree.summary().measure.files, files);
    // No explicit preparation before the first exact queries, including any
    // newly rebuilt scratch data. Opening and verification have their own clock.
    let measurements = verify_queries(&tree, files, 3, &cancel)?;

    let disk_before = tree.summary().temporary_bytes;
    let memory_before = governor.usage().categories.iter().sum::<u64>();
    let start = Instant::now();
    assert!(tree.prepare_name_search(&cancel)?);
    let prepare_ms = start.elapsed().as_secs_f64() * 1000.0;
    let usage = governor.usage();
    let memory_after = usage.categories.iter().sum::<u64>();
    let disk_after = tree.summary().temporary_bytes;
    assert_eq!((disk_after, memory_after), (disk_before, memory_before));
    println!(
        "{}",
        json!({
            "status": "PASS_SCOPED_L17_SELECTIVE_NAMES", "files": files,
            "offline_load_ms": load_ms, "explicit_preparation_recheck_ms": prepare_ms,
            "extra_index_disk_bytes_after_open": disk_after - disk_before,
            "extra_index_memory_bytes_after_open": memory_after - memory_before,
            "replayed_tree_disk_bytes": disk_after,
            "replayed_tree_accounted_memory_bytes": memory_after,
            "effective_memory_limit_bytes": usage.limits.bytes,
            "measurements": measurements,
        })
    );
    Ok(())
}
