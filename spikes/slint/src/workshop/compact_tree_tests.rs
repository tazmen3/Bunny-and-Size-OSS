//! Small native-service corpus, with Slint's software window. These callback
//! checks do not claim physical mouse/keyboard or screen-reader certification.
use super::*;
use slint::{
    Model,
    platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use std::time::{Duration, Instant};

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[track_caller]
fn until(
    host: &Rc<RefCell<Host>>,
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
    predicate: impl Fn(&Host) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        host.borrow_mut().poll(window, &controller.borrow());
        assert!(
            window.get_errors().is_empty(),
            "{}; {}",
            window.get_errors(),
            test_wait_state(&host.borrow())
        );
        if predicate(&host.borrow()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "compact tree timed out: {} / {}; {}",
            window.get_status(),
            window.get_current_path(),
            test_wait_state(&host.borrow())
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn row(window: &SortSizeWindow, name: &str) -> TreeRow {
    window
        .get_rows()
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("missing tree folder {name}"))
}

fn key(window: &SortSizeWindow, key: slint::platform::Key) {
    use slint::platform::WindowEvent;
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn compact_tree_lazy_expansion_sort_selection_keyboard_and_parent_shares() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    window.invoke_memory_select(1);
    until(&host, &window, &controller, |_| {
        sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == MemoryMode::Entries50k
    });
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let long_name =
        "Photographies au nom particulièrement long pour vérifier la navigation compacte";
    for (name, bytes) in [
        ("big/deep-large", 90),
        ("big/deep-small", 10),
        (long_name, 50),
        ("tie-a", 4),
        ("tie-b", 4),
    ] {
        let directory = source.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("payload"), vec![0; bytes]).unwrap();
    }
    std::fs::create_dir_all(source.join("empty")).unwrap();
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(source.clone())
            .into(),
    );
    window.invoke_start_scan();
    until(&host, &window, &controller, |host| {
        host.current
            .as_ref()
            .is_some_and(|page| !page.busy && page.measure_ready && page.sort_index == 0)
            && window.get_rows().row_count() == 6
    });
    let depth_one: Vec<_> = window
        .get_rows()
        .iter()
        .filter(|row| row.depth == 1)
        .map(|row| row.name.to_string())
        .collect();
    assert_eq!(depth_one, ["big", long_name, "tie-a", "tie-b", "empty"]);
    assert!(window.get_rows().iter().all(|row| row.depth <= 1));
    let big = row(&window, "big");
    assert!(big.expandable && !big.expanded);
    assert!((big.fraction - 100. / 158.).abs() < 0.00001);
    assert!(big.has_weight && !big.weight_provisional);
    assert!(row(&window, long_name).path.contains(long_name));

    // Chevron expansion changes the tree only, retaining the inspected pane.
    let main_path = window.get_current_path();
    let selected = window.get_rows().iter().find(|r| r.selected).unwrap().path;
    let detail_paths: Vec<_> = window.get_details().iter().map(|r| r.path).collect();
    let tiles = window.get_tiles();
    window.invoke_toggle_node(big.node_id);
    until(&host, &window, &controller, |_| {
        window.get_rows().iter().any(|row| row.name == "deep-large")
    });
    assert_eq!(window.get_current_path(), main_path);
    assert_eq!(
        window.get_rows().iter().find(|r| r.selected).unwrap().path,
        selected
    );
    assert_eq!(
        window
            .get_details()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        detail_paths
    );
    assert_eq!(window.get_tiles(), tiles);
    let large = row(&window, "deep-large");
    let small = row(&window, "deep-small");
    assert_eq!((large.depth, small.depth), (2, 2));
    assert!((large.fraction - 0.9).abs() < 0.00001);
    assert!((small.fraction - 0.1).abs() < 0.00001);
    let children: Vec<_> = window
        .get_rows()
        .iter()
        .filter(|r| r.depth == 2)
        .map(|r| r.name)
        .collect();
    assert_eq!(children, ["deep-large", "deep-small"]);

    // Name selection synchronizes the principal pane without closing branches.
    window.invoke_select_node(row(&window, "big").node_id);
    until(&host, &window, &controller, |_| {
        window.get_current_path().ends_with("big") && window.get_details().row_count() == 2
    });
    assert!(row(&window, "big").selected);
    assert!(row(&window, "big").expanded);
    let focused = window.get_rows().iter().find(|r| r.focused).unwrap().path;
    let rows_model = window.get_rows();
    let token = window.get_workshop_token();
    host.borrow_mut().navigate(Navigate::Refresh);
    until(&host, &window, &controller, |_| {
        window.get_workshop_token() != token
    });
    assert!(row(&window, "big").selected && row(&window, "big").expanded);
    assert_eq!(
        window.get_rows().iter().find(|r| r.focused).unwrap().path,
        focused
    );
    assert_eq!(
        window.get_rows(),
        rows_model,
        "refresh patches the existing tree model"
    );

    // Dispatch real Slint pointer/key events through the layout and FocusScope.
    // This proves widget routing; it is still a software window, not OS input.
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let _visibility = TestWindowVisibility::new(&window);
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1200, 900));
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 900];
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    });
    let position = slint::LogicalPosition::new(
        window.get_tree_list_x() + 16. + 14.,
        window.get_tree_list_y()
            + 1.5 * window.get_tree_row_height()
            + window.get_sidebar_folder_scroll(),
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
    until(&host, &window, &controller, |_| {
        !row(&window, "big").expanded
    });
    assert_eq!(
        window.get_current_path().as_str(),
        label(&source.join("big"))
    );
    assert!(window.get_rows().iter().all(|r| r.depth <= 1));
    key(&window, Key::RightArrow);
    until(&host, &window, &controller, |_| {
        row(&window, "big").expanded
    });
    assert!(row(&window, "big").selected);
    key(&window, Key::LeftArrow);
    until(&host, &window, &controller, |_| {
        !row(&window, "big").expanded
    });
    key(&window, Key::RightArrow);
    until(&host, &window, &controller, |_| {
        row(&window, "big").expanded
    });
    key(&window, Key::Home);
    until(&host, &window, &controller, |_| {
        window.get_rows().row_data(0).is_some_and(|r| r.focused)
    });
    key(&window, Key::End);
    until(&host, &window, &controller, |_| {
        window.get_rows().iter().last().is_some_and(|r| r.focused)
    });
    key(&window, Key::UpArrow);
    until(&host, &window, &controller, |_| {
        row(&window, "tie-b").focused
    });
    key(&window, Key::DownArrow);
    until(&host, &window, &controller, |_| {
        row(&window, "empty").focused
    });
    key(&window, Key::Return);
    until(&host, &window, &controller, |_| {
        window.get_current_path().ends_with("empty")
    });
    assert!(row(&window, "empty").selected);
    assert_eq!(window.get_details().row_count(), 0);
    assert_eq!(window.get_summary(), "158 bytes");
    assert_eq!(
        std::fs::metadata(source.join("big/deep-large/payload"))
            .unwrap()
            .len(),
        90
    );
    window.hide().unwrap();
}

