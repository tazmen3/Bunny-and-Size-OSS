use crate::tree::{Tree, format_bytes};
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, Coverage, InventoryLimits, ScanOptions,
    scan_inventory_with_limits,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Mailbox {
    pub(crate) progress: Option<String>,
    pub(crate) completion: Option<Result<CompletedScan, String>>,
}

pub(crate) struct CompletedScan {
    pub(crate) tree: Tree,
    pub(crate) summary: String,
    pub(crate) status: String,
    pub(crate) errors: String,
    pub(crate) warning: String,
    #[cfg(feature = "benchmarks")]
    pub(crate) benchmark_summary: BenchmarkSummary,
}

#[cfg(feature = "benchmarks")]
#[derive(Clone, Copy)]
pub(crate) struct BenchmarkSummary {
    pub(crate) files: u64,
    pub(crate) directories: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) coverage: Coverage,
    pub(crate) errors: u64,
    pub(crate) resource_limited_directories: u64,
    pub(crate) concurrency: ConcurrencyOptions,
    pub(crate) traversal_stats: sortsize_core::TraversalStats,
    pub(crate) scan_ms: u128,
    pub(crate) projection_ms: f64,
}

pub(crate) struct Task {
    pub(crate) cancellation: CancellationToken,
    pub(crate) mailbox: Arc<Mutex<Mailbox>>,
}

impl Task {
    pub(crate) fn start(root: PathBuf, workers: usize) -> std::io::Result<Self> {
        let cancellation = CancellationToken::default();
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let worker_token = cancellation.clone();
        let worker_mailbox = mailbox.clone();
        std::thread::Builder::new().name("sortsize-scan".into()).spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let mut last_update = Instant::now();
                let limits = InventoryLimits::default();
                let options = ScanOptions { concurrency: ConcurrencyOptions { workers, ..Default::default() }, ..Default::default() };
                let inventory = scan_inventory_with_limits(&root, &options, &worker_token, limits, |progress| {
                    if last_update.elapsed() >= Duration::from_millis(100) {
                        // Replace the previous update instead of queuing one event per file.
                        worker_mailbox.lock().unwrap_or_else(|e| e.into_inner()).progress = Some(format!(
                            "{} fichiers · {} dossiers · {} · {} erreurs",
                            progress.files, progress.directories, format_bytes(progress.logical_bytes), progress.errors_count
                        ));
                        last_update = Instant::now();
                    }
                }).map_err(|e| e.to_string().escape_debug().to_string())?;
                let report = &inventory.report;
                let status = match report.coverage {
                    Coverage::Complete => "Analyse terminée",
                    Coverage::Partial => "Analyse partielle — certains éléments n'ont pas été parcourus",
                    Coverage::Cancelled => "Analyse annulée — résultats partiels conservés",
                }.to_owned();
                let summary = format!(
                    "{} fichiers · {} dossiers · {} · {} ms · {} liens ignorés · {} erreurs · {} dossiers simultanés",
                    report.files, report.directories, format_bytes(report.logical_bytes), report.duration_ms, report.symlinks_skipped, report.errors_count, report.traversal_stats.worker_threads
                );
                let mut errors = if report.errors_count == 0 { String::new() } else { format!("{} erreurs", report.errors_count) };
                for issue in &report.errors {
                    errors.push_str(&format!("\n{} : {}", issue.path.display.escape_debug(), issue.message.escape_debug()));
                }
                if report.errors_omitted > 0 {
                    errors.push_str(&format!("\n{} détails supplémentaires omis", report.errors_omitted));
                }
                let mut warning = String::new();
                if inventory.entries_omitted > 0 {
                    warning = format!("Inventaire limité : {} entrées retenues, {} non affichées (plafonds : {} entrées, {} de chemins natifs). Les totaux du scan sont conservés.", inventory.entries.len(), inventory.entries_omitted, limits.max_entries, format_bytes(limits.max_path_bytes as u64));
                    if inventory.path_bytes_limit_reached {
                        warning.push_str(" Budget de chemins atteint.");
                    }
                }
                if report.excluded_entries > 0 {
                    if !warning.is_empty() { warning.push('\n'); }
                    warning.push_str(&format!("{} entrées exclues selon la politique du scanner : {}.", report.excluded_entries, report.exclusions.join(", ")));
                }
                if report.resource_limited_directories > 0 {
                    if !warning.is_empty() { warning.push('\n'); }
                    warning.push_str(&format!("Budget du parcours atteint : {} dossiers observés non parcourus ; leurs descendants ne sont pas comptés. Les tailles sont des bornes inférieures. Limites : {} dossiers et {} de chemins natifs en attente.", report.resource_limited_directories, report.traversal_limits.max_pending_directories, format_bytes(report.traversal_limits.max_pending_path_bytes as u64)));
                }
                #[cfg(feature = "benchmarks")]
                let projection_started = Instant::now();
                let (tree, _report) = Tree::new(inventory);
                Ok(CompletedScan { tree, summary, status, errors, warning,
                    #[cfg(feature = "benchmarks")]
                    benchmark_summary: BenchmarkSummary { files: _report.files,
                        directories: _report.directories, logical_bytes: _report.logical_bytes,
                        coverage: _report.coverage, errors: _report.errors_count,
                        resource_limited_directories: _report.resource_limited_directories,
                        concurrency: _report.concurrency, traversal_stats: _report.traversal_stats, scan_ms: _report.duration_ms,
                        projection_ms: projection_started.elapsed().as_secs_f64() * 1000.0 },
                })
            }).unwrap_or_else(|_| Err("Le traitement s'est interrompu de façon inattendue. Tu peux relancer l'analyse.".into()));
            worker_mailbox.lock().unwrap_or_else(|e| e.into_inner()).completion = Some(result);
        })?;
        Ok(Self {
            cancellation,
            mailbox,
        })
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // A blocked filesystem call must not force a join on the UI thread.
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn dropping_the_ui_task_requests_worker_cancellation() {
        let fixture = TempDir::new().unwrap();
        let task = Task::start(fixture.path().to_owned(), 4).unwrap();
        let token = task.cancellation.clone();
        drop(task);
        assert!(token.is_cancelled());
    }
}
