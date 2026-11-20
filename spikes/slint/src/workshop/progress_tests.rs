//! UI publication checks; actual intermediate acquisition is covered by the
//! service tests. This fixture deliberately injects a provisional publication.
use super::*;
use slint::{
    Model,
    platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use std::time::{Duration, Instant};

#[test]
fn scan_progress_samples_the_current_epoch_without_a_page() {
    use std::sync::atomic::{AtomicU64, Ordering};

    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    let mut host = host.borrow_mut();
    let count = Arc::new(AtomicU64::new(1));
    {
        let mut lane = host.service.lane.lock().unwrap();
        lane.source_epoch = 9;
        lane.phase = Some((9, service::Phase::Acquiring, GenerationId(1)));
        lane.scan_progress = Some(scan_p3::Progress {
            source_epoch: 9,
            processed_entries: count.clone(),
        });
    }
    let _scope = controller.borrow().presentation.enter();
    // The production poll uses try_lock and intentionally defers on
    // contention with the worker; wait for a successful sample.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        host.poll_scan_progress(&window);
        if !window.get_scan_progress().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "progress sample was not published"
        );
        std::thread::yield_now();
    }
    let first = window.get_scan_progress();
    assert!(first.ends_with("1 entry processed"));
    let many = u64::from(u32::MAX) + 1;
    count.store(many, Ordering::Relaxed);
    host.scan_progress_poll = Some(Instant::now());
    host.poll_scan_progress(&window);
    assert_eq!(window.get_scan_progress(), first);
    host.scan_progress_poll = None;
    while host.scan_progress_poll.is_none() {
        host.poll_scan_progress(&window);
        assert!(
            Instant::now() < deadline,
            "updated progress sample was not published"
        );
        std::thread::yield_now();
    }
    assert!(window.get_scan_progress().ends_with(&tr_count!(
        "{n} entry processed",
        "{n} entries processed",
        many
    )));
    host.service.lane.lock().unwrap().source_epoch = 10;
    host.scan_progress_poll = None;
    while host.scan_progress_poll.is_none() {
        host.poll_scan_progress(&window);
        assert!(
            Instant::now() < deadline,
            "stale progress sample was not cleared"
        );
        std::thread::yield_now();
    }
    assert!(window.get_scan_progress().is_empty());
    assert!(host.current.is_none());
}

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn until(
    host: &Rc<RefCell<Host>>,
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
    predicate: impl Fn(&Host) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        host.borrow_mut().poll(window, &controller.borrow());
        assert!(window.get_errors().is_empty(), "{}", window.get_errors());
        if predicate(&host.borrow()) {
            return;
        }
        assert!(Instant::now() < deadline, "UI publication timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn progressive_publication_keeps_tiles_stable_and_labels_the_partial_surface() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    window.invoke_memory_select(1);
    until(&host, &window, &controller, |_| {
        sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == MemoryMode::Entries50k
    });
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("small"), [1; 3]).unwrap();
    std::fs::write(source.join("large"), [2; 9]).unwrap();
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(source.clone())
            .into(),
    );
    window.invoke_start_scan();
    until(&host, &window, &controller, |host| {
        host.current
            .as_ref()
            .is_some_and(|page| !page.busy && page.measure_ready)
    });
    assert_eq!(window.get_tiles().row_count(), 2);
    let model = window.get_tiles();
    let token = window.get_workshop_token();
    host.borrow_mut().navigate(Navigate::Refresh);
    until(&host, &window, &controller, |_| {
        window.get_workshop_token() != token
    });
    assert_eq!(
        model,
        window.get_tiles(),
        "an unchanged publication must keep the Slint tile model"
    );

    // Exercise Host's real partial-publication presentation without claiming
    // that this controlled injection is an acquisition observation.
    let mut page = host.borrow_mut().current.take().unwrap();
    let frame = Arc::get_mut(&mut page.frame).expect("publication owns its frame");
    frame.provisional = true;
    frame.coverage = ScopeCoverage::Partial;
    page.measure_ready = false;
    page.busy = true;
    page.status = "Scan in progress…".into();
    host.borrow_mut().apply(&window, page);
    assert_eq!(window.get_view_warning().as_str(), tr!("Partial"));
    assert!(window.get_treemap_note().contains(tr!("Partial")));
    assert!(window.get_busy());
    assert_eq!(window.get_tiles().row_count(), 2);
    let partial_model = window.get_tiles();
    window.invoke_set_treemap_mode(1);
    assert_eq!(window.get_treemap_mode(), 1);
    assert_eq!(window.get_tiles(), partial_model);
    window.invoke_set_treemap_mode(0);
    assert_eq!(window.get_treemap_mode(), 0);
    assert_eq!(window.get_tiles(), partial_model);
    let tile = window.get_tiles().row_data(0).unwrap();
    window.invoke_workshop_action(window.get_workshop_token(), tile.node_id, false);
    assert_eq!(window.get_workshop_selected(), tile.node_id);
    assert!(!window.get_entry_path().is_empty());
    // The aggregate has no underlying row: activation must not change the
    // selection or navigate to a path, in either treemap presentation.
    let mut page = host.borrow_mut().current.take().unwrap();
    let group_index = page.tiles.len() as i32;
    page.tiles.push(MapTile {
        node_id: -1,
        name: "Other observed data outside this page".into(),
        band_w: 0.5,
        w: 0.5,
        h: 1.,
        partial: true,
        ..Default::default()
    });
    host.borrow_mut().apply(&window, page);
    let selected = host.borrow().selected;
    let epoch = host.borrow().epoch;
    let entry_path = window.get_entry_path();
    for mode in [0, 1] {
        window.invoke_set_treemap_mode(mode);
        window.invoke_activate_tile(group_index);
        assert_eq!(host.borrow().epoch, epoch);
        assert_eq!(host.borrow().selected, selected);
        assert_eq!(window.get_entry_path(), entry_path);
    }
    assert_eq!(std::fs::read(source.join("small")).unwrap(), [1; 3]);
    assert_eq!(std::fs::read(source.join("large")).unwrap(), [2; 9]);
}

