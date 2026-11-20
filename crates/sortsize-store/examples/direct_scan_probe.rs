//! Qualification of the GUI's actual DirectScan pipeline, without rendering.
//! Usage: direct_scan_probe SOURCE WORKSPACE [--fingerprint] [--directory-listing]
//! WORKSPACE must be a dedicated existing application-owned laboratory directory.
use sortsize_core::{
    acquisition::{Guard, Policy, ScanObservation},
    inventory_contract::EntryId,
};
use sortsize_store::{
    direct::{DirectScan, DirectStatus},
    inventory::Kind,
    scheduler::CollectionOptions,
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("source required")?);
    let workspace = PathBuf::from(args.next().ok_or("dedicated workspace required")?);
    let mut fingerprint = false;
    let mut observation = ScanObservation::Strict;
    for arg in args {
        if arg == "--fingerprint" {
            fingerprint = true;
        } else if arg == "--directory-listing" {
            observation = ScanObservation::DirectoryListing;
        } else {
            return Err("unknown option".into());
        }
    }
    let started = Instant::now();
    let guard = Guard::open(&source, Policy::default())?
        .with_internal_state(Guard::open(&workspace, Policy::default())?)?
        .with_scan_observation(observation);
    let scratch = tempfile::Builder::new()
        .prefix("direct-probe-")
        .tempdir_in(&workspace)?;
    let scan = DirectScan::start(
        guard,
        CollectionOptions::default(),
        scratch,
        4 * 1024 * 1024 * 1024,
    )?;
    let acquisition = Instant::now();
    let status = loop {
        let status = scan.status();
        if status != DirectStatus::Running {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let acquisition_s = acquisition.elapsed().as_secs_f64();
    let elapsed_s = started.elapsed().as_secs_f64();
    let tree = scan.tree().read().unwrap();
    let summary = tree.summary();
    // Order-independent regression fingerprint. Not a cryptographic integrity proof.
    // Occurrence paths, rather than scheduling-dependent EntryIds, distinguish hardlinks.
    let mut row_count = 0u64;
    let mut hash_xor = 0u64;
    let mut hash_sum = 0u64;
    if fingerprint {
        let mut pending = vec![EntryId(0)];
        while let Some(parent) = pending.pop() {
            let mut after = None;
            loop {
                let page = tree.children(parent, after, 200, false)?;
                for row in page.rows {
                    let mut hash = DefaultHasher::new();
                    row.path.strip_prefix(&source)?.hash(&mut hash);
                    format!("{:?}", row.kind).hash(&mut hash);
                    row.logical.hash(&mut hash);
                    row.allocated.hash(&mut hash);
                    row.modified.hash(&mut hash);
                    format!("{:?}", row.properties.allocated).hash(&mut hash);
                    format!("{:?}", row.properties.owner).hash(&mut hash);
                    // Reading metadata may itself update last-access time. Its
                    // availability must match, but its timestamp is not immutable.
                    std::mem::discriminant(&row.properties.accessed).hash(&mut hash);
                    let value = hash.finish();
                    hash_xor ^= value;
                    hash_sum = hash_sum.wrapping_add(value);
                    row_count += 1;
                    if row.kind == Kind::Directory {
                        pending.push(row.id);
                    }
                }
                if !page.more {
                    break;
                }
                after = page.next;
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "status": format!("{status:?}"), "elapsed_s": elapsed_s,
            "observation": format!("{observation:?}"),
            "acquisition_s": acquisition_s, "entries": summary.entries,
            "files": summary.measure.files, "directories": summary.measure.directories,
            "logical": format!("{:?}", summary.measure.logical),
            "allocated": format!("{:?}", summary.measure.allocated),
            "unknown": summary.measure.unknown, "allocation_unknown": summary.measure.allocation_unknown,
            "coverage": format!("{:?}", summary.coverage), "errors": summary.errors,
            "denied": summary.denied, "pending": summary.pending_directories,
            "incomplete": summary.incomplete_directories, "detached": summary.detached_directories,
            "duplicates": summary.duplicate_observations, "temporary_bytes": summary.temporary_bytes,
            "fingerprint": fingerprint.then(|| format!("{row_count}:{hash_xor:016x}:{hash_sum:016x}")),
            "cache": "OS caches not flushed", "rendering": false
        })
    );
    if status != DirectStatus::Finished {
        return Err("scan did not finish".into());
    }
    Ok(())
}
