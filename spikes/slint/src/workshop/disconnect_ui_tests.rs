//! Controller integration on synthetic native/URI sources; no NAS discovery or scan.
#![cfg(any(windows, target_os = "linux"))]
use super::*;
use slint::platform::{
    Key, Platform, WindowAdapter, WindowEvent,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct IndependentWindows;
impl Platform for IndependentWindows {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
    }
}
struct Fixture {
    window: SortSizeWindow,
    controller: Rc<RefCell<Controller>>,
    host: Rc<RefCell<Host>>,
    _temp: TempDir,
    config: PathBuf,
}
impl Fixture {
    fn new(sources: &[Source]) -> Self {
        Self::with_exclusions(sources, &[])
    }
    fn with_exclusions(sources: &[Source], excluded: &[Source]) -> Self {
        slint::platform::set_platform(Box::new(IndependentWindows)).unwrap();
        let temp = TempDir::new().unwrap();
        let config = temp.path().join("sources-policy");
        let (window, controller) =
            crate::new_session(Default::default(), Some("en".into())).unwrap();
        {
            let mut c = controller.borrow_mut();
            c.registry = source_registry::Registry::open(config.clone());
            for source in sources {
                c.registry.add(source.clone()).unwrap();
            }
            for source in excluded {
                c.registry.disconnect(source.clone()).unwrap();
            }
            let entries = c.registry.entries.clone();
            c.sources.set_registered(&entries);
            c.show_sources(&window);
        }
        let host = crate::workshop::connect(&window, &controller).unwrap();
        // These controller tests never contact the synthetic network addresses.
        // Native Win32 behavior has its own injected adapter tests.
        host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(|_, _| Ok(())));
        Self {
            window,
            controller,
            host,
            _temp: temp,
            config,
        }
    }
    fn index(&self, source: &Source) -> i32 {
        self.controller
            .borrow()
            .sources
            .current
            .as_ref()
            .unwrap()
            .discovery
            .volumes
            .iter()
            .position(|volume| volume.mounts.iter().any(|mount| mount.path == source.path))
            .unwrap() as i32
    }
    fn visible(&self, source: &Source) -> bool {
        self.window
            .get_volumes()
            .iter()
            .any(|row| row.path.as_str() == label(&source.path))
    }
    fn disconnected_dialog(&self) -> crate::AddSourceWindow {
        crate::show_add_source(&self.window, &self.controller);
        let dialog = self
            .controller
            .borrow()
            .source_dialog
            .as_ref()
            .unwrap()
            .clone_strong();
        dialog.set_disconnected_view(true);
        dialog
    }
    fn action(&self, source: &Source, reconnect: bool) {
        if reconnect
            && self
                .controller
                .borrow()
                .registry
                .is_disconnected(&source.path)
        {
            let dialog = self.disconnected_dialog();
            let index = self
                .controller
                .borrow()
                .registry
                .disconnected_sources()
                .iter()
                .position(|entry| entry.path == source.path)
                .unwrap();
            dialog.set_selected_disconnected(index as i32);
            dialog.invoke_reconnect_source(index as i32);
        } else {
            self.window.invoke_source_context_action(
                self.window.get_storage_token(),
                self.index(source),
                reconnect,
            );
        }
        if reconnect {
            self.wait_native();
        }
    }
    fn wait_native(&self) {
        let start = Instant::now();
        while self.host.borrow().disconnect.native.is_some() {
            self.host
                .borrow_mut()
                .poll_disconnect(&self.window, &mut self.controller.borrow_mut());
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "native result not published"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn discover_smb_shares(&self, server: &Source, shares: &[Source]) {
        {
            let mut host = self.host.borrow_mut();
            host.select_smb_server(&self.window, server.path.clone());
            host.server.discovery_fixture =
                Some(shares.iter().map(|source| source.path.clone()).collect());
        }
        self.window.invoke_smb_server_discover();
        self.wait_smb_discovery();
    }
    fn wait_smb_discovery(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut host = self.host.borrow_mut();
            host.poll_smb_server(&self.window, false);
            if !host.server.operations.busy() {
                return;
            }
            assert!(Instant::now() < deadline, "SMB discovery not published");
            drop(host);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn open(&self, source: &Source) -> NetworkDisconnectWindow {
        self.action(source, false);
        let start = Instant::now();
        loop {
            self.host
                .borrow_mut()
                .poll_disconnect(&self.window, &mut self.controller.borrow_mut());
            if let Some(dialog) = self.host.borrow().disconnect.dialog.as_ref() {
                return dialog.clone_strong();
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "confirmation not published"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn save(&self) {
        let start = Instant::now();
        loop {
            if let Some(success) = self.controller.borrow().registry.poll_saved() {
                assert!(success);
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "policy not saved");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn idle(&self) {
        let c = self.controller.borrow();
        let host = self.host.borrow();
        assert!(c.task.is_none());
        assert!(c.pending_auto.is_none());
        assert!(c.restored_selection.is_none());
        assert_eq!(c.generation, 0);
        assert!(host.requested.is_none());
        assert_eq!(host.epoch, 0);
        assert!(!self.window.get_busy());
        let lane = host.service.lane.lock().unwrap();
        assert!(lane.start.is_none());
        assert!(lane.smb_begin.is_none());
        assert!(!lane.writer_running);
    }
    fn scan_local_fixture(&self, root: &Path) {
        self.window.set_scan_path(
            self.controller
                .borrow_mut()
                .path_input
                .select(root.to_path_buf())
                .into(),
        );
        self.host.borrow_mut().start(
            &self.window,
            &mut self.controller.borrow_mut(),
            Start {
                root: root.to_path_buf(),
                explicit_root: false,
                observation: crate::workshop::service::size_observation(),
                preflight: None,
                options: Default::default(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.host
                .borrow_mut()
                .poll(&self.window, &self.controller.borrow());
            if self
                .host
                .borrow()
                .current
                .as_ref()
                .is_some_and(|page| !page.busy && page.measure_ready)
                && {
                    let host = self.host.borrow();
                    let lane = host.service.lane.lock().unwrap();
                    lane.reader_active || lane.direct_view == Some(lane.source_epoch)
                }
            {
                return;
            }
            assert!(Instant::now() < deadline, "local G0 was not displayed");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
fn share(name: &str) -> Source {
    Source::new(
        0,
        PathBuf::from(format!(r"\\example.invalid\{name}")),
        name,
        None,
    )
    .unwrap()
}

#[test]
fn acknowledged_cancellation_replaces_scanning_status_without_hiding_disconnect_diagnostic() {
    let fixture = Fixture::new(&[share("TV")]);
    fixture.window.set_status("Scanning".into());
    fixture
        .window
        .set_errors("Source disconnected and saved".into());
    fixture.window.set_busy(true);
    fixture.window.set_cancelling(true);
    {
        let host = fixture.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.phase = Some((0, service::Phase::Cancelled, GenerationId(1)));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        fixture
            .host
            .borrow_mut()
            .poll(&fixture.window, &fixture.controller.borrow());
        if !fixture.window.get_busy() && fixture.window.get_status() != "Scanning" {
            break;
        }
        // The product poll deliberately uses try_lock: a worker owning Lane
        // skips one UI observation, and the normal timer consumes the next.
        assert!(
            Instant::now() < deadline,
            "cancelled phase was not consumed by the UI"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_ne!(fixture.window.get_status(), "Scanning");
    assert!(fixture.window.get_status().contains("G1"));
    assert_eq!(fixture.window.get_errors(), "Source disconnected and saved");
    assert!(!fixture.window.get_busy());
    assert!(!fixture.window.get_cancelling());
}
fn key(dialog: &NetworkDisconnectWindow, key: Key) {
    dialog
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    dialog
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    dialog
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn disconnect_cancel_escape_close_enter_and_stale_tokens_are_noops() {
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    fixture.save();
    let initial_bytes = std::fs::read(&fixture.config).unwrap();
    let initial_status = fixture.window.get_status();
    fixture.window.invoke_source_context_action(
        fixture.window.get_storage_token() - 1,
        fixture.index(&source),
        false,
    );
    assert!(fixture.host.borrow().disconnect.native.is_none());
    assert!(fixture.host.borrow().disconnect.dialog.is_none());
    for choice in 0..4 {
        let dialog = fixture.open(&source);
        let token = dialog.get_request_token();
        assert_eq!(dialog.get_source_name(), "TV");
        assert_eq!(
            dialog.get_source_path().as_str(),
            source.path.to_string_lossy()
        );
        assert!(!dialog.get_disconnect_windows());
        assert!(!dialog.get_windows_option_available());
        assert!(!dialog.get_disconnect_linux());
        assert!(!dialog.get_linux_option_available());
        dialog.invoke_accepted(token - 1, true);
        assert!(
            !fixture
                .controller
                .borrow()
                .registry
                .is_disconnected(&source.path)
        );
        match choice {
            0 => dialog.invoke_rejected(token),
            1 => key(&dialog, Key::Escape),
            2 => dialog.window().dispatch_event(WindowEvent::CloseRequested),
            _ => key(&dialog, Key::Return),
        }
        assert!(fixture.host.borrow().disconnect.dialog.is_none());
        dialog.invoke_accepted(token, true); // An abandoned window cannot be reused.
        assert!(
            !fixture
                .controller
                .borrow()
                .registry
                .is_disconnected(&source.path)
        );
        assert!(
            !fixture
                .host
                .borrow()
                .service
                .source_disconnected(&source.path)
        );
        assert_eq!(fixture.window.get_status(), initial_status);
        assert!(fixture.visible(&source));
        assert_eq!(std::fs::read(&fixture.config).unwrap(), initial_bytes);
        fixture.idle();
    }
}

#[test]
fn confirmed_disconnect_persists_refuses_scan_and_reconnect_never_starts() {
    let source = share("TV");
    let other = share("Photos");
    let fixture = Fixture::new(&[source.clone(), other.clone()]);
    fixture.save();
    let dialog = fixture.open(&source);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(!fixture.visible(&source));
    assert!(fixture.visible(&other));
    assert!(
        fixture
            .window
            .get_volumes()
            .iter()
            .all(|row| !row.disconnected)
    );
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&source.path)
    );
    assert!(
        fixture
            .host
            .borrow()
            .service
            .source_disconnected(&source.path)
    );
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&other.path)
    );
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&other.path)
    );
    fixture.save();
    let restored = source_registry::Registry::open(fixture.config.clone());
    assert!(restored.is_disconnected(&source.path));
    assert_eq!(restored.entries.len(), 2);
    {
        let mut controller = fixture.controller.borrow_mut();
        let entries = restored.entries.clone();
        controller.sources.set_registered(&entries);
        controller.show_sources(&fixture.window);
    }
    assert!(!fixture.visible(&source));
    assert!(fixture.visible(&other));
    fixture
        .window
        .set_scan_path(source.path.to_str().unwrap().into());
    fixture.window.invoke_start_scan();
    fixture.window.invoke_workshop_retry();
    assert!(fixture.window.get_errors().contains("Reconnect"));
    fixture.idle();
    fixture.action(&source, true);
    assert!(fixture.visible(&source));
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&source.path)
    );
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&source.path)
    );
    assert!(fixture.window.get_status().contains("Click Scan"));
    fixture.idle();
    fixture.save();
    assert!(!source_registry::Registry::open(fixture.config.clone()).is_disconnected(&source.path));
}

#[test]
fn server_reconnect_preserves_explicit_child_exclusion_and_other_server() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let tv = share("TV");
    let photos = share("Photos");
    let external = Source::new(0, r"\\other.example.invalid\TV".into(), "Other", None).unwrap();
    let fixture = Fixture::new(&[server.clone(), tv.clone(), photos.clone(), external.clone()]);
    let dialog = fixture.open(&tv);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(!fixture.visible(&tv));
    assert!(fixture.visible(&photos));
    assert!(fixture.visible(&server));
    let dialog = fixture.open(&server);
    assert_eq!(dialog.get_affected_share_count(), 2);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(!fixture.visible(&server));
    assert!(!fixture.visible(&tv));
    assert!(!fixture.visible(&photos));
    assert!(fixture.visible(&external));
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&photos.path)
    );
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&external.path)
    );
    fixture.save();
    let disconnected_policy = std::fs::read(&fixture.config).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(move |_, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    fixture.action(&tv, true);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&fixture.config).unwrap(), disconnected_policy);
    let source_dialog = fixture
        .controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    assert!(
        source_dialog
            .get_reconnect_status()
            .contains("Reconnect the server first")
    );
    assert!(
        fixture
            .window
            .get_errors()
            .contains("individually disconnected shares")
    );
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&tv.path)
    );
    fixture.action(&server, true);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(fixture.visible(&server));
    assert!(fixture.visible(&photos));
    assert!(!fixture.visible(&tv));
    assert!(fixture.visible(&external));
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&photos.path)
    );
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&tv.path)
    );
    assert!(fixture.host.borrow().service.source_disconnected(&tv.path));
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&photos.path)
    );
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&external.path)
    );
    fixture.idle();
    fixture.save();
    let restored = source_registry::Registry::open(fixture.config.clone());
    assert!(!restored.is_disconnected(&server.path));
    assert!(restored.find(&tv.path).unwrap().disconnected);
    assert!(!restored.is_disconnected(&photos.path));
}

