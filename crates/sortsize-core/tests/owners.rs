//! Owner API is portable; native UID assertions run only on Linux.
use sortsize_core::*;
use std::{collections::BTreeMap, fs, path::Path};
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
fn collect(
    root: &Path,
    opts: &ScanOptions,
    limits: InventoryLimits,
    token: &CancellationToken,
) -> InventoryWithOwners {
    scan_inventory_with_owners(
        root,
        opts,
        token,
        limits,
        OwnerSelection {
            owner: true,
            metadata: MetadataSelection {
                dates: true,
                allocation: true,
            },
        },
        |_| {},
    )
    .unwrap()
}
fn check(result: &InventoryWithOwners) {
    let entries = &result.metadata.inventory.entries;
    assert_eq!(entries.len(), result.owners.len());
    assert_eq!(entries.len(), result.metadata.dates.len());
    assert_eq!(entries.len(), result.metadata.allocations.len());
    for (entry, owner) in entries.iter().zip(&result.owners) {
        if matches!(entry.kind, InventoryKind::File | InventoryKind::Directory) {
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(
                    owner.id,
                    MetadataValue::Known {
                        value: NativeOwnerId::UnixUid(
                            fs::symlink_metadata(&entry.path).unwrap().uid()
                        ),
                        accuracy: ValueAccuracy::Exact
                    }
                );
            }
            #[cfg(windows)]
            assert!(matches!(
                owner.id,
                MetadataValue::Known {
                    value: NativeOwnerId::WindowsSid(_),
                    accuracy: ValueAccuracy::Exact
                }
            ));
            #[cfg(not(any(target_os = "linux", windows)))]
            assert_eq!(owner.id, MetadataValue::Unsupported);
        } else {
            assert_eq!(*owner, EntryOwner::not_collected());
        }
    }
}

#[test]
fn owners_align_and_preserve_all_existing_metadata_and_results() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("été/nested")).unwrap();
    fs::write(temp.path().join("été/file"), b"1234567").unwrap();
    fs::hard_link(temp.path().join("été/file"), temp.path().join("hardlink")).unwrap();
    fs::write(temp.path().join("excluded"), b"ignored").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStringExt, fs::symlink, net::UnixListener};
        fs::write(
            temp.path()
                .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec())),
            [],
        )
        .unwrap();
        symlink(temp.path(), temp.path().join("cycle")).unwrap();
        let _socket = UnixListener::bind(temp.path().join("socket")).unwrap();
    }
    for workers in [1, 4, 8] {
        let mut opts = options(workers);
        opts.exclusions = vec!["excluded".into()];
        let result = collect(
            temp.path(),
            &opts,
            InventoryLimits::default(),
            &CancellationToken::default(),
        );
        check(&result);
        let legacy = scan_inventory_with_metadata(
            temp.path(),
            &opts,
            &CancellationToken::default(),
            InventoryLimits::default(),
            MetadataSelection {
                dates: true,
                allocation: true,
            },
            |_| {},
        )
        .unwrap();
        let shape = |r: &InventoryWithMetadata| {
            r.inventory
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    (
                        e.path.clone(),
                        (
                            e.kind,
                            e.logical_bytes,
                            e.counts,
                            e.incomplete,
                            r.dates[i].modified,
                            r.allocations[i],
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(shape(&result.metadata), shape(&legacy));
        assert_eq!(
            result.metadata.inventory.report.logical_bytes,
            legacy.inventory.report.logical_bytes
        );
        assert_eq!(
            result.metadata.inventory.report.coverage,
            legacy.inventory.report.coverage
        );
        assert_eq!(result.metadata.inventory.report.excluded_entries, 1);
        assert!(
            !result
                .metadata
                .inventory
                .entries
                .iter()
                .any(|e| e.path.ends_with("excluded"))
        );
        assert_eq!(
            result
                .metadata
                .inventory
                .top_files
                .entries
                .iter()
                .map(|e| (&e.path, e.logical_bytes))
                .collect::<Vec<_>>(),
            legacy
                .inventory
                .top_files
                .entries
                .iter()
                .map(|e| (&e.path, e.logical_bytes))
                .collect::<Vec<_>>()
        );
        let disabled = scan_inventory_with_owners(
            temp.path(),
            &opts,
            &CancellationToken::default(),
            InventoryLimits::default(),
            OwnerSelection::default(),
            |_| {},
        )
        .unwrap();
        assert!(disabled.owners.is_empty());
        assert!(disabled.metadata.dates.is_empty() && disabled.metadata.allocations.is_empty());
    }
}

#[test]
fn empty_truncated_path_limited_partial_and_cancelled_scans() {
    let temp = TempDir::new().unwrap();
    let token = CancellationToken::default();
    for workers in [1, 4, 8] {
        let result = collect(
            temp.path(),
            &options(workers),
            InventoryLimits::default(),
            &token,
        );
        check(&result);
        assert_eq!(result.owners.len(), 1); // root is an observed directory
    }
    fs::create_dir(temp.path().join("nested")).unwrap();
    for i in 0..120 {
        fs::write(temp.path().join(format!("nested/f{i}")), []).unwrap();
    }
    for workers in [1, 4, 8] {
        for limits in [
            InventoryLimits {
                max_entries: 3,
                ..Default::default()
            },
            InventoryLimits {
                max_path_bytes: 1,
                ..Default::default()
            },
            InventoryLimits {
                max_path_bytes: temp.path().as_os_str().as_encoded_bytes().len(),
                ..Default::default()
            },
        ] {
            let r = collect(temp.path(), &options(workers), limits, &token);
            check(&r);
            assert!(r.owners.capacity() <= limits.max_entries);
            assert!(r.metadata.inventory.entries_omitted > 0);
            assert_eq!(r.metadata.inventory.report.files, 120);
            assert_eq!(r.metadata.inventory.report.coverage, Coverage::Complete);
        }
        let opts = ScanOptions {
            max_depth: Some(1),
            ..options(workers)
        };
        let r = collect(temp.path(), &opts, InventoryLimits::default(), &token);
        check(&r);
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Partial);
        let token = CancellationToken::default();
        token.cancel();
        let r = collect(
            temp.path(),
            &options(workers),
            InventoryLimits::default(),
            &token,
        );
        assert!(r.owners.is_empty());
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Cancelled);
        let token = CancellationToken::default();
        let r = scan_inventory_with_owners(
            temp.path(),
            &options(workers),
            &token,
            InventoryLimits::default(),
            OwnerSelection {
                owner: true,
                ..Default::default()
            },
            |_| token.cancel(),
        )
        .unwrap();
        assert_eq!(r.owners.len(), 1);
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Cancelled);
    }
}

