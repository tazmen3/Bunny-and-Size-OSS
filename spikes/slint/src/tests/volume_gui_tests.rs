use super::*;
use sortsize_core::{MetadataValue, volumes::*};
pub(super) fn discovered(paths: &[std::path::PathBuf]) -> VolumeDiscovery {
    VolumeDiscovery {
        volumes: paths
            .iter()
            .enumerate()
            .map(|(i, path)| Volume {
                identity: VolumeIdentity::WindowsGuid(path.as_os_str().to_owned()),
                label: MetadataValue::Unknown,
                filesystem: MetadataValue::Unknown,
                kind: SourceKind::Local,
                mounts: vec![VolumeMount {
                    path: path.clone(),
                    mount_id: None,
                    root: None,
                }],
                capacity: VolumeCapacity::from_bytes(
                    MetadataValue::Unknown,
                    MetadataValue::Unknown,
                    MetadataValue::Unknown,
                ),
                availability: if i == 1 {
                    VolumeAvailability::Unavailable
                } else {
                    VolumeAvailability::Available
                },
            })
            .collect(),
        enumeration_time: Duration::ZERO,
        capacity_time: Duration::ZERO,
        capacity_calls: 0,
    }
}

pub(super) fn inject_discovery(controller: &Rc<RefCell<Controller>>, discovery: VolumeDiscovery) {
    let volumes = discovery.volumes.clone();
    let mut sources =
        crate::volume_view::Sources::with_provider(std::sync::Arc::new(move |options| {
            assert!(!options.probe_remote);
            Ok(VolumeDiscovery {
                volumes: volumes.clone(),
                enumeration_time: Duration::ZERO,
                capacity_time: Duration::ZERO,
                capacity_calls: 0,
            })
        }));
    sources.current = Some(crate::volume_view::Prepared::new(discovery));
    controller.borrow_mut().sources = sources;
}

#[test]
fn volume_click_replaces_generations_resets_views_and_rejects_obsolete_results() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let c = TempDir::new().unwrap();
    let d = TempDir::new().unwrap();
    fs::write(c.path().join("only-c"), [0; 7]).unwrap();
    fs::write(d.path().join("only-d"), [0; 41]).unwrap();
    let mut discovery = discovered(&[c.path().into(), d.path().into(), "disconnected".into()]);
    discovery.volumes[1].availability = VolumeAvailability::Available;
    inject_discovery(&controller, discovery);
    controller.borrow().show_sources(&window);
    window.invoke_select_volume(0);
    assert!(window.get_busy());
    let obsolete = controller.borrow().task.as_ref().unwrap().mailbox.clone();
    let token = controller
        .borrow()
        .task
        .as_ref()
        .unwrap()
        .cancellation
        .clone();
    let generation = controller.borrow().generation;
    // Repeated clicks on the active source are idempotent.
    for _ in 0..100 {
        window.invoke_select_volume(0);
    }
    assert_eq!(controller.borrow().generation, generation);
    window.invoke_select_volume(1);
    assert!(token.is_cancelled());
    assert_eq!(window.get_rows().row_count(), 0);
    assert_eq!(window.get_details().row_count(), 0);
    assert_eq!(window.get_tiles().row_count(), 0);
    assert_eq!(window.get_top_rows().row_count(), 0);
    assert!(
        window
            .get_scan_source()
            .contains(d.path().to_str().unwrap())
    );
    window.invoke_select_volume(0);
    obsolete.lock().unwrap().completion = Some(Err(worker::Failure::Panicked));
    wait(&window, &controller);
    assert_eq!(window.get_details().row_count(), 1);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "only-c");
    assert_eq!(window.get_top_rows().row_data(0).unwrap().name, "only-c");
    assert!(window.get_summary().contains("7"));
    assert_eq!(window.get_scanned_volume(), 0);
    // Available at discovery, removed before acquisition: explicit error, no old results.
    window.invoke_select_volume(2);
    wait(&window, &controller);
    assert_eq!(window.get_details().row_count(), 0);
    assert_eq!(window.get_top_rows().row_count(), 0);
    assert!(!window.get_errors().is_empty());
    window.invoke_select_volume(1);
    window.invoke_cancel_scan();
    wait(&window, &controller);
    assert!(!window.get_busy());
    window.invoke_select_volume(0);
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "only-c");
    assert_eq!(fs::read(c.path().join("only-c")).unwrap(), [0; 7]);
    // Editing a free path does not launch a scan or relabel the completed source.
    let source = window.get_scan_source();
    window.set_scan_path(d.path().to_str().unwrap().into());
    window.invoke_path_edited();
    assert_eq!(window.get_selected_volume(), -1);
    assert!(controller.borrow().task.is_none());
    assert_eq!(window.get_scan_source(), source);
    window.invoke_start_scan();
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "only-d");
}

