//! VIEW-008/009/010/012: actual pointer and keyboard input through the ribbon.
use super::*;
use slint::platform::{Key, PointerEventButton, WindowEvent};

fn render(window: &SortSizeWindow, adapter: &MinimalSoftwareWindow, width: u32) {
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    adapter.set_size(slint::PhysicalSize::new(width, 760));
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); width as usize * 760];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
    }));
}

fn click(window: &SortSizeWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
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
}

fn key(window: &SortSizeWindow, key: Key) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

fn display_click(window: &SortSizeWindow) {
    click(
        window,
        window.get_display_x() + window.get_display_width() / 2.,
        window.get_display_y() + window.get_display_height() / 2.,
    );
}

#[test]
fn ribbon_tabs_toggle_hide_previous_commands_and_folders_remain_independent() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, state) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    for width in [1200, 780] {
        window.set_ribbon_tab(0);
        render(&window, &adapter, width);
        assert!(window.get_navigation_controls_visible());
        let navigation_top = window.get_workspace_top();
        assert!(navigation_top < 300.);

        display_click(&window);
        render(&window, &adapter, width);
        assert_eq!(window.get_ribbon_tab(), 2);
        assert!(window.get_display_controls_visible());
        assert!(!window.get_navigation_controls_visible());
        let expanded_top = window.get_workspace_top();
        let old_compact = (
            window.get_compact_x() + window.get_compact_width() / 2.,
            window.get_compact_y() + window.get_compact_height() / 2.,
        );
        display_click(&window);
        render(&window, &adapter, width);
        assert_eq!(window.get_ribbon_tab(), -1);
        assert!(!window.get_display_controls_visible());
        assert!(window.get_display_tab_focused());
        let collapsed_top = window.get_workspace_top();
        assert!(collapsed_top < expanded_top);
        assert!(window.get_detail_viewport_height() > 100.);
        let dense = window.get_dense();
        click(&window, old_compact.0, old_compact.1);
        eprintln!(
            "after hidden click width={width} ribbon={} preferences={} cache={} display=({},{},{},{}) old-compact=({},{}), workspace={}",
            window.get_ribbon_tab(),
            window.get_preferences_open(),
            window.get_cache_visible(),
            window.get_display_x(),
            window.get_display_y(),
            window.get_display_width(),
            window.get_display_height(),
            old_compact.0,
            old_compact.1,
            window.get_workspace_top()
        );
        assert_eq!(window.get_ribbon_tab(), -1);
        assert!(!window.get_preferences_open());
        let workspace_selector_at_old_compact = old_compact.0 >= window.get_treemap_selector_x()
            && old_compact.0
                <= window.get_treemap_selector_x() + window.get_treemap_selector_width()
            && old_compact.1 >= window.get_treemap_selector_y()
            && old_compact.1
                <= window.get_treemap_selector_y() + window.get_treemap_selector_height();
        eprintln!(
            "workspace selector at old Compact={workspace_selector_at_old_compact}; geometry=({},{},{},{})",
            window.get_treemap_selector_x(),
            window.get_treemap_selector_y(),
            window.get_treemap_selector_width(),
            window.get_treemap_selector_height()
        );
        assert_eq!(
            window.get_dense(),
            dense,
            "hidden Compact retained its hitbox"
        );
        // The workspace moves up when the ribbon is closed. This coordinate
        // may now belong to a visible workspace selector; dismiss its popup
        // before beginning the next independent ribbon gesture.
        key(&window, Key::Escape);

        display_click(&window);
        render(&window, &adapter, width);
        assert_eq!(window.get_ribbon_tab(), 2);
        assert!((window.get_workspace_top() - expanded_top).abs() < 1.);
        let collapsed = window.get_sidebar_collapsed();
        click(
            &window,
            window.get_folders_x() + window.get_folders_width() / 2.,
            window.get_folders_y() + window.get_folders_height() / 2.,
        );
        render(&window, &adapter, width);
        assert_ne!(window.get_sidebar_collapsed(), collapsed);
        assert_eq!(
            window.get_ribbon_tab(),
            2,
            "Folders changed the command tab"
        );
        assert!((window.get_workspace_top() - expanded_top).abs() < 1.);

        window.set_preferences_open(true);
        window.invoke_activate_ribbon(1);
        render(&window, &adapter, width);
        assert_eq!(window.get_ribbon_tab(), 1);
        assert!(
            !window.get_preferences_open(),
            "previous tab kept its modal open"
        );
        assert!(!window.get_display_controls_visible());
        click(
            &window,
            window.get_navigation_x() + window.get_navigation_width() / 2.,
            window.get_navigation_y() + window.get_navigation_height() / 2.,
        );
        render(&window, &adapter, width);
        assert_eq!(window.get_ribbon_tab(), 0);
        assert!(window.get_navigation_controls_visible());
        assert!(window.get_navigation_tab_focused());
        assert!((window.get_workspace_top() - navigation_top).abs() < 1.);
        assert!(state.borrow().task.is_none());
        assert!(!window.get_busy());
    }
    window.hide().unwrap();
}

