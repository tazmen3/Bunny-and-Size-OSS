//! P1-01: exercise automatic intentions through the production Atelier bridge.
//! Every scan uses a temporary fixture; discovery is deterministic and local.
use super::*;
use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use sortsize_core::{MetadataValue, volumes::*};
use std::time::{Duration, Instant};

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn discovery(paths: &[PathBuf]) -> VolumeDiscovery {
    VolumeDiscovery {
        volumes: paths
            .iter()
            .map(|path| Volume {
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
                availability: VolumeAvailability::Available,
            })
            .collect(),
        enumeration_time: Duration::ZERO,
        capacity_time: Duration::ZERO,
        capacity_calls: 0,
    }
}

fn inject(controller: &Rc<RefCell<Controller>>, discovery: VolumeDiscovery, ready: bool) {
    let volumes = discovery.volumes.clone();
    let mut sources = crate::volume_view::Sources::with_provider(Arc::new(move |options| {
        assert!(!options.probe_remote);
        Ok(VolumeDiscovery {
            volumes: volumes.clone(),
            enumeration_time: Duration::ZERO,
            capacity_time: Duration::ZERO,
            capacity_calls: 0,
        })
    }));
    if ready {
        sources.current = Some(crate::volume_view::Prepared::new(discovery));
    }
    controller.borrow_mut().sources = sources;
}

struct Fixture {
    window: SortSizeWindow,
    controller: Rc<RefCell<Controller>>,
    host: Rc<RefCell<Host>>,
    source: PathBuf,
    other: PathBuf,
    _temporary: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
            RepaintBufferType::NewBuffer,
        ))))
        .unwrap();
        let (window, controller) =
            crate::new_session(Default::default(), Some("en".into())).unwrap();
        let host = connect(&window, &controller).unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let other = temporary.path().join("other");
        std::fs::create_dir_all(source.join("child")).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(source.join("child/nested"), [1; 41]).unwrap();
        std::fs::write(source.join("root-only"), [2; 7]).unwrap();
        std::fs::write(other.join("other-only"), [3; 17]).unwrap();
        inject(
            &controller,
            discovery(&[source.clone(), other.clone()]),
            true,
        );
        controller.borrow().show_sources(&window);
        let fixture = Self {
            window,
            controller,
            host,
            source,
            other,
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
        fixture
    }
    fn poll(&self) {
        self.controller.borrow_mut().poll(&self.window);
        self.host
            .borrow_mut()
            .poll_selection(&self.window, &mut self.controller.borrow_mut());
        self.host
            .borrow_mut()
            .poll(&self.window, &self.controller.borrow());
    }
    fn strict() -> Self {
        let fixture = Self::new();
        fixture
            .host
            .borrow_mut()
            .duplicate
            .force_duplicate_inventory = true;
        fixture
            .host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .force_strict = true;
        fixture
    }
    fn until(&self, predicate: impl Fn(&Host) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.poll();
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
                "P1-01 timeout: {}",
                self.window.get_status()
            );
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    fn complete(&self) {
        self.until(|host| {
            host.current
                .as_ref()
                .is_some_and(|page| !page.busy && page.measure_ready)
        });
    }
    fn source_epoch(&self) -> u64 {
        self.host.borrow().service.lane.lock().unwrap().source_epoch
    }
}

// Rebinding a retained tree advances presentation/view epochs, but must keep
// every recorded identity (including inventory, source, scan and validation).
fn assert_same_retained_context(
    mut actual: sortsize_core::inventory_contract::ViewContext,
    expected: sortsize_core::inventory_contract::ViewContext,
) {
    actual.epoch = expected.epoch;
    actual.presentation = expected.presentation;
    assert_eq!(
        actual, expected,
        "a replacement changed the retained inventory"
    );
}

struct TickGate(Arc<(Mutex<bool>, std::sync::Condvar)>);
impl Drop for TickGate {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

#[test]
fn auto_analysis_local_selection_navigation_cancel_and_manual_restart() {
    let f = Fixture::new();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    assert!(f.window.get_busy());
    let epoch = f.source_epoch();
    for _ in 0..100 {
        f.window
            .invoke_workshop_source(f.window.get_storage_token(), 0);
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        epoch,
        "duplicate selection started a second scan"
    );
    f.complete();
    assert!(f.host.borrow().current.as_ref().unwrap().measure_ready);
    let child = f
        .window
        .get_details()
        .iter()
        .find(|row| row.name == "child")
        .unwrap();
    f.window
        .invoke_workshop_action(f.window.get_workshop_token(), child.node_id, true);
    f.until(|_| {
        f.window.get_details().row_count() == 1
            && f.window
                .get_details()
                .row_data(0)
                .is_some_and(|row| row.name == "nested")
    });
    f.window.invoke_go_parent();
    f.until(|_| f.window.get_details().row_count() == 2);
    assert_eq!(
        f.source_epoch(),
        epoch,
        "folder navigation restarted acquisition"
    );
    f.window
        .set_scan_path(f.other.to_string_lossy().as_ref().into());
    f.window.invoke_path_edited();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(
        f.window.get_scan_path().as_str(),
        f.source.to_string_lossy()
    );
    assert_eq!(
        f.source_epoch(),
        epoch,
        "completed selection restarted acquisition"
    );

    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    {
        let host = f.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.tick_gate = Some(gate.0.clone());
        lane.tick_blocked = false;
    }
    f.window.invoke_start_scan();
    let manual_epoch = f.source_epoch();
    assert!(manual_epoch > epoch, "manual refresh must remain available");
    f.until(|host| host.service.lane.lock().unwrap().tick_blocked);
    f.window.invoke_cancel_scan();
    drop(gate);
    f.until(|host| {
        let lane = host.service.lane.lock().unwrap();
        !f.window.get_busy()
            && !lane.writer_running
            && lane
                .phase
                .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Cancelled))
    });
    assert!(f.window.get_status().to_lowercase().contains("cancel"));
    for _ in 0..100 {
        f.window
            .invoke_workshop_source(f.window.get_storage_token(), 0);
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        manual_epoch,
        "cancelled intention restarted automatically"
    );
    f.window.invoke_start_scan();
    assert!(f.source_epoch() > manual_epoch);
    f.complete();

    // A second full cycle must release the prior candidate's physical admission.
    let protected = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    {
        let host = f.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.tick_gate = Some(gate.0.clone());
        lane.tick_blocked = false;
    }
    f.window.invoke_start_scan();
    f.until(|host| host.service.lane.lock().unwrap().tick_blocked);
    f.window.invoke_cancel_scan();
    drop(gate);
    f.until(|host| {
        let lane = host.service.lane.lock().unwrap();
        !f.window.get_busy()
            && !lane.writer_running
            && lane
                .phase
                .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Cancelled))
    });
    f.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.frame.stamp.context.inventory == protected.inventory
                && page.frame.stamp.context.generation == protected.generation
        })
    });
    assert_same_retained_context(
        f.host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context,
        protected,
    );
    f.window.invoke_start_scan();
    f.complete();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 1);
    f.complete();
    assert_eq!(f.window.get_details().row_count(), 1);
    assert_eq!(
        f.window.get_details().row_data(0).unwrap().name,
        "other-only"
    );
}

#[test]
fn folder_picker_is_passive_and_cancel_preserves_inventory() {
    let f = Fixture::new();
    let folder = f.source.join("child");
    let picked = folder.clone();
    f.controller.borrow_mut().begin_browse(&f.window, |_| {
        crate::folder_picker::FolderPicker::start(move || Some(picked))
    });
    assert!(f.window.get_choosing_folder());
    f.until(|_| !f.window.get_choosing_folder());
    assert_eq!(f.source_epoch(), 0, "folder choice must not scan");
    f.window.invoke_start_scan();
    f.complete();
    assert_eq!(f.window.get_details().row_data(0).unwrap().name, "nested");
    let epoch = f.source_epoch();
    f.controller.borrow_mut().begin_browse(&f.window, |_| {
        crate::folder_picker::FolderPicker::start(|| None)
    });
    f.until(|_| !f.window.get_choosing_folder());
    assert_eq!(f.source_epoch(), epoch);
    assert_eq!(f.window.get_details().row_data(0).unwrap().name, "nested");
    let picked = folder.clone();
    f.controller.borrow_mut().begin_browse(&f.window, |_| {
        crate::folder_picker::FolderPicker::start(move || Some(picked))
    });
    f.until(|_| !f.window.get_choosing_folder());
    assert_eq!(
        f.source_epoch(),
        epoch,
        "same folder pick restarted acquisition"
    );
    // Editing text is not a committed folder selection.
    f.window
        .set_scan_path(f.other.to_string_lossy().as_ref().into());
    f.window.invoke_path_edited();
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), epoch);
    assert_eq!(f.window.get_details().row_data(0).unwrap().name, "nested");
}

#[test]
fn auto_analysis_startup_requires_explicit_local_available_matching_selection() {
    use crate::auto_analysis::LastSelection;
    let f = Fixture::new();
    // The startup path field (for example current_dir) is not stored consent.
    f.window
        .set_scan_path(f.source.to_string_lossy().as_ref().into());
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), 0);
    assert!(!f.window.get_busy());

    let local = discovery(&[f.source.clone(), f.other.clone()]);
    let selection = LastSelection::local(f.source.clone(), &local).unwrap();
    let mut remote = discovery(&[f.source.clone(), f.other.clone()]);
    remote.volumes[0].kind = SourceKind::Remote;
    inject(&f.controller, remote, true);
    f.controller.borrow_mut().restored_selection = Some(selection.clone());
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), 0, "a NAS must not scan on startup");

    let mut inaccessible = discovery(&[f.source.clone(), f.other.clone()]);
    inaccessible.volumes[0].availability = VolumeAvailability::Unavailable;
    inject(&f.controller, inaccessible, true);
    f.controller.borrow_mut().restored_selection = Some(selection.clone());
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), 0, "an unavailable source must not start");

    let mut replacement = discovery(&[f.source.clone(), f.other.clone()]);
    replacement.volumes[0].identity = VolumeIdentity::WindowsGuid("other-device".into());
    inject(&f.controller, replacement, true);
    f.controller.borrow_mut().restored_selection = Some(selection.clone());
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        0,
        "a new device at the same path is not stored consent"
    );

    // Independent successful startup follows the deliberate negative scenarios.
    f.window.set_errors("".into());
    // Simulate real asynchronous discovery: no inventory/source exists yet.
    inject(&f.controller, local, false);
    f.controller.borrow_mut().restored_selection = Some(selection);
    f.poll();
    assert_eq!(f.source_epoch(), 0);
    f.controller.borrow_mut().refresh_sources(&f.window, false);
    f.until(|_| !f.window.get_storage_loading());
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        0,
        "restoration must not start acquisition"
    );
    assert!(!f.window.get_busy());
    assert_eq!(f.window.get_selected_volume(), 0);
    f.window.set_selected_volume(0);
    f.window.invoke_select_volume(0);
    f.window
        .invoke_workshop_source(f.window.get_storage_token() - 1, 0);
    f.window.invoke_workshop_retry();
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        0,
        "passive selection or stale activation started acquisition"
    );
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    assert_eq!(f.window.get_scan_source().as_str(), label(&f.source));
    assert_eq!(f.window.get_details().row_count(), 2);
    assert!(
        !f.window
            .get_details()
            .iter()
            .any(|row| row.name == "other-only")
    );
    let epoch = f.source_epoch();
    for _ in 0..3 {
        f.controller.borrow_mut().refresh_sources(&f.window, false);
        f.until(|_| !f.window.get_storage_loading());
    }
    assert_eq!(
        f.source_epoch(),
        epoch,
        "discovery refresh restarted the stored intention"
    );
}

