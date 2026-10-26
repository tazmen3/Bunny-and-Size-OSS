use super::*;
use sortsize_core::{CancellationToken, InventoryLimits, ScanOptions};

fn settle(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while window.get_busy() {
        controller.borrow_mut().poll(window);
        assert!(
            Instant::now() < deadline,
            "directory request did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn fixture() -> TempDir {
    let fixture = TempDir::new().unwrap();
    for directory in ["first", "second"] {
        let folder = fixture.path().join(directory);
        fs::create_dir(&folder).unwrap();
        let mut seed = folder.join("f-00000");
        for i in 0..50_010 {
            let path = folder.join(format!("f-{i:05}"));
            if i % 500 == 0 {
                fs::write(&path, [0; 3]).unwrap();
                seed = path;
            } else {
                fs::hard_link(&seed, path).unwrap();
            }
        }
    }
    fixture
}
#[test]
fn capped_fifty_thousand_inventory_becomes_explorable_with_pages_cache_and_immutable_totals() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = new_window().unwrap();
    let controller = connect(&window);
    let fixture = fixture();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    settle(&window, &controller);
    let initial_summary = window.get_summary();
    let initial_ranking = window
        .get_top_rows()
        .iter()
        .map(|r| r.path)
        .collect::<Vec<_>>();
    let fingerprint = controller
        .borrow()
        .tree
        .as_ref()
        .unwrap()
        .aggregate_fingerprint();
    assert_eq!(
        controller.borrow().tree.as_ref().unwrap().node_count(),
        50_000
    );
    let folders: Vec<_> = window
        .get_details()
        .iter()
        .filter(|r| r.directory)
        .collect();
    let missing = folders
        .iter()
        .find(|row| {
            let mut state = controller.borrow_mut();
            let tree = state.tree.as_mut().unwrap();
            tree.select(row.node_id as usize);
            tree.needs_details() && tree.details().0.is_empty()
        })
        .expect("one complete sibling must be omitted after the cap");
    window.invoke_select_node(missing.node_id);
    assert!(window.get_busy());
    window.invoke_go_parent(); // Navigation before the first patch must be immediate.
    assert!(!window.get_busy());
    window.invoke_select_node(missing.node_id);
    assert!(window.get_busy());
    settle(&window, &controller);
    assert!(window.get_summary().ends_with(initial_summary.as_str()));
    assert_eq!(window.get_details().row_count(), 2_000);
    assert!(
        window
            .get_view_warning()
            .contains("Total initial du dossier")
    );
    assert_eq!(
        controller
            .borrow()
            .directory
            .original
            .as_ref()
            .unwrap()
            .aggregate_fingerprint(),
        fingerprint
    );
    let first_page: Vec<_> = window.get_details().iter().map(|r| r.path).collect();
    window.set_top_selected(-1);
    window.invoke_reveal_top();
    assert!(controller.borrow().directory.original.is_some());
    assert_eq!(
        window
            .get_details()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        first_page
    );

    window.invoke_change_page(true, true);
    let last_page = window
        .get_details()
        .iter()
        .map(|r| r.path)
        .collect::<Vec<_>>();
    window.invoke_change_page(true, true);
    settle(&window, &controller);
    // Keep an unfinished request alive independently of filesystem timing.
    // Returning to cached window zero must drop/cancel it before the early return.
    let abandoned = CancellationToken::default();
    let abandoned_mailbox = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox::default()));
    controller
        .borrow_mut()
        .directory
        .replace_task(worker::Task {
            cancellation: abandoned.clone(),
            mailbox: abandoned_mailbox.clone(),
        });
    window.set_busy(true);
    let generation = controller.borrow().generation;
    window.invoke_change_page(true, false);
    assert!(abandoned.is_cancelled());
    assert_eq!(controller.borrow().generation, generation + 1);
    assert!(!window.get_busy());
    abandoned_mailbox.lock().unwrap().completion = Some(Err(worker::Failure::Panicked));
    controller.borrow_mut().poll(&window);
    assert!(!window.get_status().contains("échoué"));
    assert_eq!(
        window
            .get_details()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        last_page
    );
    window.invoke_change_page(true, false);
    assert_eq!(
        window
            .get_details()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        first_page
    );

    // Two UI pages per acquisition window; retain at most four inactive windows.
    for _ in 0..6 {
        window.invoke_change_page(true, true);
        window.invoke_change_page(true, true);
        settle(&window, &controller);
        assert!(controller.borrow().directory.cache_len() <= 4);
        assert_eq!(window.get_details().row_count(), 2_000);
        assert!(window.get_summary().ends_with(initial_summary.as_str()));
    }
    window.invoke_go_parent();
    assert!(!window.get_busy());
    assert!(controller.borrow().directory.original.is_none());
    window.invoke_select_node(missing.node_id);
    settle(&window, &controller); // Page zero was evicted and must be read again.
    assert_eq!(
        window
            .get_details()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        first_page
    );
    window.invoke_go_parent();
    window.invoke_select_node(missing.node_id);
    assert!(!window.get_busy(), "recent page must be a cache hit");
    window.invoke_set_language(3);
    assert!(
        window
            .get_view_warning()
            .contains("Ursprüngliche Ordnergröße")
    );
    assert_eq!(
        window
            .get_top_rows()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        initial_ranking
    );
    window.invoke_set_units(true);
    assert_eq!(
        window
            .get_top_rows()
            .iter()
            .map(|r| r.path)
            .collect::<Vec<_>>(),
        initial_ranking
    );
    window.invoke_set_units(false);
    window.invoke_set_language(2);
    assert_eq!(
        controller
            .borrow()
            .directory
            .original
            .as_ref()
            .unwrap()
            .aggregate_fingerprint(),
        fingerprint
    );
    // A refresh observes changed content without adding it to the original tree.
    let before_change = controller
        .borrow()
        .tree
        .as_ref()
        .unwrap()
        .aggregate_fingerprint();
    fs::write(
        fixture.path().join(missing.name.as_str()).join("f-00000"),
        [0; 11],
    )
    .unwrap();
    window.invoke_load_folder_details();
    settle(&window, &controller);
    assert!(window.get_summary().ends_with(initial_summary.as_str()));
    assert_eq!(
        controller
            .borrow()
            .directory
            .original
            .as_ref()
            .unwrap()
            .aggregate_fingerprint(),
        fingerprint
    );
    assert_ne!(
        controller
            .borrow()
            .tree
            .as_ref()
            .unwrap()
            .aggregate_fingerprint(),
        before_change
    );
    let folder = fixture.path().join(missing.name.as_str());
    let moved = fixture.path().join("temporarily-moved");
    fs::rename(&folder, &moved).unwrap();
    window.invoke_load_folder_details();
    settle(&window, &controller);
    assert!(!window.get_errors().is_empty());
    fs::rename(&moved, &folder).unwrap();
    window.invoke_load_folder_details(); // Retry must work even with an empty failed view.
    settle(&window, &controller);
    assert_eq!(window.get_details().row_count(), 2_000);
    assert!(window.get_errors().is_empty());
    assert_eq!(
        controller
            .borrow()
            .directory
            .original
            .as_ref()
            .unwrap()
            .aggregate_fingerprint(),
        fingerprint
    );
    window.invoke_go_parent();
    controller.borrow_mut().directory.expire_cache();
    window.invoke_select_node(missing.node_id);
    assert!(
        window.get_busy(),
        "expired cached snapshots must be read again"
    );
    settle(&window, &controller);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A finalized partial listing must not lose its error on a cached return.
        let permissions = fs::metadata(&folder).unwrap().permissions();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
        window.invoke_load_folder_details();
        settle(&window, &controller);
        assert!(!window.get_errors().is_empty());
        window.invoke_go_parent();
        window.invoke_select_node(missing.node_id);
        settle(&window, &controller);
        let errors = window.get_errors();
        fs::set_permissions(&folder, permissions).unwrap();
        assert!(
            !errors.is_empty(),
            "returning to a partial listing hid its access error"
        );
        window.invoke_load_folder_details();
        settle(&window, &controller);
        assert!(window.get_errors().is_empty());
    }
    // A deliberately unfinished request must allow a complete retained window
    // to advance. Inject prepared real FS patches to avoid any timing assumption.
    window.invoke_load_folder_details();
    let mailbox = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox::default()));
    controller
        .borrow_mut()
        .directory
        .replace_task(worker::Task {
            cancellation: Default::default(),
            mailbox: mailbox.clone(),
        });
    let token = CancellationToken::default();
    let mut projector = tree::Projector::default();
    let mut staged = false;
    sortsize_core::scan_inventory_progressive(
        &folder,
        &ScanOptions::default(),
        &token,
        sortsize_core::ProgressiveInventoryOptions {
            direct_children_only: true,
            limits: InventoryLimits {
                max_entries: 4001,
                max_path_bytes: 1024 * 1024,
            },
            ..Default::default()
        },
        |_, preview| {
            if !staged && preview.direct_children_seen > 4000 {
                for _ in 0..2 {
                    let patch = projector.patch(sortsize_core::InventoryPreview {
                        entries: preview.entries,
                        dates: preview.dates,
                        owners: preview.owners,
                        entries_omitted: preview.entries_omitted,
                        direct_children_seen: preview.direct_children_seen,
                    });
                    let mut slot = mailbox.lock().unwrap();
                    slot.patch = Some(patch);
                    slot.directory = Some(worker::DirectoryProgress {
                        retained: preview.entries.len(),
                        seen: preview.direct_children_seen,
                        closed: true,
                        previous: None,
                    });
                    drop(slot);
                    controller.borrow_mut().poll(&window);
                }
                staged = true;
                token.cancel();
            }
        },
    )
    .unwrap();
    assert!(staged && window.get_busy());
    assert!(controller.borrow().directory.window_ready());
    assert!(window.get_selection_summary().contains("≥"));
    window.invoke_change_page(true, true); // second UI page in this window
    window.invoke_change_page(true, true); // next window before aggregation ends
    assert!(window.get_busy());
    assert_eq!(window.get_details().row_count(), 0);
    mailbox.lock().unwrap().completion = Some(Err(worker::Failure::Panicked));
    controller.borrow_mut().poll(&window); // abandoned mailbox cannot affect this request
    assert!(!window.get_status().contains("échoué"));
    window.invoke_go_parent();
    assert!(!window.get_busy());
    assert_eq!(
        controller
            .borrow()
            .tree
            .as_ref()
            .unwrap()
            .aggregate_fingerprint(),
        fingerprint
    );
    let fresh = TempDir::new().unwrap();
    fs::write(fresh.path().join("fresh"), [0; 17]).unwrap();
    window.invoke_load_folder_details();
    assert!(controller.borrow().directory.original.is_some());
    let abandoned = CancellationToken::default();
    let abandoned_mailbox = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox::default()));
    controller
        .borrow_mut()
        .directory
        .replace_task(worker::Task {
            cancellation: abandoned.clone(),
            mailbox: abandoned_mailbox.clone(),
        });
    // Model a source switch while the complementary worker is still active.
    // Initial scans are admitted independently of the complementary busy state.
    window.set_busy(true);
    let generation = controller.borrow().generation;
    window.set_scan_path(fresh.path().to_str().unwrap().into());
    window.invoke_start_scan();
    assert!(abandoned.is_cancelled());
    assert_eq!(controller.borrow().generation, generation + 1);
    abandoned_mailbox.lock().unwrap().completion = Some(Err(worker::Failure::Panicked));
    settle(&window, &controller);
    assert_eq!(window.get_details().row_count(), 1);
    assert_eq!(window.get_details().row_data(0).unwrap().name, "fresh");
    assert!(controller.borrow().directory.original.is_none());
    assert_eq!(controller.borrow().directory.cache_len(), 0);
}

