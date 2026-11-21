use super::*;
use slint::{
    Model,
    platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use std::time::{Duration, Instant};
struct TestPlatform(Rc<MinimalSoftwareWindow>);

#[cfg(windows)]
#[test]
fn observed_zero_size_rows_are_not_reported_as_inventory_preparation() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("zero-files");
    std::fs::create_dir(&source).unwrap();
    for i in 0..512 {
        std::fs::write(source.join(format!("zero-{i:04}")), []).unwrap();
    }
    h.borrow().service.lane.lock().unwrap().tick_delay = Duration::from_millis(100);
    w.set_scan_path(c.borrow_mut().path_input.select(source).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.busy && !p.frame.rows.is_empty())
    });
    assert_eq!(w.get_tiles().row_count(), 0);
    assert!(w.get_treemap_note().starts_with("Logical size"));
    assert!(w.get_view_warning().contains("Partial"));
    w.invoke_cancel_scan();
    wait(&h, &w, &c, |host| {
        host.current.as_ref().is_some_and(|p| !p.busy)
    });
    let empty = temporary.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(empty).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    assert_eq!(w.get_details().row_count(), 0);
    assert_eq!(w.get_tiles().row_count(), 0);
    assert_eq!(
        w.get_empty_message().as_str(),
        "No file observed in this scope."
    );
    assert_eq!(w.get_treemap_note().as_str(), "Logical size · current page");
}

#[cfg(windows)]
#[test]
fn fresh_windows_acquisition_clears_previous_generation_even_for_same_source() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let other = temporary.path().join("other");
    for path in [&source, &other] {
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("measured-file"), b"real observed contents").unwrap();
    }
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && !p.tiles.is_empty())
    });
    for path in [&source, &other] {
        let old_inventory = h
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .inventory;
        assert!(w.get_details().row_count() > 0);
        w.set_scan_path(c.borrow_mut().path_input.select(path.clone()).into());
        w.invoke_start_scan();
        // No GUI poll occurs here: this checks the synchronous new-generation
        // boundary, independently of how quickly the native scan completes.
        assert!(h.borrow().current.is_none());
        assert_eq!(w.get_details().row_count(), 0);
        assert_eq!(w.get_tiles().row_count(), 0);
        assert!(w.get_current_folder_summary().is_empty());
        assert!(w.get_busy());
        wait(&h, &w, &c, |host| {
            host.current
                .as_ref()
                .is_some_and(|p| !p.busy && !p.tiles.is_empty())
        });
        assert_ne!(
            h.borrow()
                .current
                .as_ref()
                .unwrap()
                .frame
                .stamp
                .context
                .inventory,
            old_inventory
        );
    }
}

#[cfg(windows)]
#[test]
fn first_rescan_with_saved_measures_open_publishes_a_fresh_inventory() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let file = source.join("changed");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(&file, [0; 12]).unwrap();
    h.borrow().service.lane.lock().unwrap().cache_root = Some(temp.path().join("cache"));
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    let ready = |host: &Host, bytes| {
        host.service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| {
                epoch == host.epoch && matches!(phase, service::Phase::Ready)
            })
            && host.current.as_ref().is_some_and(|page| {
                page.epoch == host.epoch
                    && !page.busy
                    && page.root_measure.and_then(|measure| measure.logical) == Some(bytes)
            })
    };
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| ready(host, 12));

    w.set_cache_visible(true);
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |host| {
        host.cache_current.as_ref().is_some_and(|cache| {
            cache.id == host.cache_id && cache.catalogue.iter().any(cache::is_snapshot)
        })
    });
    w.invoke_cache_action(1, 0, w.get_cache_token());
    wait(&h, &w, &c, |host| {
        host.cache_current.as_ref().is_some_and(|cache| {
            cache.id == host.cache_id
                && cache
                    .catalogue
                    .iter()
                    .any(|saved| !cache::is_snapshot(saved))
        })
    });
    let retained = h
        .borrow()
        .cache_current
        .as_ref()
        .unwrap()
        .catalogue
        .iter()
        .find(|saved| !cache::is_snapshot(saved))
        .unwrap()
        .clone();

    std::fs::write(&file, [0; 31]).unwrap();
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| ready(host, 31));
    assert!(w.get_cache_visible());
    assert!(!w.get_busy());
    wait(&h, &w, &c, |host| {
        host.cache_current.as_ref().is_some_and(|cache| {
            cache.id == host.cache_id
                && cache
                    .catalogue
                    .iter()
                    .any(|saved| cache::is_snapshot(saved) && saved.logical == Some(31))
                && cache
                    .catalogue
                    .iter()
                    .any(|saved| saved.directory == retained.directory && saved.logical == Some(12))
        })
    });
}

#[cfg(windows)]
#[test]
fn rescan_survives_a_closed_diagnostic_stream() {
    use std::process::{Command, Stdio};

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "workshop::tests::first_rescan_with_saved_measures_open_publishes_a_fresh_inventory",
            "--nocapture",
        ])
        .env("BUNNY_SCAN_P1_METRICS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn shown_window_guard_releases_pages_and_workers_after_unwind() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let governor = sortsize_core::resources::Governor::global();
    let before = governor.usage().categories;
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let visibility = TestWindowVisibility::new(&w);
    w.show().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("file"), b"owned page").unwrap();
    w.set_scan_path(
        c.borrow_mut()
            .path_input
            .select(fixture.path().to_owned())
            .into(),
    );
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|page| !page.busy && page.measure_ready)
    });
    let weak_host = Rc::downgrade(&h);
    let weak_controller = Rc::downgrade(&c);
    let weak_lane = Arc::downgrade(&h.borrow().service.lane);
    drop(h);
    drop(c);
    drop(w);
    assert!(weak_host.upgrade().is_some());
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _visibility = visibility;
        panic!("exercise shown-window cleanup during unwinding");
    }));
    assert!(unwound.is_err());
    assert!(
        weak_host.upgrade().is_none(),
        "component callbacks retained Host/pages"
    );
    assert!(
        weak_controller.upgrade().is_none(),
        "component retained Controller"
    );
    // The product destructor stays nonblocking. Only this test waits for its
    // detached workers to release their read owners and accounted capacities.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let usage = governor.usage();
        if weak_lane.upgrade().is_none()
            && usage
                .categories
                .iter()
                .zip(before)
                .all(|(after, before)| *after <= before)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "window cleanup retained resources: {usage:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn finish_views_and_sidebar_keep_offline_identity_and_proportions() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for i in 0..64 {
        std::fs::write(source.join(format!("file-{i:03}")), vec![0; i + 1]).unwrap();
    }
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready && p.sort_index == 0)
    });
    w.invoke_workshop_action(w.get_workshop_token(), 2, false);
    let before = h.borrow().current.as_ref().unwrap().tiles.clone();
    assert_eq!(before.len(), 64);
    for tile in &before {
        assert!((tile.w * tile.h - tile.band_w).abs() < 0.00001);
        assert!(
            tile.x >= 0.
                && tile.y >= 0.
                && tile.x + tile.w <= 1.00001
                && tile.y + tile.h <= 1.00001
        );
    }
    assert!((before.iter().map(|t| t.w * t.h).sum::<f32>() - 1.).abs() < 0.00001);
    let epoch = h.borrow().epoch;
    let selected = h.borrow().selected;
    let quanta = h.borrow().current.as_ref().unwrap().quanta;
    std::fs::rename(&source, t.path().join("offline")).unwrap();
    for i in 0..1000 {
        w.set_ribbon_tab(i % 5);
        w.invoke_set_treemap_mode(i % 2);
        w.invoke_set_sidebar_width(if i % 2 == 0 { 220. } else { 500. });
        w.invoke_set_sidebar_collapsed(i % 2 == 0);
        h.borrow_mut().poll(&w, &c.borrow());
    }
    let host = h.borrow();
    assert_eq!(host.epoch, epoch);
    assert_eq!(host.selected, selected);
    assert_eq!(host.current.as_ref().unwrap().quanta, quanta);
    assert_eq!(host.current.as_ref().unwrap().tiles, before);
    assert!(w.get_errors().is_empty());
    let prefs = c.borrow().preferences.clone();
    assert_eq!(
        crate::display_preferences::Preferences::decode(&prefs.encode()),
        Some(prefs)
    );
}
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}
// Functional fixtures use the same explicit profile as the native recipe.
// Auto admission under pressure is a separate negative contract, not a fixture precondition.
fn fixed_profile(w: &SortSizeWindow, c: &Rc<RefCell<Controller>>, h: &Rc<RefCell<Host>>) {
    w.invoke_memory_select(1);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        h.borrow_mut().poll(w, &c.borrow());
        if sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == MemoryMode::Entries50k
        {
            break;
        }
        assert!(Instant::now() < deadline, "resource profile not applied");
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[track_caller]
fn wait(
    host: &Rc<RefCell<Host>>,
    w: &SortSizeWindow,
    c: &Rc<RefCell<Controller>>,
    predicate: impl Fn(&Host) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        host.borrow_mut().poll(w, &c.borrow());
        assert!(
            w.get_errors().is_empty(),
            "{}; {}",
            w.get_errors(),
            test_wait_state(&host.borrow())
        );
        if predicate(&host.borrow()) {
            return;
        }
        assert!(
            Instant::now() < end,
            "native service timeout: cache={} diff={} status={} epoch={}; {}",
            w.get_cache_status(),
            w.get_diff_summary(),
            w.get_status(),
            host.borrow().epoch,
            test_wait_state(&host.borrow())
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn targeted_owner_visible_rows_navigation_refresh_and_stop() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    assert_eq!(w.get_local_owner_resolution(), cfg!(target_os = "linux"));
    w.invoke_set_column(3, true);
    fixed_profile(&w, &c, &h);
    let _visibility = TestWindowVisibility::new(&w);
    w.show().unwrap();
    w.window().set_size(slint::LogicalSize::new(1200., 800.));
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let child = source.join("child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("nested.txt"), b"nested").unwrap();
    for n in 0..36 {
        std::fs::write(source.join(format!("file-{n:02}")), b"owner").unwrap();
    }
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    #[cfg(target_os = "linux")]
    {
        // Match real text entry: the shared path callback must preserve the
        // local-only choice, without entering the historical NSS controller.
        w.invoke_path_edited();
        assert!(w.get_resolve_owner());
        assert!(c.borrow().owner_task.is_none());
    }
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    #[cfg(windows)]
    let oracle = std::process::Command::new("powershell.exe")
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; (Get-Acl -LiteralPath $env:BUNNY_OWNER_FIXTURE).Owner",
        ])
        .env("BUNNY_OWNER_FIXTURE", source.join("file-00"))
        .output()
        .unwrap();
    #[cfg(windows)]
    assert!(oracle.status.success());
    #[cfg(windows)]
    let expected = String::from_utf8_lossy(&oracle.stdout).trim().to_owned();
    #[cfg(target_os = "linux")]
    let expected = {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::symlink_metadata(source.join("file-00"))
            .unwrap()
            .uid();
        let passwd = std::fs::read_to_string("/etc/passwd").unwrap();
        passwd
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split(':').collect();
                (fields.get(2)?.parse::<u32>().ok()? == uid).then(|| fields[0].to_owned())
            })
            .unwrap_or_else(|| format!("uid:{uid} (UID)"))
    };
    wait(&h, &w, &c, |_| {
        w.get_details().iter().any(|r| r.owner == expected.as_str())
    });
    assert!(
        h.borrow()
            .current
            .as_ref()
            .unwrap()
            .owners
            .iter()
            .all(|o| o.id == sortsize_core::MetadataValue::NotCollected),
        "acquisition remains owner-free"
    );
    assert!(
        !w.get_owner_sort_available(),
        "visible enrichment cannot claim global owner sorting"
    );
    w.set_detail_scroll_y(-34. * 25.);
    wait(&h, &w, &c, |_| {
        w.get_details()
            .row_data(25)
            .is_some_and(|r| r.owner == expected.as_str())
    });
    // Fresh observations on explicit refresh, including a now-missing object.
    let token = w.get_workshop_token();
    let path = h.borrow().current.as_ref().unwrap().frame.rows[25]
        .path
        .clone();
    std::fs::remove_file(path).unwrap();
    h.borrow_mut().navigate(service::Navigate::Refresh);
    wait(&h, &w, &c, |_| {
        w.get_workshop_token() != token
            && w.get_details()
                .row_data(25)
                .is_some_and(|r| r.owner == "Unavailable")
    });
    let folder = h
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .rows
        .iter()
        .position(|r| r.kind == sortsize_store::inventory::Kind::Directory)
        .unwrap();
    w.invoke_workshop_action(w.get_workshop_token(), folder as i32, true);
    wait(&h, &w, &c, |_| {
        w.get_details().row_count() == 1
            && w.get_details()
                .row_data(0)
                .is_some_and(|r| r.owner == expected.as_str())
    });
    w.invoke_go_parent();
    wait(&h, &w, &c, |_| w.get_details().row_count() == 37);
    w.invoke_cancel_scan();
    assert!(!w.get_resolve_owner());
    assert!(!w.get_cancelling());
    #[cfg(target_os = "linux")]
    {
        w.set_scan_path(child.to_string_lossy().as_ref().into());
        w.invoke_path_edited();
        assert!(
            !w.get_resolve_owner(),
            "path changes preserve an explicit stop"
        );
        assert!(c.borrow().owner_task.is_none());
        w.set_scan_path(source.to_string_lossy().as_ref().into());
        w.invoke_path_edited();
        assert!(!w.get_resolve_owner());
    }
    w.set_resolve_owner(true);
    w.invoke_owner_resolution_changed();
    wait(&h, &w, &c, |_| {
        w.get_details().iter().any(|r| r.owner == expected.as_str())
    });
    // Cached/offline views must not replace historical owners with live values.
    h.borrow_mut().owner_live_source = false;
    h.borrow_mut().poll_owners(&w);
    assert!(w.get_details().iter().all(|r| r.owner == "—"));
    println!(
        "native visible owners: 37 files only; owner matches local platform oracle, viewport scrolling, navigation, missing-file refresh, stop/restart and offline isolation PASS"
    );
}
#[test]
fn sqlite_atelier_navigation_pages_selection_freeze_and_offline_reads() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c).unwrap();
    h.borrow().service.lane.lock().unwrap().force_strict = true;
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    for i in 0..450 {
        std::fs::write(source.join(format!("file-{i:04}")), [1; 7]).unwrap();
    }
    std::fs::write(source.join("nested/child"), [0; 30]).unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    let start_epoch = h.borrow().epoch;
    w.invoke_start_scan();
    assert_eq!(h.borrow().epoch, start_epoch);
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready && p.sort_index == 0)
    });
    assert_eq!(
        h.borrow().current.as_ref().unwrap().summary,
        crate::units::Units::Binary.format(450 * 7 + 30)
    );
    let quanta = h.borrow().current.as_ref().unwrap().quanta;
    // Deterministic 4 MiB publication category, reproducing the retained-view
    // pressure independently of the host's current free memory. Sources stay real.
    struct RestoreBudget;
    impl Drop for RestoreBudget {
        fn drop(&mut self) {
            sortsize_core::resources::Governor::global().refresh_native();
        }
    }
    let _restore = RestoreBudget;
    sortsize_core::resources::Governor::global().configure(MemoryMode::Auto, None);
    assert_eq!(w.get_details().row_count(), 200);
    // Recursive inventory counts must not be the 200 entries on this page.
    assert!(w.get_current_folder_summary().contains("451"));
    let folder_summary = w.get_current_folder_summary();
    assert_eq!(w.get_top_rows().row_count(), 100);
    let first = w.get_details().row_data(0).unwrap().path;
    let token = w.get_workshop_token();
    w.invoke_workshop_action(token, 0, false);
    let selected = h.borrow().selected;
    assert!(selected.is_some());
    std::fs::rename(&source, temp.path().join("offline")).unwrap();
    w.invoke_change_page(true, true);
    let epoch = h.borrow().epoch;
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == epoch && p.sort_index == 0)
            && w.get_details().row_data(0).is_some_and(|r| r.path != first)
    });
    assert_ne!(w.get_details().row_data(0).unwrap().path, first);
    assert_eq!(w.get_current_folder_summary(), folder_summary);
    w.invoke_workshop_action(token, 0, true);
    assert_eq!(h.borrow().epoch, epoch); // stale token cannot navigate
    w.invoke_change_page(true, false);
    let epoch = h.borrow().epoch;
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == epoch && p.sort_index == 0)
            && w.get_details().row_data(0).is_some_and(|r| r.path == first)
    });
    assert_eq!(w.get_details().row_data(0).unwrap().path, first);
    // Freeze retains the rendered identity even with a newer publication pending.
    w.invoke_workshop_gesture(true);
    let old = w.get_workshop_token();
    let same_epoch = h.borrow().epoch;
    h.borrow_mut().service.lane.lock().unwrap().navigation = Some((same_epoch, Navigate::Refresh));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.borrow_mut().poll(&w, &c.borrow());
        if h.borrow().pending.is_some() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(w.get_workshop_token(), old);
    w.invoke_workshop_action(old, 1, false);
    let selected = h.borrow().selected;
    w.invoke_workshop_gesture(false);
    h.borrow_mut().poll(&w, &c.borrow());
    assert_eq!(h.borrow().selected, selected);
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, quanta);
    w.set_top_visible(true);
    w.invoke_select_top(0);
    let revealed = h.borrow().top_selected.unwrap();
    w.invoke_reveal_top();
    let epoch = h.borrow().epoch;
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == epoch && p.sort_index == 0)
    });
    assert_eq!(h.borrow().selected, Some(revealed));
    assert!(w.get_workshop_selected() >= 0);
    assert!(!w.get_top_visible());
    let old_token = w.get_workshop_token();
    let next = temp.path().join("next");
    std::fs::create_dir(&next).unwrap();
    std::fs::write(next.join("other"), [0; 11]).unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(next).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    assert_ne!(w.get_workshop_token(), old_token);
    w.invoke_workshop_action(old_token, 0, false);
    assert_eq!(h.borrow().selected, None);
    let epoch = h.borrow().epoch;
    w.invoke_workshop_source(-1, 0);
    assert_eq!(h.borrow().epoch, epoch);
    assert!(w.get_errors().is_empty());
    assert!(c.borrow().task.is_none());
    assert!(c.borrow().tree.is_none());
}
#[test]
fn windows_paths_are_display_only_and_native_identity_is_unchanged() {
    let native = PathBuf::from(r"\\?\C:\folder\日本語.txt");
    let before = native.clone();
    let display = label(&native);
    assert_eq!(native, before);
    #[cfg(windows)]
    assert_eq!(display, r"C:\folder\日本語.txt");
    #[cfg(not(windows))]
    assert_eq!(display, r"\\?\C:\folder\日本語.txt");
}

