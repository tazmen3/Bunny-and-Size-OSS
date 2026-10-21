use super::*;
use sortsize_core::ValueAccuracy;
use sortsize_owner_resolution::AccountName;
use std::sync::Condvar;

fn id(value: u32) -> NativeOwnerId {
    NativeOwnerId::UnixUid(value)
}
fn known(value: u32) -> EntryOwner {
    EntryOwner {
        id: MetadataValue::Known {
            value: id(value),
            accuracy: ValueAccuracy::Exact,
        },
    }
}
fn data(count: usize) -> Arc<OwnerData> {
    Arc::new(OwnerData {
        owners: (0..count)
            .map(|i| known(i as u32))
            .collect::<Vec<_>>()
            .into(),
        ranges: (0..count)
            .map(|i| if i == 0 { 0..count - 1 } else { 0..0 })
            .collect::<Vec<_>>()
            .into(),
        children: (1..count).collect::<Vec<_>>().into(),
        directories: vec![false; count].into(),
    })
}
fn request(
    data: Arc<OwnerData>,
    generation: u64,
    page: u64,
    visible: Vec<NativeOwnerId>,
) -> Request {
    Request {
        generation,
        page,
        enabled: true,
        data: Some(data),
        visible,
        sort: None,
    }
}
fn wait(task: &mut Task, mut predicate: impl FnMut(&PageUpdate) -> bool) -> PageUpdate {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(update) = task.poll().page
            && predicate(&update)
        {
            return update;
        }
        assert!(Instant::now() < deadline, "owner coordinator timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Fake {
    calls: AtomicUsize,
    result: OwnerNameResolution,
}
impl OwnerLookup for Fake {
    fn lookup(
        &self,
        _: NativeOwnerId,
        _: sortsize_owner_resolution::LookupLimits,
    ) -> OwnerNameResolution {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        self.result.clone()
    }
}
fn fake(result: OwnerNameResolution) -> Arc<Fake> {
    Arc::new(Fake {
        calls: AtomicUsize::new(0),
        result,
    })
}

#[test]
fn labels_distinguish_missing_identity_from_failed_name_and_escape_encodings() {
    assert_eq!(fallback(known(0).id), "0");
    assert_eq!(fallback(MetadataValue::NotCollected), "—");
    assert_eq!(fallback(MetadataValue::AccessDenied), "Accès refusé");
    assert_eq!(fallback(MetadataValue::Unknown), "Indisponible");
    assert_eq!(fallback(MetadataValue::Unsupported), "Indisponible");
    assert_eq!(
        display_name(AccountName::UnixBytes("équipe\\用户".as_bytes().into())),
        "équipe\\用户"
    );
    let label = display_name(AccountName::WindowsUtf16(vec![0x4e2d, 0xd800, 10].into()));
    assert!(label.contains('中') && label.contains('�') && label.ends_with("\\n"));
    assert!(
        display_name(AccountName::UnixBytes(vec![b'a'; 32768].into())).len() <= DISPLAY_NAME_BYTES
    );
    #[cfg(windows)]
    {
        let sid =
            sortsize_core::WindowsSid::from_bytes(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]).unwrap();
        assert_eq!(
            fallback(MetadataValue::Known {
                value: NativeOwnerId::WindowsSid(sid),
                accuracy: ValueAccuracy::Exact
            }),
            "S-1-5-18"
        );
    }
}

