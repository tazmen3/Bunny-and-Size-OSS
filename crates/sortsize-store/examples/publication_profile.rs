//! Profil local d'une publication depuis un inventaire ferme, sans acquisition.
//! BUNNY_SCAN_P1_METRICS=1 publication_profile <source-dir> <new-destination-dir>
//!     <recorded-absolute-root> <generation-u64> <max-growth-bytes-u64>
//! Le parent de destination doit exister. Aucun nettoyage automatique : un stage
//! ou une destination produits avant une erreur restent disponibles au diagnostic.
//! Ce profil ne mesure ni catalogue Repository, ni retrait du candidat, ni Ready GUI.
use sortsize_core::{CancellationToken, inventory_contract::GenerationId};
use sortsize_store::{
    inventory::{
        Store,
        generation_a::{self, CertifiedGeneration},
    },
    report_scan_metrics,
};
use std::{
    error::Error,
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Instant,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err("destination or journal already exists; no overwrite admitted".into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn require_closed_source(directory: &Path) -> Result<()> {
    let database = directory.join("inventory.db");
    if !fs::symlink_metadata(&database)?.file_type().is_file() {
        return Err("source inventory.db must be a regular file".into());
    }
    // Conservative refusal even for empty sidecars; no checkpoint or repair.
    for name in [
        "inventory.db-wal",
        "inventory.db-shm",
        "inventory.db-journal",
    ] {
        require_absent(&directory.join(name))?;
    }
    Ok(())
}

fn number(value: OsString) -> Result<u64> {
    Ok(value
        .to_str()
        .ok_or("numeric argument is not UTF-8")?
        .parse()?)
}

fn measured<T>(boundary: &'static str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let started = Instant::now();
    let result = operation();
    let elapsed = started.elapsed();
    println!(
        "publication_profile phase={boundary} elapsed_ns={} success={}",
        elapsed.as_nanos(),
        result.is_ok(),
    );
    // Drains only this thread's phase aggregates; descendants are inclusive.
    report_scan_metrics(boundary);
    result
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let usage = "usage: publication_profile <source-dir> <new-destination-dir> <recorded-absolute-root> <generation-u64> <max-growth-bytes-u64>";
    let source_path = PathBuf::from(args.next().ok_or(usage)?);
    let destination = PathBuf::from(args.next().ok_or(usage)?);
    let root = PathBuf::from(args.next().ok_or(usage)?);
    let generation = GenerationId(number(args.next().ok_or(usage)?)?);
    let budget = number(args.next().ok_or(usage)?)?;
    if args.next().is_some() {
        return Err(usage.into());
    }
    if !source_path.is_absolute() || !destination.is_absolute() || !root.is_absolute() {
        return Err("source, destination and recorded root must be absolute".into());
    }
    if generation.0 == 0 || budget < 1024 * 1024 {
        return Err("generation must be nonzero and explicit growth budget at least 1 MiB".into());
    }
    if std::env::var_os("BUNNY_SCAN_P1_METRICS").is_none_or(|value| value != "1") {
        return Err("set BUNNY_SCAN_P1_METRICS=1 before starting this profile".into());
    }
    require_absent(&destination)?;
    let source_path = source_path.canonicalize()?;
    require_closed_source(&source_path)?;
    let parent = destination
        .parent()
        .ok_or("destination has no parent")?
        .canonicalize()?;
    if !parent.is_dir() || parent.starts_with(&source_path) {
        return Err("destination parent must exist outside the source inventory".into());
    }
    let destination = parent.join(destination.file_name().ok_or("destination has no name")?);
    require_absent(&destination)?;
    let before = fs::metadata(source_path.join("inventory.db"))?;
    let cancel = CancellationToken::default();
    let total = Instant::now();
    println!(
        "publication_profile begin generation={} max_growth_bytes={budget} sqlite_version={}",
        generation.0,
        rusqlite::version(),
    );
    let source = measured("profile_source_ro", || {
        Ok(Store::certify_readonly(&source_path, &cancel)?)
    })?;
    let measurement = source.measurement(generation)?;
    if measurement.root != root || measurement.context.generation != generation {
        return Err("recorded source root or generation differs from requested context".into());
    }
    require_closed_source(&source_path)?;
    require_absent(&destination)?;
    let report = measured("profile_publish", || {
        Ok(generation_a::publish(
            &source,
            &destination,
            generation,
            &root,
            budget,
            &cancel,
        )?)
    })?;
    let certified = measured("profile_reopen", || {
        Ok(CertifiedGeneration::open(
            &destination,
            generation,
            &root,
            &cancel,
        )?)
    })?;
    let same = measured("profile_same_observation", || {
        Ok(source.same_generation_observation(
            certified.as_store(),
            generation,
            generation,
            &cancel,
        )?)
    })?;
    if !same {
        return Err("published observation differs from the source; artifacts preserved".into());
    }
    println!(
        "publication_profile observation_equal=true entries={} oversized_paths={} artifact_bytes={} coverage={:?}",
        report.entries,
        report.oversized_paths,
        report.artifact_bytes,
        certified.coverage(),
    );
    measured("profile_close_readers", || {
        drop(certified);
        drop(source);
        Ok(())
    })?;
    require_closed_source(&source_path)?;
    let after = fs::metadata(source_path.join("inventory.db"))?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Err("source file metadata changed during profile; result not qualified".into());
    }
    println!(
        "publication_profile completed=true total_elapsed_ns={} source_file_metadata_unchanged=true gui_ready_measured=false",
        total.elapsed().as_nanos(),
    );
    Ok(())
}
