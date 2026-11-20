use super::*;
use crate::scheduler::{Session, State};
use sortsize_core::acquisition::Policy;
use std::{collections::HashSet, fs};

static TEST: Mutex<()> = Mutex::new(());
const DISK: u64 = 32 * 1024 * 1024;

fn acquire(root: &Path, scratch: &Path, id: u128) -> Session {
    acquire_with_source(root, scratch, id, id)
}

fn acquire_with_source(root: &Path, scratch: &Path, id: u128, source: u128) -> Session {
    let context = AcquisitionContext {
        inventory: InventoryId(id),
        source: SourceId(source),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut session = Session::new(
        Store::create(
            &scratch.join(format!("db-{id}")),
            context.inventory,
            context.source,
            crate::Durability::Reconstructible,
        )
        .unwrap(),
        context,
        Guard::open(root, Policy::default()).unwrap(),
    )
    .unwrap();
    let started = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    session
}

fn query<'a>(session: &'a Session, root: &Path, scratch: &Path) -> Query<'a> {
    filtered_query(session, root, scratch, &SearchQuery::default())
}

fn filtered_query<'a>(
    session: &'a Session,
    root: &Path,
    scratch: &Path,
    filter: &SearchQuery,
) -> Query<'a> {
    let mut query = session
        .store()
        .start_query(GenerationId(1), root, filter, scratch, DISK)
        .unwrap();
    while !query.advance(&CancellationToken::default()).unwrap() {}
    query
}

fn ab<'q, 's>(a: &'q Query<'s>, b: &'q Query<'s>, scratch: &Path) -> DuplicateJob<'q, 's> {
    prepare_scopes(
        vec![
            Scope {
                query: a,
                roots: vec![a.root.clone()],
                side: 0,
            },
            Scope {
                query: b,
                roots: vec![b.root.clone()],
                side: 1,
            },
        ],
        Mode::BetweenSources,
        scratch,
        DISK,
        &CancellationToken::default(),
    )
    .unwrap()
}

fn guards(job: &DuplicateJob<'_, '_>) -> Vec<Guard> {
    job.guard_roots()
        .iter()
        .map(|root| Guard::open(root, Policy::default()).unwrap())
        .collect()
}

fn roots(scratch: &Path) -> (PathBuf, PathBuf) {
    let a = scratch.join("source-a");
    let b = scratch.join("source-b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    (a, b)
}

#[test]
fn same_volume_shared_parent_inventory_preserves_ab_and_abc_sides_and_reference() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let parent = t.path().join("corpus");
    let roots = [parent.join("a"), parent.join("b"), parent.join("c")];
    for root in &roots {
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("same"), b"selected exact content").unwrap();
    }
    fs::write(parent.join("outside"), b"selected exact content").unwrap();
    let session = acquire(&parent, t.path(), 3001);
    let q = query(&session, &parent, t.path());
    for count in [2, 3] {
        let mut job = prepare_scopes(
            roots[..count]
                .iter()
                .enumerate()
                .map(|(side, root)| Scope {
                    query: &q,
                    roots: vec![root.clone()],
                    side: side as u8,
                })
                .collect(),
            if count == 2 {
                Mode::BetweenSources
            } else {
                Mode::BetweenThreeSources
            },
            t.path(),
            DISK,
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(job.summary().selected_files, count as u64);
        assert_eq!(job.summary().bytes_read, 0);
        assert_eq!(job.guard_roots(), &roots[..count]);
        job.verify_scoped(
            &guards(&job),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(job.summary().complete, "{:?}", job.summary());
        assert!(job.summary().source_independence_verified);
        assert_eq!(job.summary().groups, 1);
        assert_eq!(job.summary().folder_groups, 1);
        assert_eq!(job.summary().folder_files, 1);
        assert_eq!(job.summary().folder_logical_bytes, 22);
        let page = job.page(None, false).unwrap();
        assert_eq!(page.sides, (0..count as u8).collect::<Vec<_>>());
        assert!(page.sources.iter().all(|source| *source == SourceId(3001)));
        for (row, side) in page.rows.iter().zip(&page.sides) {
            assert_eq!(row.key.inventory, InventoryId(3001));
            assert_eq!(row.key.generation, GenerationId(1));
            assert!(row.path.starts_with(&roots[usize::from(*side)]));
        }
        let members = job.members(job.groups(None).unwrap()[0].id, None).unwrap();
        assert_eq!(
            members.iter().map(|m| m.side).collect::<Vec<_>>(),
            page.sides
        );
        for member in &members {
            let record: serde_json::Value =
                serde_json::from_slice(&member.json_record(1).unwrap()).unwrap();
            assert_eq!(record["member"]["side"], member.side);
        }
        job.set_protected_reference(Some(0)).unwrap();
        for (row, side) in page.rows.iter().zip(&page.sides) {
            let target = job.review_row_target(row).unwrap().unwrap();
            let state = job.review_state(target).unwrap();
            assert_eq!(state.eligible, *side != 0);
            if *side == 0 {
                assert_eq!(state.reason, ReviewReason::ProtectedReference);
            }
        }
        job.select_all_review_copies().unwrap();
        let review = job.review_summary().unwrap();
        assert_eq!(review.files, (count - 1) as u64);
        assert_eq!(review.logical_bytes, 22 * (count - 1) as u64);
        job.set_protected_reference(Some(1)).unwrap();
        assert_eq!(job.review_summary().unwrap().files, 0);
    }
}

#[test]
fn same_source_separate_folder_inventories_allow_duplicates_and_complete_zero_groups() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    for duplicate in [true, false] {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = roots(t.path());
        fs::write(a.join("left"), b"left").unwrap();
        fs::write(b.join("right"), if duplicate { b"left" } else { b"none" }).unwrap();
        let sa = acquire_with_source(&a, t.path(), 3011, 3000);
        let sb = acquire_with_source(&b, t.path(), 3012, 3000);
        let qa = query(&sa, &a, t.path());
        let qb = query(&sb, &b, t.path());
        let mut job = ab(&qa, &qb, t.path());
        job.verify_scoped(
            &guards(&job),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(job.summary().complete, "{:?}", job.summary());
        assert_eq!(job.summary().groups, u64::from(duplicate));
        if duplicate {
            let page = job.page(None, false).unwrap();
            assert_eq!(page.sources, [SourceId(3000); 2]);
            assert_eq!(page.sides, [0, 1]);
            assert_ne!(page.rows[0].key.inventory, page.rows[1].key.inventory);
            job.set_protected_reference(Some(0)).unwrap();
            job.select_all_review_copies().unwrap();
            assert_eq!(job.review_summary().unwrap().files, 1);
        } else {
            assert!(job.page(None, false).unwrap().rows.is_empty());
            assert!(job.groups(None).unwrap().is_empty());
        }
    }
}

#[test]
fn shared_parent_inventory_rejects_identical_and_nested_folder_scopes() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let parent = t.path().join("corpus");
    let nested = parent.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("same"), b"same").unwrap();
    let s = acquire(&parent, t.path(), 3021);
    let q = query(&s, &parent, t.path());
    for other in [&parent, &nested] {
        let result = prepare_scopes(
            vec![
                Scope {
                    query: &q,
                    roots: vec![parent.clone()],
                    side: 0,
                },
                Scope {
                    query: &q,
                    roots: vec![other.clone()],
                    side: 1,
                },
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default(),
        );
        assert!(matches!(
            result,
            Err(Error::Contract(
                "chosen comparison folders are identical or nested"
            ))
        ));
    }
}

