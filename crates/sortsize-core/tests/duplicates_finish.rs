use sortsize_core::{analysis::*, duplicates::*, *};
use std::fs;

#[test]
fn unknown_file_sizes_cannot_claim_complete_duplicate_coverage() {
    let f = tempfile::tempdir().unwrap();
    for name in ["a", "b", "unknown"] {
        fs::write(f.path().join(name), b"same").unwrap();
    }
    let scan = scan_inventory_with_dates(
        f.path(),
        &Default::default(),
        &Default::default(),
        Default::default(),
        |_| {},
    )
    .unwrap();
    let mut snapshot = AnalysisSnapshot::from_inventory(&scan.inventory, &scan.dates).unwrap();
    snapshot
        .entries
        .iter_mut()
        .find(|e| e.path.ends_with("unknown"))
        .unwrap()
        .logical_bytes = None;
    let selected = search(&snapshot, &Default::default(), 50_000, &Default::default()).unwrap();
    assert!(selected.complete); // The search can retain an unknown size without a size predicate.
    let report = find(
        &snapshot,
        &selected,
        Default::default(),
        &Default::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.groups.len(), 1);
    assert!(!report.complete); // The duplicate pass could not classify the third file.
}

#[test]
fn exact_read_budget_includes_final_binary_comparison() {
    let f = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        fs::write(f.path().join(name), b"same").unwrap();
    }
    let scan = scan_inventory_with_dates(
        f.path(),
        &Default::default(),
        &Default::default(),
        Default::default(),
        |_| {},
    )
    .unwrap();
    let snapshot = AnalysisSnapshot::from_inventory(&scan.inventory, &scan.dates).unwrap();
    let selected = search(&snapshot, &Default::default(), 50_000, &Default::default()).unwrap();
    for (budget, complete, groups) in [(15, false, 0), (16, true, 1)] {
        let report = find(
            &snapshot,
            &selected,
            DuplicateOptions {
                max_bytes_read: budget,
                ..Default::default()
            },
            &Default::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.complete, complete);
        assert_eq!(report.groups.len(), groups);
        assert!(report.bytes_read <= budget);
        assert_eq!(report.budget_limited, !complete);
    }
}

#[test]
fn typed_io_diagnostics_preserve_the_analysis_error_wire_fields() {
    let issue = DuplicateIssue {
        path: NativePath::from(std::path::Path::new("missing.txt")),
        reason: "not found".into(),
        error_kind: std::io::ErrorKind::NotFound,
    };
    let value = serde_json::to_value(&issue).unwrap();
    assert_eq!(
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["path", "reason"]
    );
    assert_eq!(value["reason"], "not found");
}
