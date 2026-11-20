/*! Bounded native acquisition for a disposable direct view.

The receiver assigns occurrence IDs and chooses which observed directories
to keep. The scheduler never reads or writes an inventory. */
use super::*;
use crate::inventory::queries::workspace::Artifact;
use sortsize_core::{
    acquisition::{NodeProperties, Stamp, profile::DirectoryMetrics},
    resources::{Category, Governor, Reservation},
};
use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

const WORKERS: usize = 8;
const BATCH_MEMORY: u64 = 4 * 1024 * 1024;
const MIN_FRONTIER: u64 = 4 * 1024 * 1024;
const CONTROL_POLL: Duration = Duration::from_millis(10);

// Fixed-size opt-in aggregates. Worker durations may overlap across threads;
// their sums must not be interpreted as additive wall-clock phases.
#[derive(Default)]
struct DirectMetrics {
    dispatched: u64,
    completed: u64,
    observations: u64,
    acquisition_permit_denials: u64,
    cursor_permit_denials: u64,
    dispatch_wait: Timing,
    completion_wait: Timing,
    worker: Timing,
    checks: Timing,
    enumeration: Timing,
    metadata: Timing,
    acquisition: DirectoryMetrics,
    ntfs: NtfsListingStats,
}

impl DirectMetrics {
    fn completion(&mut self, sample: &WorkerSample, observations: usize) {
        self.completed += 1;
        self.observations += observations as u64;
        self.dispatch_wait
            .add(sample.started.duration_since(sample.dispatched));
        self.completion_wait.add(sample.finished.elapsed());
        self.worker
            .add(sample.finished.duration_since(sample.started));
        self.checks.merge_ref(&sample.checks);
        self.enumeration.merge_ref(&sample.enumeration);
        self.metadata.merge_ref(&sample.metadata);
        self.acquisition.merge(&sample.acquisition);
        self.ntfs.batch_calls += sample.ntfs.batch_calls;
        self.ntfs.relative_opens += sample.ntfs.relative_opens;
        self.ntfs.stale_rows += sample.ntfs.stale_rows;
    }

    fn report(&self, outcome: &'static str) {
        // No native names, paths or per-entry logs. A closed diagnostic stream
        // cannot fail the acquisition. Includes quanta drained after cancellation.
        let _ = self.write(&mut io::stderr().lock(), outcome);
    }

    fn write(&self, out: &mut impl io::Write, outcome: &'static str) -> io::Result<()> {
        write!(
            out,
            "scan_direct_scheduler outcome={outcome} dispatched={} completed={} observations_returned={} acquisition_permit_denials={} cursor_permit_denials={} ntfs_batch_calls={} ntfs_relative_opens={} ntfs_stale_rows={}",
            self.dispatched,
            self.completed,
            self.observations,
            self.acquisition_permit_denials,
            self.cursor_permit_denials,
            self.ntfs.batch_calls,
            self.ntfs.relative_opens,
            self.ntfs.stale_rows,
        )?;
        for (key, timing) in [
            ("dispatch_wait", &self.dispatch_wait),
            ("completion_wait", &self.completion_wait),
            ("worker", &self.worker),
            ("worker_directory_checks", &self.checks),
            ("worker_enumeration", &self.enumeration),
            ("worker_metadata", &self.metadata),
        ] {
            timing.write(out, key)?;
        }
        self.acquisition.write(out)?;
        writeln!(out)
    }
}

// This key stays in a native cursor. It carries no durable writer authority.
const DIRECT_CONTEXT: AcquisitionContext = AcquisitionContext {
    inventory: InventoryId(0),
    generation: GenerationId(0),
    source: SourceId(0),
    scan: ScanId(0),
    policy: PolicyId(0),
    mount: MountEpoch(0),
    run: RunEpoch(0),
    validation: ValidationEpoch(0),
};

/// A folder occurrence accepted by the view. Names retain the originating
/// adapter's namespace and the adapter resolves each directory for traversal.
#[derive(Debug)]
pub struct ObservedDirectory {
    pub id: EntryId,
    pub name: NativeName,
    pub evidence: Option<NativeEvidence>,
}