#[test]
fn reconnect_preserves_remaining_legacy_alias_exclusion_without_claiming_success() {
    let original = share("TV");
    let alias = share("tv");
    assert_ne!(original.path, alias.path);
    assert!(source_registry::covers(&original.path, &alias.path));
    assert!(source_registry::covers(&alias.path, &original.path));
    let fixture = Fixture::new(&[original.clone(), alias.clone()]);
    {
        // Model a durable older policy with two exact user choices for the
        // same authorization scope; adding a source no longer creates these.
        let mut controller = fixture.controller.borrow_mut();
        controller.registry.disconnect(original.clone()).unwrap();
        controller.registry.disconnect(alias.clone()).unwrap();
        let entries = controller.registry.entries.clone();
        controller.sources.set_registered(&entries);
        controller.show_sources(&fixture.window);
    }
    fixture
        .host
        .borrow()
        .service
        .disconnect_sources(&[original.path.clone(), alias.path.clone()]);
    fixture.save();
    let initial_status = fixture.window.get_status();

    fixture.action(&original, true);
    fixture.save();
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .find(&original.path)
            .unwrap()
            .disconnected
    );
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .find(&alias.path)
            .unwrap()
            .disconnected
    );
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&original.path)
    );
    assert!(
        fixture
            .host
            .borrow()
            .service
            .source_disconnected(&original.path)
    );
    assert!(!fixture.visible(&original));
    assert!(!fixture.visible(&alias));
    assert_eq!(fixture.window.get_status(), initial_status);
    let dialog = fixture
        .controller
        .borrow()
        .source_dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    assert_eq!(dialog.get_result_state(), 21);
    assert_eq!(dialog.get_selected_disconnected(), 0);
    assert_eq!(dialog.get_disconnected_sources().row_count(), 1);
    assert!(dialog.get_access_status().contains("Click Reconnect"));
    assert!(!dialog.get_reconnect_status().contains("reconnected"));
    fixture.idle();

    fixture.action(&alias, true);
    fixture.save();
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&original.path)
    );
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&original.path)
    );
    assert!(fixture.visible(&original));
    assert!(fixture.visible(&alias));
    fixture.idle();
}

