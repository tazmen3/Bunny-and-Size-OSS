use sortsize_core::{CancellationToken, Coverage, InventoryKind, analysis::*};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};
fn entry(path: &str, kind: InventoryKind, bytes: Option<u64>) -> AnalysisEntry {
    AnalysisEntry {
        path: Arc::new(PathBuf::from(path)),
        kind,
        logical_bytes: bytes,
        modified: Some(UNIX_EPOCH + Duration::from_secs(10)),
    }
}
fn corpus() -> AnalysisSnapshot {
    AnalysisSnapshot {
        entries: vec![
            entry("root", InventoryKind::Directory, Some(999)),
            entry("root/z.TXT", InventoryKind::File, Some(5)),
            entry("root/Été.txt", InventoryKind::File, Some(0)),
            entry("root/a.bin", InventoryKind::File, Some(1024)),
            entry("root/link", InventoryKind::Symlink, None),
        ],
        scan_coverage: Coverage::Complete,
        entries_omitted: 0,
    }
}
#[test]
fn conjunctive_filters_and_native_order() {
    let s = corpus();
    let c = CancellationToken::default();
    let r = search(
        &s,
        &SearchQuery {
            extension: Some(".txt".into()),
            min_bytes: Some(0),
            max_bytes: Some(5),
            kind: Some(InventoryKind::File),
            ..Default::default()
        },
        50,
        &c,
    )
    .unwrap();
    assert_eq!(r.indices, vec![1, 2]);
    assert_eq!(r.statistics.totals.files, 2);
    assert_eq!(r.statistics.totals.logical_bytes, 5);
    assert_eq!(r.statistics.size_classes[0].files, 1);
    let r = search(
        &s,
        &SearchQuery {
            name: Some("ÉTÉ".into()),
            glob: Some("*.txt".into()),
            ..Default::default()
        },
        50,
        &c,
    )
    .unwrap();
    assert_eq!(r.indices, vec![2]);
}
#[test]
fn unknowns_limits_and_cancellation_are_not_exhaustiveness() {
    let mut s = corpus();
    s.entries[1].modified = None;
    s.entries_omitted = 12;
    let r = search(
        &s,
        &SearchQuery::default(),
        1,
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(r.matching_entries, 5);
    assert_eq!(r.omitted_results, 4);
    assert!(!r.scope.exhaustive);
    assert_eq!(r.statistics.totals.logical_bytes, 1029);
    assert_eq!(r.statistics.totals.files, 3);
    let q = SearchQuery {
        modified_after: Some(UNIX_EPOCH),
        ..Default::default()
    };
    let r = search(&s, &q, 50, &CancellationToken::default()).unwrap();
    assert_eq!(r.unknown_dates, 1);
    assert!(!r.indices.contains(&1));
    let c = CancellationToken::default();
    c.cancel();
    assert!(search(&s, &q, 50, &c).unwrap().cancelled);
}
#[test]
fn rejects_invalid_queries_and_overflow_is_explicit() {
    let mut s = corpus();
    let c = CancellationToken::default();
    assert!(
        search(
            &s,
            &SearchQuery {
                min_bytes: Some(6),
                max_bytes: Some(5),
                ..Default::default()
            },
            50,
            &c
        )
        .is_err()
    );
    assert!(
        search(
            &s,
            &SearchQuery {
                glob: Some("[".into()),
                ..Default::default()
            },
            50,
            &c
        )
        .is_err()
    );
    assert!(
        search(
            &s,
            &SearchQuery {
                name: Some("x".repeat(MAX_QUERY_BYTES + 1)),
                ..Default::default()
            },
            50,
            &c
        )
        .is_err()
    );
    s.entries[1].logical_bytes = Some(u64::MAX);
    assert!(
        search(&s, &SearchQuery::default(), 50, &c)
            .unwrap()
            .statistics
            .totals
            .overflow
    );
}

#[test]
fn fifty_thousand_entries_statistics_do_not_depend_on_result_page_size() {
    let mut s = AnalysisSnapshot {
        entries: vec![entry("root", InventoryKind::Directory, Some(u64::MAX))],
        scan_coverage: Coverage::Partial,
        entries_omitted: 17,
    };
    for index in (0..MAX_ANALYSIS_ENTRIES - 1).rev() {
        s.entries.push(entry(
            &format!("root/sub/{index:05}.txt"),
            InventoryKind::File,
            Some(index as u64),
        ));
    }
    let query = SearchQuery {
        path: Some("root/sub".into()),
        extension: Some("TXT".into()),
        glob: Some("sub/*.txt".into()),
        kind: Some(InventoryKind::File),
        min_bytes: Some(1_000),
        max_bytes: Some(3_000),
        modified_after: Some(UNIX_EPOCH + Duration::from_secs(10)),
        modified_before: Some(UNIX_EPOCH + Duration::from_secs(10)),
        ..Default::default()
    };
    let cancel = CancellationToken::default();
    assert!(search(&s, &query, 0, &cancel).is_err());
    assert!(search(&s, &query, MAX_ANALYSIS_ENTRIES + 1, &cancel).is_err());
    for limit in [1, 2_000, MAX_ANALYSIS_ENTRIES] {
        let result = search(&s, &query, limit, &cancel).unwrap();
        assert_eq!(result.matching_entries, 2_001);
        assert_eq!(result.indices.len(), limit.min(2_001));
        assert_eq!(result.omitted_results, 2_001 - limit.min(2_001));
        assert_eq!(result.statistics.totals.files, 2_001);
        assert_eq!(
            result.statistics.totals.logical_bytes,
            (1_000..=3_000).sum::<u64>()
        );
        assert_eq!(
            result
                .statistics
                .extensions
                .iter()
                .map(|r| r.counts.files)
                .sum::<u64>(),
            2_001
        );
        assert!(!result.scope.exhaustive);
        assert!(!result.complete);
        assert_eq!(result.scope.omitted_inventory_entries, 17);
        assert!(
            result
                .indices
                .windows(2)
                .all(|pair| s.entries[pair[0]].path < s.entries[pair[1]].path)
        );
    }
}
#[cfg(unix)]
#[test]
fn invalid_native_names_remain_distinct_and_exportable() {
    use std::os::unix::ffi::OsStringExt;
    let mut s = corpus();
    s.entries[1].path = Arc::new(PathBuf::from(std::ffi::OsString::from_vec(
        b"root/\xff.txt".to_vec(),
    )));
    let c = CancellationToken::default();
    let all = search(&s, &SearchQuery::default(), 50, &c).unwrap();
    assert!(all.indices.contains(&1));
    let r = search(
        &s,
        &SearchQuery {
            path: Some("root".into()),
            ..Default::default()
        },
        50,
        &c,
    )
    .unwrap();
    assert_eq!(r.unsearchable_native_text, 1);
    assert!(!r.indices.contains(&1));
}