/// Shared input to the tree and observation log, free of native paths.
pub struct ObservationBatch<'a> {
    pub parent: EntryId,
    pub sequence: u64,
    pub end: Option<ListingEnd>,
    pub observations: &'a [Observation],
    pub evidence: &'a [Option<NativeEvidence>],
    pub properties: &'a [NodeProperties],
    pub directory: Option<&'a Stamp>,
    pub detached: bool,
    pub errors: u64,
    pub denied: u64,
}

pub struct DirectBatch<'a> {
    pub batch: ObservationBatch<'a>,
    /// Listing entries before application exclusions or metadata processing.
    pub(crate) entries_seen: u64,
    /// Exact native spelling from the guarded cursor.
    pub(crate) parent_path: &'a Path,
    pub(crate) last_error: Option<&'a AcquisitionError>,
    /// Optional path-free counters with BUNNY_SCAN_P1_METRICS=1.
    pub ntfs: NtfsListingStats,
    /// Accounted capacity of the pending directories, including native paths.
    pub frontier_bytes: usize,
}

struct DirectDirectory {
    id: EntryId,
    path: PathBuf,
    evidence: Option<NativeEvidence>,
}

struct Frontier {
    queued: BTreeMap<EntryId, DirectDirectory>,
    bytes: usize,
    budget: Vec<Reservation>,
    governor: Governor,
    spill: Option<FrontierSpill>,
    fixed_limit: Option<u64>,
}
impl Frontier {
    fn charge(directory: &DirectDirectory) -> usize {
        // Allow room for internal nodes and unused B-tree slots as well as
        // each directory's path allocation. This is accounting, not RSS.
        directory.path.capacity() + 3 * std::mem::size_of::<(EntryId, DirectDirectory)>()
    }

    fn push(&mut self, directory: DirectDirectory) -> Result<()> {
        if directory.path.as_os_str().as_encoded_bytes().len() > MAX_PATH {
            return Err(Error::Contract("direct acquisition directory path budget"));
        }
        let bytes = self.bytes.saturating_add(Self::charge(&directory));
        let reserved = self
            .fixed_limit
            .unwrap_or_else(|| self.budget.iter().map(Reservation::bytes).sum());
        if bytes as u64 > reserved {
            if self.fixed_limit.is_none()
                && let Some(charge) = self
                    .governor
                    .reserve(Category::Transport, MIN_FRONTIER, None)
            {
                self.budget.push(charge);
            } else if let Some(spill) = &mut self.spill {
                return spill.push(directory);
            } else {
                return Err(Error::Contract("direct acquisition frontier memory quota"));
            }
        }
        match self.queued.entry(directory.id) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(directory);
                self.bytes = bytes;
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                Err(Error::Contract("duplicate direct acquisition directory"))
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.spill.as_ref().is_none_or(|spill| spill.end == 0)
    }
    fn refill(&mut self) -> Result<()> {
        if self.queued.is_empty()
            && let Some(spill) = &mut self.spill
            && let Some(directory) = spill.pop()?
        {
            self.bytes = Self::charge(&directory);
            self.queued.insert(directory.id, directory);
        }
        Ok(())
    }

    fn take(&mut self, id: EntryId) -> DirectDirectory {
        let directory = self.queued.remove(&id).expect("selected directory");
        self.bytes -= Self::charge(&directory);
        directory
    }
}

