use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::*};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{Kind, Observation, Store},
};
use std::io::{BufRead, BufReader};
fn name(s: &str) -> NativeName {
    #[cfg(unix)]
    {
        NativeName::Posix(s.as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        NativeName::Windows(s.encode_utf16().collect())
    }
}
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(909),
        source: SourceId(909),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn fixture(n: u64, extreme: bool) -> (tempfile::TempDir, Store) {
    fixture_named(n, extreme, |i| name(&format!("file-{i:08}.extension-{i}")))
}
fn fixture_named(
    n: u64,
    extreme: bool,
    names: impl Fn(u64) -> NativeName,
) -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let c = context();
    let mut s = Store::create(
        &t.path().join("store"),
        c.inventory,
        c.source,
        Durability::Reconstructible,
    )
    .unwrap();
    s.create_generation(c).unwrap();
    let a = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    s.begin_attempt(a).unwrap();
    for (sequence, start) in (0..n).step_by(512).enumerate() {
        let end = (start + 512).min(n);
        let rows = (start..end)
            .map(|i| Observation {
                name: names(i),
                kind: Kind::File,
                logical: if extreme {
                    if i == 0 { None } else { Some(u64::MAX) }
                } else {
                    Some(i)
                },
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        s.ingest(
            BatchHeader {
                attempt: a,
                sequence: sequence as u64,
                end: if end == n {
                    Some(ListingEnd::Exhausted)
                } else {
                    None
                },
            },
            &rows,
        )
        .unwrap();
    }
    while !s
        .reconcile_step(a, ComparisonScope::Comparable, 512)
        .unwrap()
    {}
    while !s.consolidate_step(c, 512).unwrap() {}
    s.seal(c).unwrap();
    (t, s)
}
#[test]
fn all_matches_beyond_fifty_thousand_export_and_group_without_source_io_or_overwrite() {
    let (t, s) = fixture(50_010, false);
    let root = t.path().join("offline-never-created");
    let cancel = CancellationToken::default();
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            128 * 1024 * 1024,
            &cancel,
        )
        .unwrap();
    assert_eq!(q.counts.matches, 50_011);
    let stats = q.statistics(t.path(), 32 * 1024 * 1024, &cancel).unwrap();
    assert!(stats.complete);
    assert_eq!(stats.totals.files, 50_010);
    assert_eq!(stats.totals.known_bytes, 50_010_u128 * 50_009 / 2);
    assert_eq!(stats.extensions.len(), 32);
    assert_eq!(stats.extensions[0].counts.known_bytes, 50_009);
    assert_eq!(stats.extensions[31].counts.known_bytes, 49_978);
    assert_eq!(stats.others.files, 50_010 - 32);
    assert_eq!(
        stats.others.known_bytes
            + stats
                .extensions
                .iter()
                .map(|e| e.counts.known_bytes)
                .sum::<u128>(),
        stats.totals.known_bytes
    );
    assert_eq!(
        stats.size_classes.iter().map(|c| c.files).sum::<u64>(),
        50_010
    );
    drop(stats);
    // Exhaust the actual groups database, not only the API minimum check.
    assert!(q.statistics(t.path(), 64 * 1024, &cancel).is_err());
    assert_eq!(q.counts.matches, 50_011);
    let out = t.path().join("inventory.jsonl");
    let result = q.export_jsonl(&out, 32 * 1024 * 1024, &cancel).unwrap();
    assert!(result.complete);
    assert_eq!(result.records, 50_011);
    let mut entries = 0;
    let mut previous = String::new();
    let mut trailer = false;
    for line in BufReader::new(std::fs::File::open(&out).unwrap()).lines() {
        let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        assert!(!trailer);
        match value["record"].as_str().unwrap() {
            "entry" => {
                entries += 1;
                let path = value["path"]["display"].as_str().unwrap();
                assert!(path > previous.as_str());
                previous = path.into();
            }
            "end" => {
                trailer = true;
                assert_eq!(value["coverage"], "complete");
                assert_eq!(value["records"], "50011");
            }
            "header" => assert_eq!(value["generation"], "1"),
            _ => panic!(),
        }
    }
    assert!(trailer);
    assert_eq!(entries, 50_011);
    assert!(!root.exists());
    let before = std::fs::read(&out).unwrap();
    assert!(q.export_jsonl(&out, 4096, &cancel).is_err());
    assert_eq!(std::fs::read(&out).unwrap(), before);
    let partial = t.path().join("partial.jsonl");
    let result = q.export_jsonl(&partial, 4096, &cancel).unwrap();
    assert!(result.quota && !result.complete);
    assert!(std::fs::metadata(&partial).unwrap().len() <= 4096);
    let tail = std::fs::read_to_string(&partial).unwrap();
    let tail: serde_json::Value = serde_json::from_str(tail.lines().last().unwrap()).unwrap();
    assert_eq!(tail["coverage"], "partial");
    assert_eq!(tail["quota"], true);
    cancel.cancel();
    let out = t.path().join("cancelled.jsonl");
    let result = q.export_jsonl(&out, 4096, &cancel).unwrap();
    assert!(result.cancelled && !result.complete);
    assert_eq!(result.records, 0);
    let stats = q.statistics(t.path(), 1024 * 1024, &cancel).unwrap();
    assert!(stats.cancelled && !stats.complete);
    assert_eq!(stats.totals.files, 0);
}
#[test]
fn unknown_sizes_and_extreme_totals_remain_exact() {
    let (t, s) = fixture(3, true);
    let cancel = CancellationToken::default();
    let root = t.path().join("offline");
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            1024 * 1024,
            &cancel,
        )
        .unwrap();
    let stats = q.statistics(t.path(), 1024 * 1024, &cancel).unwrap();
    assert_eq!(stats.totals.known_bytes, 2 * u128::from(u64::MAX));
    assert_eq!(stats.totals.unknown_sizes, 1);
    assert_eq!(stats.size_classes[5].files, 1);
    assert_eq!(
        stats.json()["totals"]["known_bytes"],
        "36893488147419103230"
    );
    assert!(q.statistics(t.path(), 1, &cancel).is_err());
    assert!(
        q.export_jsonl(&t.path().join("too-small"), 1, &cancel)
            .is_err()
    );
    assert!(!t.path().join("too-small").exists());
}

