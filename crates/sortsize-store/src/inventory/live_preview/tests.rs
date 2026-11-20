use super::*;
use crate::inventory::{
    generation_a::Pipeline,
    tests::{attempt_id, batch, context_id, observation},
};
use std::sync::Mutex;
static SERIAL: Mutex<()> = Mutex::new(());
impl LivePreview {
    // Force mid-file/mid-scope continuations cheaply in the relevant fixtures.
    fn small_step(&mut self, scratch: &Path) -> Result<bool> {
        self.step_with_budget(None, scratch, &Governor::global(), 256)
    }
    fn small_step_folder(&mut self, parent: EntryId, scratch: &Path) -> Result<bool> {
        self.step_with_budget(Some(parent), scratch, &Governor::global(), 256)
    }
}
fn source(temp: &Path) -> Store {
    let c = context_id(0);
    let mut store = Store::create(
        &temp.join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    store.create_generation(c).unwrap();
    store.begin_attempt(attempt_id(0, 0, 1)).unwrap();
    store
}
fn add_files(
    store: &mut Store,
    parent: u64,
    sequence: u64,
    start: u64,
    count: u64,
    size: Option<u64>,
) {
    let files = (start..start + count)
        .map(|id| {
            let mut o = observation(id, Kind::File);
            o.logical = size;
            o
        })
        .collect::<Vec<_>>();
    batch(store, attempt_id(0, parent, 1), sequence, None, &files).unwrap();
}
#[test]
fn live_preview_advances_past_256_while_writer_revisions_change_and_refreshes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[observation(1, Kind::Directory)],
    )
    .unwrap();
    store.begin_attempt(attempt_id(0, 1, 1)).unwrap();
    add_files(&mut store, 1, 0, 10, 300, Some(7));
    add_files(&mut store, 1, 1, 310, 300, Some(7));
    let mut plan = Pipeline::plan().unwrap();
    let admitted = plan.remaining().reader;
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    assert_eq!(plan.remaining().reader, 0);
    let revision = preview.revision();
    assert!(!preview.small_step(temp.path()).unwrap());
    let first = preview.measure(EntryId(0)).unwrap().unwrap();
    assert!(first.logical.unwrap() > 0 && first.logical.unwrap() < 4200);
    assert_eq!(first, preview.measure(EntryId(1)).unwrap().unwrap());
    add_files(&mut store, 1, 2, 610, 1, Some(700));
    assert_ne!(store.revision(GenerationId(0)).unwrap(), revision);
    let mut quanta = 1;
    while !preview.small_step(temp.path()).unwrap() {
        quanta += 1;
        assert!(quanta < 100);
    }
    assert!(quanta > 1);
    assert_eq!(preview.revision(), revision);
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(4200)
    );
    assert_eq!(preview.store().entry_count(GenerationId(0)).unwrap(), 602);
    assert_eq!(store.entry_count(GenerationId(0)).unwrap(), 603);
    assert!(
        preview
            .store()
            .folder_measure(GenerationId(0), EntryId(0), &Default::default())
            .unwrap()
            .is_none()
    );
    assert!(preview.store().db.is_readonly("main").unwrap());
    assert!(!preview.store().db.is_autocommit());
    plan.restore_preview(preview).unwrap();
    assert_eq!(plan.remaining().reader, admitted);
    let mut refreshed = plan.live_preview(&store, context_id(0)).unwrap();
    while !refreshed.step(temp.path()).unwrap() {}
    assert_eq!(
        refreshed.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(4900)
    );
    plan.restore_preview(refreshed).unwrap();
}
#[test]
fn live_preview_unknown_zero_overflow_and_failed_open_keep_their_meanings() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[
            observation(1, Kind::Directory),
            observation(2, Kind::Directory),
            observation(3, Kind::Directory),
        ],
    )
    .unwrap();
    for parent in 1..=3 {
        store.begin_attempt(attempt_id(0, parent, 1)).unwrap();
    }
    add_files(&mut store, 1, 0, 10, 1, None);
    add_files(&mut store, 2, 0, 11, 1, Some(0));
    add_files(&mut store, 3, 0, 12, 2, Some(u64::MAX));
    let mut plan = Pipeline::plan().unwrap();
    let admitted = plan.remaining().reader;
    assert!(plan.live_preview(&store, context_id(99)).is_err());
    assert_eq!(plan.remaining().reader, admitted);
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    while !preview.step(temp.path()).unwrap() {}
    let unknown = preview.measure(EntryId(1)).unwrap().unwrap();
    assert_eq!((unknown.logical, unknown.unknown), (None, 1));
    assert_eq!(
        preview.measure(EntryId(2)).unwrap().unwrap().logical,
        Some(0)
    );
    assert_eq!(preview.measure(EntryId(3)).unwrap().unwrap().logical, None);
    assert_eq!(preview.measure(EntryId(0)).unwrap().unwrap().logical, None);
    plan.restore_preview(preview).unwrap();
}
#[test]
fn live_preview_owns_lease_after_writer_drop_and_returns_it_on_close() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(0);
    let mut store = Store::create_canonical_admitted(
        &temp.path().join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
        Store::reserve_memory().unwrap(),
        64 * 1024 * 1024,
        None,
    )
    .unwrap();
    store.create_generation(c).unwrap();
    store.begin_attempt(attempt_id(0, 0, 1)).unwrap();
    add_files(&mut store, 0, 0, 1, 1, Some(12));
    let path = store.directory().to_owned();
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    drop(store);
    assert!(Store::reopen(&path, Durability::Retained).is_err());
    while !preview.step(temp.path()).unwrap() {}
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(12)
    );
    plan.restore_preview(preview).unwrap();
    assert!(Store::reopen(&path, Durability::Retained).is_ok());
}
#[test]
fn live_preview_deep_ancestor_quantum_never_exposes_half_a_file() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    for parent in 0..300 {
        batch(
            &mut store,
            attempt_id(0, parent, 1),
            0,
            None,
            &[observation(parent + 1, Kind::Directory)],
        )
        .unwrap();
        store.begin_attempt(attempt_id(0, parent + 1, 1)).unwrap();
    }
    add_files(&mut store, 300, 0, 400, 1, Some(31));
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    let mut quanta = 0;
    loop {
        let done = preview.small_step(temp.path()).unwrap();
        quanta += 1;
        let root = preview.measure(EntryId(0)).unwrap().and_then(|m| m.logical);
        let child = preview.measure(EntryId(1)).unwrap().and_then(|m| m.logical);
        assert_eq!(root, child);
        assert!(root.is_none() || root == Some(31));
        if done {
            break;
        }
        assert!(quanta < 30);
    }
    assert!(quanta >= 3);
    plan.restore_preview(preview).unwrap();
    // The production budget can apply more than 256 distinct ancestor deltas
    // in one quantum. Its small measure cache must fall back without loss.
    let mut larger = plan.live_preview(&store, context_id(0)).unwrap();
    loop {
        let done = larger.step(temp.path()).unwrap();
        let root = larger.measure(EntryId(0)).unwrap().and_then(|m| m.logical);
        let child = larger.measure(EntryId(1)).unwrap().and_then(|m| m.logical);
        assert_eq!(root, child);
        assert!(root.is_none() || root == Some(31));
        if done {
            break;
        }
    }
    for id in 0..=300 {
        let measure = larger.measure(EntryId(id)).unwrap().unwrap();
        assert_eq!((measure.logical, measure.files), (Some(31), 1));
    }
    plan.restore_preview(larger).unwrap();
}

