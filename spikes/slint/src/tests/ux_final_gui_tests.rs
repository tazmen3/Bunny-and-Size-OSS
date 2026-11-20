//! Focused V0.5 UX checks against allocated geometry and real pointer dispatch.
use super::*;
use slint::platform::{PointerEventButton, WindowEvent};
use std::{cell::Cell, io::Write};

fn render_frame(
    window: &SortSizeWindow,
    adapter: &MinimalSoftwareWindow,
    size: (u32, u32),
    name: &str,
) {
    let (width, height) = size;
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    adapter.set_size(slint::PhysicalSize::new(width, height));
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
    }));
    if let Some(destination) = std::env::var_os("BUNNY_UX_OUTPUT") {
        fs::create_dir_all(&destination).unwrap();
        let path = std::path::PathBuf::from(destination).join(format!("{name}.ppm"));
        let mut output = fs::File::create(path).unwrap();
        output
            .write_all(format!("P6\n{width} {height}\n255\n").as_bytes())
            .unwrap();
        for pixel in pixels {
            output.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
        }
    }
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

#[test]
fn navigation_initially_visible_and_empty_save_row_does_not_stretch() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_storage_enabled(true);
    window.set_scan_fast_available(true);
    window.set_scan_fast_can_save(false);
    for size in [(800, 760), (1600, 1200)] {
        render_frame(&window, &adapter, size, "navigation-layout");
        assert!(window.get_actual_sidebar_width() >= 220.);
        assert!(
            (window.get_workspace_top() - window.get_commands_bottom()).abs() < 2.,
            "empty save controls must not consume workspace height"
        );
    }
    render_frame(&window, &adapter, (800, 760), "navigation-toggle");
    click(
        &window,
        window.get_folders_x() + 10.,
        window.get_folders_y() + 10.,
    );
    render_frame(&window, &adapter, (800, 760), "navigation-collapsed");
    assert!(window.get_actual_sidebar_width() < 1.);
    render_frame(&window, &adapter, (1600, 900), "navigation-wide-collapsed");
    click(
        &window,
        window.get_folders_x() + 10.,
        window.get_folders_y() + 10.,
    );
    render_frame(&window, &adapter, (1600, 900), "navigation-wide-reopened");
    render_frame(&window, &adapter, (800, 760), "navigation-reopened");
    assert!(window.get_actual_sidebar_width() >= 220.);
    let weak = window.as_weak();
    window.on_close_search(move || weak.upgrade().unwrap().set_search_visible(false));
    window.set_search_visible(true);
    render_frame(&window, &adapter, (800, 760), "navigation-search-narrow");
    assert!(window.get_actual_sidebar_width() < 1.);
    render_frame(&window, &adapter, (1600, 900), "navigation-search-wide");
    assert!(window.get_actual_sidebar_width() >= 220.);
    render_frame(
        &window,
        &adapter,
        (800, 760),
        "navigation-search-narrow-again",
    );
    click(
        &window,
        window.get_folders_x() + 10.,
        window.get_folders_y() + 10.,
    );
    render_frame(&window, &adapter, (800, 760), "navigation-search-return");
    assert!(!window.get_search_visible());
    assert!(window.get_actual_sidebar_width() >= 220.);
}