#[test]
#[ignore = "native Winit desktop and explicit SORTSIZE_ONDEMAND_FIXTURE/OUTPUT required"]
fn native_million_entry_directory_listing_cache_cancel_and_source_change() {
    use super::progressive_gui_tests::snapshot;
    let fixture = std::path::PathBuf::from(std::env::var_os("SORTSIZE_ONDEMAND_FIXTURE").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("SORTSIZE_ONDEMAND_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let (window, controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window
        .window()
        .set_size(slint::LogicalSize::new(1200., 660.));
    window.set_worker_choice(
        if std::env::var("SORTSIZE_ONDEMAND_WORKERS").as_deref() == Ok("8") {
            3
        } else {
            0
        },
    );
    window.set_scan_path(fixture.to_str().unwrap().into());
    window
        .window()
        .set_position(slint::PhysicalPosition::new(0, 0));
    window.show().unwrap();
    window.invoke_start_scan();
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    let started = Instant::now();
    let mut last = Instant::now();
    let mut poll_max = 0f64;
    let mut heartbeat_max = 0f64;
    let mut initial_ms = 0u128;
    let mut listing_ms = 0u128;
    let mut next_ready_ms = 0u128;
    let mut live_ready_ms = 0u128;
    let mut live_next_ms = 0f64;
    let mut cancel_ms = 0f64;
    let mut cache_ms = 0f64;
    let mut phase = 0;
    let mut fingerprint = 0;
    let mut summary = slint::SharedString::default();
    let mut paths = Vec::new();
    let mut phase_start = Instant::now();
    let mut intermediate = false;
    let fresh = TempDir::new().unwrap();
    fs::write(fresh.path().join("fresh"), [0; 17]).unwrap();
    timer.start(TimerMode::Repeated,Duration::from_millis(20),move||{
        assert!(started.elapsed()<Duration::from_secs(1800),"native on-demand timeout phase {phase}");
        heartbeat_max=heartbeat_max.max(last.elapsed().as_secs_f64()*1000.);
        let window=weak.upgrade().unwrap();let tick=Instant::now();state.borrow_mut().poll(&window);
        poll_max=poll_max.max(tick.elapsed().as_secs_f64()*1000.);
        match phase {
            0 if !window.get_busy()=>{
                let state=state.borrow();let tree=state.tree.as_ref().unwrap();assert_eq!(tree.node_count(),50_000);assert!(tree.needs_details());
                let (bytes, counts, incomplete) = tree.selected_totals().unwrap();
                assert_eq!(bytes,Some(2_000_000));assert_eq!(counts.direct_files,2_000_000);assert_eq!(counts.recursive_files,2_000_000);assert!(!incomplete);
                fingerprint=tree.aggregate_fingerprint();summary=window.get_summary();
                if let Some(worker::DisplayLabels::Summary(data))=&state.scan_labels { initial_ms=data.report.duration_ms; }
                eprintln!("stage=initial scan_ms={initial_ms}");drop(state);
                snapshot(&window,&output,"initial-capped");window.invoke_load_folder_details();assert!(window.get_busy());phase=1;
            }
            1=>{
                if window.get_busy() && window.get_details().row_count()>0 && !intermediate {
                    assert!(window.get_selection_summary().contains("≥"));assert!(window.get_summary().ends_with(summary.as_str()));
                    snapshot(&window,&output,"additional-progress");intermediate=true;
                }
                if !window.get_busy(){
                    listing_ms=state.borrow().directory.last_scan_ms;eprintln!("stage=additional scan_ms={listing_ms}");
                    assert!(intermediate,"no progressive additional listing observed");assert!(window.get_summary().ends_with(summary.as_str()));
                    assert_eq!(state.borrow().directory.original.as_ref().unwrap().aggregate_fingerprint(),fingerprint);
                    assert!(state.borrow().tree.as_ref().unwrap().node_count()<=4_001);
                    let (bytes, counts, incomplete) = state.borrow().tree.as_ref().unwrap().selected_totals().unwrap();
                    assert_eq!(bytes,Some(2_000_000));assert_eq!(counts.direct_files,2_000_000);assert_eq!(counts.recursive_files,2_000_000);assert!(!incomplete);
                    snapshot(&window,&output,"additional-final");
                    window.invoke_change_page(true,true);paths=window.get_details().iter().map(|r|r.path).collect::<Vec<_>>();window.invoke_change_page(true,true);assert!(window.get_busy());phase_start=Instant::now();phase=2;
                }
            }
            2 if window.get_busy() && state.borrow().directory.window_ready()=>{
                next_ready_ms=phase_start.elapsed().as_millis();eprintln!("stage=next-ready elapsed_ms={next_ready_ms}");
                assert!(window.get_selection_summary().contains("≥"));
                assert_ne!(window.get_details().iter().map(|r|r.path).collect::<Vec<_>>(),paths);
                assert!(window.get_summary().ends_with(summary.as_str()));snapshot(&window,&output,"additional-next");
                let cache_started=Instant::now();window.invoke_change_page(true,false);cache_ms=cache_started.elapsed().as_secs_f64()*1000.;assert!(!window.get_busy(),"previous window must hit cache");
                assert_eq!(window.get_details().iter().map(|r|r.path).collect::<Vec<_>>(),paths);
                assert!(state.borrow().directory.cache_len()<=4);snapshot(&window,&output,"additional-cache");
                window.invoke_load_folder_details();phase_start=Instant::now();phase=3;
            }
            3 if window.get_busy() && state.borrow().directory.window_ready()=>{
                live_ready_ms=phase_start.elapsed().as_millis();
                window.invoke_change_page(true,true);
                snapshot(&window,&output,"additional-live-page");
                let advance=Instant::now();window.invoke_change_page(true,true);
                live_next_ms=advance.elapsed().as_secs_f64()*1000.;
                assert!(window.get_busy());assert_eq!(window.get_details().row_count(),0);
                eprintln!("stage=live-next ready_ms={live_ready_ms} callback_ms={live_next_ms:.3}");
                phase_start=Instant::now();phase=6;
            }
            6 if window.get_busy()&&phase_start.elapsed()>=Duration::from_millis(100)=>{
                phase_start=Instant::now();window.invoke_cancel_scan();phase=4;
            }
            4 if !window.get_busy()=>{
                cancel_ms=phase_start.elapsed().as_secs_f64()*1000.;
                assert!(window.get_summary().ends_with(summary.as_str()));
                assert_eq!(state.borrow().directory.original.as_ref().unwrap().aggregate_fingerprint(),fingerprint);
                snapshot(&window,&output,"additional-cancelled");
                window.set_scan_path(fresh.path().to_str().unwrap().into());window.invoke_start_scan();phase=5;
            }
            5 if !window.get_busy()=>{
                assert_eq!(window.get_details().row_count(),1);assert_eq!(window.get_details().row_data(0).unwrap().name,"fresh");
                assert!(state.borrow().directory.original.is_none());assert_eq!(state.borrow().directory.cache_len(),0);
                fs::write(output.join("metrics.json"),format!("{{\"progressive\":{intermediate},\"cache_hit_ms\":{cache_ms:.3},\"cancel_ms_before_capture\":{cancel_ms:.3},\"initial_scan_ms\":{initial_ms},\"additional_scan_ms\":{listing_ms},\"next_ready_ms\":{next_ready_ms},\"live_ready_ms\":{live_ready_ms},\"live_next_callback_ms\":{live_next_ms:.3},\"poll_max_ms\":{poll_max:.3},\"heartbeat_max_ms\":{heartbeat_max:.3},\"duration_ms\":{}}}",started.elapsed().as_millis())).unwrap();
                window.hide().unwrap();slint::quit_event_loop().unwrap();
            }
            _=>{}
        }
        last=Instant::now();
    });
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
}
