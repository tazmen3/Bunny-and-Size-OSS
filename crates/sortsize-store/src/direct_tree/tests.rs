use super::*;
use crate::inventory::{Observation, differential::NativeEvidence};
use sortsize_core::LocalFileId;

fn native(label: &str) -> NativeName {
    #[cfg(windows)]
    return NativeName::Windows(label.encode_utf16().collect());
    #[cfg(not(windows))]
    return NativeName::Posix(label.as_bytes().to_vec());
}

fn file(name: NativeName, logical: u64) -> Observation {
    Observation {
        name,
        kind: Kind::File,
        logical: Some(logical),
        allocated: Some(logical * 2),
        modified: Some((-2, 13)),
        metadata_known: true,
    }
}

fn dir(label: &str) -> Observation {
    Observation {
        name: native(label),
        kind: Kind::Directory,
        logical: None,
        allocated: None,
        modified: None,
        metadata_known: true,
    }
}

fn batch<'a>(
    parent: u64,
    sequence: u64,
    end: Option<ListingEnd>,
    observations: &'a [Observation],
    evidence: &'a [Option<NativeEvidence>],
    properties: &'a [NodeProperties],
) -> ObservationBatch<'a> {
    ObservationBatch {
        parent: EntryId(parent),
        sequence,
        end,
        observations,
        evidence,
        properties,
        directory: None,
        errors: 0,
        denied: 0,
        detached: false,
    }
}

#[test]
fn progressive_tree_and_replay_preserve_occurrences() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    let mut tree = DirectTree::new(root.clone(), scratch.path(), 1024 * 1024).unwrap();
    let property = NodeProperties {
        allocated: MetadataValue::Known {
            value: 12,
            accuracy: ValueAccuracy::Exact,
        },
        ..Default::default()
    };
    let hardlink = Some(NativeEvidence {
        identity: Some(LocalFileId::WindowsLegacy {
            volume: 7,
            index: 9,
        }),
        created: Some((-3, 11)),
    });
    let children = tree
        .accept(batch(
            0,
            0,
            Some(ListingEnd::Exhausted),
            &[dir("child"), file(native("premier"), 6)],
            &[None, hardlink.clone()],
            &[NodeProperties::default(), property.clone()],
        ))
        .unwrap();
    assert_eq!(children[0].id, EntryId(1));
    assert_eq!(tree.summary().pending_directories, 1);
    assert_eq!(tree.summary().coverage, ScopeCoverage::Partial);
    assert_eq!(
        tree.children(EntryId(0), None, 16, true)
            .unwrap()
            .rows
            .len(),
        1
    );
    assert_eq!(tree.path(EntryId(1)).unwrap(), root.join("child"));
    assert!(tree.replay().is_err());

    tree.accept(batch(
        1,
        0,
        Some(ListingEnd::Exhausted),
        &[file(native("alias"), 6)],
        &[hardlink],
        &[property],
    ))
    .unwrap();
    assert_eq!(
        tree.measure(EntryId(0)).unwrap(),
        FolderMeasure {
            logical: Some(12),
            allocated: Some(24),
            files: 2,
            directories: 1,
            ..Default::default()
        }
    );
    assert_eq!(tree.row(EntryId(1)).unwrap().logical, Some(6));
    assert_eq!(tree.summary().coverage, ScopeCoverage::Complete);
    assert_eq!(tree.parent(EntryId(3)).unwrap(), Some(EntryId(1)));
    assert_eq!(
        tree.top()
            .unwrap()
            .into_iter()
            .map(|row| row.id.0)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let first = tree.children(EntryId(0), None, 1, false).unwrap();
    assert_eq!(first.next, Some(EntryId(1)));
    let second = tree.children(EntryId(0), first.next, 1, false).unwrap();
    assert_eq!(second.rows[0].path, root.join("premier"));
    assert!(!second.more);
    assert_eq!(
        tree.previous_children_cursor(EntryId(0), EntryId(2), 1, false)
            .unwrap(),
        None
    );

    tree.finish().unwrap();
    let batches = tree.replay().unwrap().collect::<Result<Vec<_>>>().unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].children, vec![(EntryId(1), native("child"))]);
    assert_eq!(
        batches[0].evidence[1].as_ref().unwrap().identity,
        batches[1].evidence[0].as_ref().unwrap().identity
    );
    assert_eq!(batches[0].observations[1].modified, Some((-2, 13)));
    assert_eq!(batches[0].properties[1], batches[1].properties[0]);
}

