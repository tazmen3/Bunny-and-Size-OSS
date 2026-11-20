use super::*;
use crate::scheduler::{Session, State};
use sortsize_core::acquisition::Policy;
use std::fs;
static SERIAL: Mutex<()> = Mutex::new(());
const DISK: u64 = 32 * 1024 * 1024;
fn acquire(root: &Path, scratch: &Path, id: u128) -> Session {
    let context = AcquisitionContext {
        inventory: InventoryId(id),
        source: SourceId(id),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut session = Session::new(
        Store::create(
            &scratch.join(format!("store-{id}")),
            context.inventory,
            context.source,
            crate::Durability::Reconstructible,
        )
        .unwrap(),
        context,
        Guard::open(root, Policy::default()).unwrap(),
    )
    .unwrap();
    let start = Instant::now();
    while session.tick().unwrap().state != State::Idle {
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    session
}
fn query<'a>(session: &'a Session, root: &Path, scratch: &Path) -> Query<'a> {
    let mut query = session
        .store()
        .start_query(
            GenerationId(1),
            root,
            &SearchQuery::default(),
            scratch,
            DISK,
        )
        .unwrap();
    while !query.advance(&CancellationToken::default()).unwrap() {}
    query
}
fn job<'a, 's>(a: &'a Query<'s>, b: &'a Query<'s>, scratch: &Path) -> DuplicateJob<'a, 's> {
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
fn roots(t: &Path) -> (PathBuf, PathBuf) {
    let a = t.join("alpha");
    let b = t.join("other-root-name");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    (a, b)
}
fn guards(job: &DuplicateJob<'_, '_>) -> Vec<Guard> {
    job.guard_roots()
        .iter()
        .map(|p| Guard::open(p, Policy::default()).unwrap())
        .collect()
}
fn verify(job: &mut DuplicateJob<'_, '_>) {
    let g = guards(job);
    job.verify_scoped(&g, Default::default(), &CancellationToken::default())
        .unwrap();
}
#[test]
fn fileless_roots_and_nested_trees_are_not_grouped() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for nested in [false, true] {
        if nested {
            for root in [&a, &b] {
                fs::create_dir_all(root.join("empty/nested")).unwrap();
            }
        }
        let sa = acquire(&a, t.path(), if nested { 9003 } else { 9001 });
        let sb = acquire(&b, t.path(), if nested { 9004 } else { 9002 });
        let qa = query(&sa, &a, t.path());
        let qb = query(&sb, &b, t.path());
        let mut j = job(&qa, &qb, t.path());
        verify(&mut j);
        assert!(j.summary.complete, "{:?}", j.summary);
        assert_eq!(j.summary.folder_groups, 0);
        assert_eq!(j.summary.folder_files, 0);
        assert_eq!(j.summary.bytes_read, 0);
        assert!(j.folder_groups(None).unwrap().is_empty());
        assert!(j.folder_groups_before(None).unwrap().is_empty());
        assert!(j.folder_group(1).unwrap().is_none());
    }
}

#[test]
fn zero_byte_files_count_and_empty_children_still_define_folder_equality() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for root in [&a, &b] {
        fs::create_dir_all(root.join("empty/nested")).unwrap();
        fs::write(root.join("zero"), b"").unwrap();
    }
    let sa = acquire(&a, t.path(), 9005);
    let sb = acquire(&b, t.path(), 9006);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    let group = j.folder_group(1).unwrap().unwrap();
    assert_eq!(group.a.path, a);
    assert_eq!(group.b.path, b);
    assert_eq!(group.files, 1);
    assert_eq!(group.logical_bytes, 0);
    assert_eq!(j.summary.folder_groups, 1);
    assert_eq!(j.folder_page(group.id, None, false).unwrap().rows.len(), 3);
    drop(j);

    fs::remove_dir(b.join("empty/nested")).unwrap();
    let sb = acquire(&b, t.path(), 9007);
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    assert!(j.folder_groups(None).unwrap().is_empty());
    assert_eq!(j.file_group_count().unwrap(), 1);
}

