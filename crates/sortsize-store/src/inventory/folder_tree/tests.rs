use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id};

#[test]
fn tree_child_queries_keep_vm_work_within_the_selected_parent() {
    fn read(db: &Connection, kinds: bool, parent: u64, after: u64) -> (Vec<Vec<u8>>, i32) {
        let sql = if kinds {
            TREE_CHILD_KINDS
        } else {
            TREE_CHILDREN
        };
        let mut q = db.prepare(sql).unwrap();
        let bindings = [unsigned(0), unsigned(parent), unsigned(after)];
        let mut rows = q
            .query(rusqlite::params_from_iter(
                bindings[..if kinds { 2 } else { 3 }].iter(),
            ))
            .unwrap();
        let mut values = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            if kinds {
                values.push(row.get::<_, i64>(0).unwrap().to_be_bytes().to_vec());
            } else {
                let mut value = row.get::<_, Vec<u8>>(0).unwrap();
                value.extend(row.get::<_, i64>(1).unwrap().to_be_bytes());
                value.extend(row.get::<_, Vec<u8>>(2).unwrap());
                values.push(value);
            }
        }
        drop(rows);
        (values, q.get_status(rusqlite::StatementStatus::VmStep))
    }

    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.execute(
        "INSERT INTO generations(id,context,revision,next_entry,aggregate_sum,aggregate_unknown) VALUES(?1,x'',?1,?1,?1,?1)",
        [unsigned(0)],
    ).unwrap();
    let insert = "INSERT INTO entries(generation,id,parent,name,kind,attempt,present,metadata_known) VALUES(?1,?2,?3,?2,?4,?1,?5,1)";
    for id in 10..310u64 {
        db.execute(
            insert,
            params![
                unsigned(0),
                unsigned(id),
                unsigned(1),
                if id % 7 == 0 { 2 } else { 1 },
                id % 5 != 0
            ],
        )
        .unwrap();
    }
    let cases = [
        (false, 1, 0),
        (false, 1, 200),
        (false, 1, 309),
        (false, 99, 0),
        (true, 1, 0),
        (true, 99, 0),
    ];
    let before = cases.map(|(kinds, parent, after)| read(&db, kinds, parent, after));
    assert_eq!(before[0].0.len(), 240);
    assert!(before[2].0.is_empty());
    assert!(before[3].0.is_empty());
    assert_eq!(before[4].0.len(), 240);
    {
        let tx = db.transaction().unwrap();
        {
            let mut q = tx.prepare(insert).unwrap();
            for id in 310..40_310u64 {
                q.execute(params![unsigned(0), unsigned(id), unsigned(2), 1, true])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    for (i, (kinds, parent, after)) in cases.into_iter().enumerate() {
        let (actual, steps) = read(&db, kinds, parent, after);
        assert_eq!(actual, before[i].0);
        assert!(
            steps <= before[i].1 * 2 + 100,
            "unrelated children scanned: kinds={kinds}, parent={parent}, after={after}, before={}, steps={steps}",
            before[i].1
        );
    }
}

fn observation(name: &str, kind: Kind, logical: Option<u64>) -> Observation {
    Observation {
        name: native_path(Path::new(name)),
        kind,
        logical,
        allocated: None,
        modified: None,
        metadata_known: true,
    }
}
fn source(root: &Path) -> Store {
    let c = context_id(0);
    let mut store = Store::create(
        &root.join("store"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    store.create_generation(c).unwrap();
    store.begin_attempt(attempt_id(0, 0, 1)).unwrap();
    store
}
fn view() -> ViewContext {
    let c = context_id(0);
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
fn key(entry: u64) -> EntryKey {
    let c = context_id(0);
    EntryKey {
        inventory: c.inventory,
        generation: c.generation,
        entry: EntryId(entry),
    }
}

#[test]
fn targeted_tree_order_is_global_directory_only_bounded_and_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    let mut observations = (0..301)
        .map(|i| observation(&format!("folder-{i:03}"), Kind::Directory, None))
        .collect::<Vec<_>>();
    observations.push(observation("not-a-folder", Kind::File, Some(u64::MAX)));
    batch(&mut store, attempt_id(0, 0, 1), 0, None, &observations).unwrap();
    let measured = |id: EntryId| -> Result<Option<FolderMeasure>> {
        Ok(Some(FolderMeasure {
            logical: match id.0 {
                1 => None,
                2 => Some(0),
                301 => Some(999),
                _ => Some(20),
            },
            ..Default::default()
        }))
    };
    let mut order = TreeBranchOrder::start(
        &store,
        context_id(0),
        view(),
        key(0),
        temp.path(),
        temp.path(),
        4 * 1024 * 1024,
    )
    .unwrap();
    assert!(!order.step(&store, measured).unwrap());
    assert!(!order.ready());
    let partial = order.page(None).unwrap();
    assert!(!partial.rows.is_empty());
    assert!(partial.rows.iter().all(|row| row.kind == Kind::Directory));
    while !order.step(&store, measured).unwrap() {}
    let first = order.page(None).unwrap();
    assert_eq!(first.rows.len(), 200);
    assert_eq!(
        (first.rows[0].key.entry, first.rows[0].logical),
        (EntryId(301), Some(999))
    );
    assert_eq!(first.rows[1].key.entry, EntryId(3));
    let second = order.page(first.next.as_ref()).unwrap();
    assert_eq!(second.rows.len(), 101);
    assert!(!second.more);
    assert_eq!(second.rows[99].logical, Some(0));
    assert_eq!(second.rows[100].logical, None);
    let mut ids = first
        .rows
        .iter()
        .chain(&second.rows)
        .map(|row| row.key.entry.0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, (1..=301).collect::<Vec<_>>());
    let back = order.previous(second.first.as_ref().unwrap()).unwrap();
    assert_eq!(
        back.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
        first.rows.iter().map(|r| r.key).collect::<Vec<_>>()
    );
    assert!(order.temporary_usage().peak <= 4 * 1024 * 1024);
    assert!(order.matches(&store, context_id(0), key(0)));
    let selected = order.page_containing(EntryId(250)).unwrap().unwrap();
    assert_eq!(selected.rows[0].key.entry, EntryId(250));
    assert!(order.page_containing(EntryId(99999)).unwrap().is_none());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        1,
        None,
        &[observation("later", Kind::Directory, None)],
    )
    .unwrap();
    assert!(!order.matches(&store, context_id(0), key(0)));
    assert!(order.step(&store, measured).is_err());
}

#[test]
fn targeted_tree_order_uses_recursive_live_measurements_per_parent() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[
            observation("small", Kind::Directory, None),
            observation("large", Kind::Directory, None),
            observation("unknown", Kind::Directory, None),
        ],
    )
    .unwrap();
    for parent in 1..=3 {
        store.begin_attempt(attempt_id(0, parent, 1)).unwrap();
    }
    batch(
        &mut store,
        attempt_id(0, 1, 1),
        0,
        None,
        &[observation("small.bin", Kind::File, Some(5))],
    )
    .unwrap();
    batch(
        &mut store,
        attempt_id(0, 2, 1),
        0,
        None,
        &[
            observation("nested", Kind::Directory, None),
            observation("direct.bin", Kind::File, Some(10)),
        ],
    )
    .unwrap();
    let nested = store
        .lookup(
            GenerationId(0),
            EntryId(2),
            &native_path(Path::new("nested")),
        )
        .unwrap()
        .unwrap()
        .key
        .entry;
    store.begin_attempt(attempt_id(0, nested.0, 1)).unwrap();
    batch(
        &mut store,
        attempt_id(0, nested.0, 1),
        0,
        None,
        &[observation("deep.bin", Kind::File, Some(100))],
    )
    .unwrap();
    batch(
        &mut store,
        attempt_id(0, 3, 1),
        0,
        None,
        &[observation("unknown.bin", Kind::File, None)],
    )
    .unwrap();
    let mut plan = generation_a::Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    while !preview.step(temp.path()).unwrap() {}
    let mut order = TreeBranchOrder::start(
        preview.store(),
        context_id(0),
        view(),
        key(0),
        temp.path(),
        temp.path(),
        4 * 1024 * 1024,
    )
    .unwrap();
    while !order
        .step(preview.store(), |id| preview.measure(id))
        .unwrap()
    {}
    let rows = order.page(None).unwrap().rows;
    assert_eq!(
        rows.iter()
            .map(|r| (r.key.entry.0, r.logical))
            .collect::<Vec<_>>(),
        vec![(2, Some(110)), (1, Some(5)), (3, None)]
    );
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(115)
    );
    assert_eq!(
        preview
            .store()
            .folder_has_children(context_id(0), EntryId(2))
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        preview
            .store()
            .folder_has_children(context_id(0), EntryId(1))
            .unwrap(),
        Some(false)
    );
    let mut nested_order = TreeBranchOrder::start(
        preview.store(),
        context_id(0),
        view(),
        key(2),
        temp.path(),
        temp.path(),
        4 * 1024 * 1024,
    )
    .unwrap();
    while !nested_order
        .step(preview.store(), |id| preview.measure(id))
        .unwrap()
    {}
    let rows = nested_order.page(None).unwrap().rows;
    assert_eq!(
        (rows.len(), rows[0].key.entry, rows[0].logical),
        (1, nested, Some(100))
    );
    drop(nested_order);
    drop(order);
    plan.restore_preview(preview).unwrap();
}

