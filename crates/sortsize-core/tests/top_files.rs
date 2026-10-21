//! VIEW-004: native fixtures, independent full-sort oracle and scan policies.
use sortsize_core::{
    CancellationToken, Coverage, InventoryLimits, ScanOptions, TopFiles, scan_inventory_with_limits,
};
use std::{fs, path::PathBuf};
use tempfile::TempDir;

fn oracle(mut files: Vec<(PathBuf, u64)>) -> Vec<(PathBuf, u64)> {
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    files.truncate(100);
    files
}
fn values(top: &TopFiles) -> Vec<(PathBuf, u64)> {
    top.entries
        .iter()
        .map(|f| (f.path.clone(), f.logical_bytes))
        .collect()
}
fn file(path: PathBuf, size: u64) -> (PathBuf, u64) {
    fs::File::create(&path).unwrap().set_len(size).unwrap();
    // The local adapter reports canonical native paths (verbatim on Windows).
    // Build the independent oracle in that same identity domain.
    (fs::canonicalize(path).unwrap(), size)
}

#[test]
fn cardinalities_ties_zeros_and_workers_match_full_sort_even_without_tree_paths() {
    for count in [0, 1, 99, 100, 101, 801] {
        let fixture = TempDir::new().unwrap();
        for d in 0..8 {
            fs::create_dir(fixture.path().join(d.to_string())).unwrap();
        }
        let expected = oracle(
            (0..count)
                .map(|i| {
                    file(
                        fixture
                            .path()
                            .join((i % 8).to_string())
                            .join(format!("été 日本語 {i:04}")),
                        (i % 13) as u64,
                    )
                })
                .collect(),
        );
        for workers in [1, 2, 4, 8] {
            let mut options = ScanOptions::default();
            options.concurrency.workers = workers;
            for max_path_bytes in [1, 8 * 1024 * 1024] {
                let result = scan_inventory_with_limits(
                    fixture.path(),
                    &options,
                    &CancellationToken::default(),
                    InventoryLimits {
                        max_entries: 50_000,
                        max_path_bytes,
                    },
                    |_| {},
                )
                .unwrap();
                assert_eq!(values(&result.top_files), expected);
                assert_eq!(result.top_files.oversized_paths, 0);
                assert_eq!(result.report.coverage, Coverage::Complete);
                assert_eq!(result.report.files, count as u64);
                if max_path_bytes == 1 {
                    assert!(result.entries.is_empty());
                }
            }
        }
    }
}

