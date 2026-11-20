use super::*;
use crate::Durability;
use sortsize_core::{
    Coverage,
    analysis::{AnalysisEntry, AnalysisSnapshot, search},
};
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
pub(super) fn fixture(n: u64) -> (tempfile::TempDir, Store) {
    fixture_with(n, 1, |i| Observation {
        name: native_path(Path::new(&format!("file-{i:08}.TXT"))),
        kind: Kind::File,
        logical: Some(i),
        allocated: None,
        modified: Some((-1, 5)),
        metadata_known: true,
    })
}
pub(super) fn fixture_with(
    n: u64,
    first_id: u64,
    observation: impl Fn(u64) -> Observation,
) -> (tempfile::TempDir, Store) {
    let (tmp, mut store) = unsealed_fixture_with(n, first_id, observation);
    store.seal(context()).unwrap();
    (tmp, store)
}

fn unsealed_fixture_with(
    n: u64,
    first_id: u64,
    observation: impl Fn(u64) -> Observation,
) -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &tmp.path().join("store"),
        InventoryId(1),
        SourceId(1),
        Durability::Retained,
    )
    .unwrap();
    let c = context();
    s.create_generation(c).unwrap();
    s.db.execute(
        "UPDATE generations SET next_entry=?2 WHERE id=?1",
        params![unsigned(1), unsigned(first_id)],
    )
    .unwrap();
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
    let mut seq = 0;
    for first in (0..n).step_by(256) {
        let rows = (first..(first + 256).min(n))
            .map(&observation)
            .collect::<Vec<_>>();
        s.ingest(
            BatchHeader {
                attempt: a,
                sequence: seq,
                end: None,
            },
            &rows,
        )
        .unwrap();
        seq += 1;
    }
    s.ingest(
        BatchHeader {
            attempt: a,
            sequence: seq,
            end: Some(ListingEnd::Exhausted),
        },
        &[],
    )
    .unwrap();
    while !s
        .reconcile_step(a, ComparisonScope::Comparable, 512)
        .unwrap()
    {}
    while !s.consolidate_step(c, 512).unwrap() {}
    (tmp, s)
}
#[test]
fn all_late_results_paged_without_duplicates_or_source_io() {
    let (tmp, s) = fixture(50_010);
    let root = tmp.path().join("never-created");
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            tmp.path(),
            64 * 1024 * 1024,
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(q.complete());
    assert_eq!(q.counts.matches, 50_011);
    let mut cursor = None;
    let mut ids = std::collections::HashSet::new();
    loop {
        let p = q.page(cursor.as_ref()).unwrap();
        assert!(p.rows.len() <= 200 && p.accounted_bytes <= 512 * 1024);
        for r in p.rows {
            assert!(ids.insert(r.key.entry));
        }
        if !p.more {
            break;
        }
        cursor = p.next;
    }
    assert_eq!(ids.len(), 50_011);
    assert!(ids.contains(&EntryId(50_010)));
    assert!(!root.exists());
    // Cursor ownership survives its query. This is not a two-query admission test:
    // release the 4 MiB lease before checking it against a different query.
    drop(q);
    let other = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            tmp.path(),
            64 * 1024 * 1024,
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(other.page(cursor.as_ref()).is_err());
}
#[test]
fn shared_predicate_matches_retained_oracle_and_cancel_is_partial() {
    let (tmp, s) = fixture(40);
    let root = tmp.path().join("offline");
    let entries = std::iter::once(AnalysisEntry {
        path: Arc::new(root.clone()),
        kind: InventoryKind::Directory,
        logical_bytes: None,
        modified: None,
    })
    .chain((0..40).map(|i| AnalysisEntry {
        path: Arc::new(root.join(format!("file-{i:08}.TXT"))),
        kind: InventoryKind::File,
        logical_bytes: Some(i),
        modified: time(Some(-1), Some(5)),
    }))
    .collect();
    let snapshot = AnalysisSnapshot {
        entries,
        scan_coverage: Coverage::Complete,
        entries_omitted: 0,
    };
    for search_query in [
        SearchQuery::default(),
        SearchQuery {
            extension: Some("txt".into()),
            min_bytes: Some(20),
            ..Default::default()
        },
        SearchQuery {
            glob: Some("file-*3.TXT".into()),
            case_sensitive: true,
            ..Default::default()
        },
        SearchQuery {
            name: Some("FILE".into()),
            ..Default::default()
        },
        SearchQuery {
            modified_after: Some(SystemTime::UNIX_EPOCH),
            ..Default::default()
        },
    ] {
        let oracle = search(&snapshot, &search_query, 200, &CancellationToken::default()).unwrap();
        let q = s
            .query(
                GenerationId(1),
                &root,
                &search_query,
                tmp.path(),
                1024 * 1024,
                &CancellationToken::default(),
            )
            .unwrap();
        let page = q.page(None).unwrap();
        assert_eq!(
            page.rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
            oracle
                .indices
                .iter()
                .map(|i| snapshot.entries[*i].path.as_ref().clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(q.counts.unknown_dates, oracle.unknown_dates as u64);
        assert_eq!(q.counts.unknown_sizes, oracle.unknown_sizes as u64);
    }
    let cancel = CancellationToken::default();
    cancel.cancel();
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            tmp.path(),
            1024 * 1024,
            &cancel,
        )
        .unwrap();
    assert!(q.counts.cancelled);
    assert!(!q.complete());
}
#[test]
fn native_component_sort_key_matches_path_order() {
    let mut paths = ["a/b", "a.", "a", "b", "É", "é", "a/b/c", "a/b.", "日本語"]
        .map(PathBuf::from)
        .to_vec();
    let mut encoded = paths.clone();
    paths.sort();
    encoded.sort_by_key(|p| path_order(p));
    assert_eq!(paths, encoded);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
        let text = PathBuf::from("z");
        assert_eq!(
            invalid.cmp(&text),
            path_order(&invalid).cmp(&path_order(&text))
        );
    }
}
#[test]
fn reverse_pages_top_and_query_disk_failure_leave_source_intact() {
    let (tmp, s) = fixture(2450);
    let root = tmp.path().join("offline");
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            tmp.path(),
            1024 * 1024,
            &CancellationToken::default(),
        )
        .unwrap();
    let a = q.page(None).unwrap();
    let b = q.page(a.next.as_ref()).unwrap();
    let previous = q.page_before(b.first.as_ref().unwrap()).unwrap();
    assert_eq!(
        a.rows.iter().map(|r| r.key.entry).collect::<Vec<_>>(),
        previous
            .rows
            .iter()
            .map(|r| r.key.entry)
            .collect::<Vec<_>>()
    );
    let top = s.top100(GenerationId(1), &root).unwrap();
    assert_eq!(top.rows.len(), 100);
    assert_eq!(top.rows[0].logical, Some(2449));
    assert!(!top.provisional);
    let tiles = s.treemap(GenerationId(1), EntryId(0), &root).unwrap();
    assert_eq!(tiles.rows.len(), 256);
    assert!(
        s.query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            tmp.path(),
            64 * 1024,
            &CancellationToken::default()
        )
        .is_err()
    );
    assert_eq!(s.entry_count(GenerationId(1)).unwrap(), 2451);
    assert!(q.db.is_none());
    assert_eq!(q.native.as_ref().unwrap().count, 2451);
}

