use super::*;
use crate::inventory::tests::{
    attempt_id, batch, context_id, fold, observation as raw_observation, reconcile,
};
fn observation(i: u64, kind: Kind) -> Observation {
    let mut o = raw_observation(i, kind);
    o.name = super::super::queries::native_path(Path::new(&format!("n-{i:08}")));
    o
}
fn root() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/offline/no-source-io")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\offline.invalid\share\no-source-io")
    }
}
fn setup() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &t.path().join("store"),
        InventoryId(u128::MAX),
        SourceId(9),
        Durability::Retained,
    )
    .unwrap();
    s.enable_evidence().unwrap();
    (t, s)
}
fn finish(s: &mut Store, g: u64) {
    fold(s, g);
    s.seal(context_id(g)).unwrap();
    s.record_measurement(GenerationId(g), &root(), g as i64, g as i64 + 1)
        .unwrap();
}
fn proof(id: u64, created: i64) -> Option<NativeEvidence> {
    Some(NativeEvidence {
        identity: Some(LocalFileId::Unix {
            device: 1,
            inode: id,
        }),
        created: Some((created, 0)),
    })
}
fn generation(
    s: &mut Store,
    g: u64,
    complete: bool,
    rows: &[Observation],
    evidence: &[Option<NativeEvidence>],
) {
    s.create_generation_guarded(context_id(g), Some(b"qualified-root-proof"))
        .unwrap();
    let a = attempt_id(g, 0, 1);
    s.begin_attempt(a).unwrap();
    s.ingest_evidenced(
        BatchHeader {
            attempt: a,
            sequence: 0,
            end: Some(if complete {
                ListingEnd::Exhausted
            } else {
                ListingEnd::Incomplete
            }),
        },
        rows,
        evidence,
    )
    .unwrap();
    if complete {
        reconcile(s, a);
    }
    finish(s, g);
}
fn run<'a>(s: &'a Store, t: &Path) -> Diff<'a> {
    let mut d = Diff::start(
        s.lease(GenerationId(0)).unwrap(),
        s.lease(GenerationId(1)).unwrap(),
        t,
        32 * 1024 * 1024,
    )
    .unwrap();
    while !d.advance(&CancellationToken::default()).unwrap() {}
    d
}
fn all(d: &Diff<'_>) -> Vec<Event> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let p = d.page(cursor.as_ref(), false).unwrap();
        out.extend(p.rows);
        if !p.more {
            break;
        }
        cursor = p.next;
    }
    out
}
#[test]
fn partial_100gib_to_40gib_has_two_totals_and_no_certain_minus_60() {
    let (t, mut s) = setup();
    let mut a = observation(1, Kind::File);
    a.logical = Some(100 * 1024 * 1024 * 1024);
    let mut b = a.clone();
    b.logical = Some(40 * 1024 * 1024 * 1024);
    generation(&mut s, 0, true, &[a, observation(2, Kind::File)], &[]);
    generation(&mut s, 1, false, &[b], &[]);
    let d = run(&s, t.path());
    assert_eq!(d.summary.old_logical, Some(100 * 1024 * 1024 * 1024 + 2));
    assert_eq!(d.summary.new_logical, Some(40 * 1024 * 1024 * 1024));
    assert_eq!(d.summary.comparable_delta, None);
    assert!(
        d.root_folder_changes(t.path(), &CancellationToken::default())
            .unwrap()
            .is_empty()
    );
    let rows = all(&d);
    assert_eq!(rows[0].change, Change::FieldsChanged);
    assert_eq!(rows[1].change, Change::Unverified);
}
#[test]
fn identity_reuse_unique_rename_case_and_hardlink_sets_are_conservative() {
    let (t, mut s) = setup();
    let make = |name: &str, size| Observation {
        name: super::super::queries::native_path(Path::new(name)),
        logical: Some(size),
        ..observation(1, Kind::File)
    };
    generation(
        &mut s,
        0,
        true,
        &[
            make("old", 7),
            make("hard-a", 8),
            make("hard-b", 8),
            make("same", 3),
            make("CASE", 6),
        ],
        &[
            proof(7, 1),
            proof(8, 1),
            proof(8, 1),
            proof(3, 1),
            proof(6, 1),
        ],
    );
    generation(
        &mut s,
        1,
        true,
        &[
            make("new", 7),
            make("hard-c", 8),
            make("hard-b", 8),
            make("same", 3),
            make("case", 6),
        ],
        &[
            proof(7, 1),
            proof(8, 1),
            proof(8, 1),
            proof(3, 2),
            proof(6, 1),
        ],
    );
    let d = run(&s, t.path());
    assert_eq!(d.summary.counts[Change::ProbableMove as usize], 2);
    assert_eq!(d.summary.counts[Change::Replaced as usize], 1);
    assert_eq!(d.summary.counts[Change::Removed as usize], 1);
    assert_eq!(d.summary.counts[Change::Appeared as usize], 1);
    assert_eq!(d.summary.comparable_delta, Some("0".into()));
    let rows = all(&d);
    assert_eq!(rows.len(), 6);
    assert_eq!(
        rows.iter()
            .filter(|e| e.change == Change::ProbableMove)
            .count(),
        2
    );
}
#[test]
fn eof_of_parent_does_not_prove_absence_in_unread_descendant() {
    let (t, mut s) = setup();
    for g in 0..2 {
        s.create_generation_guarded(context_id(g), Some(b"qualified-root-proof"))
            .unwrap();
        let a = attempt_id(g, 0, 1);
        s.begin_attempt(a).unwrap();
        batch(
            &mut s,
            a,
            0,
            Some(ListingEnd::Exhausted),
            &[observation(1, Kind::Directory)],
        )
        .unwrap();
        reconcile(&mut s, a);
        if g == 0 {
            let a = attempt_id(g, 1, 1);
            s.begin_attempt(a).unwrap();
            batch(
                &mut s,
                a,
                0,
                Some(ListingEnd::Exhausted),
                &[observation(2, Kind::File)],
            )
            .unwrap();
            reconcile(&mut s, a);
        }
        finish(&mut s, g);
    }
    let d = run(&s, t.path());
    let rows = all(&d);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|x| x.change != Change::Removed));
    assert_eq!(d.summary.comparable_delta, None);
}
#[test]
fn pagination_has_no_omission_and_reverse_returns_same_page() {
    let (t, mut s) = setup();
    let rows: Vec<_> = (0..501).map(|i| observation(i, Kind::File)).collect();
    generation(&mut s, 0, true, &rows, &[]);
    let reversed: Vec<_> = rows.into_iter().rev().collect();
    generation(&mut s, 1, true, &reversed, &[]);
    let d = run(&s, t.path());
    assert_eq!(all(&d).len(), 501);
    let p = d.page(None, false).unwrap();
    assert_eq!(p.rows.len(), 200);
    let n = d.page(p.next.as_ref(), false).unwrap();
    assert_eq!(n.rows.len(), 200);
    let back = d.page(n.first.as_ref(), true).unwrap();
    assert_eq!(
        p.rows.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        back.rows.iter().map(|e| e.sequence).collect::<Vec<_>>()
    );
    let foreign = Cursor {
        owner: PathBuf::from("other"),
        sequence: 1,
        priority: None,
    };
    assert!(d.page(Some(&foreign), false).is_err());
}

