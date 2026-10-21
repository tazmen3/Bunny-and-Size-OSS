use super::*;
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, InventoryLimits, ScanOptions, ValueAccuracy,
    scan_inventory_with_dates,
};
use std::fs;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;

pub(super) fn acquire(
    path: &std::path::Path,
    workers: usize,
    limits: InventoryLimits,
) -> InventoryWithDates {
    scan_inventory_with_dates(
        path,
        &ScanOptions {
            concurrency: ConcurrencyOptions {
                workers,
                ..Default::default()
            },
            ..Default::default()
        },
        &CancellationToken::default(),
        limits,
        |_| {},
    )
    .unwrap()
}
fn exact(index: usize) -> MetadataValue<std::time::SystemTime> {
    MetadataValue::Known {
        value: UNIX_EPOCH - Duration::new(315_619_200 + index as u64, 123_456_700),
        accuracy: ValueAccuracy::Exact,
    }
}
fn verify_page(tree: &Tree, expected: &[(PathBuf, EntryDates)]) {
    for row in tree.details().0 {
        let id = row.node_id as usize;
        assert_eq!(row.path, native_label(&expected[id].0));
        assert_eq!(tree.nodes[id].path, expected[id].0);
        assert_eq!(
            row.modified,
            crate::date_label::format_date(expected[id].1.modified)
        );
        assert_eq!(
            row.accessed,
            crate::date_label::format_date(expected[id].1.accessed)
        );
    }
}

#[test]
fn aligned_dates_survive_sort_pages_navigation_and_deleted_native_source() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("été 日本語/nested")).unwrap();
    fs::write(fixture.path().join("été 日本語/nested/child"), b"child").unwrap();
    fs::write(fixture.path().join("é".repeat(110)), b"long").unwrap();
    for i in 0..2105 {
        fs::write(fixture.path().join(format!("f{i:04}")), [0; 3]).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        fs::write(
            fixture
                .path()
                .join(std::ffi::OsString::from_vec(vec![b'n', 0xff])),
            b"native",
        )
        .unwrap();
    }
    let mut results = Vec::new();
    for workers in [1, 4, 8] {
        let mut result = acquire(fixture.path(), workers, InventoryLimits::default());
        for (i, dates) in result.dates.iter_mut().enumerate() {
            dates.modified = exact(i);
            dates.accessed = match i % 5 {
                0 => exact(i + 10),
                1 => MetadataValue::Unknown,
                2 => MetadataValue::Unsupported,
                3 => MetadataValue::AccessDenied,
                _ => MetadataValue::NotCollected,
            };
        }
        let pointer = result.dates.as_ptr();
        let expected: Vec<_> = result
            .inventory
            .entries
            .iter()
            .zip(&result.dates)
            .map(|(e, d)| (e.path.clone(), *d))
            .collect();
        let (tree, _) = Tree::with_dates(result).unwrap();
        assert_eq!(tree.dates.as_ref().unwrap().as_ptr(), pointer);
        results.push((tree, expected));
    }
    fixture.close().unwrap();
    for (mut tree, expected) in results {
        let mut inspected = 0;
        for sort in 0..12 {
            tree.sort = sort;
            tree.detail_offset = 0;
            assert_eq!(tree.offset(true), 0);
            verify_page(&tree, &expected);
            let first = tree.details();
            assert_eq!(first.0.len(), 2000);
            assert!(first.1);
            inspected += first.0.len();
            tree.change_page(true, true);
            verify_page(&tree, &expected);
            assert!(!tree.details().1);
            assert!(tree.details().0.len() > 100);
            tree.change_page(true, false);
        }
        assert_eq!(inspected, 24000);
        let folder = tree
            .nodes
            .iter()
            .position(|n| n.path.ends_with("été 日本語"))
            .unwrap();
        tree.select(folder);
        verify_page(&tree, &expected);
        let nested = tree.details().0[0].node_id as usize;
        tree.select(nested);
        verify_page(&tree, &expected);
        assert_eq!(tree.details().0[0].name, "child");
        assert!(!tree.treemap(800.0, 200.0).is_empty());
        tree.parent();
        assert_eq!(tree.selected, Some(folder));
        tree.parent();
        assert_eq!(tree.selected, Some(0));
        let (_, target) = tree.top_selection(0).unwrap();
        let target = target.unwrap();
        tree.select_entry_parent(target);
        tree.reveal_entry(target);
        verify_page(&tree, &expected);
        assert!(tree.details().0.iter().any(|r| r.node_id == target as i32));
    }
}

