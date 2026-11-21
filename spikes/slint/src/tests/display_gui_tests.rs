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
    let window = crate::new_window().unwrap();
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
    let window = crate::new_window().unwrap();
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
    assert_eq!(window.get_details().row_data(0).unwrap().size, "4,1 kB");
    assert!(window.get_summary().contains("4,1 kB"));
    assert_eq!(window.get_top_rows().row_data(0).unwrap().size, "4,1 kB");
    let tiles = window.get_tiles();
    assert!(tiles.iter().any(|tile| tile.size == "4,1 kB"));
    let geometry: Vec<_> = tiles
        .iter()
        .map(|tile| (tile.node_id, tile.x, tile.y, tile.w, tile.h))
        .collect();
    assert!(!window.get_column_settings().row_data(8).unwrap().shown);
    window.invoke_set_units(false);
    assert!(window.get_summary().contains("4,0 KiB"));
    assert_eq!(window.get_top_rows().row_data(0).unwrap().size, "4,0 KiB");
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
    let window = crate::new_window().unwrap();
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
    let window = crate::new_window().unwrap();
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
    assert!(original.summary.contains("1,0 GiB"));
    assert!(decimal.summary.contains("1,1 GB"));
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
    let window = crate::new_window().unwrap();
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
    let position = slint::LogicalPosition::new(500.0, 549.0);
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

#[test]
fn l16_preferences_keyboard_focus_does_not_escape_to_background() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _state) = new_session(Default::default(), Some("en_US".into())).unwrap();
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 760));
    window.set_ribbon_tab(2);
    window.set_preferences_open(true);
    window.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1200);
    });
    for _ in 0..60 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Tab.into(),
        });
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Tab.into(),
        });
        assert!(
            !window.get_display_trigger_focused(),
            "Tab escaped preferences to the background trigger"
        );
    }
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    for _ in 0..60 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Tab.into(),
        });
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Tab.into(),
        });
        assert!(
            !window.get_display_trigger_focused(),
            "Shift-Tab escaped preferences"
        );
    }
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
    let position = slint::LogicalPosition::new(
        window.get_display_trigger_x() + 8.,
        window.get_display_trigger_y() + 8.,
    );
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
    assert!(
        window.get_preferences_open(),
        "Pointer activated the background display trigger"
    );
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });
    assert!(!window.get_preferences_open());
    assert!(window.get_display_trigger_focused());
    window.hide().unwrap();
}

#[test]
fn l16_cache_keyboard_focus_and_escape_return_to_initiator() {
    use slint::platform::{Key, WindowEvent};
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _state) = new_session(Default::default(), Some("en_US".into())).unwrap();
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 760));
    window.set_workshop_live(true);
    window.set_ribbon_tab(1);
    window.show().unwrap();
    window.set_cache_visible(true);
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1200);
    });
    // Saved measures now occupy the workspace, rather than a modal overlay.
    // Tab must let users reach the ribbon without dismissing the panel.
    let mut reached_ribbon = false;
    for _ in 0..60 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Tab.into(),
        });
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Tab.into(),
        });
        if window.get_cache_trigger_focused() {
            reached_ribbon = true;
            break;
        }
    }
    assert!(
        reached_ribbon,
        "Tab could not reach the saved measures ribbon command"
    );
    assert!(
        window.get_cache_visible(),
        "Tab dismissed the content panel"
    );

    // Reopening gives focus to the inventory list. Escape from within the
    // panel closes it and restores focus to the command that opened it.
    window.set_cache_visible(false);
    window.window().request_redraw();
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1200);
    });
    window.set_cache_visible(true);
    window.window().request_redraw();
    adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1200);
    });
    assert!(!window.get_cache_trigger_focused());
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });
    assert!(!window.get_cache_visible());
    assert!(window.get_cache_trigger_focused());
    window.hide().unwrap();
}