#[test]
fn auto_analysis_restored_parent_cannot_replace_explicit_ab_folder() {
    use crate::auto_analysis::LastSelection;
    let f = Fixture::new();
    f.window.invoke_duplicate_mode_changed(1);
    f.until(|_| {
        f.window
            .get_duplicate_source_labels()
            .iter()
            .any(|name| name == label(&f.source))
    });
    let index = f
        .window
        .get_duplicate_source_labels()
        .iter()
        .position(|name| name == label(&f.source))
        .unwrap();
    f.window.invoke_duplicate_select(0, index as i32);
    let folder = f.source.join("child");
    f.window.set_duplicate_scope_a(label(&folder).into());
    f.window.invoke_duplicate_scope_edited(0);
    assert!(f.window.get_duplicate_scope_a_valid());

    // The saved parent intention has not been consumed yet, as when native
    // source discovery finishes after the user prepares the A/B panel.
    let saved = LastSelection::local(
        f.source.clone(),
        &discovery(&[f.source.clone(), f.other.clone()]),
    )
    .unwrap();
    f.controller.borrow_mut().restored_selection = Some(saved);
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    f.host.borrow().service.lane.lock().unwrap().tick_gate = Some(gate.0.clone());
    f.window.invoke_duplicate_inventory(0);
    let epoch = f.source_epoch();
    assert!(f.window.get_busy());
    assert_eq!(f.host.borrow().requested.as_ref(), Some(&folder));

    // Run the real controller/Atelier poll order, including delayed restore.
    f.until(|host| host.service.lane.lock().unwrap().tick_blocked);
    assert_eq!(
        f.host.borrow().requested.as_ref(),
        Some(&folder),
        "a saved parent must not supersede the accepted exact-folder scan"
    );
    assert_eq!(f.source_epoch(), epoch);
    assert!(f.controller.borrow().restored_selection.is_none());
    assert!(f.controller.borrow().pending_auto.is_none());
    f.window.invoke_cancel_scan();
    drop(gate);
    f.until(|host| !host.service.lane.lock().unwrap().writer_running && !f.window.get_busy());
    for _ in 0..40 {
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        epoch,
        "cancellation must not revive the parent"
    );
    assert_eq!(f.window.get_duplicate_scope_a().as_str(), label(&folder));
    eprintln!(
        "ab_delayed_restore explicit_folder={} restored_parent={} epoch={} parent_restarted=false cancellation_terminal=true",
        label(&folder),
        label(&f.source),
        epoch
    );
}

#[test]
fn single_duplicate_search_uses_the_chosen_tree_and_ignores_search_filters() {
    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    let chosen = f.source.join("chosen");
    std::fs::create_dir_all(chosen.join("first")).unwrap();
    std::fs::create_dir(chosen.join("second")).unwrap();
    std::fs::write(chosen.join("copy.bin"), b"chosen pair").unwrap();
    std::fs::write(chosen.join("first/different.bin"), b"abcd").unwrap();
    std::fs::write(chosen.join("second/different.bin"), b"efgh").unwrap();
    std::fs::write(f.source.join("outside.bin"), b"chosen pair").unwrap();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    std::fs::write(chosen.join("second/copy.bin"), b"chosen pair").unwrap();
    let previous = f.source_epoch();
    f.window.set_search_name("matches nothing".into());
    f.window.set_search_glob("*.never".into());
    f.window.set_search_min("999".into());
    f.window.set_duplicate_protected_reference(0);
    f.window.invoke_duplicate_mode_changed(3);
    f.window.invoke_show_analysis();
    f.window.set_duplicate_single_root(label(&chosen).into());
    f.window.invoke_duplicate_scope_edited(3);
    f.poll();
    assert_eq!(f.source_epoch(), previous);
    assert!(
        f.host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .duplicate_operations
            .is_empty()
    );
    assert!(f.window.get_duplicate_ready());

    let writer_entries = f.host.borrow().service.lane.lock().unwrap().writer_entries;
    f.window.invoke_duplicate_run();
    assert_eq!(
        f.window.get_duplicate_summary(),
        "Discovering and checking files…"
    );
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert_eq!(f.source_epoch(), previous);
    assert_eq!(
        f.host.borrow().requested.as_deref(),
        Some(f.source.as_path())
    );
    assert_eq!(
        f.host.borrow().service.lane.lock().unwrap().writer_entries,
        writer_entries
    );
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert!(
        f.window.get_search_complete(),
        "{} / {}",
        f.window.get_search_status(),
        f.window.get_duplicate_warning()
    );
    assert_eq!(f.window.get_duplicate_folder_count(), "0");
    assert_eq!(f.window.get_duplicate_file_count(), "1");
    // Two hashes and one binary comparison of the 11-byte pair, and
    // two hashes for the 4-byte mismatches. The outside copy was never read.
    let read_details = "Content bytes read (hashing and comparison): 52.";
    assert!(
        f.window.get_search_status().contains(read_details),
        "{}",
        f.window.get_search_status()
    );
    let rows = f.window.get_duplicate_rows().iter().collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.name, "copy.bin");
        assert_eq!(row.group, rows[0].group);
        assert!(row.side.is_empty());
        assert_eq!(row.verification, "Verified exact");
        assert!(!row.protected && !row.selectable && !row.selected);
    }
    for parent in [&chosen, &chosen.join("second")] {
        let index = rows
            .iter()
            .position(|row| row.location == label(parent).as_str())
            .expect("absolute parent folder in location column");
        f.window.invoke_search_select(0, index as i32, false);
        assert_eq!(
            f.window.get_search_full_path(),
            label(&parent.join("copy.bin"))
        );
    }
    f.window.invoke_duplicate_mode_changed(0);
    assert_eq!(f.window.get_search_name(), "matches nothing");
    assert_eq!(f.window.get_search_glob(), "*.never");
    assert_eq!(f.window.get_search_min(), "999");
    assert_eq!(f.window.get_duplicate_protected_reference(), 0);
}

#[test]
fn tree_duplicate_command_relaunches_zero_groups_after_a_view_publication() {
    let f = Fixture::new();
    let child = f.source.join("child");
    let first = child.join("large-a");
    let second = child.join("large-b");
    let mut data = vec![0x5a; 11 * 1024 * 1024];
    std::fs::write(&first, &data).unwrap();
    data[0] = 0x33;
    std::fs::write(&second, &data).unwrap();

    let w = &f.window;
    w.invoke_workshop_source(w.get_storage_token(), 0);
    f.complete();
    w.invoke_duplicate_mode_changed(3);
    w.invoke_show_analysis();
    w.set_duplicate_single_root(label(&child).into());
    w.invoke_duplicate_scope_edited(3);
    f.until(|_| w.get_duplicate_ready());
    w.invoke_duplicate_run();
    f.until(|_| !w.get_search_busy() && w.get_search_complete());
    assert_eq!(w.get_duplicate_file_count(), "0");
    let previous_summary = w.get_duplicate_summary();

    let child_index = || {
        w.get_rows()
            .iter()
            .position(|row| row.name == "child")
            .unwrap() as i32
    };
    let before = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    assert_eq!(w.invoke_prepare_tree_menu(child_index()), child_index());
    f.host.borrow_mut().navigate(Navigate::Refresh);
    f.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            let view = page.frame.stamp.context;
            view.epoch != before.epoch
                && view.generation == before.generation
                && view.source == before.source
        })
    });

    w.invoke_tree_find_duplicates();
    assert!(w.get_search_busy());
    assert_eq!(w.get_duplicate_tree_run(), "1");
    assert_eq!(w.get_duplicate_single_root(), label(&child));
    f.until(|_| !w.get_search_busy() && w.get_search_complete());
    assert_eq!(w.get_duplicate_tree_run(), "1");
    assert_eq!(w.get_duplicate_summary(), previous_summary);

    std::fs::copy(first, second).unwrap();
    assert_eq!(w.invoke_prepare_tree_menu(child_index()), child_index());
    w.invoke_tree_find_duplicates();
    f.until(|_| !w.get_search_busy() && w.get_search_complete());
    assert_eq!(w.get_duplicate_tree_run(), "2");
    assert_eq!(w.get_duplicate_file_count(), "1");

    assert_eq!(w.invoke_prepare_tree_menu(child_index()), child_index());
    f.host
        .borrow_mut()
        .tree_menu_target
        .as_mut()
        .unwrap()
        .source_epoch ^= 1;
    w.invoke_tree_find_duplicates();
    assert!(!w.get_search_busy());
    assert_eq!(w.get_duplicate_tree_run(), "2");
    assert!(
        w.get_duplicate_warning()
            .contains("can no longer be verified")
    );

    w.set_duplicate_content_total_bytes("invalid".into());
    assert_eq!(w.invoke_prepare_tree_menu(child_index()), child_index());
    w.invoke_tree_find_duplicates();
    assert!(!w.get_search_busy());
    assert!(!w.get_search_duplicates_visible());
    assert!(w.get_duplicate_tree_run().is_empty());
    assert_eq!(w.get_duplicate_summary(), "Could not start search.");
    assert!(!w.get_duplicate_content_limits_error().is_empty());
}