#[test]
fn compact_tree_open_branch_and_selection_survive_live_snapshot_replacement() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    window.invoke_memory_select(1);
    until(&host, &window, &controller, |_| {
        sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == MemoryMode::Entries50k
    });
    // A test-only scheduler delay makes the small real corpus observable while
    // running. This is a functional acquisition test, without a timing target.
    // Eight-completion batches now advance more files per tick; leave enough
    // acquisition turns for the independent dated tree to publish its branch.
    host.borrow().service.lane.lock().unwrap().tick_delay = Duration::from_millis(250);
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    for (folder, bytes) in [("left/nested", 11), ("right", 3)] {
        let folder = source.join(folder);
        std::fs::create_dir_all(&folder).unwrap();
        for index in 0..384 {
            std::fs::write(folder.join(format!("file-{index:03}")), vec![0; bytes]).unwrap();
        }
    }
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(source.clone())
            .into(),
    );
    window.invoke_start_scan();
    until(&host, &window, &controller, |_| {
        window.get_busy() && window.get_rows().iter().any(|r| r.name == "left")
    });
    // Continuation controls have no metric; every actual folder gauge is partial.
    assert!(
        window
            .get_rows()
            .iter()
            .filter(|r| !r.proportion.is_empty())
            .all(|r| r.weight_provisional)
    );
    let first_revision = host.borrow().current.as_ref().unwrap().frame.stamp.revision;
    window.invoke_toggle_node(row(&window, "left").node_id);
    until(&host, &window, &controller, |_| {
        window.get_rows().iter().any(|r| r.name == "nested")
    });
    assert!(
        window.get_busy(),
        "the branch is usable before the scan finishes"
    );
    window.invoke_select_node(row(&window, "left").node_id);
    until(&host, &window, &controller, |_| {
        window.get_current_path().ends_with("left")
    });
    assert!(
        window.get_busy(),
        "navigation remains available during acquisition"
    );
    until(&host, &window, &controller, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.frame.stamp.revision != first_revision)
    });
    assert!(row(&window, "left").expanded);
    assert!(row(&window, "left").selected);
    // The main folder can finish before the independently scheduled tree fold.
    // Assert exact proportions only after this branch has consumed that fold.
    until(&host, &window, &controller, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
            && window
                .get_rows()
                .iter()
                .any(|r| r.name == "nested" && !r.weight_provisional)
    });
    assert!(row(&window, "left").expanded && row(&window, "left").selected);
    assert_eq!(row(&window, "nested").depth, 2);
    assert!(row(&window, "nested").has_weight);
    let exact_size = {
        let controller = controller.borrow();
        let _presentation = controller.presentation.enter();
        controller.preferences.units.format(384 * 11)
    };
    assert_eq!(row(&window, "nested").size.as_str(), exact_size);
    assert_eq!(row(&window, "left").size.as_str(), exact_size);
    assert!(
        (row(&window, "nested").fraction - 1.).abs() < f32::EPSILON,
        "nested={:?}; left={:?}; current={:?}",
        row(&window, "nested"),
        row(&window, "left"),
        host.borrow()
            .current
            .as_ref()
            .map(|page| &page.root_measure)
    );
    assert!(!row(&window, "nested").weight_provisional);
    // Main-pane completion may precede the cooperative branch order. Keep
    // polling until its retained partial sibling window converges exactly.
    until(&host, &window, &controller, |_| {
        window
            .get_rows()
            .iter()
            .filter(|r| r.depth == 1)
            .map(|r| r.name)
            .collect::<Vec<_>>()
            == ["left", "right"]
    });
    let root_children: Vec<_> = window
        .get_rows()
        .iter()
        .filter(|r| r.depth == 1)
        .map(|r| r.name)
        .collect();
    assert_eq!(root_children, ["left", "right"]);
    assert_eq!(window.get_details().row_count(), 1);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "nested");
    assert_eq!(
        std::fs::read(source.join("left/nested/file-000")).unwrap(),
        [0; 11]
    );
}

