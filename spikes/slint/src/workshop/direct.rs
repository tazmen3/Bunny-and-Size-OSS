//! In-memory scan and consultation, with independent background persistence.
#[path = "direct/branches.rs"]
mod branches;
#[path = "direct/query.rs"]
mod query;
#[path = "direct/reload.rs"]
mod reload;
use super::*;
use sortsize_core::{
    CancellationToken, MetadataValue, ValueAccuracy,
    resources::{Category, Governor, disk_limit, native_free_disk},
};
use sortsize_store::{
    direct::{DirectScan, DirectStatus},
    direct_save::SaveRequest,
    direct_snapshot::{self, SavedSnapshot},
    direct_tree::{DirectPage, DirectRow, DirectSummary, DirectTree},
    inventory::{
        Kind, Result, folder_totals::FolderMeasure, lifecycle, navigation::Frame, queries::Row,
    },
};
use std::{
    collections::BTreeMap,
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    time::SystemTime,
};

const DETAIL_LIMIT: usize = 200;
const BRANCH_LIMIT: usize = 200;
const TREE_LIMIT: usize = 512;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(200);

struct Saving {
    request: cache::Request,
    continuation: Option<cache::Request>,
    result: Receiver<Result<SavedSnapshot>>,
}

type SaveJob = Box<dyn FnOnce() + Send>;

fn pending_snapshots() -> &'static Mutex<BTreeMap<PathBuf, usize>> {
    static PENDING: std::sync::OnceLock<Mutex<BTreeMap<PathBuf, usize>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

struct PendingSnapshot(PathBuf);
impl PendingSnapshot {
    fn new(root: &Path) -> Self {
        let key = direct_snapshot::destination(Path::new(""), root);
        *pending_snapshots()
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default() += 1;
        Self(key)
    }
}
impl Drop for PendingSnapshot {
    fn drop(&mut self) {
        let mut pending = pending_snapshots().lock().unwrap();
        let count = pending.get_mut(&self.0).expect("owned pending snapshot");
        *count -= 1;
        if *count == 0 {
            pending.remove(&self.0);
        }
    }
}

pub(super) fn lock_snapshot_deletion(
    saved: &sortsize_store::inventory::catalogue::Saved,
) -> Result<std::sync::MutexGuard<'static, BTreeMap<PathBuf, usize>>> {
    let pending = pending_snapshots().lock().unwrap();
    if pending.contains_key(&direct_snapshot::destination(Path::new(""), &saved.root)) {
        return Err(Error::Contract("snapshot is currently being saved"));
    }
    Ok(pending)
}

// One running save and at most two queued immutable trees. Their memory remains
// charged to the normal governor while another source is being consulted.
fn queue_save(job: SaveJob) -> Result<()> {
    static QUEUE: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<SaveJob>>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, receiver) = sync_channel::<SaveJob>(2);
        std::thread::Builder::new()
            .name("bunny-snapshot-writer".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })
            .ok()
            .map(|_| sender)
    });
    queue
        .as_ref()
        .ok_or(Error::Contract("inventory save worker unavailable"))?
        .try_send(job)
        .map_err(|_| Error::Contract("inventory save queue unavailable or full"))
}

#[cfg(all(test, any(windows, target_os = "linux")))]
pub(super) fn pause_snapshot_writer_for_test() -> Result<(Receiver<()>, std::sync::mpsc::Sender<()>)>
{
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    queue_save(Box::new(move || {
        let _ = entered.send(());
        // Dropping the release handle also releases the queue on test failure.
        let _ = wait.recv();
    }))?;
    Ok((ready, release))
}

#[derive(Default)]
struct ViewWork {
    calls: u64,
    elapsed_ns: u128,
    max_ns: u128,
    failures: u64,
}

impl ViewWork {
    fn record(&mut self, started: Instant, failed: bool) {
        let ns = started.elapsed().as_nanos();
        self.calls = self.calls.saturating_add(1);
        self.elapsed_ns = self.elapsed_ns.saturating_add(ns);
        self.max_ns = self.max_ns.max(ns);
        self.failures = self.failures.saturating_add(u64::from(failed));
    }
}

#[derive(Default)]
struct ViewMetrics {
    read_try_calls: u64,
    read_contended: u64,
    snapshot: ViewWork,
    prepare: ViewWork,
}

impl ViewMetrics {
    // Snapshot includes temporary-spool reads while the tree read lock is held.
    // Prepare follows after unlocking. These phases exclude UI application and
    // are cumulative up to terminal publication, not additional scan wall time.
    fn report(&self, source_epoch: u64, outcome: &'static str) {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr().lock(),
            "P1_DIRECT_VIEW source_epoch={source_epoch} outcome={outcome} read_try_calls={} read_contended={} snapshot_calls={} snapshot_ns={} snapshot_max_ns={} snapshot_failures={} prepare_calls={} prepare_ns={} prepare_max_ns={} prepare_failures={}",
            self.read_try_calls,
            self.read_contended,
            self.snapshot.calls,
            self.snapshot.elapsed_ns,
            self.snapshot.max_ns,
            self.snapshot.failures,
            self.prepare.calls,
            self.prepare.elapsed_ns,
            self.prepare.max_ns,
            self.prepare.failures,
        );
    }
}

pub(super) struct DirectActive {
    scan: reload::ViewScan,
    context: AcquisitionContext,
    source_epoch: u64,
    publication_cancel: CancellationToken,
    root: PathBuf,
    workspace: PathBuf,
    options: sortsize_store::scheduler::CollectionOptions,
    started: SystemTime,
    measured: Option<SystemTime>,
    terminal_reported: bool,
    saved: bool,
    saved_path: Option<PathBuf>,
    autosave_attempted: bool,
    saving: Option<Saving>,
    epoch: u64,
    parent: EntryId,
    after: Option<EntryId>,
    native_previous: Vec<Option<EntryId>>,
    observation_pending: bool,
    observation_published: bool,
    expanded: BTreeMap<EntryId, Option<EntryId>>,
    last: Option<(Instant, Revision)>,
    force: bool,
    presentation: Option<(crate::presentation::Context, crate::units::Units)>,
    view_metrics: Option<ViewMetrics>,
    sort_index: i32,
    owner_labels: Vec<sortsize_store::inventory::ordered::OwnerLabel>,
    detail_query: Option<query::DetailQuery>,
    branch_queries: BTreeMap<EntryId, branches::BranchQuery>,
    branch_cursor: Option<EntryId>,
    search: Option<query::SearchWork>,
}

impl DirectActive {
    pub(super) fn open(
        source_epoch: u64,
        start: &Start,
        publication_cancel: CancellationToken,
        #[cfg(test)] pacing: Option<sortsize_store::direct::TestBatchPacing>,
    ) -> Result<Option<Self>> {
        let guard = Guard::open(
            &start.root,
            Policy {
                explicit_root: start.explicit_root,
                ..Default::default()
            },
        )?;
        if guard.evidence().0.class != sortsize_core::acquisition::SourceClass::Local {
            return Ok(None);
        }
        let location = work_location()?;
        let guard = location
            .scope(guard)?
            .with_scan_observation(start.observation);
        let scratch = location.temporary("bunny-direct-view-")?;
        let limit = native_free_disk(scratch.path())
            .map(disk_limit)
            .ok_or(Error::Contract("working volume free space unavailable"))?;
        let root = guard.root().to_owned();
        let started = SystemTime::now();
        let context = fresh_context(source_epoch, &root, started, start.observation)?;
        if publication_cancel.is_cancelled() {
            return Err(Error::Contract("direct scan cancelled during preparation"));
        }
        #[cfg(not(test))]
        let scan = DirectScan::start(guard, start.options, scratch, limit)?;
        #[cfg(test)]
        let scan = if let Some(pacing) = pacing {
            DirectScan::start_with_test_pacing(guard, start.options, scratch, limit, pacing)?
        } else {
            DirectScan::start(guard, start.options, scratch, limit)?
        };
        Ok(Some(Self {
            scan: reload::ViewScan::Live(scan),
            context,
            source_epoch,
            publication_cancel,
            root,
            workspace: location.path().to_owned(),
            options: start.options,
            started,
            measured: None,
            terminal_reported: false,
            saved: false,
            saved_path: None,
            autosave_attempted: false,
            saving: None,
            epoch: source_epoch,
            parent: EntryId(0),
            after: None,
            native_previous: Vec::new(),
            observation_pending: false,
            observation_published: false,
            expanded: BTreeMap::from([(EntryId(0), None)]),
            last: None,
            force: true,
            presentation: None,
            view_metrics: scan_p1::enabled().then(ViewMetrics::default),
            sort_index: 0,
            owner_labels: Vec::new(),
            detail_query: None,
            branch_queries: BTreeMap::new(),
            branch_cursor: None,
            search: None,
        }))
    }

