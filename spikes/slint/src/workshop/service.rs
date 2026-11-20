use super::*;
use scan_p1::progress_trace::{self, Channel, Field};
use sortsize_core::acquisition::{Guard, Policy, ScanObservation};
use sortsize_store::inventory::generation_a::{CertifiedGeneration, Pipeline, Repository};
use sortsize_store::{
    Durability,
    inventory::{
        Error, Store,
        folder_totals::{FolderFold, PROJECTION_DISK_BYTES},
        navigation::{FolderCursor, Mode},
        placement::{self, DiskBudget},
    },
    scheduler::{Session, State},
};
use std::time::{Duration, Instant};
#[path = "auto_inventory.rs"]
mod auto_inventory;
#[path = "direct.rs"]
mod direct;

pub(super) fn lock_snapshot_deletion(
    saved: &sortsize_store::inventory::catalogue::Saved,
) -> sortsize_store::inventory::Result<
    std::sync::MutexGuard<'static, std::collections::BTreeMap<std::path::PathBuf, usize>>,
> {
    direct::lock_snapshot_deletion(saved)
}

const ORDER_DISK_BYTES: u64 = 512 * 1024 * 1024;
// The live fold and the initial sort coexist while the writer is active.
// Tree branches are independently admitted on demand.
const START_DISK_BYTES: u64 = PROJECTION_DISK_BYTES + ORDER_DISK_BYTES + Store::WRITE_RESERVE;

fn acquisition_budget(reader_owns_preview: bool) -> DiskBudget {
    DiskBudget {
        now: if reader_owns_preview {
            Store::WRITE_RESERVE
        } else {
            START_DISK_BYTES
        },
        minimum_limit: START_DISK_BYTES,
    }
}

pub(super) fn work_location() -> sortsize_store::inventory::Result<placement::Workspace> {
    placement::Workspace::selected().map_err(|error| {
        std::io::Error::other(format!(
            "{}\n{error}",
            tr!("The working folder is unavailable. Set BUNNY_AND_SIZE_STORE_DIR to an existing local folder and restart Bunny and Size.")
        ))
        .into()
    })
}

pub(super) fn require_work_location(
    location: &placement::Workspace,
    path: &Path,
) -> sortsize_store::inventory::Result<()> {
    if location.permits_writer(path)? {
        return Ok(());
    }
    Err(std::io::Error::other(tr_format!(
        "This inventory is stored at {}. Set BUNNY_AND_SIZE_STORE_DIR to choose an existing parent of that repository and restart, or use Analyze to start a new inventory in the selected working folder.",
        label(path)
    )).into())
}
pub(super) struct Start {
    pub root: PathBuf,
    /// One acquisition intention; never persisted or granted by source selection.
    pub explicit_root: bool,
    pub preflight: Option<crate::directory_worker::Preflight>,
    pub observation: ScanObservation,
    pub options: sortsize_store::scheduler::CollectionOptions,
}

pub(super) fn size_observation() -> ScanObservation {
    if cfg!(windows) {
        ScanObservation::DirectoryListing
    } else {
        ScanObservation::Strict
    }
}

impl Start {
    fn accepts_resume(&self, policy: PolicyId) -> bool {
        // An occupancy request can continue an older strict scan. A request
        // that needs fresh file metadata must never inherit listing batches.
        self.observation == ScanObservation::DirectoryListing
            || sortsize_store::scheduler::policy_observation(policy) == ScanObservation::Strict
    }
}

#[derive(Default)]
pub(super) enum StartPurpose {
    #[default]
    Regular,
    ManualScan,
    DuplicateInventory,
    PriorityDuplicates,
    BrowseInterrupted(PathBuf),
    ResumeInterrupted {
        allow_fresh: bool,
        options: sortsize_store::scheduler::CollectionOptions,
        explicit_root: bool,
    },
}

struct ResumeOptions {
    expected_collection: Option<sortsize_store::scheduler::CollectionOptions>,
    disk: DiskBudget,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Phase {
    Preparing,
    Acquiring,
    Finalizing,
    PreviewReady,
    Certifying,
    Reopening,
    Ready,
    Cancelled,
    Failed,
    Saving,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => tr!("Preparing the new inventory…"),
            Self::Acquiring => tr!("Scanning folders…"),
            Self::Finalizing => tr!("Preparing the view…"),
            Self::PreviewReady => tr!("Analysis finished"),
            Self::Saving => tr!("Saving inventory…"),
            Self::Certifying => tr!("Checking the new inventory…"),
            Self::Reopening => tr!("Reopening and checking the saved inventory…"),
            Self::Ready => tr!("Inventory saved"),
            Self::Cancelled => tr!("Cancelled"),
            Self::Failed => tr!("Could not start scan."),
        }
    }
}
#[derive(Clone)]
pub(super) enum Navigate {
    Open(EntryKey),
    ToggleTree(EntryKey),
    MoreTree(EntryKey),
    PreviousTree(EntryKey),
    Reveal(EntryKey),
    Parent,
    Page(bool),
    ObservedPage(bool),
    Top100(Option<TopScope>),
    Refresh,
    Sort(i32),
    OwnerSort(i32, Vec<sortsize_store::inventory::ordered::OwnerLabel>),
}
#[derive(Clone)]
pub(super) struct TopScope {
    pub key: EntryKey,
    pub path: Arc<PathBuf>,
}

pub(super) struct CertifiedReadiness {
    epoch: u64,
    context: AcquisitionContext,
    page: bool,
    cleanup: bool,
}

#[derive(Default)]
pub(super) struct Lane {
    pub start: Option<(u64, Start)>,
    pub start_purpose: StartPurpose,
    /// Runtime mirror of the durable source opt-out. No source I/O occurs here.
    pub disconnected_sources: Vec<PathBuf>,
    /// Frozen logical/native aliases survive the disappearance of a mount.
    pub source_bindings: Vec<(PathBuf, PathBuf)>,
    pub source_revocations: Vec<(PathBuf, u64)>,
    pub duplicate_operations: Vec<(u64, Vec<PathBuf>, sortsize_core::CancellationToken)>,
    pub preparing_source: Option<(PathBuf, sortsize_core::CancellationToken)>,
    pub source_operation: Option<(PathBuf, sortsize_core::CancellationToken)>,
    pub source_operation_owner: Option<(u64, u64)>,
    pub source_operation_revoked: bool,
    pub acquisition_root: Option<PathBuf>,
    pub acquisition_publication: Option<(PathBuf, sortsize_core::CancellationToken)>,
    pub smb_begin: Option<super::smb_server::Request>,
    pub smb_snapshot: Option<super::smb_server::Snapshot>,
    pub smb_open: Option<(u64, usize)>,
    pub smb_open_path: Option<(u64, PathBuf)>,
    pub smb_retained: Vec<super::smb_server::Share>,
    #[cfg(test)]
    pub smb_native_fixture: Option<(PathBuf, PathBuf)>,
    pub smb_cancel: bool,
    /// None shows the virtual server; Some selects a particular share reader.
    pub smb_view: Option<PathBuf>,
    pub smb_acquisition_root: Option<PathBuf>,
    pub notify: Option<Arc<dyn Fn() + Send + Sync>>,
    pub autosave_revision: Arc<std::sync::atomic::AtomicU64>,
    pub autosave_error: Arc<Mutex<Option<String>>>,
    pub navigation: Option<(u64, Navigate)>,
    pub top_scope: Option<TopScope>,
    pub desired_folder: Option<EntryKey>,
    pub desired_relative: Option<(GenerationId, PathBuf)>,
    pub acquisition_control: Option<(u64, GenerationId, sortsize_store::scheduler::Control)>,
    pub cancel: bool,
    /// User cancellation belongs to one source intention until its terminal state.
    pub cancel_requested_epoch: Option<u64>,
    /// Cancel drained successfully; this candidate was frozen before publication.
    pub cancelled_unpublished: Option<u64>,
    pub retry: bool,
    pub stop: bool,
    pub retired_through: u64,
    pub presentation: crate::presentation::Context,
    pub units: crate::units::Units,
    pub memory: MemoryMode,
    pub detail_sort: u8,
    pub detail_anchor: Option<(EntryId, EntryKey)>,
    pub search: Option<super::search::Request>,
    pub search_page: Option<(i32, bool)>,
    pub search_statistics: Option<i32>,
    pub search_duplicates: Option<(
        i32,
        sortsize_store::inventory::queries::duplicates::DuplicateFileProfile,
    )>,
    pub search_sort: Option<(i32, i32)>,
    pub search_refresh: bool,
    pub cache: Option<super::cache::Request>,
    pub cache_root: Option<PathBuf>,
    pub cache_page: Option<(u64, bool)>,
    pub source_epoch: u64,
    #[cfg(feature = "benchmarks")]
    pub cache_open_timing: Option<(u64, Instant, f64)>,
    pub search_current: Option<u64>,
    pub reader_active: bool,
    /// The interrupted view may hold a shared lock on the raw candidate.
    pub interrupted_reader: bool,
    /// A native view is independent of any retained SQLite reader.
    pub direct_view: Option<u64>,
    // Survives a DirectActive replacement while Host retains its previous page.
    pub direct_publications: Vec<std::sync::Weak<()>>,
    pub reader_target: Option<Option<Box<Active>>>,
    pub reader_epoch: Option<(u64, PathBuf, InventoryId, SourceId)>,
    pub writer_target: Option<Box<Active>>,
    pub writer_running: bool,
    pub writer_owns_session: bool,
    pub view_epoch: u64,
    pub writer_quanta: u64,
    /// Opt-in for the next local Windows acquisition.
    pub fast_local: bool,
    pub scan_progress: Option<super::scan_p3::Progress>,
    pub fast_preview_context: Option<AcquisitionContext>,
    fast_save_cancel: Option<(u64, sortsize_core::CancellationToken)>,
    pub phase: Option<(u64, Phase, GenerationId)>,
    certified_readiness: Option<CertifiedReadiness>,
    pub publication_cancel: sortsize_core::CancellationToken,
    pub writer_epoch: Option<u64>,
    pub reader_constructing: Option<u64>,
    pub operation_cancel: Option<sortsize_core::CancellationToken>,
    pub query_cancel: Option<sortsize_core::CancellationToken>,
    #[cfg(test)]
    pub hold_generation: Option<GenerationId>,
    #[cfg(test)]
    pub writer_entries: u64,
    #[cfg(test)]
    pub writer_in_flight: usize,
    #[cfg(test)]
    pub writer_generation: Option<GenerationId>,
    #[cfg(test)]
    pub tick_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    #[cfg(test)]
    pub tick_blocked: bool,
    #[cfg(test)]
    pub turn_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    #[cfg(test)]
    pub turn_blocked: bool,
    /// Explicit historical-backend coverage; never used by production routing.
    #[cfg(test)]
    pub force_strict: bool,
    #[cfg(test)]
    pub reader_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    #[cfg(test)]
    pub reader_blocked: bool,
    #[cfg(test)]
    pub direct_reader_blocked: bool,
    #[cfg(test)]
    pub publication_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    #[cfg(test)]
    pub publication_blocked: bool,
    #[cfg(test)]
    pub cleanup_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    #[cfg(test)]
    pub cleanup_blocked: bool,
    #[cfg(test)]
    pub tick_delay: Duration,
    pub reader_workspace: Option<Arc<RepositoryWorkspace>>,
    pub retire_private: Option<Arc<RepositoryWorkspace>>,
}
impl Lane {
    pub(super) fn preparing_certified_view(&self) -> bool {
        self.certified_readiness.as_ref().is_some_and(|ready| {
            ready.epoch == self.source_epoch
                && matches!(self.phase, Some((epoch, Phase::Finalizing, generation))
                    if epoch == ready.epoch && generation == ready.context.generation)
        })
    }

    pub(super) fn certified_step_finished(
        &mut self,
        epoch: u64,
        context: AcquisitionContext,
        page: bool,
    ) {
        let Some(ready) = &mut self.certified_readiness else {
            return;
        };
        if ready.epoch != epoch || ready.context != context || self.source_epoch != epoch {
            return;
        }
        if page {
            ready.page = true;
        } else {
            ready.cleanup = true;
        }
        self.refresh_certified_readiness();
    }

    fn refresh_certified_readiness(&mut self) {
        let Some(ready) = &self.certified_readiness else {
            return;
        };
        let epoch = ready.epoch;
        let context = ready.context;
        if ready.cleanup
            && self.source_epoch == epoch
            && matches!(self.phase, Some((current, Phase::Finalizing, generation))
                if current == epoch && generation == context.generation)
        {
            if self.cancel_requested_epoch == Some(epoch) || self.publication_cancel.is_cancelled()
            {
                self.phase = Some((epoch, Phase::Cancelled, context.generation));
            } else if ready.page {
                self.phase = Some((epoch, Phase::Ready, context.generation));
            }
        }
    }

    fn cancel_fast_save(&mut self) {
        if let Some((_, cancel)) = &self.fast_save_cancel {
            cancel.cancel();
        }
        if self
            .cache
            .as_ref()
            .is_some_and(|request| matches!(request.action, super::cache::Action::SaveFast { .. }))
            && let Some(request) = self.cache.take()
        {
            request.cancel.cancel();
        }
    }

    /// Revoke the old source intention, including an independent fast save.
    pub(super) fn cancel_publication(&mut self) {
        self.publication_cancel.cancel();
        self.cancel_fast_save();
        self.refresh_certified_readiness();
    }

    pub(super) fn has_acquisition_intent(&self) -> bool {
        self.writer_running
            || self.start.is_some()
            || self.preparing_source.is_some()
            || self.smb_begin.is_some()
            || self
                .smb_snapshot
                .as_ref()
                .is_some_and(|batch| batch.running)
            || self.fast_save_cancel.is_some()
    }

    pub fn acquisition_in_progress(&self) -> bool {
        self.has_acquisition_intent()
            || self.phase.is_some_and(|(epoch, phase, _)| {
                epoch == self.source_epoch
                    && matches!(
                        phase,
                        Phase::Preparing
                            | Phase::Acquiring
                            | Phase::Finalizing
                            | Phase::Certifying
                            | Phase::Reopening
                    )
            })
    }

    /// A terminal source/batch completion can win a late Cancel click. It is
    /// still an acknowledgement; the UI must not wait for a second terminal.
    pub fn cancellation_pending(&self) -> bool {
        if self.cancel_requested_epoch != Some(self.source_epoch) {
            return false;
        }
        if let Some(snapshot) = &self.smb_snapshot
            && snapshot.epoch == self.source_epoch
        {
            return snapshot.running
                || self.writer_running
                || self.start.is_some()
                || self.preparing_source.is_some();
        }
        !self.phase.is_some_and(|(epoch, phase, _)| {
            epoch == self.source_epoch
                && !self.writer_running
                && matches!(phase, Phase::Cancelled | Phase::Ready | Phase::PreviewReady)
        })
    }
}
// Called only after publishing and releasing the output mutex. The UI still
// applies its ordinary epoch/generation/gesture guards in Host::poll.
pub(super) fn notify(lane: &Mutex<Lane>) {
    let notify = lane.lock().unwrap().notify.clone();
    if let Some(notify) = notify {
        notify();
    }
}
/// Repeated preparation refusals use the ordinary UI poll. Keeping the
/// notifier/lane out of this publication API prevents a 2 ms retry loop from
/// creating an event-loop wake for every identical refusal.
pub(super) fn publish_retry_error(output: &Mutex<Option<Output>>, epoch: u64, error: String) {
    *output.lock().unwrap() = Some(Output::Error(epoch, error, true));
}

pub(super) struct Prepared {
    pub units: crate::units::Units,
    #[cfg(feature = "benchmarks")]
    pub worker_frame_ready_at: Instant,
    pub epoch: u64,
    pub token: i32,
    pub frame: Arc<sortsize_store::inventory::navigation::Frame>,
    pub parent_entry: EntryId,
    pub top: Arc<sortsize_store::inventory::navigation::Frame>,
    pub details: Vec<DetailRow>,
    pub owners: Vec<sortsize_core::EntryOwner>,
    pub folders: Vec<TreeRow>,
    pub folder_keys: Vec<super::folder_tree::TreeTarget>,
    pub _folder_memory: Option<sortsize_core::resources::Reservation>,
    pub top_rows: Vec<TopRow>,
    pub top_scope: Option<Arc<PathBuf>>,
    pub tiles: Vec<MapTile>,
    pub path: String,
    pub status: String,
    pub scope_detail: String,
    pub summary: String,
    pub current_folder_summary: String,
    pub(super) root_measure: Option<sortsize_store::inventory::folder_totals::FolderMeasure>,
    pub(super) root_provisional: bool,
    pub busy: bool,
    pub parent: bool,
    pub previous: bool,
    #[allow(dead_code)] // Captured by native qualification and regression tests.
    pub quanta: u64,
    pub measure_ready: bool,
    pub sort_index: i32,
    pub _memory: sortsize_core::resources::Reservation,
    pub(super) p1_frame_ready_at: Option<Instant>,
    // Keep this last: a direct producer may construct the next publication
    // only after models, frames and their reservations have all been dropped.
    pub(super) _direct_publication: Option<Arc<()>>,
}
fn publication_token() -> sortsize_store::inventory::Result<i32> {
    static TOKEN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    TOKEN
        .try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| n.checked_add(1),
        )
        .map(|n| n + 1)
        .map_err(|_| Error::Contract("publication token exhausted"))
}

fn view_context(c: AcquisitionContext, epoch: u64, id: u64) -> ViewContext {
    ViewContext {
        inventory: c.inventory,
        source: c.source,
        generation: c.generation,
        scan: c.scan,
        validation: c.validation,
        view: ViewId(id),
        epoch: ViewEpoch(epoch),
        query: None,
        presentation: PresentationEpoch(epoch),
    }
}

