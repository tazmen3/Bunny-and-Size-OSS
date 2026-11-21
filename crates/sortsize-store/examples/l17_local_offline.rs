//! Exercise the production snapshot reader, offline navigation and page query.
use serde_json::json;
use sortsize_core::{
    CancellationToken, InventoryKind,
    analysis::SearchQuery,
    inventory_contract::{EntryId, MemoryMode},
    resources::{Governor, disk_limit, native_free_disk, native_memory},
};
use sortsize_store::{direct_snapshot, direct_tree::query::PageQuery};
use std::{collections::HashSet, path::PathBuf, time::Instant};

const FILES_PER_FOLDER: u64 = 2_000;

fn logical_size(index: u64) -> u64 {
    if index.is_multiple_of(64) {
        (index / 64) % 4096 + 1
    } else {
        0
    }
}

fn name(path: &std::path::Path) -> &str {
    path.file_name().and_then(|name| name.to_str()).unwrap()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let snapshot = PathBuf::from(args.next().ok_or("missing SQLite snapshot")?);
    let scratch = PathBuf::from(args.next().ok_or("missing new scratch directory")?);
    let files: u64 = args
        .next()
        .ok_or("missing count")?
        .to_str()
        .ok_or("count")?
        .parse()?;
    let mode = args.next().unwrap_or_else(|| "auto".into());
    let mode = MemoryMode::from_key(mode.to_str().ok_or("mode")?).ok_or("invalid mode")?;
    let source = direct_snapshot::inspect(&snapshot)?.root;
    if source.try_exists()? {
        return Err("fixture must be made unavailable before running the offline probe".into());
    }
    std::fs::create_dir(&scratch)?;
    let governor = Governor::global();
    governor.configure(mode, native_memory());
    let mut before_order = false;
    for value in args {
        let value = value.to_str().ok_or("offline probe option")?;
        if value == "--before-order" {
            before_order = true;
        } else {
            let bytes = value
                .parse::<u64>()?
                .checked_mul(1024 * 1024)
                .filter(|&bytes| bytes > 0)
                .ok_or("invalid memory limit")?;
            governor.set_byte_limit(Some(bytes));
        }
    }
    let quota = disk_limit(native_free_disk(&scratch).ok_or("scratch capacity unavailable")?);
    let token = CancellationToken::default();
    let clock = Instant::now();
    let loaded = direct_snapshot::load(&snapshot, &scratch, quota, &token)?;
    let load_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let tree = loaded.tree;
    let start = Instant::now();
    let (name_index, file_order) = if before_order {
        (None, None)
    } else {
        let (name, order) = tree.prepare_query_indexes(&token);
        (Some(name), Some(order))
    };
    let index_preparation_ms = (!before_order).then(|| start.elapsed().as_secs_f64() * 1000.0);
    let expected_folders = files.div_ceil(FILES_PER_FOLDER);
    assert_eq!(tree.len(), files + expected_folders + 1);
    let sum = tree
        .summary()
        .measure
        .logical
        .ok_or("incomplete fixture total")?;

    let mut search = Vec::new();
    for (scenario, name_filter, matches) in [
        ("late", Some(format!("f{:08}", files - 1)), 1),
        ("absent", Some("no-such-entry-l17".into()), 0),
        ("all_files", None, files),
    ] {
        let criteria = SearchQuery {
            name: name_filter,
            kind: Some(InventoryKind::File),
            ..SearchQuery::default()
        };
        let start = Instant::now();
        let mut query = PageQuery::new(&tree, None, Some(&criteria), 2, None, 200)?;
        while !query.advance(&tree, &token)? {}
        let rows = query.rows(&tree)?;
        let first_page_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(query.files, matches);
        assert_eq!(query.gaps, 0);
        let mut next_page_ms = None;
        if scenario == "all_files" {
            assert_eq!(query.logical, u128::from(sum));
            assert_eq!(rows.len(), 200);
            assert!(query.next().is_some());
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(name(&row.path), format!("f{index:08}"));
            }
            if files >= 400 {
                let anchor = query.next();
                let start = Instant::now();
                let mut next = PageQuery::new(&tree, None, Some(&criteria), 2, anchor, 200)?;
                while !next.advance(&tree, &token)? {}
                let rows = next.rows(&tree)?;
                next_page_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(rows.len(), 200);
                assert_eq!(next.files, files);
                assert_eq!(next.logical, u128::from(sum));
                for (index, row) in rows.iter().enumerate() {
                    assert_eq!(name(&row.path), format!("f{:08}", index + 200));
                }
            }
        }
        if scenario == "late" {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].logical, Some(logical_size(files - 1)));
        }
        search.push(json!({"scenario": scenario, "exact_matches": matches,
            "first_page_ms": first_page_ms, "lt_100_ms": first_page_ms < 100.0,
            "next_page_ms": next_page_ms}));
    }
    let start = Instant::now();
    let mut seen_folders = HashSet::new();
    let mut root_cursor = None;
    let mut pages_ms = Vec::new();
    let mut navigated_sum = 0u64;
    loop {
        let page_clock = Instant::now();
        let page = tree.children(EntryId(0), root_cursor, 200, true)?;
        pages_ms.push(page_clock.elapsed().as_secs_f64() * 1000.0);
        for folder in page.rows {
            let directory: u64 = name(&folder.path)
                .strip_prefix('d')
                .ok_or("invalid folder")?
                .parse()?;
            assert!(directory < expected_folders && seen_folders.insert(directory));
            let first = directory * FILES_PER_FOLDER;
            let end = (first + FILES_PER_FOLDER).min(files);
            let mut seen_files = HashSet::with_capacity((end - first) as usize);
            let mut cursor = None;
            loop {
                let page_clock = Instant::now();
                let rows = tree.children(folder.id, cursor, 200, false)?;
                pages_ms.push(page_clock.elapsed().as_secs_f64() * 1000.0);
                for row in rows.rows {
                    let index: u64 = name(&row.path)
                        .strip_prefix('f')
                        .ok_or("invalid file")?
                        .parse()?;
                    assert!((first..end).contains(&index) && seen_files.insert(index));
                    let expected = logical_size(index);
                    assert_eq!(row.logical, Some(expected));
                    navigated_sum += expected;
                }
                let Some(next) = rows.next else { break };
                cursor = Some(next);
            }
            assert_eq!(seen_files.len() as u64, end - first);
        }
        let Some(next) = page.next else { break };
        root_cursor = Some(next);
    }
    assert_eq!(seen_folders.len() as u64, expected_folders);
    assert_eq!(navigated_sum, sum);
    pages_ms.sort_by(f64::total_cmp);
    let page_count = pages_ms.len();
    let page_p95_ms = pages_ms[(page_count * 95).div_ceil(100) - 1];
    let navigation_ms = start.elapsed().as_secs_f64() * 1000.0;
    let used = governor.usage();
    println!(
        "{}",
        json!({"status":"PASS_SCOPED_OFFLINE_EXACTNESS", "files":files,
        "offline_load_ms":load_ms, "navigation_audit_ms":navigation_ms, "navigation_pages":page_count,
        "index_preparation_ms": index_preparation_ms, "name_index": name_index, "file_order": file_order,
        "navigation_page_p95_ms":page_p95_ms, "logical_bytes":sum, "exact_search":search,
        "start_before_reusable_order": before_order,
        "accounted_memory_bytes":used.categories.iter().sum::<u64>(), "disk_accounted_bytes":used.disk_used,
        "disk_limit_bytes":quota, "memory_limit_bytes":used.limits.bytes})
    );
    Ok(())
}
