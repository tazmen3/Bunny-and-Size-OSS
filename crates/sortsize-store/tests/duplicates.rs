use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    analysis::SearchQuery,
    duplicates::DuplicateOptions,
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{Store, queries::duplicates::DuplicateFileProfile},
    scheduler::{Session, State},
};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
fn acquire(root: &Path, db: &Path) -> Session {
    let c = AcquisitionContext {
        inventory: InventoryId(92),
        source: SourceId(92),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = Session::new(
        Store::create(db, c.inventory, c.source, Durability::Reconstructible).unwrap(),
        c,
        Guard::open(root, Policy::default()).unwrap(),
    )
    .unwrap();
    let start = Instant::now();
    while s.tick().unwrap().state != State::Idle {
        assert!(start.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(1));
    }
    while s.finalize_step().unwrap().is_none() {}
    s
}
#[test]
fn disk_groups_verify_content_aliases_budget_mutation_and_offline_preparation() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    fs::create_dir(&root).unwrap();
    for (name, data) in [
        ("a", b"hello".as_slice()),
        ("b", b"hello"),
        ("different", b"world"),
        ("empty1", b""),
        ("empty2", b""),
    ] {
        fs::write(root.join(name), data).unwrap();
    }
    fs::hard_link(root.join("a"), root.join("alias")).unwrap();
    let s = acquire(&root, &t.path().join("db"));
    let cancel = CancellationToken::default();
    let mut q = s
        .store()
        .start_query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            32 * 1024 * 1024,
        )
        .unwrap();
    while !q.advance(&cancel).unwrap() {}
    let offline = t.path().join("offline");
    fs::rename(&root, &offline).unwrap();
    let mut job = q
        .prepare_duplicates(t.path(), 32 * 1024 * 1024, &cancel)
        .unwrap();
    assert_eq!(job.candidate_count().unwrap(), 6);
    assert_eq!(job.summary().files_hashed, 0);
    assert!(!job.summary().verified);
    fs::rename(&offline, &root).unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    job.verify(&guard, DuplicateOptions::default(), &cancel)
        .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    assert_eq!(job.summary().groups, 2);
    assert_eq!(job.summary().aliases, 1);
    assert_eq!(job.summary().files_hashed, 5);
    assert_eq!(job.summary().potentially_reclaimable_bytes, None);
    let mut paths = 0;
    for g in job.groups(None).unwrap() {
        let members = job.members(g.id, None).unwrap();
        assert_eq!(members.len() as u64, g.paths);
        paths += members.len();
    }
    assert_eq!(paths, 5);
    drop(job);
    let mut job = q
        .prepare_duplicates(t.path(), 32 * 1024 * 1024, &cancel)
        .unwrap();
    job.verify(
        &guard,
        DuplicateOptions {
            max_bytes_read: Some(3),
            ..Default::default()
        },
        &cancel,
    )
    .unwrap();
    assert!(job.summary().budget_limited);
    assert!(!job.summary().complete);
    assert!(job.summary().bytes_read <= 3);
    drop(job);
    let cancelled = CancellationToken::default();
    cancelled.cancel();
    let mut job = q
        .prepare_duplicates(t.path(), 32 * 1024 * 1024, &cancelled)
        .unwrap();
    job.verify(&guard, Default::default(), &cancelled).unwrap();
    assert!(job.summary().cancelled);
    assert_eq!(job.summary().bytes_read, 0);
    drop(job);
    fs::write(root.join("a"), b"changed-size").unwrap();
    let mut job = q
        .prepare_duplicates(t.path(), 32 * 1024 * 1024, &cancel)
        .unwrap();
    job.verify(&guard, Default::default(), &cancel).unwrap();
    assert!(!job.summary().complete);
    assert!(job.summary().errors >= 1);
    assert_eq!(job.summary().groups, 1);
}
#[test]
fn large_native_empty_bucket_pages_every_member_without_a_ram_group() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    fs::create_dir(&root).unwrap();
    for i in 0..2050 {
        fs::write(root.join(format!("f{i:05}")), b"").unwrap();
    }
    let s = acquire(&root, &t.path().join("db"));
    let cancel = CancellationToken::default();
    let mut q = s
        .store()
        .start_query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            32 * 1024 * 1024,
        )
        .unwrap();
    while !q.advance(&cancel).unwrap() {}
    let mut job = q
        .prepare_duplicates(t.path(), 32 * 1024 * 1024, &cancel)
        .unwrap();
    job.verify(
        &Guard::open(&root, Policy::default()).unwrap(),
        Default::default(),
        &cancel,
    )
    .unwrap();
    assert!(job.summary().complete);
    assert_eq!(job.summary().bytes_read, 0);
    let g = job.groups(None).unwrap();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].independent_members, 2050);
    let mut after = None;
    let mut n = 0;
    loop {
        let rows = job.members(g[0].id, after).unwrap();
        assert!(rows.len() <= 200);
        if rows.is_empty() {
            break;
        }
        for r in rows {
            assert!(r.entry > after.unwrap_or(0));
            after = Some(r.entry);
            n += 1;
        }
    }
    assert_eq!(n, 2050);
}