// Disk stack owns native paths and identity evidence, never source contents.
// LIFO overflow retains the scheduler's depth-first bias. Only growth is charged.
struct FrontierSpill {
    file: std::fs::File,
    end: u64,
    high_water: u64,
    charge: FrontierCharge,
}
enum FrontierCharge {
    Direct(crate::direct_tree::storage::DiskCharge),
    Shared(Artifact),
}
impl FrontierSpill {
    fn new(scratch: &Path, budget: crate::direct_tree::storage::DiskBudget) -> Result<Self> {
        let owner = tempfile::Builder::new()
            .prefix("direct-frontier-")
            .tempfile_in(scratch)?;
        let file = owner.reopen()?;
        let charge = crate::direct_tree::storage::DiskCharge::new(budget, owner)?;
        Ok(Self {
            file,
            end: 0,
            high_water: 0,
            charge: FrontierCharge::Direct(charge),
        })
    }
    fn shared(artifact: Artifact) -> Result<Self> {
        Ok(Self {
            file: artifact.reopen()?,
            end: 0,
            high_water: 0,
            charge: FrontierCharge::Shared(artifact),
        })
    }
    fn push(&mut self, directory: DirectDirectory) -> Result<()> {
        use crate::direct_tree::spool::Encoder;
        use std::io::{Seek, SeekFrom, Write};
        let name = encode_name(directory.path.as_os_str());
        let observation = Observation {
            name: name.clone(),
            kind: Kind::Directory,
            logical: None,
            allocated: None,
            modified: None,
            metadata_known: true,
        };
        let mut encoder = Encoder(Vec::new());
        encoder.record(
            u32::try_from(directory.id.0).map_err(|_| Error::Contract("direct frontier ID"))?,
            &name.encode(),
            &observation,
            directory.evidence.as_ref(),
            &NodeProperties::default(),
        )?;
        let length = encoder.0.len() as u32;
        encoder.0.extend_from_slice(&length.to_le_bytes());
        let end = self
            .end
            .checked_add(encoder.0.len() as u64)
            .ok_or(Error::Contract("direct frontier overflow"))?;
        let _admission = if end > self.high_water {
            match &self.charge {
                FrontierCharge::Direct(charge) => {
                    charge.admit(end - self.high_water)?;
                    None
                }
                FrontierCharge::Shared(artifact) => Some(artifact.admit_growth(end)?),
            }
        } else {
            None
        };
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&encoder.0)?;
        self.high_water = self.high_water.max(end);
        self.end = end;
        Ok(())
    }
    fn pop(&mut self) -> Result<Option<DirectDirectory>> {
        use crate::direct_tree::spool;
        use std::io::{Read, Seek, SeekFrom};
        if self.end == 0 {
            return Ok(None);
        }
        self.file.seek(SeekFrom::Start(self.end - 4))?;
        let mut footer = [0u8; 4];
        self.file.read_exact(&mut footer)?;
        let length = u64::from(u32::from_le_bytes(footer));
        if length > 128 * 1024 {
            return Err(spool::invalid());
        }
        let start = self
            .end
            .checked_sub(length + 4)
            .ok_or_else(spool::invalid)?;
        self.file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        spool::read_record(&mut self.file, &mut bytes)?;
        if bytes.len() as u64 + 2 != length {
            return Err(spool::invalid());
        }
        let row = spool::record(&bytes)?;
        let directory = DirectDirectory {
            id: row.id,
            path: PathBuf::from(native_name(&row.observation.name)?),
            evidence: row.evidence,
        };
        self.end = start;
        Ok(Some(directory))
    }
}

#[derive(Clone, Copy)]
enum Ready {
    Parked(usize),
    New(EntryId),
}

struct Dispatch {
    guard: Arc<Guard>,
    active: Arc<active::ActiveRoot>,
    transport: Arc<Reservation>,
    options: CollectionOptions,
    control: Control,
    trace: bool,
    metrics: Option<DirectMetrics>,
    reply: SyncSender<Result<Completed>>,
    frontier: Frontier,
    parked: VecDeque<Cursor>,
    in_flight: usize,
    workers: usize,
}

impl Dispatch {
    fn target(&self, id: EntryId) -> Option<Ready> {
        self.parked
            .iter()
            .position(|cursor| cursor.attempt.task.directory == id)
            .map(Ready::Parked)
            .or_else(|| {
                self.frontier
                    .queued
                    .contains_key(&id)
                    .then_some(Ready::New(id))
            })
    }

