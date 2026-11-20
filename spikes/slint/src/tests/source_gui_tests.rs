use super::*;

#[test]
fn add_source_dialog_registers_without_scanning_and_access_check_is_explicit() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(
        fixture.path().join("must-not-be-hashed"),
        b"original content",
    )
    .unwrap();
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(2);
    dialog.set_source_path(fixture.path().to_str().unwrap().into());
    dialog.set_display_name("Données été 日本語".into());
    let original_path = window.get_scan_path();
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 6); // Test session has no preference writer.
    assert_eq!(window.get_volumes().row_count(), 1);
    assert!(
        window
            .get_volumes()
            .row_data(0)
            .unwrap()
            .name
            .contains("Données été 日本語")
    );
    assert_eq!(window.get_scan_path(), original_path);
    assert!(controller.borrow().task.is_none());
    assert!(controller.borrow().pending_auto.is_none());
    assert_eq!(controller.borrow().generation, 0);
    assert!(!controller.borrow().source_operations.busy());
    dialog.invoke_check_access();
    assert!(dialog.get_busy());
    let start = std::time::Instant::now();
    while controller.borrow().source_operations.busy() {
        controller.borrow_mut().poll_source_dialog();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert_eq!(dialog.get_result_state(), 1);
    assert!(controller.borrow().task.is_none());
    assert_eq!(controller.borrow().generation, 0);
    dialog.invoke_add_source();
    assert_eq!(window.get_volumes().row_count(), 1);
    assert_eq!(
        fs::read(fixture.path().join("must-not-be-hashed")).unwrap(),
        b"original content"
    );
}

#[test]
fn adding_source_preserves_active_local_generation_path_and_mailbox() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let local = TempDir::new().unwrap();
    let added = TempDir::new().unwrap();
    fs::write(local.path().join("local.txt"), b"local").unwrap();
    super::volume_gui_tests::inject_discovery(
        &controller,
        super::volume_gui_tests::discovered(&[local.path().into()]),
    );
    window.invoke_select_volume(0);
    assert!(window.get_busy());
    let generation = controller.borrow().generation;
    let mailbox = controller.borrow().task.as_ref().unwrap().mailbox.clone();
    let cancellation = controller
        .borrow()
        .task
        .as_ref()
        .unwrap()
        .cancellation
        .clone();
    let path = window.get_scan_path();
    let source = window.get_scan_source();
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(2);
    dialog.set_source_path(added.path().to_str().unwrap().into());
    dialog.invoke_add_source();
    assert_eq!(controller.borrow().generation, generation);
    assert!(std::sync::Arc::ptr_eq(
        &controller.borrow().task.as_ref().unwrap().mailbox,
        &mailbox
    ));
    assert!(!cancellation.is_cancelled());
    assert_eq!(window.get_scan_path(), path);
    assert_eq!(window.get_scan_source(), source);
    assert!(controller.borrow().pending_auto.is_none());
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "local.txt");
}

#[cfg(windows)]
#[test]
fn unc_bookmark_from_dialog_is_visible_and_selected_without_network_query() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(0);
    dialog.set_source_path(r"\\offline.invalid\partage été".into());
    dialog.set_display_name("NAS hors ligne".into());
    dialog.invoke_add_source();
    let row = window.get_volumes().row_data(0).unwrap();
    assert!(row.name.contains("NAS hors ligne (SMB)"));
    assert!(!row.has_usage);
    window.invoke_select_volume(0);
    assert_eq!(window.get_scan_path(), r"\\offline.invalid\partage été");
    assert!(controller.borrow().task.is_none());
    assert!(!controller.borrow().source_operations.busy());
    assert!(controller.borrow().pending_auto.is_none());
    assert_eq!(controller.borrow().generation, 0);
}

#[cfg(windows)]
#[test]
fn server_address_is_guided_and_cannot_register_or_check_as_a_scan_root() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    let initial = window.get_scan_path();
    for address in ["192.168.1.144", "nas.invalid", r"\\192.168.1.144"] {
        dialog.set_source_kind(0);
        dialog.set_source_path(address.into());
        dialog.invoke_input_changed();
        assert!(dialog.get_smb_server_only());
        assert_eq!(dialog.get_result_state(), 7);
        dialog.invoke_add_source();
        assert_eq!(dialog.get_result_state(), 7);
        dialog.invoke_check_access();
        assert_eq!(dialog.get_result_state(), 7);
        assert!(controller.borrow().registry.entries.is_empty());
        assert!(!controller.borrow().source_operations.busy());
        assert!(controller.borrow().task.is_none());
        assert!(controller.borrow().pending_auto.is_none());
        assert_eq!(window.get_scan_path(), initial);
    }
}