#[test]
#[ignore = "native Winit modal gestures; requires BUNNY_L16_NATIVE_OUTPUT and a display"]
fn native_l16_modal_keyboard_pointer_and_focus_restore() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("BUNNY_L16_NATIVE_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    let (window, _state) = new_session(Default::default(), Some("en_US".into())).unwrap();
    window
        .window()
        .set_size(slint::LogicalSize::new(1200., 760.));
    window.show().unwrap();
    let weak = window.as_weak();
    let timer = Timer::default();
    let mut stage = 0;
    let mut gesture = 0;
    let mut reached_cache_ribbon = false;
    let started = Instant::now();
    let key = |window: &SortSizeWindow, key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    timer.start(TimerMode::Repeated, Duration::from_millis(25), move || {
        assert!(started.elapsed() < Duration::from_secs(30), "native modal timeout");
        let window = weak.upgrade().unwrap();
        match stage {
            0 => {
                window.set_ribbon_tab(2);
                window.set_preferences_open(true);
                stage = 1;
            }
            1 | 3 => {
                if gesture == 60 {
                    window.window().dispatch_event(WindowEvent::KeyPressed { text: Key::Shift.into() });
                }
                key(&window, Key::Tab);
                if stage == 1 {
                    assert!(!window.get_display_trigger_focused(), "preferences focus escaped");
                } else {
                    assert!(window.get_cache_visible(), "Tab closed the saved measures panel");
                    reached_cache_ribbon |= window.get_cache_trigger_focused();
                }
                gesture += 1;
                if gesture == 120 {
                    window.window().dispatch_event(WindowEvent::KeyReleased { text: Key::Shift.into() });
                    let image = window.window().take_snapshot().unwrap();
                    let name = if stage == 1 { "preferences.ppm" } else { "cache.ppm" };
                    use std::io::Write;
                    let mut file = std::io::BufWriter::new(fs::File::create(output.join(name)).unwrap());
                    write!(file, "P6\n{} {}\n255\n", image.width(), image.height()).unwrap();
                    for pixel in image.as_slice() { file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap(); }
                    file.flush().unwrap();
                    if stage == 1 {
                        let position = slint::LogicalPosition::new(window.get_display_trigger_x() + 8., window.get_display_trigger_y() + 8.);
                        window.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
                        window.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
                        assert!(window.get_preferences_open());
                        key(&window, Key::Escape);
                        assert!(!window.get_preferences_open());
                        assert!(window.get_display_trigger_focused());
                    } else {
                        assert!(reached_cache_ribbon, "Tab could not reach the saved measures ribbon command");
                        // Reopen the nonmodal panel and exercise Escape with
                        // focus back inside it, after the next rendered frame.
                        window.set_cache_visible(false);
                    }
                    gesture = 0;
                    stage += 1;
                }
            }
            2 => {
                window.set_workshop_live(true);
                window.set_ribbon_tab(1);
                window.set_cache_visible(true);
                stage = 3;
            }
            4 => {
                window.set_cache_visible(true);
                stage = 5;
            }
            5 => {
                assert!(!window.get_cache_trigger_focused());
                key(&window, Key::Escape);
                assert!(!window.get_cache_visible());
                assert!(window.get_cache_trigger_focused());
                stage = 6;
            }
            _ => {
                fs::write(output.join("result.json"), format!("{{\"status\":\"PASS\",\"input\":\"WindowEvent key/pointer\",\"native_winit_software\":true,\"tab_gestures\":240,\"shift_tab_gestures\":120,\"escape_closures\":2,\"focus_restored\":true,\"background_pointer_blocked\":true,\"ribbon_reachable_while_cache_open\":true,\"os_dpi_certified\":false,\"physical_keyboard\":false,\"screen_reader\":false,\"seconds\":{}}}\n", started.elapsed().as_secs_f64())).unwrap();
                window.hide().unwrap();
                slint::quit_event_loop().unwrap();
            }
        }
    });
    slint::run_event_loop().unwrap();
    timer.stop();
}
