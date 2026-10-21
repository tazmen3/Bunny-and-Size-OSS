//! SCAN-009/010/014/015: compare the real concurrent pipeline to its sequential
//! reference, including native paths, global quotas and backpressure.
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, Coverage, InventoryKind, InventoryLimits, ScanFailure,
    ScanOptions, TraversalLimits, scan, scan_inventory_with_limits,
};
use std::{collections::BTreeMap, fs};
use tempfile::TempDir;

fn options(workers: usize) -> ScanOptions {
    ScanOptions {
        concurrency: ConcurrencyOptions {
            workers,
            ..Default::default()
        },
        ..Default::default()
    }
}
fn fixture() -> TempDir {
    let root = TempDir::new().unwrap();
    for n in 0..40 {
        let child = root.path().join(format!("été-{n:02}/空"));
        fs::create_dir_all(&child).unwrap();
        for k in 0..5 {
            fs::write(child.join(format!("{k}")), b"xyz").unwrap();
        }
    }
    fs::hard_link(root.path().join("été-00/空/0"), root.path().join("alias")).unwrap();
    root
}

#[test]
fn every_worker_count_matches_native_inventory_and_publishes_parents_first() {
    let root = fixture();
    let mut reference = None;
    for workers in [1, 2, 4, 8] {
        let inventory = scan_inventory_with_limits(
            root.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        let report = &inventory.report;
        assert_eq!(report.coverage, Coverage::Complete);
        assert_eq!(
            (report.files, report.directories, report.logical_bytes),
            (201, 81, 603)
        );
        assert_eq!(report.allocated_bytes, None);
        assert_eq!(report.potentially_reclaimable_bytes, None);
        assert_eq!(report.traversal_stats.worker_threads, workers);
        assert!(report.traversal_stats.peak_open_directories <= workers);
        assert_eq!(inventory.entries_omitted, 0);
        let mut values = BTreeMap::new();
        for (index, entry) in inventory.entries.iter().enumerate() {
            if let Some(parent) = entry.parent {
                assert!(parent < index);
                assert_eq!(
                    entry.path.parent(),
                    Some(inventory.entries[parent].path.as_path())
                );
            }
            assert!(
                values
                    .insert(
                        entry.path.clone(),
                        (
                            entry.kind,
                            entry.logical_bytes,
                            entry.incomplete,
                            entry.counts
                        )
                    )
                    .is_none()
            );
        }
        if let Some(reference) = &reference {
            assert_eq!(&values, reference);
        } else {
            reference = Some(values);
        }
    }
}

#[test]
fn single_slot_backpressure_and_global_inventory_budgets_keep_exact_totals() {
    let root = fixture();
    let path_limit = fs::canonicalize(root.path())
        .unwrap()
        .as_os_str()
        .as_encoded_bytes()
        .len()
        + 32;
    let mut options = options(8);
    options.concurrency.max_buffered_observations = 1;
    options.concurrency.max_buffered_path_bytes = path_limit;
    options.max_open_directories = 2;
    let inventory = scan_inventory_with_limits(
        root.path(),
        &options,
        &CancellationToken::default(),
        InventoryLimits {
            max_entries: 7,
            max_path_bytes: 1024,
        },
        |_| {},
    )
    .unwrap();
    assert_eq!(inventory.report.coverage, Coverage::Complete);
    assert_eq!(
        (inventory.report.files, inventory.report.logical_bytes),
        (201, 603)
    );
    assert!(inventory.entries.len() <= 7);
    assert!(inventory.retained_path_bytes <= 1024);
    assert_eq!(
        inventory.entries.len() as u64 + inventory.entries_omitted,
        282
    );
    assert_eq!(inventory.entries[0].logical_bytes, Some(603));
    let stats = inventory.report.traversal_stats;
    assert_eq!(stats.worker_threads, 2);
    assert_eq!(stats.peak_buffered_observations, 1);
    assert!(stats.peak_buffered_path_bytes <= path_limit);
    assert!(stats.peak_open_directories <= 2);
}

#[test]
fn pending_quotas_are_shared_and_partial_totals_remain_lower_bounds() {
    let root = fixture();
    let mut options = options(8);
    options.traversal_limits = TraversalLimits {
        max_pending_directories: 1,
        max_pending_path_bytes: 1024,
    };
    let inventory = scan_inventory_with_limits(
        root.path(),
        &options,
        &CancellationToken::default(),
        InventoryLimits::default(),
        |_| {},
    )
    .unwrap();
    let report = &inventory.report;
    assert_eq!(report.coverage, Coverage::Partial);
    assert!(report.resource_limited_directories > 0);
    assert!(report.files < 201);
    assert_eq!(report.logical_bytes, report.files * 3);
    assert_eq!(report.traversal_stats.peak_pending_directories, 1);
    assert!(report.traversal_stats.peak_pending_path_bytes <= 1024);
    assert!(inventory.entries[0].incomplete);
    assert_eq!(
        inventory.entries[0].logical_bytes,
        Some(report.logical_bytes)
    );
    for entry in &inventory.entries {
        if entry.kind == InventoryKind::Directory && entry.incomplete {
            assert!(entry.logical_bytes.unwrap() <= 603);
        }
    }
}

#[test]
fn cancellation_and_callback_unwind_release_workers_waiting_on_full_queues() {
    let root = fixture();
    for workers in [2, 8] {
        let mut options = options(workers);
        options.concurrency.max_buffered_observations = 1;
        let token = CancellationToken::default();
        let report = scan(root.path(), &options, &token, |_| token.cancel()).unwrap();
        assert_eq!(report.coverage, Coverage::Cancelled);
        assert_eq!((report.directories, report.files), (1, 0));
        let result = std::panic::catch_unwind(|| {
            scan(root.path(), &options, &CancellationToken::default(), |_| {
                panic!("consumer interrupted")
            })
        });
        assert!(result.is_err());
        // The same fixture remains usable after cancellation or unwinding.
        assert_eq!(
            scan(root.path(), &options, &CancellationToken::default(), |_| {})
                .unwrap()
                .coverage,
            Coverage::Complete
        );
    }
}

#[test]
fn invalid_configuration_and_oversized_transfer_paths_fail_without_hanging() {
    let root = fixture();
    for workers in [0, 33] {
        assert!(matches!(
            scan(
                root.path(),
                &options(workers),
                &CancellationToken::default(),
                |_| {}
            ),
            Err(ScanFailure::InvalidOptions(_))
        ));
    }
    let mut options = options(4);
    options.concurrency.max_buffered_path_bytes = fs::canonicalize(root.path())
        .unwrap()
        .as_os_str()
        .as_encoded_bytes()
        .len();
    // Root fits, first child does not: failure wakes both consumer and workers.
    assert!(matches!(
        scan(root.path(), &options, &CancellationToken::default(), |_| {}),
        Err(ScanFailure::ObservationPathLimit { .. })
    ));
    options.concurrency.max_buffered_observations = 0;
    assert!(matches!(
        scan(root.path(), &options, &CancellationToken::default(), |_| {}),
        Err(ScanFailure::InvalidOptions(_))
    ));
}

#[test]
fn concurrent_exclusions_depth_and_root_saturation_are_explicit() {
    let root = fixture();
    let mut options = options(4);
    options.exclusions = vec!["**/été-00/**".into()];
    options.max_depth = Some(1);
    let report = scan(root.path(), &options, &CancellationToken::default(), |_| {}).unwrap();
    assert_eq!(report.coverage, Coverage::Partial);
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (1, 40, 3)
    );
    assert_eq!(
        (report.excluded_entries, report.depth_limited_directories),
        (1, 39)
    );
    options.max_depth = None;
    options.traversal_limits.max_pending_path_bytes = 1;
    let report = scan(root.path(), &options, &CancellationToken::default(), |_| {}).unwrap();
    assert_eq!(
        (
            report.directories,
            report.files,
            report.resource_limited_directories
        ),
        (1, 0, 1)
    );
    assert_eq!(report.traversal_stats.peak_open_directories, 0);
}

#[cfg(unix)]
#[test]
fn native_non_unicode_paths_and_symlink_loops_survive_concurrent_transfer() {
    use std::ffi::OsString;
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let root = fixture();
    let file = root.path().join(OsString::from_vec(vec![b'f', 0xff]));
    fs::write(&file, b"abcde").unwrap();
    symlink(root.path(), root.path().join("cycle")).unwrap();
    let inventory = scan_inventory_with_limits(
        root.path(),
        &options(4),
        &CancellationToken::default(),
        InventoryLimits::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(inventory.report.coverage, Coverage::Complete);
    assert_eq!(
        (
            inventory.report.logical_bytes,
            inventory.report.symlinks_skipped
        ),
        (608, 1)
    );
    assert!(inventory.entries.iter().any(|entry| entry.path == file));
}
