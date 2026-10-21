use super::*;
use sortsize_core::{EntryDates, MetadataValue, ValueAccuracy};
use std::time::UNIX_EPOCH;

fn known(value: std::time::SystemTime) -> MetadataValue<std::time::SystemTime> {
    MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    }
}
pub(super) fn injected(count: usize) -> Vec<EntryDates> {
    // Keep the fixture and its oracle exact on native Windows (100 ns ticks).
    (0..count)
        .map(|i| EntryDates {
            modified: known(UNIX_EPOCH + Duration::from_nanos(i as u64 * 100)),
            accessed: known(UNIX_EPOCH - Duration::from_nanos(i as u64 * 100)),
        })
        .collect()
}
fn displayed(window: &SortSizeWindow) -> Vec<i32> {
    window.get_details().iter().map(|r| r.node_id).collect()
}
fn assert_page(window: &SortSizeWindow, expected: &[i32]) {
    assert_eq!(displayed(window), expected);
    for row in window.get_details().iter() {
        let i = row.node_id as u64;
        assert_eq!(
            row.modified,
            crate::date_label::format_date(known(UNIX_EPOCH + Duration::from_nanos(i * 100)))
        );
        assert_eq!(
            row.accessed,
            crate::date_label::format_date(known(UNIX_EPOCH - Duration::from_nanos(i * 100)))
        );
    }
}

#[test]
fn date_callbacks_show_ordered_pages_arrows_navigation_and_top100_for_all_metadata_modes() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("folder/child")).unwrap();
    fs::write(fixture.path().join("folder/child/file"), b"nested").unwrap();
    for i in 0..2105 {
        fs::write(fixture.path().join(format!("f{i:04}")), [1; 3]).unwrap();
    }
    for allocation in [false, true] {
        for owner in [false, true] {
            let mut raw = worker::acquire_with_owner(
                fixture.path(),
                &sortsize_core::ScanOptions::default(),
                &sortsize_core::CancellationToken::default(),
                sortsize_core::InventoryLimits::default(),
                allocation,
                owner,
                |_| {},
            )
            .unwrap();
            let entries = &raw.inventory().entries;
            let directory = entries
                .iter()
                .position(|e| e.path.ends_with("folder"))
                .unwrap() as i32;
            let mut ascending: Vec<_> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.parent == Some(0) && e.kind == sortsize_core::InventoryKind::File
                })
                .map(|(i, _)| i as i32)
                .collect();
            ascending.sort_unstable();
            let mut descending = ascending.clone();
            descending.reverse();
            ascending.insert(0, directory);
            descending.insert(0, directory);
            let dates = injected(entries.len());
            match &mut raw {
                worker::AcquiredInventory::Dates(r) => r.dates = dates,
                worker::AcquiredInventory::Allocated(r) => r.metadata.dates = dates,
                worker::AcquiredInventory::Owners(r) => r.metadata.dates = dates,
                worker::AcquiredInventory::OwnerAllocated(r) => r.allocated.metadata.dates = dates,
            }
            controller.borrow_mut().tree = Some(raw.into_tree().unwrap().0);
            controller.borrow_mut().show_rows(&window);
            let path = window.get_current_path();
            window.set_detail_scroll_x(-700.0);
            // First and second click, rapid alternation, plus historical orders.
            for (accessed, order) in [
                (false, 8),
                (false, 9),
                (true, 10),
                (true, 11),
                (false, 8),
                (true, 10),
                (false, 8),
            ] {
                window.set_detail_scroll_y(-35.0);
                window.invoke_sort_dates(accessed);
                assert_eq!(window.get_sort_order(), order);
                let expected = if matches!(order, 8 | 11) {
                    &descending
                } else {
                    &ascending
                };
                assert_page(&window, &expected[..2000]);
                assert_eq!(window.get_detail_scroll_y(), 0.0);
                assert_eq!(window.get_detail_scroll_x(), -700.0);
                assert_eq!(window.get_current_path(), path);
                let heading = if accessed {
                    window.get_accessed_heading()
                } else {
                    window.get_modified_heading()
                };
                assert!(heading.ends_with(if matches!(order, 8 | 10) {
                    "↓"
                } else {
                    "↑"
                }));
                let other = if accessed {
                    window.get_modified_heading()
                } else {
                    window.get_accessed_heading()
                };
                assert!(!other.ends_with('↓') && !other.ends_with('↑'));
                window.invoke_change_page(true, true);
                assert_page(&window, &expected[2000..]);
                assert!(window.get_has_previous_detail_page());
                window.invoke_sort_dates(accessed);
                assert!(!window.get_has_previous_detail_page());
                window.invoke_sort_dates(accessed);
            }
            // Historical callbacks keep their original indices and displayed data.
            for (name, orders) in [(true, [2, 3]), (false, [0, 1])] {
                window.invoke_sort_dates(false);
                for order in orders {
                    window.invoke_sort_details(name);
                    assert_eq!(window.get_sort_order(), order);
                    assert_eq!(
                        displayed(&window),
                        controller
                            .borrow()
                            .tree
                            .as_ref()
                            .unwrap()
                            .details()
                            .0
                            .iter()
                            .map(|r| r.node_id)
                            .collect::<Vec<_>>()
                    );
                }
            }
            if allocation {
                for order in [4, 5] {
                    window.invoke_sort_allocated();
                    assert_eq!(window.get_sort_order(), order);
                }
            }
            if owner {
                for order in [6, 7] {
                    window.invoke_sort_owner();
                    assert_eq!(window.get_sort_order(), order);
                }
            }
            window.invoke_sort_dates(false);
            window.invoke_select_node(directory);
            assert!(window.get_current_path().ends_with("folder"));
            let selected_path = window.get_current_path();
            window.invoke_sort_dates(true);
            assert_eq!(window.get_current_path(), selected_path);
            assert_eq!(window.get_details().row_data(0).unwrap().name, "child");
            window.invoke_go_parent();
            assert_eq!(window.get_current_path(), path);
            window.set_top_visible(true);
            window.invoke_select_top(0);
            window.invoke_reveal_top();
            assert!(!window.get_top_visible());
            assert!(displayed(&window).contains(&window.get_inspected_node()));
            assert_eq!(window.get_sort_order(), 10);
        }
    }
    if let Ok(directory) = std::env::var("SORTSIZE_DATE_SCREENSHOTS") {
        fs::create_dir_all(&directory).unwrap();
        window.invoke_select_node(0);
        window.set_detail_scroll_x(-930.0);
        for (accessed, order) in [(false, 8), (false, 9), (true, 10), (true, 11)] {
            window.invoke_sort_dates(accessed);
            assert_eq!(window.get_sort_order(), order);
            capture(
                &window,
                &adapter,
                &format!("{directory}/date-{order}.ppm"),
                (1200, 760),
            );
        }
    }
}