#[test]
fn missing_freshness_on_a_fileless_root_is_still_unverified() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let sa = acquire(&a, t.path(), 9008);
    let sb = acquire(&b, t.path(), 9009);
    sa.store()
        .db
        .execute("UPDATE generations SET sealed=0 WHERE id=?1", [unsigned(1)])
        .unwrap();
    sa.store()
        .db
        .execute(
            "UPDATE entries SET modified_s=NULL,modified_ns=NULL WHERE generation=?1 AND id=?2",
            params![unsigned(1), unsigned(0)],
        )
        .unwrap();
    sa.store()
        .db
        .execute("UPDATE generations SET sealed=1 WHERE id=?1", [unsigned(1)])
        .unwrap();
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(!j.summary.complete);
    assert!(j.summary.folder_unverified > 0);
    assert!(j.folder_groups(None).unwrap().is_empty());
}
#[test]
fn missing_root_freshness_leaves_verified_child_and_standalone_files() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::create_dir(p.join("nested")).unwrap();
        fs::write(p.join("nested/item"), b"child").unwrap();
        fs::write(p.join("loose"), b"standalone").unwrap();
    }
    let sa = acquire(&a, t.path(), 9011);
    let sb = acquire(&b, t.path(), 9012);
    sa.store()
        .db
        .execute("UPDATE generations SET sealed=0 WHERE id=?1", [unsigned(1)])
        .unwrap();
    sa.store()
        .db
        .execute(
            "UPDATE entries SET modified_s=NULL,modified_ns=NULL WHERE generation=?1 AND id=?2",
            params![unsigned(1), unsigned(0)],
        )
        .unwrap();
    sa.store()
        .db
        .execute("UPDATE generations SET sealed=1 WHERE id=?1", [unsigned(1)])
        .unwrap();
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(!j.summary.complete);
    assert!(j.summary.folder_unverified > 0);
    let groups = j.folder_groups(None).unwrap();
    assert_eq!(groups.len(), 1, "{:?}", j.summary);
    assert_eq!(groups[0].a.path, a.join("nested"));
    assert_eq!(groups[0].files, 1);
    assert_eq!(j.file_group_count().unwrap(), 1);
}
#[test]
fn addition_after_inventory_never_uses_only_the_duplicate_file_list() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::write(p.join("item"), b"same").unwrap();
    }
    let sa = acquire(&a, t.path(), 9021);
    let sb = acquire(&b, t.path(), 9022);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    fs::write(b.join("not-in-inventory"), b"additional").unwrap();
    verify(&mut j);
    assert_eq!(j.summary.folder_groups, 0);
    assert_eq!(j.summary.groups, 1);
    assert!(j.summary.folder_unverified > 0);
    assert!(!j.summary.complete);
}
#[test]
fn directory_mutation_during_hash_and_cancelled_work_keep_exact_file_results() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::write(p.join("item"), b"same").unwrap();
    }
    let sa = acquire(&a, t.path(), 9031);
    let sb = acquire(&b, t.path(), 9032);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    let g = guards(&j);
    let mut mutated = false;
    j.verify_scoped_with_progress(&g, Default::default(), &CancellationToken::default(), |s| {
        if !mutated && s.files_hashed > 0 {
            fs::create_dir(b.join("late-empty")).unwrap();
            mutated = true;
        }
    })
    .unwrap();
    assert!(mutated);
    assert_eq!(j.summary.folder_groups, 0);
    assert_eq!(j.summary.groups, 1);
    assert_eq!(j.file_group_count().unwrap(), 1);
    assert!(!j.summary.complete);
    let mut j = job(&qa, &qb, t.path());
    let g = guards(&j);
    let cancel = CancellationToken::default();
    j.verify_scoped_with_progress(&g, Default::default(), &cancel, |s| {
        if s.files_hashed > 0 {
            cancel.cancel();
        }
    })
    .unwrap();
    assert!(j.summary.cancelled);
    assert!(!j.summary.complete);
    assert_eq!(j.summary.folder_groups, 0);
    assert!(j.folder_groups(None).unwrap().is_empty());
}
#[test]
fn partial_parent_listing_does_not_hide_complete_child() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::create_dir(p.join("child")).unwrap();
        fs::write(p.join("child/item"), b"same").unwrap();
    }
    let sa = acquire(&a, t.path(), 9041);
    let sb = acquire(&b, t.path(), 9042);
    sa.store()
        .db
        .execute(
            "UPDATE tasks SET listing=2 WHERE generation=?1 AND directory=?2",
            params![unsigned(1), unsigned(0)],
        )
        .unwrap();
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    let groups = j.folder_groups(None).unwrap();
    assert_eq!(groups.len(), 1, "{:?}", j.summary);
    assert_eq!(groups[0].a.path, a.join("child"));
    assert!(!j.summary.complete);
    assert!(j.summary.folder_unverified > 0);
}
#[test]
fn native_names_are_exact_and_cursor_belongs_to_its_group_and_job() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    let names = [
        std::ffi::OsString::from("Été 日本語"),
        std::ffi::OsString::from("e\u{301}té"),
    ];
    for p in [&a, &b] {
        for name in &names {
            fs::write(p.join(name), name.to_string_lossy().as_bytes()).unwrap();
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let name = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
        for p in [&a, &b] {
            fs::write(p.join(&name), b"native").unwrap();
        }
    }
    let sa = acquire(&a, t.path(), 9051);
    let sb = acquire(&b, t.path(), 9052);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    let groups = j.folder_groups(None).unwrap();
    assert_eq!(groups.len(), 1, "{:?}", j.summary);
    let page = j.folder_page(groups[0].id, None, false).unwrap();
    assert_eq!(page.rows.len(), if cfg!(unix) { 3 } else { 2 });
    let cursor = page.first.unwrap();
    assert!(
        j.folder_page(groups[0].id + 1, Some(&cursor), false)
            .is_err()
    );
    let mut other = job(&qa, &qb, t.path());
    verify(&mut other);
    assert!(
        other
            .folder_page(groups[0].id, Some(&cursor), false)
            .is_err()
    );
}
#[test]
fn filtered_files_cannot_certify_the_unselected_tree() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::write(p.join("keep"), b"same").unwrap();
        fs::write(p.join("hidden"), b"also same").unwrap();
    }
    let sa = acquire(&a, t.path(), 9061);
    let sb = acquire(&b, t.path(), 9062);
    let filter = SearchQuery {
        name: Some("keep".into()),
        ..Default::default()
    };
    let mut qa = sa
        .store()
        .start_query(GenerationId(1), &a, &filter, t.path(), DISK)
        .unwrap();
    while !qa.advance(&CancellationToken::default()).unwrap() {}
    let mut qb = sb
        .store()
        .start_query(GenerationId(1), &b, &filter, t.path(), DISK)
        .unwrap();
    while !qb.advance(&CancellationToken::default()).unwrap() {}
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert_eq!(j.summary.folder_groups, 0);
    assert_eq!(j.summary.groups, 1);
    assert_eq!(j.file_group_count().unwrap(), 1);
    assert!(j.folder_groups(None).unwrap().is_empty());
}
#[test]
fn same_native_file_identity_is_not_a_second_folder_copy() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("item"), b"hardlink").unwrap();
    fs::hard_link(a.join("item"), b.join("item")).unwrap();
    let sa = acquire(&a, t.path(), 9071);
    let sb = acquire(&b, t.path(), 9072);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert_eq!(j.summary.folder_groups, 0);
    assert!(j.folder_groups(None).unwrap().is_empty());
}
#[test]
fn same_size_file_mutation_after_exact_verification_cannot_gain_folder_badge() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::write(p.join("item"), b"four").unwrap();
    }
    let sa = acquire(&a, t.path(), 9081);
    let sb = acquire(&b, t.path(), 9082);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    let g = guards(&j);
    let cancel = CancellationToken::default();
    let g_refs = g.iter().map(Some).collect::<Vec<_>>();
    let mut native = NativeVerification {
        verifier: Verifier::new(Default::default(), &cancel),
        guards: &g_refs,
    };
    j.verify_with(&mut native, &cancel).unwrap();
    assert_eq!(j.summary.groups, 1);
    let bytes = j.summary.bytes_read;
    fs::write(b.join("item"), b"five").unwrap();
    super::verify(&mut j, &g_refs, &cancel).unwrap();
    assert_eq!(j.summary.folder_groups, 0);
    assert!(j.folder_groups(None).unwrap().is_empty());
    assert_eq!(j.summary.groups, 0);
    assert_eq!(j.summary.bytes_read, bytes);
    assert!(j.summary.errors > 0);
    assert!(!j.summary.complete);
}
#[test]
fn highest_pair_on_b_precedes_an_earlier_small_match_on_a() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::create_dir(a.join("a-small")).unwrap();
    fs::write(a.join("a-small/item"), b"same").unwrap();
    fs::create_dir_all(a.join("z-large/child")).unwrap();
    fs::write(a.join("z-large/child/item"), b"same").unwrap();
    fs::write(a.join("z-large/other"), b"unique larger").unwrap();
    fs::create_dir_all(b.join("match/child")).unwrap();
    fs::write(b.join("match/child/item"), b"same").unwrap();
    fs::write(b.join("match/other"), b"unique larger").unwrap();
    let sa = acquire(&a, t.path(), 9091);
    let sb = acquire(&b, t.path(), 9092);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    let groups = j.folder_groups(None).unwrap();
    assert_eq!(groups.len(), 1, "{:?}", j.summary);
    assert_eq!(groups[0].a.path, a.join("z-large"));
    assert_eq!(groups[0].b.path, b.join("match"));
    assert_eq!(groups[0].files, 2);
    assert_eq!(j.summary.folder_files, 2);
    let covered: i64 =
        j.db.query_row("SELECT count(*) FROM folder_covered", [], |r| r.get(0))
            .unwrap();
    assert_eq!(covered, 4);
}
#[test]
fn late_proof_failures_keep_the_existing_error_ceiling_and_content_accounting() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for p in [&a, &b] {
        fs::write(p.join("first"), b"1").unwrap();
        fs::write(p.join("second"), b"22").unwrap();
    }
    let sa = acquire(&a, t.path(), 9101);
    let sb = acquire(&b, t.path(), 9102);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    let g = guards(&j);
    let cancel = CancellationToken::default();
    let g_refs = g.iter().map(Some).collect::<Vec<_>>();
    let mut native = NativeVerification {
        verifier: Verifier::new(Default::default(), &cancel),
        guards: &g_refs,
    };
    j.verify_with(&mut native, &cancel).unwrap();
    assert_eq!(j.summary.groups, 2);
    let bytes = j.summary.bytes_read;
    j.max_errors = 1;
    fs::write(b.join("first"), b"x").unwrap();
    fs::write(b.join("second"), b"yy").unwrap();
    super::verify(&mut j, &g_refs, &cancel).unwrap();
    assert_eq!(j.summary.errors, 1);
    assert_eq!(j.summary.folder_groups, 0);
    assert!(!j.summary.complete);
    assert_eq!(j.summary.bytes_read, bytes);
}