#[test]
fn integrated_search_native_offline_pages_statistics_tokens_and_cancel_isolation() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    for i in 0..450 {
        std::fs::write(root.join(format!("résumé-{i:04}.txt")), [1; 7]).unwrap();
    }
    w.set_scan_path(c.borrow_mut().path_input.select(root.clone()).into());
    w.invoke_start_scan();
    w.invoke_show_analysis();
    w.set_search_extension("txt".into());
    w.invoke_search_submit();
    w.invoke_search_cancel();
    assert!(!w.get_search_busy());
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    let quanta = h.borrow().current.as_ref().unwrap().quanta;
    assert!(h.borrow().search_current.is_none());
    std::fs::rename(&root, temp.path().join("offline")).unwrap();
    w.invoke_search_submit();
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| !p.busy)
    });
    assert_eq!(w.get_search_matches(), "450");
    assert!(w.get_search_complete());
    assert_eq!(w.get_search_results().row_count(), 200);
    assert!(w.get_search_next());
    assert!(!w.get_search_previous());
    w.invoke_search_select(w.get_search_token(), 10, false);
    let selected_path = w.get_search_full_path();
    w.set_search_scroll_y(-170.);
    let before_language = w.get_search_token();
    w.invoke_workshop_gesture(true);
    h.borrow().service.lane.lock().unwrap().search_refresh = true;
    wait(&h, &w, &c, |h| h.search_pending.is_some());
    let obsolete = h.borrow().search_pending.as_ref().unwrap().token;
    w.invoke_set_language(3);
    w.invoke_workshop_gesture(false);
    h.borrow_mut().poll(&w, &c.borrow());
    assert_ne!(w.get_search_token(), obsolete); // pending pre-change frame never reinstated

    let expected_inventory_status = {
        let _scope = c.borrow().presentation.enter();
        if cfg!(any(windows, target_os = "linux")) {
            format!("{} · {}", service::Phase::Ready.label(), tr!("Complete"))
        } else {
            tr!("Complete").to_owned()
        }
    };
    wait(&h, &w, &c, |h| {
        h.search_current
            .as_ref()
            .is_some_and(|p| p.token != before_language)
            && w.get_status() == expected_inventory_status
    });
    assert_eq!(w.get_search_selected(), 10);
    assert_eq!(w.get_search_full_path(), selected_path);
    assert_eq!(w.get_search_scroll_y(), -170.);
    {
        let _scope = c.borrow().presentation.enter();
        assert_eq!(w.get_status(), expected_inventory_status);
    }
    let first = w.get_search_results().row_data(0).unwrap().label;
    let stale = w.get_search_token();
    w.invoke_search_page(stale, true);
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| p.token != stale)
    });
    assert!(w.get_search_previous());
    assert_ne!(w.get_search_results().row_data(0).unwrap().label, first);
    let current = w.get_search_token();
    w.invoke_search_select(stale, 0, true);
    assert_eq!(w.get_search_token(), current);
    assert!(w.get_search_visible());
    w.invoke_search_page(current, false);
    wait(&h, &w, &c, |h| {
        h.search_current
            .as_ref()
            .is_some_and(|p| p.token != current)
    });
    assert_eq!(w.get_search_results().row_data(0).unwrap().label, first);
    // Real admission refusal leaves all results and the frozen query available.
    let governor = sortsize_core::resources::Governor::global();
    let category = sortsize_core::resources::Category::Queries;
    let usage = governor.usage();
    let spare = usage.limits.categories[category as usize]
        .saturating_sub(usage.categories[category as usize]);
    let pressure = governor.reserve(category, spare, None).unwrap();
    let statistics_token = w.get_search_token();
    w.invoke_search_stats(statistics_token);
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| {
            if cfg!(any(windows, target_os = "linux")) {
                // Direct queries accumulated their statistics while evaluating;
                // presenting those counters requires no new query workspace.
                p.token != statistics_token && p.statistics.contains("3150") && !p.busy
            } else {
                p.statistics.contains("statistics memory budget")
            }
        })
    });
    assert!(!w.get_search_busy());
    assert_eq!(w.get_search_matches(), "450");
    assert_eq!(w.get_search_results().row_count(), 200);
    assert!(w.get_search_complete());
    drop(pressure);
    w.invoke_search_stats(w.get_search_token());
    assert!(w.get_search_busy());
    wait(&h, &w, &c, |h| {
        h.search_current
            .as_ref()
            .is_some_and(|p| p.statistics.contains("3150"))
    });
    assert!(w.get_search_statistics().contains("450"));
    assert!(w.get_search_statistics().contains("3150"));
    // Each new explicit intent replaces the previous one; typing alone does not.
    let id = h.borrow().search_id;
    w.set_search_name("résumé-0449".into());
    assert_eq!(h.borrow().search_id, id);
    w.invoke_search_submit();
    w.set_search_name("résumé-0448".into());
    w.invoke_search_submit();
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| !p.busy)
    });
    assert_eq!(w.get_search_matches(), "1");
    assert!(
        w.get_search_results()
            .row_data(0)
            .unwrap()
            .label
            .contains("0448")
    );
    w.invoke_search_select(w.get_search_token(), 0, true);
    assert!(!w.get_search_visible());
    let epoch = h.borrow().epoch;
    wait(&h, &w, &c, |h| {
        h.current.as_ref().is_some_and(|p| p.epoch == epoch)
    });
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, quanta);
    assert!(w.get_entry_path().is_empty() || w.get_entry_path().contains("0448"));
}

fn seed_search_store(path: &Path, n: u64, partial: bool) -> sortsize_store::inventory::Store {
    use sortsize_store::{
        Durability,
        inventory::{Kind, Observation, Store},
    };
    let c = AcquisitionContext {
        inventory: InventoryId(77),
        source: SourceId(88),
        generation: GenerationId(3),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = Store::create(path, c.inventory, c.source, Durability::Reconstructible).unwrap();
    s.create_generation(c).unwrap();
    let a = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    s.begin_attempt(a).unwrap();
    let mut seq = 0;
    for first in (0..n).step_by(256) {
        let rows = (first..(first + 256).min(n))
            .map(|i| Observation {
                name: {
                    let name = format!("file-{i:08}.txt");
                    #[cfg(unix)]
                    let native = sortsize_store::codec::NativeName::Posix(name.into_bytes());
                    #[cfg(windows)]
                    let native =
                        sortsize_store::codec::NativeName::Windows(name.encode_utf16().collect());
                    native
                },
                kind: Kind::File,
                logical: Some(7),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        s.ingest(
            BatchHeader {
                attempt: a,
                sequence: seq,
                end: None,
            },
            &rows,
        )
        .unwrap();
        seq += 1;
    }
    s.ingest(
        BatchHeader {
            attempt: a,
            sequence: seq,
            end: Some(if partial {
                ListingEnd::Incomplete
            } else {
                ListingEnd::Exhausted
            }),
        },
        &[],
    )
    .unwrap();
    if !partial {
        while !s
            .reconcile_step(a, ComparisonScope::Comparable, 512)
            .unwrap()
        {}
    }
    while !s.consolidate_step(c, 512).unwrap() {}
    s.seal(c).unwrap();
    s
}
#[test]
fn integrated_search_outside_50k_retention_and_partial_zero_synthetic_metadata() {
    synthetic_search(50_010, false);
}
#[test]
#[ignore = "explicit 1M synthetic metadata / headless UI-model benchmark; no disk scan"]
fn synthetic_million_search_model_latency() {
    synthetic_search(1_000_000, true);
}
fn synthetic_search(n: u64, measure: bool) {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("never-created");
    let store = seed_search_store(&temp.path().join("synthetic"), n, true);
    h.borrow_mut().requested = Some(root.clone());
    let lane = h.borrow().service.lane.clone();
    let output = h.borrow().service.search_output.clone();
    let resources = h.borrow().service.resources.clone();
    let l = lane.clone();
    let root_copy = root.clone();
    let scratch = temp.path().to_owned();
    let worker = std::thread::spawn(move || {
        let context = store.acquisition_context(GenerationId(3)).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1600000000);
        loop {
            let request = {
                let mut l = l.lock().unwrap();
                if l.stop {
                    return;
                }
                l.search.take()
            };
            if let Some(request) = request {
                search::run(
                    &search::Frozen {
                        store: &store,
                        certified: None,
                        c: context,
                        root: &root_copy,
                        scratch: &scratch,
                        started: old,
                        sealed: old + Duration::from_secs(10),
                    },
                    request,
                    &l,
                    &output,
                    &resources,
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    w.invoke_show_analysis();
    let mut first_ms = 0.;
    let mut full_ms = 0.;
    if measure {
        w.invoke_search_submit();
        let start = Instant::now();
        wait(&h, &w, &c, |h| {
            h.search_current.as_ref().is_some_and(|p| {
                p.rows
                    .iter()
                    .any(|r| r.kind == sortsize_store::inventory::Kind::File)
            })
        });
        first_ms = start.elapsed().as_secs_f64() * 1000.;
        wait(&h, &w, &c, |h| {
            h.search_current.as_ref().is_some_and(|p| !p.busy)
        });
        full_ms = start.elapsed().as_secs_f64() * 1000.;
        assert_eq!(w.get_search_matches(), (n + 1).to_string());
        assert_eq!(w.get_search_results().row_count(), 200);
    }
    w.set_search_name(format!("file-{:08}", n - 1).into());
    let rare_start = Instant::now();
    w.invoke_search_submit();
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| !p.busy)
    });
    assert_eq!(w.get_search_matches(), "1");
    assert!(!w.get_search_complete());
    assert!(w.get_search_scope().contains("2020"));
    assert!(
        w.get_search_results()
            .row_data(0)
            .unwrap()
            .label
            .contains(&format!("{:08}", n - 1))
    );
    let rare_ms = rare_start.elapsed().as_secs_f64() * 1000.;
    w.set_search_name("absent-target".into());
    let absent_start = Instant::now();
    w.invoke_search_submit();
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| !p.busy)
    });
    assert_eq!(w.get_search_matches(), "0");
    assert!(!w.get_search_complete());
    assert_eq!(w.get_search_results().row_count(), 0);
    assert!(!root.exists());
    if measure {
        println!(
            "{{\"synthetic_metadata\":true,\"native_window\":false,\"ui_model_applied\":true,\"source_io\":0,\"rows\":{n},\"first_useful_ms\":{first_ms},\"full_materialization_ms\":{full_ms},\"rare_late_target_ms\":{rare_ms},\"absent_ms\":{},\"retention_choice\":50000}}",
            absent_start.elapsed().as_secs_f64() * 1000.
        );
    }
    lane.lock().unwrap().stop = true;
    h.borrow().search_cancel.cancel();
    worker.join().unwrap();
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn verify_automatic_snapshot_preserves_it_and_remeasures_the_source() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let file = source.join("changed");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(&file, [0; 12]).unwrap();
    let root = temp.path().join("cache");
    h.borrow().service.lane.lock().unwrap().cache_root = Some(root.clone());
    let snapshot =
        sortsize_store::direct_snapshot::destination(&cache::snapshot_root(&root), &source);
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |_| snapshot.is_file() && !w.get_busy());
    let snapshot = std::fs::canonicalize(snapshot).unwrap();
    let original = std::fs::read(&snapshot).unwrap();

    w.set_cache_visible(true);
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |host| {
        w.get_cache_token() != 0
            && host.cache_current.as_ref().is_some_and(|cache| {
                cache.id == host.cache_id
                    && cache.catalogue.len() == 1
                    && std::fs::canonicalize(&cache.catalogue[0].directory)
                        .is_ok_and(|path| path == snapshot)
            })
    });
    std::fs::write(&file, [0; 32]).unwrap();
    w.invoke_cache_action(3, 0, w.get_cache_token());
    wait(&h, &w, &c, |host| {
        host.current.as_ref().is_some_and(|page| {
            page.epoch == host.epoch
                && !page.busy
                && page.measure_ready
                && page.root_measure.and_then(|measure| measure.logical) == Some(32)
        })
    });
    assert_eq!(std::fs::read(snapshot).unwrap(), original);
}