#[cfg(windows)]
#[test]
fn tree_duplicate_command_waits_for_a_retiring_read_and_starts_with_normal_profile() {
    let f = Fixture::new();
    let child = f.source.join("child");
    let data = vec![0x5a; 11 * 1024 * 1024];
    for file in [
        child.join("large-a"),
        child.join("large-b"),
        f.source.join("outside"),
    ] {
        std::fs::write(file, &data).unwrap();
    }
    for name in ["small-a", "small-b"] {
        std::fs::write(child.join(name), b"small").unwrap();
    }
    let w = &f.window;
    w.invoke_workshop_source(w.get_storage_token(), 0);
    f.complete();
    let clicked = w.get_rows().iter().position(|r| r.name == "child").unwrap() as i32;
    assert_ne!(w.get_current_path(), label(&child));

    let retired_id = u64::MAX;
    let cancel = sortsize_core::CancellationToken::default();
    cancel.cancel();
    let lane = f.host.borrow().service.lane.clone();
    lane.lock()
        .unwrap()
        .duplicate_operations
        .push((retired_id, vec![f.other.clone()], cancel));
    w.invoke_prepare_tree_menu(clicked);
    w.invoke_tree_find_duplicates();
    assert_eq!(w.get_duplicate_single_root(), label(&child));
    assert_eq!(w.get_duplicate_profile_choice(), 2);
    assert!(!w.get_duplicate_exhaustive());
    assert!(w.get_search_busy());
    assert!(w.get_duplicate_waiting_for_scan());
    assert_eq!(w.get_duplicate_summary(), "Waiting");
    f.poll();
    assert!(w.get_duplicate_waiting_for_scan());
    lane.lock()
        .unwrap()
        .duplicate_operations
        .retain(|(id, _, _)| *id != retired_id);

    f.until(|_| !w.get_search_busy() && w.get_search_complete());
    assert!(
        w.get_duplicate_summary()
            .contains("Large files only (10 MiB)")
    );
    let rows = w.get_duplicate_rows();
    assert_eq!(rows.row_count(), 2);
    assert!(
        rows.iter()
            .all(|row| row.name.starts_with("large-") && row.location == label(&child))
    );

    let root = w.get_rows().iter().position(|r| r.selected).unwrap() as i32;
    assert_ne!(root, clicked);
    w.set_search_busy(true);
    w.invoke_prepare_tree_menu(root);
    w.invoke_tree_find_duplicates();
    assert!(w.get_search_busy());
    assert_eq!(w.get_duplicate_single_root(), label(&child));
    assert_eq!(w.get_duplicate_rows().row_count(), 2);
    assert!(w.get_status().contains("An analysis is active."));
    w.set_search_busy(false);
}

#[test]
fn tree_duplicate_command_keeps_the_clicked_scope_across_reordering() {
    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    let chosen = f.source.join("dossier été");
    std::fs::create_dir(&chosen).unwrap();
    for file in [
        chosen.join("first"),
        chosen.join("second"),
        f.source.join("outside"),
    ] {
        std::fs::write(file, b"same content").unwrap();
    }
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let index = f
        .window
        .get_rows()
        .iter()
        .position(|row| row.name == "dossier été")
        .unwrap();
    assert!(f.window.get_rows().row_data(0).unwrap().selected);
    let epoch = f.source_epoch();
    assert_eq!(
        f.window.invoke_prepare_tree_menu(index as i32),
        index as i32
    );
    assert_eq!(f.source_epoch(), epoch);
    assert!(!f.window.get_search_visible());
    assert!(f.window.get_rows().row_data(0).unwrap().selected);

    {
        let mut host = f.host.borrow_mut();
        let page = host.current.as_mut().unwrap();
        page.folders.swap(0, index);
        page.folder_keys.swap(0, index);
    }
    f.window.invoke_tree_find_duplicates();
    assert_eq!(f.window.get_ribbon_tab(), 1);
    assert_eq!(f.window.get_duplicate_mode(), duplicate_ui::SINGLE_MODE);
    assert_eq!(f.window.get_duplicate_single_root(), label(&chosen));
    assert!(f.window.get_search_visible());
    assert!(f.window.get_search_busy());
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert!(f.window.get_search_complete());
    assert_eq!(f.source_epoch(), epoch);
    assert_eq!(
        f.host.borrow().requested.as_deref(),
        Some(f.source.as_path())
    );
    assert_eq!(f.window.get_duplicate_file_count(), "1");
    let rows = f.window.get_duplicate_rows();
    assert_eq!(rows.row_count(), 2);
    assert!(rows.iter().all(|row| row.location == label(&chosen)));

    // Expanding or collapsing a branch advances the view epoch and search
    // sequence, while the completed duplicate worker still owns its results.
    let search_id = f.host.borrow().search_id;
    // The local job leaves the parent tree in place. Focusing its root
    // and invoking the keyboard menu now searches that wider scope.
    let focused_root = &f.source;
    let focused_row = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .folders
        .iter()
        .position(|row| row.path == label(focused_root))
        .unwrap() as i32;
    f.window.invoke_toggle_node(focused_row);
    assert_ne!(f.host.borrow().search_id, search_id);
    assert_eq!(f.window.invoke_prepare_tree_menu(-1), focused_row);
    f.window.invoke_tree_find_duplicates();
    assert_eq!(f.window.get_duplicate_single_root(), label(focused_root));
    assert!(f.window.get_search_busy());
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert!(f.window.get_search_complete());
    assert_eq!(f.window.get_duplicate_rows().row_count(), 3);
}

#[test]
fn strict_tree_duplicate_command_cancels_global_scan_and_resumes_same_generation() {
    let f = Fixture::strict();
    f.window.set_duplicate_exhaustive(true);
    let tmp = f.source.join("tmp");
    let first = tmp.join("1");
    let second = tmp.join("b");
    let nested = f.source.join("récursif");
    for folder in [&first, &second, &nested.join("a"), &nested.join("b")] {
        std::fs::create_dir_all(folder).unwrap();
    }
    for path in [first.join("premier.bin"), second.join("second.bin")] {
        std::fs::write(path, [4; 59]).unwrap();
    }
    for path in [
        nested.join("a/copie.bin"),
        nested.join("b/copie.bin"),
        f.source.join("hors-périmètre.bin"),
    ] {
        std::fs::write(path, [5; 83]).unwrap();
    }

    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let lane = f.host.borrow().service.lane.clone();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    lane.lock().unwrap().tick_gate = Some(gate.0.clone());
    f.window.invoke_start_scan();
    f.until(|_| lane.lock().unwrap().tick_blocked);
    let global_epoch = f.source_epoch();
    let global_candidate = lane.lock().unwrap().phase.unwrap().2;
    let global_view = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    let tmp_row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "tmp")
        .unwrap();
    assert!(f.window.get_rows().row_data(0).unwrap().selected);
    assert!(f.window.get_busy());
    assert_eq!(
        f.window.invoke_prepare_tree_menu(tmp_row as i32),
        tmp_row as i32
    );
    assert_eq!(lane.lock().unwrap().cancel_requested_epoch, None);
    assert!(!f.window.get_search_visible());
    f.window.invoke_tree_find_duplicates();
    assert_eq!(f.window.get_duplicate_single_root(), label(&tmp));
    assert!(f.window.get_duplicate_waiting_for_scan());
    assert!(f.window.get_search_busy());
    assert!(!f.window.get_search_complete());
    assert!(f.window.get_duplicate_disabled_reason().is_empty());
    for _ in 0..3 {
        f.poll();
        assert_eq!(f.source_epoch(), global_epoch);
        assert_eq!(
            lane.lock().unwrap().cancel_requested_epoch,
            Some(global_epoch)
        );
    }
    lane.lock().unwrap().tick_gate = None;
    drop(gate);
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    let scoped_epoch = f.source_epoch();
    assert_eq!(scoped_epoch, global_epoch + 1);
    assert_eq!(f.host.borrow().requested.as_deref(), Some(tmp.as_path()));
    assert!(f.window.get_search_complete());
    let rows = f.window.get_duplicate_rows();
    assert_eq!(rows.row_count(), 2);
    assert_eq!(f.window.get_duplicate_file_count(), "1");
    assert!(rows.iter().any(|row| row.location == label(&first)));
    assert!(rows.iter().any(|row| row.location == label(&second)));

    f.window.invoke_close_search();
    assert!(f.window.get_has_parent());
    assert_eq!(f.window.get_scan_path(), label(&f.source));
    assert_eq!(f.window.get_selected_volume(), 0);
    for _ in 0..10 {
        f.poll();
        assert_eq!(f.source_epoch(), scoped_epoch);
        assert!(!lane.lock().unwrap().acquisition_in_progress());
    }
    f.window.invoke_go_parent();
    f.until(|h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.path == label(&f.source) && !p.busy)
    });
    assert!(!f.window.get_has_parent());
    assert!(f.window.get_status().contains("Interrupted acquisition"));
    assert!(f.host.borrow().current.as_ref().unwrap().frame.provisional);
    assert!(!lane.lock().unwrap().acquisition_in_progress());
    let root_row = f.window.get_rows().row_data(0).unwrap();
    assert_eq!(root_row.path, label(&f.source));
    assert_eq!(f.window.get_selected_volume(), 0);

    // A single explicit click on the highlighted source must reopen the same
    // candidate, after releasing the read-only consultation lease.
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    {
        let mut l = lane.lock().unwrap();
        l.tick_blocked = false;
        l.tick_gate = Some(gate.0.clone());
    }
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.until(|_| lane.lock().unwrap().tick_blocked);
    assert!(f.window.get_busy());
    assert_eq!(lane.lock().unwrap().phase.unwrap().2, global_candidate);
    lane.lock().unwrap().tick_gate = None;
    drop(gate);
    f.complete();
    let resumed = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    assert_eq!(resumed.inventory, global_view.inventory);
    assert_eq!(resumed.source, global_view.source);
    assert_eq!(resumed.generation, global_candidate);

    let tmp_row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "tmp")
        .unwrap();
    f.window.invoke_select_node(tmp_row as i32);
    f.until(|h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.path == label(&tmp) && p.parent)
    });
    f.window.invoke_toggle_node(tmp_row as i32);
    f.until(|_| f.window.get_rows().iter().any(|row| row.name == "1"));
    let first_row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "1")
        .unwrap();
    f.window.invoke_prepare_tree_menu(first_row as i32);
    f.window.invoke_tree_find_duplicates();
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert_eq!(f.window.get_duplicate_single_root(), label(&first));
    assert!(f.window.get_search_complete());
    assert_eq!(f.window.get_duplicate_rows().row_count(), 0);

    f.window.invoke_close_search();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 1);
    f.complete();
    assert_eq!(
        f.host.borrow().requested.as_deref(),
        Some(f.other.as_path())
    );

    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let recursive_row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "récursif")
        .unwrap();
    f.window.invoke_prepare_tree_menu(recursive_row as i32);
    f.window.invoke_tree_find_duplicates();
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert_eq!(f.window.get_duplicate_single_root(), label(&nested));
    assert!(f.window.get_search_complete());
    let rows = f.window.get_duplicate_rows();
    assert_eq!(rows.row_count(), 2);
    assert!(rows.iter().any(|r| r.location == label(&nested.join("a"))));
    assert!(rows.iter().any(|r| r.location == label(&nested.join("b"))));
}

