use super::*;
use sortsize_core::{
    AllocationAggregationOptions, CancellationToken, ConcurrencyOptions, InventoryLimits, Metadata,
    MetadataSelection, ScanOptions, ValueAccuracy, scan_inventory_with_allocation_totals,
};
use std::fs;
use tempfile::TempDir;
pub(super) fn acquire(
    root: &std::path::Path,
    workers: usize,
    limits: InventoryLimits,
) -> InventoryWithAllocationTotals {
    scan_inventory_with_allocation_totals(
        root,
        &ScanOptions {
            concurrency: ConcurrencyOptions {
                workers,
                ..Default::default()
            },
            ..Default::default()
        },
        &CancellationToken::default(),
        limits,
        AllocationAggregationOptions {
            metadata: MetadataSelection {
                dates: true,
                allocation: false,
            },
            deduplicate_by_identity: false,
            max_identities: 0,
        },
        |_| {},
    )
    .unwrap()
}
fn label(value: MetadataValue<u64>) -> String {
    crate::allocation_label::format_allocation(Metadata {
        value,
        coverage: Coverage::Complete,
    })
}
#[test]
fn allocation_labels_follow_native_ids_through_sorts_pages_navigation_and_source_removal() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("été 日本語")).unwrap();
    fs::write(fixture.path().join("été 日本語/child"), [7; 4096]).unwrap();
    fs::write(fixture.path().join("empty"), []).unwrap();
    let sparse = fs::File::create(fixture.path().join("sparse")).unwrap();
    #[cfg(windows)]
    assert!(
        std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(fixture.path().join("sparse"))
            .status()
            .unwrap()
            .success()
    );
    sparse.set_len(64 * 1024 * 1024).unwrap();
    fs::hard_link(
        fixture.path().join("été 日本語/child"),
        fixture.path().join("hardlink"),
    )
    .unwrap();
    for i in 0..2100 {
        fs::write(fixture.path().join(format!("f{i:04}")), [1; 3]).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        fs::write(
            fixture
                .path()
                .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec())),
            b"x",
        )
        .unwrap();
        symlink(fixture.path(), fixture.path().join("cycle")).unwrap();
    }
    let mut trees = Vec::new();
    for workers in [1, 4, 8] {
        let mut input = acquire(fixture.path(), workers, InventoryLimits::default());
        assert_eq!(input.totals.identity_tracking.capacity, 0);
        assert_eq!(input.totals.by_identity.value, MetadataValue::NotCollected);
        let native_expected: Vec<_> = input
            .metadata
            .inventory
            .entries
            .iter()
            .zip(&input.entries_by_path)
            .map(|(e, m)| {
                (
                    e.path.clone(),
                    crate::allocation_label::format_allocation(*m),
                )
            })
            .collect();
        // Every absence state and a proven lower bound survive the GUI projection.
        for (i, m) in input.entries_by_path.iter_mut().enumerate().skip(10) {
            m.value = match i % 6 {
                0 => MetadataValue::Unknown,
                1 => MetadataValue::Unsupported,
                2 => MetadataValue::AccessDenied,
                3 => MetadataValue::NotCollected,
                4 => MetadataValue::Known {
                    value: 4096,
                    accuracy: ValueAccuracy::LowerBound,
                },
                _ => MetadataValue::Known {
                    value: 0,
                    accuracy: ValueAccuracy::Exact,
                },
            };
        }
        let expected: Vec<_> = input
            .entries_by_path
            .iter()
            .map(|m| crate::allocation_label::format_allocation(*m))
            .collect();
        let paths: Vec<_> = input
            .metadata
            .inventory
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect();
        let dates = input.metadata.dates.as_ptr();
        let (tree, _) = Tree::with_allocations(input).unwrap();
        assert_eq!(tree.dates.as_ref().unwrap().as_ptr(), dates);
        for (i, (_, value)) in native_expected.iter().enumerate().take(10) {
            assert_eq!(tree.allocation_label(i), value.as_str());
        }
        trees.push((tree, expected, paths));
    }
    fixture.close().unwrap();
    for (mut tree, expected, paths) in trees {
        for order in 0..6 {
            if order < 4 {
                tree.sort_by(order >= 2);
            } else {
                tree.sort_allocated();
            }
            for next in [true, false] {
                for row in tree.details().0 {
                    let id = row.node_id as usize;
                    assert_eq!(row.allocated, expected[id]);
                    assert_eq!(row.allocated, tree.allocation_label(id));
                    assert_eq!(tree.nodes[id].path, paths[id]);
                    assert_eq!(row.path, native_label(&paths[id]));
                    assert!(row.modified.ends_with(" UTC") || row.modified == "Non collectée");
                }
                tree.change_page(true, next);
            }
        }
        tree.select(0);
        let folder = tree
            .nodes
            .iter()
            .position(|n| n.path.ends_with("été 日本語"))
            .unwrap();
        tree.select(folder);
        assert_eq!(
            tree.details().0[0].allocated,
            expected[tree.details().0[0].node_id as usize]
        );
        tree.parent();
        assert_eq!(tree.selected, Some(0));
        assert!(!tree.treemap(700.0, 190.0).is_empty());
        assert_eq!(tree.top_rows.len(), 100);
        let (_, target) = tree.top_selection(0).unwrap();
        let target = target.unwrap();
        tree.select_entry_parent(target);
        tree.reveal_entry(target);
        assert!(tree.details().0.iter().any(|r| r.node_id == target as i32));
    }
}