#[test]
fn fifty_thousand_and_ten_files_late_winners_survive_retention_and_no_second_scan() {
    let fixture = TempDir::new().unwrap();
    let mut files: Vec<_> = (0..50_000)
        .map(|i| file(fixture.path().join(format!("small-{i:05}")), 0))
        .collect();
    let late = fixture.path().join("late");
    fs::create_dir(&late).unwrap();
    files.extend((0..10).map(|i| file(late.join(format!("winner-{i}")), 1000 + i)));
    let expected = oracle(files.clone());
    for workers in [2, 4, 8] {
        let mut options = ScanOptions::default();
        options.concurrency.workers = workers;
        let concurrent = scan_inventory_with_limits(
            fixture.path(),
            &options,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(values(&concurrent.top_files), expected);
        assert_eq!(concurrent.report.files, 50_010);
        assert_eq!(concurrent.entries.len(), 50_000);
    }
    let mut removed = false;
    let result = scan_inventory_with_limits(
        fixture.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        |p| {
            if p.files == 50_010 && !removed {
                // All metadata has been observed. A second scan cannot recover it.
                for (path, _) in &files {
                    fs::remove_file(path).unwrap();
                }
                removed = true;
            }
        },
    )
    .unwrap();
    assert!(removed);
    assert_eq!(result.report.coverage, Coverage::Complete);
    assert_eq!(result.report.files, 50_010);
    assert_eq!(result.entries.len(), 50_000);
    assert_eq!(result.entries_omitted, 12); // root and late directory
    assert_eq!(values(&result.top_files), expected);
    assert!(
        result.top_files.entries[..10]
            .iter()
            .all(|f| !result.entries.iter().any(|n| n.path == f.path))
    );
}

#[cfg(unix)]
#[test]
fn native_bytes_hardlinks_exclusions_symlinks_and_permission_errors() {
    use std::{
        ffi::OsString,
        os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
        },
    };
    let fixture = TempDir::new().unwrap();
    let a = file(
        fixture
            .path()
            .join(OsString::from_vec(b"native-\xff".to_vec())),
        7,
    );
    let b = file(
        fixture
            .path()
            .join(OsString::from_vec(b"native-\xfe".to_vec())),
        7,
    );
    let hard = fixture.path().join("hardlink");
    fs::hard_link(&a.0, &hard).unwrap();
    file(fixture.path().join("exclude"), 99999);
    symlink(fixture.path(), fixture.path().join("loop")).unwrap();
    let blocked = fixture.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    file(blocked.join("hidden"), 99999);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
    assert!(
        fs::read_dir(&blocked).is_err(),
        "Run as an unprivileged user to exercise real denial"
    );
    let expected = oracle(vec![a, b, (hard, 7)]);
    for workers in [1, 2, 4, 8] {
        let mut options = ScanOptions {
            exclusions: vec!["exclude".into()],
            ..Default::default()
        };
        options.concurrency.workers = workers;
        let result = scan_inventory_with_limits(
            fixture.path(),
            &options,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(values(&result.top_files), expected);
        assert_eq!(result.report.coverage, Coverage::Partial);
        assert_eq!(result.report.errors_count, 1);
        assert_eq!(result.report.logical_bytes, 21);
        assert_eq!(result.report.symlinks_skipped, 1);
        assert_eq!(result.report.excluded_entries, 1);
    }
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn cancellation_and_depth_limits_rank_only_observed_files() {
    let fixture = TempDir::new().unwrap();
    for i in 0..200 {
        file(fixture.path().join(format!("f{i:03}")), 3);
    }
    fs::create_dir(fixture.path().join("deep")).unwrap();
    file(fixture.path().join("deep/unseen"), 99999);
    for workers in [1, 2, 4, 8] {
        let mut options = ScanOptions::default();
        options.concurrency.workers = workers;
        let token = CancellationToken::default();
        let result = scan_inventory_with_limits(
            fixture.path(),
            &options,
            &token,
            InventoryLimits::default(),
            |p| {
                if p.files == 99 {
                    token.cancel();
                }
            },
        )
        .unwrap();
        assert_eq!(result.report.coverage, Coverage::Cancelled);
        let observed = oracle(
            result
                .entries
                .iter()
                .filter(|e| e.kind == sortsize_core::InventoryKind::File)
                .map(|e| (e.path.clone(), e.logical_bytes.unwrap()))
                .collect(),
        );
        assert_eq!(values(&result.top_files), observed);
        assert_eq!(result.report.files, 99);
        options.max_depth = Some(1);
        let result = scan_inventory_with_limits(
            fixture.path(),
            &options,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.report.coverage, Coverage::Partial);
        assert_eq!(result.top_files.entries.len(), 100);
        assert!(
            result
                .top_files
                .entries
                .iter()
                .all(|f| f.logical_bytes == 3)
        );
        let token = CancellationToken::default();
        token.cancel();
        let result = scan_inventory_with_limits(
            fixture.path(),
            &options,
            &token,
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.report.coverage, Coverage::Cancelled);
        assert!(result.top_files.entries.is_empty());
    }
}

#[test]
fn default_eight_mib_path_budget_does_not_limit_top100() {
    let fixture = TempDir::new().unwrap();
    let mut directory = fixture.path().to_owned();
    for _ in 0..50 {
        directory.push("long-directory-0123456789");
    }
    fs::create_dir_all(&directory).unwrap();
    let expected = oracle(
        (0..8000)
            .map(|i| file(directory.join(format!("f{i:05}")), i))
            .collect(),
    );
    let result = scan_inventory_with_limits(
        fixture.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        |_| {},
    )
    .unwrap();
    assert!(result.path_bytes_limit_reached);
    assert!(result.entries.len() < 50_000);
    assert!(result.retained_path_bytes <= 8 * 1024 * 1024);
    assert_eq!(values(&result.top_files), expected);
    assert_eq!(result.report.coverage, Coverage::Complete);
    assert_eq!(result.report.files, 8000);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the validation ACL guard and SORTSIZE_TEST_NTFS_DENIED"]
fn windows_utf16_hardlinks_exclusions_and_real_ntfs_denial() {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    let fixture = TempDir::new().unwrap();
    let root = fs::canonicalize(fixture.path()).unwrap();
    let mut expected = Vec::new();
    for units in [
        vec![0xd800],
        vec![0xdc00],
        vec![0xfffd],
        vec![0xd83d, 0xde00],
    ] {
        let mut name: Vec<u16> = "native-".encode_utf16().collect();
        name.extend(units);
        expected.push(file(root.join(OsString::from_wide(&name)), 77));
    }
    let hard = root.join("hardlink");
    fs::hard_link(&expected[0].0, &hard).unwrap();
    expected.push((hard, 77));
    expected.push(file(root.join("empty"), 0));
    file(root.join("excluded"), 99999);
    let blocked = std::env::var_os("SORTSIZE_TEST_NTFS_DENIED")
        .expect("Run with the validation ACL guard; actual NTFS denial is required");
    assert_eq!(
        fs::read_dir(&blocked).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    for workers in [1, 2, 4, 8] {
        let mut options = ScanOptions {
            exclusions: vec!["excluded".into()],
            ..Default::default()
        };
        options.concurrency.workers = workers;
        let result = scan_inventory_with_limits(
            &root,
            &options,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(values(&result.top_files), oracle(expected.clone()));
        assert_eq!(result.report.coverage, Coverage::Complete);
        assert_eq!(result.report.excluded_entries, 1);
        let partial = scan_inventory_with_limits(
            std::path::Path::new(&blocked).parent().unwrap(),
            &options,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(partial.report.coverage, Coverage::Partial);
        assert_eq!(partial.report.errors_count, 1);
        assert_eq!(partial.report.files, 1);
        assert_eq!(partial.top_files.entries[0].logical_bytes, 3);
    }
}
