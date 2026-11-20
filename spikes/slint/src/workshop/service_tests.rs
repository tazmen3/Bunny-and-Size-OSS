use super::*;
use sortsize_core::analysis::SearchQuery;

#[test]
fn metadata_intent_never_resumes_directory_listing_observations() {
    let mut request = Start {
        root: PathBuf::from("source"),
        explicit_root: true,
        preflight: None,
        observation: ScanObservation::DirectoryListing,
        options: Default::default(),
    };
    let strict = sortsize_store::scheduler::observation_policy(ScanObservation::Strict);
    let listing = sortsize_store::scheduler::observation_policy(ScanObservation::DirectoryListing);
    assert!(request.accepts_resume(strict));
    assert!(request.accepts_resume(listing));
    request.observation = ScanObservation::Strict;
    assert!(request.accepts_resume(strict));
    assert!(!request.accepts_resume(listing));
}

#[test]
fn certified_handoff_stays_preparing_until_the_reader_publishes_a_consultable_page() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("known"), [3; 17]).unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.memory = MemoryMode::Entries50k;
        lane.reader_gate = Some(gate.clone());
    }
    start(&service, 1, &source);
    until(&service, || {
        let lane = service.lane.lock().unwrap();
        lane.reader_blocked
            && lane.reader_target.is_some()
            && !lane.writer_running
            && lane
                .certified_readiness
                .as_ref()
                .is_some_and(|ready| ready.cleanup)
    });
    let preparing = matches!(
        service.lane.lock().unwrap().phase,
        Some((1, Phase::Finalizing, _))
    );
    let premature_page = service.output.lock().unwrap().as_ref().is_some_and(
        |output| matches!(output, Output::Page(page) if !page.busy && page.measure_ready),
    );
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert!(
        preparing,
        "certification must not announce an unprepared view"
    );
    assert!(!premature_page);
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Ready, _))
        )
    });
    let output = service.output.lock().unwrap();
    let Some(Output::Page(page)) = output.as_ref() else {
        panic!("Ready requires a published page");
    };
    assert!(!page.busy && page.measure_ready && !page.frame.provisional);
    assert_eq!(page.root_measure.unwrap().logical, Some(17));
    assert!(page.tiles.iter().any(|tile| tile.name == "known"));
}

#[test]
fn certified_page_stays_preparing_until_writer_cleanup_finishes() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("known"), [3; 17]).unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.memory = MemoryMode::Entries50k;
        lane.cleanup_gate = Some(gate.clone());
    }
    start(&service, 1, &source);
    until(&service, || {
        let lane = service.lane.lock().unwrap();
        lane.cleanup_blocked
            && lane
                .certified_readiness
                .as_ref()
                .is_some_and(|ready| ready.page)
    });
    let preparing = matches!(
        service.lane.lock().unwrap().phase,
        Some((1, Phase::Finalizing, _))
    );
    let consultable_but_busy = service
        .output
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|output| {
            matches!(output, Output::Page(page) if page.busy && page.measure_ready
            && !page.frame.provisional && page.root_measure.unwrap().logical == Some(17))
        });
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert!(
        preparing,
        "a prepared page must not hide remaining writer cleanup"
    );
    assert!(consultable_but_busy);
    until(&service, || {
        let lane = service.lane.lock().unwrap();
        matches!(lane.phase, Some((1, Phase::Ready, _)))
            && lane.writer_epoch.is_none()
            && service.output.lock().unwrap().as_ref().is_some_and(
                |output| matches!(output, Output::Page(page) if !page.busy && page.measure_ready),
            )
    });
}

#[test]
fn certified_completion_is_fenced_by_context_source_and_cancellation() {
    let c = fresh_context(
        9,
        Path::new("source"),
        std::time::SystemTime::now(),
        ScanObservation::Strict,
    )
    .unwrap();
    for cancel in [false, true] {
        let mut lane = Lane {
            source_epoch: 9,
            phase: Some((9, Phase::Finalizing, c.generation)),
            certified_readiness: Some(CertifiedReadiness {
                epoch: 9,
                context: c,
                page: false,
                cleanup: false,
            }),
            ..Default::default()
        };
        lane.certified_step_finished(8, c, true);
        lane.certified_step_finished(
            9,
            AcquisitionContext {
                scan: ScanId(88),
                ..c
            },
            false,
        );
        let ready = lane.certified_readiness.as_ref().unwrap();
        assert!(!ready.page && !ready.cleanup);
        lane.certified_step_finished(9, c, true);
        if cancel {
            lane.cancel_requested_epoch = Some(9);
            lane.publication_cancel.cancel();
        }
        lane.certified_step_finished(9, c, false);
        assert_eq!(matches!(lane.phase, Some((9, Phase::Ready, _))), !cancel);
    }
}

#[test]
fn progressive_mosaic_real_corpus_folds_before_completion_and_navigates() {
    // This real corpus tests visible progress during acquisition, independently
    // of how many quanta a snapshot needs. Store tests force fine-grained
    // mid-file continuations with their private operation budget.
    const FILES_PER_FOLDER: u64 = 384;
    const FINAL_BYTES: u64 = 17 + FILES_PER_FOLDER * (3 + 11);
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("anchor"), [7; 17]).unwrap();
    for (name, bytes) in [("small", 3), ("large", 11)] {
        let folder = source.join(name);
        std::fs::create_dir(&folder).unwrap();
        for index in 0..FILES_PER_FOLDER {
            std::fs::write(folder.join(format!("file-{index:03}")), vec![1; bytes]).unwrap();
        }
    }
    let mut active = Active::open(
        1,
        Start {
            root: source.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        },
        None,
        Default::default(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while active
        .session
        .store()
        .entry_count(active.c.generation)
        .unwrap()
        < 400
    {
        active.session.tick().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(active.session.status().state, State::Running);
    let first_ready = active
        .advance_live_preview(Duration::from_millis(100))
        .unwrap();
    let preview = active.live_preview.as_ref().unwrap();
    assert_eq!(preview.folder_ready(EntryId(0)), first_ready);
    let first_measure = preview.measure(EntryId(0)).unwrap().unwrap();
    let snapshot_entries = preview.store().entry_count(active.c.generation).unwrap();
    let first_bytes = first_measure.logical.unwrap();
    assert!(first_bytes > 0 && first_bytes < FINAL_BYTES);
    assert!(first_measure.files > 0 && first_measure.files < 1 + 2 * FILES_PER_FOLDER);
    if first_ready {
        assert_eq!(first_measure.files, snapshot_entries - 3);
    } else {
        assert!(!preview.complete());
        assert!(first_measure.files < snapshot_entries - 3);
    }
    let first = active.prepare(Default::default(), None).unwrap();
    assert!(first.busy && first.frame.provisional && !first.measure_ready);
    assert!(first.tiles.iter().any(|tile| tile.directory));
    assert!(first.folders.iter().any(|row| row.has_weight));
    assert!(first.folders.iter().all(|row| row.weight_provisional));
    assert!(first.details.iter().all(|row| row.weight_provisional));
    for tile in &first.tiles {
        let row = first
            .details
            .iter()
            .find(|row| row.node_id == tile.node_id)
            .unwrap();
        assert_eq!(row.color_index, tile.color_index);
        assert!(row.has_weight && row.proportion.starts_with("≈ "));
        assert!(tile.partial);
        assert_eq!(
            tile.path,
            crate::tree::native_label(&source.join(tile.name.as_str()))
        );
    }
    assert!(
        first
            .folders
            .iter()
            .filter(|row| row.has_weight)
            .all(|row| row.proportion.starts_with("≈ "))
    );
    let first_folder_weights: Vec<_> = first
        .folders
        .iter()
        .map(|row| (row.name.clone(), row.size.clone(), row.fraction))
        .collect();
    assert!(
        first
            .tiles
            .iter()
            .filter(|tile| tile.directory)
            .all(|tile| tile.size.starts_with('≥'))
    );
    let anchor = first
        .tiles
        .iter()
        .find(|tile| tile.name == "anchor")
        .unwrap();
    let first_anchor_fraction = anchor.band_w;
    let first_revision = active.live_preview.as_ref().unwrap().revision();
    println!(
        "progressive first: writer={:?} revision={} scope_ready={} tiles={} summary={} anchor_fraction={}",
        active.session.status().state,
        first_revision.0,
        first_ready,
        first.tiles.len(),
        first.summary,
        first_anchor_fraction
    );
    assert!((first.tiles.iter().map(|tile| tile.band_w).sum::<f32>() - 1.).abs() < 0.00001);
    drop(first);

    if first_ready {
        // A fast fold may finish this dated snapshot in one call. Acknowledge
        // its publication, then acquire more real files before refreshing it.
        active.revision = Some(first_revision);
        while active
            .session
            .store()
            .entry_count(active.c.generation)
            .unwrap()
            <= snapshot_entries
        {
            active.session.tick().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    while !active.advance_live_preview(Duration::ZERO).unwrap() {
        // Explicitly slow only this test's small real acquisition; production
        // has no delay knob and retains its independent scheduler.
        active.session.tick().unwrap();
        assert!(Instant::now() < deadline);
    }
    let next = active.prepare(Default::default(), None).unwrap();
    let preview = active.live_preview.as_ref().unwrap();
    if first_ready {
        assert!(preview.revision().0 > first_revision.0);
    } else {
        assert_eq!(preview.revision(), first_revision);
    }
    assert!(preview.folder_ready(EntryId(0)));
    let next_measure = preview.measure(EntryId(0)).unwrap().unwrap();
    assert!(next_measure.logical.unwrap() > first_bytes);
    assert_eq!(
        next_measure.files,
        preview.store().entry_count(active.c.generation).unwrap() - 3
    );
    let anchor = next
        .tiles
        .iter()
        .find(|tile| tile.name == "anchor")
        .unwrap();
    assert!(anchor.band_w < first_anchor_fraction);
    assert!(!next.measure_ready && next.frame.provisional);
    assert!(next.folders.iter().all(|row| row.weight_provisional));
    assert!(next.folders.iter().any(|row| row.has_weight));
    assert!(next.folders.iter().any(|row| {
        first_folder_weights.iter().any(|(name, size, fraction)| {
            row.name == *name && (row.size != *size || row.fraction != *fraction)
        })
    }));
    for tile in &next.tiles {
        assert!((tile.w * tile.h - tile.band_w).abs() < 0.00001);
    }
    println!(
        "progressive next: writer={:?} revision={} snapshot_complete=true tiles={} summary={} anchor_fraction={}",
        active.session.status().state,
        preview.revision().0,
        next.tiles.len(),
        next.summary,
        anchor.band_w
    );
    let folder = next
        .frame
        .rows
        .iter()
        .find(|row| row.kind == sortsize_store::inventory::Kind::Directory)
        .unwrap()
        .key;
    drop(next);
    active.navigate(2, Navigate::Open(folder)).unwrap();
    active
        .advance_live_preview(Duration::from_millis(100))
        .unwrap();
    let navigated = active.prepare(Default::default(), None).unwrap();
    assert!(navigated.parent && navigated.frame.provisional);
    assert!(!navigated.tiles.is_empty());
    assert!(navigated.tiles.iter().all(|tile| !tile.directory));
    assert_eq!(active.parent, folder);
    println!(
        "progressive navigation: tiles={} path={}",
        navigated.tiles.len(),
        navigated.path
    );
    drop(navigated);
    active.close_live_preview().unwrap();
    assert!(
        active
            .repository
            .as_ref()
            .unwrap()
            .plan
            .as_ref()
            .unwrap()
            .remaining()
            .reader
            > 0
    );
    active.navigate(3, Navigate::Parent).unwrap();
    while active.session.status().state != State::Sealed {
        let status = active.session.tick().unwrap();
        if matches!(status.state, State::Idle | State::Cancelled) && status.in_flight == 0 {
            active.session.finalize_step().unwrap();
        }
        assert!(Instant::now() < deadline);
    }
    while !active
        .session
        .store()
        .folder_fold_step_at(
            active.c.generation,
            &mut active.fold,
            active._temporary.path(),
        )
        .unwrap()
    {}
    let final_page = active.prepare(Default::default(), None).unwrap();
    assert!(final_page.measure_ready && !final_page.busy && !final_page.frame.provisional);
    assert_eq!(final_page.tiles.len(), 3);
    for tile in &final_page.tiles {
        let row = final_page
            .details
            .iter()
            .find(|row| row.node_id == tile.node_id)
            .unwrap();
        assert_eq!(row.color_index, tile.color_index);
        assert!(row.has_weight && !row.weight_provisional);
        assert!(!tile.partial);
        assert!((row.fraction - tile.band_w).abs() < f32::EPSILON);
    }
    assert!(
        final_page
            .tiles
            .iter()
            .all(|tile| !tile.size.starts_with('≥'))
    );
    let whole = active
        .session
        .store()
        .folder_measure(active.c.generation, EntryId(0), &active.fold)
        .unwrap()
        .unwrap();
    assert_eq!(whole.logical, Some(FINAL_BYTES));
    assert_eq!(whole.files, 1 + 2 * FILES_PER_FOLDER);
    for row in final_page.folders.iter().filter(|row| row.depth > 0) {
        assert!(row.has_weight && !row.weight_provisional);
        assert!(!row.proportion.starts_with("≈ "));
        let bytes = if row.name == "small" {
            FILES_PER_FOLDER * 3
        } else {
            FILES_PER_FOLDER * 11
        };
        assert!((row.fraction - bytes as f32 / whole.logical.unwrap() as f32).abs() < 0.00001);
    }
    println!(
        "progressive final: tiles={} logical={} coverage={:?}",
        final_page.tiles.len(),
        whole.logical.unwrap(),
        final_page.frame.coverage
    );
    assert_eq!(std::fs::read(source.join("anchor")).unwrap(), [7; 17]);
}

fn until(service: &Service, test: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(Output::Error(_, message, _)) = service.output.lock().unwrap().as_ref() {
            panic!("service failed: {message}");
        }
        if test() {
            return;
        }
        if Instant::now() >= deadline {
            let lane = service.lane.lock().unwrap();
            let output = service.output.lock().unwrap();
            let page = match output.as_ref() {
                Some(Output::Page(p)) => format!(
                    "epoch={} busy={} provisional={} revision={} path={} quanta={} tiles={:?}",
                    p.epoch,
                    p.busy,
                    p.frame.provisional,
                    p.frame.stamp.revision.0,
                    p.path,
                    p.quanta,
                    p.tiles
                        .iter()
                        .map(|t| (t.name.to_string(), t.size.to_string()))
                        .collect::<Vec<_>>()
                ),
                _ => "no page".into(),
            };
            let (running, quanta) = (lane.writer_running, lane.writer_quanta);
            drop(output);
            drop(lane);
            panic!(
                "service rendezvous timed out: writer_running={running} writer_quanta={quanta} {page}"
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn start(service: &Service, epoch: u64, root: &Path) {
    let mut lane = service.lane.lock().unwrap();
    // The recovery and certification fixtures exercise the durable backend.
    lane.force_strict = true;
    lane.publication_cancel.cancel();
    lane.publication_cancel = Default::default();
    lane.source_epoch = epoch;
    lane.view_epoch = epoch;
    lane.search_current = None;
    // Match Host::start: the new intent is running before either owner can
    // republish a retained G0. A page from G0 must not look like G1 completed.
    lane.writer_running = true;
    lane.cancel = false;
    lane.cancel_requested_epoch = None;
    lane.retry = false;
    lane.phase = None;
    lane.start = Some((
        epoch,
        Start {
            root: root.into(),
            explicit_root: false,
            observation: ScanObservation::Strict,
            preflight: None,
            options: Default::default(),
        },
    ));
}

#[test]
fn progressive_mosaic_worker_publishes_new_snapshots_and_navigation_while_running() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("anchor"), [7; 17]).unwrap();
    for (name, bytes) in [("small", 3), ("large", 11)] {
        let folder = source.join(name);
        std::fs::create_dir(&folder).unwrap();
        for index in 0..384 {
            std::fs::write(folder.join(format!("file-{index:03}")), vec![1; bytes]).unwrap();
        }
    }
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.memory = MemoryMode::Entries50k;
        // This delay exists only in cfg(test), in the acquisition service loop.
        lane.tick_delay = Duration::from_millis(100);
    }
    start(&service, 1, &source);
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|output|
        matches!(output, Output::Page(page) if page.busy && page.frame.provisional && !page.tiles.is_empty()))
    });
    let Output::Page(first) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    let first_revision = first.frame.stamp.revision;
    let first_tiles = first.tiles.clone();
    println!(
        "progressive worker first: busy={} revision={} quanta={} tiles={} summary={}",
        first.busy,
        first_revision.0,
        first.quanta,
        first.tiles.len(),
        first.summary
    );
    assert!(!first.measure_ready);
    drop(first);
    until(&service, || {
        service
            .output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|output| {
                matches!(output, Output::Page(page) if page.busy && page.frame.provisional
            && page.frame.stamp.revision != first_revision && page.tiles != first_tiles
            && page.tiles.iter().any(|tile| tile.directory))
            })
    });
    let Output::Page(next) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    assert!(service.lane.lock().unwrap().writer_running);
    println!(
        "progressive worker next: busy={} revision={} quanta={} tiles={} summary={}",
        next.busy,
        next.frame.stamp.revision.0,
        next.quanta,
        next.tiles.len(),
        next.summary
    );
    assert!((next.tiles.iter().map(|tile| tile.band_w).sum::<f32>() - 1.).abs() < 0.00001);
    let folder = next
        .frame
        .rows
        .iter()
        .find(|row| row.kind == sortsize_store::inventory::Kind::Directory)
        .unwrap()
        .key;
    drop(next);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 2;
        lane.navigation = Some((2, Navigate::Open(folder)));
    }
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|output|
        matches!(output, Output::Page(page) if page.epoch == 2 && page.parent && page.busy && !page.tiles.is_empty()))
    });
    let Output::Page(navigated) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    assert!(navigated.frame.provisional && navigated.tiles.iter().all(|tile| !tile.directory));
    println!(
        "progressive worker navigation: busy={} revision={} quanta={} tiles={} path={}",
        navigated.busy,
        navigated.frame.stamp.revision.0,
        navigated.quanta,
        navigated.tiles.len(),
        navigated.path
    );
    drop(navigated);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 3;
        lane.navigation = Some((3, Navigate::Parent));
    }
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|output|
        matches!(output, Output::Page(page) if page.epoch == 3 && !page.busy && page.measure_ready))
    });
    let Output::Page(final_page) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    assert!(!final_page.frame.provisional && !final_page.parent);
    assert_eq!(final_page.frame.coverage, ScopeCoverage::Complete);
    assert_eq!(final_page.tiles.len(), 3);
    let anchor = final_page
        .tiles
        .iter()
        .find(|tile| tile.name == "anchor")
        .unwrap();
    assert!((anchor.band_w - 17. / 5393.).abs() < 0.00001);
    assert!(
        final_page
            .tiles
            .iter()
            .all(|tile| !tile.size.starts_with('≥'))
    );
    println!(
        "progressive worker final: busy={} revision={} tiles={} coverage={:?} logical_oracle=5393",
        final_page.busy,
        final_page.frame.stamp.revision.0,
        final_page.tiles.len(),
        final_page.frame.coverage
    );
    assert_eq!(std::fs::read(source.join("anchor")).unwrap(), [7; 17]);
}
fn query(service: &Service, id: u64, name: &str) -> sortsize_core::CancellationToken {
    let cancel = sortsize_core::CancellationToken::default();
    let mut lane = service.lane.lock().unwrap();
    lane.search_current = Some(id);
    lane.search = Some(super::super::search::Request {
        id,
        query: SearchQuery {
            name: Some(name.into()),
            ..Default::default()
        },
        cancel: cancel.clone(),
    });
    cancel
}

