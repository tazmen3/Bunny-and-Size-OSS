//! Durable, versioned owner of one inventory and its physical generations.
//! Reads never repair, migrate, acquire source metadata, or purge artifacts.
use super::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
mod admission;
mod adoption_retry;
mod policy;
mod recovery;

const REPOSITORY_VERSION: u32 = 1;
// ORIGIN carries at most four native byte fields (directory + three roots).
// Each is <= 65_538 bytes; numeric Vec<u8> JSON requires at most four bytes
// per encoded byte, plus bounded contexts/headers. 2 MiB covers that envelope.
// This is an internal document limit, never an increase to global RAM/disk.
const DOCUMENT_LIMIT: u64 = 2 * 1024 * 1024;
pub(in crate::inventory) const DIRECTORY_LIMIT: usize = 4096;

#[derive(Clone, Debug)]
pub struct Descriptor {
    pub directory: PathBuf,
    pub generation: GenerationId,
    pub root: PathBuf,
    pub context: AcquisitionContext,
    pub coverage: ScopeCoverage,
    pub started: i64,
    pub measured: Option<i64>,
}
#[derive(Debug)]
pub enum PublicationOutcome {
    Committed {
        descriptor: Descriptor,
        report: BuildReport,
    },
    /// The generation exists and is certified, but it was not selected by a
    /// confirmed durable catalogue commit. Retrying must reconcile, not recopy.
    PublishedUncommitted {
        descriptor: Descriptor,
        report: BuildReport,
        error: Error,
    },
}
struct PublicationSlots {
    builder: Option<Reservation>,
    reader: Option<Reservation>,
}
impl PublicationOutcome {
    pub fn descriptor(&self) -> &Descriptor {
        match self {
            Self::Committed { descriptor, .. } | Self::PublishedUncommitted { descriptor, .. } => {
                descriptor
            }
        }
    }
    pub fn committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}
#[derive(Serialize, Deserialize)]
struct Identity {
    version: u32,
    inventory: [u8; 16],
    source: [u8; 16],
}
#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    next_generation: u64,
    #[serde(default)]
    exhausted: bool,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Current {
    version: u32,
    latest: Option<u64>,
    last_complete: Option<u64>,
}
#[derive(Serialize, Deserialize)]
struct Allocation {
    version: u32,
    generation: u64,
}
#[derive(Serialize, Deserialize)]
struct Measurement {
    version: u32,
    generation: u64,
    root: Vec<u8>,
    context: Vec<u8>,
    complete: bool,
    started: i64,
    measured: Option<i64>,
}

/// The repository lock serializes allocation, publication and retirement. A RO
/// capability owns a distinct per-generation lock and can outlive this owner.
pub struct Repository {
    directory: PathBuf,
    inventory: InventoryId,
    source: SourceId,
    _owner: Option<File>,
    current: Current,
    read_only: bool,
}
impl Repository {
    /// Create a private repository exclusively, preserving any existing path.
    pub fn create_new(path: &Path, inventory: InventoryId, source: SourceId) -> Result<Self> {
        lifecycle::private_directory(path)?;
        Self::open(path, inventory, source)
    }