#[test]
fn ux_final_layout_search_cache_and_menus() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_search_token(1);
    window.set_cache_token(1);
    window.set_search_scope("/home/example/Documents".into());
    window.set_search_status("24".into());
    window.set_search_results(slint::ModelRc::new(slint::VecModel::from(
        (0..24)
            .map(|index| AnalysisRow {
                id: index,
                label: format!("/home/example/Documents/measurement-{index:02}.txt").into(),
                size: "1 KiB".into(),
                fraction: 0.5,
            })
            .collect::<Vec<_>>(),
    )));
    window.set_cache_labels(slint::ModelRc::new(slint::VecModel::from(vec![
        "Documents - 2026-11-01 - 24 files".into(),
        "Pictures - 2026-10-31 - 42 files".into(),
        "Archive - 2026-10-30 - 16 files".into(),
    ])));

    for (language, choice) in [("en", 1), ("fr", 2), ("de", 3)] {
        window.invoke_set_language(choice);
        for theme in [1, 2] {
            window.invoke_set_theme(theme);
            let mut previous_results_height = None;
            for size @ (width, height) in [(800, 600), (1200, 760), (1600, 900)] {
                let context = format!("{language}-theme{theme}-{width}x{height}");
                window.set_cache_visible(false);
                window.set_search_visible(true);
                window.set_search_statistics_visible(false);
                window.set_ribbon_tab(1);
                render_frame(&window, &adapter, size, &format!("search-{context}"));

                let controls_top = window.get_search_controls_y();
                let controls_height = window.get_search_controls_height();
                let controls_content = window.get_search_controls_content_height();
                let results_top = window.get_search_results_y();
                let results_height = window.get_search_results_height();
                let pagination_top = window.get_search_pagination_y();
                let pagination_height = window.get_search_pagination_height();
                eprintln!(
                    "{context}: controls={controls_top}+{controls_height}/{controls_content}, results={results_top}+{results_height}, pagination={pagination_top}+{pagination_height}, two-columns={}",
                    window.get_search_two_columns()
                );
                assert!(
                    controls_top >= window.get_workspace_top(),
                    "search controls cover the ribbon: {context}"
                );
                assert!(
                    results_top >= controls_top + controls_height - 1.,
                    "search controls overlap results: {context}"
                );
                assert!(results_height >= 35., "results collapsed: {context}");
                assert!(
                    pagination_top >= results_top + results_height - 1.,
                    "pagination overlaps results: {context}"
                );
                assert!(pagination_height > 0., "pagination collapsed: {context}");
                assert!(
                    pagination_top + pagination_height <= height as f32,
                    "pagination outside window: {context}"
                );
                if width >= 1200 {
                    assert!(window.get_search_two_columns(), "{context}");
                    assert!(
                        controls_height + 1. >= controls_content,
                        "unnecessary search control scrolling: {context}"
                    );
                }
                if let Some(previous) = previous_results_height {
                    assert!(
                        results_height > previous,
                        "results did not use additional window space: {context}"
                    );
                }
                previous_results_height = Some(results_height);

                window.set_search_visible(false);
                window.set_cache_visible(true);
                render_frame(&window, &adapter, size, &format!("saved-{context}"));
                let panel_top = window.get_cache_panel_top();
                let panel_height = window.get_cache_panel_height();
                let actions_top = window.get_cache_actions_top();
                let actions_height = window.get_cache_actions_height();
                let list_height = window.get_cache_list_height();
                eprintln!(
                    "{context}: saved={panel_top}+{panel_height}, list={list_height}, actions={actions_top}+{actions_height}"
                );
                assert!(
                    panel_top >= window.get_workspace_top() - 1.,
                    "saved measures cover navigation: {context}"
                );
                assert!(
                    panel_top >= window.get_display_y() + window.get_display_height(),
                    "saved measures cover tabs: {context}"
                );
                assert!(panel_height > 100., "saved measures collapsed: {context}");
                assert!(
                    panel_top + panel_height <= height as f32,
                    "saved measures outside window: {context}"
                );
                assert!(list_height >= 64., "saved list collapsed: {context}");
                assert!(
                    list_height < panel_height / 2.,
                    "saved list leaves no room for actions: {context}"
                );
                assert!(
                    actions_top >= panel_top + list_height,
                    "saved actions overlap inventory list: {context}"
                );
                assert!(
                    actions_height > 40. && actions_height < 200.,
                    "saved action rows have disproportionate height: {context}"
                );

                // The click must traverse real hit testing while Saved measures
                // is visible; invoking the callback cannot detect an overlay.
                click(
                    &window,
                    window.get_display_x() + window.get_display_width() / 2.,
                    window.get_display_y() + window.get_display_height() / 2.,
                );
                render_frame(&window, &adapter, size, &format!("saved-display-{context}"));
                assert_eq!(window.get_ribbon_tab(), 2, "tab click blocked: {context}");
                assert!(window.get_display_controls_visible());
                assert!(!window.get_navigation_controls_visible());
                assert!(window.get_cache_visible(), "saved panel lost on tab change");
                assert!(
                    window.get_cache_panel_top() >= window.get_workspace_top() - 1.,
                    "saved measures overlap expanded menu: {context}"
                );
                let expanded_top = window.get_workspace_top();
                click(
                    &window,
                    window.get_display_x() + window.get_display_width() / 2.,
                    window.get_display_y() + window.get_display_height() / 2.,
                );
                render_frame(
                    &window,
                    &adapter,
                    size,
                    &format!("saved-collapsed-{context}"),
                );
                assert_eq!(window.get_ribbon_tab(), -1, "tab did not close: {context}");
                assert!(window.get_workspace_top() < expanded_top);
                assert!(window.get_cache_visible());
                assert!(!window.get_display_controls_visible());
            }
        }
    }
    assert!(controller.borrow().task.is_none());
    assert!(!window.get_busy());
    window.hide().unwrap();
}