#[cfg(windows)]
#[test]
fn share_selection_and_native_connection_result_do_not_authorize_acquisition() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_path("nas.invalid".into());
    dialog.invoke_input_changed();
    let initial = window.get_scan_path();
    let path = std::path::PathBuf::from(r"\\nas.invalid\Documents été");
    // Deterministic controller result injection; no real network is contacted.
    controller
        .borrow_mut()
        .source_operations
        .start(move || crate::source_dialog::Outcome {
            state: 0,
            shares: Some(vec![path]),
            limited: false,
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while controller.borrow().source_operations.busy() {
        controller.borrow_mut().poll_source_dialog();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(dialog.get_shares().row_count(), 2); // Prompt, then the native share.
    assert!(controller.borrow().registry.entries.is_empty());
    dialog.invoke_select_share(1);
    assert_eq!(dialog.get_source_path(), r"\\nas.invalid\Documents été");
    assert!(!dialog.get_smb_server_only());
    controller
        .borrow_mut()
        .source_operations
        .start(|| crate::source_dialog::Outcome::state(13))
        .unwrap();
    while controller.borrow().source_operations.busy() {
        controller.borrow_mut().poll_source_dialog();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(dialog.get_result_state(), 13);
    assert_eq!(window.get_scan_path(), initial);
    assert!(controller.borrow().registry.entries.is_empty());
    dialog.invoke_add_source();
    assert_eq!(controller.borrow().registry.entries.len(), 1);
    assert_eq!(window.get_scan_path(), initial);
    assert!(controller.borrow().task.is_none());
    assert!(controller.borrow().pending_auto.is_none());
    window.invoke_select_volume(0);
    assert!(controller.borrow().task.is_none());
    assert_eq!(controller.borrow().generation, 0);
}

#[cfg(windows)]
#[test]
fn discovered_share_roots_are_added_directly_without_access_check_or_scan() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_path(r"\\192.168.1.144\".into());
    dialog.invoke_input_changed();
    let initial = window.get_scan_path();
    // Inject only the bounded share-enumeration result. This test performs no
    // real network operation and is not evidence of a reachable SMB service.
    let roots = [
        r"\\192.168.1.144\bouquins",
        r"\\192.168.1.144\homes",
        r"\\192.168.1.144\TV",
        r"\\192.168.1.144\video",
    ];
    controller
        .borrow_mut()
        .source_operations
        .start(move || crate::source_dialog::Outcome {
            state: 18,
            shares: Some(roots.into_iter().map(std::path::PathBuf::from).collect()),
            limited: false,
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while controller.borrow().source_operations.busy() {
        controller.borrow_mut().poll_source_dialog();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(dialog.get_shares().row_count(), 5);
    assert_eq!(dialog.get_selected_share(), 0);
    assert_eq!(
        dialog.get_shares().row_data(0).unwrap(),
        dialog.get_share_placeholder()
    );
    assert!(dialog.get_smb_server_only());
    assert!(!dialog.get_can_register_source());
    dialog.invoke_select_share(0); // A prompt cannot turn the server into a source.
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 7);
    assert!(controller.borrow().registry.entries.is_empty());

    for (display_index, root) in [(3, roots[2]), (1, roots[0])] {
        dialog.invoke_select_share(display_index);
        assert_eq!(dialog.get_source_path(), root);
        assert_eq!(dialog.get_selected_share(), display_index);
        assert!(!dialog.get_smb_server_only());
        assert!(dialog.get_can_register_source());
        assert!(!controller.borrow().source_operations.busy());
        assert_eq!(dialog.get_result_state(), 0); // Access was not checked.
        dialog.invoke_add_source();
        assert_eq!(dialog.get_result_state(), 6);
        assert!(
            controller
                .borrow()
                .registry
                .entries
                .iter()
                .any(|s| s.path == std::path::Path::new(root))
        );
        assert_eq!(window.get_scan_path(), initial);
        assert!(controller.borrow().task.is_none());
        assert!(controller.borrow().pending_auto.is_none());
        assert!(!controller.borrow().source_operations.busy());
        assert_eq!(controller.borrow().generation, 0);
    }
    assert_eq!(controller.borrow().registry.entries.len(), 2);
    window.invoke_select_volume(0);
    assert!(controller.borrow().task.is_none());
    assert!(controller.borrow().pending_auto.is_none());
    assert_eq!(controller.borrow().generation, 0);
}

#[cfg(windows)]
#[test]
fn edited_or_cancelled_dialog_discards_late_shares_and_never_starts_second_worker() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_path("old.invalid".into());
    let (sender, receiver) = std::sync::mpsc::channel();
    controller
        .borrow_mut()
        .source_operations
        .start(move || {
            receiver.recv().unwrap();
            crate::source_dialog::Outcome {
                state: 0,
                shares: Some(vec![r"\\old.invalid\stale".into()]),
                limited: false,
            }
        })
        .unwrap();
    dialog.set_busy(true);
    dialog.set_source_path("new.invalid".into());
    dialog.invoke_input_changed();
    dialog.invoke_cancel();
    assert_eq!(dialog.get_result_state(), 14);
    // A new callback while the native call is blocked is refused before I/O.
    dialog.invoke_find_shares();
    sender.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while controller.borrow().source_operations.busy() {
        controller.borrow_mut().poll_source_dialog();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(dialog.get_shares().row_count(), 0);
    assert_eq!(dialog.get_source_path(), "new.invalid");
    assert_eq!(dialog.get_result_state(), 14);
    assert!(controller.borrow().registry.entries.is_empty());
    assert!(controller.borrow().task.is_none());
}

// Use the source syntax accepted natively by each platform. Windows SMB
// parsing deliberately accepts UNC paths rather than Ubuntu's SMB URIs.
fn reconnect_test_path(share: &str) -> String {
    #[cfg(windows)]
    let server = r"\\192.168.1.144\";
    #[cfg(target_os = "linux")]
    let server = "smb://192.168.1.144/";
    format!("{server}{share}")
}

#[test]
fn adding_disconnected_server_or_share_offers_reconnect_without_granting_it() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let requested = Rc::new(RefCell::new(Vec::new()));
    controller.borrow_mut().source_reconnect = Some(Rc::new({
        let requested = requested.clone();
        move |source| requested.borrow_mut().push(source)
    }));
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    let initial_path = window.get_scan_path();
    for (kind, path) in [
        (3, reconnect_test_path("")),
        (0, reconnect_test_path("bouquins")),
    ] {
        let source =
            source_registry::Source::new(kind, path.clone().into(), "NAS livres", None).unwrap();
        {
            let mut c = controller.borrow_mut();
            c.registry = source_registry::Registry::default();
            c.registry.add(source.clone()).unwrap();
            c.registry.disconnect(source.clone()).unwrap();
            let entries = c.registry.entries.clone();
            c.sources.set_registered(&entries);
            c.show_sources(&window);
        }
        requested.borrow_mut().clear();
        dialog.set_disconnected_view(false);
        dialog.set_source_kind(kind);
        dialog.set_source_path(path.into());
        dialog.invoke_add_source();
        assert_eq!(dialog.get_result_state(), 21);
        assert!(dialog.get_disconnected_view());
        assert_eq!(dialog.get_selected_disconnected(), 0);
        assert_eq!(window.get_volumes().row_count(), 0);
        assert!(controller.borrow().registry.is_disconnected(&source.path));
        assert!(requested.borrow().is_empty(), "adding does not reconnect");
        assert!(!controller.borrow().source_operations.busy());
        assert_eq!(window.get_scan_path(), initial_path);
        assert_eq!(controller.borrow().generation, 0);
        assert!(controller.borrow().task.is_none());
        assert!(controller.borrow().pending_auto.is_none());
        dialog.invoke_reconnect_source(0);
        let expected = controller
            .borrow()
            .registry
            .find(&source.path)
            .unwrap()
            .clone();
        assert_eq!(requested.borrow().as_slice(), &[expected]);
        assert_eq!(
            dialog.get_result_state(),
            0,
            "native status replaces guidance"
        );
        assert!(controller.borrow().task.is_none());
    }
}

#[test]
fn adding_child_of_disconnected_server_selects_parent_and_keeps_all_exclusions() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let parent =
        source_registry::Source::new(3, reconnect_test_path("").into(), "Serveur NAS", None)
            .unwrap();
    let child =
        source_registry::Source::new(0, reconnect_test_path("bouquins").into(), "Livres", None)
            .unwrap();
    {
        let mut c = controller.borrow_mut();
        c.registry.add(parent.clone()).unwrap();
        c.registry.add(child.clone()).unwrap();
        c.registry.disconnect(child.clone()).unwrap();
        c.registry.disconnect(parent.clone()).unwrap();
        let entries = c.registry.entries.clone();
        c.sources.set_registered(&entries);
        c.show_sources(&window);
    }
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(0);
    dialog.set_source_path(reconnect_test_path("bouquins").into());
    let initial_path = window.get_scan_path();
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 22);
    assert!(dialog.get_disconnected_view());
    let selected = dialog.get_selected_disconnected() as usize;
    assert_eq!(
        controller.borrow().registry.disconnected_sources()[selected].path,
        parent.path
    );
    assert_eq!(controller.borrow().registry.disconnected_sources().len(), 2);
    assert_eq!(window.get_volumes().row_count(), 0);
    assert!(!controller.borrow().source_operations.busy());
    assert_eq!(window.get_scan_path(), initial_path);
    assert!(controller.borrow().task.is_none());
    assert!(controller.borrow().pending_auto.is_none());
    // A new child is also blocked, without adding a bookmark hidden by its parent.
    dialog.set_disconnected_view(false);
    dialog.set_source_path(reconnect_test_path("video").into());
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 22);
    assert_eq!(controller.borrow().registry.entries.len(), 2);
}

#[test]
fn late_save_failure_does_not_replace_reconnection_offer_with_source_added() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let temp = TempDir::new().unwrap();
    let policy = temp.path().join("sources-policy");
    let source =
        source_registry::Source::new(3, reconnect_test_path("").into(), "NAS", None).unwrap();
    {
        let mut c = controller.borrow_mut();
        c.registry = source_registry::Registry::open(policy.clone());
        // An isolated directory at the file destination makes the asynchronous
        // save fail deterministically, without changing the user's preferences.
        fs::create_dir(&policy).unwrap();
        c.registry.add(source.clone()).unwrap();
        c.registry.disconnect(source.clone()).unwrap();
        let entries = c.registry.entries.clone();
        c.sources.set_registered(&entries);
        c.show_sources(&window);
    }
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(3);
    dialog.set_source_path(reconnect_test_path("").into());
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 21);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while controller.borrow().source_save_result != Some(false) {
        controller.borrow_mut().poll_source_dialog();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(dialog.get_result_state(), 21);
    dialog.set_disconnected_view(false);
    assert!(!dialog.get_access_status().contains("Source added"));
    assert_eq!(window.get_volumes().row_count(), 0);
    assert!(controller.borrow().registry.is_disconnected(&source.path));
}

#[test]
fn retained_add_dialog_clears_success_after_source_is_disconnected() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(3);
    dialog.set_source_path(reconnect_test_path("").into());
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 6);
    assert_eq!(window.get_volumes().row_count(), 1);
    dialog.invoke_cancel();
    {
        let mut c = controller.borrow_mut();
        let source = c.registry.entries[0].clone();
        // Publish the same confirmed policy change as a main-window disconnect.
        // The Add window is retained but hidden at this point.
        c.registry.disconnect(source).unwrap();
        let entries = c.registry.entries.clone();
        c.sources.set_registered(&entries);
        c.show_sources(&window);
    }
    window.invoke_show_add_source();
    assert_eq!(dialog.get_result_state(), 0);
    assert!(!dialog.get_access_status().contains("Source added"));
    assert_eq!(window.get_volumes().row_count(), 0);
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 21);
    assert!(dialog.get_disconnected_view());
    assert!(controller.borrow().task.is_none());
    assert!(controller.borrow().pending_auto.is_none());
}