    /// Keep the dated backend consultable after a replacement attempt failed.
    /// Its original acquisition is revoked; only the view gets the new UI epoch.
    pub(super) fn restore_previous(&mut self, source_epoch: u64, view_epoch: u64, lane: &mut Lane) {
        self.scan.cancel();
        self.source_epoch = source_epoch;
        self.epoch = view_epoch;
        self.publication_cancel = CancellationToken::default();
        self.terminal_reported = true;
        self.force = true;
        self.search = None;
        let failure = lane
            .phase
            .map(|(_, phase, _)| phase)
            .unwrap_or(Phase::Failed);
        self.attach(lane);
        self.set_phase(lane, failure);
        lane.acquisition_control = None;
        lane.acquisition_publication = None;
    }

    pub(super) fn pause(&self) {
        self.scan.cancel();
    }

    pub(super) fn root_path(&self) -> &Path {
        &self.root
    }

    pub(super) fn is_saved(&self) -> bool {
        self.saved
    }

    pub(super) fn owns_snapshot(
        &self,
        saved: &sortsize_store::inventory::catalogue::Saved,
    ) -> bool {
        cache::is_snapshot(saved)
            && direct_snapshot::destination(Path::new(""), &self.root)
                == direct_snapshot::destination(Path::new(""), &saved.root)
    }