    pub fn open(path: &Path, inventory: InventoryId, source: SourceId) -> Result<Self> {
        let (owner, initializing) = lock_repository(path, true)?;
        let path = std::fs::canonicalize(path)?;
        validate_layout(&path)?;
        let identity_path = path.join("IDENTITY.json");
        if !identity_path.exists() {
            if !initializing || std::fs::read_dir(&path)?.count() != 1 {
                return Err(Error::Contract(
                    "repository identity missing; preserve for recovery",
                ));
            }
            write_json(
                &path,
                "IDENTITY.json",
                &Identity {
                    version: REPOSITORY_VERSION,
                    inventory: inventory.0.to_be_bytes(),
                    source: source.0.to_be_bytes(),
                },
                false,
            )?;
            write_json(
                &path,
                "STATE.json",
                &State {
                    version: REPOSITORY_VERSION,
                    next_generation: 1,
                    exhausted: false,
                },
                false,
            )?;
            write_json(
                &path,
                "CURRENT.json",
                &Current {
                    version: REPOSITORY_VERSION,
                    ..Default::default()
                },
                false,
            )?;
        }
        let identity: Identity = read_json(&identity_path)?;
        validate_identity(&identity, inventory, source)?;
        let state: State = read_json(&path.join("STATE.json"))?;
        if state.version != REPOSITORY_VERSION || state.next_generation == 0 {
            return Err(Error::Contract(
                "future repository allocation state preserved",
            ));
        }
        let current: Current = read_json(&path.join("CURRENT.json"))?;
        if current.version != REPOSITORY_VERSION {
            return Err(Error::Contract("future repository catalogue preserved"));
        }
        let repo = Self {
            directory: path,
            inventory,
            source,
            _owner: Some(owner),
            current,
            read_only: false,
        };
        repo.validate_policy_documents()?;
        repo.account()?;
        repo.validate_pointer()?;
        Ok(repo)
    }
    /// Atomic catalogue reads do not take the writer's repository lock. The
    /// actual selected generation is protected by its own shared RO lease.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let (inventory, source) = Self::inspect_identity(path)?;
        let directory = std::fs::canonicalize(path)?;
        validate_layout(&directory)?;
        let state: State = read_json(&directory.join("STATE.json"))?;
        if state.version != REPOSITORY_VERSION || state.next_generation == 0 {
            return Err(Error::Contract(
                "future repository allocation state preserved",
            ));
        }
        let current: Current = read_json(&directory.join("CURRENT.json"))?;
        if current.version != REPOSITORY_VERSION {
            return Err(Error::Contract("future repository catalogue preserved"));
        }
        let repo = Self {
            directory,
            inventory,
            source,
            _owner: None,
            current,
            read_only: true,
        };
        repo.validate_pointer()?;
        repo.validate_policy_documents()?;
        repo.account()?;
        Ok(repo)
    }
    pub fn open_existing(path: &Path) -> Result<Self> {
        let (inventory, source) = Self::inspect_identity(path)?;
        Self::open(path, inventory, source)
    }
    pub fn inspect_identity(path: &Path) -> Result<(InventoryId, SourceId)> {
        sortsize_core::acquisition::local_state_location(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_symlink() || !metadata.is_dir() {
            return Err(Error::Contract("unsafe repository directory"));
        }
        let identity: Identity = read_json(&path.join("IDENTITY.json"))?;
        if identity.version != REPOSITORY_VERSION {
            return Err(Error::Contract("future repository identity preserved"));
        }
        Ok((
            InventoryId(u128::from_be_bytes(identity.inventory)),
            SourceId(u128::from_be_bytes(identity.source)),
        ))
    }
    /// Explicit recovery ownership. An invalid pointer is preserved until the
    /// caller requests reconstruction. Future identity/allocation formats fail.
    pub fn open_for_recovery(
        path: &Path,
        inventory: InventoryId,
        source: SourceId,
    ) -> Result<Self> {
        let (owner, _) = lock_repository(path, false)?;
        let directory = std::fs::canonicalize(path)?;
        validate_layout(&directory)?;
        let identity: Identity = read_json(&directory.join("IDENTITY.json"))?;
        validate_identity(&identity, inventory, source)?;
        let state: State = read_json(&directory.join("STATE.json"))?;
        if state.version != REPOSITORY_VERSION || state.next_generation == 0 {
            return Err(Error::Contract("future repository state preserved"));
        }
        let current = match read_json::<Current>(&directory.join("CURRENT.json")) {
            Ok(value) if value.version == REPOSITORY_VERSION => value,
            Ok(_) => return Err(Error::Contract("future repository pointer preserved")),
            Err(_) => Current {
                version: REPOSITORY_VERSION,
                ..Default::default()
            },
        };
        let repo = Self {
            directory,
            inventory,
            source,
            _owner: Some(owner),
            current,
            read_only: false,
        };
        repo.validate_policy_documents()?;
        repo.account()?;
        Ok(repo)
    }
    pub fn identity(&self) -> (InventoryId, SourceId) {
        (self.inventory, self.source)
    }
    /// Admission check before reserving the next pipeline. A cache path alone
    /// never qualifies continuity after a replacement volume or native root.
    pub fn source_matches(
        &self,
        guard: &sortsize_core::acquisition::Guard,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let Some(descriptor) = self.latest_context()? else {
            return Ok(false);
        };
        let memory = Governor::global()
            .reserve(Category::Sqlite, FRAME, None)
            .ok_or(Error::Contract("source continuity certificate admission"))?;
        let cap = CertifiedGeneration::open_admitted(
            &descriptor.directory,
            descriptor.generation,
            &descriptor.root,
            cancel,
            memory,
        )?;
        self.check_capability(&descriptor, &cap)?;
        crate::scheduler::Session::source_matches(cap.as_store(), descriptor.generation, guard)
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn candidate_path(&self, g: GenerationId) -> PathBuf {
        self.directory.join(format!("candidate-{:016x}", g.0))
    }
    pub fn generation_path(&self, g: GenerationId) -> PathBuf {
        self.directory.join(format!("generation-{:016x}", g.0))
    }
    pub fn next_context(&mut self, template: AcquisitionContext) -> Result<AcquisitionContext> {
        self.require_writer()?;
        if template.inventory != self.inventory || template.source != self.source {
            return Err(Error::Contract("repository allocation identity mismatch"));
        }
        let state: State = read_json(&self.directory.join("STATE.json"))?;
        if state.version != REPOSITORY_VERSION || state.next_generation == 0 {
            return Err(Error::Contract("repository allocation state rejected"));
        }
        if state.exhausted {
            return Err(Error::Contract("repository generation identity exhausted"));
        }
        let next = state.next_generation.saturating_add(1);
        self.admit_allocation()?;
        // High-water is durable before an ID is returned. Gaps after a crash
        // are permitted; reuse is not. Receipts survive generation retirement.
        write_json(
            &self.directory,
            "STATE.json",
            &State {
                version: REPOSITORY_VERSION,
                next_generation: next,
                exhausted: state.next_generation == u64::MAX,
            },
            true,
        )?;
        write_json(
            &self.directory,
            &format!("allocation-{:016x}.json", state.next_generation),
            &Allocation {
                version: REPOSITORY_VERSION,
                generation: state.next_generation,
            },
            false,
        )?;
        self.account()?;
        Ok(AcquisitionContext {
            generation: GenerationId(state.next_generation),
            ..template
        })
    }
    pub fn next_context_after(
        &mut self,
        template: AcquisitionContext,
        after: GenerationId,
    ) -> Result<AcquisitionContext> {
        self.require_writer()?;
        if template.inventory != self.inventory || template.source != self.source {
            return Err(Error::Contract("repository allocation identity mismatch"));
        }
        let state: State = read_json(&self.directory.join("STATE.json"))?;
        if state.version != REPOSITORY_VERSION {
            return Err(Error::Contract(
                "future repository allocation state preserved",
            ));
        }
        if state.next_generation <= after.0 {
            let next = after
                .0
                .checked_add(1)
                .ok_or(Error::Contract("repository generation identity exhausted"))?;
            write_json(
                &self.directory,
                "STATE.json",
                &State {
                    version: REPOSITORY_VERSION,
                    next_generation: next,
                    exhausted: state.exhausted,
                },
                true,
            )?;
        }
        self.next_context(template)
    }
    pub fn reserve_legacy_ids(&mut self, legacy: &Store) -> Result<()> {
        self.require_writer()?;
        legacy.require_integrity()?;
        if legacy.inventory != self.inventory || legacy.source != self.source {
            return Err(Error::Contract("legacy identity reservation mismatch"));
        }
        let mut statement = legacy
            .db
            .prepare("SELECT id FROM generations ORDER BY id LIMIT 4")?;
        let mut rows = statement.query([])?;
        let mut ids = Vec::with_capacity(3);
        while let Some(row) = rows.next()? {
            let id = GenerationId(number(row, 0)?);
            let c = legacy.acquisition_context(id)?;
            if c.inventory != self.inventory || c.source != self.source {
                return Err(Error::Contract("legacy qualified context mismatch"));
            }
            ids.push(id);
        }
        if ids.len() > 3 {
            return Err(Error::Contract(
                "legacy generation admission contract exceeded",
            ));
        }
        for id in ids {
            self.import_allocation(id)?;
        }
        Ok(())
    }
    pub fn create_candidate(
        &self,
        context: AcquisitionContext,
        plan: &mut pipeline::Pipeline,
    ) -> Result<Store> {
        self.require_writer()?;
        self.require_allocation(context.generation)?;
        if context.inventory != self.inventory || context.source != self.source {
            return Err(Error::Contract("candidate repository identity mismatch"));
        }
        let mut store = self.create_candidate_store(context, plan)?;
        store.create_generation(context)?;
        Ok(store)
    }
    /// Empty owner for Session::new_with_options, which installs its guard and
    /// canonical generation as one initialization path.
    pub fn create_candidate_store(
        &self,
        context: AcquisitionContext,
        plan: &mut pipeline::Pipeline,
    ) -> Result<Store> {
        self.create_candidate_store_with_mode(context, plan, false)
    }
    /// An ephemeral candidate must be kept in a private session repository.
    /// Its observations may be saved only via the regular sealed publication.
    pub fn create_fast_candidate_store(
        &self,
        context: AcquisitionContext,
        plan: &mut pipeline::Pipeline,
    ) -> Result<Store> {
        self.create_candidate_store_with_mode(context, plan, true)
    }
    fn create_candidate_store_with_mode(
        &self,
        context: AcquisitionContext,
        plan: &mut pipeline::Pipeline,
        fast: bool,
    ) -> Result<Store> {
        self.require_writer()?;
        self.require_allocation(context.generation)?;
        if context.inventory != self.inventory || context.source != self.source {
            return Err(Error::Contract("candidate repository identity mismatch"));
        }
        self.admit_candidate_creation(context.generation)?;
        let free = sortsize_core::resources::native_free_disk(&self.directory)
            .ok_or(Error::Contract("candidate volume free space unavailable"))?;
        let mut store = Store::create_canonical_admitted(
            &self.candidate_path(context.generation),
            self.inventory,
            self.source,
            if fast {
                Durability::Reconstructible
            } else {
                Durability::Retained
            },
            plan.writer()?,
            sortsize_core::resources::disk_limit(free),
            None,
        )?;
        store.fast_writer = fast;
        Ok(store)
    }
    pub fn open_candidate(&self, g: GenerationId, plan: &mut pipeline::Pipeline) -> Result<Store> {
        self.open_candidate_cancellable(g, plan, &Default::default())
    }
    pub fn open_candidate_cancellable(
        &self,
        g: GenerationId,
        plan: &mut pipeline::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Store> {
        self.require_writer()?;
        self.require_allocation(g)?;
        if self
            .directory
            .join(format!(".pending-adopt-{:016x}", g.0))
            .exists()
        {
            return Err(Error::Contract("incomplete candidate adoption preserved"));
        }
        let path = self.candidate_path(g);
        let free = sortsize_core::resources::native_free_disk(&path)
            .ok_or(Error::Contract("candidate volume free space unavailable"))?;
        let store = Store::reopen_canonical_cancellable_admitted(
            &path,
            Durability::Retained,
            plan.writer()?,
            sortsize_core::resources::disk_limit(free),
            cancel,
        )?;
        if store.inventory != self.inventory
            || store.source != self.source
            || store.acquisition_context(g)?.generation != g
        {
            return Err(Error::Contract(
                "reopened candidate repository identity mismatch",
            ));
        }
        Ok(store)
    }
    pub fn published(&self, g: GenerationId) -> Result<Option<Descriptor>> {
        let path = self
            .directory
            .join(format!("measurement-{:016x}.json", g.0));
        if !path.exists() {
            return Ok(None);
        }
        let disk: Measurement = read_json(&path)?;
        self.decode_measurement(disk, g).map(Some)
    }
    pub fn descriptors(&self) -> Result<Vec<Descriptor>> {
        let mut result = Vec::new();
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if let Some(g) =
                parse_name(&item.file_name().to_string_lossy(), "measurement-", ".json")
                && let Some(descriptor) = self.published(GenerationId(g))?
            {
                result.push(descriptor);
            }
            let bytes = result.capacity() * std::mem::size_of::<Descriptor>()
                + result
                    .iter()
                    .map(|d| d.directory.capacity() + d.root.capacity())
                    .sum::<usize>();
            if result.len() > 1024 || bytes > 512 * 1024 {
                return Err(Error::Contract("repository catalogue budget"));
            }
        }
        result.sort_by_key(|d| d.generation.0);
        Ok(result)
    }
    pub fn latest_context(&self) -> Result<Option<Descriptor>> {
        self.current_pointer()?
            .latest
            .map(GenerationId)
            .map(|g| self.published(g))
            .transpose()
            .map(Option::flatten)
    }
    pub fn open_latest(&self, cancel: &CancellationToken) -> Result<Option<CertifiedGeneration>> {
        self.current_pointer()?
            .latest
            .map(GenerationId)
            .map(|g| self.open_generation(g, cancel))
            .transpose()
    }
    pub fn open_latest_with_plan(
        &self,
        plan: &mut pipeline::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Option<CertifiedGeneration>> {
        let Some(g) = self.current_pointer()?.latest else {
            return Ok(None);
        };
        let descriptor = self
            .published(GenerationId(g))?
            .ok_or(Error::Contract("published repository descriptor missing"))?;
        let certification = Phase::CertifyReader.start();
        let cap = CertifiedGeneration::open_admitted(
            &descriptor.directory,
            descriptor.generation,
            &descriptor.root,
            cancel,
            plan.reader()?,
        )?;
        self.check_capability(&descriptor, &cap)?;
        drop(certification);
        Ok(Some(cap))
    }
    pub fn open_generation(
        &self,
        g: GenerationId,
        cancel: &CancellationToken,
    ) -> Result<CertifiedGeneration> {
        let descriptor = self
            .published(g)?
            .ok_or(Error::Contract("repository generation is not published"))?;
        let certification = Phase::CertifyReader.start();
        let cap = CertifiedGeneration::open(&descriptor.directory, g, &descriptor.root, cancel)?;
        self.check_capability(&descriptor, &cap)?;
        drop(certification);
        Ok(cap)
    }
    pub fn open_generation_with_plan(
        &self,
        g: GenerationId,
        plan: &mut pipeline::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<CertifiedGeneration> {
        let descriptor = self
            .published(g)?
            .ok_or(Error::Contract("repository generation is not published"))?;
        let certification = Phase::CertifyReader.start();
        let cap = CertifiedGeneration::open_admitted(
            &descriptor.directory,
            g,
            &descriptor.root,
            cancel,
            plan.reader()?,
        )?;
        self.check_capability(&descriptor, &cap)?;
        drop(certification);
        Ok(cap)
    }
    pub fn discard_candidate(&mut self, g: GenerationId, cancel: &CancellationToken) -> Result<()> {
        self.require_writer()?;
        let descriptor = self
            .published(g)?
            .ok_or(Error::Contract("cannot discard unpublished candidate"))?;
        let memory = Governor::global()
            .reserve(Category::Sqlite, FRAME, None)
            .ok_or(Error::Contract("discard publication certificate admission"))?;
        let certification = Phase::CertifyCleanup.start();
        let cap = CertifiedGeneration::open_admitted(
            &descriptor.directory,
            g,
            &descriptor.root,
            cancel,
            memory,
        )?;
        self.check_capability(&descriptor, &cap)?;
        drop(certification);
        let path = self.candidate_path(g);
        if path.exists() {
            let source = {
                // Includes ownership/schema/context checks around the nested
                // integrity_check. This envelope is not a pure SQLite timer.
                let _timer = Phase::CleanupCandidateIntegrity.start();
                self.validate_owned_group(&path, cancel)?
            };
            if source.acquisition_context(g)? != descriptor.context {
                return Err(Error::Contract(
                    "discard candidate context differs from publication",
                ));
            }
            let comparison = Phase::CleanupCompareObservation.start();
            if !source.same_generation_observation(cap.as_store(), g, g, cancel)? {
                return Err(Error::Contract(
                    "discard candidate observation differs from publication",
                ));
            }
            drop(comparison);
            remove_owned_group(&path, source)?;
        }
        self.account()?;
        Ok(())
    }
    pub fn publish(
        &mut self,
        source: &Store,
        g: GenerationId,
        root: &Path,
        max_growth: u64,
        cancel: &CancellationToken,
    ) -> Result<PublicationOutcome> {
        self.publish_inner(
            source,
            g,
            root,
            PublicationGrowth::Standard(max_growth),
            cancel,
            None,
        )
    }
    pub fn publish_with_plan(
        &mut self,
        source: &Store,
        g: GenerationId,
        root: &Path,
        max_growth: u64,
        cancel: &CancellationToken,
        plan: &mut pipeline::Pipeline,
    ) -> Result<PublicationOutcome> {
        self.publish_inner(
            source,
            g,
            root,
            PublicationGrowth::Standard(max_growth),
            cancel,
            Some(plan.publication()?),
        )
    }
    /// Publish and transfer the destination's certified, pinned RO connection.
    /// Committed always carries Some(reader); an uncommitted catalogue never
    /// exposes a reader. No stage certificate crosses the rename boundary.
    /// Both planned slots are consumed once work starts; errors release their
    /// owners and preserve physical artifacts for the existing recovery path.
    pub fn publish_with_plan_and_reader(
        &mut self,
        source: &Store,
        g: GenerationId,
        root: &Path,
        max_growth: u64,
        cancel: &CancellationToken,
        plan: &mut pipeline::Pipeline,
    ) -> Result<(PublicationOutcome, Option<CertifiedGeneration>)> {
        cancelled(cancel)?;
        let (builder, reader) = plan.publication_and_reader()?;
        self.publish_inner_with_reader(
            source,
            g,
            root,
            PublicationGrowth::Standard(max_growth),
            cancel,
            PublicationSlots {
                builder: Some(builder),
                reader: Some(reader),
            },
        )
    }
    fn publish_inner(
        &mut self,
        source: &Store,
        g: GenerationId,
        root: &Path,
        growth: PublicationGrowth,
        cancel: &CancellationToken,
        memory: Option<Reservation>,
    ) -> Result<PublicationOutcome> {
        self.publish_inner_with_reader(
            source,
            g,
            root,
            growth,
            cancel,
            PublicationSlots {
                builder: memory,
                reader: None,
            },
        )
        .map(|(outcome, _)| outcome)
    }
    fn publish_inner_with_reader(
        &mut self,
        source: &Store,
        g: GenerationId,
        root: &Path,
        growth: PublicationGrowth,
        cancel: &CancellationToken,
        slots: PublicationSlots,
    ) -> Result<(PublicationOutcome, Option<CertifiedGeneration>)> {
        let transfer_reader = slots.reader.is_some();
        self.require_writer()?;
        self.require_allocation(g)?;
        if source.inventory != self.inventory || source.source != self.source {
            return Err(Error::Contract("repository publication identity mismatch"));
        }
        let destination = self.generation_path(g);
        if destination.exists() {
            return Err(Error::Contract(
                "repository publication already exists; reconcile explicitly",
            ));
        }
        self.admit_publication(g)?;
        let report =
            match publish_admitted(source, &destination, g, root, growth, cancel, slots.builder) {
                Ok(report) => report,
                Err(error) if destination.exists() => {
                    let certification = Phase::CertifyDestination.start();
                    let cap = match slots.reader {
                        Some(memory) => CertifiedGeneration::open_admitted(
                            &destination,
                            g,
                            root,
                            cancel,
                            memory,
                        )?,
                        None => CertifiedGeneration::open(&destination, g, root, cancel)?,
                    };
                    let descriptor = self.descriptor(&cap)?;
                    drop(certification);
                    let report = BuildReport {
                        entries: cap.rows,
                        oversized_paths: cap.oversized,
                        artifact_bytes: lifecycle::artifact_bytes(&destination)?,
                    };
                    return Ok((
                        PublicationOutcome::PublishedUncommitted {
                            descriptor,
                            report,
                            error,
                        },
                        None,
                    ));
                }
                Err(error) => return Err(error),
            };
        let certification = Phase::CertifyDestination.start();
        let cap = match slots.reader {
            Some(memory) => {
                CertifiedGeneration::open_admitted(&destination, g, root, cancel, memory)?
            }
            None => CertifiedGeneration::open(&destination, g, root, cancel)?,
        };
        let descriptor = self.descriptor(&cap)?;
        drop(certification);
        // The old APIs still close the certificate before catalogue writes.
        // Only the additive handoff retains the exact connection and lease.
        let reader = if transfer_reader {
            #[cfg(test)]
            publication_reader_tests::hook("certified", self, &cap);
            cancelled(cancel)?;
            Some(cap)
        } else {
            drop(cap);
            None
        };
        let commit = (|| -> Result<()> {
            let _timer = Phase::CatalogueCommit.start();
            self.write_measurement(&descriptor)?;
            self.select(&descriptor)?;
            self.account()?;
            if let Some(cap) = reader.as_ref() {
                #[cfg(test)]
                publication_reader_tests::hook("catalogue_committed", self, cap);
                cancelled(cancel)?;
                // A writable Repository caches CURRENT for ordinary reads.
                // Handoff requires reading the committed document itself.
                let current: Current = read_json(&self.directory.join("CURRENT.json"))?;
                if current.version != REPOSITORY_VERSION
                    || current.latest != Some(g.0)
                    || current.latest != self.current.latest
                    || current.last_complete != self.current.last_complete
                {
                    return Err(Error::Contract(
                        "published reader is not the committed selection",
                    ));
                }
                let committed = self.published(g)?.ok_or(Error::Contract(
                    "published reader descriptor missing after catalogue commit",
                ))?;
                self.check_capability(&committed, cap)?;
                cancelled(cancel)?;
            }
            Ok(())
        })();
        match commit {
            Ok(()) => Ok((PublicationOutcome::Committed { descriptor, report }, reader)),
            Err(error) => Ok((
                PublicationOutcome::PublishedUncommitted {
                    descriptor,
                    report,
                    error,
                },
                None,
            )),
        }
    }
    /// Explicit legacy Resume import. An incomplete extraction is never exposed
    /// as a resumable candidate; its durable marker is preserved for diagnosis.
    pub fn adopt_candidate(
        &mut self,
        legacy: &Store,
        g: GenerationId,
        plan: &mut pipeline::Pipeline,
    ) -> Result<Store> {
        self.adopt_candidate_cancellable(legacy, g, plan, &Default::default())
    }
    pub fn adopt_candidate_cancellable(
        &mut self,
        legacy: &Store,
        g: GenerationId,
        plan: &mut pipeline::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Store> {
        self.require_writer()?;
        legacy.require_integrity()?;
        cancelled(cancel)?;
        let retrying = self.prepare_adoption_retry(legacy, g, plan, cancel)?;
        self.reserve_legacy_ids(legacy)?;
        let context = legacy.acquisition_context(g)?;
        let sealed: bool = legacy.db.query_row(
            "SELECT sealed FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| r.get(0),
        )?;
        if sealed {
            return Err(Error::Contract(
                "legacy Resume requires an unsealed candidate",
            ));
        }
        if self.candidate_path(g).exists() || self.generation_path(g).exists() {
            return Err(Error::Contract(
                "candidate adoption destination already exists",
            ));
        }
        let marker = format!(".pending-adopt-{:016x}", g.0);
        if !retrying {
            write_json(
                &self.directory,
                &marker,
                &Allocation {
                    version: REPOSITORY_VERSION,
                    generation: g.0,
                },
                false,
            )?;
        }
        let mut candidate = self.create_candidate_store(context, plan)?;
        let stop = cancel.clone();
        candidate
            .db
            .progress_handler(1000, Some(move || stop.is_cancelled()))?;
        extraction::extract(legacy, &mut candidate, g, cancel)?;
        cancelled(cancel)?;
        let check: String = candidate
            .db
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" || candidate.acquisition_context(g)? != context {
            return Err(Error::Contract(
                "adopted candidate failed full qualification",
            ));
        }
        candidate
            .db
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        sync_group(candidate.directory())?;
        cancelled(cancel)?;
        candidate.db.progress_handler(0, None::<fn() -> bool>)?;
        std::fs::remove_file(self.directory.join(marker))?;
        lifecycle::sync_directory(&self.directory)?;
        self.account()?;
        Ok(candidate)
    }
    /// Adoption is a copy/extraction publication. It never migrates or writes to
    /// the legacy Store. Its qualified IDs must already match this inventory.
    pub fn adopt_legacy(
        &mut self,
        legacy: &Store,
        g: GenerationId,
        root: &Path,
        max_growth: u64,
        cancel: &CancellationToken,
    ) -> Result<PublicationOutcome> {
        if legacy.inventory != self.inventory || legacy.source != self.source {
            return Err(Error::Contract(
                "legacy adoption repository identity mismatch",
            ));
        }
        self.reserve_legacy_ids(legacy)?;
        self.publish(legacy, g, root, max_growth, cancel)
    }
    pub fn adopt_legacy_with_plan(
        &mut self,
        legacy: &Store,
        g: GenerationId,
        root: &Path,
        max_growth: u64,
        cancel: &CancellationToken,
        plan: &mut pipeline::Pipeline,
    ) -> Result<PublicationOutcome> {
        if legacy.inventory != self.inventory || legacy.source != self.source {
            return Err(Error::Contract(
                "legacy adoption repository identity mismatch",
            ));
        }
        self.reserve_legacy_ids(legacy)?;
        self.publish_with_plan(legacy, g, root, max_growth, cancel, plan)
    }

    /// Use the same physical certificate with a disposable model index whose
    /// ranked keys can be consumed during publication. Only a fresh replay can
    /// opt into this staged policy.
    pub(crate) fn adopt_direct_replay_with_plan(
        &mut self,
        source: &crate::inventory::direct_replay::DirectReplay,
        g: GenerationId,
        root: &Path,
        max_growth: u64,
        cancel: &CancellationToken,
        plan: &mut pipeline::Pipeline,
    ) -> Result<PublicationOutcome> {
        self.reserve_legacy_ids(source)?;
        self.publish_inner(
            source,
            g,
            root,
            PublicationGrowth::DirectReplay(max_growth),
            cancel,
            Some(plan.publication()?),
        )
    }
    /// Reconstruct selection only on explicit request, certifying every known
    /// published group. Invalid/future groups are preserved and abort recovery.
    pub fn recover_catalogue(&mut self, cancel: &CancellationToken) -> Result<()> {
        self.require_writer()?;
        let mut descriptors = self.descriptors()?;
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if let Some(g) = parse_name(&item.file_name().to_string_lossy(), "generation-", "") {
                let g = GenerationId(g);
                if descriptors.iter().any(|d| d.generation == g) {
                    continue;
                }
                let inspected = Store::inspect(&item.path())?;
                let raw: Vec<u8> = inspected.db.query_row(
                    "SELECT root FROM generation_a_meta WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                let root = decode_path(&raw)?;
                drop(inspected);
                let cap = CertifiedGeneration::open(&item.path(), g, &root, cancel)?;
                let descriptor = self.descriptor(&cap)?;
                drop(cap);
                self.write_measurement(&descriptor)?;
                descriptors.push(descriptor);
            }
        }
        descriptors.sort_by_key(|d| d.generation.0);
        for d in &descriptors {
            let cap = self.open_generation(d.generation, cancel)?;
            drop(cap);
        }
        let current = Current {
            version: REPOSITORY_VERSION,
            latest: descriptors.last().map(|d| d.generation.0),
            last_complete: descriptors
                .iter()
                .rev()
                .find(|d| d.coverage == ScopeCoverage::Complete)
                .map(|d| d.generation.0),
        };
        write_json(&self.directory, "CURRENT.json", &current, true)?;
        self.current = current;
        self.account()?;
        Ok(())
    }
    /// Stable estimate, not a proof of maximum growth. Admission remains the
    /// global governor's authority and individual writes may still refuse.
    pub fn publication_budget(source: &Store) -> Result<u64> {
        source.disk_admission(0)?;
        let base = lifecycle::artifact_bytes(source.directory())?;
        let usage = Governor::global().usage();
        let free = sortsize_core::resources::native_free_disk(source.directory())
            .ok_or(Error::Contract("store free space unavailable"))?;
        let committed = usage
            .disk_used
            .checked_add(usage.disk_reserved)
            .ok_or(Error::Contract("publication estimate overflow"))?;
        publication_growth_budget(base, committed, free)
    }
    pub fn account(&self) -> Result<u64> {
        validate_layout(&self.directory)?;
        let governor = Governor::global();
        let free = sortsize_core::resources::native_free_disk(&self.directory)
            .ok_or(Error::Contract("repository free capacity unavailable"))?;
        let mut metadata_bytes = 0u64;
        let mut total = 0u64;
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            let m = std::fs::symlink_metadata(item.path())?;
            let bytes = if m.is_dir() {
                let bytes = lifecycle::artifact_bytes(&item.path())?;
                if !governor.observe_artifact_group(&item.path(), free, bytes) {
                    return Err(Error::Contract("repository artifact registry budget"));
                }
                bytes
            } else {
                metadata_bytes = metadata_bytes
                    .checked_add(m.len())
                    .ok_or(Error::Contract("repository byte overflow"))?;
                m.len()
            };
            total = total
                .checked_add(bytes)
                .ok_or(Error::Contract("repository byte overflow"))?;
        }
        if !governor.observe_artifact_group(&self.directory, free, metadata_bytes) {
            return Err(Error::Contract("repository metadata registry budget"));
        }
        Ok(total)
    }
    pub fn purge(&mut self, g: GenerationId, cancel: &CancellationToken) -> Result<()> {
        self.require_writer()?;
        if self.current.latest == Some(g.0) || self.current.last_complete == Some(g.0) {
            return Err(Error::Contract("repository reference generation protected"));
        }
        let mut stores = Vec::new();
        for path in [self.generation_path(g), self.candidate_path(g)] {
            if path.exists() {
                stores.push((path.clone(), self.validate_owned_group(&path, cancel)?));
            }
        }
        for (path, store) in stores {
            remove_owned_group(&path, store)?;
        }
        let descriptor = self
            .directory
            .join(format!("measurement-{:016x}.json", g.0));
        if descriptor.exists() {
            std::fs::remove_file(descriptor)?;
        }
        lifecycle::sync_directory(&self.directory)?;
        self.account()?;
        Ok(())
    }
    pub fn prune_unprotected(&mut self, cancel: &CancellationToken) -> Result<usize> {
        let mut count = 0;
        for descriptor in self.descriptors()? {
            if self.current.latest != Some(descriptor.generation.0)
                && self.current.last_complete != Some(descriptor.generation.0)
            {
                self.purge(descriptor.generation, cancel)?;
                count += 1;
            }
        }
        Ok(count)
    }
    /// Explicit owner cleanup, primarily for a non-retained temporary repository.
    /// Any active generation/candidate lease refuses deletion.
    pub fn forget(self, cancel: &CancellationToken) -> Result<()> {
        self.require_writer()?;
        validate_layout(&self.directory)?;
        self.validate_policy_documents()?;
        let mut groups = Vec::new();
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if item.file_type()?.is_dir() {
                groups.push((
                    item.path(),
                    self.validate_owned_group(&item.path(), cancel)?,
                ));
            }
        }
        // Validate and acquire every generation lease before deleting anything.
        crate::inventory::owner::preflight_removal(&self.directory)?;
        for (path, store) in groups {
            cancelled(cancel)?;
            remove_owned_group(&path, store)?;
        }
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if item.file_name() != "owner.lock" {
                std::fs::remove_file(item.path())?;
            }
        }
        lifecycle::sync_directory(&self.directory)?;
        Governor::global().forget_deleted_artifact_group(&self.directory);
        // Retain the stable repository lock inode. TempDir may remove it after
        // this owner drops; retained repositories are never forgotten implicitly.
        Ok(())
    }
    fn require_writer(&self) -> Result<()> {
        if self.read_only {
            Err(Error::Contract(
                "read-only repository cannot allocate, migrate, publish or purge",
            ))
        } else {
            Ok(())
        }
    }
    fn current_pointer(&self) -> Result<Current> {
        if !self.read_only {
            return Ok(self.current.clone());
        }
        let current: Current = read_json(&self.directory.join("CURRENT.json"))?;
        if current.version != REPOSITORY_VERSION {
            return Err(Error::Contract("future repository catalogue preserved"));
        }
        Ok(current)
    }
    fn require_allocation(&self, g: GenerationId) -> Result<()> {
        let record: Allocation =
            read_json(&self.directory.join(format!("allocation-{:016x}.json", g.0)))?;
        if record.version != REPOSITORY_VERSION || record.generation != g.0 {
            return Err(Error::Contract("repository allocation receipt mismatch"));
        }
        Ok(())
    }
    fn import_allocation(&mut self, g: GenerationId) -> Result<()> {
        self.require_writer()?;
        let path = self.directory.join(format!("allocation-{:016x}.json", g.0));
        if path.exists() {
            return self.require_allocation(g);
        }
        let state: State = read_json(&self.directory.join("STATE.json"))?;
        if state.version != REPOSITORY_VERSION {
            return Err(Error::Contract(
                "legacy generation allocation would reuse identity",
            ));
        }
        let next = g.0.saturating_add(1);
        write_json(
            &self.directory,
            "STATE.json",
            &State {
                version: REPOSITORY_VERSION,
                next_generation: next.max(state.next_generation),
                exhausted: state.exhausted || g.0 == u64::MAX,
            },
            true,
        )?;
        write_json(
            &self.directory,
            &format!("allocation-{:016x}.json", g.0),
            &Allocation {
                version: REPOSITORY_VERSION,
                generation: g.0,
            },
            false,
        )
    }
    fn validate_pointer(&self) -> Result<()> {
        for g in [self.current.latest, self.current.last_complete]
            .into_iter()
            .flatten()
        {
            if self.published(GenerationId(g))?.is_none() {
                return Err(Error::Contract(
                    "repository pointer requires explicit recovery",
                ));
            }
        }
        Ok(())
    }
    fn decode_measurement(&self, m: Measurement, g: GenerationId) -> Result<Descriptor> {
        if m.version != REPOSITORY_VERSION || m.generation != g.0 {
            return Err(Error::Contract("future repository descriptor preserved"));
        }
        let context = decode_context(&m.context)?;
        if context.inventory != self.inventory
            || context.source != self.source
            || context.generation != g
        {
            return Err(Error::Contract("repository descriptor identity mismatch"));
        }
        Ok(Descriptor {
            directory: self.generation_path(g),
            generation: g,
            root: decode_path(&m.root)?,
            context,
            coverage: if m.complete {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
            started: m.started,
            measured: m.measured,
        })
    }
    fn descriptor(&self, cap: &CertifiedGeneration) -> Result<Descriptor> {
        let store = cap.as_store();
        let context = store.acquisition_context(cap.generation())?;
        if context.inventory != self.inventory || context.source != self.source {
            return Err(Error::Contract("published repository context mismatch"));
        }
        let version: i64 = store
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        let (started, measured) = if version >= 3 {
            store
                .db
                .query_row(
                    "SELECT started,measured FROM measurements WHERE generation=?1",
                    [unsigned(cap.generation().0)],
                    |r| Ok((r.get::<_, i64>(0)?, Some(r.get::<_, i64>(1)?))),
                )
                .optional()?
                .unwrap_or((0, None))
        } else {
            (0, None)
        };
        Ok(Descriptor {
            directory: cap.directory().into(),
            generation: cap.generation(),
            root: cap.root().into(),
            context,
            coverage: cap.coverage(),
            started,
            measured,
        })
    }
    fn check_capability(&self, descriptor: &Descriptor, cap: &CertifiedGeneration) -> Result<()> {
        let actual = self.descriptor(cap)?;
        if actual.context != descriptor.context
            || actual.root != descriptor.root
            || actual.coverage != descriptor.coverage
            || actual.started != descriptor.started
            || actual.measured != descriptor.measured
        {
            return Err(Error::Contract(
                "repository descriptor disagrees with certified generation",
            ));
        }
        Ok(())
    }
    fn write_measurement(&self, d: &Descriptor) -> Result<()> {
        write_json(
            &self.directory,
            &format!("measurement-{:016x}.json", d.generation.0),
            &Measurement {
                version: REPOSITORY_VERSION,
                generation: d.generation.0,
                root: queries::native_path(&d.root).encode(),
                context: context(d.context),
                complete: d.coverage == ScopeCoverage::Complete,
                started: d.started,
                measured: d.measured,
            },
            false,
        )
    }
    fn select(&mut self, d: &Descriptor) -> Result<()> {
        let current = Current {
            version: REPOSITORY_VERSION,
            latest: self.current.latest.max(Some(d.generation.0)),
            last_complete: if d.coverage == ScopeCoverage::Complete {
                self.current.last_complete.max(Some(d.generation.0))
            } else {
                self.current.last_complete
            },
        };
        write_json(&self.directory, "CURRENT.json", &current, true)?;
        self.current = current;
        Ok(())
    }
}

fn validate_identity(i: &Identity, inventory: InventoryId, source: SourceId) -> Result<()> {
    if i.version != REPOSITORY_VERSION
        || i.inventory != inventory.0.to_be_bytes()
        || i.source != source.0.to_be_bytes()
    {
        return Err(Error::Contract(
            "repository identity/future format preserved",
        ));
    }
    Ok(())
}
fn parse_name(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let value = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if value.len() != 16 {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}
fn decode_path(raw: &[u8]) -> Result<PathBuf> {
    if raw.len() > 65538 {
        return Err(Error::Contract("repository root byte budget"));
    }
    let path: PathBuf = crate::scheduler::native_name(
        &NativeName::decode(raw).map_err(|_| Error::Contract("repository native root codec"))?,
    )?
    .into();
    if !path.is_absolute() {
        return Err(Error::Contract("repository root must be absolute"));
    }
    Ok(path)
}
fn decode_context(raw: &[u8]) -> Result<AcquisitionContext> {
    if raw.len() != 81 || raw[0] != 1 {
        return Err(Error::Contract("repository context codec"));
    }
    let n = |o| u64::from_be_bytes(raw[o..o + 8].try_into().unwrap());
    Ok(AcquisitionContext {
        inventory: InventoryId(u128::from_be_bytes(raw[1..17].try_into().unwrap())),
        source: SourceId(u128::from_be_bytes(raw[17..33].try_into().unwrap())),
        generation: GenerationId(n(33)),
        scan: ScanId(n(41)),
        policy: PolicyId(n(49)),
        mount: MountEpoch(n(57)),
        run: RunEpoch(n(65)),
        validation: ValidationEpoch(n(73)),
    })
}
/// Only a genuinely empty initialization target may create its owner lock.
/// Existing repository metadata is never changed to acquire recovery ownership.
fn lock_repository(path: &Path, initialize_empty: bool) -> Result<(File, bool)> {
    if initialize_empty {
        lifecycle::ensure_root(path)?;
    } else {
        lifecycle::check_directory(path)?;
    }
    let empty = std::fs::read_dir(path)?.next().transpose()?.is_none();
    let lock_path = path.join("owner.lock");
    let initializing = initialize_empty && empty;
    let file = if initializing {
        // create_new closes the race with another initializer without truncating
        // any existing lock or opening a substituted symlink.
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)?
    } else {
        let metadata = std::fs::symlink_metadata(&lock_path)?;
        if !metadata.is_file() || metadata.is_symlink() || metadata.len() != 0 {
            return Err(Error::Contract("repository owner lock metadata preserved"));
        }
        let file = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        let opened = file.metadata()?;
        let current = std::fs::symlink_metadata(&lock_path)?;
        if !opened.is_file()
            || opened.len() != 0
            || !current.is_file()
            || current.is_symlink()
            || current.len() != 0
        {
            return Err(Error::Contract(
                "repository owner lock changed during opening",
            ));
        }
        file
    };
    file.try_lock()
        .map_err(|_| Error::Contract("repository already owned or native lock unavailable"))?;
    Ok((file, initializing))
}
fn validate_layout(path: &Path) -> Result<()> {
    let mut count = 0;
    for item in std::fs::read_dir(path)? {
        count += 1;
        if count > DIRECTORY_LIMIT {
            return Err(Error::Contract("repository directory budget"));
        }
        let item = item?;
        let metadata = std::fs::symlink_metadata(item.path())?;
        if metadata.is_symlink() {
            return Err(Error::Contract("repository link preserved"));
        }
        let name = item.file_name();
        if crate::inventory::owner::is_anchor_name(&name) {
            crate::inventory::owner::validate_anchor(&item.path())?;
            continue;
        }
        let name = name
            .to_str()
            .ok_or(Error::Contract("repository foreign native name"))?;
        let file = matches!(
            name,
            "owner.lock"
                | "IDENTITY.json"
                | "STATE.json"
                | "CURRENT.json"
                | "POLICY.json"
                | "ORIGIN.json"
        ) || parse_name(name, "allocation-", ".json").is_some()
            || parse_name(name, "measurement-", ".json").is_some()
            || name.starts_with(".pending-");
        let directory = parse_name(name, "candidate-", "").is_some()
            || parse_name(name, "generation-", "").is_some()
            || name.starts_with(".stage-a-")
            || name.starts_with(".quarantine-a-");
        if !((file && metadata.is_file()) || (directory && metadata.is_dir())) {
            return Err(Error::Contract(
                "unrecognized repository artifact preserved",
            ));
        }
    }
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    read_json_in(path, &Governor::global())
}
fn read_json_in<T: serde::de::DeserializeOwned>(path: &Path, governor: &Governor) -> Result<T> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_file() || m.is_symlink() || m.len() > DOCUMENT_LIMIT {
        return Err(Error::Contract("repository document type/size"));
    }
    // Account input, decoded capacities and parser work before allocating.
    // The fixed 512 KiB also covers ORIGIN's four bounded native-byte vectors
    // (4 * 65_538) even when their decoder preallocates from a tiny document.
    // The lease is temporary; decoded metadata is bounded by its owner.
    let admitted = m
        .len()
        .checked_mul(3)
        .and_then(|n| n.checked_add(512 * 1024))
        .ok_or(Error::Contract("repository document memory overflow"))?;
    let _memory = governor
        .reserve(Category::Transport, admitted, None)
        .or_else(|| governor.reserve(Category::Margin, admitted, None))
        .ok_or(Error::Contract("repository document memory admission"))?;
    let length =
        usize::try_from(m.len()).map_err(|_| Error::Contract("repository document length"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Contract("repository document allocation refused"))?;
    bytes.resize(length, 0);
    let mut file = File::open(path)?;
    file.read_exact(&mut bytes)?;
    // A growing file cannot make Vec/read_to_end exceed the admitted length.
    if file.read(&mut [0u8; 1])? != 0 {
        return Err(Error::Contract("repository document changed while reading"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Contract("repository document invalid; explicit recovery required"))
}
fn write_json<T: Serialize>(directory: &Path, name: &str, value: &T, replace: bool) -> Result<()> {
    let destination = directory.join(name);
    if let Ok(m) = std::fs::symlink_metadata(&destination) {
        if !m.is_file() || m.is_symlink() {
            return Err(Error::Contract("repository document destination preserved"));
        }
        if !replace {
            return Err(Error::Contract(
                "repository immutable document already exists",
            ));
        }
    }
    let bytes = serde_json::to_vec(value)
        .map_err(|_| Error::Contract("repository document serialization"))?;
    if bytes.len() as u64 > DOCUMENT_LIMIT {
        return Err(Error::Contract("repository document exceeds byte budget"));
    }
    let governor = Governor::global();
    let free = sortsize_core::resources::native_free_disk(directory)
        .ok_or(Error::Contract("repository free space unavailable"))?;
    if !governor.observe_artifact_group(directory, free, metadata_bytes(directory)?) {
        return Err(Error::Contract("repository document registry budget"));
    }
    let _disk = governor
        .reserve_disk(2 * DOCUMENT_LIMIT)
        .ok_or(Error::Contract("repository document disk admission"))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".pending-")
        .tempfile_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_NORMAL, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            SetFileAttributesW,
        };
        let a: Vec<u16> = temporary
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let b: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // Preserve tempfile's noclobber normalization: a published document
        // must not retain FILE_ATTRIBUTE_TEMPORARY. Data is already flushed.
        if !replace && unsafe { SetFileAttributesW(a.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        // SAFETY: both terminated native paths live throughout the call. No
        // REPLACE flag on create-new: a racing destination is preserved too.
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), flags) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        temporary.disable_cleanup(true);
    }
    #[cfg(not(windows))]
    {
        if replace {
            std::fs::rename(temporary.path(), &destination)?;
        } else {
            temporary
                .persist_noclobber(&destination)
                .map_err(|e| Error::Io(e.error))?;
        }
    }
    lifecycle::sync_directory(directory)?;
    Ok(())
}
fn metadata_bytes(directory: &Path) -> Result<u64> {
    let mut total = 0u64;
    for item in std::fs::read_dir(directory)? {
        let item = item?;
        let m = std::fs::symlink_metadata(item.path())?;
        if m.is_file() && !m.is_symlink() {
            total = total
                .checked_add(m.len())
                .ok_or(Error::Contract("repository metadata accounting overflow"))?;
        }
    }
    Ok(total)
}
fn remove_owned_group(path: &Path, store: Store) -> Result<()> {
    let Store { db, _owner, .. } = store;
    db.close().map_err(|(_, error)| Error::Sql(error))?;
    let owner = std::sync::Arc::try_unwrap(_owner)
        .map_err(|_| Error::Contract("live preview still owns candidate lease"))?;
    remove_locked_files(path, owner)
}
fn remove_locked_files(path: &Path, owner: File) -> Result<()> {
    lifecycle::remove_canonical_temporaries(path)?;
    for name in [
        "inventory.db-journal",
        "inventory.db-wal",
        "inventory.db-shm",
        "inventory.db",
    ] {
        let file = path.join(name);
        if file.exists() {
            std::fs::remove_file(file)?;
        }
    }
    lifecycle::sync_directory(path)?;
    std::fs::remove_file(path.join("owner.lock"))?;
    std::fs::remove_dir(path)?;
    crate::inventory::owner::remove_after_group(path, &owner)?;
    drop(owner);
    lifecycle::sync_directory(
        path.parent()
            .ok_or(Error::Contract("repository group parent"))?,
    )?;
    Governor::global().forget_deleted_artifact_group(path);
    Ok(())
}