#[cfg(target_os = "linux")]
#[test]
fn completed_scope_shows_a_known_subtotal_without_turning_unknown_sizes_into_zero() {
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))
    .unwrap();
    let (window, controller) = crate::new_session(Default::default(), Some("en".into())).unwrap();
    let host = connect(&window, &controller).unwrap();
    window.invoke_memory_select(1);
    until(&host, &window, &controller, |_| {
        sortsize_core::resources::Governor::global()
            .usage()
            .limits
            .requested
            == MemoryMode::Entries50k
    });
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("small"), [1; 3]).unwrap();
    std::fs::write(source.join("large"), [2; 9]).unwrap();
    std::os::unix::fs::symlink(source.join("absent"), source.join("refused-link")).unwrap();
    window.set_scan_path(controller.borrow_mut().path_input.select(source).into());
    window.invoke_start_scan();
    until(&host, &window, &controller, |host| {
        host.current
            .as_ref()
            .is_some_and(|page| !page.busy && page.measure_ready)
    });
    let h = host.borrow();
    let page = h.current.as_ref().unwrap();
    assert_eq!(page.summary, "≥ 12 bytes");
    assert!(page.current_folder_summary.starts_with("≥ 12 bytes · "));
    assert_eq!(page.tiles.len(), 2);
    assert!(page.tiles.iter().all(|tile| tile.partial));
    assert_eq!(window.get_view_warning(), "Partial");
    let small = page.tiles.iter().find(|tile| tile.name == "small").unwrap();
    let large = page.tiles.iter().find(|tile| tile.name == "large").unwrap();
    assert!((small.w * small.h - 0.25).abs() < 0.00001);
    assert!((large.w * large.h - 0.75).abs() < 0.00001);
    drop(h);

    let unknown = temporary.path().join("unknown");
    let empty = temporary.path().join("empty");
    std::fs::create_dir(&unknown).unwrap();
    std::fs::create_dir(&empty).unwrap();
    std::os::unix::fs::symlink(unknown.join("absent"), unknown.join("refused-link")).unwrap();
    for (path, summary, folder_prefix, coverage) in [
        (unknown, "—", "Unknown · ", ScopeCoverage::Partial),
        (empty, "0 bytes", "0 bytes · ", ScopeCoverage::Complete),
    ] {
        window.set_scan_path(controller.borrow_mut().path_input.select(path).into());
        window.invoke_start_scan();
        let epoch = host.borrow().epoch;
        until(&host, &window, &controller, |host| {
            host.current
                .as_ref()
                .is_some_and(|page| page.epoch == epoch && !page.busy && page.measure_ready)
        });
        let h = host.borrow();
        let page = h.current.as_ref().unwrap();
        assert_eq!(page.frame.coverage, coverage);
        assert_eq!(page.summary, summary);
        assert!(page.current_folder_summary.starts_with(folder_prefix));
    }
}