#[test]
fn certified_g0_remains_exact_while_real_g1_ticks_and_query_cancellation_is_independent() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for i in 0..512 {
        std::fs::write(source.join(format!("old-{i}")), [1; 3]).unwrap();
    }
    let service = Service::start().unwrap();
    service.lane.lock().unwrap().memory = MemoryMode::Entries50k;
    start(&service, 1, &source);
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|o|
        matches!(o, Output::Page(p) if p.measure_ready && !p.busy && p.frame.stamp.context.generation == GenerationId(1)))
    });
    let initial = service.output.lock().unwrap().take().unwrap();
    let Output::Page(initial) = initial else {
        unreachable!()
    };
    let identity = initial.frame.stamp.context;
    assert!(!initial.frame.provisional);
    drop(initial);
    let workspace = service
        .lane
        .lock()
        .unwrap()
        .reader_workspace
        .clone()
        .unwrap();
    let g0_path = workspace
        .repository()
        .lock()
        .unwrap()
        .generation_path(GenerationId(1))
        .join("inventory.db");
    let original = std::fs::read(&g0_path).unwrap();
    for i in 0..512 {
        std::fs::write(source.join(format!("new-g2-{i}")), [1; 5]).unwrap();
    }
    service.lane.lock().unwrap().hold_generation = Some(GenerationId(2));
    start(&service, 2, &source);
    until(&service, || {
        let l = service.lane.lock().unwrap();
        l.writer_generation == Some(GenerationId(2)) && l.writer_entries > 1
    });
    let cancel = query(&service, 10, "new-g2");
    until(&service, || {
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 10 && !p.busy)
    });
    let result = service.search_output.lock().unwrap().take().unwrap();
    assert!(result.complete);
    assert_eq!(result.matches, 0);
    assert!(
        result
            .scope
            .contains(&format!("{} 1", tr!("Frozen inventory, generation")))
    );
    drop(result);
    assert!(service.lane.lock().unwrap().writer_running);
    assert_eq!(std::fs::read(&g0_path).unwrap(), original);
    cancel.cancel();
    service.lane.lock().unwrap().search_current = None;
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .query_cancel
            .as_ref()
            .is_some_and(|c| c.is_cancelled())
    });
    // Keep a second G0 result open while the acquisition actually completes.
    query(&service, 11, "old-");
    until(&service, || {
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 11 && !p.busy)
    });
    assert_eq!(
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .matches,
        512
    );
    let publication = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    {
        let mut lane = service.lane.lock().unwrap();
        lane.publication_gate = Some(publication.clone());
        lane.hold_generation = None;
    }
    until(&service, || {
        service.lane.lock().unwrap().publication_blocked
    });
    service.lane.lock().unwrap().publication_cancel.cancel();
    {
        let (lock, wake) = &*publication;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(_, phase, _)| matches!(phase, Phase::Cancelled))
    });
    assert!(
        workspace
            .repository()
            .lock()
            .unwrap()
            .published(GenerationId(2))
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(&g0_path).unwrap(), original);
    assert_eq!(
        service.search_output.lock().unwrap().as_ref().unwrap().id,
        11
    );
    {
        let mut lane = service.lane.lock().unwrap();
        lane.publication_cancel = Default::default();
        lane.retry = true;
    }
    until(&service, || {
        workspace
            .repository()
            .lock()
            .unwrap()
            .published(GenerationId(2))
            .unwrap()
            .is_some()
    });
    assert_eq!(std::fs::read(&g0_path).unwrap(), original);
    assert_eq!(
        service.search_output.lock().unwrap().as_ref().unwrap().id,
        11
    );
    service.lane.lock().unwrap().search_current = None;
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|o|
        matches!(o, Output::Page(p) if !p.busy && p.measure_ready && p.frame.stamp.context.generation == GenerationId(2)))
    });
    let Output::Page(final_page) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    assert_eq!(final_page.frame.stamp.context.inventory, identity.inventory);
    assert_eq!(final_page.frame.stamp.context.source, identity.source);
    assert!(!final_page.frame.provisional);
    until(&service, || {
        service.lane.lock().unwrap().writer_epoch.is_none()
    });
    assert!(
        !workspace
            .repository()
            .lock()
            .unwrap()
            .candidate_path(GenerationId(2))
            .exists()
    );
    drop(final_page);
    drop(workspace);
    let root = g0_path.parent().unwrap().parent().unwrap().to_owned();
    drop(service);
    let deadline = Instant::now() + Duration::from_secs(5);
    while root.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        !root.exists(),
        "private repository must be forgotten after last capability"
    );
}

