//! Ubuntu surface contract. Synthetic rendering is separate from native SMB tests.
use slint::{
    ComponentHandle, Model,
    platform::{
        Key, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    AddSourceWindow, NetworkDisconnectWindow,
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
fn key(window: &slint::Window, key: Key) {
    window.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn ubuntu_source_operations_are_explicit_localized_and_disconnect_defaults_to_cancel() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let add = AddSourceWindow::new().unwrap();
    add.set_linux_platform(true);
    assert!(!add.get_windows_platform());
    assert_eq!(add.get_source_kinds().row_count(), 4);
    add.set_source_path("smb://nas.example/".into());
    add.set_smb_server_only(true);
    assert!(!add.get_can_register_source(), "a server is not a share");
    add.set_source_kind(3);
    assert!(
        add.get_can_register_source(),
        "offline server bookmarks are allowed"
    );
    add.set_source_kind(0);
    add.set_source_path("smb://nas.example/Photos%20%C3%A9t%C3%A9".into());
    add.set_smb_server_only(false);
    assert!(add.get_can_register_source());
    let operations = Rc::new(Cell::new(0));
    for callback in [0, 1, 2, 3] {
        let count = operations.clone();
        let action = move || count.set(count.get() + 1);
        match callback {
            0 => add.on_add_source(action),
            1 => add.on_check_access(action),
            2 => add.on_connect_share(action),
            _ => add.on_find_shares(action),
        }
    }
    add.show().unwrap();
    add.window().set_size(slint::PhysicalSize::new(660, 580));
    for language in Language::ALL {
        activate_bundled(language).unwrap();
        for state in 0..=22 {
            add.set_result_state(state);
            assert!(!add.get_access_status().is_empty());
            if [10, 11, 13, 16, 17, 19, 20].contains(&state) {
                assert!(!add.get_access_status().contains("Windows"));
            }
            add.window().request_redraw();
            let mut pixels = vec![slint::Rgb8Pixel::default(); 660 * 580];
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 660);
            }));
            assert!(pixels.windows(2).any(|pair| pair[0] != pair[1]));
        }
    }
    assert_eq!(
        operations.get(),
        0,
        "restoration and rendering grant no access"
    );
    activate_bundled(Language::English).unwrap();
    add.set_result_state(20);
    assert!(
        add.get_access_status()
            .contains("requires an accessible mounted folder")
    );
    add.set_result_state(13);
    assert!(
        add.get_access_status()
            .contains("mount available for scanning")
    );
    add.set_source_kind(3);
    add.set_result_state(18);
    assert!(
        add.get_access_status()
            .starts_with("Server source ready to add")
    );
    add.set_source_kind(0);
    assert!(
        add.get_access_status()
            .starts_with("Select a share from the list")
    );
    add.hide().unwrap();
    drop(add);

    let disconnect = NetworkDisconnectWindow::new().unwrap();
    disconnect.set_linux_platform(true);
    disconnect.set_request_token(12);
    disconnect.set_source_name("NAS été".into());
    disconnect.set_source_path("smb://nas.example/Photos".into());
    assert!(!disconnect.get_disconnect_linux());
    assert!(!disconnect.get_linux_option_available());
    let accepts = Rc::new(Cell::new(0));
    let cancels = Rc::new(Cell::new(0));
    disconnect.on_accepted({
        let accepts = accepts.clone();
        move |token, native| {
            assert_eq!(token, 12);
            assert!(
                !native,
                "a stale option cannot unmount an unsafe system mount"
            );
            accepts.set(accepts.get() + 1);
        }
    });
    disconnect.on_rejected({
        let cancels = cancels.clone();
        move |token| {
            assert_eq!(token, 12);
            cancels.set(cancels.get() + 1);
        }
    });
    disconnect.show().unwrap();
    disconnect
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    disconnect
        .window()
        .set_size(slint::PhysicalSize::new(660, 520));
    for language in Language::ALL {
        activate_bundled(language).unwrap();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 660 * 520];
        disconnect.window().request_redraw();
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 660);
        }));
        assert!(!disconnect.get_disconnect_linux());
    }
    assert_eq!(accepts.get(), 0);
    key(disconnect.window(), Key::Return);
    assert_eq!(cancels.get(), 1, "initial Enter must activate Cancel");
    key(disconnect.window(), Key::Escape);
    assert_eq!(cancels.get(), 2);
    disconnect.set_disconnect_linux(true);
    key(disconnect.window(), Key::Tab);
    key(disconnect.window(), Key::Return);
    assert_eq!(accepts.get(), 1);
    disconnect.set_linux_option_available(true);
    disconnect.set_request_token(13);
    slint::platform::update_timers_and_animations();
    assert!(
        !disconnect.get_disconnect_linux(),
        "native opt-in never carries over"
    );
    disconnect.hide().unwrap();
}
