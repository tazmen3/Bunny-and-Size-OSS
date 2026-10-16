//! SCAN-009 experiment, NOT a product API: partition a fixture's immediate
//! directories across 1..=8 workers. No inventory/order/GUI claim. Each job uses
//! the real scanner; global pending budgets are divided across the workers.
use serde::Serialize;
use sortsize_core::{CancellationToken, Coverage, ScanOptions, TraversalLimits, scan};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

#[derive(Default, Debug, Serialize)]
struct Totals {
    files: u64,
    directories: u64,
    logical_bytes: u64,
    errors: u64,
    resource_limited_directories: u64,
    symlinks_skipped: u64,
    special_entries_skipped: u64,
}
impl Totals {
    fn merge(&mut self, other: Self) -> Result<(), String> {
        self.files += other.files;
        self.directories += other.directories;
        self.logical_bytes = self
            .logical_bytes
            .checked_add(other.logical_bytes)
            .ok_or("Size overflow")?;
        self.errors += other.errors;
        self.resource_limited_directories += other.resource_limited_directories;
        self.symlinks_skipped += other.symlinks_skipped;
        self.special_entries_skipped += other.special_entries_skipped;
        Ok(())
    }
}
fn probe(root: &Path, workers: usize, token: &CancellationToken) -> Result<Totals, String> {
    if !(1..=8).contains(&workers) {
        return Err("workers must be between 1 and 8".into());
    }
    let metadata = std::fs::symlink_metadata(root).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Expected a real directory".into());
    }
    if token.is_cancelled() {
        return Ok(Totals::default());
    }
    let (sender, receiver) = mpsc::sync_channel::<PathBuf>(32);
    let receiver = Mutex::new(receiver);
    let limits = TraversalLimits::default();
    let options = ScanOptions {
        traversal_limits: TraversalLimits {
            max_pending_directories: limits.max_pending_directories / workers,
            max_pending_path_bytes: limits.max_pending_path_bytes / workers,
        },
        ..Default::default()
    };
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let result = (|| {
                        let mut totals = Totals::default();
                        while !token.is_cancelled() {
                            let job = receiver
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_millis(10));
                            match job {
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Ok(path) => match scan(&path, &options, token, |_| {}) {
                                    Ok(report) => totals.merge(Totals {
                                        files: report.files,
                                        directories: report.directories,
                                        logical_bytes: report.logical_bytes,
                                        errors: report.errors_count,
                                        resource_limited_directories: report
                                            .resource_limited_directories,
                                        symlinks_skipped: report.symlinks_skipped,
                                        special_entries_skipped: report.special_entries_skipped,
                                    })?,
                                    Err(_) => {
                                        totals.directories += 1;
                                        totals.errors += 1;
                                    }
                                },
                            }
                        }
                        Ok::<_, String>(totals)
                    })();
                    if result.is_err() {
                        token.cancel();
                    }
                    result
                })
            })
            .collect();
        let mut totals = Totals {
            directories: 1,
            ..Default::default()
        };
        'listing: for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
            if token.is_cancelled() {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    totals.errors += 1;
                    continue;
                }
            };
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(_) => {
                    totals.errors += 1;
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                totals.symlinks_skipped += 1;
            } else if metadata.is_file() {
                totals.files += 1;
                totals.logical_bytes = totals
                    .logical_bytes
                    .checked_add(metadata.len())
                    .ok_or("Size overflow")?;
            } else if metadata.is_dir() {
                let mut path = entry.path();
                // At most 32 queued paths of at most 128 KiB, independent of
                // corpus size. Reject a fixture outside this experimental scope.
                if path.as_os_str().as_encoded_bytes().len() > 128 * 1024 {
                    return Err("Probe job path exceeds 128 KiB".into());
                }
                loop {
                    if token.is_cancelled() {
                        break 'listing;
                    }
                    match sender.try_send(path) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Full(returned)) => {
                            path = returned;
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            return Err("Probe workers stopped".into());
                        }
                    }
                }
            } else {
                totals.special_entries_skipped += 1;
            }
        }
        drop(sender);
        for handle in handles {
            totals.merge(handle.join().map_err(|_| "Probe worker panicked")??)?;
        }
        Ok(totals)
    })
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let workers: usize = args
        .get(1)
        .ok_or("Missing workers")?
        .to_str()
        .ok_or("Invalid workers")?
        .parse()?;
    let root = Path::new(args.get(2).ok_or("Missing fixture path")?);
    let token = CancellationToken::default();
    if let Some(ms) = args.get(3) {
        let delay = Duration::from_millis(ms.to_str().ok_or("Invalid cancel delay")?.parse()?);
        let cancel = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            cancel.cancel();
        });
    }
    let stamp = Instant::now();
    let totals = probe(root, workers, &token)?;
    let coverage = if token.is_cancelled() {
        Coverage::Cancelled
    } else if totals.errors > 0 || totals.resource_limited_directories > 0 {
        Coverage::Partial
    } else {
        Coverage::Complete
    };
    println!(
        "{}",
        serde_json::json!({"workers":workers,"totals":totals,"coverage":coverage,"wall_ms":stamp.elapsed().as_secs_f64()*1000.0})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partitioned_fixture_matches_the_real_scanner_for_every_worker_count() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..12 {
            let child = root.path().join(format!("été-{n}"));
            std::fs::create_dir(&child).unwrap();
            std::fs::write(child.join("data"), b"xyz").unwrap();
        }
        std::fs::write(root.path().join("top"), b"12").unwrap();
        let expected = scan(
            root.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            |_| {},
        )
        .unwrap();
        for workers in [1, 2, 4, 8] {
            let observed = probe(root.path(), workers, &CancellationToken::default()).unwrap();
            assert_eq!(
                (
                    observed.files,
                    observed.directories,
                    observed.logical_bytes,
                    observed.errors
                ),
                (
                    expected.files,
                    expected.directories,
                    expected.logical_bytes,
                    0
                )
            );
        }
        let token = CancellationToken::default();
        token.cancel();
        assert_eq!(probe(root.path(), 4, &token).unwrap().files, 0);
        assert!(probe(root.path(), 0, &token).is_err());
        assert!(probe(root.path(), 9, &token).is_err());
    }
}