#[test]
fn duplicate_content_limits_keep_both_inputs_clear_of_the_wrapped_guidance() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_ribbon_tab(1);
    window.set_search_visible(true);
    window.set_duplicate_mode(3);
    window.set_duplicate_content_limits_supported(true);
    window.set_duplicate_content_limits_expanded(true);
    window.set_duplicate_content_total_bytes("1073741824".into());
    window.set_duplicate_content_file_bytes("536870912".into());
    window.set_duplicate_details_expanded(true);
    window.set_duplicate_details("One current-folder result".into());

    for language in 1..=7 {
        window.invoke_set_language(language);
        for size in [(1202, 791), (780, 520)] {
            let context = format!(
                "duplicate-content-limits-language{language}-{}x{}",
                size.0, size.1
            );
            render_frame(&window, &adapter, size, &context);
            let inputs_bottom = window.get_duplicate_content_inputs_bottom();
            let guidance_top = window.get_duplicate_content_guidance_y();
            let controls_bottom =
                window.get_search_controls_y() + window.get_search_controls_height();
            assert!(
                guidance_top >= inputs_bottom + 2.,
                "guidance overlaps the inputs: {context}, inputs end {inputs_bottom}, guidance starts {guidance_top}"
            );
            if size.0 == 1202 {
                assert!(
                    inputs_bottom <= controls_bottom,
                    "inputs clipped by the viewport: {context}, inputs end {inputs_bottom}, viewport ends {controls_bottom}"
                );
            }
        }
    }
}

