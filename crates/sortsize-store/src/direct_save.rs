//! Explicit canonical publication from the observations retained by a direct view.
use crate::{
    codec::NativeName,
    inventory::{
        Error, Observation, Result, Store,
        differential::NativeEvidence,
        direct_replay::install_family,
        generation_a::{CertifiedGeneration, Pipeline, Repository},
        lifecycle,
    },
    scheduler::{CollectionOptions, session_proof},
};
use sortsize_core::{
    CancellationToken,
    acquisition::{Attachment, Guard, NodeProperties, Stamp},
    inventory_contract::{AcquisitionContext, EntryId, ListingEnd, ScopeCoverage},
    resources::{Category, Governor},
};
use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// An accepted quantum, in the order the direct view assigned entry IDs.
/// Only the first retained observation of a name may occur in the replay.
pub struct ReplayBatch {
    pub parent: EntryId,
    pub sequence: u64,
    pub end: Option<ListingEnd>,
    pub observations: Vec<Observation>,
    pub evidence: Vec<Option<NativeEvidence>>,
    pub properties: Vec<NodeProperties>,
    pub directory: Option<Stamp>,
    pub children: Vec<(EntryId, NativeName)>,
}

pub struct SaveRequest<'a> {
    pub context: AcquisitionContext,
    /// The original acquisition guard, including its workspace exclusion.
    pub guard: &'a Guard,
    pub scratch: &'a Path,
    /// New sibling of previously retained families in the history root.
    pub destination: &'a Path,
    pub started: i64,
    pub measured: i64,
    pub options: CollectionOptions,
    pub coverage: ScopeCoverage,
}

pub struct Saved {
    /// The family is durable even if a subsequent reader could not be opened.
    pub reader: Result<(Repository, CertifiedGeneration)>,
}

pub(crate) fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Contract("direct save cancelled"));
    }
    Ok(())
}

/// Rebuild the canonical store from the sealed spool. The only filesystem
/// operations outside the spool and the canonical pipeline concern placement
/// of the new state. The measured entries are never reopened or re-enumerated.
pub fn save(
    input: SaveRequest<'_>,
    batches: impl IntoIterator<Item = Result<ReplayBatch>>,
    cancel: &CancellationToken,
) -> Result<Saved> {
    let proof = session_proof(input.guard);
    if input.guard.internal_state().is_some() && proof.is_none() {
        return Err(Error::Contract(
            "direct save could not record workspace scope",
        ));
    }
    save_recorded(
        RecordedSaveRequest {
            context: input.context,
            root: input.guard.root(),
            scratch: input.scratch,
            destination: input.destination,
            started: input.started,
            measured: input.measured,
            options: input.options,
            coverage: input.coverage,
            proof: proof.as_deref(),
        },
        batches,
        cancel,
        |path| Ok(input.guard.work_path_allowed(path)?),
    )
}

/// Passive recorded metadata for a compatibility export. The caller separately
/// authorizes a newly owned private temporary workspace, never the source.
pub(crate) struct RecordedSaveRequest<'a> {
    pub context: AcquisitionContext,
    pub root: &'a Path,
    pub scratch: &'a Path,
    pub destination: &'a Path,
    pub started: i64,
    pub measured: i64,
    pub options: CollectionOptions,
    pub coverage: ScopeCoverage,
    pub proof: Option<&'a [u8]>,
}

