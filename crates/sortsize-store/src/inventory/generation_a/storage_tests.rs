use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
use sortsize_core::resources::{Governor, MemoryReading};
use std::sync::Mutex;
static SERIAL: Mutex<()> = Mutex::new(());
fn add(store: &mut Store, g: u64, name: &str, logical: u64, root: &Path) {
    let c = context_id(g);
    store.create_generation(c).unwrap();
    if store
        .db
        .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
        .unwrap()
        == 6
    {
        store.set_collection_options(c, true, false).unwrap();
    }
    let a = attempt_id(g, 0, 1);
    store.begin_attempt(a).unwrap();
    let native = queries::native_path(Path::new(name));
    batch(
        store,
        a,
        0,
        None,
        &[Observation {
            name: native,
            kind: Kind::File,
            logical: Some(logical),
            allocated: Some(logical),
            modified: None,
            metadata_known: true,
        }],
    )
    .unwrap();
    batch(store, a, 1, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(store, a);
    fold(store, g);
    store.seal(c).unwrap();
    if store
        .db
        .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
        .unwrap()
        >= 3
    {
        store
            .record_measurement(GenerationId(g), root, 10 * g as i64, 10 * g as i64 + 1)
            .unwrap();
    }
}
#[test]
fn extraction_contains_only_selected_generation_with_v6_metadata() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("offline");
    let c = context_id(1);
    let mut source = Store::create(
        &tmp.path().join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    source.enable_properties().unwrap();
    add(&mut source, 1, "older-native.txt", 101, &root);
    add(&mut source, 2, "newer-native.txt", 202, &root);
    let published = tmp.path().join("published");
    publish(
        &source,
        &published,
        GenerationId(2),
        &root,
        64 * FRAME,
        &Default::default(),
    )
    .unwrap();
    let cap =
        CertifiedGeneration::open(&published, GenerationId(2), &root, &Default::default()).unwrap();
    let db = &cap.store.db;
    for table in [
        "generations",
        "entries",
        "tasks",
        "contributions",
        "recovery_guard",
        "measurements",
        "entry_evidence",
        "acquisition_records",
        "collection_options",
        "entry_properties",
    ] {
        let column = if table == "generations" {
            "id"
        } else {
            "generation"
        };
        let missing: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}!=?1"),
                [unsigned(2)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(missing, 0, "{table}");
    }
    assert_eq!(cap.as_store().entry_count(GenerationId(2)).unwrap(), 2);
    assert_eq!(
        cap.as_store()
            .measurement(GenerationId(2))
            .unwrap()
            .measured,
        21
    );
    assert_eq!(
        cap.as_store().collection_options(GenerationId(2)).unwrap(),
        Some((true, false))
    );
    assert_eq!(
        db.pragma_query_value::<String, _>(None, "journal_mode", |r| r.get(0))
            .unwrap(),
        "delete"
    );
    assert_eq!(source.entry_count(GenerationId(1)).unwrap(), 2);
}
#[test]
fn repository_allocation_is_monotone_and_publication_survives_reopen() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repo");
    let c = context_id(1);
    let root = tmp.path().join("offline");
    let mut repo = Repository::open(&path, c.inventory, c.source).unwrap();
    let first = repo.next_context(c).unwrap();
    assert_eq!(first.generation.0, 1);
    let mut plan = Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(first, &mut plan).unwrap();
    let a = attempt_id(1, 0, 1);
    source.begin_attempt(a).unwrap();
    batch(&mut source, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut source, a);
    fold(&mut source, 1);
    source.seal(first).unwrap();
    assert!(
        repo.publish_with_plan(
            &source,
            first.generation,
            &root,
            64 * FRAME,
            &Default::default(),
            &mut plan
        )
        .unwrap()
        .committed()
    );
    drop(source);
    drop(plan);
    drop(repo);
    let mut repo = Repository::open_existing(&path).unwrap();
    assert_eq!(repo.identity(), (c.inventory, c.source));
    assert_eq!(repo.next_context(c).unwrap().generation.0, 2);
    let cap = repo.open_latest(&Default::default()).unwrap().unwrap();
    assert_eq!(cap.generation(), first.generation);
    assert!(repo.purge(first.generation, &Default::default()).is_err());
    drop(cap);
    assert!(repo.forget(&Default::default()).is_ok());
}
#[test]
fn pipeline_reserves_three_live_slots_under_actual_pressure() {
    let governor = Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 8 * 1024 * FRAME,
            available: 8 * 1024 * FRAME,
        }),
        1024 * FRAME,
    );
    let limit = governor.usage().limits.categories[Category::Sqlite as usize];
    let pressure = governor
        .reserve(Category::Sqlite, limit - 15 * FRAME, None)
        .unwrap();
    let before = governor.usage().categories[Category::Sqlite as usize];
    let plan = Pipeline::plan_in(&governor).unwrap();
    let bytes = plan.remaining();
    assert_eq!(bytes.reader, 2 * FRAME + FRAME / 2);
    assert!(bytes.writer >= FRAME && bytes.publication >= FRAME);
    assert_eq!(
        governor.usage().categories[Category::Sqlite as usize],
        before + bytes.reader + bytes.writer + bytes.publication
    );
    drop(plan);
    assert_eq!(
        governor.usage().categories[Category::Sqlite as usize],
        before
    );
    drop(pressure);
}