    fn fill(&mut self) -> Result<()> {
        while self.in_flight < self.workers {
            self.frontier.refill()?;
            let (stopped, priorities) = {
                let intent = self.control.0.lock().unwrap();
                (
                    intent.cancelled || intent.paused || self.control.external_cancelled(),
                    [intent.navigation, intent.visible, intent.enrichment],
                )
            };
            if stopped {
                break;
            }
            let priority = priorities
                .into_iter()
                .flatten()
                .find_map(|id| self.target(id));
            let background = if !self.parked.is_empty() {
                Some(Ready::Parked(0))
            } else {
                // Consume recently discovered descendants first. Broad levels
                // can fill a breadth-first frontier before processing leaves.
                self.frontier
                    .queued
                    .last_key_value()
                    .map(|(&id, _)| Ready::New(id))
            };
            let Some((ready, interactive)) = priority
                .map(|ready| (ready, true))
                .or_else(|| background.map(|ready| (ready, false)))
            else {
                break;
            };
            let Some(permit) = acquisition_lease(interactive, priority.is_some()) else {
                if let Some(metrics) = &mut self.metrics {
                    metrics.acquisition_permit_denials += 1;
                }
                break;
            };
            let (cursor, origin) = match ready {
                Ready::Parked(index) => (self.parked.remove(index).expect("selected cursor"), None),
                Ready::New(id) => {
                    let Some(lease) = cursor_lease(interactive) else {
                        if let Some(metrics) = &mut self.metrics {
                            metrics.cursor_permit_denials += 1;
                        }
                        // An interactive open must not starve existing cursors
                        // when the process-wide cursor budget is occupied.
                        let Some(cursor) = self.parked.pop_front() else {
                            break;
                        };
                        self.dispatch(cursor, None, permit)?;
                        continue;
                    };
                    let directory = self.frontier.take(id);
                    let cursor = Cursor {
                        attempt: AttemptKey {
                            task: TaskKey {
                                context: DIRECT_CONTEXT,
                                directory: id,
                                kind: TaskKind::EnumerateDirect,
                            },
                            attempt: AttemptId(1),
                            owner: OwnerEpoch(1),
                            branch: BranchRevision(1),
                        },
                        path: directory.path,
                        read: None,
                        snapshot: None,
                        sequence: 0,
                        _lease: lease,
                    };
                    (cursor, directory.evidence)
                }
            };
            self.dispatch(cursor, origin, permit)?;
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        cursor: Cursor,
        origin: Option<NativeEvidence>,
        permit: AcquisitionLease,
    ) -> Result<()> {
        let guard = self.guard.clone();
        let active = self.active.clone();
        let transport = self.transport.clone();
        let reply = self.reply.clone();
        let control = self.control.clone();
        let options = self.options;
        let dispatched = self.trace.then(Instant::now);
        // The shared pool has one queue slot per acquisition permit.
        pool()
            .send
            .send(Box::new(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    quantum_with_origin(
                        &guard,
                        cursor,
                        permit,
                        options,
                        &control,
                        dispatched,
                        origin.as_ref(),
                    )
                }));
                let result = result
                    .map_err(|_| Error::Contract("direct native acquisition worker panicked"))
                    .map(|mut completed| {
                        completed._session = Some(SessionLease {
                            _memory: transport,
                            _active: Some(active),
                        });
                        completed
                    });
                let _ = reply.send(result);
            }))
            .map_err(|_| Error::Io(io::ErrorKind::BrokenPipe.into()))?;
        self.in_flight += 1;
        if let Some(metrics) = &mut self.metrics {
            metrics.dispatched += 1;
        }
        Ok(())
    }

    fn accept(
        &mut self,
        mut done: Completed,
        accept: &mut impl FnMut(DirectBatch<'_>) -> Result<Vec<ObservedDirectory>>,
    ) -> Result<()> {
        let parent = done.cursor.attempt.task.directory;
        let children = accept(DirectBatch {
            batch: ObservationBatch {
                parent,
                sequence: done.cursor.sequence,
                end: done.end,
                observations: &done.observations,
                evidence: &done.evidence,
                properties: &done.properties,
                directory: done.directory.as_ref(),
                detached: done.detached,
                errors: done.errors.count,
                denied: done.errors.denied,
            },
            entries_seen: done.entries_seen,
            parent_path: &done.cursor.path,
            last_error: done.errors.last.as_ref(),
            ntfs: done
                .sample
                .as_ref()
                .map_or_else(NtfsListingStats::default, |s| s.ntfs),
            frontier_bytes: self.frontier.bytes,
        })?;
        if children.len() > done.observations.len() {
            return Err(Error::Contract("direct acquisition child count"));
        }
        for child in children {
            if child.id <= parent
                || !done
                    .observations
                    .iter()
                    .any(|row| row.kind == Kind::Directory && row.name == child.name)
            {
                return Err(Error::Contract("invalid direct acquisition child"));
            }
            let path = done.cursor.path.join(native_name(&child.name)?);
            if path.parent() != Some(done.cursor.path.as_path()) {
                return Err(Error::Contract("invalid direct acquisition child path"));
            }
            self.frontier.push(DirectDirectory {
                id: child.id,
                path,
                evidence: child.evidence,
            })?;
        }
        if done.end.is_none() {
            done.cursor.sequence = done.next_sequence()?;
            self.parked.push_back(done.cursor);
        }
        Ok(())
    }
}