#[test]
fn inventory_duplicate_profiles_apply_binary_boundaries_without_changing_the_query() {
    use DuplicateFileProfile::{Exhaustive, General, LargeFiles, Optimized};
    const DISK: u64 = 32 * 1024 * 1024;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    fs::create_dir(&root).unwrap();
    for size in [
        0, 1, 262_143, 262_144, 1_048_575, 1_048_576, 10_485_759, 10_485_760, 10_485_761,
    ] {
        fs::File::create(root.join(size.to_string()))
            .unwrap()
            .set_len(size)
            .unwrap();
    }
    let s = acquire(&root, &t.path().join("db"));
    let cancel = CancellationToken::default();
    let mut query = s
        .store()
        .start_query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            DISK,
        )
        .unwrap();
    while !query.advance(&cancel).unwrap() {}
    assert_eq!(DuplicateFileProfile::default(), LargeFiles);
    for (profile, selected, below, threshold) in [
        (LargeFiles, 2, 6, 10_485_760),
        (Optimized, 4, 4, 1_048_576),
        (General, 6, 2, 262_144),
        (Exhaustive, 8, 0, 1),
    ] {
        assert_eq!(profile.minimum_bytes(), threshold);
        let job = query
            .prepare_duplicates_with_profile(t.path(), DISK, profile, &cancel)
            .unwrap();
        assert_eq!(job.candidate_count().unwrap(), selected);
        let summary = job.summary();
        assert_eq!(summary.selected_files, selected);
        assert_eq!(summary.examined_files, 9);
        assert_eq!(summary.excluded_by_size, below);
        assert_eq!(summary.excluded_empty, 1);
        assert_eq!(summary.files_hashed, 0);
        assert_eq!(summary.bytes_read, 0);
    }
    let former = query.prepare_duplicates(t.path(), DISK, &cancel).unwrap();
    assert_eq!(former.candidate_count().unwrap(), 9);
    let record = serde_json::to_value(former.summary()).unwrap();
    for key in ["examined_files", "excluded_by_size", "excluded_empty"] {
        assert!(record.get(key).is_none());
    }
}

#[test]
fn profiled_groups_rank_independent_logical_gain_and_page_both_ways() {
    const DISK: u64 = 32 * 1024 * 1024;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    fs::create_dir(&root).unwrap();
    for i in 0..205 {
        fs::write(root.join(format!("many-{i:03}")), b"x").unwrap();
    }
    for i in 0..4 {
        fs::write(root.join(format!("medium-{i}")), b"xx").unwrap();
    }
    for i in 0..2 {
        fs::write(root.join(format!("large-{i}")), b"xxxxx").unwrap();
    }
    fs::hard_link(root.join("large-0"), root.join("large-link")).unwrap();
    fs::write(root.join("same-size-different-content"), b"other").unwrap();
    fs::write(root.join("zero-a"), b"").unwrap();
    fs::write(root.join("zero-b"), b"").unwrap();
    let s = acquire(&root, &t.path().join("db"));
    let cancel = CancellationToken::default();
    let mut query = s
        .store()
        .start_query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            DISK,
        )
        .unwrap();
    while !query.advance(&cancel).unwrap() {}
    let mut job = query
        .prepare_duplicates_with_profile(t.path(), DISK, DuplicateFileProfile::Exhaustive, &cancel)
        .unwrap();
    assert_eq!(job.candidate_count().unwrap(), 213);
    job.verify(
        &Guard::open(&root, Policy::default()).unwrap(),
        Default::default(),
        &cancel,
    )
    .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    assert_eq!(job.summary().files_hashed, 212);
    assert_eq!(job.summary().aliases, 1);
    let groups = job.groups(None).unwrap();
    let by_gain = groups
        .iter()
        .map(|group| (group.logical_bytes, group.independent_members, group.paths))
        .collect::<Vec<_>>();
    assert_eq!(by_gain, [(1, 205, 205), (2, 4, 4), (5, 2, 3)]);
    assert_eq!(job.groups(Some(groups[0].id)).unwrap()[0].id, groups[1].id);
    let first = job.page(None, false).unwrap();
    assert!(first.more);
    assert_eq!(first.rows.len(), 200);
    assert!(first.labels.iter().all(|label| label.0 == groups[0].id));
    let second = job.page(first.last.as_ref(), false).unwrap();
    assert_eq!(second.rows.len(), 12);
    assert!(!second.more);
    assert_eq!(second.labels[5].0, groups[1].id);
    assert_eq!(second.labels[9].0, groups[2].id);
    let back = job.page(second.first.as_ref(), true).unwrap();
    assert_eq!(back.labels, first.labels);
    assert!(
        back.rows
            .iter()
            .map(|row| &row.path)
            .eq(first.rows.iter().map(|row| &row.path))
    );
    let back_from_last = job.page(second.last.as_ref(), true).unwrap();
    assert!(back_from_last.more);
    assert!(
        back_from_last
            .labels
            .iter()
            .eq(first.labels[11..].iter().chain(&second.labels[..11]))
    );
}