#[test]
fn same_source_shared_inventory_hardlink_is_never_an_independent_copy() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let parent = t.path().join("corpus");
    let a = parent.join("a");
    let b = parent.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("same"), b"same hardlinked content").unwrap();
    fs::hard_link(a.join("same"), b.join("same")).unwrap();
    let session = acquire(&parent, t.path(), 3031);
    let q = query(&session, &parent, t.path());
    let mut job = prepare_scopes(
        vec![
            Scope {
                query: &q,
                roots: vec![a],
                side: 0,
            },
            Scope {
                query: &q,
                roots: vec![b],
                side: 1,
            },
        ],
        Mode::BetweenSources,
        t.path(),
        DISK,
        &CancellationToken::default(),
    )
    .unwrap();
    job.verify_scoped(
        &guards(&job),
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().folder_groups, 0);
    assert_eq!(job.summary().independent_members, 0);
    assert!(job.summary().aliases + job.summary().ambiguous_aliases > 0);
    job.set_protected_reference(Some(0)).unwrap();
    assert_eq!(job.review_preview().unwrap().eligible_files, 0);
    // Platforms may reject the global review as incomplete or accept an empty
    // selection; neither outcome can turn the alias into an eligible copy.
    let _ = job.select_all_review_copies();
    assert_eq!(job.review_summary().unwrap().files, 0);
}

#[test]
fn ab_native_renamed_three_locations_excludes_a_only_and_unselected_c() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let c = t.path().join("source-c");
    fs::create_dir(&c).unwrap();
    for (name, bytes) in [
        ("aa-original", b"common".as_slice()),
        ("a-only-1", b"a-only"),
        ("a-only-2", b"a-only"),
        ("different", b"left!"),
    ] {
        fs::write(a.join(name), bytes).unwrap();
    }
    fs::write(b.join("aa-renamed"), b"common").unwrap();
    fs::write(b.join("second-copy"), b"common").unwrap();
    fs::write(b.join("different"), b"right").unwrap();
    fs::write(c.join("unselected-copy"), b"common").unwrap();
    fs::write(c.join("unselected-a-only"), b"a-only").unwrap();
    let sa = acquire(&a, t.path(), 401);
    let sb = acquire(&b, t.path(), 402);
    let sc = acquire(&c, t.path(), 403);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let _qc = query(&sc, &c, t.path());
    let mut job = ab(&qa, &qb, t.path());
    assert_eq!(job.candidate_count().unwrap(), 7);
    assert_eq!(job.summary().bytes_read, 0);
    assert!(!job.summary().verified);
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    assert!(job.summary().source_independence_verified);
    assert_eq!(job.summary().unverified_sources, 0);
    let groups = job.groups(None).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].paths, 3);
    let members = job.members(groups[0].id, None).unwrap();
    assert_eq!(members.len(), 3);
    assert_eq!(
        members.iter().filter(|m| m.source == SourceId(401)).count(),
        1
    );
    assert_eq!(
        members.iter().filter(|m| m.source == SourceId(402)).count(),
        2
    );
    assert!(members.iter().all(|m| m.source != SourceId(403)));
    assert!(members.iter().all(|m| m.key.generation == GenerationId(1)));
    assert_eq!(job.summary().potentially_reclaimable_bytes, None);
}

#[test]
fn ab_entry_id_collisions_and_pagination_keep_all_composite_keys() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    // Empty files exercise a large equal-size/hash bucket without read traffic.
    for root in [&a, &b] {
        for n in 0..110 {
            fs::write(root.join(format!("file-{n:03}")), b"").unwrap();
        }
    }
    let sa = acquire(&a, t.path(), 411);
    let sb = acquire(&b, t.path(), 412);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.summary().independent_members, 220);
    let first = job.page(None, false).unwrap();
    assert_eq!(first.rows.len(), 200);
    assert!(first.more);
    let second = job.page(first.last.as_ref(), false).unwrap();
    assert_eq!(second.rows.len(), 20);
    assert!(!second.more);
    let mut keys = HashSet::new();
    let mut ids_a = HashSet::new();
    let mut ids_b = HashSet::new();
    for row in first.rows.iter().chain(&second.rows) {
        assert!(keys.insert(row.key));
        if row.key.inventory == InventoryId(411) {
            ids_a.insert(row.key.entry);
        } else {
            assert_eq!(row.key.inventory, InventoryId(412));
            ids_b.insert(row.key.entry);
        }
    }
    assert_eq!(keys.len(), 220);
    assert_eq!(ids_a.len(), 110);
    assert_eq!(ids_b.len(), 110);
    assert!(ids_a.intersection(&ids_b).next().is_some());
    let previous = job.page(second.first.as_ref(), true).unwrap();
    assert_eq!(previous.rows.len(), 200);
    assert!(
        previous
            .rows
            .iter()
            .zip(&first.rows)
            .all(|(a, b)| a.key == b.key)
    );
    let mut after = None;
    let mut member_keys = HashSet::new();
    loop {
        let members = job.members(job.groups(None).unwrap()[0].id, after).unwrap();
        if members.is_empty() {
            break;
        }
        for member in members {
            assert!(member.cursor > after.unwrap_or(0));
            after = Some(member.cursor);
            assert!(member_keys.insert(member.key));
            let record: serde_json::Value =
                serde_json::from_slice(&member.json_record(1).unwrap()).unwrap();
            assert_eq!(record["duplicates_schema"], 2);
            assert_eq!(record["member"]["cursor"], member.cursor.to_string());
            assert_eq!(record["member"]["source"], member.source.0.to_string());
            assert_eq!(
                record["member"]["inventory"],
                member.key.inventory.0.to_string()
            );
            assert_eq!(record["member"]["generation"], "1");
            assert_eq!(record["member"]["entry"], member.key.entry.0.to_string());
        }
    }
    assert_eq!(member_keys, keys);
    assert_eq!(job.summary().bytes_read, 0);
}

