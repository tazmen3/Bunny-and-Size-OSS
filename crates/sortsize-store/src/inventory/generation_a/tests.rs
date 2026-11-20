use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
use std::{
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn query_scratch(path: PathBuf) -> PathBuf {
    lifecycle::ensure_root(&path).unwrap();
    path
}

fn candidate(path: &Path, n: u64) -> Store {
    let c = context_id(1);
    let mut store =
        Store::create(path, c.inventory, c.source, crate::Durability::Retained).unwrap();
    store.create_generation(c).unwrap();
    let attempt = attempt_id(1, 0, 1);
    store.begin_attempt(attempt).unwrap();
    let mut seq = 0;
    for start in (0..n).step_by(128) {
        let rows = (start..(start + 128).min(n))
            .map(|i| {
                let text = format!("native-{i:08}.TXT");
                #[cfg(windows)]
                let name = crate::codec::NativeName::Windows(text.encode_utf16().collect());
                #[cfg(not(windows))]
                let name = crate::codec::NativeName::Posix(text.into_bytes());
                Observation {
                    name,
                    kind: Kind::File,
                    logical: Some(i),
                    allocated: None,
                    modified: Some((-1, 999_999_999)),
                    metadata_known: true,
                }
            })
            .collect::<Vec<_>>();
        batch(&mut store, attempt, seq, None, &rows).unwrap();
        seq += 1;
    }
    batch(&mut store, attempt, seq, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut store, attempt);
    fold(&mut store, 1);
    store.seal(c).unwrap();
    store
}

fn fixture(n: u64) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let source = candidate(&tmp.path().join("candidate"), n);
    let published = tmp.path().join("published");
    let root = tmp.path().join("offline-never-created");
    publish(
        &source,
        &published,
        GenerationId(1),
        &root,
        64 * 1024 * 1024,
        &Default::default(),
    )
    .unwrap();
    (tmp, published, root)
}

fn bytes(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut values = fs::read_dir(path)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            (entry.file_type().unwrap().is_file() && name != "owner.lock")
                .then(|| (name, fs::read(entry.path()).unwrap()))
        })
        .collect::<Vec<_>>();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

fn edit(path: &Path, sql: &str) {
    let db = Connection::open(path.join("inventory.db")).unwrap();
    db.execute_batch(sql).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
}

fn assert_refused_unchanged(path: &Path, root: &Path) {
    let before = bytes(path);
    assert!(CertifiedGeneration::open(path, GenerationId(1), root, &Default::default()).is_err());
    let after = bytes(path);
    let summary = |items: &Vec<(String, Vec<u8>)>| {
        items
            .iter()
            .map(|(name, data)| (name.clone(), data.len()))
            .collect::<Vec<_>>()
    };
    let differences = before
        .iter()
        .filter_map(|(name, data)| {
            after
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, other)| {
                    data.iter()
                        .zip(other)
                        .position(|(a, b)| a != b)
                        .map(|at| (name.clone(), at))
                })
        })
        .collect::<Vec<_>>();
    assert!(
        after == before,
        "refused RO open group changed: before={:?} after={:?} byte_differences={differences:?}",
        summary(&before),
        summary(&after)
    );
}

#[test]
fn future_format_refuses_without_database_wal_or_shm_writes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(17);
    edit(&path, "PRAGMA user_version=999;");
    assert_refused_unchanged(&path, &root);
}

#[test]
fn intact_btree_missing_read_model_members_is_not_a_certificate() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(23);
    edit(&path, "DELETE FROM generation_a_rows;");
    let db = Connection::open_with_flags(
        path.join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row::<String, _, _>("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    assert_refused_unchanged(&path, &root);
}

#[test]
fn intact_btree_false_logical_mapping_is_refused_before_ready() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(23);
    edit(
        &path,
        "UPDATE generation_a_rows SET logical=x'FFFFFFFFFFFFFFFF' WHERE logical IS NOT NULL;",
    );
    let db = Connection::open_with_flags(
        path.join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row::<String, _, _>("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    assert_refused_unchanged(&path, &root);
}

#[test]
fn structurally_readable_index_with_wrong_secondary_mapping_is_refused() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(23);
    edit(
        &path,
        "PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql,'generation_a_rows(kind,rank)','generation_a_rows(logical,rank)') WHERE name='generation_a_kind'; PRAGMA writable_schema=OFF;",
    );
    let changed = Connection::open_with_flags(
        path.join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let definition: String = changed
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='generation_a_kind'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        definition.contains("generation_a_rows(logical,rank)"),
        "corruption fixture must actually modify the index mapping"
    );
    drop(changed);
    assert_refused_unchanged(&path, &root);
}

#[test]
fn noncanonical_model_views_are_refused_before_expression_evaluation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for name in [
        "generation_a_meta",
        "generation_a_rows",
        "generation_a_counts",
    ] {
        let (_tmp, path, root) = fixture(17);
        edit(
            &path,
            &format!(
                "ALTER TABLE {name} RENAME TO preserved_{name}; CREATE VIEW {name} AS SELECT abs(-9223372036854775808) AS corrupt_expression;"
            ),
        );
        let before = bytes(&path);
        let result = CertifiedGeneration::open(&path, GenerationId(1), &root, &Default::default());
        assert!(
            matches!(
                result,
                Err(Error::Contract(
                    "generation A requires canonical model tables"
                ))
            ),
            "view must be rejected by type guard before evaluating its expression"
        );
        assert!(bytes(&path) == before);
    }
}

