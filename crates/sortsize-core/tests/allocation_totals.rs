//! #14 B: aggregate once from observations, independently of retained nodes.
#![cfg(any(target_os = "linux", windows))]
use sortsize_core::{
    AllocationAggregationOptions, CancellationToken, ConcurrencyOptions, Coverage, InventoryKind,
    InventoryLimits, InventoryWithAllocationTotals, Metadata, MetadataSelection, MetadataValue,
    ScanOptions, ValueAccuracy, scan_inventory_with_allocation_totals,
    scan_inventory_with_metadata,
};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use tempfile::TempDir;
fn value<T: std::fmt::Debug>(v: MetadataValue<T>) -> T {
    match v {
        MetadataValue::Known { value, .. } => value,
        other => panic!("unavailable: {other:?}"),
    }
}
// Test oracle only. These calls are outside the product's single traversal.
fn native_allocated(path: &Path) -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
        };
        let file = File::open(path).unwrap();
        // SAFETY: zero is valid for this C struct; the API receives its exact
        // type, class and buffer size, and we check success before reading it.
        let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&mut info as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        };
        assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
        u64::try_from(info.AllocationSize).unwrap()
    }
}
fn options(workers: usize) -> ScanOptions {
    ScanOptions {
        concurrency: ConcurrencyOptions {
            workers,
            max_buffered_observations: 3,
            ..Default::default()
        },
        ..Default::default()
    }
}
fn aggregation(limit: usize) -> AllocationAggregationOptions {
    AllocationAggregationOptions {
        max_identities: limit,
        metadata: MetadataSelection {
            dates: true,
            allocation: true,
        },
        ..Default::default()
    }
}
fn collect(
    root: &Path,
    workers: usize,
    limits: InventoryLimits,
    budget: usize,
) -> InventoryWithAllocationTotals {
    scan_inventory_with_allocation_totals(
        root,
        &options(workers),
        &CancellationToken::default(),
        limits,
        aggregation(budget),
        |_| {},
    )
    .unwrap()
}
fn check(result: &InventoryWithAllocationTotals) {
    let inventory = &result.metadata.inventory;
    assert_eq!(result.entries_by_path.len(), inventory.entries.len());
    assert!(result.entries_by_path.capacity() <= 50_000);
    assert_eq!(inventory.report.allocated_bytes, None);
    assert_eq!(inventory.report.potentially_reclaimable_bytes, None);
    assert_eq!(
        result.totals.potentially_reclaimable_bytes,
        MetadataValue::Unknown
    );
    assert_eq!(result.totals.by_path.coverage, inventory.report.coverage);
    assert_eq!(
        result.totals.by_identity.coverage,
        inventory.report.coverage
    );
}
fn fixture(root: &Path) {
    fs::create_dir(root.join("a")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("a/original"), [7; 8193]).unwrap();
    fs::write(root.join("b/equal-content"), [7; 8193]).unwrap();
    fs::write(root.join("empty"), []).unwrap();
    fs::hard_link(root.join("a/original"), root.join("a/link")).unwrap();
    fs::hard_link(root.join("a/original"), root.join("b/link")).unwrap();
    let mut sparse = File::create(root.join("sparse")).unwrap();
    #[cfg(windows)]
    assert!(
        std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(root.join("sparse"))
            .status()
            .unwrap()
            .success()
    );
    sparse.set_len(64 * 1024 * 1024).unwrap();
    sparse.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
    sparse.write_all(&[1; 4096]).unwrap();
    sparse.sync_all().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(root, root.join("cycle")).unwrap();
        symlink(root.join("absent"), root.join("broken")).unwrap();
    }
}