    pub(super) fn restore_interrupted(
        &mut self,
        epoch: u64,
        relative: &Path,
        cancel: CancellationToken,
    ) -> Result<()> {
        self.scan.cancel();
        self.source_epoch = epoch;
        self.epoch = epoch;
        self.publication_cancel = cancel;
        self.terminal_reported = false;
        self.force = true;
        self.search = None;
        self.detail_query = None;
        self.parent = EntryId(0);
        self.after = None;
        self.native_previous.clear();
        let tree = self
            .scan
            .tree()
            .read()
            .map_err(|_| Error::Contract("direct view unavailable"))?;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                continue;
            };
            let mut after = None;
            loop {
                let page = tree.children(self.parent, after, BRANCH_LIMIT, true)?;
                if let Some(row) = page.rows.iter().find(|r| r.path.file_name() == Some(name)) {
                    self.parent = row.id;
                    break;
                }
                if !page.more {
                    return Err(Error::Contract("recorded directory is unavailable"));
                }
                after = page.next;
            }
        }
        Ok(())
    }

    pub(super) fn attach(&mut self, l: &mut Lane) {
        self.epoch = l.view_epoch;
        l.direct_view = Some(self.source_epoch);
        l.reader_epoch = None;
        l.reader_target = Some(None);
        l.reader_active = false;
        l.acquisition_root = Some(self.root.clone());
        l.acquisition_control = Some((
            self.source_epoch,
            self.context.generation,
            self.scan.control(),
        ));
        l.acquisition_publication = Some((self.root.clone(), self.publication_cancel.clone()));
        l.scan_progress = Some(scan_p3::Progress {
            source_epoch: self.source_epoch,
            processed_entries: self.scan.progress_counter(),
        });
        self.set_phase(
            l,
            match self.scan.status() {
                DirectStatus::Running => Phase::Acquiring,
                DirectStatus::Finished => Phase::Finalizing,
                DirectStatus::Cancelled => Phase::Cancelled,
                DirectStatus::Failed(_) => Phase::Failed,
            },
        );
        self.sort_index = i32::from(l.detail_sort);
    }

    fn set_phase(&self, l: &mut Lane, phase: Phase) {
        let running = matches!(
            phase,
            Phase::Acquiring | Phase::Finalizing | Phase::Certifying
        );
        l.phase = Some((self.source_epoch, phase, self.context.generation));
        l.writer_epoch = running.then_some(self.source_epoch);
        l.writer_running = running;
        l.writer_owns_session = running;
    }

    pub(super) fn retired(&self, l: &Lane) -> bool {
        l.source_epoch != self.source_epoch || self.epoch <= l.retired_through
    }

    pub(super) fn detach(&self, l: &mut Lane) {
        if l.direct_view == Some(self.source_epoch) {
            l.direct_view = None;
            l.fast_preview_context = None;
            l.cancel_fast_save();
            l.fast_save_cancel = None;
            l.acquisition_control = None;
            l.acquisition_publication = None;
            if l.writer_epoch == Some(self.source_epoch) {
                l.writer_epoch = None;
                l.writer_running = false;
                l.writer_owns_session = false;
            }
        }
    }

    pub(super) fn report_view_error(&mut self, l: &mut Lane) {
        if !self.retired(l)
            && !self.terminal_reported
            && self.scan.status() != DirectStatus::Running
        {
            self.terminal_reported = true;
            self.set_phase(l, Phase::Failed);
        }
    }

    pub(super) fn turn(
        &mut self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<Output>>,
        cache_output: &Mutex<Option<cache::Prepared>>,
        search_output: &Mutex<Option<search::Prepared>>,
        navigation: Option<(u64, Navigate)>,
        units: crate::units::Units,
    ) -> Result<()> {
        let presentation = (crate::presentation::Context::current(), units);
        if self.presentation != Some(presentation) {
            self.presentation = Some(presentation);
            self.force = true;
        }
        {
            let l = lane.lock().unwrap();
            if !self.scan.is_loaded()
                && (self.publication_cancel.is_cancelled() || source_disconnected(&l, &self.root))
            {
                self.scan.cancel();
            }
        }
        let (desired, epoch) = {
            let l = lane.lock().unwrap();
            (l.desired_folder, l.view_epoch)
        };
        if let Some(key) = desired.filter(|key| {
            key.inventory == self.context.inventory && key.generation == self.context.generation
        }) && key.entry != self.parent
        {
            // Tree-only actions may coalesce over a pending folder request.
            // Keep the inspected folder intention independent of that action.
            self.navigate(epoch, Navigate::Open(key))?;
        }
        if let Some((epoch, action)) = navigation {
            self.navigate(epoch, action)?;
        }
        // Mailbox consumption is distinct from the lifetime of the page's
        // publication ticket, and does not imply that the UI has painted it.
        if self.observation_published && output.lock().unwrap().is_none() {
            self.observation_pending = false;
            self.observation_published = false;
        }
        if let Err(error) = self.query_turn(lane, search_output, units) {
            if let Some(work) = self.search.take() {
                *search_output.lock().unwrap() = Some(search::Prepared::error(
                    work.request_id(),
                    error.to_string(),
                ));
            } else {
                *output.lock().unwrap() = Some(Output::Error(
                    self.epoch,
                    error.to_string(),
                    self.last.is_some(),
                ));
            }
            // A query refusal must never stop acquisition, autosave or browsing.
            self.detail_query = None;
            notify(lane);
        }
        self.poll_save(lane, output, cache_output);
        self.begin_save(lane, output, cache_output);

        let status = self.scan.status();
        let cancelled = self.publication_cancel.is_cancelled();
        let running = matches!(status, DirectStatus::Running);
        let failed = match &status {
            DirectStatus::Failed(message) if !cancelled => Some(message.to_string()),
            _ => None,
        };
        let finished = matches!(status, DirectStatus::Finished);
        if finished && self.measured.is_none() {
            self.measured = Some(SystemTime::now());
            let mut l = lane.lock().unwrap();
            if !self.retired(&l) {
                l.fast_preview_context = (!self.saved).then_some(self.context);
                self.set_phase(&mut l, Phase::Finalizing);
            }
            drop(l);
            // Completion and its snapshot do not wait for a frozen UI page
            // or for room to construct the next presentation.
            self.begin_save(lane, output, cache_output);
        }
        let terminal = !running && !self.terminal_reported;
        if let Some(message) = failed.filter(|_| terminal) {
            let mut l = lane.lock().unwrap();
            if !self.retired(&l) {
                self.terminal_reported = true;
                self.set_phase(&mut l, Phase::Failed);
                *output.lock().unwrap() =
                    Some(Output::Error(l.view_epoch, message, self.last.is_some()));
            }
            drop(l);
            notify(lane);
            if let Some(metrics) = self.view_metrics.take() {
                metrics.report(self.source_epoch, "failed");
            }
            return Ok(());
        }
        if terminal && !finished {
            let mut l = lane.lock().unwrap();
            if !self.retired(&l)
                && !l.phase.is_some_and(|(epoch, phase, _)| {
                    epoch == self.source_epoch && matches!(phase, Phase::Cancelled)
                })
            {
                l.cancelled_unpublished = None;
                self.set_phase(&mut l, Phase::Cancelled);
                drop(l);
                notify(lane);
            }
        }
        // Preserve an error for the UI and defer expensive preparation while
        // it occupies the mailbox. A later error can still race preparation,
        // so the final publication check below remains necessary.
        if matches!(output.lock().unwrap().as_ref(), Some(Output::Error(epoch, ..)) if *epoch >= self.epoch)
        {
            return Ok(());
        }
        if !terminal
            && !self.force
            && self
                .last
                .is_some_and(|(last, _)| last.elapsed() < PREVIEW_INTERVAL)
        {
            return Ok(());
        }

        let tree_owner = self.scan.tree().clone();
        if let Some(metrics) = &mut self.view_metrics {
            metrics.read_try_calls = metrics.read_try_calls.saturating_add(1);
        }
        let tree = match tree_owner.try_read() {
            Ok(tree) => tree,
            Err(std::sync::TryLockError::WouldBlock) => {
                if let Some(metrics) = &mut self.view_metrics {
                    metrics.read_contended = metrics.read_contended.saturating_add(1);
                }
                return Ok(());
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(Error::Contract("direct view unavailable"));
            }
        };
        let revision = Revision(tree.revision());
        if !terminal && !self.force && self.last.is_some_and(|(_, prior)| prior == revision) {
            return Ok(());
        }
        // A page taken out of the mailbox can still be replacing the UI's
        // current page. Both remain charged until Host::apply finishes. Keep
        // at most those two publications; an empty mailbox alone is not an
        // acknowledgement that the previous page has been released.
        let obsolete = {
            let mut slot = output.lock().unwrap();
            if matches!(slot.as_ref(), Some(Output::Page(page)) if page.epoch <= self.epoch) {
                slot.take()
            } else {
                None
            }
        };
        drop(obsolete);
        {
            let mut l = lane.lock().unwrap();
            l.direct_publications
                .retain(|ticket| ticket.strong_count() != 0);
            if l.direct_publications.len() >= 2 {
                return Ok(());
            }
        }

        let started = self.view_metrics.as_ref().map(|_| Instant::now());
        let scope = lane.lock().unwrap().top_scope.clone();
        let snapshot = self.snapshot(&tree, scope, running);
        if let Some((metrics, started)) = self.view_metrics.as_mut().zip(started) {
            metrics.snapshot.record(started, snapshot.is_err());
        }
        let snapshot = snapshot?;
        drop(tree);
        let started = self.view_metrics.as_ref().map(|_| Instant::now());
        let page = self.prepare(snapshot, revision, running, finished, units);
        if let Some((metrics, started)) = self.view_metrics.as_mut().zip(started) {
            metrics.prepare.record(started, page.is_err());
        }
        let page = page?;
        let mut l = lane.lock().unwrap();
        if self.retired(&l) || l.view_epoch != page.epoch {
            return Ok(());
        }
        if terminal {
            self.terminal_reported = true;
            let phase = if finished {
                l.fast_preview_context = (!self.saved).then_some(self.context);
                if self.saved {
                    Phase::Ready
                } else if self.saving.is_some() {
                    Phase::Saving
                } else {
                    Phase::PreviewReady
                }
            } else {
                l.cancelled_unpublished = None;
                Phase::Cancelled
            };
            self.set_phase(&mut l, phase);
        }
        drop(l);
        if terminal && finished {
            self.begin_save(lane, output, cache_output);
        }
        let mut l = lane.lock().unwrap();
        if self.retired(&l) || l.view_epoch != page.epoch {
            return Ok(());
        }
        let mut pending = output.lock().unwrap();
        let published = if !matches!(pending.as_ref(), Some(Output::Error(epoch, ..)) if *epoch >= page.epoch)
        {
            l.direct_publications.push(Arc::downgrade(
                page._direct_publication
                    .as_ref()
                    .expect("direct publication ticket"),
            ));
            *pending = Some(Output::Page(Box::new(page)));
            if self.observation_pending {
                self.observation_published = true;
            }
            true
        } else {
            false
        };
        record_publication(&mut self.force, &mut self.last, revision, published);
        drop(pending);
        drop(l);
        notify(lane);
        if terminal && let Some(metrics) = self.view_metrics.take() {
            metrics.report(
                self.source_epoch,
                if finished { "finished" } else { "cancelled" },
            );
        }
        Ok(())
    }

    fn navigate(&mut self, epoch: u64, action: Navigate) -> Result<()> {
        if epoch < self.epoch {
            return Ok(());
        }
        self.epoch = epoch;
        self.force = true;
        self.observation_pending = matches!(action, Navigate::ObservedPage(_));
        self.observation_published = false;
        let tree_owner = self.scan.tree().clone();
        let tree = tree_owner
            .read()
            .map_err(|_| Error::Contract("direct view unavailable"))?;
        if self.query_navigation(&action, &tree)? {
            return Ok(());
        }
        let old_parent = self.parent;
        let valid = |key: &EntryKey| {
            key.inventory == self.context.inventory && key.generation == self.context.generation
        };
        match action {
            Navigate::Open(key) | Navigate::Reveal(key) if valid(&key) => {
                let row = tree.row(key.entry)?;
                let parent = if row.kind == Kind::Directory {
                    row.id
                } else if matches!(action, Navigate::Reveal(_)) {
                    row.parent.unwrap_or(EntryId(0))
                } else {
                    return Ok(());
                };
                self.parent = parent;
                self.after = None;
                self.native_previous.clear();
                let mut child = parent;
                for _ in 0..folder_tree::MAX_BRANCHES {
                    let Some(ancestor) = tree.parent(child)? else {
                        break;
                    };
                    if self.expanded.contains_key(&ancestor)
                        || self.expanded.len() < folder_tree::MAX_BRANCHES
                    {
                        self.expanded.insert(ancestor, None);
                        self.reveal_branch(&tree, ancestor, child)?;
                    }
                    child = ancestor;
                }
            }
            Navigate::Parent => {
                if let Some(parent) = tree.parent(self.parent)? {
                    self.parent = parent;
                    self.after = None;
                }
            }
            Navigate::ToggleTree(key) if valid(&key) => {
                if self.expanded.remove(&key.entry).is_none()
                    && self.expanded.len() < folder_tree::MAX_BRANCHES
                {
                    self.expanded.insert(key.entry, None);
                }
            }
            Navigate::MoreTree(parent) | Navigate::PreviousTree(parent) if valid(&parent) => {
                self.page_branch(&tree, parent.entry, matches!(action, Navigate::MoreTree(_)))?;
            }
            Navigate::Page(forward) | Navigate::ObservedPage(forward) => {
                if forward {
                    let page = tree.children(self.parent, self.after, DETAIL_LIMIT, false)?;
                    if let Some(next) = page.next {
                        if self.native_previous.len() == 128 {
                            self.native_previous.remove(0);
                        }
                        self.native_previous.push(self.after);
                        self.after = Some(next);
                    }
                } else if let Some(previous) = self.native_previous.pop() {
                    self.after = previous;
                }
            }
            _ => {}
        }
        if self.parent != old_parent {
            self.detail_query = None;
            self.native_previous.clear();
        }
        Ok(())
    }

    fn snapshot(
        &self,
        tree: &DirectTree,
        scope: Option<super::service::TopScope>,
        running: bool,
    ) -> Result<Snapshot> {
        let page = detail_page(
            tree,
            self.parent,
            self.after,
            self.detail_query.as_ref(),
            self.scan.status() == DirectStatus::Running,
        )?;
        let parent = tree.measure(self.parent)?;
        let root = tree.summary();
        let mut bytes = 0usize;
        let mut branches = Vec::new();
        self.tree_branch(tree, tree.row(EntryId(0))?, 0, &mut branches, &mut bytes)?;
        let details = page
            .rows
            .into_iter()
            .map(|row| {
                let measure = (row.kind == Kind::Directory)
                    .then(|| tree.measure(row.id))
                    .transpose()?;
                Ok((row, measure))
            })
            .collect::<Result<Vec<_>>>()?;
        let (top, top_coverage, top_byte_limited) = match scope.as_ref() {
            None => (tree.top()?, root.coverage, false),
            Some(scope)
                if !running
                    && scope.key.inventory == self.context.inventory
                    && scope.key.generation == self.context.generation =>
            {
                let (rows, limited) = tree.top_within(scope.key.entry)?;
                (rows, tree.coverage(scope.key.entry)?, limited)
            }
            // Do not walk the tree under its read lock while the scanner is
            // still writing batches. Its final publication fills this scope.
            Some(_) => (Vec::new(), ScopeCoverage::Partial, false),
        };
        Ok(Snapshot {
            root,
            parent,
            parent_path: tree.path(self.parent)?,
            more: page.more,
            details,
            top,
            top_coverage,
            top_byte_limited,
            top_scope: scope.map(|scope| scope.path),
            branches,
        })
    }

    fn tree_branch(
        &self,
        tree: &DirectTree,
        row: DirectRow,
        depth: usize,
        branches: &mut Vec<BranchRow>,
        bytes: &mut usize,
    ) -> Result<()> {
        if depth >= folder_tree::MAX_BRANCHES
            || branches.len() >= TREE_LIMIT
            || *bytes == 512 * 1024
        {
            return Ok(());
        }
        let cost = row.path.capacity() + 256;
        if branches.len() == TREE_LIMIT - 1 || *bytes + cost > 512 * 1024 {
            branches.push(BranchRow::Limit(depth));
            *bytes = 512 * 1024;
            return Ok(());
        }
        *bytes += cost;
        let id = row.id;
        let measure = tree.measure(id)?;
        let parent_logical = match row.parent {
            Some(parent) => tree.measure(parent)?.logical,
            None => measure.logical,
        };
        let expanded = self.expanded.contains_key(&id);
        branches.push(BranchRow::Folder {
            id,
            parent: row.parent,
            path: row.path,
            depth,
            measure,
            parent_logical,
            expanded,
        });
        if self.expanded.contains_key(&id) {
            let (page, previous) = self.branch_page(tree, id)?;
            if previous && branches.len() < TREE_LIMIT {
                branches.push(BranchRow::Previous(id, depth + 1));
            }
            for child in page.rows {
                self.tree_branch(tree, child, depth + 1, branches, bytes)?;
            }
            if page.more && branches.len() < TREE_LIMIT {
                branches.push(BranchRow::More(id, depth + 1));
            }
        }
        Ok(())
    }
}

