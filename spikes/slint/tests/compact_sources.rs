//! Controlled layout/input proof. No controller, source discovery or NAS access.
use slint::{
    ComponentHandle,
    platform::{
        Key, PointerEventButton, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    SortSizeWindow, VolumeRow, WorkshopTheme,
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
fn key(window: &SortSizeWindow, key: Key) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}
fn click(window: &SortSizeWindow, disclosure: bool) {
    click_at(
        window,
        if disclosure {
            window.get_source_disclosure_x()
        } else {
            window.get_source_click_x()
        },
        window.get_source_click_y(),
    );
}
fn click_at(window: &SortSizeWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    window
        .window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}
fn rows(reverse: bool) -> slint::ModelRc<VolumeRow> {
    let mut rows = (0..6)
        .map(|index| VolumeRow {
            name: format!("Disk {index} · Q{index}:\\").into(),
            display_name: format!("Disk {index}").into(),
            path: format!("Q{index}:\\").into(),
            compact_summary: if index % 2 == 0 { "1.0 TiB" } else { "" }.into(),
            summary: "Total 1.0 TiB · Used 500 GiB · Free 524 GiB · Available 524 GiB".into(),
            state: "Local · Available".into(),
            usage: "48.8 % used".into(),
            has_usage: index % 2 == 0,
            fraction: 0.488,
            usage_percent: 48.8,
            usage_level: 1,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    if reverse {
        rows.reverse();
    }
    Rc::new(slint::VecModel::from(rows)).into()
}

#[test]
fn compact_sources_keep_disclosure_local_stable_and_keyboard_accessible_in_seven_languages() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    window.set_storage_enabled(true);
    window.set_workshop_live(true);
    window.set_scan_enabled(true);
    window.set_storage_token(51);
    window.set_volumes(rows(false));
    let selections = Rc::new(Cell::new(0));
    window.on_workshop_source({
        let selections = selections.clone();
        move |_, _| selections.set(selections.get() + 1)
    });
    window.on_select_volume(|_| panic!("unexpected legacy selection"));
    window.on_start_scan(|| panic!("disclosure must never request a scan"));
    window.on_source_context_action(|_, _, _| panic!("disclosure is not a connection action"));
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    adapter.set_size(slint::PhysicalSize::new(1200, 850));
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 850];
    for language in Language::ALL {
        activate_bundled(language).unwrap();
        for mode in [1, 2] {
            window.global::<WorkshopTheme>().set_mode(mode);
            slint::platform::update_timers_and_animations();
            window.window().request_redraw();
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 1200);
            }));
            let average = window.get_sources_content_height() / 6.;
            assert!(
                (44. ..=60.).contains(&average),
                "compact row height {average} for {language:?}, theme {mode}"
            );
        }
    }
    let compact_height = window.get_sources_content_height();
    click(&window, false);
    assert_eq!(selections.get(), 1, "local selection must remain available");
    key(&window, Key::RightArrow);
    assert_eq!(window.get_source_details_key(), "Disk 0 · Q0:\\\nQ0:\\");
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    });
    assert!(window.get_sources_content_height() > compact_height);
    assert_eq!(selections.get(), 1, "expansion must not select again");

    let details_key = window.get_source_details_key();
    window.set_storage_token(52);
    window.set_volumes(rows(true));
    slint::platform::update_timers_and_animations();
    assert_eq!(window.get_source_details_key(), details_key);
    // Replacing the entire injected model recreates its focus scopes. Focus
    // the new first row explicitly before exercising its keyboard disclosure.
    window.window().request_redraw();
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    });
    click(&window, false);
    assert_eq!(selections.get(), 2);
    key(&window, Key::LeftArrow);
    assert!(window.get_source_details_key().is_empty());
    click(&window, true);
    assert_eq!(window.get_source_details_key(), "Disk 5 · Q5:\\\nQ5:\\");
    assert_eq!(selections.get(), 2, "chevron must not select the source");
    key(&window, Key::Space);
    assert!(window.get_source_details_key().is_empty());
    assert_eq!(selections.get(), 2);
    window.hide().unwrap();
}

