use super::*;
use sortsize_core::{
    CancellationToken, InventoryEntry, InventoryLimits, ScanOptions, ValueAccuracy,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn known(value: SystemTime) -> MetadataValue<SystemTime> {
    MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    }
}
fn input(count: usize) -> InventoryWithDates {
    let fixture = TempDir::new().unwrap();
    let mut input = super::date_tests::acquire(fixture.path(), 1, InventoryLimits::default());
    input.inventory.entries = (0..count)
        .map(|i| InventoryEntry {
            path: PathBuf::from("synthetic").join(format!("n{i:05}")),
            parent: (i != 0).then_some(0),
            kind: if i == 0 {
                InventoryKind::Directory
            } else {
                InventoryKind::File
            },
            logical_bytes: Some(i as u64),
            incomplete: false,
            counts: None,
        })
        .collect();
    // Windows SystemTime uses 100 ns ticks; each synthetic instant must be distinct.
    input.dates = (0..count)
        .map(|i| EntryDates {
            modified: known(UNIX_EPOCH + Duration::from_nanos(i as u64 * 100)),
            accessed: known(UNIX_EPOCH - Duration::from_nanos(i as u64 * 100)),
        })
        .collect();
    input
}
fn ids(tree: &Tree) -> Vec<usize> {
    tree.details()
        .0
        .iter()
        .map(|r| r.node_id as usize)
        .collect()
}

#[test]
fn four_orders_compare_real_instants_with_native_name_and_id_ties() {
    let mut input = input(15);
    let old = known(UNIX_EPOCH - Duration::new(2_208_988_800, 123_456_700));
    let future = known(UNIX_EPOCH + Duration::new(4_102_444_800, 100));
    let cases = [
        ("root", old),
        ("dir-old", old),
        ("dir-new", future),
        ("dir-denied", MetadataValue::AccessDenied),
        ("old", old),
        ("epoch", known(UNIX_EPOCH)),
        (
            "nano-low",
            known(UNIX_EPOCH + Duration::new(4_102_444_800, 99)),
        ),
        ("tie-z", future),
        ("tie-a", future),
        ("tie-a", future),
        ("unknown", MetadataValue::Unknown),
        ("denied", MetadataValue::AccessDenied),
        ("unsupported", MetadataValue::Unsupported),
        ("not-collected", MetadataValue::NotCollected),
        ("dir-unknown", MetadataValue::Unknown),
    ];
    for (i, (name, value)) in cases.into_iter().enumerate() {
        input.inventory.entries[i].path = PathBuf::from("synthetic").join(name);
        if [0, 1, 2, 3, 14].contains(&i) {
            input.inventory.entries[i].kind = InventoryKind::Directory;
        }
        input.dates[i] = EntryDates {
            modified: value,
            accessed: value,
        };
    }
    let date_pointer = input.dates.as_ptr();
    let paths: Vec<_> = input
        .inventory
        .entries
        .iter()
        .map(|e| e.path.as_os_str().as_encoded_bytes().as_ptr())
        .collect();
    let (mut tree, _) = Tree::with_dates(input).unwrap();
    assert_eq!(tree.dates.as_ref().unwrap().as_ptr(), date_pointer);
    for (node, ptr) in tree.nodes.iter().zip(paths) {
        assert_eq!(node.path.as_os_str().as_encoded_bytes().as_ptr(), ptr);
    }
    let recent = vec![2, 1, 3, 14, 8, 9, 7, 6, 5, 4, 11, 13, 10, 12];
    let ancient = vec![1, 2, 3, 14, 4, 5, 6, 8, 9, 7, 11, 13, 10, 12];
    for _ in 0..5 {
        for accessed in [false, true] {
            tree.sort_dates(accessed);
            assert_eq!(tree.sort(), if accessed { 10 } else { 8 });
            assert_eq!(ids(&tree), recent);
            tree.sort_dates(accessed);
            assert_eq!(tree.sort(), if accessed { 11 } else { 9 });
            assert_eq!(ids(&tree), ancient);
        }
    }
}

#[test]
fn large_pages_switch_all_twelve_orders_without_rebuilding_and_reveal_top100() {
    let mut input = input(50_000);
    input.inventory.entries[0].logical_bytes = Some(1_249_975_011);
    input.inventory.entries_omitted = 11;
    input.inventory.report.coverage = Coverage::Partial;
    input.inventory.entries[0].incomplete = true;
    input
        .inventory
        .top_files
        .entries
        .push(sortsize_core::TopFile {
            path: input.inventory.entries[49_999].path.clone(),
            logical_bytes: 49_999,
            retained_entry: Some(49_999),
        });
    let (mut tree, report) = Tree::with_dates(input).unwrap();
    assert_eq!(report.coverage, Coverage::Partial);
    let standard: Vec<_> = (0..4).map(|s| tree.children(0, s).to_vec()).collect();
    let date_pointers: Vec<_> = (8..12).map(|s| tree.children(0, s).as_ptr()).collect();
    // Exercise available Owner routing and allocation fallback together with dates.
    tree.attach_owners(vec![
        sortsize_core::EntryOwner {
            id: MetadataValue::NotCollected
        };
        50_000
    ])
    .unwrap();
    for _ in 0..3 {
        for sort in [0, 8, 6, 9, 4, 10, 7, 11, 5, 1, 2, 3] {
            tree.sort = sort;
            tree.detail_offset = 0;
            let expected: Vec<_> = match sort {
                8 | 11 => (1..50_000).rev().collect(),
                9 | 10 => (1..50_000).collect(),
                0..=3 => standard[sort].clone(),
                4 | 5 => standard[0].clone(),
                _ => (1..50_000).collect(),
            };
            assert_eq!(tree.children(0, sort), expected);
            let mut visited = Vec::new();
            loop {
                let (rows, more) = tree.details();
                assert!(rows.len() <= MAX_VISIBLE_ROWS);
                for row in rows {
                    let id = row.node_id as usize;
                    assert_eq!(row.path, tree.entry_path(id).unwrap());
                    assert_eq!(
                        row.modified,
                        crate::date_label::format_date(tree.dates.as_ref().unwrap()[id].modified)
                    );
                    visited.push(id);
                }
                if !more {
                    break;
                }
                tree.change_page(true, true);
            }
            assert_eq!(visited.len(), 49_999);
            assert_eq!(visited, tree.children(0, sort));
            let (_, Some(target)) = tree.top_selection(0).unwrap() else {
                panic!("missing retained identity")
            };
            tree.select_entry_parent(target);
            tree.reveal_entry(target);
            assert!(ids(&tree).contains(&target));
            assert!(!tree.treemap(800.0, 200.0).is_empty());
        }
    }
    for (i, sort) in (8..12).enumerate() {
        assert_eq!(tree.children(0, sort).as_ptr(), date_pointers[i]);
    }
    tree.change_page(true, true);
    tree.sort_dates(false);
    assert_eq!(tree.offset(true), 0);
    assert_eq!(tree.selected, Some(0));
}