#[test]
fn selection_normalizes_roots_and_deduplicates_actual_matches_across_filters() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("copy-a"), b"same").unwrap();
    fs::write(nested.join("copy-b"), b"same").unwrap();
    let s = acquire(&root, t.path(), 421);
    let filter = SearchQuery {
        name: Some("copy-a".into()),
        ..Default::default()
    };
    let selected = filtered_query(&s, &root, t.path(), &filter);
    let all = query(&s, &root, t.path());
    let mut job = prepare_scopes(
        vec![
            Scope {
                query: &selected,
                roots: vec![nested.clone(), root.clone(), nested.clone()],
                side: 0,
            },
            Scope {
                query: &all,
                roots: vec![nested.clone()],
                side: 0,
            },
        ],
        Mode::WithinSelection,
        t.path(),
        DISK,
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(job.candidate_count().unwrap(), 2);
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    // A narrower guard proves the selected files, while continuity with the
    // inventory's broader source root remains explicitly unqualified.
    assert!(!job.summary().complete, "{:?}", job.summary());
    assert_eq!(job.summary().errors, 0);
    assert_eq!(job.summary().unverified_sources, 1);
    assert!(!job.summary().source_independence_verified);
    assert!(job.warnings().iter().any(|(scope, _)| *scope == 1));
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.summary().independent_members, 2);
    let mut normalized = prepare_scopes(
        vec![Scope {
            query: &all,
            roots: vec![nested.clone(), root.clone(), nested],
            side: 0,
        }],
        Mode::WithinSelection,
        t.path(),
        DISK,
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(normalized.guard_roots(), std::slice::from_ref(&root));
    normalized
        .verify_scoped(
            &guards(&normalized),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(normalized.summary().complete, "{:?}", normalized.summary());
    assert_eq!(normalized.summary().groups, 1);
    // The CLI's existing single-query JSONL contract remains schema 1.
    let mut single = all
        .prepare_duplicates(t.path(), DISK, &CancellationToken::default())
        .unwrap();
    single
        .verify(
            &Guard::open(&root, Policy::default()).unwrap(),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    let group = single.groups(None).unwrap()[0].id;
    for member in single.members(group, None).unwrap() {
        let record: serde_json::Value =
            serde_json::from_slice(&member.json_record(group).unwrap()).unwrap();
        assert_eq!(record["duplicates_schema"], 1);
        assert!(record["member"].get("source").is_none());
        assert!(record["member"].get("inventory").is_none());
        assert!(record["member"].get("generation").is_none());
        assert!(record["member"].get("cursor").is_none());
    }
    assert!(
        prepare_scopes(
            vec![
                Scope {
                    query: &all,
                    roots: vec![root.clone()],
                    side: 0
                },
                Scope {
                    query: &all,
                    roots: vec![root],
                    side: 1
                }
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default()
        )
        .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn ab_internal_duplicates_with_only_an_alias_on_b_are_not_intersource_results() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"same").unwrap();
    fs::write(a.join("independent"), b"same").unwrap();
    fs::hard_link(a.join("original"), b.join("alias")).unwrap();
    let sa = acquire(&a, t.path(), 431);
    let sb = acquire(&b, t.path(), 432);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    // B exposes only an alias of A. Both independent copies live inside A,
    // so a verified extra path on B must not turn internal duplicates into an
    // intersource result. Native identity/representative evidence is retained.
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().independent_members, 0);
    assert_eq!(job.summary().aliases, 1);
    assert!(job.groups(None).unwrap().is_empty());
    let (independent, alias): (i64,i64) = job.db.query_row("SELECT count(DISTINCT representative),sum(side=1 AND representative!=id) FROM candidates WHERE proof IS NOT NULL",[],|r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((independent, alias), (2, 1));
    let only_original = filtered_query(
        &sa,
        &a,
        t.path(),
        &SearchQuery {
            name: Some("original".into()),
            ..Default::default()
        },
    );
    let mut only_aliases = ab(&only_original, &qb, t.path());
    only_aliases
        .verify_scoped(
            &guards(&only_aliases),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(only_aliases.summary().complete);
    assert_eq!(only_aliases.summary().groups, 0);
}

#[test]
fn ab_cancel_budget_mutation_and_offline_states_preserve_canonical_inventory() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"same").unwrap();
    fs::write(b.join("copy"), b"same").unwrap();
    let sa = acquire(&a, t.path(), 441);
    let sb = acquire(&b, t.path(), 442);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let before_a = fs::read(sa.store().directory.join("inventory.db")).unwrap();
    let before_b = fs::read(sb.store().directory.join("inventory.db")).unwrap();
    let mut budget = ab(&qa, &qb, t.path());
    budget
        .verify_scoped(
            &guards(&budget),
            DuplicateOptions {
                max_bytes_read: Some(15),
                ..Default::default()
            },
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(budget.summary().budget_limited);
    assert!(!budget.summary().complete);
    assert_eq!(budget.summary().groups, 0);
    assert_eq!(budget.summary().bytes_read, 15);
    let mut cancelled = ab(&qa, &qb, t.path());
    let cancel = CancellationToken::default();
    cancel.cancel();
    cancelled
        .verify_scoped(&guards(&cancelled), Default::default(), &cancel)
        .unwrap();
    assert!(cancelled.summary().cancelled);
    assert!(cancelled.summary().verified);
    assert!(!cancelled.summary().complete);
    assert_eq!(cancelled.summary().bytes_read, 0);
    // A new job has its own state and can finish after the old cancellation.
    let mut retry = ab(&qa, &qb, t.path());
    retry
        .verify_scoped(
            &guards(&retry),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(retry.summary().complete);
    assert_eq!(retry.summary().groups, 1);
    let mut offline = ab(&qa, &qb, t.path());
    let scoped_guards = guards(&offline);
    let moved = t.path().join("source-b-offline");
    fs::rename(&b, &moved).unwrap();
    assert_eq!(offline.summary().bytes_read, 0);
    offline
        .verify_scoped(
            &scoped_guards,
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(!offline.summary().complete);
    assert!(offline.summary().errors > 0);
    assert_eq!(offline.summary().groups, 0);
    let errors = offline.scope_errors().unwrap();
    assert_eq!(errors[0], 0);
    assert!(errors[1] > 0);
    fs::rename(moved, &b).unwrap();
    fs::write(b.join("copy"), b"changed-size").unwrap();
    let mut mutation = ab(&qa, &qb, t.path());
    mutation
        .verify_scoped(
            &guards(&mutation),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(!mutation.summary().complete);
    assert!(mutation.summary().errors > 0);
    assert_eq!(mutation.summary().groups, 0);
    assert_eq!(
        fs::read(sa.store().directory.join("inventory.db")).unwrap(),
        before_a
    );
    assert_eq!(
        fs::read(sb.store().directory.join("inventory.db")).unwrap(),
        before_b
    );
}

#[test]
fn ab_injected_read_refusal_is_partial_and_attributed_to_its_source() {
    // Fault injection is explicit: this checks the error contract independently
    // of the test account's native ACL privileges.
    struct RefuseB<'a, 'b> {
        native: NativeVerification<'a, 'b>,
    }
    impl Verification for RefuseB<'_, '_> {
        fn observe(
            &self,
            guard: usize,
            path: &Path,
            length: u64,
            modified: Option<SystemTime>,
        ) -> io::Result<Proof> {
            if guard == 1 {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected B refusal",
                ))
            } else {
                self.native.observe(guard, path, length, modified)
            }
        }
        fn hash(
            &mut self,
            guard: usize,
            path: &Path,
            proof: &Proof,
            progress: &mut dyn FnMut(DuplicateProgress),
        ) -> io::Result<[u8; 32]> {
            self.native.hash(guard, path, proof, progress)
        }
        fn validate(&self, guard: usize, path: &Path, proof: &Proof) -> io::Result<()> {
            self.native.validate(guard, path, proof)
        }
        fn equal(
            &mut self,
            a: (usize, &Path, &Proof),
            b: (usize, &Path, &Proof),
            expected_hash: &[u8; 32],
            progress: &mut dyn FnMut(DuplicateProgress),
        ) -> io::Result<bool> {
            self.native.equal(a, b, expected_hash, progress)
        }
        fn progress(&self) -> sortsize_core::duplicates::DuplicateProgress {
            self.native.progress()
        }
    }
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"same").unwrap();
    fs::write(b.join("copy"), b"same").unwrap();
    let sa = acquire(&a, t.path(), 451);
    let sb = acquire(&b, t.path(), 452);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let cancel = CancellationToken::default();
    let scoped_guards = guards(&job);
    let mut verifier = RefuseB {
        native: NativeVerification {
            verifier: Verifier::new(Default::default(), &cancel),
            guards: &scoped_guards.iter().map(Some).collect::<Vec<_>>(),
        },
    };
    job.verify_with(&mut verifier, &cancel).unwrap();
    assert!(job.summary().verified);
    assert!(!job.summary().complete);
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().errors, 1);
    assert_eq!(job.scope_errors().unwrap(), vec![0, 1]);
    assert_eq!(job.summary().files_hashed, 1);
    assert_eq!(job.summary().bytes_read, 4);
}

#[test]
fn ab_simulated_native_identity_collision_cannot_claim_two_independent_copies() {
    // This deliberately injects identical native IDs from two scopes. In
    // Windows they may come from different SMB servers, so independence must
    // remain unknown instead of being inferred from the source IDs.
    struct SameIdentity {
        proof: Proof,
        hashed: u64,
    }
    impl Verification for SameIdentity {
        fn observe(&self, _: usize, _: &Path, _: u64, _: Option<SystemTime>) -> io::Result<Proof> {
            Ok(self.proof.clone())
        }
        fn hash(
            &mut self,
            _: usize,
            _: &Path,
            _: &Proof,
            _: &mut dyn FnMut(DuplicateProgress),
        ) -> io::Result<[u8; 32]> {
            self.hashed += 1;
            Ok([0; 32])
        }
        fn validate(&self, _: usize, _: &Path, _: &Proof) -> io::Result<()> {
            Ok(())
        }
        fn equal(
            &mut self,
            _: (usize, &Path, &Proof),
            _: (usize, &Path, &Proof),
            _: &[u8; 32],
            _: &mut dyn FnMut(DuplicateProgress),
        ) -> io::Result<bool> {
            panic!("matching identities cannot become independent members");
        }
        fn progress(&self) -> sortsize_core::duplicates::DuplicateProgress {
            sortsize_core::duplicates::DuplicateProgress {
                files_hashed: self.hashed,
                bytes_read: 0,
            }
        }
    }
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"same").unwrap();
    fs::write(b.join("copy"), b"same").unwrap();
    let sa = acquire(&a, t.path(), 461);
    let sb = acquire(&b, t.path(), 462);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let cancel = CancellationToken::default();
    let guard = Guard::open(&a, Policy::default()).unwrap();
    let proof = Verifier::new(Default::default(), &cancel)
        .observe(&guard, &a.join("original"), 4, None)
        .unwrap();
    job.verify_with(&mut SameIdentity { proof, hashed: 0 }, &cancel)
        .unwrap();
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().files_hashed, 1);
    #[cfg(windows)]
    {
        assert!(!job.summary().complete);
        assert_eq!(job.summary().ambiguous_aliases, 1);
        assert_eq!(job.scope_errors().unwrap(), vec![0, 1]);
    }
    #[cfg(target_os = "linux")]
    {
        // device+inode is a shared namespace on the same Linux host.
        assert!(job.summary().complete);
        assert_eq!(job.summary().ambiguous_aliases, 0);
    }
}

