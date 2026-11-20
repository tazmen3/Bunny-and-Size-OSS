//! Régressions ciblées de navigation avec le lecteur réellement bloqué.
//! Les callbacks Slint ne certifient pas les entrées physiques Winit.
use super::*;
use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use std::time::{Duration, Instant};

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

struct Fixture {
    window: SortSizeWindow,
    host: Rc<RefCell<Host>>,
    controller: Rc<RefCell<Controller>>,
    source: PathBuf,
    _temporary: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self::create(false)
    }
    fn create(live: bool) -> Self {
        Self::create_with_backend(live, false)
    }
    fn create_strict(live: bool) -> Self {
        Self::create_with_backend(live, true)
    }
    fn create_with_backend(live: bool, strict: bool) -> Self {
        Self::create_with_entries(live, strict, 384)
    }
    fn create_with_entries(live: bool, strict: bool, entries: usize) -> Self {
        slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
            RepaintBufferType::NewBuffer,
        ))))
        .unwrap();
        let (window, controller) =
            crate::new_session(Default::default(), Some("en".into())).unwrap();
        let host = connect(&window, &controller).unwrap();
        host.borrow().service.lane.lock().unwrap().force_strict = strict;
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        for (name, bytes) in [("alpha", 9), ("bravo", 6), ("charlie", 3)] {
            let folder = source.join(name);
            std::fs::create_dir_all(folder.join("nested")).unwrap();
            std::fs::write(folder.join(format!("{name}-payload")), vec![0; bytes]).unwrap();
            if live || entries != 384 {
                // Live tests span acquisition batches; a completed fixture may
                // also request a specific number of measurable files.
                for index in 0..entries {
                    std::fs::write(folder.join(format!("item-{index:03}")), [0; 1]).unwrap();
                }
            }
        }
        let fixture = Self {
            window,
            host,
            controller,
            source,
            _temporary: temporary,
        };
        fixture.window.invoke_memory_select(1);
        fixture.until(|_| {
            sortsize_core::resources::Governor::global()
                .usage()
                .limits
                .requested
                == MemoryMode::Entries50k
        });
        fixture.window.set_scan_path(
            fixture
                .controller
                .borrow_mut()
                .path_input
                .select(fixture.source.clone())
                .into(),
        );
        if live {
            fixture
                .host
                .borrow()
                .service
                .lane
                .lock()
                .unwrap()
                .tick_delay = Duration::from_millis(100);
        }
        fixture.window.invoke_start_scan();
        fixture.until(|host| {
            host.current.as_ref().is_some_and(|page| {
                if live {
                    page.busy
                } else {
                    !page.busy && page.measure_ready
                }
            }) && (live || {
                let lane = host.service.lane.lock().unwrap();
                lane.reader_active || lane.direct_view == Some(lane.source_epoch)
            }) && fixture
                .window
                .get_rows()
                .iter()
                .any(|row| row.name == "charlie")
        });
        fixture
    }
    fn until(&self, predicate: impl Fn(&Host) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.host
                .borrow_mut()
                .poll(&self.window, &self.controller.borrow());
            assert!(
                self.window.get_errors().is_empty(),
                "{}",
                self.window.get_errors()
            );
            if predicate(&self.host.borrow()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "navigation timed out: {} / {}",
                self.window.get_status(),
                self.window.get_current_path()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn row(&self, name: &str) -> TreeRow {
        self.window
            .get_rows()
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("missing folder {name}"))
    }
    fn select(&self, name: &str) {
        self.window.invoke_select_node(self.row(name).node_id);
    }
    fn loaded(&self, name: &str) {
        self.until(|host| {
            host.current.as_ref().is_some_and(|page| {
                page.epoch == host.epoch && page.path == label(&self.source.join(name))
            }) && self
                .window
                .get_details()
                .iter()
                .any(|row| row.name == format!("{name}-payload"))
        });
    }
    fn block_reader(&self) -> ReaderGate {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let lane = self.host.borrow().service.lane.clone();
        {
            let mut lane = lane.lock().unwrap();
            lane.reader_blocked = false;
            lane.direct_reader_blocked = false;
            lane.reader_gate = Some(gate.clone());
        }
        let release = ReaderGate(gate);
        self.until(|host| {
            let lane = host.service.lane.lock().unwrap();
            if lane.direct_view.is_some() {
                lane.direct_reader_blocked
            } else {
                lane.reader_blocked
            }
        });
        release
    }
}