#[test]
fn remote_unknown_denied_and_unavailable_volumes_never_start_implicitly() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    for kind in [
        SourceKind::Local,
        SourceKind::Removable,
        SourceKind::Optical,
        SourceKind::Remote,
        SourceKind::Unknown,
    ] {
        for availability in [
            VolumeAvailability::Available,
            VolumeAvailability::Unavailable,
            VolumeAvailability::AccessDenied,
            VolumeAvailability::NotProbed,
            VolumeAvailability::Unknown,
        ] {
            let mut discovery = discovered(&[fixture.path().into()]);
            discovery.volumes[0].kind = kind;
            discovery.volumes[0].availability = availability;
            inject_discovery(&controller, discovery);
            window.invoke_select_volume(0);
            let allowed = matches!(
                kind,
                SourceKind::Local | SourceKind::Removable | SourceKind::Optical
            ) && availability == VolumeAvailability::Available;
            assert_eq!(window.get_busy(), allowed);
            if allowed {
                wait(&window, &controller);
            } else {
                assert!(controller.borrow().task.is_none());
                assert!(!window.get_errors().is_empty());
            }
        }
    }
}

#[test]
fn source_projection_is_capped_and_unknown_is_not_zero() {
    let paths: Vec<_> = (0..300)
        .map(|i| std::path::PathBuf::from(format!("source-{i}")))
        .collect();
    let prepared = crate::volume_view::Prepared::new(discovered(&paths));
    assert!(prepared.limited);
    assert_eq!(prepared.discovery.volumes.len(), 256);
    assert_eq!(prepared.discovery.volumes.len(), 256);
    let row = &prepared.rows_for(crate::units::Units::Binary)[0];
    assert!(!row.has_usage);
    assert!(row.summary.contains("Inconnu"));
    assert!(!row.summary.contains("0 octet"));
}

#[test]
fn manual_scan_never_relabels_retained_results_as_a_replacement_volume() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("old-volume-only"), [0; 17]).unwrap();
    for previously_discovered in [true, false] {
        let mut original = discovered(&[fixture.path().into()]);
        original.volumes[0].mounts[0].mount_id = Some(101);
        if !previously_discovered {
            original.volumes.clear();
        }
        inject_discovery(&controller, original);
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.invoke_start_scan();
        wait(&window, &controller);
        let source = window.get_scan_source();
        let summary = window.get_summary();
        assert_eq!(
            window.get_details().row_data(0).unwrap().name,
            "old-volume-only"
        );
        assert_eq!(
            window.get_scanned_volume(),
            if previously_discovered { 0 } else { -1 }
        );
        // A new device occupies exactly the same path; no new scan was requested.
        let mut replacement = discovered(&[fixture.path().into()]);
        replacement.volumes[0].identity = VolumeIdentity::WindowsGuid("replacement-device".into());
        replacement.volumes[0].mounts[0].mount_id = Some(202);
        inject_discovery(&controller, replacement);
        controller.borrow().show_sources(&window);
        assert_eq!(window.get_selected_volume(), 0);
        assert_eq!(
            window.get_scanned_volume(),
            -1,
            "old inventory must not mark the replacement as scanned"
        );
        assert_eq!(window.get_scan_source(), source);
        assert_eq!(window.get_summary(), summary);
        assert_eq!(
            window.get_details().row_data(0).unwrap().name,
            "old-volume-only"
        );
    }
}