#[test]
fn pipeline_admission_includes_folder_capacity_at_exhausted_category_boundaries() {
    for publication_only in [false, true] {
        let minimum = if publication_only {
            3 * FRAME
        } else {
            4 * FRAME
        };
        for available in [minimum - 1, minimum, minimum + FRAME / 2] {
            let governor = Governor::new(
                MemoryMode::Auto,
                Some(MemoryReading {
                    total: 8 * 1024 * FRAME,
                    available: 8 * 1024 * FRAME,
                }),
                1024 * FRAME,
            );
            let limit = governor.usage().limits.categories[Category::Sqlite as usize];
            let pressure = governor
                .reserve(Category::Sqlite, limit - available, None)
                .unwrap();
            let before = governor.usage().categories[Category::Sqlite as usize];
            let plan = if publication_only {
                Pipeline::publication_in(&governor)
            } else {
                Pipeline::plan_in(&governor)
            };
            if available < minimum {
                assert!(plan.is_err());
            } else {
                let plan = plan.unwrap();
                let bytes = plan.remaining();
                assert!(bytes.reader >= 2 * FRAME);
                assert_eq!(bytes.reader + bytes.writer + bytes.publication, available);
                assert!(governor.reserve(Category::Sqlite, 1, None).is_none());
                drop(plan);
            }
            assert_eq!(
                governor.usage().categories[Category::Sqlite as usize],
                before
            );
            drop(pressure);
            assert_eq!(governor.usage().categories[Category::Sqlite as usize], 0);
        }
    }
}
#[test]
fn repository_future_state_and_unknown_artifacts_are_preserved() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repo");
    let c = context_id(1);
    let repo = Repository::open(&path, c.inventory, c.source).unwrap();
    drop(repo);
    let state = path.join("STATE.json");
    let bytes = b"{\"version\":999,\"next_generation\":99}";
    std::fs::write(&state, bytes).unwrap();
    assert!(Repository::open_existing(&path).is_err());
    assert_eq!(std::fs::read(&state).unwrap(), bytes);
    assert!(Repository::open_for_recovery(&path, c.inventory, c.source).is_err());
    assert_eq!(std::fs::read(&state).unwrap(), bytes);
}

