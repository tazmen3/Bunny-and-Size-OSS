use super::*;
use sortsize_core::duplicates::DuplicateProgress;
fn native_name(i: u64) -> NativeName {
    let s = format!("f{i:08}");
    #[cfg(unix)]
    {
        NativeName::Posix(s.into_bytes())
    }
    #[cfg(windows)]
    {
        NativeName::Windows(s.encode_utf16().collect())
    }
}
struct Synthetic {
    hashed: u64,
    compared: u64,
    half: u64,
}
impl Verification for Synthetic {
    fn observe(&self, _: usize, p: &Path, length: u64, _: Option<SystemTime>) -> io::Result<Proof> {
        let n = p.file_name().unwrap().to_str().unwrap()[1..]
            .parse::<u64>()
            .unwrap();
        let representative = if n % 10 == 9 { n - 1 } else { n };
        let mut identity = vec![0];
        identity.extend(1u64.to_be_bytes());
        identity.extend(representative.to_be_bytes());
        Proof::decode(&serde_json::to_vec(&serde_json::json!({"identity":identity,"length":length,"before":false,"seconds":0,"nanos":0,"change":[0,0]})).unwrap())
    }
    fn hash(
        &mut self,
        _: usize,
        p: &Path,
        _: &Proof,
        _: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<[u8; 32]> {
        self.hashed += 1;
        let n = p.file_name().unwrap().to_str().unwrap()[1..]
            .parse::<u64>()
            .unwrap();
        Ok([u8::from(n >= self.half); 32])
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
        self.compared += 1;
        Ok(true)
    }
    fn progress(&self) -> DuplicateProgress {
        DuplicateProgress {
            bytes_read: 0,
            files_hashed: self.hashed,
        }
    }
}
fn peak() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find(|l| l.starts_with("VmHWM:"))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|v| v * 1024)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::GetCurrentProcess,
        };
        let mut p: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let n = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        p.cb = n;
        if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut p, n) } != 0 {
            Some(p.PeakWorkingSetSize as u64)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}
