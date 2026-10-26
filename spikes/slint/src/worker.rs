use crate::tree::Tree;
use crate::units::Units;
use sortsize_core::{
    AllocationAggregationOptions, CancellationToken, ConcurrencyOptions, InventoryLimits,
    InventoryReport, InventoryWithAllocationTotals, InventoryWithDates, MetadataSelection,
    ScanFailure, ScanOptions,
};
#[cfg(any(test, feature = "benchmarks"))]
use sortsize_core::{Progress, scan_inventory_with_allocation_totals, scan_inventory_with_dates};
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

#[cfg(any(test, feature = "benchmarks"))]
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
#[cfg(any(test, feature = "benchmarks"))]
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

/// Raw presentation state retained after the worker returns. No locale-dependent
/// strings cross this mailbox (native OS diagnostics remain opaque data).
pub(crate) struct ScanPresentation {
    pub(crate) report: sortsize_core::ScanReport,
    retained: usize,
    omitted: u64,
    path_limit: bool,
    limits: InventoryLimits,
}
pub(crate) enum DisplayLabels {
    Progress {
        files: u64,
        directories: u64,
        bytes: u64,
        errors: u64,
    },
    Summary(Arc<ScanPresentation>),
    Warning(Arc<ScanPresentation>),
    #[cfg(test)]
    Test(String),
}
impl DisplayLabels {
    pub(crate) fn get(&self, units: Units) -> String {
        match self {
            Self::Progress {
                files,
                directories,
                bytes,
                errors,
            } => format!(
                "{} · {} · {} · {}",
                crate::presentation::files(*files),
                crate::presentation::folders(*directories),
                units.format(*bytes),
                crate::presentation::errors(*errors)
            ),
            Self::Summary(data) => {
                let r = &data.report;
                format!(
                    "{} · {} · {} · {} ms · {} · {} · {}",
                    crate::presentation::files(r.files),
                    crate::presentation::folders(r.directories),
                    units.format(r.logical_bytes),
                    crate::number_format::integer(r.duration_ms),
                    tr_count!("{n} skipped link", "{n} skipped links", r.symlinks_skipped),
                    crate::presentation::errors(r.errors_count),
                    tr_count!(
                        "{n} concurrent folder",
                        "{n} concurrent folders",
                        r.traversal_stats.worker_threads as u64
                    )
                )
            }
            Self::Warning(data) => data.warning(units),
            #[cfg(test)]
            Self::Test(value) => value.clone(),
        }
    }
}
impl ScanPresentation {
    pub(crate) fn errors(&self) -> String {
        let report = &self.report;
        let mut errors = if report.errors_count == 0 {
            String::new()
        } else {
            crate::presentation::errors(report.errors_count)
        };
        for issue in &report.errors {
            errors.push_str(&format!(
                "\n{} : {}",
                issue.path.display.escape_debug(),
                issue.message.escape_debug()
            ));
        }
        if report.errors_omitted > 0 {
            errors.push('\n');
            errors.push_str(&tr_count!(
                "{n} additional detail omitted",
                "{n} additional details omitted",
                report.errors_omitted
            ));
        }
        errors
    }
    fn warning(&self, units: Units) -> String {
        let report = &self.report;
        let limits = self.limits;
        let mut warning = String::new();
        if self.omitted > 0 {
            warning = tr_format!(
                "Limited inventory: {}, {} (limits: {}, {} of native paths). Scan totals are preserved.",
                tr_count!(
                    "{n} retained entry",
                    "{n} retained entries",
                    self.retained as u64
                ),
                tr_count!(
                    "{n} entry not displayed",
                    "{n} entries not displayed",
                    self.omitted
                ),
                tr_count!("{n} entry", "{n} entries", limits.max_entries as u64),
                units.format(limits.max_path_bytes as u64)
            );
            if self.path_limit {
                warning.push_str(tr!(" Path budget reached."));
            }
        }
        if report.excluded_entries > 0 {
            if !warning.is_empty() {
                warning.push('\n');
            }
            warning.push_str(&tr_format!(
                "Scanner policy: {}. Rules: {}.",
                tr_count!(
                    "{n} excluded entry",
                    "{n} excluded entries",
                    report.excluded_entries
                ),
                report.exclusions.join(", ")
            ));
        }
        if report.resource_limited_directories > 0 {
            if !warning.is_empty() {
                warning.push('\n');
            }
            warning.push_str(&tr_format!("Traversal budget reached: {}. Their descendants are not counted. Sizes are lower bounds. Limits: {} and {} of pending native paths.", tr_count!("{n} observed folder not traversed", "{n} observed folders not traversed", report.resource_limited_directories), tr_count!("{n} folder", "{n} folders", report.traversal_limits.max_pending_directories as u64), units.format(report.traversal_limits.max_pending_path_bytes as u64)));
        }
        warning
    }
}
#[cfg(test)]
impl From<&str> for DisplayLabels {
    fn from(value: &str) -> Self {
        Self::Test(value.into())
    }
}
#[derive(Debug)]
pub(crate) enum Failure {
    Scan(ScanFailure),
    Inventory(&'static str),
    Panicked,
    SourceChanged,
}
impl Failure {
    pub(crate) fn render(&self) -> String {
        match self {
            Self::Scan(failure) => match failure {
                ScanFailure::RootIo { path, error } => tr_format!(
                    "Could not access {}: {}",
                    crate::tree::native_label(path),
                    error.to_string().escape_debug()
                ),
                ScanFailure::InvalidRoot(path) => tr_format!(
                    "The root {} must be a real folder, not a symbolic link",
                    crate::tree::native_label(path)
                ),
                ScanFailure::InvalidOptions(_) => {
                    tr!("Invalid scan options. Check worker count and traversal limits.").into()
                }
                ScanFailure::SizeOverflow => {
                    tr!("The sum of sizes exceeds the capacity of a 64-bit integer").into()
                }
                ScanFailure::WorkerIo(error) => tr_format!(
                    "Could not start a worker: {}",
                    error.to_string().escape_debug()
                ),
                ScanFailure::ObserverIo(error) => tr_format!(
                    "Observation stream failed: {}",
                    error.to_string().escape_debug()
                ),
                ScanFailure::WorkerPanic => {
                    tr!("An acquisition worker stopped unexpectedly").into()
                }
                ScanFailure::ObservationPathLimit { required, limit } => tr_format!(
                    "A native path requires {}; the transmission budget is {}",
                    tr_count!("{n} byte", "{n} bytes", *required as u64),
                    tr_count!("{n} byte", "{n} bytes", *limit as u64)
                ),
            },
            Self::Inventory(source) => tr!(source).into(),
            Self::SourceChanged => tr!(
                "Storage source changed or became unavailable. Refresh sources and select it again."
            )
            .into(),
            Self::Panicked => {
                tr!("Processing stopped unexpectedly. You can restart the scan.").into()
            }
        }
    }
}
#[derive(Default)]
pub(crate) struct Mailbox {
    pub(crate) directory: Option<DirectoryProgress>,
    pub(crate) progress: Option<DisplayLabels>,
    pub(crate) patch: Option<crate::tree::TreePatch>,
    pub(crate) completion: Option<Result<CompletedScan, Failure>>,
}
pub(crate) struct DirectoryProgress {
    pub(crate) retained: usize,
    pub(crate) seen: u64,
    pub(crate) closed: bool,
    pub(crate) previous: Option<u64>,
}
pub(crate) struct CompletedScan {
    pub(crate) previous_child_offset: Option<u64>,
    pub(crate) direct_children_total: u64,
    pub(crate) tree: Tree,
    pub(crate) summary: DisplayLabels,
    pub(crate) presentation: Arc<ScanPresentation>,
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
    pub(crate) coverage: sortsize_core::Coverage,
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
    #[cfg(test)]
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
        std::thread::Builder::new()
            .name("sortsize-scan".into())
            .spawn(move || {
                run_job(
                    Job {
                        root,
                        workers,
                        collect_allocation,
                        collect_owner,
                        limits: InventoryLimits::default(),
                        child_offset: 0,
                        scope: None,
                    },
                    worker_token,
                    worker_mailbox,
                );
            })?;
        Ok(Self {
            cancellation,
            mailbox,
        })
    }
}