#[test]
fn tree_empty_and_top_commands_use_clicked_folder_in_sqlite() {
    exercise_tree_empty_and_top_commands(Fixture::strict(), false);
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn tree_empty_and_top_commands_use_clicked_folder_in_direct_inventory() {
    exercise_tree_empty_and_top_commands(Fixture::new(), true);
}

fn exercise_tree_empty_and_top_commands(f: Fixture, direct: bool) {
    let child = f.source.join("child");
    let nested = child.join("nested-folder");
    std::fs::create_dir(&nested).unwrap();
    std::fs::create_dir(child.join("empty-folder")).unwrap();
    std::fs::write(child.join("empty-file"), []).unwrap();
    std::fs::write(f.source.join("outside-empty"), []).unwrap();
    for size in 1..=104 {
        std::fs::write(nested.join(format!("inner-{size:03}")), vec![0; size]).unwrap();
        std::fs::write(
            f.source.join(format!("outside-{size:03}")),
            vec![0; 200 + size],
        )
        .unwrap();
    }
    let w = &f.window;
    w.invoke_workshop_source(w.get_storage_token(), 0);
    f.complete();
    assert_eq!(
        f.host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .direct_view
            .is_some(),
        direct
    );
    let clicked = w.get_rows().iter().position(|r| r.name == "child").unwrap() as i32;
    assert_ne!(w.get_current_path(), label(&child));
    assert_eq!(w.invoke_prepare_tree_menu(clicked), clicked);
    w.invoke_tree_find_empty();
    assert_eq!(w.get_empty_scope(), label(&child));
    assert_eq!(w.get_empty_mode(), 1);
    assert!(w.get_empty_busy());
    f.until(|_| w.get_empty_can_query());
    assert_eq!(w.get_empty_rows().row_count(), 1);
    assert_eq!(w.get_empty_rows().row_data(0).unwrap().name, "empty-file");
    w.invoke_empty_kind_changed(2);
    assert_eq!(w.get_empty_mode(), 2);
    assert_eq!(w.get_empty_scope(), label(&child));
    w.invoke_empty_start();
    f.until(|_| w.get_empty_can_query());
    assert_eq!(w.get_empty_rows().row_count(), 1);
    assert_eq!(w.get_empty_rows().row_data(0).unwrap().name, "empty-folder");
    w.invoke_empty_kind_changed(1);
    assert_eq!(w.get_empty_scope(), label(&child));
    assert!(!w.get_empty_busy());
    w.invoke_empty_start();
    f.until(|_| w.get_empty_can_query());
    assert_eq!(w.get_empty_rows().row_count(), 1);
    assert_eq!(w.get_empty_rows().row_data(0).unwrap().name, "empty-file");
    w.invoke_close_search();

    // A fresh native file written after acquisition must not enter the ranking.
    std::fs::write(nested.join("not-in-inventory"), vec![0; 1024]).unwrap();
    let source_epoch = f.source_epoch();
    assert_eq!(w.invoke_prepare_tree_menu(clicked), clicked);
    w.invoke_tree_top_files();
    f.until(|_| w.get_top_rows().row_count() == 100);
    assert!(w.get_top_visible());
    assert_eq!(w.get_top_scope(), label(&child));
    let rows = w.get_top_rows();
    assert_eq!(rows.row_data(0).unwrap().name, "inner-104");
    assert!(
        rows.iter()
            .all(|row| row.path.starts_with(label(&child).as_str()))
    );
    assert!(w.get_top_note().is_empty());
    assert_eq!(f.source_epoch(), source_epoch);
    w.invoke_select_top(0);
    assert_eq!(w.get_top_path(), label(&nested.join("inner-104")));
    let scoped_token = w.get_workshop_token();
    assert_eq!(
        label(&f.host.borrow().top_row(scoped_token, 1).unwrap().path),
        label(&nested.join("inner-103"))
    );
    w.invoke_show_global_top();
    f.until(|_| w.get_top_rows().row_count() == 100);
    assert!(w.get_top_scope().is_empty());
    assert_eq!(w.get_top_rows().row_data(0).unwrap().name, "outside-104");
    assert!(f.host.borrow().top_row(scoped_token, 1).is_none());
    assert_eq!(
        label(
            &f.host
                .borrow()
                .top_row(w.get_workshop_token(), 1)
                .unwrap()
                .path
        ),
        label(&f.source.join("outside-103"))
    );
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn single_scan_tree_duplicate_menu_returns_to_direct_tree_and_refreshes_fresh() {
    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    let child = f.source.join("child");
    std::fs::write(child.join("copy"), [1; 41]).unwrap();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let original = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    let direct_epoch = f.source_epoch();
    assert_eq!(
        f.host.borrow().service.lane.lock().unwrap().direct_view,
        Some(direct_epoch)
    );
    let row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "child")
        .unwrap();
    f.window.invoke_prepare_tree_menu(row as i32);
    f.window.invoke_tree_find_duplicates();
    f.until(|_| !f.window.get_search_busy() && f.window.get_duplicate_ready());
    assert_eq!(f.window.get_duplicate_single_root(), label(&child));
    assert!(f.window.get_search_complete());
    assert_eq!(f.window.get_duplicate_rows().row_count(), 2);
    assert!(
        f.window
            .get_duplicate_rows()
            .iter()
            .all(|row| row.location == label(&child))
    );
    f.window.invoke_close_search();
    f.window.invoke_go_parent();
    f.until(|h| {
        h.current
            .as_ref()
            .is_some_and(|p| p.path == label(&f.source) && !p.busy)
    });
    assert_eq!(
        f.host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .inventory,
        original.inventory
    );
    assert!(
        !f.host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .acquisition_in_progress()
    );
    let return_epoch = f.source_epoch();
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), return_epoch);
    // A direct refresh creates a fresh observation tree; historical Strict
    // candidate continuation is covered by the distinct strict_* test above.
    f.window.invoke_start_scan();
    f.complete();
    assert_ne!(
        f.host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context
            .inventory,
        original.inventory
    );
    assert_eq!(
        f.host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .root_measure
            .unwrap()
            .files,
        3
    );
}

#[test]
fn tree_duplicate_command_is_discarded_on_cancel_during_scan_stop() {
    assert_tree_duplicate_command_is_discarded(false);
}

#[test]
fn tree_duplicate_command_is_discarded_on_scope_edit_during_scan_stop() {
    assert_tree_duplicate_command_is_discarded(true);
}

#[test]
fn tree_duplicate_command_with_an_unavailable_clicked_folder_has_incomplete_coverage() {
    let f = Fixture::new();
    let child = f.source.join("child");
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let child_row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "child")
        .unwrap();
    f.window.invoke_prepare_tree_menu(child_row as i32);
    std::fs::rename(&child, f.source.join("moved")).unwrap();
    f.window.invoke_tree_find_duplicates();
    let deadline = Instant::now() + Duration::from_secs(30);
    while f.window.get_search_busy() {
        f.poll();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(f.window.get_duplicate_single_root(), label(&child));
    assert_eq!(f.window.get_duplicate_summary(), "Incomplete coverage");
    assert!(!f.window.get_duplicate_warning().is_empty());
    assert!(!f.window.get_search_complete());
    assert!(!f.window.get_search_duplicates_visible());
}

fn assert_tree_duplicate_command_is_discarded(edit: bool) {
    let f = Fixture::new();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    #[cfg(any(windows, target_os = "linux"))]
    let old_token = f.window.get_workshop_token();
    #[cfg(any(windows, target_os = "linux"))]
    let old_inventory = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context
        .inventory;
    #[cfg(any(windows, target_os = "linux"))]
    for directory in 0..16 {
        let child = f.source.join("child").join(format!("paced-{directory:02}"));
        std::fs::create_dir(&child).unwrap();
        for file in 0..64 {
            std::fs::write(child.join(format!("file-{file:02}")), [1; 3]).unwrap();
        }
    }
    let lane = f.host.borrow().service.lane.clone();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        lane.lock().unwrap().tick_gate = Some(gate.0.clone());
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        lane.lock().unwrap().tick_delay = Duration::from_millis(100);
    }
    f.window.invoke_start_scan();
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        f.until(|_| lane.lock().unwrap().tick_blocked);
    }
    let global_epoch = f.source_epoch();
    #[cfg(any(windows, target_os = "linux"))]
    {
        assert_eq!(f.window.get_rows().row_count(), 0);
        assert_eq!(f.window.get_details().row_count(), 0);
        f.window.invoke_workshop_action(old_token, 0, true);
        f.window.invoke_prepare_tree_menu(0);
        f.window.invoke_tree_find_duplicates();
        assert!(!f.window.get_duplicate_waiting_for_scan());
        assert!(lane.lock().unwrap().duplicate_operations.is_empty());
        f.until(|host| {
            f.window.get_busy()
                && f.window.get_rows().iter().any(|row| row.name == "child")
                && host.current.as_ref().is_some_and(|page| {
                    page.busy && page.frame.stamp.context.inventory != old_inventory
                })
        });
        {
            let mut state = lane.lock().unwrap();
            state.tick_gate = Some(gate.0.clone());
            state.tick_blocked = false;
        }
        f.until(|_| lane.lock().unwrap().tick_blocked);
    }
    let child_row = f
        .window
        .get_rows()
        .iter()
        .position(|r| r.name == "child")
        .unwrap();
    f.window.invoke_prepare_tree_menu(child_row as i32);
    f.window.invoke_tree_find_duplicates();
    assert!(f.window.get_duplicate_waiting_for_scan());
    if edit {
        f.window.set_duplicate_single_root(label(&f.other).into());
        f.window.invoke_duplicate_scope_edited(3);
        assert_eq!(f.window.get_duplicate_single_root(), label(&f.other));
    } else {
        f.window.invoke_search_cancel();
        assert!(f.window.get_duplicate_warning().contains("Cancelled"));
    }
    assert!(f.window.get_search_visible());
    assert!(!f.window.get_duplicate_waiting_for_scan());
    assert!(!f.window.get_search_busy());
    assert!(!f.window.get_search_complete());
    lane.lock().unwrap().tick_gate = None;
    drop(gate);
    f.until(|_| !f.window.get_busy() && f.window.get_duplicate_ready());
    for _ in 0..5 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), global_epoch);
    assert!(!f.window.get_search_duplicates_visible());
    assert!(!f.window.get_search_complete());
    assert!(lane.lock().unwrap().duplicate_operations.is_empty());
}

