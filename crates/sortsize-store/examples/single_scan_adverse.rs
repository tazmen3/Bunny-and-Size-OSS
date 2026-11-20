//! Native Windows adverse-fixture inspection; never mutates the source.
//! Usage: single_scan_adverse SOURCE PRIVATE_WORKSPACE
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, ScanObservation},
    inventory_contract::*,
};
use sortsize_store::{
    direct::{DirectScan, DirectStatus},
    direct_save::SaveRequest,
    direct_snapshot,
    direct_tree::{DirectSummary, DirectTree},
    scheduler::{CollectionOptions, DIRECTORY_LISTING_POLICY},
};
use std::{path::PathBuf, time::Duration};

fn summary(s: DirectSummary) -> serde_json::Value {
    serde_json::json!({"entries":s.entries,"files":s.measure.files,"directories":s.measure.directories,
        "logical":s.measure.logical,"allocated":s.measure.allocated,"unknown":s.measure.unknown,
        "allocation_unknown":s.measure.allocation_unknown,"allocation_lower_bound":s.measure.allocation_lower_bound,
        "coverage":format!("{:?}",s.coverage),"errors":s.errors,"denied":s.denied,
        "pending":s.pending_directories,"incomplete":s.incomplete_directories,"detached":s.detached_directories,
        "duplicates":s.duplicate_observations})
}

fn rows(tree: &DirectTree) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let mut output = Vec::new();
    let mut id = 1u64;
    for batch in tree.replay()? {
        let batch = batch?;
        for (observation, proof) in batch.observations.iter().zip(batch.evidence) {
            let row = tree.row(EntryId(id))?;
            output.push(serde_json::json!({"id":id,"parent":row.parent.map(|x|x.0),"path_display":row.path.to_string_lossy(),
                "native_name":observation.name.encode(),"kind":format!("{:?}",row.kind),
                "logical":row.logical,"allocated":row.allocated,"allocation":format!("{:?}",row.properties.allocated),
                "metadata_known":observation.metadata_known,"identity":proof.as_ref().and_then(|p|p.identity).map(|i|format!("{i:?}"))}));
            id += 1;
        }
    }
    Ok(output)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("source required")?);
    let work = PathBuf::from(args.next().ok_or("private workspace required")?);
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    sortsize_store::inventory::lifecycle::ensure_root(&work)?;
    let guard = Guard::open(&source, Default::default())?
        .with_scan_observation(ScanObservation::DirectoryListing);
    let scratch = tempfile::Builder::new()
        .prefix("adverse-")
        .tempdir_in(&work)?;
    let options = CollectionOptions {
        allocation: true,
        owners: false,
    };
    let scan = DirectScan::start(guard, options, scratch, 512 * 1024 * 1024)?;
    loop {
        if scan.status() != DirectStatus::Running {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if scan.status() != DirectStatus::Finished {
        return Err(format!("scan failed: {:?}", scan.status()).into());
    }
    let tree = scan.tree().read().unwrap();
    let before = summary(tree.summary());
    let before_rows = rows(&tree)?;
    let history = work.join("snapshots");
    let guard = scan.guard();
    let destination = direct_snapshot::destination(&history, guard.root());
    let context = AcquisitionContext {
        inventory: InventoryId(8080),
        source: SourceId(9090),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: DIRECTORY_LISTING_POLICY,
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    direct_snapshot::save(
        SaveRequest {
            context,
            guard: &guard,
            scratch: &work,
            destination: &destination,
            started: 1,
            measured: 2,
            options,
            coverage: tree.summary().coverage,
        },
        tree.summary(),
        tree.replay()?,
        &CancellationToken::default(),
    )?;
    let loaded = direct_snapshot::load(
        &destination,
        &work,
        512 * 1024 * 1024,
        &CancellationToken::default(),
    )?;
    let after = summary(loaded.tree.summary());
    let after_rows = rows(&loaded.tree)?;
    let matched = before == after && before_rows == after_rows;
    println!(
        "{}",
        serde_json::json!({"summary":before,"rows":before_rows,"snapshot_reload_equal":matched,"snapshot":destination,"observation":"DirectoryListing","status":"Finished"})
    );
    if !matched {
        return Err("snapshot roundtrip differs".into());
    }
    Ok(())
}