#[test]
fn compact_tree_large_sibling_page_and_table_reveal_remain_independent() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    window.invoke_memory_select(1);
    until(&host, &window, &controller, |_| {
        sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == MemoryMode::Entries50k
    });
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for index in 0..231 {
        std::fs::create_dir(source.join(format!("folder-{index:03}"))).unwrap();
    }
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(source.clone())
            .into(),
    );
    window.invoke_start_scan();
    until(&host, &window, &controller, |host| {
        host.current
            .as_ref()
            .is_some_and(|page| !page.busy && page.measure_ready)
            && window.get_rows().iter().any(|r| r.name == "folder-000")
            && window
                .get_rows()
                .iter()
                .any(|r| r.name.contains("more folders") || r.name.contains("Next page"))
    });
    assert_eq!(window.get_details().row_count(), 200);
    assert!(!window.get_rows().iter().any(|r| r.name == "folder-230"));
    let main_path = window.get_current_path();
    let next = window
        .get_rows()
        .iter()
        .find(|r| r.name.contains("more folders") || r.name.contains("Next page"))
        .unwrap()
        .node_id;
    window.invoke_select_node(next);
    until(&host, &window, &controller, |_| {
        window.get_rows().iter().any(|r| r.name == "folder-230")
    });
    assert_eq!(window.get_current_path(), main_path);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "folder-000");
    let previous = window
        .get_rows()
        .iter()
        .find(|r| r.name == "Previous folders")
        .unwrap()
        .node_id;
    window.invoke_select_node(previous);
    until(&host, &window, &controller, |_| {
        window.get_rows().iter().any(|r| r.name == "folder-000")
    });
    window.invoke_change_page(true, true);
    until(&host, &window, &controller, |_| {
        window.get_details().iter().any(|r| r.name == "folder-230")
    });
    let index = window
        .get_details()
        .iter()
        .position(|r| r.name == "folder-230")
        .unwrap();
    window.invoke_workshop_action(window.get_workshop_token(), index as i32, true);
    until(&host, &window, &controller, |_| {
        window.get_current_path().ends_with("folder-230")
            && window
                .get_rows()
                .iter()
                .any(|r| r.name == "folder-230" && r.selected)
    });
    assert_eq!(window.get_details().row_count(), 0);
    assert_eq!(std::fs::read_dir(&source).unwrap().count(), 231);
}