#[test]
fn ui_destruction_signals_without_waiting_for_a_blocked_filesystem_worker() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let service = Service::start().unwrap();
    {
        let mut l = service.lane.lock().unwrap();
        l.memory = MemoryMode::Entries50k;
        l.tick_gate = Some(gate.clone());
        l.reader_gate = Some(gate.clone());
    }
    start(&service, 1, &source);
    until(&service, || {
        let l = service.lane.lock().unwrap();
        l.tick_blocked && l.reader_blocked
    });
    let pending_path = temporary.path().join("pending-native");
    let c = AcquisitionContext {
        inventory: InventoryId(133),
        source: SourceId(144),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut store =
        Store::create(&pending_path, c.inventory, c.source, Durability::Retained).unwrap();
    store.create_generation(c).unwrap();
    store.record_acquisition(c, &source, 1).unwrap();
    let pending = Active::cached(1, store, c.generation).unwrap();
    service.lane.lock().unwrap().reader_target = Some(Some(Box::new(pending)));
    assert!(Store::reopen(&pending_path, Durability::Retained).is_err());
    let lane = service.lane.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let destructor = std::thread::spawn(move || {
        drop(service);
        sent.send(()).unwrap();
    });
    let detached = received.recv_timeout(Duration::from_secs(1)).is_ok();
    assert!(
        Store::reopen(&pending_path, Durability::Retained).is_err(),
        "pending native owner was destroyed on the UI thread"
    );
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    destructor.join().unwrap();
    assert!(detached, "UI destructor waited for the provider");
    assert!(lane.lock().unwrap().stop);
    let deadline = Instant::now() + Duration::from_secs(5);
    while lane.lock().unwrap().writer_epoch.is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    loop {
        if Store::reopen(&pending_path, Durability::Retained).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pending native owner was not released by a worker"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn retain_real_partial_candidate_then_resume_and_reopen_uses_the_repository() {
    use super::super::cache;
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let state = temporary.path().join("state");
    std::fs::create_dir(&source).unwrap();
    for i in 0..512 {
        std::fs::write(source.join(format!("file-{i}")), [1; 7]).unwrap();
    }
    let service = Service::start().unwrap();
    {
        let mut l = service.lane.lock().unwrap();
        l.memory = MemoryMode::Entries50k;
        l.cache_root = Some(state.clone());
        l.hold_generation = Some(GenerationId(1));
    }
    start(&service, 1, &source);
    until(&service, || service.lane.lock().unwrap().writer_entries > 1);
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 1,
        epoch: 1,
        expected: 1,
        action: cache::Action::Save,
        cancel: Default::default(),
    });
    until(&service, || {
        service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 1 && !p.catalogue.is_empty())
    });
    let partial = service
        .cache_output
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .catalogue[0]
        .clone();
    assert!(partial.measured.is_none());
    assert!(partial.directory.join("IDENTITY.json").exists());
    assert!(!partial.directory.join("inventory.db").exists());
    {
        let mut l = service.lane.lock().unwrap();
        l.source_epoch = 2;
        l.view_epoch = 2;
        l.publication_cancel.cancel();
        l.publication_cancel = Default::default();
        l.cache = Some(cache::Request {
            id: 2,
            epoch: 2,
            expected: 1,
            action: cache::Action::Resume(partial.clone()),
            cancel: Default::default(),
        });
    }
    until(&service, || {
        service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 2)
    });
    service.lane.lock().unwrap().hold_generation = None;
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|p| {
            matches!(p,
        Output::Page(p) if p.epoch == 2 && p.measure_ready && !p.busy)
        })
    });
    query(&service, 21, "file-");
    until(&service, || {
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 21 && !p.busy)
    });
    assert_eq!(
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .matches,
        512
    );
    assert!(
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .complete
    );
    service.lane.lock().unwrap().search_current = None;
    until(&service, || {
        service.lane.lock().unwrap().writer_epoch.is_none()
    });
    let lane = service.lane.clone();
    drop(service);
    let deadline = Instant::now() + Duration::from_secs(5);
    while lane.lock().unwrap().reader_active && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(lane);
    std::fs::rename(&source, temporary.path().join("offline")).unwrap();
    let rows = sortsize_store::inventory::catalogue::list(&state, None)
        .unwrap()
        .rows;
    let saved = rows.iter().find(|s| s.measured.is_some()).unwrap();
    let view = Active::open_saved(3, saved, None, &Default::default()).unwrap();
    assert!(matches!(view.session, Backend::Certified(_)));
    let scratch = tempfile::tempdir().unwrap();
    let q = view
        .certified_generation()
        .unwrap()
        .query(
            &SearchQuery::default(),
            scratch.path(),
            512 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    assert_eq!(q.counts.matches, 513);
    assert!(q.complete());
}

#[test]
fn replacing_the_native_root_at_the_same_path_does_not_relabel_the_old_generation() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("old-only"), [1; 3]).unwrap();
    let service = Service::start().unwrap();
    service.lane.lock().unwrap().memory = MemoryMode::Entries50k;
    start(&service, 1, &source);
    until(&service, || {
        service
            .output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|o| matches!(o, Output::Page(p) if !p.busy && p.measure_ready))
    });
    let Output::Page(old) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    let old_identity = old.frame.stamp.context;
    drop(old);
    std::fs::rename(&source, temporary.path().join("old-source")).unwrap();
    std::fs::create_dir(&source).unwrap();
    for i in 0..256 {
        std::fs::write(source.join(format!("new-only-{i}")), [2; 7]).unwrap();
    }
    service.lane.lock().unwrap().hold_generation = Some(GenerationId(1));
    start(&service, 2, &source);
    until(&service, || {
        let l = service.lane.lock().unwrap();
        l.writer_generation == Some(GenerationId(1)) && l.writer_entries > 1 && !l.reader_active
    });
    if let Some(Output::Page(p)) = service.output.lock().unwrap().as_ref()
        && p.epoch == 2
    {
        assert_ne!(p.frame.stamp.context.inventory, old_identity.inventory);
        assert_eq!(
            p.frame.stamp.context.source,
            sortsize_store::inventory::catalogue::source_hint(&source)
        );
        assert!(p.frame.provisional);
    }
    service.lane.lock().unwrap().hold_generation = None;
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(
            |o| matches!(o, Output::Page(p) if p.epoch == 2 && !p.busy && p.measure_ready),
        )
    });
    let Output::Page(new) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    assert_ne!(new.frame.stamp.context.inventory, old_identity.inventory);
    assert_eq!(
        new.frame.stamp.context.source,
        sortsize_store::inventory::catalogue::source_hint(&source)
    );
    assert_eq!(new.frame.stamp.context.generation, GenerationId(1));
    assert!(!new.frame.provisional);
    assert!(new.frame.rows.iter().all(|r| {
        r.path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("new-only-"))
    }));
}

#[test]
fn legacy_partial_resume_extracts_a_candidate_without_writing_the_original_group() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for i in 0..256 {
        std::fs::write(source.join(format!("file-{i}")), [1; 7]).unwrap();
    }
    let legacy = temporary.path().join("legacy");
    let c = AcquisitionContext {
        inventory: InventoryId(818),
        source: SourceId(919),
        generation: GenerationId(7),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let store = Store::create(&legacy, c.inventory, c.source, Durability::Retained).unwrap();
    let mut session = Session::new_with_options(
        store,
        c,
        Guard::open(&source, Policy::default()).unwrap(),
        Default::default(),
    )
    .unwrap();
    session
        .store_mut()
        .record_acquisition(c, &source, 1)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while session.store().entry_count(c.generation).unwrap() <= 1 {
        session.tick().unwrap();
        assert!(Instant::now() < deadline);
    }
    drop(session);
    let mut store = Store::reopen(&legacy, Durability::Retained).unwrap();
    store.set_retention(true, 30).unwrap();
    let saved = store.saved(c.generation).unwrap();
    drop(store);
    let original = std::fs::read(legacy.join("inventory.db")).unwrap();
    let mut active = Active::resume_saved(
        2,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
        &saved,
        None,
        &Default::default(),
        Default::default(),
    )
    .unwrap();
    assert_ne!(active.session.store().directory(), legacy);
    assert_eq!(active.c.generation, c.generation);
    assert_eq!(active.c.inventory, c.inventory);
    assert_eq!(
        std::fs::read(legacy.join("inventory.db")).unwrap(),
        original
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while active.session.status().state != State::Sealed {
        let status = active.session.tick().unwrap();
        if matches!(status.state, State::Idle | State::Cancelled) && status.in_flight == 0 {
            active.session.finalize_step().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "legacy extraction resume did not finalize"
        );
    }
    active
        .session
        .store_mut()
        .unwrap()
        .record_measurement(active.c.generation, &source, 1, 2)
        .unwrap();
    active.sealed = Some(std::time::SystemTime::now());
    let lane = Mutex::new(Lane::default());
    let view = active.publish(&lane).unwrap().unwrap();
    assert!(matches!(view.session, Backend::Certified(_)));
    assert_eq!(view.c.inventory, c.inventory);
    assert_eq!(view.c.generation, c.generation);
    assert_eq!(view.session.store().entry_count(c.generation).unwrap(), 257);
    assert_eq!(
        std::fs::read(legacy.join("inventory.db")).unwrap(),
        original
    );
    drop(view);
    drop(active);
}

#[test]
fn retiring_an_unretained_workspace_preserves_its_own_native_authority() {
    let workspace = RepositoryWorkspace::temporary(
        "bunny-unretained-authority-",
        InventoryId(717),
        SourceId(818),
        PathBuf::from("offline"),
    )
    .unwrap();
    let original = workspace.path();
    assert_eq!(workspace.original.as_ref(), Some(&original));
    // This owner is deliberately still held. Retirement of an unretained
    // workspace must be a no-op, never an attempt to reacquire its native lock.
    assert!(Repository::open_existing(&original).is_err());
    workspace.retire_original();
    assert!(!*workspace.private_retired.lock().unwrap());
    assert_eq!(workspace.path(), original);
    assert!(original.exists());
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
    drop(workspace);
    assert!(!original.exists());
}

#[test]
fn retaining_g0_preserves_its_old_lease_then_retires_the_private_copy_after_release() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), [1; 3]).unwrap();
    let mut active = Active::open(
        1,
        Start {
            root: source.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        },
        None,
        Default::default(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while active.session.status().state != State::Sealed {
        let status = active.session.tick().unwrap();
        if matches!(status.state, State::Idle | State::Cancelled) && status.in_flight == 0 {
            active.session.finalize_step().unwrap();
        }
        assert!(Instant::now() < deadline);
    }
    let measured = std::time::SystemTime::now();
    active
        .session
        .store_mut()
        .unwrap()
        .record_measurement(
            active.c.generation,
            &source,
            super::super::cache::seconds(active.started),
            super::super::cache::seconds(measured),
        )
        .unwrap();
    active.sealed = Some(measured);
    let lane = Mutex::new(Lane {
        source_epoch: active.source_epoch,
        phase: Some((active.source_epoch, Phase::Certifying, active.c.generation)),
        ..Default::default()
    });
    let mut view = active.publish(&lane).unwrap().unwrap();
    // A new publication transfers its already certified destination reader.
    // No artificial Reopening phase, and no Ready before the page/cleanup gate.
    assert_eq!(view.c, active.c);
    assert!(matches!(
        lane.lock().unwrap().phase,
        Some((_, Phase::Certifying, _))
    ));
    assert_eq!(
        active
            .repository
            .as_ref()
            .unwrap()
            .plan
            .as_ref()
            .unwrap()
            .remaining()
            .reader,
        0
    );
    // Simulate recovery after commit but before the GUI accepted its handoff.
    // The existing selected generation must still take the full reopen path.
    active.repository.as_mut().unwrap().published = None;
    let recovered = active.publish(&lane).unwrap().unwrap();
    assert_eq!(recovered.c, view.c);
    assert_eq!(
        recovered
            .session
            .store()
            .entry_count(view.c.generation)
            .unwrap(),
        2
    );
    assert!(matches!(
        lane.lock().unwrap().phase,
        Some((_, Phase::Reopening, _))
    ));
    drop(recovered);
    let workspace = view.workspace.clone().unwrap();
    let original = workspace.path();
    assert!(
        view._temporary
            .path()
            .starts_with(workspace.directory.as_ref().unwrap().path()),
        "certified G0 scratch must remain in the acquisition's external private workspace"
    );
    let guard = Guard::open(&source, Policy::default()).unwrap();
    assert!(guard.store_outside(view._temporary.path()).unwrap());
    drop(active.take_repository());
    drop(active);
    let retained = view
        .retain(&temporary.path().join("state"), &Default::default())
        .unwrap();
    assert!(
        original.exists(),
        "G0 read lease must protect its private authority"
    );
    assert!(retained.exists());
    assert_eq!(
        view.session.store().entry_count(view.c.generation).unwrap(),
        2
    );
    drop(view);
    workspace.retire_original();
    assert!(
        !original.join("generation-0000000000000001").exists()
            && !original.join("candidate-0000000000000001").exists(),
        "released private copy must stop consuming disk quota: {:?}",
        super::super::cache::CLEANUP_ERROR.lock().unwrap().as_ref()
    );
    assert!(*workspace.private_retired.lock().unwrap());
    drop(workspace);
    assert!(!original.exists());
    assert!(
        retained.exists(),
        "retained family is independent of temporary owners"
    );
}

#[test]
fn p0_repository_lease_keeps_private_workspace_until_native_owner_is_released() {
    let workspace = RepositoryWorkspace::temporary(
        "bunny-p0-owner-lease-",
        InventoryId(9717),
        SourceId(9818),
        PathBuf::from("offline"),
    )
    .unwrap();
    let original = workspace.path();
    let owner = workspace.repository();
    drop(workspace);
    assert!(original.exists());
    assert_eq!(owner.lock().unwrap().directory(), original);
    assert!(Repository::open_existing(&original).is_err());
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
    drop(owner);
    assert!(!original.exists());
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
}

#[test]
fn p0_source_replacement_error_and_cancel_release_sessions_and_allow_restart() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source_a = temporary.path().join("source-a");
    let source_b = temporary.path().join("source-b");
    std::fs::create_dir(&source_a).unwrap();
    std::fs::create_dir(&source_b).unwrap();
    std::fs::write(source_a.join("first"), [1; 3]).unwrap();
    std::fs::write(source_b.join("second"), [2; 7]).unwrap();
    let mut service = Service::start().unwrap();
    service.lane.lock().unwrap().memory = MemoryMode::Entries50k;
    let complete = |epoch| {
        let ready = service
            .lane
            .lock()
            .unwrap()
            .phase
            .and_then(|(source, phase, generation)| {
                (source == epoch && matches!(phase, Phase::Ready)).then_some(generation)
            });
        service.output.lock().unwrap().as_ref().is_some_and(|o| {
            matches!(o, Output::Page(p) if p.epoch == epoch && !p.busy && p.measure_ready
                && ready == Some(p.frame.stamp.context.generation) && !p.frame.provisional)
        })
    };
    start(&service, 1, &source_a);
    until(&service, || complete(1));
    let original = service
        .lane
        .lock()
        .unwrap()
        .reader_workspace
        .as_ref()
        .unwrap()
        .path();
    start(&service, 2, &source_b);
    until(&service, || complete(2));
    until(&service, || !original.exists());
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
    let Output::Page(page) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    assert_eq!(
        page.path,
        super::super::label(&std::fs::canonicalize(&source_b).unwrap())
    );
    assert_eq!(page.frame.stamp.context.generation, GenerationId(1));
    drop(page);
    start(&service, 3, &temporary.path().join("missing"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if service
            .output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|o| matches!(o, Output::Error(3, _, false)))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "missing source error was not delivered"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(service.output.lock().unwrap().take());
    start(&service, 4, &source_b);
    until(&service, || complete(4));
    let Output::Page(page) = service.output.lock().unwrap().take().unwrap() else {
        unreachable!()
    };
    let next = GenerationId(page.frame.stamp.context.generation.0 + 1);
    drop(page);
    service.lane.lock().unwrap().hold_generation = Some(next);
    start(&service, 5, &source_b);
    until(&service, || {
        let lane = service.lane.lock().unwrap();
        lane.writer_generation == Some(next) && lane.writer_entries > 1
    });
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cancel = true;
        lane.publication_cancel.cancel();
        lane.hold_generation = None;
    }
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| epoch == 5 && matches!(phase, Phase::Cancelled))
    });
    start(&service, 6, &source_a);
    until(&service, || complete(6));
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
    let handles = std::mem::take(&mut service.workers);
    drop(service);
    // This test's final explicit shutdown waits for the two owners, while the
    // production Service destructor remains nonblocking for the UI.
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
}