#[test]
fn orders_snapshot_names_then_native_ids_then_absence_with_stable_node_ties() {
    let mut input = data(8);
    let input = Arc::get_mut(&mut input).unwrap();
    input.owners = vec![
        known(0),
        known(20),
        known(10),
        known(3),
        known(3),
        EntryOwner {
            id: MetadataValue::AccessDenied,
        },
        EntryOwner {
            id: MetadataValue::Unknown,
        },
        EntryOwner {
            id: MetadataValue::NotCollected,
        },
    ]
    .into();
    let names = [
        DisplayOwner {
            id: id(10),
            label: "Zoe".into(),
        },
        DisplayOwner {
            id: id(20),
            label: "Alice".into(),
        },
    ];
    let order = prepare_orders(input, &names);
    assert_eq!(&*order.directions[0], &[1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(&*order.directions[1], &[2, 1, 3, 4, 5, 6, 7]);
    let native = prepare_orders(input, &[]);
    assert_eq!(&native.directions[0][..4], &[3, 4, 2, 1]);
    assert_eq!(&native.directions[1][..4], &[1, 2, 3, 4]);
}

#[test]
fn repeated_page_owners_resolve_once_and_off_never_starts_a_pool() {
    let provider = fake(OwnerNameResolution::Resolved(AccountName::UnixBytes(
        b"synthetic".as_slice().into(),
    )));
    let mut task = Task::start(provider.clone()).unwrap();
    let mut off = request(data(3), 1, 1, vec![id(1)]);
    off.enabled = false;
    task.request(off);
    wait(&mut task, |u| u.page == 1);
    assert_eq!(task.pools_started.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(provider.calls.load(AtomicOrdering::SeqCst), 0);
    task.request(request(data(3), 1, 2, vec![id(1); 2000]));
    let update = wait(&mut task, |u| u.page == 2 && u.pending == 0);
    assert_eq!(update.names.len(), 1);
    assert_eq!(update.stats.lookup_calls, 1);
    task.request(request(data(3), 1, 3, vec![id(1); 2000]));
    let cached = wait(&mut task, |u| u.page == 3 && u.pending == 0);
    assert_eq!(cached.stats.lookup_calls, 1);
    assert!(cached.stats.cache_hits >= 1);
    assert_eq!(task.pools_started.load(AtomicOrdering::SeqCst), 1);
    // Hiding the details view pauses page admission, not the session/cache.
    task.request(request(data(3), 1, 4, Vec::new()));
    wait(&mut task, |u| u.page == 4 && u.pending == 0);
    task.request(request(data(3), 1, 5, vec![id(1); 2000]));
    let returned = wait(&mut task, |u| u.page == 5 && u.pending == 0);
    assert_eq!(returned.stats.lookup_calls, 1);
    assert_eq!(task.pools_started.load(AtomicOrdering::SeqCst), 1);
}

#[test]
fn page_budget_and_cache_stay_bounded_for_two_thousand_unique_names() {
    let provider = fake(OwnerNameResolution::Resolved(AccountName::UnixBytes(
        vec![b'x'; 2048].into(),
    )));
    let mut task = Task::start(provider).unwrap();
    task.request(request(data(2001), 1, 1, (1..=2000).map(id).collect()));
    let update = wait(&mut task, |u| u.pending == 0);
    assert!(update.names.iter().map(|n| n.label.len()).sum::<usize>() <= PAGE_NAME_BUDGET);
    assert!(update.names.len() < 2000);
    assert_eq!(update.stats.lookup_calls, 2000);
    assert!(update.stats.cache_entries <= 256 && update.stats.cache_bytes <= 1024 * 1024);
    assert!(update.stats.peak_pending <= 128 && update.stats.peak_running <= 2);
}

struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    calls: AtomicUsize,
}
impl OwnerLookup for Gate {
    fn lookup(
        &self,
        value: NativeOwnerId,
        _: sortsize_owner_resolution::LookupLimits,
    ) -> OwnerNameResolution {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
        OwnerNameResolution::Resolved(AccountName::UnixBytes(
            owner_id_text(value).unwrap().into_bytes().into(),
        ))
    }
}
#[test]
fn saturated_blocked_pool_is_retired_before_new_inventory_and_late_results_are_discarded() {
    let gate = Arc::new(Gate {
        open: Mutex::new(false),
        changed: Condvar::new(),
        calls: AtomicUsize::new(0),
    });
    let mut task = Task::start(gate.clone()).unwrap();
    task.request(request(data(2001), 1, 1, (1..=2000).map(id).collect()));
    let saturated = wait(&mut task, |u| u.stats.pending == 128);
    assert_eq!(saturated.stats.running, 2);
    task.request(request(data(2), 2, 2, vec![id(900000)]));
    let waiting = wait(&mut task, |u| u.generation == 2 && u.waiting_for_previous);
    assert!(waiting.names.is_empty());
    assert_eq!(task.pools_started.load(AtomicOrdering::SeqCst), 1);
    *gate.open.lock().unwrap() = true;
    gate.changed.notify_all();
    let update = wait(&mut task, |u| {
        u.generation == 2 && u.pending == 0 && !u.waiting_for_previous
    });
    assert_eq!(update.names.len(), 1);
    assert_eq!(update.names[0].id, id(900000));
    assert_eq!(update.names[0].label, "900000");
    assert_eq!(task.pools_started.load(AtomicOrdering::SeqCst), 2);
}

#[test]
fn failed_names_keep_native_fallback_and_coordinator_locks_never_block_ui() {
    for result in [
        OwnerNameResolution::NotFound,
        OwnerNameResolution::AccessDenied,
        OwnerNameResolution::TemporaryFailure(sortsize_owner_resolution::LookupError::System(5)),
    ] {
        let mut task = Task::start(fake(result)).unwrap();
        task.request(request(data(2), 1, 1, vec![id(1)]));
        let update = wait(&mut task, |u| u.pending == 0);
        assert!(update.names.is_empty());
    }
    let mut task = Task::start(fake(OwnerNameResolution::NotFound)).unwrap();
    let outbox = task.outbox.clone();
    let guard = outbox.lock().unwrap();
    assert!(task.poll().page.is_none());
    drop(guard);
    let inbox = task.inbox.clone();
    let guard = inbox.lock().unwrap();
    task.request(request(data(2), 1, 1, vec![id(1)]));
    assert!(task.pending.is_some());
    drop(guard);
    wait(&mut task, |u| u.pending == 0);
}

#[test]
fn closing_does_not_join_a_blocked_native_provider() {
    let gate = Arc::new(Gate {
        open: Mutex::new(false),
        changed: Condvar::new(),
        calls: AtomicUsize::new(0),
    });
    let mut task = Task::start(gate.clone()).unwrap();
    task.request(request(data(2), 1, 1, vec![id(1)]));
    let deadline = Instant::now() + Duration::from_secs(10);
    while gate.calls.load(AtomicOrdering::SeqCst) != 1 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(task);
        tx.send(()).unwrap();
    });
    let closed = rx.recv_timeout(Duration::from_secs(2));
    *gate.open.lock().unwrap() = true;
    gate.changed.notify_all();
    closed.unwrap();
}

