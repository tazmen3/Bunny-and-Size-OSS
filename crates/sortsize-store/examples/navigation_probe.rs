//! Native generated fixture + preparation adapter timings, NOT a Slint UI benchmark.
use sortsize_core::{
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{Store, navigation::*},
    scheduler::{Session, State},
};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).ok_or("new fixture directory")?);
    fs::create_dir(&out)?;
    let root = out.join("source");
    fs::create_dir(&root)?;
    for i in 0..400 {
        fs::write(root.join(format!("file-{i:04}.bin")), [7; 32])?;
    }
    for d in 0..16 {
        let dir = root.join(format!("folder-{d:02}"));
        fs::create_dir(&dir)?;
        for i in 0..250 {
            fs::write(dir.join(format!("file-{i:04}.bin")), [7; 32])?;
        }
    }
    let c = AcquisitionContext {
        inventory: InventoryId(808),
        source: SourceId(808),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut v = ViewContext {
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
    let store = Store::create(
        &out.join("inventory"),
        c.inventory,
        c.source,
        Durability::Retained,
    )?;
    let mut session = Session::new(store, c, Guard::open(&root, Policy::default())?)?;
    let control = session.control();
    let start = Instant::now();
    let mut last = Instant::now();
    let mut provisional_publications = 0;
    loop {
        let status = session.tick()?;
        if last.elapsed() >= Duration::from_millis(100) {
            let f = session
                .store()
                .navigation_frame(c, v, Mode::Top100, &root)?;
            assert!(f.provisional);
            provisional_publications += 1;
            last = Instant::now();
        }
        if status.state == State::Idle {
            break;
        }
        if start.elapsed() > Duration::from_secs(120) {
            return Err("native fixture timeout".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step()?.is_none() {}
    let acquisition_ms = start.elapsed().as_secs_f64() * 1000.;
    let n = session.store().entry_count(c.generation)?;
    assert_eq!(n, 4417);
    assert_eq!(session.store().totals(c.generation)?.logical, Some(140800));
    fs::rename(&root, out.join("source-offline"))?;
    let quanta = session.status().quanta;
    let mut presenter = Presenter::new(v);
    let mut apply = Vec::new();
    let mut service = Vec::new();
    for i in 0..300 {
        v.epoch = ViewEpoch(i + 2);
        let begin = Instant::now();
        let mode = if i % 2 == 0 {
            Mode::Folder {
                parent: EntryKey {
                    inventory: c.inventory,
                    generation: c.generation,
                    entry: EntryId(0),
                },
                after: None,
            }
        } else {
            Mode::Top100
        };
        let frame = session.store().navigation_frame(c, v, mode, &root)?;
        service.push(begin.elapsed().as_secs_f64() * 1000.);
        let stamp = frame.stamp;
        let expected = frame.rows[0].key;
        let begin = Instant::now();
        presenter.reset(v);
        assert!(presenter.publish(frame));
        assert_eq!(presenter.select(stamp, 0), Some(expected));
        apply.push(begin.elapsed().as_secs_f64() * 1000.);
        control.navigate(EntryId(0));
        assert_eq!(session.tick()?.state, State::Sealed);
    }
    assert_eq!(session.status().quanta, quanta);
    assert!(!root.exists());
    apply.sort_by(f64::total_cmp);
    service.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"corpus":"generated files acquired natively","entries":n,"logical_bytes":140800,"acquisition_ms":acquisition_ms,"quanta":quanta,"provisional_publications":provisional_publications,"samples":apply.len(),"apply_release_p95_ms":apply[apply.len()*95/100],"apply_release_max_ms":apply.last(),"service_p95_ms":service[service.len()*95/100],"service_max_ms":service.last(),"extra_acquisition_quanta_after_source_offline":session.status().quanta-quanta,"scope":"presentation adapter only; not Slint or physical input latency"})
    );
    Ok(())
}