#[test]
fn size_sort_live_uses_dated_sizes_and_keeps_sort_through_folder_navigation() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("largest-file"), vec![1; 9000]).unwrap();
    for (name, bytes) in [("folder-small", 1), ("folder-large", 10)] {
        let folder = source.join(name);
        std::fs::create_dir(&folder).unwrap();
        for index in 0..768 {
            std::fs::write(
                folder.join(format!("file-{index:03}")),
                vec![1; bytes + index % 3],
            )
            .unwrap();
        }
    }
    let mut active = Active::open(
        1,
        Start {
            root: source.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        },
        None,
        Default::default(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while active
        .session
        .store()
        .entry_count(active.c.generation)
        .unwrap()
        // Even if one folder is acquired first, both have more than 256
        // rows. Rebuilding either order must cross a real SQL window.
        < 1100
    {
        active.session.tick().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(active.session.status().state, State::Running);
    while !active
        .advance_live_preview(Duration::from_millis(100))
        .unwrap()
    {
        assert!(Instant::now() < deadline);
    }
    while !active.advance_order().unwrap() {
        assert!(Instant::now() < deadline);
    }
    let first = active.prepare(Default::default(), None).unwrap();
    assert_eq!(first.sort_index, 0);
    assert!(first.frame.provisional && first.busy);
    assert_eq!(first.details[0].name, "largest-file");
    assert!(
        first
            .frame
            .rows
            .windows(2)
            .all(|p| match (p[0].logical, p[1].logical) {
                (Some(a), Some(b)) => a >= b,
                (None, Some(_)) => false,
                _ => true,
            })
    );
    let folder = first
        .frame
        .rows
        .iter()
        .find(|r| r.kind == sortsize_store::inventory::Kind::Directory && r.logical.is_some())
        .unwrap()
        .key;
    let revision = first.frame.stamp.revision;
    drop(first);
    // The writer may advance, while this view keeps the exact dated order.
    for _ in 0..3 {
        active.session.tick().unwrap();
    }
    assert_eq!(
        active
            .prepare(Default::default(), None)
            .unwrap()
            .frame
            .stamp
            .revision,
        revision
    );
    active.navigate(2, Navigate::Open(folder)).unwrap();
    while !active
        .advance_live_preview(Duration::from_millis(100))
        .unwrap()
    {
        assert!(Instant::now() < deadline);
    }
    while !active.advance_order().unwrap() {
        assert!(Instant::now() < deadline);
    }
    let page = active.prepare(Default::default(), None).unwrap();
    assert_eq!(page.sort_index, 0);
    assert!(page.parent && page.frame.more);
    let last = page.frame.rows.last().unwrap().logical.unwrap();
    let first_entry = page.frame.rows[0].key.entry;
    let first_ids: std::collections::HashSet<_> =
        page.frame.rows.iter().map(|r| r.key.entry).collect();
    drop(page);
    active.navigate(3, Navigate::Page(true)).unwrap();
    let next = active.prepare(Default::default(), None).unwrap();
    assert!(!next.frame.rows.is_empty());
    assert!(
        next.frame
            .rows
            .iter()
            .all(|r| r.logical.unwrap() <= last && !first_ids.contains(&r.key.entry))
    );
    drop(next);
    // A page intention during a rebuilding dated order must survive its quanta.
    active.ordered = None;
    active.ordered_page = None;
    active.anchor = Some(first_entry);
    active.navigate(4, Navigate::Page(true)).unwrap();
    assert_eq!(active.pending_page, Some(true));
    let mut incomplete_order_steps = 0;
    while !active.advance_order().unwrap() {
        incomplete_order_steps += 1;
        assert_eq!(active.pending_page, Some(true));
        assert_eq!(active.anchor, Some(first_entry));
        assert!(Instant::now() < deadline);
    }
    assert!(incomplete_order_steps > 0);
    assert!(active.pending_page.is_none());
    assert!(
        active
            .ordered_page
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .all(|r| !first_ids.contains(&r.key.entry))
    );
    active.navigate(4, Navigate::Sort(1)).unwrap();
    while !active.advance_order().unwrap() {
        assert!(Instant::now() < deadline);
    }
    let ascending = active.prepare(Default::default(), None).unwrap();
    assert_eq!(ascending.sort_index, 1);
    assert!(
        ascending
            .frame
            .rows
            .windows(2)
            .all(|p| p[0].logical.unwrap() <= p[1].logical.unwrap())
    );
    drop(ascending);
    active.navigate(5, Navigate::Parent).unwrap();
    while !active
        .advance_live_preview(Duration::from_millis(100))
        .unwrap()
    {
        assert!(Instant::now() < deadline);
    }
    while !active.advance_order().unwrap() {
        assert!(Instant::now() < deadline);
    }
    let root = active.prepare(Default::default(), None).unwrap();
    assert!(!root.parent);
    assert_eq!(root.sort_index, 1);
    assert_eq!(root.details.last().unwrap().name, "largest-file");
    drop(root);
    active.close_live_preview().unwrap();
    active.session.control().cancel();
}

// A minimal wrapper leaves this scheduler locally owned. Cancelling through
// Service must affect its independent Control before any service quantum runs.
fn cancellation_service(active: &Active) -> Service {
    Service {
        trace: None,
        workers: Vec::new(),
        lane: Arc::new(Mutex::new(Lane {
            source_epoch: active.source_epoch,
            phase: Some((active.source_epoch, Phase::Preparing, active.c.generation)),
            writer_running: true,
            writer_owns_session: true,
            acquisition_control: Some((
                active.source_epoch,
                active.c.generation,
                active.session.control(),
            )),
            publication_cancel: active.publication_cancel.clone(),
            ..Default::default()
        })),
        output: Default::default(),
        search_output: Default::default(),
        resources: Default::default(),
        cache_output: Default::default(),
        preview_output: Default::default(),
    }
}

#[test]
fn cancellation_request_fences_acquisition_and_publication_before_service_quantum() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for index in 0..64 {
        std::fs::write(source.join(format!("file-{index:03}")), [7; 17]).unwrap();
    }
    let mut active = Active::open(
        3,
        Start {
            root: source,
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        },
        None,
        Default::default(),
    )
    .unwrap();
    let service = cancellation_service(&active);
    service.lane.lock().unwrap().retry = true;
    assert!(service.cancel_acquisition());
    {
        let lane = service.lane.lock().unwrap();
        assert!(lane.cancel);
        assert_eq!(lane.cancel_requested_epoch, Some(3));
        assert!(!lane.retry);
        assert!(lane.publication_cancel.is_cancelled());
        assert!(active.publication_cancel.is_cancelled());
    }
    // No service worker consumed lane.cancel. The scheduler already observes
    // the independently delivered request, so it submits no filesystem jobs.
    let status = active.session.tick().unwrap();
    assert_eq!(status.state, State::Cancelled);
    assert_eq!(status.in_flight, 0);
    assert_eq!(status.quanta, 0);
    active.cancelled = true;
    assert!(settle_cancelled_failure(&mut active, &service.lane));
    assert_eq!(active.session.store().latest_sealed().unwrap(), None);
    assert!(matches!(
        active.publish(&service.lane),
        Err(sortsize_store::inventory::Error::Contract(
            "generation preparation cancelled"
        ))
    ));
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((3, Phase::Cancelled, _))
    ));
    assert_eq!(active.session.status().in_flight, 0);
    assert_eq!(active.session.status().quanta, 0);
    drop(active);
    drop(service);
    let cleanup = super::super::cache::CLEANUP_ERROR.lock().unwrap();
    assert!(
        cleanup.is_none(),
        "cancelled partial fixture must retire its native owners without contaminating later tests: {cleanup:?}"
    );
}

#[test]
fn cancellation_request_interrupts_preparation_but_ignores_terminal_writer() {
    let service = Service {
        trace: None,
        workers: Vec::new(),
        lane: Arc::new(Mutex::new(Lane {
            source_epoch: 9,
            phase: Some((9, Phase::Reopening, GenerationId(2))),
            writer_running: true,
            ..Default::default()
        })),
        output: Default::default(),
        search_output: Default::default(),
        resources: Default::default(),
        cache_output: Default::default(),
        preview_output: Default::default(),
    };
    assert!(service.cancel_acquisition());
    assert!(
        service
            .lane
            .lock()
            .unwrap()
            .publication_cancel
            .is_cancelled()
    );
    {
        let mut lane = service.lane.lock().unwrap();
        lane.writer_running = false;
        lane.phase = Some((9, Phase::Cancelled, GenerationId(2)));
        lane.cancel = false;
    }
    assert!(!service.cancel_acquisition());
    assert!(!service.lane.lock().unwrap().cancel);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 10;
        lane.phase = None;
        lane.writer_running = true;
        lane.writer_owns_session = true;
        lane.publication_cancel = Default::default();
        lane.preparing_source =
            Some((PathBuf::from("new-source"), lane.publication_cancel.clone()));
    }
    assert!(service.cancel_acquisition());
    assert!(
        service
            .lane
            .lock()
            .unwrap()
            .publication_cancel
            .is_cancelled()
    );
}

#[test]
fn cancellation_request_accepts_ready_and_batch_terminal_without_sticking_ui() {
    let mut lane = Lane {
        source_epoch: 4,
        cancel_requested_epoch: Some(4),
        phase: Some((4, Phase::Reopening, GenerationId(2))),
        ..Default::default()
    };
    // Sealed acquisition alone is not terminal: certification is still busy.
    assert!(lane.cancellation_pending());
    lane.phase = Some((4, Phase::Ready, GenerationId(2)));
    assert!(!lane.cancellation_pending());
    lane.writer_running = true;
    assert!(lane.cancellation_pending());
    lane.writer_running = false;
    lane.smb_snapshot = Some(super::super::smb_server::Snapshot {
        epoch: 4,
        server: PathBuf::from("logical-server"),
        shares: Vec::new(),
        running: true,
        cancelled: true,
        discovery_uncertain: false,
    });
    // A previous share's Ready cannot acknowledge a still-running batch.
    assert!(lane.cancellation_pending());
    lane.smb_snapshot.as_mut().unwrap().running = false;
    assert!(!lane.cancellation_pending());
    // Cancellation of another epoch does not affect a replacement intention.
    lane.source_epoch = 5;
    assert!(!lane.cancellation_pending());
}

fn wait_cancel_failure(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "failed cancellation did not settle"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn candidate_bytes(path: &Path) -> (Vec<u8>, Option<Vec<u8>>) {
    (
        std::fs::read(path.join("inventory.db")).unwrap(),
        std::fs::read(path.join("inventory.db-wal")).ok(),
    )
}

fn seal_fixture(active: &mut Active) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while active.session.status().state != State::Sealed {
        let status = active.session.tick().unwrap();
        if matches!(status.state, State::Idle | State::Cancelled) && status.in_flight == 0 {
            active.session.finalize_step().unwrap();
        }
        assert!(Instant::now() < deadline);
    }
    let measured = std::time::SystemTime::now();
    active
        .session
        .store_mut()
        .unwrap()
        .record_measurement(
            active.c.generation,
            &active.root,
            super::super::cache::seconds(active.started),
            super::super::cache::seconds(measured),
        )
        .unwrap();
    active.sealed = Some(measured);
}