#[test]
fn future_model_version_and_false_root_are_refused_without_writes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for sql in [
        "UPDATE generation_a_meta SET version=999;",
        "UPDATE generation_a_meta SET root=x'010203';",
    ] {
        let (_tmp, path, root) = fixture(17);
        edit(&path, sql);
        assert_refused_unchanged(&path, &root);
    }
}

#[test]
fn cursor_from_another_query_is_rejected_even_for_same_generation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tmp, path, root) = fixture(257);
    let ready =
        CertifiedGeneration::open(&path, GenerationId(1), &root, &Default::default()).unwrap();
    let first = ready
        .query(
            &Default::default(),
            &query_scratch(tmp.path().join("q1")),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let page = first.page(None).unwrap();
    assert!(page.more && page.next.is_some());
    let second = ready
        .query(
            &Default::default(),
            &query_scratch(tmp.path().join("q2")),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    assert!(second.page(page.next.as_ref()).is_err());
    assert!(first.page(page.next.as_ref()).is_ok());
}

#[test]
fn complete_integrity_check_covers_an_unused_table_page() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(23);
    edit(
        &path,
        "CREATE TABLE unused_payload(id INTEGER PRIMARY KEY, payload BLOB); INSERT INTO unused_payload VALUES(1,zeroblob(16000));",
    );
    // A legitimate additional table does not change the read model. The next
    // corruption is deliberately outside every requested result and index.
    let ready =
        CertifiedGeneration::open(&path, GenerationId(1), &root, &Default::default()).unwrap();
    drop(ready);
    let db = Connection::open(path.join("inventory.db")).unwrap();
    let page: u64 = db
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='unused_payload'",
            [],
            |row| count(row, 0),
        )
        .unwrap();
    let size: u64 = db
        .pragma_query_value(None, "page_size", |row| count(row, 0))
        .unwrap();
    drop(db);
    let mut file = OpenOptions::new()
        .write(true)
        .open(path.join("inventory.db"))
        .unwrap();
    file.seek(SeekFrom::Start((page - 1) * size)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_refused_unchanged(&path, &root);
}

#[test]
fn reopen_recertifies_even_after_a_previous_successful_ready_session() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(23);
    let ready =
        CertifiedGeneration::open(&path, GenerationId(1), &root, &Default::default()).unwrap();
    let query = ready
        .query(
            &Default::default(),
            &query_scratch(path.parent().unwrap().join("query")),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    assert!(query.complete());
    drop(query);
    drop(ready);
    edit(&path, "DELETE FROM generation_a_rows;");
    assert_refused_unchanged(&path, &root);
}

#[test]
fn quota_or_cancelled_publication_cannot_replace_a_published_generation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tmp, g0, root) = fixture(17);
    let before = bytes(&g0);
    let next = candidate(&tmp.path().join("candidate-next"), 33);
    let g1 = tmp.path().join("g1");
    assert!(publish(&next, &g1, GenerationId(1), &root, 1, &Default::default()).is_err());
    assert!(!g1.exists());
    assert_eq!(bytes(&g0), before);
    let stop = CancellationToken::default();
    stop.cancel();
    assert!(publish(&next, &g1, GenerationId(1), &root, 64 * 1024 * 1024, &stop).is_err());
    assert!(!g1.exists());
    assert_eq!(bytes(&g0), before);
}

#[test]
fn admitted_but_insufficient_growth_preserves_stage_and_g0() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tmp, g0, root) = fixture(17);
    let before = bytes(&g0);
    let next = candidate(&tmp.path().join("large-candidate"), 3000);
    let g1 = tmp.path().join("g1");
    assert!(
        publish(
            &next,
            &g1,
            GenerationId(1),
            &root,
            1024 * 1024,
            &Default::default()
        )
        .is_err()
    );
    assert!(!g1.exists());
    assert_eq!(bytes(&g0), before);
    assert!(
        fs::read_dir(tmp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".stage-a-")
        }),
        "failed admitted build must preserve its owned stage"
    );
}

