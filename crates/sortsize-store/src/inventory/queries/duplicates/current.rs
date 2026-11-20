//! Guarded, bounded metadata input for an explicit scan of the current folder.
use super::*;
use crate::{
    inventory::differential::NativeEvidence,
    scheduler::{AcquisitionError, direct, native_name},
};
use sortsize_core::{
    acquisition::{ScanObservation, SourceClass, local_state_location},
    resources::native_free_disk,
};
use std::{
    path::Component,
    sync::atomic::{AtomicU64, Ordering},
};

const MIB: u64 = 1024 * 1024;
const QUERY_MEMORY: u64 = 2 * MIB;
const SQLITE_MIN: u64 = MIB + MIB / 2;
const TRANSPORT_MIN: u64 = MIB + MIB / 2;
const TRANSPORT_MAX: u64 = 36 * MIB;
pub const CURRENT_FOLDER_PUBLICATION_PAGE_BYTES: usize = MIB as usize;
const PUBLICATION_MEMORY: u64 = 3 * MIB;
const MINIMUM_MEMORY: u64 = QUERY_MEMORY + SQLITE_MIN + TRANSPORT_MIN + PUBLICATION_MEMORY;

#[derive(Clone, Debug, Default)]
pub struct CurrentFolderResources {
    /// Opt-in ceiling on the working capacities charged to this operation.
    pub memory_limit: Option<u64>,
    /// Opt-in upper bound on the combined private workspace.
    pub temporary_limit: Option<u64>,
    /// Existing, local parent. None selects the operating system temporary parent.
    pub temporary_parent: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateResourceCause {
    Memory,
    TemporaryLimit,
    TemporarySpace,
    TemporaryAccess,
    TemporaryPathEncoding,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct CurrentFolderResourceUsage {
    pub memory_limit: Option<u64>,
    /// Peak of the admitted capacities at preparation. The transport portion
    /// is returned as soon as discovery drains its workers.
    pub memory_reserved: u64,
    pub memory_available: u64,
    pub sqlite_cache: u64,
    pub publication_reserved: u64,
    pub discovery_workers: usize,
    pub temporary_requested: Option<u64>,
    pub temporary_limit: u64,
    pub temporary_charged: u64,
    pub temporary_peak: u64,
    pub temporary_free: Option<u64>,
    pub temporary_reserve: u64,
    pub cause: Option<DuplicateResourceCause>,
}

impl CurrentFolderResourceUsage {
    pub(super) fn refresh(&mut self, space: &workspace::Workspace) {
        let usage = space.usage();
        self.temporary_charged = usage.charged;
        self.temporary_peak = usage.peak;
        let (free, cause) = space.adaptive_observation();
        self.temporary_free = free;
        self.cause = cause.or(self.cause);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AcquisitionProgress {
    pub observations: u64,
    pub examined_files: u64,
    pub excluded_by_size: u64,
    pub excluded_empty: u64,
    pub selected_files: u64,
    pub selected_logical_bytes: u128,
    pub errors: u64,
    pub resources: Option<CurrentFolderResourceUsage>,
}

pub(super) struct Acquisition {
    pub guard: Arc<Guard>,
    pub source: SourceId,
    pub generation: GenerationId,
    pub complete: bool,
    pub errors: u64,
    issues: Vec<AcquisitionError>,
}

impl Acquisition {
    pub fn diagnostics(&self) -> impl Iterator<Item = (usize, String)> + '_ {
        self.issues.iter().map(|issue| {
            let error = issue
                .raw_os_error
                .map_or_else(|| io::Error::from(issue.kind), io::Error::from_raw_os_error);
            let reason = format!("{:?}: {} — {error}", issue.operation, issue.path.display());
            (0, reason.chars().take(512).collect())
        })
    }
}

/// All occurrence metadata comes from the strict, guarded acquisition cursor.
/// Only regular files enter this table. The native discovery frontier and SQL
/// results live under one private disk quota. This call never opens file content.
/// Keep a clone of `guard` and pass that same guard to verification. Reopening
/// the selected path could authorize a replacement root or different exclusions.
pub fn prepare_current_folder(
    guard: Arc<Guard>,
    scratch: &Path,
    max_db_bytes: u64,
    cancel: &CancellationToken,
    mut progress: impl FnMut(&AcquisitionProgress),
) -> Result<DuplicateJob<'static, 'static>> {
    prepare(guard, scratch, max_db_bytes, cancel, &mut progress).map_err(|error| {
        if is_temporary_limit(&error) {
            Error::DuplicateTemporaryLimit {
                limit: max_db_bytes,
            }
        } else {
            error
        }
    })
}

fn prepare(
    guard: Arc<Guard>,
    scratch: &Path,
    max_db_bytes: u64,
    cancel: &CancellationToken,
    progress: &mut impl FnMut(&AcquisitionProgress),
) -> Result<DuplicateJob<'static, 'static>> {
    check_cancel(cancel)?;
    if max_db_bytes < 64 * 1024 {
        return Err(Error::Contract("duplicate disk budget"));
    }
    check_guard(&guard)?;
    let scratch = scratch.canonicalize()?;
    if !guard.work_path_allowed(&scratch)? {
        return Err(Error::Contract(
            "duplicate scratch overlaps selected source",
        ));
    }
    let governor = Governor::global();
    const MEMORY: u64 = 1024 * 1024;
    let memory = governor
        .reserve(Category::Queries, MEMORY, None)
        .ok_or_else(|| Error::memory_admission(&governor, Category::Queries, MEMORY))?;
    let space = workspace::Workspace::current_folder(&scratch, max_db_bytes)?;
    if !guard.work_path_allowed(space.path())? {
        return Err(Error::Contract(
            "duplicate scratch overlaps selected source",
        ));
    }
    prepare_in_space(
        guard,
        &space,
        PreparationMemory {
            query: memory,
            sqlite: None,
            transport: None,
            publication: None,
        },
        None,
        cancel,
        progress,
        CandidateDiscovery::default(),
    )
}

struct PreparationMemory {
    query: Reservation,
    sqlite: Option<Reservation>,
    transport: Option<direct::CandidateTransport>,
    publication: Option<Reservation>,
}

/// Uses an adaptive private workspace while keeping the same strict candidate
/// acquisition. Resources and source authorization are independent.
pub fn prepare_current_folder_with_resources(
    guard: Arc<Guard>,
    resources: &CurrentFolderResources,
    cancel: &CancellationToken,
    progress: impl FnMut(&AcquisitionProgress),
) -> Result<DuplicateJob<'static, 'static>> {
    prepare_adaptive(guard, resources, None, cancel, progress)
}

/// Prepare an explicit file search, retaining the strict discovery and
/// attachment checks while omitting out-of-profile candidates from SQLite.
pub fn prepare_current_folder_with_profile(
    guard: Arc<Guard>,
    resources: &CurrentFolderResources,
    profile: DuplicateFileProfile,
    cancel: &CancellationToken,
    progress: impl FnMut(&AcquisitionProgress),
) -> Result<DuplicateJob<'static, 'static>> {
    prepare_adaptive(guard, resources, Some(profile), cancel, progress)
}

fn prepare_adaptive(
    guard: Arc<Guard>,
    resources: &CurrentFolderResources,
    profile: Option<DuplicateFileProfile>,
    cancel: &CancellationToken,
    mut progress: impl FnMut(&AcquisitionProgress),
) -> Result<DuplicateJob<'static, 'static>> {
    check_cancel(cancel)?;
    check_guard(&guard)?;
    let (space, memory, mut usage) = admit_resources(resources, cancel)?;
    let result = prepare_in_space(
        guard,
        &space,
        memory,
        Some(usage),
        cancel,
        &mut progress,
        CandidateDiscovery {
            profile,
            synthetic: None,
        },
    );
    result.map_err(|error| {
        usage.refresh(&space);
        if let Some(cause) = usage.cause {
            resource_error(usage, cause)
        } else if is_temporary_limit(&error) {
            resource_error(usage, native_resource_cause(&error))
        } else if matches!(
            &error,
            Error::Sql(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(failure.code,
                    rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::PermissionDenied
                    | rusqlite::ErrorCode::SystemIoFailure)
        ) {
            resource_error(usage, DuplicateResourceCause::TemporaryAccess)
        } else {
            error
        }
    })
}

fn resource_error(mut usage: CurrentFolderResourceUsage, cause: DuplicateResourceCause) -> Error {
    usage.cause = Some(cause);
    Error::DuplicateResources(Box::new(usage))
}

fn admit_resources(
    resources: &CurrentFolderResources,
    cancel: &CancellationToken,
) -> Result<(
    Arc<workspace::Workspace>,
    PreparationMemory,
    CurrentFolderResourceUsage,
)> {
    let mut usage = CurrentFolderResourceUsage {
        memory_limit: resources.memory_limit,
        temporary_requested: resources.temporary_limit,
        ..Default::default()
    };
    let parent = resources
        .temporary_parent
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let location = (|| -> Result<PathBuf> {
        // Refuse reparses and remote mounts before following path syntax.
        local_state_location(&parent)?;
        let path = parent.canonicalize()?;
        local_state_location(&path)?;
        Ok(path)
    })()
    .map_err(|error| resource_error(usage, native_resource_cause(&error)))?;
    // SQLite's Windows API requires UTF-8 for its own temporary paths.
    // Candidate/source paths never go through that API.
    if location.to_str().is_none() || location.as_os_str().as_encoded_bytes().len() > 16 * 1024 {
        return Err(resource_error(
            usage,
            DuplicateResourceCause::TemporaryPathEncoding,
        ));
    }
    let free = native_free_disk(&location)
        .ok_or_else(|| resource_error(usage, DuplicateResourceCause::TemporaryAccess))?;
    let policy = workspace::adaptive::SpacePolicy::new(free, resources.temporary_limit);
    usage.temporary_limit = policy.limit;
    usage.temporary_free = Some(free);
    usage.temporary_reserve = policy.reserve;
    if policy.limit < workspace::adaptive::MINIMUM {
        let cause = if resources
            .temporary_limit
            .is_some_and(|limit| limit < workspace::adaptive::MINIMUM)
        {
            DuplicateResourceCause::TemporaryLimit
        } else {
            DuplicateResourceCause::TemporarySpace
        };
        return Err(resource_error(usage, cause));
    }
    check_cancel(cancel)?;
    let governor = Governor::global();
    governor.refresh_native();
    let limit = resources.memory_limit.unwrap_or(u64::MAX);
    let extra = limit.saturating_sub(MINIMUM_MEMORY);
    let (available, memory) = governor.reserve_partition(
        limit,
        [
            (Category::Queries, QUERY_MEMORY, QUERY_MEMORY),
            (
                Category::Publications,
                PUBLICATION_MEMORY,
                PUBLICATION_MEMORY,
            ),
            (
                Category::Transport,
                TRANSPORT_MIN,
                (TRANSPORT_MIN + extra / 2).min(TRANSPORT_MAX),
            ),
            (Category::Sqlite, SQLITE_MIN, 64 * MIB),
        ],
    );
    usage.memory_available = available;
    let [query, publication, transport, sqlite] =
        memory.ok_or_else(|| resource_error(usage, DuplicateResourceCause::Memory))?;
    usage.memory_reserved =
        query.bytes() + publication.bytes() + transport.bytes() + sqlite.bytes();
    usage.publication_reserved = publication.bytes();
    usage.sqlite_cache = sqlite.bytes().saturating_sub(MIB) / 1024 * 1024;
    // Every native worker receives one 512 KiB accounting allowance besides
    // coordinator/cursor overhead. Unused transport becomes spillable frontier.
    usage.discovery_workers = (1 + (transport.bytes() - TRANSPORT_MIN) / (MIB / 2)).min(8) as usize;
    let frontier_bytes =
        transport.bytes() - (MIB / 2 + u64::try_from(usage.discovery_workers).unwrap() * (MIB / 2));
    let memory = PreparationMemory {
        query,
        sqlite: Some(sqlite),
        publication: Some(publication),
        transport: Some(direct::CandidateTransport {
            memory: transport,
            frontier_bytes,
            workers: usage.discovery_workers,
        }),
    };
    let space = workspace::Workspace::current_folder_adaptive(&location, free, policy)
        .map_err(|error| resource_error(usage, native_resource_cause(&error)))?;
    Ok((space, memory, usage))
}

pub(super) fn native_resource_cause(error: &Error) -> DuplicateResourceCause {
    if matches!(error, Error::Io(io) if matches!(
        io.kind(), io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )) {
        DuplicateResourceCause::TemporarySpace
    } else if is_temporary_limit(error) {
        DuplicateResourceCause::TemporaryLimit
    } else {
        DuplicateResourceCause::TemporaryAccess
    }
}

fn check_guard(guard: &Guard) -> Result<()> {
    if guard.scan_observation() != ScanObservation::Strict
        || guard.evidence().0.class != SourceClass::Local
        || !guard.policy().same_volume
    {
        return Err(Error::Contract(
            "current folder requires a strict local source guard",
        ));
    }
    if workspace::adaptive::is_active_stamp(guard.evidence().1) {
        return Err(Error::Contract(
            "selected source is an active duplicate workspace",
        ));
    }
    check_attachment(guard)?;
    Ok(())
}

#[derive(Default)]
struct CandidateDiscovery<'a> {
    profile: Option<DuplicateFileProfile>,
    synthetic: Option<&'a mut SyntheticDiscovery<'a>>,
}

type SyntheticDiscovery<'a> =
    dyn FnMut(&mut CandidateSink<'_>, &Arc<workspace::Workspace>) -> Result<()> + 'a;

fn prepare_in_space(
    guard: Arc<Guard>,
    space: &Arc<workspace::Workspace>,
    memory: PreparationMemory,
    mut usage: Option<CurrentFolderResourceUsage>,
    cancel: &CancellationToken,
    progress: &mut impl FnMut(&AcquisitionProgress),
    discovery: CandidateDiscovery<'_>,
) -> Result<DuplicateJob<'static, 'static>> {
    let trace = crate::scan_metrics::enabled();
    let started = trace.then(Instant::now);
    let profile = discovery.profile;
    let rank_by_gain = profile.is_some();
    let (db, vfs, owner) = if let Some(usage) = usage {
        open_current_results(space, usage.sqlite_cache, rank_by_gain)?
    } else {
        open_results(space, space.usage().limit, rank_by_gain)?
    };
    // Use a job-local occurrence namespace. Inventory zero denotes an explicit
    // live action. No durable inventory, source registration or history row is made.
    static NEXT_ACQUISITION: AtomicU64 = AtomicU64::new(1);
    let serial = NEXT_ACQUISITION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| Error::Contract("duplicate acquisition id exhausted"))?;
    let source = SourceId(u128::from(serial));
    let generation = GenerationId(serial);
    let discovery_started = trace.then(Instant::now);
    let mut ingestion_ns = 0;
    let (summary, acquisition) = {
        let mut sink = CandidateSink::new(&db, source, generation, profile)?;
        if let Some(discover) = discovery.synthetic {
            discover(&mut sink, space)?;
        } else {
            direct::run_candidates(
                guard.clone(),
                cancel,
                space.file()?,
                memory.transport,
                |batch| {
                    let ingesting = trace.then(Instant::now);
                    let children = sink.accept(batch, cancel)?;
                    if let Some(ingesting) = ingesting {
                        ingestion_ns += ingesting.elapsed().as_nanos();
                    }
                    if let Some(usage) = &mut usage {
                        usage.refresh(space);
                    }
                    sink.progress.resources = usage;
                    progress(&sink.progress);
                    Ok(children)
                },
            )?;
        }
        let discovery_ns = discovery_started.map(|start| start.elapsed().as_nanos());
        let finishing = trace.then(Instant::now);
        let observations = sink.progress.observations;
        if let Some(usage) = &mut usage {
            usage.refresh(space);
        }
        sink.progress.resources = usage;
        let (mut summary, acquisition) = sink.finish(guard.clone(), cancel, progress)?;
        if let Some(usage) = &mut summary.resources {
            usage.refresh(space);
        }
        if let Some((discovery_ns, (started, finishing))) = discovery_ns.zip(started.zip(finishing))
        {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "duplicate_candidates preparation_wall_ns={} discovery_including_ingestion_wall_ns={discovery_ns} ingestion_receiver_ns={ingestion_ns} finish_wall_ns={} observations={observations} selected_files={} unknown_sizes={} errors={} acquisition_complete={} temporary_peak_bytes={} examined_files={} excluded_by_size={} excluded_empty={}",
                started.elapsed().as_nanos(),
                finishing.elapsed().as_nanos(),
                summary.selected_files,
                summary.unknown_sizes,
                summary.errors,
                acquisition.complete && summary.errors == 0 && summary.unknown_sizes == 0,
                space.usage().peak,
                summary.examined_files,
                summary.excluded_by_size,
                summary.excluded_empty,
            );
        }
        (summary, acquisition)
    };
    Ok(DuplicateJob {
        scopes: Vec::new(),
        current: Some(acquisition),
        guard_roots: vec![guard.root().to_owned()],
        mode: Mode::WithinSelection,
        rank_by_gain,
        max_errors: 100,
        root_issues: Vec::new(),
        root_warnings: Vec::new(),
        db,
        _vfs: vfs,
        space: space.clone(),
        owner,
        _memory: memory.query,
        _sqlite_memory: memory.sqlite,
        publication_memory: memory.publication.map(Arc::new),
        summary,
    })
}

#[cfg(all(test, windows))]
mod qualification;

pub(super) fn check_attachment(guard: &Guard) -> Result<Attachment> {
    guard.validate_internal_state()?;
    let current = guard.inspect(guard.root())?;
    let attachment = guard.evidence().1.attachment(&current);
    if !current.directory || attachment == Attachment::Different {
        return Err(io::Error::other("The selected folder is no longer attached").into());
    }
    Ok(attachment)
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Io(io::ErrorKind::Interrupted.into()))
    } else {
        Ok(())
    }
}

