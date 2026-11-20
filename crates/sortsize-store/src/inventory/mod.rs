//! Single-owner canonical inventory. No source acquisition and no UI calls.
//! All business state, including reconciliation/aggregation cursors, is durable.
//! Lifecycle opt-in, authorization, scheduling and query leases remain L03+.
use crate::{
    Durability,
    codec::{NativeName, unsigned},
    scan_metrics::{self as metrics, Phase},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sortsize_core::inventory_contract::*;
use std::{
    fmt,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub mod catalogue;
pub mod differential;
pub(crate) mod direct_replay;
pub mod folder_totals;
pub mod folder_tree;
pub mod generation_a;
pub mod lifecycle;
pub mod live_preview;
pub mod navigation;
pub mod ordered;
mod owner;
pub mod placement;
pub mod preview;
pub mod properties;
pub mod queries;
mod recovery;
mod scheduling;
pub(crate) mod snapshot_io;

const APPLICATION_ID: i64 = 1112758837;
const MAX_BATCH: usize = 512;
const MAX_BYTES: usize = 1024 * 1024;
const SCHEMA: &str = include_str!("schema.sql");
// SQLite 3.54 may prefer the primary-key range and visit unrelated parents
// until EOF. Pin the parent/id range so a resumable sweep is actually bounded.
const RECONCILE_CHILDREN: &str = "SELECT id,attempt FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT ?4";

#[derive(Debug)]
pub enum Error {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Contract(&'static str),
    MemoryAdmission {
        minimum: u64,
        available: u64,
        category_limit: u64,
    },
    DuplicateTemporaryLimit {
        limit: u64,
    },
    DuplicateResources(Box<queries::duplicates::CurrentFolderResourceUsage>),
    DiskAdmission(Box<DiskAdmission>),
    Batch(BatchRejection),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
            Self::Contract(e) => f.write_str(e),
            Self::MemoryAdmission {
                minimum,
                available,
                category_limit,
            } => write!(
                f,
                "Memory admission: minimum {minimum} bytes, available {available}, category limit {category_limit}; retry after resources recover"
            ),
            Self::DuplicateTemporaryLimit { limit } => {
                write!(f, "Duplicate temporary disk quota ({limit} bytes)")
            }
            Self::DuplicateResources(usage) => write!(
                f,
                "Duplicate resources: {:?} (memory {} bytes, temporary limit {} bytes, free {:?} bytes, reserve {} bytes)",
                usage.cause,
                usage.memory_reserved,
                usage.temporary_limit,
                usage.temporary_free,
                usage.temporary_reserve,
            ),
            Self::DiskAdmission(e) => e.fmt(f),
            Self::Batch(e) => write!(f, "batch rejected: {e:?}"),
        }
    }
}
impl std::error::Error for Error {}
impl Error {
    pub(crate) fn memory_admission(
        governor: &sortsize_core::resources::Governor,
        category: sortsize_core::resources::Category,
        minimum: u64,
    ) -> Self {
        let usage = governor.usage();
        let category_limit = usage.limits.categories[category as usize];
        let used: u64 = usage.categories.iter().sum();
        Self::MemoryAdmission {
            minimum,
            available: category_limit
                .saturating_sub(usage.categories[category as usize])
                .min(usage.limits.bytes.saturating_sub(used)),
            category_limit,
        }
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct DiskAdmission {
    pub store: PathBuf,
    pub scratch: PathBuf,
    pub requested: u64,
    pub minimum_limit: u64,
    pub store_free: u64,
    pub scratch_free: u64,
    pub used: u64,
    pub reserved: u64,
    pub limit: u64,
}
impl fmt::Display for DiskAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shared disk quota: pause before write (store {}: {} free bytes, workspace {}: {} free bytes, {} requested, {} used, {} reserved, {} limit, {} minimum working limit)",
            self.store.display(),
            self.store_free,
            self.scratch.display(),
            self.scratch_free,
            self.requested,
            self.used,
            self.reserved,
            self.limit,
            self.minimum_limit,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Unknown = 0,
    File = 1,
    Directory = 2,
    Symlink = 3,
    Special = 4,
}
#[derive(Clone, Debug)]
pub struct Observation {
    pub name: NativeName,
    pub kind: Kind,
    pub logical: Option<u64>,
    pub allocated: Option<u64>,
    pub modified: Option<(i64, u32)>,
    pub metadata_known: bool,
}
pub(crate) struct DirectoryBatch<'a> {
    pub(crate) header: BatchHeader,
    pub(crate) observations: &'a [Observation],
    pub(crate) evidence: &'a [Option<differential::NativeEvidence>],
    pub(crate) properties: &'a [sortsize_core::acquisition::NodeProperties],
    pub(crate) directory: Option<&'a sortsize_core::acquisition::Stamp>,
}

struct PreparedDirectoryBatch {
    next: BatchCursor,
    has_evidence: bool,
    has_properties: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: EntryKey,
    pub parent: Option<EntryId>,
    pub name: NativeName,
    pub kind: Kind,
    pub logical: Option<u64>,
    pub allocated: Option<u64>,
    pub modified: Option<(i64, u32)>,
    pub present: bool,
    pub metadata_known: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    Applied {
        revision: Revision,
        observations: usize,
    },
    Replay,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Totals {
    /// Sum of known regular-file logical sizes, not an assertion of coverage.
    /// Unknowns stay separate; None means unfinished fold or arithmetic overflow.
    pub logical: Option<u64>,
    pub unknown: u64,
    pub overflow: bool,
    pub fold_complete: bool,
}

pub struct Store {
    directory: std::path::PathBuf,
    db: Connection,
    // Declared after db so native handles close before VFS deregistration.
    _canonical: Option<std::sync::Arc<std::sync::Mutex<generation_a::canonical::CanonicalStorage>>>,
    // Close the separate folder connection before releasing its borrowed
    // portion of _memory or its extra folder_memory reservation.
    folder_projection: std::cell::RefCell<Option<folder_totals::Projection>>,
    _memory: sortsize_core::resources::Reservation,
    folder_memory: std::cell::RefCell<Option<sortsize_core::resources::Reservation>>,
    folder_cache_ready: std::cell::Cell<bool>,
    _owner: std::sync::Arc<File>,
    inventory: InventoryId,
    source: SourceId,
    integrity_verified: bool,
    retained_writer: bool,
    fast_writer: bool,
}
fn lock(dir: &Path) -> Result<File> {
    owner::acquire(dir, false, true)
}

/// Historical backup publication may leave a standalone DB without its new
/// owner inode. Only a quiescent, supported SQLite header permits that explicit
/// RW ownership creation. This is a guard, never an integrity certificate.
fn guard_missing_owner_copy(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir.join("owner.lock")) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    use std::io::Read;
    let mut header = [0u8; 100];
    File::open(dir.join("inventory.db"))?.read_exact(&mut header)?;
    let version = u32::from_be_bytes(header[60..64].try_into().unwrap());
    let application = u32::from_be_bytes(header[68..72].try_into().unwrap());
    if &header[..16] != b"SQLite format 3\0"
        || i64::from(application) != APPLICATION_ID
        || !(1..=6).contains(&version)
    {
        return Err(Error::Contract(
            "unrecognized or future unowned copy preserved",
        ));
    }
    for name in [
        "inventory.db-wal",
        "inventory.db-shm",
        "inventory.db-journal",
    ] {
        match std::fs::symlink_metadata(dir.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
            Ok(_) => {
                return Err(Error::Contract(
                    "owner missing: nonquiescent group preserved",
                ));
            }
        }
    }
    Ok(())
}

fn number(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get_ref(index)?;
    let bytes = match value {
        rusqlite::types::ValueRef::Blob(b) => b,
        _ => {
            return Err(rusqlite::Error::InvalidColumnType(
                index,
                index.to_string(),
                value.data_type(),
            ));
        }
    };
    bytes.try_into().map(u64::from_be_bytes).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid u64 codec",
            )),
        )
    })
}
fn opt_number(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    if row.get_ref(index)? == rusqlite::types::ValueRef::Null {
        Ok(None)
    } else {
        number(row, index).map(Some)
    }
}
fn context(c: AcquisitionContext) -> Vec<u8> {
    let mut bytes = vec![1];
    bytes.extend(c.inventory.0.to_be_bytes());
    bytes.extend(c.source.0.to_be_bytes());
    for n in [
        c.generation.0,
        c.scan.0,
        c.policy.0,
        c.mount.0,
        c.run.0,
        c.validation.0,
    ] {
        bytes.extend(unsigned(n));
    }
    bytes
}
fn attempt(a: AttemptKey) -> Vec<u8> {
    let mut b = context(a.task.context);
    b.extend(unsigned(a.task.directory.0));
    b.push(match a.task.kind {
        TaskKind::EnumerateDirect => 0,
        TaskKind::Reconcile => 1,
        TaskKind::Consolidate => 2,
    });
    for n in [a.attempt.0, a.owner.0, a.branch.0] {
        b.extend(unsigned(n));
    }
    b
}
fn generation(db: &Connection, c: AcquisitionContext) -> Result<()> {
    require_generation_state(db, c, false)
}
fn require_generation_state(db: &Connection, c: AcquisitionContext, sealed: bool) -> Result<()> {
    let (stored, actual_sealed): (Vec<u8>, bool) = db.query_row(
        "SELECT context,sealed FROM generations WHERE id=?1",
        [unsigned(c.generation.0)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if stored != context(c) {
        return Err(Error::Contract("wrong acquisition context"));
    }
    if actual_sealed != sealed {
        return Err(Error::Contract(if actual_sealed {
            "sealed generation"
        } else {
            "unsealed generation"
        }));
    }
    Ok(())
}
fn visible(db: &Connection, g: GenerationId, mut id: EntryId) -> Result<bool> {
    loop {
        let row: Option<(Option<u64>, bool, i64)> = db
            .query_row(
                "SELECT parent,present,kind FROM entries WHERE generation=?1 AND id=?2",
                params![unsigned(g.0), unsigned(id.0)],
                |r| Ok((opt_number(r, 0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((parent, present, kind)) = row else {
            return Ok(false);
        };
        if !present {
            return Ok(false);
        }
        match parent {
            None => return Ok(id.0 == 0 && kind == Kind::Directory as i64),
            Some(p) if p < id.0 => {
                // A file replacing a directory invalidates all descendants as well.
                let parent_kind: i64 = db.query_row(
                    "SELECT kind FROM entries WHERE generation=?1 AND id=?2",
                    params![unsigned(g.0), unsigned(p)],
                    |r| r.get(0),
                )?;
                if parent_kind != Kind::Directory as i64 {
                    return Ok(false);
                }
                id = EntryId(p);
            }
            _ => return Err(Error::Contract("cyclic or invalid parent chain")),
        }
    }
}
// A quantum reuses bytecode, never validation results. Context and each
// ancestor/attempt/presence check retain the same SQL and failure semantics.
struct CurrentBranch<'a> {
    generation: GenerationId,
    epoch: Vec<u8>,
    entry: rusqlite::Statement<'a>,
    stamp: rusqlite::Statement<'a>,
    key: rusqlite::Statement<'a>,
    parent_attempt: rusqlite::Statement<'a>,
}
impl<'a> CurrentBranch<'a> {
    fn new(db: &'a Connection, g: GenerationId) -> Result<Self> {
        let epoch = db.query_row(
            "SELECT context FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| r.get(0),
        )?;
        Ok(Self {
            generation: g,
            epoch,
            entry: db.prepare(
                "SELECT parent,present,kind,attempt FROM entries WHERE generation=?1 AND id=?2",
            )?,
            stamp: db.prepare(
                "SELECT parent_attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",
            )?,
            key: db
                .prepare("SELECT key FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0")?,
            parent_attempt: db.prepare(
                "SELECT attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",
            )?,
        })
    }
    fn check(&mut self, mut id: EntryId, require_directory: bool) -> Result<bool> {
        let g = self.generation;
        let mut first = true;
        loop {
            let row: Option<(Option<u64>, bool, i64, u64)> = self
                .entry
                .query_row(params![unsigned(g.0), unsigned(id.0)], |r| {
                    Ok((opt_number(r, 0)?, r.get(1)?, r.get(2)?, number(r, 3)?))
                })
                .optional()?;
            let Some((parent, present, kind, seen)) = row else {
                return Ok(false);
            };
            if !present || (first && require_directory && kind != 2) {
                return Ok(false);
            }
            first = false;
            if kind == 2 {
                let stamp: Option<u64> = self
                    .stamp
                    .query_row(params![unsigned(g.0), unsigned(id.0)], |r| number(r, 0))
                    .optional()?;
                let key: Option<Vec<u8>> = self
                    .key
                    .query_row(params![unsigned(g.0), unsigned(id.0)], |r| r.get(0))
                    .optional()?
                    .flatten();
                if stamp != Some(seen) || !key.is_some_and(|k| k.starts_with(&self.epoch)) {
                    return Ok(false);
                }
            }
            let Some(parent) = parent else {
                return Ok(id.0 == 0 && kind == 2);
            };
            if parent >= id.0 {
                return Err(Error::Contract("invalid parent chain"));
            }
            let parent_attempt: Option<u64> = self
                .parent_attempt
                .query_row(params![unsigned(g.0), unsigned(parent)], |r| {
                    opt_number(r, 0)
                })
                .optional()?
                .flatten();
            if parent_attempt != Some(seen) {
                return Ok(false);
            }
            id = EntryId(parent);
        }
    }
}
fn current_branch(
    db: &Connection,
    g: GenerationId,
    mut id: EntryId,
    require_directory: bool,
) -> Result<bool> {
    let mut first = true;
    let epoch: Vec<u8> = db.query_row(
        "SELECT context FROM generations WHERE id=?1",
        [unsigned(g.0)],
        |r| r.get(0),
    )?;
    loop {
        let row: Option<(Option<u64>, bool, i64, u64)> = db
            .query_row(
                "SELECT parent,present,kind,attempt FROM entries WHERE generation=?1 AND id=?2",
                params![unsigned(g.0), unsigned(id.0)],
                |r| Ok((opt_number(r, 0)?, r.get(1)?, r.get(2)?, number(r, 3)?)),
            )
            .optional()?;
        let Some((parent, present, kind, seen)) = row else {
            return Ok(false);
        };
        if !present || (first && require_directory && kind != 2) {
            return Ok(false);
        }
        first = false;
        if kind == 2 {
            let stamp:Option<u64>=db.query_row("SELECT parent_attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",params![unsigned(g.0),unsigned(id.0)],|r|number(r,0)).optional()?;
            let key: Option<Vec<u8>> = db
                .query_row(
                    "SELECT key FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",
                    params![unsigned(g.0), unsigned(id.0)],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if stamp != Some(seen) || !key.is_some_and(|k| k.starts_with(&epoch)) {
                return Ok(false);
            }
        }
        let Some(parent) = parent else {
            return Ok(id.0 == 0 && kind == 2);
        };
        if parent >= id.0 {
            return Err(Error::Contract("invalid parent chain"));
        }
        let parent_attempt: Option<u64> = db
            .query_row(
                "SELECT attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",
                params![unsigned(g.0), unsigned(parent)],
                |r| opt_number(r, 0),
            )
            .optional()?
            .flatten();
        if parent_attempt != Some(seen) {
            return Ok(false);
        }
        id = EntryId(parent);
    }
}
fn active(db: &Connection, a: AttemptKey) -> Result<BatchCursor> {
    generation(db, a.task.context)?;
    if a.task.kind != TaskKind::EnumerateDirect
        || !current_branch(db, a.task.context.generation, a.task.directory, true)?
    {
        return Err(Error::Batch(BatchRejection::StaleAttempt));
    }
    let (key,next,end):(Option<Vec<u8>>,u64,i64)=db.query_row("SELECT key,next_sequence,listing FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",params![unsigned(a.task.context.generation.0),unsigned(a.task.directory.0)],|r|Ok((r.get(0)?,number(r,1)?,r.get(2)?)))?;
    if key.as_deref() != Some(attempt(a).as_slice()) {
        return Err(Error::Batch(BatchRejection::StaleAttempt));
    }
    let end = match end {
        0 => None,
        1 => Some(ListingEnd::Exhausted),
        2 => Some(ListingEnd::Incomplete),
        _ => return Err(Error::Contract("invalid listing state")),
    };
    BatchCursor::from_committed(a, next, end).ok_or(Error::Contract("invalid committed sequence"))
}
fn dirty(tx: &Transaction<'_>, g: GenerationId) -> Result<Revision> {
    let rev = tx
        .query_row(
            "SELECT revision FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| number(r, 0),
        )?
        .checked_add(1)
        .ok_or(Error::Contract("revision exhausted"))?;
    tx.execute("UPDATE generations SET revision=?2,aggregate_done=0,aggregate_cursor=NULL,aggregate_sum=?3,aggregate_unknown=?3,aggregate_overflow=0 WHERE id=?1",params![unsigned(g.0),unsigned(rev),unsigned(0)])?;
    Ok(Revision(rev))
}
fn enqueue(db: &Connection, g: GenerationId, id: EntryId) -> Result<()> {
    db.execute("INSERT INTO tasks(generation,directory,next_sequence,parent_attempt) SELECT ?1,?2,?3,attempt FROM entries WHERE generation=?1 AND id=?2 ON CONFLICT(generation,directory,kind) DO UPDATE SET key=NULL,listing=0,reconciled=0,sweep_cursor=NULL,next_sequence=excluded.next_sequence,parent_attempt=excluded.parent_attempt WHERE tasks.parent_attempt!=excluded.parent_attempt",params![unsigned(g.0),unsigned(id.0),unsigned(0)])?;
    Ok(())
}

impl Store {
    /// Canonical transactions retain headroom for DB and auxiliary writes.
    pub const WRITE_RESERVE: u64 = 8 * 1024 * 1024;

    fn reserve_memory() -> Result<sortsize_core::resources::Reservation> {
        let governor = sortsize_core::resources::Governor::global();
        governor.refresh_native();
        Self::reserve_memory_in(&governor)
    }
    fn reserve_memory_in(
        governor: &sortsize_core::resources::Governor,
    ) -> Result<sortsize_core::resources::Reservation> {
        use sortsize_core::resources::Category;
        const MINIMUM: u64 = 1024 * 1024;
        governor
            .reserve_up_to(Category::Sqlite, MINIMUM, 8 * 1024 * 1024)
            .ok_or_else(|| {
                let usage = governor.usage();
                let limit = usage.limits.categories[Category::Sqlite as usize];
                Error::MemoryAdmission {
                    minimum: MINIMUM,
                    available: limit.saturating_sub(usage.categories[Category::Sqlite as usize]),
                    category_limit: limit,
                }
            })
    }
    fn configure_cache(
        db: &Connection,
        memory: &sortsize_core::resources::Reservation,
    ) -> Result<()> {
        // Keep 512 KiB within the reservation for connection/statement overhead.
        // This is accounted capacity, not a promise about total SQLite/process RSS.
        let kib = (memory.bytes() - 512 * 1024) / 1024;
        db.pragma_update(None, "cache_size", -(kib as i64))?;
        Ok(())
    }
    pub fn sqlite_cache_bytes(&self) -> Result<u64> {
        let kib: i64 = self.db.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
        Ok(kib.unsigned_abs() * 1024)
    }
    pub(crate) fn disk_admission(
        &self,
        bytes: u64,
    ) -> Result<sortsize_core::resources::DiskReservation> {
        self.disk_admission_at(bytes, &self.directory)
    }
    pub(crate) fn disk_admission_at(
        &self,
        bytes: u64,
        scratch: &Path,
    ) -> Result<sortsize_core::resources::DiskReservation> {
        use sortsize_core::resources::{Governor, native_free_disk};
        let _timer = Phase::DiskAdmission.start();
        let owned = if let Some(canonical) = &self._canonical {
            canonical.lock().unwrap().usage().charged
        } else {
            lifecycle::artifact_bytes(&self.directory)?
        };
        let store_free = native_free_disk(&self.directory)
            .ok_or(Error::Contract("store free space unavailable"))?;
        let scratch_free = if scratch == self.directory {
            store_free
        } else {
            native_free_disk(scratch).ok_or(Error::Contract("scratch free space unavailable"))?
        };
        let governor = Governor::global();
        if !governor.observe_artifact_group(&self.directory, store_free.min(scratch_free), owned) {
            return Err(Error::Contract("artifact registry budget"));
        }
        if scratch == self.directory
            && self
                ._canonical
                .as_ref()
                .is_some_and(|canonical| canonical.lock().unwrap().prepaid_bytes() >= bytes)
        {
            return governor.reserve_disk(0).ok_or(Error::Contract(
                "canonical prepaid operation metadata admission",
            ));
        }
        let reservation = if scratch == self.directory && self._canonical.is_some() {
            governor.reserve_disk_for_group(&self.directory, bytes)
        } else {
            governor.reserve_disk_at(scratch, bytes)
        };
        reservation.ok_or_else(|| {
            let usage = governor.usage();
            Error::DiskAdmission(Box::new(DiskAdmission {
                store: self.directory.clone(),
                scratch: scratch.into(),
                requested: bytes,
                minimum_limit: 0,
                store_free,
                scratch_free,
                used: usage.disk_used,
                reserved: usage.disk_reserved,
                limit: usage.disk_limit,
            }))
        })
    }
    pub(super) fn require_integrity(&self) -> Result<()> {
        if !self.integrity_verified {
            return Err(Error::Contract(
                "catalogue inspection is not an integrity certificate",
            ));
        }
        Ok(())
    }
    pub fn canonical_usage(&self) -> Option<queries::TemporaryUsage> {
        self._canonical.as_ref().map(|c| c.lock().unwrap().usage())
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn create(
        dir: &Path,
        inventory: InventoryId,
        source: SourceId,
        durability: Durability,
    ) -> Result<Self> {
        Self::create_admitted(dir, inventory, source, durability, Self::reserve_memory()?)
    }
    fn create_admitted(
        dir: &Path,
        inventory: InventoryId,
        source: SourceId,
        durability: Durability,
        memory: sortsize_core::resources::Reservation,
    ) -> Result<Self> {
        Self::create_configured(dir, inventory, source, durability, memory, None)
    }
    pub(crate) fn create_canonical_admitted(
        dir: &Path,
        inventory: InventoryId,
        source: SourceId,
        durability: Durability,
        memory: sortsize_core::resources::Reservation,
        limit: u64,
        prospective: Option<sortsize_core::resources::DiskReservation>,
    ) -> Result<Self> {
        Self::create_configured(
            dir,
            inventory,
            source,
            durability,
            memory,
            Some((limit, prospective)),
        )
    }
    fn create_configured(
        dir: &Path,
        inventory: InventoryId,
        source: SourceId,
        durability: Durability,
        memory: sortsize_core::resources::Reservation,
        canonical: Option<(u64, Option<sortsize_core::resources::DiskReservation>)>,
    ) -> Result<Self> {
        lifecycle::private_directory(dir)?;
        let owner = lock(dir)?;
        let directory = std::fs::canonicalize(dir)?;
        let canonical = canonical
            .map(|(limit, disk)| {
                generation_a::canonical::CanonicalStorage::new(&directory, limit, disk)
            })
            .transpose()?;
        let mut db = crate::open_with_vfs(
            &directory.join("inventory.db"),
            durability,
            canonical.as_ref().map(|c| c.name()),
        )?;
        Self::configure_cache(&db, &memory)?;
        let tx = db.transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.execute(
            "INSERT INTO identity VALUES(?1,?2,NULL,NULL)",
            params![inventory.0.to_be_bytes(), source.0.to_be_bytes()],
        )?;
        tx.commit()?;
        Ok(Self {
            directory,
            _memory: memory,
            folder_projection: Default::default(),
            folder_memory: Default::default(),
            folder_cache_ready: Default::default(),
            db,
            _canonical: canonical.map(|c| std::sync::Arc::new(std::sync::Mutex::new(c))),
            _owner: std::sync::Arc::new(owner),
            inventory,
            source,
            integrity_verified: true,
            retained_writer: matches!(durability, Durability::Retained),
            fast_writer: false,
        })
    }
    pub fn reopen(dir: &Path, durability: Durability) -> Result<Self> {
        Self::reopen_admitted(dir, durability, Self::reserve_memory()?)
    }
    pub(crate) fn reopen_admitted(
        dir: &Path,
        durability: Durability,
        memory: sortsize_core::resources::Reservation,
    ) -> Result<Self> {
        Self::reopen_configured(dir, durability, memory, None, &Default::default())
    }
    /// Inspect the complete pinned inventory with cancellation before opening RW.
    pub fn reopen_cancellable(
        dir: &Path,
        durability: Durability,
        cancel: &sortsize_core::CancellationToken,
    ) -> Result<Self> {
        Self::reopen_configured(dir, durability, Self::reserve_memory()?, None, cancel)
    }
    pub(crate) fn reopen_canonical_cancellable_admitted(
        dir: &Path,
        durability: Durability,
        memory: sortsize_core::resources::Reservation,
        limit: u64,
        cancel: &sortsize_core::CancellationToken,
    ) -> Result<Self> {
        Self::reopen_configured(dir, durability, memory, Some(limit), cancel)
    }
    fn reopen_configured(
        dir: &Path,
        durability: Durability,
        memory: sortsize_core::resources::Reservation,
        canonical_limit: Option<u64>,
        cancel: &sortsize_core::CancellationToken,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Contract("inventory reopen cancelled"));
        }
        sortsize_core::acquisition::local_state_location(dir)?;
        // Reject links/foreign artifacts before a SQLite connection or lock open.
        if std::fs::symlink_metadata(dir)?.is_symlink() {
            return Err(Error::Contract("symlink store directory"));
        }
        lifecycle::artifact_bytes(dir)?;
        // Inspect read-only before any WAL/configuration write; future files stay intact.
        // A completed standalone legacy copy may need its new owner inode;
        // future/corrupt headers and unowned recovery artifacts stay untouched.
        guard_missing_owner_copy(dir)?;
        if cancel.is_cancelled() {
            return Err(Error::Contract(
                "inventory reopen cancelled before ownership",
            ));
        }
        let owner = lock(dir)?;
        let directory = std::fs::canonicalize(dir)?;
        let canonical = canonical_limit
            .map(|limit| generation_a::canonical::CanonicalStorage::new(&directory, limit, None))
            .transpose()?;
        let path = directory.join("inventory.db");
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
        let mut inspection = if let Some(canonical) = &canonical {
            Connection::open_with_flags_and_vfs(&path, flags, canonical.name())?
        } else {
            Connection::open_with_flags(&path, flags)?
        };
        let stop = cancel.clone();
        inspection.progress_handler(1000, Some(move || stop.is_cancelled()))?;
        inspection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 512 * 1024)?;
        inspection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 256 * 1024)?;
        // The inspection owns the same reservation as the later RW connection.
        // Configure it before the full check; drop it before reusing that budget.
        inspection.execute_batch(
            "PRAGMA trusted_schema=OFF;PRAGMA mmap_size=0;PRAGMA temp_store=FILE;",
        )?;
        Self::configure_cache(&inspection, &memory)?;
        inspection.execute_batch("BEGIN DEFERRED;")?;
        let app: i64 = inspection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = inspection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if app != APPLICATION_ID || !matches!(version, 1..=6) {
            return Err(Error::Contract("unrecognized or future inventory format"));
        }
        preview::require_tables(&inspection, version)?;
        if version >= 3 {
            let semantic: i64 = inspection.query_row(
                "SELECT semantic FROM retention WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            if semantic != 1 {
                return Err(Error::Contract("future retention semantics preserved"));
            }
        }
        // Full O(N) check is deliberate and runs on the service thread.
        // No RW connection/checkpoint is admitted on a corrupt cache.
        let check: String = inspection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(Error::Contract("corrupt inventory group preserved"));
        }
        drop(inspection);
        if cancel.is_cancelled() {
            return Err(Error::Contract(
                "inventory reopen cancelled before write admission",
            ));
        }
        let db = crate::open_with_vfs(
            &directory.join("inventory.db"),
            durability,
            canonical.as_ref().map(|c| c.name()),
        )?;
        Self::configure_cache(&db, &memory)?;
        let (i, s): (Vec<u8>, Vec<u8>) = db.query_row(
            "SELECT substr(inventory,1,17),substr(source,1,17) FROM identity",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let inventory = InventoryId(u128::from_be_bytes(
            i.try_into()
                .map_err(|_| Error::Contract("invalid inventory ID"))?,
        ));
        let source = SourceId(u128::from_be_bytes(
            s.try_into()
                .map_err(|_| Error::Contract("invalid source ID"))?,
        ));
        Ok(Self {
            directory,
            _memory: memory,
            folder_projection: Default::default(),
            folder_memory: Default::default(),
            folder_cache_ready: Default::default(),
            db,
            _canonical: canonical.map(|c| std::sync::Arc::new(std::sync::Mutex::new(c))),
            _owner: std::sync::Arc::new(owner),
            inventory,
            source,
            integrity_verified: true,
            retained_writer: matches!(durability, Durability::Retained),
            fast_writer: false,
        })
    }
    /// Full certified legacy snapshot without opening an RW connection. SQLite
    /// may need existing WAL/SHM coordination files for a legacy WAL database;
    /// this method never migrates, checkpoints or changes retention metadata.
    pub fn certify_readonly(dir: &Path, cancel: &sortsize_core::CancellationToken) -> Result<Self> {
        let mut store = Self::inspect_admitted_shared(dir, cancel, Self::reserve_memory()?)?;
        let version: i64 = store
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        for expected in SCHEMA
            .split(';')
            .map(str::trim)
            .filter(|sql| sql.starts_with("CREATE TABLE "))
        {
            let name = expected
                .strip_prefix("CREATE TABLE ")
                .unwrap()
                .split('(')
                .next()
                .unwrap()
                .trim();
            if name == "recovery_guard" && version < 2 {
                continue;
            }
            let actual: String = store.db.query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )?;
            if actual != expected {
                return Err(Error::Contract(
                    "unknown legacy canonical table definition preserved",
                ));
            }
        }
        let check: String = store
            .db
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" || cancel.is_cancelled() {
            return Err(Error::Contract(
                "legacy readonly certification failed or cancelled",
            ));
        }
        store.db.progress_handler(0, None::<fn() -> bool>)?;
        store.integrity_verified = true;
        Ok(store)
    }
    /// Metadata catalogue inspection only. Never opens the main database for
    /// writing or checkpoints a preserved corrupt group. This is not a full
    /// integrity certificate; opening a retained cache performs that check.
    pub fn inspect(dir: &Path) -> Result<Self> {
        Self::inspect_cancellable(dir, &Default::default())
    }
    pub(super) fn inspect_cancellable(
        dir: &Path,
        cancel: &sortsize_core::CancellationToken,
    ) -> Result<Self> {
        Self::inspect_admitted(dir, cancel, Self::reserve_memory()?)
    }
    pub(crate) fn inspect_admitted(
        dir: &Path,
        cancel: &sortsize_core::CancellationToken,
        memory: sortsize_core::resources::Reservation,
    ) -> Result<Self> {
        Self::inspect_owned(dir, cancel, memory, false)
    }
    pub(crate) fn inspect_admitted_shared(
        dir: &Path,
        cancel: &sortsize_core::CancellationToken,
        memory: sortsize_core::resources::Reservation,
    ) -> Result<Self> {
        Self::inspect_owned(dir, cancel, memory, true)
    }
    fn inspect_owned(
        dir: &Path,
        cancel: &sortsize_core::CancellationToken,
        memory: sortsize_core::resources::Reservation,
        shared: bool,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Contract("inventory verification cancelled"));
        }
        sortsize_core::acquisition::local_state_location(dir)?;
        if std::fs::symlink_metadata(dir)?.is_symlink() {
            return Err(Error::Contract("symlink store directory"));
        }
        lifecycle::artifact_bytes(dir)?;
        let owner = owner::acquire(dir, shared, false)?;
        let mut db = Connection::open_with_flags(
            dir.join("inventory.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Unverified input must not allocate an arbitrary corrupt BLOB/schema.
        // These connection-local bounds cover the existing bounded native rows.
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 512 * 1024)?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 256 * 1024)?;
        let stop = cancel.clone();
        db.progress_handler(1000, Some(move || stop.is_cancelled()))?;
        db.busy_timeout(std::time::Duration::from_millis(100))?;
        db.execute_batch("PRAGMA trusted_schema=OFF;PRAGMA mmap_size=0;PRAGMA temp_store=FILE;PRAGMA synchronous=FULL;")?;
        Self::configure_cache(&db, &memory)?;
        // Pin before schema guards AND metadata. A concurrent external writer
        // must not replace a guarded table with a view between those reads.
        db.execute_batch("BEGIN DEFERRED;")?;
        let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if app != APPLICATION_ID || !(1..=6).contains(&version) {
            return Err(Error::Contract("unrecognized or future inventory format"));
        }
        preview::require_tables(&db, version)?;
        if version >= 3
            && db.query_row::<i64, _, _>(
                "SELECT semantic FROM retention WHERE singleton=1",
                [],
                |r| r.get(0),
            )? != 1
        {
            return Err(Error::Contract("future retention semantics preserved"));
        }
        let (i, s): (Vec<u8>, Vec<u8>) = db.query_row(
            "SELECT substr(inventory,1,17),substr(source,1,17) FROM identity",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let inventory = InventoryId(u128::from_be_bytes(
            i.try_into()
                .map_err(|_| Error::Contract("invalid inventory ID"))?,
        ));
        let source = SourceId(u128::from_be_bytes(
            s.try_into()
                .map_err(|_| Error::Contract("invalid source ID"))?,
        ));
        Ok(Self {
            directory: std::fs::canonicalize(dir)?,
            db,
            _canonical: None,
            _memory: memory,
            folder_projection: Default::default(),
            folder_memory: Default::default(),
            folder_cache_ready: Default::default(),
            _owner: std::sync::Arc::new(owner),
            inventory,
            source,
            integrity_verified: false,
            retained_writer: false,
            fast_writer: false,
        })
    }
    pub fn create_generation(&mut self, c: AcquisitionContext) -> Result<EntryKey> {
        self.create_generation_guarded(c, None)
    }
    pub(crate) fn create_generation_guarded(
        &mut self,
        c: AcquisitionContext,
        proof: Option<&[u8]>,
    ) -> Result<EntryKey> {
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        if c.inventory != self.inventory || c.source != self.source {
            return Err(Error::Contract("wrong inventory/source"));
        }
        let tx = self.db.transaction()?;
        let (count, candidates): (i64, i64) = tx.query_row(
            "SELECT count(*),coalesce(sum(sealed=0),0) FROM generations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if count >= 3 || candidates != 0 {
            return Err(Error::Contract(
                "generation admission requires retirement or sealing",
            ));
        }
        let g = unsigned(c.generation.0);
        tx.execute("INSERT INTO generations(id,context,revision,next_entry,aggregate_sum,aggregate_unknown) VALUES(?1,?2,?3,?4,?3,?3)",params![g,context(c),unsigned(0),unsigned(1)])?;
        tx.execute("INSERT INTO entries(generation,id,parent,name,kind,attempt,metadata_known) VALUES(?1,?2,NULL,?3,2,?2,1)",params![g,unsigned(0),NativeName::Opaque(vec![]).encode()])?;
        enqueue(&tx, c.generation, EntryId(0))?;
        if let Some(proof) = proof {
            tx.execute(
                "INSERT INTO recovery_guard VALUES(?1,?2)",
                params![g, proof],
            )?;
        }
        tx.commit()?;
        Ok(EntryKey {
            inventory: self.inventory,
            generation: c.generation,
            entry: EntryId(0),
        })
    }
    pub fn begin_attempt(&mut self, a: AttemptKey) -> Result<()> {
        let _timer = Phase::Attempt.start();
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let seen = Self::validate_attempt(&self.db, a)?;
        let write = Phase::AttemptWrite.start();
        let begin = Phase::AttemptBegin.start();
        let tx = self.db.transaction()?;
        drop(begin);
        let changed = Self::apply_attempt(&tx, a, seen)?;
        drop(write);
        if changed {
            metrics::commit(tx, Phase::AttemptCommit)?;
        }
        Ok(())
    }
    pub(crate) fn supports_grouped_attempts(&self) -> bool {
        self.retained_writer || self.fast_writer
    }
    pub(crate) fn is_fast_candidate(&self) -> bool {
        self.fast_writer
    }
    /// Open up to eight directory attempts atomically. Each retains the same
    /// fences and revision update as the single-attempt path. The canonical
    /// VFS still charges actual growth under the usual write reserve.
    pub(crate) fn begin_attempt_group(&mut self, attempts: &[AttemptKey]) -> Result<()> {
        let _timer = Phase::AttemptGroup.start();
        if !self.supports_grouped_attempts() {
            return Err(Error::Contract(
                "attempt group requires an admitted candidate writer",
            ));
        }
        if !(2..=8).contains(&attempts.len()) {
            return Err(Error::Contract(
                "attempt group requires two to eight directories",
            ));
        }
        if attempts.iter().enumerate().any(|(index, attempt)| {
            attempt.task.context != attempts[0].task.context
                || attempts[..index]
                    .iter()
                    .any(|earlier| earlier.task.directory == attempt.task.directory)
        }) {
            return Err(Error::Contract(
                "invalid attempt group context or directories",
            ));
        }
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let begin = Phase::AttemptBegin.start();
        let tx = self.db.transaction()?;
        drop(begin);
        // Valid ancestors have smaller IDs. Apply them first so a later member
        // cannot invalidate a branch that the same group already accepted.
        let mut ordered = attempts.to_vec();
        ordered.sort_by_key(|a| a.task.directory);
        let mut changed = false;
        for a in ordered {
            let seen = Self::validate_attempt(&tx, a)?;
            changed |= Self::apply_attempt(&tx, a, seen)?;
        }
        if changed {
            metrics::commit(tx, Phase::AttemptCommit)?;
        }
        Ok(())
    }
    fn validate_attempt(db: &Connection, a: AttemptKey) -> Result<u64> {
        let _timer = Phase::AttemptValidate.start();
        generation(db, a.task.context)?;
        let (parent, seen, kind): (Option<u64>, u64, i64) = db.query_row(
            "SELECT parent,attempt,kind FROM entries WHERE generation=?1 AND id=?2",
            params![
                unsigned(a.task.context.generation.0),
                unsigned(a.task.directory.0)
            ],
            |r| Ok((opt_number(r, 0)?, number(r, 1)?, r.get(2)?)),
        )?;
        if a.task.kind != TaskKind::EnumerateDirect
            || kind != 2
            || !visible(db, a.task.context.generation, a.task.directory)?
        {
            return Err(Error::Contract("invalid directory task"));
        }
        if let Some(parent) = parent {
            if !current_branch(db, a.task.context.generation, EntryId(parent), true)? {
                return Err(Error::Batch(BatchRejection::StaleAttempt));
            }
            let parent_attempt = db.query_row(
                "SELECT attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",
                params![unsigned(a.task.context.generation.0), unsigned(parent)],
                |r| opt_number(r, 0),
            )?;
            if parent_attempt != Some(seen) {
                return Err(Error::Batch(BatchRejection::StaleAttempt));
            }
        }
        Ok(seen)
    }
    fn apply_attempt(tx: &Transaction<'_>, a: AttemptKey, seen: u64) -> Result<bool> {
        let g = unsigned(a.task.context.generation.0);
        let d = unsigned(a.task.directory.0);
        let task = Phase::AttemptTaskLookup.start();
        let (key,old,owner,branch):(Option<Vec<u8>>,Option<u64>,Option<u64>,Option<u64>)=tx.query_row("SELECT key,attempt,owner,branch FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",params![g,d],|r|Ok((r.get(0)?,opt_number(r,1)?,opt_number(r,2)?,opt_number(r,3)?)))?;
        drop(task);
        if key.as_deref() == Some(attempt(a).as_slice()) {
            let _replay = Phase::AttemptReplayCheck.start();
            if current_branch(tx, a.task.context.generation, a.task.directory, true)? {
                return Ok(false);
            }
            return Err(Error::Batch(BatchRejection::StaleAttempt));
        }
        if old.is_some_and(|x| a.attempt.0 <= x)
            || owner.is_some_and(|x| a.owner.0 < x)
            || branch.is_some_and(|x| a.branch.0 < x)
        {
            return Err(Error::Batch(BatchRejection::StaleAttempt));
        }
        let _mutate = Phase::AttemptMutate.start();
        tx.execute("UPDATE tasks SET key=?3,attempt=?4,owner=?5,branch=?6,next_sequence=?7,parent_attempt=?8,listing=0,sweep_cursor=NULL,reconciled=0 WHERE generation=?1 AND directory=?2 AND kind=0",params![g,d,attempt(a),unsigned(a.attempt.0),unsigned(a.owner.0),unsigned(a.branch.0),unsigned(0),unsigned(seen)])?;
        dirty(tx, a.task.context.generation)?;
        Ok(true)
    }
    pub fn ingest(&mut self, header: BatchHeader, observations: &[Observation]) -> Result<Ack> {
        self.ingest_evidenced(header, observations, &[])
    }
    pub fn ingest_evidenced(
        &mut self,
        header: BatchHeader,
        observations: &[Observation],
        evidence: &[Option<differential::NativeEvidence>],
    ) -> Result<Ack> {
        self.ingest_properties(header, observations, evidence, &[])
    }
    pub fn ingest_properties(
        &mut self,
        header: BatchHeader,
        observations: &[Observation],
        evidence: &[Option<differential::NativeEvidence>],
        properties: &[sortsize_core::acquisition::NodeProperties],
    ) -> Result<Ack> {
        self.ingest_directory_properties(header, observations, evidence, properties, None)
    }
    /// The scheduler supplies a stable end-of-listing observation, including
    /// the synthetic root. Commit it atomically with EOF and its observations;
    /// a replay or failed transaction must never fabricate fresh coverage.
    pub(crate) fn ingest_directory_properties(
        &mut self,
        header: BatchHeader,
        observations: &[Observation],
        evidence: &[Option<differential::NativeEvidence>],
        properties: &[sortsize_core::acquisition::NodeProperties],
        directory: Option<&sortsize_core::acquisition::Stamp>,
    ) -> Result<Ack> {
        let _timer = Phase::Ingest.start();
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let batch = DirectoryBatch {
            header,
            observations,
            evidence,
            properties,
            directory,
        };
        let Some(prepared) = Self::prepare_directory_batch(&self.db, &batch)? else {
            return Ok(Ack::Replay);
        };
        let tx = self.db.transaction()?;
        let ack = Self::apply_directory_batch(&tx, &batch, prepared)?;
        metrics::commit(tx, Phase::IngestCommit)?;
        Ok(ack)
    }
    /// Commit up to eight already admitted scheduler quanta as one bounded unit.
    /// Each retains its original cursor and resource permits at the caller.
    /// Scheduler payloads are capped at 64 KiB each. The usual write reserve
    /// applies to the group, and the canonical VFS still charges actual growth.
    /// Dropping the transaction on any error rolls back every mutation.
    pub(crate) fn ingest_directory_group(&mut self, batches: &[DirectoryBatch<'_>]) -> Result<()> {
        let _timer = Phase::Ingest.start();
        if !(2..=8).contains(&batches.len()) {
            return Err(Error::Contract(
                "ingestion group requires two to eight quanta",
            ));
        }
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let tx = self.db.transaction()?;
        for batch in batches {
            if let Some(prepared) = Self::prepare_directory_batch(&tx, batch)? {
                Self::apply_directory_batch(&tx, batch, prepared)?;
            }
        }
        metrics::commit(tx, Phase::IngestCommit)?;
        Ok(())
    }
    fn prepare_directory_batch(
        db: &Connection,
        batch: &DirectoryBatch<'_>,
    ) -> Result<Option<PreparedDirectoryBatch>> {
        // Fence before replay, and replay before looking at untrusted duplicate payload.
        let cursor = active(db, batch.header.attempt)?;
        let next = match cursor.prepare(batch.header).map_err(Error::Batch)? {
            BatchDecision::Replay => return Ok(None),
            BatchDecision::Apply { next } => next,
        };
        let version = db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?;
        let has_evidence = version >= 4;
        let has_properties = version >= 6;
        Self::validate_directory_batch(batch, has_evidence, has_properties)?;
        Ok(Some(PreparedDirectoryBatch {
            next,
            has_evidence,
            has_properties,
        }))
    }
    pub(crate) fn validate_directory_batch(
        batch: &DirectoryBatch<'_>,
        has_evidence: bool,
        has_properties: bool,
    ) -> Result<()> {
        let DirectoryBatch {
            header,
            observations,
            evidence,
            properties,
            directory,
        } = *batch;
        validate_batch(observations)?;
        if !evidence.is_empty() && evidence.len() != observations.len() {
            return Err(Error::Contract("evidence batch length"));
        }
        if !evidence.is_empty() && !has_evidence {
            return Err(Error::Contract("evidence schema not enabled"));
        }
        if !properties.is_empty() && (properties.len() != observations.len() || !has_properties) {
            return Err(Error::Contract("properties batch/schema"));
        }
        if !properties.is_empty() {
            let bytes = observations
                .iter()
                .try_fold(0usize, |n, o| {
                    n.checked_add(o.name.encode().len())
                        .and_then(|n| n.checked_add(384))
                })
                .ok_or(Error::Contract("properties batch size overflow"))?;
            if bytes > MAX_BYTES {
                return Err(Error::Contract("properties batch byte limit"));
            }
            for (o, p) in observations.iter().zip(properties) {
                let known = match p.allocated {
                    sortsize_core::MetadataValue::Known { value, .. } => Some(value),
                    _ => None,
                };
                if !o.metadata_known
                    && (matches!(p.allocated, sortsize_core::MetadataValue::Known { .. })
                        || matches!(p.accessed, sortsize_core::MetadataValue::Known { .. })
                        || matches!(p.owner.id, sortsize_core::MetadataValue::Known { .. }))
                {
                    return Err(Error::Contract("properties contradict failed observation"));
                }
                if known != o.allocated {
                    return Err(Error::Contract("inconsistent allocated property"));
                }
            }
        }
        if directory.is_some_and(|s| !s.directory)
            || (directory.is_some() && (header.end != Some(ListingEnd::Exhausted) || !has_evidence))
        {
            return Err(Error::Contract(
                "directory freshness requires stable EOF and evidence schema",
            ));
        }
        Ok(())
    }
    fn apply_directory_batch(
        tx: &Transaction<'_>,
        batch: &DirectoryBatch<'_>,
        prepared: PreparedDirectoryBatch,
    ) -> Result<Ack> {
        let _timer = Phase::IngestWrite.start();
        let DirectoryBatch {
            header,
            observations,
            evidence,
            properties,
            directory,
        } = *batch;
        let PreparedDirectoryBatch {
            next,
            has_evidence,
            has_properties,
        } = prepared;
        let a = header.attempt;
        let generation_id = a.task.context.generation;
        let g = unsigned(generation_id.0);
        let parent = unsigned(a.task.directory.0);
        let mut next_id =
            tx.query_row("SELECT next_entry FROM generations WHERE id=?1", [g], |r| {
                number(r, 0)
            })?;
        {
            let mut lookup = tx.prepare(
                "SELECT id,kind FROM entries WHERE generation=?1 AND parent=?2 AND name=?3",
            )?;
            let mut insert=tx.prepare("INSERT INTO entries(generation,id,parent,name,kind,logical,allocated,modified_s,modified_ns,attempt,present,metadata_known) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,?11) ON CONFLICT(generation,id) DO UPDATE SET kind=excluded.kind,logical=excluded.logical,allocated=excluded.allocated,modified_s=excluded.modified_s,modified_ns=excluded.modified_ns,attempt=excluded.attempt,present=1,metadata_known=excluded.metadata_known")?;
            let mut contrib=tx.prepare("INSERT INTO contributions VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(generation,id) DO UPDATE SET attempt=excluded.attempt,logical=excluded.logical,metadata_known=excluded.metadata_known,kind=excluded.kind")?;
            // Prepare once per bounded transaction, never once per entry.
            // Older schemas must not prepare statements for absent tables.
            let mut proof_insert = has_evidence.then(|| tx.prepare("INSERT INTO entry_evidence VALUES(?1,?2,?3) ON CONFLICT(generation,id) DO UPDATE SET proof=excluded.proof")).transpose()?;
            let mut property_insert = has_properties
                .then(|| tx.prepare(properties::INSERT))
                .transpose()?;
            for (index, o) in observations.iter().enumerate() {
                let name = o.name.encode();
                let prior: Option<(u64, i64)> = lookup
                    .query_row(params![g, parent, name], |r| Ok((number(r, 0)?, r.get(1)?)))
                    .optional()?;
                let id = if let Some((id, _)) = prior {
                    id
                } else {
                    let id = next_id;
                    next_id = next_id
                        .checked_add(1)
                        .ok_or(Error::Contract("entry IDs exhausted"))?;
                    id
                };
                let kind = if o.kind == Kind::Unknown {
                    prior.map_or(0, |x| x.1)
                } else {
                    o.kind as i64
                };
                let logical = if o.metadata_known {
                    o.logical.map(unsigned)
                } else {
                    None
                };
                let allocated = if o.metadata_known {
                    o.allocated.map(unsigned)
                } else {
                    None
                };
                let modified = if o.metadata_known { o.modified } else { None };
                insert.execute(params![
                    g,
                    unsigned(id),
                    parent,
                    name,
                    kind,
                    logical,
                    allocated,
                    modified.map(|x| x.0),
                    modified.map(|x| i64::from(x.1)),
                    unsigned(a.attempt.0),
                    o.metadata_known
                ])?;
                contrib.execute(params![
                    g,
                    unsigned(id),
                    parent,
                    unsigned(a.attempt.0),
                    logical,
                    o.metadata_known,
                    kind
                ])?;
                if let Some(statement) = &mut proof_insert {
                    let proof = evidence
                        .get(index)
                        .and_then(|p| p.as_ref())
                        .filter(|_| o.metadata_known)
                        .map(|p| p.encode());
                    statement.execute(params![g, unsigned(id), proof])?;
                }
                if let Some(statement) = &mut property_insert {
                    properties::write(
                        statement,
                        generation_id,
                        EntryId(id),
                        properties.get(index),
                        o.allocated.filter(|_| o.metadata_known),
                    )?;
                }
                if kind == Kind::Directory as i64 {
                    enqueue(tx, generation_id, EntryId(id))?;
                }
            }
        }
        tx.execute(
            "UPDATE generations SET next_entry=?2 WHERE id=?1",
            params![g, unsigned(next_id)],
        )?;
        let end = match next.listing_end() {
            None => 0,
            Some(ListingEnd::Exhausted) => 1,
            Some(ListingEnd::Incomplete) => 2,
        };
        // A stable EOF in the first batch can complete reconciliation in the
        // same transaction when every direct child belongs to this attempt.
        // Inspect at most one more row than the batch contains. Older or
        // larger listings still need the deferred, resumable sweep.
        let reconciled = directory.is_some() && header.sequence == 0 && {
            let _timer = Phase::ReconcileInline.start();
            tx.query_row(
                "SELECT count(*) <= ?4 AND count(*) = coalesce(sum(attempt = ?3), 0)
                 FROM (SELECT attempt FROM entries INDEXED BY entries_parent_id
                       WHERE generation=?1 AND parent=?2 LIMIT ?4 + 1)",
                params![g, parent, unsigned(a.attempt.0), observations.len() as i64],
                |row| row.get::<_, bool>(0),
            )?
        };
        tx.execute("UPDATE tasks SET next_sequence=?3,listing=?4,reconciled=?5 WHERE generation=?1 AND directory=?2 AND kind=0",params![g,parent,unsigned(next.next_sequence()),end,reconciled])?;
        if let Some(stamp) = directory {
            let modified = stamp.modified.and_then(properties::instant);
            tx.execute(
                "UPDATE entries SET modified_s=?3,modified_ns=?4 WHERE generation=?1 AND id=?2",
                params![g, parent, modified.map(|t| t.0), modified.map(|t| t.1)],
            )?;
            tx.execute(
                "INSERT INTO entry_evidence VALUES(?1,?2,?3) ON CONFLICT(generation,id) DO UPDATE SET proof=excluded.proof",
                params![g, parent, differential::NativeEvidence::from_stamp(stamp).encode()],
            )?;
        }
        let revision = dirty(tx, generation_id)?;
        Ok(Ack::Applied {
            revision,
            observations: observations.len(),
        })
    }
    /// Bounded key sweep. EOF alone never infers absence; comparable scope required.
    pub fn reconcile_step(
        &mut self,
        a: AttemptKey,
        scope: ComparisonScope,
        limit: usize,
    ) -> Result<bool> {
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        if !(1..=MAX_BATCH).contains(&limit) {
            return Err(Error::Contract("invalid sweep limit"));
        }
        let cursor = active(&self.db, a)?;
        if cursor.listing_end() != Some(ListingEnd::Exhausted)
            || scope != ComparisonScope::Comparable
        {
            return Err(Error::Contract("absence not proved"));
        }
        let generation_id = a.task.context.generation;
        let g = unsigned(generation_id.0);
        let d = unsigned(a.task.directory.0);
        let tx = self.db.transaction()?;
        let (after,done):(Option<Vec<u8>>,bool)=tx.query_row("SELECT sweep_cursor,reconciled FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",params![g,d],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if done {
            return Ok(true);
        }
        let rows: Vec<(u64, u64)> = tx
            .prepare(RECONCILE_CHILDREN)?
            .query_map(
                params![
                    g,
                    d,
                    after.unwrap_or_else(|| unsigned(0).to_vec()),
                    limit as i64
                ],
                |r| Ok((number(r, 0)?, number(r, 1)?)),
            )?
            .collect::<std::result::Result<_, _>>()?;
        for (id, seen) in &rows {
            if *seen != a.attempt.0 {
                tx.execute(
                    "UPDATE entries SET present=0 WHERE generation=?1 AND id=?2",
                    params![g, unsigned(*id)],
                )?;
            }
        }
        let done = rows.len() < limit;
        tx.execute("UPDATE tasks SET sweep_cursor=coalesce(?3,sweep_cursor),reconciled=?4 WHERE generation=?1 AND directory=?2 AND kind=0",params![g,d,rows.last().map(|x|unsigned(x.0)),done])?;
        dirty(&tx, generation_id)?;
        metrics::commit(tx, Phase::ReconcileCommit)?;
        Ok(done)
    }
    /// Deferred bounded fold over current observations. Checked sums never wrap.
    /// Ancestor validation is O(depth); no global ancestor/entry map in RAM.
    pub fn consolidate_step(&mut self, c: AcquisitionContext, limit: usize) -> Result<bool> {
        self.consolidate_step_inner(c, limit, true)
    }
    fn consolidate_step_inner(
        &mut self,
        c: AcquisitionContext,
        limit: usize,
        verify_branches: bool,
    ) -> Result<bool> {
        let _timer = Phase::Consolidate.start();
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        if !(1..=MAX_BATCH).contains(&limit) {
            return Err(Error::Contract("invalid fold limit"));
        }
        generation(&self.db, c)?;
        let tx = self.db.transaction()?;
        let g = unsigned(c.generation.0);
        let (after,mut sum,mut unknown,mut overflow,done):(Option<Vec<u8>>,u64,u64,bool,bool)=tx.query_row("SELECT aggregate_cursor,aggregate_sum,aggregate_unknown,aggregate_overflow,aggregate_done FROM generations WHERE id=?1",[g],|r|Ok((r.get(0)?,number(r,1)?,number(r,2)?,r.get(3)?,r.get(4)?)))?;
        if done {
            return Ok(true);
        }
        let rows:Vec<(u64,Option<u64>,bool,i64,u64)>=tx.prepare("SELECT c.id,c.logical,c.metadata_known,c.kind,c.parent FROM contributions c JOIN tasks t ON t.generation=c.generation AND t.directory=c.parent AND t.kind=0 AND t.attempt=c.attempt JOIN entries e ON e.generation=c.generation AND e.id=c.id AND e.present=1 AND e.attempt=c.attempt WHERE c.generation=?1 AND c.id>?2 ORDER BY c.id LIMIT ?3")?.query_map(params![g,after.unwrap_or_else(||unsigned(0).to_vec()),limit as i64],|r|Ok((number(r,0)?,opt_number(r,1)?,r.get(2)?,r.get(3)?,number(r,4)?)))?.collect::<std::result::Result<_,_>>()?;
        // Per-step proof cache: at most `limit` IDs, discarded at transaction
        // end. Mutations cannot invalidate it within this single-writer step.
        let mut parents =
            std::collections::HashMap::with_capacity(if verify_branches { rows.len() } else { 0 });
        for (_, size, known, kind, parent) in &rows {
            if verify_branches {
                let current = match parents.get(parent) {
                    Some(value) => *value,
                    None => {
                        let value = current_branch(&tx, c.generation, EntryId(*parent), true)?;
                        parents.insert(*parent, value);
                        value
                    }
                };
                if !current {
                    continue;
                }
            }
            if !known
                || *kind == Kind::Unknown as i64
                || (*kind == Kind::File as i64 && size.is_none())
            {
                unknown = unknown
                    .checked_add(1)
                    .ok_or(Error::Contract("unknown counter exhausted"))?;
            }
            if *kind == Kind::File as i64
                && let Some(size) = size
            {
                if let Some(n) = sum.checked_add(*size) {
                    sum = n
                } else {
                    overflow = true;
                }
            }
        }
        let done = rows.len() < limit;
        tx.execute("UPDATE generations SET aggregate_cursor=coalesce(?2,aggregate_cursor),aggregate_sum=?3,aggregate_unknown=?4,aggregate_overflow=?5,aggregate_done=?6 WHERE id=?1",params![g,rows.last().map(|x|unsigned(x.0)),unsigned(sum),unsigned(unknown),overflow,done])?;
        metrics::commit(tx, Phase::ConsolidateCommit)?;
        Ok(done)
    }
    pub fn totals(&self, g: GenerationId) -> Result<Totals> {
        let (sum,unknown,overflow,done):(u64,u64,bool,bool)=self.db.query_row("SELECT aggregate_sum,aggregate_unknown,aggregate_overflow,aggregate_done FROM generations WHERE id=?1",[unsigned(g.0)],|r|Ok((number(r,0)?,number(r,1)?,r.get(2)?,r.get(3)?)))?;
        Ok(Totals {
            logical: if done && !overflow { Some(sum) } else { None },
            unknown,
            overflow,
            fold_complete: done,
        })
    }
    pub fn seal(&mut self, c: AcquisitionContext) -> Result<ScopeCoverage> {
        let _timer = Phase::Seal.start();
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        generation(&self.db, c)?;
        let totals = self.totals(c.generation)?;
        if !totals.fold_complete {
            return Err(Error::Contract("aggregate fold pending"));
        }
        let tx = self.db.transaction()?;
        let g = unsigned(c.generation.0);
        let pending: i64 = tx.query_row(
            "WITH RECURSIVE visible(id,kind) AS (
            SELECT id,kind FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent IS NULL AND present=1
            UNION ALL SELECT e.id,e.kind FROM entries e INDEXED BY entries_parent_id JOIN visible v ON e.parent=v.id
            WHERE e.generation=?1 AND v.kind=2 AND e.kind=2 AND e.present=1 AND e.id>e.parent
          ) SELECT count(*) FROM tasks t JOIN visible v ON v.id=t.directory
          JOIN entries e ON e.generation=t.generation AND e.id=t.directory
          WHERE t.generation=?1 AND t.kind=0 AND v.kind=2 AND (t.listing!=1 OR t.reconciled=0 OR t.parent_attempt!=e.attempt OR t.key IS NULL OR substr(t.key,1,length(?2))!=?2)",
            params![g,context(c)],
            |r| r.get(0),
        )?;
        let complete = pending == 0 && totals.unknown == 0 && !totals.overflow;
        let revision = tx
            .query_row("SELECT revision FROM generations WHERE id=?1", [g], |r| {
                number(r, 0)
            })?
            .checked_add(1)
            .ok_or(Error::Contract("revision exhausted"))?;
        tx.execute(
            "UPDATE generations SET sealed=1,coverage=?2,revision=?3 WHERE id=?1",
            params![g, if complete { 2 } else { 1 }, unsigned(revision)],
        )?;
        tx.execute("UPDATE identity SET latest=?1,latest_complete=CASE WHEN ?2 THEN ?1 ELSE latest_complete END", params![g,complete])?;
        metrics::commit(tx, Phase::SealCommit)?;
        Ok(if complete {
            ScopeCoverage::Complete
        } else {
            ScopeCoverage::Partial
        })
    }
    pub fn latest_complete(&self) -> Result<Option<GenerationId>> {
        Ok(self
            .db
            .query_row("SELECT latest_complete FROM identity", [], |r| {
                opt_number(r, 0)
            })?
            .map(GenerationId))
    }
    pub fn latest_sealed(&self) -> Result<Option<GenerationId>> {
        Ok(self
            .db
            .query_row("SELECT latest FROM identity", [], |r| opt_number(r, 0))?
            .map(GenerationId))
    }
    pub fn revision(&self, g: GenerationId) -> Result<Revision> {
        Ok(Revision(self.db.query_row(
            "SELECT revision FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| number(r, 0),
        )?))
    }
    pub fn entry_count(&self, g: GenerationId) -> Result<u64> {
        Ok(self.db.query_row(
            "SELECT count(*) FROM entries WHERE generation=?1",
            [unsigned(g.0)],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }
    pub fn task_count(&self, g: GenerationId) -> Result<u64> {
        Ok(self.db.query_row(
            "SELECT count(*) FROM tasks WHERE generation=?1",
            [unsigned(g.0)],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }
    pub fn lookup(
        &self,
        g: GenerationId,
        parent: EntryId,
        name: &NativeName,
    ) -> Result<Option<Entry>> {
        let row=self.db.query_row("SELECT id,parent,name,kind,logical,present,metadata_known,allocated,modified_s,modified_ns FROM entries WHERE generation=?1 AND parent=?2 AND name=?3",params![unsigned(g.0),unsigned(parent.0),name.encode()],|r|Ok((number(r,0)?,opt_number(r,1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,i64>(3)?,opt_number(r,4)?,r.get::<_,bool>(5)?,r.get::<_,bool>(6)?,opt_number(r,7)?,r.get::<_,Option<i64>>(8)?,r.get::<_,Option<u32>>(9)?))).optional()?;
        row.map(
            |(
                id,
                parent,
                name,
                kind,
                logical,
                present,
                metadata_known,
                allocated,
                seconds,
                nanos,
            )| {
                Ok(Entry {
                    key: EntryKey {
                        inventory: self.inventory,
                        generation: g,
                        entry: EntryId(id),
                    },
                    parent: parent.map(EntryId),
                    name: NativeName::decode(&name)
                        .map_err(|_| Error::Contract("invalid native key"))?,
                    kind: match kind {
                        0 => Kind::Unknown,
                        1 => Kind::File,
                        2 => Kind::Directory,
                        3 => Kind::Symlink,
                        4 => Kind::Special,
                        _ => return Err(Error::Contract("invalid entry kind")),
                    },
                    logical,
                    allocated,
                    modified: match (seconds, nanos) {
                        (Some(s), Some(n)) => Some((s, n)),
                        (None, None) => None,
                        _ => return Err(Error::Contract("inconsistent timestamp")),
                    },
                    present: present && visible(&self.db, g, EntryId(id))?,
                    metadata_known,
                })
            },
        )
        .transpose()
    }
}
fn validate_batch(observations: &[Observation]) -> Result<()> {
    if observations.len() > MAX_BATCH {
        return Err(Error::Contract("batch observation limit"));
    }
    let mut bytes = 0usize;
    for o in observations {
        let len = match &o.name {
            NativeName::Posix(b) => {
                if b.is_empty() || b.contains(&0) || b.contains(&b'/') || b == b"." || b == b".." {
                    return Err(Error::Contract("invalid POSIX component"));
                }
                b.len()
            }
            NativeName::Windows(w) => {
                if w.is_empty()
                    || w.iter().any(|x| [0, 47, 92].contains(x))
                    || w == &[46]
                    || w == &[46, 46]
                {
                    return Err(Error::Contract("invalid Windows component"));
                }
                w.len()
                    .checked_mul(2)
                    .ok_or(Error::Contract("name size overflow"))?
            }
            NativeName::Opaque(b) => b.len(),
        };
        bytes = bytes
            .checked_add(len)
            .and_then(|n| n.checked_add(162))
            .ok_or(Error::Contract("batch size overflow"))?;
        if bytes > MAX_BYTES {
            return Err(Error::Contract("batch byte limit"));
        }
        if o.modified.is_some_and(|(_, n)| n >= 1_000_000_000) {
            return Err(Error::Contract("invalid timestamp"));
        }
    }
    Ok(())
}
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod reconcile_plan_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod memory_tests;