#[test]
fn folder_top100_preserves_partial_coverage_and_excludes_revoked_branches() {
    let (tmp, mut store) = unsealed_fixture_with(4, 1, |id| Observation {
        name: native_path(Path::new(&format!("entry-{id}"))),
        kind: if id < 2 { Kind::Directory } else { Kind::File },
        logical: (id >= 2).then_some(id * 10),
        allocated: None,
        modified: None,
        metadata_known: true,
    });
    let c = context();
    store
        .db
        .execute(
            "UPDATE entries SET parent=?1 WHERE id IN (?2,?3)",
            params![unsigned(1), unsigned(2), unsigned(3)],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE entries SET parent=?1 WHERE id=?2",
            params![unsigned(2), unsigned(4)],
        )
        .unwrap();
    store
        .db
        .execute("UPDATE entries SET present=0 WHERE id=?1", [unsigned(2)])
        .unwrap();
    assert_eq!(store.seal(c).unwrap(), ScopeCoverage::Partial);
    let key = EntryKey {
        inventory: c.inventory,
        generation: c.generation,
        entry: EntryId(1),
    };
    let view = ViewContext {
        inventory: c.inventory,
        source: c.source,
        generation: c.generation,
        scan: c.scan,
        validation: c.validation,
        view: ViewId(1),
        epoch: ViewEpoch(1),
        query: None,
        presentation: PresentationEpoch(1),
    };
    let root = tmp.path().join("offline");
    let frame = store
        .navigation_frame(c, view, navigation::Mode::FolderTop100(key), &root)
        .unwrap();
    assert_eq!(frame.coverage, ScopeCoverage::Partial);
    assert_eq!(frame.rows.len(), 1);
    assert_eq!(frame.rows[0].path, root.join("entry-0").join("entry-2"));
    assert_eq!(frame.rows[0].logical, Some(20));
}