#[test]
fn ab_offline_source_with_no_equal_size_candidates_cannot_claim_complete() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"four").unwrap();
    fs::write(b.join("different-size"), b"seven!!").unwrap();
    let sa = acquire(&a, t.path(), 471);
    let sb = acquire(&b, t.path(), 472);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let scoped_guards = guards(&job);
    fs::rename(&b, t.path().join("disconnected-b")).unwrap();
    job.verify_scoped(
        &scoped_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(!job.summary().complete, "{:?}", job.summary());
    assert!(job.summary().errors > 0);
    let errors = job.scope_errors().unwrap();
    assert_eq!(errors[0], 0);
    assert!(errors[1] > 0);
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().bytes_read, 0);
}

#[test]
fn ab_a_only_size_does_not_spend_content_budget() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("a-only-1"), b"four").unwrap();
    fs::write(a.join("a-only-2"), b"four").unwrap();
    fs::write(b.join("different-size"), b"seven!!").unwrap();
    let sa = acquire(&a, t.path(), 481);
    let sb = acquire(&b, t.path(), 482);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        DuplicateOptions {
            max_bytes_read: Some(1),
            ..Default::default()
        },
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().files_hashed, 0);
    assert_eq!(job.summary().bytes_read, 0);
    assert!(!job.summary().budget_limited);
}