#[test]
fn duplicate_ab_controls_and_pagination_fit_seven_languages_without_content_action() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_ribbon_tab(-1);
    window.set_search_visible(true);
    window.set_duplicate_mode(1);
    window.set_duplicate_source_labels(slint::ModelRc::new(slint::VecModel::from(vec![
        "A - Documents".into(),
        "B - SMB archive".into(),
    ])));
    window.set_duplicate_source_a(0);
    window.set_duplicate_source_b(1);
    window.set_duplicate_source_a_info(
        "A: /home/example/Documents\n2026-11-05 10:00 · complete · 24 files\nSource identity is logical; different mounts may still be aliases.".into(),
    );
    window.set_duplicate_source_b_info(
        "B: smb://server/archive\n2026-11-05 10:01 · partial · 18 files\nSource identity is logical; different mounts may still be aliases.".into(),
    );
    window.set_duplicate_ready(true);
    window.set_duplicate_reveal_allowed(false);
    window.set_search_scope("A: Documents · /home/example/Documents\n/home/example/Documents · G1 · complete · 2026-11-05 10:00\nB: SMB archive · smb://server/archive\nsmb://server/archive · G1 · partial · 2026-11-05 10:01".into());
    window.set_search_status("Ready".into());
    window.set_search_duplicates_visible(true);
    window.set_search_statistics("Effective duplicate budgets: 1073741824 bytes read; 1073741824 bytes per file; 1000 errors; 536870912 bytes temporary state per inventory/query; 64 roots maximum.\nPossible source aliases remain unverified. Results are incomplete.".into());
    window.set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![
        AnalysisRow {
            id: 0,
            label: "1 · A · /home/example/Documents/report.txt · verified".into(),
            size: "1 KiB".into(),
            fraction: 1.,
        },
        AnalysisRow {
            id: 1,
            label: "1 · B · smb://server/archive/renamed.txt · verified".into(),
            size: "1 KiB".into(),
            fraction: 1.,
        },
    ])));
    let content_actions = Rc::new(Cell::new(0));
    let calls = content_actions.clone();
    window.on_duplicate_run(move || calls.set(calls.get() + 1));
    for (language, choice) in [
        ("en", 1),
        ("fr", 2),
        ("de", 3),
        ("es", 4),
        ("pt", 5),
        ("it", 6),
        ("nl", 7),
    ] {
        window.invoke_set_language(choice);
        for theme in [1, 2] {
            window.invoke_set_theme(theme);
            for size @ (width, height) in [(780, 520), (1200, 760)] {
                let context = format!("duplicates-ab-{language}-theme{theme}-{width}x{height}");
                render_frame(&window, &adapter, size, &context);
                let controls_top = window.get_search_controls_y();
                let controls_height = window.get_search_controls_height();
                let results_top = window.get_search_results_y();
                let results_height = window.get_search_results_height();
                let pagination_top = window.get_search_pagination_y();
                let pagination_height = window.get_search_pagination_height();
                assert!(controls_top >= window.get_workspace_top(), "{context}");
                assert!(controls_height >= 48., "{context}");
                assert!(
                    results_top >= controls_top + controls_height - 1.,
                    "{context}"
                );
                assert!(results_height >= 35., "results collapsed: {context}");
                assert!(
                    pagination_top >= results_top + results_height - 1.,
                    "{context}"
                );
                assert!(
                    pagination_height >= 32.,
                    "{context}: pagination={pagination_height}"
                );
                assert!(
                    pagination_top + pagination_height <= height as f32,
                    "{context}"
                );
                assert_eq!(
                    content_actions.get(),
                    0,
                    "presentation read content: {context}"
                );
            }
        }
    }
    // The explicit action stays outside the scrolling preparation area.
    window.invoke_set_language(1);
    window.set_ribbon_tab(-1);
    render_frame(
        &window,
        &adapter,
        (780, 520),
        "duplicates-ab-minimum-before-scroll",
    );
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(
                window.get_search_results_x() + 100.,
                window.get_search_controls_y() + window.get_search_controls_height() - 15.,
            ),
            delta_x: 0.,
            delta_y: -10000.,
        });
    render_frame(
        &window,
        &adapter,
        (780, 520),
        "duplicates-ab-minimum-scrolled",
    );
    assert!(
        window.get_search_duplicates_y()
            >= window.get_search_controls_y() + window.get_search_controls_height() - 1.
    );
    assert!(window.get_search_duplicates_y() + 32. <= window.get_search_results_y());
    click(
        &window,
        window.get_search_duplicates_x() + 20.,
        window.get_search_duplicates_y() + 10.,
    );
    assert_eq!(content_actions.get(), 1);
    // Resize back to preferred size: hit testing must remain correct there.
    render_frame(&window, &adapter, (1200, 760), "duplicates-ab-ready");
    click(
        &window,
        window.get_search_duplicates_x() + 20.,
        window.get_search_duplicates_y() + 10.,
    );
    assert_eq!(content_actions.get(), 2);
    assert!(controller.borrow().task.is_none());
    assert!(!window.get_busy());
    window.hide().unwrap();
}

