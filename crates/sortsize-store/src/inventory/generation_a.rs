//! Physical durable generations, certified read leases, and owned publication.
//!
//! No GUI integration. Opening certifies the same pinned RO transaction that is
//! subsequently queried. Persistent rows/indexes are validated against the
//! canonical predicate input, not merely against SQLite's structural checker.
//! Canonical DB/WAL/SHM growth is admitted by the persistent mode of the quota
//! VFS before native I/O, under a global artifact guard. Filesystem metadata/COW
//! expansion and unrelated processes remain outside an absolute physical bound.
use super::*;
pub(super) mod canonical;
mod extraction;
mod model;
pub mod pipeline;
mod publication_batch;
pub mod repository;
pub use pipeline::Pipeline;
pub use repository::{Descriptor, PublicationOutcome, Repository};
mod search;
use model::{SQL, build_rows, require_model_tables, validate_semantics};
pub use search::GenerationQuery;
use sortsize_core::{
    CancellationToken,
    resources::{Category, Governor, Reservation},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

fn count(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
const VERSION: i64 = 2;
const FRAME: u64 = 1024 * 1024;
const PLATFORM: &str = if cfg!(windows) {
    "windows-native-v1"
} else {
    "posix-native-v1"
};
#[derive(Debug)]
pub struct BuildReport {
    pub entries: u64,
    pub oversized_paths: u64,
    pub artifact_bytes: u64,
}

fn cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Contract("generation A cancelled"))
    } else {
        Ok(())
    }
}
fn check_full(db: &Connection) -> Result<()> {
    let _timer = Phase::IntegrityCheck.start();
    let check: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::Contract("generation A corrupt group preserved"));
    }
    Ok(())
}
struct StageAccounting(PathBuf);
impl Drop for StageAccounting {
    fn drop(&mut self) {
        if let Ok(bytes) = lifecycle::artifact_bytes(&self.0) {
            Governor::global().observe_artifact_group(
                &self.0,
                sortsize_core::resources::native_free_disk(&self.0).unwrap_or(0),
                bytes,
            );
        }
    }
}
pub(super) fn release_store(store: Store) -> Result<Reservation> {
    let Store {
        db,
        _owner,
        _memory,
        _canonical,
        ..
    } = store;
    db.close().map_err(|(_, error)| Error::Sql(error))?;
    drop(_canonical);
    drop(_owner);
    Ok(_memory)
}
fn sync_group(dir: &Path) -> Result<()> {
    let _timer = Phase::SyncStage.start();
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("inventory.db"))?
        .sync_all()?;
    lifecycle::sync_directory(dir)
}
pub(super) fn publish_directory(from: &Path, to: &Path) -> Result<()> {
    let _timer = Phase::PublishRename.start();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: null-terminated names remain alive throughout the call.
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to)?;
    lifecycle::sync_directory(to.parent().ok_or(Error::Contract("generation A parent"))?)
}

#[derive(Clone, Copy)]
enum PublicationGrowth {
    Standard(u64),
    DirectReplay(u64),
}

impl PublicationGrowth {
    fn bytes(self) -> u64 {
        match self {
            Self::Standard(bytes) | Self::DirectReplay(bytes) => bytes,
        }
    }

    fn model_page_bytes(self) -> u64 {
        match self {
            Self::Standard(bytes) => bytes / 3,
            Self::DirectReplay(bytes) => bytes,
        }
    }

    fn consumes_keys(self) -> bool {
        matches!(self, Self::DirectReplay(_))
    }
}