#[test]
fn review_reference_folder_partial_and_1240_offscreen_copies_are_reversible() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for root in [&a, &b] {
        fs::create_dir(root.join("empty")).unwrap();
        for n in 0u32..1240 {
            fs::write(root.join(format!("item-{n:04}")), n.to_le_bytes()).unwrap();
        }
    }
    let sa = acquire(&a, t.path(), 9201);
    let sb = acquire(&b, t.path(), 9202);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    let bytes = j.summary.bytes_read;
    let hashed = j.summary.files_hashed;
    let group = j.folder_groups(None).unwrap().remove(0);
    assert_eq!(
        j.review_summary().unwrap().reason,
        ReviewReason::ChooseReference
    );
    assert!(j.select_all_review_copies().is_err());
    j.set_protected_reference(Some(0)).unwrap();
    let preview = j.review_preview().unwrap();
    assert_eq!((preview.files, preview.logical_bytes), (0, 0));
    assert_eq!(
        (preview.eligible_files, preview.eligible_logical_bytes),
        (1240, 4960)
    );
    let a_target = j.review_row_target(&group.a).unwrap().unwrap();
    assert_eq!(
        j.review_state(a_target).unwrap().reason,
        ReviewReason::ProtectedReference
    );
    assert!(j.set_review_target(a_target, true).is_err());
    j.set_review_folder(group.id, true).unwrap();
    assert!(j.review_folder_state(group.id).unwrap().selected);
    let selected = j.review_summary().unwrap();
    assert_eq!(
        (selected.files, selected.logical_bytes, selected.folders),
        (1240, 4960, 1)
    );
    assert_eq!(selected.empty_directories, 1);
    let first = j.folder_page(group.id, None, false).unwrap();
    assert!(first.more);
    let child = first
        .rows
        .iter()
        .find(|row| row.b.kind == Kind::File)
        .unwrap();
    let child_target = j.review_row_target(&child.b).unwrap().unwrap();
    j.set_review_target(child_target, false).unwrap();
    let partial = j.review_folder_state(group.id).unwrap();
    assert!(!partial.selected && partial.partial);
    assert_eq!(j.review_summary().unwrap().files, 1239);
    assert_eq!(j.review_summary().unwrap().logical_bytes, 4956);
    let second = j.folder_page(group.id, first.last.as_ref(), false).unwrap();
    assert!(
        second
            .rows
            .iter()
            .filter(|r| r.b.kind == Kind::File)
            .all(|r| j
                .review_state(j.review_row_target(&r.b).unwrap().unwrap())
                .unwrap()
                .selected)
    );
    let again = j
        .folder_page(group.id, second.first.as_ref(), true)
        .unwrap();
    assert!(again.rows.iter().any(|r| r.b.key == child.b.key));
    assert!(!j.review_state(child_target).unwrap().selected);
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 1240);
    j.clear_review_selection().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 0);
    j.set_protected_reference(Some(1)).unwrap();
    assert_eq!(j.review_summary().unwrap().files, 0);
    assert_eq!(
        j.review_state(child_target).unwrap().reason,
        ReviewReason::ProtectedReference
    );
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 1240);
    assert!(j.review_state(a_target).unwrap().selected);
    assert_eq!(
        (j.summary.bytes_read, j.summary.files_hashed),
        (bytes, hashed)
    );
    let fresh = job(&qa, &qb, t.path());
    assert_eq!(
        fresh.review_summary().unwrap().reason,
        ReviewReason::ChooseReference
    );
    assert_eq!(fresh.review_summary().unwrap().files, 0);
}