#[test]
fn targeted_tree_expansion_hint_is_explicitly_unknown_after_256_files() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    let mut observations = (0..256)
        .map(|i| observation(&format!("file-{i:03}"), Kind::File, Some(1)))
        .collect::<Vec<_>>();
    observations.push(observation("late-folder", Kind::Directory, None));
    batch(&mut store, attempt_id(0, 0, 1), 0, None, &observations).unwrap();
    assert_eq!(
        store
            .folder_has_children(context_id(0), EntryId(0))
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .folder_has_children(context_id(0), EntryId(257))
            .unwrap(),
        Some(false)
    );
    let mut order = TreeBranchOrder::start(
        &store,
        context_id(0),
        view(),
        key(0),
        temp.path(),
        temp.path(),
        4 * 1024 * 1024,
    )
    .unwrap();
    assert!(!order.step(&store, |_| Ok(None)).unwrap());
    assert!(order.page(None).unwrap().rows.is_empty());
    while !order.step(&store, |_| Ok(None)).unwrap() {}
    assert_eq!(order.page(None).unwrap().rows[0].key.entry, EntryId(257));
}

#[test]
fn targeted_tree_small_quota_rolls_back_unpublished_work() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    let observations = (0..32)
        .map(|i| {
            observation(
                &format!("folder-{i:02}-{}", "x".repeat(4096)),
                Kind::Directory,
                None,
            )
        })
        .collect::<Vec<_>>();
    batch(&mut store, attempt_id(0, 0, 1), 0, None, &observations).unwrap();
    let mut order = TreeBranchOrder::start(
        &store,
        context_id(0),
        view(),
        key(0),
        temp.path(),
        temp.path(),
        64 * 1024,
    )
    .unwrap();
    for _ in 0..2 {
        assert!(order.step(&store, |_| Ok(None)).is_err());
        assert!(!order.ready());
        assert!(order.page(None).is_err());
        assert!(order.db.is_autocommit());
        assert!(order.temporary_usage().peak <= 64 * 1024);
    }
}