/// Copy source observations into a physically independent staging database,
/// prepare indexes, validate, sync, and publish to a previously absent path.
/// Failed stages remain preserved and accounted. Canonical DB/WAL/SHM/journal
/// growth is rejected before native I/O by the explicit quota VFS. Filesystem
/// metadata and COW expansion are not an absolute physical volume guarantee.
pub fn publish(
    source: &Store,
    destination: &Path,
    generation: GenerationId,
    root: &Path,
    max_growth_bytes: u64,
    cancel: &CancellationToken,
) -> Result<BuildReport> {
    publish_admitted(
        source,
        destination,
        generation,
        root,
        PublicationGrowth::Standard(max_growth_bytes),
        cancel,
        None,
    )
}
fn publish_admitted(
    source: &Store,
    destination: &Path,
    generation: GenerationId,
    root: &Path,
    growth: PublicationGrowth,
    cancel: &CancellationToken,
    memory: Option<Reservation>,
) -> Result<BuildReport> {
    let _timer = Phase::Publish.start();
    let max_growth_bytes = growth.bytes();
    cancelled(cancel)?;
    source.require_integrity()?;
    if !root.is_absolute() || root.as_os_str().as_encoded_bytes().len() > 32 * 1024 {
        return Err(Error::Contract("generation A recorded root budget"));
    }
    if destination.exists() || max_growth_bytes < FRAME {
        return Err(Error::Contract("generation A destination/growth budget"));
    }
    let sealed: bool = source.db.query_row(
        "SELECT sealed FROM generations WHERE id=?1",
        [unsigned(generation.0)],
        |r| r.get(0),
    )?;
    if !sealed {
        return Err(Error::Contract("generation A requires sealed source"));
    }
    let parent = destination
        .parent()
        .ok_or(Error::Contract("generation A destination parent"))?;
    // Managed artifact keys must use the same native spelling as Store's
    // canonical VFS owner, including verbatim Windows prefixes. The recorded
    // source root stays byte-for-byte unchanged.
    let parent_path = std::fs::canonicalize(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })?;
    let parent = parent_path.as_path();
    let destination_path = parent.join(
        destination
            .file_name()
            .ok_or(Error::Contract("generation A destination name"))?,
    );
    let destination = destination_path.as_path();
    let stage = parent.join(format!(
        ".stage-a-{:032x}-{:016x}",
        source.inventory.0, generation.0
    ));
    if stage.exists() {
        return Err(Error::Contract("generation A previous stage preserved"));
    }
    let base = lifecycle::artifact_bytes(&source.directory)?;
    let planned = max_growth_bytes
        .checked_add(
            base.checked_mul(2)
                .ok_or(Error::Contract("generation A backup budget overflow"))?,
        )
        .and_then(|n| n.checked_add(16 * 1024 * 1024))
        .ok_or(Error::Contract("generation A backup budget overflow"))?;
    let disk = source.disk_admission(planned)?;
    let _accounting = StageAccounting(stage.clone());
    let memory = memory.map_or_else(Store::reserve_memory, Ok)?;
    let mut target = {
        let _timer = Phase::CreateStage.start();
        Store::create_canonical_admitted(
            &stage,
            source.inventory,
            source.source,
            Durability::Retained,
            memory,
            planned,
            Some(disk),
        )?
    };
    let stop = cancel.clone();
    target
        .db
        .progress_handler(1000, Some(move || stop.is_cancelled()))?;
    extraction::extract(source, &mut target, generation, cancel)?;
    {
        let _timer = Phase::IntegrityExtracted.start();
        check_full(&target.db)?;
    }
    {
        let _timer = Phase::CompareObservation.start();
        if !source.same_generation_observation(&target, generation, generation, cancel)? {
            return Err(Error::Contract(
                "publication canonical observation mismatch",
            ));
        }
    }
    let baseline = lifecycle::artifact_bytes(&stage)?;
    let page_size: u64 = target
        .db
        .pragma_query_value(None, "page_size", |r| count(r, 0))?;
    let pages: u64 = target
        .db
        .pragma_query_value(None, "page_count", |r| count(r, 0))?;
    // Only the direct replay stage can reclaim the pending index in the
    // transaction that installs each model window. Its SQLite page limit may
    // use the full logical growth envelope. The VFS still charges the whole
    // database, WAL, SHM and journals against the same admitted plan.
    let page_limit = pages
        .checked_add(growth.model_page_bytes() / page_size)
        .ok_or(Error::Contract("generation A growth overflow"))?;
    target.db.pragma_update(
        None,
        "max_page_count",
        page_limit.min(u64::from(u32::MAX)) as u32,
    )?;
    target.db.pragma_update(None, "synchronous", "FULL")?;
    {
        let _timer = Phase::CreateModel.start();
        target.db.execute_batch(SQL)?;
    }
    target.db.execute(
        "INSERT INTO generation_a_meta VALUES(1,?1,?2,?3,?4,'rust-analysis-v1',0,0,0)",
        params![
            VERSION,
            unsigned(generation.0),
            queries::native_path(root).encode(),
            PLATFORM
        ],
    )?;
    let memory = Governor::global()
        .reserve(Category::Queries, FRAME, None)
        .ok_or(Error::Contract("generation A build memory"))?;
    let result = build_rows(
        source,
        &mut target,
        generation,
        root,
        baseline,
        growth,
        cancel,
    );
    drop(memory);
    let (entries, oversized) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = target.disk_admission(0);
            return Err(error);
        }
    };
    cancelled(cancel)?;
    target.db.execute_batch("INSERT INTO generation_a_counts SELECT kind,count(*),sum(logical IS NULL),sum(ext_cs IS NULL) FROM generation_a_rows GROUP BY kind;")?;
    {
        let _timer = Phase::IntegrityModel.start();
        check_full(&target.db)?;
    }
    // Readiness is the final FULL transaction, never a filename or stale flag.
    target.db.execute(
        "UPDATE generation_a_meta SET rows=?1,oversized=?2,ready=1 WHERE singleton=1",
        params![
            i64::try_from(entries).map_err(|_| Error::Contract("generation A count overflow"))?,
            i64::try_from(oversized).map_err(|_| Error::Contract("generation A count overflow"))?
        ],
    )?;
    let (busy, _, _): (i64, i64, i64) = {
        let _timer = Phase::WalCheckpoint.start();
        target
            .db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
    };
    if busy != 0 {
        return Err(Error::Contract("generation A checkpoint busy"));
    }
    // Published G0 is a standalone, frozen SQLite file. Unlike G1's active
    // WAL, it needs no shared-memory or WAL reconstruction during RO refusal.
    let journal: String = {
        let _timer = Phase::StandaloneJournal.start();
        target
            .db
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?
    };
    if journal != "delete" {
        return Err(Error::Contract(
            "generation A standalone journal unavailable",
        ));
    }
    let memory = release_store(target)?;
    sync_group(&stage)?;
    let certified = {
        let _timer = Phase::CertifyStage.start();
        CertifiedGeneration::open_admitted(&stage, generation, root, cancel, memory)?
    };
    drop(certified);
    cancelled(cancel)?;
    #[cfg(test)]
    tests::publication_hook("before_publish");
    let publication = publish_directory(&stage, destination);
    let governor = Governor::global();
    let relocated = if !stage.exists() && destination.exists() {
        governor.relocate_artifact_group(&stage, destination)
    } else {
        false
    };
    publication?;
    if !relocated {
        return Err(Error::Contract(
            "published canonical ledger relocation requires recovery",
        ));
    }
    let bytes = lifecycle::artifact_bytes(destination)?;
    if !governor.observe_artifact_group(
        destination,
        sortsize_core::resources::native_free_disk(parent).unwrap_or(0),
        bytes,
    ) {
        return Err(Error::Contract(
            "generation A published group exceeds shared budget",
        ));
    }
    Ok(BuildReport {
        entries,
        oversized_paths: oversized,
        artifact_bytes: bytes,
    })
}

