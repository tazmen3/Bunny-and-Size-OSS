//! V0.5 service-side scheduler. `tick` belongs on the service thread, never UI.
//! One process-wide pool, durable L02 queue, retained direct-listing cursors.
use crate::{
    codec::NativeName,
    inventory::{
        DirectoryBatch, Error, Kind, Observation, Result, Store, differential::NativeEvidence,
    },
};
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, NtfsListingStats, ScanDirectory, ScanObservation, SourceClass},
    inventory_contract::*,
};
#[cfg(test)]
use std::fs;
use std::{
    collections::{HashMap, VecDeque},
    ffi::{OsStr, OsString},
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

mod active;
pub mod direct;
mod metrics;
mod recovery;
use metrics::{Metrics, Timing, WorkerSample};
mod trace;
pub(crate) use recovery::{recorded_scope_path, session_proof, workspace_identity};
pub use trace::{ProgressSnapshot, ProgressTrace, WorkerPhase, WorkerProgress, WriterPhase};
/// Durable provenance for scans allowed to use directory-index observations.
pub const DIRECTORY_LISTING_POLICY: PolicyId = PolicyId(2);

pub fn observation_policy(observation: ScanObservation) -> PolicyId {
    match observation {
        ScanObservation::Strict => PolicyId(1),
        ScanObservation::DirectoryListing => DIRECTORY_LISTING_POLICY,
    }
}

pub fn policy_observation(policy: PolicyId) -> ScanObservation {
    if policy == DIRECTORY_LISTING_POLICY {
        ScanObservation::DirectoryListing
    } else {
        ScanObservation::Strict
    }
}
// A full 255-unit Windows name encodes to 512 bytes. Sixty-four such
// observations, including the conservative 384-byte metadata allowance,
// still fit the existing 64 KiB payload. Keep the byte and time guards:
// this increases useful work per durable commit, not admission capacity.
const QUANTUM: usize = 64;
const BATCH_BYTES: usize = 64 * 1024;
const MAX_PATH: usize = 32 * 1024;
// Return to the service control lane after at most one full set of permits.
// An atomic group never exceeds eight quanta / 512 KiB of bounded payloads.
const COMMIT_QUANTA: usize = 8;
const ROTATION: [usize; 8] = [0, 0, 1, 0, 2, 1, 0, 3];
#[derive(Default)]
pub struct Fairness {
    next: usize,
}
impl Fairness {
    pub fn choose(&mut self, eligible: [bool; 4]) -> Option<usize> {
        for _ in 0..8 {
            let p = ROTATION[self.next];
            self.next = (self.next + 1) % 8;
            if eligible[p] {
                return Some(p);
            }
        }
        None
    }
}
#[derive(Default)]
struct Intent {
    navigation: Option<EntryId>,
    navigation_relative: Option<PathBuf>,
    visible: Option<EntryId>,
    enrichment: Option<EntryId>,
    paused: bool,
    cancelled: bool,
}
impl Intent {
    fn allows_background_pair(&self) -> bool {
        !self.paused
            && !self.cancelled
            && self.navigation.is_none()
            && self.navigation_relative.is_none()
            && self.visible.is_none()
            && self.enrichment.is_none()
    }
}
/// Independent coalesced control lane. No observation is replaced by a click.
#[derive(Clone, Default)]
pub struct Control(Arc<Mutex<Intent>>, Option<CancellationToken>);
impl Control {
    pub(crate) fn with_cancellation(cancel: &CancellationToken) -> Self {
        Self(Arc::default(), Some(cancel.clone()))
    }
    pub fn navigate(&self, id: EntryId) {
        let mut intent = self.0.lock().unwrap();
        intent.navigation = Some(id);
        intent.navigation_relative = None;
    }
    /// Native relative path from another generation of the same source. The
    /// service resolves it against its candidate; IDs never cross generations.
    /// No filesystem or database operation occurs on the caller's thread.
    pub fn navigate_relative(&self, path: PathBuf) -> Result<()> {
        validate_navigation_path(&path)?;
        let mut intent = self.0.lock().unwrap();
        intent.navigation = None;
        intent.navigation_relative = Some(path);
        Ok(())
    }
    pub fn visible(&self, id: Option<EntryId>) {
        self.0.lock().unwrap().visible = id;
    }
    pub fn enrich(&self, id: Option<EntryId>) {
        self.0.lock().unwrap().enrichment = id;
    }
    pub fn pause(&self, value: bool) {
        self.0.lock().unwrap().paused = value;
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.external_cancelled() || self.0.lock().unwrap().cancelled
    }
    fn external_cancelled(&self) -> bool {
        self.1.as_ref().is_some_and(CancellationToken::is_cancelled)
    }
    pub fn cancel(&self) {
        self.0.lock().unwrap().cancelled = true;
    }
    fn allows_background_pair(&self) -> bool {
        self.0.lock().unwrap().allows_background_pair()
    }
}
pub(crate) fn validate_navigation_path(path: &std::path::Path) -> Result<()> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_PATH
        || path.components().count() > 1024
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(Error::Contract(
            "navigation relative path budget or component",
        ));
    }
    Ok(())
}
struct Permit(&'static AtomicUsize);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
fn reserve(counter: &'static AtomicUsize, limit: usize) -> Option<Permit> {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            (v < limit).then_some(v + 1)
        })
        .ok()
        .map(|_| Permit(counter))
}
static ACQUISITIONS: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND_ACQUISITIONS: AtomicUsize = AtomicUsize::new(0);
static CURSORS: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND_CURSORS: AtomicUsize = AtomicUsize::new(0);
struct CursorLease {
    _total: Permit,
    _background: Option<Permit>,
}
struct AcquisitionLease {
    _total: Permit,
    _background: Option<Permit>,
}
fn acquisition_lease(interactive: bool, navigation_active: bool) -> Option<AcquisitionLease> {
    let background = if interactive {
        None
    } else {
        // Reduce the background share only while an interactive listing needs
        // work. Completed selections restore all eight slots automatically.
        Some(reserve(
            &BACKGROUND_ACQUISITIONS,
            if navigation_active { 6 } else { 8 },
        )?)
    };
    Some(AcquisitionLease {
        _total: reserve(&ACQUISITIONS, 8)?,
        _background: background,
    })
}
fn cursor_lease(interactive: bool) -> Option<CursorLease> {
    let background = if interactive {
        None
    } else {
        Some(reserve(&BACKGROUND_CURSORS, 14)?)
    };
    Some(CursorLease {
        _total: reserve(&CURSORS, 16)?,
        _background: background,
    })
}
struct Cursor {
    attempt: AttemptKey,
    path: PathBuf,
    read: Option<ScanDirectory>,
    snapshot: Option<sortsize_core::acquisition::Stamp>,
    sequence: u64,
    _lease: CursorLease,
}
struct Admission {
    cursor: Cursor,
    permit: AcquisitionLease,
}
/// Last native acquisition failure, bounded to one diagnostic per session.
/// This is a local diagnostic only; it is never logged or sent automatically.
#[derive(Clone, Debug)]
pub struct AcquisitionError {
    pub path: PathBuf,
    pub operation: AcquisitionOperation,
    pub kind: io::ErrorKind,
    pub raw_os_error: Option<i32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquisitionOperation {
    OpenDirectory,
    ReadDirectory,
    Metadata,
    ValidateDirectory,
}
#[derive(Default)]
struct AcquisitionErrors {
    count: u64,
    denied: u64,
    last: Option<AcquisitionError>,
}
impl AcquisitionErrors {
    fn record(
        &mut self,
        path: &std::path::Path,
        operation: AcquisitionOperation,
        error: &io::Error,
    ) {
        self.count = self.count.saturating_add(1);
        if error.kind() == io::ErrorKind::PermissionDenied {
            self.denied = self.denied.saturating_add(1);
        }
        self.last = Some(AcquisitionError {
            path: path.to_owned(),
            operation,
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
        });
    }
}
struct Completed {
    cursor: Cursor,
    observations: Vec<Observation>,
    entries_seen: u64,
    evidence: Vec<Option<crate::inventory::differential::NativeEvidence>>,
    properties: Vec<sortsize_core::acquisition::NodeProperties>,
    end: Option<ListingEnd>,
    directory: Option<sortsize_core::acquisition::Stamp>,
    detached: bool,
    errors: AcquisitionErrors,
    sample: Option<WorkerSample>,
    _permit: AcquisitionLease,
    _pending: Option<CompletionLease>,
    _session: Option<SessionLease>,
    _progress_job: Option<trace::JobTrace>,
}
impl Completed {
    fn batch(&self) -> DirectoryBatch<'_> {
        DirectoryBatch {
            header: BatchHeader {
                attempt: self.cursor.attempt,
                sequence: self.cursor.sequence,
                end: self.end,
            },
            observations: &self.observations,
            evidence: &self.evidence,
            properties: &self.properties,
            directory: self.directory.as_ref(),
        }
    }