#[test]
fn integration_previous_page_then_next_returns_same_page() {
    let (tmp, store) = fixture(450);
    let query = store
        .query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &SearchQuery::default(),
            tmp.path(),
            1024 * 1024,
            &CancellationToken::default(),
        )
        .unwrap();
    let first = query.page(None).unwrap();
    let second = query.page(first.next.as_ref()).unwrap();
    let previous = query.page_before(second.first.as_ref().unwrap()).unwrap();
    let forward_again = query.page(previous.next.as_ref()).unwrap();
    assert_eq!(
        second.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
        forward_again.rows.iter().map(|r| r.key).collect::<Vec<_>>()
    );
}

#[test]
fn progressive_preview_is_bounded_not_complete_and_has_no_stable_page_until_finished() {
    let (tmp, store) = fixture(4000);
    let mut q = store
        .start_query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &SearchQuery::default(),
            tmp.path(),
            8 * 1024 * 1024,
        )
        .unwrap();
    assert!(!q.complete());
    assert!(q.page(None).is_err());
    assert!(q.preview().unwrap().rows.is_empty());
    assert!(!q.advance(&CancellationToken::default()).unwrap());
    let preview = q.preview().unwrap();
    assert!(!preview.evaluation_finished);
    assert!(!preview.complete);
    assert!(!preview.rows.is_empty());
    assert!(preview.rows.len() <= 200);
    assert!(preview.accounted_bytes <= 512 * 1024);
    assert!(preview.counts.evaluated <= 256);
    let cancel = CancellationToken::default();
    cancel.cancel();
    assert!(q.advance(&cancel).unwrap());
    assert!(q.counts.cancelled);
    assert!(!q.complete());
    assert!(q.page(None).is_ok());
    drop(q);
    let mut q = store
        .start_query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &SearchQuery {
                name: Some("00003999".into()),
                ..Default::default()
            },
            tmp.path(),
            8 * 1024 * 1024,
        )
        .unwrap();
    q.advance(&CancellationToken::default()).unwrap();
    assert_eq!(q.counts.matches, 0);
    assert!(!q.complete());
    while !q.advance(&CancellationToken::default()).unwrap() {}
    assert!(q.complete());
    assert_eq!(q.counts.matches, 1);
    assert_eq!(q.counts.evaluated, 4001);
}

#[test]
fn failed_progressive_transaction_keeps_committed_counts_and_cursor() {
    let (tmp, store) = fixture(2000);
    let mut q = store
        .start_query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &SearchQuery::default(),
            tmp.path(),
            64 * 1024,
        )
        .unwrap();
    let mut prior = 0;
    loop {
        match q.advance(&CancellationToken::default()) {
            Ok(false) => prior = q.counts.matches,
            Ok(true) => panic!("tiny result quota should fail"),
            Err(_) => break,
        }
    }
    assert_eq!(q.counts.matches, prior);
    assert!(
        q.predicate_evaluated() > q.counts.evaluated,
        "rolled-back predicate calls remain work, not committed coverage"
    );
    assert!(!q.complete());
    let stored: i64 =
        q.db.as_ref()
            .unwrap()
            .query_row("SELECT count(*) FROM results", [], |r| r.get(0))
            .unwrap();
    assert_eq!(stored as u64, prior);
    assert_eq!(store.entry_count(GenerationId(1)).unwrap(), 2001);
}