#[test]
fn review_aliases_and_invalidated_or_partial_proofs_cannot_be_selected() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"copy").unwrap();
    fs::write(b.join("copy"), b"copy").unwrap();
    fs::hard_link(b.join("copy"), b.join("alias")).unwrap();
    let sa = acquire(&a, t.path(), 9211);
    let sb = acquire(&b, t.path(), 9212);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    j.set_protected_reference(Some(0)).unwrap();
    assert_eq!(j.review_preview().unwrap().eligible_files, 1);
    let alias: u64 =
        j.db.query_row(
            "SELECT id FROM candidates WHERE side=1 AND representative!=id",
            [],
            |r| number(r, 0),
        )
        .unwrap();
    assert_eq!(
        j.review_candidate_state(alias).unwrap().reason,
        ReviewReason::Alias
    );
    assert!(j.set_review_candidate(alias, true).is_err());
    j.select_all_review_copies().unwrap();
    assert_eq!(
        (
            j.review_summary().unwrap().files,
            j.review_summary().unwrap().logical_bytes
        ),
        (1, 4)
    );
    let independent: u64 =
        j.db.query_row(
            "SELECT id FROM candidates WHERE side=1 AND representative=id",
            [],
            |r| number(r, 0),
        )
        .unwrap();
    j.db.execute(
        "UPDATE candidates SET group_id=NULL WHERE id=?1",
        [unsigned(independent)],
    )
    .unwrap();
    assert!(!j.review_candidate_state(independent).unwrap().eligible);
    assert_eq!(j.review_summary().unwrap().files, 0);
    j.summary.complete = false;
    assert_eq!(
        j.review_summary().unwrap().reason,
        ReviewReason::IncompleteEvidence
    );
    assert!(j.select_all_review_copies().is_err());
}