impl Drop for DirectActive {
    fn drop(&mut self) {
        self.scan.cancel();
        if let Some(saving) = &self.saving
            && saving.request.id != 0
        {
            saving.request.cancel.cancel();
        }
    }
}

fn record_publication(
    force: &mut bool,
    last: &mut Option<(Instant, Revision)>,
    revision: Revision,
    published: bool,
) {
    if published {
        *force = false;
        *last = Some((Instant::now(), revision));
    } else {
        // The terminal transition may already have been acknowledged.
        *force = true;
    }
}

fn detail_page(
    tree: &DirectTree,
    parent: EntryId,
    after: Option<EntryId>,
    query: Option<&query::DetailQuery>,
    running: bool,
) -> Result<DirectPage> {
    if !running
        && let Some(query) = query
        && let Some(job) = if query.job.done() {
            Some(&query.job)
        } else {
            query.retained.as_ref()
        }
    {
        return Ok(DirectPage {
            rows: job.rows(tree)?,
            more: query.job.done() && job.more(),
            next: None,
        });
    }
    tree.children(parent, after, DETAIL_LIMIT, false)
}

struct Snapshot {
    root: DirectSummary,
    parent: FolderMeasure,
    parent_path: PathBuf,
    more: bool,
    details: Vec<(DirectRow, Option<FolderMeasure>)>,
    top: Vec<DirectRow>,
    top_coverage: ScopeCoverage,
    top_byte_limited: bool,
    top_scope: Option<Arc<PathBuf>>,
    branches: Vec<BranchRow>,
}

enum BranchRow {
    Folder {
        id: EntryId,
        parent: Option<EntryId>,
        path: PathBuf,
        depth: usize,
        measure: FolderMeasure,
        parent_logical: Option<u64>,
        expanded: bool,
    },
    More(EntryId, usize),
    Previous(EntryId, usize),
    Limit(usize),
}

impl DirectActive {
    fn take_save_continuation(&self, lane: &mut Lane) -> Option<cache::Request> {
        cache::discard_obsolete_request(lane);
        if self.retired(lane) {
            return None;
        }
        let eligible = lane.cache.as_ref().is_some_and(|request| {
            request.epoch >= self.source_epoch
                && !request.cancel.is_cancelled()
                && match request.action {
                    cache::Action::Save => true,
                    cache::Action::SaveFast {
                        source_epoch,
                        context,
                    } => source_epoch == self.source_epoch && context == self.context,
                    _ => false,
                }
        });
        eligible.then(|| lane.cache.take().unwrap())
    }

