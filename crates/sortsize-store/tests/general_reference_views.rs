//! Public result contracts for bounded general views. These are exactness and
//! lifecycle tests, with no performance thresholds or alternate memory modes.
use sha2::{Digest, Sha256};
use sortsize_core::{
    CancellationToken, InventoryKind, analysis::SearchQuery, inventory_contract::*,
};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{
        Error, Kind, Observation, Store,
        generation_a::{self, CertifiedGeneration},
        queries::{Page, Query},
    },
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::UNIX_EPOCH,
};
const BUDGET: u64 = 16 * 1024 * 1024;
static SERIAL: Mutex<()> = Mutex::new(());
#[derive(Clone, Copy)]
enum Variant {
    Complete,
    Opaque,
    UnknownSize,
    UnknownDate,
    Partial,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Expected {
    key: EntryKey,
    path: PathBuf,
    logical: Option<u64>,
}
struct Fixture {
    cap: CertifiedGeneration,
    expected: Vec<Expected>,
    root: PathBuf,
    total: u64,
    temp: tempfile::TempDir,
}
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(7733),
        source: SourceId(8844),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn name(i: u64, opaque: bool) -> NativeName {
    let text = format!("file-{i:04}.dat");
    #[cfg(windows)]
    {
        if opaque {
            let mut v = "file-".encode_utf16().collect::<Vec<_>>();
            v.push(0xd800);
            v.extend(format!("-{i:04}.dat").encode_utf16());
            NativeName::Windows(v)
        } else {
            NativeName::Windows(text.encode_utf16().collect())
        }
    }
    #[cfg(not(windows))]
    {
        if opaque {
            let mut v = b"file-\xff-".to_vec();
            v.extend(format!("{i:04}.dat").as_bytes());
            NativeName::Posix(v)
        } else {
            NativeName::Posix(text.into_bytes())
        }
    }
}
fn os_name(name: &NativeName) -> OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let NativeName::Windows(v) = name else {
            panic!("native test codec")
        };
        OsString::from_wide(v)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStringExt;
        let NativeName::Posix(v) = name else {
            panic!("native test codec")
        };
        OsString::from_vec(v.clone())
    }
}
fn fixture(n: u64, variant: Variant, long: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let mut root = temp.path().join("recorded-not-created");
    if long {
        for i in 0..15 {
            root.push(format!("segment-{i:02}-{}", "r".repeat(190)));
        }
    }
    let c = context();
    let mut source = Store::create(
        &temp.path().join("acquisition"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    source.enable_properties().unwrap();
    source.create_generation(c).unwrap();
    source.record_acquisition(c, &root, 100).unwrap();
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
    source.begin_attempt(a).unwrap();
    let mut expected = Vec::new();
    let mut observations = Vec::new();
    for i in 0..n {
        let name = name(i, matches!(variant, Variant::Opaque) && i == 0);
        let logical = if matches!(variant, Variant::UnknownSize) && i == n - 1 {
            None
        } else {
            Some(i)
        };
        expected.push(Expected {
            key: EntryKey {
                inventory: c.inventory,
                generation: c.generation,
                entry: EntryId(i + 1),
            },
            path: root.join(os_name(&name)),
            logical,
        });
        observations.push(Observation {
            name,
            kind: Kind::File,
            logical,
            allocated: None,
            modified: if matches!(variant, Variant::UnknownDate) && i == n - 1 {
                None
            } else {
                Some((1, 0))
            },
            metadata_known: true,
        });
    }
    if matches!(variant, Variant::Partial) {
        observations.push(Observation {
            name: name(n, false),
            kind: Kind::Directory,
            logical: None,
            allocated: None,
            modified: None,
            metadata_known: true,
        });
    }
    for (sequence, chunk) in observations.chunks(64).enumerate() {
        let end = (sequence + 1) * 64 >= observations.len();
        source
            .ingest(
                BatchHeader {
                    attempt: a,
                    sequence: sequence as u64,
                    end: end.then_some(ListingEnd::Exhausted),
                },
                chunk,
            )
            .unwrap();
    }
    while !source
        .reconcile_step(a, ComparisonScope::Comparable, 128)
        .unwrap()
    {}
    while !source.consolidate_step(c, 128).unwrap() {}
    source.seal(c).unwrap();
    source
        .record_measurement(c.generation, &root, 100, 200)
        .unwrap();
    generation_a::publish(
        &source,
        &temp.path().join("generation"),
        c.generation,
        &root,
        64 * 1024 * 1024,
        &Default::default(),
    )
    .unwrap();
    drop(source);
    let cap = CertifiedGeneration::open(
        &temp.path().join("generation"),
        c.generation,
        &root,
        &Default::default(),
    )
    .unwrap();
    expected.sort_by(|a, b| a.path.cmp(&b.path).then(a.key.entry.0.cmp(&b.key.entry.0)));
    Fixture {
        cap,
        expected,
        root,
        total: observations.len() as u64 + 1,
        temp,
    }
}
fn scratch(f: &Fixture, label: &str) -> PathBuf {
    let p = f.temp.path().join(label);
    fs::create_dir(&p).unwrap();
    p
}
fn search(max: Option<u64>, min: Option<u64>) -> SearchQuery {
    SearchQuery {
        kind: Some(InventoryKind::File),
        modified_after: Some(UNIX_EPOCH),
        max_bytes: max,
        min_bytes: min,
        ..Default::default()
    }
}
fn snapshot(path: &Path) -> BTreeMap<OsString, (u64, [u8; 32])> {
    fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let mut file = fs::File::open(e.path()).unwrap();
            let mut hash = Sha256::new();
            let mut b = [0; 16384];
            loop {
                let n = file.read(&mut b).unwrap();
                if n == 0 {
                    break;
                }
                hash.update(&b[..n]);
            }
            (
                e.file_name(),
                (e.metadata().unwrap().len(), hash.finalize().into()),
            )
        })
        .collect()
}
fn files(path: &Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                files(&e.path())
            } else {
                e.metadata().unwrap().len()
            }
        })
        .sum()
}
fn rows(page: &Page) -> Vec<Expected> {
    assert!(page.rows.len() <= 200 && page.accounted_bytes <= 512 * 1024);
    page.rows
        .iter()
        .map(|r| {
            assert_eq!(r.kind, Kind::File);
            Expected {
                key: r.key,
                path: r.path.clone(),
                logical: r.logical,
            }
        })
        .collect()
}
fn all(query: &Query<'_>) -> Vec<Expected> {
    let mut found = Vec::new();
    let mut after = None;
    loop {
        let page = query.page(after.as_ref()).unwrap();
        found.extend(rows(&page));
        if !page.more {
            break;
        }
        after = page.next;
        assert!(after.is_some());
    }
    found
}