#[test]
fn reselecting_same_folder_restores_the_cleared_treemap_model() {
    let fixture = Fixture::create_with_entries(false, false, 19);
    fixture.select("alpha");
    fixture.loaded("alpha");
    fixture.until(|host| {
        host.current
            .as_ref()
            .is_some_and(|page| page.epoch == host.epoch && page.sort_index == 0)
    });
    let before = fixture
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .tiles
        .clone();
    assert_eq!(fixture.window.get_tiles().row_count(), 20);
    assert_eq!(
        fixture.window.get_tiles().iter().collect::<Vec<_>>(),
        before
    );
    let previous_epoch = fixture.host.borrow().epoch;
    fixture.select("alpha");
    assert!(fixture.host.borrow().epoch > previous_epoch);
    assert_eq!(fixture.window.get_tiles().row_count(), 0);
    assert_eq!(
        fixture.host.borrow().current.as_ref().unwrap().tiles,
        before
    );
    fixture.loaded("alpha");
    fixture.until(|host| {
        host.current
            .as_ref()
            .is_some_and(|page| page.epoch == host.epoch && page.sort_index == 0)
    });
    assert_eq!(
        fixture.host.borrow().current.as_ref().unwrap().tiles,
        before
    );
    assert_eq!(fixture.window.get_tiles().row_count(), 20);
    assert_eq!(
        fixture.window.get_tiles().iter().collect::<Vec<_>>(),
        before
    );
}

// Release even when an assertion fails, so no worker retains a blocked fixture.
struct ReaderGate(Arc<(Mutex<bool>, std::sync::Condvar)>);
impl Drop for ReaderGate {
    fn drop(&mut self) {
        let (lock, ready) = &*self.0;
        *lock.lock().unwrap() = true;
        ready.notify_all();
    }
}

#[test]
fn strict_cancellation_frozen_unpublished_preview_does_not_wait_for_navigation() {
    let fixture = Fixture::create_strict(true);
    let initial_path = fixture.window.get_current_path();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    {
        let host = fixture.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.tick_gate = Some(gate.clone());
        lane.tick_blocked = false;
    }
    let release = ReaderGate(gate);
    fixture.until(|host| host.service.lane.lock().unwrap().tick_blocked);
    // This accepted request is still waiting when Cancel arrives.
    fixture.select("alpha");
    assert!(fixture.host.borrow().pending_folder.is_some());
    let initial_epoch = fixture.host.borrow().epoch;
    fixture.window.invoke_cancel_scan();
    {
        let host = fixture.host.borrow();
        let lane = host.service.lane.lock().unwrap();
        assert!(lane.navigation.is_none());
        assert!(lane.desired_folder.is_none());
        assert!(lane.desired_relative.is_none());
        assert_eq!(lane.view_epoch, initial_epoch);
    }
    // The click owns the cancellation marker synchronously. Do not clear the
    // snapshot for navigation while the acquisition still drains either.
    let pending = fixture.host.borrow().pending_folder;
    fixture.select("bravo");
    assert_eq!(fixture.host.borrow().pending_folder, pending);
    assert_eq!(fixture.host.borrow().epoch, initial_epoch);
    {
        fixture.host.borrow().service.lane.lock().unwrap().tick_gate = None;
    }
    drop(release);
    fixture.until(|host| {
        let lane = host.service.lane.lock().unwrap();
        lane.cancelled_unpublished == Some(lane.source_epoch)
            && !lane.cancellation_pending()
            && !fixture.window.get_busy()
            && fixture.window.get_status().contains("not published")
    });
    assert_eq!(fixture.window.get_current_path(), initial_path);
    assert!(fixture.host.borrow().pending_folder.is_none());
    assert!(!fixture.window.get_busy());
    assert!(fixture.window.get_status().contains("not published"));
    let path = fixture.window.get_current_path();
    let details = fixture.window.get_details();
    let epoch = fixture.host.borrow().epoch;
    fixture.select("alpha");
    fixture.host.borrow_mut().navigate(Navigate::Refresh);
    assert!(fixture.host.borrow().pending_folder.is_none());
    assert_eq!(fixture.host.borrow().epoch, epoch);
    assert_eq!(fixture.window.get_current_path(), path);
    assert_eq!(fixture.window.get_details(), details);
    assert!(
        fixture
            .host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .navigation
            .is_none()
    );
    fixture
        .host
        .borrow()
        .service
        .lane
        .lock()
        .unwrap()
        .tick_delay = Duration::ZERO;
    fixture.window.invoke_start_scan();
    fixture.until(|host| {
        host.current
            .as_ref()
            .is_some_and(|page| !page.busy && page.measure_ready)
            && host.service.lane.lock().unwrap().reader_active
    });
    fixture.select("alpha");
    fixture.loaded("alpha");
}

