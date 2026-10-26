use super::*;
use crate::owner_view;
use sortsize_core::*;
use sortsize_owner_resolution::{AccountName, LookupLimits, OwnerLookup, OwnerNameResolution};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Names(AtomicUsize);
impl OwnerLookup for Names {
    fn lookup(&self, id: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
        self.0.fetch_add(1, Ordering::SeqCst);
        let text = format!(
            "équipe-{}",
            sortsize_owner_resolution::owner_id_text(id).unwrap()
        );
        OwnerNameResolution::Resolved(AccountName::UnixBytes(text.into_bytes().into()))
    }
}

pub(super) fn synthetic_tree(count: usize) -> Tree {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("f"), [1; 4096]).unwrap();
    let mut result = scan_inventory_with_owners_and_allocation_totals(
        fixture.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        AllocationAggregationOptions {
            metadata: MetadataSelection {
                dates: true,
                allocation: false,
            },
            deduplicate_by_identity: false,
            max_identities: 0,
        },
        true,
        |_| {},
    )
    .unwrap();
    let dates = result.allocated.metadata.dates[0];
    let allocation = result.allocated.entries_by_path[0];
    let root = std::path::PathBuf::from("corpus-public");
    result.allocated.metadata.inventory.entries = (0..count)
        .map(|i| InventoryEntry {
            path: if i == 0 {
                root.clone()
            } else {
                root.join(format!("entrée-{i:05}"))
            },
            parent: if i == 0 { None } else { Some(0) },
            kind: if i == 0 {
                InventoryKind::Directory
            } else {
                InventoryKind::File
            },
            logical_bytes: Some(if i == 0 {
                (count - 1) as u64 * 4096
            } else {
                4096
            }),
            incomplete: false,
            counts: if i == 0 {
                Some(DirectoryCounts {
                    direct_files: (count - 1) as u64,
                    recursive_files: (count - 1) as u64,
                    ..Default::default()
                })
            } else {
                None
            },
        })
        .collect();
    result
        .allocated
        .metadata
        .inventory
        .top_files
        .entries
        .clear();
    result.allocated.metadata.dates = vec![dates; count];
    result.allocated.metadata.inventory.report.files = (count - 1) as u64;
    result.allocated.metadata.inventory.report.directories = 1;
    result.allocated.metadata.inventory.report.logical_bytes = (count - 1) as u64 * 4096;
    result.allocated.entries_by_path = vec![allocation; count];
    result.allocated.entries_by_path[0].value = MetadataValue::Known {
        value: (count - 1) as u64 * 4096,
        accuracy: ValueAccuracy::Exact,
    };
    result.owners = (0..count)
        .map(|i| EntryOwner {
            id: match i % 37 {
                34 => MetadataValue::AccessDenied,
                35 => MetadataValue::Unsupported,
                36 => MetadataValue::NotCollected,
                _ => MetadataValue::Known {
                    value: NativeOwnerId::UnixUid((i % 32) as u32),
                    accuracy: ValueAccuracy::Exact,
                },
            },
        })
        .collect();
    worker::AcquiredInventory::OwnerAllocated(result)
        .into_tree()
        .unwrap()
        .0
}

#[test]
fn owner_gui_off_native_single_scan_and_explicit_resolution() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("f"), [1; 4096]).unwrap();
    window.set_scan_path(fixture.path().to_string_lossy().as_ref().into());
    assert!(!window.get_collect_owner() && !window.get_resolve_owner());
    window.invoke_start_scan();
    wait(&window, &controller);
    assert_eq!(window.get_details().row_data(0).unwrap().owner, "—");
    assert!(controller.borrow().owner_task.is_none());
    window.set_collect_owner(true);
    window.set_collect_allocation(true);
    window.invoke_start_scan();
    wait(&window, &controller);
    let row = window.get_details().row_data(0).unwrap();
    #[cfg(windows)]
    assert!(row.owner.starts_with("S-1-"));
    #[cfg(target_os = "linux")]
    assert!(row.owner.as_str().parse::<u32>().is_ok());
    assert!(!row.allocated.is_empty() && row.modified.ends_with(" UTC"));
    assert!(controller.borrow().owner_task.is_none());
    window.invoke_sort_owner();
    assert_eq!(window.get_sort_order(), 6);
    window.invoke_sort_owner();
    assert_eq!(window.get_sort_order(), 7);
    let calls = Arc::new(Names(AtomicUsize::new(0)));
    controller.borrow_mut().owner_task = Some(owner_view::Task::start(calls.clone()).unwrap());
    window.set_resolve_owner(true);
    window.invoke_owner_resolution_changed();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !window
        .get_details()
        .row_data(0)
        .unwrap()
        .owner
        .starts_with("équipe-")
    {
        assert!(Instant::now() < deadline);
        controller.borrow_mut().poll(&window);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    window.invoke_cancel_scan();
    assert!(!window.get_resolve_owner());
    assert!(
        !window
            .get_details()
            .row_data(0)
            .unwrap()
            .owner
            .starts_with("équipe-")
    );
}