#[test]
fn live_preview_failed_quantum_keeps_its_cursor_and_measures_then_resumes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    add_files(&mut store, 0, 0, 1, 300, Some(3));
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    assert!(!preview.small_step(temp.path()).unwrap());
    let before = preview.measure(EntryId(0)).unwrap();
    let cursor = preview.after;
    let seen_before: i64 = preview
        .projection
        .as_ref()
        .unwrap()
        .db
        .query_row("SELECT count(*) FROM preview_seen", [], |row| row.get(0))
        .unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    preview
        .projection
        .as_mut()
        .unwrap()
        .db
        .progress_handler(
            1,
            Some(move || observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 100),
        )
        .unwrap();
    assert!(preview.step(temp.path()).is_err());
    // The same persistent interruption may reject the Drop rollback too. A
    // second call must neither admit a new quantum nor expose uncertain sums.
    assert!(preview.step(temp.path()).is_err());
    assert!(preview.measure(EntryId(0)).is_err());
    assert_eq!(preview.after, cursor);
    preview
        .projection
        .as_mut()
        .unwrap()
        .db
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert_eq!(preview.after, cursor);
    assert_eq!(preview.measure(EntryId(0)).unwrap(), before);
    let db = &preview.projection.as_ref().unwrap().db;
    assert!(db.is_autocommit());
    assert_eq!(
        db.query_row("SELECT count(*) FROM preview_seen", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        seen_before
    );
    while !preview.step(temp.path()).unwrap() {}
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(900)
    );
    plan.restore_preview(preview).unwrap();
}