#[test]
fn global_query_orders_native_children_across_parents_and_pages() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    let mut tree = DirectTree::new(root.clone(), scratch.path(), 1024 * 1024).unwrap();
    tree.accept(batch(
        0,
        0,
        Some(ListingEnd::Exhausted),
        &[dir("zeta"), dir("alpha")],
        &[None, None],
        &[NodeProperties::default(), NodeProperties::default()],
    ))
    .unwrap();
    #[cfg(windows)]
    let invalid_text = NativeName::Windows(vec![0xd800, b'.'.into(), b't'.into()]);
    #[cfg(not(windows))]
    let invalid_text = NativeName::Posix(vec![0xff, b'.', b't']);
    tree.accept(batch(
        1,
        0,
        Some(ListingEnd::Exhausted),
        &[file(native("échantillon.txt"), 12), file(invalid_text, 7)],
        &[None, None],
        &[NodeProperties::default(), NodeProperties::default()],
    ))
    .unwrap();
    tree.accept(batch(
        2,
        0,
        Some(ListingEnd::Exhausted),
        &[file(native("échantillon.txt"), 20)],
        &[None],
        &[NodeProperties::default()],
    ))
    .unwrap();
    tree.finish().unwrap();

    let criteria = sortsize_core::analysis::SearchQuery {
        kind: Some(sortsize_core::InventoryKind::File),
        name: Some("ÉCHANTILLON".into()),
        glob: Some("**/*.txt".into()),
        ..Default::default()
    };
    let cancel = sortsize_core::CancellationToken::default();
    for (sort, expected) in [(2, ["alpha", "zeta"]), (3, ["zeta", "alpha"])] {
        let mut cursor = None;
        for directory in expected {
            let mut query =
                query::PageQuery::new(&tree, None, Some(&criteria), sort, cursor, 1).unwrap();
            while !query.advance(&tree, &cancel).unwrap() {}
            let rows = query.rows(&tree).unwrap();
            assert_eq!(query.files, 2);
            assert_eq!(query.logical, 32);
            assert_eq!(query.gaps, 1);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].path, root.join(directory).join("échantillon.txt"));
            cursor = query.next();
        }
        assert!(cursor.is_none());
    }
}

#[test]
fn duplicated_names_and_detach_remain_partial_in_replay() {
    let scratch = tempfile::tempdir().unwrap();
    let mut tree = DirectTree::new(scratch.path().to_owned(), scratch.path(), 1024 * 1024).unwrap();
    tree.accept(batch(
        0,
        0,
        None,
        &[file(native("fixed"), 2), dir("replaced")],
        &[None, None],
        &[NodeProperties::default(), NodeProperties::default()],
    ))
    .unwrap();
    let mut detached = batch(2, 0, Some(ListingEnd::Incomplete), &[], &[], &[]);
    detached.detached = true;
    detached.errors = 1;
    detached.denied = 1;
    tree.accept(detached).unwrap();
    tree.accept(batch(
        0,
        1,
        Some(ListingEnd::Exhausted),
        &[file(native("fixed"), 99)],
        &[None],
        &[NodeProperties::default()],
    ))
    .unwrap();

    tree.finish().unwrap();
    let summary = tree.summary();
    assert_eq!(summary.coverage, ScopeCoverage::Partial);
    assert_eq!(summary.entries, 2);
    assert_eq!(summary.observations, 3);
    assert_eq!(summary.detached_directories, 1);
    assert_eq!(summary.duplicate_observations, 1);
    assert_eq!(summary.denied, 1);
    assert_eq!(summary.pending_directories, 0);
    assert_eq!(summary.measure.logical, Some(2));
    let batches = tree.replay().unwrap().collect::<Result<Vec<_>>>().unwrap();
    assert_eq!(batches[2].end, Some(ListingEnd::Incomplete));
    assert!(batches[2].observations.is_empty());
}