#[test]
fn stale_volume_identity_mount_and_newly_remote_source_are_refused_before_scan() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(
        fixture.path().join("underlying-directory-must-not-be-read"),
        [0; 23],
    )
    .unwrap();
    for mutation in 0..4 {
        let expected = discovered(&[fixture.path().into()]);
        let mut fresh = discovered(&[fixture.path().into()]);
        match mutation {
            0 => fresh.volumes.clear(),
            1 => fresh.volumes[0].identity = VolumeIdentity::WindowsPath("other-device".into()),
            2 => fresh.volumes[0].mounts[0].mount_id = Some(123),
            _ => fresh.volumes[0].kind = SourceKind::Remote,
        }
        inject_discovery(&controller, fresh);
        controller.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(expected));
        window.invoke_select_volume(0);
        wait(&window, &controller);
        assert!(controller.borrow().tree.is_none());
        assert_eq!(window.get_rows().row_count(), 0);
        assert_eq!(window.get_details().row_count(), 0);
        assert_eq!(window.get_top_rows().row_count(), 0);
        assert!(!window.get_errors().is_empty());
        assert_eq!(
            fs::read(fixture.path().join("underlying-directory-must-not-be-read")).unwrap(),
            [0; 23]
        );
    }
}

#[test]
fn replacement_device_at_same_path_is_a_new_generation_and_cancelled_source_can_restart() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("new-device"), [0; 13]).unwrap();
    let mut discovery = discovered(&[fixture.path().into()]);
    let old = discovery.volumes[0].clone();
    discovery.volumes[0].identity = VolumeIdentity::WindowsGuid("replacement-device".into());
    inject_discovery(&controller, discovery);
    let obsolete = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox::default()));
    let cancelled = sortsize_core::CancellationToken::default();
    controller.borrow_mut().task = Some(Task {
        cancellation: cancelled.clone(),
        mailbox: obsolete.clone(),
    });
    controller.borrow_mut().scan_root = Some(fixture.path().into());
    controller.borrow_mut().scan_volume = Some((old.identity, old.mounts[0].clone()));
    let generation = controller.borrow().generation;
    window.set_busy(true);
    window.invoke_select_volume(0);
    assert!(cancelled.is_cancelled());
    assert_ne!(controller.borrow().generation, generation);
    obsolete.lock().unwrap().completion = Some(Err(worker::Failure::Panicked));
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "new-device");
    window.invoke_select_volume(0);
    window.invoke_cancel_scan();
    let generation = controller.borrow().generation;
    window.invoke_select_volume(0);
    assert_ne!(controller.borrow().generation, generation);
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "new-device");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "explicit isolated mount namespace and SORTSIZE_VOLUME_GUARD_MOUNT required"]
fn native_linux_unmounted_source_does_not_scan_readable_underlying_directory() {
    assert_eq!(
        std::env::var("SORTSIZE_VOLUME_GUARD_ISOLATED").as_deref(),
        Ok("1")
    );
    assert_ne!(
        fs::read_link("/proc/self/ns/mnt").unwrap(),
        std::path::PathBuf::from(std::env::var_os("SORTSIZE_VOLUME_GUARD_HOST_MNT").unwrap())
    );
    let mount = std::path::PathBuf::from(std::env::var_os("SORTSIZE_VOLUME_GUARD_MOUNT").unwrap());
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let mut discovery = discover_volumes(DiscoveryOptions::default()).unwrap();
    let i = discovery
        .volumes
        .iter()
        .position(|v| v.mounts.iter().any(|m| m.path == mount))
        .unwrap();
    let mut volume = discovery.volumes.remove(i);
    let i = volume.mounts.iter().position(|m| m.path == mount).unwrap();
    let chosen = volume.mounts.remove(i);
    volume.mounts.insert(0, chosen);
    assert!(crate::volume_view::can_auto_scan(&volume));
    discovery.volumes = vec![volume];
    // Keep the actual native provider for the fresh worker-side check.
    controller.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(discovery));
    assert!(
        std::process::Command::new("umount")
            .arg(&mount)
            .status()
            .unwrap()
            .success()
    );
    assert!(mount.join("underlying-must-not-be-scanned").is_file());
    window.invoke_select_volume(0);
    wait(&window, &controller);
    assert!(controller.borrow().tree.is_none());
    assert_eq!(window.get_rows().row_count(), 0);
    assert_eq!(window.get_details().row_count(), 0);
    assert_eq!(
        window.get_errors(),
        tr!("Storage source changed or became unavailable. Refresh sources and select it again.")
    );
}

