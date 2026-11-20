use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

type Callback = Box<dyn FnMut(&str, &Repository, &CertifiedGeneration)>;
thread_local! {
    static HOOK: RefCell<Option<Callback>> = const { RefCell::new(None) };
}
static SERIAL: Mutex<()> = Mutex::new(());
pub(super) fn hook(point: &str, repo: &Repository, cap: &CertifiedGeneration) {
    HOOK.with(|hook| {
        if let Some(action) = hook.borrow_mut().as_mut() {
            action(point, repo, cap);
        }
    });
}
struct HookGuard;
impl Drop for HookGuard {
    fn drop(&mut self) {
        HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}
fn observe(action: impl FnMut(&str, &Repository, &CertifiedGeneration) + 'static) -> HookGuard {
    HOOK.with(|hook| assert!(hook.replace(Some(Box::new(action))).is_none()));
    HookGuard
}
fn native_connection(cap: &CertifiedGeneration) -> usize {
    // SAFETY: the connection is live and owned here; only compare its opaque
    // address. Never dereference it or call SQLite through this raw handle.
    unsafe { cap.store.db.handle() as usize }
}

struct Fixture {
    source: Store,
    repo: Repository,
    plan: Pipeline,
    governor: Governor,
    root: PathBuf,
    _temp: tempfile::TempDir,
}
fn fixture(complete: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("offline-root");
    let c = context_id(1);
    let mut source = Store::create(
        &temp.path().join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    source.create_generation(c).unwrap();
    let attempt = attempt_id(1, 0, 1);
    source.begin_attempt(attempt).unwrap();
    batch(
        &mut source,
        attempt,
        0,
        None,
        &[Observation {
            name: queries::native_path(Path::new("one.bin")),
            kind: Kind::File,
            logical: Some(7),
            allocated: None,
            modified: None,
            metadata_known: true,
        }],
    )
    .unwrap();
    batch(
        &mut source,
        attempt,
        1,
        Some(if complete {
            ListingEnd::Exhausted
        } else {
            ListingEnd::Incomplete
        }),
        &[],
    )
    .unwrap();
    if complete {
        reconcile(&mut source, attempt);
    }
    fold(&mut source, 1);
    let coverage = source.seal(c).unwrap();
    assert_eq!(
        coverage,
        if complete {
            ScopeCoverage::Complete
        } else {
            ScopeCoverage::Partial
        }
    );
    source
        .record_measurement(c.generation, &root, 10, 11)
        .unwrap();
    let mut repo = Repository::open(&temp.path().join("repo"), c.inventory, c.source).unwrap();
    repo.reserve_legacy_ids(&source).unwrap();
    let governor = Governor::new(
        MemoryMode::Auto,
        Some(sortsize_core::resources::MemoryReading {
            total: 8 * 1024 * FRAME,
            available: 8 * 1024 * FRAME,
        }),
        0,
    );
    let plan = Pipeline::publication_in(&governor).unwrap();
    Fixture {
        source,
        repo,
        plan,
        governor,
        root,
        _temp: temp,
    }
}
fn publish(
    f: &mut Fixture,
    cancel: &CancellationToken,
) -> Result<(PublicationOutcome, Option<CertifiedGeneration>)> {
    f.repo.publish_with_plan_and_reader(
        &f.source,
        GenerationId(1),
        &f.root,
        16 * FRAME,
        cancel,
        &mut f.plan,
    )
}
fn sqlite_bytes(f: &Fixture) -> u64 {
    f.governor.usage().categories[Category::Sqlite as usize]
}
fn preserved_without_reader(f: &Fixture) {
    assert!(
        f.repo
            .generation_path(GenerationId(1))
            .join("inventory.db")
            .is_file()
    );
    assert_eq!(f.plan.remaining().reader, 0);
    assert_eq!(f.plan.remaining().publication, 0);
    assert_eq!(sqlite_bytes(f), 0);
    // An exclusive inspection would fail if an unreturned shared reader lease
    // were still alive. The sealed artifact itself must remain available.
    assert!(Store::inspect(&f.repo.generation_path(GenerationId(1))).is_ok());
}

#[test]
fn committed_reader_transfers_same_connection_transaction_and_planned_lease() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for complete in [true, false] {
        let mut f = fixture(complete);
        let admitted = f.plan.remaining().reader;
        let connection = Arc::new(AtomicUsize::new(0));
        let count = Arc::new(AtomicUsize::new(0));
        let observed = connection.clone();
        let observed_count = count.clone();
        let _hook = observe(move |point, _, cap| {
            if point == "certified" {
                assert_eq!(observed_count.fetch_add(1, Ordering::Relaxed), 0);
                observed.store(native_connection(cap), Ordering::Relaxed);
                // Connection-local state distinguishes an actual handoff even
                // if a close/reopen allocator happened to reuse a raw address.
                cap.store
                    .db
                    .pragma_update(None, "query_only", true)
                    .unwrap();
            } else {
                assert_eq!(observed.load(Ordering::Relaxed), native_connection(cap));
            }
        });
        let (outcome, reader) = publish(&mut f, &Default::default()).unwrap();
        assert!(outcome.committed());
        let cap = reader.expect("committed handoff must own its reader");
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(native_connection(&cap), connection.load(Ordering::Relaxed));
        assert!(!cap.store.db.is_autocommit());
        assert!(cap.store.db.is_readonly("main").unwrap());
        assert!(
            cap.store
                .db
                .pragma_query_value::<bool, _>(None, "query_only", |r| r.get(0))
                .unwrap()
        );
        assert_eq!(cap.store._memory.bytes(), admitted);
        assert_eq!(
            cap.coverage(),
            if complete {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            }
        );
        assert_eq!(cap.rows, 2);
        assert_eq!(cap.generation(), GenerationId(1));
        assert_eq!(cap.root(), f.root.as_path());
        assert_eq!(f.plan.remaining().reader, 0);
        assert_eq!(f.plan.remaining().publication, 0);
        assert_eq!(sqlite_bytes(&f), admitted);
        assert!(!f.root.exists());
        drop(cap);
        assert_eq!(sqlite_bytes(&f), 0);
    }
}

#[test]
fn old_publication_api_keeps_reader_slot_and_missing_slot_is_nonconsuming() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut f = fixture(true);
    let reader = f.plan.reader().unwrap();
    let publication = f.plan.remaining().publication;
    assert!(publish(&mut f, &Default::default()).is_err());
    assert_eq!(f.plan.remaining().publication, publication);
    assert!(!f.repo.generation_path(GenerationId(1)).exists());
    drop(reader);
    drop(f.plan);
    f.plan = Pipeline::publication_in(&f.governor).unwrap();
    let admitted = f.plan.remaining().reader;
    let outcome = f
        .repo
        .publish_with_plan(
            &f.source,
            GenerationId(1),
            &f.root,
            16 * FRAME,
            &Default::default(),
            &mut f.plan,
        )
        .unwrap();
    assert!(outcome.committed());
    assert_eq!(f.plan.remaining().reader, admitted);
    assert_eq!(sqlite_bytes(&f), admitted);
}