#[test]
fn reconnect_native_success_cannot_clear_child_after_parent_is_disconnected() {
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let child = share("TV");
    let sibling = share("Photos");
    let fixture = Fixture::new(&[server.clone(), child.clone(), sibling.clone()]);
    let confirmation = fixture.open(&child);
    confirmation.invoke_accepted(confirmation.get_request_token(), false);
    fixture.save();

    let (entered, started) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let held = Mutex::new(held);
    fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(move |_, _| {
        entered.send(()).unwrap();
        held.lock().unwrap().recv().unwrap();
        Ok(())
    }));
    let dialog = fixture.disconnected_dialog();
    dialog.invoke_reconnect_source(0);
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    // Exercise the publication boundary independently of the UI's single-flight
    // guard: a late native response is only evidence of access, never authority.
    fixture
        .controller
        .borrow_mut()
        .registry
        .disconnect(server.clone())
        .unwrap();
    fixture
        .host
        .borrow()
        .service
        .disconnect_sources(std::slice::from_ref(&server.path));
    release.send(()).unwrap();
    fixture.wait_native();
    fixture.save();

    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .find(&child.path)
            .unwrap()
            .disconnected
    );
    assert!(
        fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&server.path)
    );
    assert!(
        fixture
            .host
            .borrow()
            .service
            .source_disconnected(&child.path)
    );
    assert!(!fixture.visible(&child));
    assert!(!fixture.visible(&sibling));
    assert!(
        dialog
            .get_reconnect_status()
            .contains("Reconnect the server first")
    );
    assert!(!dialog.get_busy());
    assert!(!fixture.controller.borrow().source_operations.busy());
    fixture.idle();

    fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(|_, _| Ok(())));
    fixture.action(&server, true);
    assert!(fixture.visible(&server));
    assert!(fixture.visible(&sibling));
    assert!(!fixture.visible(&child));
    fixture.idle();
}

