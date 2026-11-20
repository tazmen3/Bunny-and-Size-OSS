use super::*;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn c() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(71),
        source: SourceId(9),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn o(name: &str, kind: Kind) -> Observation {
    Observation {
        name: NativeName::Posix(name.as_bytes().to_vec()),
        kind,
        logical: Some(7),
        allocated: None,
        modified: None,
        metadata_known: true,
    }
}
fn put(s: &mut Store, a: AttemptKey, sequence: u64, end: Option<ListingEnd>, rows: &[Observation]) {
    s.ingest(
        BatchHeader {
            attempt: a,
            sequence,
            end,
        },
        rows,
    )
    .unwrap();
}
fn setup(path: &Path) -> Store {
    let mut s = Store::create(path, c().inventory, c().source, Durability::Retained).unwrap();
    s.create_generation_guarded(c(), Some(b"proof-v1")).unwrap();
    s
}
fn root(s: &mut Store, c: AcquisitionContext) -> AttemptKey {
    let a = s.next_attempt(c, EntryId(0)).unwrap();
    s.begin_attempt(a).unwrap();
    a
}
fn sweep(s: &mut Store, a: AttemptKey) {
    while !s.reconcile_step(a, ComparisonScope::Comparable, 1).unwrap() {}
}

#[test]
fn epochs_fence_replays_and_old_complete_branches_without_hybrid_totals() {
    let t = tempfile::tempdir().unwrap();
    let mut s = setup(&t.path().join("db"));
    let a = root(&mut s, c());
    put(
        &mut s,
        a,
        0,
        Some(ListingEnd::Exhausted),
        &[o("dir", Kind::Directory), o("gone", Kind::File)],
    );
    sweep(&mut s, a);
    let d = s
        .lookup(c().generation, EntryId(0), &o("dir", Kind::Directory).name)
        .unwrap()
        .unwrap()
        .key
        .entry;
    let child = s.next_attempt(c(), d).unwrap();
    s.begin_attempt(child).unwrap();
    put(
        &mut s,
        child,
        0,
        Some(ListingEnd::Exhausted),
        &[o("old", Kind::File)],
    );
    sweep(&mut s, child);
    while !s.consolidate_step(c(), 1).unwrap() {}
    assert_eq!(s.totals(c().generation).unwrap().logical, Some(14));
    let n = s.recover_candidate(c(), b"proof-v1").unwrap();
    assert!(n.run.0 > c().run.0 && n.validation.0 > c().validation.0);
    assert!(
        s.ingest(
            BatchHeader {
                attempt: child,
                sequence: 0,
                end: None
            },
            &[]
        )
        .is_err()
    );
    assert_eq!(s.pending_directories(n, 16).unwrap(), vec![EntryId(0)]);
    while !s.consolidate_step(n, 1).unwrap() {}
    assert_eq!(s.totals(n.generation).unwrap().logical, Some(0));
    let b = root(&mut s, n);
    put(
        &mut s,
        b,
        0,
        Some(ListingEnd::Exhausted),
        &[o("dir", Kind::Directory)],
    );
    assert!(
        s.lookup(n.generation, EntryId(0), &o("gone", Kind::File).name)
            .unwrap()
            .unwrap()
            .present
    );
    sweep(&mut s, b);
    assert!(
        !s.lookup(n.generation, EntryId(0), &o("gone", Kind::File).name)
            .unwrap()
            .unwrap()
            .present
    );
    assert_eq!(s.pending_directories(n, 16).unwrap(), vec![d]);
    let fresh = s.next_attempt(n, d).unwrap();
    assert!(fresh.attempt > child.attempt);
    s.begin_attempt(fresh).unwrap();
    let mut refused = o("old", Kind::Unknown);
    refused.metadata_known = false;
    refused.logical = None;
    put(&mut s, fresh, 0, Some(ListingEnd::Exhausted), &[refused]);
    sweep(&mut s, fresh);
    assert!(
        s.lookup(n.generation, d, &o("old", Kind::File).name)
            .unwrap()
            .unwrap()
            .present
    );
    while !s.consolidate_step(n, 1).unwrap() {}
    assert_eq!(s.totals(n.generation).unwrap().unknown, 1);
    assert_eq!(s.seal(n).unwrap(), ScopeCoverage::Partial);
    assert!(s.recover_candidate(n, b"proof-v1").is_err());
}