#[test]
fn query_cache_admission_scales_and_evaluation_capacity_is_released_for_reports() {
    use sortsize_core::resources::{Category, Governor, MemoryReading};
    let (t, s) = fixture(40);
    let mib = 1024 * 1024;
    // Exercise 1/2/4 MiB query admissions under the current available-RAM policy.
    for available in [32 * mib, 64 * mib, 128 * mib] {
        let g = Governor::new(
            MemoryMode::Auto,
            Some(MemoryReading {
                total: 32 * 1024 * mib,
                available,
            }),
            8 * 1024 * mib,
        );
        let lease = Store::query_memory_in(&g).unwrap();
        let admitted = lease.bytes();
        assert_eq!(admitted, available / 32);
        let mut q = s
            .start_query_admitted(
                GenerationId(1),
                &t.path().join("offline"),
                &SearchQuery::default(),
                t.path(),
                1024 * 1024,
                lease,
            )
            .unwrap();
        assert_eq!(
            q.db.as_ref()
                .unwrap()
                .query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            -((admitted / 2 / 1024) as i64)
        );
        while !q.advance(&CancellationToken::default()).unwrap() {}
        assert!(q.complete());
        assert_eq!(q.counts.matches, 41);
        assert_eq!(q.page(None).unwrap().rows.len(), 41);
        assert_eq!(q._memory.bytes(), mib);
        assert!(q.db.is_none(), "evaluation cache and DB must be released");
        drop(q);
        assert_eq!(g.usage().categories[Category::Queries as usize], 0);
    }
    let g = Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 32 * 1024 * mib,
            available: 16 * mib,
        }),
        8 * 1024 * mib,
    );
    assert!(matches!(
        Store::query_memory_in(&g),
        Err(Error::MemoryAdmission {
            minimum: 1_048_576,
            ..
        })
    ));
}

#[test]
fn nested_paths_survive_buffer_reuse_with_unicode_and_unknown_dates() {
    let t = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &t.path().join("store"),
        InventoryId(1),
        SourceId(1),
        Durability::Retained,
    )
    .unwrap();
    let c = context();
    s.create_generation(c).unwrap();
    let root = t.path().join("offline");
    let mut expected = vec![AnalysisEntry {
        path: Arc::new(root.clone()),
        kind: InventoryKind::Directory,
        logical_bytes: None,
        modified: None,
    }];
    for (directory, names) in [
        (0, vec!["long-directory-name", "b"]),
        (1, vec!["Été.TXT", "short.txt"]),
        (2, vec!["ǅ.TXT", "last.txt"]),
    ] {
        let a = AttemptKey {
            task: TaskKey {
                context: c,
                directory: EntryId(directory),
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(1),
            owner: OwnerEpoch(1),
            branch: BranchRevision(1),
        };
        s.begin_attempt(a).unwrap();
        let parent = match directory {
            1 => root.join("long-directory-name"),
            2 => root.join("b"),
            _ => root.clone(),
        };
        let kind = if directory == 0 {
            Kind::Directory
        } else {
            Kind::File
        };
        let rows: Vec<_> = names
            .iter()
            .map(|name| {
                expected.push(AnalysisEntry {
                    path: Arc::new(parent.join(name)),
                    kind: if directory == 0 {
                        InventoryKind::Directory
                    } else {
                        InventoryKind::File
                    },
                    logical_bytes: Some(directory),
                    modified: None,
                });
                Observation {
                    name: native_path(Path::new(name)),
                    kind,
                    logical: Some(directory),
                    allocated: None,
                    modified: None,
                    metadata_known: true,
                }
            })
            .collect();
        s.ingest(
            BatchHeader {
                attempt: a,
                sequence: 0,
                end: Some(ListingEnd::Exhausted),
            },
            &rows,
        )
        .unwrap();
        while !s
            .reconcile_step(a, ComparisonScope::Comparable, 512)
            .unwrap()
        {}
    }
    while !s.consolidate_step(c, 512).unwrap() {}
    s.seal(c).unwrap();
    let snapshot = AnalysisSnapshot {
        entries: expected,
        scan_coverage: Coverage::Complete,
        entries_omitted: 0,
    };
    for search_query in [
        SearchQuery::default(),
        SearchQuery {
            name: Some("ǆ".into()),
            ..Default::default()
        },
        SearchQuery {
            extension: Some("txt".into()),
            ..Default::default()
        },
        SearchQuery {
            path: Some("directory".into()),
            ..Default::default()
        },
        SearchQuery {
            modified_after: Some(SystemTime::UNIX_EPOCH),
            ..Default::default()
        },
    ] {
        let oracle = search(&snapshot, &search_query, 200, &CancellationToken::default()).unwrap();
        let q = s
            .query(
                GenerationId(1),
                &root,
                &search_query,
                t.path(),
                1024 * 1024,
                &CancellationToken::default(),
            )
            .unwrap();
        let paths: Vec<_> = q
            .page(None)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.path)
            .collect();
        let expected_paths: Vec<_> = oracle
            .indices
            .iter()
            .map(|i| snapshot.entries[*i].path.as_ref().clone())
            .collect();
        assert_eq!(paths, expected_paths);
        assert_eq!(q.counts.unknown_dates as usize, oracle.unknown_dates);
        assert_eq!(q.complete(), oracle.complete);
    }
    assert!(!root.exists());
}