    fn begin_save(
        &mut self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<Output>>,
        cache_output: &Mutex<Option<cache::Prepared>>,
    ) {
        let Some(measured) = self.measured else {
            return;
        };
        if self.saving.is_some() || self.saved {
            return;
        }
        let (request, continuation, root) = {
            let mut l = lane.lock().unwrap();
            if self.retired(&l)
                || self.publication_cancel.is_cancelled()
                || l.fast_preview_context != Some(self.context)
            {
                return;
            }
            let continuation = self.take_save_continuation(&mut l);
            if self.autosave_attempted && continuation.is_none() {
                return;
            }
            // The immutable completed acquisition owns its background save.
            // A UI Save only continues into explicit retention afterwards; its
            // cancellation or a source switch must not cancel this snapshot.
            self.autosave_attempted = true;
            let request = cache::Request {
                id: 0,
                epoch: l.view_epoch,
                expected: l.view_epoch,
                action: cache::Action::SaveFast {
                    source_epoch: self.source_epoch,
                    context: self.context,
                },
                cancel: CancellationToken::default(),
            };
            self.set_phase(&mut l, Phase::Saving);
            (request, continuation, l.cache_root.clone())
        };
        let cancel = request.cancel.clone();
        let presentation = crate::presentation::Context::current();
        let context = self.context;
        let guard = match self.scan.guard() {
            Ok(guard) => guard,
            Err(error) => {
                self.save_error(
                    lane,
                    output,
                    cache_output,
                    continuation.unwrap_or(request),
                    error,
                );
                return;
            }
        };
        let scratch_owner = self.scan.scratch();
        let tree = self.scan.tree().clone();
        let location = match work_location() {
            Ok(location) => location,
            Err(error) => {
                self.save_error(
                    lane,
                    output,
                    cache_output,
                    continuation.unwrap_or(request),
                    error,
                );
                return;
            }
        };
        let root = cache::snapshot_root(&root.unwrap_or_else(|| location.catalogue()));
        let started = cache::seconds(self.started);
        let measured = cache::seconds(measured);
        let options = self.options;
        let (autosave_revision, autosave_error, completion_notify) = {
            let l = lane.lock().unwrap();
            (
                l.autosave_revision.clone(),
                l.autosave_error.clone(),
                l.notify.clone(),
            )
        };
        let (sender, result) = sync_channel(1);
        let pending_snapshot = PendingSnapshot::new(guard.root());
        let worker = queue_save(Box::new(move || {
            let _pending_snapshot = pending_snapshot;
            let _scope = presentation.enter();
            let _scratch_owner = scratch_owner;
            let begin = Instant::now();
            let saved = (|| {
                require_work_location(&location, &root)?;
                if !guard.work_path_allowed(&root)? {
                    return Err(Error::Contract(
                        "cache overlaps the acquired source outside its verified workspace",
                    ));
                }
                lifecycle::ensure_root(&root)?;
                let root = std::fs::canonicalize(root)?;
                let scratch = location.temporary("bunny-direct-save-")?;
                let destination = direct_snapshot::destination(&root, guard.root());
                let (batches, summary) = {
                    let tree = tree
                        .read()
                        .map_err(|_| Error::Contract("direct view unavailable"))?;
                    (tree.replay()?, tree.summary())
                };
                direct_snapshot::save(
                    SaveRequest {
                        context,
                        guard: &guard,
                        scratch: scratch.path(),
                        destination: &destination,
                        started,
                        measured,
                        options,
                        coverage: summary.coverage,
                    },
                    summary,
                    batches,
                    &cancel,
                )
            })();
            if scan_p1::enabled() {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "scan_direct_save saved={} wall_us={}",
                    saved.is_ok(),
                    begin.elapsed().as_micros()
                );
            }
            sortsize_store::report_scan_metrics("direct_save");
            if saved.is_ok() {
                autosave_revision.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                if let Some(notify) = &completion_notify {
                    notify();
                }
            }
            if let Err(error) = &saved
                && !cancel.is_cancelled()
            {
                *autosave_error.lock().unwrap() = Some(format!(
                    "{}: {} — {}",
                    tr!("Saved measure operation failed."),
                    short_label(guard.root()),
                    error.to_string().chars().take(4096).collect::<String>()
                ));
                if let Some(notify) = completion_notify {
                    notify();
                }
            }
            let _ = sender.send(saved);
        }));
        match worker {
            Ok(_) => {
                self.saving = Some(Saving {
                    request,
                    continuation,
                    result,
                });
                self.force = true;
            }
            Err(error) => self.save_error(
                lane,
                output,
                cache_output,
                continuation.unwrap_or(request),
                error,
            ),
        }
        notify(lane);
    }

    fn poll_save(
        &mut self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<Output>>,
        cache_output: &Mutex<Option<cache::Prepared>>,
    ) {
        if self.saving.as_ref().is_some_and(|saving| {
            saving
                .continuation
                .as_ref()
                .is_none_or(|r| r.cancel.is_cancelled())
        }) {
            let continuation = self.take_save_continuation(&mut lane.lock().unwrap());
            self.saving.as_mut().unwrap().continuation = continuation;
        }
        let Some(saving) = &self.saving else { return };
        let result = match saving.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                Err(Error::Contract("inventory save worker stopped"))
            }
        };
        let Saving {
            request,
            continuation,
            ..
        } = self.saving.take().unwrap();
        let saved = match result {
            Ok(saved) => saved,
            Err(error) => {
                self.save_error(
                    lane,
                    output,
                    cache_output,
                    continuation.unwrap_or(request),
                    error,
                );
                return;
            }
        };
        {
            let mut l = lane.lock().unwrap();
            if self.retired(&l) {
                return;
            }
            self.saved = true;
            self.saved_path = Some(saved.path.clone());
            self.force = true;
            l.fast_save_cancel = None;
            l.fast_preview_context = None;
            // The recorded tree may have been restored after a newer refresh
            // failed or was cancelled. Its old save cannot complete that attempt.
            let restored_failure = l.phase.and_then(|(epoch, phase, _)| {
                (epoch == self.source_epoch && matches!(phase, Phase::Failed | Phase::Cancelled))
                    .then_some(phase)
            });
            if restored_failure.is_none() {
                l.cancel_requested_epoch = None;
            }
            self.set_phase(&mut l, restored_failure.unwrap_or(Phase::Ready));
            if let Some(mut continuation) = continuation
                && !continuation.cancel.is_cancelled()
                && l.cache.is_none()
            {
                if matches!(continuation.action, cache::Action::Save) {
                    // Execute the original archive request, with its original
                    // id/token, only after a verified snapshot is available.
                    continuation.epoch = l.view_epoch;
                    continuation.expected = l.view_epoch;
                    l.cache = Some(continuation);
                } else {
                    *cache_output.lock().unwrap() =
                        Some(cache::error(&continuation, Phase::Ready.label().to_owned()));
                }
            }
        }
        let _saved_path = saved.path;
        notify(lane);
    }

    fn save_error(
        &mut self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<Output>>,
        cache_output: &Mutex<Option<cache::Prepared>>,
        request: cache::Request,
        error: Error,
    ) {
        let mut l = lane.lock().unwrap();
        if self.retired(&l) || source_disconnected(&l, &self.root) {
            return;
        }
        l.fast_save_cancel = None;
        let restored_failure = l.phase.and_then(|(epoch, phase, _)| {
            (epoch == self.source_epoch && matches!(phase, Phase::Failed | Phase::Cancelled))
                .then_some(phase)
        });
        if restored_failure.is_none() {
            l.cancel_requested_epoch = None;
        }
        self.set_phase(&mut l, restored_failure.unwrap_or(Phase::PreviewReady));
        self.force = true;
        if !request.cancel.is_cancelled() {
            let message = acquisition_error_message(error);
            *output.lock().unwrap() = Some(Output::Error(l.view_epoch, message.clone(), true));
            *cache_output.lock().unwrap() = Some(cache::error(&request, message));
        }
        drop(l);
        notify(lane);
    }
}

