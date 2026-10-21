//! #14 tranche A: real native fixtures; no physical or reclaimable aggregate.
#![cfg(any(target_os = "linux", windows))]
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, Coverage, InventoryKind, InventoryLimits,
    InventoryWithMetadata, LocalFileId, MetadataSelection, MetadataValue, ScanOptions,
    ValueAccuracy, scan_inventory_with_metadata,
};
use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;

fn known<T: std::fmt::Debug>(value: MetadataValue<T>) -> T {
    match value {
        MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        } => value,
        other => panic!("expected an exact native observation: {other:?}"),
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
fn collect(root: &Path, workers: usize, limits: InventoryLimits) -> InventoryWithMetadata {
    scan_inventory_with_metadata(
        root,
        &options(workers),
        &CancellationToken::default(),
        limits,
        MetadataSelection {
            dates: true,
            allocation: true,
        },
        |_| {},
    )
    .unwrap()
}
fn at(result: &InventoryWithMetadata, name: &str) -> usize {
    result
        .inventory
        .entries
        .iter()
        .position(|e| e.path.ends_with(name))
        .unwrap()
}
fn check_alignment(result: &InventoryWithMetadata, limits: InventoryLimits) {
    assert_eq!(result.allocations.len(), result.inventory.entries.len());
    assert_eq!(result.dates.len(), result.inventory.entries.len());
    assert!(result.allocations.capacity() <= limits.max_entries);
    assert!(result.dates.capacity() <= limits.max_entries);
    assert_eq!(result.inventory.report.allocated_bytes, None);
    assert_eq!(result.inventory.report.potentially_reclaimable_bytes, None);
    for (entry, allocation) in result.inventory.entries.iter().zip(&result.allocations) {
        if entry.kind != InventoryKind::File {
            assert_eq!(allocation.allocated_bytes, MetadataValue::NotCollected);
            assert_eq!(allocation.identity, MetadataValue::NotCollected);
            assert_eq!(allocation.link_count, MetadataValue::NotCollected);
        }
    }
}
fn sparse_file(path: &Path) -> File {
    let file = File::create(path).unwrap();
    #[cfg(windows)]
    {
        // Native NTFS fixture setup only; the engine never invokes this command.
        let output = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success(), "fsutil sparse: {output:?}");
    }
    file.set_len(64 * 1024 * 1024).unwrap();
    file
}
fn sparse(path: &Path) {
    let mut file = sparse_file(path);
    file.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
    file.write_all(&[42; 4096]).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn native_normal_empty_sparse_hardlinks_and_equal_content_keep_distinct_meanings() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path();
    fs::write(root.join("normal"), [7; 8193]).unwrap();
    fs::write(root.join("same-content"), [7; 8193]).unwrap();
    fs::write(root.join("empty"), []).unwrap();
    sparse(&root.join("sparse"));
    drop(sparse_file(&root.join("holes-only")));
    fs::create_dir(root.join("child")).unwrap();
    fs::hard_link(root.join("normal"), root.join("child/link")).unwrap();
    let outside = TempDir::new_in(root.parent().unwrap()).unwrap();
    fs::hard_link(root.join("normal"), outside.path().join("outside-link")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        fs::write(
            root.join(std::ffi::OsString::from_vec(b"native-\xff".to_vec())),
            b"x",
        )
        .unwrap();
        symlink(root, root.join("cycle")).unwrap();
        symlink(root.join("missing"), root.join("broken")).unwrap();
    }
    let mut oracle = None;
    for workers in [1, 2, 4, 8] {
        let result = collect(root, workers, InventoryLimits::default());
        check_alignment(&result, InventoryLimits::default());
        assert_eq!(result.inventory.report.coverage, Coverage::Complete);
        let normal = result.allocations[at(&result, "normal")];
        let link = result.allocations[at(&result, "child/link")];
        let same = result.allocations[at(&result, "same-content")];
        assert_eq!(normal, link);
        assert_eq!(known(normal.link_count), 3);
        assert_ne!(known(normal.identity), known(same.identity));
        let sparse_index = at(&result, "sparse");
        assert!(known(result.allocations[sparse_index].allocated_bytes) < 64 * 1024 * 1024);
        // Empty NTFS files can be resident; no assumed block rounding.
        known(result.allocations[at(&result, "empty")].allocated_bytes);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                known(result.allocations[at(&result, "empty")].allocated_bytes),
                0
            );
            assert_eq!(
                known(result.allocations[at(&result, "holes-only")].allocated_bytes),
                0
            );
        }
        #[cfg(target_os = "linux")]
        for (entry, allocation) in result.inventory.entries.iter().zip(&result.allocations) {
            if entry.kind == InventoryKind::File {
                use std::os::unix::fs::MetadataExt;
                let native = fs::symlink_metadata(&entry.path).unwrap();
                assert_eq!(known(allocation.allocated_bytes), native.blocks() * 512);
                assert_eq!(
                    known(allocation.identity),
                    LocalFileId::Unix {
                        device: native.dev(),
                        inode: native.ino()
                    }
                );
                assert_eq!(known(allocation.link_count), native.nlink());
            }
        }
        #[cfg(windows)]
        assert!(matches!(
            known(normal.identity),
            LocalFileId::Windows { .. }
        ));
        let shape: BTreeMap<_, _> = result
            .inventory
            .entries
            .iter()
            .zip(&result.allocations)
            .map(|(e, a)| (e.path.clone(), (e.kind, e.logical_bytes, e.counts, *a)))
            .collect();
        if let Some(oracle) = &oracle {
            assert_eq!(&shape, oracle);
        } else {
            oracle = Some(shape);
        }
        assert!(
            result
                .inventory
                .report
                .traversal_stats
                .peak_buffered_observations
                <= 3
        );
    }
}