    fn next_sequence(&self) -> Result<u64> {
        self.cursor
            .sequence
            .checked_add(1)
            .ok_or(Error::Contract("sequence exhausted"))
    }
}
struct SessionLease {
    _memory: Arc<sortsize_core::resources::Reservation>,
    _active: Option<Arc<active::ActiveRoot>>,
}
// A returned quantum remains accounted while queued, retained for retry, or
// being committed. RAII also covers a failed send or Session destruction.
struct CompletionLease(Arc<AtomicUsize>);
impl CompletionLease {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}
impl Drop for CompletionLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Pool {
    send: SyncSender<Box<dyn FnOnce() + Send>>,
}
fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let (send, receive) = mpsc::sync_channel::<Box<dyn FnOnce() + Send>>(8);
        let receive = Arc::new(Mutex::new(receive));
        for i in 0..8 {
            let r = receive.clone();
            std::thread::Builder::new()
                .name(format!("v05-acquire-{i}"))
                .spawn(move || {
                    trace::set_slot(i);
                    loop {
                        let job = match r.lock().unwrap().recv() {
                            Ok(j) => j,
                            Err(_) => break,
                        };
                        job();
                    }
                })
                .expect("acquisition worker");
        }
        Pool { send }
    })
}
#[derive(Clone, Copy, Debug)]
pub struct CollectionOptions {
    pub allocation: bool,
    pub owners: bool,
}
impl Default for CollectionOptions {
    fn default() -> Self {
        Self {
            allocation: true,
            owners: false,
        }
    }
}
fn quantum(
    guard: &Guard,
    cursor: Cursor,
    permit: AcquisitionLease,
    options: CollectionOptions,
    control: &Control,
    dispatched: Option<Instant>,
) -> Completed {
    quantum_with_origin(guard, cursor, permit, options, control, dispatched, None)
}
fn quantum_with_origin(
    guard: &Guard,
    mut cursor: Cursor,
    permit: AcquisitionLease,
    options: CollectionOptions,
    control: &Control,
    dispatched: Option<Instant>,
    origin: Option<&NativeEvidence>,
) -> Completed {
    let start = Instant::now();
    let instrument = dispatched.is_some();
    let progress_worker = trace::current_worker();
    let acquisition_profile = sortsize_core::acquisition::profile::Profile::start(instrument);
    let ntfs_before = instrument.then(|| {
        cursor
            .read
            .as_ref()
            .map(ScanDirectory::ntfs_stats)
            .unwrap_or_default()
    });
    let mut checks = Timing::default();
    let mut enumeration = Timing::default();
    let mut metadata = Timing::default();
    let mut evidence = Vec::with_capacity(QUANTUM);
    let mut properties = Vec::with_capacity(QUANTUM);
    let mut observations = Vec::with_capacity(QUANTUM);
    let mut attached = true;
    let mut detached = false;
    let mut end = None;
    let mut directory = None;
    let mut errors = AcquisitionErrors::default();
    let mut bytes = 0;
    let mut entries_seen = 0;
    // Cancellation is checked between native operations. An OS call already
    // entered may remain blocked; no subsequent listing/metadata call is begun.
    let check = trace::measure(
        progress_worker.as_ref(),
        WorkerPhase::DirectoryCheck,
        || {
            checks.measure(instrument, || {
                if control.is_cancelled() {
                    end = Some(ListingEnd::Incomplete);
                    Ok(())
                } else if !options.owners
                    && cursor
                        .read
                        .as_ref()
                        .is_some_and(ScanDirectory::retains_directory_handle)
                {
                    // Explicit listing mode retains the opened directory. Attachment
                    // and protected workspace state are checked again at EOF; child
                    // descent still validates the parent observation before opening.
                    Ok(())
                } else if (cursor.read.is_some() || cfg!(not(any(target_os = "linux", windows))))
                    && let Err(error) = guard.validate_internal_state()
                {
                    Err(error)
                } else if let Some(read) = &cursor.read {
                    guard
                        .inspect(&cursor.path)
                        .and_then(|stamp| read.validate(&stamp))
                } else {
                    let before_open = |stamp: &sortsize_core::acquisition::Stamp| {
                        if control.is_cancelled() {
                            end = Some(ListingEnd::Incomplete);
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        if let Some(origin) = origin {
                            let current = NativeEvidence::from_stamp(stamp);
                            if origin.identity.is_some() && origin.identity != current.identity
                                || origin.created.is_some() && origin.created != current.created
                            {
                                detached = true;
                                return Err(io::Error::other("observed directory was replaced"));
                            }
                        }
                        Ok(())
                    };
                    // Validate the same guarded stamp against parent
                    // evidence before opening the cursor.
                    #[cfg(any(target_os = "linux", windows))]
                    let opened = guard.scan_directory_with(&cursor.path, before_open);
                    #[cfg(not(any(target_os = "linux", windows)))]
                    let opened = {
                        let mut before_open = before_open;
                        guard.inspect(&cursor.path).and_then(|stamp| {
                            before_open(&stamp)?;
                            guard.scan_directory(&cursor.path).map(|read| (stamp, read))
                        })
                    };
                    if end == Some(ListingEnd::Incomplete) {
                        return Ok(());
                    }
                    opened.and_then(|(stamp, read)| {
                        if origin.is_some() {
                            if control.is_cancelled() {
                                end = Some(ListingEnd::Incomplete);
                                return Ok(());
                            }
                            // Match the cursor actually opened, including the path-based
                            // fallback, to the same entry as the parent observation.
                            let validation = if read.has_native_directory_handle() {
                                read.validate(&stamp)
                            } else {
                                validate_directory_snapshot(guard, &cursor.path, Some(&stamp))
                                    .map(drop)
                            };
                            if validation.is_err() {
                                detached = true;
                            }
                            validation?;
                        }
                        cursor.snapshot = Some(stamp);
                        cursor.read = Some(read);
                        Ok(())
                    })
                }
            })
        },
    );
    if let Err(error) = check {
        let operation = if detached
            || cursor
                .read
                .as_ref()
                .is_some_and(ScanDirectory::is_ntfs_batch)
        {
            AcquisitionOperation::ValidateDirectory
        } else {
            AcquisitionOperation::OpenDirectory
        };
        detached |= cursor.read.is_some();
        errors.record(&cursor.path, operation, &error);
        end = Some(ListingEnd::Incomplete);
    }
    while !control.is_cancelled()
        && end.is_none()
        && observations.len() < QUANTUM
        && (observations.is_empty() || start.elapsed() < Duration::from_millis(20))
    {
        match trace::measure(progress_worker.as_ref(), WorkerPhase::Enumeration, || {
            enumeration.measure(instrument, || cursor.read.as_mut().unwrap().next())
        }) {
            None => {
                // A cached iterator EOF alone does not prove that the source is
                // still attached. In particular, a lost NAS must not certify a
                // successful listing solely from an exhausted native cursor.
                if control.is_cancelled() {
                    end = Some(ListingEnd::Incomplete);
                    break;
                }
                end = Some(
                    match trace::measure(
                        progress_worker.as_ref(),
                        WorkerPhase::DirectoryCheck,
                        || {
                            checks.measure(instrument, || {
                                sortsize_core::acquisition::profile::measure(
                                    sortsize_core::acquisition::profile::Stage::Eof,
                                    || {
                                        validate_directory_snapshot(
                                            guard,
                                            &cursor.path,
                                            cursor.snapshot.as_ref(),
                                        )
                                    },
                                )
                            })
                        },
                    ) {
                        Ok(stamp) => {
                            directory = Some(stamp);
                            ListingEnd::Exhausted
                        }
                        Err(error) => {
                            attached = false;
                            errors.record(
                                &cursor.path,
                                AcquisitionOperation::ValidateDirectory,
                                &error,
                            );
                            ListingEnd::Incomplete
                        }
                    },
                );
                break;
            }
            Some(Err(error)) => {
                errors.record(&cursor.path, AcquisitionOperation::ReadDirectory, &error);
                end = Some(ListingEnd::Incomplete);
                break;
            }
            Some(Ok(e)) => {
                if control.is_cancelled() {
                    end = Some(ListingEnd::Incomplete);
                    break;
                }
                entries_seen += 1;
                let path = e.path();
                match trace::measure(
                    progress_worker.as_ref(),
                    WorkerPhase::DirectoryCheck,
                    || guard.skip_internal_entry(path),
                ) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        errors.record(path, AcquisitionOperation::Metadata, &error);
                        end = Some(ListingEnd::Incomplete);
                        break;
                    }
                }
                let name = encode_name(e.file_name());
                let size = name.encode().len() + 384;
                // Native filename limits make one observation <64KiB. An
                // unsupported oversized name ends partial; never loses EOF.
                if bytes + size > BATCH_BYTES {
                    end = Some(ListingEnd::Incomplete);
                    break;
                }
                bytes += size;
                let stamp = trace::measure(progress_worker.as_ref(), WorkerPhase::Metadata, || {
                    metadata.measure(instrument, || {
                        cursor.read.as_mut().unwrap().observe(
                            guard,
                            &e,
                            options.allocation,
                            options.owners,
                        )
                    })
                });
                if stamp
                    .as_ref()
                    .is_ok_and(crate::inventory::queries::workspace::adaptive::is_active_stamp)
                {
                    continue;
                }
                if stamp
                    .as_ref()
                    .is_ok_and(|stamp| guard.is_internal_identity(stamp))
                {
                    match trace::measure(
                        progress_worker.as_ref(),
                        WorkerPhase::DirectoryCheck,
                        || guard.validate_internal_state(),
                    ) {
                        Ok(()) => continue,
                        Err(error) => {
                            errors.record(path, AcquisitionOperation::Metadata, &error);
                            end = Some(ListingEnd::Incomplete);
                            break;
                        }
                    }
                }
                if let Err(error) = &stamp {
                    errors.record(path, AcquisitionOperation::Metadata, error);
                }
                evidence.push(
                    stamp
                        .as_ref()
                        .ok()
                        .map(crate::inventory::differential::NativeEvidence::from_stamp),
                );
                let mut property = stamp.as_ref().map_or_else(
                    |e| {
                        use sortsize_core::{EntryOwner, MetadataValue as M};
                        let missing = if e.kind() == io::ErrorKind::PermissionDenied {
                            M::AccessDenied
                        } else {
                            M::Unknown
                        };
                        sortsize_core::acquisition::NodeProperties {
                            allocated: missing,
                            accessed: if e.kind() == io::ErrorKind::PermissionDenied {
                                M::AccessDenied
                            } else {
                                M::Unknown
                            },
                            owner: EntryOwner {
                                id: if !options.owners {
                                    M::NotCollected
                                } else if e.kind() == io::ErrorKind::PermissionDenied {
                                    M::AccessDenied
                                } else {
                                    M::Unknown
                                },
                            },
                        }
                    },
                    |s| s.properties.clone(),
                );
                if !options.allocation {
                    property.allocated = sortsize_core::MetadataValue::NotCollected;
                }
                let allocation = match property.allocated {
                    sortsize_core::MetadataValue::Known { value, .. } => Some(value),
                    _ => None,
                };
                properties.push(property);
                let o = match stamp {
                    Ok(s) => Observation {
                        name,
                        kind: if s.directory {
                            Kind::Directory
                        } else if s.regular {
                            Kind::File
                        } else {
                            Kind::Special
                        },
                        logical: s.regular.then_some(s.length),
                        allocated: allocation,
                        modified: s.modified.and_then(timestamp),
                        metadata_known: true,
                    },
                    Err(_) => Observation {
                        name,
                        kind: Kind::Unknown,
                        logical: None,
                        allocated: None,
                        modified: None,
                        metadata_known: false,
                    },
                };
                observations.push(o);
            }
        }
    }
    if end != Some(ListingEnd::Exhausted)
        && !observations.is_empty()
        && let Some(read) = cursor.read.as_ref().filter(|read| {
            read.has_native_directory_handle()
                && (options.owners || !read.retains_directory_handle())
        })
    {
        // A pinned directory handle can outlive a rename. Do not submit
        // this quantum's observations after its path ceases to be attached.
        if attached
            && let Err(error) = trace::measure(
                progress_worker.as_ref(),
                WorkerPhase::DirectoryCheck,
                || {
                    checks.measure(instrument, || {
                        guard.validate_internal_state()?;
                        read.validate(&guard.inspect(&cursor.path)?)
                    })
                },
            )
        {
            errors.record(
                &cursor.path,
                AcquisitionOperation::ValidateDirectory,
                &error,
            );
            attached = false;
        }
        if !attached {
            observations.clear();
            properties.clear();
            evidence.clear();
            end = Some(ListingEnd::Incomplete);
        }
    }
    if control.is_cancelled() {
        end = Some(ListingEnd::Incomplete);
        directory = None;
    }
    let ntfs = ntfs_before.zip(cursor.read.as_ref()).map_or_else(
        NtfsListingStats::default,
        |(before, read)| {
            let now = read.ntfs_stats();
            NtfsListingStats {
                batch_calls: now.batch_calls - before.batch_calls,
                relative_opens: now.relative_opens - before.relative_opens,
                stale_rows: now.stale_rows - before.stale_rows,
            }
        },
    );
    let acquisition = acquisition_profile.finish();
    Completed {
        cursor,
        observations,
        entries_seen,
        evidence,
        properties,
        end,
        directory,
        detached: detached || !attached,
        errors,
        sample: dispatched.map(|dispatched| WorkerSample {
            dispatched,
            started: start,
            finished: Instant::now(),
            checks,
            enumeration,
            metadata,
            acquisition,
            observation_budget_bytes: bytes,
            ntfs,
        }),
        _permit: permit,
        _pending: None,
        _session: None,
        _progress_job: None,
    }
}
fn validate_directory_snapshot(
    guard: &Guard,
    path: &std::path::Path,
    before: Option<&sortsize_core::acquisition::Stamp>,
) -> io::Result<sortsize_core::acquisition::Stamp> {
    let after = guard.inspect_scan_directory(path)?;
    if !after.directory {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    if before.is_some_and(|s| {
        s.identity != after.identity || s.created != after.created || s.modified != after.modified
    }) {
        return Err(io::Error::other("directory changed during listing"));
    }
    Ok(after)
}
#[cfg(all(test, unix))]
fn validate_directory_end(guard: &Guard, path: &std::path::Path) -> io::Result<()> {
    validate_directory_snapshot(guard, path, None).map(|_| ())
}
pub(crate) fn timestamp(t: std::time::SystemTime) -> Option<(i64, u32)> {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => Some((d.as_secs().try_into().ok()?, d.subsec_nanos())),
        Err(e) => {
            let d = e.duration();
            let s = i64::try_from(d.as_secs()).ok()?;
            if d.subsec_nanos() == 0 {
                Some((-s, 0))
            } else {
                Some((
                    s.checked_neg()?.checked_sub(1)?,
                    1_000_000_000 - d.subsec_nanos(),
                ))
            }
        }
    }
}
pub(crate) fn native_name(n: &NativeName) -> Result<OsString> {
    #[cfg(unix)]
    if let NativeName::Posix(b) = n {
        use std::os::unix::ffi::OsStringExt;
        return Ok(OsString::from_vec(b.clone()));
    }
    #[cfg(windows)]
    if let NativeName::Windows(b) = n {
        use std::os::windows::ffi::OsStringExt;
        return Ok(OsString::from_wide(b));
    }
    Err(Error::Contract("foreign native name namespace"))
}
fn encode_name(n: &OsStr) -> NativeName {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        NativeName::Posix(n.as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        NativeName::Windows(n.encode_wide().collect())
    }
    #[cfg(not(any(unix, windows)))]
    {
        NativeName::Opaque(n.as_encoded_bytes().to_vec())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Paused,
    Cancelled,
    Idle,
    StoreError,
    /// Cancellation after a storage failure, with all dispatched work drained.
    /// The failed candidate remains unsealed and must never be published as G1.
    CancelledStoreError,
    Sealed,
}
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub state: State,
    pub in_flight: usize,
    /// Returned quanta awaiting commit, including retained failed quanta.
    pub pending_completions: usize,
    /// Dispatched quanta not yet returned (may include pool dispatch/OS waits).
    /// This is not a count of native system calls currently executing.
    pub acquisitions_pending: usize,
    /// Uncommitted quanta abandoned explicitly on cancellation after an error.
    pub abandoned_quanta: u64,
    pub parked: usize,
    pub global_acquisitions: usize,
    pub global_cursors: usize,
    pub quanta: u64,
    /// Observations produced by native quanta in this run, before ingestion.
    pub processed_entries: u64,
}
/// Owns a candidate generation, including an explicitly recovered candidate.
pub struct Session {
    active: Option<Arc<active::ActiveRoot>>,
    _memory: Arc<sortsize_core::resources::Reservation>,
    store: Store,
    context: AcquisitionContext,
    options: CollectionOptions,
    guard: Arc<Guard>,
    control: Control,
    parked: HashMap<EntryId, Cursor>,
    running: Vec<EntryId>,
    send: SyncSender<Completed>,
    receive: Receiver<Completed>,
    /// At most one failed transaction's quanta, in dequeue order.
    retry: VecDeque<Completed>,
    batch_completions: bool,
    batch_admissions: bool,
    pending_completions: Arc<AtomicUsize>,
    processed_entries: Arc<AtomicU64>,
    abandoned_quanta: u64,
    fair: Fairness,
    state: State,
    quanta: u64,
    errors: AcquisitionErrors,
    metrics: Option<Metrics>,
    progress_trace: Option<ProgressTrace>,
}
impl Session {
    pub fn new(store: Store, context: AcquisitionContext, guard: Guard) -> Result<Self> {
        Self::new_with_options(store, context, guard, CollectionOptions::default())
    }
    pub fn new_with_options(
        store: Store,
        context: AcquisitionContext,
        guard: Guard,
        options: CollectionOptions,
    ) -> Result<Self> {
        if policy_observation(context.policy) != guard.scan_observation() {
            return Err(Error::Contract(
                "scan observation method differs from durable policy",
            ));
        }
        Self::open_session_options(store, context, guard, false, options)
    }
    fn open_session(
        store: Store,
        context: AcquisitionContext,
        guard: Guard,
        recover: bool,
    ) -> Result<Self> {
        let (allocation, owners) = store
            .collection_options(context.generation)?
            .unwrap_or((true, false));
        Self::open_session_options(
            store,
            context,
            guard,
            recover,
            CollectionOptions { allocation, owners },
        )
    }
    fn open_session_options(
        mut store: Store,
        mut context: AcquisitionContext,
        guard: Guard,
        recover: bool,
        options: CollectionOptions,
    ) -> Result<Self> {
        if guard.root().as_os_str().as_encoded_bytes().len() > MAX_PATH {
            return Err(Error::Contract("root path budget"));
        }
        if !guard.work_path_allowed(store.directory())? {
            return Err(Error::Contract(
                "store must be outside the source or in its verified internal workspace",
            ));
        }
        let memory = sortsize_core::resources::Governor::global()
            .reserve(
                sortsize_core::resources::Category::Transport,
                1024 * 1024,
                None,
            )
            .ok_or(Error::Contract("global session transport budget"))?;
        let active = active::acquire(&guard)?;
        store.enable_properties()?;
        let proof = recovery::session_proof(&guard);
        if guard.internal_state().is_some() && proof.is_none() {
            return Err(Error::Contract(
                "internal workspace scope cannot be recorded",
            ));
        }
        if recover {
            context = store.recover_candidate(
                context,
                proof
                    .as_deref()
                    .ok_or(Error::Contract("source continuity is uncertain"))?,
            )?;
        } else {
            store.create_generation_guarded(context, proof.as_deref())?;
        }
        store.set_collection_options(context, options.allocation, options.owners)?;
        let (send, receive) = mpsc::sync_channel(8);
        let metrics = Metrics::requested(memory.bytes());
        let pending_completions = Arc::new(AtomicUsize::new(0));
        let processed_entries = Arc::new(AtomicU64::new(0));
        let progress_trace = ProgressTrace::requested(&pending_completions, &processed_entries);
        let batch_admissions = cfg!(any(target_os = "linux", windows))
            && guard.evidence().0.class == SourceClass::Local
            && std::env::var_os("BUNNY_SCAN_P2_ADMISSIONS").is_none_or(|value| value == "1");
        Ok(Self {
            active: Some(active),
            _memory: Arc::new(memory),
            store,
            context,
            options,
            guard: Arc::new(guard),
            control: Control::default(),
            parked: HashMap::new(),
            running: Vec::with_capacity(8),
            send,
            receive,
            retry: VecDeque::new(),
            batch_completions: std::env::var_os("BUNNY_SCAN_P1_BATCH").is_none_or(|v| v == "1"),
            batch_admissions,
            pending_completions,
            processed_entries,
            abandoned_quanta: 0,
            fair: Fairness::default(),
            state: State::Running,
            quanta: 0,
            errors: AcquisitionErrors::default(),
            metrics,
            progress_trace,
        })
    }
    /// Native failures observed by settled acquisition quanta. Coverage remains
    /// durable in SQLite; these bounded diagnostics describe the live session.
    pub fn acquisition_error_count(&self) -> u64 {
        self.errors.count
    }
    pub fn last_acquisition_error(&self) -> Option<&AcquisitionError> {
        self.errors.last.as_ref()
    }
    pub fn context(&self) -> AcquisitionContext {
        self.context
    }
    /// Source classification from the native boundary bound to this session.
    pub fn source_class(&self) -> SourceClass {
        self.guard.evidence().0.class
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }
    /// Serialized between ticks. Native workers never own a SQLite connection.
    pub fn can_place_store(&self, path: &std::path::Path) -> Result<bool> {
        Ok(self.guard.work_path_allowed(path)?)
    }
    pub fn replace_store(&mut self, store: Store) -> Result<()> {
        if !self.guard.work_path_allowed(store.directory())? {
            return Err(Error::Contract(
                "store must be outside the source or in its verified internal workspace",
            ));
        }
        if store.acquisition_context(self.context.generation)? != self.context
            || store.revision(self.context.generation)?
                != self.store.revision(self.context.generation)?
        {
            return Err(Error::Contract(
                "replacement store context/revision mismatch",
            ));
        }
        self.store = store;
        Ok(())
    }
    /// A caller can sample native progress without the writer or its SQLite locks.
    pub fn progress_counter(&self) -> Arc<AtomicU64> {
        self.processed_entries.clone()
    }