#[test]
fn selected_scope_precedes_unrelated_files_and_never_double_counts_after_navigation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[
            observation(1, Kind::Directory),
            observation(2, Kind::Directory),
        ],
    )
    .unwrap();
    store.begin_attempt(attempt_id(0, 1, 1)).unwrap();
    store.begin_attempt(attempt_id(0, 2, 1)).unwrap();
    for chunk in 0..4 {
        add_files(&mut store, 1, chunk, 10 + chunk * 250, 250, Some(3));
    }
    add_files(&mut store, 2, 0, 2000, 2, Some(11));
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    let pinned_revision = preview.revision();
    assert!(preview.small_step_folder(EntryId(2), temp.path()).unwrap());
    assert!(preview.folder_ready(EntryId(2)));
    assert!(!preview.complete());
    assert_eq!(
        preview.measure(EntryId(2)).unwrap().unwrap().logical,
        Some(22)
    );
    // Scope is stable even though acquisition adds newer rows independently.
    add_files(&mut store, 2, 1, 2002, 1, Some(100));
    assert_eq!(preview.revision(), pinned_revision);
    assert!(preview.step_folder(EntryId(1003), temp.path()).is_err());
    let mut ready = false;
    for _ in 0..100 {
        ready = preview.step_folder(EntryId(1), temp.path()).unwrap();
        if ready {
            break;
        }
    }
    assert!(ready);
    assert_eq!(
        preview.measure(EntryId(1)).unwrap().unwrap().logical,
        Some(3000)
    );
    assert!(preview.step_folder(EntryId(2), temp.path()).unwrap());
    while !preview.step(temp.path()).unwrap() {}
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(3022)
    );
    assert_eq!(preview.measure(EntryId(0)).unwrap().unwrap().files, 1002);
    assert_eq!(
        preview.measure(EntryId(2)).unwrap().unwrap().logical,
        Some(22)
    );
    plan.restore_preview(preview).unwrap();
}

#[test]
fn priority_change_during_ancestor_application_preserves_one_contribution() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[
            observation(1, Kind::Directory),
            observation(2, Kind::Directory),
        ],
    )
    .unwrap();
    store.begin_attempt(attempt_id(0, 1, 1)).unwrap();
    store.begin_attempt(attempt_id(0, 2, 1)).unwrap();
    let mut parent = 1;
    for depth in 0..300 {
        batch(
            &mut store,
            attempt_id(0, parent, 1),
            0,
            None,
            &[observation(10 + depth, Kind::Directory)],
        )
        .unwrap();
        parent = 3 + depth;
        store.begin_attempt(attempt_id(0, parent, 1)).unwrap();
    }
    add_files(&mut store, parent, 0, 1000, 1, Some(31));
    add_files(&mut store, 2, 0, 1001, 1, Some(7));
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    let mut interrupted = false;
    for _ in 0..20 {
        preview.small_step_folder(EntryId(1), temp.path()).unwrap();
        if preview
            .pending
            .as_ref()
            .is_some_and(|p| p.applying.is_some_and(|n| n > 0 && n < p.ancestors.len()))
        {
            interrupted = true;
            break;
        }
    }
    assert!(interrupted);
    for _ in 0..20 {
        if preview.small_step_folder(EntryId(2), temp.path()).unwrap() {
            break;
        }
    }
    assert!(preview.folder_ready(EntryId(2)));
    assert_eq!(
        preview.measure(EntryId(2)).unwrap().unwrap().logical,
        Some(7)
    );
    while !preview.small_step(temp.path()).unwrap() {}
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(38)
    );
    assert_eq!(preview.measure(EntryId(0)).unwrap().unwrap().files, 2);
    assert_eq!(
        preview.measure(EntryId(1)).unwrap().unwrap().logical,
        Some(31)
    );
    plan.restore_preview(preview).unwrap();
}

