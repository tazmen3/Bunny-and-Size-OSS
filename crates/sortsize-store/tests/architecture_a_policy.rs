//! Retention, independent copies, partial snapshots and qualified legacy families.
use sortsize_core::inventory_contract::*;
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{
        Kind, Observation, Store, catalogue,
        generation_a::{Pipeline, Repository},
        lifecycle,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
static SERIAL: Mutex<()> = Mutex::new(());
const INV: InventoryId = InventoryId(910);
const SOURCE: SourceId = SourceId(920);
fn context(g: u64) -> AcquisitionContext {
    AcquisitionContext {
        inventory: INV,
        source: SOURCE,
        generation: GenerationId(g),
        scan: ScanId(g),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(g),
        validation: ValidationEpoch(g),
    }
}
fn name(text: &str) -> NativeName {
    #[cfg(windows)]
    {
        NativeName::Windows(text.encode_utf16().collect())
    }
    #[cfg(not(windows))]
    {
        NativeName::Posix(text.as_bytes().to_vec())
    }
}
fn populate(s: &mut Store, c: AcquisitionContext, root: &Path, start: i64, measured: Option<i64>) {
    if s.acquisition_context(c.generation).is_err() {
        s.create_generation(c).unwrap();
    }
    s.record_acquisition(c, root, start).unwrap();
    let a = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    s.begin_attempt(a).unwrap();
    s.ingest(
        BatchHeader {
            attempt: a,
            sequence: 0,
            end: None,
        },
        &[Observation {
            name: name("known.dat"),
            kind: Kind::File,
            logical: Some(42),
            allocated: None,
            modified: None,
            metadata_known: true,
        }],
    )
    .unwrap();
    if let Some(date) = measured {
        s.ingest(
            BatchHeader {
                attempt: a,
                sequence: 1,
                end: Some(ListingEnd::Exhausted),
            },
            &[],
        )
        .unwrap();
        while !s
            .reconcile_step(a, ComparisonScope::Comparable, 128)
            .unwrap()
        {}
        while !s.consolidate_step(c, 128).unwrap() {}
        s.seal(c).unwrap();
        s.record_measurement(c.generation, root, start, date)
            .unwrap();
    }
}
fn published(path: &Path, root: &Path) -> Repository {
    let mut r = Repository::open(path, INV, SOURCE).unwrap();
    let c = r.next_context(context(1)).unwrap();
    let mut p = Pipeline::plan().unwrap();
    let mut s = r.create_candidate(c, &mut p).unwrap();
    populate(&mut s, c, root, 100, Some(200));
    assert!(
        r.publish_with_plan(
            &s,
            c.generation,
            root,
            32 * 1024 * 1024,
            &Default::default(),
            &mut p
        )
        .unwrap()
        .committed()
    );
    drop(s);
    drop(p);
    r
}
fn group(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file() && e.file_name() != "owner.lock")
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}
fn legacy_family(t: &Path) -> (Repository, PathBuf, PathBuf) {
    let legacy = t.join("inv-legacy");
    let root = t.join("offline");
    let mut s = Store::create(&legacy, INV, SOURCE, Durability::Retained).unwrap();
    populate(&mut s, context(1), &root, 100, Some(200));
    populate(&mut s, context(7), &root, 300, None);
    let destination = Repository::companion_path(&legacy).unwrap();
    let mut repo = Repository::open(&destination, INV, SOURCE).unwrap();
    repo.bind_legacy_origin(&s).unwrap();
    assert!(
        repo.adopt_legacy(
            &s,
            GenerationId(1),
            &root,
            32 * 1024 * 1024,
            &Default::default()
        )
        .unwrap()
        .committed()
    );
    drop(s);
    (repo, legacy, root)
}
#[test]
fn retention_is_versioned_and_consultation_never_refreshes_measurement() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let root = base.as_path().join("offline");
    let mut repo = published(&base.as_path().join("inv-a"), &root);
    assert!(!repo.retention().unwrap().enabled);
    repo.set_retention(true, 1).unwrap();
    assert!(!repo.expired(200 + 86400 - 1).unwrap());
    let cap = repo.open_latest(&Default::default()).unwrap().unwrap();
    assert_eq!(
        cap.as_store()
            .measurement(GenerationId(1))
            .unwrap()
            .measured,
        200
    );
    drop(cap);
    assert!(repo.expired(200 + 86400).unwrap());
    assert!(repo.set_retention(true, 0).is_err());
    repo.set_retention(false, 1).unwrap();
    assert!(repo.retention().unwrap().pending_purge);
    let directory = repo.directory().to_owned();
    let policy_path = directory.join("POLICY.json");
    let mut policy: serde_json::Value =
        serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
    policy["version"] = serde_json::json!(999);
    let future = serde_json::to_vec(&policy).unwrap();
    fs::write(&policy_path, &future).unwrap();
    assert!(repo.retention().is_err());
    assert!(repo.set_retention(true, 30).is_err());
    let destination = base.join("inv-future-copy");
    assert!(repo.copy_to(&destination, &Default::default()).is_err());
    assert!(!destination.exists());
    assert!(repo.forget_family(&Default::default()).is_err());
    assert_eq!(fs::read(policy_path).unwrap(), future);
    assert!(
        directory
            .join("generation-0000000000000001/inventory.db")
            .is_file()
    );
}
#[test]
fn copy_is_independent_durable_and_works_while_g0_has_a_shared_reader() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let root = base.as_path().join("offline");
    let mut repo = published(&base.as_path().join("inv-a"), &root);
    repo.set_retention(true, 30).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    let cap = repo
        .open_latest_with_plan(&mut plan, &Default::default())
        .unwrap()
        .unwrap();
    drop(plan);
    let original = group(cap.directory());
    let before_disk = sortsize_core::resources::Governor::global()
        .usage()
        .disk_used;
    let copy = repo
        .copy_to(&base.as_path().join("inv-copy"), &Default::default())
        .unwrap();
    let copied_bytes = Repository::account_directory(copy.directory()).unwrap();
    assert_eq!(
        sortsize_core::resources::Governor::global()
            .usage()
            .disk_used
            - before_disk,
        copied_bytes,
        "rename must rebase owned bytes without keeping a duplicate stage ledger"
    );
    assert!(
        copy.directory()
            .join("generation-0000000000000001/owner.lock")
            .is_file()
    );
    assert!(copy.legacy_origin().unwrap().is_none());
    let copied = copy.open_latest(&Default::default()).unwrap().unwrap();
    assert_eq!(
        copied.as_store().measurement(GenerationId(1)).unwrap(),
        cap.as_store().measurement(GenerationId(1)).unwrap()
    );
    assert!(group(cap.directory()) == original);
    assert!(
        repo.copy_to(&base.as_path().join("inv-copy"), &Default::default())
            .is_err()
    );
    #[cfg(unix)]
    {
        let alias = base.join("inv-dangling");
        std::os::unix::fs::symlink(base.join("missing"), &alias).unwrap();
        assert!(repo.copy_to(&alias, &Default::default()).is_err());
        assert!(fs::symlink_metadata(alias).unwrap().is_symlink());
    }
    assert_eq!(copy.retention().unwrap().days, 30);
}
#[test]
fn active_snapshot_is_catalogued_as_partial_and_preserves_highwater() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let root = base.as_path().join("offline");
    fs::create_dir(&root).unwrap();
    let mut repo = Repository::open(&base.as_path().join("inv-a"), INV, SOURCE).unwrap();
    let c = repo.next_context(context(1)).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(c, &mut plan).unwrap();
    populate(&mut source, c, &root, 100, None);
    drop(plan);
    assert!(
        repo.copy_to(&base.as_path().join("inv-empty-copy"), &Default::default())
            .is_err()
    );
    assert!(
        repo.copy_to_from_active(&root.join("inv-denied"), &source, &Default::default())
            .is_err()
    );
    assert!(!root.join("inv-denied").exists());
    let mut copy = repo
        .copy_to_from_active(
            &base.as_path().join("inv-copy"),
            &source,
            &Default::default(),
        )
        .unwrap();
    copy.set_retention(true, 30).unwrap();
    assert!(copy.open_latest(&Default::default()).unwrap().is_none());
    let records = catalogue::list(copy.directory(), None).unwrap();
    assert_eq!(records.rows.len(), 1);
    assert_eq!(records.rows[0].measured, None);
    assert_eq!(records.rows[0].directory, copy.directory());
    assert_eq!(records.rows[0].context, c);
    let next = copy.next_context(context(2)).unwrap();
    assert!(next.generation > c.generation);
    assert!(
        copy.candidate_path(c.generation)
            .join("inventory.db")
            .exists()
    );
    assert_eq!(source.entry_count(c.generation).unwrap(), 2);
    drop(source);
    let closed = repo
        .copy_to(&base.join("inv-closed-copy"), &Default::default())
        .unwrap();
    let records = catalogue::list(closed.directory(), None).unwrap();
    assert_eq!(records.rows.len(), 1);
    assert_eq!(records.rows[0].context, c);
    assert_eq!(records.rows[0].measured, None);
}
#[test]
fn qualified_origin_keeps_unadopted_records_and_does_not_reuse_candidate_ids() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let (mut repo, legacy, _) = legacy_family(base.as_path());
    let before = fs::read(legacy.join("inventory.db")).unwrap();
    assert_eq!(
        Repository::resolve_legacy(&legacy, &Default::default()).unwrap(),
        Some(repo.directory().to_owned())
    );
    let c = repo.next_context(context(2)).unwrap();
    assert!(c.generation.0 > 7);
    let records = catalogue::list(base.as_path(), None).unwrap();
    assert_eq!(records.rows.len(), 2);
    assert!(
        records
            .rows
            .iter()
            .any(|s| s.context.generation == GenerationId(7)
                && s.measured.is_none()
                && s.directory == fs::canonicalize(&legacy).unwrap())
    );
    assert_eq!(fs::read(legacy.join("inventory.db")).unwrap(), before);
}
#[test]
fn foreign_or_future_origin_never_authorizes_forgetting_either_half() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let (repo, legacy, _) = legacy_family(base.as_path());
    let source = fs::read(legacy.join("inventory.db")).unwrap();
    let directory = repo.directory().to_owned();
    let origin = directory.join("ORIGIN.json");
    let mut data: serde_json::Value = serde_json::from_slice(&fs::read(&origin).unwrap()).unwrap();
    data["version"] = serde_json::json!(999);
    fs::write(&origin, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(Repository::resolve_legacy(&legacy, &Default::default()).is_err());
    assert!(repo.forget_family(&Default::default()).is_err());
    assert_eq!(fs::read(legacy.join("inventory.db")).unwrap(), source);
    assert!(directory.join("CURRENT.json").exists());
}
#[test]
fn family_forget_preflights_all_readers_and_deletes_only_owned_inventory_data() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let (repo, legacy, root) = legacy_family(base.as_path());
    let directory = repo.directory().to_owned();
    let cap = repo.open_latest(&Default::default()).unwrap().unwrap();
    let before = fs::read(legacy.join("inventory.db")).unwrap();
    assert!(repo.forget_family(&Default::default()).is_err());
    assert_eq!(fs::read(legacy.join("inventory.db")).unwrap(), before);
    assert!(directory.join("CURRENT.json").exists());
    drop(cap);
    let repo = Repository::open_existing(&directory).unwrap();
    repo.forget_family(&Default::default()).unwrap();
    assert!(!legacy.join("inventory.db").exists());
    assert!(!directory.join("CURRENT.json").exists());
    assert!(!root.exists());
}
#[test]
fn family_prune_is_atomic_on_busy_and_recognizes_nested_repository_accounting() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let (mut repo, legacy, _) = legacy_family(base.as_path());
    repo.set_retention(true, 1).unwrap();
    let directory = repo.directory().to_owned();
    assert!(lifecycle::account_root(base.as_path()).unwrap() > 0);
    // The partial legacy candidate started later, so the whole family lives
    // until its actual observation age, never the time of catalogue reading.
    assert!(!repo.expired(300 + 86400 - 1).unwrap());
    assert!(repo.expired(300 + 86400).unwrap());
    assert_eq!(
        catalogue::prune(base.as_path(), 300 + 86400 - 1).unwrap(),
        0
    );
    assert_eq!(catalogue::prune(base.as_path(), 300 + 86400).unwrap(), 0);
    let cap = repo.open_latest(&Default::default()).unwrap().unwrap();
    drop(repo);
    assert_eq!(catalogue::prune(base.as_path(), 300 + 86400).unwrap(), 0);
    assert!(legacy.join("inventory.db").is_file());
    assert!(directory.join("CURRENT.json").is_file());
    drop(cap);
    assert_eq!(catalogue::prune(base.as_path(), 300 + 86400).unwrap(), 1);
    assert!(!legacy.join("inventory.db").exists());
    assert!(!directory.join("CURRENT.json").exists());
}