fn fresh_context(
    epoch: u64,
    root: &Path,
    time: std::time::SystemTime,
    observation: ScanObservation,
) -> sortsize_store::inventory::Result<AcquisitionContext> {
    Ok(AcquisitionContext {
        inventory: InventoryId(
            time.duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::Contract("inventory timestamp unavailable"))?
                .as_nanos()
                ^ (u128::from(std::process::id()) << 96),
        ),
        source: sortsize_store::inventory::catalogue::source_hint(root),
        generation: GenerationId(1),
        scan: ScanId(epoch),
        policy: sortsize_store::scheduler::observation_policy(observation),
        mount: MountEpoch(epoch),
        run: RunEpoch(epoch),
        validation: ValidationEpoch(1),
    })
}
pub(super) enum Output {
    Page(Box<Prepared>),
    Error(u64, String, bool),
}
pub(super) struct Service {
    pub trace: Option<Arc<progress_trace::Trace>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    pub lane: Arc<Mutex<Lane>>,
    pub output: Arc<Mutex<Option<Output>>>,
    pub search_output: Arc<Mutex<Option<super::search::Prepared>>>,
    pub resources: Arc<Mutex<Option<(crate::presentation::Context, String)>>>,
    pub cache_output: Arc<Mutex<Option<super::cache::Prepared>>>,
    pub preview_output: Arc<Mutex<Option<super::cache::PreviewOutput>>>,
}
impl Drop for Service {
    fn drop(&mut self) {
        if let Some(trace) = &self.trace {
            trace.stop();
        }
        {
            let mut lane = self.lane.lock().unwrap();
            lane.stop = true;
            lane.cancel_publication();
            if let Some(cancel) = &lane.query_cancel {
                cancel.cancel();
            }
            if let Some(cancel) = &lane.operation_cancel {
                cancel.cancel();
            }
            if let Some(request) = &lane.cache {
                request.cancel.cancel();
            }
        }
        // Filesystem providers and FULL sync may not be interruptible. UI
        // destruction only signals; worker-owned Arcs retain cleanup owners.
        self.workers.clear();
    }
}
impl Service {
    pub fn bind_source(&self, logical: PathBuf, native: PathBuf) {
        if logical == native {
            return;
        }
        let mut lane = self.lane.lock().unwrap();
        if lane
            .source_bindings
            .iter()
            .any(|binding| binding == &(logical.clone(), native.clone()))
        {
            return;
        }
        // At most one active single-source alias and the bounded NAS scope.
        if lane.source_bindings.len() > super::smb_server::MAX_SHARES {
            lane.source_bindings.remove(0);
        }
        lane.source_bindings.push((logical, native));
    }
    /// Called only after the controller has accepted an explicit confirmation.
    /// Revoke admission before signalling workers; an in-flight native call may
    /// finish later, but its control remains cancelled even after reconnection.
    pub fn disconnect_sources(&self, sources: &[PathBuf]) {
        let mut lane = self.lane.lock().unwrap();
        let revoked_through = lane.source_epoch;
        let native_revocations = lane
            .source_bindings
            .iter()
            .filter(|(logical, _)| {
                sources
                    .iter()
                    .any(|scope| crate::source_registry::covers(scope, logical))
            })
            .map(|(_, native)| native.clone())
            .collect::<Vec<_>>();
        for path in sources {
            if !lane.disconnected_sources.contains(path) {
                lane.disconnected_sources.push(path.clone());
            }
            if let Some((_, through)) = lane
                .source_revocations
                .iter_mut()
                .find(|(scope, _)| scope == path)
            {
                *through = (*through).max(revoked_through);
            } else {
                lane.source_revocations
                    .push((path.clone(), revoked_through));
            }
        }
        for path in native_revocations {
            if let Some((_, through)) = lane
                .source_revocations
                .iter_mut()
                .find(|(scope, _)| scope == &path)
            {
                *through = (*through).max(revoked_through);
            } else {
                lane.source_revocations.push((path, revoked_through));
            }
        }
        let bindings = lane
            .smb_snapshot
            .as_ref()
            .into_iter()
            .flat_map(|snapshot| &snapshot.shares)
            .filter_map(|share| {
                share
                    .native_root
                    .as_ref()
                    .map(|root| (share.path.clone(), root.clone()))
            })
            .chain(lane.source_bindings.iter().cloned())
            .collect::<Vec<_>>();
        let revoked = |path: &Path| {
            sources.iter().any(|scope| {
                crate::source_registry::covers(scope, path)
                    || bindings.iter().any(|(logical, root)| {
                        crate::source_registry::covers(scope, logical)
                            && crate::source_registry::covers(root, path)
                    })
            })
        };
        if lane
            .start
            .as_ref()
            .is_some_and(|(_, start)| revoked(&start.root))
        {
            lane.start = None;
            if !lane.writer_owns_session && lane.preparing_source.is_none() {
                lane.writer_running = false;
            }
        }
        for (root, cancel) in lane
            .preparing_source
            .iter()
            .chain(lane.source_operation.iter())
            .chain(lane.acquisition_publication.iter())
        {
            if revoked(root) {
                cancel.cancel();
            }
        }
        for (_, roots, cancel) in &lane.duplicate_operations {
            if roots.iter().any(|root| revoked(root)) {
                cancel.cancel();
            }
        }
        if lane
            .source_operation
            .as_ref()
            .is_some_and(|(root, _)| revoked(root))
        {
            lane.source_operation_revoked = true;
        }
        if let Some(request) = lane.cache.as_ref()
            && request.action.acquisition_source().is_some_and(revoked)
        {
            request.cancel.cancel();
            lane.cache = None;
        }
        if let Some(next) = lane.writer_target.as_mut()
            && revoked(&next.root)
        {
            next.request_cancel();
        }
        if lane.acquisition_root.as_deref().is_some_and(revoked) {
            lane.cancel_fast_save();
            if let Some((_, _, control)) = &lane.acquisition_control {
                control.cancel();
            }
            // Do not use the global cancel flag: a different Start may already
            // be coalesced and must not inherit this source's cancellation.
            lane.retry = false;
        }
        if let Some(request) = &mut lane.smb_begin {
            request.disable_sources(sources);
        }
    }
    /// Reconnection restores eligibility only. It never enqueues or retries work.
    pub fn reconnect_sources(&self, sources: &[PathBuf]) {
        let mut lane = self.lane.lock().unwrap();
        // A child action under a disabled parent cannot discard the child's
        // independent exclusion, even if a delayed caller bypasses the UI.
        let eligible = sources
            .iter()
            .filter(|scope| {
                !lane.disconnected_sources.iter().any(|parent| {
                    crate::source_registry::covers(parent, scope)
                        && !crate::source_registry::covers(scope, parent)
                })
            })
            .collect::<Vec<_>>();
        lane.disconnected_sources.retain(|path| {
            !eligible.iter().any(|scope| {
                crate::source_registry::covers(scope, path)
                    && crate::source_registry::covers(path, scope)
            })
        });
    }
    pub fn source_disconnected(&self, path: &Path) -> bool {
        source_disconnected(&self.lane.lock().unwrap(), path)
    }
    #[cfg(test)]
    pub(super) fn stop_for_test(mut self) {
        self.lane.lock().unwrap().stop = true;
        for worker in std::mem::take(&mut self.workers) {
            worker.join().unwrap();
        }
    }
    pub fn begin_smb_batch(
        &self,
        epoch: u64,
        server: PathBuf,
        shares: Vec<PathBuf>,
        discovery_uncertain: bool,
        options: sortsize_store::scheduler::CollectionOptions,
    ) -> Result<(), String> {
        let mut request =
            super::smb_server::Request::new(epoch, server, shares, discovery_uncertain, options)?;
        #[cfg(test)]
        if let Some((path, native)) = self.lane.lock().unwrap().smb_native_fixture.as_ref()
            && let Some(share) = request
                .snapshot
                .shares
                .iter_mut()
                .find(|share| &share.path == path)
        {
            share.native_root = Some(native.clone());
        }
        for share in &request.snapshot.shares {
            if let Some(native) = &share.native_root {
                self.bind_source(share.path.clone(), native.clone());
            }
        }
        let mut lane = self.lane.lock().unwrap();
        if source_disconnected(&lane, &request.snapshot.server) {
            return Err(tr!("Disconnected").into());
        }
        request.disable_sources(&lane.disconnected_sources);
        let mut union = std::collections::HashSet::new();
        for path in lane
            .smb_retained
            .iter()
            .map(|share| &share.path)
            .chain(request.snapshot.shares.iter().map(|share| &share.path))
        {
            union.insert(path);
        }
        if union.len() > super::smb_server::MAX_SHARES {
            return Err(tr!("Could not start scan.").into());
        }
        lane.cancel_publication();
        lane.publication_cancel = Default::default();
        lane.source_epoch = epoch;
        lane.view_epoch = epoch;
        lane.cancel_requested_epoch = None;
        lane.cancelled_unpublished = None;
        lane.smb_snapshot = Some(request.snapshot.clone());
        lane.smb_begin = Some(request);
        lane.smb_cancel = false;
        lane.smb_open = None;
        lane.smb_open_path = None;
        lane.smb_view = None;
        lane.start = None;
        lane.navigation = None;
        lane.desired_folder = None;
        lane.desired_relative = None;
        lane.reader_target = Some(None);
        lane.reader_epoch = None;
        lane.writer_running = true;
        Ok(())
    }
    pub fn smb_snapshot(&self) -> Option<super::smb_server::Snapshot> {
        self.lane.lock().unwrap().smb_snapshot.clone()
    }
    pub fn retained_smb_shares(&self) -> Vec<super::smb_server::Share> {
        self.lane.lock().unwrap().smb_retained.clone()
    }
    /// Reopen a dated, locally held inventory outside the current confirmed
    /// acquisition scope. It never probes or retries its remote source.
    pub fn open_smb_path(&self, view_epoch: u64, path: &Path) -> bool {
        let mut lane = self.lane.lock().unwrap();
        if !lane
            .smb_retained
            .iter()
            .any(|share| share.path == path && (share.files.is_some() || share.measured.is_some()))
        {
            return false;
        }
        lane.view_epoch = view_epoch;
        lane.smb_view = lane
            .smb_retained
            .iter()
            .find(|share| share.path == path)
            .and_then(|share| share.native_root.clone());
        lane.smb_open_path = Some((view_epoch, path.to_owned()));
        lane.navigation = None;
        lane.desired_folder = None;
        lane.desired_relative = None;
        true
    }
    /// Select a stored share without granting acquisition or revalidating SMB.
    pub fn open_smb_share(&self, view_epoch: u64, index: usize) -> bool {
        let available = self
            .lane
            .lock()
            .unwrap()
            .smb_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.shares.get(index))
            .is_some_and(|share| share.files.is_some());
        available && self.view_smb_share(view_epoch, index)
    }
    /// Pure view selection, also usable before the confirmed scan publishes its
    /// first page. This method never enqueues an acquisition intention.
    pub fn view_smb_share(&self, view_epoch: u64, index: usize) -> bool {
        let mut lane = self.lane.lock().unwrap();
        let Some(path) = lane
            .smb_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.shares.get(index))
            .and_then(|share| share.native_root.clone())
        else {
            return false;
        };
        lane.view_epoch = view_epoch;
        lane.smb_view = Some(path);
        lane.smb_open = Some((view_epoch, index));
        lane.navigation = None;
        lane.desired_folder = None;
        lane.desired_relative = None;
        true
    }
    pub fn leave_smb_batch(&self) {
        let mut lane = self.lane.lock().unwrap();
        lane.smb_begin = None;
        lane.smb_snapshot = None;
        lane.smb_open = None;
        lane.smb_open_path = None;
        lane.smb_view = None;
        lane.smb_cancel = true;
        if let Some((_, _, control)) = &lane.acquisition_control {
            control.cancel();
        }
    }
    /// Signal the scheduler's independent control immediately. The worker still
    /// drains in-flight native calls/results, then freezes the partial candidate
    /// and dated preview without mandatory consolidation or publication. A
    /// blocked OS call is not interrupted.
    pub fn cancel_acquisition(&self) -> bool {
        let _trace_binding = self
            .trace
            .as_ref()
            .map(|trace| trace.ui_sql.bind_current_thread());
        let _trace = sortsize_store::scan_trace::phase("ui_cancel");
        if let Some(trace) = &self.trace {
            trace.mark_first(&trace.cancel_requested);
        }
        let mut lane = self.lane.lock().unwrap();
        if let Some((epoch, cancel)) = &lane.fast_save_cancel
            && *epoch == lane.source_epoch
        {
            cancel.cancel();
            lane.cancel_requested_epoch = Some(lane.source_epoch);
            return true;
        }
        if lane.cache.as_ref().is_some_and(|request| {
            matches!(request.action, super::cache::Action::SaveFast { source_epoch, .. }
                if source_epoch == lane.source_epoch)
        }) && let Some(request) = lane.cache.take()
        {
            request.cancel.cancel();
            // The pending request is already gone. No worker acknowledgement
            // or acquisition cancellation is needed.
            return false;
        }
        if !lane.writer_running
            && lane.start.is_none()
            && lane.preparing_source.is_none()
            && lane.phase.is_none_or(|(_, phase, _)| {
                matches!(phase, Phase::Ready | Phase::Cancelled | Phase::PreviewReady)
            })
        {
            return false;
        }
        lane.cancel_requested_epoch = Some(lane.source_epoch);
        lane.cancel = true;
        lane.retry = false;
        if !lane.reader_active
            && lane.direct_view.is_none()
            && lane
                .navigation
                .as_ref()
                .is_none_or(|(epoch, _)| *epoch <= lane.view_epoch)
        {
            // The unpublished SQLite view freezes on Cancel. Its queued
            // navigation cannot be served and must not survive into a retry.
            // A certified/direct reader and a newer view intention keep theirs.
            lane.navigation = None;
            lane.desired_folder = None;
            lane.desired_relative = None;
        }
        if let Some((epoch, _, control)) = &lane.acquisition_control
            && *epoch == lane.source_epoch
        {
            control.cancel();
            if let Some(trace) = &self.trace {
                trace.mark_first(&trace.cancel_control_sent);
            }
        }
        if let Some(target) = &lane.writer_target
            && target.source_epoch == lane.source_epoch
        {
            target.session.control().cancel();
            if let Some(trace) = &self.trace {
                trace.mark_first(&trace.cancel_control_sent);
            }
        }
        // Fence publication immediately even while acquisition still owns the
        // scheduler: a finalization quantum may already be in progress beyond
        // its last lane check. This token does not release any acquisition lease.
        lane.cancel_publication();
        true
    }
    pub fn cancel_smb_batch(&self) {
        let mut lane = self.lane.lock().unwrap();
        let current_batch = lane
            .smb_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.epoch == lane.source_epoch && snapshot.running)
            || lane
                .smb_begin
                .as_ref()
                .is_some_and(|request| request.snapshot.epoch == lane.source_epoch);
        if !current_batch {
            return;
        }
        lane.smb_cancel = true;
        lane.cancel_requested_epoch = Some(lane.source_epoch);
        lane.cancel = true;
        lane.retry = false;
        lane.cancel_publication();
        if let Some((_, _, control)) = &lane.acquisition_control {
            control.cancel();
        }
    }

    pub fn start() -> std::io::Result<Self> {
        let trace = progress_trace::Trace::start();
        let writer_trace = trace.clone();
        let reader_trace = trace.clone();
        let lane = Arc::new(Mutex::new(Lane::default()));
        let output = Arc::new(Mutex::new(None));
        let search_output = Arc::new(Mutex::new(None));
        let resources = Arc::new(Mutex::new(None));
        let cache_output = Arc::new(Mutex::new(None));
        let co = cache_output.clone();
        let preview_output = Arc::new(Mutex::new(None));
        let po = preview_output.clone();
        let so = search_output.clone();
        let rs = resources.clone();
        let l = lane.clone();
        let o = output.clone();
        let read_lane = lane.clone();
        let read_output = output.clone();
        let read_search = search_output.clone();
        let read_resources = resources.clone();
        let read_cache = cache_output.clone();
        let read_preview = preview_output.clone();
        let reader = std::thread::Builder::new()
            .name("v05-generation-reader".into())
            .spawn(move || {
                let _trace_binding = progress_trace::bind(reader_trace.as_ref(), Channel::Reader);
                let _sql_binding = reader_trace
                    .as_ref()
                    .map(|trace| trace.reader_sql.bind_current_thread());
                super::reader::worker(
                    read_lane,
                    read_output,
                    read_search,
                    read_resources,
                    read_cache,
                    read_preview,
                )
            })
            .inspect_err(|_| {
                if let Some(trace) = &trace {
                    trace.stop();
                }
            })?;
        let writer = match std::thread::Builder::new()
            .name("v05-workshop".into())
            .spawn(move || {
                let _trace_binding = progress_trace::bind(writer_trace.as_ref(), Channel::Writer);
                let _sql_binding = writer_trace
                    .as_ref()
                    .map(|trace| trace.writer_sql.bind_current_thread());
                worker(l, o, so, rs, co, po)
            }) {
            Ok(worker) => worker,
            Err(error) => {
                if let Some(trace) = &trace {
                    trace.stop();
                }
                lane.lock().unwrap().stop = true;
                return Err(error);
            }
        };
        Ok(Self {
            trace,
            workers: vec![reader, writer],
            lane,
            output,
            search_output,
            resources,
            cache_output,
            preview_output,
        })
    }
}
pub(super) struct Scratch(Option<tempfile::TempDir>);
impl Scratch {
    fn new(t: tempfile::TempDir) -> Self {
        Self(Some(t))
    }
    pub fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(temp) = self.0.take() {
            let group = std::fs::canonicalize(temp.path().join("inventory")).ok();
            match temp.close() {
                Ok(()) => {
                    if let Some(group) = group {
                        sortsize_core::resources::Governor::global()
                            .forget_deleted_artifact_group(&group);
                    }
                }
                Err(e) => *super::cache::CLEANUP_ERROR.lock().unwrap() = Some(e.to_string()),
            }
        }
    }
}
pub(super) enum Backend {
    Live(Box<Session>),
    Cached(Box<Store>),
    Certified(Box<sortsize_store::inventory::generation_a::CertifiedGeneration>),
}
impl Backend {
    fn is_local_live(&self) -> bool {
        matches!(self, Self::Live(session)
            if session.source_class() == sortsize_core::acquisition::SourceClass::Local)
    }

