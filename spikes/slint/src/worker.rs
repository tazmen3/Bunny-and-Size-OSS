use crate::tree::Tree;
use crate::units::Units;
use sortsize_core::{
    AllocationAggregationOptions, CancellationToken, ConcurrencyOptions, Coverage, InventoryLimits,
    InventoryReport, InventoryWithAllocationTotals, InventoryWithDates, MetadataSelection,
    Progress, ScanFailure, ScanOptions, scan_inventory_with_allocation_totals,
    scan_inventory_with_dates,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) enum AcquiredInventory {
    Dates(InventoryWithDates),
    Allocated(InventoryWithAllocationTotals),
    Owners(sortsize_core::InventoryWithOwners),
    OwnerAllocated(sortsize_core::InventoryWithOwnersAndAllocationTotals),
}
impl AcquiredInventory {
    pub(crate) fn inventory(&self) -> &InventoryReport {
        match self {
            Self::Dates(result) => &result.inventory,
            Self::Allocated(result) => &result.metadata.inventory,
            Self::Owners(result) => &result.metadata.inventory,
            Self::OwnerAllocated(result) => &result.allocated.metadata.inventory,
        }
    }
    pub(crate) fn into_tree(self) -> Result<(Tree, sortsize_core::ScanReport), &'static str> {
        match self {
            Self::Dates(result) => Tree::with_dates(result),
            Self::Allocated(result) => Tree::with_allocations(result),
            Self::Owners(result) => {
                let (mut tree, report) = Tree::with_dates(InventoryWithDates {
                    inventory: result.metadata.inventory,
                    dates: result.metadata.dates,
                })?;
                tree.attach_owners(result.owners)?;
                Ok((tree, report))
            }
            Self::OwnerAllocated(result) => {
                let (mut tree, report) = Tree::with_allocations(result.allocated)?;
                tree.attach_owners(result.owners)?;
                Ok((tree, report))
            }
        }
    }
}

pub(crate) fn acquire_with_owner(
    root: &std::path::Path,
    options: &ScanOptions,
    token: &CancellationToken,
    limits: InventoryLimits,
    collect_allocation: bool,
    collect_owner: bool,
    progress: impl FnMut(&Progress),
) -> Result<AcquiredInventory, ScanFailure> {
    if !collect_owner {
        return acquire(root, options, token, limits, collect_allocation, progress);
    }
    if collect_allocation {
        sortsize_core::scan_inventory_with_owners_and_allocation_totals(
            root,
            options,
            token,
            limits,
            AllocationAggregationOptions {
                metadata: MetadataSelection {
                    dates: true,
                    allocation: false,
                },
                deduplicate_by_identity: false,
                max_identities: 0,
            },
            true,
            progress,
        )
        .map(AcquiredInventory::OwnerAllocated)
    } else {
        sortsize_core::scan_inventory_with_owners(
            root,
            options,
            token,
            limits,
            sortsize_core::OwnerSelection {
                metadata: MetadataSelection {
                    dates: true,
                    allocation: false,
                },
                owner: true,
            },
            progress,
        )
        .map(AcquiredInventory::Owners)
    }
}
pub(crate) fn acquire(
    root: &std::path::Path,
    options: &ScanOptions,
    token: &CancellationToken,
    limits: InventoryLimits,
    collect_allocation: bool,
    progress: impl FnMut(&Progress),
) -> Result<AcquiredInventory, ScanFailure> {
    if collect_allocation {
        scan_inventory_with_allocation_totals(
            root,
            options,
            token,
            limits,
            AllocationAggregationOptions {
                metadata: MetadataSelection {
                    dates: true,
                    allocation: false,
                },
                deduplicate_by_identity: false,
                max_identities: 0,
            },
            progress,
        )
        .map(AcquiredInventory::Allocated)
    } else {
        scan_inventory_with_dates(root, options, token, limits, progress)
            .map(AcquiredInventory::Dates)
    }
}

pub(crate) struct DisplayLabels([String; 2]);
impl DisplayLabels {
    fn new(format: impl Fn(Units) -> String) -> Self {
        Self([format(Units::Binary), format(Units::Decimal)])
    }
    pub(crate) fn get(&self, units: Units) -> &str {
        &self.0[usize::from(units == Units::Decimal)]
    }
}
#[cfg(test)]
impl From<&str> for DisplayLabels {
    fn from(value: &str) -> Self {
        Self([value.into(), value.into()])
    }
}
#[derive(Default)]
pub(crate) struct Mailbox {
    pub(crate) progress: Option<DisplayLabels>,
    pub(crate) completion: Option<Result<CompletedScan, String>>,
}

