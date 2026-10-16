use sortsize_core::{
    CancellationToken, Coverage, InventoryKind, InventoryLimits, InventoryReport, ScanOptions,
    scan, scan_inventory, scan_inventory_with_limits,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn inventory(root: &Path, limit: usize) -> InventoryReport {
    scan_inventory(
        root,
        &ScanOptions::default(),
        &CancellationToken::default(),
        limit,
        |_| {},
    )
    .unwrap()
}

#[test]
fn tree_aggregates_nested_empty_unicode_and_hardlinked_files() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("été/空")).unwrap();
    fs::write(fixture.path().join("été/data"), b"12345").unwrap();
    fs::hard_link(
        fixture.path().join("été/data"),
        fixture.path().join("alias"),
    )
    .unwrap();
    fs::write(fixture.path().join("empty"), []).unwrap();
    let result = inventory(fixture.path(), 100);
    let root = &result.entries[0];
    assert_eq!(root.parent, None);
    assert_eq!(root.logical_bytes, Some(10));
    assert_eq!(result.report.logical_bytes, 10);
    let folder = result
        .entries
        .iter()
        .find(|e| e.path.ends_with("été"))
        .unwrap();
    assert_eq!(folder.logical_bytes, Some(5));
    assert!(!folder.incomplete);
    let empty = result
        .entries
        .iter()
        .find(|e| e.path.ends_with("空"))
        .unwrap();
    assert_eq!(empty.kind, InventoryKind::Directory);
    assert_eq!(empty.logical_bytes, Some(0));
    assert_eq!(result.entries_omitted, 0);
    for (index, node) in result.entries.iter().enumerate().skip(1) {
        assert!(node.parent.unwrap() < index);
    }
}

#[test]
fn truncated_tree_keeps_full_scan_and_retained_directory_totals() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("nested/deeper")).unwrap();
    fs::write(fixture.path().join("nested/a"), b"123").unwrap();
    fs::write(fixture.path().join("nested/deeper/b"), b"12345").unwrap();
    for limit in 1..=6 {
        let result = inventory(fixture.path(), limit);
        assert!(result.entries.len() <= limit);
        assert_eq!(result.entries[0].logical_bytes, Some(8));
        assert_eq!(result.report.coverage, Coverage::Complete);
        assert_eq!(result.entries.len() as u64 + result.entries_omitted, 5);
        for directory in result
            .entries
            .iter()
            .filter(|e| e.kind == InventoryKind::Directory)
        {
            let summary = scan(
                &directory.path,
                &ScanOptions::default(),
                &CancellationToken::default(),
                |_| {},
            )
            .unwrap();
            assert_eq!(directory.logical_bytes, Some(summary.logical_bytes));
        }
    }
}

#[test]
fn exclusions_and_depth_limits_use_the_same_policy_as_the_summary() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join(".git/objects")).unwrap();
    fs::create_dir_all(fixture.path().join("child/grandchild")).unwrap();
    fs::write(fixture.path().join(".git/objects/hidden"), b"hidden").unwrap();
    let options = ScanOptions {
        exclusions: vec!["**/.git/**".into()],
        max_depth: Some(1),
        ..Default::default()
    };
    let result = scan_inventory(
        fixture.path(),
        &options,
        &CancellationToken::default(),
        100,
        |_| {},
    )
    .unwrap();
    assert_eq!(result.report.coverage, Coverage::Partial);
    assert_eq!(result.entries.len(), 2);
    assert!(result.entries.iter().all(|e| e.incomplete));
    assert_eq!(result.report.excluded_entries, 1);
}

#[test]
fn cancelled_inventory_preserves_observed_nodes_and_lower_bounds() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("file"), b"123").unwrap();
    let token = CancellationToken::default();
    let result = scan_inventory(fixture.path(), &ScanOptions::default(), &token, 10, |_| {
        token.cancel()
    })
    .unwrap();
    assert_eq!(result.report.coverage, Coverage::Cancelled);
    assert_eq!(result.entries.len(), 1);
    assert!(result.entries[0].incomplete);
    let token = CancellationToken::default();
    token.cancel();
    let result =
        scan_inventory(fixture.path(), &ScanOptions::default(), &token, 10, |_| {}).unwrap();
    assert!(result.entries.is_empty());
    assert_eq!(result.report.coverage, Coverage::Cancelled);
}

#[test]
fn zero_inventory_limit_is_rejected() {
    let fixture = TempDir::new().unwrap();
    assert!(
        scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            0,
            |_| {}
        )
        .is_err()
    );
}

#[test]
fn native_path_byte_budget_omits_nodes_without_losing_totals_or_coverage() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("été/long-child-name")).unwrap();
    fs::write(fixture.path().join("été/long-child-name/data"), b"12345").unwrap();
    fs::write(fixture.path().join("short"), b"123").unwrap();
    let root = fs::canonicalize(fixture.path()).unwrap();
    let root_bytes = root.as_os_str().as_encoded_bytes().len();
    for bytes in [
        1,
        root_bytes,
        root_bytes + 1,
        root_bytes * 3,
        8 * 1024 * 1024,
    ] {
        let result = scan_inventory_with_limits(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            InventoryLimits {
                max_entries: 100,
                max_path_bytes: bytes,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(result.report.files, 2);
        assert_eq!(result.report.directories, 3);
        assert_eq!(result.report.logical_bytes, 8);
        assert_eq!(result.report.coverage, Coverage::Complete);
        assert_eq!(result.entries.len() as u64 + result.entries_omitted, 5);
        assert!(result.retained_path_bytes <= bytes);
        assert_eq!(
            result.retained_path_bytes,
            result
                .entries
                .iter()
                .map(|n| n.path.as_os_str().as_encoded_bytes().len())
                .sum::<usize>()
        );
        for entry in &result.entries {
            if entry.kind == InventoryKind::Directory {
                let reference = scan(
                    &entry.path,
                    &ScanOptions::default(),
                    &CancellationToken::default(),
                    |_| {},
                )
                .unwrap();
                assert_eq!(entry.logical_bytes, Some(reference.logical_bytes));
            }
            if let Some(parent) = entry.parent {
                assert_eq!(
                    entry.path.parent(),
                    Some(result.entries[parent].path.as_path())
                );
            }
        }
        assert_eq!(result.path_bytes_limit_reached, bytes < 8 * 1024 * 1024);
    }
    assert!(
        scan_inventory_with_limits(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            InventoryLimits {
                max_path_bytes: 0,
                ..Default::default()
            },
            |_| {},
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn links_and_non_unicode_paths_are_preserved_without_following() {
    use std::ffi::OsString;
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let fixture = TempDir::new().unwrap();
    let file = fixture.path().join(OsString::from_vec(vec![b'f', 0xff]));
    fs::write(&file, b"123").unwrap();
    symlink(fixture.path(), fixture.path().join("cycle")).unwrap();
    let result = inventory(fixture.path(), 100);
    assert!(result.entries.iter().any(|e| e.path == file));
    let link = result
        .entries
        .iter()
        .find(|e| e.kind == InventoryKind::Symlink)
        .unwrap();
    assert_eq!(link.logical_bytes, None);
    assert_eq!(result.report.logical_bytes, 3);
    assert_eq!(result.report.symlinks_skipped, 1);
}