#[test]
fn saved_measures_offline_diff_paging_languages_and_stale_actions() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    h.borrow().service.lane.lock().unwrap().cache_root = Some(state.clone());
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("changed"), [0; 240]).unwrap();
    for i in 0..430 {
        std::fs::write(source.join(format!("zero-{i:04}")), []).unwrap();
    }
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    w.set_cache_visible(true);
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |h| h.cache_current.is_some());
    eprintln!("cache-stage: save");
    w.invoke_cache_action(1, 0, w.get_cache_token());
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.catalogue.len() == 1)
    });
    let original = h.borrow().cache_current.as_ref().unwrap().catalogue[0].clone();
    assert_eq!(original.logical, Some(240));
    assert!(original.measured.is_some());
    let parent_time = std::fs::metadata(&source).unwrap().modified().unwrap();
    std::fs::write(source.join("changed"), [0; 160]).unwrap();
    assert_eq!(
        parent_time,
        std::fs::metadata(&source).unwrap().modified().unwrap()
    );
    eprintln!("cache-stage: verify");
    w.invoke_cache_action(3, 0, w.get_cache_token());
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.epoch == h.epoch && p.catalogue.len() == 2)
    });
    let previous = h
        .borrow()
        .cache_current
        .as_ref()
        .unwrap()
        .catalogue
        .iter()
        .position(|s| s.context.generation == original.context.generation)
        .unwrap() as i32;
    assert_eq!(
        w.get_cache_choice(),
        previous,
        "catalogue updates must preserve the selected measure"
    );
    eprintln!("cache-stage: compare");
    w.invoke_cache_action(5, previous, w.get_cache_token());
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.rows.len() == 200 && !p.summary.is_empty())
    });
    assert!(w.get_diff_summary().contains("240") && w.get_diff_summary().contains("160"));
    let first = w.get_diff_rows().row_data(0).unwrap().old_path;
    w.invoke_cache_page(true);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.rows.first().is_some_and(|r| r.old_path != first))
    });
    w.invoke_cache_page(false);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.rows.first().is_some_and(|r| r.old_path == first))
    });
    eprintln!("cache-stage: languages");
    for language in 0..7 {
        w.invoke_set_language(language);
        wait(&h, &w, &c, |h| {
            h.cache_current.as_ref().is_some_and(|p| {
                p.presentation == h.presentation
                    && p.rows.first().is_some_and(|r| r.old_path == first)
            })
        });
        assert!(w.get_diff_summary().contains("240") && w.get_diff_summary().contains("160"));
    }
    eprintln!("cache-stage: offline open");
    let stale = w.get_cache_token();
    w.invoke_cache_action(2, previous, stale);
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.quanta == 0 && p.measure_ready)
    });
    assert!(w.get_summary().contains("240"));
    let epoch = h.borrow().epoch;
    eprintln!("cache-stage: verify");
    w.invoke_cache_action(3, 0, stale);
    assert_eq!(
        h.borrow().epoch,
        epoch,
        "stale source action must be rejected"
    );
    std::fs::rename(&source, temp.path().join("source-offline")).unwrap();
    w.invoke_show_analysis();
    w.set_search_name("zero-".into());
    w.invoke_search_submit();
    wait(&h, &w, &c, |h| {
        h.search_current
            .as_ref()
            .is_some_and(|p| !p.busy && p.matches == 430)
    });
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, 0);
    assert_eq!(
        h.borrow()
            .cache_current
            .as_ref()
            .unwrap()
            .catalogue
            .iter()
            .find(|s| s.context.generation == original.context.generation)
            .unwrap()
            .measured,
        original.measured
    );
}

#[test]
fn cached_source_selection_replaces_native_intent_before_a_burst_scan() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("a");
    let b = t.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    std::fs::write(a.join("a"), b"a").unwrap();
    std::fs::write(b.join("b"), b"bb").unwrap();
    h.borrow().service.lane.lock().unwrap().cache_root = Some(t.path().join("state"));
    w.set_scan_path(c.borrow_mut().path_input.select(a.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && !p.busy && p.measure_ready && p.sort_index >= 0)
            && h.service
                .lane
                .lock()
                .unwrap()
                .phase
                .is_some_and(|(epoch, phase, _)| {
                    epoch == h.epoch && matches!(phase, service::Phase::Ready)
                })
    });
    // The direct scan saves a snapshot. The SQLite scan can instead
    // retain an explicit dated measure through the same catalogue.
    let refresh_saved = || {
        w.invoke_cache_action(0, 0, 0);
        if !cfg!(any(windows, target_os = "linux")) {
            wait(&h, &w, &c, |h| {
                h.cache_current
                    .as_ref()
                    .is_some_and(|page| page.id == h.cache_id && page.epoch == h.epoch)
            });
            w.invoke_cache_action(1, 0, w.get_cache_token());
        }
    };
    let saved_kind = |saved: &sortsize_store::inventory::catalogue::Saved| {
        if cfg!(any(windows, target_os = "linux")) {
            cache::is_snapshot(saved)
        } else {
            saved.measured.is_some()
        }
    };
    refresh_saved();
    wait(&h, &w, &c, |h| {
        h.cache_current.as_ref().is_some_and(|p| {
            p.id == h.cache_id
                && p.epoch == h.epoch
                && p.catalogue
                    .iter()
                    .any(|saved| saved.root == a && saved_kind(saved))
        })
    });
    w.set_scan_path(c.borrow_mut().path_input.select(b.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && !p.busy && p.measure_ready && p.sort_index >= 0)
            && h.service
                .lane
                .lock()
                .unwrap()
                .phase
                .is_some_and(|(epoch, phase, _)| {
                    epoch == h.epoch && matches!(phase, service::Phase::Ready)
                })
    });
    refresh_saved();
    wait(&h, &w, &c, |h| {
        h.cache_current.as_ref().is_some_and(|p| {
            p.id == h.cache_id
                && p.epoch == h.epoch
                && [&a, &b].iter().all(|root| {
                    p.catalogue
                        .iter()
                        .any(|saved| &saved.root == *root && saved_kind(saved))
                })
        })
    });
    let saved_a_index = || {
        h.borrow()
            .cache_current
            .as_ref()
            .unwrap()
            .catalogue
            .iter()
            .position(|saved| saved.root == a && saved_kind(saved))
            .unwrap() as i32
    };
    w.invoke_cache_action(2, saved_a_index(), w.get_cache_token());
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && !p.busy && p.measure_ready && p.sort_index >= 0)
    });
    assert_eq!(h.borrow().requested.as_ref(), Some(&a));
    assert_eq!(c.borrow().path_input.resolve(w.get_scan_path().as_str()), a);
    assert!(c.borrow().scan_root.is_none());
    assert!(c.borrow().scan_volume.is_none());
    assert_eq!(w.get_scanned_volume(), -1);
    assert!(w.get_scan_source().is_empty());
    let offline_epoch = h.borrow().service.lane.lock().unwrap().source_epoch;
    w.set_errors("offline inventory diagnostic".into());
    w.invoke_workshop_retry();
    assert_eq!(
        h.borrow().service.lane.lock().unwrap().source_epoch,
        offline_epoch
    );
    assert!(h.borrow().service.lane.lock().unwrap().start.is_none());
    w.set_errors("".into());
    c.borrow().show_sources(&w);
    assert_eq!(w.get_scanned_volume(), -1);
    w.set_cache_visible(true);
    w.invoke_set_language(3);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && p.presentation == h.presentation)
    });
    w.invoke_cache_action(3, saved_a_index(), w.get_cache_token());
    let verify_epoch = h.borrow().epoch;
    w.set_scan_path(c.borrow_mut().path_input.select(b.clone()).into());
    w.invoke_start_scan();
    assert!(
        h.borrow().epoch > verify_epoch,
        "last explicit B intent must replace cache verification of A"
    );
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && !p.busy && p.measure_ready && p.sort_index >= 0)
    });
    assert_eq!(h.borrow().requested.as_ref(), Some(&b));
    assert!(w.get_summary().contains('2'));
}

#[test]
fn atelier_metadata_twelve_global_sorts_offline_and_explicit_duplicates() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("folder")).unwrap();
    for i in 0..450 {
        std::fs::write(source.join(format!("file-{i:04}")), b"same content").unwrap();
    }
    std::fs::hard_link(source.join("file-0000"), source.join("alias")).unwrap();
    w.set_collect_owner(true);
    w.set_collect_allocation(true);
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && !p.busy && p.measure_ready && p.sort_index == 0)
    });
    {
        let host = h.borrow();
        let p = host.current.as_ref().unwrap();
        assert_eq!(p.owners.len(), p.details.len());
        for (row, owner) in p.details.iter().zip(&p.owners) {
            assert!(!row.modified.is_empty());
            assert_ne!(row.modified, "—");
            assert_ne!(row.accessed, "—");
            assert_ne!(row.allocated, "—");
            assert!(matches!(
                owner.id,
                sortsize_core::MetadataValue::Known { .. }
            ));
            assert_ne!(row.owner, "—");
        }
    }
    let offline = temp.path().join("offline");
    std::fs::rename(&source, &offline).unwrap();
    let quanta = h.borrow().current.as_ref().unwrap().quanta;
    // SizeDown (0) is the consolidated default. Establish SizeUp (1) so
    // the first real click below requests exactly SizeDown again.
    let token = w.get_workshop_token();
    assert_eq!(w.get_sort_order(), 0);
    w.invoke_sort_details(false);
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.token != token && p.sort_index == 1 && !p.busy)
    });
    for index in 0..12 {
        let token = w.get_workshop_token();
        match index {
            0 | 1 => w.invoke_sort_details(false),
            2 | 3 => w.invoke_sort_details(true),
            4 | 5 => w.invoke_sort_allocated(),
            6 | 7 => w.invoke_sort_owner(),
            8 | 9 => w.invoke_sort_dates(false),
            _ => w.invoke_sort_dates(true),
        };
        wait(&h, &w, &c, |h| {
            h.current
                .as_ref()
                .is_some_and(|p| p.token != token && p.sort_index == index && !p.busy)
        });
        let host = h.borrow();
        let p = host.current.as_ref().unwrap();
        assert_eq!(p.quanta, quanta);
        if index == 0 {
            assert_eq!(p.frame.rows[0].kind, sortsize_store::inventory::Kind::File);
            assert!(
                p.frame
                    .rows
                    .windows(2)
                    .all(|pair| { pair[0].logical.unwrap() >= pair[1].logical.unwrap() })
            );
        } else {
            assert_eq!(
                p.frame.rows[0].kind,
                sortsize_store::inventory::Kind::Directory
            );
        }
        assert!(p.frame.more);
    }
    // Use the actual table callback, including reverse traversal after a global sort.
    let token = w.get_workshop_token();
    w.invoke_change_page(true, true);
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.token != token && !p.busy && w.get_has_previous_detail_page())
    });
    assert!(w.get_has_previous_detail_page());
    std::fs::rename(&offline, &source).unwrap();
    w.invoke_show_analysis();
    w.invoke_search_submit();
    wait(&h, &w, &c, |h| {
        h.search_current
            .as_ref()
            .is_some_and(|p| !p.busy && p.complete)
    });
    for index in [2, 1, 0] {
        let token = w.get_search_token();
        w.invoke_search_sort(token, index);
        wait(&h, &w, &c, |h| {
            h.search_current
                .as_ref()
                .is_some_and(|p| p.token != token && !p.busy && p.sort_index == index)
        });
        assert_eq!(w.get_search_order(), index);
    }
    assert_eq!(
        w.get_resolve_owner(),
        cfg!(any(windows, target_os = "linux"))
    );
    assert!(!w.get_search_duplicates_visible());
    let token = w.get_search_token();
    w.set_duplicate_exhaustive(true);
    w.invoke_search_duplicates(token);
    wait(&h, &w, &c, |h| {
        h.search_current
            .as_ref()
            .is_some_and(|p| p.duplicates && !p.busy)
    });
    // Windows consultation uses directory listings; other platforms stay strict.
    // Exact positive groups remain usable without an implicit acquisition.
    assert_eq!(
        w.get_search_complete(),
        !cfg!(windows),
        "{}",
        w.get_search_status()
    );
    assert_eq!(
        w.get_search_status().contains("Incomplete coverage"),
        cfg!(windows)
    );
    assert_eq!(
        w.get_search_statistics()
            .contains("Archived directory sizes may be outdated"),
        cfg!(windows)
    );
    assert_eq!(w.get_search_matches(), "451");
    assert!(w.get_search_status().contains("hardlink aliases: 1"));
    assert!(w.get_search_next());
    let first = h
        .borrow()
        .search_current
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.key)
        .collect::<Vec<_>>();
    let token = w.get_search_token();
    w.invoke_search_page(token, true);
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| p.token != token)
    });
    assert!(w.get_search_previous());
    let token = w.get_search_token();
    w.invoke_search_page(token, false);
    wait(&h, &w, &c, |h| {
        h.search_current.as_ref().is_some_and(|p| p.token != token)
    });
    assert_eq!(
        first,
        h.borrow()
            .search_current
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.key)
            .collect::<Vec<_>>()
    );
}

