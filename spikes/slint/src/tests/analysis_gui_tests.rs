use super::*;
#[test]
fn analysis_reuses_inventory_and_reveals_native_identity() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("été.txt"), b"12345").unwrap();
    fs::write(fixture.path().join("other.bin"), b"123").unwrap();
    fs::create_dir(fixture.path().join("nested")).unwrap();
    fs::write(fixture.path().join("nested/deep.txt"), b"deep").unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    let generation = controller.borrow().generation;
    let fingerprint = controller.borrow().tree.as_ref().unwrap().raw_fingerprint();
    window.invoke_show_analysis();
    let panel = controller.borrow().analysis.test_window().unwrap();
    let poll = || {
        let start = Instant::now();
        while controller.borrow().analysis.test_busy() {
            controller.borrow_mut().poll(&window);
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    poll();
    // The analysis must continue to work after every source file disappears.
    fs::remove_file(fixture.path().join("été.txt")).unwrap();
    fs::remove_file(fixture.path().join("other.bin")).unwrap();
    fs::remove_file(fixture.path().join("nested/deep.txt")).unwrap();
    fs::remove_dir(fixture.path().join("nested")).unwrap();
    panel.set_extension_filter("txt".into());
    panel.invoke_search();
    poll();
    assert_eq!(panel.get_results().row_count(), 2);
    let deep = panel
        .get_results()
        .iter()
        .find(|r| r.label.contains("deep.txt"))
        .unwrap();

    let row = panel
        .get_results()
        .iter()
        .find(|r| r.label.contains("été.txt"))
        .unwrap();
    assert!(row.label.contains("été.txt"));
    assert!(row.size.contains('5'));
    assert_eq!(controller.borrow().generation, generation);
    assert_eq!(
        controller.borrow().tree.as_ref().unwrap().raw_fingerprint(),
        fingerprint
    );
    panel.invoke_reveal(deep.id);
    assert!(window.get_current_path().ends_with("nested"));
    assert!(window.get_entry_path().ends_with("deep.txt"));
    panel.invoke_reveal(row.id);
    assert!(window.get_entry_path().contains("été.txt"));
    let other = TempDir::new().unwrap();
    fs::write(other.path().join("different.txt"), b"other").unwrap();
    let inventory = sortsize_core::scan_inventory(
        other.path(),
        &Default::default(),
        &Default::default(),
        50_000,
        |_| {},
    )
    .unwrap();
    // A complementary view changes controller generation but does not replace
    // the source inventory. Reveal must restore the retained primary tree.
    let other_tree = Tree::new(inventory).0;
    {
        let mut state = controller.borrow_mut();
        state.directory.original = state.tree.take();
        state.tree = Some(other_tree);
        state.generation += 1;
    }
    panel.invoke_reveal(deep.id);
    assert!(window.get_entry_path().ends_with("deep.txt"));
    assert!(controller.borrow().directory.original.is_none());
    let inventory = sortsize_core::scan_inventory(
        other.path(),
        &Default::default(),
        &Default::default(),
        50_000,
        |_| {},
    )
    .unwrap();
    controller.borrow_mut().tree = Some(Tree::new(inventory).0);
    window.set_entry_path("keep this selection".into());
    panel.invoke_reveal(row.id);
    assert_eq!(window.get_entry_path(), "keep this selection");
    assert_eq!(panel.get_status(), "Unavailable");
    panel.set_min_filter("bad".into());
    panel.invoke_search();
    assert!(panel.get_status().contains("Invalid"));
    assert!(!panel.get_busy());
    panel.hide().unwrap();
}

/// Opt-in Winit event-loop recipe. Inputs are injected through public callbacks;
/// this is distinct from physical keyboard/mouse and human screen-reader review.
#[test]
#[ignore = "native Winit; requires BUNNY_V04_NATIVE_OUTPUT and a display"]
fn native_v04_analysis_seven_languages() {
    use std::io::Write;
    let output = std::path::PathBuf::from(std::env::var_os("BUNNY_V04_NATIVE_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for name in ["été-a.txt", "日本語-b.txt"] {
        fs::write(fixture.path().join(name), b"same exact bytes").unwrap();
    }
    fs::write(fixture.path().join("different.bin"), b"other").unwrap();
    window.set_product_version(env!("CARGO_PKG_VERSION").into());
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.show().unwrap();
    window.invoke_start_scan();
    let timer = Timer::default();
    let weak = window.as_weak();
    let state = controller.clone();
    let mut phase = 0;
    let mut case = 0usize;
    let start = Instant::now();
    let mut records = vec![];
    timer.start(TimerMode::Repeated,Duration::from_millis(150),move||{
        assert!(start.elapsed()<Duration::from_secs(90),"native recipe timed out");
        let window=weak.upgrade().unwrap();state.borrow_mut().poll(&window);
        if window.get_busy()||state.borrow().analysis.test_busy(){return;}
        if phase==0 {window.invoke_show_analysis();phase=1;return;}
        let panel=state.borrow().analysis.test_window().unwrap();
        if phase==1 {panel.set_extension_filter("txt".into());panel.invoke_search();phase=2;return;}
        if phase==2 {assert_eq!(panel.get_results().row_count(),2);panel.invoke_duplicates();phase=3;return;}
        if phase==3 {assert_eq!(panel.get_results().row_count(),2);assert!(panel.get_status().contains("Exact groups: 1"));phase=4;return;}
        if case==crate::locale::Language::ALL.len()*6 {fs::write(output.join("records.json"),format!("[{}]\n",records.join(",\n"))).unwrap();panel.hide().unwrap();window.hide().unwrap();slint::quit_event_loop().unwrap();return;}
        let language=crate::locale::Language::ALL[case/6];let(width,height)=if case%6<2{(1000,650)}else if case%6<4{(780,520)}else{(1600,950)};
        if case.is_multiple_of(2){window.invoke_set_language((case/6+1)as i32);panel.window().set_size(slint::LogicalSize::new(width as f32,height as f32));panel.set_statistics_visible((2..4).contains(&(case%6)));}
        else {
            assert!(panel.get_result_viewport_height()>=80.);let pixels=panel.window().take_snapshot().unwrap();assert_eq!((pixels.width(),pixels.height()),(width,height));let mut file=fs::File::create(output.join(format!("{}-{}x{}.ppm",language.as_str(),width,height))).unwrap();write!(file,"P6\n{} {}\n255\n",pixels.width(),pixels.height()).unwrap();for p in pixels.as_slice(){file.write_all(&[p.r,p.g,p.b]).unwrap();}
            assert_eq!(panel.get_results().row_count(),2);assert!(panel.get_results().row_data(0).unwrap().label.contains("été")||panel.get_results().row_data(0).unwrap().label.contains("日本語"));
            records.push(format!("{{\"language\":\"{}\",\"width\":{},\"height\":{},\"rows\":2,\"bytes\":32,\"backend\":\"winit/software\",\"input\":\"injected callbacks\"}}",language.as_str(),pixels.width(),pixels.height()));
        }case+=1;
    });
    slint::run_event_loop().unwrap();
    timer.stop();
}

fn poll_analysis(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
    let start = Instant::now();
    while controller.borrow().analysis.test_busy() {
        controller.borrow_mut().poll(window);
        assert!(start.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn analysis_pages_sort_selection_and_case_are_inventory_only() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for i in 0..2_105 {
        fs::write(
            fixture.path().join(format!("file-{i:04}.TXT")),
            vec![0; i % 20],
        )
        .unwrap();
    }
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    let generation = controller.borrow().generation;
    window.invoke_show_analysis();
    let panel = controller.borrow().analysis.test_window().unwrap();
    poll_analysis(&window, &controller);
    panel.set_extension_filter("txt".into());
    panel.invoke_search();
    poll_analysis(&window, &controller);
    // Remove the entire fixture. Page, sort, filter, and reveal must use memory.
    fixture.close().unwrap();
    assert_eq!(panel.get_results().row_count(), 200);
    assert!(panel.get_page_label().contains("2105") || panel.get_page_label().contains("2,105"));
    for _ in 0..10 {
        panel.invoke_change_page(true);
    }
    assert_eq!(panel.get_results().row_count(), 105);
    assert!(!panel.get_has_next());
    assert!(panel.get_has_previous());
    let selected = panel.get_results().row_data(104).unwrap();
    panel.invoke_reveal(selected.id);
    assert!(window.get_entry_path().ends_with("file-2104.TXT"));
    assert_eq!(window.get_inspected_node(), selected.id);
    assert_eq!(panel.get_selected_id(), selected.id);
    panel.set_sort_order(2);
    panel.invoke_change_sort();
    assert!(panel.get_results().iter().any(|r| r.id == selected.id));
    panel.invoke_search();
    poll_analysis(&window, &controller);
    assert_eq!(panel.get_selected_id(), selected.id);
    panel.set_case_sensitive(true);
    panel.invoke_search();
    poll_analysis(&window, &controller);
    assert_eq!(panel.get_results().row_count(), 0);
    assert_eq!(panel.get_selected_id(), -1);
    assert_eq!(controller.borrow().generation, generation);
    panel.hide().unwrap();
}

#[test]
fn analysis_duplicate_pages_explain_copies_aliases_and_final_verification() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for i in 0..2_105 {
        fs::write(fixture.path().join(format!("copy-{i:04}.txt")), b"same").unwrap();
    }
    fs::hard_link(
        fixture.path().join("copy-0000.txt"),
        fixture.path().join("alias.txt"),
    )
    .unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    window.invoke_show_analysis();
    let panel = controller.borrow().analysis.test_window().unwrap();
    poll_analysis(&window, &controller);
    panel.set_extension_filter("txt".into());
    panel.invoke_duplicates();
    poll_analysis(&window, &controller);
    assert!(panel.get_duplicates_visible());
    assert!(panel.get_status().contains("Exact groups: 1"));
    assert!(panel.get_status().contains("hardlink aliases: 1"));
    assert_eq!(panel.get_results().row_count(), 200);
    assert!(
        panel
            .get_results()
            .iter()
            .any(|r| r.label.contains("Hardlink alias"))
    );
    for _ in 0..10 {
        panel.invoke_change_page(true);
    }
    assert_eq!(panel.get_results().row_count(), 106);
    let last = panel.get_results().row_data(105).unwrap();
    assert!(last.label.ends_with("copy-2104.txt"));
    panel.invoke_reveal(last.id);
    assert!(
        panel
            .get_result_note()
            .contains("independent copies: 2,105")
    );
    assert!(
        panel
            .get_result_note()
            .contains("0967115f2813a3541eaef77de9d9d5773f1c0c04314b0bbfe4ff3b3b1c55b5d5")
    );
    assert!(panel.get_result_note().contains("byte-for-byte verified"));
    panel.hide().unwrap();
}

#[test]
fn analysis_invalid_filters_never_leave_actionable_stale_results() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("a.txt"), b"same").unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    window.invoke_show_analysis();
    let panel = controller.borrow().analysis.test_window().unwrap();
    poll_analysis(&window, &controller);
    assert!(panel.get_results().row_count() > 0);
    for (min, max, glob, reason) in [
        ("-1", "", "", "whole bytes"),
        ("2", "1", "", "minimum size"),
        ("", "", "[", "glob"),
    ] {
        panel.set_min_filter(min.into());
        panel.set_max_filter(max.into());
        panel.set_glob_filter(glob.into());
        panel.invoke_search();
        assert!(!panel.get_busy());
        assert!(panel.get_status().contains(reason));
        assert_eq!(panel.get_results().row_count(), 0);
        assert_eq!(panel.get_statistics().row_count(), 0);
        assert_eq!(panel.get_selected_id(), -1);
    }
    panel.hide().unwrap();
}

#[test]
#[ignore = "native Winit pointer/key interactions; requires BUNNY_V04_NATIVE_OUTPUT and a display"]
fn native_v04_pointer_keyboard_flow() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("BUNNY_V04_NATIVE_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("nested")).unwrap();
    for i in 0..250 {
        fs::write(
            fixture.path().join(format!("nested/copy-{i:03}.txt")),
            b"same",
        )
        .unwrap();
    }
    fs::write(fixture.path().join("other.bin"), b"different").unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.show().unwrap();
    window.invoke_start_scan();
    let click = |panel: &AnalysisWindow, x, y| {
        let position = slint::LogicalPosition::new(x, y);
        panel.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        panel.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    };
    let timer = Timer::default();
    let weak = window.as_weak();
    let state = controller.clone();
    let mut phase = 0;
    let start = Instant::now();
    timer.start(TimerMode::Repeated, Duration::from_millis(120), move || {
        assert!(start.elapsed() < Duration::from_secs(60));
        let window = weak.upgrade().unwrap(); state.borrow_mut().poll(&window);
        if window.get_busy() || state.borrow().analysis.test_busy() { return; }
        if phase == 0 { window.invoke_show_analysis(); phase = 1; return; }
        let panel = state.borrow().analysis.test_window().unwrap();
        if phase == 1 {
            panel.window().set_size(slint::LogicalSize::new(780., 520.)); phase = 2; return;
        }
        if phase == 2 {
            click(&panel, panel.get_name_input_x() + 20., panel.get_name_input_y() + 16.);
            for ch in "copy".chars().map(|c| c.to_string()).chain(std::iter::once(slint::SharedString::from(Key::Return).to_string())) {
                panel.window().dispatch_event(WindowEvent::KeyPressed { text: ch.clone().into() });
                panel.window().dispatch_event(WindowEvent::KeyReleased { text: ch.into() });
            }
            phase = 3; return;
        }
        if phase == 3 {
            assert_eq!(panel.get_name_filter(), "copy"); assert_eq!(panel.get_results().row_count(), 200);
            assert!(panel.get_has_next());
            click(&panel, panel.get_next_button_x() + 20., panel.get_next_button_y() + 15.);
            phase = 4; return;
        }
        if phase == 4 {
            assert_eq!(panel.get_results().row_count(), 50);
            click(&panel, panel.get_result_list_x() + 20., panel.get_result_list_y() + 17.);
            phase = 5; return;
        }
        assert!(panel.get_selected_id() >= 0);
        assert!(window.get_current_path().ends_with("nested"));
        assert!(window.get_entry_path().ends_with("copy-200.txt"));
        assert!(panel.get_result_viewport_height() >= 80.);
        fs::write(output.join("pointer-keyboard.txt"), format!("input=WindowEvent pointer + individual keys + Return\nfilter={}\npage={}\nselection={}\npath={}\nviewport_height={}\n", panel.get_name_filter(), panel.get_page_label(), panel.get_selected_id(), window.get_entry_path(), panel.get_result_viewport_height())).unwrap();
        panel.hide().unwrap(); window.hide().unwrap(); slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop().unwrap();
    timer.stop();
}

#[test]
fn analysis_language_changes_preserve_page_selection_and_translate_errors() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for i in 0..250 {
        fs::write(fixture.path().join(format!("copy-{i:03}.txt")), b"same").unwrap();
    }
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    window.invoke_show_analysis();
    let panel = controller.borrow().analysis.test_window().unwrap();
    poll_analysis(&window, &controller);
    panel.invoke_change_page(true);
    let selected = panel.get_results().row_data(1).unwrap().id;
    panel.invoke_reveal(selected);
    fixture.close().unwrap(); // Any further search or presentation must be inventory-only.
    let generation = controller.borrow().generation;
    for (index, language) in crate::locale::Language::ALL.iter().enumerate() {
        window.invoke_set_language((index + 1) as i32);
        assert_eq!(panel.get_selected_id(), selected);
        assert_eq!(panel.get_results().row_data(1).unwrap().id, selected);
        let context = controller.borrow().presentation;
        let _scope = context.enter();
        assert_eq!(context.language, *language);
        assert_eq!(
            panel.get_statistics().row_data(0).unwrap().label,
            tr!("All matched files")
        );
        assert!(panel.get_scope_note().contains(tr!("Retained inventory")));
    }
    panel.set_glob_filter("[".into());
    panel.invoke_search();
    for (index, _) in crate::locale::Language::ALL.iter().enumerate() {
        window.invoke_set_language((index + 1) as i32);
        let context = controller.borrow().presentation;
        let _scope = context.enter();
        assert_eq!(
            panel.get_status(),
            tr!("Invalid filter: check the glob and the limit of 4096 bytes per field.")
        );
    }
    assert_eq!(controller.borrow().generation, generation);
    panel.hide().unwrap();
}

#[test]
#[ignore = "native Winit idle sample; requires BUNNY_V04_NATIVE_OUTPUT and a display"]
fn native_v04_idle_panel() {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("BUNNY_V04_NATIVE_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_US".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for name in ["a.txt", "b.txt"] {
        fs::write(fixture.path().join(name), b"same").unwrap();
    }
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.show().unwrap();
    window.invoke_start_scan();
    let timer = Timer::default();
    let state = controller.clone();
    let weak = window.as_weak();
    let mut phase = 0;
    let started = Instant::now();
    // Same poll interval as the product. No redraw request during the sample.
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        assert!(started.elapsed() < Duration::from_secs(30));
        let window = weak.upgrade().unwrap();
        state.borrow_mut().poll(&window);
        if window.get_busy() || state.borrow().analysis.test_busy() {
            return;
        }
        if phase == 0 {
            window.invoke_show_analysis();
            phase = 1;
            return;
        }
        if phase == 1 {
            phase = 2;
            let panel = state.borrow().analysis.test_window().unwrap();
            fs::write(
                output.join("idle-ready"),
                b"analysis panel open; retained inventory ready; 150ms product poll",
            )
            .unwrap();
            slint::Timer::single_shot(Duration::from_secs(10), move || {
                panel.hide().unwrap();
                window.hide().unwrap();
                slint::quit_event_loop().unwrap();
            });
        }
    });
    slint::run_event_loop().unwrap();
    timer.stop();
}