#[test]
fn theme_compact_and_units_hitboxes_are_bounded_separate_and_keyboard_operable() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, state) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_ribbon_tab(2);
    for (language, width) in (1..=7).flat_map(|language| [1200, 780].map(|width| (language, width)))
    {
        window.invoke_set_language(language);
        window.invoke_set_theme(0);
        window.invoke_set_density(false);
        render(&window, &adapter, width);
        assert_eq!(window.get_theme_choice(), 0);
        assert_eq!(window.get_theme_selector_index(), 0);
        assert_eq!(window.get_size_unit_choice(), 0);
        assert_eq!(window.get_size_unit_selector_index(), 0);
        assert!(!window.get_dense());
        assert!(!window.get_compact_checked());
        assert!(window.get_theme_width() >= 90. && window.get_theme_width() <= 200.);
        assert!(window.get_compact_width() > 30. && window.get_compact_width() <= 180.);
        assert!(window.get_theme_x() >= 0. && window.get_theme_height() > 0.);
        assert!(window.get_compact_x() + window.get_compact_width() <= width as f32);
        assert!(
            window.get_compact_x() >= window.get_theme_x() + window.get_theme_width() + 6.,
            "theme and Compact hitboxes overlap or lack spacing"
        );
        let dense = window.get_dense();
        click(
            &window,
            window.get_theme_x() + window.get_theme_width() + 3.,
            window.get_theme_y() + window.get_theme_height() / 2.,
        );
        assert_eq!(window.get_dense(), dense, "spacing clicked Compact");
        let previous_theme = window.get_theme_choice();
        click(
            &window,
            window.get_compact_x() + window.get_compact_width() / 2.,
            window.get_compact_y() + window.get_compact_height() / 2.,
        );
        render(&window, &adapter, width);
        assert_eq!(window.get_dense(), !dense);
        assert_eq!(window.get_compact_checked(), !dense);
        assert!(
            window.get_compact_focused(),
            "Compact click did not receive focus"
        );
        assert_eq!(window.get_theme_choice(), previous_theme);
        key(&window, Key::Space);
        render(&window, &adapter, width);
        assert_eq!(
            window.get_dense(),
            dense,
            "keyboard could not toggle Compact"
        );
        assert_eq!(window.get_compact_checked(), dense);
        click(
            &window,
            window.get_theme_x() + window.get_theme_width() / 2.,
            window.get_theme_y() + window.get_theme_height() / 2.,
        );
        key(&window, Key::DownArrow);
        key(&window, Key::Return);
        render(&window, &adapter, width);
        assert_eq!(window.get_theme_choice(), previous_theme + 1);
        assert_eq!(window.get_theme_selector_index(), previous_theme + 1);
        assert_eq!(window.get_dense(), dense, "theme selection clicked Compact");

        assert!(window.get_size_unit_width() >= 90. && window.get_size_unit_width() <= 200.);
        assert!(window.get_size_unit_x() >= 0. && window.get_size_unit_height() > 0.);
        assert!(window.get_size_unit_x() + window.get_size_unit_width() <= width as f32);
        assert!(window.get_size_unit_y() >= window.get_theme_y() + window.get_theme_height());
        assert_eq!(window.get_size_unit_choice(), 0);
        click(
            &window,
            window.get_size_unit_x() + window.get_size_unit_width() / 2.,
            window.get_size_unit_y() + window.get_size_unit_height() / 2.,
        );
        key(&window, Key::DownArrow);
        key(&window, Key::Return);
        render(&window, &adapter, width);
        assert_eq!(window.get_size_unit_choice(), 1);
        assert_eq!(window.get_size_unit_selector_index(), 1);
        assert_eq!(state.borrow().preferences.units.size_unit(), 1);
        assert_eq!(window.get_dense(), dense);
        assert_eq!(window.get_compact_checked(), dense);
        assert_eq!(window.get_theme_choice(), previous_theme + 1);
        assert_eq!(window.get_theme_selector_index(), previous_theme + 1);
        // Changing the base updates labels without losing the forced exponent
        // or leaving the visible selector behind its controller preference.
        for decimal in [true, false] {
            window.invoke_set_units(decimal);
            render(&window, &adapter, width);
            assert_eq!(window.get_decimal_units(), decimal);
            assert_eq!(window.get_size_unit_choice(), 1);
            assert_eq!(window.get_size_unit_selector_index(), 1);
            assert_eq!(state.borrow().preferences.units.size_unit(), 1);
            assert_eq!(window.get_theme_selector_index(), previous_theme + 1);
            assert_eq!(window.get_compact_checked(), dense);
        }
        assert!(state.borrow().task.is_none());
        assert!(!window.get_busy());
        window.invoke_set_size_unit(0);
        assert_eq!(window.get_size_unit_choice(), 0);
        assert_eq!(window.get_size_unit_selector_index(), 0);
    }
    window.hide().unwrap();
}