#[test]
fn single_duplicate_search_waits_for_an_active_scan_and_cancels_its_own_preparation() {
    let f = Fixture::strict();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let lane = f.host.borrow().service.lane.clone();
    lane.lock().unwrap().tick_gate = Some(gate.0.clone());
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.until(|_| lane.lock().unwrap().tick_blocked);
    let previous = f.source_epoch();
    f.window.invoke_duplicate_mode_changed(3);
    f.window.set_duplicate_single_root(label(&f.other).into());
    f.window.invoke_duplicate_scope_edited(3);
    f.window.invoke_duplicate_run();
    assert!(!f.window.get_duplicate_ready());
    assert!(
        f.window
            .get_duplicate_disabled_reason()
            .contains(&label(&f.source))
    );
    assert_eq!(f.source_epoch(), previous);
    f.window.invoke_cancel_scan();
    drop(gate);
    f.until(|_| f.window.get_duplicate_ready());

    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    {
        let mut l = lane.lock().unwrap();
        l.tick_gate = Some(gate.0.clone());
        l.tick_blocked = false;
    }
    f.window.invoke_duplicate_run();
    f.until(|_| lane.lock().unwrap().tick_blocked);
    let epoch = f.source_epoch();
    f.window.invoke_search_cancel();
    assert!(f.window.get_search_busy());
    drop(gate);
    f.until(|_| f.window.get_duplicate_ready() && !f.window.get_search_busy());
    for _ in 0..20 {
        f.poll();
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(f.source_epoch(), epoch);
    assert!(f.window.get_duplicate_warning().contains("Cancelled"));
    assert!(!f.window.get_search_complete());
    assert!(!f.window.get_search_duplicates_visible());
    assert!(lane.lock().unwrap().duplicate_operations.is_empty());
}

#[test]
fn current_folder_duplicate_cancellation_is_terminal_in_each_phase() {
    use duplicates::FolderPhase;

    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    let _presentation = f.controller.borrow().presentation.enter();
    std::fs::write(f.source.join("copy"), [2; 7]).unwrap();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let lane = f.host.borrow().service.lane.clone();
    let parent = f.source_epoch();
    f.window
        .invoke_duplicate_mode_changed(duplicate_ui::SINGLE_MODE);
    f.window.invoke_show_analysis();
    f.window.set_duplicate_single_root(label(&f.source).into());
    f.window.invoke_duplicate_scope_edited(3);

    for phase in [
        FolderPhase::Discovering,
        FolderPhase::Comparing,
        FolderPhase::Coverage,
    ] {
        let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
        let waiting = gate.0.clone();
        f.host.borrow_mut().duplicate.folder_observer = Some(Arc::new(move |observed| {
            if observed == phase {
                let waiting_for_cancel = waiting.0.lock().unwrap();
                drop(
                    waiting
                        .1
                        .wait_while(waiting_for_cancel, |released| !*released)
                        .unwrap(),
                );
            }
        }));
        assert!(f.window.get_duplicate_ready());
        f.window.invoke_duplicate_run();
        f.until(|_| {
            f.window.get_search_busy()
                && f.window.get_duplicate_summary().starts_with(phase.label())
                && !f.window.get_duplicate_details().is_empty()
        });
        assert_eq!(f.source_epoch(), parent);
        assert!(!lane.lock().unwrap().acquisition_in_progress());
        assert_eq!(f.window.get_duplicate_rows().row_count(), 0);
        f.window.invoke_search_cancel();
        f.window.invoke_search_cancel();
        assert_eq!(f.window.get_duplicate_summary(), "Cancelling…");
        drop(gate);
        f.until(|_| {
            !f.window.get_search_busy()
                && f.window.get_duplicate_ready()
                && (phase == FolderPhase::Coverage
                    || lane.lock().unwrap().duplicate_operations.is_empty())
        });
        assert!(f.window.get_duplicate_warning().contains("Cancelled"));
        assert!(!f.window.get_search_complete());
        if phase == FolderPhase::Coverage {
            assert!(f.window.get_search_duplicates_visible());
            assert_eq!(f.window.get_duplicate_file_count(), "1");
            assert_eq!(f.window.get_duplicate_rows().row_count(), 2);
            assert!(!lane.lock().unwrap().duplicate_operations.is_empty());
        } else {
            assert!(!f.window.get_search_duplicates_visible());
            assert_eq!(f.window.get_duplicate_rows().row_count(), 0);
        }
    }

    f.host.borrow_mut().duplicate.folder_observer = None;
    f.window.invoke_duplicate_run();
    f.until(|_| !f.window.get_search_busy() && f.window.get_search_duplicates_visible());
    assert_eq!(f.window.get_duplicate_file_count(), "1");
    assert_eq!(f.source_epoch(), parent);
    f.window.invoke_close_search();
    f.until(|_| lane.lock().unwrap().duplicate_operations.is_empty());
}

#[cfg(windows)]
#[test]
fn cancelled_empty_folder_result_survives_a_language_change_before_poll() {
    use duplicates::FolderPhase;

    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    std::fs::write(f.source.join("different-seven-byte-file"), [4; 7]).unwrap();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let waiting = gate.0.clone();
    f.host.borrow_mut().duplicate.folder_observer = Some(Arc::new(move |phase| {
        if phase == FolderPhase::Coverage {
            drop(
                waiting
                    .1
                    .wait_while(waiting.0.lock().unwrap(), |released| !*released)
                    .unwrap(),
            );
        }
    }));
    f.window.invoke_duplicate_mode_changed(3);
    f.window.set_duplicate_single_root(label(&f.source).into());
    f.window.invoke_duplicate_scope_edited(3);
    f.until(|_| f.window.get_duplicate_ready());
    f.window.invoke_duplicate_run();
    f.until(|_| {
        f.window
            .get_duplicate_summary()
            .starts_with("Checking coverage")
    });
    f.window.invoke_search_cancel();
    drop(gate);

    // The lane removes this operation after publishing its final reply. Do
    // not poll while waiting, so the reply still carries the old language.
    let lane = f.host.borrow().service.lane.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lane.lock().unwrap().duplicate_operations.is_empty() {
        assert!(Instant::now() < deadline, "duplicate worker did not exit");
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(f.window.get_search_busy());
    f.window.invoke_set_language(2);
    f.until(|_| !f.window.get_search_busy());
    assert!(!f.window.get_search_duplicates_visible());
    assert!(!f.window.get_search_complete());
    assert_eq!(f.window.get_duplicate_warning(), "Annulée");
    assert!(
        f.window
            .get_duplicate_details()
            .contains("Octets de contenu lus (empreintes et comparaison) : 14.")
    );
}

#[test]
fn duplicate_resource_edits_apply_only_to_explicit_new_searches() {
    use duplicates::FolderPhase;

    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    let scratch = f._temporary.path().join("separate-duplicate-scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::write(f.source.join("copy"), [2; 7]).unwrap();
    {
        let mut controller = f.controller.borrow_mut();
        controller.preferences.duplicate_resources.temporary_parent = Some(scratch.clone());
        controller.show_duplicate_resource_preferences(&f.window);
    }
    f.window
        .invoke_duplicate_mode_changed(duplicate_ui::SINGLE_MODE);
    f.window.set_duplicate_single_root(label(&f.source).into());
    f.window.invoke_duplicate_scope_edited(3);
    f.window.set_duplicate_resource_memory_mib("1".into());
    f.window.invoke_duplicate_resource_apply();
    assert!(!f.window.get_duplicate_resource_invalid());
    assert_eq!(
        f.window.get_duplicate_resource_temporary_parent(),
        label(&scratch)
    );
    let epoch = f.source_epoch();
    let lane = f.host.borrow().service.lane.clone();
    assert!(lane.lock().unwrap().duplicate_operations.is_empty());

    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let waiting = gate.0.clone();
    f.host.borrow_mut().duplicate.folder_observer = Some(Arc::new(move |phase| {
        if phase == FolderPhase::Discovering {
            drop(
                waiting
                    .1
                    .wait_while(waiting.0.lock().unwrap(), |released| !*released)
                    .unwrap(),
            );
        }
    }));
    f.until(|_| f.window.get_duplicate_ready());
    f.window.invoke_duplicate_run();
    f.until(|_| f.window.get_duplicate_summary().starts_with("Discovering"));
    f.window.set_duplicate_resource_memory_mib("0".into());
    f.window.invoke_duplicate_resource_apply();
    assert!(f.window.get_duplicate_resource_invalid());
    assert_eq!(
        f.controller
            .borrow()
            .preferences
            .duplicate_resources
            .memory_limit,
        Some(crate::display_preferences::MIB)
    );
    f.window.set_duplicate_resource_memory_mib("".into());
    f.window.invoke_duplicate_resource_apply();
    f.window.invoke_duplicate_resource_reset_parent();
    assert_eq!(
        f.controller
            .borrow()
            .preferences
            .duplicate_resources
            .memory_limit,
        None
    );
    assert!(!f.window.get_duplicate_resource_temporary_custom());
    assert!(f.window.get_search_busy());
    drop(gate);

    f.until(|_| {
        !f.window.get_search_busy() && lane.lock().unwrap().duplicate_operations.is_empty()
    });
    assert_eq!(
        f.window.get_duplicate_warning(),
        "Search RAM budget unavailable. Set at least 8 MiB or use Automatic."
    );
    assert!(f.window.get_duplicate_details().contains(&label(&scratch)));
    assert!(f.window.get_duplicate_details().contains("1,048,576 bytes"));
    assert!(f.window.get_duplicate_content_retry());
    assert!(!f.window.get_search_complete());
    assert!(!f.window.get_search_duplicates_visible());
    assert_eq!(f.window.get_duplicate_rows().row_count(), 0);
    assert!(
        !f.window
            .get_duplicate_summary()
            .contains("No exact duplicates")
    );
    assert_eq!(f.source_epoch(), epoch);

    f.host.borrow_mut().duplicate.folder_observer = None;
    f.window.invoke_duplicate_run();
    f.until(|_| !f.window.get_search_busy() && f.window.get_search_duplicates_visible());
    assert!(f.window.get_search_complete());
    assert_eq!(f.window.get_duplicate_file_count(), "1");
    assert!(!f.window.get_duplicate_details().contains(&label(&scratch)));
    assert_eq!(f.source_epoch(), epoch);
    f.window.invoke_close_search();
    f.until(|_| lane.lock().unwrap().duplicate_operations.is_empty());
}

#[cfg(windows)]
#[test]
fn current_folder_duplicates_cannot_publish_after_source_navigation() {
    use duplicates::FolderPhase;

    let f = Fixture::new();
    f.window.set_duplicate_exhaustive(true);
    let _presentation = f.controller.borrow().presentation.enter();
    std::fs::write(f.source.join("copy"), [2; 7]).unwrap();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let lane = f.host.borrow().service.lane.clone();
    let epoch = f.source_epoch();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let waiting = gate.0.clone();
    f.host.borrow_mut().duplicate.folder_observer = Some(Arc::new(move |phase| {
        if phase == FolderPhase::Coverage {
            let lock = waiting.0.lock().unwrap();
            drop(waiting.1.wait_while(lock, |released| !*released).unwrap());
        }
    }));
    f.window
        .invoke_duplicate_mode_changed(duplicate_ui::SINGLE_MODE);
    f.window.invoke_show_analysis();
    f.window.set_duplicate_single_root(label(&f.source).into());
    f.window.invoke_duplicate_scope_edited(3);
    assert!(f.window.get_duplicate_ready());
    f.window.invoke_duplicate_run();
    f.until(|_| {
        f.window
            .get_duplicate_summary()
            .starts_with(FolderPhase::Coverage.label())
    });
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 1);
    assert_ne!(f.source_epoch(), epoch);
    drop(gate);
    f.until(|_| lane.lock().unwrap().duplicate_operations.is_empty());
    f.complete();
    assert_eq!(
        f.host.borrow().requested.as_deref(),
        Some(f.other.as_path())
    );
    assert!(!f.window.get_search_visible());
    assert!(!f.window.get_search_busy());
    assert!(!f.window.get_search_complete());
    assert_eq!(f.window.get_duplicate_rows().row_count(), 0);
}

#[cfg(windows)]
#[test]
#[ignore = "explicit Windows mode 3 qualification on an isolated corpus"]
fn current_folder_duplicates_measurement() {
    use duplicates::FolderPhase;
    use serde_json::json;

    let scope =
        PathBuf::from(std::env::var_os("BUNNY_DUPLICATE_MEASURE_ROOT").expect("corpus scope"));
    let parent = std::env::var_os("BUNNY_DUPLICATE_MEASURE_PARENT")
        .map_or_else(|| scope.clone(), PathBuf::from);
    let output =
        PathBuf::from(std::env::var_os("BUNNY_DUPLICATE_MEASURE_OUTPUT").expect("JSONL output"));
    let workspace =
        PathBuf::from(std::env::var_os("BUNNY_AND_SIZE_STORE_DIR").expect("isolated workspace"));
    assert!(scope.is_absolute() && scope.is_dir() && scope.starts_with(&parent));
    assert!(workspace.is_absolute() && workspace.is_dir() && !workspace.starts_with(&parent));
    assert!(output.is_absolute() && !output.exists() && !output.starts_with(&parent));
    let legacy = std::env::var("BUNNY_DUPLICATE_MEASURE_LEGACY").as_deref() == Ok("1");
    let mut f = Fixture::new();
    let requested_ram = std::env::var("BUNNY_DUPLICATE_MEASURE_RAM_MIB").ok();
    if let Some(mib) = &requested_ram {
        f.window.set_memory_limit(mib.as_str().into());
        f.window.invoke_memory_limit_apply();
        let expected =
            crate::display_preferences::memory_limit_bytes(mib.parse().unwrap()).unwrap();
        assert_eq!(
            sortsize_core::resources::Governor::global()
                .usage()
                .limits
                .bytes,
            expected
        );
    }
    f.source = parent.clone();
    inject(
        &f.controller,
        discovery(std::slice::from_ref(&parent)),
        true,
    );
    f.controller.borrow().show_sources(&f.window);
    let lane = f.host.borrow().service.lane.clone();
    let revision = lane.lock().unwrap().autosave_revision.clone();
    let autosave_before = revision.load(std::sync::atomic::Ordering::Acquire);
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    let deadline = Instant::now() + Duration::from_secs(900);
    while revision.load(std::sync::atomic::Ordering::Acquire) == autosave_before {
        f.poll();
        assert!(
            Instant::now() < deadline,
            "parent autosave: {}",
            f.window.get_status()
        );
        assert!(
            f.window.get_errors().is_empty(),
            "{}",
            f.window.get_errors()
        );
        std::thread::sleep(Duration::from_millis(3));
    }
    f.complete();
    let parent_epoch = f.source_epoch();

    f.host.borrow_mut().duplicate.force_duplicate_inventory = legacy;
    f.window
        .invoke_duplicate_mode_changed(duplicate_ui::SINGLE_MODE);
    f.window.invoke_show_analysis();
    f.window.set_duplicate_single_root(label(&scope).into());
    f.window.invoke_duplicate_scope_edited(3);
    let total_limit = std::env::var("BUNNY_DUPLICATE_MEASURE_TOTAL_BYTES").ok();
    let file_limit = std::env::var("BUNNY_DUPLICATE_MEASURE_FILE_BYTES").ok();
    let temporary_mib = std::env::var("BUNNY_DUPLICATE_MEASURE_TEMP_MIB").ok();
    let job_ram_mib = std::env::var("BUNNY_DUPLICATE_MEASURE_JOB_RAM_MIB").ok();
    let work_root = std::env::var_os("BUNNY_DUPLICATE_MEASURE_WORK_ROOT").map(PathBuf::from);
    let prep_failure = std::env::var("BUNNY_DUPLICATE_MEASURE_EXPECT_PREP_FAILURE").ok();
    let limited = std::env::var("BUNNY_DUPLICATE_MEASURE_EXPECT_PARTIAL").as_deref() == Ok("1");
    if let Some(root) = &work_root {
        assert!(root.is_absolute());
        f.controller
            .borrow_mut()
            .preferences
            .duplicate_resources
            .temporary_parent = Some(root.clone());
    }
    if let Some(value) = &temporary_mib {
        f.window
            .set_duplicate_resource_temporary_mib(value.as_str().into());
    }
    if let Some(value) = &job_ram_mib {
        f.window
            .set_duplicate_resource_memory_mib(value.as_str().into());
    }
    if temporary_mib.is_some() || job_ram_mib.is_some() {
        f.window.invoke_duplicate_resource_apply();
        assert!(!f.window.get_duplicate_resource_invalid());
    }
    if let Some(value) = &total_limit {
        f.window
            .set_duplicate_content_total_bytes(value.as_str().into());
    }
    if let Some(value) = &file_limit {
        f.window
            .set_duplicate_content_file_bytes(value.as_str().into());
    }
    assert!(
        f.window.get_duplicate_ready(),
        "{}",
        f.window.get_duplicate_disabled_reason()
    );
    let phases = Arc::new(Mutex::new([None; 3]));
    let worker_phases = phases.clone();
    let start = Instant::now();
    f.host.borrow_mut().duplicate.folder_observer = Some(Arc::new(move |phase| {
        let index = match phase {
            FolderPhase::Discovering => 0,
            FolderPhase::Comparing => 1,
            FolderPhase::Coverage => 2,
        };
        worker_phases.lock().unwrap()[index] = Some(start.elapsed().as_secs_f64() * 1_000.);
    }));
    f.window.invoke_duplicate_run();
    let mut legacy_ready_ms = None;
    let mut legacy_acquisition_ms = None;
    let deadline = Instant::now() + Duration::from_secs(900);
    while f.window.get_search_busy() {
        f.poll();
        if legacy {
            use service::Phase;
            let lane = lane.lock().unwrap();
            if let Some((epoch, phase, _)) = lane.phase
                && epoch != parent_epoch
            {
                if matches!(
                    phase,
                    Phase::Finalizing | Phase::Certifying | Phase::Reopening | Phase::Ready
                ) {
                    legacy_acquisition_ms
                        .get_or_insert_with(|| start.elapsed().as_secs_f64() * 1_000.);
                }
                if matches!(phase, Phase::Ready) {
                    legacy_ready_ms.get_or_insert_with(|| start.elapsed().as_secs_f64() * 1_000.);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            f.window.get_duplicate_summary()
        );
        if f.window.get_search_busy() {
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    let terminal_ms = start.elapsed().as_secs_f64() * 1_000.;
    assert_eq!(
        f.window.get_search_complete(),
        !limited && prep_failure.is_none(),
        "{} / {}",
        f.window.get_duplicate_warning(),
        f.window.get_duplicate_details()
    );
    assert_eq!(
        f.window.get_duplicate_content_retry(),
        limited || prep_failure.is_some()
    );
    assert_eq!(f.source_epoch() == parent_epoch, !legacy);
    let [discovering_ms, comparing_ms, coverage_ms] = *phases.lock().unwrap();
    assert_eq!(discovering_ms.is_none(), legacy);
    println!(
        "{}",
        json!({
            "legacy": legacy,
            "terminal_ms": terminal_ms,
            "discovering_ms": discovering_ms,
            "comparing_ms": comparing_ms,
            "coverage_ms": coverage_ms,
            "legacy_acquisition_ms": legacy_acquisition_ms,
            "legacy_ready_ms": legacy_ready_ms,
            "memory_budget_mib": requested_ram,
            "memory_budget_effective_bytes": sortsize_core::resources::Governor::global().usage().limits.bytes,
            "job_ram_mib": job_ram_mib,
            "temporary_mib": temporary_mib,
            "temporary_parent": work_root.as_deref().map(label),
            "preparation_failure": prep_failure.as_deref(),
            "requested_file_bytes": file_limit,
            "requested_total_bytes": total_limit,
            "first_page_rows": f.window.get_duplicate_rows().row_count(),
            "has_next_page": f.window.get_search_next(),
            "status": f.window.get_search_status().as_str(),
            "warning": f.window.get_duplicate_warning().as_str(),
            "details": f.window.get_duplicate_details().as_str(),
        })
    );
    if let Some(expected) = prep_failure {
        assert!(
            f.window.get_duplicate_warning().contains(&expected),
            "{}",
            f.window.get_duplicate_warning()
        );
        assert!(
            f.window
                .get_duplicate_details()
                .contains("Content bytes read (hashing and comparison): 0.")
        );
        assert!(comparing_ms.is_none() && coverage_ms.is_none());
        assert!(!f.window.get_search_duplicates_visible());
        assert_eq!(f.window.get_duplicate_rows().row_count(), 0);
        assert!(!output.exists());
        return;
    }
    while !output.is_file() {
        f.poll();
        assert!(
            Instant::now() < deadline,
            "{}",
            f.window.get_duplicate_details()
        );
        assert_eq!(f.window.get_search_complete(), !limited);
        std::thread::sleep(Duration::from_millis(3));
    }
}

#[test]
fn auto_analysis_refresh_failure_keeps_the_visible_inventory_consultable() {
    let f = Fixture::new();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let protected_context = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    let rows = f.window.get_details().row_count();
    let summary = f.window.get_summary();
    // The source becomes inaccessible between two explicit acquisition attempts.
    std::fs::rename(&f.source, f.source.with_file_name("source-offline")).unwrap();
    f.window.invoke_start_scan();
    assert!(f.window.get_busy());
    if cfg!(any(windows, target_os = "linux")) {
        assert_eq!(f.window.get_details().row_count(), 0);
        assert_ne!(f.window.get_summary(), summary);
    } else {
        assert_eq!(f.window.get_details().row_count(), rows);
        assert_eq!(f.window.get_summary(), summary);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        f.poll();
        if !f.window.get_busy()
            && !f.window.get_errors().is_empty()
            && f.host.borrow().current.as_ref().is_some_and(|page| {
                page.frame.stamp.context.inventory == protected_context.inventory
                    && page.frame.stamp.context.generation == protected_context.generation
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected refresh failure: {}",
            f.window.get_status()
        );
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(f.window.get_details().row_count(), rows);
    assert_same_retained_context(
        f.host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context,
        protected_context,
    );
    let epoch = f.source_epoch();
    let child = f
        .window
        .get_details()
        .iter()
        .find(|row| row.name == "child")
        .unwrap();
    f.window
        .invoke_workshop_action(f.window.get_workshop_token(), child.node_id, true);
    while !f
        .window
        .get_details()
        .iter()
        .any(|row| row.name == "nested")
    {
        f.poll();
        assert!(
            Instant::now() < deadline,
            "G0 child navigation failed after refresh error"
        );
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(f.window.get_details().row_count(), 1);
    f.window.invoke_go_parent();
    while f.window.get_details().row_count() != rows {
        f.poll();
        assert!(
            Instant::now() < deadline,
            "G0 parent navigation failed after refresh error"
        );
        std::thread::sleep(Duration::from_millis(3));
    }
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(
        f.source_epoch(),
        epoch,
        "a failure retriggered automatic acquisition"
    );
    assert_eq!(f.window.get_details().row_count(), rows);
}

#[test]
fn auto_analysis_cancel_during_preflight_keeps_g0_and_does_not_restart() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    let protected_context = f
        .host
        .borrow()
        .current
        .as_ref()
        .unwrap()
        .frame
        .stamp
        .context;
    let rows = f.window.get_details().row_count();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let entered = Arc::new(AtomicBool::new(false));
    let signal = entered.clone();
    let hold = gate.0.clone();
    // Gate only native revalidation; cancellation still uses the product UI.
    let preflight = Box::new(move || {
        signal.store(true, Ordering::Release);
        let (lock, ready) = &*hold;
        let mut release = lock.lock().unwrap();
        while !*release {
            release = ready.wait(release).unwrap();
        }
        true
    });
    f.host.borrow_mut().start(
        &f.window,
        &mut f.controller.borrow_mut(),
        Start {
            root: f.source.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: Some(preflight),
            options: sortsize_store::scheduler::CollectionOptions {
                allocation: f.window.get_collect_allocation(),
                owners: f.window.get_collect_owner(),
            },
        },
    );
    f.until(|_| entered.load(Ordering::Acquire));
    assert!(f.window.get_busy());
    assert!(
        !f.host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .writer_owns_session
    );
    assert_eq!(
        f.window.get_details().row_count(),
        if cfg!(any(windows, target_os = "linux")) {
            0
        } else {
            rows
        }
    );
    f.window.invoke_cancel_scan();
    drop(gate);
    f.until(|host| {
        !f.window.get_busy()
            && host
                .service
                .lane
                .lock()
                .unwrap()
                .phase
                .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Cancelled))
    });
    f.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.frame.stamp.context.inventory == protected_context.inventory
                && page.frame.stamp.context.generation == protected_context.generation
        })
    });
    assert!(f.window.get_status().to_lowercase().contains("cancel"));
    assert_eq!(f.window.get_details().row_count(), rows);
    assert_same_retained_context(
        f.host
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .frame
            .stamp
            .context,
        protected_context,
    );
    let epoch = f.source_epoch();
    for _ in 0..50 {
        f.window
            .invoke_workshop_source(f.window.get_storage_token(), 0);
        f.poll();
    }
    assert_eq!(f.source_epoch(), epoch);
    assert!(!f.window.get_busy());
}

#[test]
fn auto_analysis_early_folder_selection_is_remembered_after_discovery_without_restart() {
    use crate::auto_analysis::{LastSelection, SelectionStore};
    let f = Fixture::new();
    let catalogue = discovery(&[f.source.clone(), f.other.clone()]);
    inject(
        &f.controller,
        discovery(&[f.source.clone(), f.other.clone()]),
        false,
    );
    let state = f._temporary.path().join("last-source-v1.conf");
    f.controller.borrow_mut().selection_store = Some(SelectionStore::start(state.clone()).unwrap());
    f.controller.borrow_mut().restored_selection =
        LastSelection::local(f.other.clone(), &catalogue);
    let folder = f.source.join("child");
    let picked = folder.clone();
    f.controller.borrow_mut().begin_browse(&f.window, |_| {
        crate::folder_picker::FolderPicker::start(move || Some(picked))
    });
    f.until(|_| !f.window.get_choosing_folder());
    assert_eq!(f.source_epoch(), 0);
    f.window.invoke_start_scan();
    f.complete();
    assert_eq!(
        f.controller.borrow().pending_remember.as_ref(),
        Some(&folder)
    );
    let epoch = f.source_epoch();
    f.controller.borrow_mut().refresh_sources(&f.window, false);
    f.until(|_| f.controller.borrow().pending_remember.is_none());
    assert_eq!(f.window.get_scan_path().as_str(), folder.to_string_lossy());
    drop(f.controller.borrow_mut().selection_store.take());
    assert_eq!(
        crate::auto_analysis::load(&state),
        LastSelection::local(folder, &catalogue)
    );
    assert_eq!(f.source_epoch(), epoch);
    assert_eq!(f.window.get_details().row_data(0).unwrap().name, "nested");
}

#[test]
fn strict_auto_analysis_bootstrap_serves_saved_g0_before_refresh_quantum() {
    use crate::auto_analysis::LastSelection;
    let mut f = Fixture::strict();
    let state = f._temporary.path().join("state");
    f.host.borrow().service.lane.lock().unwrap().cache_root = Some(state.clone());
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.complete();
    f.window.set_cache_visible(true);
    f.window.invoke_cache_action(0, 0, 0);
    f.until(|host| host.cache_current.is_some());
    f.window
        .invoke_cache_action(1, 0, f.window.get_cache_token());
    f.until(|host| {
        host.cache_current
            .as_ref()
            .is_some_and(|page| page.id == host.cache_id && page.catalogue.len() == 1)
    });
    let saved = f.host.borrow().cache_current.as_ref().unwrap().catalogue[0].clone();
    assert!(saved.measured.is_some());
    f.window.set_cache_visible(false);
    std::fs::write(f.source.join("child/nested"), [4; 83]).unwrap();

    // A real restart closes the process and its native repository handles.
    // Replacing callbacks drops the old Host here, but Service::drop deliberately
    // detaches its workers. Wait only in this same-process fixture for both lane
    // owners to finish their existing drain before restoring the saved source.
    let retired_lane = f.host.borrow().service.lane.clone();
    let retired_workspace = Arc::downgrade(
        retired_lane
            .lock()
            .unwrap()
            .reader_workspace
            .as_ref()
            .unwrap(),
    );
    assert!(retired_workspace.upgrade().is_some());
    let host = connect(&f.window, &f.controller).unwrap();
    f.host = host;
    let shutdown_deadline = Instant::now() + Duration::from_secs(30);
    while Arc::strong_count(&retired_lane) > 1 {
        assert!(
            Instant::now() < shutdown_deadline,
            "old fixture owners did not finish draining"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        retired_workspace.upgrade().is_none(),
        "old native repository lock is still owned"
    );
    drop(retired_lane);
    // The new Host is empty: the old in-memory page cannot satisfy this test.
    assert!(f.host.borrow().current.is_none());
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    {
        let host = f.host.borrow();
        let mut lane = host.service.lane.lock().unwrap();
        lane.cache_root = Some(state);
        lane.force_strict = true;
        lane.tick_gate = Some(gate.0.clone());
    }
    {
        let mut controller = f.controller.borrow_mut();
        controller.auto_analysis = Default::default();
        controller.last_auto_selection = None;
        controller.restored_selection = LastSelection::local(
            f.source.clone(),
            &controller.sources.current.as_ref().unwrap().discovery,
        );
    }
    for _ in 0..20 {
        f.poll();
    }
    assert!(!f.window.get_busy(), "restoration must stay passive");
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    f.until(|host| {
        ({
            let lane = host.service.lane.lock().unwrap();
            lane.tick_blocked && lane.writer_running && lane.reader_active
                && matches!(lane.phase, Some((epoch, service::Phase::Acquiring, _)) if epoch == lane.source_epoch)
        })
            && host.current.as_ref().is_some_and(|page| {
                page.frame.stamp.context.generation == saved.context.generation
                    && page.details.len() == 2
            })
    });
    // Reapply the real certified G0 as a non-busy reader page while the G1
    // writer is gated. This deterministically exercises a late G0 delivery.
    let mut page = f.host.borrow_mut().current.take().unwrap();
    page.busy = false;
    f.host.borrow_mut().apply(&f.window, page);
    assert!(
        f.window.get_busy(),
        "cached G0 must not hide the ongoing refresh"
    );
    assert_eq!(
        f.host.borrow().service.lane.lock().unwrap().writer_quanta,
        0
    );
    let source_epoch = f.source_epoch();
    let child = f
        .window
        .get_details()
        .iter()
        .find(|row| row.name == "child")
        .unwrap();
    f.window
        .invoke_workshop_action(f.window.get_workshop_token(), child.node_id, true);
    f.until(|host| {
        host.current.as_ref().is_some_and(|page| {
            page.path == label(&f.source.join("child"))
                && page.frame.stamp.context.generation == saved.context.generation
                && page.frame.rows.len() == 1
                && page.frame.rows[0].logical == Some(41)
        })
    });
    assert_eq!(f.window.get_details().row_data(0).unwrap().name, "nested");
    assert_eq!(
        f.source_epoch(),
        source_epoch,
        "navigation restarted the background scan"
    );
    assert_eq!(
        f.host.borrow().service.lane.lock().unwrap().writer_quanta,
        0
    );
    drop(gate);
    f.complete();
    assert!(!f.window.get_busy());
    let host = f.host.borrow();
    let page = host.current.as_ref().unwrap();
    assert_ne!(
        page.frame.stamp.context.generation,
        saved.context.generation
    );
    assert_eq!(page.path, label(&f.source.join("child")));
    assert_eq!(page.frame.rows.len(), 1);
    assert_eq!(page.frame.rows[0].logical, Some(83));
}

#[test]
fn nas_selection_only_fills_path_and_requires_manual_analysis() {
    let f = Fixture::new();
    let mut remote = discovery(&[f.source.clone(), f.other.clone()]);
    remote.volumes[0].kind = SourceKind::Remote;
    remote.volumes[0].availability = VolumeAvailability::NotProbed;
    inject(&f.controller, remote, true);
    f.controller.borrow().show_sources(&f.window);
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    assert_eq!(
        f.window.get_scan_path().as_str(),
        f.source.to_string_lossy()
    );
    assert!(f.controller.borrow().pending_auto.is_none());
    assert!(f.controller.borrow().last_auto_selection.is_none());
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), 0);
    assert!(!f.window.get_busy());
    // This is a controller simulation on a local fixture, not a real SMB test.
    f.window.set_errors("".into());
    f.window.invoke_start_scan();
    f.complete();
    let epoch = f.source_epoch();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), epoch);
    assert!(!f.window.get_busy());
}