#[test]
fn native_names_roundtrip_and_oversized_extension_projection_keeps_others_exact() {
    let (t, s) = fixture_named(2, false, |i| {
        if i == 0 {
            return name("line\n=not-a-formula.txt");
        }
        #[cfg(unix)]
        {
            NativeName::Posix(b"native.\xff".to_vec())
        }
        #[cfg(windows)]
        {
            NativeName::Windows(vec![110, 97, 116, 105, 118, 101, 46, 0xd800])
        }
    });
    let cancel = CancellationToken::default();
    let root = t.path().join("offline");
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            1024 * 1024,
            &cancel,
        )
        .unwrap();
    let stats = q.statistics(t.path(), 1024 * 1024, &cancel).unwrap();
    assert_eq!(stats.extensions.len(), 2);
    assert!(stats.extensions.iter().any(|e| e.extension.raw.is_some()));
    drop(stats);
    let out = t.path().join("native.jsonl");
    assert!(q.export_jsonl(&out, 1024 * 1024, &cancel).unwrap().complete);
    let rows = std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect::<Vec<_>>();
    assert!(rows.iter().any(|v| v["path"]["raw"].is_object()));
    assert!(rows.iter().any(|v| {
        v["path"]["display"]
            .as_str()
            .is_some_and(|p| p.contains("line\n=not-a-formula"))
    }));
    drop(q);
    drop(s);
    let (t, s) = fixture_named(2, false, |i| {
        name(&format!("file-{i}.{}", "x".repeat(20_000)))
    });
    let q = s
        .query(
            GenerationId(1),
            &root,
            &SearchQuery::default(),
            t.path(),
            1024 * 1024,
            &cancel,
        )
        .unwrap();
    let stats = q.statistics(t.path(), 1024 * 1024, &cancel).unwrap();
    assert!(stats.extensions_byte_limited);
    assert_eq!(stats.others.files, 2);
    assert_eq!(stats.totals.known_bytes, 1);
    assert!(stats.complete);
}
