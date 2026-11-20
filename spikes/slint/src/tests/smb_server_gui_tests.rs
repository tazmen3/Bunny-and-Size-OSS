//! Controller fixtures only: no native share discovery or SMB scan is invoked.
#![cfg(windows)]

use super::*;
use sortsize_core::{MetadataValue, volumes::*};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn source_provider_must_stay_idle(controller: &Rc<RefCell<Controller>>) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    controller.borrow_mut().sources =
        crate::volume_view::Sources::with_provider(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(std::io::ErrorKind::PermissionDenied.into())
        }));
    calls
}

fn assert_no_acquisition(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
    let state = controller.borrow();
    assert!(state.task.is_none());
    assert!(state.pending_auto.is_none());
    assert!(state.restored_selection.is_none());
    assert_eq!(state.generation, 0);
    assert!(!state.source_operations.busy());
    assert!(!state.sources.loading());
    assert!(!window.get_busy());
}

fn assert_unprobed_server(controller: &Rc<RefCell<Controller>>) {
    let state = controller.borrow();
    let source = &state.registry.entries[0];
    assert_eq!(source.kind, crate::source_registry::Kind::SmbServer);
    assert!(source.is_server());
    assert_eq!(source.path, std::path::Path::new(r"\\offline.invalid"));
    let discovery = &state.sources.current.as_ref().unwrap().discovery;
    assert_eq!(discovery.capacity_calls, 0);
    assert_eq!(discovery.volumes.len(), 1);
    let volume = &discovery.volumes[0];
    assert_eq!(volume.kind, SourceKind::Remote);
    assert_eq!(volume.availability, VolumeAvailability::NotProbed);
    assert_eq!(volume.capacity.total, MetadataValue::NotCollected);
    assert_eq!(volume.capacity.used, MetadataValue::NotCollected);
    assert!(!crate::volume_view::can_auto_scan(volume));
}

#[test]
fn adding_and_selecting_virtual_server_never_discovers_or_scans_implicitly() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let provider_calls = source_provider_must_stay_idle(&controller);
    let host = crate::workshop::connect(&window, &controller).unwrap();
    let original = window.get_scan_path();
    // Adding a bookmark preserves the old local intention; selecting a remote
    // server then clears its restoration just like selecting a single share.
    let old_local = std::path::PathBuf::from(r"C:\previous-local");
    controller.borrow_mut().last_auto_selection = Some(crate::auto_analysis::LastSelection {
        root: old_local.clone(),
        identity: VolumeIdentity::WindowsPath(old_local.clone()),
        mount: VolumeMount {
            path: old_local.clone(),
            mount_id: None,
            root: None,
        },
    });
    controller.borrow_mut().pending_remember = Some(old_local);
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(3);
    dialog.set_source_path(r"\\offline.invalid\".into());
    dialog.invoke_input_changed();
    assert!(dialog.get_smb_server_only());
    assert!(dialog.get_can_register_source());
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 6); // No preference writer in this session.
    assert_eq!(window.get_scan_path(), original);
    assert_eq!(window.get_volumes().row_count(), 1);
    assert!(
        window
            .get_volumes()
            .row_data(0)
            .unwrap()
            .name
            .contains("NAS")
    );
    assert!(!window.get_volumes().row_data(0).unwrap().has_usage);
    assert_unprobed_server(&controller);
    assert_no_acquisition(&window, &controller);
    assert!(controller.borrow().last_auto_selection.is_some());

    window.invoke_workshop_source(window.get_storage_token(), 0);
    assert!(controller.borrow().last_auto_selection.is_none());
    assert!(controller.borrow().pending_remember.is_none());
    assert!(window.get_smb_server_context());
    assert!(window.get_smb_server_visible());
    assert_eq!(window.get_scan_path(), r"\\offline.invalid");
    assert_eq!(window.get_current_path(), r"\\offline.invalid");
    assert_eq!(window.get_smb_share_rows().row_count(), 0);
    assert!(
        window
            .get_smb_server_summary()
            .contains("Discover shares explicitly")
    );
    let token = window.get_smb_server_token();
    for _ in 0..8 {
        window.invoke_smb_server_home();
        host.borrow_mut().poll(&window, &controller.borrow());
        assert_eq!(window.get_smb_server_token(), token);
        assert_no_acquisition(&window, &controller);
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_unprobed_server(&controller);
}

#[test]
fn persisted_server_restores_and_opens_without_renewing_network_authorization() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let fixture = TempDir::new().unwrap();
    let store = fixture.path().join("added-sources-v1.conf");
    {
        let mut registry = crate::source_registry::Registry::open(store.clone());
        registry
            .add(
                crate::source_registry::Source::new(
                    3,
                    "offline.invalid".into(),
                    "NAS hors ligne",
                    None,
                )
                .unwrap(),
            )
            .unwrap();
    } // Flush the bounded atomic writer before simulating a fresh session.
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let provider_calls = source_provider_must_stay_idle(&controller);
    {
        let mut state = controller.borrow_mut();
        state.registry = crate::source_registry::Registry::open(store);
        let bookmarks = state.registry.entries.clone();
        state.sources.set_registered(&bookmarks);
        state.show_sources(&window);
    }
    let host = crate::workshop::connect(&window, &controller).unwrap();
    assert_eq!(window.get_volumes().row_count(), 1);
    assert!(
        window
            .get_volumes()
            .row_data(0)
            .unwrap()
            .name
            .contains("NAS hors ligne")
    );
    assert_unprobed_server(&controller);
    assert_no_acquisition(&window, &controller);
    window.invoke_workshop_source(window.get_storage_token(), 0);
    host.borrow_mut().poll(&window, &controller.borrow());
    assert!(window.get_smb_server_context());
    assert!(window.get_smb_server_visible());
    assert_eq!(window.get_smb_share_rows().row_count(), 0);
    assert_no_acquisition(&window, &controller);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn server_and_share_dialog_modes_do_not_reinterpret_each_others_roots() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let provider_calls = source_provider_must_stay_idle(&controller);
    let _host = crate::workshop::connect(&window, &controller).unwrap();
    window.invoke_show_add_source();
    let dialog = controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    dialog.set_source_kind(0);
    dialog.set_source_path(r"\\offline.invalid\".into());
    dialog.invoke_input_changed();
    assert!(!dialog.get_can_register_source());
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 7);
    assert!(controller.borrow().registry.entries.is_empty());

    dialog.set_source_kind(3);
    dialog.set_source_path(r"\\offline.invalid\TV".into());
    dialog.invoke_input_changed();
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 4);
    assert!(controller.borrow().registry.entries.is_empty());

    dialog.set_source_kind(0);
    dialog.invoke_input_changed();
    dialog.invoke_add_source();
    assert_eq!(dialog.get_result_state(), 6);
    assert_eq!(controller.borrow().registry.entries.len(), 1);
    assert_eq!(
        controller.borrow().registry.entries[0].kind,
        crate::source_registry::Kind::Smb
    );
    assert_eq!(
        controller.borrow().registry.entries[0].path,
        std::path::Path::new(r"\\offline.invalid\TV")
    );
    assert_no_acquisition(&window, &controller);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
}