#[test]
fn quantum_measure_cache_accounts_capacity_and_declines_without_budget() {
    let governor = Governor::new(MemoryMode::Auto, None, 0);
    let cached = StepMeasures::new(&governor);
    assert!(cached._memory.is_some());
    assert!(cached.rows.capacity() >= MAX_CACHED_MEASURES);
    assert!(
        cached.rows.capacity() * std::mem::size_of::<(EntryId, FolderMeasure)>()
            + std::mem::size_of::<StepMeasures>()
            <= MEASURE_CACHE_BYTES as usize
    );
    assert_eq!(
        governor.usage().categories[Category::Queries as usize],
        MEASURE_CACHE_BYTES
    );
    drop(cached);
    assert_eq!(governor.usage().categories[Category::Queries as usize], 0);

    let limit = governor.usage().limits.categories[Category::Queries as usize];
    let held = governor.reserve(Category::Queries, limit, None).unwrap();
    let refused = StepMeasures::new(&governor);
    assert!(refused._memory.is_none());
    assert_eq!(refused.rows.capacity(), 0);
    assert_eq!(
        governor.usage().categories[Category::Queries as usize],
        limit
    );
    drop((refused, held));
    assert_eq!(governor.usage().categories[Category::Queries as usize], 0);
}

#[test]
fn large_operation_budget_still_flushes_at_most_256_cached_measures() {
    // Enough distinct measures to exercise both paths, independent of the
    // production operation ceiling and machine-dependent 20ms cutoffs.
    const OPERATIONS: usize = 4096;
    let governor = Governor::new(MemoryMode::Auto, None, 0);
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE folder_totals(id BLOB PRIMARY KEY,logical BLOB,unknown_count BLOB,files BLOB,dirs BLOB,allocated BLOB,allocation_unknown BLOB,allocation_lower_bound INTEGER) WITHOUT ROWID;").unwrap();
    let tx = db.transaction().unwrap();
    let mut measures = StepMeasures::new(&governor);
    let value = FolderMeasure {
        logical: Some(7),
        files: 1,
        ..Default::default()
    };
    for id in 0..OPERATIONS {
        measures.write(&tx, EntryId(id as u64), value).unwrap();
    }
    assert_eq!(measures.rows.len(), MAX_CACHED_MEASURES);
    assert_eq!(
        governor.usage().categories[Category::Queries as usize],
        MEASURE_CACHE_BYTES
    );
    let before: i64 = tx
        .query_row("SELECT count(*) FROM folder_totals", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before as usize, OPERATIONS - MAX_CACHED_MEASURES);
    assert_eq!(measures.read(&tx, EntryId(0)).unwrap(), Some(value));
    assert_eq!(
        measures
            .read(&tx, EntryId((OPERATIONS - 1) as u64))
            .unwrap(),
        Some(value)
    );
    measures.flush(&tx).unwrap();
    let after: i64 = tx
        .query_row("SELECT count(*) FROM folder_totals", [], |r| r.get(0))
        .unwrap();
    assert_eq!((after - before) as usize, MAX_CACHED_MEASURES);
    assert_eq!(after as usize, OPERATIONS);
    drop(measures);
    assert_eq!(governor.usage().categories[Category::Queries as usize], 0);
    tx.rollback().unwrap();
    let remaining: i64 = db
        .query_row("SELECT count(*) FROM folder_totals", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn quantum_cache_amortizes_sibling_writes_and_matches_budget_fallback() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &(1..=4)
            .map(|id| observation(id, Kind::Directory))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    for parent in 1..=4 {
        store.begin_attempt(attempt_id(0, parent, 1)).unwrap();
    }
    add_files(&mut store, 1, 0, 10, 300, Some(3));
    add_files(&mut store, 2, 0, 400, 1, None);
    add_files(&mut store, 3, 0, 401, 1, Some(0));
    add_files(&mut store, 4, 0, 402, 2, Some(u64::MAX));
    let run = |governor: &Governor| {
        let mut plan = Pipeline::plan().unwrap();
        let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
        assert!(
            !preview
                .step_with_budget(None, temp.path(), governor, 256)
                .unwrap()
        );
        preview.projection.as_ref().unwrap().db.execute_batch(
            "CREATE TABLE measure_writes(n INTEGER NOT NULL); INSERT INTO measure_writes VALUES(0);
             CREATE TRIGGER count_measure_writes AFTER INSERT ON folder_totals
             BEGIN UPDATE measure_writes SET n=n+1; END;",
        ).unwrap();
        let mut quanta = 0;
        while !preview
            .step_with_governor(None, temp.path(), governor)
            .unwrap()
        {
            quanta += 1;
            assert!(quanta < 100);
        }
        let values: Vec<_> = (0..=4)
            .map(|id| preview.measure(EntryId(id)).unwrap())
            .collect();
        let writes: i64 = preview
            .projection
            .as_ref()
            .unwrap()
            .db
            .query_row("SELECT n FROM measure_writes", [], |row| row.get(0))
            .unwrap();
        plan.restore_preview(preview).unwrap();
        assert_eq!(governor.usage().categories[Category::Queries as usize], 0);
        (values, writes)
    };
    let admitted = Governor::new(MemoryMode::Auto, None, 0);
    let refused = Governor::new(
        MemoryMode::Auto,
        Some(sortsize_core::resources::MemoryReading {
            total: 1,
            available: 0,
        }),
        0,
    );
    let (cached, cached_writes) = run(&admitted);
    let (fallback, fallback_writes) = run(&refused);
    assert_eq!(cached, fallback);
    assert_eq!(cached[1].unwrap().logical, Some(900));
    assert_eq!(
        (cached[2].unwrap().logical, cached[2].unwrap().unknown),
        (None, 1)
    );
    assert_eq!(cached[3].unwrap().logical, Some(0));
    assert_eq!(cached[4].unwrap().logical, None);
    assert_eq!(cached[0].unwrap().files, 304);
    assert!(
        cached_writes > 0 && cached_writes * 2 < fallback_writes,
        "cached={cached_writes}, immediate={fallback_writes}"
    );
}

#[test]
fn failed_late_measure_flush_rolls_back_sql_and_continuation_before_retry() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[observation(1, Kind::Directory)],
    )
    .unwrap();
    store.begin_attempt(attempt_id(0, 1, 1)).unwrap();
    add_files(&mut store, 1, 0, 10, 300, Some(7));
    let governor = Governor::new(MemoryMode::Auto, None, 0);
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    assert!(
        !preview
            .step_with_budget(Some(EntryId(1)), temp.path(), &governor, 256)
            .unwrap()
    );
    let continuation = (
        preview.after,
        preview.pending.clone(),
        preview.selected,
        preview.priority_done,
        preview.complete,
    );
    let contents = |preview: &LivePreview| {
        let db = &preview.projection.as_ref().unwrap().db;
        [
            "folder_totals ORDER BY id",
            "preview_seen ORDER BY id",
            "preview_queue ORDER BY scope,id",
            "preview_scopes ORDER BY id",
        ]
        .map(|table| {
            let mut query = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = query.column_count();
            query
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        })
    };
    let before = contents(&preview);
    preview
        .projection
        .as_ref()
        .unwrap()
        .db
        .execute_batch(
            "CREATE TABLE flush_audit(n INTEGER NOT NULL); INSERT INTO flush_audit VALUES(0);
         CREATE TRIGGER count_flush AFTER INSERT ON folder_totals
         BEGIN UPDATE flush_audit SET n=n+1; END;
         CREATE TRIGGER fail_late_flush BEFORE INSERT ON folder_totals
         WHEN (SELECT n FROM flush_audit)>0
         BEGIN SELECT RAISE(ABORT,'late measure flush'); END;",
        )
        .unwrap();
    let failure = preview
        .step_with_governor(Some(EntryId(1)), temp.path(), &governor)
        .unwrap_err();
    assert!(failure.to_string().contains("late measure flush"));
    assert_eq!(contents(&preview), before);
    assert_eq!(
        (
            preview.after,
            preview.pending.clone(),
            preview.selected,
            preview.priority_done,
            preview.complete
        ),
        continuation
    );
    assert_eq!(governor.usage().categories[Category::Queries as usize], 0);
    let db = &preview.projection.as_ref().unwrap().db;
    assert_eq!(
        db.query_row("SELECT n FROM flush_audit", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(db.is_autocommit());
    db.execute_batch("DROP TRIGGER fail_late_flush;").unwrap();
    while !preview
        .step_with_governor(None, temp.path(), &governor)
        .unwrap()
    {}
    assert_eq!(
        preview.measure(EntryId(0)).unwrap(),
        preview.measure(EntryId(1)).unwrap()
    );
    assert_eq!(
        preview.measure(EntryId(0)).unwrap().unwrap().logical,
        Some(2100)
    );
    assert_eq!(preview.measure(EntryId(0)).unwrap().unwrap().files, 300);
    plan.restore_preview(preview).unwrap();
}

#[test]
fn empty_directory_queue_prepares_child_lookup_per_quantum_not_per_directory() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    let directories: Vec<_> = (1..=600)
        .map(|id| observation(id, Kind::Directory))
        .collect();
    for (sequence, chunk) in directories.chunks(300).enumerate() {
        batch(
            &mut store,
            attempt_id(0, 0, 1),
            sequence as u64,
            None,
            chunk,
        )
        .unwrap();
    }
    let mut plan = Pipeline::plan().unwrap();
    let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
    let preparations = Arc::new(AtomicUsize::new(0));
    let observed = preparations.clone();
    // The authorizer runs during compilation, not on every row/VM step.
    // Both the child selector and ancestor lookup read entries.parent.
    preview
        .store
        .db
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "entries",
                    column_name: "parent"
                }
            ) {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            Authorization::Allow
        }))
        .unwrap();
    let mut quanta = 0;
    while !preview.complete() {
        preview.step_folder(EntryId(0), temp.path()).unwrap();
        quanta += 1;
        assert!(quanta < 3000);
    }
    let compiled = preparations.load(Ordering::Relaxed);
    assert!(
        compiled >= quanta,
        "authorizer must observe actual preparations"
    );
    assert!(
        compiled <= 4 * quanta + 4,
        "preparations={compiled}, quanta={quanta}"
    );
    assert!(preview.folder_ready(EntryId(0)));
    assert_eq!(preview.measure(EntryId(0)).unwrap(), None);
    preview
        .store
        .db
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    plan.restore_preview(preview).unwrap();
}

