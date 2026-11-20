//! Regression coverage for the V0.5 Workshop metadata and global sibling order.
use super::tests::{attempt_id, batch, context_id, fold, observation, reconcile};
use super::*;
use sortsize_core::{
    MetadataValue as M, NativeOwnerId, ValueAccuracy as A, acquisition::NodeProperties,
};
use std::time::{Duration, UNIX_EPOCH};
fn exact<T>(value: T) -> M<T> {
    M::Known {
        value,
        accuracy: A::Exact,
    }
}
fn setup() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &t.path().join("db"),
        context_id(0).inventory,
        context_id(0).source,
        Durability::Retained,
    )
    .unwrap();
    s.enable_properties().unwrap();
    s.create_generation(context_id(0)).unwrap();
    s.set_collection_options(context_id(0), true, true).unwrap();
    s.begin_attempt(attempt_id(0, 0, 1)).unwrap();
    (t, s)
}
fn header(seq: u64, end: Option<ListingEnd>) -> BatchHeader {
    BatchHeader {
        attempt: attempt_id(0, 0, 1),
        sequence: seq,
        end,
    }
}
fn key(id: u64) -> EntryKey {
    EntryKey {
        inventory: context_id(0).inventory,
        generation: GenerationId(0),
        entry: EntryId(id),
    }
}
#[test]
fn properties_are_atomic_replay_fenced_immutable_and_preserve_unknowns() {
    let (t, mut s) = setup();
    let mut o = observation(1, Kind::File);
    o.allocated = Some(0);
    let p = NodeProperties {
        allocated: exact(0),
        accessed: exact(UNIX_EPOCH - Duration::new(2, 1)),
        owner: sortsize_core::EntryOwner {
            id: exact(NativeOwnerId::UnixUid(0)),
        },
    };
    s.db.execute_batch("CREATE TEMP TRIGGER fail_properties BEFORE INSERT ON entry_properties BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        s.ingest_properties(header(0, None), &[o.clone()], &[], std::slice::from_ref(&p))
            .is_err()
    );
    assert_eq!(s.entry_count(GenerationId(0)).unwrap(), 1);
    s.db.execute_batch("DROP TRIGGER fail_properties;").unwrap();
    s.ingest_properties(header(0, None), &[o.clone()], &[], std::slice::from_ref(&p))
        .unwrap();
    assert_eq!(s.entry_properties(key(1)).unwrap(), p);
    assert_eq!(
        s.ingest_properties(header(0, None), &vec![o.clone(); 513], &[], &[])
            .unwrap(),
        Ack::Replay
    );
    assert!(
        s.set_collection_options(context_id(0), false, true)
            .is_err()
    );
    let states = [M::NotCollected, M::Unknown, M::Unsupported, M::AccessDenied];
    for (seq, state) in states.into_iter().enumerate() {
        let mut o = observation(seq as u64 + 2, Kind::File);
        o.allocated = None;
        let p = NodeProperties {
            allocated: state,
            accessed: M::NotCollected,
            owner: sortsize_core::EntryOwner {
                id: M::NotCollected,
            },
        };
        s.ingest_properties(
            header(seq as u64 + 1, None),
            &[o],
            &[],
            std::slice::from_ref(&p),
        )
        .unwrap();
        assert_eq!(s.entry_properties(key(seq as u64 + 2)).unwrap(), p);
    }
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        5,
        Some(ListingEnd::Exhausted),
        &[],
    )
    .unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    assert!(
        s.db.execute("UPDATE entry_properties SET allocated_state=2", [])
            .is_err()
    );
    assert!(s.db.execute("DELETE FROM collection_options", []).is_err());
    drop(s);
    let s = Store::reopen(&t.path().join("db"), Durability::Retained).unwrap();
    assert_eq!(s.entry_properties(key(1)).unwrap(), p);
    assert_eq!(
        s.collection_options(GenerationId(0)).unwrap(),
        Some((true, true))
    );
    let mut f = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    let m = s
        .folder_measure(GenerationId(0), EntryId(0), &f)
        .unwrap()
        .unwrap();
    assert_eq!(m.allocated, None);
    assert_eq!(m.allocation_unknown, 4);
}
#[test]
fn globally_ordered_siblings_keep_all_pages_unknowns_last_and_native_ties() {
    let (t, mut s) = setup();
    for chunk in 0..2 {
        let mut obs = Vec::new();
        let mut props = Vec::new();
        for i in chunk * 220u64..(chunk + 1) * 220u64 {
            let mut o = observation(i, Kind::File);
            o.name = queries::native_path(Path::new(&format!("name-{:04}", 439 - i)));
            o.allocated = (!i.is_multiple_of(11)).then_some(i);
            o.logical = (!i.is_multiple_of(13)).then_some(if i == 439 { u64::MAX } else { i });
            o.modified = if i % 7 == 0 {
                None
            } else {
                Some((i as i64 - 220, 3))
            };
            props.push(NodeProperties {
                allocated: o.allocated.map_or(M::Unknown, exact),
                accessed: if i % 5 == 0 {
                    M::AccessDenied
                } else {
                    exact(UNIX_EPOCH + Duration::from_secs(i))
                },
                owner: sortsize_core::EntryOwner {
                    id: if i % 3 == 0 {
                        M::NotCollected
                    } else {
                        exact(NativeOwnerId::UnixUid(i as u32))
                    },
                },
            });
            obs.push(o);
        }
        s.ingest_properties(header(chunk, None), &obs, &[], &props)
            .unwrap();
    }
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        2,
        Some(ListingEnd::Exhausted),
        &[],
    )
    .unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    let mut f = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    // Native SQLite VM work must depend on the returned page, not on the
    // already visited prefix. Wall-clock timing is deliberately not asserted.
    let vm_steps = |after: u64| {
        let mut q = s.db.prepare(ordered::SIBLINGS).unwrap();
        let mut r = q
            .query(params![unsigned(0), unsigned(0), unsigned(after)])
            .unwrap();
        let mut count = 0;
        while r.next().unwrap().is_some() {
            count += 1;
        }
        drop(r);
        assert_eq!(count, 256);
        q.get_status(rusqlite::StatementStatus::VmStep)
    };
    let first = vm_steps(0);
    let last = vm_steps(184);
    assert!(
        last <= first + 200,
        "prefix scan returned: first={first},last={last}"
    );
    let mut q =
        s.db.prepare(&format!("EXPLAIN QUERY PLAN {}", ordered::SIBLINGS))
            .unwrap();
    let plans = q
        .query_map(params![unsigned(0), unsigned(0), unsigned(184)], |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    eprintln!(
        "SQLite {} sibling range: {:?}; VM first={first},last={last}",
        s.db.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))
            .unwrap(),
        plans
    );
    drop(q);
    let all = (0..440).collect::<Vec<u64>>();
    for index in 0..12 {
        let mut expected = all.clone();
        expected.sort_by(|a, b| {
            let present = |i: u64| match index {
                0 | 1 => !i.is_multiple_of(13),
                4 | 5 => !i.is_multiple_of(11),
                6 | 7 => !i.is_multiple_of(3),
                8 | 9 => !i.is_multiple_of(7),
                10 | 11 => !i.is_multiple_of(5),
                _ => true,
            };
            let value = |i: u64| {
                if matches!(index, 0 | 1) && i == 439 {
                    u64::MAX
                } else {
                    i
                }
            };
            let value_order = if index == 2 || index == 3 {
                let cmp = (439 - *a).cmp(&(439 - *b));
                if index == 3 { cmp.reverse() } else { cmp }
            } else {
                let cmp = value(*a).cmp(&value(*b));
                if matches!(index, 0 | 4 | 7 | 8 | 10) {
                    cmp.reverse()
                } else {
                    cmp
                }
            };
            (!present(*a))
                .cmp(&(!present(*b)))
                .then(if present(*a) && present(*b) {
                    value_order
                } else {
                    std::cmp::Ordering::Equal
                })
                .then_with(|| (439 - *a).cmp(&(439 - *b)))
        });
        let mut order = s
            .start_ordered_folder(
                context_id(0),
                EntryId(0),
                t.path(),
                ordered::Sort::from_index(index).unwrap(),
                t.path(),
                8 * 1024 * 1024,
            )
            .unwrap();
        assert!(order.page(None).is_err());
        while !order.step(&s, &f).unwrap() {}
        let located = order.locate(EntryId(439)).unwrap().unwrap();
        assert_eq!(located.rows[0].key.entry, EntryId(439));
        let first = order.page(None).unwrap();
        let second = order.page(first.next.as_ref()).unwrap();
        let back = order.previous(second.first.as_ref().unwrap()).unwrap();
        assert_eq!(
            first.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
            back.rows.iter().map(|r| r.key).collect::<Vec<_>>()
        );
        let mut found = Vec::new();
        let mut page = first;
        loop {
            found.extend(page.rows.iter().map(|r| r.key.entry.0 - 1));
            if !page.more {
                break;
            }
            page = order.page(page.next.as_ref()).unwrap();
        }
        assert_eq!(found, expected, "sort {index}");
        drop(order);
        if matches!(index, 0 | 1) {
            let cancel = sortsize_core::CancellationToken::default();
            let q = s
                .query(
                    GenerationId(0),
                    t.path(),
                    &sortsize_core::analysis::SearchQuery {
                        kind: Some(sortsize_core::InventoryKind::File),
                        ..Default::default()
                    },
                    t.path(),
                    8 * 1024 * 1024,
                    &cancel,
                )
                .unwrap();
            let mut sorted = q
                .order_by_size(index == 0, t.path(), 8 * 1024 * 1024)
                .unwrap();
            assert!(sorted.page(None, false).is_err());
            while !sorted.advance(&cancel).unwrap() {}
            let mut p = sorted.page(None, false).unwrap();
            let second = sorted.page(p.next.as_ref(), false).unwrap();
            assert_eq!(
                p.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
                sorted
                    .page(second.first.as_ref(), true)
                    .unwrap()
                    .rows
                    .iter()
                    .map(|r| r.key)
                    .collect::<Vec<_>>()
            );
            assert!(
                sorted
                    .page(q.page(None).unwrap().first.as_ref(), false)
                    .is_err()
            );
            let mut found = Vec::new();
            loop {
                found.extend(p.rows.iter().map(|r| r.key.entry.0 - 1));
                if !p.more {
                    break;
                }
                p = sorted.page(p.next.as_ref(), false).unwrap();
            }
            assert_eq!(found, expected);
        }
    }
}
#[test]
fn catalogue_inspection_cannot_certify_query_or_projection() {
    let (t, mut s) = setup();
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        0,
        Some(ListingEnd::Exhausted),
        &[observation(1, Kind::File)],
    )
    .unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    drop(s);
    let s = Store::inspect(&t.path().join("db")).unwrap();
    assert!(
        s.start_query(
            GenerationId(0),
            t.path(),
            &Default::default(),
            t.path(),
            1024 * 1024
        )
        .is_err()
    );
    assert!(s.top100(GenerationId(0), t.path()).is_err());
}