#[test]
fn same_id_in_a_new_inventory_cannot_receive_the_old_generations_name() {
    struct Versions {
        gate: Arc<Gate>,
    }
    impl OwnerLookup for Versions {
        fn lookup(
            &self,
            _: NativeOwnerId,
            _: sortsize_owner_resolution::LookupLimits,
        ) -> OwnerNameResolution {
            let sequence = self.gate.calls.fetch_add(1, AtomicOrdering::SeqCst);
            let mut open = self.gate.open.lock().unwrap();
            while !*open {
                open = self.gate.changed.wait(open).unwrap();
            }
            let name = if sequence == 0 {
                "old-generation"
            } else {
                "new-generation"
            };
            OwnerNameResolution::Resolved(AccountName::UnixBytes(name.as_bytes().into()))
        }
    }
    let gate = Arc::new(Gate {
        open: Mutex::new(false),
        changed: Condvar::new(),
        calls: AtomicUsize::new(0),
    });
    let mut task = Task::start(Arc::new(Versions { gate: gate.clone() })).unwrap();
    task.request(request(data(2), 1, 1, vec![id(1)]));
    let deadline = Instant::now() + Duration::from_secs(10);
    while gate.calls.load(AtomicOrdering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    task.request(request(data(2), 2, 2, vec![id(1)]));
    wait(&mut task, |update| {
        update.generation == 2 && update.waiting_for_previous
    });
    *gate.open.lock().unwrap() = true;
    gate.changed.notify_all();
    let update = wait(&mut task, |update| {
        update.generation == 2 && update.pending == 0 && !update.waiting_for_previous
    });
    assert_eq!(update.names[0].label, "new-generation");
    assert_eq!(task.pools_started.load(AtomicOrdering::SeqCst), 2);
}
