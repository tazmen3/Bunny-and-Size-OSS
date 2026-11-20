use slint::{
    ComponentHandle, Model,
    platform::{
        Key, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use sortsize_slint_spike::{
    AddSourceWindow,
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
fn key(window: &AddSourceWindow, text: slint::SharedString) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

#[test]
fn add_source_dialog_seven_languages_states_render_and_keyboard_actions_are_separate() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let dialog = AddSourceWindow::new().unwrap();
    dialog.set_windows_platform(true);
    dialog.set_source_path(r"\\server\share".into());
    dialog.window().set_size(slint::PhysicalSize::new(660, 580));
    let checks = Rc::new(Cell::new(0));
    let adds = Rc::new(Cell::new(0));
    let cancels = Rc::new(Cell::new(0));
    let searches = Rc::new(Cell::new(0));
    let connections = Rc::new(Cell::new(0));
    let selections = Rc::new(Cell::new(0));
    let reconnects = Rc::new(Cell::new(0));
    let reconnected_index = Rc::new(Cell::new(-1));
    dialog.on_reconnect_source({
        let count = reconnects.clone();
        let selected = reconnected_index.clone();
        move |index| {
            count.set(count.get() + 1);
            selected.set(index);
        }
    });
    let disconnected_sources: slint::ModelRc<slint::SharedString> =
        Rc::new(slint::VecModel::from(vec![
            "NAS été · smb://server/".into(),
            "Documents · smb://server/Documents".into(),
        ]))
        .into();
    dialog.set_disconnected_sources(disconnected_sources.clone());
    dialog.set_selected_disconnected(0);
    dialog.on_check_access({
        let count = checks.clone();
        move || count.set(count.get() + 1)
    });
    dialog.on_add_source({
        let count = adds.clone();
        move || count.set(count.get() + 1)
    });
    dialog.on_cancel({
        let count = cancels.clone();
        move || count.set(count.get() + 1)
    });
    dialog.on_find_shares({
        let count = searches.clone();
        move || count.set(count.get() + 1)
    });
    dialog.on_connect_share({
        let count = connections.clone();
        move || count.set(count.get() + 1)
    });
    dialog.on_select_share({
        let count = selections.clone();
        let weak = dialog.as_weak();
        move |index| {
            if index == 0 {
                return; // The visible prompt is never a source.
            }
            assert!(index == 1 || index == 2);
            count.set(count.get() + 1);
            let dialog = weak.upgrade().unwrap();
            dialog.set_source_path(
                if index == 1 {
                    r"\\server\Documents"
                } else {
                    r"\\server\Photos été"
                }
                .into(),
            );
            dialog.set_smb_server_only(false);
        }
    });
    dialog.set_shares(
        Rc::new(slint::VecModel::from(vec![
            dialog.get_share_placeholder(),
            "Documents".into(),
            "Photos été".into(),
        ]))
        .into(),
    );
    dialog.set_selected_share(0);
    dialog.show().unwrap();
    // Repeated activation of an already selected tab must keep its visible
    // selected state; checkable native buttons used to toggle it off.
    for _ in 0..2 {
        key(&dialog, Key::Return.into());
        assert!(dialog.get_add_tab_selected());
        assert!(!dialog.get_disconnected_tab_selected());
        assert!(!dialog.get_disconnected_view());
    }
    for (language, title, smb, reconnected) in [
        (
            Language::English,
            "Add a source",
            "SMB share",
            "Source reconnected. No scan has been started.",
        ),
        (
            Language::French,
            "Ajouter une source",
            "Partage SMB",
            "Source reconnectée. Aucune analyse n’a été lancée.",
        ),
        (
            Language::German,
            "Quelle hinzufügen",
            "SMB-Freigabe",
            "Quelle wieder verbunden. Es wurde kein Scan gestartet.",
        ),
        (
            Language::Spanish,
            "Añadir una fuente",
            "Recurso compartido SMB",
            "Fuente reconectada. No se ha iniciado ningún análisis.",
        ),
        (
            Language::Portuguese,
            "Adicionar uma origem",
            "Partilha SMB",
            "Origem reconectada. Nenhuma análise foi iniciada.",
        ),
        (
            Language::Italian,
            "Aggiungi una sorgente",
            "Condivisione SMB",
            "Sorgente riconnessa. Nessuna scansione avviata.",
        ),
        (
            Language::Dutch,
            "Bron toevoegen",
            "SMB-share",
            "Bron opnieuw verbonden. Er is geen scan gestart.",
        ),
    ] {
        activate_bundled(language).unwrap();
        assert_eq!(dialog.get_localized_title(), title);
        assert_eq!(dialog.get_source_kinds().row_data(0).unwrap(), smb);
        assert_eq!(
            dialog.get_source_kinds().row_count(),
            if dialog.get_windows_platform() { 4 } else { 3 }
        );
        for state in 0..=22 {
            dialog.set_result_state(state);
            assert!(!dialog.get_access_status().is_empty());
            if state == 21 || state == 22 {
                assert!(!dialog.get_access_status().contains("Source added"));
                if language != Language::English {
                    assert!(!dialog.get_access_status().starts_with("This source"));
                    assert!(!dialog.get_access_status().starts_with("This share"));
                }
            }
            let mut pixels = vec![slint::Rgb8Pixel::default(); 660 * 580];
            dialog.window().request_redraw();
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 660);
            }));
            assert!(pixels.windows(2).any(|pair| pair[0] != pair[1]));
            if state == 0
                && let Some(directory) = std::env::var_os("BUNNY_SOURCE_DIALOG_CAPTURE_DIR")
            {
                use std::io::Write;
                let directory = std::path::PathBuf::from(directory);
                std::fs::create_dir_all(&directory).unwrap();
                let mut file = std::io::BufWriter::new(
                    std::fs::File::create(
                        directory.join(format!("add-source-{}.ppm", language.as_str())),
                    )
                    .unwrap(),
                );
                write!(file, "P6\n660 580\n255\n").unwrap();
                for pixel in &pixels {
                    file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
                }
            }
        }
        dialog.set_disconnected_view(true);
        assert!(!dialog.get_add_tab_selected());
        assert!(dialog.get_disconnected_tab_selected());
        assert!(!dialog.get_can_register_source());
        assert_eq!(dialog.get_reconnected_message(), reconnected);
        for empty in [false, true] {
            dialog.set_disconnected_sources(if empty {
                Rc::new(slint::VecModel::<slint::SharedString>::default()).into()
            } else {
                disconnected_sources.clone()
            });
            dialog.set_selected_disconnected(if empty { -1 } else { 0 });
            let mut pixels = vec![slint::Rgb8Pixel::default(); 660 * 580];
            dialog.window().request_redraw();
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 660);
            }));
            assert!(pixels.windows(2).any(|pair| pair[0] != pair[1]));
            if !empty && let Some(directory) = std::env::var_os("BUNNY_SOURCE_DIALOG_CAPTURE_DIR") {
                use std::io::Write;
                let directory = std::path::PathBuf::from(directory);
                std::fs::create_dir_all(&directory).unwrap();
                let mut file = std::io::BufWriter::new(
                    std::fs::File::create(
                        directory.join(format!("disconnected-sources-{}.ppm", language.as_str())),
                    )
                    .unwrap(),
                );
                write!(file, "P6\n660 580\n255\n").unwrap();
                for pixel in &pixels {
                    file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
                }
            }
        }
        dialog.set_disconnected_sources(disconnected_sources.clone());
        dialog.set_selected_disconnected(0);
        dialog.set_disconnected_view(false);
        assert_eq!(reconnects.get(), 0, "rendering never reconnects a source");
        assert_eq!(
            checks.get(),
            0,
            "rendering and locale changes never check access"
        );
        assert_eq!(
            adds.get(),
            0,
            "rendering and locale changes never add a source"
        );
        assert_eq!(searches.get(), 0, "listing is always an explicit action");
        assert_eq!(connections.get(), 0, "Windows sign-in is always explicit");
        assert_eq!(
            selections.get(),
            0,
            "a model update is not a user selection"
        );
    }
    activate_bundled(Language::English).unwrap();
    dialog.set_result_state(0);
    dialog.set_source_path("192.168.1.144".into());
    dialog.set_smb_server_only(true);
    assert!(
        !dialog.get_can_register_source(),
        "a server is never a scan root"
    );
    assert!(
        dialog
            .get_access_status()
            .starts_with("Server address accepted.")
    );
    dialog.set_source_path(r"\\server\share".into());
    dialog.set_smb_server_only(false);
    assert!(dialog.get_can_register_source());
    dialog.set_shares(Rc::new(slint::VecModel::<slint::SharedString>::default()).into());
    dialog.set_selected_share(-1);
    // The permanent navigation starts at Add a source. Tab reaches the
    // disconnected-sources tab, source type, then path before all existing
    // explicit operations in their visual order.
    for _ in 0..3 {
        key(&dialog, Key::Tab.into());
    }
    key(&dialog, "x".into());
    assert!(dialog.get_source_path().contains('x'));
    key(&dialog, Key::Tab.into());
    key(&dialog, Key::Return.into());
    assert_eq!(searches.get(), 1);
    assert_eq!(connections.get(), 0);
    assert_eq!(adds.get(), 0);
    key(&dialog, Key::Tab.into());
    key(&dialog, Key::Return.into());
    assert_eq!(connections.get(), 1);
    assert_eq!(checks.get(), 0);
    dialog.set_shares(
        Rc::new(slint::VecModel::from(vec![
            dialog.get_share_placeholder(),
            "Documents".into(),
            "Photos été".into(),
        ]))
        .into(),
    );
    dialog.set_selected_share(0);
    key(&dialog, Key::Tab.into());
    // The prompt is visible before a user chooses a share. Return opens the
    // popup; one Down selects the FIRST real share, without skipping it or
    // requiring a subfolder. Return closes the popup before Tab continues.
    key(&dialog, Key::Return.into());
    key(&dialog, Key::DownArrow.into());
    key(&dialog, Key::Return.into());
    assert_eq!(selections.get(), 1);
    assert_eq!(dialog.get_selected_share(), 1);
    assert_eq!(dialog.get_source_path(), r"\\server\Documents");
    assert!(dialog.get_can_register_source());
    assert_eq!(adds.get(), 0);
    assert_eq!(checks.get(), 0);
    key(&dialog, Key::DownArrow.into());
    assert_eq!(selections.get(), 2);
    assert_eq!(dialog.get_selected_share(), 2);
    assert_eq!(dialog.get_source_path(), r"\\server\Photos été");
    for _ in 0..2 {
        key(&dialog, Key::Tab.into());
    }
    key(&dialog, Key::Return.into());
    assert_eq!(checks.get(), 1);
    assert_eq!(adds.get(), 0);
    for _ in 0..2 {
        key(&dialog, Key::Tab.into());
    }
    key(&dialog, Key::Return.into());
    assert_eq!(adds.get(), 1);
    assert_eq!(checks.get(), 1);
    key(&dialog, Key::Escape.into());
    assert_eq!(cancels.get(), 1);
    assert_eq!(reconnects.get(), 0);
    // From Add, Tab wraps through the two navigation buttons. Opening the
    // management page never checks access, adds a source, or reconnects it.
    for _ in 0..2 {
        key(&dialog, Key::Tab.into());
    }
    key(&dialog, Key::Return.into());
    assert!(dialog.get_disconnected_view());
    assert!(!dialog.get_add_tab_selected());
    assert!(dialog.get_disconnected_tab_selected());
    key(&dialog, Key::Return.into());
    assert!(
        dialog.get_disconnected_tab_selected(),
        "active tab stays selected"
    );
    assert!(dialog.get_disconnected_view());
    assert!(!dialog.get_can_register_source());
    assert_eq!(reconnects.get(), 0);
    key(&dialog, Key::Tab.into());
    key(&dialog, Key::Return.into());
    key(&dialog, Key::DownArrow.into());
    key(&dialog, Key::Return.into());
    assert_eq!(dialog.get_selected_disconnected(), 1);
    assert_eq!(reconnects.get(), 0, "selection is not reconnection");
    for _ in 0..2 {
        key(&dialog, Key::Tab.into());
    }
    key(&dialog, Key::Return.into());
    assert_eq!(reconnects.get(), 1);
    assert_eq!(reconnected_index.get(), 1);
    assert_eq!(adds.get(), 1);
    assert_eq!(checks.get(), 1);
    assert_eq!(searches.get(), 1);
    assert_eq!(connections.get(), 1);
    key(&dialog, Key::Escape.into());
    assert_eq!(cancels.get(), 2);
    assert_eq!(reconnects.get(), 1, "Escape never reconnects a source");
    dialog.hide().unwrap();
}
