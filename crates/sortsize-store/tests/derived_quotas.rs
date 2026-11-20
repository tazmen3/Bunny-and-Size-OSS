//! Public contracts for independent derived workspaces. No native source I/O,
//! large campaign, benchmark, mode override or resource-limit adjustment.
use sha2::{Digest, Sha256};
use sortsize_core::{
    CancellationToken, analysis::SearchQuery, inventory_contract::*, resources::Governor,
};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{
        Error, Kind, Observation, Store,
        differential::Diff,
        folder_totals::FolderFold,
        generation_a::{self, CertifiedGeneration},
        ordered::Sort,
        queries::TemporaryUsage,
    },
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};
const N: u64 = 4096;
const MIB: u64 = 1024 * 1024;
static SERIAL: Mutex<()> = Mutex::new(());
fn context(g: u64) -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(40401),
        source: SourceId(40402),
        generation: GenerationId(g),
        scan: ScanId(g),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(g),
        validation: ValidationEpoch(g),
    }
}
fn native(text: String) -> NativeName {
    #[cfg(windows)]
    {
        NativeName::Windows(text.encode_utf16().collect())
    }
    #[cfg(not(windows))]
    {
        NativeName::Posix(text.into_bytes())
    }
}
fn fill(store: &mut Store, g: u64, root: &Path) {
    let c = context(g);
    store.create_generation(c).unwrap();
    store.record_acquisition(c, root, 100 * g as i64).unwrap();
    let attempt = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    store.begin_attempt(attempt).unwrap();
    for (sequence, start) in (0..N).step_by(128).enumerate() {
        let end = (start + 128).min(N);
        let observations = (start..end)
            .map(|i| Observation {
                name: native(format!(
                    "generation-{g}-member-{i:05}.extension-{i:05}-{}",
                    "x".repeat(64)
                )),
                kind: Kind::File,
                logical: Some(i % 8),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        store
            .ingest(
                BatchHeader {
                    attempt,
                    sequence: sequence as u64,
                    end: (end == N).then_some(ListingEnd::Exhausted),
                },
                &observations,
            )
            .unwrap();
    }
    while !store
        .reconcile_step(attempt, ComparisonScope::Comparable, 128)
        .unwrap()
    {}
    while !store.consolidate_step(c, 128).unwrap() {}
    store.seal(c).unwrap();
    store
        .record_measurement(c.generation, root, 100 * g as i64, 100 * g as i64 + 50)
        .unwrap();
}
struct Fixture {
    first: CertifiedGeneration,
    second: Option<CertifiedGeneration>,
    root: PathBuf,
    temp: tempfile::TempDir,
}
fn fixture(second: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("never-acquired-source");
    let c = context(1);
    let mut source = Store::create(
        &temp.path().join("acquisition"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    source.enable_properties().unwrap();
    fill(&mut source, 1, &root);
    generation_a::publish(
        &source,
        &temp.path().join("g1"),
        c.generation,
        &root,
        64 * MIB,
        &Default::default(),
    )
    .unwrap();
    if second {
        let new_root = temp.path().join("never-acquired-new-source");
        fill(&mut source, 2, &new_root);
        generation_a::publish(
            &source,
            &temp.path().join("g2"),
            GenerationId(2),
            &new_root,
            64 * MIB,
            &Default::default(),
        )
        .unwrap();
    }
    drop(source);
    let first = CertifiedGeneration::open(
        &temp.path().join("g1"),
        GenerationId(1),
        &root,
        &Default::default(),
    )
    .unwrap();
    let second = second.then(|| {
        CertifiedGeneration::open(
            &temp.path().join("g2"),
            GenerationId(2),
            &temp.path().join("never-acquired-new-source"),
            &Default::default(),
        )
        .unwrap()
    });
    Fixture {
        first,
        second,
        root,
        temp,
    }
}
fn scratch(f: &Fixture, label: &str) -> PathBuf {
    let path = f.temp.path().join(label);
    fs::create_dir(&path).unwrap();
    path
}
fn snapshot(path: &Path) -> BTreeMap<OsString, (u64, [u8; 32])> {
    fs::read_dir(path)
        .unwrap()
        .map(|item| {
            let item = item.unwrap();
            let metadata = fs::symlink_metadata(item.path()).unwrap();
            assert!(metadata.is_file() && !metadata.is_symlink());
            let mut file = fs::File::open(item.path()).unwrap();
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 16384];
            loop {
                let n = file.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            (item.file_name(), (metadata.len(), hash.finalize().into()))
        })
        .collect()
}
fn bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|item| {
            let item = item.unwrap();
            let meta = fs::symlink_metadata(item.path()).unwrap();
            assert!(!meta.is_symlink());
            if meta.is_dir() {
                bytes(&item.path())
            } else {
                assert!(meta.is_file());
                meta.len()
            }
        })
        .sum()
}
fn bounded(path: &Path, usage: TemporaryUsage, limit: u64) {
    assert_eq!(usage.limit, limit);
    assert!(usage.charged <= usage.peak && usage.peak <= limit);
    assert!(
        bytes(path) <= usage.charged,
        "all visible job files must remain charged"
    );
}
fn full(error: &Error) {
    assert!(
        matches!(error,Error::Sql(rusqlite::Error::SqliteFailure(code,_)) if code.code==rusqlite::ErrorCode::DiskFull),
        "expected SQLITE_FULL at the declared derived budget, got {error:?}"
    );
}

#[test]
fn statistics_and_duplicate_preparation_refuse_small_budgets_without_false_complete_or_source_mutation()
 {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(false);
    let original = snapshot(f.first.directory());
    let query_path = scratch(&f, "query");
    let query = f
        .first
        .query(
            &SearchQuery::default(),
            &query_path,
            16 * MIB,
            &Default::default(),
        )
        .unwrap();
    assert!(query.complete());
    assert_eq!(query.counts.matches, N + 1);
    for quota in [64 * 1024, 128 * 1024] {
        let path = scratch(&f, &format!("statistics-{quota}"));
        let reserved = Governor::global().usage().disk_reserved;
        match query.statistics(&path, quota, &Default::default()) {
            Err(error) => full(&error),
            Ok(statistics) => panic!(
                "quota should not publish aggregate: complete={}",
                statistics.complete
            ),
        }
        assert_eq!(bytes(&path), 0);
        assert_eq!(Governor::global().usage().disk_reserved, reserved);
        let path = scratch(&f, &format!("duplicates-{quota}"));
        match query.prepare_duplicates(&path, quota, &Default::default()) {
            Err(error) => full(&error),
            Ok(job) => panic!(
                "quota should not publish candidates: {}",
                job.candidate_count().unwrap()
            ),
        }
        assert_eq!(bytes(&path), 0);
        assert_eq!(Governor::global().usage().disk_reserved, reserved);
        assert!(query.complete());
        assert_eq!(query.counts.matches, N + 1);
        assert_eq!(snapshot(f.first.directory()), original);
    }
    let path = scratch(&f, "statistics-control");
    let statistics = query
        .statistics(&path, 8 * MIB, &Default::default())
        .unwrap();
    assert!(statistics.complete);
    assert_eq!(statistics.totals.files, N);
    assert_eq!(statistics.totals.known_bytes, u128::from(N / 8 * 28));
    assert_eq!(statistics.totals.unknown_sizes, 0);
    drop(statistics);
    assert_eq!(bytes(&path), 0);
    let path = scratch(&f, "duplicates-live");
    let reserved = Governor::global().usage().disk_reserved;
    let job = query
        .prepare_duplicates(&path, 8 * MIB, &Default::default())
        .unwrap();
    assert_eq!(job.candidate_count().unwrap(), N);
    assert!(!job.summary().verified && !job.summary().complete);
    bounded(&path, job.temporary_usage(), 8 * MIB);
    assert!(job.temporary_usage().charged > 0);
    assert!(Governor::global().usage().disk_reserved >= reserved + 8 * MIB);
    drop(job);
    assert_eq!(bytes(&path), 0);
    assert_eq!(Governor::global().usage().disk_reserved, reserved);
    assert_eq!(snapshot(f.first.directory()), original);
    assert!(!f.root.exists());
}

#[test]
fn ordered_folder_keeps_failed_work_charged_and_never_publishes_a_partial_order() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(false);
    let original = snapshot(f.first.directory());
    let mut fold = FolderFold::default();
    while !f
        .first
        .as_store()
        .folder_fold_step(GenerationId(1), &mut fold)
        .unwrap()
    {}
    for quota in [64 * 1024, 128 * 1024] {
        let path = scratch(&f, &format!("ordered-{quota}"));
        let reserved = Governor::global().usage().disk_reserved;
        match f.first.as_store().start_ordered_folder(
            context(1),
            EntryId(0),
            &f.root,
            Sort::NameDown,
            &path,
            quota,
        ) {
            Err(error) => full(&error),
            Ok(mut job) => {
                let mut failed = false;
                for _ in 0..128 {
                    bounded(&path, job.temporary_usage(), quota);
                    match job.step(f.first.as_store(), &fold) {
                        Err(error) => {
                            full(&error);
                            failed = true;
                            break;
                        }
                        Ok(true) => panic!("oversized order incorrectly completed"),
                        Ok(false) => (),
                    }
                }
                assert!(failed);
                assert!(!job.ready());
                assert!(job.page(None).is_err());
                bounded(&path, job.temporary_usage(), quota);
                assert!(Governor::global().usage().disk_reserved >= reserved + quota);
                drop(job);
            }
        }
        assert_eq!(bytes(&path), 0);
        assert_eq!(Governor::global().usage().disk_reserved, reserved);
        assert_eq!(snapshot(f.first.directory()), original);
    }
    let path = scratch(&f, "ordered-live");
    let reserved = Governor::global().usage().disk_reserved;
    let mut job = f
        .first
        .as_store()
        .start_ordered_folder(
            context(1),
            EntryId(0),
            &f.root,
            Sort::NameDown,
            &path,
            8 * MIB,
        )
        .unwrap();
    // A live partial job is sufficient for the lease test. Full successful
    // orders are covered by public navigation tests; do not raise this local
    // control budget to hide cumulative WAL admission.
    assert!(!job.step(f.first.as_store(), &fold).unwrap());
    assert!(!job.ready());
    assert!(job.page(None).is_err());
    bounded(&path, job.temporary_usage(), 8 * MIB);
    assert!(job.temporary_usage().charged > 0);
    assert!(Governor::global().usage().disk_reserved >= reserved + 8 * MIB);
    drop(job);
    assert_eq!(bytes(&path), 0);
    assert_eq!(Governor::global().usage().disk_reserved, reserved);
    assert_eq!(snapshot(f.first.directory()), original);
    assert!(!f.root.exists());
}

#[test]
fn differential_respects_its_existing_envelope_and_blocks_new_authority_scratch() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(true);
    let second = f.second.as_ref().unwrap();
    let old = snapshot(f.first.directory());
    let new = snapshot(second.directory());
    let path = scratch(&f, "diff");
    let reserved = Governor::global().usage().disk_reserved;
    let mut job = Diff::start(
        f.first.as_store().lease(GenerationId(1)).unwrap(),
        second.as_store().lease(GenerationId(2)).unwrap(),
        &path,
        MIB,
    )
    .unwrap();
    let envelope = 2 * MIB + 8 * MIB;
    let mut finished = false;
    for _ in 0..256 {
        bounded(&path, job.temporary_usage(), envelope);
        if job.advance(&CancellationToken::default()).unwrap() {
            finished = true;
            break;
        }
    }
    assert!(finished && job.summary().finished);
    assert_eq!(job.summary().counts.iter().sum::<u64>(), 2 * N);
    bounded(&path, job.temporary_usage(), envelope);
    assert!(job.temporary_usage().charged > 0);
    assert!(Governor::global().usage().disk_reserved >= reserved + envelope);
    drop(job);
    assert_eq!(bytes(&path), 0);
    assert_eq!(Governor::global().usage().disk_reserved, reserved);
    let illegal = second.directory();
    let refused = Diff::start(
        f.first.as_store().lease(GenerationId(1)).unwrap(),
        second.as_store().lease(GenerationId(2)).unwrap(),
        illegal,
        MIB,
    );
    assert!(matches!(refused, Err(Error::Contract(_))));
    assert!(illegal.is_dir());
    assert!(!f.root.exists());
    assert!(!second.root().exists());
    assert_eq!(snapshot(f.first.directory()), old);
    assert_eq!(snapshot(second.directory()), new);
}
