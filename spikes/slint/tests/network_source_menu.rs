//! Input-level menu proof with an injected source, without network access.
use slint::{
    ComponentHandle,
    platform::{
        Key, PointerEventButton, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{SortSizeWindow, VolumeRow};
use std::{cell::RefCell, rc::Rc};

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
fn click(window: &SortSizeWindow, button: PointerEventButton) {
    let position =
        slint::LogicalPosition::new(window.get_source_click_x(), window.get_source_click_y());
    window
        .window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}
fn disclosure_click(window: &SortSizeWindow, button: PointerEventButton) {
    let position = slint::LogicalPosition::new(
        window.get_source_disclosure_x(),
        window.get_source_click_y(),
    );
    window
        .window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}
fn row(network: bool, disconnected: bool) -> slint::ModelRc<VolumeRow> {
    Rc::new(slint::VecModel::from(vec![VolumeRow {
        name: "Controlled NAS source".into(),
        network,
        disconnected,
        ..Default::default()
    }]))
    .into()
}

#[test]
fn right_click_and_keyboard_request_the_exact_source_without_selecting_or_scanning() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    window.set_storage_enabled(true);
    window.set_workshop_live(true);
    window.set_scan_enabled(false); // Context menu remains available during acquisition.
    window.set_storage_token(17);
    window.set_volumes(row(true, false));
    let requests = Rc::new(RefCell::new(Vec::new()));
    window.on_source_context_action({
        let requests = requests.clone();
        move |token, index, reconnect| requests.borrow_mut().push((token, index, reconnect))
    });
    let selections = Rc::new(RefCell::new(Vec::new()));
    window.on_workshop_source({
        let selections = selections.clone();
        move |token, index| selections.borrow_mut().push((token, index))
    });
    window.on_select_volume(|_| panic!("a right click must never select a source"));
    window.on_start_scan(|| panic!("a context menu must never start an acquisition"));
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    adapter.set_size(slint::PhysicalSize::new(1200, 850));
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 850];
    window.window().request_redraw();
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    }));
    click(&window, PointerEventButton::Right);
    assert!(requests.borrow().is_empty());
    assert!(
        selections.borrow().is_empty(),
        "right click must not select"
    );
    key(&window, Key::Escape);
    assert!(requests.borrow().is_empty());
    click(&window, PointerEventButton::Right);
    key(&window, Key::DownArrow);
    key(&window, Key::Return);
    assert_eq!(&*requests.borrow(), &[(17, 0, false)]);
    // A publication while a menu is open must not silently retarget its action.
    click(&window, PointerEventButton::Right);
    window.set_storage_token(18);
    key(&window, Key::DownArrow);
    key(&window, Key::Return);
    assert_eq!(requests.borrow().last(), Some(&(17, 0, false)));
    window.set_volumes(row(true, true));
    // Right click focuses the exact source without selecting it; Menu reopens it.
    click(&window, PointerEventButton::Right);
    key(&window, Key::Escape);
    key(&window, Key::Menu);
    key(&window, Key::DownArrow);
    key(&window, Key::Return);
    assert_eq!(requests.borrow().last(), Some(&(18, 0, true)));
    assert!(selections.borrow().is_empty(), "network menus never select");
    // An unqueried source offers both commands. A local acquisition does not
    // disable reconnection, and the disclosure preserves the same right menu.
    window.set_volumes(
        Rc::new(slint::VecModel::from(vec![VolumeRow {
            name: "Controlled NAS source".into(),
            network: true,
            reconnect_needed: true,
            ..Default::default()
        }]))
        .into(),
    );
    disclosure_click(&window, PointerEventButton::Right);
    assert!(window.get_source_details_key().is_empty());
    key(&window, Key::DownArrow);
    key(&window, Key::Return);
    assert_eq!(requests.borrow().last(), Some(&(18, 0, true)));
    click(&window, PointerEventButton::Right);
    key(&window, Key::DownArrow);
    key(&window, Key::DownArrow);
    key(&window, Key::Return);
    assert_eq!(requests.borrow().last(), Some(&(18, 0, false)));
    assert!(selections.borrow().is_empty());
    // ContextMenuArea::enabled disables only its menu, never local selection.
    // Exercise the real local card callback used by P1-01 after replacing it.
    window.set_storage_token(19);
    window.set_volumes(row(false, false));
    window.set_scan_enabled(true);
    slint::platform::update_timers_and_animations();
    click(&window, PointerEventButton::Left);
    assert_eq!(&*selections.borrow(), &[(19, 0)]);
    let request_count = requests.borrow().len();
    click(&window, PointerEventButton::Right);
    key(&window, Key::Escape);
    assert_eq!(requests.borrow().len(), request_count);
    assert_eq!(&*selections.borrow(), &[(19, 0)]);
    window.hide().unwrap();
}
