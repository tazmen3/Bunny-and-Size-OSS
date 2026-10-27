use sortsize_core::{analysis::*, duplicates::*, *};
use std::{fs, path::Path};
fn snapshot(root: &Path) -> AnalysisSnapshot {
    let r = scan_inventory_with_dates(
        root,
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        |_| {},
    )
    .unwrap();
    AnalysisSnapshot::from_inventory(&r.inventory, &r.dates).unwrap()
}
fn run(s: &AnalysisSnapshot, options: DuplicateOptions, c: &CancellationToken) -> DuplicateReport {
    let r = search(
        s,
        &SearchQuery::default(),
        50_000,
        &CancellationToken::default(),
    )
    .unwrap();
    find(s, &r, options, c, |_| {}).unwrap()
}
#[test]
fn exact_groups_byte_comparison_hardlinks_empty_and_read_only() {
    let f = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("a", b"hello".as_slice()),
        ("b", b"hello"),
        ("different", b"world"),
        ("empty1", b""),
        ("empty2", b""),
    ] {
        fs::write(f.path().join(name), bytes).unwrap();
    }
    fs::hard_link(f.path().join("a"), f.path().join("alias")).unwrap();
    let s = snapshot(f.path());
    let r = run(&s, Default::default(), &CancellationToken::default());
    assert!(r.complete, "{r:?}");
    assert_eq!(r.groups.len(), 2);
    let group = r
        .groups
        .iter()
        .find(|g| g.logical_bytes_per_file == 5)
        .unwrap();
    assert_eq!(
        group.sha256,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!(group.members.len(), 2);
    assert_eq!(
        group.members.iter().map(|m| m.paths.len()).sum::<usize>(),
        3
    );
    assert_eq!(r.hardlink_aliases, 1);
    assert!(r.potentially_reclaimable_bytes.is_none());
    assert_eq!(fs::read(f.path().join("a")).unwrap(), b"hello");
    assert_eq!(fs::read(f.path().join("different")).unwrap(), b"world");
}
#[test]
fn changed_files_budgets_and_cancellation_never_claim_complete() {
    let f = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        fs::write(f.path().join(name), [1; 262_144]).unwrap();
    }
    let s = snapshot(f.path());
    let limited = run(
        &s,
        DuplicateOptions {
            max_bytes_read: 1000,
            ..Default::default()
        },
        &CancellationToken::default(),
    );
    assert!(limited.budget_limited);
    assert!(limited.bytes_read <= 1000);
    assert!(!limited.complete);
    assert!(limited.groups.is_empty());
    let c = CancellationToken::default();
    c.cancel();
    assert!(run(&s, Default::default(), &c).cancelled);
    let sel = search(
        &s,
        &SearchQuery::default(),
        50_000,
        &CancellationToken::default(),
    )
    .unwrap();
    let c = CancellationToken::default();
    let mut invoked = false;
    let r = find(&s, &sel, Default::default(), &c, |_| {
        if !invoked {
            invoked = true;
            c.cancel();
        }
    })
    .unwrap();
    assert!(r.cancelled);
    assert!(r.groups.is_empty());
    fs::write(f.path().join("a"), b"changed").unwrap();
    let r = run(&s, Default::default(), &CancellationToken::default());
    assert!(!r.complete);
    assert!(r.errors_count > 0);
    assert!(r.groups.is_empty());
}
#[cfg(unix)]
#[test]
fn rejects_replaced_symlinks_and_mutation_during_hash() {
    let f = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        fs::write(f.path().join(name), [1; 262_144]).unwrap();
    }
    let s = snapshot(f.path());
    let sel = search(
        &s,
        &SearchQuery::default(),
        50_000,
        &CancellationToken::default(),
    )
    .unwrap();
    let mut changed = false;
    let r = find(
        &s,
        &sel,
        Default::default(),
        &CancellationToken::default(),
        |_| {
            if !changed {
                changed = true;
                fs::write(f.path().join("a"), [2; 262_144]).unwrap();
            }
        },
    )
    .unwrap();
    assert!(r.errors_count > 0);
    assert!(r.groups.is_empty());
    let s = snapshot(f.path());
    fs::remove_file(f.path().join("b")).unwrap();
    std::os::unix::fs::symlink(f.path().join("a"), f.path().join("b")).unwrap();
    let r = run(&s, Default::default(), &CancellationToken::default());
    assert!(r.errors_count > 0);
    assert!(r.groups.is_empty());
}
#[test]
fn aliases_alone_are_not_duplicate_copies() {
    let f = tempfile::tempdir().unwrap();
    fs::write(f.path().join("original"), b"same").unwrap();
    fs::hard_link(f.path().join("original"), f.path().join("alias")).unwrap();
    let r = run(
        &snapshot(f.path()),
        Default::default(),
        &CancellationToken::default(),
    );
    assert!(r.groups.is_empty());
    assert_eq!(r.hardlink_aliases, 1);
    assert!(r.complete);
}

#[test]
fn matching_sha_requires_the_remaining_binary_comparison_budget() {
    let f = tempfile::tempdir().unwrap();
    let bytes = vec![42; 65_537]; // Cross the reader's 64 KiB boundary.
    for name in ["a", "b"] {
        fs::write(f.path().join(name), &bytes).unwrap();
    }
    let s = snapshot(f.path());
    let length = bytes.len() as u64;
    for budget in [2 * length, 4 * length - 1, 4 * length] {
        let r = run(
            &s,
            DuplicateOptions {
                max_bytes_read: budget,
                ..Default::default()
            },
            &CancellationToken::default(),
        );
        assert_eq!(r.files_hashed, 2, "both SHA-256 reads must finish");
        assert!(r.bytes_read <= budget);
        if budget < 4 * length {
            assert!(r.groups.is_empty(), "a matching hash alone is insufficient");
            assert!(r.budget_limited);
            assert!(!r.complete);
        } else {
            assert_eq!(r.groups.len(), 1);
            assert_eq!(r.bytes_read, 4 * length);
            assert!(r.complete);
            assert!(!r.budget_limited);
        }
    }
    assert_eq!(fs::read(f.path().join("a")).unwrap(), bytes);
    assert_eq!(fs::read(f.path().join("b")).unwrap(), bytes);
}
#[test]
fn cancellation_before_empty_file_verification_does_not_publish_a_group() {
    let f = tempfile::tempdir().unwrap();
    for name in ["empty-a", "empty-b"] {
        fs::write(f.path().join(name), b"").unwrap();
    }
    let s = snapshot(f.path());
    let c = CancellationToken::default();
    let selection = search(&s, &SearchQuery::default(), 50_000, &c).unwrap();
    let report = find(&s, &selection, Default::default(), &c, |p| {
        if p.files_hashed == 2 {
            c.cancel();
        }
    })
    .unwrap();
    assert!(report.cancelled);
    assert!(report.groups.is_empty());
    assert_eq!(report.bytes_read, 0);
}
