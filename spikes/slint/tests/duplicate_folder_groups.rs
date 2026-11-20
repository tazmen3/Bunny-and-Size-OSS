//! #193/#194: production Slint disclosure actions, independent from live scans.
use slint::{
    ComponentHandle, Model, VecModel,
    platform::{
        Key, PointerEventButton, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    AnalysisRow, DuplicateRow, SortSizeWindow,
    locale::{Language, activate_bundled},
};
use std::{cell::Cell, rc::Rc};

struct Platform(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Platform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}
fn key(window: &SortSizeWindow, key: Key) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}
fn render(window: &SortSizeWindow, adapter: &MinimalSoftwareWindow, width: u32, height: u32) {
    adapter.set_size(slint::PhysicalSize::new(width, height));
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); width as usize * height as usize];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
    }));
    assert!(window.get_duplicate_results_height() >= 80.);
}

#[test]
fn folder_summary_discloses_with_pointer_and_enter_and_collapses_with_left_arrow() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    window.set_search_visible(true);
    window.set_duplicate_mode(1);
    window.set_duplicate_settings_expanded(false);
    window.set_search_duplicates_visible(true);
    window.set_duplicate_folder_view(true);
    window.set_duplicate_folder_count("1".into());
    window.set_duplicate_file_count("0".into());
    window.set_duplicate_summary("Complete · 1 folder group · 0 file groups".into());
    window.set_duplicate_scope_summary(
        Rc::new(VecModel::from(vec![
            "/lab/first-root".into(),
            "/lab/other-root".into(),
        ]))
        .into(),
    );
    window.set_search_results(
        Rc::new(VecModel::from(vec![AnalysisRow {
            id: 0,
            label: "Exact folder pair".into(),
            size: "24 KiB".into(),
            fraction: 0.,
        }]))
        .into(),
    );
    window.set_duplicate_rows(
        Rc::new(VecModel::from(vec![DuplicateRow {
            group: "1".into(),
            side: "A / B".into(),
            name: "1240 files · first-root".into(),
            location: "A: /lab/first-root\nB: /lab/other-root".into(),
            size: "24 KiB".into(),
            verification: "Verified exact".into(),
            ..Default::default()
        }]))
        .into(),
    );
    let reads = Rc::new(Cell::new(0));
    window.on_duplicate_run({
        let reads = reads.clone();
        move || reads.set(reads.get() + 1)
    });
    window.on_duplicate_inventory({
        let reads = reads.clone();
        move |_| reads.set(reads.get() + 1)
    });
    window.on_start_scan({
        let reads = reads.clone();
        move || reads.set(reads.get() + 1)
    });
    let opens = Rc::new(Cell::new(0));
    window.on_duplicate_open_folder({
        let opens = opens.clone();
        let weak = window.as_weak();
        move |index| {
            assert_eq!(index, 0);
            opens.set(opens.get() + 1);
            let window = weak.upgrade().unwrap();
            window.set_duplicate_folder_view(true);
            window.set_duplicate_folder_detail(true);
        }
    });
    window.on_search_select({
        let weak = window.as_weak();
        move |_, index, _| weak.upgrade().unwrap().set_search_selected(index)
    });
    let closes = Rc::new(Cell::new(0));
    window.on_duplicate_category_changed({
        let weak = window.as_weak();
        let closes = closes.clone();
        move |index| {
            assert_eq!(index, 0);
            closes.set(closes.get() + 1);
            weak.upgrade().unwrap().set_duplicate_folder_detail(false);
        }
    });
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    for mode in [1, 2] {
        window.set_duplicate_mode(mode);
        window.set_duplicate_protected_reference(if mode == 2 { 2 } else { 0 });
        if mode == 2 {
            window.set_duplicate_scope_summary(
                Rc::new(VecModel::from(vec![
                    "/lab/first-root".into(),
                    "/lab/other-root".into(),
                    "/lab/third-root".into(),
                ]))
                .into(),
            );
            let mut row = window.get_duplicate_rows().iter().next().unwrap();
            row.side = "A / B / C".into();
            row.name = "1240 files · 3 locations".into();
            row.location = "A: /lab/first-root\nB: /lab/other-root\nC: /lab/third-root".into();
            window.set_duplicate_rows(Rc::new(VecModel::from(vec![row])).into());
        }
        for language in Language::ALL {
            activate_bundled(language).unwrap();
            for (width, height) in [(780, 520), (1200, 760), (1920, 1080)] {
                render(&window, &adapter, width, height);
            }
        }
    }
    window.set_duplicate_mode(2);
    window.set_duplicate_scope_c("/lab/third-root".into());
    window.set_duplicate_settings_expanded(true);
    for language in Language::ALL {
        activate_bundled(language).unwrap();
        for (width, height) in [(780, 520), (1200, 760), (1920, 1080)] {
            render(&window, &adapter, width, height);
        }
    }
    window.set_duplicate_settings_expanded(false);
    activate_bundled(Language::English).unwrap();
    render(&window, &adapter, 1200, 760);
    let position = slint::LogicalPosition::new(
        window.get_search_results_x() + 100.,
        window.get_search_results_y() + 20.,
    );
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
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
    assert_eq!(opens.get(), 1, "pointer opens the rendered folder row");
    assert!(window.get_duplicate_folder_detail());
    key(&window, Key::LeftArrow);
    assert_eq!(closes.get(), 1);
    assert!(!window.get_duplicate_folder_detail());
    key(&window, Key::Home);
    key(&window, Key::Return);
    assert_eq!(opens.get(), 2, "Enter opens the selected folder row");
    key(&window, Key::LeftArrow);
    assert_eq!(closes.get(), 2);
    window.set_duplicate_folder_view(false);
    window.set_duplicate_reference_available(true);
    window.set_duplicate_reference_label("Open folder group 1".into());
    key(&window, Key::Return);
    assert_eq!(
        opens.get(),
        3,
        "a residual file opens its matching folder reference"
    );
    assert_eq!(reads.get(), 0, "presentation grants no source access");
    window.hide().unwrap();
}