pub(crate) struct CompletedScan {
    pub(crate) tree: Tree,
    pub(crate) summary: DisplayLabels,
    pub(crate) status: String,
    pub(crate) errors: String,
    pub(crate) warning: DisplayLabels,
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
    pub(crate) fn start(
        root: PathBuf,
        workers: usize,
        collect_allocation: bool,
        collect_owner: bool,
    ) -> std::io::Result<Self> {
        let cancellation = CancellationToken::default();
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let worker_token = cancellation.clone();
        let worker_mailbox = mailbox.clone();
        std::thread::Builder::new().name("sortsize-scan".into()).spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let mut last_update = Instant::now();
                let limits = InventoryLimits::default();
                let options = ScanOptions { concurrency: ConcurrencyOptions { workers, ..Default::default() }, ..Default::default() };
                let acquired = acquire_with_owner(&root, &options, &worker_token, limits, collect_allocation, collect_owner, |progress| {
                    if last_update.elapsed() >= Duration::from_millis(100) {
                        // Replace the previous update instead of queuing one event per file.
                        worker_mailbox.lock().unwrap_or_else(|e| e.into_inner()).progress = Some(DisplayLabels::new(|units| format!(
                            "{} fichiers · {} dossiers · {} · {} erreurs",
                            progress.files, progress.directories, units.format(progress.logical_bytes), progress.errors_count
                        )));
                        last_update = Instant::now();
                    }
                }).map_err(|e| e.to_string().escape_debug().to_string())?;
                let inventory = acquired.inventory();
                let report = &inventory.report;
                let status = match report.coverage {
                    Coverage::Complete => "Analyse terminée",
                    Coverage::Partial => "Analyse partielle — certains éléments n'ont pas été parcourus",
                    Coverage::Cancelled => "Analyse annulée — résultats partiels conservés",
                }.to_owned();
                let summary = DisplayLabels::new(|units| format!(
                    "{} fichiers · {} dossiers · {} · {} ms · {} liens ignorés · {} erreurs · {} dossiers simultanés",
                    report.files, report.directories, units.format(report.logical_bytes), report.duration_ms, report.symlinks_skipped, report.errors_count, report.traversal_stats.worker_threads
                ));
                let mut errors = if report.errors_count == 0 { String::new() } else { format!("{} erreurs", report.errors_count) };
                for issue in &report.errors {
                    errors.push_str(&format!("\n{} : {}", issue.path.display.escape_debug(), issue.message.escape_debug()));
                }
                if report.errors_omitted > 0 {
                    errors.push_str(&format!("\n{} détails supplémentaires omis", report.errors_omitted));
                }
                let warning = DisplayLabels::new(|units| {
                let mut warning = String::new();
                if inventory.entries_omitted > 0 {
                    warning = format!("Inventaire limité : {} entrées retenues, {} non affichées (plafonds : {} entrées, {} de chemins natifs). Les totaux du scan sont conservés.", inventory.entries.len(), inventory.entries_omitted, limits.max_entries, units.format(limits.max_path_bytes as u64));
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
                    warning.push_str(&format!("Budget du parcours atteint : {} dossiers observés non parcourus ; leurs descendants ne sont pas comptés. Les tailles sont des bornes inférieures. Limites : {} dossiers et {} de chemins natifs en attente.", report.resource_limited_directories, report.traversal_limits.max_pending_directories, units.format(report.traversal_limits.max_pending_path_bytes as u64)));
                }
                warning
                });
                #[cfg(feature = "benchmarks")]
                let projection_started = Instant::now();
                let (tree, _report) = acquired.into_tree().map_err(str::to_owned)?;
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
    fn visible_option_selects_one_pass_with_dates_and_without_identity_tracking() {
        let fixture = TempDir::new().unwrap();
        std::fs::write(fixture.path().join("f"), [1; 4096]).unwrap();
        for workers in [1, 4, 8] {
            for collect in [false, true] {
                let task = Task::start(fixture.path().to_owned(), workers, collect, false).unwrap();
                let start = Instant::now();
                let completion = loop {
                    if let Some(result) = task.mailbox.lock().unwrap().completion.take() {
                        break result.unwrap();
                    }
                    assert!(start.elapsed() < Duration::from_secs(10));
                    std::thread::sleep(Duration::from_millis(5));
                };
                let rows = completion.tree.details().0;
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].allocated == "Non collectée", !collect);
                assert!(rows[0].modified.ends_with(" UTC"));
                let raw = acquire(
                    fixture.path(),
                    &ScanOptions::default(),
                    &CancellationToken::default(),
                    InventoryLimits::default(),
                    collect,
                    |_| {},
                )
                .unwrap();
                match raw {
                    AcquiredInventory::Dates(_) => assert!(!collect),
                    AcquiredInventory::Allocated(result) => {
                        assert!(collect);
                        assert_eq!(result.totals.identity_tracking.capacity, 0);
                        assert_eq!(
                            result.totals.by_identity.value,
                            sortsize_core::MetadataValue::NotCollected
                        );
                        assert!(result.metadata.allocations.is_empty());
                        assert_eq!(result.metadata.dates.len(), result.entries_by_path.len());
                    }
                    _ => panic!("Owner must remain off"),
                }
            }
        }
    }

    #[test]
    fn dropping_the_ui_task_requests_worker_cancellation() {
        let fixture = TempDir::new().unwrap();
        let task = Task::start(fixture.path().to_owned(), 4, false, false).unwrap();
        let token = task.cancellation.clone();
        drop(task);
        assert!(token.is_cancelled());
    }
}