#[test]
fn file_directory_and_identity_sums_are_distinct_and_independent_of_worker_order() {
    let temp = TempDir::new().unwrap();
    fixture(temp.path());
    let outside = TempDir::new_in(temp.path().parent().unwrap()).unwrap();
    fs::hard_link(temp.path().join("a/original"), outside.path().join("link")).unwrap();
    let mut oracle = None;
    for workers in [1, 4, 8] {
        let result = collect(temp.path(), workers, InventoryLimits::default(), 16);
        check(&result);
        let inventory = &result.metadata.inventory;
        assert_eq!(inventory.report.coverage, Coverage::Complete);
        let mut by_path = 0;
        let mut unique = HashMap::new();
        for (index, entry) in inventory.entries.iter().enumerate() {
            let total = result.entries_by_path[index];
            assert_eq!(total.coverage, Coverage::Complete);
            if entry.kind == InventoryKind::File {
                let allocation = result.metadata.allocations[index];
                assert_eq!(total.value, allocation.allocated_bytes);
                by_path += value(allocation.allocated_bytes);
                unique
                    .entry(value(allocation.identity))
                    .or_insert(value(allocation.allocated_bytes));
                if entry.path.ends_with("a/original") {
                    assert_eq!(value(allocation.link_count), 4);
                }
            } else if entry.kind == InventoryKind::Directory {
                let expected: u64 = inventory
                    .entries
                    .iter()
                    .zip(&result.metadata.allocations)
                    .filter(|(e, _)| {
                        e.kind == InventoryKind::File && e.path.starts_with(&entry.path)
                    })
                    .map(|(_, a)| value(a.allocated_bytes))
                    .sum();
                assert_eq!(value(total.value), expected);
            } else {
                assert_eq!(total.value, MetadataValue::NotCollected);
            }
        }
        assert_eq!(value(result.totals.by_path.value), by_path);
        assert_eq!(
            value(result.totals.by_identity.value),
            unique.values().sum::<u64>()
        );
        assert!(value(result.totals.by_identity.value) < by_path);
        assert_eq!(
            result.totals.identity_tracking.retained_identities,
            unique.len()
        );
        assert_eq!(value(result.entries_by_path[0].value), by_path);
        let shape: BTreeMap<_, _> = inventory
            .entries
            .iter()
            .zip(&result.entries_by_path)
            .map(|(entry, total)| {
                (
                    entry.path.clone(),
                    (entry.logical_bytes, entry.counts, *total),
                )
            })
            .collect();
        if let Some(oracle) = &oracle {
            assert_eq!(&shape, oracle);
        } else {
            oracle = Some(shape);
        }
        let legacy = scan_inventory_with_metadata(
            temp.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            aggregation(16).metadata,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            legacy.inventory.report.logical_bytes,
            inventory.report.logical_bytes
        );
        assert_eq!(
            legacy
                .inventory
                .top_files
                .entries
                .iter()
                .map(|e| (&e.path, e.logical_bytes))
                .collect::<Vec<_>>(),
            inventory
                .top_files
                .entries
                .iter()
                .map(|e| (&e.path, e.logical_bytes))
                .collect::<Vec<_>>()
        );
        let dates = |i: &sortsize_core::InventoryWithMetadata| {
            i.inventory
                .entries
                .iter()
                .zip(&i.dates)
                .map(|(e, d)| (e.path.clone(), d.modified))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(dates(&legacy), dates(&result.metadata));
    }
}

#[test]
fn budgets_and_raw_property_selection_do_not_change_path_totals() {
    let temp = TempDir::new().unwrap();
    fixture(temp.path());
    for workers in [1, 4, 8] {
        let expected = collect(temp.path(), workers, InventoryLimits::default(), 16);
        for budget in [0, 1, 2] {
            let result = collect(temp.path(), workers, InventoryLimits::default(), budget);
            assert_eq!(result.totals.by_path, expected.totals.by_path);
            assert_eq!(result.totals.by_identity.value, MetadataValue::Unknown);
            assert!(result.totals.identity_tracking.budget_exhausted);
            assert_eq!(result.totals.identity_tracking.retained_identities, budget);
            assert_eq!(
                result.metadata.inventory.report.coverage,
                Coverage::Complete
            );
        }
        for dates in [false, true] {
            for allocation in [false, true] {
                let result = scan_inventory_with_allocation_totals(
                    temp.path(),
                    &options(workers),
                    &CancellationToken::default(),
                    InventoryLimits::default(),
                    AllocationAggregationOptions {
                        metadata: MetadataSelection { dates, allocation },
                        deduplicate_by_identity: false,
                        max_identities: 0,
                    },
                    |_| {},
                )
                .unwrap();
                assert_eq!(result.totals.by_path, expected.totals.by_path);
                assert_eq!(result.totals.by_identity.value, MetadataValue::NotCollected);
                assert_eq!(result.totals.identity_tracking.capacity, 0);
                assert_eq!(result.metadata.dates.is_empty(), !dates);
                assert_eq!(result.metadata.allocations.is_empty(), !allocation);
            }
        }
        for limits in [
            InventoryLimits {
                max_entries: 1,
                ..Default::default()
            },
            InventoryLimits {
                max_entries: 3,
                ..Default::default()
            },
            InventoryLimits {
                max_path_bytes: 1,
                ..Default::default()
            },
            InventoryLimits {
                max_path_bytes: fs::canonicalize(temp.path())
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .len(),
                ..Default::default()
            },
        ] {
            let result = collect(temp.path(), workers, limits, 16);
            assert_eq!(result.totals.by_path, expected.totals.by_path);
            assert_eq!(result.totals.by_identity, expected.totals.by_identity);
            check(&result);
        }
    }
}

#[test]
fn empty_scopes_cancellation_and_partial_scopes_keep_accuracy_separate_from_coverage() {
    let temp = TempDir::new().unwrap();
    for workers in [1, 4, 8] {
        let result = collect(temp.path(), workers, InventoryLimits::default(), 0);
        for total in [result.totals.by_path, result.totals.by_identity] {
            assert_eq!(
                total,
                Metadata {
                    value: MetadataValue::Known {
                        value: 0,
                        accuracy: ValueAccuracy::Exact
                    },
                    coverage: Coverage::Complete
                }
            );
        }
        assert_eq!(result.totals.identity_tracking.capacity, 0);
    }
    fixture(temp.path());
    for workers in [1, 4, 8] {
        for after in [0, 1, 3] {
            let token = CancellationToken::default();
            if after == 0 {
                token.cancel();
            }
            let result = scan_inventory_with_allocation_totals(
                temp.path(),
                &options(workers),
                &token,
                InventoryLimits::default(),
                aggregation(16),
                |p| {
                    if p.files >= after {
                        token.cancel();
                    }
                },
            )
            .unwrap();
            check(&result);
            assert_eq!(result.metadata.inventory.report.files, after);
            assert_eq!(result.totals.by_path.coverage, Coverage::Cancelled);
            for total in [result.totals.by_path, result.totals.by_identity] {
                assert!(matches!(
                    total.value,
                    MetadataValue::Known {
                        accuracy: ValueAccuracy::LowerBound,
                        ..
                    }
                ));
            }
            for (entry, total) in result
                .metadata
                .inventory
                .entries
                .iter()
                .zip(&result.entries_by_path)
            {
                if entry.kind == InventoryKind::File {
                    assert!(matches!(
                        total.value,
                        MetadataValue::Known {
                            accuracy: ValueAccuracy::Exact,
                            ..
                        }
                    ));
                    assert_eq!(total.coverage, Coverage::Complete);
                }
            }
        }
        let result = scan_inventory_with_allocation_totals(
            temp.path(),
            &ScanOptions {
                max_depth: Some(1),
                ..options(workers)
            },
            &CancellationToken::default(),
            InventoryLimits::default(),
            aggregation(16),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.totals.by_path.coverage, Coverage::Partial);
        assert!(matches!(
            result.totals.by_path.value,
            MetadataValue::Known {
                accuracy: ValueAccuracy::LowerBound,
                ..
            }
        ));
    }
}

#[test]
fn all_omitted_files_and_cross_directory_links_contribute_after_50000_retained_entries() {
    let temp = TempDir::new().unwrap();
    let mut expected = 0;
    for i in 0..50_000 {
        let path = temp.path().join(format!("f-{i:05}"));
        File::create(&path).unwrap();
        expected += native_allocated(&path);
    }
    fs::write(temp.path().join("seed"), [3; 4096]).unwrap();
    fs::create_dir(temp.path().join("late")).unwrap();
    expected += native_allocated(&temp.path().join("seed"));
    for i in 0..12 {
        let path = temp.path().join(format!("late/f-{i}"));
        fs::write(&path, [4; 8192]).unwrap();
        expected += native_allocated(&path);
    }
    let unique = expected;
    for i in 0..2 {
        let path = temp.path().join(format!("late/link-{i}"));
        fs::hard_link(temp.path().join("seed"), &path).unwrap();
        expected += native_allocated(&path);
    }
    for workers in [1, 4, 8] {
        let result = collect(temp.path(), workers, InventoryLimits::default(), 65_536);
        check(&result);
        let inventory = &result.metadata.inventory;
        assert_eq!(inventory.entries.len(), 50_000);
        assert_eq!(result.entries_by_path.capacity(), 50_000);
        assert_eq!(inventory.report.files, 50_015);
        assert_eq!(inventory.entries_omitted, 17);
        assert_eq!(value(result.totals.by_path.value), expected);
        assert_eq!(value(result.entries_by_path[0].value), expected);
        assert_eq!(value(result.totals.by_identity.value), unique);
        assert_eq!(result.totals.identity_tracking.retained_identities, 50_013);
        assert_eq!(inventory.report.coverage, Coverage::Complete);
        assert_eq!(inventory.entries[0].counts.unwrap().recursive_files, 50_015);
        assert!(
            inventory
                .top_files
                .entries
                .iter()
                .take(12)
                .all(|e| e.logical_bytes == 8192)
        );
        for top in &inventory.top_files.entries {
            if let Some(index) = top.retained_entry {
                assert_eq!(inventory.entries[index].path, top.path);
            }
        }
        if workers == 1 {
            assert!(
                inventory
                    .top_files
                    .entries
                    .iter()
                    .take(12)
                    .all(|e| e.retained_entry.is_none())
            );
        }
    }
}

#[test]
fn aggregates_survive_file_disappearance_after_observation() {
    for workers in [1, 4, 8] {
        let temp = TempDir::new().unwrap();
        for i in 0..100 {
            fs::write(temp.path().join(format!("f-{i}")), [1; 4096]).unwrap();
        }
        let result = scan_inventory_with_allocation_totals(
            temp.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            aggregation(128),
            |p| {
                if p.files > 0 {
                    fs::remove_file(&p.current_path).unwrap();
                }
            },
        )
        .unwrap();
        check(&result);
        assert_eq!(
            result.metadata.inventory.report.coverage,
            Coverage::Complete
        );
        assert_eq!(result.metadata.inventory.report.files, 100);
        assert_eq!(result.totals.by_path, result.totals.by_identity);
        assert_eq!(
            value(result.entries_by_path[0].value),
            value(result.totals.by_path.value)
        );
        assert_eq!(result.totals.identity_tracking.retained_identities, 100);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn denied_listing_is_a_partial_sum_while_accessible_sibling_remains_exact() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let temp = TempDir::new().unwrap();
    let blocked = temp.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("hidden"), [1; 8192]).unwrap();
    fs::create_dir(temp.path().join("good")).unwrap();
    fs::write(temp.path().join("good/f"), [2; 4096]).unwrap();
    let expected = fs::symlink_metadata(temp.path().join("good/f"))
        .unwrap()
        .blocks()
        * 512;
    struct Restore<'a>(&'a Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(&blocked);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read_dir(&blocked).is_err(),
        "native denial requires an unprivileged account"
    );
    for workers in [1, 4, 8] {
        let result = collect(temp.path(), workers, InventoryLimits::default(), 8);
        assert_eq!(
            result.totals.by_path,
            Metadata {
                value: MetadataValue::Known {
                    value: expected,
                    accuracy: ValueAccuracy::LowerBound
                },
                coverage: Coverage::Partial
            }
        );
        assert_eq!(result.totals.by_identity, result.totals.by_path);
        assert!(
            result
                .metadata
                .inventory
                .report
                .errors
                .iter()
                .any(|e| e.kind == "PermissionDenied")
        );
        let i = result
            .metadata
            .inventory
            .entries
            .iter()
            .position(|e| e.path.ends_with("good"))
            .unwrap();
        assert_eq!(
            result.entries_by_path[i],
            Metadata {
                value: MetadataValue::Known {
                    value: expected,
                    accuracy: ValueAccuracy::Exact
                },
                coverage: Coverage::Complete
            }
        );
    }
}