#[test]
fn denied_recovery_leaves_context_and_observations_untouched() {
    let t = tempfile::tempdir().unwrap();
    let mut s = setup(&t.path().join("db"));
    let a = root(&mut s, c());
    put(&mut s, a, 0, None, &[o("a", Kind::File)]);
    let rev = s.revision(c().generation).unwrap();
    assert!(s.recover_candidate(c(), b"different").is_err());
    assert_eq!(s.acquisition_context(c().generation).unwrap(), c());
    assert_eq!(s.revision(c().generation).unwrap(), rev);
    s.db.pragma_update(None, "user_version", 1).unwrap();
    assert!(s.recover_candidate(c(), b"proof-v1").is_err());
}

#[test]
fn large_revocation_is_fenced_in_constant_writes_then_swept_in_bounded_steps() {
    let t = tempfile::tempdir().unwrap();
    let mut s = setup(&t.path().join("db"));
    let a = root(&mut s, c());
    let start = Instant::now();
    for (seq, lo) in (0..50_000).step_by(512).enumerate() {
        let rows: Vec<_> = (lo..(lo + 512).min(50_000))
            .map(|n| o(&format!("d{n:06}"), Kind::Directory))
            .collect();
        put(&mut s, a, seq as u64, None, &rows);
    }
    put(&mut s, a, 98, Some(ListingEnd::Exhausted), &[]);
    let child = s.next_attempt(c(), EntryId(50_000)).unwrap();
    s.begin_attempt(child).unwrap();
    put(&mut s, child, 0, None, &[o("late-child", Kind::File)]);
    let changes = s.db.total_changes();
    let n = s.recover_candidate(c(), b"proof-v1").unwrap();
    assert!(s.db.total_changes() - changes <= 3);
    assert!(
        s.ingest(
            BatchHeader {
                attempt: child,
                sequence: 1,
                end: Some(ListingEnd::Exhausted)
            },
            &[o("stale", Kind::File)]
        )
        .is_err()
    );
    let b = root(&mut s, n);
    put(&mut s, b, 0, Some(ListingEnd::Exhausted), &[]);
    let mut steps = 0;
    loop {
        let before = s.db.total_changes();
        let done = s
            .reconcile_step(b, ComparisonScope::Comparable, 512)
            .unwrap();
        assert!(s.db.total_changes() - before <= 514);
        steps += 1;
        if done {
            break;
        }
    }
    assert_eq!(steps, 98);
    while !s.consolidate_step(n, 512).unwrap() {}
    assert_eq!(s.totals(n.generation).unwrap().logical, Some(0));
    assert_eq!(s.seal(n).unwrap(), ScopeCoverage::Complete);
    eprintln!(
        "synthetic 50k directory revocation: {steps} bounded sweep transactions, {:?}; source_io=0",
        start.elapsed()
    );
}