    /// Independent opt-in progress handle, with no SQLite or writer lock.
    pub fn progress_trace(&self) -> Option<ProgressTrace> {
        self.progress_trace.clone()
    }

    pub fn status(&self) -> Status {
        let pending_completions = self.pending_completions.load(Ordering::SeqCst);
        Status {
            state: self.state,
            in_flight: self.running.len(),
            pending_completions,
            acquisitions_pending: self.running.len().saturating_sub(pending_completions),
            abandoned_quanta: self.abandoned_quanta,
            parked: self.parked.len(),
            global_acquisitions: ACQUISITIONS.load(Ordering::SeqCst),
            global_cursors: CURSORS.load(Ordering::SeqCst),
            quanta: self.quanta,
            processed_entries: self.processed_entries.load(Ordering::Relaxed),
        }
    }
    /// Explicit retry after a write error. Observations/cursor remain owned until
    /// the transaction commits; no acquisition admission on a full store.
    pub fn retry_store(&mut self) {
        if self.state == State::StoreError && !self.control.is_cancelled() {
            self.state = State::Running;
        }
    }
    /// Local bounded consolidation. No new acquisition and no UI-thread wait.
    pub fn finalize_step(&mut self) -> Result<Option<ScopeCoverage>> {
        let _trace = self
            .progress_trace
            .as_ref()
            .map(|trace| trace.writer(WriterPhase::Finalize));
        if !matches!(self.state, State::Idle | State::Cancelled)
            || !self.running.is_empty()
            || !self.retry.is_empty()
        {
            return Err(Error::Contract("acquisitions not settled"));
        }
        self.guard.validate_internal_state()?;
        if !self.store.reconcile_next(self.context)?
            || !self.store.consolidate_step(self.context, 512)?
        {
            return Ok(None);
        }
        let coverage = self.store.seal(self.context)?;
        self.state = State::Sealed;
        self.active = None;
        Ok(Some(coverage))
    }
    pub fn tick(&mut self) -> Result<Status> {
        let start = self
            .metrics
            .as_mut()
            .map(|metrics| metrics.start_tick(self.pending_completions.load(Ordering::SeqCst)));
        let result = self.tick_inner();
        if let Some(start) = start {
            self.metrics
                .as_mut()
                .unwrap()
                .end_tick(start, self.pending_completions.load(Ordering::SeqCst));
            if self.running.is_empty()
                && matches!(
                    self.state,
                    State::Idle | State::Cancelled | State::CancelledStoreError
                )
            {
                self.metrics.take().unwrap().report(
                    self.state,
                    self.quanta,
                    self.abandoned_quanta,
                    self.errors.count,
                );
            }
        }
        result
    }