#[test]
fn navigation_rapid_selections_remain_interactive_while_reader_is_blocked() {
    let fixture = Fixture::new();
    let generation = fixture
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context
        .generation;
    let epoch = fixture.host.borrow().epoch;
    let gate = fixture.block_reader();
    let callbacks = Instant::now();
    for name in ["alpha", "bravo", "charlie"] {
        let callback = Instant::now();
        fixture.select(name);
        println!(
            "navigation callback folder={name} elapsed_us={} reader_blocked=true",
            callback.elapsed().as_micros()
        );
        assert!(
            fixture.row(name).selected,
            "selection must be visible before a reader reply"
        );
        assert_eq!(
            fixture.window.get_current_path().as_str(),
            label(&fixture.source.join(name))
        );
        assert_eq!(
            fixture.window.get_details().row_count(),
            0,
            "the former folder must not masquerade as the requested folder"
        );
        assert_eq!(fixture.window.get_tiles().row_count(), 0);
        assert!(!fixture.window.get_empty_message().is_empty());
    }
    println!(
        "navigation three_callbacks_us={} generation={}",
        callbacks.elapsed().as_micros(),
        generation.0
    );
    assert_eq!(
        fixture.host.borrow().epoch,
        epoch + 3,
        "every click must be accepted without polling"
    );
    {
        let host = fixture.host.borrow();
        let lane = host.service.lane.lock().unwrap();
        assert!(if lane.direct_view.is_some() {
            lane.direct_reader_blocked
        } else {
            lane.reader_blocked
        });
    }
    drop(gate);
    fixture.loaded("charlie");
    assert_eq!(
        fixture
            .host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .generation,
        generation
    );
}

#[cfg(windows)]
#[test]
fn single_scan_cancelled_partial_tree_remains_navigable_without_restart() {
    let fixture = Fixture::create(true);
    fixture.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.busy
                && page.sort_index < 0
                && page.root_measure.is_some_and(|measure| measure.files > 0)
        })
    });
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    {
        let host = fixture.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.tick_gate = Some(gate.clone());
        lane.tick_blocked = false;
    }
    let release = ReaderGate(gate);
    fixture.until(|host| host.service.lane.lock().unwrap().tick_blocked);
    fixture.window.invoke_cancel_scan();
    drop(release);
    fixture.until(|host| {
        let lane = host.service.lane.lock().unwrap();
        !lane.cancellation_pending()
            && host.current.as_ref().is_some_and(|page| {
                page.epoch == host.epoch
                    && !page.busy
                    && page.sort_index >= 0
                    && page.frame.coverage == ScopeCoverage::Partial
            })
    });
    let (inventory, files, source_epoch) = {
        let host = fixture.host.borrow();
        let page = host.current.as_ref().unwrap();
        (
            page.frame.stamp.context.inventory,
            page.root_measure.unwrap().files,
            host.service.lane.lock().unwrap().source_epoch,
        )
    };
    assert!(
        files > 0 && files < 3 * 385,
        "the fixture must stop mid-acquisition"
    );
    for name in ["alpha", "bravo", "charlie"] {
        fixture.select(name);
        fixture.until(|host| {
            host.current.as_ref().is_some_and(|page| {
                page.epoch == host.epoch
                    && page.path == label(&fixture.source.join(name))
                    && !page.busy
            })
        });
        let host = fixture.host.borrow();
        let page = host.current.as_ref().unwrap();
        assert_eq!(page.frame.stamp.context.inventory, inventory);
        assert_eq!(page.root_measure.unwrap().files, files);
        assert_eq!(page.frame.coverage, ScopeCoverage::Partial);
        let lane = host.service.lane.lock().unwrap();
        assert_eq!(lane.source_epoch, source_epoch);
        assert!(!lane.acquisition_in_progress());
    }
}