#[test]
fn tables_remain_aligned_after_both_retention_limits_and_all_selection_combinations() {
    let fixture = TempDir::new().unwrap();
    for d in 0..12 {
        let child = fixture.path().join(format!("dir-{d}"));
        fs::create_dir(&child).unwrap();
        for n in 0..12 {
            fs::write(child.join(format!("f-{n}")), [1; 31]).unwrap();
        }
    }
    let root = fs::canonicalize(fixture.path()).unwrap();
    for workers in [1, 4, 8] {
        for limits in [
            InventoryLimits {
                max_entries: 1,
                ..Default::default()
            },
            InventoryLimits {
                max_entries: 7,
                ..Default::default()
            },
            InventoryLimits {
                max_path_bytes: 1,
                ..Default::default()
            },
            InventoryLimits {
                max_path_bytes: root.as_os_str().as_encoded_bytes().len(),
                ..Default::default()
            },
        ] {
            let result = collect(fixture.path(), workers, limits);
            check_alignment(&result, limits);
            assert_eq!(result.inventory.report.coverage, Coverage::Complete);
            assert_eq!(result.inventory.report.files, 144);
            assert_eq!(result.inventory.report.logical_bytes, 144 * 31);
            assert_eq!(result.inventory.top_files.entries.len(), 100);
            assert_eq!(
                result.inventory.entries.len() as u64 + result.inventory.entries_omitted,
                157
            );
            if let Some(root) = result.inventory.entries.first() {
                assert_eq!(root.logical_bytes, Some(144 * 31));
                assert!(!root.incomplete);
            }
        }
        for dates in [false, true] {
            for allocation in [false, true] {
                let result = scan_inventory_with_metadata(
                    fixture.path(),
                    &options(workers),
                    &CancellationToken::default(),
                    InventoryLimits::default(),
                    MetadataSelection { dates, allocation },
                    |_| {},
                )
                .unwrap();
                assert_eq!(result.dates.len(), if dates { 157 } else { 0 });
                assert_eq!(result.allocations.len(), if allocation { 157 } else { 0 });
            }
        }
    }
}