/// Traverse the guarded source with the existing native quanta and bounded
/// process-wide permits. Call on a worker thread. Cancellation or an error
/// stops dispatch, discards queued completions and drains all dispatched
/// work before returning. An entered native call may take time to return.
pub fn run(
    guard: Arc<Guard>,
    options: CollectionOptions,
    control: &Control,
    mut accept: impl FnMut(DirectBatch<'_>) -> Result<Vec<ObservedDirectory>>,
) -> Result<()> {
    run_inner(guard, options, control, &mut accept, None, None)
}
pub(crate) fn run_with_spill(
    guard: Arc<Guard>,
    options: CollectionOptions,
    control: &Control,
    scratch: &Path,
    disk: crate::direct_tree::storage::DiskBudget,
    mut accept: impl FnMut(DirectBatch<'_>) -> Result<Vec<ObservedDirectory>>,
) -> Result<()> {
    let spill = FrontierSpill::new(scratch, disk)?;
    run_inner(guard, options, control, &mut accept, Some(spill), None)
}
pub(crate) struct CandidateTransport {
    pub memory: Reservation,
    pub frontier_bytes: u64,
    pub workers: usize,
}
pub(crate) fn run_candidates(
    guard: Arc<Guard>,
    cancel: &CancellationToken,
    frontier: Artifact,
    transport: Option<CandidateTransport>,
    mut accept: impl FnMut(DirectBatch<'_>) -> Result<Vec<ObservedDirectory>>,
) -> Result<()> {
    run_inner(
        guard,
        CollectionOptions {
            // This consumer uses only size, time and identity of guarded objects.
            allocation: false,
            owners: false,
        },
        &Control::with_cancellation(cancel),
        &mut accept,
        Some(FrontierSpill::shared(frontier)?),
        transport,
    )
    .map_err(|error| match error {
        Error::Contract(
            "global direct batch transport budget" | "global direct acquisition frontier budget",
        ) => Error::memory_admission(
            &Governor::global(),
            Category::Transport,
            BATCH_MEMORY + MIN_FRONTIER,
        ),
        _ => error,
    })
}
fn run_inner(
    guard: Arc<Guard>,
    options: CollectionOptions,
    control: &Control,
    mut accept: impl FnMut(DirectBatch<'_>) -> Result<Vec<ObservedDirectory>>,
    spill: Option<FrontierSpill>,
    candidate_transport: Option<CandidateTransport>,
) -> Result<()> {
    let governor = Governor::global();
    let (transport, budget, fixed_limit, workers) = if let Some(plan) = candidate_transport {
        (
            plan.memory,
            Vec::new(),
            Some(plan.frontier_bytes),
            plan.workers,
        )
    } else {
        let transport = governor
            .reserve(Category::Transport, BATCH_MEMORY, None)
            .ok_or(Error::Contract("global direct batch transport budget"))?;
        let budget = governor
            .reserve(Category::Transport, MIN_FRONTIER, None)
            .ok_or(Error::Contract("global direct acquisition frontier budget"))?;
        (transport, vec![budget], None, WORKERS)
    };
    let active = active::acquire(&guard)?;
    let mut frontier = Frontier {
        queued: BTreeMap::new(),
        bytes: 0,
        budget,
        governor,
        spill,
        fixed_limit,
    };
    frontier.push(DirectDirectory {
        id: EntryId(0),
        path: guard.root().to_owned(),
        evidence: Some(NativeEvidence::from_stamp(guard.evidence().1)),
    })?;
    let (reply, receive) = mpsc::sync_channel(workers);
    let trace = std::env::var_os("BUNNY_SCAN_P1_METRICS").is_some_and(|v| v == "1");
    let mut dispatch = Dispatch {
        guard,
        active,
        transport: Arc::new(transport),
        options,
        control: control.clone(),
        trace,
        metrics: trace.then(DirectMetrics::default),
        reply,
        frontier,
        parked: VecDeque::new(),
        in_flight: 0,
        workers,
    };
    let mut result = Ok(());
    loop {
        if result.is_ok() && !control.is_cancelled() {
            result = dispatch.fill();
        }
        if result.is_err() {
            control.cancel();
        }
        if control.is_cancelled() {
            dispatch.parked.clear();
            dispatch.frontier.queued.clear();
            dispatch.frontier.bytes = 0;
            if let Some(spill) = &mut dispatch.frontier.spill {
                spill.end = 0;
            }
        }
        if dispatch.in_flight == 0 {
            if control.is_cancelled()
                || (dispatch.frontier.is_empty() && dispatch.parked.is_empty())
            {
                break;
            }
            std::thread::sleep(CONTROL_POLL);
            continue;
        }
        match receive.recv_timeout(CONTROL_POLL) {
            Ok(done) => {
                dispatch.in_flight -= 1;
                if let Some(metrics) = &mut dispatch.metrics
                    && let Ok(done) = &done
                    && let Some(sample) = &done.sample
                {
                    metrics.completion(sample, done.observations.len());
                }
                if result.is_ok() && !control.is_cancelled() {
                    result = done.and_then(|done| dispatch.accept(done, &mut accept));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Io(io::ErrorKind::BrokenPipe.into()));
            }
        }
    }
    if let Some(metrics) = &dispatch.metrics {
        metrics.report(if result.is_err() {
            "failed"
        } else if control.is_cancelled() {
            "cancelled"
        } else {
            "drained"
        });
    }
    result?;
    if control.is_cancelled() {
        return Err(Error::Io(io::ErrorKind::Interrupted.into()));
    }
    dispatch.guard.validate_internal_state()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::acquisition::Policy;

    #[test]
    fn overflow_frontier_retains_native_paths_proofs_and_releases_lifo() {
        let temp = tempfile::tempdir().unwrap();
        let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
        let usage = governor.usage();
        let _filled = governor
            .reserve(Category::Transport, usage.limits.categories[2], None)
            .unwrap();
        let disk = crate::direct_tree::storage::DiskBudget::new(1024 * 1024);
        let mut frontier = Frontier {
            queued: BTreeMap::new(),
            bytes: 0,
            budget: Vec::new(),
            governor,
            spill: Some(FrontierSpill::new(temp.path(), disk.clone()).unwrap()),
            fixed_limit: None,
        };
        let path = temp.path().join("étoile-🐇");
        let evidence = NativeEvidence {
            identity: Some(sortsize_core::LocalFileId::WindowsLegacy {
                volume: 42,
                index: 19,
            }),
            created: Some((-9, 13)),
        };
        for id in 1..=1024 {
            frontier
                .push(DirectDirectory {
                    id: EntryId(id),
                    path: path.clone(),
                    evidence: Some(evidence.clone()),
                })
                .unwrap();
        }
        assert!(frontier.queued.is_empty());
        assert!(!frontier.is_empty());
        assert!(disk.used() > 0);
        for id in (1..=1024).rev() {
            frontier.refill().unwrap();
            let value = frontier.take(EntryId(id));
            assert_eq!(value.path, path);
            assert_eq!(value.evidence.as_ref().unwrap().identity, evidence.identity);
            assert_eq!(value.evidence.unwrap().created, evidence.created);
        }
        assert!(frontier.is_empty());
    }

    #[test]
    fn direct_metrics_preserve_phase_sums_maxima_and_native_counters() {
        let finished = Instant::now();
        let started = finished - Duration::from_millis(30);
        let mut sample = WorkerSample {
            dispatched: started - Duration::from_millis(3),
            started,
            finished,
            checks: Timing::default(),
            enumeration: Timing::default(),
            metadata: Timing::default(),
            acquisition: DirectoryMetrics::default(),
            observation_budget_bytes: 1024,
            ntfs: NtfsListingStats {
                batch_calls: 5,
                relative_opens: 7,
                stale_rows: 2,
            },
        };
        sample.checks.add(Duration::from_millis(2));
        sample.enumeration.add(Duration::from_millis(1));
        sample.metadata.add(Duration::from_millis(5));
        sample.metadata.add(Duration::from_millis(7));
        let mut metrics = DirectMetrics::default();
        metrics.completion(&sample, 7);
        metrics.completion(&sample, 7);
        let mut output = Vec::new();
        metrics.write(&mut output, "cancelled").unwrap();
        let output = String::from_utf8(output).unwrap();
        let fields = output.split_whitespace().collect::<Vec<_>>();
        for expected in [
            "outcome=cancelled",
            "completed=2",
            "observations_returned=14",
            "dispatch_wait_ns=6000000",
            "worker_ns=60000000",
            "worker_directory_checks_ns=4000000",
            "worker_enumeration_ns=2000000",
            "worker_metadata_calls=4",
            "worker_metadata_ns=24000000",
            "worker_metadata_max_ns=7000000",
            "ntfs_batch_calls=10",
            "ntfs_relative_opens=14",
            "ntfs_stale_rows=4",
        ] {
            assert!(fields.contains(&expected), "missing {expected}: {output}");
        }
    }

    #[test]
    fn direct_does_not_scan_a_replaced_child() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("old"), b"old").unwrap();
        let guard = Arc::new(Guard::open(temp.path(), Policy::default()).unwrap());
        let mut terminal = false;
        run(
            guard,
            CollectionOptions::default(),
            &Control::default(),
            |event| {
                let batch = event.batch;
                if batch.parent == EntryId(0) && !batch.observations.is_empty() {
                    assert_eq!(batch.observations.len(), 1);
                    assert_eq!(batch.observations[0].kind, Kind::Directory);
                    assert!(batch.evidence[0].as_ref().unwrap().identity.is_some());
                    fs::rename(&child, temp.path().join("retired")).unwrap();
                    fs::create_dir(&child).unwrap();
                    fs::write(child.join("new"), b"new").unwrap();
                    return Ok(vec![ObservedDirectory {
                        id: EntryId(1),
                        name: batch.observations[0].name.clone(),
                        evidence: batch.evidence[0].clone(),
                    }]);
                }
                if batch.parent == EntryId(1) {
                    assert_eq!(batch.sequence, 0);
                    assert_eq!(batch.end, Some(ListingEnd::Incomplete));
                    assert!(batch.detached);
                    assert_eq!(batch.errors, 1);
                    assert!(batch.observations.is_empty());
                    terminal = true;
                }
                Ok(Vec::new())
            },
        )
        .unwrap();
        assert!(terminal);
    }

    #[test]
    fn direct_cancellation_releases_the_native_root() {
        let temp = tempfile::tempdir().unwrap();
        for d in 0..8 {
            let folder = temp.path().join(format!("directory-{d}"));
            fs::create_dir(&folder).unwrap();
            for i in 0..40 {
                fs::write(folder.join(format!("file-{i}")), b"content").unwrap();
            }
        }
        let guard = Arc::new(Guard::open(temp.path(), Policy::default()).unwrap());
        let control = Control::default();
        let mut next_id = 1;
        let mut child_batches = 0;
        let result = run(
            guard.clone(),
            CollectionOptions::default(),
            &control,
            |event| {
                let batch = event.batch;
                assert!(batch.observations.len() <= QUANTUM);
                if batch.parent == EntryId(0) {
                    return Ok(batch
                        .observations
                        .iter()
                        .zip(batch.evidence)
                        .map(|(row, evidence)| {
                            let child = ObservedDirectory {
                                id: EntryId(next_id),
                                name: row.name.clone(),
                                evidence: evidence.clone(),
                            };
                            next_id += 1;
                            child
                        })
                        .collect());
                }
                child_batches += 1;
                control.cancel();
                Ok(Vec::new())
            },
        );
        assert!(matches!(result, Err(Error::Io(e)) if e.kind() == io::ErrorKind::Interrupted));
        assert_eq!(child_batches, 1);
        assert!(active::acquire(&guard).is_ok());
    }
}