#[test]
fn spool_quota_rejects_the_whole_batch() {
    let scratch = tempfile::tempdir().unwrap();
    let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut tree = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        128,
        governor.clone(),
    )
    .unwrap();
    tree.accept(batch(
        0,
        0,
        None,
        &[file(native("ok"), 1)],
        &[None],
        &[NodeProperties::default()],
    ))
    .unwrap();
    let accepted = tree.summary().temporary_bytes;
    let usage = governor.usage();
    let _filled = governor
        .reserve(
            Category::Cache,
            usage.limits.categories[0] - usage.categories[0],
            None,
        )
        .unwrap();
    assert!(
        tree.accept(batch(
            0,
            1,
            Some(ListingEnd::Exhausted),
            &[file(NativeName::Opaque(vec![0; 256]), 3)],
            &[None],
            &[NodeProperties::default()],
        ))
        .is_err()
    );
    assert_eq!(tree.summary().temporary_bytes, accepted);
    assert_eq!(tree.summary().entries, 1);
    assert_eq!(tree.row(EntryId(1)).unwrap().logical, Some(1));
    assert!(tree.finish().is_err());
    assert!(tree.replay().is_err());
}

#[test]
fn opaque_keys_survive_reads_from_buffer_and_file() {
    let scratch = tempfile::tempdir().unwrap();
    let mut tree =
        DirectTree::new(scratch.path().to_owned(), scratch.path(), 2 * 1024 * 1024).unwrap();
    for i in 0..256u64 {
        let mut bytes = vec![0xff; 2048];
        bytes[..8].copy_from_slice(&i.to_le_bytes());
        let name = NativeName::Opaque(bytes);
        tree.accept(batch(
            0,
            i,
            None,
            &[file(name.clone(), 1)],
            &[None],
            &[NodeProperties::default()],
        ))
        .unwrap();
        assert_eq!(tree.name(EntryId(i + 1)).unwrap(), name);
    }
    assert_eq!(
        tree.name(EntryId(1)).unwrap(),
        NativeName::Opaque([vec![0; 8], vec![0xff; 2040]].concat())
    );
    let mut last = batch(0, 256, Some(ListingEnd::Incomplete), &[], &[], &[]);
    last.denied = 1;
    tree.accept(last).unwrap();
    tree.finish().unwrap();
    assert_eq!(tree.summary().coverage, ScopeCoverage::Partial);
    assert_eq!(
        tree.replay()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .len(),
        257
    );
}

#[test]
fn unknown_sizes_and_allocation_lower_bounds_are_explicit() {
    let scratch = tempfile::tempdir().unwrap();
    let mut tree = DirectTree::new(scratch.path().to_owned(), scratch.path(), 1024 * 1024).unwrap();
    let unknown = Observation {
        name: native("refused"),
        kind: Kind::Unknown,
        logical: None,
        allocated: None,
        modified: None,
        metadata_known: false,
    };
    tree.accept(batch(
        0,
        0,
        None,
        &[unknown],
        &[None],
        &[NodeProperties {
            allocated: MetadataValue::AccessDenied,
            ..Default::default()
        }],
    ))
    .unwrap();
    assert_eq!(tree.summary().measure.allocated, None);
    assert_eq!(tree.summary().measure.unknown, 1);
    assert_eq!(tree.row(EntryId(1)).unwrap().logical, None);
    let mut observed = file(native("sparse"), 16);
    observed.allocated = Some(3);
    tree.accept(batch(
        0,
        1,
        Some(ListingEnd::Exhausted),
        &[observed],
        &[None],
        &[NodeProperties {
            allocated: MetadataValue::Known {
                value: 3,
                accuracy: ValueAccuracy::LowerBound,
            },
            ..Default::default()
        }],
    ))
    .unwrap();
    tree.finish().unwrap();
    let summary = tree.summary();
    assert_eq!(summary.coverage, ScopeCoverage::Partial);
    assert_eq!(summary.measure.logical, Some(16));
    assert_eq!(summary.measure.allocated, Some(3));
    assert_eq!(summary.measure.allocation_unknown, 1);
    assert!(summary.measure.allocation_lower_bound);
    let replay = tree.replay().unwrap().collect::<Result<Vec<_>>>().unwrap();
    assert_eq!(
        replay[0].properties[0].allocated,
        MetadataValue::AccessDenied
    );
    assert_eq!(
        replay[1].properties[0].allocated,
        MetadataValue::Known {
            value: 3,
            accuracy: ValueAccuracy::LowerBound
        }
    );
}

