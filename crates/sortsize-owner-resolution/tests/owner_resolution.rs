//! Deterministic simulations; native provider tests are in a separate file.
use sortsize_core::*;
use sortsize_owner_resolution::*;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

fn id(uid: u32) -> NativeOwnerId {
    NativeOwnerId::UnixUid(uid)
}
struct Fake {
    calls: AtomicUsize,
    result: OwnerNameResolution,
}
impl OwnerLookup for Fake {
    fn lookup(&self, _: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.result.clone()
    }
}
fn fake(result: OwnerNameResolution) -> Arc<Fake> {
    Arc::new(Fake {
        calls: AtomicUsize::new(0),
        result,
    })
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < end, "test timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn take(r: &OwnerResolver) -> ResolvedOwner {
    let mut result = None;
    wait_for(|| {
        result = r.try_take();
        result.is_some()
    });
    result.unwrap()
}
fn resolve(r: &OwnerResolver, uid: u32) -> OwnerNameResolution {
    match r.try_submit(id(uid)) {
        SubmitResult::Cached(value) => value,
        SubmitResult::Queued => {
            let result = take(r);
            assert_eq!(result.id, id(uid));
            result.resolution
        }
        other => panic!("unexpected submission: {other:?}"),
    }
}
#[test]
fn cache_hits_misses_lru_eviction_and_negative_results() {
    let provider = fake(OwnerNameResolution::NotFound);
    let r = OwnerResolver::new(
        ResolverConfig {
            max_cache_entries: 2,
            ..Default::default()
        },
        provider.clone(),
    )
    .unwrap();
    for uid in [1, 2, 1, 3, 1, 2] {
        assert_eq!(resolve(&r, uid), OwnerNameResolution::NotFound);
    }
    let stats = r.stats();
    assert_eq!(stats.cache_hits, 2);
    assert_eq!(stats.cache_misses, 4);
    assert_eq!(stats.evictions, 2);
    assert_eq!(provider.calls.load(Ordering::Relaxed), 4);
    assert_eq!(stats.cache_entries, 2);
}
#[test]
fn unicode_and_invalid_native_encodings_are_preserved() {
    let names = [
        AccountName::UnixBytes("été/用户".as_bytes().into()),
        AccountName::UnixBytes(b"bad-\xff".as_slice().into()),
        AccountName::WindowsUtf16(vec![0x57, 0x5c, 0x4e2d, 0xd800].into_boxed_slice()),
    ];
    for name in names {
        let result = OwnerNameResolution::Resolved(name.clone());
        let r = OwnerResolver::new(ResolverConfig::default(), fake(result.clone())).unwrap();
        assert_eq!(resolve(&r, 1), result);
        assert_eq!(resolve(&r, 1), result);
        assert!(!name.to_string_lossy().is_empty());
    }
    assert_eq!(owner_id_text(id(0)).as_deref(), Some("0"));
    assert_eq!(owner_id_text(id(u32::MAX)).as_deref(), Some("4294967295"));
}
#[test]
fn failures_are_distinct_retryable_and_keep_the_id() {
    for value in [
        OwnerNameResolution::AccessDenied,
        OwnerNameResolution::Unavailable(LookupError::UnsupportedIdentity),
        OwnerNameResolution::TemporaryFailure(LookupError::System(123)),
    ] {
        let r = OwnerResolver::new(ResolverConfig::default(), fake(value.clone())).unwrap();
        assert_eq!(resolve(&r, 20), value);
        assert_eq!(resolve(&r, 20), value);
        assert_eq!(r.stats().lookup_calls, 2);
        assert_eq!(r.stats().cache_entries, 0);
    }
}
#[test]
fn memory_budget_eviction_oversized_results_and_disabled_cache() {
    for entries in [0, 1, 2] {
        let config = ResolverConfig {
            max_cache_entries: entries,
            max_cache_bytes: 512,
            ..Default::default()
        };
        let r = OwnerResolver::new(
            config,
            fake(OwnerNameResolution::Resolved(AccountName::UnixBytes(
                vec![b'x'; 300].into_boxed_slice(),
            ))),
        )
        .unwrap();
        let fixed = r.fixed_storage_bytes();
        for uid in 0..20 {
            resolve(&r, uid);
            assert_eq!(r.fixed_storage_bytes(), fixed);
            assert!(r.stats().cache_bytes <= 512);
            assert!(r.stats().cache_entries <= entries);
        }
        if entries == 0 {
            assert_eq!(r.stats().cache_entries, 0);
        }
    }
    // Payload can be delivered even when it cannot fit in the cache budget.
    let r = OwnerResolver::new(
        ResolverConfig {
            max_cache_entries: 1,
            max_cache_bytes: 256,
            ..Default::default()
        },
        fake(OwnerNameResolution::Resolved(AccountName::UnixBytes(
            vec![b'x'; 300].into_boxed_slice(),
        ))),
    )
    .unwrap();
    resolve(&r, 1);
    resolve(&r, 1);
    assert_eq!(r.stats().cache_entries, 0);
    assert_eq!(r.stats().lookup_calls, 2);
    let r = OwnerResolver::new(
        ResolverConfig {
            lookup_limits: LookupLimits {
                max_name_bytes: 2,
                max_buffer_bytes: 10,
            },
            ..Default::default()
        },
        fake(OwnerNameResolution::Resolved(AccountName::UnixBytes(
            vec![1; 3].into_boxed_slice(),
        ))),
    )
    .unwrap();
    assert_eq!(
        resolve(&r, 1),
        OwnerNameResolution::Unavailable(LookupError::BufferLimit)
    );
    assert_eq!(r.stats().cache_entries, 0);
}
#[test]
fn invalid_configs_are_rejected() {
    for config in [
        ResolverConfig {
            workers: 0,
            ..Default::default()
        },
        ResolverConfig {
            workers: 9,
            ..Default::default()
        },
        ResolverConfig {
            max_pending: 0,
            ..Default::default()
        },
        ResolverConfig {
            max_pending: 4097,
            ..Default::default()
        },
        ResolverConfig {
            max_cache_bytes: 1,
            ..Default::default()
        },
        ResolverConfig {
            lookup_limits: LookupLimits {
                max_name_bytes: 0,
                max_buffer_bytes: 1,
            },
            ..Default::default()
        },
    ] {
        assert!(matches!(
            OwnerResolver::new(config, fake(OwnerNameResolution::NotFound)),
            Err(ResolverStartError::InvalidConfig)
        ));
    }
}
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    calls: AtomicUsize,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(false),
            changed: Condvar::new(),
            calls: AtomicUsize::new(0),
        })
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
impl OwnerLookup for Gate {
    fn lookup(&self, _: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
        OwnerNameResolution::NotFound
    }
}
#[test]
fn slow_provider_coalesces_and_bounds_workers_queue_and_completed_results() {
    let gate = Gate::new();
    let r = Arc::new(
        OwnerResolver::new(
            ResolverConfig {
                workers: 2,
                max_pending: 4,
                max_cache_entries: 0,
                ..Default::default()
            },
            gate.clone(),
        )
        .unwrap(),
    );
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let r = r.clone();
            scope.spawn(move || {
                for _ in 0..20 {
                    assert!(matches!(
                        r.try_submit(id(1)),
                        SubmitResult::Queued | SubmitResult::AlreadyPending
                    ));
                }
            });
        }
    });
    assert_eq!(r.try_submit(id(2)), SubmitResult::Queued);
    wait_for(|| gate.calls.load(Ordering::SeqCst) == 2);
    for uid in [3, 4] {
        assert_eq!(r.try_submit(id(uid)), SubmitResult::Queued);
    }
    assert_eq!(r.try_submit(id(5)), SubmitResult::Full);
    assert_eq!(r.stats().pending, 4);
    assert_eq!(r.stats().lookup_calls, 2);
    assert_eq!(r.stats().coalesced, 159);
    gate.release();
    wait_for(|| r.stats().lookup_calls == 4 && r.stats().running == 0);
    assert_eq!(r.try_submit(id(5)), SubmitResult::Full);
    assert_eq!(r.try_submit(id(1)), SubmitResult::AlreadyPending);
    let mut ids = vec![];
    for _ in 0..4 {
        ids.push(take(&r).id);
    }
    for uid in 1..=4 {
        assert!(ids.contains(&id(uid)));
    }
    assert_eq!(r.stats().peak_running, 2);
    assert_eq!(r.stats().peak_pending, 4);
    assert_eq!(r.stats().pending, 0);
}
#[test]
fn default_limits_bound_a_blocked_provider_and_release_all_slots() {
    let config = ResolverConfig::default();
    assert_eq!(config.workers, 2);
    assert_eq!(config.max_pending, 128);
    assert_eq!(config.max_cache_entries, 256);
    assert_eq!(config.max_cache_bytes, 1024 * 1024);
    let gate = Gate::new();
    let r = OwnerResolver::new(config, gate.clone()).unwrap();
    let reserved_cache = r.stats().cache_bytes;
    for uid in 0..128 {
        assert_eq!(r.try_submit(id(uid)), SubmitResult::Queued);
    }
    wait_for(|| gate.calls.load(Ordering::SeqCst) == 2);
    assert_eq!(r.try_submit(id(128)), SubmitResult::Full);
    assert_eq!(r.try_submit(id(0)), SubmitResult::AlreadyPending);
    assert_eq!(r.stats().pending, 128);
    assert_eq!(r.stats().running, 2);
    let fixed = r.fixed_storage_bytes();
    r.cancel();
    assert!(!r.is_stopped());
    gate.release();
    wait_for(|| r.is_stopped());
    for _ in 0..128 {
        assert_eq!(take(&r).resolution, OwnerNameResolution::Cancelled);
    }
    assert!(r.try_take().is_none());
    assert_eq!(r.stats().pending, 0);
    assert_eq!(r.stats().lookup_calls, 2);
    assert_eq!(r.stats().cache_entries, 0);
    assert_eq!(r.stats().cache_bytes, reserved_cache);
    assert_eq!(r.fixed_storage_bytes(), fixed);
}
#[test]
fn cancellation_discards_late_result_and_does_not_wait_for_native_call() {
    let gate = Gate::new();
    let r = OwnerResolver::new(
        ResolverConfig {
            workers: 1,
            max_pending: 2,
            ..Default::default()
        },
        gate.clone(),
    )
    .unwrap();
    assert_eq!(r.try_submit(id(1)), SubmitResult::Queued);
    wait_for(|| gate.calls.load(Ordering::SeqCst) == 1);
    assert_eq!(r.try_submit(id(2)), SubmitResult::Queued);
    r.cancel();
    assert_eq!(r.try_submit(id(3)), SubmitResult::Cancelled);
    assert!(!r.is_stopped());
    let queued = take(&r);
    assert_eq!(queued.id, id(2));
    assert_eq!(queued.resolution, OwnerNameResolution::Cancelled);
    gate.release();
    let running = take(&r);
    assert_eq!(running.id, id(1));
    assert_eq!(running.resolution, OwnerNameResolution::Cancelled);
    wait_for(|| r.is_stopped());
    assert_eq!(r.stats().lookup_calls, 1);
    assert_eq!(r.stats().cache_entries, 0);
}
#[test]
fn drop_returns_before_a_blocked_lookup_finishes() {
    let gate = Gate::new();
    let r = OwnerResolver::new(
        ResolverConfig {
            workers: 1,
            ..Default::default()
        },
        gate.clone(),
    )
    .unwrap();
    r.try_submit(id(1));
    wait_for(|| gate.calls.load(Ordering::SeqCst) == 1);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(r);
        tx.send(()).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(2));
    gate.release();
    result.unwrap();
}
#[test]
fn resolution_uses_only_ids_after_source_removal_and_preserves_metadata() {
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::write(temp.path().join("file"), b"123").unwrap();
    let report = scan_inventory_with_owners(
        temp.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        OwnerSelection {
            owner: true,
            metadata: MetadataSelection {
                dates: true,
                allocation: true,
            },
        },
        |_| {},
    )
    .unwrap();
    let before = format!("{report:?}");
    drop(temp); // No source files exist during lookup.
    let r = OwnerResolver::new(
        ResolverConfig::default(),
        fake(OwnerNameResolution::NotFound),
    )
    .unwrap();
    for owner in &report.owners {
        if let MetadataValue::Known { value, .. } = owner.id {
            match r.try_submit(value) {
                SubmitResult::Queued => {
                    assert_eq!(take(&r).id, value);
                }
                SubmitResult::Cached(_) => {}
                other => panic!("{other:?}"),
            }
        }
    }
    assert_eq!(before, format!("{report:?}"));
}
#[test]
fn blocked_resolution_does_not_hold_scanner_resources() {
    let gate = Gate::new();
    let r = OwnerResolver::new(ResolverConfig::default(), gate.clone()).unwrap();
    r.try_submit(id(1));
    wait_for(|| gate.calls.load(Ordering::SeqCst) == 1);
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::write(temp.path().join("file"), b"123").unwrap();
    let report = scan(
        temp.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(r.stats().running, 1);
    gate.release();
}
#[test]
fn provider_panic_is_retryable_without_losing_a_slot() {
    struct Panic;
    impl OwnerLookup for Panic {
        fn lookup(&self, _: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
            panic!("injected provider panic");
        }
    }
    let r = OwnerResolver::new(ResolverConfig::default(), Arc::new(Panic)).unwrap();
    for _ in 0..2 {
        assert_eq!(
            resolve(&r, 1),
            OwnerNameResolution::TemporaryFailure(LookupError::ProviderPanicked)
        );
    }
    assert_eq!(r.stats().pending, 0);
}
#[cfg(windows)]
#[test]
fn textual_sid_fallback_uses_only_collected_bytes() {
    let sid = WindowsSid::from_bytes(&[1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0]).unwrap();
    assert_eq!(
        owner_id_text(NativeOwnerId::WindowsSid(sid)).as_deref(),
        Some("S-1-5-32-544")
    );
    let sid = WindowsSid::from_bytes(&[1, 0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]).unwrap();
    assert_eq!(
        owner_id_text(NativeOwnerId::WindowsSid(sid)).as_deref(),
        Some("S-1-0x123456789abc")
    );
}

#[test]
fn idle_system_provider_does_not_lookup_without_submission() {
    let provider = Arc::new(SystemOwnerLookup::new(
        SystemProviderConsent::AllowPotentialNetworkLookup,
    ));
    let r = OwnerResolver::new(ResolverConfig::default(), provider.clone()).unwrap();
    assert_eq!(provider.native_calls(), 0);
    assert_eq!(r.stats().lookup_calls, 0);
    r.cancel();
    wait_for(|| r.is_stopped());
    assert_eq!(provider.native_calls(), 0);
}
