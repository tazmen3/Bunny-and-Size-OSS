//! Regression proofs for exact redundant-copy reclamation after publication.
use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
use std::collections::BTreeMap;
use std::sync::Mutex;
static SERIAL: Mutex<()> = Mutex::new(());

fn publish_with_closed_copy_later(
    repo: &mut Repository,
    root: &Path,
) -> (Store, AcquisitionContext) {
    let c = repo.next_context(context_id(0)).unwrap();
    let mut plan = Pipeline::plan().unwrap();
    let mut source = repo.create_candidate(c, &mut plan).unwrap();
    source.record_acquisition(c, root, 100).unwrap();
    let a = attempt_id(c.generation.0, 0, 1);
    source.begin_attempt(a).unwrap();
    batch(&mut source, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut source, a);
    fold(&mut source, c.generation.0);
    source.seal(c).unwrap();
    source
        .record_measurement(c.generation, root, 100, 101)
        .unwrap();
    assert!(
        repo.publish_with_plan(
            &source,
            c.generation,
            root,
            16 * FRAME,
            &Default::default(),
            &mut plan
        )
        .unwrap()
        .committed()
    );
    (source, c)
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
fn committed_publication_closed_copy_is_reclaimed_after_restart_before_new_allocation() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let template = context_id(0);
    let path = temp.path().join("repo");
    let mut child=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored","--exact","inventory::generation_a::repository::reclaim_tests::publication_closed_copy_crash_child"])
        .env("BUNNY_RECLAIM_CRASH_ROOT",temp.path())
        .stdout(std::process::Stdio::null())
        .spawn().unwrap();
    let marker = temp.path().join("closed-copy-ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("publication crash fixture exited before marker: {status}");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("publication crash fixture timed out before marker");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The child has closed source/repository owners after durable publication,
    // but deliberately never called discard_candidate. Kill before its next op.
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let c = context_id(1);
    let candidate = path.join("candidate-0000000000000001");
    let published = path.join("generation-0000000000000001");
    let before = snapshot(&published);
    let mut repo = Repository::open_existing(&path).unwrap();
    assert!(candidate.join("inventory.db").exists());
    let g0 = repo
        .open_generation(c.generation, &Default::default())
        .unwrap();
    let next = repo.next_context(template).unwrap();
    assert_eq!(next.generation.0, c.generation.0 + 1);
    assert!(!candidate.exists());
    assert_eq!(before, snapshot(&published));
    assert_eq!(g0.as_store().acquisition_context(c.generation).unwrap(), c);
    assert_eq!(repo.latest_context().unwrap().unwrap().context, c);
    let mut plan = Pipeline::plan().unwrap();
    let g1 = repo.create_candidate(next, &mut plan).unwrap();
    assert_eq!(g1.acquisition_context(next.generation).unwrap(), next);
    assert_eq!(before, snapshot(&published));
}
#[test]
fn divergent_non_summary_task_cell_blocks_discard_and_allocation_without_data_loss() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let template = context_id(0);
    let path = temp.path().join("repo");
    let mut repo = Repository::open(&path, template.inventory, template.source).unwrap();
    let (source, c) = publish_with_closed_copy_later(&mut repo, &temp.path().join("offline"));
    let candidate = repo.candidate_path(c.generation);
    let published = repo.generation_path(c.generation);
    let summary = |s: &Store| {
        s.db.query_row::<(Vec<u8>, Vec<u8>, i64), _, _>(
            "SELECT aggregate_sum,aggregate_unknown,coverage FROM generations WHERE id=?1",
            [unsigned(c.generation.0)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    let previous = summary(&source);
    assert_eq!(
        source
            .db
            .execute(
                "UPDATE tasks SET owner=?1 WHERE generation=?2",
                params![unsigned(999), unsigned(c.generation.0)]
            )
            .unwrap(),
        1
    );
    assert_eq!(previous, summary(&source));
    assert_eq!(source.acquisition_context(c.generation).unwrap(), c);
    // Keep rejection snapshots strict on all artifacts: a standalone fixture
    // does not require SQLite to create WAL coordination files during RO checks.
    source
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);PRAGMA journal_mode=DELETE;")
        .unwrap();
    drop(source);
    let candidate_before = snapshot(&candidate);
    let published_before = snapshot(&published);
    let current = std::fs::read(path.join("CURRENT.json")).unwrap();
    let state = std::fs::read(path.join("STATE.json")).unwrap();
    assert!(matches!(
        repo.discard_candidate(c.generation, &Default::default()),
        Err(Error::Contract(
            "discard candidate observation differs from publication"
        ))
    ));
    assert_eq!(candidate_before, snapshot(&candidate));
    assert_eq!(published_before, snapshot(&published));
    assert!(matches!(
        repo.next_context(template),
        Err(Error::Contract(
            "discard candidate observation differs from publication"
        ))
    ));
    assert_eq!(candidate_before, snapshot(&candidate));
    assert_eq!(published_before, snapshot(&published));
    assert_eq!(current, std::fs::read(path.join("CURRENT.json")).unwrap());
    assert_eq!(state, std::fs::read(path.join("STATE.json")).unwrap());
    assert_eq!(repo.latest_context().unwrap().unwrap().context, c);
}

#[test]
#[ignore = "subprocess fixture executed only by the publication crash regression"]
fn publication_closed_copy_crash_child() {
    let Some(root) = std::env::var_os("BUNNY_RECLAIM_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let template = context_id(0);
    let mut repo =
        Repository::open(&root.join("repo"), template.inventory, template.source).unwrap();
    let (source, _) = publish_with_closed_copy_later(&mut repo, &root.join("offline"));
    drop(source);
    drop(repo);
    let mut marker = File::create(root.join("closed-copy-ready")).unwrap();
    marker
        .write_all(b"publication committed; owners closed; no discard")
        .unwrap();
    marker.sync_all().unwrap();
    drop(marker);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
