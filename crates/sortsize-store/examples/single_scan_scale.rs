//! Synthetic qualification of the real DirectTree, never a filesystem benchmark.
//! Usage: single_scan_scale [FILES=9000000] [MEMORY_MIB=4096] [WORKSPACE] [--directories]
//! Builds two wide sibling scopes, validates algebraic totals, ranked pages,
//! Top100, random native names, replay counts and resource release.
use sortsize_core::{
    CancellationToken, MetadataValue, ValueAccuracy,
    acquisition::NodeProperties,
    inventory_contract::{EntryId, ListingEnd, ScopeCoverage},
    resources::{Governor, Usage},
};
use sortsize_store::{
    codec::NativeName,
    direct_tree::{DirectTree, query::PageQuery},
    inventory::{Kind, Observation},
    scheduler::direct::ObservationBatch,
};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

fn native(name: &str) -> NativeName {
    #[cfg(windows)]
    {
        NativeName::Windows(name.encode_utf16().collect())
    }
    #[cfg(not(windows))]
    {
        NativeName::Posix(name.as_bytes().to_vec())
    }
}
fn name(number: u64) -> String {
    format!("fichier-é-{number:010}")
}
fn accept(
    tree: &mut DirectTree,
    parent: u64,
    sequence: u64,
    end: Option<ListingEnd>,
    rows: &[Observation],
    properties: &[NodeProperties],
) -> Result<(), Box<dyn std::error::Error>> {
    tree.accept(ObservationBatch {
        parent: EntryId(parent),
        sequence,
        end,
        observations: rows,
        evidence: &vec![None; rows.len()],
        properties,
        directory: None,
        detached: false,
        errors: 0,
        denied: 0,
    })?;
    Ok(())
}
#[cfg(windows)]
fn memory() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    // SAFETY: initialized writable counters of the exact requested native size.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) == 0
        {
            None
        } else {
            Some((
                counters.WorkingSetSize as u64,
                counters.PeakWorkingSetSize as u64,
            ))
        }
    }
}
#[cfg(not(windows))]
fn memory() -> Option<(u64, u64)> {
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let count: u64 = args
        .first()
        .map(|s| s.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(9_000_000);
    let memory_mib: u64 = args
        .get(1)
        .map(|s| s.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(4096);
    if count < 400 || count >= u64::from(u32::MAX) - 3 || !(32..=4096).contains(&memory_mib) {
        return Err("expected 400..u32::MAX-4 files and 32..4096 MiB".into());
    }
    let scratch = if let Some(path) = args.get(2) {
        tempfile::Builder::new()
            .prefix("single-scale-")
            .tempdir_in(Path::new(path))?
    } else {
        tempfile::tempdir()?
    };
    let directory_mode = args.get(3).is_some_and(|arg| arg == "--directories");
    if args.len() > 4 || (args.len() == 4 && !directory_mode) {
        return Err("unknown scale option".into());
    }
    let governor = Governor::global();
    governor.set_byte_limit(Some(memory_mib * 1024 * 1024));
    let initial = governor.usage();
    let started = Instant::now();
    let mut tree = DirectTree::new(
        PathBuf::from("synthetic-inventory"),
        scratch.path(),
        16 * 1024 * 1024 * 1024,
    )?;
    if directory_mode {
        return directory_scale(count, memory_mib, tree, governor, initial, started);
    }
    let directories = ["premier", "second"].map(|s| Observation {
        name: native(s),
        kind: Kind::Directory,
        logical: None,
        allocated: None,
        modified: None,
        metadata_known: true,
    });
    accept(
        &mut tree,
        0,
        0,
        Some(ListingEnd::Exhausted),
        &directories,
        &[NodeProperties::default(), NodeProperties::default()],
    )?;
    let split = count / 2;
    let mut temporary_high_water = 0;
    let mut rows = Vec::with_capacity(64);
    let mut properties = Vec::with_capacity(64);
    for (parent, first, last) in [(1, 1, split), (2, split + 1, count)] {
        let mut first_in_batch = first;
        let mut sequence = 0;
        while first_in_batch <= last {
            rows.clear();
            properties.clear();
            let end = (first_in_batch + 63).min(last);
            for number in first_in_batch..=end {
                rows.push(Observation {
                    name: native(&name(number)),
                    kind: Kind::File,
                    logical: Some(number),
                    allocated: Some(number * 2),
                    modified: Some((123, 456)),
                    metadata_known: true,
                });
                properties.push(NodeProperties {
                    allocated: MetadataValue::Known {
                        value: number * 2,
                        accuracy: ValueAccuracy::Exact,
                    },
                    ..Default::default()
                });
            }
            accept(&mut tree, parent, sequence, None, &rows, &properties)?;
            first_in_batch = end + 1;
            sequence += 1;
            temporary_high_water = temporary_high_water.max(tree.summary().temporary_bytes);
            if end / 1_000_000 != (end - rows.len() as u64) / 1_000_000 {
                println!(
                    "{}",
                    serde_json::json!({"phase":"synthetic_acquisition", "files":end, "seconds":started.elapsed().as_secs_f64(), "temporary_bytes":tree.summary().temporary_bytes, "working_set_peak":memory()})
                );
            }
        }
        accept(
            &mut tree,
            parent,
            sequence,
            Some(ListingEnd::Exhausted),
            &[],
            &[],
        )?;
    }
    tree.finish()?;
    let acquisition_s = started.elapsed().as_secs_f64();
    let summary = tree.summary();
    let total = count * (count + 1) / 2;
    assert_eq!(summary.entries, count + 2);
    assert_eq!(summary.measure.files, count);
    assert_eq!(summary.measure.directories, 2);
    assert_eq!(summary.measure.logical, Some(total));
    assert_eq!(summary.measure.allocated, Some(total * 2));
    assert_eq!(summary.coverage, ScopeCoverage::Complete);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.pending_directories, 0);
    assert_eq!(
        tree.measure(EntryId(1))?.logical,
        Some(split * (split + 1) / 2)
    );
    for number in [1, split, split + 1, count] {
        let row = tree.row(EntryId(number + 2))?;
        assert_eq!(row.logical, Some(number));
        assert_eq!(
            row.path.file_name(),
            Some(std::ffi::OsStr::new(&name(number)))
        );
    }
    let mut random = 42u64;
    for _ in 0..1024 {
        random = random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let number = random % count + 1;
        assert_eq!(tree.name(EntryId(number + 2))?, native(&name(number)));
    }
    let top = tree.top()?;
    assert_eq!(top.len(), 100);
    for (offset, row) in top.iter().enumerate() {
        assert_eq!(row.logical, Some(count - offset as u64));
    }
    let query_started = Instant::now();
    let cancel = CancellationToken::default();
    for (parent, sort) in [(1, 0), (2, 1)] {
        let mut query = PageQuery::new(&tree, Some(EntryId(parent)), None, sort, None, 200)?;
        while !query.advance(&tree, &cancel)? {}
        let ranked = query.rows(&tree)?;
        assert_eq!(ranked.len(), 200);
        assert_eq!(
            query.matches,
            if parent == 1 { split } else { count - split }
        );
        for (rank, row) in ranked.iter().enumerate() {
            let expected = if parent == 1 {
                split - rank as u64
            } else {
                split + 1 + rank as u64
            };
            assert_eq!(row.logical, Some(expected));
        }
    }
    let query_s = query_started.elapsed().as_secs_f64();
    let replay_started = Instant::now();
    let mut replay_files = 0u64;
    let mut replay_total = 0u128;
    for batch in tree.replay()? {
        for row in batch?.observations {
            if row.kind == Kind::File {
                replay_files += 1;
                replay_total += u128::from(row.logical.unwrap());
            }
        }
    }
    assert_eq!(replay_files, count);
    assert_eq!(replay_total, u128::from(total));
    let replay_s = replay_started.elapsed().as_secs_f64();
    let used = governor.usage();
    let process_memory = memory();
    let before_drop_disk = used.disk_used;
    drop(tree);
    let after_drop = governor.usage();
    assert_eq!(after_drop.categories, initial.categories);
    assert_eq!(after_drop.disk_used, initial.disk_used);
    println!(
        "{}",
        serde_json::json!({"phase":"finished", "synthetic":true, "filesystem_acquisition":false, "files":count, "entries":count+2, "requested_mib":memory_mib, "effective_bytes":used.limits.bytes, "acquisition_s":acquisition_s, "rank_queries_s":query_s, "replay_verification_s":replay_s, "elapsed_s":started.elapsed().as_secs_f64(), "logical":total, "allocated":total*2, "coverage":"Complete", "temporary_high_water_sampled":temporary_high_water, "temporary_bytes_before_drop":summary.temporary_bytes, "global_disk_before_drop":before_drop_disk, "accounted_categories_before_drop":used.categories, "working_set_and_peak_bytes":process_memory, "global_disk_after_drop":after_drop.disk_used, "oracle":"PASS"})
    );
    Ok(())
}

fn directory_scale(
    count: u64,
    memory_mib: u64,
    mut tree: DirectTree,
    governor: Governor,
    initial: Usage,
    started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    const DEPTH: u64 = 128;
    let broad = count - DEPTH;
    let mut sequence = 0;
    let mut first = 1;
    let mut peak_temporary = 0;
    while first <= broad {
        let end = (first + 63).min(broad);
        let rows: Vec<_> = (first..=end)
            .map(|id| Observation {
                name: native(&format!("dossier-{id:010}")),
                kind: Kind::Directory,
                logical: None,
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect();
        let properties = vec![NodeProperties::default(); rows.len()];
        accept(&mut tree, 0, sequence, None, &rows, &properties)?;
        peak_temporary = peak_temporary.max(tree.summary().temporary_bytes);
        first = end + 1;
        sequence += 1;
    }
    accept(
        &mut tree,
        0,
        sequence,
        Some(ListingEnd::Exhausted),
        &[],
        &[],
    )?;
    for id in 2..=broad {
        accept(&mut tree, id, 0, Some(ListingEnd::Exhausted), &[], &[])?;
        peak_temporary = peak_temporary.max(tree.summary().temporary_bytes);
        if id % 100_000 == 0 {
            println!(
                "{}",
                serde_json::json!({"phase":"synthetic_directories", "closed_directories":id-1, "directories":count, "seconds":started.elapsed().as_secs_f64(), "temporary_bytes":tree.summary().temporary_bytes, "working_set_peak":memory()})
            );
        }
    }
    let mut parent = 1;
    for level in 1..=DEPTH {
        let row = Observation {
            name: native(&format!("profondeur-{level:03}")),
            kind: Kind::Directory,
            logical: None,
            allocated: None,
            modified: None,
            metadata_known: true,
        };
        accept(
            &mut tree,
            parent,
            0,
            Some(ListingEnd::Exhausted),
            &[row],
            &[NodeProperties::default()],
        )?;
        parent = broad + level;
    }
    let file = Observation {
        name: native("terminal-é"),
        kind: Kind::File,
        logical: Some(12345),
        allocated: Some(24690),
        modified: None,
        metadata_known: true,
    };
    let property = NodeProperties {
        allocated: MetadataValue::Known {
            value: 24690,
            accuracy: ValueAccuracy::Exact,
        },
        ..Default::default()
    };
    accept(
        &mut tree,
        parent,
        0,
        Some(ListingEnd::Exhausted),
        &[file],
        &[property],
    )?;
    tree.finish()?;
    let acquisition_s = started.elapsed().as_secs_f64();
    let summary = tree.summary();
    assert_eq!(summary.entries, count + 1);
    assert_eq!(summary.measure.directories, count);
    assert_eq!(summary.measure.files, 1);
    assert_eq!(summary.measure.logical, Some(12345));
    assert_eq!(summary.measure.allocated, Some(24690));
    assert_eq!(summary.coverage, ScopeCoverage::Complete);
    assert_eq!(summary.pending_directories, 0);
    assert_eq!(tree.measure(EntryId(1))?.directories, DEPTH);
    assert_eq!(tree.parent(EntryId(count + 1))?, Some(EntryId(count)));
    assert_eq!(
        tree.path(EntryId(count + 1))?.components().count() as u64,
        DEPTH + 3
    );
    assert_eq!(tree.top()?.len(), 1);
    assert_eq!(tree.top()?[0].logical, Some(12345));
    let query_started = Instant::now();
    let cancel = CancellationToken::default();
    let mut query = PageQuery::new(&tree, Some(EntryId(0)), None, 0, None, 200)?;
    while !query.advance(&tree, &cancel)? {}
    let ranked = query.rows(&tree)?;
    assert_eq!(ranked.len(), 200);
    assert_eq!(ranked[0].id, EntryId(1));
    assert_eq!(ranked[0].logical, Some(12345));
    assert_eq!(query.matches, broad);
    assert!(ranked[1..].iter().all(|row| row.logical == Some(0)));
    drop(query);
    let query_s = query_started.elapsed().as_secs_f64();
    let replay_started = Instant::now();
    let mut replay_directories = 0;
    let mut replay_files = 0;
    for batch in tree.replay()? {
        for row in batch?.observations {
            match row.kind {
                Kind::Directory => replay_directories += 1,
                Kind::File => replay_files += 1,
                _ => return Err("unexpected synthetic kind".into()),
            }
        }
    }
    assert_eq!(replay_directories, count);
    assert_eq!(replay_files, 1);
    let replay_s = replay_started.elapsed().as_secs_f64();
    let usage = governor.usage();
    let process_memory = memory();
    drop(tree);
    let after = governor.usage();
    assert_eq!(after.categories, initial.categories);
    assert_eq!(after.disk_used, initial.disk_used);
    println!(
        "{}",
        serde_json::json!({"phase":"finished", "synthetic":true, "filesystem_acquisition":false, "directories":count, "files":1, "entries":count+1, "nested_depth":DEPTH+1, "requested_mib":memory_mib, "effective_bytes":usage.limits.bytes, "acquisition_s":acquisition_s, "rank_queries_s":query_s, "replay_verification_s":replay_s, "elapsed_s":started.elapsed().as_secs_f64(), "logical":12345, "allocated":24690, "coverage":"Complete", "temporary_high_water_sampled":peak_temporary, "temporary_bytes_before_drop":summary.temporary_bytes, "global_disk_before_drop":usage.disk_used, "accounted_categories_before_drop":usage.categories, "working_set_and_peak_bytes":process_memory, "global_disk_after_drop":after.disk_used, "oracle":"PASS"})
    );
    Ok(())
}