pub(crate) fn save_recorded(
    input: RecordedSaveRequest<'_>,
    batches: impl IntoIterator<Item = Result<ReplayBatch>>,
    cancel: &CancellationToken,
    allowed: impl Fn(&Path) -> Result<bool>,
) -> Result<Saved> {
    const CHUNK: usize = 32;
    let measured_from = crate::scan_metrics::enabled().then(Instant::now);
    check_cancelled(cancel)?;
    let context = input.context;
    let generation = context.generation;
    let root = input.root;
    let name = input
        .destination
        .file_name()
        .ok_or(Error::Contract("direct save destination name"))?;
    let parent = input
        .destination
        .parent()
        .ok_or(Error::Contract("direct save destination parent"))?;
    if !allowed(input.scratch)? || !allowed(parent)? {
        return Err(Error::Contract(
            "direct save requires safe local state placement",
        ));
    }
    lifecycle::ensure_root(parent)?;
    let parent = std::fs::canonicalize(parent)?;
    let destination = parent.join(name);
    if std::fs::symlink_metadata(&destination).is_ok() {
        return Err(Error::Contract("direct save destination already exists"));
    }
    let scratch = ReplayScratch::new(input.scratch)?;
    if !allowed(&scratch.store)? {
        return Err(Error::Contract("direct save replay placement changed"));
    }
    let mut store = Store::create_direct_replay(&scratch.store, context, cancel)?;
    store.enable_properties()?;
    store.create_generation_guarded(context, input.proof)?;
    store.set_collection_options(context, input.options.allocation, input.options.owners)?;
    store.record_acquisition(context, root, input.started)?;
    {
        let _transport = Governor::global()
            .reserve(Category::Transport, 4 * 1024 * 1024, None)
            .ok_or(Error::Contract("direct save replay transport budget"))?;
        let mut chunk = Vec::with_capacity(CHUNK);
        let mut replayed = 0usize;
        let mut next_trace = Duration::from_secs(10);
        for batch in batches {
            check_cancelled(cancel)?;
            chunk.push(batch?);
            if chunk.len() == CHUNK {
                store.replay_direct_chunk(context, &chunk, cancel)?;
                if let Some(start) = measured_from {
                    replayed += chunk
                        .iter()
                        .map(|batch| batch.observations.len())
                        .sum::<usize>();
                    let elapsed = start.elapsed();
                    if elapsed >= next_trace {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "scan_direct_save_progress wall_us={} entries={replayed}",
                            elapsed.as_micros(),
                        );
                        next_trace = elapsed + Duration::from_secs(10);
                    }
                }
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            store.replay_direct_chunk(context, &chunk, cancel)?;
        }
    }
    let coverage = store.finish_direct_replay(context, cancel)?;
    if coverage != input.coverage {
        return Err(Error::Contract(
            "direct save coverage differs from the sealed view",
        ));
    }
    store.record_measurement(generation, root, input.started, input.measured)?;
    store.checkpoint_direct_replay()?;
    if let Some(start) = measured_from {
        let _ = writeln!(
            std::io::stderr().lock(),
            "scan_direct_save_replayed wall_us={} canonical_bytes={}",
            start.elapsed().as_micros(),
            store.canonical_usage().map_or(0, |usage| usage.charged),
        );
    }
    check_cancelled(cancel)?;

    let mut stage_name = OsString::from(".stage-direct-");
    stage_name.push(name);
    let mut stage = StagingFamily::new(parent.join(stage_name), context)?;
    let mut plan = Pipeline::for_publication()?;
    let mut repository = Repository::open(&stage.path, context.inventory, context.source)?;
    let outcome = repository.adopt_direct_replay_with_plan(
        &store,
        generation,
        root,
        Repository::publication_budget(&store)?,
        cancel,
        &mut plan,
    )?;
    if !outcome.committed() {
        return Err(Error::Contract(
            "direct save certification was not published in staging",
        ));
    }
    repository.set_retention(true, 30)?;
    drop(repository);
    drop(store);
    drop(scratch);

    // The catalog only recognizes inv-* families. At this point the private
    // family contains a complete certificate, with no open SQLite handles.
    if !allowed(&parent)? || !stage.owns_path() {
        return Err(Error::Contract("direct save history placement changed"));
    }
    check_cancelled(cancel)?;
    install_family(&stage.path, &destination)?;
    stage.installed = true;
    stage.relocate_accounting(&destination);
    if let Some(start) = measured_from {
        let _ = writeln!(
            std::io::stderr().lock(),
            "scan_direct_save_committed wall_us={}",
            start.elapsed().as_micros(),
        );
    }

    let reader = Repository::open_existing(&destination).and_then(|repository| {
        let certified = repository
            .open_latest_with_plan(&mut plan, cancel)?
            .ok_or(Error::Contract("direct save committed generation missing"))?;
        Ok((repository, certified))
    });
    Ok(Saved { reader })
}

struct ReplayScratch {
    temp: Option<tempfile::TempDir>,
    store: PathBuf,
}
impl ReplayScratch {
    fn new(parent: &Path) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("direct-replay-")
            .tempdir_in(parent)?;
        let store = std::fs::canonicalize(temp.path())?.join("inventory");
        Ok(Self {
            temp: Some(temp),
            store,
        })
    }
}
impl Drop for ReplayScratch {
    fn drop(&mut self) {
        if self.temp.take().is_some_and(|temp| temp.close().is_ok()) {
            Governor::global().forget_deleted_artifact_group(&self.store);
        }
    }
}

struct StagingFamily {
    path: PathBuf,
    guard: Guard,
    generation: String,
    build_stage: String,
    installed: bool,
}
impl StagingFamily {
    fn new(path: PathBuf, context: AcquisitionContext) -> Result<Self> {
        lifecycle::private_directory(&path)?;
        let guard = Guard::open(&path, Default::default())?;
        let g = context.generation.0;
        Ok(Self {
            path,
            guard,
            generation: format!("generation-{g:016x}"),
            build_stage: format!(".stage-a-{:032x}-{g:016x}", context.inventory.0),
            installed: false,
        })
    }

    fn owns_path(&self) -> bool {
        lifecycle::check_directory(&self.path).is_ok()
            && Guard::open(&self.path, Default::default()).is_ok_and(|current| {
                let (boundary, stamp) = self.guard.evidence();
                let (current_boundary, current_stamp) = current.evidence();
                boundary == current_boundary
                    && stamp.attachment(current_stamp) == Attachment::Compatible
            })
    }