#[test]
fn nas_selection_and_reconnection_do_not_replace_an_active_local_acquisition() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new();
    let gate = TickGate(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let entered = Arc::new(AtomicBool::new(false));
    let signal = entered.clone();
    let hold = gate.0.clone();
    f.host.borrow_mut().start(
        &f.window,
        &mut f.controller.borrow_mut(),
        Start {
            root: f.source.clone(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: Some(Box::new(move || {
                signal.store(true, Ordering::Release);
                let (lock, ready) = &*hold;
                drop(
                    ready
                        .wait_while(lock.lock().unwrap(), |open| !*open)
                        .unwrap(),
                );
                true
            })),
            options: sortsize_store::scheduler::CollectionOptions {
                allocation: f.window.get_collect_allocation(),
                owners: f.window.get_collect_owner(),
            },
        },
    );
    f.until(|_| entered.load(Ordering::Acquire));
    let epoch = f.source_epoch();
    let original_path = f.window.get_scan_path();
    let original_scan_root = f.controller.borrow().scan_root.clone();
    let original_selection = f.window.get_selected_volume();
    let dialog = crate::AddSourceWindow::new().unwrap();
    dialog.set_display_name("NAS été 日本語".into());
    #[cfg(windows)]
    let added_path = {
        dialog.set_source_kind(0);
        PathBuf::from(r"\\offline.invalid\share-197")
    };
    #[cfg(target_os = "linux")]
    let added_path = {
        // Native mount metadata is injected; Add must not inspect this fixture.
        let mut sources = discovery(&[f.source.clone(), f.other.clone()]);
        sources.volumes[1].kind = SourceKind::Remote;
        sources.volumes[1].identity = VolumeIdentity::Linux {
            major: 0,
            minor: 99,
            filesystem: "nfs4".into(),
            source: "dedicated-test-server:/export".into(),
        };
        sources.volumes[1].availability = VolumeAvailability::NotProbed;
        inject(&f.controller, sources, true);
        dialog.set_source_kind(1);
        f.other.clone()
    };
    dialog.set_source_path(added_path.to_string_lossy().as_ref().into());
    f.controller.borrow_mut().add_source(&f.window, &dialog);
    assert_eq!(dialog.get_result_state(), 6); // Test registry has no store.
    assert!(f.controller.borrow().registry.find(&added_path).is_some());
    assert_eq!(f.window.get_scan_path(), original_path);
    assert_eq!(f.controller.borrow().scan_root, original_scan_root);
    assert_eq!(f.window.get_selected_volume(), original_selection);
    assert_eq!(f.source_epoch(), epoch);
    assert_eq!(f.host.borrow().requested.as_ref(), Some(&f.source));
    assert!(f.controller.borrow().pending_auto.is_none());
    assert!(!f.host.borrow().service.lane.lock().unwrap().cancel);
    assert!(f.window.get_busy());
    for availability in [
        VolumeAvailability::Unavailable,
        VolumeAvailability::NotProbed,
        VolumeAvailability::Available,
    ] {
        let mut sources = discovery(&[f.source.clone(), f.other.clone()]);
        sources.volumes[1].kind = SourceKind::Remote;
        sources.volumes[1].availability = availability;
        inject(&f.controller, sources, true);
        f.controller.borrow().show_sources(&f.window);
        f.window
            .invoke_workshop_source(f.window.get_storage_token(), 1);
        for _ in 0..5 {
            f.poll();
        }
        assert_eq!(f.source_epoch(), epoch);
        assert_eq!(f.host.borrow().requested.as_ref(), Some(&f.source));
        assert!(f.window.get_busy());
        assert!(f.controller.borrow().pending_auto.is_none());
        let host = f.host.borrow();
        let lane = host.service.lane.lock().unwrap();
        assert!(!lane.cancel);
        assert!(!lane.publication_cancel.is_cancelled());
    }
    f.window.set_errors("".into());
    drop(gate);
    f.complete();
    assert_eq!(f.host.borrow().requested.as_ref(), Some(&f.source));
    assert_eq!(f.source_epoch(), epoch);
    assert_eq!(f.window.get_details().row_count(), 2);
}

/// Opt-in real native share: the caller supplies a disposable mounted corpus.
#[test]
#[ignore = "requires BUNNY_NAS_TEST_ROOT with a real mounted SMB/NFS corpus"]
fn nas_real_share_requires_explicit_start() {
    let mut f = Fixture::new();
    f.source = std::env::var_os("BUNNY_NAS_TEST_ROOT")
        .expect("native NAS root")
        .into();
    assert!(f.source.is_absolute());
    let mut remote = discovery(&[f.source.clone()]);
    remote.volumes[0].kind = SourceKind::Remote;
    remote.volumes[0].availability = VolumeAvailability::NotProbed;
    inject(&f.controller, remote, true);
    f.controller.borrow().show_sources(&f.window);
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), 0);
    assert!(!f.window.get_busy());
    f.window.set_errors("".into());
    f.window.invoke_start_scan();
    f.complete();
    assert!(f.window.get_details().row_count() > 0);
    let epoch = f.source_epoch();
    f.window
        .invoke_workshop_source(f.window.get_storage_token(), 0);
    for _ in 0..20 {
        f.poll();
    }
    assert_eq!(f.source_epoch(), epoch);
    assert!(!f.window.get_busy());
}