#[test]
fn sorted_source_replacement_closes_result_before_scratch_cleanup() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("first");
    let b = t.path().join("second");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    std::fs::write(a.join("first-file"), b"preserved").unwrap();
    std::fs::write(b.join("second-file"), b"preserved").unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(a.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    w.invoke_sort_details(true);
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.sort_index == 2)
    });
    w.set_scan_path(c.borrow_mut().path_input.select(b.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.epoch == h.epoch && !p.busy && p.measure_ready)
    });
    assert_eq!(std::fs::read(a.join("first-file")).unwrap(), b"preserved");
    assert_eq!(std::fs::read(b.join("second-file")).unwrap(), b"preserved");
    assert!(cache::CLEANUP_ERROR.lock().unwrap().is_none());
}

/// SCAN-010 / VIEW-008: a failed replacement retires the old presentation,
/// and a subsequent explicit acquisition can start in the same window.
#[test]
fn p0_failed_source_replacement_resets_presentation_and_can_restart() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let t = tempfile::tempdir().unwrap();
    let first = t.path().join("first");
    let second = t.path().join("second");
    for source in [&first, &second] {
        std::fs::create_dir(source).unwrap();
    }
    std::fs::write(first.join("first-file"), b"first").unwrap();
    std::fs::write(second.join("second-file"), b"second").unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(first.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    assert_eq!(w.get_scan_source(), short_label(&first));
    let stale_token = w.get_workshop_token();
    // Reproduce a page retained during a gesture followed by a failure.
    {
        let mut host = h.borrow_mut();
        let mut pending = host.current.take().unwrap();
        pending.busy = true;
        host.pending = Some(pending);
        host.gesture = true;
        *host.service.output.lock().unwrap() = Some(Output::Error(
            host.epoch,
            "injected acquisition refusal".into(),
            false,
        ));
    }
    w.set_cancelling(true);
    h.borrow_mut().poll(&w, &c.borrow());
    assert!(!w.get_busy());
    assert!(!w.get_cancelling());
    assert!(h.borrow().pending.is_none());
    assert_eq!(w.get_errors(), "injected acquisition refusal");
    w.invoke_workshop_gesture(false);
    h.borrow_mut().poll(&w, &c.borrow());
    assert!(!w.get_busy());

    let missing = t.path().join("missing");
    w.set_selection_summary("old folder".into());
    w.set_has_parent(true);
    w.set_has_next_detail_page(true);
    w.set_top_note("old acquisition in progress".into());
    w.set_cancelling(true);
    w.set_scan_path(c.borrow_mut().path_input.select(missing.clone()).into());
    w.invoke_start_scan();
    assert!(w.get_busy());
    assert!(!w.get_cancelling());
    assert!(!w.get_has_parent());
    assert!(!w.get_has_next_detail_page());
    assert!(w.get_selection_summary().is_empty());
    assert!(w.get_current_folder_summary().is_empty());
    assert!(w.get_current_path().is_empty());
    assert_eq!(w.get_details().row_count(), 0);
    assert_eq!(w.get_rows().row_count(), 0);
    assert_eq!(w.get_top_rows().row_count(), 0);
    assert_eq!(w.get_tiles().row_count(), 0);
    assert_eq!(w.get_scan_source(), short_label(&missing));
    assert_eq!(c.borrow().scan_root.as_ref(), Some(&missing));
    let deadline = Instant::now() + Duration::from_secs(30);
    while w.get_errors().is_empty() {
        h.borrow_mut().poll(&w, &c.borrow());
        assert!(Instant::now() < deadline, "missing source did not fail");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!w.get_busy());
    assert!(!w.get_cancelling());
    assert_eq!(w.get_status(), "Could not start scan.");
    assert_eq!(w.get_top_note(), w.get_status());
    assert_eq!(w.get_treemap_note(), w.get_status());
    w.invoke_workshop_action(stale_token, 0, true);
    assert!(h.borrow().current.is_none());

    w.set_scan_path(c.borrow_mut().path_input.select(second.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.epoch == host.epoch && !p.busy && p.measure_ready)
    });
    assert_eq!(w.get_scan_source(), short_label(&second));
    assert_eq!(c.borrow().scan_root.as_ref(), Some(&second));
    assert_eq!(w.get_summary(), "6 bytes");
    assert_eq!(w.get_details().row_count(), 1);
    assert!(
        w.get_details()
            .row_data(0)
            .unwrap()
            .path
            .contains("second-file")
    );
    assert!(w.get_errors().is_empty());
    assert!(!w.get_cancelling());
    assert_eq!(std::fs::read(first.join("first-file")).unwrap(), b"first");
}

#[cfg(feature = "experimental-preview")]
#[test]
fn experimental_preview_waiting_translation_keeps_the_request_and_epoch() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    h.borrow_mut().poll(&w, &c.borrow());
    let epoch = h.borrow().epoch;
    let id = h.borrow().cache_id;
    let cancel = h.borrow().cache_cancel.clone();
    for state in [1, 2] {
        w.invoke_set_language(1);
        h.borrow_mut().poll(&w, &c.borrow());
        w.set_preview_state(state);
        w.set_status("Preparing saved measures…".into());
        w.set_cache_status("Preparing saved measures…".into());
        w.invoke_set_language(2);
        h.borrow_mut().poll(&w, &c.borrow());
        assert_ne!(w.get_status(), "Preparing saved measures…");
        assert_eq!(w.get_status(), w.get_cache_status());
        assert_eq!(w.get_preview_state(), state);
        assert_eq!(h.borrow().epoch, epoch);
        assert_eq!(h.borrow().cache_id, id);
        assert!(!cancel.is_cancelled());
        assert!(h.borrow().service.lane.lock().unwrap().cache.is_none());
    }
}

#[cfg(feature = "experimental-preview")]
#[test]
fn experimental_offline_open_fences_preview_and_rechecks_explicit_mutations() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let t = tempfile::tempdir().unwrap();
    let mut s = seed_search_store(&t.path().join("original"), 450, false);
    s.record_measurement(
        GenerationId(3),
        &t.path().join("unmounted-source"),
        100,
        200,
    )
    .unwrap();
    let root = t.path().join("state");
    sortsize_store::inventory::lifecycle::ensure_root(&root).unwrap();
    let retained = s.retain_copy(&root.join("inv-preview"), 30).unwrap();
    drop(retained);
    drop(s);
    h.borrow().service.lane.lock().unwrap().cache_root = Some(root.clone());
    w.set_cache_visible(true);
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.catalogue.len() == 1)
    });
    assert!(
        root.join("inv-preview/inventory.db").exists(),
        "dated preview must not implicitly prune"
    );
    w.invoke_cache_action(2, 0, w.get_cache_token());
    assert_eq!(w.get_preview_state(), 1);
    assert!(h.borrow().current.is_none());
    assert!(!w.get_status().contains("Complete"));
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.measure_ready && !p.busy)
    });
    assert_eq!(w.get_preview_state(), 3);
    assert_eq!(w.get_preview_rows().row_count(), 0);
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, 0);
    let id = h.borrow().cache_id;
    let epoch = h.borrow().epoch;
    *h.borrow().service.preview_output.lock().unwrap() = Some(cache::PreviewOutput::End {
        id: id - 1,
        epoch,
        state: 4,
    });
    h.borrow_mut().poll_cache(&w);
    assert_eq!(w.get_preview_state(), 3);
    // Save is explicit. It releases RO, performs the full check before RW and
    // resets all TEMP projections instead of attempting a write to the snapshot.
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.token > 0)
    });
    w.invoke_cache_action(1, 0, w.get_cache_token());
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.token > 0)
    });
    assert!(w.get_cache_diagnostic().is_empty());
    let mut late = h.borrow_mut().current.take().unwrap();
    w.invoke_cache_action(2, 0, w.get_cache_token());
    assert_eq!(w.get_preview_state(), 1);
    let pending_epoch = h.borrow().epoch;
    w.invoke_cancel_scan();
    assert!(h.borrow().epoch > pending_epoch);
    assert_eq!(w.get_preview_state(), 5);
    assert_eq!(w.get_preview_rows().row_count(), 0);
    assert!(!w.get_busy());
    late.epoch = pending_epoch;
    *h.borrow().service.output.lock().unwrap() = Some(service::Output::Page(late));
    h.borrow_mut().poll(&w, &c.borrow());
    assert!(h.borrow().current.is_none());
    *h.borrow().service.preview_output.lock().unwrap() = Some(cache::PreviewOutput::End {
        id,
        epoch,
        state: 3,
    });
    h.borrow_mut().poll_cache(&w);
    assert_eq!(w.get_preview_state(), 5);
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.token > 0)
    });
    w.invoke_cache_action(2, 0, w.get_cache_token());
    assert_eq!(w.get_preview_state(), 1);
    w.invoke_cache_action(0, 0, 0);
    assert!(!w.get_busy());
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.token > 0)
    });
    assert!(h.borrow().current.is_none());
    h.borrow_mut().clear_preview(&w, 4);
    w.set_errors("corrupt cache".into());
    w.invoke_workshop_retry();
    assert!(h.borrow().service.lane.lock().unwrap().start.is_none());
    assert!(!w.get_busy());
}

#[test]
fn strict_verify_and_resume_are_refused_without_retiring_a_live_candidate_or_changing_intent() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    // This contract exercises the retained canonical candidate/certification path.
    h.borrow().service.lane.lock().unwrap().force_strict = true;
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for i in 0..256 {
        std::fs::write(source.join(format!("file-{i}")), [1; 7]).unwrap();
    }
    h.borrow().service.lane.lock().unwrap().cache_root = Some(temporary.path().join("state"));
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.measure_ready && !p.busy)
    });
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |h| {
        h.cache_current.as_ref().is_some_and(|p| p.id == h.cache_id)
    });
    w.invoke_cache_action(1, 0, w.get_cache_token());
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && p.catalogue.len() == 1)
    });
    h.borrow().service.lane.lock().unwrap().hold_generation = Some(GenerationId(2));
    w.invoke_start_scan();
    wait(&h, &w, &c, |h| {
        let lane = h.service.lane.lock().unwrap();
        lane.writer_entries > 1 && lane.writer_generation == Some(GenerationId(2))
    });
    w.invoke_cache_action(0, 0, 0);
    wait(&h, &w, &c, |h| {
        h.cache_current
            .as_ref()
            .is_some_and(|p| p.id == h.cache_id && !p.catalogue.is_empty())
    });
    let host = h.borrow();
    let epoch = host.epoch;
    let id = host.cache_id;
    let lane = host.service.lane.clone();
    let workspace = lane.lock().unwrap().reader_workspace.clone().unwrap();
    let candidate = workspace
        .repository()
        .lock()
        .unwrap()
        .candidate_path(GenerationId(2));
    drop(host);
    assert!(candidate.exists());
    w.invoke_cache_action(3, 0, w.get_cache_token());
    assert_eq!(h.borrow().epoch, epoch);
    assert_eq!(h.borrow().cache_id, id);
    assert_eq!(lane.lock().unwrap().source_epoch, epoch);
    assert!(!lane.lock().unwrap().publication_cancel.is_cancelled());
    assert!(lane.lock().unwrap().writer_owns_session);
    assert!(candidate.exists());
    assert_eq!(
        w.get_cache_status().as_str(),
        tr!("Wait for the current acquisition to finish before verifying or resuming.")
    );
    // The service guard enforces the same rule even for a directly queued request.
    let saved = h.borrow().cache_current.as_ref().unwrap().catalogue[0].clone();
    let mut no_owner = None;
    cache::run(
        cache::Request {
            id: id + 1,
            epoch,
            expected: epoch,
            action: cache::Action::Resume(saved),
            cancel: Default::default(),
        },
        &mut no_owner,
        &lane,
        &h.borrow().service.cache_output,
        &h.borrow().service.output,
        &h.borrow().service.preview_output,
    );
    assert_eq!(lane.lock().unwrap().source_epoch, epoch);
    assert!(candidate.exists());
    w.invoke_show_analysis();
    w.invoke_duplicate_mode_changed(1);
    assert!(w.get_search_visible());
    w.invoke_cancel_scan();
    lane.lock().unwrap().hold_generation = None;
    wait(&h, &w, &c, |h| {
        h.current.as_ref().is_some_and(|p| {
            p.measure_ready && !p.busy && p.frame.stamp.context.generation == GenerationId(1)
        })
    });
    // Cancelling a replacement preserves the G0 already being consulted.
    // It must not promote the interrupted G1 merely to satisfy a view oracle.
    assert_eq!(
        h.borrow().current.as_ref().unwrap().frame.coverage,
        ScopeCoverage::Complete
    );
    if let Some(latest) = workspace
        .repository()
        .lock()
        .unwrap()
        .latest_context()
        .unwrap()
    {
        assert!(latest.generation == GenerationId(1) || latest.coverage != ScopeCoverage::Complete);
    }
    assert!(!h.borrow().current.as_ref().unwrap().frame.provisional);
}