/// Shared scan/projection pipeline for the initial scan and one-level pages.
pub(crate) struct Job {
    pub(crate) root: PathBuf,
    pub(crate) workers: usize,
    pub(crate) collect_allocation: bool,
    pub(crate) collect_owner: bool,
    pub(crate) limits: InventoryLimits,
    pub(crate) child_offset: u64,
    pub(crate) scope: Option<PathBuf>,
}
pub(crate) fn run_job(
    job: Job,
    worker_token: CancellationToken,
    worker_mailbox: Arc<Mutex<Mailbox>>,
) {
    let root = &job.root;
    let workers = job.workers;
    let collect_allocation = job.collect_allocation;
    let collect_owner = job.collect_owner;
    let limits = job.limits;
    let result = std::panic::catch_unwind(|| {
        let mut last_update = Instant::now();
        let mut last_projection = None::<Instant>;
        let mut projector = crate::tree::Projector::default();
        let mut prefix_start = 0;
        let mut prefix_count = 0usize;
        let root_bytes = root.as_os_str().as_encoded_bytes().len();
        let mut prefix_bytes = root_bytes;
        let mut seen = 0;
        let mut direct_children_total = 0;

        let options = ScanOptions {
            concurrency: ConcurrencyOptions {
                workers,
                ..Default::default()
            },
            ..Default::default()
        };
        if let Some(scope) = &job.scope {
            sortsize_core::validate_directory_scope(scope, root).map_err(Failure::Scan)?;
        }
        let streamed = sortsize_core::scan_inventory_progressive(
            root,
            &options,
            &worker_token,
            sortsize_core::ProgressiveInventoryOptions {
                limits,
                child_offset: job.child_offset,
                direct_children_only: job.scope.is_some(),
                selection: sortsize_core::OwnerSelection {
                    metadata: MetadataSelection {
                        dates: true,
                        allocation: false,
                    },
                    owner: collect_owner,
                },
                aggregation: collect_allocation.then_some(AllocationAggregationOptions {
                    metadata: MetadataSelection {
                        dates: true,
                        allocation: false,
                    },
                    deduplicate_by_identity: false,
                    max_identities: 0,
                }),
            },
            |progress, preview| {
                // Reconstruct the preceding window with O(1) state while the
                // requested prefix is already enumerated. No page-history list.
                let observed = preview.direct_children_seen;
                direct_children_total = observed;
                if observed > seen && observed <= job.child_offset {
                    let bytes = progress.current_path.as_os_str().as_encoded_bytes().len();
                    if prefix_count > 0
                        && (prefix_count + 1 >= limits.max_entries
                            || prefix_bytes.saturating_add(bytes) > limits.max_path_bytes)
                    {
                        prefix_start = observed - 1;
                        prefix_count = 0;
                        prefix_bytes = root_bytes;
                    }
                    prefix_count += 1;
                    prefix_bytes = prefix_bytes.saturating_add(bytes);
                }
                seen = observed;
                // One clock read per observation, shared by both
                // coalescing cadences. This hot path also runs after
                // the inventory cap on multi-million-entry scans.
                let now = Instant::now();
                if now.duration_since(last_update) >= Duration::from_millis(100) {
                    worker_mailbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .progress = Some(DisplayLabels::Progress {
                        files: progress.files,
                        directories: progress.directories,
                        bytes: progress.logical_bytes,
                        errors: progress.errors_count,
                    });
                    last_update = now;
                }
                if last_projection
                    .is_none_or(|time| now.duration_since(time) >= Duration::from_millis(500))
                    && worker_mailbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .patch
                        .is_none()
                    && !worker_token.is_cancelled()
                {
                    let directory = job.scope.as_ref().map(|_| DirectoryProgress {
                        retained: preview.entries.len(),
                        seen: observed,
                        closed: observed
                            > job.child_offset + preview.entries.len().saturating_sub(1) as u64,
                        previous: (job.child_offset > 0).then_some(prefix_start),
                    });
                    let patch = projector.patch(preview);
                    let mut mailbox = worker_mailbox.lock().unwrap_or_else(|e| e.into_inner());
                    mailbox.patch = Some(patch);
                    mailbox.directory = directory;
                    last_projection = Some(Instant::now());
                }
            },
        )
        .map_err(Failure::Scan)?;
        // Release worker mirrors before the final projection. The
        // UI keeps only its admitted paths and shared sort arrays.
        drop(projector);
        let acquired = if let Some((totals, entries_by_path)) = streamed.aggregates {
            let allocated = InventoryWithAllocationTotals {
                metadata: streamed.collected.metadata,
                totals,
                entries_by_path,
            };
            if collect_owner {
                AcquiredInventory::OwnerAllocated(
                    sortsize_core::InventoryWithOwnersAndAllocationTotals {
                        allocated,
                        owners: streamed.collected.owners,
                    },
                )
            } else {
                AcquiredInventory::Allocated(allocated)
            }
        } else if collect_owner {
            AcquiredInventory::Owners(streamed.collected)
        } else {
            AcquiredInventory::Dates(InventoryWithDates {
                inventory: streamed.collected.metadata.inventory,
                dates: streamed.collected.metadata.dates,
            })
        };
        let inventory = acquired.inventory();
        let retained = inventory.entries.len();
        let omitted = inventory.entries_omitted;
        let path_limit = inventory.path_bytes_limit_reached;
        #[cfg(feature = "benchmarks")]
        let projection_started = Instant::now();
        let (tree, report) = acquired.into_tree().map_err(Failure::Inventory)?;
        let presentation = Arc::new(ScanPresentation {
            report,
            retained,
            omitted,
            path_limit,
            limits,
        });
        let summary = DisplayLabels::Summary(presentation.clone());
        let warning = DisplayLabels::Warning(presentation.clone());
        #[cfg(feature = "benchmarks")]
        let _report = &presentation.report;
        Ok(CompletedScan {
            previous_child_offset: (job.child_offset > 0).then_some(prefix_start),
            direct_children_total,
            tree,
            summary,
            presentation: presentation.clone(),
            warning,
            #[cfg(feature = "benchmarks")]
            benchmark_summary: BenchmarkSummary {
                files: _report.files,
                directories: _report.directories,
                logical_bytes: _report.logical_bytes,
                coverage: _report.coverage,
                errors: _report.errors_count,
                resource_limited_directories: _report.resource_limited_directories,
                concurrency: _report.concurrency,
                traversal_stats: _report.traversal_stats,
                scan_ms: _report.duration_ms,
                projection_ms: projection_started.elapsed().as_secs_f64() * 1000.0,
            },
        })
    })
    .unwrap_or_else(|_| Err(Failure::Panicked));
    worker_mailbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .completion = Some(result);
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

#[cfg(test)]
mod i18n_tests {
    use super::*;
    use crate::{locale::Language, presentation::Context};
    #[test]
    fn structured_failures_warnings_errors_and_plurals_render_without_a_source() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().to_path_buf();
        let (_, mut report) = acquire(
            &source,
            &Default::default(),
            &Default::default(),
            Default::default(),
            false,
            |_| {},
        )
        .unwrap()
        .into_tree()
        .unwrap();
        fixture.close().unwrap();
        report.excluded_entries = 1;
        report.exclusions = vec!["opaque-pattern-{}".into()];
        report.resource_limited_directories = 1;
        report.errors_count = 1;
        report.errors_omitted = 1;
        report.errors = vec![sortsize_core::ScanIssue {
            path: std::path::Path::new("opaque-{n}-UID-1").into(),
            operation: "read_dir",
            kind: "PermissionDenied".into(),
            message: "OS native diagnostic".into(),
        }];
        let data = ScanPresentation {
            report,
            retained: 1,
            omitted: 1,
            path_limit: true,
            limits: InventoryLimits {
                max_entries: 1,
                max_path_bytes: 7,
            },
        };
        for language in [Language::English, Language::French] {
            let _scope = Context {
                language,
                generation: 17,
            }
            .enter();
            let warning = data.warning(Units::Binary);
            let errors = data.errors();
            assert!(warning.contains(if language == Language::English {
                "1 retained entry"
            } else {
                "1 entrée retenue"
            }));
            assert!(warning.contains(if language == Language::English {
                "1 observed folder not traversed"
            } else {
                "1 dossier observé non parcouru"
            }));
            assert!(warning.contains("opaque-pattern-{}"));
            assert!(errors.contains("opaque-{n}-UID-1"));
            assert!(errors.contains("OS native diagnostic"));
            assert!(errors.ends_with(if language == Language::English {
                "1 additional detail omitted"
            } else {
                "1 détail supplémentaire omis"
            }));
            let failures = [
                Failure::Scan(ScanFailure::RootIo {
                    path: "opaque-{}".into(),
                    error: std::io::Error::other("OS native diagnostic"),
                }),
                Failure::Scan(ScanFailure::InvalidRoot("opaque-{}".into())),
                Failure::Scan(ScanFailure::InvalidOptions(
                    "raw engine diagnostic retained".into(),
                )),
                Failure::Scan(ScanFailure::SizeOverflow),
                Failure::Scan(ScanFailure::WorkerIo(std::io::Error::other(
                    "OS native diagnostic",
                ))),
                Failure::Scan(ScanFailure::ObserverIo(std::io::Error::other(
                    "OS native diagnostic",
                ))),
                Failure::Scan(ScanFailure::WorkerPanic),
                Failure::Scan(ScanFailure::ObservationPathLimit {
                    required: 7,
                    limit: 1,
                }),
                Failure::Inventory("Owners and inventory are inconsistent"),
                Failure::Panicked,
            ];
            for failure in failures {
                assert!(!failure.render().is_empty());
            }
            for n in [0, 1, 2, 7] {
                let progress = DisplayLabels::Progress {
                    files: n,
                    directories: n,
                    bytes: n,
                    errors: n,
                };
                assert!(
                    progress
                        .get(Units::Binary)
                        .contains(&crate::presentation::errors(n))
                );
            }
        }
    }
}
