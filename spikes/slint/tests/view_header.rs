//! VIEW-001/008/012: the rendered header stays compact and keyboard operable.
use slint::{
    ComponentHandle,
    platform::{
        Key, PointerEventButton, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    SortSizeWindow,
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

fn render(window: &SortSizeWindow, adapter: &MinimalSoftwareWindow, width: u32) {
    adapter.set_size(slint::PhysicalSize::new(width, 760));
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); width as usize * 760];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
    }));
}

fn key(window: &SortSizeWindow, key: Key) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn folder_header_stays_compact_in_seven_languages_and_both_modes_remain_keyboard_accessible() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let view_changes = Rc::new(Cell::new(0));
    window.on_set_treemap_mode({
        let count = view_changes.clone();
        move |_| count.set(count.get() + 1)
    });
    let scans = Rc::new(Cell::new(0));
    window.on_start_scan({
        let count = scans.clone();
        move || count.set(count.get() + 1)
    });
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    for language in [
        Language::English,
        Language::French,
        Language::German,
        Language::Spanish,
        Language::Portuguese,
        Language::Italian,
        Language::Dutch,
    ] {
        activate_bundled(language).unwrap();
        for theme in [1, 2] {
            window.set_theme_choice(theme);
            for compact in [false, true] {
                window.set_dense(compact);
                let mut previous_width: Option<f32> = None;
                for width in [780, 1200] {
                    window.set_current_folder_summary("".into());
                    render(&window, &adapter, width);
                    let before_summary = window.get_detail_viewport_height();
                    window.set_current_folder_summary(
                        "≥ 12,0 GiB · ≥ 2650 fichiers · ≥ 297 dossiers · Partiel".into(),
                    );
                    render(&window, &adapter, width);
                    assert!(
                        window.get_detail_viewport_height() < before_summary - 10.,
                        "summary must remain visible in compact={compact}"
                    );
                    let selector_width = window.get_treemap_selector_width();
                    assert!(
                        (100.0..=240.0).contains(&selector_width),
                        "{language:?}: view selector width={selector_width} at window={width}"
                    );
                    if let Some(previous_width) = previous_width {
                        assert!(
                            (selector_width - previous_width).abs() < 1.,
                            "selector must not stretch as the window grows"
                        );
                    }
                    previous_width = Some(selector_width);
                    assert!(window.get_detail_viewport_height() > 80.);
                }
            }
        }
    }
    let position = slint::LogicalPosition::new(
        window.get_treemap_selector_x() + window.get_treemap_selector_width() / 2.,
        window.get_treemap_selector_y() + window.get_treemap_selector_height() / 2.,
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
    key(&window, Key::Escape);
    window.set_treemap_mode(0);
    key(&window, Key::DownArrow);
    assert_eq!(window.get_treemap_mode(), 1);
    key(&window, Key::UpArrow);
    assert_eq!(window.get_treemap_mode(), 0);
    assert_eq!(view_changes.get(), 2);
    assert_eq!(scans.get(), 0);
    window.hide().unwrap();
}

#[test]
fn folder_summary_names_the_logical_size_in_all_seven_languages() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    window.show().unwrap();
    let measure = "≥ 12.0 GiB · ≥ 2,650 files · ≥ 297 folders · Partial";
    for (language, label) in [
        (Language::English, "Logical size of scanned files"),
        (Language::French, "Taille logique des fichiers analysés"),
        (Language::German, "Logische Größe der gescannten Dateien"),
        (
            Language::Spanish,
            "Tamaño lógico de los archivos analizados",
        ),
        (
            Language::Portuguese,
            "Tamanho lógico dos ficheiros analisados",
        ),
        (Language::Italian, "Dimensione logica dei file analizzati"),
        (
            Language::Dutch,
            "Logische grootte van de gescande bestanden",
        ),
    ] {
        activate_bundled(language).unwrap();
        window.set_current_folder_summary("".into());
        assert!(window.get_current_folder_summary_text().is_empty());
        window.set_current_folder_summary(measure.into());
        assert_eq!(
            window.get_current_folder_summary_text().as_str(),
            format!("{label} · {measure}")
        );
        render(&window, &adapter, 1200);
    }
    window.hide().unwrap();
}