#[test]
fn reused_statements_preserve_each_branch_visibility_and_budget_fallback() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    let directories: Vec<_> = (1..=5).map(|id| observation(id, Kind::Directory)).collect();
    batch(&mut store, attempt_id(0, 0, 1), 0, None, &directories).unwrap();
    for parent in 1..=5 {
        store.begin_attempt(attempt_id(0, parent, 1)).unwrap();
        add_files(&mut store, parent, 0, parent * 100, 20, Some(3));
    }
    store
        .db
        .execute(
            "UPDATE entries SET present=0 WHERE generation=?1 AND id=?2",
            params![unsigned(0), unsigned(2)],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE tasks SET key=NULL WHERE generation=?1 AND directory=?2 AND kind=0",
            params![unsigned(0), unsigned(3)],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE tasks SET parent_attempt=?3 WHERE generation=?1 AND directory=?2 AND kind=0",
            params![unsigned(0), unsigned(4), unsigned(2)],
        )
        .unwrap();
    let stale_file = store
        .lookup(
            GenerationId(0),
            EntryId(5),
            &observation(500, Kind::File).name,
        )
        .unwrap()
        .unwrap()
        .key
        .entry;
    assert_eq!(
        store
            .db
            .execute(
                "UPDATE entries SET attempt=?3 WHERE generation=?1 AND id=?2",
                params![unsigned(0), unsigned(stale_file.0), unsigned(2)]
            )
            .unwrap(),
        1
    );
    let run = |governor: &Governor| {
        let mut plan = Pipeline::plan().unwrap();
        let mut preview = plan.live_preview(&store, context_id(0)).unwrap();
        let mut quanta = 0;
        while !preview.complete() {
            preview
                .step_with_governor(Some(EntryId(0)), temp.path(), governor)
                .unwrap();
            quanta += 1;
            assert!(quanta < 100);
        }
        let measures: Vec<_> = (0..=5)
            .map(|id| preview.measure(EntryId(id)).unwrap())
            .collect();
        plan.restore_preview(preview).unwrap();
        assert_eq!(governor.usage().categories[Category::Queries as usize], 0);
        measures
    };
    let admitted = Governor::new(MemoryMode::Auto, None, 0);
    let refused = Governor::new(
        MemoryMode::Auto,
        Some(sortsize_core::resources::MemoryReading {
            total: 1,
            available: 0,
        }),
        0,
    );
    let cached = run(&admitted);
    assert_eq!(cached, run(&refused));
    let root = cached[0].unwrap();
    assert_eq!((root.logical, root.files), (Some(117), 39));
    assert_eq!(cached[1].unwrap().logical, Some(60));
    assert_eq!(&cached[2..=4], &[None, None, None]);
    assert_eq!(cached[5].unwrap().logical, Some(57));
}