#[test]
fn active_nas_tree_hides_disabled_share_and_remaps_context_actions() {
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let tv = share("TV");
    let photos = share("Photos");
    let fixture = Fixture::new(&[server.clone(), tv.clone(), photos.clone()]);
    {
        let mut host = fixture.host.borrow_mut();
        host.select_smb_server(&fixture.window, server.path.clone());
        host.server.shares = vec![tv.path.clone(), photos.path.clone()];
        host.server.selected = vec![true, true];
        host.server.memory = sortsize_core::resources::Governor::global().reserve(
            sortsize_core::resources::Category::Publications,
            64 * 1024,
            None,
        );
        host.server.dirty = true;
        host.poll_smb_server(&fixture.window, true);
    }
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 2);
    let dialog = fixture.open(&tv);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 1);
    let summary = fixture.window.get_smb_server_summary();
    assert!(summary.contains("Visible shares: 1"), "{summary}");
    assert!(
        summary.contains("Shares disconnected in Bunny: 1"),
        "{summary}"
    );
    assert_eq!(
        fixture
            .window
            .get_smb_share_rows()
            .row_data(0)
            .unwrap()
            .path,
        label(&photos.path)
    );
    assert!(fixture.window.get_smb_server_visible());
    assert!(fixture.visible(&photos));

    // The remaining row has visible index zero, while its frozen discovery
    // index is one. Its context action must still target Photos.
    let token = fixture.host.borrow().server.token;
    fixture
        .window
        .invoke_smb_share_context_action(token, 0, false);
    fixture.wait_native();
    let dialog = fixture
        .host
        .borrow()
        .disconnect
        .dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    assert_eq!(dialog.get_source_path(), label(&photos.path));
    dialog.invoke_rejected(dialog.get_request_token());
    assert!(fixture.visible(&photos));
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 1);

    let dialog = fixture.open(&server);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 0);
    assert!(!fixture.window.get_smb_server_visible());
    assert!(!fixture.window.get_smb_server_context());
    assert!(fixture.window.get_current_path().is_empty());
    assert!(fixture.window.get_scan_path().is_empty());
    assert!(!fixture.visible(&server));
    assert!(!fixture.visible(&photos));
    let host = fixture.host.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
}

#[test]
fn nas_disconnected_rows_reconnect_only_the_displayed_share_without_scanning() {
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let shares = [
        "bouquins",
        "home",
        "homes",
        "PlexMediaServer",
        "TV",
        "video",
    ]
    .map(share);
    let homes = &shares[2];
    let tv = &shares[4];
    let tv_alias = share("tv");
    let absent = share("not-discovered");
    let fixture = Fixture::with_exclusions(
        std::slice::from_ref(&server),
        &[homes.clone(), tv_alias.clone(), absent.clone()],
    );
    fixture.save();
    let initial_policy = std::fs::read(&fixture.config).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(move |source, _| {
        let mut paths = observed.lock().unwrap();
        paths.push(source.path.clone());
        if paths.len() == 1 {
            Err(ConnectionState::AccessDenied)
        } else {
            Ok(())
        }
    }));

    let visible = || fixture.window.get_smb_share_rows().row_count();
    let disconnected = || {
        fixture
            .window
            .get_smb_disconnected_share_rows()
            .iter()
            .map(|row| row.name.to_string())
            .collect::<Vec<_>>()
    };
    let reconnect = |token, index| {
        fixture
            .window
            .invoke_smb_disconnected_share_reconnect(token, index);
    };
    let token = || fixture.window.get_smb_disconnected_share_token();

    fixture.discover_smb_shares(&server, &shares);
    assert_eq!(visible(), 4);
    assert_eq!(disconnected(), ["homes", "TV"]);
    assert!(calls.lock().unwrap().is_empty());
    fixture.idle();

    let first_token = token();
    reconnect(first_token, -1);
    reconnect(first_token, 2);
    fixture.window.set_smb_server_visible(false);
    reconnect(first_token, 1);
    fixture.window.set_smb_server_visible(true);
    assert!(fixture.host.borrow().disconnect.native.is_none());
    reconnect(first_token, 1);
    fixture.wait_native();
    assert_eq!(*calls.lock().unwrap(), [tv_alias.path.clone()]);
    assert!(fixture.window.get_errors().contains("Access denied"));
    assert_eq!(visible(), 4);
    assert_eq!(disconnected(), ["homes", "TV"]);
    assert_eq!(std::fs::read(&fixture.config).unwrap(), initial_policy);
    fixture.idle();

    let mut reversed = shares.clone();
    reversed.reverse();
    fixture.discover_smb_shares(&server, &reversed);
    assert_eq!(disconnected(), ["TV", "homes"]);
    assert_ne!(token(), first_token);
    // The old TV button had index one. In the new projection that index is homes.
    reconnect(first_token, 1);
    assert!(fixture.host.borrow().disconnect.native.is_none());
    assert_eq!(calls.lock().unwrap().len(), 1);

    fixture.window.invoke_start_scan();
    fixture.wait_smb_discovery();
    let dialog = fixture
        .host
        .borrow()
        .server
        .dialog
        .as_ref()
        .unwrap()
        .clone_strong();
    assert_eq!(dialog.get_selected_count(), 4);
    let paths: Vec<_> = dialog.get_shares().iter().map(|row| row.path).collect();
    assert_eq!(paths.len(), 4);
    assert!(!paths.contains(&label(&homes.path).into()));
    assert!(!paths.contains(&label(&tv.path).into()));
    dialog.invoke_cancel(dialog.get_token());
    fixture.wait_smb_discovery();
    fixture.idle();

    let tv_token = token();
    reconnect(tv_token, 0);
    fixture.wait_native();
    fixture.save();
    assert_eq!(visible(), 5);
    assert_eq!(disconnected(), ["homes"]);
    assert!(
        fixture
            .window
            .get_smb_share_rows()
            .iter()
            .any(|row| { row.name == "TV" && row.state == "Unknown" && row.size == "—" })
    );
    let restored = source_registry::Registry::open(fixture.config.clone());
    assert!(!restored.is_disconnected(&tv.path));
    assert!(restored.is_disconnected(&homes.path));
    assert!(restored.is_disconnected(&absent.path));
    assert_eq!(calls.lock().unwrap().len(), 2);
    fixture.idle();

    // After restoring TV, index zero now belongs to homes.
    assert_ne!(token(), tv_token);
    reconnect(tv_token, 0);
    assert!(fixture.host.borrow().disconnect.native.is_none());
    assert_eq!(calls.lock().unwrap().len(), 2);
    reconnect(token(), 0);
    fixture.wait_native();
    fixture.save();
    assert_eq!(visible(), 6);
    assert!(disconnected().is_empty());
    assert_eq!(
        *calls.lock().unwrap(),
        [
            tv_alias.path.clone(),
            tv_alias.path.clone(),
            homes.path.clone()
        ]
    );
    let restored = source_registry::Registry::open(fixture.config.clone());
    assert!(!restored.is_disconnected(&homes.path));
    assert!(restored.is_disconnected(&absent.path));
    assert!(fixture.host.borrow().service.smb_snapshot().is_none());
    fixture.idle();
}

