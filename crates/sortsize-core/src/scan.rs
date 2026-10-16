use crate::local::{LocalWalker, Observation};
use crate::{CancellationToken, Coverage, Progress, ScanOptions, ScanReport};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum ScanFailure {
    RootIo {
        path: PathBuf,
        error: std::io::Error,
    },
    InvalidRoot(PathBuf),
    InvalidOptions(String),
    SizeOverflow,
    WorkerIo(std::io::Error),
    WorkerPanic,
    ObservationPathLimit {
        required: usize,
        limit: usize,
    },
}

impl fmt::Display for ScanFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootIo { path, error } => {
                write!(f, "Impossible d'accéder à {} : {error}", path.display())
            }
            Self::InvalidRoot(path) => write!(
                f,
                "La racine {} doit être un dossier réel, pas un lien symbolique",
                path.display()
            ),
            Self::InvalidOptions(message) => write!(f, "Options invalides : {message}"),
            Self::WorkerIo(error) => write!(f, "Impossible de démarrer un travailleur : {error}"),
            Self::WorkerPanic => write!(f, "Un travailleur d'acquisition s'est interrompu"),
            Self::ObservationPathLimit { required, limit } => write!(
                f,
                "Un chemin natif nécessite {required} octets ; le budget de transmission est de {limit} octets"
            ),
            Self::SizeOverflow => write!(
                f,
                "La somme des tailles dépasse la capacité d'un entier 64 bits"
            ),
        }
    }
}

impl std::error::Error for ScanFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RootIo { error, .. } | Self::WorkerIo(error) => Some(error),
            _ => None,
        }
    }
}

/// Scan a real local directory. Progress is synchronous: invoke from a worker in a GUI.
/// Cancellation is checked between entries, not during a blocked OS call.
pub fn scan(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    progress: impl FnMut(&Progress),
) -> Result<ScanReport, ScanFailure> {
    scan_observed(root, options, cancellation, progress, |_, _| {})
}

pub(crate) fn scan_observed(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    progress: impl FnMut(&Progress),
    mut observe: impl FnMut(&Path, &Observation),
) -> Result<ScanReport, ScanFailure> {
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let started = Instant::now();
    let mut walker = if options.concurrency.workers == 1 {
        Walker::Sequential(Box::new(LocalWalker::new(root, options, cancellation)?))
    } else {
        Walker::Concurrent(crate::pool::Pool::new(root, options, cancellation)?)
    };
    let (root, exclusions) = match &walker {
        Walker::Sequential(w) => (w.root().to_owned(), w.effective_exclusions().to_vec()),
        Walker::Concurrent(w) => (w.root().to_owned(), w.effective_exclusions().to_vec()),
    };
    let options = ScanOptions {
        exclusions,
        ..options.clone()
    };
    let mut report = aggregate(
        &root,
        &options,
        cancellation,
        progress,
        walker
            .by_ref()
            .inspect(|(path, observation)| observe(path, observation)),
        started_at,
        started,
    )?;
    report.traversal_stats = match &mut walker {
        Walker::Sequential(w) => w.stats,
        Walker::Concurrent(w) => w.finish()?,
    };
    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

enum Walker {
    Sequential(Box<LocalWalker>),
    Concurrent(crate::pool::Pool),
}
impl Iterator for Walker {
    type Item = (PathBuf, Observation);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Sequential(w) => w.next(),
            Self::Concurrent(w) => w.next(),
        }
    }
}