#[test]
fn exhausted_child_lookup_does_not_scan_unrelated_generation_tail() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let mut store = source(temp.path());
    batch(
        &mut store,
        attempt_id(0, 0, 1),
        0,
        None,
        &[
            observation(1, Kind::Directory),
            observation(2, Kind::Directory),
        ],
    )
    .unwrap();
    store.begin_attempt(attempt_id(0, 1, 1)).unwrap();
    store.begin_attempt(attempt_id(0, 2, 1)).unwrap();
    add_files(&mut store, 1, 0, 10, 4, Some(7));
    for sequence in 0..8 {
        add_files(&mut store, 2, sequence, 100 + sequence * 512, 512, Some(11));
    }
    let last = store
        .lookup(
            GenerationId(0),
            EntryId(1),
            &observation(13, Kind::File).name,
        )
        .unwrap()
        .unwrap()
        .key
        .entry;
    let reference_sql = "SELECT id,kind,logical FROM entries WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT 1";
    let mut candidate = store.db.prepare(CHILD_QUERY).unwrap();
    let mut reference = store.db.prepare(reference_sql).unwrap();
    let result = |query: &mut rusqlite::Statement<'_>, parent: u64, after: u64| {
        query.reset_status(rusqlite::StatementStatus::VmStep);
        let row = query
            .query_row(
                params![unsigned(0), unsigned(parent), unsigned(after)],
                |r| Ok((number(r, 0)?, r.get::<_, i64>(1)?, opt_number(r, 2)?)),
            )
            .optional()
            .unwrap();
        (row, query.get_status(rusqlite::StatementStatus::VmStep))
    };
    // Includes a hit, the last child, exhausted siblings, and a missing parent.
    // Each query must remain a bounded range seek despite 4096 later entries.
    for (parent, after) in [(1, 0), (1, last.0 - 1), (1, last.0), (9999, 0)] {
        let (actual, vm_steps) = result(&mut candidate, parent, after);
        let (expected, _) = result(&mut reference, parent, after);
        assert_eq!(actual, expected);
        assert!(
            (1..=128).contains(&vm_steps),
            "parent={parent} after={after}: {vm_steps} VM steps for one child"
        );
    }
}