#[test]
fn mismatched_tables_are_rejected_and_legacy_trees_are_not_collected() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("f"), [1; 4096]).unwrap();
    for which in 0..3 {
        let mut input = acquire(fixture.path(), 1, InventoryLimits::default());
        match which {
            0 => {
                input.entries_by_path.pop();
            }
            1 => {
                input.metadata.dates.pop();
            }
            _ => {
                input
                    .metadata
                    .allocations
                    .push(sortsize_core::EntryAllocation {
                        allocated_bytes: MetadataValue::Unknown,
                        identity: MetadataValue::Unknown,
                        link_count: MetadataValue::Unknown,
                    });
            }
        }
        assert!(Tree::with_allocations(input).is_err());
    }
    let input = acquire(fixture.path(), 1, InventoryLimits::default());
    let (tree, _) = Tree::with_dates(InventoryWithDates {
        inventory: input.metadata.inventory,
        dates: input.metadata.dates,
    })
    .unwrap();
    assert!(tree.allocation_values.is_none());
    assert_eq!(tree.details().0[0].allocated, "Non collectée");
}

#[test]
fn omitted_file_allocations_remain_in_directory_labels_and_logical_views() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("folder")).unwrap();
    for i in 0..50_010 {
        fs::write(fixture.path().join(format!("folder/f{i:05}")), [1; 3]).unwrap();
    }
    for workers in [1, 4, 8] {
        let input = acquire(fixture.path(), workers, InventoryLimits::default());
        assert_eq!(input.metadata.inventory.entries.len(), 50_000);
        assert_eq!(input.metadata.inventory.entries_omitted, 12);
        let folder = input
            .metadata
            .inventory
            .entries
            .iter()
            .position(|e| e.path.ends_with("folder"))
            .unwrap();
        let expected = crate::allocation_label::format_allocation(input.entries_by_path[folder]);
        let (mut tree, report) = Tree::with_allocations(input).unwrap();
        assert_eq!(report.files, 50_010);
        assert_eq!(tree.details().0[0].allocated, expected);
        assert!(!tree.details().0[0].allocated.starts_with('≥'));
        tree.select(folder);
        for _ in 0..24 {
            tree.change_page(true, true);
        }
        assert!(!tree.details().1);
        assert_eq!(tree.top_rows.len(), 100);
        assert!(!tree.treemap(700.0, 190.0).is_empty());
    }
}

#[test]
fn partial_cancelled_and_unknown_values_never_infer_a_numeric_bound_from_coverage() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("folder")).unwrap();
    fs::write(fixture.path().join("folder/f"), [1; 4096]).unwrap();
    for coverage in [Coverage::Partial, Coverage::Cancelled] {
        let mut input = acquire(fixture.path(), 1, InventoryLimits::default());
        let folder = input
            .metadata
            .inventory
            .entries
            .iter()
            .position(|e| e.path.ends_with("folder"))
            .unwrap();
        input.metadata.inventory.report.coverage = coverage;
        input.entries_by_path[folder] = Metadata {
            value: MetadataValue::Unknown,
            coverage,
        };
        let (tree, report) = Tree::with_allocations(input).unwrap();
        assert_eq!(report.coverage, coverage);
        assert_eq!(tree.details().0[0].allocated, "Inconnue");
    }
    assert_eq!(
        label(MetadataValue::Known {
            value: 0,
            accuracy: ValueAccuracy::Exact
        }),
        "0 octets"
    );
}

#[test]
fn real_partial_and_cancelled_scans_keep_proven_directory_bounds_and_exact_dates() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("folder")).unwrap();
    for i in 0..100 {
        fs::write(fixture.path().join(format!("folder/f{i}")), [1; 4096]).unwrap();
    }
    for workers in [1, 4, 8] {
        for cancelled in [false, true] {
            let token = CancellationToken::default();
            let input = crate::worker::acquire(
                fixture.path(),
                &ScanOptions {
                    max_depth: if cancelled { None } else { Some(1) },
                    concurrency: ConcurrencyOptions {
                        workers,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &token,
                InventoryLimits::default(),
                true,
                |p| {
                    if cancelled && p.files >= 1 {
                        token.cancel();
                    }
                },
            )
            .unwrap();
            let (mut tree, report) = input.into_tree().unwrap();
            assert_eq!(
                report.coverage,
                if cancelled {
                    Coverage::Cancelled
                } else {
                    Coverage::Partial
                }
            );
            tree.sort_allocated();
            assert_eq!(tree.sort(), 4);
            tree.sort_allocated();
            assert_eq!(tree.sort(), 5);
            let row = &tree.details().0[0];
            assert!(row.allocated.starts_with("≥ "));
            assert!(row.modified.ends_with(" UTC"));
        }
    }
}

#[cfg(windows)]
#[test]
fn unpaired_utf16_path_stays_paired_with_its_allocation_label() {
    use std::os::windows::ffi::OsStringExt;
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("f"), [1; 4096]).unwrap();
    let mut input = acquire(fixture.path(), 1, InventoryLimits::default());
    let path = input.metadata.inventory.entries[0]
        .path
        .join(std::ffi::OsString::from_wide(&[0x006e, 0xd800]));
    input.metadata.inventory.entries[1].path = path.clone();
    let expected = crate::allocation_label::format_allocation(input.entries_by_path[1]);
    let (tree, _) = Tree::with_allocations(input).unwrap();
    assert_eq!(tree.nodes[1].path, path);
    assert_eq!(tree.details().0[0].allocated, expected);
    assert!(tree.details().0[0].path.contains("D800") || tree.details().0[0].path.contains("d800"));
}
