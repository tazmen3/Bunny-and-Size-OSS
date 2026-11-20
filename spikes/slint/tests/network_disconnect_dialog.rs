//! Software-rendered interaction proof; native Windows qualification is separate.
use slint::{
    ComponentHandle,
    platform::{
        Key, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    NetworkDisconnectWindow,
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
fn key(window: &slint::Window, text: slint::SharedString) {
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

#[test]
fn network_confirmation_defaults_to_cancel_and_renders_seven_languages_without_side_effects() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let dialog = NetworkDisconnectWindow::new().unwrap();
    dialog.set_request_token(41);
    dialog.set_source_name("NAS été".into());
    dialog.set_source_path(r"\\192.0.2.1\TV".into());
    dialog.set_affected_share_count(3);
    dialog.set_windows_platform(true);
    dialog.set_windows_option_available(false);
    dialog.window().set_size(slint::PhysicalSize::new(660, 520));
    let accepted = Rc::new(Cell::new(0));
    let rejected = Rc::new(Cell::new(0));
    dialog.on_accepted({
        let accepted = accepted.clone();
        move |token, windows| {
            assert_eq!(token, 41);
            assert!(
                !windows,
                "an unavailable Windows operation cannot be accepted"
            );
            accepted.set(accepted.get() + 1);
        }
    });
    dialog.on_rejected({
        let rejected = rejected.clone();
        move |token| {
            assert_eq!(token, 41);
            rejected.set(rejected.get() + 1);
        }
    });
    dialog.show().unwrap();
    dialog
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    for (language, title) in [
        (Language::English, "Confirm disconnection"),
        (Language::French, "Confirmer la déconnexion"),
        (Language::German, "Trennung bestätigen"),
        (Language::Spanish, "Confirmar desconexión"),
        (Language::Portuguese, "Confirmar desconexão"),
        (Language::Italian, "Conferma disconnessione"),
        (Language::Dutch, "Verbinding verbreken bevestigen"),
    ] {
        activate_bundled(language).unwrap();
        assert_eq!(dialog.get_localized_title(), title);
        assert!(dialog.get_confirmation_message().contains("NAS été"));
        assert!(!dialog.get_disconnect_windows());
        let mut pixels = vec![slint::Rgb8Pixel::default(); 660 * 520];
        dialog.window().request_redraw();
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 660);
        }));
        assert!(pixels.windows(2).any(|pair| pair[0] != pair[1]));
    }
    assert_eq!(accepted.get(), 0);
    assert_eq!(rejected.get(), 0);
    key(dialog.window(), Key::Return.into());
    assert_eq!(rejected.get(), 1, "initial Enter must activate Cancel");
    assert_eq!(accepted.get(), 0);
    key(dialog.window(), Key::Escape.into());
    assert_eq!(rejected.get(), 2);
    assert_eq!(accepted.get(), 0);
    // Even a stale programmatic opt-in must not reach an unsafe Windows target.
    dialog.set_disconnect_windows(true);
    // Voluntarily tab from Cancel onto Confirm, then activate it.
    key(dialog.window(), Key::Tab.into());
    key(dialog.window(), Key::Return.into());
    assert_eq!(accepted.get(), 1);
    // An old opt-in cannot carry over to a new immutable request.
    dialog.set_disconnect_windows(true);
    dialog.set_request_token(42);
    slint::platform::update_timers_and_animations();
    assert!(!dialog.get_disconnect_windows());
    dialog.hide().unwrap();
}