#[test]
fn navigation_stale_frame_cannot_replace_the_latest_requested_folder() {
    let fixture = Fixture::new();
    let gate = fixture.block_reader();
    fixture.select("alpha");
    fixture.select("bravo");
    let stale = fixture.host.borrow_mut().current.take().unwrap();
    let old_epoch = stale.epoch;
    assert!(old_epoch < fixture.host.borrow().epoch);
    *fixture.host.borrow().service.output.lock().unwrap() = Some(Output::Page(stale));
    fixture
        .host
        .borrow_mut()
        .poll(&fixture.window, &fixture.controller.borrow());
    assert_eq!(
        fixture.window.get_current_path().as_str(),
        label(&fixture.source.join("bravo"))
    );
    assert_eq!(fixture.window.get_details().row_count(), 0);
    assert_eq!(fixture.window.get_tiles().row_count(), 0);
    assert!(fixture.host.borrow().pending.is_none());
    drop(gate);
    fixture.loaded("bravo");
}

#[test]
fn navigation_tree_chevron_during_loading_preserves_the_folder_request() {
    let fixture = Fixture::new();
    let gate = fixture.block_reader();
    fixture.select("alpha");
    fixture
        .window
        .invoke_toggle_node(fixture.row("bravo").node_id);
    assert!(fixture.row("alpha").selected);
    assert_eq!(
        fixture.window.get_current_path().as_str(),
        label(&fixture.source.join("alpha"))
    );
    drop(gate);
    fixture.loaded("alpha");
    fixture.until(|_| fixture.row("bravo").expanded);
    assert!(fixture.row("alpha").selected);
}

#[test]
fn navigation_return_to_an_inventoried_folder_reuses_the_same_generation() {
    let fixture = Fixture::new();
    let (generation, quanta) = {
        let host = fixture.host.borrow();
        let page = host.current.as_ref().unwrap();
        (page.frame.stamp.context.generation, page.quanta)
    };
    for name in ["alpha", "bravo", "alpha"] {
        fixture.select(name);
        fixture.loaded(name);
        let host = fixture.host.borrow();
        let page = host.current.as_ref().unwrap();
        assert_eq!(page.frame.stamp.context.generation, generation);
        assert_eq!(
            page.quanta, quanta,
            "navigation must not restart acquisition"
        );
        assert!(!page.frame.provisional);
        assert!(fixture.row(name).selected);
    }
}

