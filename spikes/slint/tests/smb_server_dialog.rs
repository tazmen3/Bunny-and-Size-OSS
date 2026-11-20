//! #198: registration and confirmation are separate, keyboard-accessible actions.
use slint::{
    ComponentHandle, Model,
    platform::{
        Key, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    AddSourceWindow, SmbServerScanWindow, SmbShareChoice,
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

fn key(window: &SmbServerScanWindow, key: Key) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn complete_server_registration_and_fixed_scope_confirmation_in_seven_languages() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let add = AddSourceWindow::new().unwrap();
    add.set_windows_platform(true);
    assert_eq!(add.get_source_kinds().row_count(), 4);
    add.set_source_kind(3);
    add.set_source_path(r"\\server".into());
    add.set_smb_server_only(true);
    assert!(add.get_can_register_source());
    add.set_source_kind(0);
    assert!(!add.get_can_register_source());
    add.set_windows_platform(false);
    assert_eq!(add.get_source_kinds().row_count(), 3);
    drop(add);

    let dialog = SmbServerScanWindow::new().unwrap();
    dialog.set_token(42);
    dialog.set_server_path(r"\\synthetic-server".into());
    let shares = Rc::new(slint::VecModel::from(vec![
        SmbShareChoice {
            name: "Documents".into(),
            path: r"\\synthetic-server\Documents".into(),
            selected: true,
            disconnected: false,
        },
        SmbShareChoice {
            name: "Archives".into(),
            path: r"\\synthetic-server\Archives".into(),
            selected: true,
            disconnected: false,
        },
    ]));
    dialog.set_shares(shares.clone().into());
    dialog.set_selected_count(2);
    let confirmations = Rc::new(Cell::new(0));
    let cancellations = Rc::new(Cell::new(0));
    let choices = Rc::new(Cell::new(0));
    dialog.on_confirm({
        let count = confirmations.clone();
        move |token| {
            assert_eq!(token, 42);
            count.set(count.get() + 1);
        }
    });
    dialog.on_cancel({
        let count = cancellations.clone();
        move |token| {
            assert_eq!(token, 42);
            count.set(count.get() + 1);
        }
    });
    dialog.on_select_all({
        let weak = dialog.as_weak();
        let shares = shares.clone();
        move |token, selected| {
            assert_eq!(token, 42);
            for index in 0..shares.row_count() {
                let mut share = shares.row_data(index).unwrap();
                share.selected = selected;
                shares.set_row_data(index, share);
            }
            weak.upgrade().unwrap().set_selected_count(if selected {
                shares.row_count() as i32
            } else {
                0
            });
        }
    });
    dialog.on_toggle_share({
        let count = choices.clone();
        let weak = dialog.as_weak();
        let shares = shares.clone();
        move |token, index, selected| {
            assert_eq!(token, 42);
            count.set(count.get() + 1);
            let mut share = shares.row_data(index as usize).unwrap();
            share.selected = selected;
            shares.set_row_data(index as usize, share);
            weak.upgrade().unwrap().set_selected_count(
                (0..shares.row_count())
                    .filter(|&index| shares.row_data(index).unwrap().selected)
                    .count() as i32,
            );
        }
    });
    dialog.show().unwrap();
    dialog
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    for language in Language::ALL {
        activate_bundled(language).unwrap();
        for uncertain in [false, true] {
            dialog.set_discovery_uncertain(uncertain);
            adapter.set_size(slint::PhysicalSize::new(740, 620));
            slint::platform::update_timers_and_animations();
            dialog.window().request_redraw();
            let mut pixels = vec![slint::Rgb8Pixel::default(); 740 * 620];
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 740);
            }));
            assert!(dialog.get_can_confirm());
        }
    }
    assert_eq!(confirmations.get(), 0, "opening/discovery never confirms");
    key(&dialog, Key::Tab); // Deselect all
    key(&dialog, Key::Return);
    assert_eq!(dialog.get_selected_count(), 0);
    assert!(!dialog.get_can_confirm());
    key(&dialog, Key::Tab); // First share
    key(&dialog, Key::Space);
    assert_eq!(choices.get(), 1);
    assert_eq!(dialog.get_selected_count(), 1);
    assert!(dialog.get_can_confirm());
    assert_eq!(confirmations.get(), 0, "a checkbox never confirms");
    key(&dialog, Key::Tab); // Second share
    key(&dialog, Key::Tab); // Cancel
    key(&dialog, Key::Tab); // Confirm scan
    key(&dialog, Key::Return);
    assert_eq!(confirmations.get(), 1);
    dialog.set_busy(true);
    assert!(!dialog.get_can_confirm());
    key(&dialog, Key::Escape);
    assert_eq!(cancellations.get(), 1);
    dialog.hide().unwrap();
}