/// WIN-01 / VIEW-008: inspect the actual embedded icon pixels, independently
/// of system font coverage, while exercising the existing shared tab input.
#[test]
fn ribbon_vector_indicators_render_right_closed_down_open_for_all_five_tabs() {
    use std::{cell::Cell, time::Duration};

    struct RibbonPlatform {
        adapter: Rc<MinimalSoftwareWindow>,
        clock: Rc<Cell<Duration>>,
    }
    impl Platform for RibbonPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.adapter.clone())
        }
        fn duration_since_start(&self) -> Duration {
            self.clock.get()
        }
    }

    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let clock = Rc::new(Cell::new(Duration::ZERO));
    slint::platform::set_platform(Box::new(RibbonPlatform {
        adapter: adapter.clone(),
        clock: clock.clone(),
    }))
    .unwrap();
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    let width = 1200u32;
    let height = 760u32;
    adapter.set_size(slint::PhysicalSize::new(width, height));
    let bounds = |window: &SortSizeWindow| {
        [
            (
                window.get_navigation_x(),
                window.get_navigation_y(),
                window.get_navigation_width(),
                window.get_navigation_height(),
            ),
            (
                window.get_analysis_x(),
                window.get_analysis_y(),
                window.get_analysis_width(),
                window.get_analysis_height(),
            ),
            (
                window.get_display_x(),
                window.get_display_y(),
                window.get_display_width(),
                window.get_display_height(),
            ),
            (
                window.get_extensions_x(),
                window.get_extensions_y(),
                window.get_extensions_width(),
                window.get_extensions_height(),
            ),
            (
                window.get_help_x(),
                window.get_help_y(),
                window.get_help_width(),
                window.get_help_height(),
            ),
        ]
    };
    let check = |theme: i32, selected: i32| {
        // Keep hover away from the icon crop. Each button retains its real
        // checked/focus background, sampled independently for pixel contrast.
        window.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(0., 0.),
        });
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        }));
        // Evaluate the first frame before advancing only this test's clock,
        // so pending transitions settle before inspecting the icon/background
        // pair. Product animations and input behavior remain unchanged.
        clock.set(clock.get() + Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        }));
        if let Some(directory) = std::env::var_os("BUNNY_RIBBON_CAPTURE_DIR") {
            use std::io::Write;
            fs::create_dir_all(&directory).unwrap();
            let path = std::path::Path::new(&directory)
                .join(format!("ribbon-theme-{theme}-open-{selected}.ppm"));
            let mut output = std::io::BufWriter::new(fs::File::create(path).unwrap());
            write!(output, "P6\n{width} {height}\n255\n").unwrap();
            for pixel in &pixels {
                output.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
            }
        }
        for (tab, (x, y, tab_width, tab_height)) in bounds(&window).into_iter().enumerate() {
            assert!(tab_width > 24. && tab_height > 10.);
            // RibbonTab shares the 10px icon geometry on every platform,
            // before the translated label. The surrounding transparent
            // margin supplies that tab's background color.
            let left = (x + 10.).round() as usize;
            let top = (y + (tab_height - 10.) / 2.).round() as usize;
            let background = pixels[top * width as usize + left];
            let mut ink = Vec::new();
            for row in 0..10usize {
                for column in 0..10usize {
                    let pixel = pixels[(top + row) * width as usize + left + column];
                    let contrast = u16::from(pixel.r.abs_diff(background.r))
                        + u16::from(pixel.g.abs_diff(background.g))
                        + u16::from(pixel.b.abs_diff(background.b));
                    if contrast > 100 {
                        ink.push((column, row));
                    }
                }
            }
            assert!(
                ink.len() >= 8,
                "theme={theme} tab={tab}: indicator is invisible"
            );
            let extent = |axis: usize| {
                let coordinate = |point: &(usize, usize)| if axis == 0 { point.0 } else { point.1 };
                ink.iter().map(coordinate).max().unwrap()
                    - ink.iter().map(coordinate).min().unwrap()
                    + 1
            };
            let (ink_width, ink_height) = (extent(0), extent(1));
            if tab as i32 == selected {
                assert!(
                    ink_width > ink_height,
                    "theme={theme} tab={tab}: open indicator must point down ({ink_width}x{ink_height})"
                );
            } else {
                assert!(
                    ink_height > ink_width,
                    "theme={theme} tab={tab}: closed indicator must point right ({ink_width}x{ink_height})"
                );
            }
            let axis = usize::from(tab as i32 == selected);
            let coordinates: Vec<_> = ink
                .iter()
                .map(|point| if axis == 0 { point.0 } else { point.1 })
                .collect();
            let middle_twice =
                coordinates.iter().min().unwrap() + coordinates.iter().max().unwrap();
            assert!(
                coordinates.iter().sum::<usize>() * 2 < coordinates.len() * middle_twice,
                "theme={theme} tab={tab}: triangle points in the wrong direction"
            );
        }
    };
    for theme in [1, 2] {
        window.invoke_set_theme(theme);
        window.set_ribbon_tab(-1);
        check(theme, -1);
        for tab in 0..5 {
            let (x, y, tab_width, tab_height) = bounds(&window)[tab];
            click(&window, x + tab_width / 2., y + tab_height / 2.);
            assert_eq!(window.get_ribbon_tab(), tab as i32);
            check(theme, tab as i32);
            key(&window, Key::Return);
            assert_eq!(
                window.get_ribbon_tab(),
                -1,
                "focused tab must close with Return"
            );
            check(theme, -1);
        }
    }
    assert!(controller.borrow().task.is_none());
    assert!(!window.get_busy());
    window.hide().unwrap();
}