// Admission can legitimately provide less than 2 MiB. Observe the real initial
// lease, rather than changing limits or mistaking a synced fallback for failure.
fn evaluated<'a>(f: &'a Fixture, criteria: &SearchQuery, path: &Path) -> (Query<'a>, u64) {
    let mut query = f.cap.start_query(criteria, path, BUDGET).unwrap();
    let admitted = query.working_memory_bytes();
    while !query.advance(&Default::default()).unwrap() {}
    (query, admitted)
}
fn storage(query: &Query<'_>, count: u64, admitted: u64) -> bool {
    let reference = count <= 200 && admitted >= 2 * 1024 * 1024;
    assert!(!query.indexed());
    assert_eq!(query.references_generation(), reference);
    assert_eq!(query.new_result_sync_required(), !reference);
    if reference {
        assert!(query.working_memory_bytes() >= 2 * 1024 * 1024);
    }
    reference
}

#[test]
fn cardinalities_zero_one_two_hundred_and_two_hundred_one_keep_exact_storage_and_counts() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(201, Variant::Complete, false);
    let before = snapshot(f.cap.directory());
    for (count, max, min) in [
        (0, None, Some(999)),
        (1, Some(0), None),
        (200, Some(199), None),
        (201, Some(200), None),
    ] {
        let path = scratch(&f, &format!("count-{count}"));
        let (query, admitted) = evaluated(&f, &search(max, min), &path);
        assert!(query.complete());
        let reference = storage(&query, count, admitted);
        assert_eq!(query.counts.evaluated, f.total);
        assert_eq!(query.predicate_evaluated(), f.total);
        assert_eq!(query.counts.matches, count);
        assert_eq!(
            query.counts.unknown_sizes
                + query.counts.unknown_dates
                + query.counts.native_text
                + query.counts.oversized_paths
                + query.counts.unknown_kinds,
            0
        );
        let expected = f
            .expected
            .iter()
            .filter(|e| {
                e.logical
                    .is_some_and(|n| max.is_none_or(|m| n <= m) && min.is_none_or(|m| n >= m))
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(all(&query), expected);
        if reference {
            assert_eq!(
                files(&path),
                0,
                "only the reconstructible intermediate may exist before this view is published"
            );
        } else {
            assert!(files(&path) > 0);
        }
        assert_eq!(snapshot(f.cap.directory()), before);
    }
    assert!(!f.root.exists());
}

#[test]
fn long_native_paths_remain_multipage_reversible_and_reject_foreign_query_cursors() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(201, Variant::Opaque, true);
    let before = snapshot(f.cap.directory());
    let path = scratch(&f, "native-pages");
    let (query, admitted) = evaluated(&f, &search(Some(199), None), &path);
    assert!(query.complete());
    storage(&query, 200, admitted);
    let expected = f
        .expected
        .iter()
        .filter(|e| e.logical.unwrap() <= 199)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(all(&query), expected);
    let first = query.page(None).unwrap();
    assert!(first.more && first.rows.len() < 200);
    let next = query.page(first.next.as_ref()).unwrap();
    let back = query.page_before(next.first.as_ref().unwrap()).unwrap();
    assert_eq!(rows(&back), rows(&first));
    let other_path = scratch(&f, "other-query");
    let other = f
        .cap
        .query(
            &search(Some(199), None),
            &other_path,
            BUDGET,
            &Default::default(),
        )
        .unwrap();
    assert!(other.page(first.next.as_ref()).is_err());
    assert!(matches!(
        Store::reopen(f.cap.directory(), Durability::Retained),
        Err(Error::Contract(
            "inventory already owned or native lock unavailable"
        ))
    ));
    assert_eq!(snapshot(f.cap.directory()), before);
    assert!(!f.root.exists());
}