#[test]
fn strict_cancelling_preparation_never_publishes_ready_and_retry_uses_the_preserved_candidate() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let w = crate::new_window().unwrap();
    let c = crate::connect(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    // This contract exercises the retained canonical candidate/certification path.
    h.borrow().service.lane.lock().unwrap().force_strict = true;
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), [1; 7]).unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let lane = h.borrow().service.lane.clone();
    lane.lock().unwrap().publication_gate = Some(gate.clone());
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |_| lane.lock().unwrap().publication_blocked);
    let epoch = h.borrow().epoch;
    assert!(
        lane.lock()
            .unwrap()
            .phase
            .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Certifying))
    );
    w.invoke_cancel_scan();
    assert!(lane.lock().unwrap().publication_cancel.is_cancelled());
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    wait(&h, &w, &c, |_| {
        lane.lock()
            .unwrap()
            .phase
            .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Cancelled))
            // A worker may publish its phase after Host::poll in this helper's
            // iteration. Wait for the corresponding UI acknowledgement too.
            && !w.get_busy()
            && !w.get_cancelling()
    });
    assert!(!lane.lock().unwrap().reader_active);
    assert!(lane.lock().unwrap().writer_owns_session);
    assert!(!w.get_busy());
    w.invoke_show_analysis();
    w.invoke_search_submit();
    assert!(!w.get_search_complete());
    assert!(!w.get_search_busy());
    // Retry must address the preserved writer even before any live frame was
    // accepted, rather than allocating a second acquisition over that source.
    h.borrow_mut().active_service = false;
    w.invoke_workshop_retry();
    assert_eq!(h.borrow().epoch, epoch);
    wait(&h, &w, &c, |h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.measure_ready && !p.busy && !p.frame.provisional)
    });
    assert_eq!(
        h.borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .generation,
        GenerationId(1)
    );
    assert!(!lane.lock().unwrap().publication_cancel.is_cancelled());
}

#[test]
fn publication_notify_is_optional_and_releases_lane_before_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let lane = Arc::new(Mutex::new(service::Lane::default()));
    service::notify(&lane);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let weak = Arc::downgrade(&lane);
    lane.lock().unwrap().notify = Some(Arc::new(move || {
        let lane = weak.upgrade().unwrap();
        assert!(lane.try_lock().is_ok(), "dispatch must not hold the lane");
        observed.fetch_add(1, Ordering::Relaxed);
    }));
    service::notify(&lane);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    lane.lock().unwrap().notify = None;
    service::notify(&lane);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn repeated_preparation_errors_remain_coalesced_without_wakes_and_apply_on_poll() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observed = callbacks.clone();
    h.borrow().service.lane.lock().unwrap().notify = Some(Arc::new(move || {
        observed.fetch_add(1, Ordering::Relaxed);
    }));
    // Deterministic retry pressure, without a clock or scheduler assumption.
    // The last error is retained in one bounded slot for the fallback poll.
    for index in 0..1000 {
        let host = h.borrow();
        service::publish_retry_error(&host.service.output, host.epoch, format!("refusal {index}"));
    }
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    h.borrow_mut().poll(&w, &c.borrow());
    assert_eq!(w.get_errors().as_str(), "refusal 999");
    assert!(h.borrow().service.output.lock().unwrap().is_none());
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
}

#[cfg(feature = "benchmarks")]
#[test]
fn every_terminal_notification_has_the_exact_stable_page_even_if_partial() {
    use sortsize_store::inventory::generation_a::Repository;
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture");
    super::cached_a_qualification::prepare_smoke(fixture.clone()).unwrap();
    let repo = Repository::open_readonly(&fixture.join("repository")).unwrap();
    let cap = repo.open_latest(&Default::default()).unwrap().unwrap();
    let context = cap
        .as_store()
        .acquisition_context(cap.generation())
        .unwrap();
    for (id, query, expected_matches, expected_next, expected_complete) in [
        (
            1,
            sortsize_core::analysis::SearchQuery::default(),
            258,
            true,
            true,
        ),
        (
            2,
            sortsize_core::analysis::SearchQuery {
                min_bytes: Some(256),
                max_bytes: Some(256),
                ..Default::default()
            },
            1,
            false,
            false,
        ),
    ] {
        let output = Arc::new(Mutex::new(None::<search::Prepared>));
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        lane.lock().unwrap().search_current = Some(id);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let publication = output.clone();
        let weak = Arc::downgrade(&lane);
        lane.lock().unwrap().notify = Some(Arc::new(move || {
            let terminal = {
                let output = publication.lock().unwrap();
                output.as_ref().filter(|p| !p.busy && p.token > 0).map(|p| {
                    (
                        p.id,
                        p.rows.len(),
                        p.next,
                        p.complete,
                        p.counts.matches,
                        p.source_context,
                    )
                })
            };
            if let Some(page) = terminal {
                observed.lock().unwrap().push(page);
                // Synchronous notification observes the exact first publication.
                // Finish deterministically before the idle loop, without sleeps.
                weak.upgrade().unwrap().lock().unwrap().search_current = None;
            }
        }));
        let frozen = search::Frozen {
            store: cap.as_store(),
            certified: Some(&cap),
            c: context,
            root: cap.root(),
            scratch: temp.path(),
            started: std::time::SystemTime::now(),
            sealed: std::time::SystemTime::now(),
        };
        search::run(
            &frozen,
            search::Request {
                id,
                query,
                cancel: Default::default(),
            },
            &lane,
            &output,
            &Mutex::new(None),
        );
        let pages = seen.lock().unwrap();
        assert_eq!(
            pages.len(),
            1,
            "one stable terminal publication must be observed"
        );
        let &(actual_id, rows, next, complete, matches, source) = &pages[0];
        assert_eq!(
            (actual_id, rows, next, complete, matches, source),
            (
                id,
                expected_matches.min(200) as usize,
                expected_next,
                expected_complete,
                expected_matches,
                Some(context)
            )
        );
    }
}

#[test]
fn p0_placement_refusal_is_localized_and_keeps_recovery_instructions() {
    let _scope = crate::presentation::Context {
        language: crate::locale::Language::French,
        generation: 0,
    }
    .enter();
    let message = placement_message(sortsize_store::inventory::placement::PlacementError {
        root: PathBuf::from("source"),
        attempts: Vec::new(),
    });
    assert!(message.starts_with("Le dossier de travail est indisponible"));
    assert!(message.contains("BUNNY_AND_SIZE_STORE_DIR"));
    assert!(message.contains("dossier local existant"));
    assert!(message.contains("relancez Bunny and Size"));
}

/// An error remains idle across retained live pages and presentation changes.
#[test]
fn strict_p0_runtime_failure_cannot_be_reactivated_by_refresh_or_late_page() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    // This contract exercises the retained canonical candidate/certification path.
    h.borrow().service.lane.lock().unwrap().force_strict = true;
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("retained-file"), b"retained").unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    // A reader refusal while a second acquisition is actually held running
    // must not pretend that the independent writer stopped.
    h.borrow().service.lane.lock().unwrap().hold_generation = Some(GenerationId(2));
    w.invoke_start_scan();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        h.borrow_mut().poll(&w, &c.borrow());
        let ready = {
            let host = h.borrow();
            let lane = host.service.lane.lock().unwrap();
            lane.writer_generation == Some(GenerationId(2))
                && lane.writer_entries > 1
                && lane.writer_running
                && host.current.is_some()
        };
        if ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "second acquisition not held running"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    {
        let host = h.borrow();
        *host.service.output.lock().unwrap() = Some(Output::Error(
            host.epoch,
            "reader request refusal".into(),
            true,
        ));
    }
    h.borrow_mut().poll(&w, &c.borrow());
    assert!(
        w.get_busy(),
        "reader refusal must preserve a running acquisition"
    );
    assert!(h.borrow().current.as_ref().unwrap().busy);
    assert!(
        !h.borrow().failed,
        "reader error cannot latch an acquisition failure"
    );
    h.borrow().service.lane.lock().unwrap().hold_generation = None;
    loop {
        h.borrow_mut().poll(&w, &c.borrow());
        if h.borrow()
            .current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "second acquisition did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!h.borrow().failed);
    assert_ne!(w.get_status(), "Could not start scan.");
    assert_ne!(w.get_top_note(), "Could not start scan.");
    {
        let mut host = h.borrow_mut();
        host.current.as_mut().unwrap().busy = true;
        *host.service.output.lock().unwrap() = Some(Output::Error(
            host.epoch,
            "runtime acquisition refusal".into(),
            true,
        ));
    }
    h.borrow_mut().poll(&w, &c.borrow());
    assert!(!w.get_busy());
    assert!(!h.borrow().current.as_ref().unwrap().busy);
    let token = w.get_workshop_token();
    assert_eq!(w.get_details().row_count(), 1);
    // The worker may publish a page still reporting Paused as busy. Retain the
    // observed data while fencing that old running indication.
    {
        let mut host = h.borrow_mut();
        let mut page = host.current.take().unwrap();
        page.busy = true;
        page.status = "old running status".into();
        *host.service.output.lock().unwrap() = Some(Output::Page(page));
    }
    h.borrow_mut().poll(&w, &c.borrow());
    assert_eq!(w.get_workshop_token(), token);
    assert!(!w.get_busy());
    assert!(!h.borrow().current.as_ref().unwrap().busy);
    assert_eq!(w.get_status(), "Could not start scan.");
    assert_eq!(w.get_top_note(), w.get_status());
    assert_eq!(w.get_treemap_note(), w.get_status());
    assert_eq!(w.get_errors(), "runtime acquisition refusal");
    w.set_search_visible(true);
    w.invoke_set_language(2);
    h.borrow_mut().poll(&w, &c.borrow());
    assert!(!w.get_busy());
    assert!(!w.get_status().contains("en cours"));
    assert_ne!(w.get_status(), "Could not start scan.");
    assert_eq!(w.get_top_note(), w.get_status());
    assert_eq!(w.get_treemap_note(), w.get_status());
    w.set_search_visible(false);
    w.invoke_set_language(1);
    w.invoke_start_scan();
    assert!(!h.borrow().failed);
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    assert!(w.get_errors().is_empty());
    assert_eq!(
        std::fs::read(source.join("retained-file")).unwrap(),
        b"retained"
    );
}

// VIEW-001/002/004/009, SCAN-004: changing presentation never reacquires a source.
#[test]
fn forced_size_units_reformat_sqlite_views_offline_without_new_scan() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    h.borrow().service.lane.lock().unwrap().force_strict = true;
    fixed_profile(&w, &c, &h);
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    std::fs::create_dir_all(source.join("child")).unwrap();
    std::fs::write(source.join("root-file"), vec![0; 2048]).unwrap();
    std::fs::write(source.join("child/nested-file"), vec![0; 1024]).unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.busy && p.measure_ready)
    });
    w.invoke_workshop_action(w.get_workshop_token(), 2, false);
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| !p.tiles.is_empty() && !p.top_rows.is_empty())
    });
    let (epoch, quanta, bytes, selected) = {
        let host = h.borrow();
        let page = host.current.as_ref().unwrap();
        (
            host.epoch,
            page.quanta,
            page.frame
                .rows
                .iter()
                .map(|r| r.logical)
                .collect::<Vec<_>>(),
            host.selected,
        )
    };
    assert_eq!(
        w.get_current_folder_summary(),
        "3.0 KiB · 2 files · 1 folder · Complete"
    );
    std::fs::rename(&source, fixture.path().join("offline")).unwrap();
    // Deterministically deliver a page formatted before the first-current-page
    // boundary. No timing-dependent filesystem or thread delay is required.
    let before_applied = h.borrow().applied;
    {
        let mut host = h.borrow_mut();
        let stale = host.current.take().unwrap();
        assert_eq!(stale.units, crate::units::Units::Binary);
        *host.service.output.lock().unwrap() = Some(Output::Page(stale));
    }
    w.invoke_set_size_unit(3);
    h.borrow_mut().poll(&w, &c.borrow());
    assert_eq!(
        h.borrow().applied,
        before_applied,
        "stale unit page cannot be applied"
    );
    assert!(h.borrow().current.is_none());
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.units == crate::units::Units::BinaryFixed(3))
    });
    for decimal in [false, true] {
        w.invoke_set_units(decimal);
        for choice in 1..=4 {
            let before = w.get_workshop_token();
            w.invoke_set_size_unit(choice);
            let suffix = if decimal {
                ["kB", "MB", "GB", "TB"]
            } else {
                ["KiB", "MiB", "GiB", "TiB"]
            }[(choice - 1) as usize];
            wait(&h, &w, &c, |host| {
                w.get_workshop_token() != before
                    && host.current.as_ref().is_some_and(|p| {
                        p.summary.ends_with(suffix)
                            && !p.details.is_empty()
                            && p.details.iter().all(|r| r.size.ends_with(suffix))
                            && !p.folders.is_empty()
                            && p.folders.iter().all(|r| r.size.ends_with(suffix))
                            && p.top_rows.iter().all(|r| r.size.ends_with(suffix))
                            && p.tiles.iter().all(|r| r.size.ends_with(suffix))
                    })
            });
            let host = h.borrow();
            let page = host.current.as_ref().unwrap();
            assert!(page.current_folder_summary.contains(suffix));
            assert_eq!(w.get_current_folder_summary(), page.current_folder_summary);
            assert_eq!(
                host.service.lane.lock().unwrap().source_epoch,
                epoch,
                "presentation cannot create a scan epoch"
            );
            assert_eq!(
                page.quanta, quanta,
                "presentation cannot acquire more entries"
            );
            assert_eq!(host.selected, selected);
            assert_eq!(
                page.frame
                    .rows
                    .iter()
                    .map(|r| r.logical)
                    .collect::<Vec<_>>(),
                bytes
            );
            assert!(!w.get_busy());
        }
    }
    let child = w.get_details().iter().position(|r| r.directory).unwrap();
    w.invoke_workshop_action(w.get_workshop_token(), child as i32, true);
    assert_eq!(w.get_current_folder_summary(), "Preparing inventory…");
    let navigation_epoch = h.borrow().epoch;
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.epoch == navigation_epoch)
    });
    assert!(w.get_current_path().ends_with("child"));
    assert!(
        w.get_current_folder_summary()
            .contains("1 file · 0 folders · Complete")
    );
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, quanta);
    assert_eq!(h.borrow().service.lane.lock().unwrap().source_epoch, epoch);
}