#[test]
fn owner_name_snapshot_is_bounded_frozen_and_native_ids_break_ties() {
    let (t, mut s) = setup();
    let rows = (0..4)
        .map(|i| {
            let mut o = observation(i, Kind::File);
            o.name = queries::native_path(Path::new(&format!("n-{i:08}")));
            o
        })
        .collect::<Vec<_>>();
    let props = (0..4)
        .map(|i| NodeProperties {
            owner: sortsize_core::EntryOwner {
                id: if i == 3 {
                    M::AccessDenied
                } else {
                    exact(NativeOwnerId::UnixUid(i as u32))
                },
            },
            ..Default::default()
        })
        .collect::<Vec<_>>();
    s.ingest_properties(header(0, Some(ListingEnd::Exhausted)), &rows, &[], &props)
        .unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    let mut f = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    for (sort, expected) in [
        (ordered::Sort::OwnerUp, vec![2, 1, 3, 4]),
        (ordered::Sort::OwnerDown, vec![1, 2, 3, 4]),
    ] {
        let mut order = s
            .start_ordered_folder(
                context_id(0),
                EntryId(0),
                t.path(),
                sort,
                t.path(),
                1024 * 1024,
            )
            .unwrap();
        order
            .set_owner_labels(&[
                ordered::OwnerLabel {
                    owner: NativeOwnerId::UnixUid(0),
                    label: "z".into(),
                },
                ordered::OwnerLabel {
                    owner: NativeOwnerId::UnixUid(1),
                    label: "A".into(),
                },
            ])
            .unwrap();
        while !order.step(&s, &f).unwrap() {}
        assert_eq!(
            order
                .page(None)
                .unwrap()
                .rows
                .iter()
                .map(|r| r.key.entry.0)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(order.set_owner_labels(&[]).is_err());
    }
}

#[test]
fn denied_observation_clears_old_values_and_aggregate_does_not_become_zero() {
    let (_t, mut s) = setup();
    let mut o = observation(1, Kind::File);
    o.allocated = Some(4096);
    let mut p = NodeProperties {
        allocated: M::Known {
            value: 4096,
            accuracy: A::LowerBound,
        },
        ..Default::default()
    };
    s.ingest_properties(header(0, None), &[o.clone()], &[], std::slice::from_ref(&p))
        .unwrap();
    let mut f = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    let m = s
        .folder_measure(GenerationId(0), EntryId(0), &f)
        .unwrap()
        .unwrap();
    assert_eq!(m.allocated, Some(4096));
    assert!(m.allocation_lower_bound);
    o.metadata_known = false;
    o.allocated = None;
    assert!(
        s.ingest_properties(header(1, None), &[o.clone()], &[], std::slice::from_ref(&p))
            .is_err()
    );
    p.allocated = M::AccessDenied;
    p.accessed = M::AccessDenied;
    p.owner.id = M::AccessDenied;
    s.ingest_properties(
        header(1, Some(ListingEnd::Exhausted)),
        &[o],
        &[],
        std::slice::from_ref(&p),
    )
    .unwrap();
    assert_eq!(s.entry_properties(key(1)).unwrap(), p);
    assert_eq!(s.entry_modified(key(1)).unwrap(), M::Unknown);
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    let m = s
        .folder_measure(GenerationId(0), EntryId(0), &f)
        .unwrap()
        .unwrap();
    assert_eq!(m.logical, None);
    assert_eq!(m.allocated, None);
    assert_eq!(m.allocation_unknown, 1);
}

#[test]
fn long_recorded_paths_fit_ordered_publications_and_continue_without_loss() {
    let (t, mut s) = setup();
    let mut seq = 0;
    for first in (0..240).step_by(24) {
        let rows = (first..first + 24)
            .map(|i| {
                let mut o = observation(i, Kind::File);
                o.name = queries::native_path(Path::new(&format!("f-{i:04}-{}", "x".repeat(4096))));
                #[cfg(windows)]
                {
                    let mut units = format!("f-{i:04}-").encode_utf16().collect::<Vec<_>>();
                    units.extend(std::iter::repeat_n(0xd800, 4096));
                    o.name = NativeName::Windows(units);
                }

                o
            })
            .collect::<Vec<_>>();
        batch(&mut s, attempt_id(0, 0, 1), seq, None, &rows).unwrap();
        seq += 1;
    }
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        seq,
        Some(ListingEnd::Exhausted),
        &[],
    )
    .unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    let mut f = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    let revision = s.revision(GenerationId(0)).unwrap();
    {
        let mut limited = s
            .start_ordered_folder(
                context_id(0),
                EntryId(0),
                t.path(),
                ordered::Sort::NameUp,
                t.path(),
                64 * 1024,
            )
            .unwrap();
        let error = loop {
            match limited.step(&s, &f) {
                Ok(false) => {}
                Ok(true) => panic!("fixture unexpectedly fits quota"),
                Err(e) => break e,
            }
        };
        assert!(matches!(error, Error::Sql(_)));
        assert!(limited.page(None).is_err());
    }
    assert_eq!(s.revision(GenerationId(0)).unwrap(), revision);
    assert_eq!(s.entry_count(GenerationId(0)).unwrap(), 241);
    let mut order = s
        .start_ordered_folder(
            context_id(0),
            EntryId(0),
            t.path(),
            ordered::Sort::NameUp,
            t.path(),
            32 * 1024 * 1024,
        )
        .unwrap();
    while !order.step(&s, &f).unwrap() {}
    let c = context_id(0);
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
    let mut cursor = None;
    let mut n = 0;
    loop {
        let p = order.page(cursor.as_ref()).unwrap();
        assert!(!p.rows.is_empty());
        let more = p.more;
        cursor = p.next.clone();
        let frame = s.ordered_navigation_frame(c, view, p).unwrap();
        assert!(frame.accounted_bytes <= 512 * 1024);
        if more {
            assert!(frame.byte_limited);
            assert!(frame.rows.len() < 200)
        }
        for r in &frame.rows {
            assert_eq!(r.key.entry.0, n + 1);
            n += 1;
        }
        if !more {
            break;
        }
    }
    assert_eq!(n, 240);
}
#[test]
fn cached_parent_path_keeps_nested_sibling_scope_and_order() {
    let (t, mut s) = setup();
    let mut directory = observation(1, Kind::Directory);
    directory.name = queries::native_path(Path::new("nested"));
    batch(
        &mut s,
        attempt_id(0, 0, 1),
        0,
        Some(ListingEnd::Exhausted),
        &[directory],
    )
    .unwrap();
    let child = attempt_id(0, 1, 1);
    s.begin_attempt(child).unwrap();
    let rows = [15, 3, 8]
        .into_iter()
        .map(|n| {
            let mut row = observation(n, Kind::File);
            row.name = queries::native_path(Path::new(&format!("file-{n:02}")));
            row
        })
        .collect::<Vec<_>>();
    batch(&mut s, child, 0, Some(ListingEnd::Exhausted), &rows).unwrap();
    reconcile(&mut s, attempt_id(0, 0, 1));
    reconcile(&mut s, child);
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    let mut f = folder_totals::FolderFold::default();
    while !s.folder_fold_step(GenerationId(0), &mut f).unwrap() {}
    for (sort, names) in [
        (ordered::Sort::NameUp, ["file-03", "file-08", "file-15"]),
        (ordered::Sort::SizeDown, ["file-15", "file-08", "file-03"]),
    ] {
        let mut order = s
            .start_ordered_folder(
                context_id(0),
                EntryId(1),
                t.path(),
                sort,
                t.path(),
                32 * 1024 * 1024,
            )
            .unwrap();
        while !order.step(&s, &f).unwrap() {}
        let page = order.page(None).unwrap();
        assert!(!page.more);
        assert_eq!(page.rows.len(), 3);
        for (row, name) in page.rows.iter().zip(names) {
            assert_eq!(row.path, t.path().join("nested").join(name));
        }
    }
}