    fn receive_completion(&mut self) -> Option<Completed> {
        let _trace = self
            .progress_trace
            .as_ref()
            .map(|trace| trace.writer(WriterPhase::Receive));
        let start = self.metrics.as_ref().map(|_| Instant::now());
        let mut done = self.retry.pop_front().or_else(|| {
            let done = self.receive.try_recv().ok();
            if done.is_some()
                && let Some(trace) = &self.progress_trace
            {
                trace.dequeue();
            }
            done
        });
        if let Some(start) = start {
            let metrics = self.metrics.as_mut().unwrap();
            metrics.receive.add(start.elapsed());
            // Consume the native timing once, even if ingestion fails and the
            // same completion is later retried or explicitly abandoned.
            if let Some(done) = &mut done
                && let Some(sample) = done.sample.take()
            {
                metrics.observations_returned += done.observations.len();
                metrics.completion(sample);
            }
        }
        done
    }

    fn acknowledge(&mut self, mut done: Completed, next_sequence: u64) {
        self.errors.count = self.errors.count.saturating_add(done.errors.count);
        self.errors.denied = self.errors.denied.saturating_add(done.errors.denied);
        if done.errors.last.is_some() {
            self.errors.last = done.errors.last.take();
        }
        done.cursor.sequence = next_sequence;
        let id = done.cursor.attempt.task.directory;
        self.running.retain(|x| *x != id);
        self.quanta += 1;
        if let Some(trace) = &self.progress_trace {
            trace.ack();
        }
        if done.end.is_none() {
            self.parked.insert(id, done.cursor);
        } // EOF retained in durable task.
    }

