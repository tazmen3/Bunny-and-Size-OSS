use super::*;
use crate::inventory::{
    generation_a::{CertifiedGeneration, Repository},
    tests::{attempt_id, batch, context_id, fold, reconcile},
};
use sortsize_core::resources::Governor;
use std::{collections::BTreeMap, sync::Mutex};
static SERIAL: Mutex<()> = Mutex::new(());
fn fixture(temp: &Path, count: usize) -> (CertifiedGeneration, Repository) {
    let c = context_id(1);
    let mut source = Store::create(
        &temp.join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    source.create_generation(c).unwrap();
    let root = temp.join("offline");
    source.record_acquisition(c, &root, 100).unwrap();
    let a = attempt_id(1, 0, 1);
    source.begin_attempt(a).unwrap();
    let mut sequence = 0;
    for start in (0..count).step_by(128) {
        let observations = (start..(start + 128).min(count))
            .map(|i| Observation {
                name: queries::native_path(Path::new(&format!("directory-{i:08}"))),
                kind: Kind::Directory,
                logical: None,
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        batch(&mut source, a, sequence, None, &observations).unwrap();
        sequence += 1;
    }
    batch(&mut source, a, sequence, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut source, a);
    // Folder branches become current only after their own attempt is known.
    // Enumerate each empty child through the real acquisition protocol.
    for id in 1..=count as u64 {
        let child = attempt_id(1, id, 1);
        source.begin_attempt(child).unwrap();
        batch(&mut source, child, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
        reconcile(&mut source, child);
    }
    fold(&mut source, 1);
    source.seal(c).unwrap();
    source
        .record_measurement(c.generation, &root, 100, 101)
        .unwrap();
    let mut repo = Repository::open(&temp.join("repo"), c.inventory, c.source).unwrap();
    assert!(
        repo.adopt_legacy(
            &source,
            c.generation,
            &root,
            32 * 1024 * 1024,
            &Default::default()
        )
        .unwrap()
        .committed()
    );
    drop(source);
    let cap = repo
        .open_generation(c.generation, &Default::default())
        .unwrap();
    (cap, repo)
}
fn snapshot(path: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect()
}
#[test]
fn ro_folder_projection_is_external_charged_until_close_and_preserves_authority() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, _repo) = fixture(temp.path(), 1024);
    let before = snapshot(cap.directory());
    let governor = Governor::global();
    let reserved = governor.usage().disk_reserved;
    let mut first = FolderFold::default();
    assert!(
        !cap.as_store()
            .folder_fold_step_at(GenerationId(1), &mut first, temp.path())
            .unwrap()
    );
    let derived = cap
        .as_store()
        .folder_projection
        .borrow()
        .as_ref()
        .unwrap()
        .space
        .path()
        .to_owned();
    assert!(!derived.starts_with(cap.directory()));
    assert_eq!(governor.usage().disk_reserved, reserved + 512 * 1024 * 1024);
    while !cap
        .as_store()
        .folder_fold_step_at(GenerationId(1), &mut first, temp.path())
        .unwrap()
    {}
    let usage = cap.as_store().folder_temporary_usage().unwrap();
    assert!(usage.charged > 0 && usage.charged <= 512 * 1024 * 1024);
    assert_eq!(
        cap.as_store()
            .folder_measure(GenerationId(1), EntryId(0), &first)
            .unwrap()
            .unwrap()
            .directories,
        1024
    );
    assert_eq!(before, snapshot(cap.directory()));
    let page_size: i64 = cap
        .as_store()
        .folder_projection
        .borrow()
        .as_ref()
        .unwrap()
        .db
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .unwrap();
    let max_pages: i64 = cap
        .as_store()
        .folder_projection
        .borrow()
        .as_ref()
        .unwrap()
        .db
        .pragma_query_value(None, "max_page_count", |r| r.get(0))
        .unwrap();
    assert_eq!(page_size * max_pages, 128 * 1024 * 1024);
    first = FolderFold::default();
    assert_eq!(first.owner, 0);
    assert!(derived.exists());
    assert_eq!(governor.usage().disk_reserved, reserved + 512 * 1024 * 1024);
    drop(cap);
    assert!(!derived.exists());
    assert_eq!(governor.usage().disk_reserved, reserved);
}
#[test]
fn old_completed_fold_and_foreign_store_fold_never_observe_partial_replacement() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, _repo) = fixture(temp.path(), 1024);
    let mut first = FolderFold::default();
    while !cap
        .as_store()
        .folder_fold_step(GenerationId(1), &mut first)
        .unwrap()
    {}
    let mut second = FolderFold::default();
    assert!(
        !cap.as_store()
            .folder_fold_step(GenerationId(1), &mut second)
            .unwrap()
    );
    assert!(
        cap.as_store()
            .folder_measure(GenerationId(1), EntryId(0), &first)
            .unwrap()
            .is_none()
    );
    while !cap
        .as_store()
        .folder_fold_step(GenerationId(1), &mut second)
        .unwrap()
    {}
    let other = _repo
        .open_generation(GenerationId(1), &Default::default())
        .unwrap();
    let mut other_fold = FolderFold::default();
    while !other
        .as_store()
        .folder_fold_step(GenerationId(1), &mut other_fold)
        .unwrap()
    {}
    assert!(
        other
            .as_store()
            .folder_measure(GenerationId(1), EntryId(0), &second)
            .unwrap()
            .is_none()
    );
    assert!(
        cap.as_store()
            .folder_measure(GenerationId(1), EntryId(0), &other_fold)
            .unwrap()
            .is_none()
    );
}
#[test]
fn folder_vfs_full_rolls_back_quantum_and_preserves_cursor_and_ro_generation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, _repo) = fixture(temp.path(), 4096);
    let before = snapshot(cap.directory());
    // Admit the same RAM class through the public method before replacing only
    // this test's disk allowance. Production always uses the 512 MiB envelope.
    let mut initial = FolderFold::default();
    cap.as_store()
        .folder_fold_step_at(GenerationId(1), &mut initial, temp.path())
        .unwrap();
    cap.as_store().folder_projection.borrow_mut().take();
    let limited = Projection::open_limit(cap.as_store(), temp.path(), 128 * 1024).unwrap();
    *cap.as_store().folder_projection.borrow_mut() = Some(limited);
    let mut fold = FolderFold::default();
    let mut failed = false;
    for _ in 0..32 {
        let cursor = fold.before;
        let committed = if cursor.is_some() {
            let cache = cap.as_store().folder_projection.borrow();
            Some(cache.as_ref().unwrap().db.query_row::<(i64,u64),_,_>(
                "SELECT (SELECT count(*) FROM folder_totals),dirs FROM folder_totals WHERE id=?1",
                [unsigned(0)],|r|Ok((r.get(0)?,number(r,1)?))).unwrap())
        } else {
            None
        };
        match cap
            .as_store()
            .folder_fold_step_at(GenerationId(1), &mut fold, temp.path())
        {
            Ok(false) => (),
            Ok(true) => panic!("test disk budget did not refuse growth"),
            Err(Error::Sql(e)) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) => {
                assert!(
                    cursor.is_some(),
                    "FULL must occur after a committed quantum"
                );
                assert_eq!(fold.before, cursor);
                let cache = cap.as_store().folder_projection.borrow();
                let cache = cache.as_ref().unwrap();
                assert!(cache.db.is_autocommit());
                let preserved=cache.db.query_row::<(i64,u64),_,_>(
                    "SELECT (SELECT count(*) FROM folder_totals),dirs FROM folder_totals WHERE id=?1",
                    [unsigned(0)],|r|Ok((r.get(0)?,number(r,1)?))).unwrap();
                assert_eq!(committed, Some(preserved));
                assert!(!fold.done);
                failed = true;
                break;
            }
            Err(e) => panic!("unexpected folder error: {e:?}"),
        }
    }
    assert!(failed);
    let projection = cap.as_store().folder_projection.borrow();
    let projection = projection.as_ref().unwrap();
    let usage = projection.space.usage();
    assert!(usage.charged <= 128 * 1024);
    let physical = std::fs::read_dir(projection.space.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum::<u64>();
    assert!(physical <= usage.charged);
    assert!(
        cap.as_store()
            .folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .is_none()
    );
    assert_eq!(before, snapshot(cap.directory()));
}
#[test]
fn interrupted_derived_write_keeps_last_committed_folder_quantum_and_can_resume() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, _repo) = fixture(temp.path(), 1024);
    let before = snapshot(cap.directory());
    let mut fold = FolderFold::default();
    assert!(
        !cap.as_store()
            .folder_fold_step(GenerationId(1), &mut fold)
            .unwrap()
    );
    let cursor = fold.before;
    let owner = fold.owner;
    let callbacks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = callbacks.clone();
    cap.as_store()
        .folder_projection
        .borrow_mut()
        .as_mut()
        .unwrap()
        .db
        .progress_handler(
            1,
            Some(move || observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 100),
        )
        .unwrap();
    assert!(
        cap.as_store()
            .folder_fold_step(GenerationId(1), &mut fold)
            .is_err()
    );
    cap.as_store()
        .folder_projection
        .borrow_mut()
        .as_mut()
        .unwrap()
        .db
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert_eq!(fold.before, cursor);
    assert_eq!(fold.owner, owner);
    assert!(!cap.as_store().db.is_autocommit());
    while !cap
        .as_store()
        .folder_fold_step(GenerationId(1), &mut fold)
        .unwrap()
    {}
    assert_eq!(
        cap.as_store()
            .folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .unwrap()
            .directories,
        1024
    );
    assert_eq!(before, snapshot(cap.directory()));
}
#[test]
fn folder_scratch_inside_canonical_authority_is_refused_without_creating_files() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, _repo) = fixture(temp.path(), 1);
    let before = snapshot(cap.directory());
    assert!(
        cap.as_store()
            .folder_fold_step_at(GenerationId(1), &mut FolderFold::default(), cap.directory())
            .is_err()
    );
    assert_eq!(before, snapshot(cap.directory()));
}

