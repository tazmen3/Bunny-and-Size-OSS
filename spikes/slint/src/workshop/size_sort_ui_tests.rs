//! Targeted folder-table checks on Slint's software window and native inventory
//! service. Pointer/key routing here does not qualify physical Winit input.
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

fn until(
    host: &Rc<RefCell<Host>>,
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
    predicate: impl Fn(&Host) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        host.borrow_mut().poll(window, &controller.borrow());
        assert!(window.get_errors().is_empty(), "{}", window.get_errors());
        if predicate(&host.borrow()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "size sort UI timed out: {} / {}",
            window.get_status(),
            window.get_current_path()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn await_epoch(
    host: &Rc<RefCell<Host>>,
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
) {
    let epoch = host.borrow().epoch;
    until(host, window, controller, |host| {
        host.current.as_ref().is_some_and(|page| {
            page.epoch == epoch && !page.busy && page.measure_ready && page.sort_index >= 0
        })
    });
}

fn scan(
    host: &Rc<RefCell<Host>>,
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
    source: &Path,
) {
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(source.to_owned())
            .into(),
    );
    window.invoke_start_scan();
    await_epoch(host, window, controller);
}

fn names(window: &SortSizeWindow) -> Vec<String> {
    window
        .get_details()
        .iter()
        .map(|row| row.name.to_string())
        .collect()
}

#[test]
fn size_sort_ui_global_pages_folder_changes_focus_and_explicit_preference_reopen() {
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
    std::fs::create_dir_all(source.join("middle")).unwrap();
    std::fs::create_dir_all(source.join("empty")).unwrap();
    std::fs::write(source.join("middle/a"), [0; 80]).unwrap();
    std::fs::write(source.join("middle/z"), [0; 70]).unwrap();
    std::fs::write(source.join("large-root"), [0; 500]).unwrap();
    for i in 0..220 {
        std::fs::write(source.join(format!("file-{i:03}")), vec![0; i + 1]).unwrap();
    }
    let config = fixture.path().join("settings/display-v1.conf");
    controller.borrow_mut().preference_store =
        Some(crate::preferences_store::Store::start(config.clone()).unwrap());
    scan(&host, &window, &controller, &source);
    assert_eq!(window.get_sort_order(), 0);
    assert_eq!(window.get_details().row_count(), 200);
    assert_eq!(names(&window)[0], "large-root");
    assert!(window.get_has_next_detail_page());
    assert!(!window.get_has_previous_detail_page());
    let middle = names(&window)
        .iter()
        .position(|name| name == "middle")
        .unwrap();
    assert!(
        middle > 1,
        "folders and files share one logical-size ranking"
    );
    assert_eq!(names(&window)[middle - 1], "file-149");
    assert_eq!(names(&window)[middle + 1], "file-148");

    // Focus lives outside row delegates. A refresh must keep this focus owner,
    // the selected identity, the live model and the viewport's sub-row offset.
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let _visibility = TestWindowVisibility::new(&window);
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1200, 900));
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 900];
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    });
    let position =
        slint::LogicalPosition::new(window.get_detail_click_x(), window.get_detail_click_y());
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
    until(&host, &window, &controller, |host| host.selected.is_some());
    assert!(window.get_detail_table_focused());
    let selected = host.borrow().selected;
    let details = window.get_details();
    window.set_detail_scroll_y(-34. * 5. - 7.);
    let scroll = window.get_detail_scroll_y();
    let token = window.get_workshop_token();
    host.borrow_mut().navigate(Navigate::Refresh);
    await_epoch(&host, &window, &controller);
    assert_ne!(window.get_workshop_token(), token);
    assert_eq!(window.get_details(), details);
    assert_eq!(host.borrow().selected, selected);
    assert!(window.get_detail_table_focused());
    assert!((window.get_detail_scroll_y() - scroll).abs() < 0.01);
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Space.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Space.into(),
    });
    assert_eq!(host.borrow().selected, selected);
    // Keyboard traversal keeps the selected row visible within the stable scope.
    for _ in 0..30 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::DownArrow.into(),
        });
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::DownArrow.into(),
        });
    }
    assert!(window.get_detail_table_focused());
    assert_eq!(window.get_workshop_selected(), 30);
    assert!(window.get_detail_scroll_y() < -34. * 5.);
    for _ in 0..30 {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::UpArrow.into(),
        });
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::UpArrow.into(),
        });
    }
    assert_eq!(window.get_workshop_selected(), 0);
    assert!(window.get_detail_scroll_y().abs() < 0.01);

    // Concatenate inventory pages, including the last 23 rows. The global assertion
    // would fail if sorting were restricted to the first Slint model window.
    let mut logical = host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .rows
        .iter()
        .map(|row| row.logical.unwrap())
        .collect::<Vec<_>>();
    let mut identities = host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .rows
        .iter()
        .map(|row| row.key)
        .collect::<Vec<_>>();
    window.invoke_change_page(true, true);
    await_epoch(&host, &window, &controller);
    assert_eq!(window.get_details().row_count(), 23);
    assert!(!window.get_has_next_detail_page());
    assert!(window.get_has_previous_detail_page());
    logical.extend(
        host.borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .rows
            .iter()
            .map(|row| row.logical.unwrap()),
    );
    identities.extend(
        host.borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .rows
            .iter()
            .map(|row| row.key),
    );
    assert_eq!(logical.len(), 223);
    assert!(logical.windows(2).all(|pair| pair[0] >= pair[1]));
    assert_eq!(logical[0], 500);
    assert_eq!(logical.last(), Some(&0));
    let unique: std::collections::HashSet<_> = identities.iter().map(|key| key.entry.0).collect();
    assert_eq!(unique.len(), identities.len());
    window.invoke_change_page(true, false);
    await_epoch(&host, &window, &controller);
    assert_eq!(names(&window)[0], "large-root");

    let middle = names(&window)
        .iter()
        .position(|name| name == "middle")
        .unwrap();
    window.invoke_workshop_action(window.get_workshop_token(), middle as i32, true);
    await_epoch(&host, &window, &controller);
    assert_eq!(names(&window), ["a", "z"]);
    assert_eq!(window.get_sort_order(), 0);
    assert!(!window.get_has_previous_detail_page() && !window.get_has_next_detail_page());
    window.invoke_sort_details(true);
    await_epoch(&host, &window, &controller);
    assert_eq!(window.get_sort_order(), 2);
    assert_eq!(names(&window), ["a", "z"]);
    window.invoke_sort_details(true);
    await_epoch(&host, &window, &controller);
    assert_eq!(window.get_sort_order(), 3);
    assert_eq!(names(&window), ["z", "a"]);
    window.invoke_go_parent();
    await_epoch(&host, &window, &controller);
    assert_eq!(window.get_sort_order(), 3);
    assert_eq!(names(&window)[..2], ["middle", "empty"]);
    assert_eq!(controller.borrow().preferences.detail_sort, 3);
    drop(controller.borrow_mut().preference_store.take());
    let (saved, status) = crate::preferences_store::load(&config);
    assert_eq!(status, 0);
    assert_eq!(saved.detail_sort, 3);
    window.hide().unwrap();
    drop(host);
    drop(controller);
    drop(window);

    // Reopen from the exact temporary profile written by the manual action.
    let (window, controller) = crate::new_session(saved, Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    scan(&host, &window, &controller, &source.join("middle"));
    assert_eq!(window.get_sort_order(), 3);
    assert_eq!(names(&window), ["z", "a"]);
    assert_eq!(std::fs::read(source.join("large-root")).unwrap(), [0; 500]);
}
