//! Dated, owned acquisition snapshots. Never a published/certified capability.
//! A read transaction stays pinned while bounded derivative quanta accumulate
//! measured file bytes. New writer revisions cannot reset this cursor.
use super::*;
use folder_totals::{FolderMeasure, Projection};
use sortsize_core::resources::{Category, Governor, Reservation};
use std::time::{Duration, Instant};

const MAX_DEPTH: usize = 32768;
const QUANTUM_OPERATIONS: usize = 65_536;
const MAX_CACHED_MEASURES: usize = 256;
const MEASURE_CACHE_BYTES: u64 = 32 * 1024;
// SQLite 3.54 may prefer the generation/id primary key for LIMIT 1, scanning
// unrelated siblings until EOF. The canonical parent/id index bounds each
// lookup to this parent's range, including empty/exhausted directories.
const CHILD_QUERY: &str = "SELECT id,kind,logical FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT 1";

/// Only the current savepoint may own these values. Repeated sibling files
/// update the same measure in order; SQLite receives its final value once.
/// Quota or allocation pressure keeps the original immediate-write path.
struct StepMeasures {
    rows: Vec<(EntryId, FolderMeasure)>,
    _memory: Option<Reservation>,
}
impl StepMeasures {
    fn new(governor: &Governor) -> Self {
        let mut rows = Vec::new();
        let memory = governor.reserve(Category::Queries, MEASURE_CACHE_BYTES, None);
        if memory.is_some()
            && rows.try_reserve_exact(MAX_CACHED_MEASURES).is_ok()
            && rows.capacity() * std::mem::size_of::<(EntryId, FolderMeasure)>()
                + std::mem::size_of::<Self>()
                <= MEASURE_CACHE_BYTES as usize
        {
            Self {
                rows,
                _memory: memory,
            }
        } else {
            // Drop any allocation before releasing the attempted reservation.
            drop(rows);
            drop(memory);
            Self {
                rows: Vec::new(),
                _memory: None,
            }
        }
    }

    fn read(&self, db: &Connection, id: EntryId) -> Result<Option<FolderMeasure>> {
        match self.rows.iter().find(|(entry, _)| *entry == id) {
            Some((_, measure)) => Ok(Some(*measure)),
            None => Store::folder_measure_raw(db, id),
        }
    }

    fn write(&mut self, db: &Connection, id: EntryId, measure: FolderMeasure) -> Result<()> {
        if let Some((_, stored)) = self.rows.iter_mut().find(|(entry, _)| *entry == id) {
            *stored = measure;
        } else if self._memory.is_some() && self.rows.len() < MAX_CACHED_MEASURES {
            self.rows.push((id, measure));
        } else {
            Store::write_folder_measure(db, id, measure)?;
        }
        Ok(())
    }