#[cfg(windows)]
#[test]
fn duplicate_folder_limit_edits_wait_for_a_fresh_explicit_search() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().to_path_buf();
    for name in ["first", "second"] {
        std::fs::write(source.join(name), [1; 23]).unwrap();
    }
    let source_epoch = h.borrow().service.lane.lock().unwrap().source_epoch;
    w.set_duplicate_exhaustive(true);
    w.invoke_duplicate_mode_changed(3);
    w.set_duplicate_single_root(label(&source).into());
    w.invoke_duplicate_scope_edited(3);
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    assert!(w.get_duplicate_content_total_bytes().is_empty());
    assert!(w.get_duplicate_content_file_bytes().is_empty());

    w.set_duplicate_content_total_bytes("18446744073709551616".into());
    w.invoke_duplicate_content_limits_edited();
    w.invoke_duplicate_run();
    assert!(!w.get_search_busy());
    assert!(w.get_duplicate_content_limits_expanded());
    assert!(
        w.get_duplicate_content_limits_error()
            .contains("whole numbers")
    );
    assert!(
        h.borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .duplicate_operations
            .is_empty()
    );

    w.set_duplicate_content_total_bytes("".into());
    w.set_duplicate_content_file_bytes("22".into());
    w.invoke_duplicate_content_limits_edited();
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        !w.get_search_busy() && w.get_duplicate_content_retry()
    });
    assert!(!w.get_search_complete());
    assert!(w.get_duplicate_warning().contains("size limit of 22 bytes"));
    assert!(w.get_duplicate_warning().contains("not fully verified: 2"));
    assert!(w.get_duplicate_details().contains("known sizes: 46 bytes"));
    assert!(w.get_search_status().contains("comparison): 0."));
    let limited_search = h.borrow().search_id;

    std::fs::write(source.join("created-after-limit"), [1; 23]).unwrap();
    w.set_duplicate_content_file_bytes("".into());
    w.invoke_duplicate_content_limits_edited();
    assert_eq!(h.borrow().search_id, limited_search);
    assert!(!w.get_search_busy() && w.get_duplicate_content_retry());
    w.invoke_duplicate_run();
    assert!(h.borrow().search_id > limited_search);
    wait(&h, &w, &c, |_| {
        !w.get_search_busy() && w.get_search_complete()
    });
    assert!(!w.get_duplicate_content_retry());
    assert_eq!(w.get_duplicate_rows().row_count(), 3);
    assert!(w.get_duplicate_details().contains("known sizes: 69 bytes"));
    assert!(w.get_search_status().contains("comparison): 161."));
    assert_eq!(
        h.borrow().service.lane.lock().unwrap().source_epoch,
        source_epoch
    );

    w.set_duplicate_content_total_bytes("0".into());
    w.invoke_duplicate_content_limits_edited();
    assert!(w.get_search_complete());
    w.invoke_close_search();
    assert_eq!(w.get_duplicate_content_total_bytes(), "0");
    wait(&h, &w, &c, |host| {
        host.service
            .lane
            .lock()
            .unwrap()
            .duplicate_operations
            .is_empty()
    });
    h.borrow_mut()
        .start_folder_duplicates(&w, &mut c.borrow_mut(), source);
    assert!(w.get_search_busy(), "{}", w.get_duplicate_disabled_reason());
    wait(&h, &w, &c, |_| {
        !w.get_search_busy() && w.get_duplicate_content_retry()
    });
    assert!(
        w.get_duplicate_warning()
            .contains("read limit reached: 0 bytes")
    );
    assert!(!w.get_search_complete());
}

/// Real Atelier callbacks and real SQLite generations, with small local files.
/// This does not qualify any network protocol or a native desktop interaction.
#[test]
fn duplicate_ab_workshop_groups_only_selected_folders_and_rejects_identical_folders() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let third = temp.path().join("C");
    for root in [&a, &b, &third] {
        std::fs::create_dir(root).unwrap();
    }
    let common = b"content shared between A and B";
    std::fs::write(a.join("original.txt"), common).unwrap();
    std::fs::write(a.join("a-only.txt"), b"two copies exclusively in source A").unwrap();
    std::fs::write(
        a.join("a-only-copy.txt"),
        b"two copies exclusively in source A",
    )
    .unwrap();
    std::fs::write(b.join("renamed.bin"), common).unwrap();
    std::fs::write(b.join("another-name.dat"), common).unwrap();
    std::fs::write(b.join("same-size-different.bin"), vec![b'X'; common.len()]).unwrap();
    std::fs::write(third.join("unselected-copy.txt"), common).unwrap();
    for (label, root) in [("A local", &a), ("B local", &b), ("C unselected", &third)] {
        let source = crate::source_registry::Source::new(2, root.clone(), label, None).unwrap();
        c.borrow_mut().registry.add(source).unwrap();
    }
    // C has a real retained inventory too: excluding it must be deliberate.
    for root in [&third, &a, &b] {
        w.set_scan_path(c.borrow_mut().path_input.select(root.clone()).into());
        w.invoke_start_scan();
        let epoch = h.borrow().epoch;
        wait(&h, &w, &c, |host| {
            host.current
                .as_ref()
                .is_some_and(|p| p.epoch == epoch && !p.busy && p.measure_ready)
        });
    }
    let source_epoch = h.borrow().service.lane.lock().unwrap().source_epoch;
    let inventory_quanta = h.borrow().current.as_ref().unwrap().quanta;
    w.invoke_show_analysis();
    w.invoke_duplicate_mode_changed(1);
    wait(&h, &w, &c, |_| {
        w.get_duplicate_source_labels()
            .iter()
            .any(|label| label.ends_with("C unselected"))
    });
    let index = |name: &str| {
        w.get_duplicate_source_labels()
            .iter()
            .position(|label| label.ends_with(name))
            .unwrap() as i32
    };
    w.invoke_duplicate_select(0, index("A local"));
    w.invoke_duplicate_select(1, index("A local"));
    assert!(!w.get_duplicate_ready());
    w.invoke_duplicate_run();
    assert!(!w.get_search_busy(), "A = B must not start content reads");
    assert!(
        w.get_duplicate_settings_expanded(),
        "invalid launch keeps folders visible"
    );
    w.invoke_duplicate_select(1, index("B local"));
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    assert!(
        w.get_duplicate_source_a_info()
            .contains("Inventory available")
    );
    assert!(
        w.get_duplicate_source_b_info()
            .contains("Inventory available")
    );
    assert!(w.get_duplicate_source_a_info().contains("A local"));
    assert!(w.get_duplicate_source_b_info().contains("B local"));
    assert!(!w.get_duplicate_a_needs_inventory());
    assert!(!w.get_duplicate_b_needs_inventory());
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    // Windows consultation uses directory listings; other platforms stay strict.
    // Exact positive groups remain usable without an implicit acquisition.
    assert_eq!(
        w.get_search_complete(),
        !cfg!(windows),
        "{}",
        w.get_search_status()
    );
    assert_eq!(
        w.get_search_status().contains("Incomplete coverage"),
        cfg!(windows)
    );
    assert!(
        !w.get_search_statistics()
            .contains("Archived directory sizes may be outdated")
    );
    assert_eq!(
        w.get_duplicate_warning()
            .contains("Verify the saved inventories before searching again"),
        cfg!(windows)
    );
    assert_eq!(w.get_search_matches(), "1");
    assert!(
        !w.get_duplicate_settings_expanded(),
        "accepted launch collapses folders"
    );
    let projected = w.get_duplicate_rows().iter().collect::<Vec<_>>();
    assert_eq!(projected.len(), 3);
    assert!(projected.iter().all(|row| !row.name.is_empty()
        && !row.location.is_empty()
        && !row.verification.is_empty()));
    assert!(
        projected
            .iter()
            .any(|row| row.name == "original.txt" && row.side == "A")
    );
    assert!(
        projected
            .iter()
            .any(|row| row.name == "renamed.bin" && row.side == "B")
    );
    assert!(
        projected
            .iter()
            .all(|row| row.size.contains("byte") && !row.size.starts_with("0.0"))
    );
    let rows = w.get_search_results().iter().collect::<Vec<_>>();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().filter(|r| r.label.starts_with("A ·")).count(),
        1
    );
    assert_eq!(
        rows.iter().filter(|r| r.label.starts_with("B ·")).count(),
        2
    );
    assert!(rows.iter().all(|r| r.label.contains("Verified exact")));
    assert!(rows.iter().any(|r| r.label.contains("original.txt")));
    assert!(rows.iter().any(|r| r.label.contains("renamed.bin")));
    assert!(!rows.iter().any(|r| r.label.contains("a-only")));
    assert!(!rows.iter().any(|r| r.label.contains("unselected")));
    assert!(!rows.iter().any(|r| r.label.contains("same-size-different")));
    assert!(
        w.get_search_statistics()
            .contains("Effective duplicate budgets")
    );
    assert_eq!(
        h.borrow().service.lane.lock().unwrap().source_epoch,
        source_epoch
    );
    assert_eq!(
        h.borrow().current.as_ref().unwrap().quanta,
        inventory_quanta
    );
    // Selecting a displayed member is metadata-only and preserves the source.
    w.invoke_search_select(w.get_search_token(), 0, false);
    assert!(
        w.get_search_full_path().contains("original.txt")
            || w.get_search_full_path().contains("renamed.bin")
            || w.get_search_full_path().contains("another-name.dat")
    );
    assert_eq!(
        h.borrow().service.lane.lock().unwrap().source_epoch,
        source_epoch
    );
    // Measure the actual rendered panel after a real acquisition/comparison.
    let _visibility = TestWindowVisibility::new(&w);
    w.show().unwrap();
    for (width, height) in [(1200, 760), (1920, 1080), (780, 520)] {
        adapter.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::update_timers_and_animations();
        w.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
        adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        });
        eprintln!(
            "ab_geometry window={width}x{height} panel={} results={} settings={} two_columns={} controls={}",
            w.get_duplicate_panel_height(),
            w.get_duplicate_results_height(),
            w.get_duplicate_settings_height(),
            w.get_duplicate_two_columns(),
            w.get_search_controls_height()
        );
        if width >= 1200 {
            assert!(
                w.get_duplicate_results_height() / w.get_duplicate_panel_height() >= 0.60,
                "results must retain most of the panel beside protected-review controls"
            );
        } else {
            assert!(
                w.get_duplicate_results_height() >= 136.,
                "minimum window must retain four readable rows beside review controls"
            );
        }
    }
    // The form is presentation only: even offline members can be expanded,
    // collapsed and selected without another acquisition or content read.
    let selected_index = w.get_search_selected();
    let selected_path = w.get_search_full_path();
    let scopes = (w.get_duplicate_scope_a(), w.get_duplicate_scope_b());
    std::fs::rename(&a, temp.path().join("A-offline")).unwrap();
    std::fs::rename(&b, temp.path().join("B-offline")).unwrap();
    for expanded in [true, false, true] {
        w.set_duplicate_settings_expanded(expanded);
        h.borrow_mut().poll(&w, &c.borrow());
        assert_eq!(w.get_search_selected(), selected_index);
        assert_eq!(w.get_search_full_path(), selected_path);
        assert_eq!(
            (w.get_duplicate_scope_a(), w.get_duplicate_scope_b()),
            scopes
        );
        assert_eq!(w.get_duplicate_rows().iter().collect::<Vec<_>>(), projected);
        assert_eq!(
            h.borrow().service.lane.lock().unwrap().source_epoch,
            source_epoch
        );
        assert!(!w.get_search_busy());
    }
    w.invoke_close_search();
}

#[test]
fn duplicate_ab_workshop_cancel_restart_ignores_old_task_and_revocation_preserves_g0() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    for (label, root) in [("A restart", &a), ("B restart", &b)] {
        std::fs::create_dir(root).unwrap();
        std::fs::write(root.join("copy.bin"), vec![23u8; 512 * 1024]).unwrap();
        c.borrow_mut()
            .registry
            .add(crate::source_registry::Source::new(2, root.clone(), label, None).unwrap())
            .unwrap();
        w.set_scan_path(c.borrow_mut().path_input.select(root.clone()).into());
        w.invoke_start_scan();
        let epoch = h.borrow().epoch;
        wait(&h, &w, &c, |host| {
            host.current
                .as_ref()
                .is_some_and(|p| p.epoch == epoch && !p.busy && p.measure_ready)
        });
    }
    let before_epoch = h.borrow().service.lane.lock().unwrap().source_epoch;
    let before_quanta = h.borrow().current.as_ref().unwrap().quanta;
    w.invoke_show_analysis();
    w.invoke_duplicate_mode_changed(1);
    wait(&h, &w, &c, |_| {
        w.get_duplicate_source_labels()
            .iter()
            .any(|s| s.ends_with("B restart"))
    });
    for (side, name) in [(0, "A restart"), (1, "B restart")] {
        let index = w
            .get_duplicate_source_labels()
            .iter()
            .position(|s| s.ends_with(name))
            .unwrap();
        w.invoke_duplicate_select(side, index as i32);
    }
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    w.invoke_duplicate_run();
    let cancelled_id = h.borrow().search_id;
    w.invoke_search_cancel();
    // Starting a replacement closes the previous output mailbox immediately.
    w.invoke_duplicate_run();
    let replacement_id = h.borrow().search_id;
    assert!(replacement_id > cancelled_id);
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    assert_eq!(h.borrow().search_id, replacement_id);
    // Windows consultation uses directory listings; other platforms stay strict.
    // Exact positive groups remain usable without an implicit acquisition.
    assert_eq!(
        w.get_search_complete(),
        !cfg!(windows),
        "{}",
        w.get_search_status()
    );
    assert_eq!(
        w.get_search_status().contains("Incomplete coverage"),
        cfg!(windows)
    );
    assert!(
        !w.get_search_statistics()
            .contains("Archived directory sizes may be outdated")
    );
    assert_eq!(
        w.get_duplicate_warning()
            .contains("Verify the saved inventories before searching again"),
        cfg!(windows)
    );
    assert_eq!(w.get_search_matches(), "1");
    assert_eq!(w.get_search_results().row_count(), 1);
    assert_eq!(w.get_duplicate_folder_count(), "1");
    assert_eq!(w.get_duplicate_file_count(), "0");
    assert!(w.get_duplicate_folder_view());
    let expected = w.get_search_results().iter().collect::<Vec<_>>();
    for _ in 0..30 {
        h.borrow_mut().poll(&w, &c.borrow());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(h.borrow().search_id, replacement_id);
    assert_eq!(w.get_search_results().iter().collect::<Vec<_>>(), expected);
    assert_eq!(w.get_search_complete(), !cfg!(windows));
    // Inject the service's existing revocation boundary. This is a deterministic
    // disconnected-source test, not a local-source disconnect UI or live SMB.
    h.borrow()
        .service
        .disconnect_sources(std::slice::from_ref(&a));
    w.invoke_duplicate_run();
    assert!(!w.get_search_busy());
    assert!(!w.get_search_complete());
    assert!(w.get_search_status().contains("Incomplete coverage"));
    assert_eq!(
        h.borrow().service.lane.lock().unwrap().source_epoch,
        before_epoch
    );
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, before_quanta);
    w.invoke_close_search();
}