#[test]
fn ab_root_replaced_before_guard_open_cannot_reuse_the_old_inventory_provenance() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"four").unwrap();
    fs::write(b.join("different-size"), b"seven!!").unwrap();
    let sa = acquire(&a, t.path(), 491);
    let sb = acquire(&b, t.path(), 492);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    fs::rename(&b, t.path().join("previous-b")).unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(b.join("different-size"), b"seven!!").unwrap();
    let mut job = ab(&qa, &qb, t.path());
    let scoped_guards = guards(&job);
    // A fresh guard can legitimately authorize the replacement root. This
    // alone does not establish continuity with the inventory's original root.
    assert!(scoped_guards[1].inspect(&b).unwrap().directory);
    assert!(!Session::source_matches(sb.store(), GenerationId(1), &scoped_guards[1]).unwrap());
    job.verify_scoped(
        &scoped_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(!job.summary().complete, "{:?}", job.summary());
    assert!(job.summary().errors > 0);
    let errors = job.scope_errors().unwrap();
    assert_eq!(errors[0], 0);
    assert!(errors[1] > 0);
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().bytes_read, 0);
}

#[test]
fn ab_unverified_source_compares_current_files_without_claiming_coverage_or_copies() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("renamed"), b"current same content").unwrap();
    fs::write(b.join("one"), b"current same content").unwrap();
    fs::write(b.join("two"), b"current same content").unwrap();
    let sa = acquire(&a, t.path(), 501);
    let sb = acquire(&b, t.path(), 502);
    // Model a provider that could not record a root birth-time certificate.
    // This is fixture setup only; verification never writes this canonical DB.
    sb.store()
        .db
        .execute(
            "DELETE FROM recovery_guard WHERE generation=?1",
            [unsigned(1)],
        )
        .unwrap();
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let before_a = fs::read(sa.store().directory.join("inventory.db")).unwrap();
    let before_b = fs::read(sb.store().directory.join("inventory.db")).unwrap();
    let mut job = ab(&qa, &qb, t.path());
    let scoped_guards = guards(&job);
    assert_eq!(
        Session::source_attachment(sb.store(), GenerationId(1), &scoped_guards[1]).unwrap(),
        Attachment::Uncertain
    );
    assert!(!Session::source_matches(sb.store(), GenerationId(1), &scoped_guards[1]).unwrap());
    job.verify_scoped(
        &scoped_guards,
        DuplicateOptions {
            max_errors: 1,
            ..Default::default()
        },
        &CancellationToken::default(),
    )
    .unwrap();
    let s = job.summary();
    assert!(s.verified);
    assert!(!s.complete);
    assert!(!s.source_independence_verified);
    assert_eq!(s.unverified_sources, 1);
    assert_eq!(s.errors, 0);
    assert_eq!(s.groups, 1);
    assert!(s.bytes_read > 0);
    assert_eq!(job.scope_errors().unwrap(), vec![0, 0]);
    assert_eq!(job.warnings().len(), 1);
    assert_eq!(job.warnings()[0].0, 1);
    let group = job.groups(None).unwrap().remove(0);
    assert_eq!(group.paths, 3);
    let page = job.page(None, false).unwrap();
    assert_eq!(page.rows.len(), 3);
    let observed = s.bytes_read;
    let moved = t.path().join("offline-b");
    fs::rename(&b, &moved).unwrap();
    assert_eq!(job.page(None, false).unwrap().rows.len(), 3);
    assert_eq!(job.summary().bytes_read, observed);
    fs::rename(moved, &b).unwrap();
    let mut limited = ab(&qa, &qb, t.path());
    limited
        .verify_scoped(
            &guards(&limited),
            DuplicateOptions {
                max_bytes_read: Some(1),
                ..Default::default()
            },
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(limited.summary().budget_limited);
    assert!(!limited.summary().complete);
    assert_eq!(limited.summary().unverified_sources, 1);
    assert_eq!(limited.summary().errors, 0);
    let cancel = CancellationToken::default();
    cancel.cancel();
    let mut cancelled = ab(&qa, &qb, t.path());
    cancelled
        .verify_scoped(&guards(&cancelled), Default::default(), &cancel)
        .unwrap();
    assert!(cancelled.summary().cancelled);
    assert_eq!(cancelled.summary().bytes_read, 0);
    assert!(!cancelled.summary().source_independence_verified);
    fs::write(b.join("two"), b"changed length").unwrap();
    let mut changed = ab(&qa, &qb, t.path());
    changed
        .verify_scoped(
            &guards(&changed),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(changed.summary().unverified_sources, 1);
    assert!(changed.summary().errors > 0);
    assert!(!changed.summary().complete);
    assert!(!changed.summary().source_independence_verified);
    assert_eq!(
        fs::read(sa.store().directory.join("inventory.db")).unwrap(),
        before_a
    );
    assert_eq!(
        fs::read(sb.store().directory.join("inventory.db")).unwrap(),
        before_b
    );
}

#[cfg(unix)]
#[test]
fn ab_unverified_source_still_does_not_turn_known_hardlinks_into_independent_copies() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"hardlinked current bytes").unwrap();
    fs::hard_link(a.join("original"), b.join("alias")).unwrap();
    let sa = acquire(&a, t.path(), 511);
    let sb = acquire(&b, t.path(), 512);
    sb.store()
        .db
        .execute(
            "DELETE FROM recovery_guard WHERE generation=?1",
            [unsigned(1)],
        )
        .unwrap();
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(job.summary().groups, 0);
    assert_eq!(job.summary().unverified_sources, 1);
    assert_eq!(job.summary().errors, 0);
    assert!(!job.summary().complete);
    assert!(!job.summary().source_independence_verified);
}