#[test]
fn cancellation_after_publication_error_acknowledges_parked_candidate_and_preserves_g0() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let next_source = temporary.path().join("next-source");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&next_source).unwrap();
    std::fs::write(source.join("g0-file"), [1; 17]).unwrap();
    std::fs::write(next_source.join("next-file"), [2; 19]).unwrap();
    let mut service = Service::start().unwrap();
    service.lane.lock().unwrap().memory = MemoryMode::Entries50k;
    start(&service, 1, &source);
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| epoch == 1 && matches!(phase, Phase::Ready))
    });
    until(&service, || {
        service.lane.lock().unwrap().reader_workspace.is_some()
    });
    let workspace = service
        .lane
        .lock()
        .unwrap()
        .reader_workspace
        .clone()
        .unwrap();
    let g0_epoch = service.lane.lock().unwrap().reader_epoch.clone();
    let g0_path = workspace
        .repository()
        .lock()
        .unwrap()
        .generation_path(GenerationId(1));
    let g0_bytes = std::fs::read(g0_path.join("inventory.db")).unwrap();
    let mut candidate = Active::open(
        2,
        Start {
            root: source.clone(),
            explicit_root: true,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        },
        Some((
            RepositoryState {
                owner: workspace.repository(),
                plan: None,
                published: Some(GenerationId(1)),
            },
            workspace.clone(),
        )),
        Default::default(),
    )
    .unwrap();
    seal_fixture(&mut candidate);
    let generation = candidate.c.generation;
    let candidate_path = candidate.session.store().directory().to_owned();
    // A real preserved publication stage produces the ordinary guarded error,
    // before cancellation. It is never removed by the cancellation itself.
    let stage = workspace.path().join(format!(
        ".stage-a-{:032x}-{:016x}",
        candidate.c.inventory.0, generation.0
    ));
    std::fs::create_dir(&stage).unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 2;
        lane.view_epoch = 2;
        lane.phase = Some((2, Phase::Preparing, generation));
        lane.writer_running = true;
        lane.publication_cancel = candidate.publication_cancel.clone();
        lane.writer_target = Some(Box::new(candidate));
    }
    wait_cancel_failure(|| {
        matches!(
            service.output.lock().unwrap().as_ref(),
            Some(Output::Error(2, message, true)) if message.contains("previous stage preserved")
        )
    });
    assert!(service.cancel_acquisition());
    wait_cancel_failure(|| {
        let lane = service.lane.lock().unwrap();
        !lane.cancellation_pending() && matches!(lane.phase, Some((2, Phase::Cancelled, _)))
    });
    assert_eq!(service.lane.lock().unwrap().reader_epoch, g0_epoch);
    assert_eq!(
        std::fs::read(g0_path.join("inventory.db")).unwrap(),
        g0_bytes
    );
    assert!(
        stage.exists(),
        "cancellation must preserve the failed stage"
    );
    assert!(
        candidate_path.exists(),
        "failed candidate must remain owned"
    );
    let frozen = candidate_bytes(&candidate_path);
    let quanta = service.lane.lock().unwrap().writer_quanta;
    std::fs::remove_dir(&stage).unwrap(); // Only this test's injected empty stage.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(candidate_bytes(&candidate_path), frozen);
    assert_eq!(service.lane.lock().unwrap().writer_quanta, quanta);
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((2, Phase::Cancelled, _))
    ));
    assert!(
        !workspace
            .repository()
            .lock()
            .unwrap()
            .generation_path(generation)
            .exists()
    );
    drop(service.output.lock().unwrap().take());
    start(&service, 3, &next_source);
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| epoch == 3 && matches!(phase, Phase::Ready))
    });
    drop(workspace);
    let handles = std::mem::take(&mut service.workers);
    drop(service);
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
}

#[test]
fn cancellation_skips_finalization_and_freezes_unsealed_candidate_after_real_drain() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for index in 0..128 {
        std::fs::write(source.join(format!("file-{index:03}")), [7; 17]).unwrap();
    }
    let mut candidate = Active::open(
        4,
        Start {
            root: source.clone(),
            explicit_root: true,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        },
        None,
        Default::default(),
    )
    .unwrap();
    let status = candidate.session.tick().unwrap();
    assert!(
        status.in_flight > 0,
        "fixture must cancel a genuinely active acquisition"
    );
    let control = cancellation_service(&candidate);
    assert!(control.cancel_acquisition());
    candidate.cancelled = true;
    assert!(
        !settle_cancelled_failure(&mut candidate, &control.lane),
        "outstanding native work must prevent terminal cancellation"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = candidate.session.tick().unwrap();
        if status.state == State::Cancelled && status.in_flight == 0 {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let generation = candidate.c.generation;
    let path = candidate.session.store().directory().to_owned();
    // Start both owners before the failure injection. Exhaust only the existing
    // governor's bounded lease metadata, without reserving any disk bytes or
    // moving SQLite files that remain open on Windows.
    let mut service = Service::start().unwrap();
    let governor = sortsize_core::resources::Governor::global();
    let reserved = governor.usage().disk_reserved;
    let mut held_disk_leases = Vec::<sortsize_core::resources::DiskReservation>::new();
    for _ in 0..1024 {
        let Some(lease) = governor.reserve_disk(0) else {
            break;
        };
        held_disk_leases.push(lease);
    }
    assert!(!held_disk_leases.is_empty());
    assert!(governor.reserve_disk(0).is_none());
    assert_eq!(governor.usage().disk_reserved, reserved);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.memory = MemoryMode::Entries50k;
        lane.source_epoch = 4;
        lane.view_epoch = 4;
        lane.phase = Some((4, Phase::Preparing, generation));
        lane.cancel_requested_epoch = Some(4);
        lane.writer_running = true;
        lane.publication_cancel = candidate.publication_cancel.clone();
        lane.writer_target = Some(Box::new(candidate));
    }
    wait_cancel_failure(|| !service.lane.lock().unwrap().cancellation_pending());
    assert!(
        !matches!(
            service.output.lock().unwrap().as_ref(),
            Some(Output::Error(4, _, _))
        ),
        "Cancel must not attempt the unavailable finalization pipeline"
    );
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((4, Phase::Cancelled, _))
    ));
    // RAII releases every injected lease even if an assertion above unwinds.
    // Removing the admission failure must not restart the frozen candidate.
    drop(held_disk_leases);
    assert!(governor.reserve_disk(0).is_some());
    assert!(path.join("inventory.db").is_file());
    let frozen = candidate_bytes(&path);
    let quanta = service.lane.lock().unwrap().writer_quanta;
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(candidate_bytes(&path), frozen);
    assert_eq!(service.lane.lock().unwrap().writer_quanta, quanta);
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((4, Phase::Cancelled, _))
    ));
    drop(control);
    let handles = std::mem::take(&mut service.workers);
    drop(service);
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
}

#[test]
fn strict_cancellation_after_ingestion_error_drains_and_keeps_unsealed_candidate_and_g0() {
    cancellation_freezes_candidate_and_preserves_g0(true);
}

#[test]
fn strict_cancellation_skips_derivatives_and_publication_and_keeps_g0() {
    cancellation_freezes_candidate_and_preserves_g0(false);
}

fn cancellation_freezes_candidate_and_preserves_g0(inject_ingestion_error: bool) {
    let governor = sortsize_core::resources::Governor::global();
    governor.configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let next_source = temporary.path().join("next-source");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&next_source).unwrap();
    std::fs::write(source.join("g0-file"), [1; 17]).unwrap();
    std::fs::write(next_source.join("next-file"), [2; 19]).unwrap();
    let mut service = Service::start().unwrap();
    service.lane.lock().unwrap().memory = MemoryMode::Entries50k;
    service.lane.lock().unwrap().force_strict = true;
    start(&service, 1, &source);
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| epoch == 1 && matches!(phase, Phase::Ready))
    });
    until(&service, || {
        service.lane.lock().unwrap().reader_workspace.is_some()
    });
    let workspace = service
        .lane
        .lock()
        .unwrap()
        .reader_workspace
        .clone()
        .unwrap();
    let g0_epoch = service.lane.lock().unwrap().reader_epoch.clone();
    let g0_path = workspace
        .repository()
        .lock()
        .unwrap()
        .generation_path(GenerationId(1));
    let g0_bytes = std::fs::read(g0_path.join("inventory.db")).unwrap();
    for index in 0..128 {
        std::fs::write(source.join(format!("new-file-{index:03}")), [7; 17]).unwrap();
    }
    for directory in 0..8 {
        let folder = source.join(format!("parallel-{directory}"));
        std::fs::create_dir(&folder).unwrap();
        for index in 0..64 {
            std::fs::write(folder.join(format!("file-{index:03}")), [7; 17]).unwrap();
        }
    }
    let mut candidate = Active::open(
        2,
        Start {
            root: source.clone(),
            explicit_root: true,
            observation: ScanObservation::Strict,
            preflight: None,
            options: Default::default(),
        },
        Some((
            RepositoryState {
                owner: workspace.repository(),
                plan: None,
                published: Some(GenerationId(1)),
            },
            workspace.clone(),
        )),
        Default::default(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut status = candidate.session.tick().unwrap();
    while status.in_flight < 4 {
        assert!(
            Instant::now() < deadline,
            "fixture must admit parallel acquisition"
        );
        std::thread::sleep(Duration::from_millis(1));
        status = candidate.session.tick().unwrap();
    }
    assert!(
        status.in_flight >= 4,
        "must have several native/completed jobs before cancellation"
    );
    assert_eq!(candidate.session.store().latest_sealed().unwrap(), None);
    let generation = candidate.c.generation;
    let candidate_path = candidate.session.store().directory().to_owned();
    // Exhaust bounded disk-lease metadata only. No source/database file is
    // modified to inject the refusal, and every lease is released by RAII.
    let reserved = governor.usage().disk_reserved;
    let mut leases = Vec::new();
    if inject_ingestion_error {
        for _ in 0..1024 {
            let Some(lease) = governor.reserve_disk(0) else {
                break;
            };
            leases.push(lease);
        }
        assert!(!leases.is_empty());
        assert!(governor.reserve_disk(0).is_none());
    } else {
        let control = cancellation_service(&candidate);
        assert!(control.cancel_acquisition());
    }
    assert_eq!(governor.usage().disk_reserved, reserved);
    {
        let mut lane = service.lane.lock().unwrap();
        if !inject_ingestion_error {
            lane.cancel_requested_epoch = Some(2);
            lane.cancel = true;
        }
        lane.source_epoch = 2;
        lane.view_epoch = 2;
        lane.phase = Some((2, Phase::Preparing, generation));
        lane.writer_running = true;
        lane.publication_cancel = candidate.publication_cancel.clone();
        lane.writer_target = Some(Box::new(candidate));
    }
    if inject_ingestion_error {
        wait_cancel_failure(|| {
            matches!(
                service.output.lock().unwrap().as_ref(),
                Some(Output::Error(2, message, true))
                    if message.contains("canonical prepaid operation metadata admission")
                        || message.contains("shared disk quota")
            )
        });
        assert!(
            service.lane.lock().unwrap().writer_running,
            "retained completion is still accounted as outstanding work"
        );
        assert!(service.cancel_acquisition());
    }
    wait_cancel_failure(|| {
        let lane = service.lane.lock().unwrap();
        !lane.cancellation_pending() && matches!(lane.phase, Some((2, Phase::Cancelled, _)))
    });
    {
        let lane = service.lane.lock().unwrap();
        assert!(!lane.writer_running);
        assert_eq!(lane.writer_in_flight, 0);
        assert_eq!(lane.cancelled_unpublished, Some(2));
        assert_eq!(lane.reader_epoch, g0_epoch);
    }
    assert_eq!(
        std::fs::read(g0_path.join("inventory.db")).unwrap(),
        g0_bytes
    );
    assert!(candidate_path.join("inventory.db").is_file());
    // Inspect cannot certify an unsealed failed candidate as a published G1.
    assert!(
        !workspace
            .repository()
            .lock()
            .unwrap()
            .generation_path(generation)
            .exists()
    );
    let frozen = candidate_bytes(&candidate_path);
    let quanta = service.lane.lock().unwrap().writer_quanta;
    drop(leases);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(candidate_bytes(&candidate_path), frozen);
    assert_eq!(service.lane.lock().unwrap().writer_quanta, quanta);
    drop(service.output.lock().unwrap().take());
    // The next voluntary analysis of this SAME source recovers the preserved
    // candidate under a fresh Control/context, rather than allocating another
    // physical candidate or silently retiring raw observations.
    start(&service, 3, &source);
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| epoch == 3 && matches!(phase, Phase::Ready))
    });
    let published = workspace
        .repository()
        .lock()
        .unwrap()
        .published(generation)
        .unwrap()
        .unwrap();
    assert_eq!(published.coverage, ScopeCoverage::Complete);
    assert!(published.context.run.0 > 2);
    assert_eq!(
        std::fs::read(g0_path.join("inventory.db")).unwrap(),
        g0_bytes
    );
    drop(service.output.lock().unwrap().take());
    start(&service, 4, &next_source);
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .phase
            .is_some_and(|(epoch, phase, _)| epoch == 4 && matches!(phase, Phase::Ready))
    });
    drop(workspace);
    let handles = std::mem::take(&mut service.workers);
    drop(service);
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(super::super::cache::CLEANUP_ERROR.lock().unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn fast_snapshot_stays_consultable_during_save_and_previous_family_survives_failure() {
    use std::sync::atomic::Ordering;

    sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), [1; 3]).unwrap();
    let request = || Start {
        root: source.clone(),
        explicit_root: false,
        observation: crate::workshop::service::size_observation(),
        preflight: None,
        options: Default::default(),
    };
    let acquire = |writer: &mut Active| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while writer.session.status().state != State::Sealed {
            let status = writer.session.tick().unwrap();
            if status.state == State::Idle {
                writer.session.finalize_step().unwrap();
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let measured = std::time::SystemTime::now();
        writer
            .session
            .store_mut()
            .unwrap()
            .record_measurement(
                writer.c.generation,
                &writer.root,
                super::super::cache::seconds(writer.started),
                super::super::cache::seconds(measured),
            )
            .unwrap();
        writer.sealed = Some(measured);
    };

    let mut standard = Active::open(1, request(), None, Default::default()).unwrap();
    acquire(&mut standard);
    let mut g0 = standard
        .publish(&Mutex::new(Lane::default()))
        .unwrap()
        .unwrap();
    drop(standard);
    let retained_root = temporary.path().join("state");
    let old_path = g0.retain(&retained_root, &Default::default()).unwrap();
    let selected = std::fs::read(old_path.join("CURRENT.json")).unwrap();

    std::fs::write(source.join("second"), [1; 11]).unwrap();
    let mut fast = Active::open_with_placement_log(
        2,
        request(),
        None,
        Default::default(),
        acquisition_budget(true),
        None,
        true,
    )
    .unwrap();
    assert!(fast.fast_mode == FastMode::Acquiring);
    let Backend::Live(session) = &fast.session else {
        panic!("fast writer must own a session");
    };
    let progress = session.progress_counter();
    assert_eq!(progress.load(Ordering::Relaxed), 0);
    acquire(&mut fast);
    assert_eq!(progress.load(Ordering::Relaxed), 2);
    assert_eq!(fast.session.status().processed_entries, 2);

    let private = fast.workspace.as_ref().unwrap().path();
    assert!(!private.join("generation-0000000000000001").exists());
    let lane = Mutex::new(Lane {
        source_epoch: 2,
        view_epoch: 2,
        writer_epoch: Some(2),
        writer_owns_session: true,
        ..Default::default()
    });
    fast.attach_fast_reader(&lane).unwrap();
    assert!(!fast.owns_acquisition());
    {
        let shared = lane.lock().unwrap();
        assert!(shared.writer_epoch.is_none());
        assert!(!shared.writer_owns_session);
    }
    let reader = lane.lock().unwrap().reader_target.take().unwrap().unwrap();
    assert!(reader.is_fast_preview());
    assert!(reader.certified_generation().is_none());
    assert_eq!(
        reader
            .session
            .store()
            .entry_count(reader.c.generation)
            .unwrap(),
        3
    );

    fast.fast_mode = FastMode::Saving;
    let blocked = retained_root.join("blocked");
    std::fs::write(&blocked, b"keep").unwrap();
    assert!(fast.retain_fast(&blocked, &Default::default()).is_err());
    assert_eq!(std::fs::read(&blocked).unwrap(), b"keep");
    assert_eq!(
        std::fs::read(old_path.join("CURRENT.json")).unwrap(),
        selected
    );
    assert_eq!(g0.session.store().entry_count(g0.c.generation).unwrap(), 2);
    assert_eq!(
        reader
            .session
            .store()
            .entry_count(reader.c.generation)
            .unwrap(),
        3
    );

    let new_path = retained_root.join("new");
    fast.retain_fast(&new_path, &Default::default()).unwrap();
    assert!(fast.certified_generation().is_some());
    assert_eq!(
        fast.session.store().entry_count(fast.c.generation).unwrap(),
        3
    );
    assert_eq!(
        reader
            .session
            .store()
            .entry_count(reader.c.generation)
            .unwrap(),
        3
    );
    assert_eq!(
        std::fs::read(old_path.join("CURRENT.json")).unwrap(),
        selected
    );
}

