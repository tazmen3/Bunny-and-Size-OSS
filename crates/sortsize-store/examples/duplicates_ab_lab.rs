//! Opt-in, read-only comparison of two small operator-prepared qualification roots.
//! Usage: duplicates_ab_lab ROOT_A ROOT_B NEW_LOCAL_STATE EXPECTED_REMOTE_COUNT
//!        [unverified-roots [await-disconnect]]
//! Creates only local disposable inventory/derived state, never mounts/connects or writes a source.
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy, SourceClass, local_state_location},
    analysis::SearchQuery,
    duplicates::DuplicateOptions,
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{
        Store,
        queries::duplicates::{self, Mode, Scope},
    },
    scheduler::{Session, State},
};
use std::{
    error::Error,
    path::Path,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
fn explicit(root: &Path) -> Result<Guard> {
    Ok(Guard::open(
        root,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
    )?)
}
fn acquire(root: &Path, output: &Path, id: u128) -> Result<Session> {
    let c = AcquisitionContext {
        inventory: InventoryId(id),
        source: SourceId(id),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = Session::new(
        Store::create(output, c.inventory, c.source, Durability::Retained)?,
        c,
        explicit(root)?,
    )?;
    let start = Instant::now();
    while s.tick()?.state != State::Idle {
        if start.elapsed() > Duration::from_secs(30) {
            s.control().cancel();
            return Err("small fixture acquisition exceeded 30 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    loop {
        if let Some(coverage) = s.finalize_step()? {
            if coverage != ScopeCoverage::Complete {
                return Err("fixture inventory incomplete".into());
            }
            break;
        }
    }
    Ok(s)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !(4..=6).contains(&args.len()) {
        return Err("ROOT_A ROOT_B NEW_LOCAL_STATE EXPECTED_REMOTE_COUNT required".into());
    }
    let a = Path::new(&args[0]);
    let b = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    let expected_remote: usize = args[3].to_str().ok_or("remote count")?.parse()?;
    let unverified = args.get(4).and_then(|a| a.to_str()) == Some("unverified-roots");
    let await_disconnect = args.get(5).and_then(|a| a.to_str()) == Some("await-disconnect");
    if (args.len() >= 5 && !unverified) || (args.len() == 6 && !await_disconnect) {
        return Err("optional flags must be unverified-roots [await-disconnect]".into());
    }
    local_state_location(output)?;
    let initial_guards = [explicit(a)?, explicit(b)?];
    let root_creation_evidence = initial_guards
        .each_ref()
        .map(|g| g.evidence().1.created.is_some());
    let remote = initial_guards
        .iter()
        .filter(|g| g.evidence().0.class == SourceClass::Remote)
        .count();
    if remote != expected_remote {
        return Err("native source classification did not match requested scenario".into());
    }
    std::fs::create_dir(output)?;
    let sa = acquire(a, &output.join("inventory-a"), 19201)?;
    let sb = acquire(b, &output.join("inventory-b"), 19202)?;
    let revisions = (
        sa.store().revision(GenerationId(1))?,
        sb.store().revision(GenerationId(1))?,
    );
    let quanta = (sa.status().quanta, sb.status().quanta);
    let entries = (
        sa.store().entry_count(GenerationId(1))?,
        sb.store().entry_count(GenerationId(1))?,
    );
    let cancel = CancellationToken::default();
    let mut qa = sa.store().start_query(
        GenerationId(1),
        a,
        &SearchQuery::default(),
        output,
        16 * 1024 * 1024,
    )?;
    let mut qb = sb.store().start_query(
        GenerationId(1),
        b,
        &SearchQuery::default(),
        output,
        16 * 1024 * 1024,
    )?;
    while !qa.advance(&cancel)? {}
    while !qb.advance(&cancel)? {}
    let scopes = || {
        vec![
            Scope {
                query: &qa,
                roots: vec![a.to_path_buf(), a.join("nested"), a.to_path_buf()],
                side: 0,
            },
            Scope {
                query: &qb,
                roots: vec![b.to_path_buf()],
                side: 1,
            },
        ]
    };
    let mut job = duplicates::prepare_scopes(
        scopes(),
        Mode::BetweenSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    if job.summary().bytes_read != 0 {
        return Err("preparation read content".into());
    }
    let guards: Vec<_> = job
        .guard_roots()
        .iter()
        .map(|p| explicit(p))
        .collect::<Result<_>>()?;
    job.verify_scoped(&guards, DuplicateOptions::default(), &cancel)?;
    if job.summary().groups != 1 || job.summary().complete == unverified {
        return Err(format!("unexpected comparison coverage: {:?}", job.summary()).into());
    }
    if unverified {
        if job.summary().unverified_sources != remote as u64
            || job.summary().source_independence_verified
            || job.summary().errors != 0
        {
            return Err(format!(
                "uncertain roots must remain visible and incomplete: {:?}",
                job.summary()
            )
            .into());
        }
    } else if job.summary().unverified_sources != 0 || !job.summary().source_independence_verified {
        return Err(
            "qualified roots must retain verified independence and complete coverage".into(),
        );
    }
    let before = job.summary().bytes_read;
    let groups = job.groups(None)?;
    let members = job.members(groups[0].id, None)?;
    if members.len() != 3
        || groups[0].paths != 3
        || (!unverified && groups[0].independent_members != 3)
    {
        return Err("expected one group and three independent locations".into());
    }
    let count_a = members
        .iter()
        .filter(|m| m.source == SourceId(19201))
        .count();
    let count_b = members
        .iter()
        .filter(|m| m.source == SourceId(19202))
        .count();
    if count_a != 1 || count_b != 2 {
        return Err("group must have one A and two B members".into());
    }
    if members
        .iter()
        .any(|m| m.source != SourceId(19201) && m.source != SourceId(19202))
    {
        return Err("unselected source included".into());
    }
    if job.summary().bytes_read != before {
        return Err("presentation read content".into());
    }
    let success = serde_json::to_value(job.summary())?;
    drop(job);
    let mut limited = duplicates::prepare_scopes(
        scopes(),
        Mode::BetweenSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    limited.verify_scoped(
        &guards,
        DuplicateOptions {
            max_bytes_read: Some(1),
            ..Default::default()
        },
        &cancel,
    )?;
    if limited.summary().complete || !limited.summary().budget_limited {
        return Err("budget limit did not mark incomplete".into());
    }
    let limited_summary = serde_json::to_value(limited.summary())?;
    drop(limited);
    let cancelled = CancellationToken::default();
    cancelled.cancel();
    let mut cancelled_job = duplicates::prepare_scopes(
        scopes(),
        Mode::BetweenSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    cancelled_job.verify_scoped(&guards, Default::default(), &cancelled)?;
    if cancelled_job.summary().complete
        || !cancelled_job.summary().cancelled
        || cancelled_job.summary().bytes_read != 0
    {
        return Err("cancellation did not mark incomplete".into());
    }
    let cancelled_summary = serde_json::to_value(cancelled_job.summary())?;
    drop(cancelled_job);
    let mut offline_summary = None;
    if await_disconnect {
        std::fs::write(
            output.join("ready-for-disconnect"),
            b"inventories-sealed-no-more-acquisition\n",
        )?;
        let started = Instant::now();
        while !output.join("continue-after-disconnect").is_file() {
            if started.elapsed() > Duration::from_secs(60) {
                return Err("explicit lab disconnect signal not received".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut offline = duplicates::prepare_scopes(
            scopes(),
            Mode::BetweenSources,
            output,
            32 * 1024 * 1024,
            &cancel,
        )?;
        offline.verify_scoped(&guards, Default::default(), &cancel)?;
        if offline.summary().complete
            || offline.summary().errors == 0
            || offline.summary().bytes_read != 0
        {
            return Err(format!(
                "offline source must refuse content and stay incomplete: {:?}",
                offline.summary()
            )
            .into());
        }
        offline_summary = Some(serde_json::to_value(offline.summary())?);
    }
    if quanta != (sa.status().quanta, sb.status().quanta)
        || entries
            != (
                sa.store().entry_count(GenerationId(1))?,
                sb.store().entry_count(GenerationId(1))?,
            )
    {
        return Err("comparison started a new acquisition or changed inventory entries".into());
    }
    if revisions
        != (
            sa.store().revision(GenerationId(1))?,
            sb.store().revision(GenerationId(1))?,
        )
    {
        return Err("G0 inventory changed".into());
    }
    println!(
        "{}",
        serde_json::json!({"status":"PASS","native_remote_sources":remote,"inventories_complete":true,"nested_roots_deduplicated":true,"renamed_file_matches":true,"a_only_duplicates_excluded":true,"same_size_different_excluded":true,"unselected_c_excluded":true,"group_members_a":count_a,"group_members_b":count_b,"presentation_content_reads":0,"g0_unchanged":true,"comparison":success,"limited":limited_summary,"cancelled":cancelled_summary,"offline":offline_summary,"acquisition_quanta_unchanged":true,"root_creation_evidence":root_creation_evidence,"unverified_roots_mode":unverified})
    );
    Ok(())
}