fn render_recipe(window: &SortSizeWindow, adapter: &MinimalSoftwareWindow) {
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1200, 800));
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 800];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    }));
}

fn click_recipe(window: &SortSizeWindow, x: f32, y: f32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    let position = slint::LogicalPosition::new(x, y);
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
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
}

/// Pointer events hit the rendered production buttons; acquisition and SQLite
/// publication use the real workers. This is not a native desktop session.
#[test]
fn recipe_cancel_pointer_from_duplicates_and_tree_stops_acquisition_without_restart() {
    let observed_entries = |lane: &service::Lane| {
        lane.scan_progress
            .as_ref()
            .filter(|progress| progress.source_epoch == lane.source_epoch)
            .map_or(lane.writer_entries, |progress| {
                progress
                    .processed_entries
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
    };
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let _visibility = TestWindowVisibility::new(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temporary = tempfile::tempdir().unwrap();
    for duplicates in [true, false] {
        let source = temporary
            .path()
            .join(if duplicates { "duplicates" } else { "tree" });
        std::fs::create_dir(&source).unwrap();
        for n in 0..2048 {
            std::fs::write(source.join(format!("file-{n:03}")), [n as u8; 8]).unwrap();
        }
        let lane = h.borrow().service.lane.clone();
        {
            let mut l = lane.lock().unwrap();
            l.tick_delay = Duration::from_millis(100);
            l.tick_gate = None;
            l.tick_blocked = false;
        }
        w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
        w.invoke_start_scan();
        let epoch = h.borrow().epoch;
        wait(&h, &w, &c, |_| {
            let lane = lane.lock().unwrap();
            lane.source_epoch == epoch
                && lane.writer_epoch == Some(epoch)
                && observed_entries(&lane) > 1
                && lane.writer_running
                && lane.acquisition_control.is_some()
        });
        // Hold a real native batch while software rendering and pointer routing run.
        // The 512-entry listing quantum is larger than the old fixture.
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        {
            let mut l = lane.lock().unwrap();
            l.tick_gate = Some(gate.clone());
            l.tick_blocked = false;
        }
        wait(&h, &w, &c, |_| lane.lock().unwrap().tick_blocked);
        if duplicates {
            w.invoke_show_analysis();
            w.invoke_duplicate_mode_changed(1);
            w.set_ribbon_tab(1);
            assert!(w.get_search_visible());
        } else {
            w.invoke_close_search();
            w.set_ribbon_tab(0);
            assert!(!w.get_search_visible());
        }
        render_recipe(&w, &adapter);
        let (x, y) = if duplicates {
            assert!(w.get_top_cancel_visible());
            assert!(w.get_top_cancel_enabled());
            (
                w.get_top_cancel_x() + w.get_top_cancel_width() / 2.,
                w.get_top_cancel_y() + w.get_top_cancel_height() / 2.,
            )
        } else {
            assert!(w.get_scan_cancel_enabled());
            (
                w.get_scan_cancel_x() + w.get_scan_cancel_width() / 2.,
                w.get_scan_cancel_y() + w.get_scan_cancel_height() / 2.,
            )
        };
        assert!(x > 0. && x < 1200. && y > 0. && y < 800.);
        click_recipe(&w, x, y);
        assert!(
            w.get_cancelling(),
            "rendered button did not request cancellation"
        );
        assert!(w.get_busy(), "request is not a terminal acknowledgement");
        render_recipe(&w, &adapter);
        assert_eq!(w.get_scan_cancel_text(), "Cancelling…");
        assert!(!w.get_scan_cancel_enabled());
        if duplicates {
            assert_eq!(w.get_top_cancel_text(), "Cancelling…");
            assert!(!w.get_top_cancel_enabled());
        }
        // The test barrier is separate from the real cancellation token.
        // Allow the worker to observe cancellation before awaiting its drain.
        lane.lock().unwrap().tick_gate = None;
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        wait(&h, &w, &c, |_| {
            let lane = lane.lock().unwrap();
            lane.phase
                .is_some_and(|(e, p, _)| e == epoch && matches!(p, service::Phase::Cancelled))
                && !lane.writer_running
                && lane.writer_in_flight == 0
                && !w.get_busy()
                && !w.get_cancelling()
        });
        assert!(
            w.get_status().starts_with("Cancelled"),
            "{}",
            w.get_status()
        );
        let stopped = {
            let lane = lane.lock().unwrap();
            (
                lane.source_epoch,
                lane.writer_quanta,
                observed_entries(&lane),
                lane.writer_generation,
            )
        };
        assert!(
            (2..2049).contains(&stopped.2),
            "fixture completed instead of being interrupted"
        );
        for _ in 0..40 {
            h.borrow_mut().poll_selection(&w, &mut c.borrow_mut());
            h.borrow_mut().poll(&w, &c.borrow());
            std::thread::sleep(Duration::from_millis(5));
        }
        let lane = lane.lock().unwrap();
        assert_eq!(
            (
                lane.source_epoch,
                lane.writer_quanta,
                observed_entries(&lane),
                lane.writer_generation
            ),
            stopped
        );
        assert!(!lane.writer_running);
        assert!(!lane.retry, "cancellation must not queue a retry");
        assert!(!w.get_busy());
        eprintln!(
            "recipe_cancel view={} epoch={} quanta={} entries={} terminal=Cancelled writer_running=false unchanged_after_40_polls=true",
            if duplicates {
                "Duplicates"
            } else {
                "Tree/Treemap"
            },
            stopped.0,
            stopped.1,
            stopped.2
        );
    }
}

fn ab_recipe_directories() -> (tempfile::TempDir, tempfile::TempDir) {
    let a = tempfile::tempdir().unwrap();
    let b = std::env::var_os("BUNNY_AB_SECOND_FIXTURE_DIR").map_or_else(
        || tempfile::tempdir().unwrap(),
        |root| tempfile::tempdir_in(root).unwrap(),
    );
    (a, b)
}

fn select_recipe_sources(
    w: &SortSizeWindow,
    h: &Rc<RefCell<Host>>,
    c: &Rc<RefCell<Controller>>,
    a: &Path,
    b: &Path,
) {
    w.invoke_show_analysis();
    w.invoke_duplicate_mode_changed(1);
    wait(h, w, c, |_| {
        w.get_duplicate_source_labels()
            .iter()
            .any(|s| s.ends_with("Recipe B"))
    });
    // Choosing folders directly must work without any source selection.
    let before = h.borrow().epoch;
    for (side, scope) in [(0, a), (1, b)] {
        if side == 0 {
            w.set_duplicate_scope_a(label(scope).into());
        } else {
            w.set_duplicate_scope_b(label(scope).into());
        }
        w.invoke_duplicate_scope_edited(side);
    }
    assert!(w.get_duplicate_scope_a_valid());
    assert!(w.get_duplicate_scope_b_valid());
    assert!(w.get_duplicate_source_a_info().contains("Recipe A"));
    assert!(w.get_duplicate_source_b_info().contains("Recipe B"));
    assert_eq!(h.borrow().epoch, before, "choosing folders cannot scan");
}

#[test]
fn recipe_ab_analyzes_only_two_selected_small_folders() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let (ta, tb) = ab_recipe_directories();
    let roots = [ta.path().join("source-a"), tb.path().join("source-b")];
    let selected = roots.each_ref().map(|r| r.join("chosen"));
    for (index, root) in roots.iter().enumerate() {
        std::fs::create_dir_all(root.join("chosen")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::fs::write(root.join("chosen/shared.txt"), b"chosen identical content").unwrap();
        for n in 0..40 {
            std::fs::write(
                root.join(format!("outside/uncounted-{n}")),
                b"outside identical content",
            )
            .unwrap();
        }
        c.borrow_mut()
            .registry
            .add(
                crate::source_registry::Source::new(
                    2,
                    root.clone(),
                    if index == 0 { "Recipe A" } else { "Recipe B" },
                    None,
                )
                .unwrap(),
            )
            .unwrap();
    }
    select_recipe_sources(&w, &h, &c, &selected[0], &selected[1]);
    let no_scan_epoch = h.borrow().epoch;
    for invalid in [
        label(tb.path()).into(),
        "chosen".into(),
        label(&roots[0].join("chosen/../outside")).into(),
    ] {
        w.set_duplicate_scope_a(invalid);
        w.invoke_duplicate_scope_edited(0);
        assert!(!w.get_duplicate_scope_a_valid());
        assert!(!w.get_duplicate_ready());
        w.invoke_duplicate_inventory(0);
        assert_eq!(
            h.borrow().epoch,
            no_scan_epoch,
            "invalid scope cannot launch acquisition"
        );
    }
    w.set_duplicate_scope_a(label(&selected[0]).into());
    w.invoke_duplicate_scope_edited(0);
    assert!(w.get_duplicate_scope_a_valid());
    assert!(w.get_duplicate_a_needs_inventory());
    assert!(w.get_duplicate_b_needs_inventory());
    assert!(w.get_duplicate_source_a_info().contains("Inventory absent"));
    assert!(w.get_duplicate_source_b_info().contains("Inventory absent"));
    assert!(!w.get_duplicate_ready());
    for (side, selected_scope) in selected.iter().enumerate() {
        w.invoke_duplicate_inventory(side as i32);
        assert!(c.borrow().pending_auto.is_none());
        assert!(w.get_search_visible());
        assert_eq!(w.get_duplicate_mode(), 1);
        let source_info = if side == 0 {
            w.get_duplicate_source_a_info()
        } else {
            w.get_duplicate_source_b_info()
        };
        assert!(source_info.contains("Scan in progress"), "{source_info}");
        let epoch = h.borrow().epoch;
        wait(&h, &w, &c, |host| {
            host.current
                .as_ref()
                .is_some_and(|p| p.epoch == epoch && !p.busy && p.measure_ready)
        });
        let host = h.borrow();
        let page = host.current.as_ref().unwrap();
        assert_eq!(host.requested.as_deref(), Some(selected_scope.as_path()));
        assert_eq!(page.frame.rows.len(), 1);
        assert!(
            page.frame
                .rows
                .iter()
                .all(|r| r.path.starts_with(selected_scope))
        );
        let writer_entries = host.service.lane.lock().unwrap().writer_entries;
        assert_eq!(
            writer_entries, 2,
            "only selected folder and its one file may be inventoried"
        );
        eprintln!(
            "recipe_ab side={} scanned={} writer_entries=2 outside_files=40 excluded=true",
            side,
            label(selected_scope)
        );
    }
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    assert!(
        w.get_duplicate_source_a_info()
            .contains("Inventory available")
    );
    assert!(
        w.get_duplicate_source_b_info()
            .contains("Inventory available")
    );
    let before = h.borrow().service.lane.lock().unwrap().source_epoch;
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    assert!(w.get_search_complete(), "{}", w.get_search_status());
    assert_eq!(w.get_search_matches(), "1");
    assert_eq!(w.get_search_results().row_count(), 1);
    assert_eq!(w.get_duplicate_folder_count(), "1");
    assert_eq!(w.get_duplicate_file_count(), "0");
    assert!(w.get_duplicate_folder_view());
    w.invoke_duplicate_open_folder(0);
    wait(&h, &w, &c, |_| w.get_duplicate_folder_detail());
    assert_eq!(w.get_search_results().row_count(), 2);
    assert!(
        w.get_search_results()
            .iter()
            .all(|r| r.label.contains("shared.txt") && !r.label.contains("uncounted"))
    );
    assert_eq!(h.borrow().service.lane.lock().unwrap().source_epoch, before);
}

#[test]
fn recipe_ab_reuses_broader_inventory_without_expanding_selected_folders() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let (ta, tb) = ab_recipe_directories();
    let roots = [ta.path().join("source-a"), tb.path().join("source-b")];
    let selected = roots.each_ref().map(|r| r.join("chosen"));
    for (index, root) in roots.iter().enumerate() {
        std::fs::create_dir_all(root.join("chosen")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::fs::write(root.join("chosen/shared.txt"), b"chosen identical content").unwrap();
        std::fs::write(
            root.join("outside/excluded.txt"),
            b"outside identical content",
        )
        .unwrap();
        c.borrow_mut()
            .registry
            .add(
                crate::source_registry::Source::new(
                    2,
                    root.clone(),
                    if index == 0 { "Recipe A" } else { "Recipe B" },
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        w.set_scan_path(c.borrow_mut().path_input.select(root.clone()).into());
        w.invoke_start_scan();
        let epoch = h.borrow().epoch;
        wait(&h, &w, &c, |host| {
            host.current
                .as_ref()
                .is_some_and(|p| p.epoch == epoch && !p.busy && p.measure_ready)
        });
    }
    let before = {
        let host = h.borrow();
        let lane = host.service.lane.lock().unwrap();
        (lane.source_epoch, lane.writer_quanta, lane.writer_entries)
    };
    select_recipe_sources(&w, &h, &c, &selected[0], &selected[1]);
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    assert!(!w.get_duplicate_a_needs_inventory());
    assert!(!w.get_duplicate_b_needs_inventory());
    assert!(
        w.get_duplicate_source_a_info()
            .contains("Inventory available")
    );
    assert!(
        w.get_duplicate_source_b_info()
            .contains("Inventory available")
    );
    assert!(w.get_duplicate_source_a_info().contains("Recipe A"));
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    // Windows consultation uses directory listings; other platforms stay strict.
    // Exact positive groups remain usable without an implicit acquisition.
    assert_eq!(
        w.get_search_complete(),
        !cfg!(windows),
        "{}",
        w.get_search_status()
    );
    assert_eq!(
        w.get_search_status().contains("Incomplete coverage"),
        cfg!(windows)
    );
    assert!(
        !w.get_search_statistics()
            .contains("Archived directory sizes may be outdated")
    );
    assert_eq!(
        w.get_duplicate_warning()
            .contains("Verify the saved inventories before searching again"),
        cfg!(windows)
    );
    assert!(!w.get_duplicate_settings_expanded());
    assert_eq!(w.get_duplicate_warning().is_empty(), !cfg!(windows));
    assert!(!w.get_duplicate_details_expanded());
    assert!(
        !w.get_search_statistics()
            .contains("Source continuity is unverified")
    );
    assert_eq!(
        w.get_search_matches(),
        "1",
        "outside duplicate group must remain excluded"
    );
    assert_eq!(w.get_search_results().row_count(), 1);
    assert_eq!(w.get_duplicate_folder_count(), "1");
    w.invoke_duplicate_open_folder(0);
    wait(&h, &w, &c, |_| w.get_duplicate_folder_detail());
    assert_eq!(w.get_search_results().row_count(), 2);
    assert!(
        w.get_search_results()
            .iter()
            .all(|r| r.label.contains("shared.txt") && !r.label.contains("excluded"))
    );
    let host = h.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert_eq!(
        (lane.source_epoch, lane.writer_quanta, lane.writer_entries),
        before,
        "reuse must not start another acquisition"
    );
    eprintln!(
        "recipe_ab broader_inventories_reused=true selected_groups=1 selected_members=2 outside_group_excluded=true acquisition_unchanged=true"
    );
    drop(lane);
    drop(host);
    // A retained ancestor is reusable, and an explicit refresh must still
    // acquire exactly the selected folder instead of its recorded ancestor.
    w.invoke_duplicate_inventory(0);
    let refresh_epoch = h.borrow().epoch;
    assert!(refresh_epoch > before.0);
    assert_eq!(h.borrow().requested.as_deref(), Some(selected[0].as_path()));
    assert!(w.get_search_visible() && w.get_duplicate_mode() == 1);
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.epoch == refresh_epoch && !p.busy && p.measure_ready)
    });
    assert_eq!(h.borrow().service.lane.lock().unwrap().writer_entries, 2);
    assert!(
        h.borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .rows
            .iter()
            .all(|r| r.path.starts_with(&selected[0]))
    );
    eprintln!(
        "recipe_ab explicit_refresh_after_ancestor_reuse=true writer_entries=2 scope_exact=true"
    );
    // The corrective action refreshes each exact scope with strict acquisition.
    // On Windows, refreshing A alone cannot certify the still-listing B inventory.
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    w.invoke_duplicate_inventory(1);
    let second_refresh_epoch = h.borrow().epoch;
    assert!(second_refresh_epoch > refresh_epoch);
    assert_eq!(h.borrow().requested.as_deref(), Some(selected[1].as_path()));
    wait(&h, &w, &c, |host| {
        host.current
            .as_ref()
            .is_some_and(|p| p.epoch == second_refresh_epoch && !p.busy && p.measure_ready)
            && w.get_duplicate_ready()
    });
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    assert!(w.get_search_complete(), "{}", w.get_search_status());
    assert!(w.get_duplicate_warning().is_empty());
    assert_eq!(w.get_search_matches(), "1");
    assert_eq!(w.get_duplicate_folder_count(), "1");
}

#[test]
fn recipe_cancel_stale_ready_pointer_reports_completed_acquisition() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let _visibility = TestWindowVisibility::new(&w);
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("completed");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), b"completed before click").unwrap();
    w.set_scan_path(c.borrow_mut().path_input.select(root).into());
    w.invoke_start_scan();
    wait(&h, &w, &c, |host| {
        let lane = host.service.lane.lock().unwrap();
        lane.phase
            .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Ready))
            && !lane.writer_running
            && host
                .current
                .as_ref()
                .is_some_and(|p| !p.busy && p.measure_ready)
    });
    let before = {
        let host = h.borrow();
        let lane = host.service.lane.lock().unwrap();
        (lane.source_epoch, lane.writer_quanta, lane.writer_entries)
    };
    w.invoke_show_analysis();
    w.invoke_duplicate_mode_changed(1);
    w.set_ribbon_tab(1);
    // A retained UI page can be older than the independent acquisition lane.
    h.borrow_mut().current.as_mut().unwrap().busy = true;
    w.set_busy(true);
    w.set_status("Scan in progress…".into());
    render_recipe(&w, &adapter);
    assert!(w.get_top_cancel_visible() && w.get_top_cancel_enabled());
    click_recipe(
        &w,
        w.get_top_cancel_x() + w.get_top_cancel_width() / 2.,
        w.get_top_cancel_y() + w.get_top_cancel_height() / 2.,
    );
    assert!(!w.get_busy());
    assert!(!w.get_cancelling());
    assert!(!h.borrow().current.as_ref().unwrap().busy);
    let ready_label = {
        let _scope = c.borrow().presentation.enter();
        service::Phase::Ready.label().to_owned()
    };
    assert!(
        w.get_status().starts_with(&ready_label),
        "{}",
        w.get_status()
    );
    h.borrow_mut().poll(&w, &c.borrow());
    let host = h.borrow();
    let lane = host.service.lane.lock().unwrap();
    assert_eq!(
        (lane.source_epoch, lane.writer_quanta, lane.writer_entries),
        before
    );
    assert!(lane.cancel_requested_epoch.is_none());
    assert!(!lane.cancel && !lane.retry && !lane.writer_running);
    eprintln!(
        "recipe_cancel stale_ui=true actual_phase=Ready false_cancellation=false acquisition_unchanged=true"
    );
}