#[test]
fn nas_disconnected_row_cannot_clear_an_exclusion_under_a_disconnected_parent() {
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let tv = share("TV");
    let photos = share("Photos");
    let fixture =
        Fixture::with_exclusions(&[server.clone(), photos.clone()], std::slice::from_ref(&tv));
    fixture.discover_smb_shares(&server, &[tv.clone(), photos.clone()]);
    assert!(fixture.visible(&photos));
    let token = fixture.window.get_smb_disconnected_share_token();
    assert_eq!(
        fixture.window.get_smb_disconnected_share_rows().row_count(),
        1
    );

    let (entered, started) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let held = Mutex::new(held);
    fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(move |source, _| {
        entered.send(source.path.clone()).unwrap();
        held.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        Ok(())
    }));
    fixture
        .window
        .invoke_smb_disconnected_share_reconnect(token, 0);
    assert_eq!(
        started.recv_timeout(Duration::from_secs(5)).unwrap(),
        tv.path
    );
    // Revoke the parent after the worker starts, before publishing native success.
    fixture
        .controller
        .borrow_mut()
        .registry
        .disconnect(server.clone())
        .unwrap();
    fixture
        .host
        .borrow()
        .service
        .disconnect_sources(std::slice::from_ref(&server.path));
    release.send(()).unwrap();
    fixture.wait_native();
    fixture.save();
    assert!(
        fixture
            .window
            .get_errors()
            .contains("Reconnect the server first")
    );
    assert!(fixture.host.borrow().service.source_disconnected(&tv.path));
    assert!(!fixture.visible(&photos));
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 0);
    fixture.window.invoke_smb_disconnected_share_reconnect(
        fixture.window.get_smb_disconnected_share_token(),
        0,
    );
    assert!(fixture.host.borrow().disconnect.native.is_none());
    let restored = source_registry::Registry::open(fixture.config.clone());
    assert!(restored.find(&tv.path).unwrap().disconnected);
    assert!(restored.find(&server.path).unwrap().disconnected);
    fixture.idle();
}

#[test]
fn disconnecting_visible_server_neutralizes_even_when_a_local_page_is_hidden_behind_it() {
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let tv = share("TV");
    let fixture = Fixture::new(&[server.clone(), tv.clone()]);
    let local = fixture._temp.path().join("local-behind-server");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("local-file"), [1; 11]).unwrap();
    fixture.scan_local_fixture(&local);
    {
        let mut host = fixture.host.borrow_mut();
        host.select_smb_server(&fixture.window, server.path.clone());
        host.server.shares = vec![tv.path.clone()];
        host.server.selected = vec![true];
        host.server.dirty = true;
        host.poll_smb_server(&fixture.window, true);
        assert_eq!(host.current.as_ref().unwrap().path, label(&local));
    }
    assert!(fixture.window.get_smb_server_visible());
    assert_eq!(fixture.window.get_current_path(), label(&server.path));
    let dialog = fixture.open(&server);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(!fixture.window.get_smb_server_visible());
    assert!(!fixture.window.get_smb_server_context());
    assert!(fixture.window.get_current_path().is_empty());
    assert!(fixture.window.get_scan_path().is_empty());
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 0);
    assert_eq!(fixture.window.get_rows().row_count(), 0);
    assert_eq!(fixture.window.get_details().row_count(), 0);
    assert!(fixture.host.borrow().current.is_none());
    assert!(fixture.host.borrow().requested.is_none());
    assert!(!fixture.visible(&server));
    assert!(!fixture.visible(&tv));
    let host = fixture.host.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
}

#[test]
fn neutral_view_observes_an_independent_local_writer_finishing_without_reopening_it() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Release(Option<mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let fixture = Fixture::new(std::slice::from_ref(&server));
    let local = fixture._temp.path().join("independent-writer");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("local-file"), [1; 19]).unwrap();
    let (sender, receiver) = mpsc::channel();
    let release = Release(Some(sender));
    let entered = Arc::new(AtomicBool::new(false));
    let signal = entered.clone();
    fixture.window.set_scan_path(label(&local).into());
    fixture.host.borrow_mut().start(
        &fixture.window,
        &mut fixture.controller.borrow_mut(),
        Start {
            root: local.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: Some(Box::new(move || {
                signal.store(true, Ordering::Release);
                receiver.recv_timeout(Duration::from_secs(30)).is_ok()
            })),
            options: Default::default(),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "local preparation did not start");
        std::thread::sleep(Duration::from_millis(2));
    }
    let (local_epoch, cancellation) = {
        let host = fixture.host.borrow();
        let lane = host.service.lane.lock().unwrap();
        (
            lane.source_epoch,
            lane.preparing_source.as_ref().unwrap().1.clone(),
        )
    };
    assert!(fixture.window.get_busy());
    fixture
        .window
        .invoke_workshop_source(fixture.window.get_storage_token(), fixture.index(&server));
    assert!(fixture.window.get_smb_server_visible());
    let dialog = fixture.open(&server);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(!cancellation.is_cancelled());
    assert!(fixture.host.borrow().requested.is_none());
    assert!(fixture.host.borrow().current.is_none());
    assert!(fixture.window.get_current_path().is_empty());
    assert!(!fixture.host.borrow().service.source_disconnected(&local));
    assert!(fixture.window.get_busy());
    drop(release);

    loop {
        fixture
            .host
            .borrow_mut()
            .poll(&fixture.window, &fixture.controller.borrow());
        assert!(fixture.host.borrow().current.is_none());
        assert!(fixture.window.get_current_path().is_empty());
        assert_eq!(fixture.window.get_rows().row_count(), 0);
        assert_eq!(fixture.window.get_details().row_count(), 0);
        let completed = {
            let host = fixture.host.borrow();
            let lane = host.service.lane.lock().unwrap();
            !lane.writer_running
                && matches!(lane.phase, Some((epoch, service::Phase::Ready, _)) if epoch == local_epoch)
        };
        if completed && !fixture.window.get_busy() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "independent completion stayed busy"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!cancellation.is_cancelled());
    assert!(!fixture.window.get_busy());
    assert!(!fixture.window.get_cancelling());
    let host = fixture.host.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert_eq!(lane.source_epoch, local_epoch);
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
    assert!(lane.smb_snapshot.is_none());
}

