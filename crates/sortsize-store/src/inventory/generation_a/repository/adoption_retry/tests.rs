use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id};
use std::sync::Mutex;
static SERIAL: Mutex<()> = Mutex::new(());

fn snapshot(path: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let name = e.file_name();
            let bytes = if name == "owner.lock" {
                // Windows mandatory byte locks also reject ReadFile against
                // this empty lease file. Its exact content follows from a
                // verified regular, non-reparse file with length zero.
                let metadata = std::fs::symlink_metadata(e.path()).unwrap();
                assert!(metadata.is_file() && !metadata.is_symlink());
                assert_eq!(metadata.len(), 0);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    assert_eq!(
                        metadata.file_attributes()
                            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
                        0
                    );
                }
                Vec::new()
            } else {
                std::fs::read(e.path()).unwrap()
            };
            (name, bytes)
        })
        .collect()
}
fn setup(temp: &Path) -> (Store, Repository, AcquisitionContext) {
    let c = context_id(7);
    let path = temp.join("legacy");
    let mut legacy = Store::create(&path, c.inventory, c.source, Durability::Retained).unwrap();
    legacy.enable_properties().unwrap();
    legacy.create_generation(c).unwrap();
    legacy
        .record_acquisition(c, &temp.join("offline"), 10)
        .unwrap();
    let attempt = attempt_id(7, 0, 1);
    legacy.begin_attempt(attempt).unwrap();
    batch(
        &mut legacy,
        attempt,
        0,
        None,
        &[Observation {
            name: queries::native_path(Path::new("original-exact.txt")),
            kind: Kind::File,
            logical: Some(42),
            allocated: Some(42),
            modified: None,
            metadata_known: true,
        }],
    )
    .unwrap();
    drop(legacy);
    let legacy = Store::certify_readonly(&path, &Default::default()).unwrap();
    let mut repo = Repository::open(
        &Repository::companion_path(&path).unwrap(),
        c.inventory,
        c.source,
    )
    .unwrap();
    repo.bind_legacy_origin(&legacy).unwrap();
    write_json(
        repo.directory(),
        &format!(".pending-adopt-{:016x}", c.generation.0),
        &Allocation {
            version: 1,
            generation: c.generation.0,
        },
        false,
    )
    .unwrap();
    (legacy, repo, c)
}
fn interrupted(repo: &Repository, c: AcquisitionContext, with_generation: bool) {
    let mut partial = Store::create(
        &repo.candidate_path(c.generation),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    if with_generation {
        partial.create_generation(c).unwrap();
    }
    // Stable standalone fixture keeps rejection snapshots strict on all files.
    partial
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
}
#[test]
fn static_known_migrations_match_every_real_store_version() {
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let mut store = Store::create(
        &temp.path().join("store"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    known_schema(&store.db, 2).unwrap();
    store.enable_lifecycle().unwrap();
    known_schema(&store.db, 3).unwrap();
    store.enable_evidence().unwrap();
    known_schema(&store.db, 4).unwrap();
    store.enable_catalogue().unwrap();
    known_schema(&store.db, 5).unwrap();
    store.enable_properties().unwrap();
    known_schema(&store.db, 6).unwrap();
}
#[test]
fn explicit_retry_preserves_interrupted_copy_and_extracts_exact_original_same_id() {
    let _serial = SERIAL.lock().unwrap();
    for with_generation in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, mut repo, c) = setup(temp.path());
        interrupted(&repo, c, with_generation);
        let old = snapshot(&repo.candidate_path(c.generation));
        let source = snapshot(legacy.directory());
        let before = repo.account().unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let candidate = repo
            .adopt_candidate_cancellable(&legacy, c.generation, &mut plan, &Default::default())
            .unwrap();
        assert_eq!(candidate.acquisition_context(c.generation).unwrap(), c);
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
        assert_eq!(
            candidate
                .db
                .query_row::<Vec<u8>, _, _>(
                    "SELECT name FROM entries WHERE parent IS NOT NULL",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            queries::native_path(Path::new("original-exact.txt")).encode()
        );
        assert_eq!(candidate.latest_sealed().unwrap(), None);
        assert!(
            !repo
                .directory()
                .join(format!(".pending-adopt-{:016x}", c.generation.0))
                .exists()
        );
        assert_eq!(source, snapshot(legacy.directory()));
        let quarantines = std::fs::read_dir(repo.directory())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarantine-a-")
            })
            .collect::<Vec<_>>();
        assert_eq!(quarantines.len(), 1);
        assert_eq!(old, snapshot(&quarantines[0]));
        assert!(repo.account().unwrap() > before);
        drop(candidate);
        drop(plan);
        let mut plan = Pipeline::plan().unwrap();
        assert!(repo.open_candidate(c.generation, &mut plan).is_ok());
    }
}
#[test]
fn marker_only_retry_after_quarantine_recreates_candidate() {
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (legacy, mut repo, c) = setup(temp.path());
    let mut plan = Pipeline::plan().unwrap();
    assert!(
        repo.adopt_candidate(&legacy, c.generation, &mut plan)
            .is_ok()
    );
}
#[test]
fn future_marker_unknown_ddl_foreign_and_live_copies_are_preserved() {
    let _serial = SERIAL.lock().unwrap();
    for case in [
        "future_marker",
        "future_format",
        "corrupt",
        "unknown_ddl",
        "unknown_migration_ddl",
        "model",
        "foreign_context",
        "live",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, mut repo, c) = setup(temp.path());
        interrupted(&repo, c, true);
        let mut live = None;
        match case {
            "future_marker" => write_json(
                repo.directory(),
                &format!(".pending-adopt-{:016x}", c.generation.0),
                &Allocation {
                    version: 499,
                    generation: c.generation.0,
                },
                true,
            )
            .unwrap(),
            "future_format" => {
                let db = Connection::open(repo.candidate_path(c.generation).join("inventory.db"))
                    .unwrap();
                db.pragma_update(None, "user_version", 499).unwrap();
            }
            "corrupt" => {
                use std::io::{Seek, SeekFrom};
                let mut file = OpenOptions::new()
                    .write(true)
                    .open(repo.candidate_path(c.generation).join("inventory.db"))
                    .unwrap();
                file.seek(SeekFrom::Start(100)).unwrap();
                file.write_all(&[0xff]).unwrap();
                file.sync_all().unwrap();
            }
            "unknown_migration_ddl" => {
                let db = Connection::open(repo.candidate_path(c.generation).join("inventory.db"))
                    .unwrap();
                db.execute_batch("CREATE TABLE entry_properties(generation BLOB,id BLOB) STRICT;")
                    .unwrap();
            }
            "model" => {
                let db = Connection::open(repo.candidate_path(c.generation).join("inventory.db"))
                    .unwrap();
                db.execute_batch("CREATE TABLE generation_a_meta(singleton INTEGER) STRICT;")
                    .unwrap();
            }
            "unknown_ddl" => {
                let db = Connection::open(repo.candidate_path(c.generation).join("inventory.db"))
                    .unwrap();
                db.execute_batch("CREATE TABLE foreign_payload(value TEXT);")
                    .unwrap();
            }
            "foreign_context" => {
                let db = Connection::open(repo.candidate_path(c.generation).join("inventory.db"))
                    .unwrap();
                db.execute(
                    "UPDATE generations SET context=?1",
                    [context(context_id(8))],
                )
                .unwrap();
            }
            "live" => {
                let memory = Governor::global()
                    .reserve(Category::Sqlite, FRAME, None)
                    .unwrap();
                live = Some(
                    Store::inspect_admitted(
                        &repo.candidate_path(c.generation),
                        &Default::default(),
                        memory,
                    )
                    .unwrap(),
                );
            }
            _ => unreachable!(),
        }
        let before = snapshot(&repo.candidate_path(c.generation));
        let marker = std::fs::read(
            repo.directory()
                .join(format!(".pending-adopt-{:016x}", c.generation.0)),
        )
        .unwrap();
        let source = snapshot(legacy.directory());
        let mut plan = Pipeline::plan().unwrap();
        assert!(
            repo.adopt_candidate(&legacy, c.generation, &mut plan)
                .is_err(),
            "{case}"
        );
        assert_eq!(
            before,
            snapshot(&repo.candidate_path(c.generation)),
            "{case}"
        );
        assert_eq!(
            marker,
            std::fs::read(
                repo.directory()
                    .join(format!(".pending-adopt-{:016x}", c.generation.0))
            )
            .unwrap()
        );
        assert_eq!(source, snapshot(legacy.directory()));
        assert_eq!(
            std::fs::read_dir(repo.directory())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".quarantine-a-"))
                .count(),
            0
        );
        drop(live);
    }
}