struct Candidate<'a> {
    path: &'a Path,
    size: u64,
    modified: Option<(i64, u32)>,
    evidence: Option<&'a NativeEvidence>,
}

struct CandidateSink<'db> {
    db: &'db Connection,
    insert: rusqlite::Statement<'db>,
    tx: Option<rusqlite::Transaction<'db>>,
    source: SourceId,
    generation: GenerationId,
    serial: u64,
    batch: usize,
    pending: u64,
    complete: bool,
    profile: Option<DuplicateFileProfile>,
    progress: AcquisitionProgress,
    unknown_sizes: u64,
    issues: Vec<AcquisitionError>,
}

impl<'db> CandidateSink<'db> {
    fn new(
        db: &'db Connection,
        source: SourceId,
        generation: GenerationId,
        profile: Option<DuplicateFileProfile>,
    ) -> Result<Self> {
        Ok(Self {
            db,
            insert: db.prepare(
                "INSERT INTO candidates(id,scope,guard,side,source,inventory,generation,entry,path,size,modified_s,modified_ns,acquired)
                 VALUES(?1,0,0,0,?2,zeroblob(16),?3,?1,?4,?5,?6,?7,?8) ON CONFLICT(path) DO NOTHING",
            )?,
            tx: Some(db.unchecked_transaction()?),
            source,
            generation,
            serial: 0,
            batch: 0,
            pending: 1,
            complete: true,
            profile,
            progress: AcquisitionProgress::default(),
            unknown_sizes: 0,
            issues: Vec::new(),
        })
    }