#[test]
fn planned_reader_lends_folder_capacity_with_full_sqlite_category() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, repo) = fixture(temp.path(), 1);
    drop(cap);
    let mut plan = crate::inventory::generation_a::Pipeline::plan().unwrap();
    let cap = repo
        .open_generation_with_plan(GenerationId(1), &mut plan, &Default::default())
        .unwrap();
    let governor = Governor::global();
    let usage = governor.usage();
    let index = sortsize_core::resources::Category::Sqlite as usize;
    let _pressure = governor
        .reserve(
            sortsize_core::resources::Category::Sqlite,
            usage.limits.categories[index] - usage.categories[index],
            None,
        )
        .unwrap();
    let before = governor.usage().categories[index];
    let mut fold = FolderFold::default();
    while !cap
        .as_store()
        .folder_fold_step_at(GenerationId(1), &mut fold, temp.path())
        .unwrap()
    {}
    assert_eq!(governor.usage().categories[index], before);
    assert!(
        cap.as_store()
            .folder_measure(GenerationId(1), EntryId(0), &fold)
            .unwrap()
            .is_some()
    );
}

#[test]
fn derived_sqlite_subjournal_spill_is_owned_charged_and_removed() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let (cap, _repo) = fixture(temp.path(), 1);
    let before = snapshot(cap.directory());
    let mut fold = FolderFold::default();
    while !cap
        .as_store()
        .folder_fold_step_at(GenerationId(1), &mut fold, temp.path())
        .unwrap()
    {}
    let cache = cap.as_store().folder_projection.borrow();
    let cache = cache.as_ref().unwrap();
    cache.db.execute_batch("CREATE TABLE spill(id INTEGER PRIMARY KEY, payload BLOB); WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<256) INSERT INTO spill SELECT i,zeroblob(8192) FROM n;").unwrap();
    cache.db.execute_batch("BEGIN;UPDATE spill SET payload=randomblob(8192) WHERE id=1; SAVEPOINT spill_inner; UPDATE spill SET payload=randomblob(8192); ROLLBACK TO spill_inner;RELEASE spill_inner;COMMIT;").unwrap();
    assert!(cache.space.transients_created() > 0);
    let usage = cache.space.usage();
    assert!(usage.charged <= usage.limit && usage.peak > 0);
    assert!(!std::fs::read_dir(cache.space.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".sqlite-temp-")
    }));
    assert_eq!(before, snapshot(cap.directory()));
}