#[test]
fn review_residual_folder_reference_and_empty_subtree_have_one_identity() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for root in [&a, &b] {
        fs::create_dir(root.join("matched")).unwrap();
        fs::create_dir(root.join("matched/empty")).unwrap();
        fs::write(root.join("matched/item"), b"common").unwrap();
    }
    fs::write(a.join("left-only"), b"left").unwrap();
    fs::write(b.join("extra-copy"), b"common").unwrap();
    let sa = acquire(&a, t.path(), 9221);
    let sb = acquire(&b, t.path(), 9222);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    let group = j.folder_groups(None).unwrap().remove(0);
    j.set_protected_reference(Some(0)).unwrap();
    let residual = j.file_page(None, false).unwrap();
    assert_eq!(residual.rows.len(), 1);
    assert_eq!(residual.folder_references[0], Some(group.id));
    let extra = j.review_row_target(&residual.rows[0]).unwrap().unwrap();
    j.set_review_target(extra, true).unwrap();
    j.set_review_folder(group.id, true).unwrap();
    assert_eq!(
        (
            j.review_summary().unwrap().files,
            j.review_summary().unwrap().logical_bytes
        ),
        (2, 12)
    );
    let detail = j.folder_page(group.id, None, false).unwrap();
    let empty = detail
        .rows
        .iter()
        .find(|r| r.b.kind == Kind::Directory)
        .unwrap();
    j.set_review_target(j.review_row_target(&empty.b).unwrap().unwrap(), false)
        .unwrap();
    assert!(j.review_folder_state(group.id).unwrap().partial);
    assert_eq!(
        (
            j.review_summary().unwrap().files,
            j.review_summary().unwrap().logical_bytes
        ),
        (2, 12)
    );
    j.set_review_folder(group.id, false).unwrap();
    assert_eq!(
        (
            j.review_summary().unwrap().files,
            j.review_summary().unwrap().logical_bytes
        ),
        (1, 6)
    );
    assert!(j.review_state(extra).unwrap().selected);
    // An invalidated file proof is not a newly discovered native alias. The
    // folder checkbox must give the corresponding unverified-evidence reason.
    let file = detail.rows.iter().find(|r| r.b.kind == Kind::File).unwrap();
    let candidate = j.review_candidate_for_row(&file.b).unwrap().unwrap();
    j.db.execute(
        "UPDATE candidates SET proof=NULL WHERE id=?1",
        [unsigned(candidate)],
    )
    .unwrap();
    assert_eq!(
        j.review_candidate_state(candidate).unwrap().reason,
        ReviewReason::Unverified
    );
    assert_eq!(
        j.review_folder_state(group.id).unwrap().reason,
        ReviewReason::Unverified
    );
    assert!(j.set_review_folder(group.id, true).is_err());
}

#[cfg(unix)]
#[test]
fn review_reference_alias_protects_native_identity_even_with_an_independent_third_location() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    fs::write(a.join("original"), b"content").unwrap();
    fs::hard_link(a.join("original"), b.join("alias-of-a")).unwrap();
    fs::write(b.join("independent"), b"content").unwrap();
    let sa = acquire(&a, t.path(), 9231);
    let sb = acquire(&b, t.path(), 9232);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let mut j = job(&qa, &qb, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    j.set_protected_reference(Some(0)).unwrap();
    assert_eq!(j.review_preview().unwrap().eligible_files, 1);
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 1);
    j.set_protected_reference(Some(1)).unwrap();
    let original: u64 =
        j.db.query_row("SELECT id FROM candidates WHERE side=0", [], |r| {
            number(r, 0)
        })
        .unwrap();
    assert_eq!(
        j.review_candidate_state(original).unwrap().reason,
        ReviewReason::Alias
    );
    assert_eq!(j.review_preview().unwrap().eligible_files, 0);
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 0);
}

