//! Publication must remain admitted as its own writes reduce native free space.
use super::*;

#[test]
fn publication_budget_survives_stage_creation_and_reserved_growth() {
    // Physical size of Louis's sealed candidate, including its retained WAL.
    publication_remains_admitted(40 * 1024 * FRAME, 2_168_849_180);
}

#[test]
fn publication_budget_respects_low_free_and_capped_quota_branches() {
    publication_remains_admitted(600 * FRAME, 4 * FRAME);
    publication_remains_admitted(100 * 1024 * FRAME, 2_168_849_180);
}

fn publication_remains_admitted(free: u64, base: u64) {
    let governor = Governor::new(MemoryMode::Auto, None, free);
    let source = Path::new("source");
    let stage = Path::new("publication-stage");
    assert!(governor.observe_artifact_group(source, free, base));
    let growth = publication_growth_budget(base, base, free).unwrap();
    let planned = growth + 2 * base + 16 * FRAME;
    let mut reservation = governor.reserve_disk(planned).unwrap();
    // mkdir/owner creation precedes the canonical SQLite ledger. The quota
    // tracks free/4, so that native metadata allocation lowers it immediately.
    let stage_guard = governor
        .guard_artifact_group(stage, free - 4096, 0)
        .expect("publication must leave room for its stage metadata");
    assert!(stage_guard.admit(planned - FRAME, Some(&mut reservation)));
    assert!(governor.observe_artifact_group(
        stage,
        free - 4096 - (planned - FRAME),
        planned - FRAME
    ));
    assert!(stage_guard.admit(planned, Some(&mut reservation)));
    assert!(governor.observe_artifact_group(stage, free - 4096 - planned, planned));
    let usage = governor.usage();
    assert!(usage.disk_used + usage.disk_reserved <= usage.disk_limit);
}

#[test]
fn publication_budget_still_refuses_insufficient_global_quota() {
    assert!(publication_growth_budget(64 * FRAME, 64 * FRAME, 600 * FRAME).is_err());
    assert!(publication_growth_budget(0, 0, 512 * FRAME).is_err());
    assert!(publication_growth_budget(u64::MAX, 0, u64::MAX).is_err());
    assert!(publication_growth_budget(0, u64::MAX, u64::MAX).is_err());
    assert!(publication_growth_budget(0, 0, u64::MAX).is_ok());
}