#[test]
fn catalogue_failure_drops_reader_and_preserves_published_artifact() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut f = fixture(true);
    let _hook = observe(|point, repo, _| {
        if point == "certified" {
            std::fs::create_dir(repo.directory.join("measurement-0000000000000001.json")).unwrap();
        }
    });
    let (outcome, reader) = publish(&mut f, &Default::default()).unwrap();
    assert!(!outcome.committed());
    assert!(reader.is_none());
    preserved_without_reader(&f);
}

#[test]
fn handoff_rechecks_committed_current_and_descriptor_not_cached_selection() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for replacement in ["current", "root", "context"] {
        let mut f = fixture(true);
        let _hook = observe(move |point, repo, _| {
            if point != "catalogue_committed" {
                return;
            }
            if replacement == "current" {
                let current = Current {
                    version: REPOSITORY_VERSION,
                    latest: None,
                    last_complete: None,
                };
                std::fs::write(
                    repo.directory.join("CURRENT.json"),
                    serde_json::to_vec(&current).unwrap(),
                )
                .unwrap();
            } else {
                let path = repo.directory.join("measurement-0000000000000001.json");
                let mut descriptor: Measurement = read_json(&path).unwrap();
                if replacement == "root" {
                    descriptor.root =
                        queries::native_path(&repo.directory.join("different-root")).encode();
                } else {
                    let mut changed = context_id(1);
                    changed.run = RunEpoch(99);
                    descriptor.context = context(changed);
                }
                std::fs::write(path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
            }
        });
        let (outcome, reader) = publish(&mut f, &Default::default()).unwrap();
        assert!(!outcome.committed(), "replacement={replacement}");
        assert!(reader.is_none());
        preserved_without_reader(&f);
    }
}

#[test]
fn cancellation_never_hands_off_a_reader_and_preserves_committed_bytes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut f = fixture(true);
    let before = sqlite_bytes(&f);
    let cancelled = CancellationToken::default();
    cancelled.cancel();
    assert!(publish(&mut f, &cancelled).is_err());
    assert_eq!(sqlite_bytes(&f), before);
    assert!(!f.repo.generation_path(GenerationId(1)).exists());
    drop(f);
    for boundary in ["certified", "catalogue_committed"] {
        let mut f = fixture(true);
        let cancel = CancellationToken::default();
        let stop = cancel.clone();
        let _hook = observe(move |point, _, _| {
            if point == boundary {
                stop.cancel();
            }
        });
        match publish(&mut f, &cancel) {
            Err(_) => {}
            Ok((outcome, reader)) => {
                assert!(!outcome.committed());
                assert!(reader.is_none());
            }
        }
        preserved_without_reader(&f);
    }
}