fn checkpoint(path: &Path) -> ! {
    std::fs::write(path, b"ready").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[test]
#[ignore = "child process driven by kill test"]
fn crash_child() {
    let Some(base) = std::env::var_os("BUNNY_L11_CRASH_DIR") else {
        return;
    };
    let base = std::path::PathBuf::from(base);
    let stage = std::env::var("BUNNY_L11_CRASH_STAGE").unwrap();
    let mut s = setup(&base.join("db"));
    let a = root(&mut s, c());
    put(
        &mut s,
        a,
        0,
        None,
        &[o("a", Kind::File), o("dir", Kind::Directory)],
    );
    let mut a = a;
    if stage.starts_with("sweep") || stage.starts_with("fold") {
        put(&mut s, a, 1, Some(ListingEnd::Exhausted), &[]);
        a = root(&mut s, c());
        put(
            &mut s,
            a,
            0,
            Some(ListingEnd::Exhausted),
            &[o("dir", Kind::Directory)],
        );
    }
    let ready = base.join("ready");
    if stage.ends_with("before") {
        let ready = ready.clone();
        s.db.commit_hook(Some(move || checkpoint(&ready))).unwrap();
    }
    if stage.starts_with("ingest") {
        put(&mut s, a, 1, None, &[o("b", Kind::File)]);
    } else if stage.starts_with("eof") {
        put(&mut s, a, 1, Some(ListingEnd::Exhausted), &[]);
    } else if stage.starts_with("sweep") {
        s.reconcile_step(a, ComparisonScope::Comparable, 1).unwrap();
    } else if stage.starts_with("fold") {
        s.consolidate_step(c(), 1).unwrap();
    } else if stage.starts_with("recover") {
        s.recover_candidate(c(), b"proof-v1").unwrap();
    } else {
        panic!("unknown stage")
    }
    checkpoint(&ready)
}
#[test]
fn killed_process_before_after_commit_eof_sweep_fold_and_recovery() {
    for stage in [
        "ingest-before",
        "ingest-after",
        "eof-before",
        "eof-after",
        "sweep-before",
        "sweep-after",
        "fold-before",
        "fold-after",
        "recover-before",
        "recover-after",
    ] {
        let t = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(t.path().join("child.log")).unwrap();
        let mut p = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "inventory::recovery::tests::crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("BUNNY_L11_CRASH_DIR", t.path())
            .env("BUNNY_L11_CRASH_STAGE", stage)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !t.path().join("ready").exists() {
            if let Some(status) = p.try_wait().unwrap() {
                panic!(
                    "{stage}: {status}, {}",
                    std::fs::read_to_string(t.path().join("child.log")).unwrap()
                );
            }
            if start.elapsed() > Duration::from_secs(30) {
                p.kill().unwrap();
                p.wait().unwrap();
                panic!("checkpoint timeout {stage}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        p.kill().unwrap();
        p.wait().unwrap();
        let mut s = Store::reopen(&t.path().join("db"), Durability::Retained).unwrap();
        let integrity: String =
            s.db.query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
        assert_eq!(integrity, "ok");
        let ctx = s.acquisition_context(c().generation).unwrap();
        let before = stage.ends_with("before");
        if stage.starts_with("ingest") {
            assert_eq!(
                s.entry_count(c().generation).unwrap(),
                if before { 3 } else { 4 }
            );
        }
        if stage.starts_with("sweep") {
            assert_eq!(
                s.lookup(c().generation, EntryId(0), &o("a", Kind::File).name)
                    .unwrap()
                    .unwrap()
                    .present,
                before
            );
        }
        if stage.starts_with("recover") {
            assert_eq!(ctx.run.0, if before { 1 } else { 2 });
        }
        let n = s.recover_candidate(ctx, b"proof-v1").unwrap();
        let a = root(&mut s, n);
        put(
            &mut s,
            a,
            0,
            Some(ListingEnd::Exhausted),
            &[
                o("a", Kind::File),
                o("b", Kind::File),
                o("dir", Kind::Directory),
            ],
        );
        assert_eq!(
            s.ingest(
                BatchHeader {
                    attempt: a,
                    sequence: 0,
                    end: Some(ListingEnd::Exhausted)
                },
                &[]
            )
            .unwrap(),
            Ack::Replay
        );
        sweep(&mut s, a);
        let d = s
            .lookup(n.generation, EntryId(0), &o("dir", Kind::Directory).name)
            .unwrap()
            .unwrap()
            .key
            .entry;
        let child = s.next_attempt(n, d).unwrap();
        s.begin_attempt(child).unwrap();
        put(
            &mut s,
            child,
            0,
            Some(ListingEnd::Exhausted),
            &[o("nested", Kind::File)],
        );
        sweep(&mut s, child);
        while !s.consolidate_step(n, 1).unwrap() {}
        assert_eq!(s.totals(n.generation).unwrap().logical, Some(21));
        assert_eq!(s.entry_count(n.generation).unwrap(), 5);
        assert_eq!(s.seal(n).unwrap(), ScopeCoverage::Complete);
        eprintln!("{stage}: killed process, integrity/replay/children/fresh totals PASS");
    }
}