    fn relocate_accounting(&self, destination: &Path) {
        let governor = Governor::global();
        for (from, to) in [
            (
                self.path.join(&self.generation),
                destination.join(&self.generation),
            ),
            (self.path.clone(), destination.to_owned()),
        ] {
            if !governor.relocate_artifact_group(&from, &to) {
                // A concurrent catalogue read can already have observed the
                // renamed artifact. The old key is now confirmed absent.
                governor.forget_deleted_artifact_group(&from);
            }
        }
    }
}
impl Drop for StagingFamily {
    fn drop(&mut self) {
        if !self.installed && self.owns_path() && std::fs::remove_dir_all(&self.path).is_ok() {
            let governor = Governor::global();
            for path in [
                self.path.join(&self.generation),
                self.path.join(&self.build_stage),
                self.path.clone(),
            ] {
                governor.forget_deleted_artifact_group(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{inventory::Kind, scheduler::Session};
    use sortsize_core::inventory_contract::{
        GenerationId, InventoryId, MountEpoch, PolicyId, RunEpoch, ScanId, SourceId,
        ValidationEpoch,
    };

    fn context() -> AcquisitionContext {
        AcquisitionContext {
            inventory: InventoryId(818),
            source: SourceId(819),
            generation: GenerationId(1),
            scan: ScanId(1),
            run: RunEpoch(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            validation: ValidationEpoch(1),
        }
    }

    fn sample_name() -> NativeName {
        #[cfg(windows)]
        {
            NativeName::Windows("recorded.txt".encode_utf16().collect())
        }
        #[cfg(not(windows))]
        {
            NativeName::Posix(b"recorded.txt".to_vec())
        }
    }

    fn retained(guard: &Guard) -> ReplayBatch {
        ReplayBatch {
            parent: EntryId(0),
            sequence: 0,
            end: Some(ListingEnd::Exhausted),
            observations: vec![Observation {
                name: sample_name(),
                kind: Kind::File,
                logical: Some(12345),
                allocated: None,
                modified: None,
                metadata_known: true,
            }],
            evidence: vec![None],
            properties: vec![NodeProperties::default()],
            directory: Some(guard.evidence().1.clone()),
            children: Vec::new(),
        }
    }

    #[test]
    fn direct_save_replays_without_reading_source_and_cancel_preserves_previous() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        lifecycle::private_directory(&source).unwrap();
        let work = source.join("work");
        lifecycle::private_directory(&work).unwrap();
        let scratch = work.join("scratch");
        lifecycle::private_directory(&scratch).unwrap();
        let history = work.join("history");
        let destination = history.join("inv-direct-first");
        let guard = Guard::open(&source, Default::default())
            .unwrap()
            .with_internal_state(Guard::open(&work, Default::default()).unwrap())
            .unwrap();
        let c = context();
        let request = |destination| SaveRequest {
            context: c,
            guard: &guard,
            scratch: &scratch,
            destination,
            started: 10,
            measured: 20,
            options: CollectionOptions::default(),
            coverage: ScopeCoverage::Complete,
        };
        let (repository, reader) = save(
            request(&destination),
            [Ok(retained(&guard))],
            &Default::default(),
        )
        .unwrap()
        .reader
        .unwrap();
        assert!(repository.retention().unwrap().enabled);
        assert_eq!(reader.as_store().entry_count(c.generation).unwrap(), 2);
        let entry = reader
            .as_store()
            .lookup(c.generation, EntryId(0), &sample_name())
            .unwrap()
            .unwrap();
        assert_eq!(entry.key.entry, EntryId(1));
        assert_eq!(entry.logical, Some(12345));
        assert!(Session::source_matches(reader.as_store(), c.generation, &guard).unwrap());
        assert_eq!(
            Session::excluded_state_path(reader.as_store(), c.generation)
                .unwrap()
                .as_deref(),
            guard.internal_state().map(Guard::root),
        );
        assert!(!source.join("recorded.txt").exists());

        let previous = std::fs::read(destination.join("CURRENT.json")).unwrap();
        let cancelled_path = history.join("inv-direct-cancelled");
        let cancel = CancellationToken::default();
        let mut batches = [Ok(retained(&guard))].into_iter();
        let during_replay = std::iter::from_fn(|| {
            let batch = batches.next();
            if batch.is_none() {
                cancel.cancel();
            }
            batch
        });
        assert!(save(request(&cancelled_path), during_replay, &cancel).is_err());
        assert!(!cancelled_path.exists());
        let malformed_path = history.join("inv-direct-malformed");
        let mut malformed = retained(&guard);
        malformed
            .observations
            .push(retained(&guard).observations.remove(0));
        malformed.evidence.push(None);
        malformed.properties.push(NodeProperties::default());
        assert!(
            save(
                request(&malformed_path),
                [Ok(malformed)],
                &Default::default()
            )
            .is_err()
        );
        assert!(!malformed_path.exists());
        assert!(
            save(
                request(&destination),
                [Ok(retained(&guard))],
                &Default::default()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(destination.join("CURRENT.json")).unwrap(),
            previous
        );
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&history).unwrap().count(), 1);
    }
}