impl DirectActive {
    fn prepare(
        &self,
        snapshot: Snapshot,
        revision: Revision,
        busy: bool,
        finished: bool,
        units: crate::units::Units,
    ) -> Result<Prepared> {
        let whole = snapshot.root.measure;
        let coverage = if finished {
            snapshot.root.coverage
        } else {
            ScopeCoverage::Partial
        };
        let partial = coverage != ScopeCoverage::Complete;
        // A positive subtotal remains useful when some sizes are unknown.
        // A zero subtotal with unknown observations does not measure an
        // empty scope.
        let displayed_logical = |measure: FolderMeasure| {
            measure.logical.filter(|&bytes| {
                !(cfg!(target_os = "linux") && finished && bytes == 0 && measure.unknown > 0)
            })
        };
        let key = |entry| EntryKey {
            inventory: self.context.inventory,
            generation: self.context.generation,
            entry,
        };
        let to_row = |row: &DirectRow| Row {
            key: key(row.id),
            path: row.path.clone(),
            kind: row.kind,
            logical: row.logical,
        };
        let stamp = |id| ViewStamp {
            context: view_context(self.context, self.epoch, id),
            revision,
        };
        let frame = Frame::from_direct(
            stamp(1),
            snapshot
                .details
                .iter()
                .map(|(row, _)| to_row(row))
                .collect(),
            snapshot.more,
            !finished,
            coverage,
        )?;
        let mut top_bytes = 1024;
        let rows: Vec<_> = snapshot
            .top
            .iter()
            .take_while(|row| {
                top_bytes += row.path.capacity() + std::mem::size_of::<Row>() + 128;
                top_bytes < PUBLICATION_BYTES / 2
            })
            .map(to_row)
            .collect();
        let top_coverage = if snapshot.top_byte_limited || rows.len() < snapshot.top.len() {
            ScopeCoverage::Partial
        } else {
            snapshot.top_coverage
        };
        let top = Frame::from_direct(stamp(2), rows, false, !finished, top_coverage)?;
        // Charge the displayed name and path of each model. The detail rows
        // may also appear in the treemap with unclipped labels. Include fixed
        // room for the model copies, cells, and their strings.
        let detail_bytes = frame
            .rows
            .iter()
            .map(|row| {
                2048 + label_pair_bytes(&row.path, false) + label_pair_bytes(&row.path, true)
            })
            .sum::<usize>();
        let top_bytes = top
            .rows
            .iter()
            .map(|row| 1024 + label_pair_bytes(&row.path, true))
            .sum::<usize>();
        let tree_bytes = snapshot
            .branches
            .iter()
            .map(|branch| match branch {
                BranchRow::Folder { path, .. } => {
                    1024 + path.capacity() + label_pair_bytes(path, false)
                }
                _ => 512,
            })
            .sum::<usize>();
        let memory = Governor::global()
            .reserve(
                Category::Publications,
                (64 * 1024 + detail_bytes + top_bytes) as u64,
                None,
            )
            .ok_or(Error::Contract("direct presentation memory unavailable"))?;
        let folder_memory = Governor::global()
            .reserve(
                Category::Publications,
                (16 * 1024 + tree_bytes) as u64,
                None,
            )
            .ok_or(Error::Contract(
                "direct tree presentation memory unavailable",
            ))?;
        let mut details = Vec::with_capacity(snapshot.details.len());
        let mut owners = Vec::with_capacity(snapshot.details.len());
        let mut tiles = Vec::new();
        let mut weights = Vec::new();
        let mut band = 0.;
        let shown = snapshot
            .details
            .iter()
            .filter_map(|(row, _)| row.logical)
            .try_fold(0u64, u64::checked_add);
        let total = snapshot.parent.logical.or(shown);
        for (index, (row, measure)) in snapshot.details.into_iter().enumerate() {
            let folder = row.kind == Kind::Directory;
            let name = row.path.file_name().map(Path::new).unwrap_or(&row.path);
            let size = format_size(row.logical, folder && partial, units);
            let allocated = measure.map_or(row.properties.allocated, |measure| {
                measure
                    .allocated
                    .map_or(MetadataValue::Unknown, |value| MetadataValue::Known {
                        value,
                        accuracy: if partial || measure.allocation_lower_bound {
                            ValueAccuracy::LowerBound
                        } else {
                            ValueAccuracy::Exact
                        },
                    })
            });
            let (fraction, has_weight, proportion) =
                crate::tree::folder_tree_weight(row.logical, total, partial);
            owners.push(row.properties.owner);
            details.push(DetailRow {
                node_id: index as i32,
                name: short_label(name).into(),
                path: short_label(&row.path).into(),
                size: size.clone().into(),
                directory: folder,
                proportion,
                fraction,
                has_weight,
                weight_provisional: partial,
                color_index: (row.id.0 % 8) as i32,
                files: measure
                    .map_or_else(|| "—".into(), |m| m.files.to_string())
                    .into(),
                directories: measure
                    .map_or_else(|| "—".into(), |m| m.directories.to_string())
                    .into(),
                modified: crate::date_label::format_date(modified(row.modified)).into(),
                accessed: crate::date_label::format_date(row.properties.accessed).into(),
                allocated: crate::allocation_label::format_value(allocated, units).into(),
                owner: crate::owner_view::fallback(row.properties.owner.id),
                note: if partial {
                    tr!("Partial").into()
                } else {
                    "".into()
                },
            });
            if let Some(bytes) = row.logical.filter(|_| has_weight && fraction > 0.) {
                weights.push(crate::treemap::Weight {
                    target: crate::treemap::Target::Node(index),
                    bytes,
                });
                tiles.push(MapTile {
                    band_x: band,
                    band_w: fraction,
                    node_id: index as i32,
                    name: crate::tree::native_label(name).into(),
                    path: crate::tree::native_label(&row.path).into(),
                    partial,
                    size: size.into(),
                    directory: folder,
                    color_index: (row.id.0 % 8) as i32,
                    ..Default::default()
                });
                band += fraction;
            }
        }
        if let Some((total, other)) = snapshot
            .parent
            .logical
            .zip(shown)
            .and_then(|(total, shown)| total.checked_sub(shown).map(|other| (total, other)))
            && other > 0
        {
            weights.push(crate::treemap::Weight {
                target: crate::treemap::Target::Other,
                bytes: other,
            });
            let fraction = (other as f64 / total as f64) as f32;
            let partial = partial || snapshot.parent.unknown > 0;
            tiles.push(MapTile {
                band_x: band,
                band_w: fraction,
                node_id: -1,
                name: tr!("Other observed data outside this page").into(),
                path: "".into(),
                partial,
                size: format_size(Some(other), partial, units).into(),
                directory: false,
                color_index: 6,
                ..Default::default()
            });
        }
        for (tile, rect) in tiles
            .iter_mut()
            .zip(crate::treemap::layout(&weights, 1000., 350.))
        {
            tile.x = rect.x as f32;
            tile.y = rect.y as f32;
            tile.w = rect.width as f32;
            tile.h = rect.height as f32;
        }
        let (folders, folder_keys) = snapshot.branches.into_iter().enumerate().map(|(index, branch)| {
            use folder_tree::TreeTarget;
            match branch {
                BranchRow::Folder { id, parent, path, depth, measure, parent_logical, expanded } => {
                    let key = key(id);
                    let partial = partial || measure.unknown > 0;
                    let (fraction, has_weight, proportion) = crate::tree::folder_tree_weight(
                        measure.logical, parent_logical, partial,
                    );
                    let name = path.file_name().map(Path::new).unwrap_or(&path);
                    (TreeRow {
                        node_id: index as i32, depth: depth as i32,
                        name: short_label(name).into(), path: label(&path).into(),
                        size: format_size(measure.logical, partial, units).into(),
                        expandable: busy || measure.directories > 0, expanded,
                        selected: self.parent == id, fraction, has_weight, proportion,
                        weight_provisional: partial, color_index: (id.0 % 8) as i32,
                        ..Default::default()
                    }, TreeTarget::Folder { key, parent, path: Arc::new(path) })
                }
                BranchRow::More(id, depth) | BranchRow::Previous(id, depth) => {
                    let (name, target) = if matches!(branch, BranchRow::More(..)) {
                        (tr!("Load more folders"), TreeTarget::More { parent: key(id) })
                    } else {
                        (tr!("Previous folders"), TreeTarget::Previous { parent: key(id) })
                    };
                    (TreeRow {
                        node_id: index as i32, depth: depth as i32,
                        name: name.into(), ..Default::default()
                    }, target)
                }
                BranchRow::Limit(depth) => (
                    TreeRow {
                        node_id: index as i32,
                        depth: depth as i32,
                        name: tr!("Folder tree display limit reached. Collapse a branch to show more folders.").into(),
                        ..Default::default()
                    },
                    TreeTarget::Limit,
                ),
            }
        }).unzip();
        let top_rows = top
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| TopRow {
                rank: (index + 1) as i32,
                name: short_label(row.path.file_name().map(Path::new).unwrap_or(&row.path)).into(),
                size: format_size(row.logical, false, units).into(),
                path: short_label(&row.path).into(),
            })
            .collect();
        let coverage_label = if partial {
            tr!("Partial")
        } else {
            tr!("Complete")
        };
        let sorting = self
            .detail_query
            .as_ref()
            .is_none_or(|q| !q.job.done() || q.revision != revision.0);
        let status = if (sorting || self.branches_pending(revision.0)) && !busy {
            tr!("Preparing the folder order…").to_owned()
        } else if busy {
            tr!("Scan in progress…").to_owned()
        } else if self.saving.is_some() {
            format!("{} · {coverage_label}", Phase::Saving.label())
        } else if self.saved {
            format!("{} · {coverage_label}", Phase::Ready.label())
        } else if finished {
            format!("{} · {coverage_label}", Phase::PreviewReady.label())
        } else {
            coverage_label.to_owned()
        };
        Ok(Prepared {
            units,
            #[cfg(feature = "benchmarks")]
            worker_frame_ready_at: Instant::now(),
            epoch: self.epoch,
            token: publication_token()?,
            frame,
            parent_entry: self.parent,
            top,
            details,
            owners,
            folders,
            folder_keys,
            _folder_memory: Some(folder_memory),
            top_rows,
            top_scope: snapshot.top_scope,
            tiles,
            path: label(&snapshot.parent_path),
            status,
            scope_detail: if sortsize_core::acquisition::contains_path(&self.root, &self.workspace)
            {
                tr_format!(
                    "Scope excludes Bunny and Size's verified working folder: {}",
                    label(&self.workspace)
                )
            } else {
                String::new()
            },
            summary: format_size(displayed_logical(whole), partial, units),
            current_folder_summary: crate::current_folder_summary::render(
                displayed_logical(snapshot.parent),
                Some(snapshot.parent.files),
                Some(snapshot.parent.directories),
                partial || snapshot.parent.unknown > 0,
                units,
            ),
            root_measure: Some(whole),
            root_provisional: !finished,
            busy,
            parent: self.parent != EntryId(0),
            previous: self
                .detail_query
                .as_ref()
                .map_or(!self.native_previous.is_empty(), |q| {
                    if q.job.done() {
                        !q.previous.is_empty()
                    } else {
                        q.retained.is_none() && !self.native_previous.is_empty()
                    }
                }),
            quanta: revision.0,
            measure_ready: finished,
            sort_index: if sorting
                && self
                    .detail_query
                    .as_ref()
                    .is_none_or(|q| q.retained.is_none())
            {
                -1
            } else {
                self.detail_query
                    .as_ref()
                    .and_then(|q| q.retained_sort)
                    .unwrap_or(self.sort_index)
            },
            _memory: memory,
            p1_frame_ready_at: scan_p1::enabled().then(Instant::now),
            _direct_publication: Some(Arc::new(())),
        })
    }
}