#[test]
fn duplicate_file_context_menu_targets_the_clicked_row_without_activating_it() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    window.set_desktop_entry_menu(true);
    window.set_search_visible(true);
    window.set_duplicate_mode(1);
    window.set_duplicate_settings_expanded(false);
    window.set_search_duplicates_visible(true);
    window.set_duplicate_menu_token(5);
    window.set_search_selected(0);
    window.set_duplicate_selection_ready(true);
    window.set_search_results(Rc::new(VecModel::from(vec![AnalysisRow::default(); 2])).into());
    window.set_duplicate_rows(
        Rc::new(VecModel::from(vec![
            DuplicateRow {
                name: "first.txt".into(),
                location: r"C:\Fixtures\A".into(),
                selectable: true,
                selected: true,
                ..Default::default()
            },
            DuplicateRow {
                name: "second.txt".into(),
                location: r"\\server\share\B".into(),
                selectable: true,
                ..Default::default()
            },
        ]))
        .into(),
    );
    window.on_search_select(|_, _, _| panic!("right click must not activate the row"));
    window.on_duplicate_open_folder(|_| panic!("right click must not open a folder group"));
    window.on_duplicate_toggle_copy(|_, _| panic!("right click must not toggle a review box"));
    window.on_duplicate_run(|| panic!("right click must not start a search"));
    window.on_start_scan(|| panic!("right click must not start a scan"));
    let invoked = Rc::new(Cell::new(None));
    window.on_duplicate_entry_menu({
        let invoked = invoked.clone();
        move |token, index, x, y| invoked.set(Some((token, index, x, y)))
    });
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    render(&window, &adapter, 1200, 760);
    let position = slint::LogicalPosition::new(
        window.get_search_results_x() + 440.,
        window.get_search_results_y() + 64.,
    );
    let press = || {
        window
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Right,
        });
    };
    let release = || {
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Right,
            });
    };
    press();
    assert_eq!(invoked.take(), None);
    release();
    let (token, index, x, y) = invoked.take().expect("context menu request");
    assert_eq!((token, index), (5, 1));
    assert!((x - position.x).abs() < 1.);
    assert!((y - position.y).abs() < 1.);
    assert!(window.get_duplicate_rows().row_data(0).unwrap().selected);

    press();
    window.set_duplicate_menu_token(6);
    release();
    assert_eq!(invoked.take(), None, "obsolete page");

    window.set_duplicate_folder_view(true);
    render(&window, &adapter, 1200, 760);
    press();
    release();
    assert_eq!(invoked.take(), None, "folder summary");
    window.hide().unwrap();
}

#[test]
fn duplicate_resource_apply_remains_reachable_in_the_small_workspace() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    window.set_search_visible(true);
    window.set_duplicate_mode(3);
    window.set_duplicate_single_root("C:\\Fixtures".into());
    window.set_duplicate_content_limits_supported(true);
    window.set_duplicate_content_limits_expanded(true);
    window.set_duplicate_resource_expanded(true);
    window.set_duplicate_resource_temporary_parent("D:\\Private workspace".into());
    window.set_duplicate_resource_temporary_custom(true);
    window.set_duplicate_resource_memory_mib("16".into());
    window.set_duplicate_resource_temporary_mib("256".into());
    window.set_duplicate_resource_dirty(true);
    let saves = Rc::new(Cell::new(0));
    window.on_duplicate_resource_apply({
        let saves = saves.clone();
        move || saves.set(saves.get() + 1)
    });
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    for language in Language::ALL {
        activate_bundled(language).unwrap();
        render(&window, &adapter, 780, 520);
        let top = window.get_duplicate_controls_y();
        let bottom = top + window.get_duplicate_controls_height();
        let x = window.get_duplicate_resource_apply_x();
        let y = window.get_duplicate_resource_apply_y();
        if y >= bottom || y <= top {
            window
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(x, (top + bottom) / 2.),
                    delta_x: 0.,
                    delta_y: (top + bottom) / 2. - y,
                });
            render(&window, &adapter, 780, 520);
        }
        let y = window.get_duplicate_resource_apply_y();
        assert!(
            y > top && y < bottom,
            "{language:?} {y} outside {top}..{bottom}"
        );
        let position = slint::LogicalPosition::new(x, y);
        let previous = saves.get();
        window
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
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
        assert_eq!(saves.get(), previous + 1, "{language:?} apply button");
    }
    window.hide().unwrap();
}
