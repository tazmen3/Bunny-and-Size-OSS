//! SCAN-009/010/014/015: finite traversal budgets must never claim full coverage.
use sortsize_core::{
    CancellationToken, Coverage, ScanOptions, TraversalLimits, scan, scan_inventory,
};
use std::fs;
use tempfile::TempDir;

fn fixture() -> TempDir {
    let root = TempDir::new().unwrap();
    for n in 0..12 {
        let child = root.path().join(format!("été-{n:02}"));
        fs::create_dir(&child).unwrap();
        fs::write(child.join("data"), b"xyz").unwrap();
    }
    root
}

#[test]
fn saturated_directory_queue_marks_only_unvisited_subtrees_incomplete() {
    let root = fixture();
    let options = ScanOptions {
        traversal_limits: TraversalLimits {
            max_pending_directories: 3,
            ..Default::default()
        },
        ..Default::default()
    };
    let inventory = scan_inventory(
        root.path(),
        &options,
        &CancellationToken::default(),
        100,
        |_| {},
    )
    .unwrap();
    let report = &inventory.report;
    assert_eq!(report.coverage, Coverage::Partial);
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (3, 13, 9)
    );
    assert_eq!(report.resource_limited_directories, 9);
    assert_eq!(report.errors_count, 0);
    assert_eq!(report.traversal_stats.peak_pending_directories, 3);
    assert_eq!(report.traversal_stats.peak_open_directories, 1);
    assert!(inventory.entries[0].incomplete);
    assert_eq!(inventory.entries[0].logical_bytes, Some(9));
    assert_eq!(
        inventory.entries.iter().filter(|e| e.incomplete).count(),
        10
    );
    assert_eq!(inventory.entries_omitted, 0); // Traversal limits are not display truncation.
    let complete = scan(
        root.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(complete.coverage, Coverage::Complete);
    assert_eq!(
        (complete.files, complete.directories, complete.logical_bytes),
        (12, 13, 36)
    );
}

#[test]
fn native_path_byte_budget_limits_traversal_independently_of_node_count() {
    let root = fixture();
    let child = fs::canonicalize(root.path().join("été-00")).unwrap();
    let bytes = child.as_os_str().as_encoded_bytes().len();
    let options = ScanOptions {
        traversal_limits: TraversalLimits {
            max_pending_directories: 1000,
            max_pending_path_bytes: bytes,
        },
        ..Default::default()
    };
    let report = scan(root.path(), &options, &CancellationToken::default(), |_| {}).unwrap();
    assert_eq!(report.coverage, Coverage::Partial);
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (1, 13, 3)
    );
    assert_eq!(report.resource_limited_directories, 11);
    assert_eq!(report.traversal_stats.peak_pending_path_bytes, bytes);
    assert_eq!(report.traversal_stats.peak_pending_directories, 1);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["traversal_limits"]["max_pending_path_bytes"], bytes);
    assert_eq!(json["resource_limited_directories"], 11);
}

#[test]
fn root_budget_and_depth_limits_are_distinct_and_zero_budgets_are_rejected() {
    let root = fixture();
    let token = CancellationToken::default();
    let mut options = ScanOptions {
        traversal_limits: TraversalLimits {
            max_pending_path_bytes: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let report = scan(root.path(), &options, &token, |_| {}).unwrap();
    assert_eq!(report.coverage, Coverage::Partial);
    assert_eq!(
        (
            report.directories,
            report.resource_limited_directories,
            report.files
        ),
        (1, 1, 0)
    );
    assert_eq!(report.traversal_stats.peak_open_directories, 0);
    options.max_depth = Some(0);
    let report = scan(root.path(), &options, &token, |_| {}).unwrap();
    assert_eq!(
        (
            report.depth_limited_directories,
            report.resource_limited_directories
        ),
        (1, 0)
    );
    for limits in [
        TraversalLimits {
            max_pending_directories: 0,
            ..Default::default()
        },
        TraversalLimits {
            max_pending_path_bytes: 0,
            ..Default::default()
        },
    ] {
        options.traversal_limits = limits;
        assert!(matches!(
            scan(root.path(), &options, &token, |_| {}),
            Err(sortsize_core::ScanFailure::InvalidOptions(_))
        ));
    }
}

#[test]
fn cancellation_takes_precedence_over_resource_limit_and_keeps_lower_bounds() {
    let root = fixture();
    let token = CancellationToken::default();
    let options = ScanOptions {
        traversal_limits: TraversalLimits {
            max_pending_directories: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut observations = 0;
    let report = scan(root.path(), &options, &token, |_| {
        observations += 1;
        if observations == 13 {
            token.cancel();
        }
    })
    .unwrap();
    assert_eq!(report.coverage, Coverage::Cancelled);
    assert_eq!(report.resource_limited_directories, 11);
    assert_eq!(report.files, 0);
    assert_eq!(report.traversal_stats.peak_open_directories, 1);
}

#[cfg(unix)]
#[test]
fn a_deferred_directory_replaced_by_a_symlink_is_not_followed() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    fs::write(external.path().join("outside"), b"never count").unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    let report = scan(
        root.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        |progress| {
            if progress.current_path == child && progress.errors_count == 0 {
                fs::remove_dir(&child).unwrap();
                symlink(external.path(), &child).unwrap();
            }
        },
    )
    .unwrap();
    assert_eq!(report.coverage, Coverage::Partial);
    assert_eq!((report.files, report.directories), (0, 2));
    assert_eq!(report.errors_count, 1);
    assert_eq!(report.errors[0].kind, "ChangedDuringScan");
}