#[test]
fn combined_properties_survive_removal_in_progress_callback_without_a_second_scan() {
    for workers in [1, 4, 8] {
        let fixture = TempDir::new().unwrap();
        let timestamp = UNIX_EPOCH + Duration::from_secs(123456789);
        for n in 0..128 {
            let path = fixture.path().join(format!("f-{n}"));
            fs::write(&path, [8; 4096]).unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(timestamp))
                .unwrap();
        }
        let result = scan_inventory_with_metadata(
            fixture.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            MetadataSelection {
                dates: true,
                allocation: true,
            },
            |p| {
                if p.files > 0 {
                    fs::remove_file(&p.current_path).unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(result.inventory.report.coverage, Coverage::Complete);
        assert_eq!(result.inventory.report.files, 128);
        check_alignment(&result, InventoryLimits::default());
        for index in 1..result.inventory.entries.len() {
            assert!(!result.inventory.entries[index].path.exists());
            assert_eq!(known(result.dates[index].modified), timestamp);
            known(result.allocations[index].allocated_bytes);
            known(result.allocations[index].identity);
        }
    }
}

#[test]
fn cancellation_and_depth_limits_preserve_observed_values_and_coverage() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("child")).unwrap();
    for n in 0..100 {
        fs::write(fixture.path().join(format!("child/f-{n}")), [1; 4096]).unwrap();
    }
    for workers in [1, 4, 8] {
        for cancel_after in [0, 1, 7] {
            let token = CancellationToken::default();
            if cancel_after == 0 {
                token.cancel();
            }
            let result = scan_inventory_with_metadata(
                fixture.path(),
                &options(workers),
                &token,
                InventoryLimits::default(),
                MetadataSelection {
                    dates: true,
                    allocation: true,
                },
                |p| {
                    if p.files >= cancel_after {
                        token.cancel();
                    }
                },
            )
            .unwrap();
            assert_eq!(result.inventory.report.coverage, Coverage::Cancelled);
            assert_eq!(result.inventory.report.files, cancel_after);
            check_alignment(&result, InventoryLimits::default());
            for (entry, allocation) in result.inventory.entries.iter().zip(&result.allocations) {
                if entry.kind == InventoryKind::File {
                    known(allocation.allocated_bytes);
                }
                if entry.kind == InventoryKind::Directory {
                    assert!(entry.incomplete);
                }
            }
        }
        let result = scan_inventory_with_metadata(
            fixture.path(),
            &ScanOptions {
                max_depth: Some(1),
                ..options(workers)
            },
            &CancellationToken::default(),
            InventoryLimits::default(),
            MetadataSelection {
                dates: true,
                allocation: true,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(result.inventory.report.coverage, Coverage::Partial);
        assert_eq!(result.inventory.report.files, 0);
        check_alignment(&result, InventoryLimits::default());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn denied_directory_and_read_denied_file_keep_metadata_semantics() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = TempDir::new().unwrap();
    let blocked = fixture.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("hidden"), [1; 4096]).unwrap();
    let visible = fixture.path().join("read-denied");
    fs::write(&visible, [1; 4096]).unwrap();
    struct Restore<'a>(&'a Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(&blocked);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(&visible, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read_dir(&blocked).is_err(),
        "run this denial test as an unprivileged user"
    );
    for workers in [1, 4, 8] {
        let result = collect(fixture.path(), workers, InventoryLimits::default());
        assert_eq!(result.inventory.report.coverage, Coverage::Partial);
        assert_eq!(result.inventory.report.files, 1);
        assert!(
            result
                .inventory
                .report
                .errors
                .iter()
                .any(|e| e.kind == "PermissionDenied")
        );
        known(result.allocations[at(&result, "read-denied")].allocated_bytes);
        check_alignment(&result, InventoryLimits::default());
    }
}