    fn flush(&self, db: &Connection) -> Result<()> {
        for (id, measure) in &self.rows {
            Store::write_folder_measure(db, *id, *measure)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    file: EntryId,
    id: EntryId,
    child_attempt: Option<u64>,
    ancestors: Vec<EntryId>,
    logical: Option<u64>,
    applying: Option<usize>,
}
/// The source's native owner and canonical VFS are shared, not reacquired.
/// This snapshot must close before the writer publishes/renames its group.
pub struct LivePreview {
    // Derived connection closes before the Store releases its borrowed RAM.
    projection: Option<Projection>,
    store: Store,
    context: AcquisitionContext,
    revision: Revision,
    after: EntryId,
    pending: Option<Pending>,
    complete: bool,
    selected: Option<EntryId>,
    priority_done: bool,
    _paths: Reservation,
}
/// Last committed Rust continuation only. Reading this never queries SQLite;
/// the cursor counts traversal positions, not an exact number of folded files.
#[derive(Clone, Copy, Debug)]
pub struct LivePreviewProgress {
    pub revision: u64,
    pub after: u64,
    pub pending_entry: Option<u64>,
    pub pending_ancestor: Option<u64>,
    pub pending_applying: Option<usize>,
    pub priority_done: bool,
    pub complete: bool,
}
impl LivePreview {
    pub fn progress_snapshot(&self) -> LivePreviewProgress {
        LivePreviewProgress {
            revision: self.revision.0,
            after: self.after.0,
            pending_entry: self.pending.as_ref().map(|p| p.file.0),
            pending_ancestor: self.pending.as_ref().map(|p| p.id.0),
            pending_applying: self.pending.as_ref().and_then(|p| p.applying),
            priority_done: self.priority_done,
            complete: self.complete,
        }
    }
    pub(super) fn open(
        source: &Store,
        c: AcquisitionContext,
        memory: Reservation,
        sealed: bool,
    ) -> std::result::Result<Self, Box<(Error, Reservation)>> {
        let opened = (|| -> Result<_> {
            source.require_integrity()?;
            if !source.db.is_autocommit() || source.db.is_readonly("main")? {
                return Err(Error::Contract(
                    "live preview requires owned acquisition writer",
                ));
            }
            require_generation_state(&source.db, c, sealed)?;
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let db = if let Some(canonical) = &source._canonical {
                Connection::open_with_flags_and_vfs(
                    source.directory.join("inventory.db"),
                    flags,
                    canonical.lock().unwrap().name(),
                )?
            } else {
                Connection::open_with_flags(source.directory.join("inventory.db"), flags)?
            };
            db.execute_batch(
                "PRAGMA trusted_schema=OFF;PRAGMA mmap_size=0;PRAGMA temp_store=FILE;",
            )?;
            Store::configure_cache(&db, &memory)?;
            db.busy_timeout(Duration::from_millis(100))?;
            db.execute_batch("BEGIN DEFERRED;")?;
            // This first read pins the actual WAL snapshot, before identity/context.
            require_generation_state(&db, c, sealed)?;
            let identity: (Vec<u8>, Vec<u8>) =
                db.query_row("SELECT inventory,source FROM identity", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            if identity
                != (
                    source.inventory.0.to_be_bytes().to_vec(),
                    source.source.0.to_be_bytes().to_vec(),
                )
            {
                return Err(Error::Contract("live preview identity mismatch"));
            }
            let revision = Revision(db.query_row(
                "SELECT revision FROM generations WHERE id=?1",
                [unsigned(c.generation.0)],
                |r| number(r, 0),
            )?);
            let paths = Governor::global()
                .reserve(Category::Publications, (MAX_DEPTH * 16) as u64, None)
                .ok_or(Error::Contract("live preview ancestor memory admission"))?;
            Ok((db, revision, paths))
        })();
        let (db, revision, paths) = match opened {
            Ok(opened) => opened,
            Err(error) => return Err(Box::new((error, memory))),
        };
        Ok(Self {
            store: Store {
                directory: source.directory.clone(),
                db,
                _canonical: source._canonical.clone(),
                folder_projection: Default::default(),
                _memory: memory,
                folder_memory: Default::default(),
                folder_cache_ready: Default::default(),
                _owner: source._owner.clone(),
                inventory: source.inventory,
                source: source.source,
                integrity_verified: true,
                retained_writer: false,
                fast_writer: false,
            },
            context: c,
            revision,
            projection: None,
            after: EntryId(0),
            pending: None,
            complete: false,
            selected: None,
            priority_done: false,
            _paths: paths,
        })
    }
    // Used only by the sealed fast-mode view, before creating any derivative.
    pub(super) fn into_store(self) -> Store {
        debug_assert!(self.projection.is_none());
        self.store
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
    pub(crate) fn acquisition_context(&self) -> AcquisitionContext {
        self.context
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    /// Stop admitting entry/ancestor operations after 65536 operations or 20ms,
    /// then flush at most 256 measures in the same savepoint. A long parent
    /// chain resumes in the next quantum without double counting.
    pub fn step(&mut self, scratch: &Path) -> Result<bool> {
        self.step_inner(None, scratch)
    }
    /// Prioritize one selected subtree using the existing parent/id index. The
    /// queue and visited-file set live in the quota-accounted derivative, not
    /// RAM. Scope cursors survive navigation; switching never deletes a large
    /// queue or restarts a visited subtree. A file contributes once even after rapid navigation or global work.
    /// True means this selected scope is folded; `complete()` remains global.
    pub fn step_folder(&mut self, parent: EntryId, scratch: &Path) -> Result<bool> {
        self.step_inner(Some(parent), scratch)
    }
    pub fn folder_ready(&self, parent: EntryId) -> bool {
        self.complete || self.selected == Some(parent) && self.priority_done
    }
    fn step_inner(&mut self, selected: Option<EntryId>, scratch: &Path) -> Result<bool> {
        self.step_with_governor(selected, scratch, &Governor::global())
    }
    fn step_with_governor(
        &mut self,
        selected: Option<EntryId>,
        scratch: &Path,
        governor: &Governor,
    ) -> Result<bool> {
        self.step_with_budget(selected, scratch, governor, QUANTUM_OPERATIONS)
    }
    // Tests can force a continuation without constructing thousands of deep
    // ancestors. Production always uses QUANTUM_OPERATIONS and the same 20ms
    // cooperative check; changing this budget never grows the measure cache.
    fn step_with_budget(
        &mut self,
        selected: Option<EntryId>,
        scratch: &Path,
        governor: &Governor,
        operations: usize,
    ) -> Result<bool> {
        self.settle_failed_step()?;
        if self.complete {
            return Ok(true);
        }
        if self.projection.is_none() {
            if self.store._memory.bytes() < 2 * 1024 * 1024 {
                return Err(Error::Contract(
                    "live preview requires planned folder capacity",
                ));
            }
            let kib = (self.store._memory.bytes() - 1536 * 1024) / 1024;
            self.store
                .db
                .pragma_update(None, "cache_size", -(kib as i64))?;
            self.store.db.execute_batch("PRAGMA shrink_memory;")?;
            let projection = Projection::open(&self.store, scratch)?;
            projection.db.execute_batch("CREATE TABLE folder_totals(id BLOB PRIMARY KEY,logical BLOB,unknown_count BLOB,files BLOB,dirs BLOB,allocated BLOB,allocation_unknown BLOB,allocation_lower_bound INTEGER) WITHOUT ROWID;")?;
            projection.db.execute_batch("CREATE TABLE preview_seen(id BLOB PRIMARY KEY) WITHOUT ROWID;CREATE TABLE preview_queue(scope BLOB NOT NULL,id BLOB NOT NULL,after BLOB NOT NULL,PRIMARY KEY(scope,id)) WITHOUT ROWID;CREATE TABLE preview_scopes(id BLOB PRIMARY KEY) WITHOUT ROWID;")?;
            self.projection = Some(projection);
        }
        let projection = self.projection.as_ref().unwrap();
        let tx = super::preview::ProjectionStep::begin(&projection.db)?;
        let mut measures = StepMeasures::new(governor);
        let mut priority_done = self.priority_done;
        let next_selected = selected.or(self.selected);
        let scope = unsigned(next_selected.map_or(0, |id| id.0));
        if let Some(parent) = selected
            && selected != self.selected
        {
            let kind: Option<i64> = self
                .store
                .db
                .query_row(
                    "SELECT kind FROM entries WHERE generation=?1 AND id=?2",
                    params![unsigned(self.context.generation.0), unsigned(parent.0)],
                    |r| r.get(0),
                )
                .optional()?;
            if kind != Some(Kind::Directory as i64)
                || !visible(&self.store.db, self.context.generation, parent)?
            {
                return Err(Error::Contract(
                    "live priority requires a visible directory",
                ));
            }
            priority_done = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM preview_scopes WHERE id=?1)",
                [unsigned(parent.0)],
                |r| r.get(0),
            )?;
            if !priority_done {
                tx.execute(
                    "INSERT OR IGNORE INTO preview_queue VALUES(?1,?1,?2)",
                    params![unsigned(parent.0), unsigned(0)],
                )?;
            }
        }
        let mut after = self.after;
        let mut pending = self.pending.clone();
        let mut complete = false;
        let started = Instant::now();
        let epoch = context(self.context);
        let g = unsigned(self.context.generation.0);
        // Reuse bytecode only within this savepoint. Empty directory walks are
        // still bounded by the same operation/time limits; neither visibility
        // proofs nor rows survive the quantum. Drop all statements before flush.
        {
            let mut next = self.store.db.prepare(
            "SELECT id,kind,logical FROM entries WHERE generation=?1 AND id>?2 ORDER BY id LIMIT 1",
        )?;
            let mut ancestor = self.store.db.prepare(
                "SELECT parent,present,kind,attempt FROM entries WHERE generation=?1 AND id=?2",
            )?;
            let mut task = self.store.db.prepare("SELECT parent_attempt,key,attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0")?;
            let mut queued_directory = tx
                .prepare("SELECT id,after FROM preview_queue WHERE scope=?1 ORDER BY id LIMIT 1")?;
            let mut child = self.store.db.prepare(CHILD_QUERY)?;
            let mut queue_advance =
                tx.prepare("UPDATE preview_queue SET after=?3 WHERE scope=?1 AND id=?2")?;
            let mut queue_insert =
                tx.prepare("INSERT OR IGNORE INTO preview_queue VALUES(?1,?2,?3)")?;
            let mut queue_remove =
                tx.prepare("DELETE FROM preview_queue WHERE scope=?1 AND id=?2")?;
            let mut seen = tx.prepare("SELECT EXISTS(SELECT 1 FROM preview_seen WHERE id=?1)")?;
            let mut mark_seen = tx.prepare("INSERT INTO preview_seen VALUES(?1)")?;
            for n in 0..operations {
                if n > 0 && started.elapsed() >= Duration::from_millis(20) {
                    break;
                }
                if pending.is_none() {
                    let queued = if !priority_done && next_selected.is_some() {
                        queued_directory
                            .query_row([scope], |r| {
                                Ok((EntryId(number(r, 0)?), EntryId(number(r, 1)?)))
                            })
                            .optional()?
                    } else {
                        None
                    };
                    let row = if let Some((parent, cursor)) = queued {
                        let row = child
                            .query_row(params![g, unsigned(parent.0), unsigned(cursor.0)], |r| {
                                Ok((
                                    EntryId(number(r, 0)?),
                                    r.get::<_, i64>(1)?,
                                    opt_number(r, 2)?,
                                ))
                            })
                            .optional()?;
                        if let Some((id, kind, _)) = row {
                            queue_advance.execute(params![
                                scope,
                                unsigned(parent.0),
                                unsigned(id.0)
                            ])?;
                            if kind == Kind::Directory as i64 {
                                queue_insert.execute(params![
                                    scope,
                                    unsigned(id.0),
                                    unsigned(0)
                                ])?;
                            }
                        } else {
                            queue_remove.execute(params![scope, unsigned(parent.0)])?;
                            continue;
                        }
                        row
                    } else {
                        if !priority_done && let Some(parent) = next_selected {
                            tx.execute(
                                "INSERT OR IGNORE INTO preview_scopes VALUES(?1)",
                                [unsigned(parent.0)],
                            )?;
                            priority_done = true;
                        }
                        let row = next
                            .query_row(params![g, unsigned(after.0)], |r| {
                                Ok((
                                    EntryId(number(r, 0)?),
                                    r.get::<_, i64>(1)?,
                                    opt_number(r, 2)?,
                                ))
                            })
                            .optional()?;
                        if let Some((id, _, _)) = row {
                            after = id;
                        }
                        row
                    };
                    let Some((id, kind, logical)) = row else {
                        complete = true;
                        break;
                    };
                    if kind != Kind::File as i64
                        || seen.query_row([unsigned(id.0)], |r| r.get::<_, bool>(0))?
                    {
                        continue;
                    }
                    pending = Some(Pending {
                        file: id,
                        id,
                        logical,
                        child_attempt: None,
                        ancestors: Vec::new(),
                        applying: None,
                    });
                }
                let p = pending.as_mut().unwrap();
                if let Some(index) = p.applying {
                    if index == p.ancestors.len() {
                        mark_seen.execute([unsigned(p.file.0)])?;
                        pending = None;
                        continue;
                    }
                    let id = p.ancestors[index];
                    let mut measure = measures.read(&tx, id)?.unwrap_or(FolderMeasure {
                        logical: Some(0),
                        ..Default::default()
                    });
                    measure.logical = measure
                        .logical
                        .and_then(|old| old.checked_add(p.logical.unwrap_or(0)));
                    measure.files = measure.files.saturating_add(1);
                    measure.unknown = measure
                        .unknown
                        .saturating_add(u64::from(p.logical.is_none()));
                    measures.write(&tx, id, measure)?;
                    p.applying = Some(index + 1);
                    continue;
                }
                let row = ancestor
                    .query_row(params![g, unsigned(p.id.0)], |r| {
                        Ok((
                            opt_number(r, 0)?,
                            r.get::<_, bool>(1)?,
                            r.get::<_, i64>(2)?,
                            number(r, 3)?,
                        ))
                    })
                    .optional()?;
                let Some((parent, present, kind, seen)) = row else {
                    pending = None;
                    continue;
                };
                let mut valid = present;
                if kind == Kind::Directory as i64 {
                    let row: Option<(u64, Option<Vec<u8>>, Option<u64>)> = task
                        .query_row(params![g, unsigned(p.id.0)], |r| {
                            Ok((number(r, 0)?, r.get(1)?, opt_number(r, 2)?))
                        })
                        .optional()?;
                    valid &= row.is_some_and(|(stamp, key, attempt)| {
                        stamp == seen
                            && key.is_some_and(|k| k.starts_with(&epoch))
                            && p.child_attempt.is_none_or(|s| attempt == Some(s))
                    });
                    if p.ancestors.len() == MAX_DEPTH {
                        return Err(Error::Contract("live preview ancestor depth admission"));
                    }
                    p.ancestors.push(p.id);
                } else if p.child_attempt.is_some() || kind != Kind::File as i64 {
                    valid = false;
                }
                if !valid {
                    pending = None;
                    continue;
                }
                if let Some(parent) = parent {
                    if parent >= p.id.0 {
                        return Err(Error::Contract("invalid live preview parent chain"));
                    }
                    p.id = EntryId(parent);
                    p.child_attempt = Some(seen);
                } else if p.id == EntryId(0) && kind == Kind::Directory as i64 {
                    p.applying = Some(0);
                } else {
                    pending = None;
                }
            }
        }
        // All cached values, visited files and scope cursors share the original
        // rollback boundary. Rust continuation state advances only after this
        // bounded flush and the savepoint release both succeed.
        measures.flush(&tx)?;
        tx.commit()?;
        self.after = after;
        self.pending = pending;
        self.complete = complete;
        self.selected = next_selected;
        self.priority_done = priority_done;
        Ok(complete || selected.is_some() && priority_done)
    }
    /// Known measured bytes of this dated snapshot. Always a lower bound while
    /// acquisition runs, including after the derivative itself has completed.
    /// Unknown-only and untouched subtrees never acquire an invented zero.
    pub fn measure(&self, id: EntryId) -> Result<Option<FolderMeasure>> {
        self.settle_failed_step()?;
        let Some(projection) = &self.projection else {
            return Ok(None);
        };
        let mut measure = Store::folder_measure_raw(&projection.db, id)?;
        if let Some(measure) = &mut measure {
            // Quanta may stop during propagation of one file. Do not expose
            // that file until all its ancestors have received the same delta.
            if let Some(p) = &self.pending
                && let Some(index) = p.applying
                && index < p.ancestors.len()
                && p.ancestors[..index].contains(&id)
            {
                measure.logical = measure
                    .logical
                    .and_then(|v| v.checked_sub(p.logical.unwrap_or(0)));
                measure.files -= 1;
                measure.unknown -= u64::from(p.logical.is_none());
            }
            if measure.files == measure.unknown {
                measure.logical = None;
            }
        }
        Ok(measure)
    }

    /// A persistent SQLite interruption can also interrupt the savepoint's
    /// best-effort Drop rollback. Rust cursors still describe the last commit,
    /// so never read or reuse the derivative until the outstanding transaction
    /// has really rolled back. Its connection has no intentional outer writer
    /// transaction; the pinned source snapshot lives on a different connection.
    fn settle_failed_step(&self) -> Result<()> {
        if let Some(projection) = &self.projection
            && !projection.db.is_autocommit()
        {
            projection.db.execute_batch("ROLLBACK;")?;
            if !projection.db.is_autocommit() {
                return Err(Error::Contract("live preview rollback remains incomplete"));
            }
        }
        Ok(())
    }

    pub(super) fn close(self) -> Result<Reservation> {
        let Self {
            store, projection, ..
        } = self;
        drop(projection);
        super::generation_a::release_store(store)
    }
}
#[cfg(test)]
mod tests;