#[test]
fn navigation_active_acquisition_accepts_clicks_and_enriches_the_selected_scope() {
    // Each directory spans four native 512-entry batches. A 384-entry
    // directory fits in one batch and cannot exhibit incremental enrichment.
    let fixture = Fixture::create_with_entries(true, false, 2048);
    let generation = fixture
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context
        .generation;
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    {
        let host = fixture.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.tick_blocked = false;
        lane.tick_gate = Some(gate.clone());
        lane.tick_delay = Duration::from_millis(250);
    }
    let release = ReaderGate(gate);
    fixture.until(|host| host.service.lane.lock().unwrap().tick_blocked);
    // The filesystem/service owner is stopped at a committed quantum boundary.
    // Returning with a visible selection proves the callback did not wait for it.
    fixture.select("alpha");
    fixture.select("bravo");
    fixture.select("charlie");
    assert!(fixture.row("charlie").selected);
    assert_eq!(
        fixture.window.get_current_path().as_str(),
        label(&fixture.source.join("charlie"))
    );
    assert_eq!(fixture.window.get_details().row_count(), 0);
    drop(release);
    fixture.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.epoch == host.epoch
                && page.path == label(&fixture.source.join("charlie"))
                && page.busy
                && !page.details.is_empty()
        })
    });
    let first_count = fixture.window.get_details().row_count();
    let (first_summary, first_revision) = {
        let host = fixture.host.borrow();
        let page = host.current.as_ref().unwrap();
        (
            page.current_folder_summary.clone(),
            page.frame.stamp.revision,
        )
    };
    println!(
        "navigation live first_rows={first_count} generation={} acquisition_active=true",
        generation.0
    );
    assert!(fixture.window.get_tiles().row_count() > 0);
    fixture.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.epoch == host.epoch
                && page.path == label(&fixture.source.join("charlie"))
                && page.frame.stamp.revision > first_revision
                && page.current_folder_summary != first_summary
                && !page.details.is_empty()
                && page.busy
        })
    });
    println!(
        "navigation live enriched_rows={} generation={} acquisition_active=true",
        fixture.window.get_details().row_count(),
        generation.0
    );
    assert_eq!(
        fixture
            .host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .generation,
        generation
    );
}

#[test]
fn navigation_tree_reordering_waits_for_interaction_without_losing_focus_or_scroll() {
    let fixture = Fixture::new();
    fixture.select("alpha");
    fixture.loaded("alpha");
    let gate = fixture.block_reader();
    *fixture.host.borrow().service.output.lock().unwrap() = None;
    fixture.host.borrow_mut().navigate(Navigate::Refresh);
    let epoch = fixture.host.borrow().epoch;
    drop(gate);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut page = loop {
        let output = fixture.host.borrow().service.output.lock().unwrap().take();
        if let Some(Output::Page(page)) = output
            && page.epoch == epoch
        {
            break page;
        }
        assert!(
            Instant::now() < deadline,
            "reader did not produce the requested refresh"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    let paths = || {
        fixture
            .window
            .get_rows()
            .iter()
            .map(|row| row.path)
            .collect::<Vec<_>>()
    };
    let old_paths = paths();
    let model = fixture.window.get_rows();
    let focused = fixture.host.borrow().tree_focused.clone();
    let selected = fixture
        .window
        .get_rows()
        .iter()
        .find(|row| row.selected)
        .unwrap()
        .path;
    let scroll = fixture.window.get_sidebar_folder_scroll();
    assert!(page.folders.len() >= 4);
    page.folders.swap(2, 3);
    page.folder_keys.swap(2, 3);
    for (index, row) in page.folders.iter_mut().enumerate() {
        row.node_id = index as i32;
    }
    let new_paths = page
        .folders
        .iter()
        .map(|row| row.path.clone())
        .collect::<Vec<_>>();
    {
        let mut host = fixture.host.borrow_mut();
        host.tree_quiet_until = Some(Instant::now() + Duration::from_secs(60));
        host.apply(&fixture.window, page);
        assert!(host.deferred_tree.is_some());
    }
    assert_eq!(
        paths(),
        old_paths,
        "a numeric reorder must wait during interaction"
    );
    assert_eq!(fixture.window.get_rows(), model);
    assert_eq!(fixture.window.get_sidebar_folder_scroll(), scroll);
    fixture.host.borrow_mut().tree_quiet_until = Some(Instant::now() - Duration::from_millis(1));
    fixture
        .host
        .borrow_mut()
        .poll(&fixture.window, &fixture.controller.borrow());
    assert_eq!(paths(), new_paths);
    assert_eq!(fixture.window.get_rows(), model);
    assert_eq!(fixture.host.borrow().tree_focused, focused);
    assert_eq!(
        fixture
            .window
            .get_rows()
            .iter()
            .find(|row| row.selected)
            .unwrap()
            .path,
        selected
    );
    assert_eq!(fixture.window.get_sidebar_folder_scroll(), scroll);
}

#[test]
fn navigation_parent_replaces_a_pending_folder_request_without_changing_generation() {
    let fixture = Fixture::new();
    let generation = fixture
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context
        .generation;
    let gate = fixture.block_reader();
    fixture.select("alpha");
    assert!(fixture.host.borrow().pending_folder.is_some());
    fixture.window.invoke_go_parent();
    assert!(
        fixture.host.borrow().pending_folder.is_none(),
        "Parent must replace the pending Open guard"
    );
    let epoch = fixture.host.borrow().epoch;
    drop(gate);
    fixture.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.epoch == epoch
                && page.parent_entry == EntryId(0)
                && page.path == label(&fixture.source)
        })
    });
    let host = fixture.host.borrow();
    assert_eq!(
        host.current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .generation,
        generation
    );
    assert_eq!(
        fixture.window.get_current_path().as_str(),
        label(&fixture.source)
    );
    assert!(fixture.window.get_rows().row_data(0).unwrap().selected);
}

