//! VIEW-002/003/008/009: the persisted choice, selector and displayed layout
//! share the same mode, including startup and SQLite source transitions.
use super::*;

fn assert_mode(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>, mode: i32) {
    assert_eq!(controller.borrow().preferences.treemap_mode, mode as u8);
    assert_eq!(window.get_treemap_mode(), mode);
    assert_eq!(window.get_treemap_selector_index(), mode);
}

fn render(window: &SortSizeWindow, adapter: &MinimalSoftwareWindow) {
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1200, 760));
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    });
}

fn layout_pixels(
    window: &SortSizeWindow,
    adapter: &MinimalSoftwareWindow,
    flipped_mosaic: bool,
) -> Vec<u8> {
    // Complete the first layout before installing the controlled tile model;
    // initialization callbacks must not invalidate this rendering fixture.
    render(window, adapter);
    let tiles = (0..3)
        .map(|index| MapTile {
            node_id: index,
            color_index: index,
            name: format!("folder-{index}").into(),
            size: "1 KiB".into(),
            directory: true,
            // Three horizontal mosaic rows, three vertical bands. Changing
            // only mosaic coordinates must have no effect in Bands mode.
            x: 0.,
            y: if flipped_mosaic { 2 - index } else { index } as f32 / 3.,
            w: 1.,
            h: 1. / 3.,
            band_x: index as f32 / 3.,
            band_w: 1. / 3.,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    window.set_tiles(Rc::new(VecModel::from(tiles)).into());
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1200, 760));
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    }));
    assert_eq!(window.get_tiles().row_count(), 3);
    // Compare only tile fill pixels, excluding the selector, labels and other
    // chrome: a changed selector text alone cannot satisfy the layout test.
    let mask = pixels
        .into_iter()
        .map(|pixel| match (pixel.r, pixel.g, pixel.b) {
            (0x23, 0x65, 0x8a) => 1,
            (0x38, 0x6b, 0x47) => 2,
            (0x70, 0x4e, 0x89) => 3,
            _ => 0,
        })
        .collect::<Vec<_>>();
    assert!(
        mask.iter().filter(|value| **value != 0).count() > 10_000,
        "map={}x{}, mode={}, tiles={}",
        window.get_map_width(),
        window.get_map_height(),
        window.get_treemap_mode(),
        window.get_tiles().row_count()
    );
    mask
}

#[test]
fn treemap_divider_resizes_both_panes_and_keeps_a_list_in_a_small_window() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};

    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    render(&window, &adapter);
    let initial_map = window.get_map_height();
    let initial_list = window.get_detail_viewport_height();
    let from = slint::LogicalPosition::new(
        window.get_treemap_divider_x() + 60.,
        window.get_treemap_divider_y() + 5.,
    );
    // Shrinking has room at 1200 × 760 even when system fonts enlarge the
    // surrounding controls and limit the amount of available growth.
    let to = slint::LogicalPosition::new(from.x, from.y - 70.);
    for event in [
        WindowEvent::PointerMoved { position: from },
        WindowEvent::PointerPressed {
            position: from,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerMoved { position: to },
        WindowEvent::PointerReleased {
            position: to,
            button: PointerEventButton::Left,
        },
    ] {
        window.window().dispatch_event(event);
    }
    let shrunk = window.get_map_height();
    assert!((initial_map - shrunk - 70.).abs() < 2.);
    assert!((window.get_detail_viewport_height() - initial_list - 70.).abs() < 2.);
    assert!((f32::from(controller.borrow().preferences.treemap_height) - shrunk).abs() < 1.);
    let key = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    key(Key::DownArrow);
    assert!((window.get_map_height() - shrunk - 20.).abs() < 2.);
    key(Key::End);
    assert!(window.get_detail_viewport_height() >= 34.);
    key(Key::Return);
    assert!((window.get_map_height() - initial_map).abs() < 1.);
    assert_eq!(controller.borrow().preferences.treemap_height, 0);

    window.invoke_set_treemap_height(600);
    adapter.set_size(slint::PhysicalSize::new(960, 600));
    slint::platform::update_timers_and_animations();
    assert!(window.get_map_height() < 1.);
    window.set_treemap_expanded(true);
    assert!(window.get_map_height() >= 80.);
    assert!(window.get_map_height() < 600.);
    assert!(window.get_detail_viewport_height() >= 34.);
    assert_eq!(window.get_treemap_height_choice(), 600);
    window.hide().unwrap();
}