#[test]
fn fifty_thousand_nodes_keep_pages_identity_scroll_snapshot_sort_and_stale_guards() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let window = crate::new_window().unwrap();
    let controller = connect(&window);
    window.set_scan_path("corpus-public".into());
    window.set_collect_owner(true);
    window.set_collect_allocation(true);
    controller.borrow_mut().tree = Some(synthetic_tree(50_000));
    controller.borrow_mut().show_rows(&window);
    assert_eq!(window.get_details().row_count(), 2000);
    assert!(window.get_has_next_detail_page());
    for name in [false, false, true, true] {
        window.invoke_sort_details(name);
    }
    for _ in 0..2 {
        window.invoke_sort_allocated();
    }
    window.invoke_inspect_entry(17);
    assert_eq!(window.get_inspected_node(), 17);
    let ids: Vec<_> = window.get_details().iter().map(|row| row.node_id).collect();
    let calls = Arc::new(Names(AtomicUsize::new(0)));
    controller.borrow_mut().owner_task = Some(owner_view::Task::start(calls.clone()).unwrap());
    window.set_resolve_owner(true);
    window.invoke_owner_resolution_changed();
    // resolution_changed deliberately renders a fresh fallback page; subsequent
    // name patches must preserve its selection and scroll rather than rebuilding it.
    window.invoke_inspect_entry(17);
    window.set_detail_scroll_x(-200.0);
    window.set_detail_scroll_y(-50.0);
    let generation = controller.borrow().generation;
    let page = controller.borrow().owner_page;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !window.get_owner_status().starts_with("Noms de la page") {
        assert!(Instant::now() < deadline);
        controller.borrow_mut().poll(&window);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        window
            .get_details()
            .iter()
            .map(|r| r.node_id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(window.get_inspected_node(), 17);
    assert_eq!(window.get_detail_scroll_x(), -200.0);
    assert_eq!(window.get_detail_scroll_y(), -50.0);
    assert!(calls.0.load(Ordering::SeqCst) <= 32);
    window.invoke_sort_owner();
    assert!(window.get_owner_sort_preparing());
    while window.get_owner_sort_preparing() {
        assert!(Instant::now() < deadline);
        controller.borrow_mut().poll(&window);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(window.get_sort_order(), 6);
    assert_eq!(window.get_inspected_node(), 17);
    window.invoke_sort_owner();
    while window.get_owner_sort_preparing() {
        controller.borrow_mut().poll(&window);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(window.get_sort_order(), 7);
    window.invoke_change_page(true, true);
    assert_eq!(window.get_details().row_count(), 2000);
    window.invoke_change_page(true, false);
    // A stale page/name must not be applied even if its node positions are reused.
    controller.borrow_mut().generation = generation.wrapping_add(10);
    controller.borrow_mut().owner_page = page.wrapping_add(10);
    controller.borrow_mut().owner_names = Arc::default();
    controller.borrow_mut().poll(&window);
    window.set_resolve_owner(false);
    window.invoke_owner_resolution_changed();
    assert!(
        !window
            .get_details()
            .iter()
            .any(|row| row.owner.starts_with("équipe-"))
    );
    if let Ok(directory) = std::env::var("SORTSIZE_OWNER_SCREENSHOTS") {
        fs::create_dir_all(&directory).unwrap();
        window.set_resolve_owner(true);
        window.invoke_owner_resolution_changed();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !window.get_owner_status().starts_with("Noms de la page") {
            assert!(Instant::now() < deadline);
            controller.borrow_mut().poll(&window);
            std::thread::sleep(Duration::from_millis(5));
        }
        window.set_owner_status("Corpus synthétique : aucune identité personnelle".into());
        window.set_status("Corpus synthétique — 50 000 entrées".into());
        window.set_summary("Identifiants de démonstration · aucune résolution système".into());
        window.set_detail_scroll_x(-300.0);
        window.set_detail_scroll_y(0.0);
        for (percent, scale) in [(100, 1.0), (125, 1.25), (150, 1.5), (200, 2.0)] {
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                    scale_factor: scale,
                });
            capture(
                &window,
                &adapter,
                &format!("{directory}/owner-{percent}.ppm"),
                ((960.0 * scale) as u32, (600.0 * scale) as u32),
            );
        }
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
        window.set_detail_scroll_x(0.0);
        capture(
            &window,
            &adapter,
            &format!("{directory}/owner-wide.ppm"),
            (1200, 760),
        );
    }
}