#[cfg(windows)]
#[test]
fn automatic_source_start_clears_a_previous_fast_mode_selection() {
    let fixture = Fixture::new();
    fixture.window.set_scan_fast_mode(true);
    fixture
        .window
        .invoke_workshop_source(fixture.window.get_storage_token(), 0);
    fixture.until(|host| host.requested.as_ref() == Some(&fixture.source));
    assert!(!fixture.window.get_scan_fast_mode());
    assert!(
        !fixture
            .host
            .borrow()
            .service
            .lane
            .lock()
            .unwrap()
            .fast_local
    );
    fixture.window.invoke_cancel_scan();
}

#[test]
fn duplicate_size_profile_is_captured_and_exhaustive_preserves_the_normal_choice() {
    let f = Fixture::new();
    let pair = vec![0x5a; 256 * 1024];
    for name in ["first.bin", "second.bin"] {
        std::fs::write(f.source.join(name), &pair).unwrap();
    }
    for (name, bytes) in [
        ("empty-a", &b""[..]),
        ("empty-b", &b""[..]),
        ("small-a", &b"x"[..]),
        ("small-b", &b"x"[..]),
    ] {
        std::fs::write(f.source.join(name), bytes).unwrap();
    }
    let w = &f.window;
    assert_eq!(w.get_duplicate_profile_choice(), 2);
    assert!(!w.get_duplicate_exhaustive());
    w.invoke_duplicate_mode_changed(3);
    w.invoke_show_analysis();
    w.set_duplicate_single_root(label(&f.source).into());
    w.invoke_duplicate_scope_edited(3);
    let run = || {
        f.until(|_| w.get_duplicate_ready());
        w.invoke_duplicate_run();
        f.until(|_| !w.get_search_busy() && w.get_search_duplicates_visible());
        w.get_duplicate_rows().row_count()
    };
    assert_eq!(run(), 0);
    assert!(
        w.get_duplicate_summary()
            .contains("Large files only (10 MiB)")
    );
    assert!(
        w.get_duplicate_summary()
            .contains("Empty files excluded: 2")
    );
    w.invoke_duplicate_profile_changed(0);
    assert_eq!(run(), 2);
    assert!(
        w.get_duplicate_summary()
            .contains("General search (256 KiB)")
    );
    w.set_duplicate_exhaustive(true);
    assert_eq!(run(), 4);
    assert!(w.get_duplicate_summary().contains("Exhaustive search"));
    assert!(
        w.get_duplicate_summary()
            .contains("Empty files excluded: 2")
    );
    w.set_duplicate_exhaustive(false);
    assert_eq!(w.get_duplicate_profile_choice(), 0);
    assert_eq!(
        f.controller
            .borrow()
            .preferences
            .duplicate_profile
            .minimum_bytes(),
        256 * 1024
    );
    assert!(w.get_duplicate_summary().contains("Exhaustive search"));
    assert_eq!(run(), 2);
}