#[test]
fn compact_native_keys_preserve_component_order_and_root_without_text_conversion() {
    let root = tempfile::tempdir().unwrap();
    let mut names = vec![
        "",
        "a",
        "a/b",
        "a.",
        "a/b.",
        "a/b/c",
        "b",
        "É",
        "é",
        "日本語",
        "a\0z",
        "a\u{1}z",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<Vec<_>>();
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        for bytes in [vec![0xff], vec![b'a', 0, 255], vec![0xfe, b'/', 0xff]] {
            names.push(OsString::from_vec(bytes).into());
        }
    }
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        for wide in [
            vec![0xd800],
            vec![0xdfff],
            vec![0xd800, 0xdc00],
            vec![97, 0, 98],
        ] {
            names.push(OsString::from_wide(&wide).into());
        }
    }
    let mut native = names
        .iter()
        .map(|n| root.path().join(n))
        .collect::<Vec<_>>();
    let mut compact = native.clone();
    native.sort();
    compact.sort_by_cached_key(|p| compact_path_order(p, root.path()).unwrap());
    assert_eq!(native, compact);
    assert_eq!(compact.first().unwrap(), root.path());
    assert!(compact_path_order(&root.path().join("child"), &root.path().join("other")).is_err());
}

#[test]
fn deferred_rows_keep_byte_limited_pages_and_common_root_out_of_projection() {
    let (tmp, store) = fixture(450);
    let mut root = tmp.path().join("offline");
    for _ in 0..24 {
        root.push("x".repeat(100));
    }
    let query = store
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            tmp.path(),
            1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    assert!(query.db.is_none());
    let native = query.native.as_ref().unwrap();
    assert_eq!(
        std::fs::metadata(native.path()).unwrap().len(),
        sequence::Sequence::<8>::planned_bytes(451).unwrap()
    );
    let first = query.page(None).unwrap();
    assert!(first.rows.len() < 200 && first.more);
    assert_eq!(first.rows[0].key.entry, EntryId(0));
    assert_eq!(first.rows[0].path, root);
    let second = query.page(first.next.as_ref()).unwrap();
    let previous = query.page_before(second.first.as_ref().unwrap()).unwrap();
    assert_eq!(
        first.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
        previous.rows.iter().map(|r| r.key).collect::<Vec<_>>()
    );
    let mut page = first;
    let mut count = 0;
    loop {
        assert!(page.accounted_bytes < 512 * 1024);
        for row in &page.rows {
            assert_eq!(row.key.entry, EntryId(count));
            if count > 0 {
                assert_eq!(row.path, root.join(format!("file-{:08}.TXT", count - 1)));
                assert_eq!(row.logical, Some(count - 1));
            }
            count += 1;
        }
        if !page.more {
            break;
        }
        page = query.page(page.next.as_ref()).unwrap();
    }
    assert_eq!(count, 451);
    assert!(!root.exists());
}