fn qualify(n: u64) {
    assert!(n >= 20 && n.is_multiple_of(20));
    let temp = tempfile::tempdir().unwrap();
    let c = AcquisitionContext {
        inventory: InventoryId(94),
        source: SourceId(94),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = Store::create(
        &temp.path().join("store"),
        c.inventory,
        c.source,
        crate::Durability::Reconstructible,
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
    let build = Instant::now();
    for (seq, lo) in (0..n).step_by(512).enumerate() {
        let end = (lo + 512).min(n);
        let rows: Vec<_> = (lo..end)
            .map(|i| Observation {
                name: native_name(i),
                kind: Kind::File,
                logical: Some(if i < n / 2 { 0 } else { 7 }),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect();
        s.ingest(
            BatchHeader {
                attempt: a,
                sequence: seq as u64,
                end: (end == n).then_some(ListingEnd::Exhausted),
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
    let build_ms = build.elapsed().as_secs_f64() * 1000.;
    let cancel = CancellationToken::default();
    let root = temp.path().join("never-created-source");
    let mut q = s
        .start_query(
            c.generation,
            &root,
            &SearchQuery::default(),
            temp.path(),
            1024 * 1024 * 1024,
        )
        .unwrap();
    while !q.advance(&cancel).unwrap() {}
    let start = Instant::now();
    let mut job = q
        .prepare_duplicates(temp.path(), 2 * 1024 * 1024 * 1024, &cancel)
        .unwrap();
    let preparation_ms = start.elapsed().as_secs_f64() * 1000.;
    assert_eq!(job.candidate_count().unwrap(), n);
    assert_eq!(job.summary().bytes_read, 0);
    let mut simulated = Synthetic {
        hashed: 0,
        compared: 0,
        half: n / 2,
    };
    let start = Instant::now();
    job.verify_with(&mut simulated, &cancel).unwrap();
    let grouping_ms = start.elapsed().as_secs_f64() * 1000.;
    assert!(job.summary().complete);
    assert_eq!(job.summary().groups, 2);
    assert_eq!(job.summary().independent_members, n - n / 10);
    assert_eq!(job.summary().aliases, n / 10);
    assert_eq!(simulated.hashed, n - n / 10);
    assert_eq!(simulated.compared, n - n / 10 - 2);
    let mut count = 0;
    for g in job.groups(None).unwrap() {
        assert_eq!(g.paths, n / 2);
        let mut after = None;
        loop {
            let rows = job.members(g.id, after).unwrap();
            if rows.is_empty() {
                break;
            }
            assert!(rows.len() <= 200);
            for r in rows {
                assert!(r.entry > after.unwrap_or(0));
                after = Some(r.entry);
                count += 1;
            }
        }
    }
    assert_eq!(count, n);
    assert!(!root.exists());
    println!(
        "{}",
        serde_json::json!({"synthetic":true,"source_io":0,"content_verifier":"injected simulation, no native hashing/comparison","files":n,"empty_files":n/2,"same_size_nonempty_files":n/2,"build_ms":build_ms,"preparation_ms":preparation_ms,"grouping_ms":grouping_ms,"peak_process_bytes":peak(),"summary":job.summary(),"all_oracles_pass":true})
    );
}
#[test]
fn bounded_disk_grouping_oracle() {
    qualify(1000)
}

#[test]
fn listing_policy_keeps_positive_groups_without_certifying_size_preselection() {
    // The injected verifier proves the grouping contract only. In particular,
    // it cannot discover a stale singleton size because preselection omits it.
    for policy in [PolicyId(1), crate::scheduler::DIRECTORY_LISTING_POLICY] {
        let temp = tempfile::tempdir().unwrap();
        let c = AcquisitionContext {
            inventory: InventoryId(95),
            source: SourceId(95),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy,
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let mut store = Store::create(
            &temp.path().join("store"),
            c.inventory,
            c.source,
            crate::Durability::Reconstructible,
        )
        .unwrap();
        store.create_generation(c).unwrap();
        let attempt = AttemptKey {
            task: TaskKey {
                context: c,
                directory: EntryId(0),
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(1),
            owner: OwnerEpoch(1),
            branch: BranchRevision(1),
        };
        store.begin_attempt(attempt).unwrap();
        let observations: Vec<_> = [7, 7, 9]
            .into_iter()
            .enumerate()
            .map(|(index, size)| Observation {
                name: native_name(index as u64),
                kind: Kind::File,
                logical: Some(size),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect();
        store
            .ingest(
                BatchHeader {
                    attempt,
                    sequence: 0,
                    end: Some(ListingEnd::Exhausted),
                },
                &observations,
            )
            .unwrap();
        while !store
            .reconcile_step(attempt, ComparisonScope::Comparable, 32)
            .unwrap()
        {}
        while !store.consolidate_step(c, 32).unwrap() {}
        store.seal(c).unwrap();
        let root = temp.path().join("never-opened-source");
        let cancel = CancellationToken::default();
        let mut query = store
            .start_query(
                c.generation,
                &root,
                &SearchQuery::default(),
                temp.path(),
                32 * 1024 * 1024,
            )
            .unwrap();
        while !query.advance(&cancel).unwrap() {}
        assert!(query.complete(), "traversal coverage remains complete");
        let mut job = query
            .prepare_duplicates(temp.path(), 32 * 1024 * 1024, &cancel)
            .unwrap();
        let mut verifier = Synthetic {
            hashed: 0,
            compared: 0,
            half: u64::MAX,
        };
        job.verify_with(&mut verifier, &cancel).unwrap();
        let listing = policy == crate::scheduler::DIRECTORY_LISTING_POLICY;
        assert_eq!(job.summary().directory_listing_sources, u64::from(listing));
        assert_eq!(job.summary().complete, !listing);
        assert_eq!(job.summary().source_independence_verified, !listing);
        assert_eq!(job.summary().unverified_sources, 0);
        assert_eq!(job.summary().errors, 0);
        assert_eq!(job.summary().groups, 1);
        assert_eq!(job.groups(None).unwrap()[0].paths, 2);
        assert_eq!(verifier.hashed, 2, "singleton size never reaches hashing");
        assert!(!root.exists(), "no implicit source refresh or content read");
    }
}

#[test]
#[ignore = "explicit synthetic million-entry campaign; not native file verification"]
fn synthetic_million_equal_sizes_and_empty_groups() {
    qualify(
        std::env::var("BUNNY_L10_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1_000_000),
    )
}