#[cfg(windows)]
#[test]
#[ignore = "explicit SORTSIZE_VOLUME_GUARD_LETTER for an unused virtual SUBST drive required"]
fn native_windows_removed_virtual_drive_is_rejected_before_scan() {
    let letter = std::env::var("SORTSIZE_VOLUME_GUARD_LETTER").unwrap();
    assert!(
        letter.len() == 2 && letter.as_bytes()[0].is_ascii_uppercase() && letter.ends_with(':')
    );
    let mount = std::path::PathBuf::from(format!("{letter}\\"));
    let discovery = discover_volumes(DiscoveryOptions::default()).unwrap();
    assert!(
        !discovery
            .volumes
            .iter()
            .any(|v| v.mounts.iter().any(|m| m.path == mount)),
        "drive letter must be unused"
    );
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("test-data"), [0; 13]).unwrap();
    assert!(
        std::process::Command::new("subst.exe")
            .arg(&letter)
            .arg(fixture.path())
            .status()
            .unwrap()
            .success()
    );
    struct RemoveDrive(String);
    impl Drop for RemoveDrive {
        fn drop(&mut self) {
            let _ = std::process::Command::new("subst.exe")
                .arg(&self.0)
                .arg("/D")
                .status();
        }
    }
    let cleanup = RemoveDrive(letter.clone());
    let mut discovery = discover_volumes(DiscoveryOptions::default()).unwrap();
    let i = discovery
        .volumes
        .iter()
        .position(|v| v.mounts.iter().any(|m| m.path == mount))
        .unwrap();
    let mut volume = discovery.volumes.remove(i);
    let i = volume.mounts.iter().position(|m| m.path == mount).unwrap();
    let chosen = volume.mounts.remove(i);
    volume.mounts.insert(0, chosen);
    assert!(matches!(volume.identity, VolumeIdentity::WindowsPath(_)));
    assert!(!crate::volume_view::can_auto_scan(&volume));
    let old = volume.clone();
    discovery.volumes = vec![volume];
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    controller.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(discovery));
    // Explicit paths remain supported, but their location-only identities must
    // never associate the retained inventory with a replacement SUBST target.
    window.set_scan_path(mount.to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "test-data");
    assert_eq!(window.get_scanned_volume(), -1);
    let replacement = TempDir::new().unwrap();
    fs::write(replacement.path().join("replacement-data"), [0; 29]).unwrap();
    assert!(
        std::process::Command::new("subst.exe")
            .arg(&letter)
            .arg("/D")
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("subst.exe")
            .arg(&letter)
            .arg(replacement.path())
            .status()
            .unwrap()
            .success()
    );
    let mut fresh = discover_volumes(DiscoveryOptions::default()).unwrap();
    let index = fresh
        .volumes
        .iter()
        .position(|v| v.mounts.iter().any(|m| m.path == mount))
        .unwrap();
    let new = fresh.volumes.remove(index);
    assert_eq!(old.identity, new.identity);
    assert_eq!(old.mounts, new.mounts);
    assert!(!controller
        .borrow()
        .sources
        .scan_preflight(&old, &old.mounts[0])());
    fresh.volumes = vec![new];
    controller.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(fresh));
    controller.borrow().show_sources(&window);
    assert_eq!(window.get_scanned_volume(), -1);
    window.invoke_select_volume(0);
    assert!(controller.borrow().task.is_none());
    assert_eq!(window.get_details().row_data(0).unwrap().name, "test-data");
    // The new contents are reachable only through a new explicit scan.
    window.invoke_start_scan();
    wait(&window, &controller);
    assert_eq!(
        window.get_details().row_data(0).unwrap().name,
        "replacement-data"
    );
    drop(cleanup);
    window.invoke_select_volume(0);
    assert!(controller.borrow().task.is_none());
    assert_eq!(
        window.get_details().row_data(0).unwrap().name,
        "replacement-data"
    );
    assert!(!window.get_errors().is_empty());
}