    fn ingest_completions(&mut self, completed: Vec<Completed>) -> Result<()> {
        let _trace = self
            .progress_trace
            .as_ref()
            .map(|trace| trace.writer(WriterPhase::Ingest));
        let start = self.metrics.as_ref().map(|_| Instant::now());
        let ingested = (|| {
            // Validate every advance before SQLite commits: acknowledgement
            // cannot fail halfway through an atomic group.
            let next = completed
                .iter()
                .map(Completed::next_sequence)
                .collect::<Result<Vec<_>>>()?;
            let batches: Vec<_> = completed.iter().map(Completed::batch).collect();
            if batches.len() > 1 {
                self.store.ingest_directory_group(&batches)?;
            } else {
                let batch = &batches[0];
                self.store.ingest_directory_properties(
                    batch.header,
                    batch.observations,
                    batch.evidence,
                    batch.properties,
                    batch.directory,
                )?;
            }
            Ok(next)
        })();
        if let Some(start) = start {
            let metrics = self.metrics.as_mut().unwrap();
            metrics.ingest.add(start.elapsed());
            metrics.grouped_ingestions += u64::from(completed.len() > 1);
        }
        match ingested {
            Ok(next) => {
                if let Some(trace) = &self.progress_trace {
                    trace.ingested(
                        completed.len() as u64,
                        completed
                            .iter()
                            .map(|done| done.observations.len() as u64)
                            .sum(),
                    );
                }
                for (done, sequence) in completed.into_iter().zip(next) {
                    self.acknowledge(done, sequence);
                }
                Ok(())
            }
            Err(error) => {
                // Retain the complete failed transaction, including cursor
                // positions and permits, ahead of any newly returned work.
                for done in completed.into_iter().rev() {
                    self.retry.push_front(done);
                }
                self.state = State::StoreError;
                Err(error)
            }
        }
    }

