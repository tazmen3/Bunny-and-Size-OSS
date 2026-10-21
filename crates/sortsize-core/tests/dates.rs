//! #17: native local dates, opt-in, single acquisition and bounded retention.
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, Coverage, InventoryKind, InventoryLimits, MetadataValue,
    ScanOptions, ValueAccuracy, scan_inventory_with_dates, scan_inventory_with_limits,
};
use std::collections::BTreeMap;
use std::fs::{self, FileTimes};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
fn modified() -> SystemTime {
    UNIX_EPOCH - Duration::from_secs(315_619_200) + Duration::from_nanos(123_456_700)
}
fn accessed() -> SystemTime {
    UNIX_EPOCH + Duration::new(946_684_800, 765_432_100)
}
fn set_dates(path: &Path) {
    #[cfg(windows)]
    let handle = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES + FILE_FLAG_BACKUP_SEMANTICS for directories.
        fs::OpenOptions::new()
            .access_mode(0x100)
            .custom_flags(0x02000000)
            .open(path)
            .unwrap()
    };
    #[cfg(not(windows))]
    let handle = fs::File::open(path).unwrap();
    handle
        .set_times(
            FileTimes::new()
                .set_modified(modified())
                .set_accessed(accessed()),
        )
        .unwrap();
}
fn known(value: MetadataValue<SystemTime>) -> SystemTime {
    match value {
        MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        } => value,
        other => panic!("date is not an exact observed instant: {other:?}"),
    }
}
fn check_table(result: &sortsize_core::InventoryWithDates) {
    assert_eq!(result.dates.len(), result.inventory.entries.len());
    for (entry, dates) in result.inventory.entries.iter().zip(&result.dates) {
        if matches!(entry.kind, InventoryKind::File | InventoryKind::Directory) {
            assert!(known(dates.modified) < UNIX_EPOCH);
            assert!(known(dates.accessed) >= accessed());
        } else {
            assert_eq!(dates.modified, MetadataValue::NotCollected);
            assert_eq!(dates.accessed, MetadataValue::NotCollected);
        }
    }
}
fn populate(root: &Path) -> Vec<PathBuf> {
    fs::create_dir_all(root.join("été 日本語/empty")).unwrap();
    fs::write(root.join("empty"), []).unwrap();
    fs::write(root.join("été 日本語/data"), b"1234567").unwrap();
    fs::hard_link(root.join("été 日本語/data"), root.join("hardlink")).unwrap();
    let mut paths = vec![
        root.to_owned(),
        root.join("été 日本語"),
        root.join("été 日本語/empty"),
        root.join("empty"),
        root.join("été 日本語/data"),
        root.join("hardlink"),
    ];
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let native = root.join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
        fs::write(&native, b"native").unwrap();
        paths.push(native);
        symlink(root, root.join("cycle")).unwrap();
        symlink(root.join("missing"), root.join("broken-link")).unwrap();
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let native = root.join(std::ffi::OsString::from_wide(&[0x006e, 0xd800]));
        match fs::write(&native, b"native") {
            Ok(()) => paths.push(native),
            // ERROR_INVALID_NAME: some Win32/filesystem combinations reject a
            // lone surrogate. This is a fixture limitation, not a scan result.
            Err(error) if error.raw_os_error() == Some(123) => {
                eprintln!(
                    "Windows refused the D800 fixture (ERROR_INVALID_NAME); native units are also tested in memory in the GUI"
                );
            }
            Err(error) => panic!("cannot create native Windows fixture: {error}"),
        }
    }
    paths
}