#[test]
fn rendered_mosaic_and_bands_follow_the_default_and_restored_explicit_choice() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("display-v1.conf");
    let (mut window, mut controller) =
        new_session(preferences_store::load(&path).0, Some("en_GB".into())).unwrap();
    let initial = layout_pixels(&window, &adapter, false);
    assert_mode(&window, &controller, 0);
    let mosaic_flipped = layout_pixels(&window, &adapter, true);
    assert_ne!(
        initial, mosaic_flipped,
        "Mosaic must use mosaic coordinates"
    );
    for mode in [1, 0] {
        controller.borrow_mut().preference_store =
            Some(preferences_store::Store::start(path.clone()).unwrap());
        window.invoke_set_treemap_mode(mode);
        let pixels = layout_pixels(&window, &adapter, false);
        let flipped = layout_pixels(&window, &adapter, true);
        if mode == 1 {
            assert_ne!(pixels, initial, "Bands must visibly differ from Mosaic");
            assert_eq!(pixels, flipped, "Bands must ignore mosaic coordinates");
        } else {
            assert_eq!(pixels, initial, "explicit Mosaic must match first startup");
            assert_ne!(pixels, flipped);
        }
        controller.borrow_mut().preference_store = None;
        // The test platform has one adapter. Close the old component before
        // attaching the restart, then keep that restart for the next choice.
        window.hide().unwrap();
        drop(window);
        drop(controller);
        (window, controller) =
            new_session(preferences_store::load(&path).0, Some("en_GB".into())).unwrap();
        assert_eq!(layout_pixels(&window, &adapter, false), pixels);
        assert_mode(&window, &controller, mode);
    }
    window.hide().unwrap();
}

#[test]
fn mosaic_first_start_legacy_settings_and_both_explicit_choices_reopen_consistently() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("sortsize/display-v1.conf");
    let (preferences, status) = preferences_store::load(&path);
    assert_eq!(status, 0);
    let (window, controller) = new_session(preferences, Some("en_GB".into())).unwrap();
    assert_mode(&window, &controller, 0);
    render(&window, &adapter);
    assert_mode(&window, &controller, 0);
    assert!(
        !path.exists(),
        "first startup must not create a preference file"
    );
    window.hide().unwrap();

    for original in [
        "version=1\nunits=decimal\nhidden_columns=owner\n",
        "version=2\nunits=binary\nhidden_columns=\nlanguage=en\n",
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, original).unwrap();
        let (window, controller) =
            new_session(preferences_store::load(&path).0, Some("en_GB".into())).unwrap();
        assert_mode(&window, &controller, 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        for mode in [1, 0, 1, 0] {
            controller.borrow_mut().preference_store =
                Some(preferences_store::Store::start(path.clone()).unwrap());
            window.invoke_set_treemap_mode(mode);
            render(&window, &adapter);
            assert_mode(&window, &controller, mode);
            for language in [2, 1] {
                window.invoke_set_language(language);
                render(&window, &adapter);
                assert_mode(&window, &controller, mode);
            }
            controller.borrow_mut().preference_store = None;
            let (loaded, status) = preferences_store::load(&path);
            assert_eq!(status, 0);
            assert_eq!(loaded.treemap_mode, mode as u8);
            let bytes = fs::read(&path).unwrap();
            let (restart, state) = new_session(loaded, Some("en_GB".into())).unwrap();
            render(&restart, &adapter);
            assert_mode(&restart, &state, mode);
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "reopen must not rewrite settings"
            );
            restart.hide().unwrap();
        }
        window.hide().unwrap();
    }
}

#[test]
fn explicit_view_survives_sqlite_scan_folder_navigation_and_volume_switch() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    let host = crate::workshop::connect(&window, &controller).unwrap();
    window.invoke_memory_select(1);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        host.borrow_mut().poll(&window, &controller.borrow());
        if sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == sortsize_core::inventory_contract::MemoryMode::Entries50k
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "explicit resource profile not applied"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let directory = TempDir::new().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    for source in [&first, &second] {
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("root-file"), [0; 9]).unwrap();
        fs::write(source.join("nested/child"), [0; 3]).unwrap();
    }
    let mut discovery = volume_gui_tests::discovered(&[first, second]);
    discovery.volumes[1].availability = sortsize_core::volumes::VolumeAvailability::Available;
    volume_gui_tests::inject_discovery(&controller, discovery);
    controller.borrow().show_sources(&window);
    for mode in [1, 0] {
        window.invoke_set_treemap_mode(mode);
        for volume in [0, 1] {
            window.invoke_workshop_source(window.get_storage_token(), volume);
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                host.borrow_mut().poll(&window, &controller.borrow());
                assert_mode(&window, &controller, mode);
                assert!(window.get_errors().is_empty(), "{}", window.get_errors());
                if !window.get_busy() && window.get_rows().row_count() > 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "SQLite source scan timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            let nested = window
                .get_rows()
                .iter()
                .find(|row| row.name == "nested")
                .unwrap();
            let token = window.get_workshop_token();
            window.invoke_select_node(nested.node_id);
            while window.get_workshop_token() == token {
                host.borrow_mut().poll(&window, &controller.borrow());
                assert_mode(&window, &controller, mode);
                assert!(Instant::now() < deadline, "folder navigation timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(window.get_current_path().ends_with("nested"));
            let token = window.get_workshop_token();
            window.invoke_go_parent();
            while window.get_workshop_token() == token {
                host.borrow_mut().poll(&window, &controller.borrow());
                assert_mode(&window, &controller, mode);
                assert!(Instant::now() < deadline, "parent navigation timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!window.get_current_path().ends_with("nested"));
        }
    }
}