#[test]
fn repository_policy_headers_do_not_open_or_stat_the_original_cache() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let (mut repo, legacy, _) = legacy_family(&base);
    let moved = base.join("original-temporarily-unavailable");
    fs::rename(&legacy, &moved).unwrap();
    assert!(!repo.retention().unwrap().enabled);
    repo.set_retention(true, 30).unwrap();
    let cap = repo.open_latest(&Default::default()).unwrap().unwrap();
    assert_eq!(cap.generation(), GenerationId(1));
    // Family resolution/retirement still requires the actual origin, rather
    // than silently treating the lexical header as authority over a stranger.
    assert!(repo.legacy_origin().is_err());
    drop(cap);
    assert!(repo.forget_family(&Default::default()).is_err());
    assert!(moved.join("inventory.db").is_file());
}

#[test]
fn foreign_database_or_wrong_generation_in_a_known_group_never_authorizes_family_deletion() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for wrong_generation in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("private");
        lifecycle::ensure_root(&base).unwrap();
        let (mut repo, legacy, root) = legacy_family(&base);
        let allocated = repo.next_context(context(8)).unwrap();
        let candidate_path = repo.candidate_path(allocated.generation);
        let mut c = allocated;
        if wrong_generation {
            c.generation = GenerationId(c.generation.0 + 1);
        } else {
            c.inventory = InventoryId(999_123);
        }
        let mut foreign =
            Store::create(&candidate_path, c.inventory, c.source, Durability::Retained).unwrap();
        populate(&mut foreign, c, &root, 400, None);
        drop(foreign);
        let repo_path = repo.directory().to_owned();
        let published_path = repo_path.join("generation-0000000000000001/inventory.db");
        let source_before = fs::read(legacy.join("inventory.db")).unwrap();
        let published_before = fs::read(&published_path).unwrap();
        let foreign_before = fs::read(candidate_path.join("inventory.db")).unwrap();
        let current_before = fs::read(repo_path.join("CURRENT.json")).unwrap();
        assert!(repo.forget_family(&Default::default()).is_err());
        assert_eq!(
            fs::read(legacy.join("inventory.db")).unwrap(),
            source_before
        );
        assert_eq!(fs::read(published_path).unwrap(), published_before);
        assert_eq!(
            fs::read(candidate_path.join("inventory.db")).unwrap(),
            foreign_before
        );
        assert_eq!(
            fs::read(repo_path.join("CURRENT.json")).unwrap(),
            current_before
        );
    }
}