#[test]
fn file_and_directory_dates_preserve_precision_native_identity_and_legacy_results() {
    let fixture = TempDir::new().unwrap();
    let paths = populate(fixture.path());
    for workers in [1, 4, 8] {
        for path in &paths {
            set_dates(path);
        }
        // Test oracle reads metadata before the scan; no post-scan lookup of dates.
        let expected: BTreeMap<_, _> = paths
            .iter()
            .map(|p| {
                let metadata = fs::symlink_metadata(p).unwrap();
                (
                    fs::canonicalize(p).unwrap(),
                    (metadata.modified().unwrap(), metadata.accessed().unwrap()),
                )
            })
            .collect();
        let result = scan_inventory_with_dates(
            fixture.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        check_table(&result);
        assert_eq!(result.inventory.report.coverage, Coverage::Complete);
        for (entry, dates) in result.inventory.entries.iter().zip(&result.dates) {
            if let Some((mtime, atime)) = expected.get(&entry.path) {
                assert_eq!(known(dates.modified), *mtime);
                #[cfg(not(windows))]
                assert_eq!(known(dates.accessed), *atime);
                #[cfg(windows)]
                {
                    // NTFS may update last-access time when canonicalization or
                    // directory enumeration opens the object. The inventory is
                    // an observation, not a snapshot of the pre-scan oracle.
                    // Bound it by independent native observations on both sides;
                    // keep mtime equality and the exact-value unit tests above.
                    let after = fs::symlink_metadata(&entry.path)
                        .unwrap()
                        .accessed()
                        .unwrap();
                    let observed = known(dates.accessed);
                    assert!(
                        *atime <= observed && observed <= after,
                        "path={:?} workers={workers}: {atime:?} <= {observed:?} <= {after:?}",
                        entry.path
                    );
                }
            }
            if let Some(parent) = entry.parent {
                assert_eq!(
                    entry.path.parent(),
                    Some(result.inventory.entries[parent].path.as_path())
                );
            }
        }
        let legacy = scan_inventory_with_limits(
            fixture.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        let shape = |entries: &[sortsize_core::InventoryEntry]| {
            entries
                .iter()
                .map(|e| {
                    (
                        e.path.clone(),
                        (
                            e.kind,
                            e.logical_bytes,
                            e.counts,
                            e.incomplete,
                            e.parent.map(|p| entries[p].path.clone()),
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(shape(&result.inventory.entries), shape(&legacy.entries));
        assert_eq!(
            result.inventory.report.logical_bytes,
            legacy.report.logical_bytes
        );
        assert_eq!(
            result
                .inventory
                .top_files
                .entries
                .iter()
                .map(|e| (&e.path, e.logical_bytes))
                .collect::<Vec<_>>(),
            legacy
                .top_files
                .entries
                .iter()
                .map(|e| (&e.path, e.logical_bytes))
                .collect::<Vec<_>>()
        );
        assert!(result.dates.capacity() <= InventoryLimits::default().max_entries);
    }
}

#[test]
fn dates_survive_files_removed_during_the_scan_without_reacquisition() {
    for workers in [1, 4, 8] {
        let fixture = TempDir::new().unwrap();
        for i in 0..120 {
            let path = fixture.path().join(format!("file-{i:03}"));
            fs::write(&path, b"data").unwrap();
            set_dates(&path);
        }
        let result = scan_inventory_with_dates(
            fixture.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |p| {
                if p.current_path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("file-"))
                {
                    // Observation and its date were consumed before this progress callback.
                    fs::remove_file(&p.current_path).unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(result.inventory.report.coverage, Coverage::Complete);
        assert_eq!(result.inventory.report.files, 120);
        assert_eq!(result.inventory.report.logical_bytes, 480);
        for (entry, dates) in result.inventory.entries.iter().zip(&result.dates).skip(1) {
            assert!(!entry.path.exists());
            assert_eq!(known(dates.modified), modified());
            assert_eq!(known(dates.accessed), accessed());
        }
        drop(fixture);
        assert_eq!(result.dates.len(), 121);
        assert_eq!(known(result.dates[1].modified), modified());
    }
}

#[test]
fn cancellation_and_partial_subtree_totals_never_make_dates_lower_bounds() {
    let fixture = TempDir::new().unwrap();
    let paths = populate(fixture.path());
    for workers in [1, 4, 8] {
        for path in &paths {
            set_dates(path);
        }
        let token = CancellationToken::default();
        token.cancel();
        let result = scan_inventory_with_dates(
            fixture.path(),
            &options(workers),
            &token,
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.inventory.report.coverage, Coverage::Cancelled);
        assert!(result.inventory.entries.is_empty() && result.dates.is_empty());
        let token = CancellationToken::default();
        let result = scan_inventory_with_dates(
            fixture.path(),
            &options(workers),
            &token,
            InventoryLimits::default(),
            |_| token.cancel(),
        )
        .unwrap();
        assert_eq!(result.inventory.report.coverage, Coverage::Cancelled);
        assert_eq!(result.dates.len(), 1);
        assert!(result.inventory.entries[0].incomplete);
        assert_eq!(known(result.dates[0].modified), modified());
        let opts = ScanOptions {
            max_depth: Some(1),
            ..options(workers)
        };
        let result = scan_inventory_with_dates(
            fixture.path(),
            &opts,
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.inventory.report.coverage, Coverage::Partial);
        assert!(result.inventory.entries[0].incomplete);
        check_table(&result);
    }
}

#[test]
fn retention_and_path_byte_limits_bound_only_the_date_table_not_totals_or_top100() {
    let fixture = TempDir::new().unwrap();
    for i in 0..50_000 {
        fs::write(fixture.path().join(format!("f{i:05}")), []).unwrap();
    }
    fs::create_dir(fixture.path().join("late")).unwrap();
    for i in 0..10 {
        fs::write(fixture.path().join(format!("late/winner{i}")), [42; 123]).unwrap();
    }
    let result = scan_inventory_with_dates(
        fixture.path(),
        &options(1),
        &CancellationToken::default(),
        InventoryLimits::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.inventory.report.coverage, Coverage::Complete);
    assert_eq!(result.inventory.report.files, 50_010);
    assert_eq!(result.inventory.report.logical_bytes, 1230);
    assert_eq!(result.inventory.entries.len(), 50_000);
    assert_eq!(result.inventory.entries_omitted, 12);
    assert_eq!(result.dates.len(), 50_000);
    assert_eq!(result.dates.capacity(), 50_000);
    assert!(
        result
            .inventory
            .top_files
            .entries
            .iter()
            .take(10)
            .all(|f| f.logical_bytes == 123 && f.retained_entry.is_none())
    );
    let root = fs::canonicalize(fixture.path()).unwrap();
    for workers in [1, 4, 8] {
        for budget in [1, root.as_os_str().as_encoded_bytes().len()] {
            let limits = InventoryLimits {
                max_path_bytes: budget,
                ..Default::default()
            };
            let result = scan_inventory_with_dates(
                fixture.path(),
                &options(workers),
                &CancellationToken::default(),
                limits,
                |_| {},
            )
            .unwrap();
            assert_eq!(result.inventory.report.coverage, Coverage::Complete);
            assert_eq!(result.inventory.report.files, 50_010);
            assert!(result.inventory.path_bytes_limit_reached);
            assert_eq!(result.dates.len(), usize::from(budget > 1));
            if budget > 1 {
                known(result.dates[0].modified);
                assert!(!result.inventory.entries[0].incomplete);
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn a_real_listing_denial_does_not_falsely_mark_observed_dates_access_denied() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = TempDir::new().unwrap();
    let blocked = fixture.path().join("denied");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("hidden"), b"secret fixture").unwrap();
    set_dates(&blocked);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = fs::read_dir(&blocked).is_err();
    if !denied {
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!(
            "POSIX denial is not reproducible with this privileged account; scenario skipped"
        );
        return;
    }
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(blocked.clone());
    eprintln!("POSIX PermissionDenied reproduced; metadata dates remain observable");
    for workers in [1, 4, 8] {
        let result = scan_inventory_with_dates(
            fixture.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        let index = result
            .inventory
            .entries
            .iter()
            .position(|e| e.path == blocked)
            .unwrap();
        assert_eq!(known(result.dates[index].modified), modified());
        if denied {
            assert_eq!(result.inventory.report.coverage, Coverage::Partial);
            assert!(result.inventory.entries[index].incomplete);
            assert_eq!(result.inventory.report.files, 0);
            assert!(
                result
                    .inventory
                    .report
                    .errors
                    .iter()
                    .any(|e| e.kind == "PermissionDenied")
            );
        }
    }
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
}
