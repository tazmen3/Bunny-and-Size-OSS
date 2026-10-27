//! Reproducible retained-inventory query fixture; no source data is read.
use sortsize_core::{CancellationToken, Coverage, InventoryKind, analysis::*};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
fn main() {
    let snapshot = AnalysisSnapshot {
        entries: (0..50_000)
            .map(|i| AnalysisEntry {
                path: Arc::new(PathBuf::from(format!(
                    "/fixture/directory-{}/unicode-été-日本語-{i:05}.txt",
                    i / 100
                ))),
                kind: InventoryKind::File,
                logical_bytes: Some(i as u64),
                modified: None,
            })
            .collect(),
        scan_coverage: Coverage::Complete,
        entries_omitted: 123,
    };
    let mut runs = vec![];
    for _ in 0..10 {
        let start = Instant::now();
        let r = search(
            &snapshot,
            &SearchQuery {
                extension: Some("txt".into()),
                min_bytes: Some(10_000),
                ..Default::default()
            },
            2000,
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(r.matching_entries, 40_000);
        assert_eq!(
            r.statistics.totals.logical_bytes,
            (10_000u64 + 49_999) * 40_000 / 2
        );
        runs.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let c = CancellationToken::default();
    let token = c.clone();
    let start = Instant::now();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2));
        token.cancel();
        Instant::now()
    });
    let result = search(
        &snapshot,
        &SearchQuery {
            path: Some("unicode".into()),
            ..Default::default()
        },
        2000,
        &c,
    )
    .unwrap();
    let finished = Instant::now();
    let cancelled_at = handle.join().unwrap();
    println!(
        "{}",
        serde_json::json!({"entries":50000,"path_bytes":snapshot.entries.iter().map(|e|e.path.as_os_str().as_encoded_bytes().len()).sum::<usize>(),"query_ms":runs,"cancel_observed":result.cancelled,"cancel_latency_ms":finished.saturating_duration_since(cancelled_at).as_secs_f64()*1000.0,"cancel_total_ms":start.elapsed().as_secs_f64()*1000.0,"source":"synthetic retained inventory, not disk or network"})
    );
}