#[test]
fn canonical_temporary_names_are_accounted_preserved_on_read_and_removed_only_on_explicit_forget() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("private");
    lifecycle::ensure_root(&base).unwrap();
    let directory = base.join("inv-temporary");
    let store = Store::create(&directory, INV, SOURCE, Durability::Retained).unwrap();
    let before = lifecycle::account_root(&base).unwrap();
    let temporary = directory.join(".sqlite-temp-00000001-0000000000000001");
    fs::write(&temporary, b"retained temporary payload").unwrap();
    let after = lifecycle::account_root(&base).unwrap();
    assert_eq!(after - before, 26);
    drop(store);
    let readonly = Store::certify_readonly(&directory, &Default::default()).unwrap();
    assert!(temporary.is_file());
    drop(readonly);
    Store::reopen(&directory, Durability::Retained)
        .unwrap()
        .forget()
        .unwrap();
    assert!(!temporary.exists());
    assert!(!directory.join("inventory.db").exists());

    let directory = base.join("inv-malformed");
    let store = Store::create(&directory, INV, SOURCE, Durability::Retained).unwrap();
    drop(store);
    let store = Store::reopen(&directory, Durability::Retained).unwrap();
    let malformed = directory.join(".sqlite-temp-00000001-not-a-nonce");
    fs::write(&malformed, b"foreign").unwrap();
    let database = fs::read(directory.join("inventory.db")).unwrap();
    assert!(store.forget().is_err());
    assert_eq!(fs::read(directory.join("inventory.db")).unwrap(), database);
    assert_eq!(fs::read(malformed).unwrap(), b"foreign");
}