#[test]
fn consecutive_parent_aggregation_preserves_nested_interleaving_overflow_and_unknowns() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let mut s = Store::create(
        &temp.path().join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    s.create_generation(c).unwrap();
    let make = |n, kind, logical, allocated| Observation {
        name: queries::native_path(Path::new(&format!("entry-{n}"))),
        kind,
        logical,
        allocated,
        modified: None,
        metadata_known: true,
    };
    let root = attempt_id(1, 0, 1);
    s.begin_attempt(root).unwrap();
    batch(
        &mut s,
        root,
        0,
        Some(ListingEnd::Exhausted),
        &[
            make(1, Kind::Directory, None, None),
            make(2, Kind::Directory, None, None),
        ],
    )
    .unwrap();
    reconcile(&mut s, root);
    let a = attempt_id(1, 1, 1);
    let b = attempt_id(1, 2, 1);
    s.begin_attempt(a).unwrap();
    s.begin_attempt(b).unwrap();
    batch(&mut s, a, 0, None, &[make(3, Kind::Directory, None, None)]).unwrap();
    let first = (0..150)
        .map(|i| make(i + 10, Kind::File, Some(1), Some(1)))
        .collect::<Vec<_>>();
    batch(&mut s, a, 1, None, &first).unwrap();
    batch(
        &mut s,
        b,
        0,
        Some(ListingEnd::Exhausted),
        &[make(999, Kind::File, None, Some(2))],
    )
    .unwrap();
    reconcile(&mut s, b);
    let last = (150..300)
        .map(|i| make(i + 10, Kind::File, Some(1), Some(1)))
        .collect::<Vec<_>>();
    batch(&mut s, a, 2, Some(ListingEnd::Exhausted), &last).unwrap();
    reconcile(&mut s, a);
    let nested = attempt_id(1, 3, 1);
    s.begin_attempt(nested).unwrap();
    batch(
        &mut s,
        nested,
        0,
        Some(ListingEnd::Exhausted),
        &[make(1000, Kind::File, Some(u64::MAX), Some(u64::MAX))],
    )
    .unwrap();
    reconcile(&mut s, nested);
    // The optimized validator must agree with the pre-existing path for every
    // entry and directory requirement, including absent IDs.
    let mut branch = CurrentBranch::new(&s.db, c.generation).unwrap();
    for id in 0..306 {
        for directory in [false, true] {
            assert_eq!(
                branch.check(EntryId(id), directory).unwrap(),
                current_branch(&s.db, c.generation, EntryId(id), directory).unwrap()
            );
        }
    }
    drop(branch);
    let mut fold = FolderFold::default();
    let mut steps = 1;
    while !s
        .folder_fold_step_at(c.generation, &mut fold, temp.path())
        .unwrap()
    {
        steps += 1;
    }
    assert!(steps >= 2); // the consecutive run crosses the 256-row quantum.
    let root = s
        .folder_measure(c.generation, EntryId(0), &fold)
        .unwrap()
        .unwrap();
    assert_eq!(
        root,
        FolderMeasure {
            logical: None,
            unknown: 1,
            allocated: None,
            allocation_unknown: 0,
            allocation_lower_bound: false,
            files: 302,
            directories: 3
        }
    );
    let a = s
        .folder_measure(c.generation, EntryId(1), &fold)
        .unwrap()
        .unwrap();
    assert_eq!(
        (a.logical, a.unknown, a.allocated, a.files, a.directories),
        (None, 0, None, 301, 1)
    );
    let b = s
        .folder_measure(c.generation, EntryId(2), &fold)
        .unwrap()
        .unwrap();
    assert_eq!(
        (b.logical, b.unknown, b.allocated, b.files, b.directories),
        (None, 1, Some(2), 1, 0)
    );
    let nested = s
        .folder_measure(c.generation, EntryId(3), &fold)
        .unwrap()
        .unwrap();
    assert_eq!(
        (nested.logical, nested.allocated, nested.files),
        (Some(u64::MAX), Some(u64::MAX), 1)
    );
}