#[test]
fn broken_date_alignment_is_rejected_and_legacy_trees_show_not_collected() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("data"), b"x").unwrap();
    let mut result = acquire(fixture.path(), 1, InventoryLimits::default());
    result.dates.pop();
    assert!(Tree::with_dates(result).is_err());
    let result = acquire(fixture.path(), 1, InventoryLimits::default());
    let (tree, _) = Tree::new(result.inventory);
    assert!(tree.dates.is_none());
    let row = &tree.details().0[0];
    assert_eq!(row.modified, "Non collectée");
    assert_eq!(row.accessed, "Non collectée");
}

#[test]
fn dated_inventory_cap_keeps_ids_top100_and_treemap_without_formatting_all_nodes() {
    let fixture = TempDir::new().unwrap();
    for i in 0..50010 {
        fs::write(fixture.path().join(format!("f{i:05}")), b"x").unwrap();
    }
    let result = acquire(fixture.path(), 4, InventoryLimits::default());
    assert_eq!(result.dates.len(), 50000);
    assert_eq!(result.inventory.entries_omitted, 11);
    let (mut tree, report) = Tree::with_dates(result).unwrap();
    fixture.close().unwrap();
    assert_eq!(report.files, 50010);
    assert_eq!(tree.dates.as_ref().unwrap().len(), 50000);
    assert_eq!(tree.details().0.len(), 2000);
    assert_eq!(tree.top_rows.len(), 100);
    assert!(
        tree.nodes[0]
            .map
            .as_ref()
            .unwrap()
            .weights
            .contains(&treemap::Weight {
                target: Target::Omitted,
                bytes: 11
            })
    );
    for _ in 0..24 {
        tree.change_page(true, true);
    }
    assert_eq!(tree.details().0.len(), 1999);
    for row in tree.details().0 {
        let id = row.node_id as usize;
        assert_eq!(
            row.modified,
            crate::date_label::format_date(tree.dates.as_ref().unwrap()[id].modified)
        );
        assert_eq!(row.path, tree.entry_path(id).unwrap());
    }
}

#[test]
fn cancellation_and_partial_coverage_do_not_hide_exact_node_dates() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("nested/child")).unwrap();
    for workers in [1, 4, 8] {
        let mut opts = ScanOptions {
            max_depth: Some(1),
            concurrency: ConcurrencyOptions {
                workers,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = scan_inventory_with_dates(
            fixture.path(),
            &opts,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        let (mut tree, report) = Tree::with_dates(result).unwrap();
        for accessed in [false, false, true, true] {
            tree.sort_dates(accessed);
            assert!(!tree.details().0.is_empty());
        }
        assert_eq!(report.coverage, Coverage::Partial);
        assert!(tree.nodes[0].incomplete);
        assert!(tree.details().0[0].modified.ends_with(" UTC"));
        let token = CancellationToken::default();
        opts.max_depth = None;
        let result = scan_inventory_with_dates(
            fixture.path(),
            &opts,
            &token,
            InventoryLimits::default(),
            |_| token.cancel(),
        )
        .unwrap();
        let (mut tree, report) = Tree::with_dates(result).unwrap();
        for accessed in [false, false, true, true] {
            tree.sort_dates(accessed);
            assert!(tree.details().0.is_empty());
        }
        assert_eq!(report.coverage, Coverage::Cancelled);
        assert_eq!(tree.dates.as_ref().unwrap().len(), 1);
        assert!(tree.details().0.is_empty());
        assert!(matches!(
            tree.dates.as_ref().unwrap()[0].modified,
            MetadataValue::Known {
                accuracy: ValueAccuracy::Exact,
                ..
            }
        ));
    }
}

#[cfg(windows)]
#[test]
fn unpaired_windows_surrogate_is_preserved_in_memory_with_its_date_identity() {
    use std::os::windows::ffi::OsStringExt;
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("placeholder"), b"x").unwrap();
    let mut result = acquire(fixture.path(), 1, InventoryLimits::default());
    let native = result.inventory.entries[0]
        .path
        .join(std::ffi::OsString::from_wide(&[0x006e, 0xd800]));
    result.inventory.entries[1].path = native.clone();
    result.dates[1].modified = exact(123);
    let (tree, _) = Tree::with_dates(result).unwrap();
    assert_eq!(tree.nodes[1].path, native);
    assert_eq!(
        tree.details().0[0].modified,
        crate::date_label::format_date(exact(123))
    );
    assert!(tree.details().0[0].path.contains("D800") || tree.details().0[0].path.contains("d800"));
}