#[test]
fn intermediate_write_failure_does_not_publish_or_change_the_canonical_generation() {
    let (tmp, store) = fixture(2300);
    let canonical = || {
        ["inventory.db", "inventory.db-wal", "owner.lock"]
            .map(|name| std::fs::read(store.directory().join(name)).ok())
    };
    let before = canonical();
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    {
        let mut q = store
            .start_query(
                GenerationId(1),
                &tmp.path().join("offline"),
                &Default::default(),
                &scratch,
                8 * 1024 * 1024,
            )
            .unwrap();
        assert!(!q.advance(&Default::default()).unwrap());
        let evaluated = q.counts.evaluated;
        let matches = q.counts.matches;
        let after = q.after;
        q.workspace.inject("vfs_write");
        assert!(q.advance(&Default::default()).is_err());
        assert_eq!(q.counts.evaluated, evaluated);
        assert_eq!(q.counts.matches, matches);
        assert_eq!(q.after, after);
        assert!(!q.complete());
        assert!(q.native.is_none());
        assert!(q.page(None).is_err());
        assert_eq!(
            q.db.as_ref()
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM results", [], |r| r.get(0))
                .unwrap(),
            i64::try_from(matches).unwrap()
        );
    }
    assert_eq!(canonical(), before);
    assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    let q = store
        .query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &Default::default(),
            &scratch,
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    assert!(q.complete());
    let mut reader = q.native.as_ref().unwrap().reader().unwrap();
    for id in 0..=2300 {
        assert_eq!(reader.next().unwrap(), Some(unsigned(id)));
    }
    assert!(reader.next().unwrap().is_none());
    assert_eq!(canonical(), before);
}

#[test]
fn failed_conversion_keeps_sqlite_view_and_can_retry_without_duplicate_ids() {
    let (tmp, store) = fixture(2300);
    for fault in ["write", "sync"] {
        let mut q = store
            .start_query(
                GenerationId(1),
                &tmp.path().join("offline"),
                &Default::default(),
                tmp.path(),
                8 * 1024 * 1024,
            )
            .unwrap();
        while !q.evaluation_done {
            assert!(!q.advance(&Default::default()).unwrap());
        }
        let prior = q
            .preview()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.key)
            .collect::<Vec<_>>();
        q.workspace.inject(fault);
        loop {
            match q.advance(&Default::default()) {
                Err(_) => break,
                Ok(false) => (),
                Ok(true) => panic!("conversion should fail"),
            }
        }
        assert!(q.db.is_some() && q.native.is_none() && q.conversion.is_none());
        assert_eq!(
            q.preview()
                .unwrap()
                .rows
                .iter()
                .map(|r| r.key)
                .collect::<Vec<_>>(),
            prior
        );
        while !q.advance(&Default::default()).unwrap() {}
        let mut reader = q.native.as_ref().unwrap().reader().unwrap();
        for id in 0..=2300 {
            assert_eq!(reader.next().unwrap(), Some(unsigned(id)));
        }
        assert!(reader.next().unwrap().is_none());
    }
}

#[test]
fn sequence_corruption_and_truncation_are_errors_not_partial_success() {
    use std::io::{Seek, SeekFrom, Write};
    let (tmp, store) = fixture(2300);
    let q = store
        .query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &Default::default(),
            tmp.path(),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let native = q.native.as_ref().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(native.path())
        .unwrap();
    // Corrupt an uncached block without altering its length.
    file.seek(SeekFrom::Start(64 + 8224)).unwrap();
    file.write_all(&[0xff; 8]).unwrap();
    file.sync_all().unwrap();
    assert!(native.at(1024).is_err());
    file.set_len(64).unwrap();
    assert!(q.page(None).is_err());
    assert_eq!(store.entry_count(GenerationId(1)).unwrap(), 2301);
}