#[test]
fn extraction_refuses_unknown_check_and_trigger_without_executing_source_ddl() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for statement in [
        "PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql,'CHECK(length(generation)=8 AND length(id)=8)','CHECK(1)') WHERE name='entries'; PRAGMA writable_schema=OFF;",
        "CREATE TRIGGER foreign_application_logic BEFORE INSERT ON entries BEGIN SELECT RAISE(ABORT,'unknown source trigger must never execute'); END;",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("offline");
        let c = context_id(1);
        let mut source = Store::create(
            &tmp.path().join("source"),
            c.inventory,
            c.source,
            Durability::Retained,
        )
        .unwrap();
        add(&mut source, 1, "native.txt", 5, &root);
        source.db.execute_batch(statement).unwrap();
        let before = std::fs::read(source.directory.join("inventory.db")).unwrap();
        let before_wal = std::fs::read(source.directory.join("inventory.db-wal")).unwrap();
        assert!(
            publish(
                &source,
                &tmp.path().join("published"),
                GenerationId(1),
                &root,
                64 * FRAME,
                &Default::default()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(source.directory.join("inventory.db")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(source.directory.join("inventory.db-wal")).unwrap(),
            before_wal
        );
        assert!(!tmp.path().join("published").exists());
    }
}

#[test]
fn shared_readers_exclude_purge_and_reverse_adoption_does_not_regress_latest() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("offline");
    let c = context_id(1);
    let mut source = Store::create(
        &tmp.path().join("legacy"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    add(&mut source, 1, "older.txt", 1, &root);
    add(&mut source, 2, "newer.txt", 2, &root);
    let path = tmp.path().join("repo");
    let mut repo = Repository::open(&path, c.inventory, c.source).unwrap();
    assert!(
        repo.adopt_legacy(
            &source,
            GenerationId(2),
            &root,
            64 * FRAME,
            &Default::default()
        )
        .unwrap()
        .committed()
    );
    assert!(
        repo.adopt_legacy(
            &source,
            GenerationId(1),
            &root,
            64 * FRAME,
            &Default::default()
        )
        .unwrap()
        .committed()
    );
    assert_eq!(
        repo.latest_context().unwrap().unwrap().generation,
        GenerationId(2)
    );
    let reader = Repository::open_readonly(&path).unwrap();
    // The test covers shared leases, with explicit bounded reader slots even
    // when native Auto leaves only ~15 MiB for SQLite and source remains live.
    let mut first_plan = Pipeline::for_publication().unwrap();
    let first = reader
        .open_generation_with_plan(GenerationId(1), &mut first_plan, &Default::default())
        .unwrap();
    drop(first_plan);
    let mut second_plan = Pipeline::for_publication().unwrap();
    let second = reader
        .open_generation_with_plan(GenerationId(1), &mut second_plan, &Default::default())
        .unwrap();
    drop(second_plan);
    assert!(repo.purge(GenerationId(1), &Default::default()).is_err());
    assert!(first.directory().join("inventory.db").exists());
    drop(first);
    drop(second);
    repo.purge(GenerationId(1), &Default::default()).unwrap();
    assert_eq!(
        repo.next_context_after(c, GenerationId(10))
            .unwrap()
            .generation,
        GenerationId(11)
    );
    assert!(
        repo.descriptors()
            .unwrap()
            .iter()
            .all(|d| d.generation == GenerationId(2))
    );
    assert_eq!(source.entry_count(GenerationId(1)).unwrap(), 2);
}

#[test]
fn canonical_store_real_wal_quota_rolls_back_and_can_move_between_workers() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("canonical");
    let c = context_id(1);
    let memory = Governor::global()
        .reserve(Category::Sqlite, FRAME, None)
        .unwrap();
    let store = Store::create_canonical_admitted(
        &directory,
        c.inventory,
        c.source,
        Durability::Retained,
        memory,
        512 * 1024,
        None,
    )
    .unwrap();
    let source = std::thread::spawn(move || {
        store
            .db
            .execute_batch("CREATE TABLE quota_probe(id INTEGER PRIMARY KEY,payload BLOB NOT NULL)")
            .unwrap();
        let mut committed = 0;
        loop {
            let result = store.db.execute(
                "INSERT INTO quota_probe VALUES(?1,zeroblob(4000))",
                [committed],
            );
            match result {
                Ok(_) => committed += 1,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DiskFull =>
                {
                    break;
                }
                Err(error) => panic!("unexpected canonical quota result: {error}"),
            }
            assert!(committed < 1000, "canonical VFS quota never enforced");
        }
        assert!(committed > 0);
        assert_eq!(
            store
                .db
                .query_row::<i64, _, _>("SELECT count(*) FROM quota_probe", [], |r| r.get(0))
                .unwrap(),
            committed
        );
        check_full(&store.db).unwrap();
        let usage = store.canonical_usage().unwrap();
        assert!(usage.peak <= usage.limit);
        store
    })
    .join()
    .unwrap();
    assert!(source.canonical_usage().is_some());
    drop(source);
    let memory = Governor::global()
        .reserve(Category::Sqlite, FRAME, None)
        .unwrap();
    let reopened = Store::reopen_canonical_cancellable_admitted(
        &directory,
        Durability::Retained,
        memory,
        2 * FRAME,
        &Default::default(),
    )
    .unwrap();
    assert!(reopened.canonical_usage().is_some());
    check_full(&reopened.db).unwrap();
}

#[test]
fn canonical_twenty_thousand_observations_allow_statement_journal_spill() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = tempfile::tempdir().unwrap();
    let location = temporary.path().join("repo");
    let root = temporary.path().join("offline");
    let c = context_id(1);
    let mut repository = Repository::open(&location, c.inventory, c.source).unwrap();
    let c = repository.next_context(c).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    let mut source = repository.create_candidate(c, &mut plan).unwrap();
    source.record_acquisition(c, &root, 100).unwrap();
    let attempt = attempt_id(c.generation.0, 0, 1);
    source.begin_attempt(attempt).unwrap();
    let mut sequence = 0;
    for first in (0..20000u64).step_by(128) {
        let observations = (first..(first + 128).min(20000))
            .map(|id| Observation {
                name: queries::native_path(Path::new(&format!("g1-file-{id:08}.TXT"))),
                kind: Kind::File,
                logical: Some(id),
                allocated: None,
                modified: Some((1, 0)),
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        batch(&mut source, attempt, sequence, None, &observations).unwrap();
        sequence += 1;
    }
    batch(
        &mut source,
        attempt,
        sequence,
        Some(ListingEnd::Exhausted),
        &[],
    )
    .unwrap();
    reconcile(&mut source, attempt);
    fold(&mut source, c.generation.0);
    source.seal(c).unwrap();
    assert_eq!(source.entry_count(c.generation).unwrap(), 20001);
    check_full(&source.db).unwrap();
}

#[test]
fn legacy_candidate_adoption_is_mono_generation_and_preserves_original() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(2);
    let legacy_path = temp.path().join("legacy");
    let mut legacy =
        Store::create(&legacy_path, c.inventory, c.source, Durability::Retained).unwrap();
    add(&mut legacy, 1, "old.txt", 1, temp.path());
    legacy.create_generation(c).unwrap();
    let attempt = attempt_id(2, 0, 1);
    legacy.begin_attempt(attempt).unwrap();
    batch(
        &mut legacy,
        attempt,
        0,
        None,
        &[Observation {
            name: queries::native_path(Path::new("unfinished.txt")),
            kind: Kind::File,
            logical: Some(42),
            allocated: Some(42),
            modified: None,
            metadata_known: true,
        }],
    )
    .unwrap();
    drop(legacy);
    let before = std::fs::read(legacy_path.join("inventory.db")).unwrap();
    let legacy = Store::certify_readonly(&legacy_path, &Default::default()).unwrap();
    let mut repo = Repository::open(&temp.path().join("repo"), c.inventory, c.source).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    let candidate = repo
        .adopt_candidate(&legacy, c.generation, &mut plan)
        .unwrap();
    assert_eq!(candidate.acquisition_context(c.generation).unwrap(), c);
    assert_eq!(candidate.latest_sealed().unwrap(), None);
    assert_eq!(
        candidate
            .db
            .query_row::<i64, _, _>("SELECT count(*) FROM generations", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        candidate
            .db
            .query_row::<i64, _, _>("SELECT sealed FROM generations", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        candidate
            .db
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM entries WHERE parent IS NOT NULL",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
    drop(candidate);
    drop(plan);
    let mut plan = Pipeline::plan().unwrap();
    let reopened = repo
        .open_candidate_cancellable(c.generation, &mut plan, &Default::default())
        .unwrap();
    assert_eq!(reopened.acquisition_context(c.generation).unwrap(), c);
    assert_eq!(
        before,
        std::fs::read(legacy_path.join("inventory.db")).unwrap()
    );
}
#[test]
fn cancelled_reopen_and_unfinished_adoption_do_not_admit_writes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let path = temp.path().join("legacy");
    drop(Store::create(&path, c.inventory, c.source, Durability::Retained).unwrap());
    let before = std::fs::read(path.join("inventory.db")).unwrap();
    let cancel = CancellationToken::default();
    cancel.cancel();
    assert!(Store::reopen_cancellable(&path, Durability::Retained, &cancel).is_err());
    assert_eq!(before, std::fs::read(path.join("inventory.db")).unwrap());
    let mut repo = Repository::open(&temp.path().join("repo"), c.inventory, c.source).unwrap();
    let context = repo.next_context(c).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    drop(repo.create_candidate(context, &mut plan).unwrap());
    drop(plan);
    std::fs::write(
        repo.directory()
            .join(format!(".pending-adopt-{:016x}", context.generation.0)),
        b"interrupted",
    )
    .unwrap();
    let mut plan = Pipeline::plan().unwrap();
    assert!(repo.open_candidate(context.generation, &mut plan).is_err());
    assert!(
        repo.candidate_path(context.generation)
            .join("inventory.db")
            .exists()
    );
}

#[test]
fn readonly_certification_preserves_missing_owner_metadata_without_repair() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let path = temp.path().join("legacy");
    let store = Store::create(&path, c.inventory, c.source, Durability::Retained).unwrap();
    drop(store);
    std::fs::remove_file(path.join("owner.lock")).unwrap();
    let before = std::fs::read(path.join("inventory.db")).unwrap();
    assert!(Store::certify_readonly(&path, &Default::default()).is_err());
    assert!(!path.join("owner.lock").exists());
    assert_eq!(before, std::fs::read(path.join("inventory.db")).unwrap());
}

#[test]
fn default_ro_reader_class_leaves_two_readers_alongside_a_live_writer() {
    let governor = Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 8 * 1024 * FRAME,
            available: 8 * 1024 * FRAME,
        }),
        1024 * FRAME,
    );
    let limit = governor.usage().limits.categories[Category::Sqlite as usize];
    let pressure = governor
        .reserve(Category::Sqlite, limit - 15 * FRAME, None)
        .unwrap();
    let writer = governor.reserve(Category::Sqlite, 8 * FRAME, None).unwrap();
    let first = CertifiedGeneration::reserve_reader_memory_in(&governor).unwrap();
    let second = CertifiedGeneration::reserve_reader_memory_in(&governor).unwrap();
    assert_eq!(first.bytes(), FRAME + FRAME / 2);
    assert_eq!(second.bytes(), FRAME + FRAME / 2);
    assert_eq!(
        governor.usage().categories[Category::Sqlite as usize],
        limit - 4 * FRAME
    );
    drop((first, second, writer, pressure));
    assert_eq!(governor.usage().categories[Category::Sqlite as usize], 0);
}