#[test]
fn duplicate_ab_unverified_source_diagnostics_remain_visible_and_incomplete() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_ribbon_tab(-1);
    window.set_search_visible(true);
    window.set_duplicate_mode(1);
    window.set_duplicate_source_labels(slint::ModelRc::new(slint::VecModel::from(vec![
        "A - Local documents".into(),
        "B - GVfs SMB archive".into(),
    ])));
    window.set_duplicate_source_a(0);
    window.set_duplicate_source_b(1);
    window.set_duplicate_ready(true);
    window.set_duplicate_reveal_allowed(false);
    window.set_search_duplicates_visible(true);
    window.set_search_complete(false);
    window.set_search_busy(false);
    window.set_search_matches("1".into());
    let content_actions = Rc::new(Cell::new(0));
    let calls = content_actions.clone();
    window.on_duplicate_run(move || calls.set(calls.get() + 1));
    for (language, choice) in [
        ("en", 1),
        ("fr", 2),
        ("de", 3),
        ("es", 4),
        ("pt", 5),
        ("it", 6),
        ("nl", 7),
    ] {
        window.invoke_set_language(choice);
        let _scope = controller.borrow().presentation.enter();
        let incomplete = tr!("Incomplete coverage");
        let warning = tr!(
            "Source continuity is unverified. Current file contents can be compared, but inventory coverage and independent copies are not proven."
        );
        window.set_search_status(incomplete.into());
        window.set_duplicate_source_a_info("A · /home/example/Documents · G1".into());
        window.set_duplicate_source_b_info(
            format!("B · smb://server/archive · G1\n{}", tr!("Unknown")).into(),
        );
        window.set_search_scope("A: /home/example/Documents\nB: smb://server/archive".into());
        window.set_search_statistics(
            format!("{}\n{warning}", tr_format!("Unverified sources: {}.", 1)).into(),
        );
        window.set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![
            AnalysisRow {
                id: 0,
                label: format!(
                    "A · {} {} · /home/example/Documents/report.txt",
                    tr_format!("Group {} · location {} ·", 1, 1),
                    tr!("Verified exact")
                )
                .into(),
                size: "1 KiB".into(),
                fraction: 1.,
            },
            AnalysisRow {
                id: 1,
                label: format!(
                    "B · {} {} · smb://server/archive/renamed.txt",
                    tr_format!("Group {} · location {} ·", 1, 2),
                    tr!("Verified exact")
                )
                .into(),
                size: "1 KiB".into(),
                fraction: 1.,
            },
        ])));
        for size @ (width, height) in [(780, 520), (1200, 760)] {
            render_frame(
                &window,
                &adapter,
                size,
                &format!("duplicates-unverified-{language}-{width}x{height}"),
            );
            assert!(!window.get_search_complete(), "{language}");
            assert_eq!(window.get_search_status(), incomplete);
            assert!(window.get_search_statistics().contains(warning));
            assert_eq!(window.get_search_results().row_count(), 2);
            assert!(window.get_search_results_height() >= 35., "{language}");
            assert!(window.get_search_pagination_height() >= 32., "{language}");
            assert!(
                window.get_search_pagination_y() + window.get_search_pagination_height()
                    <= height as f32,
                "{language}"
            );
            assert_eq!(
                content_actions.get(),
                0,
                "rendering read content: {language}"
            );
        }
    }
    assert!(controller.borrow().task.is_none());
    assert!(!window.get_busy());
    window.hide().unwrap();
}

