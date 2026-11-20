//! Explicit native scan/recovery of an existing real tree, not generated metadata.
use sortsize_core::{
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::Store,
    scheduler::{Session, State},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};
fn oracle(p: &Path) -> std::io::Result<(u64, u64)> {
    let mut count = 1;
    let mut bytes = 0;
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let m = e.path().symlink_metadata()?;
        if m.is_dir() {
            let (n, b) = oracle(&e.path())?;
            count += n;
            bytes += b
        } else {
            count += 1;
            if m.is_file() {
                bytes += m.len()
            }
        }
    }
    Ok((count, bytes))
}
fn run(s: &mut Session, timeout: Duration) -> sortsize_store::inventory::Result<()> {
    let start = Instant::now();
    while s.tick()?.state != State::Idle {
        assert!(start.elapsed() < timeout);
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let requested_mode = args
        .get(2)
        .map_or("auto", |s| s.to_str().unwrap_or("invalid"));
    let mode = MemoryMode::from_key(requested_mode).ok_or("invalid memory mode")?;
    let governor = sortsize_core::resources::Governor::global();
    governor.configure(mode, sortsize_core::resources::native_memory());
    let exercise_cancel = args.get(3).is_some_and(|s| s == "cancel");
    let timeout_seconds = args
        .get(4)
        .map(|s| s.to_string_lossy().parse::<u64>())
        .transpose()?
        .unwrap_or(120);
    if !(1..=1800).contains(&timeout_seconds) {
        return Err(
            "operational timeout must be in 1..=1800 seconds, not a performance SLA".into(),
        );
    }
    let timeout = Duration::from_secs(timeout_seconds);
    let root = Path::new(&args[0]);
    let out = Path::new(&args[1]);
    std::fs::create_dir(out)?;
    let expected = oracle(root)?; // Deliberately warms metadata; no cold claim.
    let c = AcquisitionContext {
        inventory: InventoryId(70),
        source: SourceId(70),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = Session::new(
        Store::create(
            &out.join("store"),
            c.inventory,
            c.source,
            Durability::Retained,
        )?,
        c,
        Guard::open(root, Policy::default())?,
    )?;
    let cancellation_ms = if exercise_cancel {
        // A bounded worker quantum may finish; cancellation never means that
        // already authorized observations are silently discarded.
        s.tick()?;
        s.control().cancel();
        let start = Instant::now();
        assert_eq!(s.tick()?.state, State::Cancelled);
        let elapsed = start.elapsed().as_secs_f64() * 1000.;
        // Source exclusion follows outstanding native jobs after Cancelled.
        // Drain authorized returns before reopening the same root.
        while s.tick()?.in_flight != 0 {
            assert!(start.elapsed() < timeout);
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(s);
        s = Session::resume(
            Store::reopen(&out.join("store"), Durability::Retained)?,
            c.generation,
            Guard::open(root, Policy::default())?,
        )?;
        Some(elapsed)
    } else {
        None
    };
    let start = Instant::now();
    run(&mut s, timeout)?;
    let initial_ms = start.elapsed().as_secs_f64() * 1000.;
    let initial_quanta = s.status().quanta;
    assert_eq!(s.store().entry_count(c.generation)?, expected.0);
    drop(s);
    let start = Instant::now();
    let mut s = Session::resume(
        Store::reopen(&out.join("store"), Durability::Retained)?,
        c.generation,
        Guard::open(root, Policy::default())?,
    )?;
    let admission_ms = start.elapsed().as_secs_f64() * 1000.;
    run(&mut s, timeout)?;
    let scan_ms = start.elapsed().as_secs_f64() * 1000.;
    let coverage = loop {
        if let Some(c) = s.finalize_step()? {
            break c;
        }
    };
    let elapsed = start.elapsed().as_secs_f64() * 1000.;
    assert_eq!(coverage, ScopeCoverage::Complete);
    assert_eq!(s.store().totals(c.generation)?.logical, Some(expected.1));
    assert_eq!(s.store().entry_count(c.generation)?, expected.0);
    let result = serde_json::json!({"real_tree":true,"synthetic_metadata":false,"operational_timeout_seconds":timeout_seconds,"timeout_is_operational_not_sla":true,"requested_mode":requested_mode,"effective_mode":governor.usage().limits.requested.key(),"effective_byte_limit":governor.usage().limits.bytes,"effective_entry_limit":governor.usage().limits.entries,"cancellation_ms":cancellation_ms,"cancelled_then_resumed":exercise_cancel,"oracle_warmed_metadata":true,"entries":expected.0,"logical_bytes":expected.1,"initial_scan_ms":initial_ms,"initial_quanta":initial_quanta,"resume_admission_ms":admission_ms,"resume_scan_ms":scan_ms,"resume_and_finalize_ms":elapsed,"resume_quanta":s.status().quanta,"run_epoch":s.context().run.0,"validation_epoch":s.context().validation.0,"all_oracles_pass":true});
    std::fs::write(out.join("qualification.json"), format!("{result}\n"))?;
    println!("{result}");
    Ok(())
}