fn three_job<'a, 's>(
    a: &'a Query<'s>,
    b: &'a Query<'s>,
    c: &'a Query<'s>,
    scratch: &Path,
) -> DuplicateJob<'a, 's> {
    prepare_scopes(
        [a, b, c]
            .into_iter()
            .enumerate()
            .map(|(side, query)| Scope {
                query,
                roots: vec![query.root.clone()],
                side: side as u8,
            })
            .collect(),
        Mode::BetweenThreeSources,
        scratch,
        DISK,
        &CancellationToken::default(),
    )
    .unwrap()
}
fn three_roots(t: &Path) -> [PathBuf; 3] {
    let paths = [
        t.join("alpha"),
        t.join("renamed-beta"),
        t.join("different-gamma"),
    ];
    for root in &paths {
        fs::create_dir(root).unwrap();
    }
    paths
}
fn tree(root: &Path, count: u32) {
    fs::create_dir(root.join("empty")).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    for n in 0..count {
        fs::write(
            root.join("nested").join(format!("item-{n:04}")),
            n.to_le_bytes(),
        )
        .unwrap();
    }
}

#[test]
fn three_exact_trees_have_one_group_three_locations_and_one_content_total() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    for root in [&a, &b, &c] {
        tree(root, 10);
    }
    let sa = acquire(&a, t.path(), 9301);
    let sb = acquire(&b, t.path(), 9302);
    let sc = acquire(&c, t.path(), 9303);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    assert_eq!(
        (
            j.summary.folder_groups,
            j.summary.folder_files,
            j.summary.folder_logical_bytes
        ),
        (1, 10, 40)
    );
    assert_eq!(j.file_group_count().unwrap(), 0);
    let group = j.folder_groups(None).unwrap().remove(0);
    assert_eq!(
        (
            &group.a.path,
            &group.b.path,
            group.c.as_ref().map(|r| &r.path)
        ),
        (&a, &b, Some(&c))
    );
    assert_eq!((group.a_side, group.b_side, group.c_side), (0, 1, Some(2)));
    let read = (j.summary.bytes_read, j.summary.files_hashed);
    let exact_id = j.folder_group(group.id).unwrap().unwrap();
    assert_eq!(
        exact_id.c.as_ref().unwrap().key,
        group.c.as_ref().unwrap().key
    );
    assert!(j.folder_group(group.id + 1).unwrap().is_none());
    assert!(j.folder_group(u64::MAX).unwrap().is_none());
    let detail = j.folder_page(group.id, None, false).unwrap();
    assert_eq!(
        detail
            .rows
            .iter()
            .filter(|r| r.a.kind == Kind::File)
            .count(),
        10
    );
    assert!(
        detail
            .rows
            .iter()
            .all(|r| r.c.is_some() && r.c_side == Some(2))
    );
    j.set_protected_reference(Some(0)).unwrap();
    assert_eq!(j.review_summary().unwrap().files, 0);
    assert_eq!(j.review_preview().unwrap().eligible_files, 20);
    j.select_all_review_copies().unwrap();
    assert_eq!(
        (
            j.review_summary().unwrap().files,
            j.review_summary().unwrap().logical_bytes
        ),
        (20, 80)
    );
    let b_target = j.review_row_target(&group.b).unwrap().unwrap();
    j.set_review_target(b_target, false).unwrap();
    assert_eq!(j.review_summary().unwrap().files, 10);
    j.set_protected_reference(Some(2)).unwrap();
    assert_eq!(j.review_summary().unwrap().files, 0);
    assert_eq!(
        j.review_state(
            j.review_row_target(group.c.as_ref().unwrap())
                .unwrap()
                .unwrap()
        )
        .unwrap()
        .reason,
        ReviewReason::ProtectedReference
    );
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 20);
    for root in [&a, &b, &c] {
        fs::rename(root, root.with_extension("offline")).unwrap();
    }
    assert_eq!(j.folder_groups(None).unwrap().len(), 1);
    assert_eq!(
        j.folder_page(group.id, None, false).unwrap().rows.len(),
        detail.rows.len()
    );
    j.clear_review_selection().unwrap();
    j.set_protected_reference(Some(1)).unwrap();
    assert_eq!(j.review_preview().unwrap().eligible_files, 20);
    assert_eq!((j.summary.bytes_read, j.summary.files_hashed), read);
}

#[test]
fn different_third_tree_keeps_complete_ab_without_a_false_three_copy_group() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for change in ["empty", "relative-name", "bytes"] {
        let t = tempfile::tempdir().unwrap();
        let [a, b, c] = three_roots(t.path());
        for root in [&a, &b, &c] {
            tree(root, 2);
        }
        match change {
            "empty" => fs::remove_dir(c.join("empty")).unwrap(),
            "relative-name" => {
                fs::rename(c.join("nested/item-0000"), c.join("nested/renamed")).unwrap()
            }
            _ => fs::write(c.join("nested/item-0000"), b"nope").unwrap(),
        }
        let sa = acquire(&a, t.path(), 9311);
        let sb = acquire(&b, t.path(), 9312);
        let sc = acquire(&c, t.path(), 9313);
        let qa = query(&sa, &a, t.path());
        let qb = query(&sb, &b, t.path());
        let qc = query(&sc, &c, t.path());
        let mut j = three_job(&qa, &qb, &qc, t.path());
        verify(&mut j);
        assert!(j.summary.complete, "{change}: {:?}", j.summary);
        let groups = j.folder_groups(None).unwrap();
        let parent = groups
            .iter()
            .find(|g| g.a.path == a && g.b.path == b)
            .unwrap();
        assert!(parent.c.is_none(), "{change}");
        assert!(groups.iter().all(|g| g.c.is_none()), "{change}");
        assert_eq!((parent.files, parent.logical_bytes), (2, 8));
    }
}

