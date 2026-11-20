//! Small opt-in lab harness. Never mounts, reconnects, writes to a source, or
//! changes server settings. The operator controls all interruption signals.
//! Usage: nas_qualification inspect ROOT
//!        nas_qualification interrupt ROOT NEW_LOCAL_OUTPUT_DIRECTORY
//! Interrupt mode requires a small root with >32 direct files or pending child
//! directories. Write local signal `continue-g1`, then `start-new-explicit`.
use sortsize_core::{
    acquisition::{Guard, Policy, SourceClass, local_state_location},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::Store,
    scheduler::{Session, State},
};
use std::{
    error::Error,
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn explicit(root: &Path) -> Result<Guard> {
    let guard = Guard::open(
        root,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
    )?;
    if guard.evidence().0.class != SourceClass::Remote {
        return Err("qualification requires a positively classified remote source".into());
    }
    Ok(guard)
}
fn context(phase: u64) -> AcquisitionContext {
    // Independent inventories: no unproven durable continuity is inferred.
    AcquisitionContext {
        inventory: InventoryId(u128::from(phase)),
        source: SourceId(u128::from(phase)),
        generation: GenerationId(1),
        scan: ScanId(phase),
        policy: PolicyId(1),
        mount: MountEpoch(phase),
        run: RunEpoch(phase),
        validation: ValidationEpoch(phase),
    }
}
fn acquire(root: &Path, output: &Path, phase: u64) -> Result<Session> {
    let c = context(phase);
    Ok(Session::new(
        Store::create(
            &output.join(format!("inventory-{phase}")),
            c.inventory,
            c.source,
            Durability::Retained,
        )?,
        c,
        explicit(root)?,
    )?)
}
fn settle(session: &mut Session, paused: bool) -> Result<()> {
    let started = Instant::now();
    loop {
        let status = session.tick()?;
        if (paused && status.in_flight == 0) || (!paused && status.state == State::Idle) {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(90) {
            session.control().cancel();
            return Err("native acquisition still blocked after 90s; cancellation requested, not an immediate kernel interruption".into());
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn seal(session: &mut Session) -> Result<ScopeCoverage> {
    loop {
        if let Some(coverage) = session.finalize_step()? {
            return Ok(coverage);
        }
    }
}
fn report(label: &str, session: &Session, coverage: ScopeCoverage) -> Result<()> {
    let failure = session.last_acquisition_error().map(|e| {
        serde_json::json!({
            "path": e.path, "operation": format!("{:?}", e.operation),
            "kind": format!("{:?}", e.kind), "raw_os_error": e.raw_os_error,
        })
    });
    println!(
        "{}",
        serde_json::json!({
            "phase": label, "coverage": format!("{coverage:?}"),
            "entries": session.store().entry_count(GenerationId(1))?,
            "logical": session.store().totals(GenerationId(1))?.logical,
            "native_errors": session.acquisition_error_count(), "last_error": failure,
        })
    );
    Ok(())
}
fn signal(output: &Path, name: &str) -> Result<()> {
    fs::write(output.join(name), b"ready\n")?;
    println!("{}", serde_json::json!({"signal": name}));
    Ok(())
}
fn wait(output: &Path, name: &str) -> Result<()> {
    let started = Instant::now();
    while !output.join(name).is_file() {
        if started.elapsed() > Duration::from_secs(180) {
            return Err(format!("missing explicit lab signal: {name}").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args
        .get(1)
        .and_then(|s| s.to_str())
        .ok_or("mode required")?;
    let root = Path::new(args.get(2).ok_or("absolute remote root required")?);
    if Guard::open(root, Policy::default()).is_ok() {
        return Err("implicit root unexpectedly accepted".into());
    }
    let guard = explicit(root)?;
    println!(
        "{}",
        serde_json::json!({
            "implicit_denied": true, "class": "Remote", "native_identity": format!("{:?}", guard.evidence().1.identity),
            "creation_available": guard.evidence().1.created.is_some(), "explicit_inspect": guard.inspect(root)?.directory,
        })
    );
    if mode == "inspect" {
        return Ok(());
    }
    if mode != "interrupt" {
        return Err("mode must be inspect or interrupt".into());
    }
    let output = Path::new(args.get(3).ok_or("new local output directory required")?);
    local_state_location(output)?;
    if !guard.store_outside(output)? {
        return Err("output must be local and outside source".into());
    }
    fs::create_dir(output)?; // Never replace old evidence.
    let mut baseline = acquire(root, output, 1)?;
    settle(&mut baseline, false)?;
    let coverage = seal(&mut baseline)?;
    report("baseline", &baseline, coverage)?;
    if coverage != ScopeCoverage::Complete {
        return Err("baseline must be complete; choose a readable test subtree".into());
    }
    let before_revision = baseline.store().revision(GenerationId(1))?;
    let before_count = baseline.store().entry_count(GenerationId(1))?;
    let before_logical = baseline.store().totals(GenerationId(1))?.logical;
    let mut candidate = acquire(root, output, 2)?;
    candidate.tick()?;
    candidate.control().pause(true);
    settle(&mut candidate, true)?;
    if candidate.status().parked == 0
        && candidate
            .store()
            .pending_directories(candidate.context(), 1)?
            .is_empty()
    {
        return Err(
            "corpus finished in one quantum; use >32 direct files or nested directories".into(),
        );
    }
    signal(output, "g1-paused")?;
    wait(output, "continue-g1")?;
    candidate.control().pause(false);
    settle(&mut candidate, false)?;
    let partial = seal(&mut candidate)?;
    report("interrupted-candidate", &candidate, partial)?;
    if baseline.store().revision(GenerationId(1))? != before_revision
        || baseline.store().entry_count(GenerationId(1))? != before_count
        || baseline.store().totals(GenerationId(1))?.logical != before_logical
    {
        return Err("sealed baseline changed".into());
    }
    println!("{}", serde_json::json!({"baseline_preserved": true}));
    let quanta = candidate.status().quanta;
    for _ in 0..3 {
        candidate.tick()?;
    }
    if candidate.status().quanta != quanta {
        return Err("unexpected automatic acquisition".into());
    }
    signal(output, "g1-done")?;
    // A server restart alone never starts a source read. The separate signal
    // represents a new explicit action, not reuse of a saved network grant.
    wait(output, "start-new-explicit")?;
    drop(candidate);
    drop(baseline);
    let mut fresh = acquire(root, output, 3)?;
    settle(&mut fresh, false)?;
    let coverage = seal(&mut fresh)?;
    report("new-explicit-inventory", &fresh, coverage)?;
    if partial == ScopeCoverage::Complete {
        return Err(
            "interruption did not cause incomplete coverage (possibly native cache); not a PASS"
                .into(),
        );
    }
    if coverage != ScopeCoverage::Complete {
        return Err("new explicit acquisition did not complete".into());
    }
    Ok(())
}