#[test]
fn disconnecting_hidden_share_keeps_the_visible_parent_server_and_its_neighbor() {
    let server = Source::new(3, r"\\example.invalid\".into(), "NAS", None).unwrap();
    let tv = share("TV");
    let photos = share("Photos");
    let fixture = Fixture::new(&[server.clone(), tv.clone(), photos.clone()]);
    let local = fixture._temp.path().join("hidden-share-page");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("file"), [2; 13]).unwrap();
    fixture.scan_local_fixture(&local);
    {
        let mut host = fixture.host.borrow_mut();
        // Only the presentation identity is injected: the real local view remains a
        // bounded recorded G0 and no synthetic NAS address is ever contacted.
        host.current.as_mut().unwrap().path = label(&tv.path);
        host.select_smb_server(&fixture.window, server.path.clone());
        host.server.shares = vec![tv.path.clone(), photos.path.clone()];
        host.server.selected = vec![true, true];
        host.server.dirty = true;
        host.poll_smb_server(&fixture.window, true);
        assert_eq!(host.current.as_ref().unwrap().path, label(&tv.path));
    }
    let original_epoch = fixture.host.borrow().epoch;
    let dialog = fixture.open(&tv);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(fixture.window.get_smb_server_visible());
    assert!(fixture.window.get_smb_server_context());
    assert_eq!(fixture.window.get_current_path(), label(&server.path));
    assert_eq!(fixture.window.get_scan_path(), label(&server.path));
    assert_eq!(fixture.window.get_smb_share_rows().row_count(), 1);
    assert_eq!(
        fixture
            .window
            .get_smb_share_rows()
            .row_data(0)
            .unwrap()
            .path,
        label(&photos.path)
    );
    assert!(!fixture.visible(&tv));
    assert!(fixture.visible(&server));
    assert!(fixture.visible(&photos));
    assert_eq!(fixture.host.borrow().epoch, original_epoch);
    let host = fixture.host.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
}

#[test]
fn disconnecting_displayed_share_clears_central_view_without_selecting_another_source() {
    let source = share("TV");
    let other = share("Photos");
    let fixture = Fixture::new(&[source.clone(), other.clone()]);
    fixture.host.borrow_mut().requested = Some(source.path.clone());
    fixture.window.set_scan_path(label(&source.path).into());
    fixture.window.set_current_path(label(&source.path).into());
    fixture.window.set_rows(
        Rc::new(VecModel::from(vec![crate::TreeRow {
            path: label(&source.path).into(),
            name: "Old inventory".into(),
            ..Default::default()
        }]))
        .into(),
    );
    fixture.window.set_details(
        Rc::new(VecModel::from(vec![crate::DetailRow {
            name: "Old file".into(),
            ..Default::default()
        }]))
        .into(),
    );
    let dialog = fixture.open(&source);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(fixture.window.get_scan_path().is_empty());
    assert!(fixture.window.get_current_path().is_empty());
    assert_eq!(fixture.window.get_rows().row_count(), 0);
    assert_eq!(fixture.window.get_details().row_count(), 0);
    assert_eq!(fixture.window.get_selected_volume(), -1);
    assert!(!fixture.visible(&source));
    assert!(fixture.visible(&other));
    let host = fixture.host.borrow();
    assert!(host.requested.is_none());
    let lane = host.service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
    assert!(!fixture.window.get_busy());
}

#[test]
fn sorting_or_keyboard_navigation_cannot_reopen_a_disconnected_recorded_g0() {
    let fixture = Fixture::new(&[]);
    let root = fixture._temp.path().join("network-fixture");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), [1; 17]).unwrap();
    // Inject the registered protocol classification while all acquisition and
    // inventory work uses this bounded local fixture, never a physical NAS.
    let source = Source {
        kind: Kind::Nfs,
        path: root.clone(),
        label: "Fixture share".into(),
        disconnected: false,
    };
    {
        let mut controller = fixture.controller.borrow_mut();
        controller.registry.add(source.clone()).unwrap();
        let entries = controller.registry.entries.clone();
        controller.sources.set_registered(&entries);
        controller.show_sources(&fixture.window);
    }
    fixture.scan_local_fixture(&root);
    assert_eq!(fixture.window.get_details().row_count(), 1);
    let dialog = fixture.open(&source);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    let neutral_epoch = fixture.host.borrow().epoch;
    let neutral_status = fixture.window.get_status();
    let original_sort = fixture.window.get_sort_order();
    fixture.window.invoke_sort_details(true);
    fixture.window.invoke_sort_allocated();
    fixture.window.invoke_sort_owner();
    fixture.window.invoke_sort_dates(false);
    fixture.window.invoke_go_parent();
    fixture.window.invoke_change_page(true, true);
    assert_eq!(fixture.host.borrow().epoch, neutral_epoch);
    assert_eq!(fixture.window.get_sort_order(), original_sort);
    assert_eq!(fixture.window.get_status(), neutral_status);
    assert!(!fixture.window.get_busy());

    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        fixture
            .host
            .borrow_mut()
            .poll(&fixture.window, &fixture.controller.borrow());
        assert!(fixture.host.borrow().current.is_none());
        assert_eq!(fixture.window.get_rows().row_count(), 0);
        assert_eq!(fixture.window.get_details().row_count(), 0);
        assert!(fixture.window.get_current_path().is_empty());
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let host = fixture.host.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert!(lane.navigation.is_none());
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
}

