use super::allocation_tests::acquire;
use super::*;
use sortsize_core::{InventoryLimits, ValueAccuracy};
use std::fs;
use tempfile::TempDir;

fn known(value: u64) -> MetadataValue<u64> {
    MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    }
}

#[test]
fn allocated_orders_use_numbers_keep_absences_last_and_preserve_bounds() {
    let fixture = TempDir::new().unwrap();
    for name in ["dir-big", "dir-small", "dir-unknown"] {
        fs::create_dir(fixture.path().join(name)).unwrap();
    }
    // Similar formatted values, integer precision above f64, and all absence states.
    let cases = [
        ("dir-big", known(9000)),
        ("dir-small", known(0)),
        ("dir-unknown", MetadataValue::AccessDenied),
        ("big", known(u64::MAX)),
        ("almost-big", known(u64::MAX - 1)),
        ("same-z", known(4096)),
        ("same-a", known(4096)),
        ("round-up", known(4097)),
        (
            "lower",
            MetadataValue::Known {
                value: 5000,
                accuracy: ValueAccuracy::LowerBound,
            },
        ),
        ("zero", known(0)),
        ("unknown", MetadataValue::Unknown),
        ("denied", MetadataValue::AccessDenied),
        ("unsupported", MetadataValue::Unsupported),
        ("not-collected", MetadataValue::NotCollected),
    ];
    for (name, _) in cases.iter().skip(3) {
        fs::write(fixture.path().join(name), []).unwrap();
    }
    for workers in [1, 4, 8] {
        let mut input = acquire(fixture.path(), workers, InventoryLimits::default());
        for (entry, metric) in input
            .metadata
            .inventory
            .entries
            .iter()
            .zip(&mut input.entries_by_path)
        {
            if let Some((_, value)) = cases.iter().find(|(name, _)| entry.path.ends_with(name)) {
                metric.value = *value;
            }
        }
        // Paths move into nodes rather than allocating a second native buffer.
        let pointers: Vec<_> = input
            .metadata
            .inventory
            .entries
            .iter()
            .map(|e| e.path.as_os_str().as_encoded_bytes().as_ptr())
            .collect();
        let (mut tree, _) = Tree::with_allocations(input).unwrap();
        for (node, pointer) in tree.nodes.iter().zip(pointers) {
            assert_eq!(node.path.as_os_str().as_encoded_bytes().as_ptr(), pointer);
        }
        assert!(tree.allocation_sort_available());
        tree.sort_allocated();
        assert_eq!(tree.sort(), 4);
        assert_eq!(
            tree.details()
                .0
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            [
                "dir-big",
                "dir-small",
                "dir-unknown",
                "big",
                "almost-big",
                "lower",
                "round-up",
                "same-a",
                "same-z",
                "zero",
                "denied",
                "not-collected",
                "unknown",
                "unsupported"
            ]
        );
        tree.sort_allocated();
        assert_eq!(tree.sort(), 5);
        let rows = tree.details().0;
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            [
                "dir-small",
                "dir-big",
                "dir-unknown",
                "zero",
                "same-a",
                "same-z",
                "round-up",
                "lower",
                "almost-big",
                "big",
                "denied",
                "not-collected",
                "unknown",
                "unsupported"
            ]
        );
        assert!(
            rows.iter()
                .find(|r| r.name == "lower")
                .unwrap()
                .allocated
                .starts_with("≥ ")
        );
        assert_eq!(
            rows.iter().find(|r| r.name == "same-a").unwrap().allocated,
            rows.iter()
                .find(|r| r.name == "round-up")
                .unwrap()
                .allocated
        );
        // Switching columns retains the historical first click directions.
        tree.sort_by(false);
        assert_eq!(tree.sort(), 0);
        tree.sort_by(false);
        assert_eq!(tree.sort(), 1);
        tree.sort_by(true);
        assert_eq!(tree.sort(), 2);
        tree.sort_by(true);
        assert_eq!(tree.sort(), 3);
        tree.sort_allocated();
        assert_eq!(tree.sort(), 4);
    }
}

