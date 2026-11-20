use sortsize_core::inventory_contract::*;
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store, navigation::*},
};
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(808),
        source: SourceId(808),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn view() -> ViewContext {
    let c = context();
    ViewContext {
        inventory: c.inventory,
        source: c.source,
        generation: c.generation,
        scan: c.scan,
        validation: c.validation,
        view: ViewId(1),
        epoch: ViewEpoch(1),
        query: None,
        presentation: PresentationEpoch(1),
    }
}
fn key(id: u64) -> EntryKey {
    EntryKey {
        inventory: context().inventory,
        generation: GenerationId(1),
        entry: EntryId(id),
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
fn list(store: &mut Store, pass: u64, n: u64) {
    let a = AttemptKey {
        task: TaskKey {
            context: context(),
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(pass),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    store.begin_attempt(a).unwrap();
    let rows = (0..n)
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
}
fn fixture() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &t.path().join("store"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    s.create_generation(context()).unwrap();
    list(&mut s, 1, 450);
    (t, s)
}
#[test]
fn pages_are_source_free_bounded_and_reject_foreign_context() {
    let (t, mut s) = fixture();
    let root = t.path().join("offline");
    let mut cursor = None;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let f = s
            .navigation_frame(
                context(),
                view(),
                Mode::Folder {
                    parent: key(0),
                    after: cursor,
                },
                &root,
            )
            .unwrap();
        assert!(f.provisional);
        assert!(f.rows.len() <= 200 && f.accounted_bytes <= 512 * 1024);
        for row in &f.rows {
            assert!(ids.insert(row.key.entry));
        }
        if !f.more {
            break;
        }
        cursor = f.next.clone();
    }
    assert_eq!(ids.len(), 450);
    assert!(!root.exists());
    let mut wrong = view();
    wrong.generation = GenerationId(2);
    assert!(
        s.navigation_frame(context(), wrong, Mode::Top100, &root)
            .is_err()
    );
    assert!(
        s.navigation_frame(
            context(),
            view(),
            Mode::Folder {
                parent: key(1),
                after: None
            },
            &root
        )
        .is_err()
    );
    while !s.consolidate_step(context(), 512).unwrap() {}
    s.seal(context()).unwrap();
    let f = s
        .navigation_frame(context(), view(), Mode::Top100, &root)
        .unwrap();
    assert!(!f.provisional);
    assert_eq!(f.coverage, ScopeCoverage::Complete);
}
#[test]
fn gesture_freezes_identity_and_replacement_never_selects_new_rank() {
    let (t, mut s) = fixture();
    let root = t.path().join("offline");
    let mut presenter = Presenter::new(view());
    let old = s
        .navigation_frame(context(), view(), Mode::Top100, &root)
        .unwrap();
    let old_stamp = old.stamp;
    let removed = old.rows[0].key;
    assert!(presenter.publish(old));
    presenter.gesture(true);
    list(&mut s, 2, 449);
    let newer = s
        .navigation_frame(context(), view(), Mode::Top100, &root)
        .unwrap();
    let new_stamp = newer.stamp;
    assert_ne!(newer.rows[0].key, removed);
    assert!(presenter.publish(newer));
    assert_eq!(presenter.select(old_stamp, 0), Some(removed));
    assert!(
        s.navigation_target(context(), view(), removed)
            .unwrap()
            .is_none()
    );
    presenter.gesture(false);
    assert_eq!(presenter.current().unwrap().stamp, new_stamp);
    assert_eq!(presenter.selected(), Some(removed));
    assert_eq!(presenter.select(old_stamp, 0), None);
    let mut next = view();
    next.epoch = ViewEpoch(2);
    presenter.reset(next);
    let stale = s
        .navigation_frame(context(), view(), Mode::Top100, &root)
        .unwrap();
    assert!(!presenter.publish(stale));
    assert_eq!(presenter.selected(), Some(removed));
}
#[test]
fn native_folder_gauges_share_one_revision_and_survive_offline_source() {
    use sortsize_core::acquisition::{Guard, Policy};
    use sortsize_store::{
        inventory::folder_totals::FolderFold,
        scheduler::{Session, State},
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    std::fs::create_dir_all(root.join("nested/deep")).unwrap();
    std::fs::write(root.join("a"), [0; 5]).unwrap();
    std::fs::write(root.join("nested/b"), [0; 10]).unwrap();
    std::fs::write(root.join("nested/deep/c"), [0; 20]).unwrap();
    let s = Store::create(
        &t.path().join("store"),
        context().inventory,
        context().source,
        Durability::Reconstructible,
    )
    .unwrap();
    let mut session =
        Session::new(s, context(), Guard::open(&root, Policy::default()).unwrap()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while session.tick().unwrap().state != State::Idle {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    let s = session.store();
    let mut fold = FolderFold::default();
    assert!(
        s.folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .is_none()
    );
    while !s.folder_fold_step(GenerationId(1), &mut fold).unwrap() {}
    let total = s
        .folder_measure(GenerationId(1), EntryId(0), &fold)
        .unwrap()
        .unwrap();
    assert_eq!(
        (total.logical, total.files, total.directories),
        (Some(35), 3, 2)
    );
    let frame = s
        .navigation_frame(
            context(),
            view(),
            Mode::Folder {
                parent: key(0),
                after: None,
            },
            &root,
        )
        .unwrap();
    let child = frame
        .rows
        .iter()
        .find(|r| r.kind == Kind::Directory)
        .unwrap();
    assert_eq!(
        s.folder_measure(GenerationId(1), child.key.entry, &fold)
            .unwrap()
            .unwrap()
            .logical,
        Some(30)
    );
    assert_eq!(
        s.navigation_parent(GenerationId(1), child.key.entry)
            .unwrap(),
        Some(EntryId(0))
    );
    std::fs::rename(&root, t.path().join("offline")).unwrap();
    assert_eq!(
        s.folder_measure(GenerationId(1), child.key.entry, &fold)
            .unwrap()
            .unwrap()
            .logical,
        Some(30)
    );
}
#[test]
fn unknown_file_size_keeps_folder_size_unknown() {
    use sortsize_store::inventory::folder_totals::FolderFold;
    let t = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &t.path().join("store"),
        context().inventory,
        context().source,
        Durability::Retained,
    )
    .unwrap();
    s.create_generation(context()).unwrap();
    let attempt = AttemptKey {
        task: TaskKey {
            context: context(),
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    s.begin_attempt(attempt).unwrap();
    let rows = [Some(25), None]
        .into_iter()
        .enumerate()
        .map(|(i, logical)| Observation {
            name: name(&format!("file-{i}")),
            kind: Kind::File,
            logical,
            allocated: None,
            modified: None,
            metadata_known: logical.is_some(),
        })
        .collect::<Vec<_>>();
    s.ingest(
        BatchHeader {
            attempt,
            sequence: 0,
            end: Some(ListingEnd::Exhausted),
        },
        &rows,
    )
    .unwrap();
    while !s
        .reconcile_step(attempt, ComparisonScope::Comparable, 512)
        .unwrap()
    {}
    let mut fold = FolderFold::default();
    while !s.folder_fold_step(GenerationId(1), &mut fold).unwrap() {}
    let measure = s
        .folder_measure(GenerationId(1), EntryId(0), &fold)
        .unwrap()
        .unwrap();
    assert_eq!(
        (measure.logical, measure.unknown, measure.files),
        (None, 1, 2)
    );
}
#[test]
fn folder_projection_rejects_stale_revision_and_another_generation() {
    use sortsize_store::inventory::folder_totals::FolderFold;
    let (_temp, mut s) = fixture();
    let mut fold = FolderFold::default();
    while !s.folder_fold_step(GenerationId(1), &mut fold).unwrap() {}
    assert!(
        s.folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .is_some()
    );
    list(&mut s, 2, 449);
    assert!(
        s.folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .is_none()
    );
    while !s.folder_fold_step(GenerationId(1), &mut fold).unwrap() {}
    while !s.consolidate_step(context(), 512).unwrap() {}
    s.seal(context()).unwrap();
    while !s.folder_fold_step(GenerationId(1), &mut fold).unwrap() {}
    let mut c = context();
    c.generation = GenerationId(2);
    s.create_generation(c).unwrap();
    let mut other = FolderFold::default();
    while !s.folder_fold_step(c.generation, &mut other).unwrap() {}
    assert!(
        s.folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .is_none()
    );
    while !s.folder_fold_step(GenerationId(1), &mut fold).unwrap() {}
    assert_eq!(
        s.folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .unwrap()
            .files,
        449
    );
}