#[test]
fn sidebar_ratio_persists_on_action_and_legacy_startup_keeps_original_bytes() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("display-v1.conf");
    for legacy in [
        "version=1\nunits=decimal\nhidden_columns=owner\n",
        "version=2\nunits=binary\nhidden_columns=\nlanguage=fr\n",
    ] {
        fs::write(&path, legacy).unwrap();
        let preferences = preferences_store::load(&path).0;
        let (window, controller) = new_session(preferences, Some("en_GB".into())).unwrap();
        controller.borrow_mut().preference_store =
            Some(preferences_store::Store::start(path.clone()).unwrap());
        controller.borrow_mut().poll(&window);
        assert_eq!(fs::read(&path).unwrap(), legacy.as_bytes());
        for ratio in [-1, 100, 375, 900, 1001] {
            let generation = controller.borrow().generation;
            window.invoke_resize_sidebar(ratio);
            assert_eq!(window.get_sidebar_ratio(), ratio.clamp(100, 900));
            assert_eq!(controller.borrow().generation, generation);
            assert!(controller.borrow().task.is_none());
        }
        window.invoke_resize_sidebar(375);
        // Store flushes the last coalesced action before closing.
        controller.borrow_mut().preference_store = None;
        let reloaded = preferences_store::load(&path).0;
        assert_eq!(reloaded.sidebar_ratio, 375);
        let (restart, _) = new_session(reloaded, Some("en_GB".into())).unwrap();
        assert_eq!(restart.get_sidebar_ratio(), 375);
    }
}