#[test]
fn deleting_observed_files_does_not_trigger_owner_reacquisition() {
    for workers in [1, 4, 8] {
        let temp = TempDir::new().unwrap();
        for i in 0..120 {
            fs::write(temp.path().join(format!("f{i}")), []).unwrap();
        }
        let r = scan_inventory_with_owners(
            temp.path(),
            &options(workers),
            &CancellationToken::default(),
            InventoryLimits::default(),
            OwnerSelection {
                owner: true,
                ..Default::default()
            },
            |p| {
                if p.current_path
                    .file_name()
                    .is_some_and(|n| n.as_encoded_bytes().starts_with(b"f"))
                {
                    fs::remove_file(&p.current_path).unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(r.metadata.inventory.report.files, 120);
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Complete);
        assert_eq!(r.owners.len(), 121);
        #[cfg(target_os = "linux")]
        assert!(r.owners.iter().all(|o| matches!(
            o.id,
            MetadataValue::Known {
                accuracy: ValueAccuracy::Exact,
                ..
            }
        )));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn denied_listing_keeps_the_observed_directory_owner() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let blocked = temp.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("hidden"), []).unwrap();
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let _restore = Restore(blocked.clone());
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&blocked).is_ok() {
        eprintln!(
            "Native denial unavailable for this account; injected denial is tested separately"
        );
        return;
    }
    eprintln!("Native directory listing PermissionDenied reproduced");
    for workers in [1, 4, 8] {
        let r = collect(
            temp.path(),
            &options(workers),
            InventoryLimits::default(),
            &CancellationToken::default(),
        );
        check(&r);
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Partial);
        assert_eq!(r.metadata.inventory.report.files, 0);
        assert!(
            r.metadata
                .inventory
                .report
                .errors
                .iter()
                .any(|e| e.kind == "PermissionDenied")
        );
        let i = r
            .metadata
            .inventory
            .entries
            .iter()
            .position(|e| e.path == blocked)
            .unwrap();
        assert!(matches!(
            r.owners[i].id,
            MetadataValue::Known {
                accuracy: ValueAccuracy::Exact,
                ..
            }
        ));
    }
}