#[test]
fn changing_view_does_not_revoke_fast_save_but_changing_source_does() {
    let context = AcquisitionContext {
        inventory: InventoryId(133),
        source: SourceId(144),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let queued = sortsize_core::CancellationToken::default();
    let running = sortsize_core::CancellationToken::default();
    let mut lane = Lane {
        source_epoch: 1,
        view_epoch: 1,
        cache: Some(cache::Request {
            id: 1,
            epoch: 1,
            expected: 1,
            action: cache::Action::SaveFast {
                source_epoch: 1,
                context,
            },
            cancel: queued.clone(),
        }),
        fast_save_cancel: Some((1, running.clone())),
        ..Default::default()
    };
    lane.view_epoch = 2;
    let navigation_cancel = sortsize_core::CancellationToken::default();
    navigation_cancel.cancel();
    assert!(!queued.is_cancelled());
    assert!(!running.is_cancelled());
    lane.cancel_publication();
    assert!(lane.publication_cancel.is_cancelled());
    assert!(queued.is_cancelled());
    assert!(running.is_cancelled());
    assert!(lane.cache.is_none());
}

#[cfg(windows)]
#[test]
fn direct_live_pages_refresh_under_retained_publication_pressure() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Auto, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for directory in 0..12 {
        let folder = source.join(format!("directory-{directory:02}"));
        std::fs::create_dir(&folder).unwrap();
        for file in 0..256 {
            std::fs::write(folder.join(format!("file-{file:03}")), [7; 9]).unwrap();
        }
    }
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.tick_delay = Duration::from_millis(100);
        lane.cache_root = Some(temporary.path().join("history"));
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: ScanObservation::DirectoryListing,
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    let mut held: Vec<Box<Prepared>> = Vec::new();
    for _ in 0..2 {
        let captured = Mutex::new(None);
        until(&service, || {
            let mut output = service.output.lock().unwrap();
            if output.as_ref().is_some_and(|output| {
                matches!(output, Output::Page(page) if page.busy && !page.frame.rows.is_empty()
                    && !page.tiles.is_empty()
                    && page.tiles.iter().any(|tile| tile.band_w > 0.)
                    && held.last().is_none_or(|prior| prior.quanta < page.quanta
                        && prior.root_measure.unwrap().logical < page.root_measure.unwrap().logical))
            }) {
                *captured.lock().unwrap() = output.take();
                true
            } else { false }
        });
        let Some(Output::Page(page)) = captured.into_inner().unwrap() else {
            unreachable!()
        };
        assert_eq!(page.sort_index, -1);
        held.push(page);
    }
    let revision = held[1].quanta;
    let observed = service
        .lane
        .lock()
        .unwrap()
        .scan_progress
        .as_ref()
        .unwrap()
        .processed_entries
        .load(std::sync::atomic::Ordering::Relaxed);
    until(&service, || {
        service
            .lane
            .lock()
            .unwrap()
            .scan_progress
            .as_ref()
            .unwrap()
            .processed_entries
            .load(std::sync::atomic::Ordering::Relaxed)
            > observed
    });
    assert!(service.output.lock().unwrap().is_none());
    drop(held.remove(0));
    let captured = Mutex::new(None);
    until(&service, || {
        let mut output = service.output.lock().unwrap();
        if output.as_ref().is_some_and(|output| {
            matches!(output, Output::Page(page) if page.busy && page.quanta > revision
                && !page.frame.rows.is_empty() && !page.tiles.is_empty())
        }) {
            *captured.lock().unwrap() = output.take();
            true
        } else {
            false
        }
    });
    let Some(Output::Page(page)) = captured.into_inner().unwrap() else {
        unreachable!()
    };
    let folder_row = page
        .frame
        .rows
        .iter()
        .find(|row| row.kind == sortsize_store::inventory::Kind::Directory && row.logical > Some(0))
        .unwrap();
    let folder = folder_row.key;
    let folder_path = folder_row.path.clone();
    drop(page);
    drop(held);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 2;
        lane.navigation = Some((2, Navigate::Open(folder)));
    }
    let first_ids = Mutex::new(None);
    until(&service, || {
        let output = service.output.lock().unwrap();
        if output.as_ref().is_some_and(|output| {
            matches!(output, Output::Page(page) if page.epoch == 2 && page.busy
                && page.parent_entry == folder.entry && page.frame.more && !page.frame.rows.is_empty()
                && page.frame.rows.iter().all(|row| row.key.inventory == folder.inventory
                    && row.key.generation == folder.generation && row.path.starts_with(&folder_path)))
        }) {
            let Some(Output::Page(page)) = output.as_ref() else { unreachable!() };
            *first_ids.lock().unwrap() = Some(page.frame.rows.iter().map(|row| row.key.entry).collect::<Vec<_>>());
            true
        } else { false }
    });
    let first_ids = first_ids.into_inner().unwrap().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 3;
        lane.navigation = Some((3, Navigate::ObservedPage(true)));
    }
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|output| {
            matches!(output, Output::Page(page) if page.epoch == 3 && page.parent_entry == folder.entry
                && !page.frame.rows.is_empty() && page.frame.rows.iter().all(|row| !first_ids.contains(&row.key.entry)))
        })
    });
    assert!(service.cancel_acquisition());
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Cancelled, _))
        )
    });
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|output| {
            matches!(output, Output::Page(page) if page.epoch == 3 && page.parent_entry == folder.entry
                && !page.busy && page.frame.coverage == ScopeCoverage::Partial)
        })
    });
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 4;
        lane.navigation = Some((4, Navigate::Parent));
    }
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|output| {
            matches!(output, Output::Page(page) if page.epoch == 4 && page.parent_entry == EntryId(0)
                && !page.busy && page.frame.coverage == ScopeCoverage::Partial
                && page.frame.rows.iter().all(|row| row.key.generation == folder.generation))
        })
    });
}