#[test]
#[ignore = "native Winit; explicit SORTSIZE_UX_OUTPUT required"]
fn native_ux_sidebar_mouse_keyboard_scroll_and_five_languages() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let output = std::path::PathBuf::from(std::env::var_os("SORTSIZE_UX_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let (window, controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    let fixture = TempDir::new().unwrap();
    for i in 0..80 {
        let dir = fixture
            .path()
            .join(format!("folder-{i:03}-long-name-for-scroll"));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file"), [0; 4096]).unwrap();
    }
    let mut discovery = discovered(&vec![fixture.path().into(); 12]);
    for (i, volume) in discovery.volumes.iter_mut().enumerate() {
        volume.availability = VolumeAvailability::Available;
        volume.label = MetadataValue::Known {
            value: format!("Local volume {i:02}").into(),
            accuracy: sortsize_core::ValueAccuracy::Exact,
        };
        let used = [0, 1, 491, 700, 899, 900, 973, 1000][i % 8];
        let known = |value| MetadataValue::Known {
            value,
            accuracy: sortsize_core::ValueAccuracy::Exact,
        };
        volume.capacity = VolumeCapacity::from_bytes(
            known(1_000_000_000_000),
            known((1000 - used) * 1_000_000_000),
            known((1000 - used) * 1_000_000_000),
        );
    }
    inject_discovery(&controller, discovery);
    controller.borrow().show_sources(&window);
    window
        .window()
        .set_size(slint::LogicalSize::new(1200., 760.));
    window.show().unwrap();
    window.invoke_select_volume(0);
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    let started = Instant::now();
    let mut phase = 0;
    let mut case = 0usize;
    let mut records =
        String::from("language\twidth\theight\tratio\ttree_height\tsource_height\tscale\n");
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        assert!(started.elapsed() < Duration::from_secs(120));
        let window = weak.upgrade().unwrap();
        state.borrow_mut().poll(&window);
        if window.get_busy() {
            return;
        }
        if phase == 0 {
            assert_eq!(window.get_details().row_count(), 80);
            let old_ratio = window.get_sidebar_ratio();
            let from = slint::LogicalPosition::new(
                window.get_sidebar_split_x() + 30.,
                window.get_sidebar_split_y() + 6.,
            );
            let to = slint::LogicalPosition::new(from.x, from.y + 25.);
            window
                .window()
                .dispatch_event(WindowEvent::PointerMoved { position: from });
            window.window().dispatch_event(WindowEvent::PointerPressed {
                position: from,
                button: PointerEventButton::Left,
            });
            window
                .window()
                .dispatch_event(WindowEvent::PointerMoved { position: to });
            window
                .window()
                .dispatch_event(WindowEvent::PointerReleased {
                    position: to,
                    button: PointerEventButton::Left,
                });
            assert!(
                window.get_sidebar_ratio() > old_ratio,
                "drag must enlarge folders"
            );
            let dragged = window.get_sidebar_ratio();
            for key in [slint::platform::Key::UpArrow, slint::platform::Key::Return] {
                window
                    .window()
                    .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
                window
                    .window()
                    .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
                if key == slint::platform::Key::UpArrow {
                    assert_eq!(window.get_sidebar_ratio(), dragged - 25);
                }
            }
            assert_eq!(window.get_sidebar_ratio(), 550);
            let x = window.get_sidebar_split_x() + 50.;
            let y = window.get_sidebar_split_y();
            let source_scroll = window.get_sidebar_source_scroll();
            window
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(x, y - 30.),
                    delta_x: 0.,
                    delta_y: -120.,
                });
            assert!(window.get_sidebar_folder_scroll() < 0.);
            assert_eq!(window.get_sidebar_source_scroll(), source_scroll);
            let folder_scroll = window.get_sidebar_folder_scroll();
            window
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(x, y + 90.),
                    delta_x: 0.,
                    delta_y: -120.,
                });
            assert!(window.get_sidebar_source_scroll() < source_scroll);
            assert_eq!(window.get_sidebar_folder_scroll(), folder_scroll);
            phase = 1;
            return;
        }
        if case == 30 {
            fs::write(output.join("geometry.tsv"), &records).unwrap();
            window.hide().unwrap();
            slint::quit_event_loop().unwrap();
            return;
        }
        let language = case / 6;
        let ratio = [100, 550, 900][case % 3];
        let (width, height) = if case % 6 < 3 {
            (960., 660.)
        } else {
            (1200., 760.)
        };
        if phase == 1 {
            window.invoke_set_language(language as i32 + 1);
            window.invoke_resize_sidebar(ratio);
            window.set_sidebar_source_scroll(-((case % 8) as f32) * 128.);
            let scale = window.window().scale_factor();
            window.window().set_size(slint::PhysicalSize::new(
                (width * scale) as u32,
                (height * scale) as u32,
            ));
            phase = 2;
        } else {
            assert!(window.get_sidebar_tree_height() >= 79.);
            assert!(window.get_sidebar_source_height() >= 149.);
            assert_eq!(window.get_sidebar_ratio(), ratio);
            records += &format!(
                "{language}\t{width}\t{height}\t{ratio}\t{}\t{}\t{}\n",
                window.get_sidebar_tree_height(),
                window.get_sidebar_source_height(),
                window.window().scale_factor()
            );
            progressive_gui_tests::snapshot(
                &window,
                &output,
                &format!("language-{language}-{width}-{ratio}"),
            );
            case += 1;
            phase = 1;
        }
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
}

