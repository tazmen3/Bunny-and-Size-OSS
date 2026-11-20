use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
use std::time::{Duration, UNIX_EPOCH};

fn fixture() -> (tempfile::TempDir, Store, CertifiedGeneration) {
    let tmp = tempfile::tempdir().unwrap();
    let context = context_id(1);
    let mut source = Store::create(
        &tmp.path().join("canonical"),
        context.inventory,
        context.source,
        Durability::Retained,
    )
    .unwrap();
    source.create_generation(context).unwrap();
    // Entry identifiers above i64::MAX remain BLOBs; only the dense rank is signed.
    source
        .db
        .execute(
            "UPDATE generations SET next_entry=?1",
            [unsigned(u64::MAX - 100)],
        )
        .unwrap();
    let attempt = attempt_id(1, 0, 1);
    source.begin_attempt(attempt).unwrap();
    let names = [
        "z.TXT",
        "a.log",
        "İΣ.txt",
        "b.txt",
        "unknown.bin",
        "date.txt",
        "zero.txt",
        "max.txt",
        "no-extension",
        ".hidden",
        "link.txt",
        "special.txt",
    ];
    let mut observations: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| Observation {
            name: queries::native_path(Path::new(name)),
            kind: match index {
                4 => Kind::Unknown,
                10 => Kind::Symlink,
                11 => Kind::Special,
                _ => Kind::File,
            },
            logical: match index {
                0 | 4 => None,
                7 => Some(u64::MAX),
                _ => Some((index % 3) as u64),
            },
            allocated: None,
            modified: if index == 5 { None } else { Some((-1, 5)) },
            metadata_known: true,
        })
        .collect();
    #[cfg(unix)]
    observations.push(Observation {
        name: NativeName::Posix(b"native-\xff.txt".to_vec()),
        kind: Kind::File,
        logical: Some(2),
        allocated: None,
        modified: Some((0, 0)),
        metadata_known: true,
    });
    #[cfg(windows)]
    observations.push(Observation {
        name: NativeName::Windows(vec![0x61, 0xd800, 0x2e, 0x74, 0x78, 0x74]),
        kind: Kind::File,
        logical: Some(2),
        allocated: None,
        modified: Some((0, 0)),
        metadata_known: true,
    });
    for logical in [None, Some(2)] {
        #[cfg(unix)]
        let name = NativeName::Posix(if logical.is_some() {
            b"known-size.\xff".to_vec()
        } else {
            b"unknown-size.\xff".to_vec()
        });
        #[cfg(windows)]
        let name = NativeName::Windows(vec![
            if logical.is_some() { 0x6b } else { 0x75 },
            0x2e,
            0xd800,
        ]);
        observations.push(Observation {
            name,
            kind: Kind::File,
            logical,
            allocated: None,
            modified: Some((0, 0)),
            metadata_known: true,
        });
    }
    batch(&mut source, attempt, 0, None, &observations).unwrap();
    batch(&mut source, attempt, 1, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut source, attempt);
    fold(&mut source, 1);
    source.seal(context).unwrap();
    let published = tmp.path().join("published");
    let root = tmp.path().join("offline-root");
    publish(
        &source,
        &published,
        GenerationId(1),
        &root,
        64 * 1024 * 1024,
        &Default::default(),
    )
    .unwrap();
    let certified =
        CertifiedGeneration::open(&published, GenerationId(1), &root, &Default::default()).unwrap();
    (tmp, source, certified)
}
fn rows(query: &Query<'_>) -> Vec<(EntryId, PathBuf, Kind, Option<u64>)> {
    let mut cursor = None;
    let mut result = Vec::new();
    loop {
        let page = query.page(cursor.as_ref()).unwrap();
        result.extend(
            page.rows
                .into_iter()
                .map(|row| (row.key.entry, row.path, row.kind, row.logical)),
        );
        if !page.more {
            break;
        }
        cursor = page.next;
    }
    result
}
fn counts(query: &Query<'_>) -> [u64; 7] {
    let c = query.counts;
    [
        c.evaluated,
        c.matches,
        c.unknown_sizes,
        c.unknown_dates,
        c.native_text,
        c.oversized_paths,
        c.unknown_kinds,
    ]
}
#[test]
fn shared_query_preserves_all_consumers_and_unknown_precedence() {
    let (tmp, source, certified) = fixture();
    let searches = [
        SearchQuery::default(),
        SearchQuery {
            kind: Some(InventoryKind::File),
            ..Default::default()
        },
        SearchQuery {
            extension: Some(".TXT".into()),
            ..Default::default()
        },
        SearchQuery {
            min_bytes: Some(2),
            max_bytes: Some(2),
            ..Default::default()
        },
        SearchQuery {
            kind: Some(InventoryKind::File),
            min_bytes: Some(0),
            name: Some("i\u{307}".into()),
            ..Default::default()
        },
        SearchQuery {
            modified_after: Some(UNIX_EPOCH - Duration::from_secs(2)),
            ..Default::default()
        },
        SearchQuery {
            name: Some("absent".into()),
            ..Default::default()
        },
        SearchQuery {
            glob: Some("*.txt".into()),
            ..Default::default()
        },
        SearchQuery {
            min_bytes: Some(u64::MAX),
            max_bytes: Some(u64::MAX),
            ..Default::default()
        },
    ];
    for (index, search) in searches.into_iter().enumerate() {
        let expected = source
            .query(
                GenerationId(1),
                &certified.root,
                &search,
                tmp.path(),
                8 * 1024 * 1024,
                &Default::default(),
            )
            .unwrap();
        let mut actual = certified
            .start_query(&search, tmp.path(), 8 * 1024 * 1024)
            .unwrap();
        assert!(!actual.complete());
        assert!(!actual.evaluation_finished());
        assert!(actual.page(None).is_err());
        while !actual.advance(&Default::default()).unwrap() {}
        assert_eq!(rows(&actual), rows(&expected));
        assert_eq!(counts(&actual), counts(&expected));
        assert_eq!(actual.complete(), expected.complete());
        let statistics = actual
            .statistics(tmp.path(), 1024 * 1024, &Default::default())
            .unwrap();
        let baseline = expected
            .statistics(tmp.path(), 1024 * 1024, &Default::default())
            .unwrap();
        assert_eq!(statistics.json(), baseline.json());
        let duplicates = actual
            .prepare_duplicates(tmp.path(), 1024 * 1024, &Default::default())
            .unwrap();
        let baseline_duplicates = expected
            .prepare_duplicates(tmp.path(), 1024 * 1024, &Default::default())
            .unwrap();
        assert_eq!(
            duplicates.candidate_count().unwrap(),
            baseline_duplicates.candidate_count().unwrap()
        );
        let export_path = tmp.path().join(format!("actual-{index}.jsonl"));
        let baseline_path = tmp.path().join(format!("baseline-{index}.jsonl"));
        actual
            .export_jsonl(&export_path, 1024 * 1024, &Default::default())
            .unwrap();
        expected
            .export_jsonl(&baseline_path, 1024 * 1024, &Default::default())
            .unwrap();
        assert_eq!(
            std::fs::read(export_path).unwrap(),
            std::fs::read(baseline_path).unwrap()
        );
        let mut order = actual
            .order_by_size(true, tmp.path(), 8 * 1024 * 1024)
            .unwrap();
        let mut baseline_order = expected
            .order_by_size(true, tmp.path(), 8 * 1024 * 1024)
            .unwrap();
        while !order.advance(&Default::default()).unwrap() {}
        while !baseline_order.advance(&Default::default()).unwrap() {}
        let ids = |order: &queries::ordering::SizeOrder<'_, '_>| {
            order
                .page(None, false)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row.key.entry)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&order), ids(&baseline_order));
    }
}
#[test]
fn persistent_query_cancellation_and_foreign_cursors_remain_explicit() {
    let (tmp, _source, certified) = fixture();
    let mut pending = certified
        .start_query(&Default::default(), tmp.path(), 1024 * 1024)
        .unwrap();
    assert!(pending.preview().unwrap().rows.is_empty());
    let stop = CancellationToken::default();
    stop.cancel();
    assert!(pending.advance(&stop).unwrap());
    assert!(pending.counts.cancelled);
    assert!(!pending.complete());
    assert!(pending.page(None).unwrap().rows.is_empty());
    let first = certified
        .query(
            &Default::default(),
            tmp.path(),
            1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let second = certified
        .query(
            &Default::default(),
            tmp.path(),
            1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let cursor = first.page(None).unwrap().next.unwrap();
    assert!(second.page(Some(&cursor)).is_err());
    let mut corrupted = cursor.clone();
    corrupted.id = EntryId(42);
    assert!(first.page_before(&corrupted).is_err());
}

#[test]
fn ranked_pages_reverse_and_cancelled_size_order_preserve_old_view() {
    let tmp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let mut source = Store::create(
        &tmp.path().join("canonical"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    source.create_generation(c).unwrap();
    let attempt = attempt_id(1, 0, 1);
    source.begin_attempt(attempt).unwrap();
    let observations: Vec<_> = (0..620)
        .rev()
        .map(|index| Observation {
            name: queries::native_path(Path::new(&format!("file-{index:04}.txt"))),
            kind: Kind::File,
            logical: Some(index % 7),
            allocated: None,
            modified: None,
            metadata_known: true,
        })
        .collect();
    for (index, chunk) in observations.chunks(128).enumerate() {
        batch(&mut source, attempt, index as u64, None, chunk).unwrap();
    }
    batch(&mut source, attempt, 5, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(&mut source, attempt);
    fold(&mut source, 1);
    source.seal(c).unwrap();
    let root = tmp.path().join("offline");
    let published = tmp.path().join("published");
    publish(
        &source,
        &published,
        GenerationId(1),
        &root,
        16 * 1024 * 1024,
        &Default::default(),
    )
    .unwrap();
    let certified =
        CertifiedGeneration::open(&published, GenerationId(1), &root, &Default::default()).unwrap();
    let indexed = certified
        .query(
            &Default::default(),
            tmp.path(),
            4 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
    let first = indexed.page(None).unwrap();
    let second = indexed.page(first.next.as_ref()).unwrap();
    let backward = indexed.page_before(second.first.as_ref().unwrap()).unwrap();
    assert_eq!(
        first
            .rows
            .iter()
            .map(|row| row.key.entry)
            .collect::<Vec<_>>(),
        backward
            .rows
            .iter()
            .map(|row| row.key.entry)
            .collect::<Vec<_>>()
    );
    let retained = rows(&indexed);
    let mut order = indexed
        .order_by_size(false, tmp.path(), 4 * 1024 * 1024)
        .unwrap();
    assert!(!order.advance(&Default::default()).unwrap());
    let stop = CancellationToken::default();
    stop.cancel();
    assert!(order.advance(&stop).is_err());
    assert!(order.page(None, false).is_err());
    assert_eq!(rows(&indexed), retained);
    let general = SearchQuery {
        name: Some("file-".into()),
        ..Default::default()
    };
    let mut progressive = certified
        .start_query(&general, tmp.path(), 4 * 1024 * 1024)
        .unwrap();
    assert!(!progressive.advance(&Default::default()).unwrap());
    let prefix = progressive.preview().unwrap();
    assert!(!prefix.complete);
    assert!(!prefix.rows.is_empty());
    assert!(prefix.rows.len() <= 200);
    assert!(progressive.advance(&stop).unwrap());
    assert!(progressive.counts.cancelled);
    assert!(!progressive.complete());
    assert!(!progressive.page(None).unwrap().rows.is_empty());
}

#[test]
fn combined_scalar_partitions_match_common_oracle_without_false_unknowns() {
    let (tmp, source, certified) = fixture();
    for kind in [
        None,
        Some(InventoryKind::File),
        Some(InventoryKind::Special),
    ] {
        for extension in [None, Some(".TXT"), Some("..."), Some("absent")] {
            for range in [None, Some((0, 2)), Some((u64::MAX, u64::MAX))] {
                for case_sensitive in [false, true] {
                    let search = SearchQuery {
                        kind,
                        extension: extension.map(str::to_owned),
                        min_bytes: range.map(|r| r.0),
                        max_bytes: range.map(|r| r.1),
                        case_sensitive,
                        ..Default::default()
                    };
                    let expected = source
                        .query(
                            GenerationId(1),
                            &certified.root,
                            &search,
                            tmp.path(),
                            1024 * 1024,
                            &Default::default(),
                        )
                        .unwrap();
                    let actual = certified
                        .query(&search, tmp.path(), 1024 * 1024, &Default::default())
                        .unwrap();
                    assert_eq!(counts(&actual), counts(&expected), "{search:?}");
                    assert_eq!(rows(&actual), rows(&expected), "{search:?}");
                    assert_eq!(actual.complete(), expected.complete(), "{search:?}");
                }
            }
        }
    }
}