#[test]
fn disconnecting_a_selected_network_source_preserves_the_independent_local_view() {
    let network = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&network));
    let local = fixture._temp.path().join("local");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("local-file"), [2; 23]).unwrap();
    fixture.scan_local_fixture(&local);
    let original_path = fixture.window.get_current_path();
    let original_epoch = fixture.host.borrow().epoch;
    let original_generation = fixture
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context
        .generation;
    // Network selection changes the prospective path, while P1-01 keeps the
    // completed local inventory visible until an explicit new scan.
    fixture
        .window
        .invoke_workshop_source(fixture.window.get_storage_token(), fixture.index(&network));
    assert_eq!(fixture.window.get_scan_path(), label(&network.path));
    assert_eq!(fixture.window.get_current_path(), original_path);
    let dialog = fixture.open(&network);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(!fixture.visible(&network));
    assert!(fixture.window.get_scan_path().is_empty());
    assert_eq!(fixture.window.get_current_path(), original_path);
    assert_eq!(fixture.window.get_details().row_count(), 1);
    let host = fixture.host.borrow();
    assert_eq!(host.requested.as_ref(), Some(&local));
    assert_eq!(host.epoch, original_epoch);
    assert_eq!(
        host.current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .generation,
        original_generation
    );
    let lane = host.service.lane.lock().unwrap();
    assert!(lane.reader_active || lane.direct_view == Some(lane.source_epoch));
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
}

#[test]
fn reconnect_is_explicit_and_failures_keep_the_durable_exclusion_and_inventory_intentions() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    fixture.save();
    let dialog = fixture.open(&source);
    dialog.invoke_accepted(dialog.get_request_token(), false);
    fixture.save();
    let excluded_bytes = std::fs::read(&fixture.config).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    for state in [
        ConnectionState::Cancelled,
        ConnectionState::AuthenticationRequired,
        ConnectionState::Unavailable,
        ConnectionState::AccessDenied,
        ConnectionState::SessionConflict,
    ] {
        let counted = calls.clone();
        fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(state)
        }));
        let before = calls.load(Ordering::SeqCst);
        fixture
            .controller
            .borrow_mut()
            .show_sources(&fixture.window);
        assert_eq!(calls.load(Ordering::SeqCst), before);
        fixture.action(&source, true);
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        assert!(
            fixture
                .controller
                .borrow()
                .registry
                .is_disconnected(&source.path)
        );
        assert!(
            fixture
                .host
                .borrow()
                .service
                .source_disconnected(&source.path)
        );
        assert_eq!(std::fs::read(&fixture.config).unwrap(), excluded_bytes);
        assert!(
            fixture
                .window
                .get_errors()
                .contains(if cfg!(target_os = "linux") {
                    "Files"
                } else {
                    "File Explorer"
                })
        );
        assert!(!fixture.visible(&source));
        fixture.idle();
    }
    let restored = source_registry::Registry::open(fixture.config.clone());
    assert!(restored.is_disconnected(&source.path));
    assert_eq!(restored.entries[0].label, source.label);
    assert_eq!(restored.entries[0].path, source.path);
}

#[test]
fn reconnect_during_local_busy_preserves_selection_status_and_scheduler_intentions() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Hold a genuine local preparation instead of fabricating writer_running:
    // the real scheduler is free to clear a synthetic idle-writer flag.
    struct Release(Option<mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    fixture.save();
    let local = fixture._temp.path().join("local-source");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("small.txt"), b"local acquisition").unwrap();
    let (sender, receiver) = mpsc::channel();
    let release = Release(Some(sender));
    let entered = Arc::new(AtomicBool::new(false));
    let signal = entered.clone();
    fixture.host.borrow_mut().start(
        &fixture.window,
        &mut fixture.controller.borrow_mut(),
        Start {
            root: local.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: Some(Box::new(move || {
                signal.store(true, Ordering::Release);
                receiver.recv_timeout(Duration::from_secs(30)).is_ok()
            })),
            options: sortsize_store::scheduler::CollectionOptions {
                allocation: fixture.window.get_collect_allocation(),
                owners: fixture.window.get_collect_owner(),
            },
        },
    );
    let start = Instant::now();
    while !entered.load(Ordering::Acquire) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "local preflight did not start"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let original_path = fixture.window.get_scan_path();
    let original_status = fixture.window.get_status();
    let original_generation = fixture.controller.borrow().generation;
    let original_scan_root = fixture.controller.borrow().scan_root.clone();
    let original_epoch = fixture.host.borrow().epoch;
    let source_epoch = fixture
        .host
        .borrow()
        .service
        .lane
        .lock()
        .unwrap()
        .source_epoch;
    assert!(fixture.window.get_busy());
    fixture.action(&source, true);
    assert!(fixture.window.get_busy());
    assert_eq!(fixture.window.get_status(), original_status);
    assert_eq!(fixture.window.get_scan_path(), original_path);
    assert_eq!(fixture.controller.borrow().generation, original_generation);
    assert_eq!(fixture.controller.borrow().scan_root, original_scan_root);
    assert_eq!(fixture.host.borrow().epoch, original_epoch);
    assert_eq!(
        fixture.host.borrow().requested.as_deref(),
        Some(local.as_path())
    );
    let (new_source_epoch, cancelled, publication_cancelled, has_pending_start, has_smb_start) = {
        let host = fixture.host.borrow();
        let lane = host.service.lane.lock().unwrap();
        (
            lane.source_epoch,
            lane.cancel,
            lane.publication_cancel.is_cancelled(),
            lane.start.is_some(),
            lane.smb_begin.is_some(),
        )
    };
    // Keep assertions outside the lock: failures must not poison Service::drop.
    assert_eq!(new_source_epoch, source_epoch);
    assert!(!cancelled);
    assert!(!publication_cancelled);
    assert!(!has_pending_start);
    assert!(!has_smb_start);
    assert!(fixture.controller.borrow().pending_auto.is_none());
    assert_eq!(
        fixture
            .window
            .get_volumes()
            .row_data(fixture.index(&source) as usize)
            .unwrap()
            .state,
        "Available"
    );
    drop(release);
    let start = Instant::now();
    loop {
        fixture.controller.borrow_mut().poll(&fixture.window);
        fixture
            .host
            .borrow_mut()
            .poll_selection(&fixture.window, &mut fixture.controller.borrow_mut());
        fixture
            .host
            .borrow_mut()
            .poll(&fixture.window, &fixture.controller.borrow());
        let running = fixture
            .host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .writer_running;
        if !fixture.window.get_busy() && !running && fixture.window.get_details().row_count() == 1 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "local acquisition did not finish"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(fixture.host.borrow().epoch, original_epoch);
    assert_eq!(fixture.window.get_details().row_count(), 1);
}