#[test]
fn occupancy_thresholds_unknowns_rounding_languages_and_units_have_exact_oracles() {
    use crate::{locale::Language, presentation::Context, units::Units};
    for language in Language::ALL {
        let _scope = Context {
            language,
            generation: 1,
        }
        .enter();
        for units in [Units::Binary, Units::Decimal] {
            for (used, expected_level, display) in [
                (0, 1, 0.),
                (100, 1, 1.),
                (4910, 1, 49.1),
                (6999, 1, 69.9),
                (7000, 2, 70.),
                (8990, 2, 89.9),
                (8999, 2, 89.9),
                (9000, 3, 90.),
                (9730, 3, 97.3),
                (9999, 3, 99.9),
                (10000, 3, 100.),
            ] {
                let known = |value| MetadataValue::Known {
                    value,
                    accuracy: sortsize_core::ValueAccuracy::Exact,
                };
                let mut discovery = discovered(&["local".into()]);
                discovery.volumes[0].capacity = VolumeCapacity::from_bytes(
                    known(10000),
                    known(10000 - used),
                    known(10000 - used),
                );
                let rows = crate::volume_view::Prepared::new(discovery).rows_for(units);
                assert_eq!(rows[0].usage_level, expected_level);
                assert!((rows[0].fraction - used as f32 / 10000.).abs() < 1e-6);
                assert!(
                    rows[0]
                        .usage
                        .contains(&crate::number_format::decimal(display))
                );
                assert!(rows[0].has_usage);
            }
        }
    }
    let known = |value| MetadataValue::Known {
        value,
        accuracy: sortsize_core::ValueAccuracy::Exact,
    };
    for (total, free) in [
        (MetadataValue::Unknown, known(1)),
        (known(0), known(0)),
        (known(10), known(11)),
        (MetadataValue::AccessDenied, known(0)),
    ] {
        let mut discovery = discovered(&["local".into()]);
        discovery.volumes[0].capacity = VolumeCapacity::from_bytes(total, free, free);
        let row = crate::volume_view::Prepared::new(discovery)
            .rows_for(Units::Binary)
            .remove(0);
        assert!(!row.has_usage);
        assert_eq!(row.usage_level, 0);
        assert_eq!(row.fraction, 0.);
    }
    let mut discovery = discovered(&["local".into()]);
    discovery.volumes[0].capacity = VolumeCapacity::from_bytes(known(u64::MAX), known(0), known(0));
    let mut prepared = crate::volume_view::Prepared::new(discovery);
    let row = prepared.rows_for(Units::Binary).remove(0);
    assert_eq!(row.usage_level, 3);
    assert_eq!(row.fraction, 1.);
    prepared.discovery.volumes[0].availability = VolumeAvailability::Unavailable;
    let row = prepared.rows_for(Units::Binary).remove(0);
    assert!(!row.has_usage);
    assert_eq!(row.usage_level, 0);
}

#[test]
fn accessible_occupancy_does_not_round_up_across_thresholds_or_to_full() {
    let total = u64::MAX / 100 * 100;
    let known = |value| MetadataValue::Known {
        value,
        accuracy: sortsize_core::ValueAccuracy::Exact,
    };
    for (used, expected_level, expected_percent) in [
        (total / 100 * 70 - 1, 1, 69.9),
        (total / 100 * 70, 2, 70.0),
        (total / 100 * 90 - 1, 2, 89.9),
        (total / 100 * 90, 3, 90.0),
        (total - 1, 3, 99.9),
        (total, 3, 100.0),
    ] {
        let mut discovery = discovered(&["local".into()]);
        discovery.volumes[0].capacity =
            VolumeCapacity::from_bytes(known(total), known(total - used), known(total - used));
        let row = crate::volume_view::Prepared::new(discovery)
            .rows_for(crate::units::Units::Binary)
            .remove(0);
        assert_eq!(row.usage_level, expected_level);
        assert!(
            row.usage
                .contains(&crate::number_format::decimal(expected_percent))
        );
        assert!((row.usage_percent - expected_percent as f32).abs() < 0.0001);
    }
}