#[cfg(windows)]
#[test]
fn ab_windows_requires_exact_native_scope_spelling_and_keeps_subfolders() {
    let _serial = TEST.lock().unwrap();
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("ScopeSourceA");
    let b = t.path().join("ScopeSourceB");
    for source in [&a, &b] {
        fs::create_dir_all(source.join("ChosenFolder")).unwrap();
        fs::write(source.join("ChosenFolder").join("inside"), b"chosen").unwrap();
        fs::write(source.join("outside"), b"outside").unwrap();
    }
    let sa = acquire(&a, t.path(), 991);
    let sb = acquire(&b, t.path(), 992);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    // Scope spelling is intentionally as strict as Guard authorization. A
    // case-folding metadata helper cannot grant access in case-sensitive dirs.
    assert!(
        prepare_scopes(
            vec![
                Scope {
                    query: &qa,
                    roots: vec![t.path().join("scopesourcea").join("chosenfolder")],
                    side: 0
                },
                Scope {
                    query: &qb,
                    roots: vec![t.path().join("scopesourceb").join("chosenfolder")],
                    side: 1
                },
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default(),
        )
        .is_err()
    );
    let mut job = prepare_scopes(
        vec![
            Scope {
                query: &qa,
                roots: vec![a.join("ChosenFolder")],
                side: 0,
            },
            Scope {
                query: &qb,
                roots: vec![b.join("ChosenFolder")],
                side: 1,
            },
        ],
        Mode::BetweenSources,
        t.path(),
        DISK,
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(job.summary().selected_files, 2);
    let selected_guards = guards(&job);
    job.verify_scoped(
        &selected_guards,
        DuplicateOptions::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.summary().errors, 0);
    let page = job.page(None, false).unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(
        page.rows
            .iter()
            .all(|r| r.path.file_name().unwrap() == "inside")
    );
}

#[cfg(windows)]
#[test]
fn windows_comparison_rejects_case_and_verbatim_nested_aliases() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let parent = t.path().join("ScopeParent");
    let child = parent.join("ChosenFolder");
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("inside"), b"same").unwrap();
    let parent_session = acquire_with_source(&parent, t.path(), 3041, 3000);
    let parent_query = query(&parent_session, &parent, t.path());
    // Exercise only the verbatim namespace spelling. canonicalize would also
    // expand any 8.3 ancestor in TEMP, changing the fixture's path components.
    let verbatim_child = if matches!(child.components().next(),
        Some(std::path::Component::Prefix(prefix)) if prefix.kind().is_verbatim())
    {
        child.clone()
    } else {
        let mut native = std::ffi::OsString::from(r"\\?\");
        native.push(child.as_os_str());
        PathBuf::from(native)
    };
    for (id, alias) in [
        (3042, t.path().join("scopeparent").join("chosenfolder")),
        (3043, verbatim_child),
    ] {
        assert!(
            contains_path(&parent, &alias),
            "case={id}: fixture must preserve native components: parent={parent:?}, alias={alias:?}"
        );
        let child_session = acquire_with_source(&alias, t.path(), id, 3000);
        let child_query = query(&child_session, &alias, t.path());
        let result = prepare_scopes(
            vec![
                Scope {
                    query: &parent_query,
                    roots: vec![parent.clone()],
                    side: 0,
                },
                Scope {
                    query: &child_query,
                    roots: vec![alias.clone()],
                    side: 1,
                },
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default(),
        );
        let outcome = match &result {
            Ok(_) => "accepted comparison".to_owned(),
            Err(error) => error.to_string(),
        };
        eprintln!("case={id}, parent={parent:?}, alias={alias:?}, result={outcome}");
        assert!(
            matches!(
                result,
                Err(Error::Contract(
                    "chosen comparison folders are identical or nested"
                ))
            ),
            "case={id}, parent={parent:?}, alias={alias:?}, result={outcome}"
        );
    }
}

#[test]
fn file_result_pages_partition_folder_members_without_source_reads() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for root in [&a, &b] {
        for index in 0..110 {
            fs::write(root.join(format!("entry-{index:03}")), b"").unwrap();
        }
    }
    let sa = acquire(&a, t.path(), 971);
    let sb = acquire(&b, t.path(), 972);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    job.verify_scoped(
        &guards(&job),
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    // Exercise result-page partitioning independently of folder discovery: the
    // first ten candidate locations are represented by a folder summary.
    job.db.execute("DELETE FROM folder_covered", []).unwrap();
    job.db.execute(
        "INSERT INTO folder_covered(candidate,folder) SELECT id,?1 FROM candidates ORDER BY id LIMIT 10",
        [unsigned(1)],
    ).unwrap();
    let bytes_before = job.summary().bytes_read;
    fs::rename(&a, t.path().join("offline-a")).unwrap();
    fs::rename(&b, t.path().join("offline-b")).unwrap();
    let first = job.file_page(None, false).unwrap();
    assert_eq!(first.rows.len(), 200);
    assert!(first.more);
    assert_eq!(job.file_group_count().unwrap(), 1);
    let second = job.file_page(first.last.as_ref(), false).unwrap();
    assert_eq!(second.rows.len(), 10);
    assert!(!second.more);
    let previous = job.file_page(second.first.as_ref(), true).unwrap();
    assert_eq!(previous.rows.len(), 200);
    assert_eq!(
        first.rows.iter().map(|r| &r.path).collect::<Vec<_>>(),
        previous.rows.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
    assert_eq!(job.page(None, false).unwrap().rows.len(), 200);
    assert_eq!(job.summary().bytes_read, bytes_before);
    let covered: Vec<PathBuf> = job
        .db
        .prepare("SELECT c.path FROM candidates c JOIN folder_covered f ON f.candidate=c.id")
        .unwrap()
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|r| path(r.unwrap()).unwrap())
        .collect();
    assert!(
        first
            .rows
            .iter()
            .chain(&second.rows)
            .all(|r| !covered.contains(&r.path))
    );
}

#[test]
fn ab_rejects_an_explicit_third_source_on_either_side_before_content() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let c = t.path().join("source-c");
    fs::create_dir(&c).unwrap();
    for root in [&a, &b, &c] {
        fs::write(root.join("same"), b"same bytes").unwrap();
    }
    let sa = acquire(&a, t.path(), 981);
    let sb = acquire(&b, t.path(), 982);
    let sc = acquire(&c, t.path(), 983);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    for third_side in [0, 1] {
        let prepared = prepare_scopes(
            vec![
                Scope {
                    query: &qa,
                    roots: vec![a.clone()],
                    side: 0,
                },
                Scope {
                    query: &qb,
                    roots: vec![b.clone()],
                    side: 1,
                },
                Scope {
                    query: &qc,
                    roots: vec![c.clone()],
                    side: third_side,
                },
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default(),
        );
        assert!(matches!(prepared, Err(Error::Contract(_))));
    }
}

#[test]
fn ab_selected_child_reuses_only_qualified_recorded_child_evidence() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let left = a.join("chosen-a");
    let right = b.join("chosen-b");
    for root in [&left, &right] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("inside"), b"exact selected bytes").unwrap();
        fs::create_dir(root.join("empty")).unwrap();
    }
    fs::write(a.join("outside"), b"outside A").unwrap();
    fs::write(b.join("outside"), b"outside B").unwrap();
    let sa = acquire(&a, t.path(), 985);
    let sb = acquire(&b, t.path(), 986);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let selected = || {
        prepare_scopes(
            vec![
                Scope {
                    query: &qa,
                    roots: vec![left.clone()],
                    side: 0,
                },
                Scope {
                    query: &qb,
                    roots: vec![right.clone()],
                    side: 1,
                },
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default(),
        )
        .unwrap()
    };
    let mut exact = selected();
    assert_eq!(exact.summary().selected_files, 2);
    exact
        .verify_scoped(
            &guards(&exact),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(exact.summary().unverified_sources, 0);
    assert_eq!(exact.summary().folder_groups, 1, "{:?}", exact.summary());
    let group = exact.folder_groups(None).unwrap().remove(0);
    assert_eq!((group.a.path, group.b.path), (left.clone(), right.clone()));
    assert_eq!(exact.file_group_count().unwrap(), 0);
    drop(exact);
    let id = sb
        .store()
        .db
        .query_row(
            "SELECT id FROM entries WHERE generation=?1 AND parent=?2 AND name=?3",
            params![
                unsigned(1),
                unsigned(0),
                native_path(Path::new("chosen-b")).encode()
            ],
            |r| number(r, 0),
        )
        .unwrap();
    let proof: Vec<u8> = sb
        .store()
        .db
        .query_row(
            "SELECT proof FROM entry_evidence WHERE generation=?1 AND id=?2",
            params![unsigned(1), unsigned(id)],
            |r| r.get(0),
        )
        .unwrap();
    sb.store()
        .db
        .execute(
            "DELETE FROM entry_evidence WHERE generation=?1 AND id=?2",
            params![unsigned(1), unsigned(id)],
        )
        .unwrap();
    let mut missing = selected();
    missing
        .verify_scoped(
            &guards(&missing),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(missing.summary().unverified_sources, 1);
    assert_eq!(missing.summary().groups, 1);
    assert_eq!(missing.summary().folder_groups, 0);
    assert!(!missing.summary().complete);
    drop(missing);
    sb.store()
        .db
        .execute(
            "INSERT INTO entry_evidence VALUES(?1,?2,?3)",
            params![unsigned(1), unsigned(id), proof],
        )
        .unwrap();
    let root_proof: Vec<u8> = sb
        .store()
        .db
        .query_row(
            "SELECT proof FROM recovery_guard WHERE generation=?1",
            [unsigned(1)],
            |r| r.get(0),
        )
        .unwrap();
    sb.store()
        .db
        .execute(
            "DELETE FROM recovery_guard WHERE generation=?1",
            [unsigned(1)],
        )
        .unwrap();
    let mut unknown_source = selected();
    unknown_source
        .verify_scoped(
            &guards(&unknown_source),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(unknown_source.summary().unverified_sources, 1);
    assert_eq!(unknown_source.summary().groups, 1);
    assert_eq!(unknown_source.summary().folder_groups, 0);
    drop(unknown_source);
    sb.store()
        .db
        .execute(
            "INSERT INTO recovery_guard VALUES(?1,?2)",
            params![unsigned(1), root_proof],
        )
        .unwrap();
    fs::rename(&right, b.join("old-chosen-b")).unwrap();
    fs::create_dir(&right).unwrap();
    fs::write(right.join("inside"), b"exact selected bytes").unwrap();
    fs::create_dir(right.join("empty")).unwrap();
    let mut replaced = selected();
    replaced
        .verify_scoped(
            &guards(&replaced),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(replaced.summary().errors > 0);
    assert_eq!(replaced.summary().groups, 0);
    assert_eq!(replaced.summary().bytes_read, 0);
}

#[test]
fn residual_file_location_references_the_covered_folder_without_recounting_it() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::create_dir(b.join("mirror")).unwrap();
    for p in [
        a.join("item"),
        b.join("mirror").join("item"),
        b.join("extra-copy"),
    ] {
        fs::write(p, b"one exact content").unwrap();
    }
    let sa = acquire(&a, t.path(), 987);
    let sb = acquire(&b, t.path(), 988);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut job = ab(&qa, &qb, t.path());
    job.verify_scoped(
        &guards(&job),
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.summary().folder_groups, 1, "{:?}", job.summary());
    let folder = job.folder_groups(None).unwrap().remove(0);
    assert_eq!(folder.files, 1);
    assert_eq!(job.summary().folder_files, 1);
    let files = job.file_page(None, false).unwrap();
    assert_eq!(files.rows.len(), 1);
    assert_eq!(files.rows[0].path, b.join("extra-copy"));
    assert_eq!(files.folder_references, vec![Some(folder.id)]);
    assert_eq!(files.folder_reference_counts, vec![1]);
    assert_eq!(job.file_group_count().unwrap(), 1);
    assert_eq!(
        job.folder_page(folder.id, None, false).unwrap().rows.len(),
        1
    );
    assert_eq!(job.page(None, false).unwrap().rows.len(), 3);
    let bytes = job.summary().bytes_read;
    fs::rename(&a, t.path().join("offline-a")).unwrap();
    fs::rename(&b, t.path().join("offline-b")).unwrap();
    let cached = job.file_page(None, false).unwrap();
    assert_eq!(cached.folder_references, vec![Some(folder.id)]);
    assert_eq!(cached.folder_reference_counts, vec![1]);
    assert_eq!(job.summary().bytes_read, bytes);
}

// Three local folders with injected distinct catalogue identities exercise the
// engine contract only; this fixture is not a qualification of three volumes.
fn abc<'q, 's>(
    a: &'q Query<'s>,
    b: &'q Query<'s>,
    c: &'q Query<'s>,
    scratch: &Path,
) -> DuplicateJob<'q, 's> {
    prepare_scopes(
        vec![
            Scope {
                query: a,
                roots: vec![a.root.clone()],
                side: 0,
            },
            Scope {
                query: b,
                roots: vec![b.root.clone()],
                side: 1,
            },
            Scope {
                query: c,
                roots: vec![c.root.clone()],
                side: 2,
            },
        ],
        Mode::BetweenThreeSources,
        scratch,
        DISK,
        &CancellationToken::default(),
    )
    .unwrap()
}

#[test]
fn abc_files_require_two_independent_sides_and_reference_membership() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let c = t.path().join("source-c");
    fs::create_dir(&c).unwrap();
    fs::write(a.join("alone-1"), b"internal only").unwrap();
    fs::write(a.join("alone-2"), b"internal only").unwrap();
    fs::write(a.join("other"), b"different").unwrap();
    fs::write(b.join("copy-b"), b"shared bc").unwrap();
    fs::write(c.join("copy-c"), b"shared bc").unwrap();
    let sa = acquire(&a, t.path(), 1001);
    let sb = acquire(&b, t.path(), 1002);
    let sc = acquire(&c, t.path(), 1003);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = abc(&qa, &qb, &qc, t.path());
    assert_eq!(j.summary().bytes_read, 0);
    j.verify_scoped(
        &guards(&j),
        Default::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(j.summary().complete, "{:?}", j.summary());
    assert_eq!(j.summary().groups, 1);
    let group = j.groups(None).unwrap().remove(0);
    assert_eq!(group.paths, 2);
    let members = j.members(group.id, None).unwrap();
    assert!(members.iter().all(|m| m.source != SourceId(1001)));
    j.set_protected_reference(Some(0)).unwrap();
    assert_eq!(
        j.review_preview().unwrap().reason,
        ReviewReason::MissingReference
    );
    assert!(j.select_all_review_copies().is_err());
    for m in &members {
        assert_eq!(
            j.review_candidate_state(m.cursor).unwrap().reason,
            ReviewReason::MissingReference
        );
    }
    j.set_protected_reference(Some(1)).unwrap();
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 1);
    assert_eq!(j.review_summary().unwrap().logical_bytes, 9);
    j.set_protected_reference(Some(2)).unwrap();
    assert_eq!(j.review_summary().unwrap().files, 0);
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 1);
    let bytes = j.summary().bytes_read;
    fs::rename(&b, t.path().join("offline-b")).unwrap();
    fs::rename(&c, t.path().join("offline-c")).unwrap();
    j.clear_review_selection().unwrap();
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 1);
    assert_eq!(j.summary().bytes_read, bytes);
}