    fn next_id(&mut self) -> Result<EntryId> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(Error::Contract("duplicate occurrence id exhausted"))?;
        Ok(EntryId(self.serial))
    }

    fn issue(&mut self, issue: AcquisitionError) {
        // Bound the sample without losing the separate count. Oversized paths
        // are never retained in diagnostic memory.
        if self.issues.len() < 32 && issue.path.as_os_str().as_encoded_bytes().len() <= 4096 {
            self.issues.push(issue);
        }
    }

    fn accept(
        &mut self,
        event: direct::DirectBatch<'_>,
        cancel: &CancellationToken,
    ) -> Result<Vec<direct::ObservedDirectory>> {
        let batch = event.batch;
        self.progress.errors += batch.errors;
        if let Some(error) = event.last_error {
            self.issue(error.clone());
        }
        if batch.end.is_some() {
            self.pending = self
                .pending
                .checked_sub(1)
                .ok_or(Error::Contract("duplicate directory completion"))?;
        }
        if batch.end == Some(ListingEnd::Incomplete) || batch.detached {
            self.complete = false;
        }
        let mut children = Vec::new();
        for (row, evidence) in batch.observations.iter().zip(batch.evidence) {
            check_cancel(cancel)?;
            self.progress.observations += 1;
            match row.kind {
                Kind::Directory => {
                    if !batch.detached {
                        children.push(direct::ObservedDirectory {
                            id: self.next_id()?,
                            name: row.name.clone(),
                            evidence: evidence.clone(),
                        });
                        self.pending += 1;
                    }
                }
                Kind::Unknown => self.unknown_sizes += 1,
                Kind::File => {
                    self.progress.examined_files += 1;
                    let Some(size) = row.logical.filter(|_| row.metadata_known) else {
                        self.progress.selected_files += u64::from(self.profile.is_none());
                        self.unknown_sizes += 1;
                        continue;
                    };
                    if self.profile.is_some_and(|profile| {
                        profile.excludes(
                            size,
                            &mut self.progress.excluded_by_size,
                            &mut self.progress.excluded_empty,
                        )
                    }) {
                        continue;
                    }
                    let name = native_name(&row.name)?;
                    let mut components = Path::new(&name).components();
                    if !matches!(components.next(), Some(Component::Normal(_)))
                        || components.next().is_some()
                    {
                        return Err(Error::Contract("invalid duplicate candidate name"));
                    }
                    let path = event.parent_path.join(name);
                    if path.parent() != Some(event.parent_path) {
                        return Err(Error::Contract("invalid duplicate candidate scope"));
                    }
                    self.insert(Candidate {
                        path: &path,
                        size,
                        modified: row.modified,
                        evidence: evidence.as_ref(),
                    })?;
                }
                Kind::Symlink | Kind::Special => (),
            }
        }
        Ok(children)
    }

    fn insert(&mut self, candidate: Candidate<'_>) -> Result<()> {
        let id = self.next_id()?.0;
        let evidence = candidate
            .evidence
            .map_or_else(Vec::new, NativeEvidence::encode_occurrence);
        let (modified_s, modified_ns) = candidate.modified.unzip();
        let inserted = self.insert.execute(params![
            unsigned(id),
            self.source.0.to_be_bytes().as_slice(),
            unsigned(self.generation.0),
            native_path(candidate.path).encode(),
            unsigned(candidate.size),
            modified_s,
            modified_ns,
            &evidence,
        ])?;
        if inserted == 0 || evidence.is_empty() {
            self.progress.errors += 1;
            self.issue(AcquisitionError {
                path: candidate.path.to_owned(),
                operation: crate::scheduler::AcquisitionOperation::Metadata,
                kind: io::ErrorKind::InvalidData,
                raw_os_error: None,
            });
        }
        self.progress.selected_files += inserted as u64;
        if inserted != 0 {
            self.progress.selected_logical_bytes += u128::from(candidate.size);
        }
        self.batch += 1;
        if self.batch == 512 {
            self.tx.take().unwrap().commit()?;
            self.tx = Some(self.db.unchecked_transaction()?);
            self.batch = 0;
        }
        Ok(())
    }

    fn finish(
        mut self,
        guard: Arc<Guard>,
        cancel: &CancellationToken,
        progress: &mut impl FnMut(&AcquisitionProgress),
    ) -> Result<(Summary, Acquisition)> {
        check_cancel(cancel)?;
        self.tx.take().unwrap().commit()?;
        let attached = check_attachment(&guard)?;
        check_cancel(cancel)?;
        progress(&self.progress);
        check_cancel(cancel)?;
        Ok((
            Summary {
                selected_files: self.progress.selected_files,
                examined_files: self.progress.examined_files,
                excluded_by_size: self.progress.excluded_by_size,
                excluded_empty: self.progress.excluded_empty,
                selected_logical_bytes: self.progress.selected_logical_bytes,
                unknown_sizes: self.unknown_sizes,
                errors: self.progress.errors,
                resources: self.progress.resources,
                ..Summary::default()
            },
            Acquisition {
                guard,
                source: self.source,
                generation: self.generation,
                complete: self.complete && self.pending == 0 && attached == Attachment::Compatible,
                errors: self.progress.errors,
                issues: self.issues,
            },
        ))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use sortsize_core::acquisition::Policy;
    use std::fs;

    #[test]
    fn current_folder_scratch_requires_a_proven_exclusion_inside_the_source() {
        let source = tempfile::tempdir().unwrap();
        let state = source.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("existing-state"), b"content").unwrap();
        fs::write(source.path().join("candidate"), b"content").unwrap();
        let guard = Guard::open(source.path(), Policy::default()).unwrap();
        let cancel = CancellationToken::default();
        let disk_limit = 1024 * 1024;

        let denied = prepare_current_folder(Arc::new(guard), &state, disk_limit, &cancel, |_| {});
        assert!(matches!(
            denied,
            Err(Error::Contract(
                "duplicate scratch overlaps selected source"
            ))
        ));
        assert_eq!(fs::read_dir(&state).unwrap().count(), 1);

        let guard = Guard::open(source.path(), Policy::default())
            .unwrap()
            .with_internal_state(Guard::open(&state, Policy::default()).unwrap())
            .unwrap();
        let mut progress = AcquisitionProgress::default();
        let job = prepare_current_folder(Arc::new(guard), &state, disk_limit, &cancel, |update| {
            progress = *update;
        })
        .unwrap();
        assert_eq!(progress.errors, 0);
        assert_eq!(progress.observations, 1);
        assert_eq!(job.summary().selected_files, 1);
        assert_eq!(job.candidate_count().unwrap(), 1);
        drop(job);
        assert_eq!(fs::read_dir(&state).unwrap().count(), 1);
    }

    #[test]
    fn adaptive_workspaces_are_excluded_by_identity_even_for_general_acquisition() {
        let source = tempfile::tempdir().unwrap();
        let old = source.path().join(".bunny-derived-retained");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("original"), b"content").unwrap();
        fs::write(source.path().join("candidate"), b"content").unwrap();
        let cancel = CancellationToken::default();
        let resources = CurrentFolderResources {
            memory_limit: Some(8 * MIB),
            temporary_limit: Some(MIB),
            temporary_parent: Some(source.path().to_owned()),
        };
        let (peer, memory, _) = admit_resources(&resources, &cancel).unwrap();
        drop(memory);
        let foreign = peer.path().join("foreign");
        fs::write(&foreign, b"unexpected").unwrap();
        let guard = Arc::new(Guard::open(source.path(), Policy::default()).unwrap());
        let job = prepare_current_folder_with_resources(guard.clone(), &resources, &cancel, |_| {})
            .unwrap();
        assert_eq!(job.candidate_count().unwrap(), 2);
        assert_eq!(job.summary().errors, 0);
        let mut names = Vec::new();
        direct::run(
            guard,
            crate::scheduler::CollectionOptions::default(),
            &crate::scheduler::Control::default(),
            |event| {
                names.extend(
                    event
                        .batch
                        .observations
                        .iter()
                        .map(|row| native_name(&row.name).unwrap()),
                );
                Ok(Vec::new())
            },
        )
        .unwrap();
        names.sort();
        assert_eq!(
            names,
            [
                std::ffi::OsString::from(".bunny-derived-retained"),
                "candidate".into()
            ]
        );
        drop(job);
        drop(peer);
        assert_eq!(fs::read(foreign).unwrap(), b"unexpected");
        assert_eq!(fs::read(old.join("original")).unwrap(), b"content");
    }

    #[test]
    fn a_foreign_or_replaced_database_is_never_opened_for_write_or_removed() {
        let root = tempfile::tempdir().unwrap();
        let resources = CurrentFolderResources {
            memory_limit: Some(8 * MIB),
            temporary_limit: Some(MIB),
            temporary_parent: Some(root.path().to_owned()),
        };
        let cancel = CancellationToken::default();
        let (space, _memory, usage) = admit_resources(&resources, &cancel).unwrap();
        let existing = space.path().join("duplicates.db");
        let original = root.path().join("original");
        fs::write(&original, b"foreign file").unwrap();
        fs::hard_link(&original, &existing).unwrap();
        assert!(open_current_results(&space, usage.sqlite_cache, false).is_err());
        drop(space);
        assert_eq!(fs::read(existing).unwrap(), b"foreign file");
        assert_eq!(fs::read(original).unwrap(), b"foreign file");

        let (space, _second_memory, usage) = admit_resources(&resources, &cancel).unwrap();
        let (db, vfs, owner) = open_current_results(&space, usage.sqlite_cache, false).unwrap();
        drop(db);
        drop(vfs);
        let moved = space.path().join("moved");
        fs::rename(&owner, &moved).unwrap();
        fs::write(&owner, b"foreign replacement").unwrap();
        drop(space);
        assert_eq!(fs::read(owner).unwrap(), b"foreign replacement");
        assert!(moved.is_file());
    }
}