    fn tick_inner(&mut self) -> Result<Status> {
        if matches!(self.state, State::CancelledStoreError | State::Sealed) {
            return Ok(self.status());
        }
        if self.state == State::StoreError {
            if self.control.is_cancelled() {
                // Cancellation must remain reachable after a failed ingestion.
                // The failed transaction rolled back: keep its durable partial
                // candidate, stop all admission, and release returned jobs
                // without trying to write through the failed store again. This
                // explicit error path never seals or certifies that candidate.
                self.parked.clear();
                for _ in 0..COMMIT_QUANTA {
                    let Some(done) = self.receive_completion() else {
                        break;
                    };
                    self.running
                        .retain(|id| *id != done.cursor.attempt.task.directory);
                    self.abandoned_quanta = self.abandoned_quanta.saturating_add(1);
                    if let Some(trace) = &self.progress_trace {
                        trace.abandon();
                    }
                }
                // Still-blocked native jobs retain their acquisition/cursor,
                // memory and root leases. Never acknowledge them as stopped.
                if self.running.is_empty() {
                    self.state = State::CancelledStoreError;
                    self.active = None;
                }
            }
            return Ok(self.status());
        }
        let validation = {
            let _trace = self
                .progress_trace
                .as_ref()
                .map(|trace| trace.writer(WriterPhase::ValidateWorkspace));
            self.guard.validate_internal_state()
        };
        if let Err(error) = validation {
            self.state = State::StoreError;
            return Err(error.into());
        }
        // Commit returned data even when paused/cancelled; it was authorized
        // earlier. A full queue never discards valid source observations.
        let committed_before = self.quanta;
        let mut remaining = if self.retry.is_empty() {
            COMMIT_QUANTA
        } else {
            self.retry.len()
        };
        while remaining > 0 {
            // A retry is exactly the failed transaction; do not append new
            // work to it. Otherwise consume only already returned work, never
            // wait to fill a group, and retain the eight-permit memory bound.
            let limit = if !self.retry.is_empty() {
                self.retry.len().min(remaining)
            } else if self.batch_completions {
                remaining
            } else {
                1
            };
            let mut completed = Vec::with_capacity(limit);
            for _ in 0..limit {
                let Some(done) = self.receive_completion() else {
                    break;
                };
                completed.push(done);
            }
            if completed.is_empty() {
                break;
            }
            remaining -= completed.len();
            self.ingest_completions(completed)?;
        }
        if let Some(metrics) = &mut self.metrics
            && self.quanta - committed_before == COMMIT_QUANTA as u64
            && self.pending_completions.load(Ordering::SeqCst) > 0
        {
            metrics.capped_drain();
        }
        let (priorities, relative, paused, cancelled) = {
            let i = self.control.0.lock().unwrap();
            (
                [i.navigation, i.visible, None, i.enrichment],
                i.navigation_relative.clone(),
                i.paused,
                i.cancelled,
            )
        };
        if cancelled {
            self.parked.clear();
            self.state = State::Cancelled;
            if self.running.is_empty() {
                self.active = None;
            }
            return Ok(self.status());
        }
        if paused {
            self.state = State::Paused;
            return Ok(self.status());
        }
        let start = self.metrics.as_ref().map(|_| Instant::now());
        let admitted = self.admit(priorities, relative);
        if let Some(start) = start {
            self.metrics
                .as_mut()
                .unwrap()
                .admission
                .add(start.elapsed());
        }
        admitted?;
        Ok(self.status())
    }