#[test]
fn strict_navigation_selected_g0_folder_survives_g1_publication_with_new_entry_ids() {
    let fixture = Fixture::create_strict(false);
    let old_key = {
        let host = fixture.host.borrow();
        let page = host.current.as_ref().unwrap();
        let index = page
            .folders
            .iter()
            .position(|row| row.name == "alpha")
            .unwrap();
        let folder_tree::TreeTarget::Folder { key, .. } = page.folder_keys[index] else {
            unreachable!()
        };
        key
    };
    assert_eq!(old_key.generation, GenerationId(1));
    // Controlled Windows fixture: an earlier native sibling shifts the new
    // generation's IDs. Only this temporary corpus is modified.
    std::fs::create_dir(fixture.source.join("000-before-alpha")).unwrap();
    std::fs::write(fixture.source.join("alpha/new-g1-marker"), [1; 13]).unwrap();
    fixture
        .host
        .borrow()
        .service
        .lane
        .lock()
        .unwrap()
        .hold_generation = Some(GenerationId(2));
    fixture.window.invoke_start_scan();
    fixture.until(|host| {
        let lane = host.service.lane.lock().unwrap();
        lane.writer_generation == Some(GenerationId(2))
            && lane.writer_entries > 1
            && host.current.as_ref().is_some_and(|page| {
                page.busy && page.frame.stamp.context.generation == GenerationId(1)
            })
            && fixture
                .window
                .get_rows()
                .iter()
                .any(|row| row.name == "alpha")
    });
    fixture.select("alpha");
    fixture.loaded("alpha");
    assert_eq!(
        fixture
            .host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .generation,
        GenerationId(1)
    );
    assert!(
        fixture
            .window
            .get_details()
            .iter()
            .all(|row| row.name != "new-g1-marker"),
        "G0 must remain an exact frozen view"
    );
    fixture
        .host
        .borrow()
        .service
        .lane
        .lock()
        .unwrap()
        .hold_generation = None;
    fixture.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.epoch == host.epoch
                && !page.busy
                && page.frame.stamp.context.generation == GenerationId(2)
                && page.path == label(&fixture.source.join("alpha"))
        })
    });
    let host = fixture.host.borrow();
    let page = host.current.as_ref().unwrap();
    assert_eq!(page.frame.stamp.context.inventory, old_key.inventory);
    assert_ne!(
        page.parent_entry, old_key.entry,
        "the fixture must exercise native-path remapping, not coincidentally equal IDs"
    );
    assert!(
        fixture
            .window
            .get_details()
            .iter()
            .any(|row| row.name == "new-g1-marker")
    );
    assert!(fixture.row("alpha").selected);
    println!(
        "navigation G0_to_G1 old_entry={} new_entry={} selected_scope=alpha exact_marker=true",
        old_key.entry.0, page.parent_entry.0
    );
}