#[test]
fn temporal_orders_work_in_all_four_metadata_combinations_after_source_deletion() {
    let fixture = TempDir::new().unwrap();
    std::fs::create_dir(fixture.path().join("folder")).unwrap();
    std::fs::write(fixture.path().join("folder/child"), b"x").unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(fixture.path().join(name), b"x").unwrap();
    }
    let mut trees = Vec::new();
    for allocation in [false, true] {
        for owner in [false, true] {
            let mut result = crate::worker::acquire_with_owner(
                fixture.path(),
                &ScanOptions::default(),
                &CancellationToken::default(),
                InventoryLimits::default(),
                allocation,
                owner,
                |_| {},
            )
            .unwrap();
            let dates = match &mut result {
                crate::worker::AcquiredInventory::Dates(r) => &mut r.dates,
                crate::worker::AcquiredInventory::Allocated(r) => &mut r.metadata.dates,
                crate::worker::AcquiredInventory::Owners(r) => &mut r.metadata.dates,
                crate::worker::AcquiredInventory::OwnerAllocated(r) => {
                    &mut r.allocated.metadata.dates
                }
            };
            for (i, date) in dates.iter_mut().enumerate() {
                *date = EntryDates {
                    modified: known(UNIX_EPOCH + Duration::from_nanos(i as u64 * 100)),
                    accessed: known(UNIX_EPOCH - Duration::from_nanos(i as u64 * 100)),
                };
            }
            let (tree, _) = result.into_tree().unwrap();
            assert_eq!(tree.owner_data().is_some(), owner);
            assert_eq!(tree.allocation_sort_available(), allocation);
            trees.push(tree);
        }
    }
    fixture.close().unwrap();
    for mut tree in trees {
        for accessed in [false, true] {
            for _ in 0..2 {
                tree.sort_dates(accessed);
                let rows = tree.details().0;
                assert_eq!(rows[0].name, "folder");
                let mut indices = ids(&tree)[1..].to_vec();
                let mut expected = indices.clone();
                expected.sort_unstable();
                if matches!(tree.sort(), 8 | 11) {
                    expected.reverse();
                }
                assert_eq!(indices, expected);
                indices.clear();
                let directory = rows[0].node_id as usize;
                tree.select(directory);
                assert_eq!(tree.details().0[0].name, "child");
                tree.parent();
                assert_eq!(tree.selected, Some(0));
            }
        }
    }
}

#[test]
fn legacy_and_misaligned_tables_are_safe_in_every_order() {
    let mut bad = input(4);
    bad.dates.pop();
    assert!(Tree::with_dates(bad).is_err());
    let (mut legacy, _) = Tree::new(input(4).inventory);
    let default = ids(&legacy);
    for sort in 4..=12 {
        legacy.sort = sort;
        assert_eq!(ids(&legacy), default);
        assert!(
            legacy
                .details()
                .0
                .iter()
                .all(|r| r.modified == "Non collectée")
        );
    }
    let (tree, _) = Tree::with_dates(input(1)).unwrap();
    for sort in 0..12 {
        assert!(tree.children(0, sort).is_empty());
    }
}

#[test]
fn native_names_break_equal_dates_without_lossy_unicode_collisions() {
    let mut input = input(4);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        for (i, bytes) in [
            (1, vec![b'n', 0xff]),
            (2, vec![b'n', 0xfe]),
            (3, vec![b'n', 0xfd]),
        ] {
            input.inventory.entries[i].path =
                PathBuf::from("synthetic").join(std::ffi::OsString::from_vec(bytes));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        for (i, units) in [
            (1, [0x006e, 0xd802]),
            (2, [0x006e, 0xd801]),
            (3, [0x006e, 0xd800]),
        ] {
            input.inventory.entries[i].path =
                PathBuf::from("synthetic").join(std::ffi::OsString::from_wide(&units));
        }
    }
    input.dates.fill(EntryDates {
        modified: known(UNIX_EPOCH),
        accessed: MetadataValue::Unknown,
    });
    let paths: Vec<_> = input
        .inventory
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect();
    let (mut tree, _) = Tree::with_dates(input).unwrap();
    for sort in 8..12 {
        tree.sort = sort;
        assert_eq!(ids(&tree), [3, 2, 1]);
        for row in tree.details().0 {
            assert_eq!(
                tree.nodes[row.node_id as usize].path,
                paths[row.node_id as usize]
            );
        }
    }
}