fn aggregate(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    mut progress: impl FnMut(&Progress),
    mut observations: impl Iterator<Item = (PathBuf, Observation)>,
    started_at: u128,
    started: Instant,
) -> Result<ScanReport, ScanFailure> {
    let mut report = ScanReport {
        schema_version: 2,
        provider: "local",
        root: root.into(),
        started_at_unix_ms: started_at,
        duration_ms: 0,
        coverage: Coverage::Complete,
        files: 0,
        directories: 0,
        logical_bytes: 0,
        logical_size_basis: "per_file_path",
        allocated_bytes: None,
        potentially_reclaimable_bytes: None,
        symlinks_skipped: 0,
        special_entries_skipped: 0,
        excluded_entries: 0,
        depth_limited_directories: 0,
        resource_limited_directories: 0,
        traversal_limits: options.traversal_limits,
        traversal_stats: Default::default(),
        concurrency: options.concurrency,
        hardlinked_file_paths: if cfg!(unix) { Some(0) } else { None },
        errors_count: 0,
        errors_omitted: 0,
        errors: Vec::new(),
        follow_symlinks: false,
        exclusions: options.exclusions.clone(),
        max_depth: options.max_depth,
        mount_policy: "cross_accessible_mounts",
    };
    loop {
        if cancellation.is_cancelled() {
            report.coverage = Coverage::Cancelled;
            break;
        }
        let Some((path, observation)) = observations.next() else {
            break;
        };
        match observation {
            Observation::File { bytes, hardlinked } => {
                report.files += 1;
                report.logical_bytes = report
                    .logical_bytes
                    .checked_add(bytes)
                    .ok_or(ScanFailure::SizeOverflow)?;
                if hardlinked == Some(true)
                    && let Some(count) = &mut report.hardlinked_file_paths
                {
                    *count += 1;
                }
            }
            Observation::Directory {
                depth_limited,
                resource_limited,
            } => {
                report.directories += 1;
                if resource_limited {
                    report.resource_limited_directories += 1;
                }
                if depth_limited {
                    report.depth_limited_directories += 1;
                }
            }
            Observation::Symlink => report.symlinks_skipped += 1,
            Observation::Special => report.special_entries_skipped += 1,
            Observation::Excluded => report.excluded_entries += 1,
            Observation::Error(issue) => {
                report.errors_count += 1;
                if report.errors.len() < options.max_errors {
                    report.errors.push(issue);
                } else {
                    report.errors_omitted += 1;
                }
            }
        }
        progress(&Progress {
            current_path: path,
            files: report.files,
            directories: report.directories,
            logical_bytes: report.logical_bytes,
            errors_count: report.errors_count,
        });
    }
    if cancellation.is_cancelled() {
        report.coverage = Coverage::Cancelled;
    }
    if report.coverage != Coverage::Cancelled
        && (report.errors_count > 0
            || report.depth_limited_directories > 0
            || report.resource_limited_directories > 0)
    {
        report.coverage = Coverage::Partial;
    }
    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScanIssue;
    use std::io;

    #[test]
    fn access_errors_are_partial_and_stored_details_are_bounded() {
        let root = Path::new("fixture");
        let items = (0..3).map(|_| {
            (
                root.to_owned(),
                Observation::Error(ScanIssue::io(
                    root,
                    "list",
                    &io::Error::from(io::ErrorKind::PermissionDenied),
                )),
            )
        });
        let options = ScanOptions {
            max_errors: 1,
            ..Default::default()
        };
        let report = aggregate(
            root,
            &options,
            &CancellationToken::default(),
            |_| {},
            items,
            0,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(report.coverage, Coverage::Partial);
        assert_eq!(report.errors_count, 3);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors_omitted, 2);
        assert_eq!(report.errors[0].kind, "PermissionDenied");
    }

    #[test]
    fn size_overflow_is_an_error_instead_of_wrapping() {
        let root = Path::new("fixture");
        let items = [u64::MAX, 1].into_iter().map(|bytes| {
            (
                root.to_owned(),
                Observation::File {
                    bytes,
                    hardlinked: None,
                },
            )
        });
        assert!(matches!(
            aggregate(
                root,
                &ScanOptions::default(),
                &CancellationToken::default(),
                |_| {},
                items,
                0,
                Instant::now()
            ),
            Err(ScanFailure::SizeOverflow)
        ));
    }
}