#[test]
fn unavailable_third_source_preserves_verified_ab_but_never_global_completeness() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    for root in [&a, &b, &c] {
        tree(root, 2);
    }
    let sa = acquire(&a, t.path(), 9321);
    let sb = acquire(&b, t.path(), 9322);
    let sc = acquire(&c, t.path(), 9323);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    fs::rename(&c, t.path().join("offline-c")).unwrap();
    let guards = [
        Some(Guard::open(&a, Policy::default()).unwrap()),
        Some(Guard::open(&b, Policy::default()).unwrap()),
        None,
    ];
    j.verify_available_scoped_with_progress(
        &guards,
        Default::default(),
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert!(!j.summary.complete, "{:?}", j.summary);
    let groups = j.folder_groups(None).unwrap();
    assert_eq!(groups.len(), 1, "{:?}", j.summary);
    assert!(groups[0].c.is_none());
    assert_eq!((&groups[0].a.path, &groups[0].b.path), (&a, &b));
    j.set_protected_reference(Some(0)).unwrap();
    assert_eq!(
        j.review_preview().unwrap().reason,
        ReviewReason::IncompleteEvidence
    );
    assert!(j.select_all_review_copies().is_err());
}

#[test]
fn only_bc_exact_is_a_bc_group_with_real_sides_and_no_reference_a_copy() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    fs::write(a.join("different"), b"nothing shared").unwrap();
    for root in [&b, &c] {
        tree(root, 2);
    }
    let sa = acquire(&a, t.path(), 9331);
    let sb = acquire(&b, t.path(), 9332);
    let sc = acquire(&c, t.path(), 9333);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    let group = j.folder_groups(None).unwrap().remove(0);
    assert_eq!((&group.a.path, &group.b.path), (&b, &c));
    assert_eq!((group.a_side, group.b_side, group.c_side), (1, 2, None));
    j.set_protected_reference(Some(0)).unwrap();
    let target = j.review_row_target(&group.a).unwrap().unwrap();
    assert!(!j.review_state(target).unwrap().eligible);
    assert!(j.set_review_target(target, true).is_err());
    assert_eq!(j.review_preview().unwrap().eligible_files, 0);
    assert!(j.select_all_review_copies().is_err());
    assert_eq!(j.review_summary().unwrap().files, 0);
    j.set_protected_reference(Some(1)).unwrap();
    j.select_all_review_copies().unwrap();
    assert_eq!(j.review_summary().unwrap().files, 2);
}

#[test]
fn known_root_alias_cannot_be_added_as_a_third_independent_source() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let (a, b) = roots(t.path());
    for root in [&a, &b] {
        tree(root, 2);
    }
    let sa = acquire(&a, t.path(), 9341);
    let sb = acquire(&b, t.path(), 9342);
    let sc = acquire(&b, t.path(), 9343);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &b, t.path());
    let result = prepare_scopes(
        [&qa, &qb, &qc]
            .into_iter()
            .enumerate()
            .map(|(side, query)| Scope {
                query,
                roots: vec![query.root.clone()],
                side: side as u8,
            })
            .collect(),
        Mode::BetweenThreeSources,
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

#[test]
fn hardlinked_third_tree_cannot_add_a_third_independent_folder_copy() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    for root in [&a, &b] {
        tree(root, 2);
    }
    fs::create_dir(c.join("empty")).unwrap();
    fs::create_dir(c.join("nested")).unwrap();
    for n in 0..2 {
        let relative = format!("nested/item-{n:04}");
        fs::hard_link(b.join(&relative), c.join(&relative)).unwrap();
    }
    let sa = acquire(&a, t.path(), 9351);
    let sb = acquire(&b, t.path(), 9352);
    let sc = acquire(&c, t.path(), 9353);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    verify(&mut j);
    let groups = j.folder_groups(None).unwrap();
    assert!(groups.iter().all(|g| g.c.is_none()), "{:?}", j.summary);
    assert_eq!(j.summary.folder_files, 2);
    assert!(j.summary.aliases + j.summary.ambiguous_aliases > 0);
    j.set_protected_reference(Some(0)).unwrap();
    let b_copy = groups
        .iter()
        .find(|g| g.a.path == a && g.b.path == b)
        .unwrap();
    let b_target = j.review_row_target(&b_copy.b).unwrap().unwrap();
    assert!(!j.review_state(b_target).unwrap().eligible);
    for child in j.folder_page(b_copy.id, None, false).unwrap().rows {
        if child.b.kind == Kind::File {
            let target = j.review_row_target(&child.b).unwrap().unwrap();
            assert!(!j.review_state(target).unwrap().eligible);
        }
    }
    let preview = j.review_preview().unwrap();
    assert_eq!(
        (
            preview.eligible_files,
            preview.eligible_folders,
            preview.eligible_empty_directories
        ),
        (0, 0, 0)
    );
    let _ = j.select_all_review_copies();
    assert_eq!(
        (
            j.review_summary().unwrap().files,
            j.review_summary().unwrap().empty_directories
        ),
        (0, 0)
    );
    let marks: i64 = j.db.query_row("SELECT (SELECT count(*) FROM review_selected)+(SELECT count(*) FROM review_empty_directories)", [], |r| r.get(0)).unwrap();
    assert_eq!(marks, 0);
}