#[test]
fn abc_absent_guard_keeps_verified_ab_but_global_evidence_incomplete() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let c = t.path().join("source-c");
    fs::create_dir(&c).unwrap();
    for root in [&a, &b, &c] {
        fs::write(root.join("item"), b"same").unwrap();
    }
    let sa = acquire(&a, t.path(), 1011);
    let sb = acquire(&b, t.path(), 1012);
    let sc = acquire(&c, t.path(), 1013);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = abc(&qa, &qb, &qc, t.path());
    let available = vec![
        Some(Guard::open(&a, Policy::default()).unwrap()),
        Some(Guard::open(&b, Policy::default()).unwrap()),
        None,
    ];
    fs::rename(&c, t.path().join("offline-c")).unwrap();
    j.verify_available_scoped_with_progress(
        &available,
        Default::default(),
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert!(!j.summary().complete, "{:?}", j.summary());
    assert!(!j.summary().source_independence_verified);
    assert_eq!(j.summary().files_hashed, 2);
    assert_eq!(j.summary().groups, 1);
    assert_eq!(j.groups(None).unwrap()[0].paths, 2);
    assert!(j.scope_errors().unwrap()[2] > 0);
    j.set_protected_reference(Some(0)).unwrap();
    assert_eq!(
        j.review_summary().unwrap().reason,
        ReviewReason::IncompleteEvidence
    );
    assert!(j.select_all_review_copies().is_err());
    assert_eq!(j.review_summary().unwrap().files, 0);
}