pub struct CertifiedGeneration {
    store: Store,
    generation: GenerationId,
    root: PathBuf,
    rows: u64,
    oversized: u64,
    coverage: ScopeCoverage,
    pub(super) query_cancel: Arc<Mutex<Option<CancellationToken>>>,
}
impl CertifiedGeneration {
    /// Complete structural AND semantic certification before READY. The RO
    /// transaction remains pinned for this capability's entire lifetime.
    pub fn open(
        directory: &Path,
        generation: GenerationId,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        cancelled(cancel)?;
        let governor = Governor::global();
        governor.refresh_native();
        Self::open_admitted(
            directory,
            generation,
            root,
            cancel,
            Self::reserve_reader_memory_in(&governor)?,
        )
    }
    /// RO capabilities use the same reader class as a prospective Pipeline,
    /// rather than greedily consuming the source writer's 8 MiB preference.
    fn reserve_reader_memory_in(governor: &Governor) -> Result<Reservation> {
        governor
            .reserve_up_to(Category::Sqlite, FRAME, FRAME + FRAME / 2)
            .ok_or_else(|| {
                let usage = governor.usage();
                let limit = usage.limits.categories[Category::Sqlite as usize];
                Error::MemoryAdmission {
                    minimum: FRAME,
                    available: limit.saturating_sub(usage.categories[Category::Sqlite as usize]),
                    category_limit: limit,
                }
            })
    }
    fn open_admitted(
        directory: &Path,
        generation: GenerationId,
        root: &Path,
        cancel: &CancellationToken,
        memory: Reservation,
    ) -> Result<Self> {
        let _timer = Phase::Certify.start();
        cancelled(cancel)?;
        let store = {
            let _timer = Phase::CertifyInspect.start();
            Store::inspect_admitted_shared(directory, cancel, memory)?
        };
        Self::certify_store(store, generation, root, cancel)
    }
    fn certify_store(
        mut store: Store,
        generation: GenerationId,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let stop = cancel.clone();
        store
            .db
            .progress_handler(1000, Some(move || stop.is_cancelled()))?;
        require_model_tables(&store.db)?;
        check_full(&store.db)?;
        let (version,stored_generation,stored_root,platform,predicate,rows,oversized,ready): (i64,Vec<u8>,Vec<u8>,String,String,u64,u64,bool) = store.db.query_row("SELECT version,generation,root,platform,predicate,rows,oversized,ready FROM generation_a_meta WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,count(r,5)?,count(r,6)?,r.get(7)?)))?;
        if version != VERSION
            || stored_generation != unsigned(generation.0)
            || stored_root != queries::native_path(root).encode()
            || platform != PLATFORM
            || predicate != "rust-analysis-v1"
            || !ready
        {
            return Err(Error::Contract(
                "generation A format/context/readiness mismatch",
            ));
        }
        let (sealed, coverage): (bool, i64) = store.db.query_row(
            "SELECT sealed,coverage FROM generations WHERE id=?1",
            [unsigned(generation.0)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !sealed
            || store
                .db
                .query_row::<i64, _, _>("SELECT count(*) FROM generations", [], |r| r.get(0))?
                != 1
        {
            return Err(Error::Contract(
                "generation A requires one sealed canonical generation",
            ));
        }
        let qualified = store.acquisition_context(generation)?;
        if qualified.inventory != store.inventory
            || qualified.source != store.source
            || qualified.generation != generation
        {
            return Err(Error::Contract(
                "certified generation qualified context mismatch",
            ));
        }
        let memory = Governor::global()
            .reserve(Category::Queries, FRAME, None)
            .ok_or(Error::Contract("generation A certification memory"))?;
        {
            let _timer = Phase::CertifySemantics.start();
            validate_semantics(&store, generation, root, rows, oversized, cancel)?;
        }
        drop(memory);
        cancelled(cancel)?;
        let query_cancel = Arc::new(Mutex::new(None::<CancellationToken>));
        let active_cancel = query_cancel.clone();
        store.db.progress_handler(
            1000,
            Some(move || {
                active_cancel
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            }),
        )?;
        store.integrity_verified = true;
        Ok(Self {
            store,
            generation,
            root: root.into(),
            rows,
            oversized,
            query_cancel,
            coverage: if coverage == 2 {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
        })
    }
    pub fn cancel_query(&self) {
        if let Some(cancel) = self
            .query_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            cancel.cancel();
        }
    }
    pub fn as_store(&self) -> &Store {
        &self.store
    }
    pub fn generation(&self) -> GenerationId {
        self.generation
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn coverage(&self) -> ScopeCoverage {
        self.coverage
    }
    pub fn directory(&self) -> &Path {
        &self.store.directory
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod storage_tests;