#[test]
fn partial_c_keeps_residual_file_copy_linked_to_the_ab_folder_without_double_totals() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    for root in [&a, &b] {
        tree(root, 2);
    }
    fs::write(c.join("renamed-copy"), 0u32.to_le_bytes()).unwrap();
    let sa = acquire(&a, t.path(), 9361);
    let sb = acquire(&b, t.path(), 9362);
    let sc = acquire(&c, t.path(), 9363);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    let groups = j.folder_groups(None).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!((groups[0].files, j.summary.folder_files), (2, 2));
    assert!(groups[0].c.is_none());
    assert_eq!(j.file_group_count().unwrap(), 1);
    let page = j.file_page(None, false).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].path, c.join("renamed-copy"));
    assert_eq!(page.folder_references[0], Some(groups[0].id));
    j.set_protected_reference(Some(0)).unwrap();
    j.select_all_review_copies().unwrap();
    let selected = j.review_summary().unwrap();
    assert_eq!((selected.files, selected.logical_bytes), (3, 12));
}

#[test]
fn triple_folder_pages_keep_the_existing_two_hundred_rendered_row_budget_and_cursors() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    for root in [&a, &b, &c] {
        tree(root, 67);
    }
    let sa = acquire(&a, t.path(), 9371);
    let sb = acquire(&b, t.path(), 9372);
    let sc = acquire(&c, t.path(), 9373);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    verify(&mut j);
    let group = j.folder_groups(None).unwrap().remove(0);
    let read = (j.summary.bytes_read, j.summary.files_hashed);
    let first = j.folder_page(group.id, None, false).unwrap();
    assert_eq!(first.rows.len(), 65);
    assert!(first.more);
    assert!(first.rows.iter().all(|r| r.c.is_some()));
    let second = j.folder_page(group.id, first.last.as_ref(), false).unwrap();
    assert_eq!(second.rows.len(), 4);
    assert!(!second.more);
    let back = j
        .folder_page(group.id, second.first.as_ref(), true)
        .unwrap();
    assert_eq!(back.rows.len(), 65);
    assert_eq!(back.rows[0].a.key, first.rows[0].a.key);
    assert_eq!(first.rows.len() + second.rows.len(), 69);
    assert_eq!((j.summary.bytes_read, j.summary.files_hashed), read);
}

#[test]
fn reference_file_contents_do_not_certify_a_missing_reference_folder_copy() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let [a, b, c] = three_roots(t.path());
    for root in [&b, &c] {
        tree(root, 2);
    }
    // A has both exact byte strings, but neither the internal names nor the
    // empty/nested directory structure of the exact B/C folder group.
    for n in 0u32..2 {
        fs::write(a.join(format!("renamed-{n}")), n.to_le_bytes()).unwrap();
    }
    let sa = acquire(&a, t.path(), 9381);
    let sb = acquire(&b, t.path(), 9382);
    let sc = acquire(&c, t.path(), 9383);
    let qa = query(&sa, &a, t.path());
    let qb = query(&sb, &b, t.path());
    let qc = query(&sc, &c, t.path());
    let mut j = three_job(&qa, &qb, &qc, t.path());
    verify(&mut j);
    assert!(j.summary.complete, "{:?}", j.summary);
    let group = j.folder_groups(None).unwrap().remove(0);
    assert_eq!((group.a_side, group.b_side, group.c_side), (1, 2, None));
    assert_eq!(j.summary.groups, 2);
    j.set_protected_reference(Some(0)).unwrap();
    let b_target = j.review_row_target(&group.a).unwrap().unwrap();
    let c_target = j.review_row_target(&group.b).unwrap().unwrap();
    assert!(!j.review_state(b_target).unwrap().eligible);
    assert!(!j.review_state(c_target).unwrap().eligible);
    assert_eq!(j.review_preview().unwrap().eligible_files, 0);
    assert!(j.select_all_review_copies().is_err());
    assert_eq!(j.review_summary().unwrap().files, 0);
}