#[test]
fn external_size_order_matches_native_oracle_with_u64_ids_ties_unknowns_and_invalid_text() {
    let make = |i: u64| {
        let base = format!("name-{:04}-é", 1999 - i);
        #[cfg(unix)]
        let name = {
            let mut bytes = base.into_bytes();
            if i.is_multiple_of(7) {
                bytes.push(0xff);
            }
            NativeName::Posix(bytes)
        };
        #[cfg(windows)]
        let name = {
            let mut units = base.encode_utf16().collect::<Vec<_>>();
            if i.is_multiple_of(7) {
                units.push(0xd800);
            }
            NativeName::Windows(units)
        };
        #[cfg(not(any(unix, windows)))]
        let name = NativeName::Opaque(base.into_bytes());
        Observation {
            name,
            kind: Kind::File,
            logical: match i % 5 {
                0 => None,
                1 => Some(0),
                2 => Some(u64::MAX),
                _ => Some(7),
            },
            allocated: None,
            modified: None,
            metadata_known: true,
        }
    };
    let first_id = u64::MAX - 4096;
    let (tmp, store) = fixture_with(2000, first_id, make);
    let root = tmp.path().join("offline");
    let q = store
        .query(
            GenerationId(1),
            &root,
            &Default::default(),
            tmp.path(),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let mut expected = vec![(0, None, root.clone())];
    for i in 0..2000 {
        let o = make(i);
        expected.push((
            first_id + i,
            o.logical,
            root.join(crate::scheduler::native_name(&o.name).unwrap()),
        ));
    }
    for descending in [false, true] {
        expected.sort_by(|a, b| {
            let cmp = match (a.1, b.1) {
                (Some(a), Some(b)) => {
                    if descending {
                        b.cmp(&a)
                    } else {
                        a.cmp(&b)
                    }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            cmp.then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0))
        });
        let mut order = q
            .order_by_size(descending, tmp.path(), 8 * 1024 * 1024)
            .unwrap();
        while !order.advance(&Default::default()).unwrap() {}
        let first = order.page(None, false).unwrap();
        let second = order.page(first.next.as_ref(), false).unwrap();
        assert_eq!(
            first.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
            order
                .page(second.first.as_ref(), true)
                .unwrap()
                .rows
                .iter()
                .map(|r| r.key)
                .collect::<Vec<_>>()
        );
        let mut page = first;
        let mut index = 0;
        loop {
            for row in &page.rows {
                let e = &expected[index];
                assert_eq!((row.key.entry.0, row.logical, &row.path), (e.0, e.1, &e.2));
                index += 1;
            }
            if !page.more {
                break;
            }
            page = order.page(page.next.as_ref(), false).unwrap();
        }
        assert_eq!(index, expected.len());
    }
}

#[test]
fn short_read_does_not_relabel_partial_bytes_as_the_previous_cached_block() {
    use std::io::{Seek, SeekFrom, Write};
    let (tmp, store) = fixture(2300);
    let q = store
        .query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &Default::default(),
            tmp.path(),
            8 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let native = q.native.as_ref().unwrap();
    let original = std::fs::read(native.path()).unwrap();
    assert_eq!(native.at(1).unwrap(), unsigned(1));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(native.path())
        .unwrap();
    file.set_len(64 + 8224 + 256).unwrap();
    assert!(native.at(1024).is_err());
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&original).unwrap();
    file.sync_all().unwrap();
    assert_eq!(native.at(1).unwrap(), unsigned(1));
}

#[test]
fn conversion_refusal_is_before_growth_and_retains_all_sqlite_matches() {
    let (tmp, store) = fixture(2300);
    let mut q = store
        .start_query(
            GenerationId(1),
            &tmp.path().join("offline"),
            &Default::default(),
            tmp.path(),
            8 * 1024 * 1024,
        )
        .unwrap();
    while !q.evaluation_done {
        assert!(!q.advance(&Default::default()).unwrap());
    }
    let held = q.workspace.file().unwrap();
    let available = 8 * 1024 * 1024 - (8 * 1024 * 1024 / 64) - q.temporary_usage().charged;
    held.grow(available).unwrap();
    let prior = q.temporary_usage();
    assert!(q.advance(&Default::default()).is_err());
    assert_eq!(q.temporary_usage().charged, prior.charged);
    assert!(q.native.is_none() && q.db.is_some());
    assert_eq!(
        q.db.as_ref()
            .unwrap()
            .query_row("SELECT count(*) FROM results", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2301
    );
    drop(held);
    while !q.advance(&Default::default()).unwrap() {}
    assert!(q.complete());
}