#[test]
fn empty_analysis_has_independent_views_pages_filter_breakdown_and_cancellation() {
    use slint::platform::{Key, WindowEvent};
    let f = Fixture::new();
    let scope = f.source.join("samples");
    let branch = scope.join("only-child");
    std::fs::create_dir_all(branch.join("directly-empty")).unwrap();
    for index in 0..113 {
        std::fs::write(scope.join(format!("été-{index:03}.txt")), b"").unwrap();
    }
    std::fs::write(scope.join("nonempty"), b"x").unwrap();
    let w = &f.window;
    w.window().set_size(slint::LogicalSize::new(780.0, 520.0));
    w.invoke_activate_ribbon(1);
    w.invoke_open_empty_analysis(1);
    assert!(!w.get_empty_busy());
    assert!(!w.get_empty_can_query());
    w.set_empty_scope(label(&scope).into());
    w.invoke_empty_scope_edited();
    w.invoke_empty_start();
    f.until(|_| w.get_empty_can_query());
    assert!(w.get_empty_status().contains("Matches: 113"));
    assert!(w.get_empty_status().contains("Complete coverage"));
    assert!(w.get_empty_next());
    assert!(!w.get_empty_previous());
    let first = w.get_empty_rows().row_data(0).unwrap();
    assert!(first.name.starts_with("été-"));
    assert_eq!(first.parent, label(&scope));
    assert!(first.detail.contains("UTC"));
    w.invoke_empty_page(true);
    f.until(|_| w.get_empty_can_query() && w.get_empty_previous());
    assert!(!w.get_empty_next());
    w.invoke_empty_page(false);
    f.until(|_| w.get_empty_can_query() && !w.get_empty_previous());
    assert_eq!(w.get_empty_rows().row_data(0).unwrap().path, first.path);
    w.set_empty_filter("112".into());
    w.invoke_empty_query();
    f.until(|_| w.get_empty_can_query());
    assert_eq!(w.get_empty_rows().row_count(), 1);
    assert!(
        w.get_empty_rows()
            .row_data(0)
            .unwrap()
            .path
            .ends_with("été-112.txt")
    );
    w.set_empty_filter("".into());
    w.set_empty_breakdown(true);
    w.invoke_empty_query();
    f.until(|_| w.get_empty_can_query());
    assert_eq!(w.get_empty_rows().row_count(), 1);
    assert_eq!(w.get_empty_rows().row_data(0).unwrap().detail, "113");

    w.invoke_open_empty_analysis(2);
    w.set_empty_scope(label(&scope).into());
    w.invoke_empty_scope_edited();
    w.invoke_empty_start();
    f.until(|_| w.get_empty_can_query());
    assert_eq!(w.get_empty_rows().row_count(), 1);
    let row = w.get_empty_rows().row_data(0).unwrap();
    assert_eq!(row.name, "directly-empty");
    assert_eq!(row.parent, label(&branch));

    for index in 1..=7 {
        w.invoke_set_language(index);
        w.invoke_set_theme(if index % 2 == 0 { 1 } else { 2 });
        f.poll();
        assert_eq!(w.get_empty_rows().row_data(0).unwrap().path, row.path);
        assert!(!w.get_empty_status().is_empty());
    }

    w.invoke_empty_start();
    assert!(w.get_empty_busy());
    w.invoke_focus_empty_results();
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });
    assert!(!w.get_empty_busy());
    assert_eq!(w.get_empty_rows().row_count(), 0);
    assert_eq!(w.get_empty_mode(), 2);
    assert!(w.get_search_visible());
}