#[cfg(target_os = "linux")]
#[test]
fn ubuntu_direct_scan_keeps_sqlite_unchanged_until_acquisition_finishes() {
    struct ReleaseGate(sortsize_store::direct::TestBatchGate);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            let (lock, ready) = &*self.0;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
    }

    let temporary = single_scan_save_fixture();
    let source = temporary.path().join("source");
    let history = temporary.path().join("history");
    std::fs::create_dir(&source).unwrap();
    let file = source.join("observed");
    std::fs::write(&file, [1; 11]).unwrap();
    let service = Service::start().unwrap();
    service.lane.lock().unwrap().cache_root = Some(history.clone());
    let request = |epoch, gate| {
        let mut lane = service.lane.lock().unwrap();
        lane.cancel_publication();
        lane.publication_cancel = Default::default();
        lane.source_epoch = epoch;
        lane.view_epoch = epoch;
        lane.start_purpose = StartPurpose::ManualScan;
        lane.tick_gate = gate;
        lane.tick_blocked = false;
        lane.start = Some((
            epoch,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
    };
    let snapshot =
        sortsize_store::direct_snapshot::destination(&cache::snapshot_root(&history), &source);
    request(1, None);
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Ready, _))
        )
    });
    let snapshot_before = std::fs::read(&snapshot).unwrap();
    let recorded = sortsize_store::direct_snapshot::inspect(&snapshot).unwrap();
    assert_eq!(recorded.summary.measure.logical, Some(11));
    assert_eq!(
        sortsize_store::scheduler::policy_observation(recorded.context.policy),
        ScanObservation::Strict
    );

    std::fs::write(&file, [2; 29]).unwrap();
    let admit = ReleaseGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let acquire = ReleaseGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    request(2, Some(admit.0.clone()));
    until(&service, || service.lane.lock().unwrap().tick_blocked);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.tick_blocked = false;
        lane.tick_gate = Some(acquire.0.clone());
    }
    drop(admit);
    until(&service, || {
        let lane = service.lane.lock().unwrap();
        lane.tick_blocked
            && lane.direct_view == Some(2)
            && matches!(lane.phase, Some((2, Phase::Acquiring, _)))
            && service.output.lock().unwrap().as_ref().is_some_and(|out| {
                matches!(out, Output::Page(page) if page.epoch == 2
                    && page.busy && page.root_measure.and_then(|m| m.logical) == Some(29))
            })
    });
    assert!(service.lane.lock().unwrap().reader_workspace.is_none());
    assert_eq!(std::fs::read(&snapshot).unwrap(), snapshot_before);

    assert!(service.cancel_acquisition());
    service.lane.lock().unwrap().tick_gate = None;
    drop(acquire);
    until(&service, || {
        let lane = service.lane.lock().unwrap();
        lane.direct_view == Some(2)
            && matches!(lane.phase, Some((2, Phase::Cancelled, _)))
            && service.output.lock().unwrap().as_ref().is_some_and(|out| {
                matches!(out, Output::Page(page) if page.epoch == 2
                    && !page.busy && page.frame.coverage == ScopeCoverage::Partial
                    && page.root_measure.and_then(|m| m.logical) == Some(29))
            })
    });
    assert_eq!(std::fs::read(&snapshot).unwrap(), snapshot_before);
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_manual_and_automatic_use_direct_without_resume() {
    sortsize_core::resources::Governor::global().configure(MemoryMode::Auto, None);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for i in 0..350 {
        std::fs::write(
            source.join(format!("file-{i:04}")),
            vec![7; (i + 1) as usize],
        )
        .unwrap();
    }
    let service = Service::start().unwrap();
    service.lane.lock().unwrap().cache_root = Some(temporary.path().join("catalogue"));
    for (epoch, manual) in [(1, false), (2, true)] {
        {
            let mut lane = service.lane.lock().unwrap();
            lane.publication_cancel.cancel();
            lane.publication_cancel = Default::default();
            lane.source_epoch = epoch;
            lane.view_epoch = epoch;
            lane.fast_local = manual && cfg!(windows);
            lane.start_purpose = if manual {
                StartPurpose::ManualScan
            } else {
                StartPurpose::Regular
            };
            lane.cancel = false;
            lane.phase = None;
            lane.cancel_requested_epoch = None;
            lane.start = Some((
                epoch,
                Start {
                    root: source.clone(),
                    explicit_root: manual,
                    observation: size_observation(),
                    preflight: None,
                    options: Default::default(),
                },
            ));
        }
        until(&service, || {
            service.lane.lock().unwrap().direct_view == Some(epoch)
        });
        until(
            &service,
            || matches!(service.lane.lock().unwrap().phase,Some((e,Phase::Ready,_)) if e==epoch),
        );
        until(&service, || {
            service.output.lock().unwrap().as_ref().is_some_and(
                |o| matches!(o,Output::Page(p) if p.epoch==epoch && !p.busy && p.sort_index==0),
            )
        });
        let output = service.output.lock().unwrap();
        let Some(Output::Page(page)) = output.as_ref() else {
            unreachable!()
        };
        assert_eq!(page.root_measure.unwrap().files, 350);
        assert_eq!(page.frame.rows.first().unwrap().logical, Some(350));
        drop(output);
        {
            let mut lane = service.lane.lock().unwrap();
            lane.search_current = Some(epoch);
            lane.search = Some(search::Request {
                id: epoch,
                query: SearchQuery {
                    min_bytes: Some(331),
                    ..Default::default()
                },
                cancel: Default::default(),
            });
        }
        until(&service, || {
            service
                .search_output
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|p| p.id == epoch && !p.busy)
        });
        let search = service.search_output.lock().unwrap();
        let search = search.as_ref().unwrap();
        assert_eq!(search.matches, 20);
        assert!(search.complete);
        assert_eq!(search.rows.len(), 20);
    }
}

#[cfg(windows)]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/qualification/windows_single_scan/service_measure.rs"
));

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_snapshot_reloads_without_source_and_retains_sort_and_search() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for (name, bytes) in [("alpha", 3), ("omega", 31)] {
        std::fs::write(source.join(name), vec![7; bytes]).unwrap();
    }
    let history = temporary.path().join("history");
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cache_root = Some(history.clone());
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Ready, _))
        )
    });
    let path = std::fs::read_dir(cache::snapshot_root(&history))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite")
        })
        .expect("saved snapshot file");
    let meta = sortsize_store::direct_snapshot::inspect(&path).unwrap();
    let saved = sortsize_store::inventory::catalogue::Saved {
        directory: std::fs::canonicalize(&path).unwrap(),
        context: meta.context,
        root: meta.root,
        started: meta.started,
        measured: Some(meta.measured),
        coverage: meta.summary.coverage,
        logical: meta.summary.measure.logical,
        unknown: meta.summary.measure.unknown,
        continuity: None,
    };
    // The owned fixture is moved out of the recorded location. Reload must not acquire it.
    std::fs::rename(&source, temporary.path().join("unavailable-source")).unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 2;
        lane.view_epoch = 2;
        lane.publication_cancel = Default::default();
        lane.cache = Some(cache::Request {
            id: 7,
            epoch: 2,
            expected: 2,
            action: cache::Action::Open(saved.clone()),
            cancel: Default::default(),
        });
    }
    until(&service, || {
        service
            .output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|o| matches!(o,Output::Page(p) if p.epoch==2&&!p.busy&&p.sort_index==0))
    });
    let output = service.output.lock().unwrap();
    let Some(Output::Page(page)) = output.as_ref() else {
        unreachable!()
    };
    assert_eq!(page.root_measure.unwrap().logical, Some(34));
    assert_eq!(page.frame.rows.first().unwrap().logical, Some(31));
    drop(output);
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((2, Phase::Ready, _))
    ));
    {
        let mut lane = service.lane.lock().unwrap();
        lane.search_current = Some(8);
        lane.search = Some(search::Request {
            id: 8,
            query: SearchQuery {
                name: Some("omega".into()),
                ..Default::default()
            },
            cancel: Default::default(),
        });
    }
    until(&service, || {
        service
            .search_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 8 && !p.busy && p.matches == 1)
    });
    assert!(!source.exists());
    // The catalogue path uses native normal spelling, whereas the selected file
    // uses its canonical verbatim spelling. Forget must accept both and release
    // the persisted file's disk charge, while the loaded tree stays consultable.
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 9,
        epoch: 2,
        expected: 2,
        action: cache::Action::Forget(saved.clone()),
        cancel: Default::default(),
    });
    until(&service, || {
        service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 9)
    });
    assert!(
        saved.directory.exists(),
        "the active inventory must not be forgotten"
    );
    assert!(
        !service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .diagnostic
            .is_empty()
    );
    let replacement = temporary.path().join("replacement-source");
    std::fs::create_dir(&replacement).unwrap();
    std::fs::write(replacement.join("other"), [1; 5]).unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 3;
        lane.view_epoch = 3;
        lane.publication_cancel = Default::default();
        lane.start = Some((
            3,
            Start {
                root: replacement,
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((3, Phase::Ready, _))
        )
    });
    let bytes = std::fs::metadata(&saved.directory).unwrap().len();
    let disk_before = sortsize_core::resources::Governor::global()
        .usage()
        .disk_used;
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 10,
        epoch: 3,
        expected: 3,
        action: cache::Action::Forget(saved.clone()),
        cancel: Default::default(),
    });
    until(&service, || {
        service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.id == 10)
    });
    assert!(!saved.directory.exists());
    assert!(
        service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .diagnostic
            .is_empty()
    );
    let disk_after = sortsize_core::resources::Governor::global()
        .usage()
        .disk_used;
    assert!(
        disk_after <= disk_before.saturating_sub(bytes),
        "snapshot quota was not released: before={disk_before}, after={disk_after}, bytes={bytes}"
    );
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_navigation_survives_background_save() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let folder = source.join("selected");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("inside"), [5; 23]).unwrap();
    for i in 0..2048 {
        std::fs::write(source.join(format!("leaf-{i:04}")), [3; 7]).unwrap();
    }
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cache_root = Some(temporary.path().join("history"));
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|o|matches!(o,Output::Page(p) if p.frame.rows.iter().any(|r|r.kind==sortsize_store::inventory::Kind::Directory)))
    });
    let key = {
        let output = service.output.lock().unwrap();
        let Some(Output::Page(p)) = output.as_ref() else {
            unreachable!()
        };
        p.frame
            .rows
            .iter()
            .find(|r| r.kind == sortsize_store::inventory::Kind::Directory)
            .unwrap()
            .key
    };
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 2;
        lane.navigation = Some((2, Navigate::Open(key)));
    }
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Ready, _))
        )
    });
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(
            |o| matches!(o,Output::Page(p) if p.epoch==2&&p.parent_entry==key.entry&&!p.busy),
        )
    });
    let output = service.output.lock().unwrap();
    let Some(Output::Page(p)) = output.as_ref() else {
        unreachable!()
    };
    assert_eq!(p.frame.rows.len(), 1);
    assert_eq!(p.frame.rows[0].logical, Some(23));
    assert_eq!(p.parent_entry, key.entry);
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_interrupted_tree_survives_strict_priority_and_return() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for d in 0..48 {
        let child = source.join(format!("branch-{d:03}"));
        std::fs::create_dir(&child).unwrap();
        for f in 0..128 {
            std::fs::write(child.join(format!("f-{f:03}")), [7; 3]).unwrap();
        }
    }
    let service = Service::start().unwrap();
    let request = |epoch, root: PathBuf, purpose, observation| {
        let mut lane = service.lane.lock().unwrap();
        lane.publication_cancel.cancel();
        lane.publication_cancel = Default::default();
        lane.source_epoch = epoch;
        lane.view_epoch = epoch;
        lane.phase = None;
        lane.cancel = false;
        lane.cancel_requested_epoch = None;
        lane.start_purpose = purpose;
        lane.start = Some((
            epoch,
            Start {
                root,
                explicit_root: true,
                observation,
                preflight: None,
                options: Default::default(),
            },
        ));
    };
    service.lane.lock().unwrap().cache_root = Some(temporary.path().join("history"));
    request(1, source.clone(), StartPurpose::Regular, size_observation());
    until(&service, || {
        service.lane.lock().unwrap().direct_view == Some(1)
    });
    assert!(service.cancel_acquisition());
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Cancelled, _))
        )
    });
    until(&service, || {
        service
            .output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|o| matches!(o,Output::Page(p) if p.epoch==1&&!p.busy))
    });
    let (inventory, files) = {
        let o = service.output.lock().unwrap();
        let Some(Output::Page(p)) = o.as_ref() else {
            unreachable!()
        };
        (
            p.frame.stamp.context.inventory,
            p.root_measure.unwrap().files,
        )
    };
    // Pause the worker after it captured an empty start slot. The next
    // intention arrives before the same worker checks whether Direct retired.
    struct TurnRelease(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for TurnRelease {
        fn drop(&mut self) {
            let (lock, ready) = &*self.0;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
    }
    let turn_gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release = TurnRelease(turn_gate.clone());
    {
        let mut lane = service.lane.lock().unwrap();
        lane.turn_gate = Some(turn_gate.clone());
        lane.turn_blocked = false;
    }
    until(&service, || service.lane.lock().unwrap().turn_blocked);
    request(
        2,
        source.join("branch-000"),
        StartPurpose::PriorityDuplicates,
        ScanObservation::Strict,
    );
    service.lane.lock().unwrap().turn_gate = None;
    drop(release);
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((2, Phase::Ready, _))
        )
    });
    request(
        3,
        source,
        StartPurpose::BrowseInterrupted(PathBuf::new()),
        size_observation(),
    );
    until(&service, || {
        service
            .output
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|o| matches!(o,Output::Page(p) if p.epoch==3&&!p.busy))
    });
    let output = service.output.lock().unwrap();
    let Some(Output::Page(page)) = output.as_ref() else {
        unreachable!()
    };
    assert_eq!(page.frame.stamp.context.inventory, inventory);
    assert_eq!(page.root_measure.unwrap().files, files);
    assert_eq!(page.frame.coverage, ScopeCoverage::Partial);
    drop(output);
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((3, Phase::Cancelled, _))
    ));
}