#[test]
fn display_order_prioritizes_changes_across_pages_without_renumbering_events() {
    let (t, mut s) = setup();
    let before: Vec<_> = (0..430).map(|i| observation(i, Kind::File)).collect();
    let mut after = before.clone();
    for row in &mut after[220..] {
        row.logical = Some(9_000);
    }
    generation(&mut s, 0, true, &before, &[]);
    generation(&mut s, 1, true, &after, &[]);
    let mut d = run(&s, t.path());
    let ordinary = all(&d);
    assert_eq!(ordinary[0].change, Change::FieldsUnchanged);
    let old_cursor = d.page(None, false).unwrap().next.unwrap();
    d.prioritize_changes(&CancellationToken::default()).unwrap();
    assert!(d.page(Some(&old_cursor), false).is_err());

    let displayed = all(&d);
    let expected: Vec<_> = ordinary[220..]
        .iter()
        .chain(&ordinary[..220])
        .map(|e| e.sequence)
        .collect();
    assert_eq!(
        displayed.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(displayed[0].change, Change::FieldsChanged);
    let first = d.page(None, false).unwrap();
    let second = d.page(first.next.as_ref(), false).unwrap();
    let back = d.page(second.first.as_ref(), true).unwrap();
    assert!(!back.more);
    assert_eq!(
        back.rows.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        expected[..200]
    );
}
#[test]
fn uncertain_root_unknown_metadata_and_cancellation_do_not_invent_absence() {
    let (t, mut s) = setup();
    let rows = [observation(1, Kind::File)];
    generation(&mut s, 0, true, &rows, &[]);
    generation(&mut s, 1, true, &[], &[]);
    s.db.execute(
        "UPDATE recovery_guard SET proof=x'01' WHERE generation=?1",
        [unsigned(1)],
    )
    .unwrap();
    let d = run(&s, t.path());
    assert!(!d.summary.compatible);
    assert_eq!(all(&d)[0].change, Change::Unverified);
    assert!(
        d.root_folder_changes(t.path(), &CancellationToken::default())
            .unwrap()
            .is_empty()
    );
    drop(d);
    let cancel = CancellationToken::default();
    cancel.cancel();
    let mut d = Diff::start(
        s.lease(GenerationId(0)).unwrap(),
        s.lease(GenerationId(1)).unwrap(),
        t.path(),
        1024 * 1024,
    )
    .unwrap();
    assert!(d.advance(&cancel).unwrap());
    assert!(d.summary.cancelled);
    assert!(!d.summary.finished);
    assert!(d.page(None, false).is_err());
}

#[test]
fn recorded_root_rankings_match_native_names_across_generations_in_one_store() {
    let (t, mut s) = setup();
    let named = |label, kind, logical| Observation {
        name: super::super::queries::native_path(Path::new(label)),
        logical,
        ..observation(1, kind)
    };
    for (g, folders, direct_file) in [
        (
            0,
            [("Images", 80), ("Données-été", 30), ("Archives", 50)],
            5,
        ),
        (
            1,
            [("Données-été", 110), ("Export", 40), ("Images", 20)],
            130,
        ),
    ] {
        s.create_generation_guarded(context_id(g), Some(b"qualified-root-proof"))
            .unwrap();
        let root = attempt_id(g, 0, 1);
        s.begin_attempt(root).unwrap();
        let mut children = folders
            .iter()
            .map(|(label, _)| named(*label, Kind::Directory, None))
            .collect::<Vec<_>>();
        children.push(named("racine.bin", Kind::File, Some(direct_file)));
        batch(&mut s, root, 0, Some(ListingEnd::Exhausted), &children).unwrap();
        reconcile(&mut s, root);
        for (index, (_, size)) in folders.into_iter().enumerate() {
            let folder = attempt_id(g, index as u64 + 1, 1);
            s.begin_attempt(folder).unwrap();
            batch(
                &mut s,
                folder,
                0,
                Some(ListingEnd::Exhausted),
                &[named("contenu.bin", Kind::File, Some(size))],
            )
            .unwrap();
            reconcile(&mut s, folder);
        }
        finish(&mut s, g);
    }
    let saved = s.saved(GenerationId(0)).unwrap();
    let mut other = s.saved(GenerationId(1)).unwrap();
    assert!(saved.same_scope(&other));
    other.continuity = None;
    assert!(!saved.same_scope(&other));
    assert!(!other.same_scope(&other));

    let cancel = CancellationToken::default();
    let new = s.lease(GenerationId(1)).unwrap();
    let largest = largest_root_entries(&new, t.path(), &cancel).unwrap();
    assert_eq!(
        largest
            .iter()
            .map(|row| (row.name.to_str().unwrap(), row.is_folder, row.logical))
            .collect::<Vec<_>>(),
        [
            ("racine.bin", false, 130),
            ("Données-été", true, 110),
            ("Export", true, 40),
            ("Images", true, 20),
        ]
    );
    let d = run(&s, t.path());
    assert_eq!(d.summary().comparable_delta.as_deref(), Some("135"));
    for _ in 0..2 {
        let changes = d.root_folder_changes(t.path(), &cancel).unwrap();
        assert_eq!(
            changes
                .iter()
                .map(|row| (row.name.to_str().unwrap(), row.delta))
                .collect::<Vec<_>>(),
            [
                ("Données-été", 80),
                ("Export", 40),
                ("Images", -60),
                ("Archives", -50),
            ]
        );
    }
    cancel.cancel();
    assert!(d.root_folder_changes(t.path(), &cancel).is_err());
}

#[test]
#[ignore = "synthetic scale qualification; BUNNY_DIFF_SCALE=1000000 or 10000000"]
fn synthetic_scale_diff_under_budget() {
    let n = std::env::var("BUNNY_DIFF_SCALE")
        .unwrap_or_else(|_| "1000000".into())
        .parse::<u64>()
        .unwrap();
    assert!((1000..=10_000_000).contains(&n));
    let (t, mut s) = setup();
    let build = std::time::Instant::now();
    for g in 0..2 {
        s.create_generation_guarded(context_id(g), Some(b"synthetic compatible root"))
            .unwrap();
        let a = attempt_id(g, 0, 1);
        s.begin_attempt(a).unwrap();
        for lo in (0..n).step_by(10000) {
            let tx = s.db.transaction().unwrap();
            {
                let mut q=tx.prepare("INSERT INTO entries(generation,id,parent,name,kind,logical,modified_s,modified_ns,attempt,metadata_known) VALUES(?1,?2,?3,?4,1,?5,0,0,?6,1)").unwrap();
                for i in lo..(lo + 10000).min(n) {
                    q.execute(params![
                        unsigned(g),
                        unsigned(i + 1),
                        unsigned(0),
                        super::super::queries::native_path(Path::new(&format!("n-{i:08}")))
                            .encode(),
                        unsigned(i + u64::from(g == 1 && i == n - 1)),
                        unsigned(1)
                    ])
                    .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        s.db.execute(
            "UPDATE tasks SET listing=1,reconciled=1 WHERE generation=?1",
            [unsigned(g)],
        )
        .unwrap();
        s.db.execute(
            "UPDATE generations SET aggregate_done=1,aggregate_sum=?2 WHERE id=?1",
            params![unsigned(g), unsigned(n * (n - 1) / 2 + g)],
        )
        .unwrap();
        s.seal(context_id(g)).unwrap();
        s.record_measurement(GenerationId(g), &root(), g as i64, g as i64 + 1)
            .unwrap();
    }
    let build_s = build.elapsed().as_secs_f64();
    let started = std::time::Instant::now();
    let result_budget = std::env::var("BUNNY_DIFF_RESULT_BYTES")
        .unwrap_or_else(|_| "1073741824".into())
        .parse::<u64>()
        .unwrap();
    eprintln!(
        "admission: result cap={result_budget} source_artifacts={} free_disk={:?}",
        super::super::lifecycle::artifact_bytes(s.directory()).unwrap(),
        sortsize_core::resources::native_free_disk(s.directory())
    );
    let mut d = Diff::start(
        s.lease(GenerationId(0)).unwrap(),
        s.lease(GenerationId(1)).unwrap(),
        t.path(),
        result_budget,
    )
    .unwrap();
    let mut quanta = 0;
    while !d.advance(&CancellationToken::default()).unwrap() {
        quanta += 1;
    }
    let seconds = started.elapsed().as_secs_f64();
    assert_eq!(d.summary.visited, n);
    assert_eq!(d.summary.counts[4], 1);
    assert_eq!(d.summary.counts[5], n - 1);
    assert_eq!(d.summary.comparable_delta, Some("1".into()));
    let p = d.page(None, false).unwrap();
    assert_eq!(p.rows.len(), 200);
    drop(p);
    let last = d
        .page(
            Some(&Cursor {
                owner: d.owner.clone(),
                sequence: n - 1,
                priority: None,
            }),
            false,
        )
        .unwrap();
    assert_eq!(last.rows.len(), 1);
    assert_eq!(last.rows[0].change, Change::FieldsChanged);
    let bytes = std::fs::metadata(&d.owner).unwrap().len();
    eprintln!(
        "{}",
        serde_json::json!({"kind":"synthetic_metadata_no_source_io","entries_per_generation":n,"build_seconds":build_s,"diff_seconds":seconds,"quanta":quanta,"working_reservation_bytes":d._memory.bytes(),"result_db_bytes":bytes,"summary":d.summary})
    );
    if std::env::var_os("BUNNY_DIFF_CACHE_OPEN_PROBE").is_some() {
        drop(last);
        drop(d);
        s.set_retention(true, 30).unwrap();
        let path = s.directory().to_owned();
        drop(s);
        let started = std::time::Instant::now();
        let inspected = Store::inspect(&path).unwrap();
        assert_eq!(inspected.measurements().unwrap().len(), 2);
        let inspect_s = started.elapsed().as_secs_f64();
        drop(inspected);
        let started = std::time::Instant::now();
        let opened = Store::reopen(&path, Durability::Retained).unwrap();
        assert_eq!(opened.measurements().unwrap().len(), 2);
        eprintln!(
            "{}",
            serde_json::json!({"cache_open_probe":true,"entries_per_generation":n,"catalogue_inspect_seconds":inspect_s,"full_integrity_open_seconds":started.elapsed().as_secs_f64(),"source_io":false})
        );
    }
}

#[test]
fn byte_limited_name_batches_do_not_turn_unread_keys_into_absences() {
    let (t, mut s) = setup();
    let rows: Vec<_> = (0..50)
        .map(|i| {
            let mut o = observation(i, Kind::File);
            o.name = super::super::queries::native_path(Path::new(&format!(
                "{i:04}-{}",
                "x".repeat(8000)
            )));
            o
        })
        .collect();
    generation(&mut s, 0, true, &rows, &[]);
    let reversed: Vec<_> = rows.into_iter().rev().collect();
    generation(&mut s, 1, true, &reversed, &[]);
    let d = run(&s, t.path());
    assert_eq!(d.summary.counts[Change::FieldsUnchanged as usize], 50);
    assert_eq!(d.summary.counts[Change::Removed as usize], 0);
    assert_eq!(d.summary.counts[Change::Appeared as usize], 0);
    assert_eq!(all(&d).len(), 50);
}

#[test]
fn windows_legacy_evidence_survives_sqlite_without_aliasing_128_bit_ids() {
    let (t, mut store) = setup();
    let legacy = NativeEvidence {
        identity: Some(LocalFileId::WindowsLegacy {
            volume: 7,
            index: 9,
        }),
        created: Some((42, 0)),
    };
    let modern = NativeEvidence {
        identity: Some(LocalFileId::Windows {
            volume_serial: 7,
            file_id: 9_u128.to_le_bytes(),
        }),
        created: legacy.created,
    };
    assert_ne!(legacy.encode(), modern.encode());
    let row = observation(1, Kind::File);
    generation(
        &mut store,
        0,
        true,
        std::slice::from_ref(&row),
        &[Some(legacy.clone())],
    );
    let persisted: Vec<u8> = store
        .db
        .query_row(
            "SELECT proof FROM entry_evidence WHERE generation=?1",
            params![unsigned(0)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(persisted, legacy.encode());
    generation(&mut store, 1, true, &[row], &[Some(modern)]);
    let diff = run(&store, t.path());
    assert_eq!(diff.summary.counts[Change::Replaced as usize], 1);
    assert_eq!(diff.summary.counts[Change::FieldsUnchanged as usize], 0);
    let unknown = NativeEvidence {
        identity: Some(LocalFileId::WindowsLegacy {
            volume: 7,
            index: 0,
        }),
        created: legacy.created,
    };
    assert!(unknown.encode().is_empty());
}