fn label_pair_bytes(path: &Path, clipped: bool) -> usize {
    let label_bytes = |path: &Path| {
        let bytes = match path.to_str() {
            Some(text) if !text.chars().any(char::is_control) => text.len(),
            // Native debug escapes include quotes and can encode one byte
            // as a six-byte control escape such as \u{7f}.
            _ => 2 + 6 * path.as_os_str().len(),
        };
        if clipped { bytes.min(512 * 4) } else { bytes }
    };
    label_bytes(path) + label_bytes(path.file_name().map(Path::new).unwrap_or(path))
}

fn format_size(size: Option<u64>, lower_bound: bool, units: crate::units::Units) -> String {
    match size {
        Some(0) if lower_bound => "—".into(),
        Some(n) if lower_bound => format!("≥ {}", units.format(n)),
        Some(n) => units.format(n),
        None => "—".into(),
    }
}

fn modified(time: Option<(i64, u32)>) -> MetadataValue<SystemTime> {
    time.and_then(|(seconds, nanos)| {
        cache::system_time(seconds)
            .ok()?
            .checked_add(Duration::from_nanos(u64::from(nanos)))
    })
    .map_or(MetadataValue::Unknown, |value| MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    })
}

#[cfg(test)]
mod progressive_detail_tests {
    use super::*;
    use sortsize_core::{acquisition::NodeProperties, inventory_contract::ListingEnd};
    use sortsize_store::{
        codec::NativeName, direct_tree::query::PageQuery, inventory::Observation,
        scheduler::direct::ObservationBatch,
    };

    fn wide_tree(scratch: &Path, first_positive: bool) -> DirectTree {
        let mut tree =
            DirectTree::new(scratch.join("synthetic"), scratch, 8 * 1024 * 1024).unwrap();
        let observations = (0..202)
            .map(|index| {
                let name = format!("entry-{index:03}");
                #[cfg(windows)]
                let name = NativeName::Windows(name.encode_utf16().collect());
                #[cfg(not(windows))]
                let name = NativeName::Posix(name.into_bytes());
                let (kind, logical, metadata_known) = match index {
                    0 => (Kind::File, None, false),
                    1 if first_positive => (Kind::File, Some(3), true),
                    200 => (Kind::File, Some(5), true),
                    201 => (Kind::File, Some(7), true),
                    _ => (Kind::Directory, None, true),
                };
                Observation {
                    name,
                    kind,
                    logical,
                    allocated: None,
                    modified: None,
                    metadata_known,
                }
            })
            .collect::<Vec<_>>();
        tree.accept(ObservationBatch {
            parent: EntryId(0),
            sequence: 0,
            end: Some(ListingEnd::Exhausted),
            evidence: &vec![None; observations.len()],
            properties: &vec![NodeProperties::default(); observations.len()],
            observations: &observations,
            directory: None,
            errors: 0,
            denied: 0,
            detached: false,
        })
        .unwrap();
        tree
    }

