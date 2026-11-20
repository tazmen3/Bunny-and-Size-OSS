//! Real recursive metadata acquisition with the V0.5 service, no UI.
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
    path::PathBuf,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("source")?);
    let db = PathBuf::from(args.next().ok_or("new db path")?);
    let c = AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let start = Instant::now();
    let guard = Guard::open(&source, Policy::default())?;
    let store = Store::create(&db, c.inventory, c.source, Durability::Retained)?;
    let mut service = Session::new(store, c, guard)?;
    let mut acquisitions = 0;
    let mut cursors = 0;
    let mut parked = 0;
    loop {
        let s = service.tick()?;
        acquisitions = acquisitions.max(s.global_acquisitions);
        cursors = cursors.max(s.global_cursors);
        parked = parked.max(s.parked);
        if s.state == State::Idle {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let coverage = loop {
        if let Some(c) = service.finalize_step()? {
            break c;
        }
    };
    println!(
        "{}",
        serde_json::json!({"kind":"native_recursive_metadata","synthetic":false,"elapsed_ms":start.elapsed().as_secs_f64()*1000.,"entries":service.store().entry_count(c.generation)?,"logical_known":service.store().totals(c.generation)?.logical,"coverage":format!("{coverage:?}"),"quanta":service.status().quanta,"max_acquisitions":acquisitions,"max_cursors":cursors,"max_parked_cursors":parked,"cache":"OS caches not flushed"})
    );
    Ok(())
}