#[test]
fn protected_reference_review_controls_seven_languages() {
    use slint::{Model, ModelRc, VecModel};
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_ribbon_tab(-1);
    window.set_search_visible(true);
    window.set_duplicate_mode(1);
    window.set_duplicate_settings_expanded(false);
    window.set_search_duplicates_visible(true);
    window.set_search_token(1);
    window.set_duplicate_scope_summary(
        Rc::new(VecModel::from(vec![
            "/lab/original".into(),
            "/lab/copy".into(),
        ]))
        .into(),
    );
    window.set_duplicate_summary("1 folder group · 1240 files".into());
    window.set_duplicate_folder_view(false);
    window.set_search_results(ModelRc::new(VecModel::from(vec![
        AnalysisRow {
            id: 0,
            label: "A original".into(),
            size: "40 B".into(),
            fraction: 0.,
        },
        AnalysisRow {
            id: 1,
            label: "B copy".into(),
            size: "40 B".into(),
            fraction: 0.,
        },
    ])));
    let rows = Rc::new(VecModel::from(vec![
        crate::DuplicateRow {
            group: "1".into(),
            side: "A".into(),
            name: "original.txt".into(),
            location: "/lab/original".into(),
            size: "40 B".into(),
            verification: "Verified exact".into(),
            selectable: false,
            selected: false,
            partial: false,
            selection_label: "Protected A: /lab/original/original.txt".into(),
            protected: true,
            group_start: true,
            role_label: "Protected reference".into(),
            ..Default::default()
        },
        crate::DuplicateRow {
            group: "1".into(),
            side: "B".into(),
            name: "copy.txt".into(),
            location: "/lab/copy".into(),
            size: "40 B".into(),
            verification: "Verified exact".into(),
            selectable: true,
            selected: false,
            partial: false,
            selection_label: "For review B: /lab/copy/copy.txt".into(),
            copy: true,
            role_label: "Copy".into(),
            ..Default::default()
        },
    ]));
    window.set_duplicate_rows(rows.clone().into());
    let weak = window.as_weak();
    window.on_search_select(move |_token, index, _reveal| {
        weak.upgrade().unwrap().set_search_selected(index);
    });
    let content = Rc::new(Cell::new(0));
    let calls = content.clone();
    window.on_duplicate_run(move || calls.set(calls.get() + 1));
    let toggles = Rc::new(Cell::new(0));
    let calls = toggles.clone();
    let model = rows.clone();
    window.on_duplicate_toggle_copy(move |index, selected| {
        assert_eq!(index, 1, "protected row dispatched a toggle");
        calls.set(calls.get() + 1);
        let mut row = model.row_data(1).unwrap();
        row.selected = selected;
        model.set_row_data(1, row);
    });
    let previews = Rc::new(Cell::new(0));
    let calls = previews.clone();
    let weak = window.as_weak();
    window.on_duplicate_selection_preview(move || {
        calls.set(calls.get() + 1);
        weak.upgrade()
            .unwrap()
            .set_duplicate_selection_preview_open(true);
    });
    let clears = Rc::new(Cell::new(0));
    let calls = clears.clone();
    window.on_duplicate_selection_clear(move || calls.set(calls.get() + 1));
    let applies = Rc::new(Cell::new(0));
    let calls = applies.clone();
    let weak = window.as_weak();
    window.on_duplicate_selection_apply(move || {
        calls.set(calls.get() + 1);
        weak.upgrade()
            .unwrap()
            .set_duplicate_selection_preview_open(false);
    });
    let dismisses = Rc::new(Cell::new(0));
    let calls = dismisses.clone();
    let weak = window.as_weak();
    window.on_duplicate_selection_dismiss(move || {
        calls.set(calls.get() + 1);
        weak.upgrade()
            .unwrap()
            .set_duplicate_selection_preview_open(false);
    });
    for (language, choice) in [
        ("en", 1),
        ("fr", 2),
        ("de", 3),
        ("es", 4),
        ("pt", 5),
        ("it", 6),
        ("nl", 7),
    ] {
        window.invoke_set_language(choice);
        window.set_duplicate_protected_reference(0);
        window.set_duplicate_selection_ready(true);
        window.set_duplicate_selection_summary(
            tr_format!(
                "Selected for review: {} files · {} folders · {} logical.",
                "0",
                "0",
                "0 B"
            )
            .into(),
        );
        window.set_duplicate_selection_preview_text(format!("{}\n{}", tr!("All eligible results in this comparison, including pages not displayed. Logical size is not recoverable space."), tr!("Selection uses the recorded comparison evidence; compare explicitly again to refresh it.")).into());
        for size @ (width, height) in [(780, 520), (1200, 760), (1920, 1080)] {
            window.set_duplicate_selection_preview_open(false);
            render_frame(
                &window,
                &adapter,
                size,
                &format!("protected-reference-{language}-{width}x{height}"),
            );
            assert!(
                window.get_search_results_height() >= 136.,
                "results collapsed: {language} {width}"
            );
            assert!(
                window.get_search_pagination_y() + window.get_search_pagination_height()
                    <= height as f32,
                "pagination outside window: {language} {width}"
            );
            assert!(window.get_duplicate_protected_reference_x() >= 0.);
            for y in [
                window.get_duplicate_protected_reference_y(),
                window.get_duplicate_selection_preview_y(),
                window.get_duplicate_selection_clear_y(),
            ] {
                assert!(
                    y >= window.get_workspace_top() && y + 30. <= window.get_search_results_y(),
                    "review controls outside allocated space: {language} {width}"
                );
            }
            assert!(
                window.get_duplicate_selection_clear_x() + 20. < width as f32,
                "review actions outside window: {language} {width}"
            );
            let before = previews.get();
            click(
                &window,
                window.get_duplicate_selection_preview_x() + 20.,
                window.get_duplicate_selection_preview_y() + 12.,
            );
            assert_eq!(
                previews.get(),
                before + 1,
                "preview click: {language} {width}"
            );
            render_frame(
                &window,
                &adapter,
                size,
                &format!("protected-reference-preview-{language}-{width}x{height}"),
            );
            assert!(
                window.get_search_results_height() >= 80.,
                "preview collapsed results: {language} {width}"
            );
            assert!(
                window.get_search_pagination_y() + window.get_search_pagination_height()
                    <= height as f32,
                "preview pagination outside window: {language} {width}"
            );
            for (x, y, button_width) in [
                (
                    window.get_duplicate_selection_apply_x(),
                    window.get_duplicate_selection_apply_y(),
                    window.get_duplicate_selection_apply_width(),
                ),
                (
                    window.get_duplicate_selection_dismiss_x(),
                    window.get_duplicate_selection_dismiss_y(),
                    window.get_duplicate_selection_dismiss_width(),
                ),
            ] {
                assert!(
                    x >= 0. && x + button_width <= width as f32,
                    "preview action outside horizontal bounds: {language} {width}"
                );
                assert!(
                    y >= window.get_workspace_top() && y + 32. <= window.get_search_results_y(),
                    "preview action outside vertical bounds: {language} {width}"
                );
            }
            let before = applies.get();
            click(
                &window,
                window.get_duplicate_selection_apply_x() + 20.,
                window.get_duplicate_selection_apply_y() + 12.,
            );
            assert_eq!(applies.get(), before + 1, "apply click: {language} {width}");
            assert!(!window.get_duplicate_selection_preview_open());
            window.set_duplicate_selection_preview_open(true);
            render_frame(
                &window,
                &adapter,
                size,
                &format!("protected-reference-cancel-{language}-{width}x{height}"),
            );
            let before = dismisses.get();
            click(
                &window,
                window.get_duplicate_selection_dismiss_x() + 20.,
                window.get_duplicate_selection_dismiss_y() + 12.,
            );
            assert_eq!(
                dismisses.get(),
                before + 1,
                "cancel click: {language} {width}"
            );
            assert!(!window.get_duplicate_selection_preview_open());
            render_frame(
                &window,
                &adapter,
                size,
                &format!("protected-reference-ready-{language}-{width}x{height}"),
            );
            let before = clears.get();
            click(
                &window,
                window.get_duplicate_selection_clear_x() + 20.,
                window.get_duplicate_selection_clear_y() + 12.,
            );
            assert_eq!(clears.get(), before + 1, "clear click: {language} {width}");
            let before = toggles.get();
            click(
                &window,
                window.get_search_results_x() + 12.,
                window.get_search_results_y() + 17.,
            );
            assert_eq!(
                toggles.get(),
                before,
                "protected reference checkbox: {language}"
            );
            click(
                &window,
                window.get_search_results_x() + 12.,
                window.get_search_results_y() + 51.,
            );
            assert_eq!(
                toggles.get(),
                before + 1,
                "copy checkbox: {language} {width}"
            );
            window
                .window()
                .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
            window
                .window()
                .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
            assert_eq!(
                toggles.get(),
                before + 2,
                "copy checkbox keyboard: {language} {width}"
            );
            assert_eq!(
                content.get(),
                0,
                "review triggered comparison: {language} {width}"
            );
        }
    }
    assert!(controller.borrow().task.is_none());
    window.hide().unwrap();
}