#[test]
fn targeted_tree_certified_generation_remains_readable_and_context_is_fenced() {
    use crate::inventory::tests::{fold, reconcile};
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        Some(ListingEnd::Exhausted),
        &[
            observation("folder", Kind::Directory, None),
            observation("file", Kind::File, Some(10)),
        ],
    )
    .unwrap();
    reconcile(&mut store, attempt_id(0, 0, 1));
    store.begin_attempt(attempt_id(0, 1, 1)).unwrap();
    batch(
        &mut store,
        attempt_id(0, 1, 1),
        0,
        Some(ListingEnd::Exhausted),
        &[],
    )
    .unwrap();
    reconcile(&mut store, attempt_id(0, 1, 1));
    fold(&mut store, 0);
    store.seal(context_id(0)).unwrap();
    assert_eq!(
        store
            .folder_has_children(context_id(0), EntryId(0))
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        store
            .folder_has_children(context_id(0), EntryId(1))
            .unwrap(),
        Some(false)
    );
    let mut wrong = context_id(0);
    wrong.inventory = InventoryId(1);
    assert!(store.folder_has_children(wrong, EntryId(0)).is_err());
    let mut wrong = context_id(0);
    wrong.scan = ScanId(999);
    assert!(store.folder_has_children(wrong, EntryId(0)).is_err());
    let mut order = TreeBranchOrder::start(
        &store,
        context_id(0),
        view(),
        key(0),
        temp.path(),
        temp.path(),
        4 * 1024 * 1024,
    )
    .unwrap();
    while !order.step(&store, |_| Ok(None)).unwrap() {}
    assert_eq!(order.page(None).unwrap().rows[0].key.entry, EntryId(1));
}