#[test]
fn existing_future_repository_without_owner_is_preserved_by_all_writer_opens() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    for document in ["IDENTITY.json", "STATE.json", "POLICY.json"] {
        let path = temp.path().join(document);
        drop(Repository::open(&path, c.inventory, c.source).unwrap());
        let mut value = if path.join(document).exists() {
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(path.join(document)).unwrap(),
            )
            .unwrap()
        } else {
            serde_json::json!({"enabled":false,"days":30,"pending_purge":false})
        };
        value["version"] = serde_json::json!(499);
        std::fs::write(path.join(document), serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::remove_file(path.join("owner.lock")).unwrap();
        let snapshot = || {
            let mut files = std::fs::read_dir(&path)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    (e.file_name(), std::fs::read(e.path()).unwrap())
                })
                .collect::<Vec<_>>();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        };
        let before = snapshot();
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&path).unwrap().permissions().mode()
        };
        assert!(Repository::open(&path, c.inventory, c.source).is_err());
        assert_eq!(before, snapshot());
        assert!(Repository::open_for_recovery(&path, c.inventory, c.source).is_err());
        assert_eq!(before, snapshot());
        assert!(Repository::open_existing(&path).is_err());
        assert_eq!(before, snapshot());
        assert!(!path.join("owner.lock").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                permissions,
                std::fs::metadata(&path).unwrap().permissions().mode()
            );
        }
    }
}
#[test]
fn owner_only_interrupted_repository_is_not_implicitly_initialized() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let path = temp.path().join("repo");
    lifecycle::ensure_root(&path).unwrap();
    std::fs::write(path.join("owner.lock"), []).unwrap();
    assert!(Repository::open(&path, c.inventory, c.source).is_err());
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
    assert!(!path.join("IDENTITY.json").exists());
}