#[test]
fn duplicate_abc_rows_and_primary_action_fit_normal_and_narrow_windows_without_scrolling() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.set_workshop_live(true);
    window.set_ribbon_tab(-1);
    window.set_search_visible(true);
    window.set_duplicate_mode(2);
    window.set_duplicate_settings_expanded(true);
    window.set_duplicate_ready(true);
    window.set_duplicate_scope_a("D:/corpus/A".into());
    window.set_duplicate_scope_b("D:/corpus/B".into());
    window.set_duplicate_scope_c("D:/corpus/C".into());
    for language in 1..=7 {
        window.invoke_set_language(language);
        for size @ (width, height) in [(2048, 1100), (1200, 760), (960, 680)] {
            render_frame(&window, &adapter, size, "compact-abc-folders");
            assert!(window.get_duplicate_folder_c_y() >= window.get_search_controls_y());
            assert!(
                window.get_duplicate_folder_c_y() + window.get_duplicate_folder_c_height()
                    <= window.get_search_duplicates_y(),
                "C hidden: language={language}, width={width}"
            );
            assert!(window.get_search_duplicates_y() + 32. <= height as f32);
            assert!(window.get_search_results_height() >= 35.);
        }
    }
    assert!(controller.borrow().task.is_none());
    assert!(!window.get_busy());
    window.hide().unwrap();
}