#[test]
fn unknowns_partial_coverage_and_cancellation_never_become_complete_via_small_views() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for variant in [
        Variant::UnknownSize,
        Variant::UnknownDate,
        Variant::Partial,
        Variant::Opaque,
    ] {
        let f = fixture(201, variant, false);
        let before = snapshot(f.cap.directory());
        let path = scratch(&f, "incomplete");
        let criteria = if matches!(variant, Variant::Partial) {
            search(Some(199), None)
        } else if matches!(variant, Variant::Opaque) {
            SearchQuery {
                kind: Some(InventoryKind::File),
                name: Some("file".into()),
                ..Default::default()
            }
        } else {
            search(Some(200), None)
        };
        let (query, admitted) = evaluated(&f, &criteria, &path);
        storage(&query, 200, admitted);
        assert!(!query.complete());
        assert_eq!(query.counts.matches, 200);
        assert_eq!(query.counts.evaluated, f.total);
        if matches!(variant, Variant::UnknownSize) {
            assert_eq!(query.counts.unknown_sizes, 1);
        }
        if matches!(variant, Variant::UnknownDate) {
            assert_eq!(query.counts.unknown_dates, 1);
        }
        if matches!(variant, Variant::Opaque) {
            assert_eq!(query.counts.native_text, 1);
        }
        if matches!(variant, Variant::Partial) {
            assert_eq!(f.cap.coverage(), ScopeCoverage::Partial);
        }
        let expected = f
            .expected
            .iter()
            .filter(|e| {
                if matches!(variant, Variant::Opaque) {
                    e.key.entry.0 != 1
                } else {
                    e.key.entry.0 <= 200
                }
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(all(&query), expected);
        assert_eq!(snapshot(f.cap.directory()), before);
    }
    let f = fixture(600, Variant::Complete, false);
    let path = scratch(&f, "cancelled");
    let mut query = f
        .cap
        .start_query(&search(Some(199), None), &path, BUDGET)
        .unwrap();
    assert!(!query.complete() && query.page(None).is_err());
    let token = CancellationToken::default();
    assert!(!query.advance(&token).unwrap());
    assert!(query.counts.evaluated > 0 && query.counts.evaluated < f.total);
    assert!(query.counts.matches > 0);
    token.cancel();
    while !query.advance(&token).unwrap() {}
    assert!(query.counts.cancelled && !query.complete());
    assert!(!query.references_generation() && query.new_result_sync_required());
    assert!(!query.preview().unwrap().complete);
    assert!(query.order_by_size(true, &path, BUDGET).is_err());
}

#[test]
fn every_consumer_uses_all_small_reference_members_and_new_sort_still_materializes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(201, Variant::Complete, true);
    let before = snapshot(f.cap.directory());
    let path = scratch(&f, "consumers");
    let (query, admitted) = evaluated(&f, &search(Some(199), None), &path);
    let reference = storage(&query, 200, admitted);
    let groups = scratch(&f, "groups");
    let stats = query
        .statistics(&groups, 4 * 1024 * 1024, &Default::default())
        .unwrap();
    assert!(stats.complete);
    assert_eq!(stats.totals.files, 200);
    assert_eq!(stats.totals.known_bytes, 19900);
    assert_eq!(stats.totals.unknown_sizes, 0);
    drop(stats);
    let duplicate_path = scratch(&f, "duplicates");
    let duplicates = query
        .prepare_duplicates(&duplicate_path, 8 * 1024 * 1024, &Default::default())
        .unwrap();
    assert_eq!(duplicates.candidate_count().unwrap(), 200);
    assert!(!duplicates.summary().verified && !duplicates.summary().complete);
    drop(duplicates);
    let destination = f.temp.path().join("export.jsonl");
    let exported = query
        .export_jsonl(&destination, BUDGET, &Default::default())
        .unwrap();
    assert!(exported.complete);
    assert_eq!(exported.records, 200);
    let records = fs::read_to_string(&destination)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        records.iter().filter(|r| r["record"] == "entry").count(),
        200
    );
    for (i, row) in records
        .iter()
        .filter(|r| r["record"] == "entry")
        .enumerate()
    {
        assert_eq!(row["entry"], (i + 1).to_string());
        assert_eq!(row["logical_bytes"], i.to_string());
        assert_eq!(row["kind"], "file");
        assert_eq!(row["path"]["display"], f.expected[i].path.to_str().unwrap());
        assert!(row["path"]["raw"].is_null());
    }
    assert_eq!(records.last().unwrap()["coverage"], "complete");
    assert_eq!(records.last().unwrap()["matched_not_exported"], "0");
    let before_order = files(&path);
    let mut order = query.order_by_size(true, &path, BUDGET).unwrap();
    while !order.advance(&Default::default()).unwrap() {}
    assert!(
        files(&path) > before_order,
        "a new size-order artifact is not exempt from materialization"
    );
    let mut after = None;
    let mut ids = Vec::new();
    loop {
        let page = order.page(after.as_ref(), false).unwrap();
        ids.extend(rows(&page).iter().map(|r| r.key.entry.0));
        if !page.more {
            break;
        }
        after = page.next;
    }
    assert_eq!(ids, (1..=200).rev().collect::<Vec<_>>());
    drop(order);
    assert_eq!(all(&query).len(), 200);
    assert_eq!(query.new_result_sync_required(), !reference);
    assert_eq!(snapshot(f.cap.directory()), before);
    assert!(!f.root.exists());
}