static TAMPER_AFTER_HANDOFF: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) fn after_anchor_handoff(path: &Path) {
    if TAMPER_AFTER_HANDOFF.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let db = Connection::open(path.join("inventory.db")).unwrap();
        db.pragma_update(None, "user_version", 499).unwrap();
    }
}
#[test]
fn anchor_handoff_revalidates_future_input_and_restart_preserves_the_old_copy() {
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (legacy, mut repo, c) = setup(temp.path());
    interrupted(&repo, c, true);
    let source = snapshot(legacy.directory());
    let path = repo.candidate_path(c.generation);
    TAMPER_AFTER_HANDOFF.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut plan = Pipeline::plan().unwrap();
    // The first snapshot was valid; after its lease is released the test
    // mutates the format. Reusing the first certificate would move this group.
    assert!(
        repo.adopt_candidate_cancellable(&legacy, c.generation, &mut plan, &Default::default())
            .is_err()
    );
    drop(plan);
    assert!(path.exists());
    assert!(
        repo.directory()
            .join(format!(".pending-adopt-{:016x}", c.generation.0))
            .exists()
    );
    assert_eq!(source, snapshot(legacy.directory()));
    assert!(!std::fs::read_dir(repo.directory()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".quarantine-a-")
    }));
    let db = Connection::open(path.join("inventory.db")).unwrap();
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        499
    );
    db.pragma_update(None, "user_version", 2).unwrap();
    drop(db);
    // A crash after installing the hard link requires no repair/GC. Reopen
    // validates that existing anchor, then retries from the unchanged origin.
    let repository_path = repo.directory().to_owned();
    drop(repo);
    let mut repo = Repository::open_existing(&repository_path).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    let candidate = repo
        .adopt_candidate_cancellable(&legacy, c.generation, &mut plan, &Default::default())
        .unwrap();
    assert_eq!(candidate.acquisition_context(c.generation).unwrap(), c);
    assert_eq!(source, snapshot(legacy.directory()));
}