#[test]
fn abc_explicit_mode_is_required_and_content_budget_remains_global() {
    let _serial = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let c = t.path().join("source-c");
    fs::create_dir(&c).unwrap();
    for root in [&a, &b, &c] {
        fs::write(root.join("item"), b"same").unwrap();
    }
    let sa = acquire(&a, t.path(), 1021);
    let sb = acquire(&b, t.path(), 1022);
    let sc = acquire(&c, t.path(), 1023);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    assert!(
        prepare_scopes(
            vec![
                Scope {
                    query: &qa,
                    roots: vec![a.clone()],
                    side: 0
                },
                Scope {
                    query: &qb,
                    roots: vec![b.clone()],
                    side: 1
                },
                Scope {
                    query: &qc,
                    roots: vec![c.clone()],
                    side: 2
                }
            ],
            Mode::BetweenSources,
            t.path(),
            DISK,
            &CancellationToken::default()
        )
        .is_err()
    );
    let mut j = abc(&qa, &qb, &qc, t.path());
    j.verify_scoped(
        &guards(&j),
        DuplicateOptions {
            max_bytes_read: Some(8),
            ..Default::default()
        },
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(j.summary().budget_limited, "{:?}", j.summary());
    assert!(j.summary().bytes_read <= 8);
    assert!(!j.summary().complete);
    assert_eq!(j.summary().groups, 0);
    drop(j);
    let mut ab_job = ab(&qa, &qb, t.path());
    assert!(ab_job.set_protected_reference(Some(2)).is_err());
    ab_job
        .verify_scoped(
            &guards(&ab_job),
            Default::default(),
            &CancellationToken::default(),
        )
        .unwrap();
    assert!(ab_job.summary().complete);
    assert_eq!(ab_job.summary().files_hashed, 2);
    assert_eq!(ab_job.groups(None).unwrap()[0].paths, 2);
}