#[test]
fn foreign_generation_or_native_root_is_not_promoted() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, path, root) = fixture(17);
    let before = bytes(&path);
    assert!(CertifiedGeneration::open(&path, GenerationId(2), &root, &Default::default()).is_err());
    assert!(
        CertifiedGeneration::open(
            &path,
            GenerationId(1),
            &root.join("foreign"),
            &Default::default()
        )
        .is_err()
    );
    assert_eq!(bytes(&path), before);
}

#[test]
fn g0_reader_runs_while_g1_is_really_constructed_and_published() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tmp, g0, root) = fixture(257);
    let before = bytes(&g0);
    let start = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let reader_start = start.clone();
    let reader_stop = stop.clone();
    let reader_path = g0.clone();
    let reader_root = root.clone();
    let scratch = query_scratch(tmp.path().join("reader-scratch"));
    let reader = std::thread::spawn(move || {
        let ready = CertifiedGeneration::open(
            &reader_path,
            GenerationId(1),
            &reader_root,
            &Default::default(),
        )
        .unwrap();
        reader_start.wait();
        let mut passes = 0;
        loop {
            let query = ready
                .query(
                    &Default::default(),
                    &scratch,
                    8 * 1024 * 1024,
                    &Default::default(),
                )
                .unwrap();
            assert!(query.complete());
            assert_eq!(query.counts.matches, 258);
            let mut cursor = None;
            let mut count = 0;
            loop {
                let page = query.page(cursor.as_ref()).unwrap();
                count += page.rows.len();
                if !page.more {
                    break;
                }
                cursor = page.next;
            }
            assert_eq!(count, 258);
            passes += 1;
            if reader_stop.load(Ordering::Acquire) {
                break;
            }
        }
        passes
    });
    start.wait();
    let next = candidate(&tmp.path().join("g1-candidate"), 1025);
    let g1 = tmp.path().join("g1");
    publish(
        &next,
        &g1,
        GenerationId(1),
        &root,
        64 * 1024 * 1024,
        &Default::default(),
    )
    .unwrap();
    stop.store(true, Ordering::Release);
    assert!(reader.join().unwrap() >= 1);
    assert_eq!(bytes(&g0), before);
    let ready =
        CertifiedGeneration::open(&g1, GenerationId(1), &root, &Default::default()).unwrap();
    let query = ready
        .query(
            &Default::default(),
            &query_scratch(tmp.path().join("g1-query")),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    assert!(query.complete());
    assert_eq!(query.counts.matches, 1026);
    assert!(!root.exists());
}

// Called only by publication in test builds. The child process blocks at the
// actual commit boundary so its parent can kill it, not simulate a returned I/O error.
pub(super) fn publication_hook(stage: &str) {
    if std::env::var("BUNNY_GENERATION_A_STAGE").ok().as_deref() != Some(stage) {
        return;
    }
    if let Some(marker) = std::env::var_os("BUNNY_GENERATION_A_MARKER") {
        fs::write(marker, stage).unwrap();
        loop {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
#[ignore = "child process helper for real publication crash test"]
fn publication_crash_child() {
    let Some(path) = std::env::var_os("BUNNY_GENERATION_A_CRASH_ROOT") else {
        return;
    };
    let path = PathBuf::from(path);
    let source = candidate(&path.join("child-source"), 97);
    let _ = publish(
        &source,
        &path.join("g1"),
        GenerationId(1),
        &path.join("offline"),
        64 * 1024 * 1024,
        &Default::default(),
    );
}

#[test]
fn killed_publication_child_keeps_g0_and_does_not_publish_partial_g1() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tmp, g0, root) = fixture(17);
    let before = bytes(&g0);
    let marker = tmp.path().join("crash-marker");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inventory::generation_a::tests::publication_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("BUNNY_GENERATION_A_CRASH_ROOT", tmp.path())
        .env("BUNNY_GENERATION_A_STAGE", "before_publish")
        .env("BUNNY_GENERATION_A_MARKER", &marker)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        if child.try_wait().unwrap().is_some() {
            panic!("child exited before real publication boundary");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("publication hook did not fire");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!tmp.path().join("g1").exists());
    assert_eq!(bytes(&g0), before);
    let ready =
        CertifiedGeneration::open(&g0, GenerationId(1), &root, &Default::default()).unwrap();
    assert!(
        ready
            .query(
                &Default::default(),
                &query_scratch(tmp.path().join("after-crash-query")),
                8 * 1024 * 1024,
                &Default::default()
            )
            .unwrap()
            .complete()
    );
}