#[test]
fn completed_owner_sort_arriving_after_date_click_cannot_replace_temporal_rows() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    controller.borrow_mut().tree = Some(super::owner_gui_tests::synthetic_tree(50_000));
    let provider = std::sync::Arc::new(NoNames);
    controller.borrow_mut().owner_task = Some(owner_view::Task::start(provider).unwrap());
    window.set_resolve_owner(true);
    controller.borrow_mut().show_rows(&window);
    for (accessed, ascending) in [(false, false), (false, true), (true, false), (true, true)] {
        // A pending Owner sort preserves the current order until its reply arrives.
        window.invoke_sort_details(false);
        window.invoke_sort_owner();
        assert!(window.get_owner_sort_preparing());
        // Wait for the real coordinator's result without letting the UI consume it.
        let deadline = Instant::now() + Duration::from_secs(10);
        let update = loop {
            if let Some(update) = controller
                .borrow_mut()
                .owner_task
                .as_mut()
                .unwrap()
                .poll()
                .sort
            {
                break update;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        window.invoke_sort_dates(accessed);
        if ascending {
            window.invoke_sort_dates(accessed);
        }
        assert_eq!(
            window.get_sort_order(),
            8 + i32::from(accessed) * 2 + i32::from(ascending)
        );
        let expected = displayed(&window);
        let sort = window.get_sort_order();
        assert!(!window.get_owner_sort_preparing());
        controller
            .borrow_mut()
            .owner_task
            .as_mut()
            .unwrap()
            .inject_sort_update(update);
        controller.borrow_mut().poll_owners(&window);
        assert_eq!(window.get_sort_order(), sort);
        assert_eq!(displayed(&window), expected);
        assert!(controller.borrow().owner_sort_pending.is_none());
    }
}
struct NoNames;
impl sortsize_owner_resolution::OwnerLookup for NoNames {
    fn lookup(
        &self,
        _: sortsize_core::NativeOwnerId,
        _: sortsize_owner_resolution::LookupLimits,
    ) -> sortsize_owner_resolution::OwnerNameResolution {
        sortsize_owner_resolution::OwnerNameResolution::NotFound
    }
}