#[test]
fn real_allocations_sort_sparse_hardlinks_directory_totals_pages_and_native_paths() {
    let fixture = TempDir::new().unwrap();
    // The scanner retains canonical native paths (verbatim paths on Windows).
    let canonical_root = fs::canonicalize(fixture.path()).unwrap();
    fs::create_dir(fixture.path().join("été 日本語")).unwrap();
    fs::write(fixture.path().join("été 日本語/child"), [1; 16384]).unwrap();
    fs::hard_link(
        fixture.path().join("été 日本語/child"),
        fixture.path().join("hardlink"),
    )
    .unwrap();
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
    for i in 0..2100 {
        fs::write(fixture.path().join(format!("f{i:04}")), [1; 4096]).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        for bytes in [b"native-\xfe", b"native-\xff"] {
            fs::write(
                fixture
                    .path()
                    .join(std::ffi::OsString::from_vec(bytes.to_vec())),
                [],
            )
            .unwrap();
        }
    }
    let mut signatures = Vec::new();
    for workers in [1, 4, 8] {
        let input = acquire(fixture.path(), workers, InventoryLimits::default());
        let values: Vec<_> = input
            .entries_by_path
            .iter()
            .map(|m| match m.value {
                MetadataValue::Known { value, .. } => value,
                _ => panic!("expected native allocation on this fixture"),
            })
            .collect();
        let (mut tree, _) = Tree::with_allocations(input).unwrap();
        let id = |name: &str| {
            tree.nodes
                .iter()
                .position(|n| n.path.ends_with(name))
                .unwrap()
        };
        let (folder, hardlink, sparse) = (id("été 日本語"), id("hardlink"), id("sparse"));
        assert_eq!(values[folder], values[hardlink]);
        assert!(values[sparse] < values[hardlink]);
        let map = tree
            .treemap(700.0, 190.0)
            .iter()
            .map(|t| t.node_id)
            .collect::<Vec<_>>();
        let top = tree
            .top_rows
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>();
        for descending in [true, false] {
            tree.sort_allocated();
            let indices = tree.children(0, tree.sort());
            assert_eq!(indices[0], folder);
            for pair in indices[1..].windows(2) {
                assert!(if descending {
                    values[pair[0]] >= values[pair[1]]
                } else {
                    values[pair[0]] <= values[pair[1]]
                });
                if values[pair[0]] == values[pair[1]] {
                    assert!(tree.nodes[pair[0]].path < tree.nodes[pair[1]].path);
                }
            }
            signatures.push(
                indices
                    .iter()
                    .map(|&i| {
                        tree.nodes[i]
                            .path
                            .strip_prefix(&canonical_root)
                            .unwrap()
                            .to_owned()
                    })
                    .collect::<Vec<_>>(),
            );
            let mut seen = Vec::new();
            loop {
                let (page, next) = tree.details();
                assert!(page.len() <= 2000);
                seen.extend(page.iter().map(|r| r.node_id as usize));
                if !next {
                    break;
                }
                assert_eq!(page.len(), 2000);
                tree.change_page(true, true);
            }
            assert_eq!(seen, tree.children(0, tree.sort()));
            let position = tree
                .children(0, tree.sort())
                .iter()
                .position(|&i| i == hardlink)
                .unwrap();
            assert_eq!(tree.reveal_entry(hardlink), position % MAX_VISIBLE_ROWS);
            assert!(
                tree.details()
                    .0
                    .iter()
                    .any(|r| r.node_id == hardlink as i32)
            );
            assert!(tree.entry_path(hardlink).unwrap().ends_with("hardlink"));
            tree.select(folder);
            assert_eq!(tree.details().0.len(), 1);
            tree.parent();
            assert_eq!(tree.selected, Some(0));
            assert_eq!(
                tree.treemap(700.0, 190.0)
                    .iter()
                    .map(|t| t.node_id)
                    .collect::<Vec<_>>(),
                map
            );
            assert_eq!(
                tree.top_rows
                    .iter()
                    .map(|r| r.path.clone())
                    .collect::<Vec<_>>(),
                top
            );
        }
    }
    assert_eq!(signatures[0], signatures[2]);
    assert_eq!(signatures[0], signatures[4]);
    assert_eq!(signatures[1], signatures[3]);
    assert_eq!(signatures[1], signatures[5]);
}

#[test]
fn absent_numeric_allocations_disable_sort_without_changing_order_or_page() {
    let fixture = TempDir::new().unwrap();
    for i in 0..2001 {
        fs::write(fixture.path().join(format!("f{i}")), []).unwrap();
    }
    for collected in [false, true] {
        let mut input = acquire(fixture.path(), 1, InventoryLimits::default());
        for m in &mut input.entries_by_path {
            m.value = MetadataValue::NotCollected;
        }
        let mut tree = if collected {
            Tree::with_allocations(input).unwrap().0
        } else {
            Tree::with_dates(InventoryWithDates {
                inventory: input.metadata.inventory,
                dates: input.metadata.dates,
            })
            .unwrap()
            .0
        };
        assert!(!tree.allocation_sort_available());
        assert!(
            tree.nodes
                .iter()
                .filter_map(|n| n.children.as_ref())
                .all(|o| o.allocated.is_none())
        );
        tree.sort_by(true);
        tree.change_page(true, true);
        let before = tree
            .details()
            .0
            .iter()
            .map(|r| r.node_id)
            .collect::<Vec<_>>();
        tree.sort_allocated();
        assert_eq!(tree.sort(), 2);
        assert_eq!(tree.detail_offset, 2000);
        assert_eq!(
            before,
            tree.details()
                .0
                .iter()
                .map(|r| r.node_id)
                .collect::<Vec<_>>()
        );
    }
}