    fn admit(
        &mut self,
        mut priorities: [Option<EntryId>; 4],
        relative: Option<PathBuf>,
    ) -> Result<()> {
        let _trace = self
            .progress_trace
            .as_ref()
            .map(|trace| trace.writer(WriterPhase::Admission));
        if let Some(relative) = relative {
            // Retain the bounded intention until replaced. As parents reveal
            // children, the nearest owned ancestor advances toward the target.
            priorities[0] = Some(self.store.navigation_directory(self.context, &relative)?);
        }
        self.state = State::Running;
        let mut remaining = 8;
        while remaining > 0 {
            remaining -= 1;
            if self.control.is_cancelled() {
                break;
            }
            let selection = self.metrics.as_ref().map(|_| Instant::now());
            let selection_trace = self
                .progress_trace
                .as_ref()
                .map(|trace| trace.writer(WriterPhase::Selection));
            let mut choices = [None; 4];
            for p in [0, 1, 3] {
                if let Some(id) = priorities[p]
                    && !self.running.contains(&id)
                    && (self.parked.contains_key(&id)
                        || self.store.directory_pending(self.context, id)?)
                {
                    choices[p] = Some(id);
                }
            }
            // Stable round-robin among retained cursors: move the last-served
            // entry behind its peers using its sequence, then native EntryId.
            let parked = self
                .parked
                .iter()
                .filter(|(id, _)| !priorities.contains(&Some(**id)))
                .min_by_key(|(id, c)| (c.sequence, **id))
                .map(|(id, _)| *id);
            // Keep this bounded selection only through this admission. No SQL
            // mutation occurs before prepare_admissions validates its attempts;
            // a changed control intention still preempts the background group.
            let candidates = if parked.is_none() {
                self.store.pending_directories(self.context, 16)?
            } else {
                Vec::new()
            };
            choices[2] = parked.or_else(|| {
                candidates
                    .iter()
                    .copied()
                    .find(|id| !priorities.contains(&Some(*id)) && !self.running.contains(id))
            });
            let navigation_active = choices[..2].iter().any(Option::is_some)
                || priorities[..2]
                    .iter()
                    .flatten()
                    .any(|id| self.running.contains(id));
            // Navigation replaces its target, never the global rotation. Keep
            // the candidate position until a quantum is actually admitted:
            // exhausted permits/cursors must not consume background turns.
            let mut candidate_fair = Fairness {
                next: self.fair.next,
            };
            let choice = candidate_fair.choose(choices.map(|c| c.is_some()));
            drop(selection_trace);
            if let Some(start) = selection {
                self.metrics
                    .as_mut()
                    .unwrap()
                    .selection
                    .add(start.elapsed());
            }
            let Some(mut p) = choice else {
                break;
            };
            let requested = p;
            let permit = acquisition_lease(p < 2, navigation_active);
            if let Some(metrics) = &mut self.metrics
                && permit.is_none()
            {
                metrics.acquisition_permit_denials += 1;
            }
            let permit = match permit {
                Some(permit) => permit,
                None if p >= 2 => {
                    let Some(interactive) = (0..2).find(|p| choices[*p].is_some()) else {
                        break;
                    };
                    let Some(permit) = acquisition_lease(true, navigation_active) else {
                        break;
                    };
                    p = interactive;
                    permit
                }
                None => break,
            };
            let id = choices[p].unwrap();
            let (cursor, additional) = if let Some(cursor) = self.parked.remove(&id) {
                if let Some(metrics) = &mut self.metrics {
                    metrics.parked_dispatches += 1;
                }
                (cursor, Vec::new())
            } else {
                let Some(lease) = cursor_lease(p < 2) else {
                    if let Some(metrics) = &mut self.metrics {
                        metrics.cursor_permit_denials += 1;
                    }
                    break;
                };
                let may_group = self.batch_admissions
                    && self.store.supports_grouped_attempts()
                    && remaining > 0
                    && self.parked.is_empty()
                    && priorities.iter().all(Option::is_none);
                let Some(prepared) = self.prepare_admissions(
                    id,
                    lease,
                    if may_group { remaining } else { 0 },
                    &candidates,
                )?
                else {
                    // A new control intention preempted this group before commit.
                    return Ok(());
                };
                prepared
            };
            let first = Admission { cursor, permit };
            if !additional.is_empty() {
                let extra = additional.len();
                let mut group = Vec::with_capacity(extra + 1);
                group.push(first);
                group.extend(additional);
                if !self.dispatch_background_group(group)? {
                    return Ok(());
                }
                remaining -= extra;
                // No competing intentions existed at selection or dispatch.
                for _ in 0..extra {
                    let _ = candidate_fair.choose([false, false, true, false]);
                }
            } else {
                self.dispatch(first)?;
            }
            if p == requested {
                self.fair = candidate_fair;
            } // An interactive fallback keeps the unpaid background turn.
        }
        if self.running.is_empty()
            && self.parked.is_empty()
            && self.store.pending_directories(self.context, 1)?.is_empty()
        {
            self.state = State::Idle;
        }
        Ok(())
    }