#[test]
fn ram_inventory_does_not_write_records_to_temporary_disk() {
    let scratch = tempfile::tempdir().unwrap();
    let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut tree = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        0,
        governor.clone(),
    )
    .unwrap();
    tree.accept(batch(
        0,
        0,
        Some(ListingEnd::Exhausted),
        &[file(native("mémoire-🐇"), 7)],
        &[None],
        &[NodeProperties::default()],
    ))
    .unwrap();
    tree.finish().unwrap();
    assert_eq!(tree.summary().temporary_bytes, 0);
    assert_eq!(
        tree.replay()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .observations
            .len(),
        1
    );
    assert_eq!(tree.row(EntryId(1)).unwrap().logical, Some(7));
    assert!(governor.usage().categories[0] > 0);
    drop(tree);
    assert_eq!(governor.usage().categories, [0; 6]);
    assert_eq!(governor.usage().disk_used, 0);
}

#[test]
fn forced_spill_keeps_large_name_index_arenas_and_many_chunk_growths_exact() {
    let scratch = tempfile::tempdir().unwrap();
    let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut tree = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        64 * 1024 * 1024,
        governor.clone(),
    )
    .unwrap();
    let usage = governor.usage();
    let filled = governor
        .reserve(
            Category::Cache,
            usage.limits.categories[0] - usage.categories[0],
            None,
        )
        .unwrap();
    let count = 20_000u64;
    for sequence in 0..count / 8 {
        let observations: Vec<_> = (sequence * 8..sequence * 8 + 8)
            .map(|i| file(native(&format!("fichier-{i:06}")), i))
            .collect();
        tree.accept(batch(
            0,
            sequence,
            None,
            &observations,
            &vec![None; 8],
            &vec![NodeProperties::default(); 8],
        ))
        .unwrap();
    }
    // Duplicate detection after spilling must preserve the first occurrence.
    tree.accept(batch(
        0,
        count / 8,
        Some(ListingEnd::Exhausted),
        &[file(native("fichier-000000"), 999)],
        &[None],
        &[NodeProperties::default()],
    ))
    .unwrap();
    tree.finish().unwrap();
    let summary = tree.summary();
    assert_eq!(summary.entries, count);
    assert_eq!(summary.duplicate_observations, 1);
    assert_eq!(summary.measure.logical, Some(count * (count - 1) / 2));
    assert_eq!(summary.coverage, ScopeCoverage::Partial);
    assert!(summary.temporary_bytes > 0);
    assert_eq!(tree.row(EntryId(1)).unwrap().logical, Some(0));
    assert_eq!(tree.row(EntryId(count)).unwrap().logical, Some(count - 1));
    assert_eq!(
        tree.children(EntryId(0), Some(EntryId(8190)), 8, false)
            .unwrap()
            .rows
            .len(),
        8
    );
    let replay = tree.replay().unwrap();
    let reconstructed_governor =
        Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut restored = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        64 * 1024 * 1024,
        reconstructed_governor,
    )
    .unwrap();
    for batch in replay {
        restored.accept_replay(batch.unwrap()).unwrap();
    }
    restored.finish().unwrap();
    restored.restore_diagnostics(summary).unwrap();
    assert_eq!(restored.summary().measure, summary.measure);
    assert_eq!(restored.summary().duplicate_observations, 1);
    drop(filled);
    drop(tree);
    assert_eq!(governor.usage().categories, [0; 6]);
    assert_eq!(governor.usage().disk_used, 0);
}

#[test]
fn forced_directory_arena_overflow_preserves_links_and_subtotals() {
    let scratch = tempfile::tempdir().unwrap();
    let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut tree = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        64 * 1024 * 1024,
        governor.clone(),
    )
    .unwrap();
    let usage = governor.usage();
    let _filled = governor
        .reserve(
            Category::Cache,
            usage.limits.categories[0] - usage.categories[0],
            None,
        )
        .unwrap();
    let count = 8_256;
    for sequence in 0..count / 64 {
        let observations: Vec<_> = (sequence * 64..sequence * 64 + 64)
            .map(|i| dir(&format!("d-{i:06}")))
            .collect();
        let end = (sequence + 1 == count / 64).then_some(ListingEnd::Exhausted);
        tree.accept(batch(
            0,
            sequence,
            end,
            &observations,
            &vec![None; 64],
            &vec![NodeProperties::default(); 64],
        ))
        .unwrap();
    }
    for id in 1..=count {
        tree.accept(batch(id, 0, Some(ListingEnd::Exhausted), &[], &[], &[]))
            .unwrap();
    }
    tree.finish().unwrap();
    assert_eq!(tree.summary().coverage, ScopeCoverage::Complete);
    assert_eq!(tree.summary().measure.directories, count);
    assert_eq!(tree.parent(EntryId(count)).unwrap(), Some(EntryId(0)));
    assert_eq!(
        tree.children(EntryId(0), Some(EntryId(count - 2)), 8, true)
            .unwrap()
            .rows
            .len(),
        2
    );
    assert_eq!(tree.measure(EntryId(count)).unwrap().files, 0);
}

