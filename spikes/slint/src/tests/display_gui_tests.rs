use super::*;
use crate::units::Units;
use sortsize_core::{MetadataValue, ValueAccuracy};
fn ids(window: &SortSizeWindow) -> Vec<i32> {
    window.get_details().iter().map(|r| r.node_id).collect()
}
fn known(value: u64) -> MetadataValue<u64> {
    MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    }
}

#[test]
fn fifty_thousand_nodes_all_twelve_orders_page_selection_and_owner_generation_survive_display_edits()
 {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("seed"), [1; 4096]).unwrap();
    let mut acquired = worker::acquire_with_owner(
        fixture.path(),
        &Default::default(),
        &Default::default(),
        Default::default(),
        true,
        true,
        |_| {},
    )
    .unwrap();
    let worker::AcquiredInventory::OwnerAllocated(raw) = &mut acquired else {
        panic!()
    };
    let inventory = &mut raw.allocated.metadata.inventory;
    inventory.entries.truncate(1);
    for i in 1..50_000 {
        inventory.entries.push(sortsize_core::InventoryEntry {
            path: fixture.path().join(format!("f{i:05}")),
            parent: Some(0),
            kind: sortsize_core::InventoryKind::File,
            logical_bytes: Some(i as u64 * 1234),
            incomplete: false,
            counts: None,
        });
    }
    raw.allocated.metadata.dates = temporal_gui_tests::injected(50_000);
    raw.allocated.entries_by_path = (0..50_000)
        .map(|i| sortsize_core::Metadata {
            value: if i % 7 == 0 {
                MetadataValue::Unknown
            } else {
                known(i * 4096)
            },
            coverage: sortsize_core::Coverage::Complete,
        })
        .collect();
    raw.owners.resize(
        50_000,
        sortsize_core::EntryOwner {
            id: MetadataValue::NotCollected,
        },
    );
    controller.borrow_mut().tree = Some(acquired.into_tree().unwrap().0);
    controller.borrow_mut().show_rows(&window);
    fixture.close().unwrap(); // No filesystem source remains for a rescan.
    for order in 0..12 {
        {
            let mut state = controller.borrow_mut();
            let tree = state.tree.as_mut().unwrap();
            match order {
                0 | 1 => {
                    tree.sort_by(false);
                    if tree.sort() != order {
                        tree.sort_by(false);
                    }
                }
                2 | 3 => {
                    tree.sort_by(true);
                    if tree.sort() != order {
                        tree.sort_by(true);
                    }
                }
                4 | 5 => {
                    tree.sort_allocated();
                    if tree.sort() != order {
                        tree.sort_allocated();
                    }
                }
                6 | 7 => tree.sort_owner(None, order - 6),
                _ => {
                    tree.sort_dates(order >= 10);
                    if tree.sort() != order {
                        tree.sort_dates(order >= 10);
                    }
                }
            }
            state.show_rows(&window);
        }
        window.invoke_change_page(true, true);
        let row = window.get_details().row_data(32).unwrap();
        window.invoke_inspect_entry(row.node_id);
        window.set_detail_scroll_x(-900.0);
        window.set_detail_scroll_y(-102.0);
        let expected = ids(&window);
        let path = window.get_entry_path();
        let model = window.get_details();
        let page = controller.borrow().tree.as_ref().unwrap().owner_page_key();
        let generation = controller.borrow().generation;
        let owner_page = controller.borrow().owner_page;
        let owner_serial = controller.borrow().owner_sort_serial;
        for col in 1..11 {
            window.invoke_set_column(col, false);
        }
        window.invoke_set_column(0, false);
        assert_eq!(window.get_table_pixels(), 202.0);
        assert!(window.get_column_settings().row_data(0).unwrap().shown);
        for decimal in [true, false, true] {
            window.invoke_set_units(decimal);
            assert_eq!(window.get_details(), model); // Same Slint model, size fields patched in place.
            assert_eq!(ids(&window), expected);
            assert_eq!(window.get_entry_path(), path);
            assert_eq!(window.get_inspected_node(), row.node_id);
            assert_eq!(window.get_sort_order(), order as i32);
            assert_eq!(window.get_detail_scroll_y(), -102.0);
            assert!(window.get_has_previous_detail_page());
            let state = controller.borrow();
            assert_eq!(state.generation, generation);
            assert_eq!(state.owner_page, owner_page);
            assert_eq!(state.owner_sort_serial, owner_serial);
            assert_eq!(state.tree.as_ref().unwrap().owner_page_key(), page);
            assert!(state.task.is_none());
            let shown = window.get_details().row_data(32).unwrap();
            assert_eq!(
                shown.size,
                if decimal {
                    Units::Decimal
                } else {
                    Units::Binary
                }
                .format(shown.node_id as u64 * 1234)
            );
            assert_eq!(shown.modified, row.modified);
            assert_eq!(shown.owner, row.owner);
            assert_eq!(shown.path, row.path);
        }
        window.invoke_reset_display();
        assert_eq!(ids(&window), expected);
        assert_eq!(window.get_table_pixels(), 1946.0);
    }
}
#[test]
fn display_edits_during_scan_use_latest_preferences_and_do_not_replace_the_task() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("file"), [0; 4096]).unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.set_collect_allocation(true);
    window.invoke_start_scan();
    let mailbox = controller.borrow().task.as_ref().unwrap().mailbox.clone();
    window.set_preferences_open(true);
    window.invoke_set_units(true);
    window.invoke_set_column(8, false);
    window.set_preferences_open(false);
    assert!(std::sync::Arc::ptr_eq(
        &mailbox,
        &controller.borrow().task.as_ref().unwrap().mailbox
    ));
    let start = Instant::now();
    while window.get_busy() {
        controller.borrow_mut().poll(&window);
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert_eq!(window.get_details().row_data(0).unwrap().size, "4.1 kB");
    assert!(window.get_summary().contains("4.1 kB"));
    assert_eq!(window.get_top_rows().row_data(0).unwrap().size, "4.1 kB");
    let tiles = window.get_tiles();
    assert!(tiles.iter().any(|tile| tile.size == "4.1 kB"));
    let geometry: Vec<_> = tiles
        .iter()
        .map(|tile| (tile.node_id, tile.x, tile.y, tile.w, tile.h))
        .collect();
    assert!(!window.get_column_settings().row_data(8).unwrap().shown);
    window.invoke_set_units(false);
    assert!(window.get_summary().contains("4.0 KiB"));
    assert_eq!(window.get_top_rows().row_data(0).unwrap().size, "4.0 KiB");
    assert_eq!(
        window
            .get_tiles()
            .iter()
            .map(|tile| (tile.node_id, tile.x, tile.y, tile.w, tile.h))
            .collect::<Vec<_>>(),
        geometry
    );
    assert!(
        window
            .get_details()
            .row_data(0)
            .unwrap()
            .allocated
            .ends_with("KiB")
    );
}
#[test]
fn compact_and_large_windows_visibility_patterns_scroll_clamp_and_persistence_are_isolated() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    let config = fixture.path().join("config/display-v1.conf");
    controller.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(config.clone()).unwrap());
    for (w, h) in [(1200, 760), (960, 600), (1600, 1000)] {
        window.window().set_size(slint::PhysicalSize::new(w, h));
        window.show().unwrap();
        for pattern in [
            vec![],
            vec![1],
            (1..11).filter(|i| i % 2 == 0).collect(),
            vec![1, 2, 3, 4, 5, 6, 7],
            vec![1, 2, 4, 5, 6, 7, 8, 9],
        ] {
            for col in 1..11 {
                window.invoke_set_column(col, pattern.contains(&col));
            }
            let expected = 2
                + 200
                + pattern
                    .iter()
                    .map(|i| u32::from(crate::display_preferences::WIDTHS[*i as usize]) + 6)
                    .sum::<u32>();
            assert_eq!(window.get_table_pixels(), expected as f32);
            window.set_detail_scroll_x(-1500.0);
            window.invoke_reset_display();
            for col in 1..11 {
                window.invoke_set_column(col, false);
            }
            slint::platform::update_timers_and_animations();
            let mut pixels = vec![slint::Rgb8Pixel::default(); w as usize * h as usize];
            adapter.draw_if_needed(|r| {
                r.render(&mut pixels, w as usize);
            });
            assert_eq!(window.get_detail_scroll_x(), 0.0);
            window.set_preferences_open(true);
            adapter.draw_if_needed(|r| {
                r.render(&mut pixels, w as usize);
            });
            window.set_preferences_open(false);
        }
    }
    window.invoke_set_units(true);
    controller.borrow_mut().preference_store = None;
    let (p, _) = crate::preferences_store::load(&config);
    assert_eq!(p, controller.borrow().preferences);
    window.hide().unwrap();
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn injected_volume_capacity_units_preserve_usage_unknowns_selection_and_never_discover() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let mut discovery = volume_gui_tests::discovered(&["volume".into()]);
    discovery.volumes[0].capacity = sortsize_core::volumes::VolumeCapacity::from_bytes(
        known(1 << 30),
        known(1 << 29),
        MetadataValue::AccessDenied,
    );
    let mut sources = crate::volume_view::Sources::with_provider(Arc::new(move |_| {
        called.fetch_add(1, Ordering::SeqCst);
        panic!("display must never discover")
    }));
    sources.current = Some(crate::volume_view::Prepared::new(discovery));
    controller.borrow_mut().sources = sources;
    controller.borrow().show_sources(&window);
    let original = window.get_volumes().row_data(0).unwrap();
    window.invoke_set_units(true);
    let decimal = window.get_volumes().row_data(0).unwrap();
    assert!(original.summary.contains("1.0 GiB"));
    assert!(decimal.summary.contains("1.1 GB"));
    assert!(decimal.summary.contains("Accès refusé"));
    assert_eq!(decimal.fraction, original.fraction);
    assert_eq!(decimal.has_usage, original.has_usage);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(controller.borrow().task.is_none());
}

#[test]
fn unit_selector_keyboard_changes_preferences_through_the_actual_slint_widget() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 760));
    window.set_preferences_open(true);
    window.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    });
    let position = slint::LogicalPosition::new(500.0, 459.0);
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::DownArrow.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::DownArrow.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Return.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Return.into(),
    });
    assert_eq!(controller.borrow().preferences.units, Units::Decimal);
    assert!(window.get_decimal_units());
    window.hide().unwrap();
}