#[test]
fn wrapped_source_details_fit_at_both_widths_and_the_last_card_can_be_scrolled() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    activate_bundled(Language::French).unwrap();
    let path = format!("\\\\NAS\\Archives\\{}", "DonnéesÀConserver".repeat(12));
    let expanded_row = |name: &str| VolumeRow {
        name: name.into(),
        path: path.clone().into(),
        summary: "Total 1,2 TiB · Utilisé 663,2 GiB · Libre 551,8 GiB · Disponible 551,8 GiB"
            .into(),
        usage: "54,5 % utilisés".into(),
        state: "Source réseau · Reconnexion requise".into(),
        network: true,
        reconnect_needed: true,
        has_usage: true,
        fraction: 0.545,
        ..Default::default()
    };
    let mut volumes = vec![expanded_row("Archives")];
    volumes.extend((1..11).map(|index| VolumeRow {
        name: format!("Lecteur {index}").into(),
        ..Default::default()
    }));
    volumes.push(expanded_row("Sauvegarde"));
    window.set_volumes(Rc::new(slint::VecModel::from(volumes)).into());
    window.set_storage_enabled(true);
    window.set_workshop_live(true);
    window.set_scan_enabled(true);
    window.set_storage_token(51);
    let selected = Rc::new(Cell::new(-1));
    window.on_workshop_source({
        let selected = selected.clone();
        move |_, index| selected.set(index)
    });
    let reconnect = Rc::new(Cell::new(None));
    window.on_source_context_action({
        let reconnect = reconnect.clone();
        move |token, index, requested| reconnect.set(Some((token, index, requested)))
    });
    window.show().unwrap();
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    window
        .window()
        .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.5 });
    adapter.set_size(slint::PhysicalSize::new(2100, 1650));
    let mut pixels = vec![slint::Rgb8Pixel::default(); 2100 * 1650];
    let mut draw = || {
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 2100);
        }));
    };

    window.set_sidebar_width(440.);
    draw();
    let compact_height = window.get_sources_content_height();
    assert_eq!(compact_height, 12. * 52.);
    click(&window, true);
    draw();
    assert_eq!(selected.get(), -1);
    let wide_details = window.get_sources_content_height() - compact_height;
    window.set_sidebar_width(300.);
    draw();
    let medium_details = window.get_sources_content_height() - compact_height;
    window.set_sidebar_width(220.);
    draw();
    let narrow_details = window.get_sources_content_height() - compact_height;
    assert!(
        wide_details >= 85.,
        "wide card details height {wide_details}"
    );
    assert!(
        narrow_details > wide_details + 30.,
        "narrow card details height {narrow_details}, wide {wide_details}"
    );
    assert!(wide_details < medium_details && medium_details < narrow_details);
    // Reveal the next row even if the expanded card is taller than the viewport.
    window.set_sidebar_source_scroll(-narrow_details);
    draw();
    click_at(
        &window,
        window.get_source_click_x(),
        window.get_source_click_y() + 52. + narrow_details + window.get_sidebar_source_scroll(),
    );
    assert_eq!(
        selected.get(),
        1,
        "the next card starts after all the expanded details"
    );

    key(&window, Key::LeftArrow);
    draw();
    assert_eq!(window.get_sources_content_height(), compact_height);
    let x = window.get_source_click_x();
    let y = window.get_source_click_y();
    let scroll_down = || {
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(x, y),
                delta_x: 0.,
                delta_y: -2000.,
            })
    };
    scroll_down();
    draw();
    let last_header = y + 11. * 52. + window.get_sidebar_source_scroll();
    click_at(&window, window.get_source_disclosure_x(), last_header);
    draw();
    assert_eq!(
        window.get_source_details_key(),
        format!("Sauvegarde\n{path}")
    );
    assert_eq!(
        window.get_sources_content_height(),
        compact_height + narrow_details
    );
    scroll_down();
    draw();
    let last_button =
        y - 20. + window.get_sidebar_source_scroll() + window.get_sources_content_height() - 15.;
    click_at(&window, x, last_button);
    assert_eq!(
        reconnect.get(),
        Some((51, 11, true)),
        "the last control can be reached by scrolling"
    );
    assert_eq!(selected.get(), 1);
    window.hide().unwrap();
}