#[test]
fn reconnect_worker_panic_is_terminal_and_releases_the_native_lane() {
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    fixture.host.borrow_mut().disconnect.reconnect_provider =
        Some(Arc::new(|_, _| panic!("injected failure")));
    fixture.action(&source, true);
    assert!(!fixture.controller.borrow().source_operations.busy());
    let row = fixture
        .window
        .get_volumes()
        .row_data(fixture.index(&source) as usize)
        .unwrap();
    assert!(!row.reconnect_busy);
    assert!(row.reconnect_needed);
    assert_eq!(row.state, "Connection failed");
    fixture.idle();
}

#[test]
fn reconnect_worker_is_single_flight_and_never_retargets_after_selection_changes() {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    let source = share("TV");
    let other = share("Photos");
    let fixture = Fixture::new(&[source.clone(), other.clone()]);
    let (tx, rx) = mpsc::channel();
    let gate = Mutex::new(rx);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let expected = source.path.clone();
    fixture.host.borrow_mut().disconnect.reconnect_provider = Some(Arc::new(move |source, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        assert_eq!(source.path, expected);
        gate.lock().unwrap().recv().unwrap();
        Ok(())
    }));
    fixture.window.invoke_source_context_action(
        fixture.window.get_storage_token(),
        fixture.index(&source),
        true,
    );
    let row = fixture
        .window
        .get_volumes()
        .row_data(fixture.index(&source) as usize)
        .unwrap();
    assert!(row.reconnect_busy);
    fixture
        .window
        .set_scan_path(other.path.to_str().unwrap().into());
    fixture.window.invoke_source_context_action(
        fixture.window.get_storage_token(),
        fixture.index(&other),
        true,
    );
    tx.send(()).unwrap();
    fixture.wait_native();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.window.get_scan_path(), other.path.to_str().unwrap());
    fixture.idle();
}

#[test]
fn corrupt_registry_never_performs_cancellation_or_claims_disconnect_success() {
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    fixture.save();
    // Freeze a valid displayed intent, then inject a damaged persistence store
    // at the boundary before its acceptance. No Windows operation is requested.
    let dialog = fixture.open(&source);
    let damaged = fixture._temp.path().join("damaged-policy");
    std::fs::write(&damaged, b"invalid registry").unwrap();
    fixture.controller.borrow_mut().registry = source_registry::Registry::open(damaged.clone());
    dialog.invoke_accepted(dialog.get_request_token(), false);
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&source.path)
    );
    assert!(
        fixture
            .window
            .get_errors()
            .to_lowercase()
            .contains("failed")
    );
    assert_eq!(std::fs::read(damaged).unwrap(), b"invalid registry");
    fixture.idle();
}

#[test]
fn full_registry_keeps_pending_intention_and_never_reports_a_disconnection() {
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    let dialog = fixture.open(&source);
    let mut full = source_registry::Registry::default();
    for index in 0..source_registry::MAX_SOURCES {
        full.add(share(&format!("full-{index}"))).unwrap();
    }
    fixture.controller.borrow_mut().registry = full;
    fixture.controller.borrow_mut().pending_auto = Some((source.path.clone(), None));
    dialog.invoke_accepted(dialog.get_request_token(), true);
    assert!(
        !fixture
            .controller
            .borrow()
            .registry
            .is_disconnected(&source.path)
    );
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .source_disconnected(&source.path)
    );
    assert_eq!(
        fixture.controller.borrow().pending_auto.as_ref().unwrap().0,
        source.path
    );
    assert!(
        fixture
            .window
            .get_errors()
            .to_lowercase()
            .contains("failed")
    );
    assert!(fixture.host.borrow().disconnect.native.is_none());
    assert_eq!(fixture.host.borrow().epoch, 0);
    assert!(!fixture.window.get_busy());
}

#[test]
fn windows_results_and_failed_persistence_remain_visible_in_either_completion_order() {
    let source = share("TV");
    let fixture = Fixture::new(std::slice::from_ref(&source));
    for windows_success in [false, true] {
        for windows_first in [false, true] {
            let (tx, rx) = mpsc::sync_channel(1);
            {
                let mut host = fixture.host.borrow_mut();
                host.disconnect.save_pending = true;
                host.disconnect.save_is_disconnect = true;
                host.disconnect.saved = None;
                host.disconnect.windows_result = None;
                host.disconnect.native = Some(rx);
            }
            if windows_first {
                tx.send(NativeResult::Disconnected(windows_success))
                    .unwrap();
                fixture
                    .host
                    .borrow_mut()
                    .poll_disconnect(&fixture.window, &mut fixture.controller.borrow_mut());
                fixture.controller.borrow_mut().source_save_result = Some(false);
            } else {
                fixture.controller.borrow_mut().source_save_result = Some(false);
                fixture
                    .host
                    .borrow_mut()
                    .poll_disconnect(&fixture.window, &mut fixture.controller.borrow_mut());
                tx.send(NativeResult::Disconnected(windows_success))
                    .unwrap();
            }
            fixture
                .host
                .borrow_mut()
                .poll_disconnect(&fixture.window, &mut fixture.controller.borrow_mut());
            let error = fixture.window.get_errors();
            assert!(error.contains("saving failed"), "{error}");
            assert!(
                error.contains(if cfg!(target_os = "linux") {
                    if windows_success {
                        "and Ubuntu"
                    } else {
                        "Ubuntu mount is still active"
                    }
                } else if windows_success {
                    "and Windows"
                } else {
                    "Windows connection is still active"
                }),
                "{error}"
            );
            assert!(
                !fixture
                    .controller
                    .borrow()
                    .registry
                    .is_disconnected(&source.path)
            );
            fixture.idle();
        }
    }
}