#[cfg(any(windows, target_os = "linux"))]
fn single_scan_save_fixture() -> tempfile::TempDir {
    // Honour the same explicit workspace boundary as production, without
    // changing process-global environment or writing into the user's profile.
    match std::env::var_os("BUNNY_AND_SIZE_STORE_DIR") {
        Some(parent) => tempfile::Builder::new()
            .prefix("single-scan-save-test-")
            .tempdir_in(parent)
            .unwrap(),
        None => tempfile::tempdir().unwrap(),
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn single_scan_save_response(service: &Service, id: u64) -> cache::Prepared {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut response = service.cache_output.lock().unwrap();
        if response.as_ref().is_some_and(|response| response.id == id) {
            return response.take().unwrap();
        }
        drop(response);
        assert!(
            Instant::now() < deadline,
            "explicit Save {id} did not terminate"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(windows)]
#[test]
fn single_scan_explicit_save_before_snapshot_commit_retains_once() {
    let temporary = single_scan_save_fixture();
    let source = temporary.path().join("source");
    let history = temporary.path().join("history");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("native-é-file"), [7; 29]).unwrap();
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cache_root = Some(history.clone());
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: ScanObservation::DirectoryListing,
                preflight: None,
                options: Default::default(),
            },
        ));
        // Queue the real explicit retention request before any snapshot exists.
        lane.cache = Some(cache::Request {
            id: 91,
            epoch: 1,
            expected: 1,
            action: cache::Action::Save,
            cancel: Default::default(),
        });
    }
    let response = single_scan_save_response(&service, 91);
    assert!(response.diagnostic.is_empty(), "{}", response.diagnostic);
    let snapshot =
        sortsize_store::direct_snapshot::destination(&cache::snapshot_root(&history), &source);
    let metadata = sortsize_store::direct_snapshot::inspect(&snapshot).unwrap();
    let archives = sortsize_store::inventory::catalogue::list_open(&history, &[]).unwrap();
    assert_eq!(archives.rows.len(), 1);
    let archive = &archives.rows[0];
    assert_eq!(archive.context, metadata.context);
    assert_eq!(archive.root, source);
    let verified = Active::open_saved(1, archive, None, &Default::default()).unwrap();
    assert!(matches!(verified.session, Backend::Certified(_)));
    drop(verified);
    let captured = Mutex::new(None);
    until(&service, || {
        let output = service.output.lock().unwrap();
        if let Some(Output::Page(page)) = output.as_ref()
            && page.epoch == 1
            && page.sort_index == 0
            && !page.busy
            && !page.frame.rows.is_empty()
        {
            *captured.lock().unwrap() = Some((
                page.frame.rows[0].path.clone(),
                page.frame.stamp.context,
                page.frame.rows[0].key,
                page.quanta,
            ));
            true
        } else {
            false
        }
    });
    let (path, context, key, quanta) = captured.into_inner().unwrap().unwrap();
    assert_eq!(path, source.join("native-é-file"));
    let identity = (context, key, quanta);
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 92,
        epoch: 1,
        expected: 1,
        action: cache::Action::Save,
        cancel: Default::default(),
    });
    let response = single_scan_save_response(&service, 92);
    assert!(response.diagnostic.is_empty(), "{}", response.diagnostic);
    let repeated = sortsize_store::inventory::catalogue::list_open(&history, &[]).unwrap();
    assert_eq!(
        repeated.rows.len(),
        1,
        "repeated Save must not append archives"
    );
    assert_eq!(repeated.rows[0].directory, archive.directory);
    assert_eq!(repeated.rows[0].context, metadata.context);
    let observed = Mutex::new(None);
    until(&service, || {
        let output = service.output.lock().unwrap();
        if let Some(Output::Page(page)) = output.as_ref()
            && page.epoch == 1
            && !page.frame.rows.is_empty()
        {
            *observed.lock().unwrap() = Some((
                page.frame.stamp.context,
                page.frame.rows[0].key,
                page.quanta,
            ));
            true
        } else {
            false
        }
    });
    assert_eq!(observed.into_inner().unwrap().unwrap(), identity);

    let other = temporary.path().join("another-source");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("another-file"), [8; 41]).unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 2;
        lane.view_epoch = 2;
        lane.start = Some((
            2,
            Start {
                root: other.clone(),
                explicit_root: true,
                observation: ScanObservation::DirectoryListing,
                preflight: None,
                options: Default::default(),
            },
        ));
        lane.cache = Some(cache::Request {
            id: 93,
            epoch: 2,
            expected: 2,
            action: cache::Action::Save,
            cancel: Default::default(),
        });
    }
    let response = single_scan_save_response(&service, 93);
    assert!(response.diagnostic.is_empty(), "{}", response.diagnostic);
    assert_eq!(
        sortsize_store::inventory::catalogue::list_open(&history, &[])
            .unwrap()
            .rows
            .len(),
        2
    );
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 94,
        epoch: 2,
        expected: 2,
        action: cache::Action::Forget(archive.clone()),
        cancel: Default::default(),
    });
    let response = single_scan_save_response(&service, 94);
    assert!(response.diagnostic.is_empty(), "{}", response.diagnostic);
    let remaining = sortsize_store::inventory::catalogue::list_open(&history, &[]).unwrap();
    assert_eq!(remaining.rows.len(), 1);
    assert_eq!(remaining.rows[0].root, other);
    assert!(
        snapshot.is_file(),
        "the automatic snapshot is a separate object"
    );
    assert_eq!(std::fs::read(&path).unwrap(), [7; 29]);
    let journal = std::fs::read_to_string(history.with_extension("deletions.log")).unwrap();
    assert_eq!(
        journal
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .collect::<Vec<_>>(),
        ["requested", "deleted"]
    );
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_failed_snapshot_explicit_retry_terminates_and_preserves_tree() {
    let temporary = single_scan_save_fixture();
    let source = temporary.path().join("source");
    let history = temporary.path().join("history");
    let blocked = cache::snapshot_root(&history);
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("recorded"), [4; 17]).unwrap();
    std::fs::write(&blocked, b"owned fixture blocks snapshot directory").unwrap();
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cache_root = Some(history.clone());
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    let automatic = single_scan_save_response(&service, 0);
    assert!(!automatic.diagnostic.is_empty());
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((1, Phase::PreviewReady, _))
    ));
    assert!(matches!(
        service.output.lock().unwrap().take(),
        Some(Output::Error(1, _, true))
    ));
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 101,
        epoch: 1,
        expected: 1,
        action: cache::Action::Save,
        cancel: Default::default(),
    });
    let failure = single_scan_save_response(&service, 101);
    assert!(
        !failure.diagnostic.is_empty(),
        "explicit retry must report its own failure"
    );
    assert!(
        service.lane.lock().unwrap().cache.is_none(),
        "failed request must terminate"
    );
    assert_eq!(
        std::fs::read(&blocked).unwrap(),
        b"owned fixture blocks snapshot directory"
    );
    service.output.lock().unwrap().take();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = 2;
        lane.navigation = Some((2, Navigate::Refresh));
    }
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(
        |output| matches!(output, Output::Page(page) if page.epoch == 2 && page.sort_index == 0 && !page.busy)
    )
    });
    let identity = {
        let output = service.output.lock().unwrap();
        let Some(Output::Page(page)) = output.as_ref() else {
            unreachable!()
        };
        assert_eq!(page.root_measure.unwrap().logical, Some(17));
        (
            page.frame.stamp.context,
            page.frame.rows[0].key,
            page.quanta,
        )
    };
    // The source is unavailable: retry must persist the already recorded tree.
    let offline = temporary.path().join("offline-source");
    std::fs::rename(&source, &offline).unwrap();
    // Remove only the fixture blocker, without touching any previous inventory.
    std::fs::remove_file(&blocked).unwrap();
    service.lane.lock().unwrap().cache = Some(cache::Request {
        id: 102,
        epoch: 2,
        expected: 2,
        action: cache::Action::Save,
        cancel: Default::default(),
    });
    let success = single_scan_save_response(&service, 102);
    assert!(success.diagnostic.is_empty(), "{}", success.diagnostic);
    let snapshot = sortsize_store::direct_snapshot::destination(&blocked, &source);
    let acquired = sortsize_store::direct_snapshot::inspect(&snapshot)
        .unwrap()
        .context;
    assert_eq!(
        (
            acquired.inventory,
            acquired.generation,
            acquired.source,
            acquired.scan,
            acquired.validation
        ),
        (
            identity.0.inventory,
            identity.0.generation,
            identity.0.source,
            identity.0.scan,
            identity.0.validation
        ),
    );
    let archives = sortsize_store::inventory::catalogue::list_open(&history, &[]).unwrap();
    assert_eq!(archives.rows.len(), 1);
    assert_eq!(archives.rows[0].context, acquired);
    assert_eq!(archives.rows[0].logical, Some(17));
    let output = service.output.lock().unwrap();
    let Some(Output::Page(page)) = output.as_ref() else {
        unreachable!()
    };
    assert_eq!(
        (
            page.frame.stamp.context,
            page.frame.rows[0].key,
            page.quanta
        ),
        identity
    );
    assert!(!source.exists());
    assert_eq!(std::fs::read(offline.join("recorded")).unwrap(), [4; 17]);
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_explicit_save_cancelled_by_new_source_keeps_completed_snapshot() {
    let temporary = single_scan_save_fixture();
    let source = temporary.path().join("source-a");
    let next_source = temporary.path().join("source-b");
    let history = temporary.path().join("history");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&next_source).unwrap();
    std::fs::write(source.join("a"), [1; 11]).unwrap();
    std::fs::write(next_source.join("b"), [2; 23]).unwrap();
    let (writer_ready, release_writer) = direct::pause_snapshot_writer_for_test().unwrap();
    writer_ready.recv_timeout(Duration::from_secs(30)).unwrap();
    let service = Service::start().unwrap();
    let explicit_cancel = sortsize_core::CancellationToken::default();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cache_root = Some(history.clone());
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
        lane.cache = Some(cache::Request {
            id: 111,
            epoch: 1,
            expected: 1,
            action: cache::Action::Save,
            cancel: explicit_cancel.clone(),
        });
    }
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Saving, _))
        )
    });
    until(&service, || {
        service.output.lock().unwrap().as_ref().is_some_and(|out| {
            matches!(out, Output::Page(page) if page.epoch == 1
                && !page.busy && page.measure_ready
                && page.root_measure.and_then(|m| m.logical) == Some(11))
        })
    });
    assert!(
        !sortsize_store::direct_snapshot::destination(&cache::snapshot_root(&history), &source)
            .exists()
    );
    {
        let mut lane = service.lane.lock().unwrap();
        explicit_cancel.cancel();
        lane.cancel_publication();
        lane.publication_cancel = Default::default();
        lane.source_epoch = 2;
        lane.view_epoch = 2;
        lane.start = Some((
            2,
            Start {
                root: next_source.clone(),
                explicit_root: true,
                observation: size_observation(),
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    until(&service, || {
        service.lane.lock().unwrap().direct_view == Some(2)
    });
    drop(release_writer);
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((2, Phase::Ready, _))
        )
    });
    let snapshot =
        sortsize_store::direct_snapshot::destination(&cache::snapshot_root(&history), &source);
    assert_eq!(
        sortsize_store::direct_snapshot::inspect(&snapshot)
            .unwrap()
            .summary
            .measure
            .logical,
        Some(11)
    );
    assert!(
        sortsize_store::inventory::catalogue::list_open(&history, &[])
            .unwrap()
            .rows
            .is_empty(),
        "cancelled explicit continuation must not create an archive"
    );
    assert!(
        service
            .cache_output
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|response| response.id != 111)
    );
}

#[cfg(windows)]
#[test]
fn single_scan_two_live_publications_defer_the_next_page_without_losing_navigation() {
    let temporary = single_scan_save_fixture();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), [9; 11]).unwrap();
    let service = Service::start().unwrap();
    {
        let mut lane = service.lane.lock().unwrap();
        lane.cache_root = Some(temporary.path().join("history"));
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: source.clone(),
                explicit_root: true,
                observation: ScanObservation::DirectoryListing,
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((1, Phase::Ready, _))
        ) && matches!(service.output.lock().unwrap().as_ref(), Some(Output::Page(page)) if page.epoch == 1 && page.sort_index == 0)
    });
    let first = service.output.lock().unwrap().take().unwrap();
    let request = |epoch| {
        let mut lane = service.lane.lock().unwrap();
        lane.view_epoch = epoch;
        lane.navigation = Some((epoch, Navigate::Refresh));
    };
    request(2);
    until(
        &service,
        || matches!(service.output.lock().unwrap().as_ref(), Some(Output::Page(page)) if page.epoch == 2),
    );
    // A completed unchanged page must stay in the mailbox even after the
    // producer's 200 ms refresh interval; coalescing cannot silently discard it.
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        matches!(service.output.lock().unwrap().as_ref(), Some(Output::Page(page)) if page.epoch == 2)
    );
    // Host::apply temporarily owns both its old current page and the new page
    // taken from the mailbox while it updates Slint models.
    let second = service.output.lock().unwrap().take().unwrap();
    request(3);
    until(&service, || {
        service.lane.lock().unwrap().navigation.is_none()
    });
    for _ in 0..20 {
        assert!(
            service.output.lock().unwrap().is_none(),
            "a third retained page or an admission error escaped while the UI was applying its second page"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(first);
    until(
        &service,
        || matches!(service.output.lock().unwrap().as_ref(), Some(Output::Page(page)) if page.epoch == 3 && page.sort_index == 0),
    );
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((1, Phase::Ready, _))
    ));
    let third = service.output.lock().unwrap().take().unwrap();
    // A replacement DirectActive must inherit the same two-page limit. Its
    // completed snapshot still commits while both old UI pages remain held.
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 4;
        lane.view_epoch = 4;
        lane.publication_cancel = Default::default();
        lane.start = Some((
            4,
            Start {
                root: source,
                explicit_root: true,
                observation: ScanObservation::DirectoryListing,
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    until(&service, || {
        matches!(
            service.lane.lock().unwrap().phase,
            Some((4, Phase::Ready, _))
        )
    });
    assert!(
        service.output.lock().unwrap().is_none(),
        "a replacement view bypassed the two-page limit"
    );
    drop(second);
    until(
        &service,
        || matches!(service.output.lock().unwrap().as_ref(), Some(Output::Page(page)) if page.epoch == 4 && page.sort_index == 0),
    );
    drop(third);
}