    fn prepare_admissions(
        &mut self,
        id: EntryId,
        lease: CursorLease,
        extra_limit: usize,
        candidates: &[EntryId],
    ) -> Result<Option<(Cursor, Vec<Admission>)>> {
        let _trace = self
            .progress_trace
            .as_ref()
            .map(|trace| trace.writer(WriterPhase::Preparation));
        let start = self.metrics.as_ref().map(|_| Instant::now());
        let prepared: Result<_> = (|| {
            let cursor = self.prepare_cursor(id, lease)?;
            let mut additional = Vec::new();
            if extra_limit > 0 {
                if !self.control.allows_background_pair() {
                    return Ok(None);
                }
                // Reuse the same bounded queue selection. These IDs are hints,
                // not proofs: paths and attempts are prepared below, and every
                // attempt is validated again inside the committing transaction.
                for candidate in candidates
                    .iter()
                    .copied()
                    .filter(|candidate| *candidate != id && !self.running.contains(candidate))
                {
                    if additional.len() == extra_limit.min(7) {
                        break;
                    }
                    if !self.control.allows_background_pair() {
                        return Ok(None);
                    }
                    let Some(permit) = acquisition_lease(false, false) else {
                        break;
                    };
                    let Some(lease) = cursor_lease(false) else {
                        break;
                    };
                    additional.push(Admission {
                        cursor: self.prepare_cursor(candidate, lease)?,
                        permit,
                    });
                }
                if !self.control.allows_background_pair() {
                    return Ok(None);
                }
            }
            if !additional.is_empty() {
                if let Err(error) = self.guard.validate_internal_state() {
                    self.state = State::StoreError;
                    return Err(error.into());
                }
                let mut attempts = Vec::with_capacity(additional.len() + 1);
                attempts.push(cursor.attempt);
                attempts.extend(additional.iter().map(|admission| admission.cursor.attempt));
                let committed = self.store.begin_attempt_group(&attempts);
                if let Some(metrics) = &mut self.metrics {
                    metrics.grouped_admissions += u64::from(committed.is_ok());
                    metrics.grouped_admission_failures += u64::from(committed.is_err());
                }
                if committed.is_err() {
                    self.state = State::StoreError;
                }
                committed?;
            } else {
                self.store.begin_attempt(cursor.attempt)?;
            }
            Ok(Some((cursor, additional)))
        })();
        if let Some(start) = start {
            let metrics = self.metrics.as_mut().unwrap();
            metrics.preparation.add(start.elapsed());
            if let Ok(Some((_, additional))) = &prepared {
                metrics.new_attempts += 1 + additional.len() as u64;
            }
        }
        prepared
    }

    fn prepare_cursor(&self, id: EntryId, lease: CursorLease) -> Result<Cursor> {
        let path = self
            .guard
            .root()
            .join(self.store.relative_directory(self.context, id)?);
        if path.as_os_str().as_encoded_bytes().len() > MAX_PATH {
            return Err(Error::Contract("directory path budget"));
        }
        let attempt = self.store.next_attempt(self.context, id)?;
        Ok(Cursor {
            attempt,
            path,
            read: None,
            snapshot: None,
            sequence: 0,
            _lease: lease,
        })
    }

    fn dispatch_background_group(&mut self, group: Vec<Admission>) -> Result<bool> {
        let validated = self.guard.validate_internal_state();
        let control = self.control.clone();
        let intent = control.0.lock().unwrap();
        if validated.is_err() || !intent.allows_background_pair() {
            // All attempts already committed. Keep them schedulable after a
            // pause or new priority, but release their acquisition permits.
            if !intent.cancelled {
                for Admission { cursor, .. } in group {
                    self.parked.insert(cursor.attempt.task.directory, cursor);
                }
            }
            if let Err(error) = validated {
                self.state = State::StoreError;
                return Err(error.into());
            }
            return Ok(false);
        }
        // No filesystem or SQL work under the control lock. Pool capacity is
        // eight and every queued job owns one of eight acquisition permits,
        // including this group, so the sends cannot block on a full queue.
        for admission in group {
            self.dispatch(admission)?;
        }
        Ok(true)
    }

    fn dispatch(&mut self, Admission { cursor, permit }: Admission) -> Result<()> {
        let _trace = self
            .progress_trace
            .as_ref()
            .map(|trace| trace.writer(WriterPhase::Dispatch));
        let id = cursor.attempt.task.directory;
        let memory = self._memory.clone();
        let active = self.active.clone();
        let guard = self.guard.clone();
        let reply = self.send.clone();
        let options = self.options;
        let control = self.control.clone();
        let pending_completions = self.pending_completions.clone();
        let processed_entries = self.processed_entries.clone();
        let progress_trace = self.progress_trace.clone();
        let ticket = progress_trace.as_ref().map(ProgressTrace::ticket);
        let progress_job = progress_trace.as_ref().map(ProgressTrace::job);
        let dispatched = self.metrics.as_ref().map(|_| Instant::now());
        let sent = pool().send.send(Box::new(move || {
            let run = progress_trace
                .as_ref()
                .zip(ticket)
                .map(|(trace, ticket)| trace.start_worker(ticket));
            let mut result = quantum(&guard, cursor, permit, options, &control, dispatched);
            let observations = result.observations.len() as u64;
            let _ = processed_entries.try_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                Some(total.saturating_add(observations))
            });
            result._pending = Some(CompletionLease::new(pending_completions));
            result._session = Some(SessionLease {
                _memory: memory,
                _active: active,
            });
            result._progress_job = progress_job;
            if let Some(run) = &run {
                run.ready(observations);
                run.enqueue_begin();
            }
            let sent = reply.send(result);
            if sent.is_ok()
                && let Some(run) = &run
            {
                run.enqueued();
            }
        }));
        if sent.is_ok()
            && let Some(trace) = &self.progress_trace
            && let Some((_, when)) = ticket
        {
            trace.dispatched(when);
        }
        if let Some(start) = dispatched {
            let metrics = self.metrics.as_mut().unwrap();
            metrics.dispatch.add(start.elapsed());
            metrics.dispatched += u64::from(sent.is_ok());
        }
        sent.map_err(|_| Error::Io(io::ErrorKind::BrokenPipe.into()))?;
        self.running.push(id);
        Ok(())
    }
}
#[cfg(test)]
mod folder_freshness_tests;
#[cfg(test)]
mod tests;