/// Exercises real inventory acquisition, worker commands and Slint projection.
/// No mocked row identity is used to preserve a review choice across views.
#[test]
fn protected_review_workshop_reference_preview_partial_and_context_reset() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let h = connect(&w, &c).unwrap();
    fixed_profile(&w, &c, &h);
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("reference-original");
    let b = temp.path().join("copies-different-root");
    for (name, root) in [("Review A", &a), ("Review B", &b)] {
        std::fs::create_dir_all(root.join("empty")).unwrap();
        for index in 0..3 {
            std::fs::write(root.join(format!("file-{index}.bin")), [index as u8; 11]).unwrap();
        }
        c.borrow_mut()
            .registry
            .add(crate::source_registry::Source::new(2, root.clone(), name, None).unwrap())
            .unwrap();
        w.set_scan_path(c.borrow_mut().path_input.select(root.clone()).into());
        w.invoke_start_scan();
        let epoch = h.borrow().epoch;
        wait(&h, &w, &c, |host| {
            host.current
                .as_ref()
                .is_some_and(|p| p.epoch == epoch && !p.busy && p.measure_ready)
        });
    }
    w.invoke_show_analysis();
    w.invoke_duplicate_mode_changed(1);
    wait(&h, &w, &c, |_| {
        w.get_duplicate_source_labels()
            .iter()
            .any(|name| name.ends_with("Review B"))
    });
    for (side, suffix) in [(0, "Review A"), (1, "Review B")] {
        let index = w
            .get_duplicate_source_labels()
            .iter()
            .position(|name| name.ends_with(suffix))
            .unwrap();
        w.invoke_duplicate_select(side, index as i32);
    }
    // Labels exist before the independent autosaves are committed. Invoke
    // the action only when its real UI enablement admits both inventories.
    wait(&h, &w, &c, |_| w.get_duplicate_ready());
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    // Windows consultation uses directory listings; other platforms stay strict.
    // Exact positive groups remain usable without an implicit acquisition.
    assert_eq!(
        w.get_search_complete(),
        !cfg!(windows),
        "{}",
        w.get_search_status()
    );
    assert_eq!(
        w.get_search_status().contains("Incomplete coverage"),
        cfg!(windows)
    );
    assert!(
        !w.get_search_statistics()
            .contains("Archived directory sizes may be outdated")
    );
    assert_eq!(
        w.get_duplicate_warning()
            .contains("Verify the saved inventories before searching again"),
        cfg!(windows)
    );
    #[cfg(windows)]
    {
        // Listing archives retain consultable exact groups, but protected review
        // keeps its existing complete-evidence gate until explicit verification.
        w.invoke_duplicate_reference_changed(0);
        wait(&h, &w, &c, |_| {
            !w.get_duplicate_selection_summary().is_empty()
                && w.get_duplicate_protected_reference() == 0
        });
        assert!(!w.get_duplicate_selection_ready());
        assert!(w.get_duplicate_rows().iter().all(|row| !row.selectable));
        w.invoke_duplicate_reference_changed(-1);
        for (side, root) in [(0, &a), (1, &b)] {
            wait(&h, &w, &c, |_| w.get_duplicate_ready());
            w.invoke_duplicate_inventory(side);
            let refresh_epoch = h.borrow().epoch;
            assert_eq!(h.borrow().requested.as_deref(), Some(root.as_path()));
            wait(&h, &w, &c, |host| {
                host.current
                    .as_ref()
                    .is_some_and(|p| p.epoch == refresh_epoch && !p.busy && p.measure_ready)
                    && w.get_duplicate_ready()
            });
        }
        w.invoke_duplicate_run();
        wait(&h, &w, &c, |_| {
            w.get_search_duplicates_visible() && !w.get_search_busy()
        });
        assert!(w.get_search_complete(), "{}", w.get_search_status());
        assert!(w.get_duplicate_warning().is_empty());
    }
    assert_eq!(w.get_duplicate_rows().row_count(), 1);
    assert_eq!(w.get_duplicate_protected_reference(), -1);
    assert_eq!(w.get_duplicate_category_index(), 0);
    assert!(!w.get_duplicate_rows().row_data(0).unwrap().selected);
    w.invoke_duplicate_reference_changed(0);
    wait(&h, &w, &c, |_| w.get_duplicate_selection_ready());
    let folder = w.get_duplicate_rows().row_data(0).unwrap();
    assert!(folder.selectable && !folder.selected && !folder.partial);
    w.invoke_duplicate_selection_preview();
    wait(&h, &w, &c, |_| w.get_duplicate_selection_preview_open());
    assert!(w.get_duplicate_selection_preview_text().contains("33"));
    assert!(
        !w.get_duplicate_rows().row_data(0).unwrap().selected,
        "preview must not apply selection"
    );
    w.invoke_duplicate_selection_apply();
    wait(&h, &w, &c, |_| {
        w.get_duplicate_rows().row_data(0).unwrap().selected
    });
    let selected_summary = w.get_duplicate_selection_summary();
    assert!(selected_summary.contains("33"));
    let source_epoch = h.borrow().service.lane.lock().unwrap().source_epoch;
    let quanta = h.borrow().current.as_ref().unwrap().quanta;
    w.invoke_duplicate_open_folder(0);
    wait(&h, &w, &c, |_| w.get_duplicate_folder_detail());
    let child = w
        .get_duplicate_rows()
        .iter()
        .enumerate()
        .find(|(_, row)| row.side == "B" && row.name.ends_with(".bin"))
        .unwrap();
    assert!(child.1.selectable && child.1.selected);
    assert!(
        w.get_duplicate_rows()
            .iter()
            .filter(|r| r.side == "A")
            .all(|r| !r.selectable && !r.selected)
    );
    w.invoke_search_select(w.get_search_token(), child.0 as i32, false);
    let consulted_path = w.get_search_full_path();
    w.invoke_duplicate_toggle_copy(child.0 as i32, false);
    wait(&h, &w, &c, |_| {
        !w.get_duplicate_rows().row_data(child.0).unwrap().selected
    });
    assert!(w.get_duplicate_selection_summary().contains("22"));
    assert_eq!(w.get_search_selected(), child.0 as i32);
    assert_eq!(w.get_search_full_path(), consulted_path);
    w.invoke_duplicate_category_changed(0);
    wait(&h, &w, &c, |_| !w.get_duplicate_folder_detail());
    let folder = w.get_duplicate_rows().row_data(0).unwrap();
    assert!(folder.partial && !folder.selected);
    assert!(w.get_duplicate_selection_summary().contains("22"));
    w.invoke_duplicate_category_changed(1);
    wait(&h, &w, &c, |_| !w.get_duplicate_folder_view());
    assert_eq!(w.get_duplicate_category_index(), 1);
    assert!(w.get_duplicate_selection_summary().contains("22"));
    w.invoke_duplicate_category_changed(0);
    wait(&h, &w, &c, |_| w.get_duplicate_folder_view());
    w.invoke_duplicate_reference_changed(1);
    wait(&h, &w, &c, |_| {
        w.get_duplicate_selection_ready()
            && w.get_duplicate_protected_reference() == 1
            && !w.get_duplicate_rows().row_data(0).unwrap().partial
    });
    assert!(!w.get_duplicate_rows().row_data(0).unwrap().selected);
    w.invoke_duplicate_toggle_copy(0, true);
    wait(&h, &w, &c, |_| {
        w.get_duplicate_rows().row_data(0).unwrap().selected
    });
    w.invoke_duplicate_open_folder(0);
    wait(&h, &w, &c, |_| w.get_duplicate_folder_detail());
    assert!(
        w.get_duplicate_rows()
            .iter()
            .filter(|r| r.side == "B")
            .all(|r| !r.selectable && !r.selected)
    );
    assert!(
        w.get_duplicate_rows()
            .iter()
            .filter(|r| r.side == "A" && r.name.ends_with(".bin"))
            .all(|r| r.selectable && r.selected)
    );
    w.invoke_duplicate_selection_clear();
    wait(&h, &w, &c, |_| {
        w.get_duplicate_rows().iter().all(|r| !r.selected)
    });
    assert_eq!(
        h.borrow().service.lane.lock().unwrap().source_epoch,
        source_epoch
    );
    assert_eq!(h.borrow().current.as_ref().unwrap().quanta, quanta);
    // A fresh comparison retires both selected identities and pending previews.
    w.invoke_duplicate_selection_preview();
    wait(&h, &w, &c, |_| w.get_duplicate_selection_preview_open());
    w.invoke_duplicate_run();
    wait(&h, &w, &c, |_| {
        w.get_search_duplicates_visible() && !w.get_search_busy()
    });
    assert!(!w.get_duplicate_selection_preview_open());
    assert!(w.get_duplicate_rows().iter().all(|r| !r.selected));
    let source_a = w.get_duplicate_source_a();
    w.invoke_duplicate_select(1, source_a);
    assert!(!w.get_duplicate_selection_ready());
    assert!(!w.get_duplicate_selection_preview_open());
    w.invoke_close_search();
}