#[test]
fn spilled_name_split_quota_failure_keeps_previous_partial_view_coherent() {
    let scratch = tempfile::tempdir().unwrap();
    let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut tree = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        8 * 1024 * 1024,
        governor.clone(),
    )
    .unwrap();
    let usage = governor.usage();
    let _filled = governor
        .reserve(
            Category::Cache,
            usage.limits.categories[0] - usage.categories[0],
            None,
        )
        .unwrap();
    for sequence in 0..64 {
        let rows: Vec<_> = (sequence * 64..sequence * 64 + 64)
            .map(|i| file(native(&format!("split-{i}")), 1))
            .collect();
        tree.accept(batch(
            0,
            sequence,
            None,
            &rows,
            &vec![None; 64],
            &vec![NodeProperties::default(); 64],
        ))
        .unwrap();
    }
    let before = tree.summary();
    assert_eq!(before.entries, 4096);
    let observation = file(native("trigger-split"), 999);
    let property = NodeProperties::default();
    let mut encoded = spool::Encoder(vec![0; 4]);
    encoded.header(EntryId(0), 64, None, None, 1).unwrap();
    encoded
        .record(
            4097,
            &observation.name.encode(),
            &observation,
            None,
            &property,
        )
        .unwrap();
    // Accept the observation record and hash record, then reject the extra head
    // slot of the first split after some tree links have already been modified.
    tree.disk
        .cap_remaining_for_test(encoded.0.len() as u64 + 20);
    assert!(
        tree.accept(batch(0, 64, None, &[observation], &[None], &[property]))
            .is_err()
    );
    assert_eq!(tree.summary().entries, before.entries);
    assert_eq!(tree.summary().measure, before.measure);
    assert_eq!(tree.summary().coverage, ScopeCoverage::Partial);
    assert_eq!(tree.summary().errors, before.errors + 1);
    assert!(
        tree.children(EntryId(0), Some(EntryId(4096)), 8, false)
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(tree.row(EntryId(4096)).unwrap().logical, Some(1));
    assert!(tree.row(EntryId(4097)).is_err());
    assert!(tree.top().unwrap().iter().all(|row| row.logical == Some(1)));
    assert!(tree.finish().is_err());
    assert!(tree.replay().is_err());
}

#[test]
fn ram_name_prefix_remains_exact_after_new_hashes_spill_without_migration() {
    let scratch = tempfile::tempdir().unwrap();
    let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
    let mut tree = DirectTree::with_governor(
        scratch.path().to_owned(),
        scratch.path(),
        8 * 1024 * 1024,
        governor.clone(),
    )
    .unwrap();
    tree.accept(batch(
        0,
        0,
        None,
        &[file(native("préfixe"), 7)],
        &[None],
        &[NodeProperties::default()],
    ))
    .unwrap();
    let usage = governor.usage();
    let _filled = governor
        .reserve(
            Category::Cache,
            usage.limits.categories[0] - usage.categories[0],
            None,
        )
        .unwrap();
    for sequence in 1..=64 {
        let rows: Vec<_> = (sequence * 64..sequence * 64 + 64)
            .map(|i| file(native(&format!("nouveau-{i}")), 1))
            .collect();
        tree.accept(batch(
            0,
            sequence,
            None,
            &rows,
            &vec![None; 64],
            &vec![NodeProperties::default(); 64],
        ))
        .unwrap();
    }
    let seen = tree.seen.get(&0).unwrap();
    assert!(seen.disk.is_some());
    assert!(!seen.heads.is_empty());
    tree.accept(batch(
        0,
        65,
        Some(ListingEnd::Exhausted),
        &[
            file(native("préfixe"), 999),
            file(native("nouveau-64"), 999),
        ],
        &[None, None],
        &[NodeProperties::default(), NodeProperties::default()],
    ))
    .unwrap();
    tree.finish().unwrap();
    assert_eq!(tree.summary().duplicate_observations, 2);
    assert_eq!(tree.summary().entries, 4097);
    assert_eq!(tree.summary().measure.logical, Some(4103));
    assert_eq!(tree.row(EntryId(1)).unwrap().logical, Some(7));
}
