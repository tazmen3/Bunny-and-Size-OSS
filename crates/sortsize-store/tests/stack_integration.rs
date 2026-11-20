//! L00-L06 public API composition; synthetic metadata and native sources kept separate.
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    analysis::SearchQuery,
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store},
    scheduler::{Session, State},
};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(811),
        source: SourceId(811),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn name(text: &str) -> NativeName {
    #[cfg(unix)]
    {
        NativeName::Posix(text.as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        NativeName::Windows(text.encode_utf16().collect())
    }
}
fn new_store(path: &Path) -> Store {
    Store::create(
        path,
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap()
}
fn attempt(number: u64) -> AttemptKey {
    AttemptKey {
        task: TaskKey {
            context: context(),
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(number),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    }
}
#[test]
fn same_native_root_cannot_have_two_acquisition_sessions() {
    // This test exercises native-root exclusion after two Store admissions.
    // Auto may correctly reserve its entire small category for the first
    // Store; use the existing explicit 50k profile to reach the root gate.
    // Auto refusal/minimum admission remain covered by isolated memory tests.
    struct RestoreMode;
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            sortsize_core::resources::Governor::global()
                .configure(MemoryMode::Auto, sortsize_core::resources::native_memory());
        }
    }
    let _restore = RestoreMode;
    sortsize_core::resources::Governor::global().configure(
        MemoryMode::Entries50k,
        sortsize_core::resources::native_memory(),
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let _first = Session::new(
        new_store(&temp.path().join("first")),
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    let second = Session::new(
        new_store(&temp.path().join("second")),
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    );
    assert!(
        second.is_err(),
        "same native root accepted by two independent acquisition sessions"
    );
}
#[test]
fn native_acquisition_commits_then_offline_query_and_top_never_rescan() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let mut expected = 0;
    for d in 0..8 {
        let dir = root.join(format!("directory-{d}"));
        fs::create_dir(&dir).unwrap();
        for i in 0..90 {
            let size = (i + 1) as usize;
            fs::write(dir.join(format!("entry-{i}.bin")), vec![b'x'; size]).unwrap();
            expected += size as u64;
        }
    }
    let store_path = temp.path().join("inventory");
    let mut session = Session::new(
        new_store(&store_path),
        context(),
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    let clock = Instant::now();
    let mut provisional = false;
    loop {
        let status = session.tick().unwrap();
        let top = session.store().top100(GenerationId(1), &root).unwrap();
        assert!(top.rows.len() <= 100);
        provisional |= top.provisional;
        if status.state == State::Idle {
            break;
        }
        assert!(clock.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(provisional);
    while session.finalize_step().unwrap().is_none() {}
    assert_eq!(
        session.store().totals(GenerationId(1)).unwrap().logical,
        Some(expected)
    );
    assert_eq!(session.store().entry_count(GenerationId(1)).unwrap(), 729);
    fs::rename(&root, temp.path().join("source-offline")).unwrap();
    let before = session.status().quanta;
    session.control().navigate(EntryId(0));
    assert_eq!(session.tick().unwrap().state, State::Sealed);
    assert_eq!(session.status().quanta, before);
    let query = session
        .store()
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            temp.path(),
            16 * 1024 * 1024,
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(query.complete());
    assert_eq!(query.counts.matches, 729);
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = query.page(cursor.as_ref()).unwrap();
        for r in page.rows {
            assert!(seen.insert(r.key.entry));
        }
        if !page.more {
            break;
        }
        cursor = page.next;
    }
    assert_eq!(seen.len(), 729);
    let top = session.store().top100(GenerationId(1), &root).unwrap();
    assert!(!top.provisional);
    assert_eq!(top.rows.len(), 100);
    assert_eq!(top.rows[0].logical, Some(90));
    assert!(!root.exists());
}
#[test]
fn live_top_removal_promotes_successor_after_comparable_reconciliation() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = new_store(&temp.path().join("inventory"));
    store.create_generation(context()).unwrap();
    for (pass, count) in [(1, 150), (2, 149)] {
        let a = attempt(pass);
        store.begin_attempt(a).unwrap();
        let rows = (0..count)
            .map(|i| Observation {
                name: name(&format!("file-{i}")),
                kind: Kind::File,
                logical: Some(i),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        store
            .ingest(
                BatchHeader {
                    attempt: a,
                    sequence: 0,
                    end: Some(ListingEnd::Exhausted),
                },
                &rows,
            )
            .unwrap();
        while !store
            .reconcile_step(a, ComparisonScope::Comparable, 512)
            .unwrap()
        {}
        let top = store
            .top100(GenerationId(1), &temp.path().join("never-created"))
            .unwrap();
        assert!(top.provisional);
        assert_eq!(top.rows.len(), 100);
        assert_eq!(top.rows[0].logical, Some(count - 1));
        assert_eq!(top.rows[99].logical, Some(count - 100));
    }
    while !store.consolidate_step(context(), 512).unwrap() {}
    assert_eq!(store.seal(context()).unwrap(), ScopeCoverage::Complete);
}