#[test]
fn public_purge_forget_and_discard_preserve_foreign_known_stores_before_any_retirement() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for operation in ["purge", "forget", "discard"] {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("private");
        lifecycle::ensure_root(&base).unwrap();
        let root = base.join("offline");
        let mut repo = published(&base.join("inv-a"), &root);
        let allocated = if operation == "discard" {
            context(1)
        } else {
            repo.next_context(context(2)).unwrap()
        };
        let path = repo.candidate_path(allocated.generation);
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        let foreign_context = AcquisitionContext {
            inventory: InventoryId(456789),
            ..allocated
        };
        let mut foreign = Store::create(
            &path,
            foreign_context.inventory,
            foreign_context.source,
            Durability::Retained,
        )
        .unwrap();
        populate(&mut foreign, foreign_context, &root, 700, None);
        drop(foreign);
        let directory = repo.directory().to_owned();
        let published_path = directory.join("generation-0000000000000001/inventory.db");
        let reference = fs::read(&published_path).unwrap();
        let foreign_before = fs::read(path.join("inventory.db")).unwrap();
        let current = fs::read(directory.join("CURRENT.json")).unwrap();
        let result = match operation {
            "purge" => repo.purge(allocated.generation, &Default::default()),
            "discard" => repo.discard_candidate(allocated.generation, &Default::default()),
            "forget" => repo.forget(&Default::default()),
            _ => unreachable!(),
        };
        assert!(
            result.is_err(),
            "{operation} must reject the foreign database"
        );
        assert_eq!(fs::read(path.join("inventory.db")).unwrap(), foreign_before);
        assert_eq!(fs::read(published_path).unwrap(), reference);
        assert_eq!(fs::read(directory.join("CURRENT.json")).unwrap(), current);
    }
}

#[test]
fn every_repository_open_path_refuses_future_state_and_policy_without_changing_files() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for document in ["STATE.json", "POLICY.json"] {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("private");
        lifecycle::ensure_root(&base).unwrap();
        let mut repo = published(&base.join("inv-a"), &base.join("offline"));
        repo.set_retention(true, 30).unwrap();
        let directory = repo.directory().to_owned();
        drop(repo);
        let file = directory.join(document);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        value["version"] = serde_json::json!(499);
        let future = serde_json::to_vec(&value).unwrap();
        fs::write(&file, &future).unwrap();
        let reference =
            fs::read(directory.join("generation-0000000000000001/inventory.db")).unwrap();
        assert!(Repository::open_readonly(&directory).is_err());
        assert!(Repository::open_existing(&directory).is_err());
        assert!(Repository::open(&directory, INV, SOURCE).is_err());
        assert!(Repository::open_for_recovery(&directory, INV, SOURCE).is_err());
        assert_eq!(fs::read(file).unwrap(), future);
        assert_eq!(
            fs::read(directory.join("generation-0000000000000001/inventory.db")).unwrap(),
            reference
        );
    }
}