#[cfg(test)]
mod metadata_tests;

#[cfg(test)]
mod write_json_tests;

#[cfg(test)]
mod reclaim_tests;

fn publication_growth_budget(base: u64, committed: u64, free: u64) -> Result<u64> {
    use sortsize_core::resources::disk_limit;
    // A reservation becomes physical writes while its unused portion remains
    // charged. Those writes also lower native free space and hence disk_limit.
    // Do not saturate today's quota and lose admission on mkdir or the next
    // page. Bound the whole plan against the quota after it materializes,
    // leaving one frame for stage/catalogue metadata. The monotone predicate
    // handles all three disk_limit branches without changing any global limit.
    let mut lower = 0;
    let mut upper = disk_limit(free).saturating_sub(committed);
    while lower < upper {
        let planned = lower + (upper - lower) / 2 + 1;
        let later_limit = disk_limit(free.saturating_sub(planned).saturating_sub(FRAME));
        if committed
            .checked_add(planned)
            .is_some_and(|total| total <= later_limit)
        {
            lower = planned;
        } else {
            upper = planned - 1;
        }
    }
    let overhead = base
        .checked_mul(2)
        .and_then(|n| n.checked_add(16 * FRAME))
        .ok_or(Error::Contract("publication estimate overflow"))?;
    let growth = base
        .checked_mul(8)
        .and_then(|n| n.checked_add(16 * FRAME))
        .ok_or(Error::Contract("publication estimate overflow"))?
        .min(lower.saturating_sub(overhead));
    if growth < FRAME {
        return Err(Error::Contract(
            "publication pipeline has insufficient global disk quota",
        ));
    }
    Ok(growth)
}

#[cfg(test)]
mod publication_budget_tests;
#[cfg(test)]
mod publication_reader_tests;