    pub fn store(&self) -> &Store {
        match self {
            Self::Live(s) => s.store(),
            Self::Cached(s) => s,
            Self::Certified(s) => s.as_store(),
        }
    }
    pub fn store_mut(&mut self) -> sortsize_store::inventory::Result<&mut Store> {
        match self {
            Self::Live(s) => Ok(s.store_mut()),
            Self::Cached(s) => Ok(s),
            Self::Certified(_) => Err(sortsize_store::inventory::Error::Contract(
                "published generation is read-only",
            )),
        }
    }
    pub fn control(&self) -> sortsize_store::scheduler::Control {
        match self {
            Self::Live(s) => s.control(),
            Self::Cached(_) | Self::Certified(_) => Default::default(),
        }
    }
    pub fn status(&self) -> sortsize_store::scheduler::Status {
        match self {
            Self::Live(s) => s.status(),
            Self::Cached(_) | Self::Certified(_) => sortsize_store::scheduler::Status {
                state: State::Sealed,
                in_flight: 0,
                pending_completions: 0,
                acquisitions_pending: 0,
                abandoned_quanta: 0,
                parked: 0,
                global_acquisitions: 0,
                global_cursors: 0,
                quanta: 0,
                processed_entries: 0,
            },
        }
    }
    pub fn tick(&mut self) -> sortsize_store::inventory::Result<sortsize_store::scheduler::Status> {
        match self {
            Self::Live(s) => s.tick(),
            Self::Cached(_) | Self::Certified(_) => Ok(self.status()),
        }
    }
    pub fn finalize_step(&mut self) -> sortsize_store::inventory::Result<Option<ScopeCoverage>> {
        let _trace = sortsize_store::scan_trace::phase("service_finalize");
        match self {
            Self::Live(s) => s.finalize_step(),
            Self::Cached(_) | Self::Certified(_) => Ok(None),
        }
    }
    pub fn retry_store(&mut self) {
        if let Self::Live(s) = self {
            s.retry_store();
        }
    }
    pub fn can_place_store(&self, path: &Path) -> sortsize_store::inventory::Result<bool> {
        match self {
            Self::Live(s) => s.can_place_store(path),
            Self::Cached(_) | Self::Certified(_) => Ok(true),
        }
    }
    pub fn replace_store(&mut self, store: Store) -> sortsize_store::inventory::Result<()> {
        match self {
            Self::Live(s) => s.replace_store(store),
            Self::Cached(s) => {
                **s = store;
                Ok(())
            }
            Self::Certified(_) => Err(sortsize_store::inventory::Error::Contract(
                "cannot replace a certified generation in place",
            )),
        }
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FastMode {
    #[default]
    Standard,
    Acquiring,
    PreviewWriter,
    Saving,
    Reader,
}

pub(super) struct Active {
    pub(super) folder_tree: RefCell<super::folder_tree::FolderTree>,
    live_preview: Option<sortsize_store::inventory::live_preview::LivePreview>,
    preview_floor: u64,
    pub(super) session: Backend,
    pub(super) c: AcquisitionContext,
    pub(super) root: PathBuf,
    pub(super) started: std::time::SystemTime,
    pub(super) sealed: Option<std::time::SystemTime>,
    pub(super) parent: EntryKey,
    cursor: Option<FolderCursor>,
    next: RefCell<Option<FolderCursor>>,
    pub(super) fold: FolderFold,
    pub(super) epoch: u64,
    pub(super) source_epoch: u64,
    pub(super) last: Instant,
    pub(super) p1_handoff_at: Option<Instant>,
    revision: Option<Revision>,
    pub(super) force: bool,
    measured: bool,
    last_state: State,
    sort: Option<sortsize_store::inventory::ordered::Sort>,
    sort_index: i32,
    owner_labels: Vec<sortsize_store::inventory::ordered::OwnerLabel>,
    anchor: Option<EntryId>,
    ordered: Option<sortsize_store::inventory::ordered::OrderedFolder>,
    ordered_page: Option<sortsize_store::inventory::queries::Page>,
    pending_page: Option<bool>,
    // Drop every owned SQLite result before removing its scratch directory.
    pub(super) _temporary: Scratch,
    repository: Option<RepositoryState>,
    // Last: published readers and candidate owners close before the private
    // repository is forgotten. A reader handoff keeps this root alive.
    pub(super) workspace: Option<Arc<RepositoryWorkspace>>,
    pub(super) retained_copy: Option<PathBuf>,
    publication_cancel: sortsize_core::CancellationToken,
    preparation_blocked: bool,
    cancelled: bool,
    /// Disconnect revocation is permanent for this intention, unlike ordinary
    /// cancellation of certification, which supports an explicit local Retry.
    source_revoked: bool,
    fast_mode: FastMode,
}
type RepositoryOwner = Arc<Mutex<Repository>>;
pub(super) struct RepositoryWorkspace {
    directory: Option<tempfile::TempDir>,
    // Keep the repository's canonical native spelling. On Windows rebuilding
    // it from TempDir can omit the verbatim prefix and misidentify our own
    // still-owned private authority as a superseded copy.
    original: Option<PathBuf>,
    owner: Option<RepositoryOwner>,
    writable: std::sync::atomic::AtomicBool,
    private_retired: Mutex<bool>,
    // One physical candidate per repository, retained even when a composite
    // batch retires its Active owner. Fixed metadata, no additional registry.
    cancelled_candidate: Mutex<Option<(AcquisitionContext, i64)>>,
    inventory: InventoryId,
    source: SourceId,
    root: PathBuf,
    // Explicit compatibility conversion only; the readers close before it.
    snapshot_owner: Option<Arc<sortsize_store::direct_snapshot::SnapshotOwner>>,
}
impl RepositoryWorkspace {
    fn temporary(
        prefix: &str,
        inventory: InventoryId,
        source: SourceId,
        root: PathBuf,
    ) -> sortsize_store::inventory::Result<Arc<Self>> {
        let directory = work_location()?.temporary(prefix)?;
        Self::from_temporary(directory, inventory, source, root)
    }
    fn temporary_for_acquisition(
        prefix: &str,
        inventory: InventoryId,
        source: SourceId,
        guard: &Guard,
        location: &placement::Workspace,
        budget: DiskBudget,
        mut placement_log: Option<&mut std::fs::File>,
    ) -> sortsize_store::inventory::Result<Arc<Self>> {
        let directory = location.temporary(prefix)?;
        if !guard.work_path_allowed(directory.path())? {
            return Err(Error::Contract(
                "working directory is outside the verified workspace",
            ));
        }
        let admission =
            placement::check_disk_admission_at(directory.path(), directory.path(), budget);
        if let (Some(file), Err(Error::DiskAdmission(refused))) =
            (placement_log.as_mut(), &admission)
        {
            use std::io::Write;
            let _ = writeln!(
                file,
                "step=placement_quota_refused location={:?} requested={} used={} reserved={} limit={} free={} minimum_limit={}",
                directory.path(),
                refused.requested,
                refused.used,
                refused.reserved,
                refused.limit,
                refused.store_free,
                refused.minimum_limit
            );
        }
        admission?;
        Self::from_temporary(directory, inventory, source, guard.root().to_owned())
    }
    fn acquisition_view(
        &self,
        guard: &Guard,
        location: &placement::Workspace,
        prefix: &str,
        budget: DiskBudget,
    ) -> sortsize_store::inventory::Result<tempfile::TempDir> {
        let store = self.path();
        require_work_location(location, &store)?;
        if !guard.work_path_allowed(&store)? {
            return Err(Error::Contract(
                "inventory overlaps the acquired source outside its verified workspace",
            ));
        }
        let temp = if let Some(directory) = &self.directory
            && self.original.as_ref() == Some(&store)
        {
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(directory.path())?
        } else {
            location.temporary(prefix)?
        };
        if !guard.work_path_allowed(temp.path())? {
            return Err(Error::Contract(
                "scratch overlaps the acquired source outside its verified workspace",
            ));
        }
        placement::check_disk_admission_at(&store, temp.path(), budget)?;
        Ok(temp)
    }
    fn from_temporary(
        directory: tempfile::TempDir,
        inventory: InventoryId,
        source: SourceId,
        root: PathBuf,
    ) -> sortsize_store::inventory::Result<Arc<Self>> {
        let repository = Repository::open(&directory.path().join("repository"), inventory, source)?;
        let original = repository.directory().to_owned();
        let owner = Arc::new(Mutex::new(repository));
        Ok(Arc::new(Self {
            directory: Some(directory),
            original: Some(original),
            owner: Some(owner),
            writable: std::sync::atomic::AtomicBool::new(true),
            private_retired: Mutex::new(false),
            cancelled_candidate: Mutex::new(None),
            inventory,
            source,
            root,
            snapshot_owner: None,
        }))
    }
    pub(super) fn persistent(repository: Repository, root: PathBuf, writable: bool) -> Arc<Self> {
        let (inventory, source) = repository.identity();
        Arc::new(Self {
            directory: None,
            original: None,
            owner: Some(Arc::new(Mutex::new(repository))),
            writable: std::sync::atomic::AtomicBool::new(writable),
            private_retired: Mutex::new(false),
            cancelled_candidate: Mutex::new(None),
            inventory,
            source,
            root,
            snapshot_owner: None,
        })
    }
    pub(super) fn require_writer(&self) -> sortsize_store::inventory::Result<()> {
        use std::sync::atomic::Ordering;
        if !self.writable.load(Ordering::Acquire) {
            let mut owner = self.owner.as_ref().unwrap().lock().unwrap();
            if !self.writable.load(Ordering::Relaxed) {
                let writable = Repository::open_existing(owner.directory())?;
                *owner = writable;
                self.writable.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
    pub(super) fn repository(self: &Arc<Self>) -> RepositoryLease {
        RepositoryLease {
            owner: self.owner.as_ref().unwrap().clone(),
            _workspace: self.clone(),
        }
    }
    pub(super) fn path(&self) -> PathBuf {
        self.owner
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .directory()
            .to_owned()
    }
    pub(super) fn root(&self) -> &Path {
        &self.root
    }
    fn cancelled_candidate(&self) -> Option<sortsize_store::inventory::catalogue::Saved> {
        let (context, started) = (*self.cancelled_candidate.lock().unwrap())?;
        Some(sortsize_store::inventory::catalogue::Saved {
            directory: self.path(),
            context,
            root: self.root.clone(),
            started,
            measured: None,
            coverage: ScopeCoverage::Unknown,
            logical: None,
            unknown: 1,
            continuity: None,
        })
    }
    /// Local metadata only. Call on a worker, never while holding the UI lane.
    pub(super) fn saved_records(
        &self,
        generation: Option<GenerationId>,
    ) -> sortsize_store::inventory::Result<Vec<sortsize_store::inventory::catalogue::Saved>> {
        let owner = self.owner.as_ref().unwrap().lock().unwrap();
        let mut saved = Vec::new();
        if let Some(descriptor) = owner
            .descriptors()?
            .into_iter()
            .filter(|d| {
                d.measured.is_some()
                    && generation.is_none_or(|generation| d.generation == generation)
            })
            .max_by_key(|d| (d.measured, d.generation.0))
        {
            saved.push(sortsize_store::inventory::catalogue::Saved {
                directory: owner.directory().to_owned(),
                context: descriptor.context,
                root: descriptor.root,
                started: descriptor.started,
                measured: descriptor.measured,
                coverage: descriptor.coverage,
                logical: None,
                unknown: 0,
                continuity: None,
            });
        }
        Ok(saved)
    }
    pub(super) fn is_temporary(&self) -> bool {
        self.directory.is_some()
    }
    pub(super) fn retire_original(&self) {
        let Some(original) = &self.original else {
            return;
        };
        if self.path() == *original {
            return;
        }
        let mut retired = self.private_retired.lock().unwrap();
        if *retired {
            return;
        }
        match Repository::open_existing(original)
            .and_then(|repository| repository.forget(&Default::default()))
        {
            Ok(()) => *retired = true,
            Err(sortsize_store::inventory::Error::Contract(
                "inventory already owned or native lock unavailable",
            )) => (),
            Err(error) => *super::cache::CLEANUP_ERROR.lock().unwrap() = Some(error.to_string()),
        }
    }
    pub(super) fn mark_writable(&self) {
        self.writable
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
impl Drop for RepositoryWorkspace {
    fn drop(&mut self) {
        let owner = self.owner.take().unwrap();
        match Arc::try_unwrap(owner) {
            Ok(owner) => drop(owner.into_inner().unwrap()),
            Err(_) => {
                if let Some(directory) = self.directory.take() {
                    let _preserved = directory.keep();
                }
                *super::cache::CLEANUP_ERROR.lock().unwrap() =
                    Some("repository owner still leased".into());
                return;
            }
        }
        if let Some(directory) = self.directory.take() {
            if *self.private_retired.get_mut().unwrap() {
                if let Err(error) = directory.close() {
                    *super::cache::CLEANUP_ERROR.lock().unwrap() = Some(error.to_string());
                }
                return;
            }
            let result =
                Repository::open(self.original.as_ref().unwrap(), self.inventory, self.source)
                    .and_then(|repository| repository.forget(&Default::default()));
            if let Err(error) = result {
                let _preserved = directory.keep();
                *super::cache::CLEANUP_ERROR.lock().unwrap() = Some(error.to_string());
            } else if let Err(error) = directory.close() {
                *super::cache::CLEANUP_ERROR.lock().unwrap() = Some(error.to_string());
            }
        }
    }
}
// Every external repository handle retains its cleanup workspace. Local binding
// destruction is reverse declaration order, so an independent owner Arc could
// otherwise outlive the workspace during a failed start or source replacement.
// Fields drop in declaration order: release the native owner before the final
// workspace reference attempts guarded repository retirement.
pub(super) struct RepositoryLease {
    owner: RepositoryOwner,
    _workspace: Arc<RepositoryWorkspace>,
}
impl RepositoryLease {
    pub(super) fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Repository>> {
        self.owner.lock()
    }
}
struct RepositoryState {
    owner: RepositoryLease,
    plan: Option<Pipeline>,
    published: Option<GenerationId>,
}
impl Active {
    pub(super) fn is_fast_preview(&self) -> bool {
        self.fast_mode == FastMode::Reader
    }

    pub(super) fn continue_fast_navigation(&mut self, previous: &Self) {
        if !previous.is_fast_preview()
            || self.c != previous.c
            || self.source_epoch != previous.source_epoch
        {
            return;
        }
        self.parent = previous.parent;
        self.sort = previous.sort;
        self.sort_index = previous.sort_index;
        self.owner_labels = previous.owner_labels.clone();
        self.anchor = previous.anchor;
        if let Ok(tree) = previous.folder_tree.borrow().copy_navigation() {
            self.folder_tree = RefCell::new(tree);
        }
    }

    fn owns_acquisition(&self) -> bool {
        matches!(self.session, Backend::Live(_)) && self.fast_mode != FastMode::PreviewWriter
    }

    fn attach_fast_reader(&mut self, lane: &Mutex<Lane>) -> sortsize_store::inventory::Result<()> {
        self.close_live_preview()?;
        let workspace = self.workspace.as_ref().unwrap().clone();
        let store = self
            .repository
            .as_mut()
            .unwrap()
            .plan
            .as_mut()
            .unwrap()
            .sealed_fast_reader(self.session.store(), self.c)?;
        let mut view = Self::cached(self.epoch, store, self.c.generation)?;
        view.workspace = Some(workspace);
        view.fast_mode = FastMode::Reader;
        view.source_epoch = self.source_epoch;
        view.parent = self.parent;
        view.sort = self.sort;
        view.sort_index = self.sort_index;
        view.owner_labels = self.owner_labels.clone();
        view.anchor = self.anchor;
        view.folder_tree = RefCell::new(self.folder_tree.borrow().copy_navigation()?);

        let mut l = lane.lock().unwrap();
        if l.source_epoch != self.source_epoch || self.publication_cancel.is_cancelled() {
            return Ok(());
        }
        self.fast_mode = FastMode::PreviewWriter;
        view.epoch = l.view_epoch;
        view.p1_handoff_at = scan_p1::enabled().then(Instant::now);
        l.reader_active = true;
        l.writer_running = false;
        l.writer_owns_session = false;
        l.writer_epoch = None;
        let previous = l.reader_target.replace(Some(Box::new(view)));
        drop(l);
        drop(previous);
        Ok(())
    }

    fn retain_fast(
        &mut self,
        destination: &Path,
        cancel: &sortsize_core::CancellationToken,
    ) -> sortsize_store::inventory::Result<()> {
        if self.fast_mode != FastMode::Saving || self.cancelled || cancel.is_cancelled() {
            return Err(Error::Contract(tr!(
                "Wait until the quick analysis is available before saving."
            )));
        }
        drop(self.repository.as_mut().unwrap().plan.take());
        let mut plan = Pipeline::for_publication()?;
        let source = self.session.store();
        // Reserve a new family. The source and any previous inventory are
        // untouched by extraction, certification, or a failed publication.
        let mut repository = Repository::create_new(destination, self.c.inventory, self.c.source)?;
        let outcome = repository.adopt_legacy_with_plan(
            source,
            self.c.generation,
            &self.root,
            Repository::publication_budget(source)?,
            cancel,
            &mut plan,
        )?;
        if !outcome.committed() {
            return Err(Error::Contract(
                "published generation preserved; catalogue recovery required",
            ));
        }
        let certified = repository
            .open_latest_with_plan(&mut plan, cancel)?
            .ok_or(Error::Contract("published generation missing"))?;
        let workspace = RepositoryWorkspace::persistent(repository, self.root.clone(), true);
        let mut view = Self::certified(self.epoch, certified, workspace.clone())?;
        view.source_epoch = self.source_epoch;
        view.parent = self.parent;
        view.sort = self.sort;
        view.sort_index = self.sort_index;
        view.owner_labels = self.owner_labels.clone();
        view.anchor = self.anchor;
        view.retained_copy = Some(destination.to_owned());
        if cancel.is_cancelled() {
            return Err(Error::Contract("generation preparation cancelled"));
        }
        workspace
            .repository()
            .lock()
            .unwrap()
            .set_retention(true, 30)?;
        *self = view;
        Ok(())
    }
    /// Revoke this source intention permanently, without touching its retained
    /// inventories. An ordinary user Cancel uses the independent control lane.
    pub(super) fn request_cancel(&mut self) {
        self.source_revoked = true;
        self.cancel_acquisition();
    }
    pub(super) fn publication_token(&self) -> sortsize_core::CancellationToken {
        self.publication_cancel.clone()
    }
    /// Stop a user-cancelled operation while retaining explicit Retry support.
    pub(super) fn cancel_acquisition(&mut self) {
        self.cancelled = true;
        self.session.control().cancel();
        self.publication_cancel.cancel();
    }

    pub(super) fn retain(
        &mut self,
        root: &Path,
        cancel: &sortsize_core::CancellationToken,
    ) -> sortsize_store::inventory::Result<PathBuf> {
        self.close_live_preview()?;
        sortsize_store::inventory::lifecycle::ensure_root(root)?;
        let root = std::fs::canonicalize(root)?;
        let workspace = self
            .workspace
            .as_ref()
            .ok_or(sortsize_store::inventory::Error::Contract(
                "retained repository owner unavailable",
            ))?
            .clone();
        if workspace.path().parent() == Some(root.as_path()) && !workspace.is_temporary() {
            workspace.require_writer()?;
            workspace
                .repository()
                .lock()
                .unwrap()
                .set_retention(true, 30)?;
            self.retained_copy = Some(workspace.path());
            return Ok(workspace.path());
        }
        // A previous copy can be the live owner even while old temporary G0
        // readers still keep the original directory alive.
        if workspace.path().parent() == Some(root.as_path()) {
            workspace.require_writer()?;
            workspace
                .repository()
                .lock()
                .unwrap()
                .set_retention(true, 30)?;
            return Ok(workspace.path());
        }
        if let Some(repository) = &mut self.repository {
            drop(repository.plan.take());
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| sortsize_store::inventory::Error::Contract("cache timestamp unavailable"))?
            .as_nanos();
        let destination = root.join(format!("inv-{:032x}-a-{nonce:032x}", self.c.inventory.0));
        let owner = workspace.repository();
        if matches!(
            self.fast_mode,
            FastMode::Acquiring | FastMode::PreviewWriter | FastMode::Saving
        ) {
            drop(owner);
            self.retain_fast(&destination, cancel)?;
            return Ok(destination);
        }
        let mut repository = owner.lock().unwrap();
        let live = matches!(self.session, Backend::Live(_));
        let candidate = repository.candidate_path(self.c.generation);
        let mut retained = if live || self.session.store().directory() == candidate {
            repository.copy_to_from_active(&destination, self.session.store(), cancel)?
        } else {
            repository.copy_to(&destination, cancel)?
        };
        retained.set_retention(true, 30)?;
        if live {
            let mut replacement_plan = Pipeline::plan()?;
            let replacement = retained.open_candidate(self.c.generation, &mut replacement_plan)?;
            self.replace_store(replacement)?;
            if let Some(repository) = &mut self.repository {
                repository.plan = Some(replacement_plan);
            }
        }
        *repository = retained;
        drop(repository);
        workspace.mark_writable();
        workspace.retire_original();
        self.retained_copy = Some(destination.clone());
        Ok(destination)
    }
    pub(super) fn is_snapshot_reference(&self) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|workspace| workspace.snapshot_owner.is_some())
    }

    pub(super) fn open_saved(
        epoch: u64,
        saved: &sortsize_store::inventory::catalogue::Saved,
        reused: Option<Arc<RepositoryWorkspace>>,
        cancel: &sortsize_core::CancellationToken,
    ) -> sortsize_store::inventory::Result<Self> {
        if super::cache::is_snapshot(saved) {
            let location = work_location()?;
            let reference = sortsize_store::direct_snapshot::canonical_reference(
                &saved.directory,
                location.path(),
                cancel,
            )?;
            let (repository, reader) = reference.reader;
            let actual = reader.as_store().saved(saved.context.generation)?;
            if actual.context != saved.context
                || actual.root != saved.root
                || actual.measured != saved.measured
            {
                return Err(Error::Contract(
                    "saved snapshot changed; refresh the catalogue",
                ));
            }
            // The reference is a private copy opened with a writer lock.
            // Reuse that lock for explicit verification of the source.
            let mut workspace =
                RepositoryWorkspace::persistent(repository, saved.root.clone(), true);
            Arc::get_mut(&mut workspace).unwrap().snapshot_owner = Some(reference.owner);
            return Self::certified(epoch, reader, workspace);
        }
        let repository_path = if saved.directory.join("inventory.db").exists() {
            Repository::resolve_legacy(&saved.directory, cancel)?
        } else {
            Some(saved.directory.clone())
        };
        if let Some(path) = repository_path {
            let workspace = if let Some(reused) = reused.filter(|owner| owner.path() == path) {
                reused
            } else {
                RepositoryWorkspace::persistent(
                    Repository::open_readonly(&path)?,
                    saved.root.clone(),
                    false,
                )
            };
            let owner = workspace.repository();
            if owner
                .lock()
                .unwrap()
                .published(saved.context.generation)?
                .is_some()
            {
                let mut plan = Pipeline::plan()?;
                let certified = owner.lock().unwrap().open_generation_with_plan(
                    saved.context.generation,
                    &mut plan,
                    cancel,
                )?;
                drop(plan);
                let mut view = Self::certified(epoch, certified, workspace)?;
                view.retained_copy = Some(path);
                return Ok(view);
            }
            let candidate = owner
                .lock()
                .unwrap()
                .candidate_path(saved.context.generation);
            if candidate.exists() {
                let store = Store::certify_readonly(&candidate, cancel)?;
                let mut view = Self::cached(epoch, store, saved.context.generation)?;
                view.workspace = Some(workspace);
                view.retained_copy = Some(path);
                return Ok(view);
            }
            if !saved.directory.join("inventory.db").exists() {
                return Err(sortsize_store::inventory::Error::Contract(
                    "selected saved generation is absent",
                ));
            }
            if saved.measured.is_some() {
                let store = Store::certify_readonly(&saved.directory, cancel)?;
                let mut view = Self::cached(epoch, store, saved.context.generation)?;
                view.workspace = Some(workspace);
                view.retained_copy = Some(path);
                return Ok(view);
            }
        }
        let store = Store::certify_readonly(&saved.directory, cancel)?;
        Self::cached(epoch, store, saved.context.generation)
    }
    #[cfg(test)]
    fn open(
        epoch: u64,
        start: Start,
        previous: Option<(RepositoryState, Arc<RepositoryWorkspace>)>,
        publication_cancel: sortsize_core::CancellationToken,
    ) -> Result<Self, String> {
        Self::open_with_placement_log(
            epoch,
            start,
            previous,
            publication_cancel,
            acquisition_budget(false),
            None,
            false,
        )
    }
    fn open_with_placement_log(
        epoch: u64,
        start: Start,
        previous: Option<(RepositoryState, Arc<RepositoryWorkspace>)>,
        publication_cancel: sortsize_core::CancellationToken,
        budget: DiskBudget,
        placement_log: Option<&mut std::fs::File>,
        fast_requested: bool,
    ) -> Result<Self, String> {
        if publication_cancel.is_cancelled() {
            return Err(tr!("Cancelled").into());
        }
        if start.preflight.is_some_and(|f| !f()) {
            return Err(tr!("Source is no longer available.").into());
        }
        let options = start.options;
        let location = work_location().map_err(super::acquisition_error_message)?;
        let guard = Guard::open(
            &start.root,
            Policy {
                explicit_root: start.explicit_root,
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())?;
        let guard = location
            .scope(guard)
            .map_err(super::acquisition_error_message)?
            .with_scan_observation(start.observation);
        if publication_cancel.is_cancelled() {
            return Err(tr!("Cancelled").into());
        }
        let root = guard.root().to_owned();
        let fast = cfg!(windows)
            && fast_requested
            && guard.evidence().0.class == sortsize_core::acquisition::SourceClass::Local;
        let mut c = fresh_context(
            epoch,
            &root,
            std::time::SystemTime::now(),
            start.observation,
        )
        .map_err(super::acquisition_error_message)?;
        // Textual path equality is insufficient after a mount or native root
        // replacement. Certify continuity before reserving future pipeline slots.
        let previous = match previous {
            Some((mut repository, workspace))
                if !fast
                    && workspace.root == root
                    && location.permits_writer(&workspace.path()).unwrap_or(false)
                    && guard.work_path_allowed(&workspace.path()).unwrap_or(false) =>
            {
                drop(repository.plan.take());
                let same_source = repository
                    .owner
                    .lock()
                    .unwrap()
                    .source_matches(&guard, &publication_cancel)
                    .map_err(|e| e.to_string())?;
                if same_source {
                    Some((repository, workspace))
                } else {
                    None
                }
            }
            _ => None,
        };
        let (mut repository, workspace) = if let Some((repository, workspace)) = previous {
            let RepositoryState { owner, plan, .. } = repository;
            drop(plan);
            (
                RepositoryState {
                    owner,
                    plan: None,
                    published: None,
                },
                workspace,
            )
        } else {
            let workspace = RepositoryWorkspace::temporary_for_acquisition(
                if fast {
                    "bunny-fast-generations-"
                } else {
                    "bunny-generations-"
                },
                c.inventory,
                c.source,
                &guard,
                &location,
                budget,
                placement_log,
            )
            .map_err(super::acquisition_error_message)?;
            let owner = workspace.repository();
            (
                RepositoryState {
                    owner,
                    plan: None,
                    published: None,
                },
                workspace,
            )
        };
        let temp = workspace
            .acquisition_view(&guard, &location, "bunny-v05-", budget)
            .map_err(super::acquisition_error_message)?;
        if publication_cancel.is_cancelled() {
            return Err(tr!("Cancelled").into());
        }
        c.inventory = workspace.inventory;
        c.source = workspace.source;
        workspace
            .require_writer()
            .map_err(|error| error.to_string())?;
        c = repository
            .owner
            .lock()
            .unwrap()
            .next_context(c)
            .map_err(|e| e.to_string())?;
        repository.plan = Some(Pipeline::plan().map_err(|e| e.to_string())?);
        let store = {
            let owner = repository.owner.lock().unwrap();
            let plan = repository.plan.as_mut().unwrap();
            if fast {
                owner.create_fast_candidate_store(c, plan)
            } else {
                owner.create_candidate_store(c, plan)
            }
        }
        .map_err(super::acquisition_error_message)?;
        let mut session = Session::new_with_options(store, c, guard, options)
            .map_err(super::acquisition_error_message)?;
        let started = std::time::SystemTime::now();
        session
            .store_mut()
            .record_acquisition(c, &root, super::cache::seconds(started))
            .map_err(super::acquisition_error_message)?;
        Ok(Self {
            session: Backend::Live(Box::new(session)),
            c,
            root,
            _temporary: Scratch::new(temp),
            started,
            sealed: None,
            parent: EntryKey {
                inventory: c.inventory,
                generation: c.generation,
                entry: EntryId(0),
            },
            cursor: None,
            next: Default::default(),
            folder_tree: Default::default(),
            live_preview: None,
            preview_floor: 0,
            fold: FolderFold::default(),
            epoch,
            source_epoch: epoch,
            last: Instant::now(),
            p1_handoff_at: None,
            revision: None,
            force: true,
            measured: false,
            last_state: State::Running,
            sort: Some(sortsize_store::inventory::ordered::Sort::SizeDown),
            sort_index: 0,
            owner_labels: Vec::new(),
            anchor: None,
            ordered: None,
            ordered_page: None,
            pending_page: None,
            repository: Some(repository),
            workspace: Some(workspace),
            retained_copy: None,
            publication_cancel,
            preparation_blocked: false,
            cancelled: false,
            source_revoked: false,
            fast_mode: if fast {
                FastMode::Acquiring
            } else {
                FastMode::Standard
            },
        })
    }
    pub(super) fn cached(
        epoch: u64,
        store: Store,
        g: GenerationId,
    ) -> sortsize_store::inventory::Result<Self> {
        let saved = store.saved(g)?;
        let temp = work_location()?.temporary("bunny-cache-view-")?;
        let interrupted = saved.measured.is_none();
        Ok(Self {
            session: Backend::Cached(Box::new(store)),
            c: saved.context,
            root: saved.root,
            _temporary: Scratch::new(temp),
            started: super::cache::system_time(saved.started)?,
            sealed: saved.measured.map(super::cache::system_time).transpose()?,
            parent: EntryKey {
                inventory: saved.context.inventory,
                generation: g,
                entry: EntryId(0),
            },
            cursor: None,
            next: Default::default(),
            folder_tree: Default::default(),
            live_preview: None,
            preview_floor: 0,
            fold: FolderFold::default(),
            epoch,
            source_epoch: epoch,
            last: Instant::now(),
            p1_handoff_at: None,
            revision: None,
            force: true,
            measured: false,
            last_state: State::Sealed,
            // A preserved raw candidate can be browsed in observation order.
            // The immutable order builder requires a sealed generation.
            sort: (!interrupted).then_some(sortsize_store::inventory::ordered::Sort::SizeDown),
            sort_index: if interrupted { -1 } else { 0 },
            owner_labels: Vec::new(),
            anchor: None,
            ordered: None,
            ordered_page: None,
            pending_page: None,
            repository: None,
            workspace: None,
            retained_copy: None,
            publication_cancel: Default::default(),
            preparation_blocked: false,
            cancelled: false,
            source_revoked: false,
            fast_mode: FastMode::Standard,
        })
    }
    pub(super) fn certified(
        epoch: u64,
        certified: CertifiedGeneration,
        workspace: Arc<RepositoryWorkspace>,
    ) -> sortsize_store::inventory::Result<Self> {
        let g = certified.generation();
        let saved = certified.as_store().saved(g)?;
        let location = work_location()?;
        // Live acquisitions already chose a private root. Keep G0's
        // query scratch beside that repository instead of falling back onto a
        // drive that may be the source of the next acquisition. Persistent
        // catalogues retain their strict layout and offline-opening behavior.
        let temp = if let Some(directory) = &workspace.directory
            && location.contains(directory.path())?
        {
            tempfile::Builder::new()
                .prefix("bunny-certified-view-")
                .tempdir_in(directory.path())?
        } else {
            location.temporary("bunny-certified-view-")?
        };
        Ok(Self {
            session: Backend::Certified(Box::new(certified)),
            c: saved.context,
            root: saved.root,
            _temporary: Scratch::new(temp),
            started: super::cache::system_time(saved.started)?,
            sealed: saved.measured.map(super::cache::system_time).transpose()?,
            parent: EntryKey {
                inventory: saved.context.inventory,
                generation: g,
                entry: EntryId(0),
            },
            cursor: None,
            next: Default::default(),
            folder_tree: Default::default(),
            live_preview: None,
            preview_floor: 0,
            fold: FolderFold::default(),
            epoch,
            source_epoch: epoch,
            last: Instant::now(),
            p1_handoff_at: None,
            revision: None,
            force: true,
            measured: false,
            last_state: State::Sealed,
            sort: Some(sortsize_store::inventory::ordered::Sort::SizeDown),
            sort_index: 0,
            owner_labels: vec![],
            anchor: None,
            ordered: None,
            ordered_page: None,
            pending_page: None,
            repository: None,
            workspace: Some(workspace),
            retained_copy: None,
            publication_cancel: Default::default(),
            preparation_blocked: false,
            cancelled: false,
            source_revoked: false,
            fast_mode: FastMode::Standard,
        })
    }
    fn take_repository(&mut self) -> Option<(RepositoryState, Arc<RepositoryWorkspace>)> {
        Some((self.repository.take()?, self.workspace.take()?))
    }
    #[cfg(test)]
    fn publish(&mut self, lane: &Mutex<Lane>) -> sortsize_store::inventory::Result<Option<Active>> {
        self.publish_measured(lane, None)
    }
    fn publish_measured(
        &mut self,
        lane: &Mutex<Lane>,
        mut work: Option<&mut scan_p1::PublicationWork>,
    ) -> sortsize_store::inventory::Result<Option<Active>> {
        let _trace = sortsize_store::scan_trace::phase("service_publish");
        scan_p1::measure(work.as_mut().map(|m| &mut m.close_preview), || {
            self.close_live_preview()
        })?;
        if self.repository.is_none() {
            self.adopt_repository()?;
        }
        let repository = self.repository.as_mut().unwrap();
        if repository.published == Some(self.c.generation) {
            return Ok(None);
        }
        let cancel = self.publication_cancel.clone();
        if cancel.is_cancelled() {
            return Err(sortsize_store::inventory::Error::Contract(
                "generation preparation cancelled",
            ));
        }
        if repository
            .plan
            .as_ref()
            .is_none_or(|plan| plan.remaining().reader == 0)
        {
            drop(repository.plan.take());
            repository.plan = Some(Pipeline::for_publication()?);
        }
        let certified = if repository
            .owner
            .lock()
            .unwrap()
            .published(self.c.generation)?
            .is_none()
        {
            let (outcome, reader) =
                scan_p1::measure(work.as_mut().map(|m| &mut m.repository_publish), || {
                    repository
                        .owner
                        .lock()
                        .unwrap()
                        .publish_with_plan_and_reader(
                            self.session.store(),
                            self.c.generation,
                            &self.root,
                            Repository::publication_budget(self.session.store())?,
                            &cancel,
                            repository.plan.as_mut().unwrap(),
                        )
                })?;
            if !outcome.committed() {
                return Err(sortsize_store::inventory::Error::Contract(
                    "published generation preserved; catalogue recovery required",
                ));
            }
            // The repository checked CURRENT and its descriptor after commit.
            // Keep its destination capability, RO transaction and reader lease;
            // a missing transfer is a contract failure, not a reason to recertify.
            reader.ok_or(sortsize_store::inventory::Error::Contract(
                "committed generation reader missing",
            ))?
        } else {
            // Existing publications and explicit recovery still reopen through
            // the full certification path. Do not advertise this phase when a
            // newly published capability was transferred without reopening.
            {
                let mut l = lane.lock().unwrap();
                if l.source_epoch == self.source_epoch {
                    l.phase = Some((self.source_epoch, Phase::Reopening, self.c.generation));
                }
            }
            scan_p1::measure(work.map(|m| &mut m.open_certified), || {
                repository
                    .owner
                    .lock()
                    .unwrap()
                    .open_latest_with_plan(repository.plan.as_mut().unwrap(), &cancel)
            })?
            .ok_or(sortsize_store::inventory::Error::Contract(
                "published generation missing",
            ))?
        };
        if cancel.is_cancelled() {
            return Err(sortsize_store::inventory::Error::Contract(
                "generation preparation cancelled",
            ));
        }
        if certified.generation() != self.c.generation {
            return Err(sortsize_store::inventory::Error::Contract(
                "published generation is not selected; catalogue recovery required",
            ));
        }
        let mut view = Self::certified(
            self.epoch,
            certified,
            self.workspace.as_ref().unwrap().clone(),
        )?;
        // Navigation advances the view epoch independently from the acquisition
        // intention. Certification must preserve that source fence at handoff.
        view.source_epoch = self.source_epoch;
        view.cancelled = self.cancelled;
        view.parent = self.parent;
        view.sort = self.sort;
        view.sort_index = self.sort_index;
        view.owner_labels = self.owner_labels.clone();
        view.anchor = self.anchor;
        view.folder_tree = RefCell::new(self.folder_tree.borrow().copy_navigation()?);
        view.retained_copy = self.retained_copy.clone();
        repository.published = Some(self.c.generation);
        Ok(Some(view))
    }
    fn adopt_repository(&mut self) -> sortsize_store::inventory::Result<()> {
        if self.repository.is_some() {
            return Ok(());
        }
        let location = work_location()?;
        let workspace = if let Some(workspace) = &self.workspace {
            require_work_location(&location, &workspace.path())?;
            workspace.require_writer()?;
            workspace.clone()
        } else if self.session.store().retention()?.enabled {
            let source = self.session.store();
            let companion = Repository::companion_path(source.directory())?;
            require_work_location(&location, source.directory())?;
            require_work_location(&location, &companion)?;
            let mut repository = Repository::open(&companion, self.c.inventory, self.c.source)?;
            repository.bind_legacy_origin(source)?;
            repository.set_retention(true, source.retention()?.days)?;
            self.retained_copy = Some(companion);
            RepositoryWorkspace::persistent(repository, self.root.clone(), true)
        } else {
            RepositoryWorkspace::temporary(
                "bunny-adopted-generation-",
                self.c.inventory,
                self.c.source,
                self.root.clone(),
            )?
        };
        let owner = workspace.repository();
        let plan = Pipeline::plan()?;
        self.repository = Some(RepositoryState {
            owner,
            plan: Some(plan),
            published: None,
        });
        self.workspace = Some(workspace);
        let repository = self.repository.as_mut().unwrap();
        let outcome = repository.owner.lock().unwrap().adopt_legacy_with_plan(
            self.session.store(),
            self.c.generation,
            &self.root,
            Repository::publication_budget(self.session.store())?,
            &self.publication_cancel,
            repository.plan.as_mut().unwrap(),
        )?;
        if !outcome.committed() {
            return Err(sortsize_store::inventory::Error::Contract(
                "legacy copy preserved; catalogue recovery required",
            ));
        }
        Ok(())
    }
    fn into_reader(
        mut self,
    ) -> sortsize_store::inventory::Result<(Self, (RepositoryState, Arc<RepositoryWorkspace>))>
    {
        self.close_live_preview()?;
        self.adopt_repository()?;
        let repository = self.repository.as_mut().unwrap();
        let certified = repository
            .owner
            .lock()
            .unwrap()
            .open_latest_with_plan(repository.plan.as_mut().unwrap(), &self.publication_cancel)?
            .ok_or(sortsize_store::inventory::Error::Contract(
                "adopted generation missing",
            ))?;
        let mut view = Self::certified(
            self.epoch,
            certified,
            self.workspace.as_ref().unwrap().clone(),
        )?;
        view.source_epoch = self.source_epoch;
        view.parent = self.parent;
        view.sort = self.sort;
        view.sort_index = self.sort_index;
        view.owner_labels = self.owner_labels.clone();
        view.anchor = self.anchor;
        view.folder_tree = RefCell::new(self.folder_tree.borrow().copy_navigation()?);
        view.retained_copy = self.retained_copy.clone();
        let mut owner = self.take_repository().unwrap();
        drop(owner.0.plan.take());
        drop(self);
        Ok((view, owner))
    }
    pub(super) fn certified_generation(&self) -> Option<&CertifiedGeneration> {
        match &self.session {
            Backend::Certified(capability) => Some(capability),
            _ => None,
        }
    }
    pub(super) fn sync_sort(&mut self, index: u8) -> bool {
        let index = i32::from(index.min(11));
        if self.sort_index >= 0 && self.sort_index != index {
            self.sort = sortsize_store::inventory::ordered::Sort::from_index(index);
            self.sort_index = index;
            self.anchor = None;
            self.ordered = None;
            self.ordered_page = None;
            self.cursor = None;
            self.force = true;
            return true;
        }
        false
    }
    pub(super) fn view_anchor(&mut self, lane: &Mutex<Lane>) {
        if self.ordered_page.is_none() {
            return;
        }
        self.anchor = lane
            .lock()
            .unwrap()
            .detail_anchor
            .filter(|(parent, key)| {
                *parent == self.parent.entry
                    && key.inventory == self.c.inventory
                    && key.generation == self.c.generation
            })
            .map(|(_, key)| key.entry);
    }
    pub(super) fn advance_order(&mut self) -> sortsize_store::inventory::Result<bool> {
        let _trace = sortsize_store::scan_trace::phase("detail_order");
        let Some(sort) = self.sort else {
            return Ok(true);
        };
        let preview = self.live_preview.as_ref();
        if self.ordered.is_none() {
            let mut ordered = match preview {
                Some(p) => p.start_ordered_folder(
                    self.c,
                    self.parent.entry,
                    &self.root,
                    sort,
                    self._temporary.path(),
                    ORDER_DISK_BYTES,
                )?,
                None => self.session.store().start_ordered_folder(
                    self.c,
                    self.parent.entry,
                    &self.root,
                    sort,
                    self._temporary.path(),
                    ORDER_DISK_BYTES,
                )?,
            };
            ordered.set_owner_labels(&self.owner_labels)?;
            self.ordered = Some(ordered);
        }
        let order = self.ordered.as_mut().unwrap();
        let was_ready = order.ready();
        let ready = match preview {
            Some(p) => order.step_live(p)?,
            None => order.step(self.session.store(), &self.fold)?,
        };
        if !ready && preview.is_none() {
            return Ok(false);
        }
        if self.ordered_page.is_none() || !was_ready {
            // A provisional order may need several quanta. Keep the anchor
            // until completion, because the next step replaces this page.
            let anchor = if ready {
                self.anchor.take()
            } else {
                self.anchor
            };
            self.ordered_page = Some(if let Some(key) = anchor {
                order.page_containing(key)?.unwrap_or(order.page(None)?)
            } else {
                order.page(None)?
            });
        }
        // Applying the intention to a provisional page would lose it when
        // the next quantum replaces that page. Consume it only once ready.
        if ready
            && let Some(forward) = self.pending_page.take()
            && let Some(p) = &self.ordered_page
        {
            let next = if forward && p.more {
                Some(order.page(p.next.as_ref())?)
            } else if !forward {
                p.first.as_ref().map(|c| order.previous(c)).transpose()?
            } else {
                None
            };
            if let Some(next) = next {
                self.ordered_page = Some(next);
            }
        }
        Ok(ready)
    }
    pub(super) fn writable(mut self) -> sortsize_store::inventory::Result<Self> {
        if let Backend::Cached(store) = self.session {
            let directory = store.directory().to_owned();
            let expected = store.saved(self.c.generation)?;
            let revision = store.revision(self.c.generation)?;
            drop(store);
            let store = Store::reopen(&directory, Durability::Retained)?;
            let actual = store.saved(self.c.generation)?;
            if actual.context != self.c
                || actual.root != expected.root
                || actual.started != expected.started
                || actual.measured != expected.measured
                || actual.coverage != expected.coverage
                || actual.logical != expected.logical
                || actual.unknown != expected.unknown
                || store.revision(self.c.generation)? != revision
            {
                return Err(sortsize_store::inventory::Error::Contract(
                    "cached generation changed before write",
                ));
            }
            self.session = Backend::Cached(Box::new(store));
            self.fold = FolderFold::default();
            self.ordered = None;
            self.ordered_page = None;
            self.force = true;
            self.revision = None;
        }
        Ok(self)
    }
    fn resumed(
        epoch: u64,
        store: Store,
        g: GenerationId,
        guard: Guard,
        temp: tempfile::TempDir,
    ) -> sortsize_store::inventory::Result<Self> {
        let saved = store.saved(g)?;
        let session = Session::resume(store, g, guard)?;
        let c = session.context();
        Ok(Self {
            session: Backend::Live(Box::new(session)),
            c,
            root: saved.root,
            _temporary: Scratch::new(temp),
            started: super::cache::system_time(saved.started)?,
            sealed: None,
            parent: EntryKey {
                inventory: c.inventory,
                generation: g,
                entry: EntryId(0),
            },
            cursor: None,
            next: Default::default(),
            folder_tree: Default::default(),
            live_preview: None,
            preview_floor: 0,
            fold: FolderFold::default(),
            epoch,
            source_epoch: epoch,
            last: Instant::now(),
            p1_handoff_at: None,
            revision: None,
            force: true,
            measured: false,
            last_state: State::Running,
            sort: Some(sortsize_store::inventory::ordered::Sort::SizeDown),
            sort_index: 0,
            owner_labels: Vec::new(),
            anchor: None,
            ordered: None,
            ordered_page: None,
            pending_page: None,
            repository: None,
            workspace: None,
            retained_copy: None,
            publication_cancel: Default::default(),
            preparation_blocked: false,
            cancelled: false,
            source_revoked: false,
            fast_mode: FastMode::Standard,
        })
    }
    pub(super) fn resume_saved(
        epoch: u64,
        policy: Policy,
        saved: &sortsize_store::inventory::catalogue::Saved,
        reused: Option<Arc<RepositoryWorkspace>>,
        cancel: &sortsize_core::CancellationToken,
        publication_cancel: sortsize_core::CancellationToken,
    ) -> sortsize_store::inventory::Result<Self> {
        Self::resume_saved_with_options(
            epoch,
            policy,
            saved,
            reused,
            cancel,
            publication_cancel,
            ResumeOptions {
                expected_collection: None,
                disk: acquisition_budget(false),
            },
        )
    }
    fn resume_saved_with_options(
        epoch: u64,
        policy: Policy,
        saved: &sortsize_store::inventory::catalogue::Saved,
        reused: Option<Arc<RepositoryWorkspace>>,
        cancel: &sortsize_core::CancellationToken,
        publication_cancel: sortsize_core::CancellationToken,
        options: ResumeOptions,
    ) -> sortsize_store::inventory::Result<Self> {
        let location = work_location()?;
        require_work_location(&location, &saved.directory)?;
        let guard = location.scope(Guard::open(&saved.root, policy)?)?;
        let view = Self::open_saved(epoch, saved, reused.clone(), cancel)?;
        if !Session::resume_scope_matches(view.session.store(), saved.context.generation, &guard)? {
            return Err(Error::Contract(tr!(
                "The candidate has a different or uncertain scan scope. Use Analyze to start a new inventory."
            )));
        }
        if let Some(expected) = options.expected_collection
            && view
                .session
                .store()
                .collection_options(saved.context.generation)?
                != Some((expected.allocation, expected.owners))
        {
            return Err(sortsize_store::inventory::Error::Contract(tr!(
                "Restart this cancelled scan with the same allocation and owner options."
            )));
        }
        let path = view.session.store().directory().to_owned();
        require_work_location(&location, &path)?;
        if !guard.work_path_allowed(&path)? {
            return Err(Error::Contract(
                "candidate overlaps the acquired source outside its verified workspace",
            ));
        }
        if cancel.is_cancelled() {
            return Err(Error::Contract("resume cancelled"));
        }
        let mut plan = Pipeline::plan()?;
        let (workspace, store, temp) = if let Some(workspace) = view.workspace.clone() {
            // The existing candidate was accounted when its repository opened.
            // Check its actual volume before reopening or fencing the attempt.
            let temp = workspace.acquisition_view(
                &guard,
                &location,
                "bunny-resumed-view-",
                options.disk,
            )?;
            workspace.require_writer()?;
            drop(view);
            let store = workspace
                .repository()
                .lock()
                .unwrap()
                .open_candidate_cancellable(saved.context.generation, &mut plan, cancel)?;
            (workspace, store, temp)
        } else {
            // Resume is a new writer over an extracted copy. The legacy
            // authority stays certified read-only, including its guard/epochs.
            let reference = view.session.store();
            let resolved = Repository::resolve_legacy(&path, cancel)?;
            let retention = reference.retention()?;
            let workspace = if retention.enabled || resolved.is_some() {
                let companion = match resolved {
                    Some(path) => path,
                    None => Repository::companion_path(&path)?,
                };
                if let Some(owner) = reused.filter(|owner| owner.path() == companion) {
                    owner.require_writer()?;
                    owner
                } else {
                    let mut repository = Repository::open(
                        &companion,
                        saved.context.inventory,
                        saved.context.source,
                    )?;
                    repository.bind_legacy_origin(reference)?;
                    if retention.enabled {
                        repository.set_retention(true, retention.days)?;
                    }
                    RepositoryWorkspace::persistent(repository, saved.root.clone(), true)
                }
            } else {
                if cancel.is_cancelled() {
                    return Err(sortsize_store::inventory::Error::Contract(
                        "resume cancelled",
                    ));
                }
                RepositoryWorkspace::temporary_for_acquisition(
                    "bunny-resumed-generation-",
                    saved.context.inventory,
                    saved.context.source,
                    &guard,
                    &location,
                    options.disk,
                    None,
                )?
            };
            let temp = workspace.acquisition_view(
                &guard,
                &location,
                "bunny-resumed-view-",
                options.disk,
            )?;
            let store = workspace
                .repository()
                .lock()
                .unwrap()
                .adopt_candidate_cancellable(
                    reference,
                    saved.context.generation,
                    &mut plan,
                    cancel,
                )?;
            drop(view);
            (workspace, store, temp)
        };
        if cancel.is_cancelled() {
            return Err(sortsize_store::inventory::Error::Contract(
                "resume cancelled",
            ));
        }
        let mut active = Self::resumed(epoch, store, saved.context.generation, guard, temp)?;
        *workspace.cancelled_candidate.lock().unwrap() = None;
        active.publication_cancel = publication_cancel;
        active.repository = Some(RepositoryState {
            owner: workspace.repository(),
            plan: Some(plan),
            published: None,
        });
        if !workspace.is_temporary() {
            active.retained_copy = Some(workspace.path());
        }
        active.workspace = Some(workspace);
        Ok(active)
    }
    pub(super) fn verified(
        epoch: u64,
        mut reader: Self,
        after: GenerationId,
        cancel: &sortsize_core::CancellationToken,
        publication_cancel: sortsize_core::CancellationToken,
    ) -> sortsize_store::inventory::Result<(Self, Self)> {
        if reader.sealed.is_none() {
            return Err(sortsize_store::inventory::Error::Contract(
                "verification requires a measured generation",
            ));
        }
        let location = work_location()?;
        let guard = location.scope(Guard::open(
            &reader.root,
            Policy {
                explicit_root: true,
                ..Default::default()
            },
        )?)?;
        let path = reader.session.store().directory();
        require_work_location(&location, path)?;
        if !guard.work_path_allowed(path)? {
            return Err(Error::Contract(
                "inventory overlaps the acquired source outside its verified workspace",
            ));
        }
        if !matches!(reader.session, Backend::Certified(_)) {
            reader.publication_cancel = cancel.clone();
            let (view, parked) = reader.into_reader()?;
            drop(parked);
            reader = view;
        }
        let generation = reader.c.generation;
        let reference = reader.session.store();
        let saved = reference.saved(generation)?;
        if cancel.is_cancelled() {
            return Err(sortsize_store::inventory::Error::Contract(
                "verification cancelled",
            ));
        }
        if cancel.is_cancelled() {
            return Err(sortsize_store::inventory::Error::Contract(
                "verification cancelled",
            ));
        }
        let workspace = reader.workspace.as_ref().unwrap().clone();
        let temp = workspace.acquisition_view(
            &guard,
            &location,
            "bunny-verified-view-",
            acquisition_budget(false),
        )?;
        workspace.require_writer()?;
        let owner = workspace.repository();
        let c = owner.lock().unwrap().next_context_after(
            AcquisitionContext {
                scan: ScanId(epoch),
                policy: sortsize_store::scheduler::observation_policy(ScanObservation::Strict),
                mount: MountEpoch(epoch),
                run: RunEpoch(epoch),
                validation: ValidationEpoch(epoch),
                ..saved.context
            },
            after,
        )?;
        let mut plan = Pipeline::plan()?;
        let candidate = owner.lock().unwrap().create_candidate_store(c, &mut plan)?;
        let mut session = Session::verify_candidate(reference, generation, candidate, c, guard)?;
        let started = std::time::SystemTime::now();
        session
            .store_mut()
            .record_acquisition(c, &saved.root, super::cache::seconds(started))?;
        let active = Self {
            session: Backend::Live(Box::new(session)),
            c,
            root: saved.root,
            _temporary: Scratch::new(temp),
            started,
            sealed: None,
            parent: EntryKey {
                inventory: c.inventory,
                generation: c.generation,
                entry: EntryId(0),
            },
            cursor: None,
            next: Default::default(),
            folder_tree: Default::default(),
            live_preview: None,
            preview_floor: 0,
            fold: FolderFold::default(),
            epoch,
            source_epoch: epoch,
            last: Instant::now(),
            p1_handoff_at: None,
            revision: None,
            force: true,
            measured: false,
            last_state: State::Running,
            sort: Some(sortsize_store::inventory::ordered::Sort::SizeDown),
            sort_index: 0,
            owner_labels: vec![],
            anchor: None,
            ordered: None,
            ordered_page: None,
            pending_page: None,
            repository: Some(RepositoryState {
                owner,
                plan: Some(plan),
                published: None,
            }),
            workspace: Some(workspace),
            retained_copy: None,
            publication_cancel,
            preparation_blocked: false,
            cancelled: false,
            source_revoked: false,
            fast_mode: FastMode::Standard,
        };
        Ok((active, reader))
    }
    pub(super) fn replace_store(&mut self, store: Store) -> sortsize_store::inventory::Result<()> {
        self.close_live_preview()?;
        self.session.replace_store(store)?;
        self.ordered = None;
        self.ordered_page = None;
        // TEMP gauges belong to the old SQLite connection and are deliberately
        // absent from a coherent main-database backup.
        self.fold = FolderFold::default();
        self.measured = false;
        self.revision = None;
        self.force = true;
        Ok(())
    }
    fn close_live_preview(&mut self) -> sortsize_store::inventory::Result<()> {
        let _trace = sortsize_store::scan_trace::phase("preview_close");
        if self.live_preview.is_some() {
            self.ordered = None;
            self.ordered_page = None;
        }
        if let Some(preview) = self.live_preview.take() {
            self.repository
                .as_mut()
                .and_then(|repository| repository.plan.as_mut())
                .ok_or(sortsize_store::inventory::Error::Contract(
                    "live preview has no pipeline owner",
                ))?
                .restore_preview(preview)?;
        }
        Ok(())
    }
    /// Acquisition keeps ticking while a separate, pinned candidate snapshot is
    /// folded in bounded quanta. Never restart that fold on writer revisions.
    fn advance_live_preview(
        &mut self,
        refresh_after: Duration,
    ) -> sortsize_store::inventory::Result<bool> {
        let _trace = sortsize_store::scan_trace::phase("preview_fold");
        let _preview_work = progress_trace::PreviewWork::start();
        let revision = self.session.store().revision(self.c.generation)?;
        progress_trace::current(|_, counters| {
            counters.write(&[(Field::AcquiredRevision, revision.0)])
        });
        if self.live_preview.as_ref().is_some_and(|preview| {
            preview.folder_ready(self.parent.entry)
                && self.revision == Some(preview.revision())
                && preview.revision() != revision
                && self.last.elapsed() >= refresh_after
                && self.ordered.as_ref().is_none_or(|o| o.ready())
        }) {
            self.preview_floor = self
                .live_preview
                .as_ref()
                .unwrap()
                .measure(self.parent.entry)?
                .and_then(|measure| measure.logical)
                .unwrap_or(0);
            progress_trace::current(|_, counters| {
                counters.write(&[
                    (Field::Floor, self.preview_floor),
                    (Field::CatchupStarted, progress_trace::now_ns()),
                ])
            });
            self.close_live_preview()?;
        }
        if self.live_preview.is_none() {
            let _trace = sortsize_store::scan_trace::phase("preview_open");
            let plan = self
                .repository
                .as_mut()
                .and_then(|r| r.plan.as_mut())
                .ok_or(sortsize_store::inventory::Error::Contract(
                    "live preview pipeline missing",
                ))?;
            self.live_preview = Some(plan.live_preview(self.session.store(), self.c)?);
        }
        let preview = self.live_preview.as_mut().unwrap();
        let was_ready = preview.folder_ready(self.parent.entry);
        let ready = preview.step_folder(self.parent.entry, self._temporary.path())?;
        // Finish the source-wide derivative cooperatively after the visible
        // scope, so background gauges still converge without delaying clicks.
        if ready && !preview.complete() {
            preview.step(self._temporary.path())?;
        }
        if ready && !was_ready {
            self.ordered = None;
            self.ordered_page = None;
            self.force = true;
        }
        progress_trace::current(|_, counters| {
            counters.add(Field::PreviewQuanta);
            let progress = preview.progress_snapshot();
            counters.write(&[
                (Field::PreviewRevision, progress.revision),
                (Field::Cursor, progress.after),
                (Field::PendingEntry, progress.pending_entry.unwrap_or(0)),
                (Field::Ancestor, progress.pending_ancestor.unwrap_or(0)),
                (
                    Field::Applying,
                    progress.pending_applying.map_or(0, |n| n as u64 + 1),
                ),
                (Field::PriorityDone, u64::from(progress.priority_done)),
                (Field::PreviewComplete, u64::from(progress.complete)),
            ]);
        });
        Ok(ready)
    }
    pub(super) fn advance_folder_tree(&self) -> sortsize_store::inventory::Result<()> {
        let _trace = sortsize_store::scan_trace::phase("folder_tree");
        let preview = self.live_preview.as_ref();
        let store = preview.map_or_else(|| self.session.store(), |p| p.store());
        self.folder_tree.borrow_mut().advance(
            store,
            self.c,
            match preview {
                Some(preview) => preview.complete(),
                None => store
                    .folder_measure(self.c.generation, EntryId(0), &self.fold)?
                    .is_some(),
            },
            |id| match preview {
                Some(p) => p.measure(id),
                None => store.folder_measure(self.c.generation, id, &self.fold),
            },
        )
    }
    fn view(&self, id: u64) -> ViewContext {
        view_context(self.c, self.epoch, id)
    }
    pub(super) fn navigate(
        &mut self,
        epoch: u64,
        action: Navigate,
    ) -> sortsize_store::inventory::Result<()> {
        if epoch < self.epoch {
            return Ok(());
        }
        let reset = matches!(
            &action,
            Navigate::Open(_) | Navigate::Parent | Navigate::Reveal(_)
        );
        if reset {
            self.pending_page = None;
            self.preview_floor = 0;
            // A click inside the pinned snapshot reuses its measurements.
            // Only a newly discovered folder requires a newer snapshot.
            let target = match &action {
                Navigate::Open(key) | Navigate::Reveal(key) => Some(*key),
                _ => None,
            };
            if let (Some(preview), Some(key)) = (&self.live_preview, target)
                && preview
                    .store()
                    .navigation_target(self.c, self.view(1), key)?
                    .is_none()
            {
                self.close_live_preview()?;
            }
        }
        match action {
            Navigate::ToggleTree(key) => {
                if self
                    .session
                    .store()
                    .navigation_target(self.c, self.view(1), key)?
                    .is_some()
                {
                    self.folder_tree.borrow_mut().toggle(key.entry);
                }
            }
            Navigate::MoreTree(key) => {
                self.folder_tree.borrow_mut().page_branch(key.entry, true)?
            }
            Navigate::PreviousTree(key) => self
                .folder_tree
                .borrow_mut()
                .page_branch(key.entry, false)?,
            Navigate::Open(key) => {
                self.anchor = None;
                let Some(target) =
                    self.session
                        .store()
                        .navigation_target(self.c, self.view(1), key)?
                else {
                    return Ok(());
                };
                if target.kind != sortsize_store::inventory::Kind::Directory {
                    return Ok(());
                }
                self.parent = key;
                self.cursor = None;
            }
            Navigate::Reveal(key) => {
                if self
                    .session
                    .store()
                    .navigation_target(self.c, self.view(1), key)?
                    .is_some()
                    && let Some(parent) = self
                        .session
                        .store()
                        .navigation_parent(self.c.generation, key.entry)?
                {
                    self.parent.entry = parent;
                    self.anchor = Some(key.entry);
                    self.cursor = self
                        .session
                        .store()
                        .folder_cursor_for_entry(self.c, parent, key.entry)?;
                }
            }
            Navigate::Parent => {
                self.anchor = None;
                if let Some(id) = self
                    .session
                    .store()
                    .navigation_parent(self.c.generation, self.parent.entry)?
                {
                    self.parent.entry = id;
                    self.cursor = None;
                }
            }
            Navigate::OwnerSort(..) | Navigate::Sort(_) if self.sort_index < 0 => {}
            Navigate::OwnerSort(index, labels) => {
                self.pending_page = None;
                self.anchor = None;
                self.sort = sortsize_store::inventory::ordered::Sort::from_index(index);
                self.sort_index = index;
                self.owner_labels = labels;
                self.ordered = None;
                self.ordered_page = None;
                self.cursor = None;
            }
            Navigate::Sort(index) => {
                self.pending_page = None;
                self.anchor = None;
                self.sort = sortsize_store::inventory::ordered::Sort::from_index(index);
                self.sort_index = index;
                self.ordered = None;
                self.ordered_page = None;
                self.cursor = None;
            }
            Navigate::Page(forward) | Navigate::ObservedPage(forward) if self.sort.is_some() => {
                if let (Some(order), Some(p)) = (&self.ordered, &self.ordered_page) {
                    self.ordered_page = if forward && p.more {
                        Some(order.page(p.next.as_ref())?)
                    } else if !forward {
                        p.first.as_ref().map(|c| order.previous(c)).transpose()?
                    } else {
                        return Ok(());
                    };
                } else {
                    self.pending_page = Some(forward);
                }
            }
            Navigate::Page(true) | Navigate::ObservedPage(true) => {
                self.cursor = self.next.borrow().clone()
            }
            Navigate::Page(false) | Navigate::ObservedPage(false) => {
                self.cursor = match &self.cursor {
                    Some(current) => self.session.store().previous_folder_cursor(
                        self.c,
                        self.parent.entry,
                        current,
                    )?,
                    None => None,
                };
            }
            Navigate::Refresh | Navigate::Top100(_) => {}
        }
        if reset {
            self.folder_tree.borrow_mut().reveal(
                self.session.store(),
                self.c,
                self.parent.entry,
            )?;
            self.ordered = None;
            self.ordered_page = None;
        }
        self.epoch = epoch;
        self.force = true;
        self.session.control().navigate(self.parent.entry);
        Ok(())
    }
    pub(super) fn sync_requested_folder(
        &mut self,
        lane: &Mutex<Lane>,
    ) -> sortsize_store::inventory::Result<()> {
        let (desired, relative, epoch) = {
            let l = lane.lock().unwrap();
            (l.desired_folder, l.desired_relative.clone(), l.view_epoch)
        };
        if let Some(key) = desired
            .filter(|key| key.inventory == self.c.inventory && key.generation == self.c.generation)
            && key != self.parent
        {
            self.navigate(epoch, Navigate::Open(key))?;
        } else if let Some((generation, relative)) = relative
            && generation != self.c.generation
            && desired.is_none_or(|key| key.generation != self.c.generation)
            && !matches!(self.session, Backend::Live(_))
        {
            let entry = self
                .session
                .store()
                .navigation_directory(self.c, &relative)?;
            let key = EntryKey {
                inventory: self.c.inventory,
                generation: self.c.generation,
                entry,
            };
            if key != self.parent {
                self.navigate(epoch, Navigate::Open(key))?;
            }
            let mut l = lane.lock().unwrap();
            if l.source_epoch == self.source_epoch && l.view_epoch == epoch {
                l.desired_folder = Some(key);
            }
        }
        Ok(())
    }
    /// G0 and its in-progress G1 use separate entry namespaces. Translate via
    /// native relative components on the reader, never via a display label.
    pub(super) fn prioritize_reader_folder(
        &self,
        lane: &Mutex<Lane>,
    ) -> sortsize_store::inventory::Result<()> {
        let control = {
            let l = lane.lock().unwrap();
            l.acquisition_control
                .as_ref()
                .filter(|(source, generation, _)| {
                    *source == self.source_epoch && *generation != self.c.generation
                })
                .map(|(_, _, control)| control.clone())
        };
        if let Some(control) = control {
            let path = self.session.store().navigation_path(
                self.c.generation,
                self.parent.entry,
                &self.root,
            )?;
            let relative = path.strip_prefix(&self.root).map_err(|_| {
                sortsize_store::inventory::Error::Contract("reader folder outside acquisition root")
            })?;
            let relative = relative.to_owned();
            let mut l = lane.lock().unwrap();
            if self.source_epoch == l.source_epoch && self.epoch == l.view_epoch {
                l.desired_relative = Some((self.c.generation, relative.clone()));
                drop(l);
                control.navigate_relative(relative)?;
            }
        }
        Ok(())
    }
    pub(super) fn describe_folder_fallback(&self, lane: &Mutex<Lane>, page: &mut Prepared) {
        let relative = lane.lock().unwrap().desired_relative.clone();
        if let Some((generation, relative)) = relative
            && generation != self.c.generation
            && page.path != label(&self.root.join(relative))
        {
            page.status.push_str(&format!(
                " · {}",
                tr!("File no longer accessible. Check the path or network connection.")
            ));
        }
    }
    pub(super) fn prepare(
        &self,
        units: crate::units::Units,
        top_scope: Option<TopScope>,
    ) -> sortsize_store::inventory::Result<Prepared> {
        let _trace = sortsize_store::scan_trace::phase("prepare_page");
        use sortsize_core::resources::{Category, Governor};
        use sortsize_store::inventory::Kind;
        // Frame, paths, properties and weights must describe the same pinned
        // instant. A live preview is never a certified generation or final sum.
        let preview = self.live_preview.as_ref();
        let store = preview.map_or_else(|| self.session.store(), |p| p.store());
        let measure = |id| match preview {
            Some(p) => p.measure(id),
            None => store.folder_measure(self.c.generation, id, &self.fold),
        };
        let detail_trace = sortsize_store::scan_trace::phase("prepare_detail");
        let frame = if let Some(p) = &self.ordered_page {
            let page = sortsize_store::inventory::queries::Page {
                first: p.first.clone(),
                rows: p.rows.clone(),
                next: p.next.clone(),
                more: p.more,
                accounted_bytes: p.accounted_bytes,
            };
            match preview {
                Some(preview) => preview.ordered_navigation_frame(self.c, self.view(1), page)?,
                None => store.ordered_navigation_frame(self.c, self.view(1), page)?,
            }
        } else {
            store.navigation_frame(
                self.c,
                self.view(1),
                Mode::Folder {
                    parent: self.parent,
                    after: self.cursor.clone(),
                },
                &self.root,
            )?
        };
        drop(detail_trace);
        let top = {
            let _trace = sortsize_store::scan_trace::phase("prepare_top100");
            let mode = top_scope
                .as_ref()
                .map_or(Mode::Top100, |scope| Mode::FolderTop100(scope.key));
            store.navigation_frame(self.c, self.view(2), mode, &self.root)?
        };
        // Native labels may escape bytes; short_label caps output at 512 characters.
        // Include shared-string overhead, temporary formatting, both retained and
        // Slint model structs, and full native Treemap labels. Refcounts share strings.
        let text_bound = |p: &Path| (p.as_os_str().as_encoded_bytes().len() * 4).min(2048) + 128;
        let full_text_bound = |p: &Path| p.as_os_str().as_encoded_bytes().len() * 4 + 128;
        let row_bound = |r: &sortsize_store::inventory::queries::Row| {
            let name = r.path.file_name().map(Path::new).unwrap_or(&r.path);
            2 * text_bound(&r.path)
                + 4 * text_bound(name)
                + 2 * full_text_bound(&r.path)
                + 2 * full_text_bound(name)
                + 2048
        };
        let longest_path = frame
            .rows
            .iter()
            .chain(top.rows.iter())
            .map(|r| r.path.as_os_str().as_encoded_bytes().len())
            .max()
            .unwrap_or(0)
            .max(self.root.as_os_str().as_encoded_bytes().len());
        let bytes = 64 * 1024
            + (longest_path * 4 + 128) * 8
            + frame.rows.iter().map(row_bound).sum::<usize>()
            + top.rows.iter().map(row_bound).sum::<usize>()
            + 2 * frame.rows.capacity()
                * (std::mem::size_of::<DetailRow>()
                    + std::mem::size_of::<TreeRow>()
                    + std::mem::size_of::<MapTile>())
            + 2 * top.rows.capacity() * std::mem::size_of::<TopRow>()
            + frame.rows.capacity()
                * (std::mem::size_of::<
                    Option<sortsize_store::inventory::folder_totals::FolderMeasure>,
                >() + std::mem::size_of::<Option<u64>>())
            + (frame.rows.len() + 1)
                * (std::mem::size_of::<crate::treemap::Weight>()
                    + std::mem::size_of::<crate::treemap::Rect>());
        let memory = Governor::global()
            .reserve(Category::Publications, bytes as u64, None)
            .ok_or(sortsize_store::inventory::Error::Contract(
                "presentation memory unavailable; retry",
            ))?;
        let parent = measure(self.parent.entry)?;
        let whole = measure(EntryId(0))?;
        progress_trace::current(|_, counters| {
            counters.write(&[
                (Field::Logical, whole.and_then(|m| m.logical).unwrap_or(0)),
                (Field::Files, whole.map_or(0, |m| m.files)),
                (Field::FilesKnown, u64::from(whole.is_some())),
                (
                    Field::Known,
                    u64::from(whole.and_then(|m| m.logical).is_some()),
                ),
                (Field::PageRevision, frame.stamp.revision.0),
                (
                    Field::PreviewLogical,
                    parent.and_then(|m| m.logical).unwrap_or(0),
                ),
                (
                    Field::PreviewLogicalKnown,
                    u64::from(parent.and_then(|m| m.logical).is_some()),
                ),
            ])
        });
        let measures = frame
            .rows
            .iter()
            .map(|row| {
                if row.kind == Kind::Directory {
                    measure(row.key.entry)
                } else {
                    Ok(None)
                }
            })
            .collect::<sortsize_store::inventory::Result<Vec<_>>>()?;
        let logical_sizes = frame
            .rows
            .iter()
            .zip(&measures)
            .map(|(row, measure)| {
                if row.kind == Kind::Directory && self.ordered_page.is_none() {
                    measure.and_then(|m| m.logical)
                } else {
                    row.logical
                }
            })
            .collect::<Vec<_>>();
        // A live page describes only its displayed known measurements. Its
        // parent subtotal may still be folding, so it cannot be a denominator
        // for files already present in this same snapshot but not folded yet.
        // A sealed partial scope can still have an unknown parent total
        // (for example a refused link). Keep the known visible measurements
        // drawable without claiming a complete parent denominator.
        let partial_unknown_parent = parent.is_some_and(|m| m.logical.is_none())
            && (frame.provisional || frame.coverage != ScopeCoverage::Complete);
        let tile_total = if preview.is_some() || partial_unknown_parent {
            logical_sizes
                .iter()
                .flatten()
                .try_fold(0u64, |sum, value| sum.checked_add(*value))
        } else {
            parent.and_then(|m| m.logical)
        };
        let mut details = Vec::with_capacity(frame.rows.len());
        let mut owners = Vec::with_capacity(frame.rows.len());

        let mut tiles = Vec::new();
        let mut x = 0.;
        let mut tile_weights = Vec::with_capacity(frame.rows.len() + 1);
        let properties_trace = sortsize_store::scan_trace::phase("prepare_properties");
        for (index, row) in frame.rows.iter().enumerate() {
            let folder = row.kind == Kind::Directory;
            let measure = measures[index];
            let logical = logical_sizes[index];
            let props = store.entry_properties(row.key)?;
            owners.push(props.owner);
            let allocation = if folder {
                measure.and_then(|m| m.allocated).map_or(
                    sortsize_core::MetadataValue::Unknown,
                    |value| sortsize_core::MetadataValue::Known {
                        value,
                        accuracy: if frame.provisional
                            || frame.coverage != ScopeCoverage::Complete
                            || measure.is_some_and(|m| m.allocation_lower_bound)
                        {
                            sortsize_core::ValueAccuracy::LowerBound
                        } else {
                            sortsize_core::ValueAccuracy::Exact
                        },
                    },
                )
            } else {
                props.allocated
            };
            let modified = store.entry_modified(row.key)?;
            let fraction = logical
                .zip(tile_total)
                .filter(|(_, p)| *p > 0)
                .map(|(v, p)| (v as f64 / p as f64).clamp(0., 1.) as f32);
            let name = short_label(
                row.path
                    .file_name()
                    .map(std::path::Path::new)
                    .unwrap_or(&row.path),
            );
            let weight_provisional = preview.is_some()
                || frame.provisional
                || frame.coverage != ScopeCoverage::Complete
                || parent.is_some_and(|m| m.unknown > 0);
            let size = logical.map_or_else(
                || "—".into(),
                |n| {
                    if folder
                        && (preview.is_some()
                            || frame.provisional
                            || frame.coverage != ScopeCoverage::Complete)
                    {
                        if n == 0 {
                            "—".into()
                        } else {
                            format!("≥ {}", units.format(n))
                        }
                    } else {
                        units.format(n)
                    }
                },
            );
            details.push(DetailRow {
                node_id: index as i32,
                name: name.clone().into(),
                path: short_label(&row.path).into(),
                size: size.clone().into(),
                directory: folder,
                proportion: fraction
                    .map_or_else(
                        || "—".into(),
                        |f| {
                            format!(
                                "{}{}{}%",
                                if weight_provisional { "≈ " } else { "" },
                                if f > 0. && f < 0.0005 { "< " } else { "" },
                                crate::number_format::tenths(if f > 0. && f < 0.0005 {
                                    1
                                } else {
                                    (f * 1000.).round() as u128
                                })
                            )
                        },
                    )
                    .into(),
                fraction: fraction.unwrap_or(0.),
                has_weight: fraction.is_some(),
                weight_provisional,
                color_index: (row.key.entry.0 % 8) as i32,
                files: measure
                    .map_or_else(
                        || "—".into(),
                        |m| {
                            if preview.is_some() {
                                format!("≥ {}", m.files)
                            } else {
                                m.files.to_string()
                            }
                        },
                    )
                    .into(),
                directories: if preview.is_some() {
                    "—".into()
                } else {
                    measure
                        .map_or_else(|| "—".into(), |m| m.directories.to_string())
                        .into()
                },
                modified: crate::date_label::format_date(modified).into(),
                accessed: crate::date_label::format_date(props.accessed).into(),
                allocated: crate::allocation_label::format_value(allocation, units).into(),
                owner: crate::owner_view::fallback(props.owner.id),
                note: if frame.provisional || frame.coverage != ScopeCoverage::Complete {
                    tr!("Partial").into()
                } else {
                    "".into()
                },
            });
            if let Some(f) = fraction.filter(|v| *v > 0.) {
                tile_weights.push(crate::treemap::Weight {
                    target: crate::treemap::Target::Node(index),
                    bytes: logical.unwrap(),
                });
                tiles.push(MapTile {
                    band_x: x,
                    band_w: f,
                    node_id: index as i32,
                    name: crate::tree::native_label(
                        row.path.file_name().map(Path::new).unwrap_or(&row.path),
                    )
                    .into(),
                    path: crate::tree::native_label(&row.path).into(),
                    partial: weight_provisional || measure.is_some_and(|m| m.unknown > 0),
                    size: size.into(),
                    directory: folder,
                    color_index: (row.key.entry.0 % 8) as i32,
                    x,
                    y: 0.,
                    w: f,
                    h: 1.,
                });
                x += f;
            }
        }
        drop(properties_trace);
        let tree_trace = sortsize_store::scan_trace::phase("prepare_tree_rows");
        let (folders, folder_keys, folder_memory) = self.folder_tree.borrow_mut().rows(
            store,
            self.c,
            self.view(3),
            &self.root,
            self._temporary.path(),
            self.parent,
            preview.is_some() || frame.provisional || frame.coverage != ScopeCoverage::Complete,
            preview.map_or(whole.is_some(), |preview| preview.complete()),
            units,
            measure,
        )?;
        drop(tree_trace);
        // Reuse the V0.4 pure mosaic layout on the worker. The unshown part of
        // the parent's known total keeps its area empty on a partial page.
        if let Some(total) = tile_total {
            let sum = tile_weights
                .iter()
                .map(|w| u128::from(w.bytes))
                .sum::<u128>();
            if sum <= u128::from(total) {
                if sum < u128::from(total) {
                    tile_weights.push(crate::treemap::Weight {
                        target: crate::treemap::Target::Other,
                        bytes: total - sum as u64,
                    });
                }
                let geometry = crate::treemap::layout(&tile_weights, 1000., 350.);
                for (tile, rect) in tiles.iter_mut().zip(geometry) {
                    tile.x = rect.x as f32;
                    tile.y = rect.y as f32;
                    tile.w = rect.width as f32;
                    tile.h = rect.height as f32;
                }
            } else {
                tiles.clear();
            }
        }
        let top_rows = top
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| TopRow {
                rank: (i + 1) as i32,
                name: short_label(
                    r.path
                        .file_name()
                        .map(std::path::Path::new)
                        .unwrap_or(&r.path),
                )
                .into(),
                size: r
                    .logical
                    .map_or_else(|| "—".into(), |n| units.format(n))
                    .into(),
                path: short_label(&r.path).into(),
            })
            .collect();
        *self.next.borrow_mut() = frame.next.clone();
        let token = publication_token()?;
        let status = self.session.status();
        Ok(Prepared {
            units,
            root_measure: whole,
            root_provisional: preview.is_some() || frame.provisional,
            current_folder_summary: crate::current_folder_summary::render(
                parent.and_then(|m| m.logical),
                parent.map(|m| m.files),
                if preview.is_some() {
                    None
                } else {
                    parent.map(|m| m.directories)
                },
                preview.is_some()
                    || frame.provisional
                    || frame.coverage != ScopeCoverage::Complete
                    || parent.is_none_or(|m| m.unknown > 0),
                units,
            ),
            #[cfg(feature = "benchmarks")]
            worker_frame_ready_at: Instant::now(),
            epoch: self.epoch,
            token,
            path: label(&store.navigation_path(
                self.c.generation,
                self.parent.entry,
                &self.root,
            )?),
            summary: whole.map_or_else(
                || tr!("Preparing inventory…").to_owned(),
                |m| {
                    m.logical.map_or_else(
                        || "—".to_owned(),
                        |n| {
                            if preview.is_some()
                                || frame.provisional
                                || frame.coverage != ScopeCoverage::Complete
                            {
                                if n == 0 {
                                    "—".into()
                                } else {
                                    format!("≥ {}", units.format(n))
                                }
                            } else {
                                units.format(n)
                            }
                        },
                    )
                },
            ),
            status: if self.is_fast_preview() {
                if frame.coverage == ScopeCoverage::Complete {
                    tr!("Complete").to_owned()
                } else {
                    tr!("Partial").to_owned()
                }
            } else if matches!(self.session, Backend::Cached(_) | Backend::Certified(_)) {
                format!(
                    "{} · {} · G{}",
                    if matches!(self.session, Backend::Certified(_)) {
                        tr!("Ready: verified inventory")
                    } else {
                        tr!("Saved locally. Source not queried.")
                    },
                    self.sealed.map_or_else(
                        || tr!("Interrupted acquisition").into(),
                        |t| crate::date_label::format_date(sortsize_core::MetadataValue::Known {
                            value: t,
                            accuracy: sortsize_core::ValueAccuracy::Exact
                        })
                    ),
                    self.c.generation.0
                )
            } else if status.state == State::Sealed {
                if frame.coverage == ScopeCoverage::Complete {
                    tr!("Complete").to_owned()
                } else {
                    tr!("Partial").to_owned()
                }
            } else {
                tr!("Scan in progress…").to_owned()
            },
            scope_detail: Session::excluded_state_path(store, self.c.generation)?.map_or_else(
                String::new,
                |path| {
                    tr_format!(
                        "Scope excludes Bunny and Size's verified working folder: {}",
                        label(&path)
                    )
                },
            ),
            busy: writer_running(status),
            parent: self.parent.entry.0 != 0,
            previous: if let (Some(order), Some(p)) = (&self.ordered, &self.ordered_page) {
                p.first
                    .as_ref()
                    .map(|c| order.previous(c).map(|p| !p.rows.is_empty()))
                    .transpose()?
                    .unwrap_or(false)
            } else {
                self.cursor.is_some()
            },
            sort_index: self.sort_index,
            quanta: status.quanta,
            measure_ready: preview.is_none() && parent.is_some(),
            parent_entry: self.parent.entry,
            frame,
            top,
            details,
            owners,
            folders,
            folder_keys,
            _folder_memory: folder_memory,
            top_rows,
            top_scope: top_scope.map(|scope| scope.path),
            tiles,
            _memory: memory,
            p1_frame_ready_at: scan_p1::enabled().then(Instant::now),
            _direct_publication: None,
        })
    }
}
pub(super) fn set_reader_workspace(
    lane: &Mutex<Lane>,
    active: bool,
    workspace: Option<Arc<RepositoryWorkspace>>,
) {
    let previous = {
        let mut l = lane.lock().unwrap();
        l.reader_active = active;
        std::mem::replace(&mut l.reader_workspace, workspace)
    };
    drop(previous);
}
pub(super) fn drain_stopped_lane(lane: &Mutex<Lane>) {
    let pending = {
        let mut l = lane.lock().unwrap();
        (
            l.reader_target.take(),
            l.writer_target.take(),
            l.reader_workspace.take(),
            l.retire_private.take(),
        )
    };
    // Native connections and temporary roots are destroyed by a worker, never
    // by a final UI-side Arc<Lane> after both owners have returned.
    drop(pending);
}
pub(super) fn retire_cancelled(active: &mut Option<Active>, lane: &Arc<Mutex<Lane>>) {
    let through = lane.lock().unwrap().retired_through;
    if through > 0
        && active.as_ref().is_some_and(|a| a.epoch <= through)
        && let Some(a) = active.take()
    {
        a.session.control().cancel();
        drop(a);
    }
}

/// Cancellation can arrive while certification owns no scheduler jobs. Its
/// publication token is independent of Control and must fence the final handoff.
fn cancel_revoked_publication(
    active: &mut Active,
    lane: &Mutex<Lane>,
    batch: &mut Option<super::smb_server::Batch>,
) -> bool {
    let mut l = lane.lock().unwrap();
    if !active.publication_cancel.is_cancelled() && !source_disconnected(&l, &active.root) {
        return false;
    }
    if source_disconnected(&l, &active.root) {
        active.request_cancel();
    } else {
        // Ordinary Cancel may interrupt certification after acquisition has
        // already sealed. Keep its candidate retryable without restarting IO.
        active.cancelled = true;
        active.session.control().cancel();
    }
    active.preparation_blocked = true;
    if let Some(batch) = batch {
        batch.disable_sources(std::slice::from_ref(&active.root));
        batch.finish(None);
    }
    l.writer_running = false;
    l.phase = Some((active.source_epoch, Phase::Cancelled, active.c.generation));
    true
}

// A store refusal is quiescent only once every admitted job/result releases
// its permit. In particular StoreError itself does not prove a native wait.
fn writer_running(status: sortsize_store::scheduler::Status) -> bool {
    status.in_flight != 0
        || !matches!(
            status.state,
            State::Sealed | State::StoreError | State::CancelledStoreError | State::Paused
        )
}

/// A failed candidate is not a usable published inventory. Once its actual
/// scheduler has drained, cancellation may acknowledge that failure without
/// retrying the failed fold/publication or replacing the retained G0.
fn settle_cancelled_failure(active: &mut Active, lane: &Mutex<Lane>) -> bool {
    let status = active.session.status();
    if !active.cancelled
        || status.in_flight != 0
        || !matches!(
            status.state,
            State::Cancelled | State::CancelledStoreError | State::Idle | State::Sealed
        )
    {
        return false;
    }
    active.preparation_blocked = true;
    if matches!(
        status.state,
        State::Cancelled | State::CancelledStoreError | State::Idle
    ) && active.fast_mode == FastMode::Standard
        && let Some(workspace) = &active.workspace
    {
        *workspace.cancelled_candidate.lock().unwrap() =
            Some((active.c, super::cache::seconds(active.started)));
    }
    let mut l = lane.lock().unwrap();
    if l.source_epoch == active.source_epoch {
        l.writer_running = false;
        if matches!(
            status.state,
            State::Cancelled | State::CancelledStoreError | State::Idle
        ) {
            l.cancelled_unpublished = Some(active.source_epoch);
        }
        l.phase = Some((active.source_epoch, Phase::Cancelled, active.c.generation));
    }
    true
}

pub(super) fn source_disconnected(lane: &Lane, path: &Path) -> bool {
    lane.disconnected_sources.iter().any(|scope| {
        crate::source_registry::covers(scope, path)
            || lane.source_bindings.iter().any(|(logical, root)| {
                crate::source_registry::covers(scope, logical)
                    && crate::source_registry::covers(root, path)
            })
            || lane.smb_snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.shares.iter().any(|share| {
                    crate::source_registry::covers(scope, &share.path)
                        && share
                            .native_root
                            .as_ref()
                            .is_some_and(|root| crate::source_registry::covers(root, path))
                })
            })
    })
}

fn smb_snapshot(lane: &Mutex<Lane>, batch: &Option<super::smb_server::Batch>) {
    if let Some(batch) = batch {
        let mut l = lane.lock().unwrap();
        if l.source_epoch == batch.snapshot.epoch
            && l.smb_snapshot.as_ref() != Some(&batch.snapshot)
        {
            l.smb_snapshot = Some(batch.snapshot.clone());
        }
    }
}

fn smb_reader(
    kept: &super::smb_server::Kept,
    source_epoch: u64,
    view_epoch: u64,
) -> sortsize_store::inventory::Result<Active> {
    let certified = kept
        .workspace
        .repository()
        .lock()
        .unwrap()
        .open_generation(kept.generation, &Default::default())?;
    let mut reader = Active::certified(view_epoch, certified, kept.workspace.clone())?;
    reader.source_epoch = source_epoch;
    Ok(reader)
}

pub(super) fn smb_view_matches(lane: &Lane, root: &Path) -> bool {
    lane.smb_view
        .as_deref()
        .map_or(lane.smb_snapshot.is_none(), |view| view == root)
}

fn smb_retained(
    lane: &Mutex<Lane>,
    kept: &std::collections::BTreeMap<PathBuf, super::smb_server::Kept>,
) {
    let mut l = lane.lock().unwrap();
    if !l
        .smb_retained
        .iter()
        .eq(kept.values().map(|kept| &kept.share))
    {
        l.smb_retained = kept.values().map(|kept| kept.share.clone()).collect();
    }
}

fn smb_unknown_share(
    path: PathBuf,
    coverage: ScopeCoverage,
    measured: Option<std::time::SystemTime>,
    logical_key: Option<PathBuf>,
) -> super::smb_server::Share {
    super::smb_server::Share {
        native_root: Some(path.clone()),
        path: logical_key.unwrap_or(path),
        logical: None,
        files: None,
        directories: None,
        coverage,
        phase: super::smb_server::SharePhase::Complete,
        error: None,
        measured,
        current_observation: false,
    }
}

/// Explicit local diagnostic for a controlled reproduction. Disabled unless
/// the caller chooses a new file; no source path, content or credential is
/// collected, and no diagnostic is sent to a service.
struct LocalDiagnostics {
    file: Option<std::fs::File>,
    last: Instant,
    sampling: bool,
}
impl LocalDiagnostics {
    fn new() -> Self {
        let file = std::env::var_os("BUNNY_CANCEL_DIAGNOSTICS").and_then(|path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .ok()
        });
        Self {
            file,
            last: Instant::now(),
            sampling: false,
        }
    }
    fn begin(&mut self) {
        self.sampling = self.file.is_some() && self.last.elapsed() >= Duration::from_millis(250);
        if self.sampling {
            self.last = Instant::now();
        }
    }
    fn prepared(&mut self, active: &Active, resumed: bool, budget: DiskBudget) {
        let Some(file) = &mut self.file else {
            return;
        };
        use sortsize_core::resources::{Governor, disk_limit, native_free_disk};
        use std::io::Write;
        let store = active.session.store().directory();
        let scratch = active._temporary.path();
        let store_free = native_free_disk(store);
        let scratch_free = native_free_disk(scratch);
        let limit = store_free
            .zip(scratch_free)
            .map(|(a, b)| disk_limit(a.min(b)));
        let usage = Governor::global().usage();
        let _ = writeln!(
            file,
            "step=writer_prepared resumed={resumed} store={store:?} scratch={scratch:?} store_free={store_free:?} scratch_free={scratch_free:?} requested={} used={} reserved={} limit={limit:?} minimum_limit={}",
            budget.now, usage.disk_used, usage.disk_reserved, budget.minimum_limit,
        );
    }
    fn event(&mut self, lane: &Mutex<Lane>, active: &Active, step: &str) {
        if !self.sampling {
            return;
        }
        let Some(file) = &mut self.file else {
            return;
        };
        let status = active.session.status();
        let (source_epoch, view_epoch, cancel_epoch, phase, running, owns, reader) = {
            let l = lane.lock().unwrap();
            (
                l.source_epoch,
                l.view_epoch,
                l.cancel_requested_epoch,
                l.phase,
                l.writer_running,
                l.writer_owns_session,
                l.reader_active,
            )
        };
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        use std::io::Write;
        let _ = writeln!(
            file,
            "unix_ms={unix_ms} step={step} epoch={} generation={} cancelled={} source_epoch={source_epoch} view_epoch={view_epoch} cancel_epoch={cancel_epoch:?} phase={phase:?} writer_running={running} owns_session={owns} reader_active={reader} status={status:?}",
            active.source_epoch, active.c.generation.0, active.cancelled
        );
    }
}

fn worker(
    lane: Arc<Mutex<Lane>>,
    output: Arc<Mutex<Option<Output>>>,
    search_output: Arc<Mutex<Option<super::search::Prepared>>>,
    resources: Arc<Mutex<Option<(crate::presentation::Context, String)>>>,
    cache_output: Arc<Mutex<Option<super::cache::Prepared>>>,
    preview_output: Arc<Mutex<Option<super::cache::PreviewOutput>>>,
) {
    let mut meter = super::search::Meter::default();
    let mut diagnostics = LocalDiagnostics::new();
    let mut projection_cadence = scan_p1::ProjectionCadence::default();
    let regulate_projections = match std::env::var_os("BUNNY_SCAN_P1_PROJECTIONS").as_deref() {
        Some(value) if value == "0" => Some(false),
        Some(value) if value == "1" => Some(true),
        _ => None,
    };
    let drain_without_wait = cfg!(any(target_os = "linux", windows))
        && std::env::var_os("BUNNY_SCAN_P1_DRAIN").is_none_or(|value| value != "0");
    let mut scan_metrics = None::<scan_p1::WriterMetrics>;
    let mut active: Option<Active> = None;
    let mut direct: Option<direct::DirectActive> = None;
    let mut interrupted_direct: Option<direct::DirectActive> = None;
    let mut previous_direct: Option<(u64, direct::DirectActive)> = None;
    let mut parked_repository: Option<(RepositoryState, Arc<RepositoryWorkspace>)> = None;
    let mut parked_epoch = 0;
    // At most one interrupted global repository survives a targeted search.
    // Its owner stays on this worker, with no open writer or scheduler lease.
    let mut interrupted_workspace: Option<Arc<RepositoryWorkspace>> = None;
    let mut smb_batch: Option<super::smb_server::Batch> = None;
    // The bounded parked repository map remains charged after leaving the view.
    let mut _smb_memory = None;
    // Repository handles own files and leases, not one SQLite connection per share.
    let mut smb_kept = std::collections::BTreeMap::<PathBuf, super::smb_server::Kept>::new();
    #[cfg(test)]
    let mut last_acquisition_tick = Instant::now();
    loop {
        // Batch failures can be rendered before ordinary acquisition starts.
        // Enter the UI context for the whole turn, including Batch::next.
        let (begin, batch_cancel, open, open_path, batch_epoch, view_epoch, batch_presentation) = {
            let mut l = lane.lock().unwrap();
            (
                l.smb_begin.take(),
                std::mem::take(&mut l.smb_cancel),
                l.smb_open.take(),
                l.smb_open_path.take(),
                l.source_epoch,
                l.view_epoch,
                l.presentation,
            )
        };
        let _iteration_scope = batch_presentation.enter();
        if let Some(mut request) = begin {
            request.disable_sources(&lane.lock().unwrap().disconnected_sources);
            let memory = request.memory();
            if let Some(old) = active.take() {
                if let Some(workspace) = &old.workspace
                    && request
                        .snapshot
                        .shares
                        .iter()
                        .any(|share| share.native_root.as_deref() == Some(old.root.as_path()))
                    && let Ok(Some(descriptor)) =
                        workspace.repository().lock().unwrap().latest_context()
                {
                    smb_kept
                        .entry(old.root.clone())
                        .or_insert_with(|| super::smb_server::Kept {
                            workspace: workspace.clone(),
                            generation: descriptor.generation,
                            share: smb_unknown_share(
                                old.root.clone(),
                                descriptor.coverage,
                                old.sealed,
                                request
                                    .snapshot
                                    .shares
                                    .iter()
                                    .find(|share| {
                                        share.native_root.as_deref() == Some(old.root.as_path())
                                    })
                                    .map(|share| share.path.clone()),
                            ),
                            _memory: memory.clone(),
                        });
                }
                old.session.control().cancel();
            }
            // Retained G0s outside a narrower confirmed scope remain owned.
            // Admission checked the union before replacing the operation.
            let mut next = super::smb_server::Batch::new(request);
            if let Some(previous) = &smb_batch {
                for share in &mut next.snapshot.shares {
                    if let Some(old) = previous
                        .snapshot
                        .shares
                        .iter()
                        .find(|old| old.path == share.path)
                    {
                        share.logical = old.logical;
                        share.files = old.files;
                        share.directories = old.directories;
                        share.coverage = old.coverage;
                    }
                }
            }
            for share in &mut next.snapshot.shares {
                if let Some(kept) = smb_kept.values().find(|kept| kept.share.path == share.path) {
                    share.logical = kept.share.logical;
                    share.files = kept.share.files;
                    share.directories = kept.share.directories;
                    share.coverage = kept.share.coverage;
                    share.measured = kept.share.measured;
                }
            }
            _smb_memory = Some(next.memory());
            smb_batch = Some(next);
            drop(parked_repository.take());
        }
        if smb_batch
            .as_ref()
            .is_some_and(|batch| batch.snapshot.epoch != batch_epoch)
        {
            smb_batch = None;
            let mut l = lane.lock().unwrap();
            l.smb_snapshot = None;
            l.smb_view = None;
        }
        let revoked = std::mem::take(&mut lane.lock().unwrap().source_revocations);
        if let Some(batch) = &mut smb_batch {
            let scopes = revoked
                .iter()
                .filter(|(_, through)| batch.snapshot.epoch <= *through)
                .map(|(scope, _)| scope.clone())
                .collect::<Vec<_>>();
            batch.disable_sources(&scopes);
        }
        if let Some(a) = &mut active
            && (source_disconnected(&lane.lock().unwrap(), &a.root)
                || revoked.iter().any(|(scope, through)| {
                    a.source_epoch <= *through && crate::source_registry::covers(scope, &a.root)
                }))
        {
            a.request_cancel();
        }
        if batch_cancel && let Some(batch) = &mut smb_batch {
            batch.cancel();
            if let Some(a) = &mut active {
                a.cancelled = true;
                a.session.control().cancel();
            }
        }
        if let Some((epoch, index)) = open
            && let Some(batch) = &smb_batch
            && let Some(share) = batch.snapshot.shares.get(index)
        {
            if let Some(kept) = smb_kept
                .values()
                .find(|kept| kept.share.path == share.path)
                .filter(|kept| {
                    kept.workspace
                        .repository()
                        .lock()
                        .unwrap()
                        .published(kept.generation)
                        .ok()
                        .flatten()
                        .is_some()
                })
            {
                match smb_reader(kept, batch_epoch, epoch) {
                    Ok(reader) => {
                        let mut l = lane.lock().unwrap();
                        l.reader_epoch = None;
                        l.reader_target = Some(Some(Box::new(reader)));
                        l.reader_active = true;
                    }
                    Err(error) => {
                        *output.lock().unwrap() =
                            Some(Output::Error(epoch, error.to_string(), false))
                    }
                }
            } else if let Some(a) = active
                .as_mut()
                .filter(|a| share.native_root.as_deref() == Some(a.root.as_path()))
            {
                let mut l = lane.lock().unwrap();
                l.reader_target = Some(None);
                l.reader_active = false;
                l.reader_epoch = None;
                drop(l);
                let key = EntryKey {
                    inventory: a.c.inventory,
                    generation: a.c.generation,
                    entry: EntryId(0),
                };
                if let Err(error) = a.navigate(epoch, Navigate::Open(key)) {
                    *output.lock().unwrap() = Some(Output::Error(epoch, error.to_string(), false));
                }
            }
        }
        if let Some((epoch, path)) = open_path
            && let Some(kept) = smb_kept.values().find(|kept| kept.share.path == path)
        {
            match smb_reader(kept, batch_epoch, epoch) {
                Ok(reader) => {
                    let mut l = lane.lock().unwrap();
                    l.reader_epoch = None;
                    l.reader_target = Some(Some(Box::new(reader)));
                    l.reader_active = true;
                }
                Err(error) => {
                    *output.lock().unwrap() = Some(Output::Error(epoch, error.to_string(), false))
                }
            }
        }
        if active.is_none()
            && let Some(batch) = &mut smb_batch
            && batch.current.is_some_and(|index| {
                batch.snapshot.shares[index].phase == super::smb_server::SharePhase::Cancelled
            })
        {
            // A disconnect may remove the coalesced Start after Batch::next
            // selected it but before the worker consumes it. Settle that slot.
            batch.finish(None);
        }
        if active.is_none()
            && let Some(batch) = &mut smb_batch
            && let Some(start) = batch.next()
        {
            let mut l = lane.lock().unwrap();
            l.smb_acquisition_root = Some(start.root.clone());
            if l.publication_cancel.is_cancelled() {
                l.publication_cancel = Default::default();
            }
            l.start = Some((batch.snapshot.epoch, start));
            l.writer_running = true;
            l.view_epoch = view_epoch;
            l.cancel = false;
        }
        if active.is_none()
            && smb_batch
                .as_ref()
                .is_some_and(|batch| !batch.snapshot.running)
        {
            lane.lock().unwrap().writer_running = false;
        }
        smb_retained(&lane, &smb_kept);
        smb_snapshot(&lane, &smb_batch);
        let (
            start,
            navigation,
            cancel,
            retry,
            stop,
            presentation,
            units,
            handoff,
            source_epoch,
            publication_cancel,
            fast_local,
            start_purpose,
        ) = {
            let mut l = lane.lock().unwrap();
            let reader = l.direct_view.is_none() && (l.reader_active || l.reader_target.is_some());
            if l.start.is_some() && l.interrupted_reader {
                // Close the shared reader before opening the same candidate
                // for writing. The reader acknowledges after dropping it.
                l.reader_target = Some(None);
            }
            let start = if l.reader_constructing.is_none() && !l.interrupted_reader {
                l.start.take()
            } else {
                None
            };
            if let Some((_, start)) = &start {
                // Publish the preparation cancellation handle atomically with
                // taking the intention, before a UI disconnect can race it.
                l.preparing_source = Some((start.root.clone(), l.publication_cancel.clone()));
                l.acquisition_root = Some(start.root.clone());
            }
            let start_purpose = if start.is_some() {
                std::mem::take(&mut l.start_purpose)
            } else {
                StartPurpose::Regular
            };
            (
                start,
                if reader { None } else { l.navigation.take() },
                std::mem::take(&mut l.cancel),
                std::mem::take(&mut l.retry),
                l.stop,
                l.presentation,
                l.units,
                l.writer_target.take(),
                l.source_epoch,
                l.publication_cancel.clone(),
                l.fast_local && smb_batch.is_none(),
                start_purpose,
            )
        };
        if stop {
            break;
        }
        #[cfg(test)]
        if direct.is_some() && start.is_none() {
            let gate = {
                let mut l = lane.lock().unwrap();
                let gate = l.turn_gate.clone();
                if gate.is_some() {
                    l.turn_blocked = true;
                }
                gate
            };
            if let Some(gate) = gate {
                let (lock, ready) = &*gate;
                let mut release = lock.lock().unwrap();
                while !*release {
                    release = ready.wait(release).unwrap();
                }
            }
        }
        let _scope = presentation.enter();
        retire_cancelled(&mut active, &lane);
        meter.update(&lane, &resources, units);
        if direct.is_none()
            && start.is_none()
            && let Some((epoch, _)) = &previous_direct
        {
            let mut l = lane.lock().unwrap();
            if l.source_epoch == *epoch
                && l.phase
                    .is_some_and(|(_, phase, _)| matches!(phase, Phase::Failed | Phase::Cancelled))
            {
                let (epoch, mut view) = previous_direct.take().unwrap();
                let view_epoch = l.view_epoch;
                view.restore_previous(epoch, view_epoch, &mut l);
                direct = Some(view);
            } else if l.source_epoch != *epoch {
                drop(previous_direct.take());
            }
        }
        if let Some(view) = &direct {
            let mut l = lane.lock().unwrap();
            // A cancelled Verify/Open can remain queued across a newer Start.
            // Retiring this view belongs only to the current source intention.
            cache::discard_obsolete_request(&mut l);
            let opens_inventory = l.cache.as_ref().is_some_and(|request| {
                matches!(
                    request.action,
                    cache::Action::Open(_) | cache::Action::Resume(_) | cache::Action::Verify(_)
                )
            });
            if view.retired(&l) || start.is_some() || handoff.is_some() || opens_inventory {
                // A new intention can arrive after this turn extracted its
                // local start. Decide how to park the existing owner while
                // holding the same lane lock used by the retirement check.
                let local = start.as_ref().filter(|(epoch, _)| *epoch == l.source_epoch);
                let (queued, purpose) = if local.is_some() {
                    (local, &start_purpose)
                } else {
                    (
                        l.start
                            .as_ref()
                            .filter(|(epoch, _)| *epoch == l.source_epoch),
                        &l.start_purpose,
                    )
                };
                let priority =
                    matches!(purpose, StartPurpose::PriorityDuplicates) && queued.is_some();
                let same_root = queued
                    .filter(|(_, new)| new.root == view.root_path())
                    .map(|(epoch, _)| *epoch);
                view.detach(&mut l);
                drop(l);
                if priority {
                    view.pause();
                    interrupted_direct = direct.take();
                } else if let Some(epoch) = same_root {
                    view.pause();
                    previous_direct = direct.take().map(|view| (epoch, view));
                } else {
                    drop(direct.take());
                }
            }
        }
        if let Some(next) = handoff {
            drop(parked_repository.take());
            if let Some(old) = active.take() {
                old.session.control().cancel();
            }
            active = Some(*next);
            lane.lock().unwrap().acquisition_publication = active
                .as_ref()
                .filter(|a| matches!(a.session, Backend::Live(_)))
                .map(|a| (a.root.clone(), a.publication_cancel.clone()));
            lane.lock().unwrap().writer_owns_session =
                active.as_ref().is_some_and(Active::owns_acquisition);
        }
        if let Some((epoch, mut start)) = start {
            {
                let l = lane.lock().unwrap();
                if source_disconnected(&l, &start.root) || publication_cancel.is_cancelled() {
                    drop(l);
                    lane.lock().unwrap().preparing_source = None;
                    if let Some(batch) = &mut smb_batch {
                        batch.disable_sources(std::slice::from_ref(&start.root));
                        batch.finish(None);
                    }
                    auto_inventory::cancelled_start(epoch, &lane);
                    continue;
                }
            }
            if let Some(previous) = &mut active
                && let Err(error) = previous.close_live_preview()
            {
                *output.lock().unwrap() = Some(Output::Error(epoch, error.to_string(), false));
                continue;
            }
            if let StartPurpose::BrowseInterrupted(relative) = &start_purpose
                && interrupted_direct
                    .as_ref()
                    .is_some_and(|view| view.root_path() == start.root)
            {
                let mut view = interrupted_direct.take().unwrap();
                if let Some(old) = active.take() {
                    old.session.control().cancel();
                }
                let restored =
                    view.restore_interrupted(epoch, relative, publication_cancel.clone());
                let mut l = lane.lock().unwrap();
                l.preparing_source = None;
                match restored {
                    Ok(()) => {
                        drop(previous_direct.take());
                        view.attach(&mut l);
                        direct = Some(view);
                    }
                    Err(error) => {
                        *output.lock().unwrap() =
                            Some(Output::Error(epoch, error.to_string(), false));
                    }
                }
                drop(l);
                notify(&lane);
                continue;
            }
            if let StartPurpose::BrowseInterrupted(relative) = &start_purpose {
                let opened = (|| {
                    let workspace = interrupted_workspace
                        .as_ref()
                        .filter(|workspace| workspace.root == start.root)
                        .ok_or(Error::Contract(tr!("Source is no longer available.")))?;
                    let candidate = workspace.cancelled_candidate().map(|saved| {
                        Active::open_saved(
                            epoch,
                            &saved,
                            Some(workspace.clone()),
                            &publication_cancel,
                        )
                    });
                    let mut reader = match candidate {
                        Some(Ok(reader)) => reader,
                        candidate => {
                            let certified =
                                workspace
                                    .repository()
                                    .lock()
                                    .unwrap()
                                    .open_latest(&publication_cancel)?
                                    .ok_or_else(|| {
                                        candidate.and_then(Result::err).unwrap_or(Error::Contract(
                                            tr!("Source is no longer available."),
                                        ))
                                    })?;
                            Active::certified(epoch, certified, workspace.clone())?
                        }
                    };
                    let entry = reader
                        .session
                        .store()
                        .navigation_directory(reader.c, relative)?;
                    reader.parent.entry = entry;
                    Ok::<_, Error>(reader)
                })();
                let mut l = lane.lock().unwrap();
                l.preparing_source = None;
                if l.source_epoch != epoch || publication_cancel.is_cancelled() {
                    drop(l);
                    auto_inventory::cancelled_start(epoch, &lane);
                    continue;
                }
                match opened {
                    Ok(reader) => {
                        l.phase = None;
                        l.interrupted_reader = true;
                        let previous = l.reader_target.replace(Some(Box::new(reader)));
                        drop(l);
                        drop(previous);
                    }
                    Err(error) => {
                        drop(l);
                        *output.lock().unwrap() = Some(Output::Error(
                            epoch,
                            super::acquisition_error_message(error),
                            false,
                        ));
                    }
                }
                notify(&lane);
                continue;
            }
            let mut previous = active
                .as_mut()
                .and_then(Active::take_repository)
                .or_else(|| parked_repository.take())
                .or_else(|| {
                    lane.lock()
                        .unwrap()
                        .reader_workspace
                        .clone()
                        .map(|workspace| {
                            (
                                RepositoryState {
                                    owner: workspace.repository(),
                                    plan: None,
                                    published: None,
                                },
                                workspace,
                            )
                        })
                });
            if smb_batch.is_some()
                && let Some(kept) = smb_kept.get(&start.root)
            {
                previous = Some((
                    RepositoryState {
                        owner: kept.workspace.repository(),
                        plan: None,
                        published: None,
                    },
                    kept.workspace.clone(),
                ));
            }
            if matches!(start_purpose, StartPurpose::PriorityDuplicates) {
                interrupted_workspace = previous.take().map(|(_, workspace)| workspace);
            } else if matches!(start_purpose, StartPurpose::ManualScan)
                && !fast_local
                && let Some(workspace) = interrupted_workspace
                    .as_ref()
                    .filter(|workspace| workspace.root == start.root)
                && workspace
                    .cancelled_candidate()
                    .is_some_and(|saved| start.accepts_resume(saved.context.policy))
            {
                previous = Some((
                    RepositoryState {
                        owner: workspace.repository(),
                        plan: None,
                        published: None,
                    },
                    workspace.clone(),
                ));
            }
            // The bounded workspace survives an Active owner retired by an
            // SMB batch, as well as an ordinary cancelled local acquisition.
            let cancelled_candidate = previous
                .as_ref()
                .filter(|(_, workspace)| workspace.root == start.root)
                .and_then(|(_, workspace)| workspace.cancelled_candidate())
                .filter(|saved| start.accepts_resume(saved.context.policy));
            let recovering_cancelled = cancelled_candidate.is_some();
            if let Some(old) = active.take() {
                old.session.control().cancel();
                drop(old);
            }
            let retired = lane.lock().unwrap().retire_private.take();
            if let Some(workspace) = retired {
                workspace.retire_original();
            }
            {
                let mut l = lane.lock().unwrap();
                if l.source_epoch != epoch {
                    parked_repository = previous.take();
                    parked_epoch = l.source_epoch;
                    continue;
                }
                l.writer_epoch = Some(epoch);
                l.reader_epoch = None;
                let generation = l.phase.map_or(GenerationId(0), |(_, _, g)| g);
                l.phase = Some((epoch, Phase::Preparing, generation));
                l.scan_progress = None;
                l.fast_preview_context = None;
            }
            // Reopen existing local inventories on this worker. A certified G0
            // has an independent reader before preparation of its successor.
            // Volume discovery preflight is FnOnce and must be consumed once,
            // before catalogue recovery or a fresh source acquisition.
            let available = start.preflight.take().is_none_or(|check| check());
            if publication_cancel.is_cancelled() {
                parked_repository = previous.take();
                parked_epoch = epoch;
                lane.lock().unwrap().preparing_source = None;
                if let Some(batch) = &mut smb_batch {
                    batch.disable_sources(std::slice::from_ref(&start.root));
                    batch.finish(None);
                }
                auto_inventory::cancelled_start(epoch, &lane);
                continue;
            }
            let may_use_reader_preview = smb_batch.is_none();
            #[cfg(test)]
            let force_strict = lane.lock().unwrap().force_strict;
            #[cfg(not(test))]
            let force_strict = false;
            if force_strict {
                start.observation = ScanObservation::Strict;
            }
            // Native metadata policy and the destination of the observations
            // are independent. Linux keeps strict metadata in its direct view.
            // A legacy cancelled candidate retains its SQLite recovery path.
            let resume_requires_sqlite = match start_purpose {
                StartPurpose::ResumeInterrupted { allow_fresh, .. } => {
                    !allow_fresh
                        || interrupted_workspace
                            .as_ref()
                            .filter(|workspace| workspace.root == start.root)
                            .and_then(|workspace| workspace.cancelled_candidate())
                            .is_some_and(|saved| start.accepts_resume(saved.context.policy))
                }
                _ => false,
            };
            let occupancy_direct = (cfg!(windows)
                && start.observation == ScanObservation::DirectoryListing)
                || (cfg!(target_os = "linux") && !recovering_cancelled && !resume_requires_sqlite);
            let direct_choice = if available
                && occupancy_direct
                && !force_strict
                && !matches!(
                    start_purpose,
                    StartPurpose::DuplicateInventory | StartPurpose::PriorityDuplicates
                )
                && smb_batch.is_none()
            {
                Guard::open(
                    &start.root,
                    Policy {
                        explicit_root: start.explicit_root,
                        ..Default::default()
                    },
                )
                .map(|guard| {
                    guard.evidence().0.class == sortsize_core::acquisition::SourceClass::Local
                })
            } else {
                Ok(false)
            };
            let use_direct = matches!(&direct_choice, Ok(true));
            let resume_interrupted = |allow_fresh, options, explicit_root| {
                if let Some(workspace) = interrupted_workspace
                    .as_ref()
                    .filter(|workspace| workspace.root == start.root)
                    && let Some(saved) = workspace
                        .cancelled_candidate()
                        .filter(|saved| start.accepts_resume(saved.context.policy))
                {
                    let disk = acquisition_budget(false);
                    let resumed = Active::resume_saved_with_options(
                        epoch,
                        Policy {
                            explicit_root,
                            ..Default::default()
                        },
                        &saved,
                        Some(workspace.clone()),
                        &publication_cancel,
                        publication_cancel.clone(),
                        ResumeOptions {
                            expected_collection: Some(options),
                            disk,
                        },
                    );
                    if let Ok(active) = resumed {
                        return Ok(Some((active, disk)));
                    }
                }
                if allow_fresh || publication_cancel.is_cancelled() {
                    Ok(None)
                } else {
                    Err(Error::Contract(tr!(
                        "Automatic scan unavailable: select an accessible local source. Other paths require an explicit scan."
                    )))
                }
            };
            let resumed = if let Err(error) = direct_choice {
                Err(error.into())
            } else if available && use_direct {
                Ok(None)
            } else if available {
                if let StartPurpose::ResumeInterrupted {
                    allow_fresh,
                    options,
                    explicit_root,
                } = start_purpose
                {
                    resume_interrupted(allow_fresh, options, explicit_root).and_then(|resumed| {
                        if resumed.is_some() || publication_cancel.is_cancelled() {
                            return Ok(resumed);
                        }
                        auto_inventory::restore(
                            epoch,
                            &start,
                            &mut previous,
                            &lane,
                            &publication_cancel,
                            may_use_reader_preview,
                            false,
                        )
                    })
                } else if matches!(start_purpose, StartPurpose::PriorityDuplicates) {
                    Ok(None)
                } else if let Some(saved) = cancelled_candidate.filter(|_| !use_direct) {
                    auto_inventory::preserve_reader(
                        epoch,
                        &start.root,
                        Policy {
                            explicit_root: start.explicit_root,
                            ..Default::default()
                        },
                        previous.as_ref(),
                        &lane,
                    );
                    if let Some((repository, _)) = &mut previous {
                        drop(repository.plan.take());
                    }
                    let disk = acquisition_budget(
                        may_use_reader_preview && lane.lock().unwrap().reader_active,
                    );
                    Active::resume_saved_with_options(
                        epoch,
                        Policy {
                            explicit_root: start.explicit_root,
                            ..Default::default()
                        },
                        &saved,
                        previous.as_ref().map(|(_, workspace)| workspace.clone()),
                        &publication_cancel,
                        publication_cancel.clone(),
                        ResumeOptions {
                            expected_collection: Some(start.options),
                            disk,
                        },
                    )
                    .map(|active| Some((active, disk)))
                } else {
                    auto_inventory::restore(
                        epoch,
                        &start,
                        &mut previous,
                        &lane,
                        &publication_cancel,
                        may_use_reader_preview,
                        use_direct,
                    )
                }
            } else {
                auto_inventory::preserve_reader(
                    epoch,
                    &start.root,
                    Policy::default(),
                    previous.as_ref(),
                    &lane,
                );
                Err(sortsize_store::inventory::Error::Contract(tr!(
                    "Source is no longer available."
                )))
            };
            if publication_cancel.is_cancelled() || lane.lock().unwrap().source_epoch != epoch {
                parked_repository = previous.take();
                parked_epoch = epoch;
                lane.lock().unwrap().preparing_source = None;
                if let Some(batch) = &mut smb_batch {
                    batch.disable_sources(std::slice::from_ref(&start.root));
                    batch.finish(None);
                }
                auto_inventory::cancelled_start(epoch, &lane);
                continue;
            }
            let start_cancel = publication_cancel.clone();
            if smb_batch.is_some()
                && let Some((_, workspace)) = &previous
                && let Ok(Some(descriptor)) =
                    workspace.repository().lock().unwrap().latest_context()
            {
                smb_kept
                    .entry(start.root.clone())
                    .or_insert_with(|| super::smb_server::Kept {
                        workspace: workspace.clone(),
                        generation: descriptor.generation,
                        share: smb_unknown_share(
                            start.root.clone(),
                            descriptor.coverage,
                            descriptor
                                .measured
                                .and_then(|seconds| u64::try_from(seconds).ok())
                                .and_then(|seconds| {
                                    std::time::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
                                }),
                            smb_batch.as_ref().and_then(|batch| {
                                batch
                                    .snapshot
                                    .shares
                                    .iter()
                                    .find(|share| {
                                        share.native_root.as_deref() == Some(start.root.as_path())
                                    })
                                    .map(|share| share.path.clone())
                            }),
                        ),
                        _memory: smb_batch.as_ref().unwrap().memory(),
                    });
            }
            let is_resume = matches!(&resumed, Ok(Some(_)));
            if scan_p1::enabled() {
                use std::io::Write;
                // An unavailable diagnostic stream must not terminate acquisition.
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "bunny_scan_route requested_fast={fast_local} local_direct={use_direct} resumed={is_resume} direct={}",
                    use_direct && matches!(&resumed, Ok(None))
                );
            }
            // Retain ownership before preparation, including its error and
            // cancellation exits. A later normal request can still resume the
            // previous candidate after the direct view is cancelled or closed.
            if use_direct && matches!(&resumed, Ok(None)) {
                if let Some((repository, _)) = &mut previous {
                    drop(repository.plan.take());
                }
                parked_repository = previous.take();
                parked_epoch = epoch;
                // The direct test gate pauses admission, before the native
                // worker starts. Mid-acquisition cancellation has separate
                // service/core tests; this gate proves UI intention handling.
                #[cfg(test)]
                if let Some(gate) = { lane.lock().unwrap().tick_gate.clone() } {
                    lane.lock().unwrap().tick_blocked = true;
                    let (lock, ready) = &*gate;
                    let mut release = lock.lock().unwrap();
                    while !*release {
                        release = ready.wait(release).unwrap();
                    }
                }
                if publication_cancel.is_cancelled() || lane.lock().unwrap().source_epoch != epoch {
                    lane.lock().unwrap().preparing_source = None;
                    auto_inventory::cancelled_start(epoch, &lane);
                    continue;
                }
                #[cfg(test)]
                let pacing = {
                    let l = lane.lock().unwrap();
                    if l.tick_delay.is_zero() && l.tick_gate.is_none() {
                        None
                    } else {
                        let owner = Arc::clone(&lane);
                        Some(Arc::new(move || {
                            let mut lane = owner.lock().unwrap();
                            if lane.tick_gate.is_some() {
                                lane.tick_blocked = true;
                            }
                            (lane.tick_delay, lane.tick_gate.clone())
                        })
                            as sortsize_store::direct::TestBatchPacing)
                    }
                };
                let opened = direct::DirectActive::open(
                    epoch,
                    &start,
                    publication_cancel.clone(),
                    #[cfg(test)]
                    pacing,
                )
                .and_then(|view| {
                    // Eligibility was checked above. A changed source must
                    // not silently fall back with its previous owner parked.
                    view.ok_or(Error::Contract(tr!("Source is no longer available.")))
                });
                let mut l = lane.lock().unwrap();
                if l.source_epoch != epoch
                    || publication_cancel.is_cancelled()
                    || source_disconnected(&l, &start.root)
                {
                    l.preparing_source = None;
                    drop(l);
                    drop(opened);
                    auto_inventory::cancelled_start(epoch, &lane);
                    continue;
                }
                match opened {
                    Ok(mut view) => {
                        l.preparing_source = None;
                        drop(previous_direct.take());
                        view.attach(&mut l);
                        direct = Some(view);
                        drop(l);
                        notify(&lane);
                        continue;
                    }
                    Err(error) => {
                        l.preparing_source = None;
                        l.phase = Some((epoch, Phase::Failed, GenerationId(0)));
                        l.writer_running = false;
                        l.writer_epoch = None;
                        l.writer_owns_session = false;
                        let view_epoch = l.view_epoch;
                        drop(l);
                        *output.lock().unwrap() = Some(Output::Error(
                            view_epoch,
                            acquisition_error_message(error),
                            false,
                        ));
                        notify(&lane);
                        continue;
                    }
                }
            }
            let opened = match resumed {
                Ok(Some(resumed)) => Ok(resumed),
                Ok(None) => {
                    let budget = acquisition_budget(
                        may_use_reader_preview && lane.lock().unwrap().reader_active,
                    );
                    Active::open_with_placement_log(
                        epoch,
                        start,
                        previous,
                        publication_cancel,
                        budget,
                        diagnostics.file.as_mut(),
                        fast_local,
                    )
                    .map(|active| (active, budget))
                }
                Err(error) => {
                    if recovering_cancelled {
                        // A rejected option change or failed recovery must not
                        // drop the only owner of the preserved raw candidate.
                        parked_repository = previous.take();
                        parked_epoch = epoch;
                    }
                    Err(super::acquisition_error_message(error))
                }
            };
            lane.lock().unwrap().preparing_source = None;
            if start_cancel.is_cancelled() {
                drop(opened);
                if let Some(batch) = &mut smb_batch {
                    // The current intention was irrevocably cancelled, even if
                    // the user already reconnected the source in the meantime.
                    if let Some(index) = batch.current {
                        batch.snapshot.shares[index].phase =
                            super::smb_server::SharePhase::Cancelled;
                    }
                    batch.finish(None);
                }
                auto_inventory::cancelled_start(epoch, &lane);
                continue;
            }
            match opened {
                Ok((mut a, budget)) => {
                    diagnostics.prepared(&a, is_resume, budget);
                    let mut l = lane.lock().unwrap();
                    if l.source_epoch != epoch
                        || a.publication_cancel.is_cancelled()
                        || source_disconnected(&l, &a.root)
                    {
                        drop(l);
                        a.session.control().cancel();
                        drop(a);
                        if let Some(batch) = &mut smb_batch {
                            if let Some(index) = batch.current {
                                batch.snapshot.shares[index].phase =
                                    super::smb_server::SharePhase::Cancelled;
                            }
                            batch.finish(None);
                        }
                        auto_inventory::cancelled_start(epoch, &lane);
                        continue;
                    }
                    if smb_view_matches(&l, &a.root) {
                        a.epoch = l.view_epoch;
                        l.reader_epoch =
                            Some((l.view_epoch, a.root.clone(), a.c.inventory, a.c.source));
                    }
                    l.phase = Some((a.source_epoch, Phase::Acquiring, a.c.generation));
                    if let Backend::Live(session) = &a.session {
                        l.scan_progress = Some(super::scan_p3::Progress {
                            source_epoch: a.source_epoch,
                            processed_entries: session.progress_counter(),
                        });
                    }
                    l.acquisition_control =
                        Some((a.source_epoch, a.c.generation, a.session.control()));
                    l.acquisition_publication =
                        Some((a.root.clone(), a.publication_cancel.clone()));
                    if let (Some(parked), Some(workspace)) = (&interrupted_workspace, &a.workspace)
                        && Arc::ptr_eq(parked, workspace)
                    {
                        interrupted_workspace = None;
                    }
                    active = Some(a);
                    l.writer_owns_session = true;
                }
                Err(error) => {
                    let mut l = lane.lock().unwrap();
                    if l.source_epoch == epoch {
                        let generation = l.phase.map_or(GenerationId(0), |(_, _, g)| g);
                        l.phase = Some((epoch, Phase::Failed, generation));
                        l.writer_running = false;
                        l.writer_epoch = None;
                        l.writer_owns_session = false;
                    }
                    drop(l);
                    if let Some(batch) = &mut smb_batch {
                        batch.finish(Some(error));
                        smb_retained(&lane, &smb_kept);
                        smb_snapshot(&lane, &smb_batch);
                        notify(&lane);
                    } else {
                        *output.lock().unwrap() = Some(Output::Error(epoch, error, false));
                    }
                }
            }
        } else {
            if active
                .as_ref()
                .is_some_and(|a| a.source_epoch != source_epoch)
                && let Some(old) = active.take()
            {
                old.session.control().cancel();
            }
            if parked_epoch != source_epoch {
                drop(parked_repository.take());
            }
        }
        let snapshot_request = {
            let mut l = lane.lock().unwrap();
            if l.cache.as_ref().is_some_and(|r| {
                matches!(&r.action,
                cache::Action::Open(saved) if cache::is_snapshot(saved))
            }) {
                cache::take_request(&mut l)
            } else {
                None
            }
        };
        if let Some(request) = snapshot_request {
            {
                let mut l = lane.lock().unwrap();
                if request.cancel.is_cancelled() || l.source_epoch != request.epoch {
                    continue;
                }
                l.operation_cancel = Some(request.cancel.clone());
            }
            let cache::Action::Open(saved) = &request.action else {
                unreachable!()
            };
            lane.lock().unwrap().phase =
                Some((request.epoch, Phase::Reopening, saved.context.generation));
            notify(&lane);
            let loaded = direct::DirectActive::open_snapshot(request.epoch, saved, &request.cancel);
            match loaded {
                Ok(mut view) => {
                    let mut l = lane.lock().unwrap();
                    if !request.cancel.is_cancelled() && l.source_epoch == request.epoch {
                        if let Some(old) = active.take() {
                            old.session.control().cancel();
                        }
                        view.attach(&mut l);
                        direct = Some(view);
                    }
                    drop(l);
                    let result = work_location().and_then(|location| {
                        let root = lane
                            .lock()
                            .unwrap()
                            .cache_root
                            .clone()
                            .unwrap_or_else(|| location.catalogue());
                        cache::prepared(&request, &root, None, None, None, units)
                    });
                    *cache_output.lock().unwrap() =
                        Some(result.unwrap_or_else(|e| cache::error(&request, e.to_string())));
                }
                Err(error) => {
                    let mut l = lane.lock().unwrap();
                    if l.source_epoch == request.epoch {
                        l.phase = Some((
                            request.epoch,
                            if request.cancel.is_cancelled() {
                                Phase::Cancelled
                            } else {
                                Phase::Failed
                            },
                            saved.context.generation,
                        ));
                    }
                    drop(l);
                    *output.lock().unwrap() =
                        Some(Output::Error(request.epoch, error.to_string(), false));
                    *cache_output.lock().unwrap() = Some(cache::error(&request, error.to_string()));
                }
            }
            notify(&lane);
        }
        if let Some(view) = &mut direct {
            #[cfg(test)]
            if let Some(gate) = { lane.lock().unwrap().reader_gate.clone() } {
                lane.lock().unwrap().direct_reader_blocked = true;
                let (lock, ready) = &*gate;
                let mut release = lock.lock().unwrap();
                while !*release {
                    release = ready.wait(release).unwrap();
                }
            }
            view.spawn_explicit_duplicates(&lane, &search_output, &resources);
            match view.turn(
                &lane,
                &output,
                &cache_output,
                &search_output,
                navigation,
                units,
            ) {
                Ok(()) => {
                    let request = {
                        let mut l = lane.lock().unwrap();
                        if l.cache.as_ref().is_some_and(|request| {
                            !matches!(
                                request.action,
                                cache::Action::Open(_)
                                    | cache::Action::Resume(_)
                                    | cache::Action::Verify(_)
                            ) && (!request.action.is_save() || view.is_saved())
                        }) {
                            cache::take_request(&mut l)
                        } else {
                            None
                        }
                    };
                    if let Some(mut request) = request {
                        if matches!(request.action, cache::Action::SaveFast { .. }) {
                            request.action = cache::Action::Save;
                        }
                        if matches!(&request.action, cache::Action::Forget(saved) if view.owns_snapshot(saved))
                        {
                            *cache_output.lock().unwrap() = Some(cache::error(
                                &request,
                                tr!("Select another inventory before forgetting this cache.")
                                    .to_owned(),
                            ));
                            notify(&lane);
                        } else if matches!(
                            request.action,
                            cache::Action::Save | cache::Action::Compare(_)
                        ) {
                            let reference = view.saved_record().and_then(|saved| {
                                Active::open_saved(request.epoch, &saved, None, &request.cancel)
                            });
                            match reference {
                                Ok(reference) => cache::run(
                                    request,
                                    &mut Some(reference),
                                    &lane,
                                    &cache_output,
                                    &output,
                                    &preview_output,
                                ),
                                Err(error) => {
                                    *cache_output.lock().unwrap() =
                                        Some(cache::error(&request, error.to_string()))
                                }
                            }
                            view.refresh_after_compatibility();
                        } else {
                            cache::run(
                                request,
                                &mut active,
                                &lane,
                                &cache_output,
                                &output,
                                &preview_output,
                            );
                        }
                    }
                }
                Err(error) => {
                    let mut l = lane.lock().unwrap();
                    view.report_view_error(&mut l);
                    publish_retry_error(&output, l.view_epoch, acquisition_error_message(error));
                    drop(l);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        lane.lock().unwrap().writer_owns_session =
            active.as_ref().is_some_and(Active::owns_acquisition);
        if let Some(a) = &active
            && let Backend::Live(session) = &a.session
        {
            let mut l = lane.lock().unwrap();
            l.acquisition_control = Some((a.source_epoch, a.c.generation, session.control()));
            l.acquisition_publication = Some((a.root.clone(), a.publication_cancel.clone()));
            if l.source_epoch == a.source_epoch
                && l.scan_progress
                    .as_ref()
                    .is_none_or(|p| p.source_epoch != a.source_epoch)
            {
                l.scan_progress = Some(super::scan_p3::Progress {
                    source_epoch: a.source_epoch,
                    processed_entries: session.progress_counter(),
                });
                l.phase = Some((a.source_epoch, Phase::Acquiring, a.c.generation));
            }
        }
        let cache = {
            let mut l = lane.lock().unwrap();
            let save_writer = active
                .as_ref()
                .is_some_and(|a| matches!(a.session, Backend::Live(_)))
                && l.cache
                    .as_ref()
                    .is_some_and(|request| request.action.is_save());
            if ((!l.reader_active && l.reader_target.is_none()) || save_writer)
                && l.reader_constructing.is_none()
            {
                let request = super::cache::take_request(&mut l);
                if save_writer
                    && let Some(a) = &mut active
                    && a.fast_mode == FastMode::PreviewWriter
                    && l.source_epoch == a.source_epoch
                    && l.fast_preview_context == Some(a.c)
                    && l.phase.is_some_and(|(epoch, phase, _)| {
                        epoch == a.source_epoch && matches!(phase, Phase::PreviewReady)
                    })
                    && let Some(request) = &request
                    && matches!(request.action,
                        super::cache::Action::SaveFast { source_epoch, context }
                            if source_epoch == a.source_epoch && context == a.c
                    )
                    && !request.cancel.is_cancelled()
                {
                    a.fast_mode = FastMode::Saving;
                    l.phase = Some((a.source_epoch, Phase::Certifying, a.c.generation));
                    l.fast_save_cancel = Some((a.source_epoch, request.cancel.clone()));
                    l.writer_epoch = Some(a.source_epoch);
                    l.writer_owns_session = true;
                    l.writer_running = true;
                }
                request
            } else {
                None
            }
        };
        let mut completed_fast_save = false;
        if let Some(request) = cache {
            let fast_save = lane.lock().unwrap().fast_save_cancel.is_some();
            let save_started = (fast_save && scan_p1::enabled()).then(Instant::now);
            if fast_save {
                notify(&lane);
            }
            if matches!(
                request.action,
                super::cache::Action::Open(_)
                    | super::cache::Action::Verify(_)
                    | super::cache::Action::Resume(_)
            ) {
                lane.lock().unwrap().writer_epoch = Some(request.epoch);
            }
            super::cache::run(
                request,
                &mut active,
                &lane,
                &cache_output,
                &output,
                &preview_output,
            );
            if fast_save {
                let mut l = lane.lock().unwrap();
                l.fast_save_cancel = None;
                let saved = active
                    .as_ref()
                    .is_some_and(|a| matches!(a.session, Backend::Certified(_)));
                completed_fast_save = saved;
                if let Some(a) = &mut active {
                    if !saved {
                        a.fast_mode = FastMode::PreviewWriter;
                    }
                    if l.source_epoch == a.source_epoch {
                        l.writer_epoch = None;
                        l.writer_owns_session = false;
                        l.writer_running = false;
                        if l.cancel_requested_epoch == Some(a.source_epoch) {
                            l.cancel_requested_epoch = None;
                        }
                        l.phase = Some((
                            a.source_epoch,
                            if saved {
                                Phase::Ready
                            } else {
                                Phase::PreviewReady
                            },
                            a.c.generation,
                        ));
                        if saved {
                            l.fast_preview_context = None;
                        }
                    }
                }
                drop(l);
                if let Some(started) = save_started {
                    use std::io::Write;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "scan_p3_save saved={saved} wall_ns={}",
                        started.elapsed().as_nanos()
                    );
                    sortsize_store::report_scan_metrics(if saved {
                        "fast_saved"
                    } else {
                        "fast_save_failed"
                    });
                }
                notify(&lane);
            }
        }
        retire_cancelled(&mut active, &lane);
        if active
            .as_ref()
            .is_some_and(|a| !matches!(a.session, Backend::Live(_)))
        {
            if active.as_ref().is_some_and(|a| {
                matches!(a.session, Backend::Certified(_))
                    || a.sealed.is_none()
                    || !work_location()
                        .and_then(|location| location.permits_writer(a.session.store().directory()))
                        .unwrap_or(false)
            }) {
                let mut view = active.take().unwrap();
                let mut l = lane.lock().unwrap();
                if completed_fast_save {
                    if l.source_epoch != view.source_epoch {
                        drop(l);
                        drop(view);
                        continue;
                    }
                    view.epoch = l.view_epoch;
                }
                let previous = l.reader_target.replace(Some(Box::new(view)));
                l.reader_active = true;
                l.writer_running = false;
                l.writer_epoch = None;
                drop(l);
                drop(previous);
                continue;
            }
            let mut cached = active.take().unwrap();
            cached.publication_cancel = lane.lock().unwrap().publication_cancel.clone();
            let epoch = cached.epoch;
            match cached.into_reader() {
                Ok((view, repository)) => {
                    parked_epoch = view.source_epoch;
                    parked_repository = Some(repository);
                    let mut l = lane.lock().unwrap();
                    let previous = l.reader_target.replace(Some(Box::new(view)));
                    l.reader_active = true;
                    l.writer_running = false;
                    drop(l);
                    drop(previous);
                }
                Err(error) => {
                    *output.lock().unwrap() = Some(Output::Error(epoch, error.to_string(), false))
                }
            }
        }
        let mut smb_retire_active = false;
        let mut ticked = false;
        let mut finalization_progressed = false;
        if let Some(a) = &mut active {
            progress_trace::current(|trace, counters| {
                counters.context(a.source_epoch, a.c.generation.0, a.epoch, a.c.inventory.0);
                counters.write(&[(Field::SelectedFolder, a.parent.entry.0)]);
                if let Backend::Live(session) = &a.session {
                    trace.native(a.source_epoch, a.c.generation.0, session);
                }
            });
            let scan = (a.source_epoch, a.c.generation);
            projection_cadence.bind(scan);
            if scan_p1::enabled() && scan_metrics.as_ref().is_none_or(|m| m.scan != scan) {
                scan_metrics = Some(scan_p1::WriterMetrics::new(scan));
            }
            diagnostics.begin();
            diagnostics.event(&lane, a, "iteration");
            if cancel {
                diagnostics.sampling = diagnostics.file.is_some();
                diagnostics.event(&lane, a, "cancel_received");
                progress_trace::current(|trace, _| {
                    trace.mark_for_source(a.source_epoch, &trace.cancel_observed)
                });
                a.cancelled = true;
                // The pinned dated preview remains owned and unchanged. Cancel
                // does not restart its folds, ordering or display preparation.
                a.session.control().cancel();
            }
            if retry
                && !a.source_revoked
                && !source_disconnected(&lane.lock().unwrap(), &a.root)
                && lane.lock().unwrap().cancel_requested_epoch != Some(a.source_epoch)
            {
                if let Err(error) = a.close_live_preview() {
                    a.preparation_blocked = true;
                    a.session.control().pause(true);
                    *output.lock().unwrap() = Some(Output::Error(a.epoch, error.to_string(), true));
                    continue;
                }
                // Only sealed candidates need to rebuild a publication plan.
                // An unsealed writer keeps the reader slot for its live view.
                a.cancelled = false;
                a.preparation_blocked = false;
                {
                    let mut l = lane.lock().unwrap();
                    a.publication_cancel = l.publication_cancel.clone();
                    l.acquisition_publication =
                        Some((a.root.clone(), a.publication_cancel.clone()));
                    l.phase = Some((a.source_epoch, Phase::Preparing, a.c.generation));
                }
                if a.session.status().state == State::Sealed
                    && let Some(repository) = &mut a.repository
                {
                    // Explicit recovery certifies preserved files before any
                    // future reader/builder reservation consumes that capacity.
                    drop(repository.plan.take());
                    let recovered = repository
                        .owner
                        .lock()
                        .unwrap()
                        .prepare_publication_retry(a.c.generation, &a.publication_cancel);
                    let result = recovered.and_then(|()| {
                        repository.plan = Some(Pipeline::for_publication()?);
                        Ok(())
                    });
                    if let Err(error) = result {
                        a.preparation_blocked = true;
                        *output.lock().unwrap() =
                            Some(Output::Error(a.epoch, error.to_string(), true));
                    }
                }
                sortsize_core::resources::Governor::global().refresh_native();
                a.session.retry_store();
                a.session.control().pause(false);
                a.force = true;
            }
            let mut projection_started = None;
            let result = (|| -> sortsize_store::inventory::Result<()> {
                // An earlier publication error parks the sealed candidate.
                // A later Cancel must still reach its terminal acknowledgement,
                // before the preparation_blocked fast path below. Freeze all
                // automatic work after that acknowledgement.
                if a.preparation_blocked && settle_cancelled_failure(a, &lane) {
                    return Ok(());
                }
                if !a.cancelled && !a.preparation_blocked {
                    let sort = lane.lock().unwrap().detail_sort;
                    let changed_sort = a.sync_sort(sort);
                    let previous_parent = a.parent;
                    let user_requested = changed_sort || navigation.is_some();
                    if !changed_sort {
                        a.view_anchor(&lane);
                    }
                    if let Some((epoch, command)) = navigation {
                        a.navigate(epoch, command)?;
                    }
                    a.sync_requested_folder(&lane)?;
                    if user_requested || a.parent != previous_parent {
                        projection_cadence.prioritize();
                    }
                }
                #[cfg(test)]
                if let Some(gate) = { lane.lock().unwrap().tick_gate.clone() } {
                    lane.lock().unwrap().tick_blocked = true;
                    let (lock, ready) = &*gate;
                    let mut release = lock.lock().unwrap();
                    while !*release {
                        release = ready.wait(release).unwrap();
                    }
                }
                #[cfg(test)]
                {
                    let l = lane.lock().unwrap();
                    if l.hold_generation == Some(a.c.generation)
                        && l.writer_generation == Some(a.c.generation)
                        && l.writer_entries > 1
                    {
                        return Ok(());
                    }
                }
                // Test pacing delays acquisition only. Dated fold/order quanta
                // must keep advancing while that controlled writer is idle.
                #[cfg(test)]
                let acquisition_due = {
                    let delay = lane.lock().unwrap().tick_delay;
                    delay.is_zero() || last_acquisition_tick.elapsed() >= delay
                };
                #[cfg(not(test))]
                let acquisition_due = true;
                if source_disconnected(&lane.lock().unwrap(), &a.root) {
                    a.request_cancel();
                }
                let status = if acquisition_due {
                    #[cfg(test)]
                    {
                        last_acquisition_tick = Instant::now();
                    }
                    diagnostics.event(&lane, a, "tick_begin");
                    let status =
                        scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.tick), || {
                            let _trace = sortsize_store::scan_trace::phase("service_tick");
                            a.session.tick()
                        })?;
                    diagnostics.event(&lane, a, "tick_end");
                    ticked = true;
                    status
                } else {
                    a.session.status()
                };
                if matches!(status.state, State::Cancelled | State::CancelledStoreError) {
                    progress_trace::current(|trace, _| {
                        trace.mark_for_source(a.source_epoch, &trace.cancel_terminal)
                    });
                    a.cancelled = true;
                }
                progress_trace::current(|_, counters| {
                    counters.write(&[(Field::State, status.state as u64 + 1)])
                });
                {
                    let mut l = lane.lock().unwrap();
                    l.writer_running = writer_running(status);
                    if l.cancel_requested_epoch == Some(a.source_epoch)
                        && a.fast_mode != FastMode::PreviewWriter
                    {
                        a.cancelled = true;
                    }
                    if a.preparation_blocked
                        && !l.writer_running
                        && !a.cancelled
                        && l.source_epoch == a.source_epoch
                    {
                        l.phase = Some((a.source_epoch, Phase::Failed, a.c.generation));
                    }
                    l.writer_quanta = status.quanta;
                    #[cfg(test)]
                    {
                        // The last tick can commit EOF and make publication
                        // possible in this turn. Expose the committed entries
                        // and honor the test hold before finalization.
                        l.writer_entries = a.session.store().entry_count(a.c.generation)?;
                        l.writer_generation = Some(a.c.generation);
                        l.writer_in_flight = status.in_flight;
                        if l.hold_generation == Some(a.c.generation) && l.writer_entries > 1 {
                            return Ok(());
                        }
                    }
                }
                // An ingestion refusal owns uncommitted results. While it is
                // parked, do not run display folds/orders or certification. A
                // Cancel must first drain the scheduler; its failed terminal
                // remains unsealed and can never replace G0.
                if a.cancelled {
                    if settle_cancelled_failure(a, &lane) {
                        diagnostics.sampling = diagnostics.file.is_some();
                        diagnostics.event(&lane, a, "cancelled_frozen");
                        if let Some(m) = &mut scan_metrics {
                            m.report("cancelled");
                        }
                        if let Some(batch) = &mut smb_batch {
                            // Revocation remains attached to the old intention
                            // even if Reconnect already removed the opt-out.
                            batch.disable_sources(std::slice::from_ref(&a.root));
                            batch.finish(None);
                            smb_retire_active = true;
                        }
                    }
                    // While native jobs/results still drain, keep every dated
                    // projection frozen too. No display derivative may delay
                    // the next scheduler quantum or the terminal handoff.
                    return Ok(());
                }
                if status.state == State::StoreError || a.preparation_blocked {
                    return Ok(());
                }
                if a.fast_mode == FastMode::PreviewWriter {
                    return Ok(());
                }
                if matches!(status.state, State::Idle | State::Cancelled) && status.in_flight == 0 {
                    let mut l = lane.lock().unwrap();
                    if l.source_epoch == a.source_epoch {
                        l.phase = Some((a.source_epoch, Phase::Finalizing, a.c.generation));
                    }
                    drop(l);
                    if let Some(m) = &mut scan_metrics {
                        m.acquisition_finished();
                    }
                    diagnostics.event(&lane, a, "finalize_begin");
                    let finalized =
                        scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.finalize), || {
                            a.session.finalize_step()
                        })?;
                    // A successful, unfinished Live quantum has committed a
                    // reconciliation cursor/task or a consolidation window.
                    // Errors leave this false; cached readers cannot signal it.
                    finalization_progressed =
                        finalized.is_none() && matches!(a.session, Backend::Live(_));
                    diagnostics.event(&lane, a, "finalize_end");
                }
                if a.session.status().state == State::Sealed && a.sealed.is_none() {
                    let measured = std::time::SystemTime::now();
                    scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.measurement), || {
                        a.session.store_mut()?.record_measurement(
                            a.c.generation,
                            &a.root,
                            super::cache::seconds(a.started),
                            super::cache::seconds(measured),
                        )
                    })?;
                    a.sealed = Some(measured);
                }
                if a.session.status().state == State::Sealed {
                    if a.fast_mode == FastMode::Acquiring {
                        a.attach_fast_reader(&lane)?;
                        if let Some(m) = &mut scan_metrics {
                            m.report("ephemeral_handoff");
                        }
                        return Ok(());
                    }
                    if a.preparation_blocked {
                        return Ok(());
                    }
                    if smb_batch
                        .as_ref()
                        .is_some_and(|batch| batch.current.is_some())
                    {
                        // Reuse the existing bounded derivative; never copy canonical
                        // entries into a second server inventory.
                        if !a.session.store().folder_fold_step_at(
                            a.c.generation,
                            &mut a.fold,
                            a._temporary.path(),
                        )? {
                            return Ok(());
                        }
                        if a.cancelled
                            && smb_kept
                                .values()
                                .find(|kept| {
                                    kept.share.native_root.as_deref() == Some(a.root.as_path())
                                        || smb_batch.as_ref().and_then(|batch| {
                                            batch
                                                .current
                                                .map(|index| &batch.snapshot.shares[index].path)
                                        }) == Some(&kept.share.path)
                                })
                                .is_some_and(|kept| {
                                    kept.workspace
                                        .repository()
                                        .lock()
                                        .unwrap()
                                        .published(kept.generation)
                                        .ok()
                                        .flatten()
                                        .is_some()
                                })
                        {
                            // A revoke can arrive during this quantum and be
                            // followed immediately by reconnection. Its cancelled
                            // control is final even before queued scopes are drained.
                            // G0 stays dated; this operation must report Cancelled.
                            if let Some(batch) = &mut smb_batch {
                                batch.disable_sources(std::slice::from_ref(&a.root));
                                batch.finish(None);
                            }
                            smb_retire_active = true;
                            return Ok(());
                        }
                    }
                    if !a.cancelled
                        && a.repository
                            .as_ref()
                            .is_some_and(|repository| repository.published != Some(a.c.generation))
                    {
                        lane.lock().unwrap().phase =
                            Some((a.source_epoch, Phase::Certifying, a.c.generation));
                    }
                    #[cfg(test)]
                    if let Some(gate) = { lane.lock().unwrap().publication_gate.clone() } {
                        lane.lock().unwrap().publication_blocked = true;
                        let (lock, ready) = &*gate;
                        let mut release = lock.lock().unwrap();
                        while !*release {
                            release = ready.wait(release).unwrap();
                        }
                    }
                    if cancel_revoked_publication(a, &lane, &mut smb_batch) {
                        smb_retire_active = smb_batch.is_some();
                        return Ok(());
                    }
                    diagnostics.event(&lane, a, "publish_begin");
                    let read_view = match &mut scan_metrics {
                        Some(m) => scan_p1::measure(Some(&mut m.publish), || {
                            a.publish_measured(&lane, Some(&mut m.publication))
                        }),
                        None => a.publish_measured(&lane, None),
                    }?;
                    if let Some(mut read_view) = read_view {
                        diagnostics.event(&lane, a, "publish_end");
                        // Publication contains native FULL sync/rename calls. If
                        // one finished concurrently with revocation, keep G0's
                        // selected reader and never announce this operation Ready.
                        if cancel_revoked_publication(a, &lane, &mut smb_batch) {
                            smb_retire_active = smb_batch.is_some();
                            return Ok(());
                        }
                        let measure = if smb_batch
                            .as_ref()
                            .is_some_and(|batch| batch.current.is_some())
                        {
                            a.session
                                .store()
                                .folder_measure(a.c.generation, EntryId(0), &a.fold)?
                        } else {
                            None
                        };
                        let coverage = read_view
                            .certified_generation()
                            .map_or(ScopeCoverage::Partial, |generation| generation.coverage());
                        // Only the in-memory handoff is atomic with a UI revoke.
                        // All SQLite, certification and filesystem calls finished
                        // before taking this lock; old owners are dropped after it.
                        let mut l = lane.lock().unwrap();
                        if a.publication_cancel.is_cancelled() || source_disconnected(&l, &a.root) {
                            if source_disconnected(&l, &a.root) {
                                a.request_cancel();
                            } else {
                                a.cancelled = true;
                                a.session.control().cancel();
                            }
                            drop(l);
                            cancel_revoked_publication(a, &lane, &mut smb_batch);
                            smb_retire_active = smb_batch.is_some();
                            return Ok(());
                        }
                        // A click can race a native publication call after its
                        // last cooperative check. Keep the operation cancelled
                        // and G0 selected, even if certification just completed.
                        if l.cancel_requested_epoch == Some(a.source_epoch) {
                            a.cancelled = true;
                            read_view.cancelled = true;
                        }
                        let mut replaced_share = None;
                        if let Some(batch) = &mut smb_batch
                            && batch.current.is_some()
                        {
                            batch.observe(measure, coverage, false);
                            let index = batch.current.unwrap();
                            batch.snapshot.shares[index].measured = a.sealed;
                            batch.finish(None);
                            let logical_key = &batch.snapshot.shares[index].path;
                            smb_kept.retain(|root, kept| {
                                root == &a.root || &kept.share.path != logical_key
                            });
                            replaced_share = smb_kept.insert(
                                a.root.clone(),
                                super::smb_server::Kept {
                                    workspace: a.workspace.as_ref().unwrap().clone(),
                                    generation: a.c.generation,
                                    share: batch.snapshot.shares[index].clone(),
                                    _memory: batch.memory(),
                                },
                            );
                        }
                        let previous = if a.source_epoch == l.source_epoch {
                            l.certified_readiness = Some(CertifiedReadiness {
                                epoch: a.source_epoch,
                                context: a.c,
                                page: false,
                                cleanup: false,
                            });
                            read_view.epoch = l.view_epoch;
                            read_view.p1_handoff_at = scan_metrics.as_ref().map(|_| Instant::now());
                            // A cancelled refresh keeps the already consulted G0.
                            // Without G0, its sealed partial result remains usable.
                            let previous = if (!a.cancelled || !l.reader_active)
                                && smb_view_matches(&l, &a.root)
                            {
                                l.reader_active = true;
                                l.reader_target.replace(Some(Box::new(read_view)))
                            } else {
                                None
                            };
                            l.phase = Some((
                                a.source_epoch,
                                if a.cancelled {
                                    Phase::Cancelled
                                } else {
                                    Phase::Finalizing
                                },
                                a.c.generation,
                            ));
                            previous
                        } else {
                            None
                        };
                        l.writer_running = false;
                        drop(l);
                        if let Some(m) = &mut scan_metrics {
                            m.report(if a.cancelled {
                                "cancelled"
                            } else {
                                "published"
                            });
                        }
                        drop(previous);
                        drop(replaced_share);
                    }
                    lane.lock().unwrap().writer_running = false;
                }
                if lane.lock().unwrap().reader_active && smb_batch.is_none() {
                    return Ok(());
                }
                // Queue pressure can disappear immediately after a batch is
                // ingested. Pace throughout acquisition, not only at that
                // transient high-water mark. Once ingestion ends, the certified
                // reader owns preparation of the final view; avoid building a
                // competing provisional derivative between finalization steps.
                let regulated = regulate_projections.unwrap_or_else(|| a.session.is_local_live());
                let acquiring = regulated && status.state == State::Running;
                let defer_finalizing = status.state == State::Idle
                    && status.in_flight == 0
                    && !projection_cadence.finalizing_quantum()
                    && regulate_projections.unwrap_or_else(|| {
                        // On Linux, preserve the first or retried preview and
                        // the acquisition cadence. Once a preview was published,
                        // only explicit navigation needs another provisional fold.
                        a.session.is_local_live() && (cfg!(windows) || !a.force)
                    });
                if (acquiring
                    && !projection_cadence.allow_quantum(
                        a.force,
                        scan_p1::writer_drain_ready(status),
                        Instant::now(),
                    ))
                    || defer_finalizing
                {
                    if let Some(m) = &mut scan_metrics {
                        m.projection_deferred += 1;
                    }
                    progress_trace::current(|_, counters| {
                        counters.add(Field::DeferredQuanta);
                        counters.write(&[(Field::HoldReason, 1)]);
                    });
                    return Ok(());
                }
                if acquiring {
                    projection_started = Some(Instant::now());
                }
                let frame_interval = projection_cadence.frame_interval(acquiring);
                diagnostics.event(&lane, a, "fold_begin");
                let ready = scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.fold), || {
                    if matches!(a.session, Backend::Live(_))
                        && a.session.status().state != State::Sealed
                    {
                        a.advance_live_preview(frame_interval)
                    } else {
                        a.session.store().folder_fold_step_at(
                            a.c.generation,
                            &mut a.fold,
                            a._temporary.path(),
                        )
                    }
                })?;
                diagnostics.event(&lane, a, "fold_end");
                // Keep the prior presentation until the dated measurements and
                // global order finish. The acquisition itself remains partial;
                // writer revisions never restart an unfinished derivative.
                let below_floor = if let Some(preview) = &a.live_preview
                    && !ready
                {
                    let logical = preview
                        .measure(a.parent.entry)?
                        .and_then(|measure| measure.logical);
                    progress_trace::current(|_, counters| {
                        counters.write(&[
                            (Field::PreviewLogical, logical.unwrap_or(0)),
                            (Field::PreviewLogicalKnown, u64::from(logical.is_some())),
                        ])
                    });
                    logical.unwrap_or(0) < a.preview_floor
                } else {
                    false
                };
                if below_floor {
                    progress_trace::current(|_, counters| {
                        counters.add(Field::FloorHolds);
                        counters.write(&[(Field::HoldReason, 2)]);
                    });
                    return Ok(());
                }
                diagnostics.event(&lane, a, "order_begin");
                let order_ready =
                    scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.order), || {
                        a.advance_order()
                    })?;
                diagnostics.event(&lane, a, "order_end");
                if !order_ready && a.live_preview.is_none() {
                    progress_trace::current(|_, counters| {
                        counters.write(&[(Field::HoldReason, 3)])
                    });
                    return Ok(());
                }
                let revision = a.live_preview.as_ref().map_or_else(
                    || a.session.store().revision(a.c.generation),
                    |preview| Ok(preview.revision()),
                )?;
                diagnostics.event(&lane, a, "folder_tree_begin");
                scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.tree), || {
                    a.advance_folder_tree()
                })?;
                diagnostics.event(&lane, a, "folder_tree_end");
                if a.force
                    || a.last.elapsed() >= frame_interval
                        && (a.folder_tree.borrow().dirty
                            || a.revision != Some(revision)
                            || ready != a.measured
                            || !order_ready
                            || a.live_preview
                                .as_ref()
                                .is_some_and(|preview| !preview.complete())
                            || a.last_state != a.session.status().state)
                {
                    diagnostics.event(&lane, a, "prepare_begin");
                    let top_scope = lane.lock().unwrap().top_scope.clone();
                    let page =
                        scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.prepare), || {
                            a.prepare(units, top_scope)
                        })?;
                    diagnostics.event(&lane, a, "prepare_end");
                    if let Some(batch) = &mut smb_batch
                        && batch.current.is_some()
                    {
                        batch.observe(
                            page.root_measure,
                            page.frame.coverage,
                            page.root_provisional,
                        );
                    }
                    a.force = false;
                    a.last = Instant::now();
                    a.revision = Some(revision);
                    a.measured = ready;
                    a.last_state = a.session.status().state;
                    let lane = lane.lock().unwrap();
                    if a.source_epoch == lane.source_epoch
                        && page.epoch == lane.view_epoch
                        && smb_view_matches(&lane, &a.root)
                    {
                        let mut output = output.lock().unwrap();
                        // A retained G0 or a paused writer must not hide a
                        // failure before the UI has had one chance to read it.
                        if !matches!(output.as_ref(), Some(Output::Error(epoch, _, _)) if *epoch >= page.epoch)
                        {
                            projection_cadence.published(
                                page.root_measure
                                    .and_then(|measure| measure.logical)
                                    .is_some_and(|logical| logical > 0),
                                ready && order_ready && !a.folder_tree.borrow().has_work(),
                            );
                            let _trace = sortsize_store::scan_trace::phase("publish_mailbox");
                            progress_trace::current(|_, counters| {
                                counters.add(Field::Pages);
                                counters.write(&[
                                    (Field::Token, page.token as u64),
                                    (Field::HoldReason, 0),
                                    (Field::PublishedNs, progress_trace::now_ns()),
                                ]);
                            });
                            *output = Some(Output::Page(Box::new(page)));
                        } else {
                            progress_trace::current(|_, counters| {
                                counters.write(&[(Field::HoldReason, 5)])
                            });
                        }
                    } else {
                        progress_trace::current(|_, counters| {
                            counters.write(&[(Field::HoldReason, 4)])
                        });
                    }
                }
                Ok(())
            })();
            // Include fold, order, tree and page preparation, including an early
            // return below the retained total or a storage error. Every next
            // projection still follows a fresh acquisition/control turn.
            if let Some(started) = projection_started {
                projection_cadence.finished_quantum(started.elapsed());
            }
            if let Err(error) = result {
                if let Some(m) = &mut scan_metrics {
                    m.report("error");
                }
                diagnostics.event(&lane, a, "error");
                if a.publication_cancel.is_cancelled() {
                    a.cancelled = true;
                    if let Some(batch) = &mut smb_batch {
                        batch.disable_sources(std::slice::from_ref(&a.root));
                    }
                }
                let message = match a.close_live_preview() {
                    Ok(()) => super::acquisition_error_message(error),
                    Err(close) => format!("{}\n{close}", super::acquisition_error_message(error)),
                };
                a.preparation_blocked = true;
                a.session.control().pause(true);
                lane.lock().unwrap().writer_running = writer_running(a.session.status());
                if let Some(batch) = &mut smb_batch {
                    if let Some(workspace) = &a.workspace {
                        let logical_key = batch
                            .current
                            .map(|index| batch.snapshot.shares[index].path.clone());
                        if !smb_kept
                            .values()
                            .any(|kept| Some(&kept.share.path) == logical_key.as_ref())
                        {
                            smb_kept.entry(a.root.clone()).or_insert_with(|| {
                                super::smb_server::Kept {
                                    workspace: workspace.clone(),
                                    generation: a.c.generation,
                                    share: smb_unknown_share(
                                        a.root.clone(),
                                        ScopeCoverage::Unknown,
                                        None,
                                        logical_key,
                                    ),
                                    _memory: batch.memory(),
                                }
                            });
                        }
                    }
                    batch.finish(Some(message.clone()));
                    smb_retire_active = true;
                }
                if settle_cancelled_failure(a, &lane) {
                    if smb_batch.is_none() && !a.publication_cancel.is_cancelled() {
                        *output.lock().unwrap() = Some(Output::Error(a.epoch, message, true));
                    }
                } else if smb_batch.is_none() {
                    *output.lock().unwrap() = Some(Output::Error(a.epoch, message, true));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else {
            lane.lock().unwrap().writer_running = false;
        }
        if smb_retire_active && let Some(old) = active.take() {
            old.session.control().cancel();
            drop(old);
        }
        smb_retained(&lane, &smb_kept);
        smb_snapshot(&lane, &smb_batch);
        let mut cleaned = None;
        if active.as_ref().is_some_and(|a| {
            a.repository
                .as_ref()
                .is_some_and(|repository| repository.published == Some(a.c.generation))
        }) {
            let _trace = sortsize_store::scan_trace::phase("writer_cleanup");
            let mut finished = active.take().unwrap();
            let generation = finished.c.generation;
            let context = finished.c;
            let epoch = finished.source_epoch;
            let (mut repository, workspace) = finished.take_repository().unwrap();
            #[cfg(test)]
            if let Some(gate) = { lane.lock().unwrap().cleanup_gate.clone() } {
                lane.lock().unwrap().cleanup_blocked = true;
                let (lock, ready) = &*gate;
                let mut release = lock.lock().unwrap();
                while !*release {
                    release = ready.wait(release).unwrap();
                }
            }
            // Close Session, its canonical connection, TEMP folds and result
            // files before retiring the redundant acquisition candidate.
            drop(finished);
            drop(repository.plan.take());
            if let Err(error) = repository
                .owner
                .lock()
                .unwrap()
                .discard_candidate(generation, &Default::default())
            {
                *super::cache::CLEANUP_ERROR.lock().unwrap() = Some(error.to_string());
            }
            parked_epoch = epoch;
            parked_repository = Some((repository, workspace));
            cleaned = Some((epoch, context));
        }
        if active.is_none() {
            let retired = lane.lock().unwrap().retire_private.take();
            if let Some(workspace) = retired {
                workspace.retire_original();
            }
        }
        {
            let mut l = lane.lock().unwrap();
            l.writer_epoch = active
                .as_ref()
                .filter(|a| a.owns_acquisition())
                .map(|a| a.source_epoch);
            l.acquisition_root = active.as_ref().map(|a| a.root.clone());
            l.acquisition_publication = active
                .as_ref()
                .filter(|a| matches!(a.session, Backend::Live(_)))
                .map(|a| (a.root.clone(), a.publication_cancel.clone()));
            l.smb_acquisition_root = smb_batch
                .as_ref()
                .and(active.as_ref())
                .map(|a| a.root.clone());
            l.writer_owns_session = active.as_ref().is_some_and(Active::owns_acquisition);
            if let Some((epoch, context)) = cleaned {
                l.certified_step_finished(epoch, context, false);
            }
        }
        let metrics = scan_metrics.as_mut().filter(|m| !m.reported);
        if active.as_ref().is_some_and(|a| {
            !a.cancelled
                && !a.preparation_blocked
                && scan_p1::writer_finalize_ready(a.session.status(), finalization_progressed)
        }) {
            // Like the reader's bounded fold, productive finalization returns
            // to the command/acquisition checks without an idle-poll delay.
            // Never keep this credit across iterations, blocked work or sealing.
            std::thread::yield_now();
        } else if drain_without_wait
            && ticked
            && active.as_ref().is_some_and(|a| {
                !a.cancelled
                    && !a.preparation_blocked
                    && a.session.is_local_live()
                    && scan_p1::writer_drain_ready(a.session.status())
            })
        {
            // Returned work can sustain the next normal bounded turn. Yield
            // to other threads and recheck all controls at the top of the loop.
            scan_p1::measure(metrics.map(|m| &mut m.drain_yield), std::thread::yield_now);
        } else {
            scan_p1::measure(metrics.map(|m| &mut m.sleep), || {
                std::thread::sleep(Duration::from_millis(2))
            });
        }
    }
    drop(direct);
    drop(active);
    drop(parked_repository);
    drain_stopped_lane(&lane);
    let mut l = lane.lock().unwrap();
    l.writer_epoch = None;
    l.writer_owns_session = false;
    drop(l);
    drop(search_output);
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