    #[test]
    fn wide_pages_show_only_known_observed_bytes_outside_the_current_page() {
        let source = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let start = Start {
            root: source.path().to_owned(),
            explicit_root: true,
            observation: ScanObservation::DirectoryListing,
            preflight: None,
            options: Default::default(),
        };
        let mut view = DirectActive::open(1, &start, CancellationToken::default(), None)
            .unwrap()
            .unwrap();
        let tree = wide_tree(scratch.path(), false);
        let first = view.snapshot(&tree, None, false).unwrap();
        assert_eq!(first.details.len(), 200);
        assert!(first.more);
        assert_eq!(first.parent.logical, Some(12));
        assert!(first.details.iter().any(|(row, _)| row.logical.is_none()));
        let page = view
            .prepare(
                first,
                Revision(tree.revision()),
                true,
                false,
                Default::default(),
            )
            .unwrap();
        assert_eq!(page.tiles.len(), 1);
        let group = &page.tiles[0];
        assert_eq!(group.node_id, -1);
        assert!(!group.directory && group.path.is_empty() && group.partial);
        assert_eq!(
            group.size.as_str(),
            format_size(Some(12), true, Default::default())
        );
        assert!((group.band_x - 0.).abs() < 0.00001 && (group.band_w - 1.).abs() < 0.00001);
        assert!((group.x - 0.).abs() < 0.00001 && (group.y - 0.).abs() < 0.00001);
        assert!((group.w - 1.).abs() < 0.00001 && (group.h - 1.).abs() < 0.00001);
        drop(page);

        let mixed = wide_tree(scratch.path(), true);
        let page = view
            .prepare(
                view.snapshot(&mixed, None, false).unwrap(),
                Revision(mixed.revision()),
                true,
                false,
                Default::default(),
            )
            .unwrap();
        assert_eq!(page.tiles.len(), 2);
        let group = page.tiles.iter().find(|tile| tile.node_id == -1).unwrap();
        assert_eq!(
            group.size.as_str(),
            format_size(Some(12), true, Default::default())
        );
        assert!((group.band_w - 0.8).abs() < 0.00001);
        assert!((page.tiles.iter().map(|tile| tile.band_w).sum::<f32>() - 1.).abs() < 0.00001);
        assert!(page.tiles.iter().all(|tile| tile.x >= 0.
            && tile.y >= 0.
            && tile.x + tile.w <= 1.00001
            && tile.y + tile.h <= 1.00001));
        drop(page);
        view.after = mixed
            .children(EntryId(0), None, DETAIL_LIMIT, false)
            .unwrap()
            .next;
        let last = view.snapshot(&mixed, None, false).unwrap();
        assert!(!last.more);
        let page = view
            .prepare(
                last,
                Revision(mixed.revision()),
                true,
                false,
                Default::default(),
            )
            .unwrap();
        let group = page.tiles.iter().find(|tile| tile.node_id == -1).unwrap();
        assert_eq!(
            group.size.as_str(),
            format_size(Some(3), true, Default::default())
        );
        assert!((group.band_w - 0.2).abs() < 0.00001);
        drop(page);

        for parent_total in [None, Some(1), Some(12)] {
            let mut snapshot = view.snapshot(&tree, None, false).unwrap();
            snapshot.parent.logical = parent_total;
            let page = view
                .prepare(
                    snapshot,
                    Revision(tree.revision()),
                    true,
                    false,
                    Default::default(),
                )
                .unwrap();
            assert!(page.tiles.iter().all(|tile| tile.node_id >= 0));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_page_fits_with_a_retained_preview_at_128_mib() {
        let governor = Governor::global();
        governor.configure(sortsize_core::inventory_contract::MemoryMode::Auto, None);
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("long-root-".repeat(18));
        std::fs::create_dir(&source).unwrap();
        for number in 0..205 {
            std::fs::create_dir(source.join(format!("folder-{number:03}"))).unwrap();
        }
        let folder = source.join("folder-000");
        for number in 0..100 {
            std::fs::write(
                folder.join(format!("file-{number:03}-{}", "n".repeat(220))),
                [7; 8],
            )
            .unwrap();
        }
        let start = Start {
            root: source,
            explicit_root: true,
            observation: ScanObservation::Strict,
            preflight: None,
            options: Default::default(),
        };
        let view = DirectActive::open(1, &start, CancellationToken::default(), None)
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while view.scan.status() == DirectStatus::Running {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(view.scan.status(), DirectStatus::Finished);
        let tree = view.scan.tree().read().unwrap();
        let prepare = |busy| {
            view.prepare(
                view.snapshot(&tree, None, busy).unwrap(),
                Revision(tree.revision()),
                busy,
                !busy,
                Default::default(),
            )
            .unwrap()
        };
        let preview = prepare(true);
        let ready = prepare(false);
        assert!(preview.busy && ready.measure_ready && !ready.busy);
        assert_eq!(ready.frame.coverage, ScopeCoverage::Complete);
        assert_eq!(ready.frame.rows.len(), DETAIL_LIMIT);
        assert!(ready.frame.more);
        assert_eq!(ready.top.rows.len(), 100);
        assert_eq!(ready.root_measure.unwrap().logical, Some(800));
    }

    #[test]
    fn rejected_page_remains_forced_until_mailbox_accepts_it() {
        let previous = Instant::now();
        let mut last = Some((previous, Revision(1)));
        let mut force = false;
        record_publication(&mut force, &mut last, Revision(2), false);
        assert!(force);
        assert_eq!(last, Some((previous, Revision(1))));
        record_publication(&mut force, &mut last, Revision(2), true);
        assert!(!force);
        assert_eq!(last.unwrap().1, Revision(2));
    }

    #[test]
    fn restored_page_retries_after_error_mailbox_is_consumed() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("observed"), [7; 9]).unwrap();
        let start = Start {
            root: source.path().to_owned(),
            explicit_root: true,
            observation: ScanObservation::DirectoryListing,
            preflight: None,
            options: Default::default(),
        };
        let mut view = DirectActive::open(1, &start, CancellationToken::default(), None)
            .unwrap()
            .unwrap();
        view.autosave_attempted = true;
        let deadline = Instant::now() + Duration::from_secs(30);
        while view.scan.status() == DirectStatus::Running {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(view.scan.status(), DirectStatus::Finished);
        let revision = Revision(view.scan.tree().read().unwrap().revision());
        let mut state = Lane {
            source_epoch: 1,
            view_epoch: 1,
            ..Default::default()
        };
        view.attach(&mut state);
        let lane = Mutex::new(state);
        let output = Mutex::new(None);
        let cache_output = Mutex::new(None);
        let search_output = Mutex::new(None);
        view.turn(
            &lane,
            &output,
            &cache_output,
            &search_output,
            None,
            Default::default(),
        )
        .unwrap();
        assert!(matches!(
            output.lock().unwrap().take(),
            Some(Output::Page(_))
        ));
        assert!(view.measured.is_some());
        let previous = view.last.unwrap().0;
        {
            let mut state = lane.lock().unwrap();
            state.source_epoch = 2;
            state.view_epoch = 2;
            state.phase = Some((2, Phase::Failed, view.context.generation));
            view.restore_previous(2, 2, &mut state);
        }
        *output.lock().unwrap() = Some(Output::Error(2, "expected failure".into(), false));
        view.turn(
            &lane,
            &output,
            &cache_output,
            &search_output,
            None,
            Default::default(),
        )
        .unwrap();
        assert!(matches!(
            output.lock().unwrap().as_ref(),
            Some(Output::Error(2, ..))
        ));
        assert!(view.force);
        assert_eq!(view.last, Some((previous, revision)));
        assert!(
            lane.lock()
                .unwrap()
                .direct_publications
                .iter()
                .all(|ticket| ticket.strong_count() == 0)
        );
        assert!(matches!(
            lane.lock().unwrap().phase,
            Some((2, Phase::Failed, _))
        ));
        output.lock().unwrap().take();
        view.turn(
            &lane,
            &output,
            &cache_output,
            &search_output,
            None,
            Default::default(),
        )
        .unwrap();
        assert!(
            matches!(output.lock().unwrap().as_ref(), Some(Output::Page(page))
            if page.epoch == 2 && page.frame.stamp.context.inventory == view.context.inventory
                && page.frame.rows.iter().any(|row| row.logical == Some(9)))
        );
        assert_eq!(lane.lock().unwrap().direct_publications.len(), 1);
    }

    #[test]
    fn completed_empty_order_does_not_hide_later_live_observations() {
        let scratch = tempfile::tempdir().unwrap();
        let mut tree =
            DirectTree::new(scratch.path().join("source"), scratch.path(), 1024 * 1024).unwrap();
        let mut job = PageQuery::new(&tree, Some(EntryId(0)), None, 0, None, DETAIL_LIMIT).unwrap();
        assert!(job.advance(&tree, &CancellationToken::default()).unwrap());
        let stale = query::DetailQuery {
            revision: tree.revision(),
            job,
            retained: None,
            retained_sort: None,
            previous: Vec::new(),
            anchor: None,
        };
        for sequence in 0..2 {
            let name = format!("file-{sequence}");
            #[cfg(windows)]
            let name = NativeName::Windows(name.encode_utf16().collect());
            #[cfg(not(windows))]
            let name = NativeName::Posix(name.into_bytes());
            let observations = [Observation {
                name,
                kind: Kind::File,
                logical: Some(sequence + 1),
                allocated: None,
                modified: None,
                metadata_known: true,
            }];
            tree.accept(ObservationBatch {
                parent: EntryId(0),
                sequence,
                end: (sequence == 1).then_some(ListingEnd::Exhausted),
                observations: &observations,
                evidence: &[None],
                properties: &[NodeProperties::default()],
                directory: None,
                errors: 0,
                denied: 0,
                detached: false,
            })
            .unwrap();
            let page = detail_page(&tree, EntryId(0), None, Some(&stale), true).unwrap();
            assert_eq!(page.rows.len(), sequence as usize + 1);
            assert!(page.rows.iter().all(|row| row.logical.unwrap() > 0));
        }
        // This is the previous decision: a completed, empty order wins even
        // though the live tree has advanced. It reproduces the blank page.
        assert!(stale.job.rows(&tree).unwrap().is_empty());
    }
}
