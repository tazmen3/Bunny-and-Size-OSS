use sortsize_core::*;

#[test]
fn combined_api_keeps_one_observation_per_path_and_every_table_aligned() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("été-目录")).unwrap();
    for index in 0..24 {
        std::fs::write(
            root.path().join(format!("été-目录/f-{index}")),
            vec![7; index * 13],
        )
        .unwrap();
    }
    std::fs::hard_link(root.path().join("été-目录/f-23"), root.path().join("alias")).unwrap();
    for workers in [1, 4, 8] {
        for retained in [3, 50_000] {
            for owner in [false, true] {
                let mut options = ScanOptions::default();
                options.concurrency.workers = workers;
                let result = scan_inventory_with_owners_and_allocation_totals(
                    root.path(),
                    &options,
                    &CancellationToken::default(),
                    InventoryLimits {
                        max_entries: retained,
                        ..Default::default()
                    },
                    AllocationAggregationOptions {
                        metadata: MetadataSelection {
                            dates: true,
                            allocation: true,
                        },
                        ..Default::default()
                    },
                    owner,
                    |_| {},
                )
                .unwrap();
                let metadata = &result.allocated.metadata;
                let inventory = &metadata.inventory;
                let count = inventory.entries.len();
                assert_eq!(inventory.report.files, 25);
                assert_eq!(inventory.report.directories, 2);
                assert_eq!(inventory.report.coverage, Coverage::Complete);
                assert_eq!(
                    inventory.report.logical_bytes,
                    (0..24).sum::<u64>() * 13 + 23 * 13
                );
                assert_eq!(inventory.top_files.entries.len(), 25);
                assert_eq!(count, retained.min(27));
                assert_eq!(metadata.dates.len(), count);
                assert_eq!(metadata.allocations.len(), count);
                assert_eq!(result.allocated.entries_by_path.len(), count);
                assert_eq!(result.owners.len(), if owner { count } else { 0 });
                for (index, entry) in inventory.entries.iter().enumerate() {
                    if owner {
                        #[cfg(any(target_os = "linux", windows))]
                        assert!(matches!(
                            result.owners[index].id,
                            MetadataValue::Known { .. }
                        ));
                    }
                    assert!(matches!(
                        metadata.dates[index].modified,
                        MetadataValue::Known { .. }
                    ));
                    if entry.kind == InventoryKind::File {
                        assert_eq!(
                            metadata.allocations[index].allocated_bytes,
                            result.allocated.entries_by_path[index].value
                        );
                    }
                }
                assert_eq!(
                    result.allocated.entries_by_path[0],
                    result.allocated.totals.by_path
                );
                let MetadataValue::Known { value: paths, .. } =
                    result.allocated.totals.by_path.value
                else {
                    panic!("native allocation expected");
                };
                let MetadataValue::Known {
                    value: identities, ..
                } = result.allocated.totals.by_identity.value
                else {
                    panic!("native identity expected");
                };
                assert!(identities < paths);
            }
        }
    }
}

#[test]
fn combined_cancellation_keeps_lower_bounds_without_inventing_owner_records() {
    let root = tempfile::TempDir::new().unwrap();
    for i in 0..100 {
        std::fs::write(root.path().join(i.to_string()), [1; 4096]).unwrap();
    }
    let token = CancellationToken::default();
    let result = scan_inventory_with_owners_and_allocation_totals(
        root.path(),
        &ScanOptions::default(),
        &token,
        InventoryLimits::default(),
        AllocationAggregationOptions {
            metadata: MetadataSelection {
                dates: true,
                allocation: false,
            },
            ..Default::default()
        },
        true,
        |progress| {
            if progress.files >= 5 {
                token.cancel();
            }
        },
    )
    .unwrap();
    let inventory = &result.allocated.metadata.inventory;
    assert_eq!(inventory.report.coverage, Coverage::Cancelled);
    assert_eq!(inventory.entries.len(), result.owners.len());
    assert_eq!(
        inventory.entries.len(),
        result.allocated.entries_by_path.len()
    );
    assert!(result.allocated.metadata.allocations.is_empty());
    assert!(matches!(
        result.allocated.totals.by_path.value,
        MetadataValue::Known {
            accuracy: ValueAccuracy::LowerBound,
            ..
        }
    ));
}
