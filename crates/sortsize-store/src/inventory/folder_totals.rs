//! Reconstructible, bounded-step folder sums. Never traverse the source filesystem.
use super::*;
use std::time::{Duration, Instant};
pub const PROJECTION_DISK_BYTES: u64 = 512 * 1024 * 1024;
#[derive(Default)]
pub struct FolderFold {
    revision: Option<(GenerationId, Revision)>,
    before: Option<EntryId>,
    done: bool,
    owner: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderMeasure {
    pub logical: Option<u64>,
    pub unknown: u64,
    pub allocated: Option<u64>,
    pub allocation_unknown: u64,
    pub allocation_lower_bound: bool,
    pub files: u64,
    pub directories: u64,
}
// db must close before its registered VFS and workspace are dropped.
pub(super) struct Projection {
    pub(super) db: Connection,
    _vfs: queries::workspace::vfs::Vfs,
    space: std::sync::Arc<queries::workspace::Workspace>,
    owner: u64,
    complete: bool,
}
impl Projection {
    pub(super) fn open(store: &Store, scratch: &Path) -> Result<Self> {
        Self::open_limit(store, scratch, PROJECTION_DISK_BYTES)
    }
    fn open_limit(store: &Store, scratch: &Path, limit: u64) -> Result<Self> {
        let space = queries::workspace::Workspace::derived(store, scratch, limit)?;
        let vfs = queries::workspace::vfs::Vfs::new(space.clone())?;
        let db = crate::open_with_vfs(
            &space.path().join("folders.db"),
            Durability::Reconstructible,
            Some(vfs.name()),
        )?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 512 * 1024)?;
        db.execute_batch(
            "PRAGMA page_size=4096;PRAGMA cache_size=-512;PRAGMA max_page_count=32768;",
        )?;
        let page_size: i64 = db.pragma_query_value(None, "page_size", |r| r.get(0))?;
        if page_size != 4096 {
            return Err(Error::Contract("folder projection page size"));
        }
        Ok(Self {
            db,
            _vfs: vfs,
            space,
            owner: 0,
            complete: false,
        })
    }
    fn next_owner() -> Result<u64> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Contract("folder projection ownership exhausted"))
    }
}
impl Store {
    /// One derived folder projection per Store; invalidate on any inventory revision.
    pub fn folder_fold_step(
        &self,
        generation: GenerationId,
        fold: &mut FolderFold,
    ) -> Result<bool> {
        self.folder_fold_step_at(generation, fold, &std::env::temp_dir())
    }
    /// Explicit private scratch for the derivative; canonical bytes remain RO.
    pub fn folder_fold_step_at(
        &self,
        generation: GenerationId,
        fold: &mut FolderFold,
        scratch: &Path,
    ) -> Result<bool> {
        let revision = self.revision(generation)?;
        if !self.folder_cache_ready.get() {
            const MIB: u64 = 1024 * 1024;
            if self._memory.bytes() >= 2 * MIB {
                // The main connection may have admitted every available byte.
                // Rebalance its existing lease: 512 KiB main overhead, 1 MiB
                // TEMP cache + overhead, and >=512 KiB main page cache. No
                // admission is bypassed and no new byte is charged twice.
                let kib = (self._memory.bytes() - MIB - MIB / 2) / 1024;
                self.db.pragma_update(None, "cache_size", -(kib as i64))?;
                self.db.execute_batch("PRAGMA shrink_memory;")?;
            } else {
                // A minimally admitted Store has no capacity to lend. Keep the
                // real extra admission, and refuse honestly if it cannot fit.
                *self.folder_memory.borrow_mut() = Some(
                    sortsize_core::resources::Governor::global()
                        .reserve(sortsize_core::resources::Category::Sqlite, MIB, None)
                        .ok_or(Error::Contract("folder projection memory budget"))?,
                );
            }
            self.folder_cache_ready.set(true);
        }
        let mut slot = self.folder_projection.borrow_mut();
        // A failed rollback must never let a later quantum inherit partial
        // contributions. Rebuild this disposable cache if closure was uncertain.
        if slot.as_ref().is_some_and(|p| !p.db.is_autocommit()) {
            slot.take();
        }
        if slot.is_none() {
            *slot = Some(Projection::open(self, scratch)?);
        }
        let projection = slot
            .as_mut()
            .ok_or(Error::Contract("folder projection missing"))?;
        let owns = fold.owner != 0
            && fold.owner == projection.owner
            && if fold.revision.is_some() {
                projection.db.query_row(
                    "SELECT generation,revision FROM folder_stamp",
                    [],
                    |r| Ok((number(r, 0)?, number(r, 1)?)),
                )? == (generation.0, revision.0)
            } else {
                false
            };
        if !owns || fold.revision != Some((generation, revision)) {
            projection.complete = false;
            projection.owner = Projection::next_owner()?;
            projection.db.execute_batch("DROP TABLE IF EXISTS folder_totals; DROP TABLE IF EXISTS folder_stamp; CREATE TABLE folder_stamp(generation BLOB,revision BLOB); CREATE TABLE folder_totals(id BLOB PRIMARY KEY,logical BLOB,unknown_count BLOB,files BLOB,dirs BLOB,allocated BLOB,allocation_unknown BLOB,allocation_lower_bound INTEGER) WITHOUT ROWID;")?;
            projection.db.execute(
                "INSERT INTO folder_stamp VALUES(?1,?2)",
                params![unsigned(generation.0), unsigned(revision.0)],
            )?;
            fold.revision = Some((generation, revision));
            fold.owner = projection.owner;
            fold.before = None;
            fold.done = false;
        }
        if fold.done && projection.complete {
            return Ok(true);
        }
        let started = Instant::now();
        let tx = super::preview::ProjectionStep::begin(&projection.db)?;
        let mut after = fold.before;
        let mut done = false;
        {
            let schema: i64 = self
                .db
                .pragma_query_value(None, "user_version", |r| r.get(0))?;
            let sql = if schema >= 6 {
                "SELECT id,parent,kind,logical,allocated,COALESCE((SELECT allocated_state=5 FROM entry_properties p WHERE p.generation=entries.generation AND p.id=entries.id),0) FROM entries WHERE generation=?1 AND (?2 IS NULL OR id<?2) ORDER BY id DESC LIMIT 256"
            } else {
                "SELECT id,parent,kind,logical,allocated,0 FROM entries WHERE generation=?1 AND (?2 IS NULL OR id<?2) ORDER BY id DESC LIMIT 256"
            };
            // A nullable OR prevents an id-range seek and re-scans the prefix
            // on every quantum. Include MAX on the first step; strict thereafter.
            let predicate = if fold.before.is_some() {
                "id<?2"
            } else {
                "id<=?2"
            };
            let sql = sql.replace("(?2 IS NULL OR id<?2)", predicate);
            let mut q = self.db.prepare(&sql)?;
            let mut rows = q.query(params![
                unsigned(generation.0),
                unsigned(fold.before.map_or(u64::MAX, |v| v.0))
            ])?;
            let mut branch = CurrentBranch::new(&self.db, generation)?;
            let mut pending: Option<(EntryId, FolderMeasure)> = None;
            let mut n = 0;
            while n == 0 || started.elapsed() < Duration::from_millis(20) {
                let Some(row) = rows.next()? else {
                    // Exhausting a LIMIT batch is not the end of the inventory.
                    done = n < 256;
                    break;
                };
                let id = EntryId(number(row, 0)?);
                after = Some(id);
                n += 1;
                if !branch.check(id, false)? {
                    continue;
                }
                let parent = opt_number(row, 1)?.map(EntryId);
                let kind: i64 = row.get(2)?;
                // Only one parent's consecutive contributions are retained. Flush
                // before reading a directory subtotal: it may itself be that
                // parent. This keeps reverse-ID postorder and rollback semantics.
                if pending
                    .as_ref()
                    .is_some_and(|(p, _)| Some(*p) != parent || kind == 2)
                    && let Some((parent, measure)) = pending.take()
                {
                    Self::write_folder_measure(&tx, parent, measure)?;
                }
                let measure = match kind {
                    1 => {
                        let logical = opt_number(row, 3)?;
                        FolderMeasure {
                            logical,
                            allocated: opt_number(row, 4)?,
                            allocation_unknown: u64::from(opt_number(row, 4)?.is_none()),
                            allocation_lower_bound: row.get(5)?,
                            unknown: u64::from(logical.is_none()),
                            files: 1,
                            directories: 0,
                        }
                    }
                    2 => Self::folder_measure_raw(&tx, id)?.unwrap_or(FolderMeasure {
                        logical: Some(0),
                        allocated: Some(0),
                        ..Default::default()
                    }),
                    0 => FolderMeasure {
                        logical: None,
                        allocated: None,
                        unknown: 1,
                        allocation_unknown: 1,
                        ..Default::default()
                    },
                    _ => FolderMeasure {
                        logical: Some(0),
                        allocated: Some(0),
                        ..Default::default()
                    },
                };
                if kind == 2 {
                    Self::write_folder_measure(&tx, id, measure)?;
                }
                if let Some(parent) = parent {
                    let old = match pending.take() {
                        Some((_, old)) => old,
                        None => Self::folder_measure_raw(&tx, parent)?.unwrap_or(FolderMeasure {
                            logical: Some(0),
                            allocated: Some(0),
                            ..Default::default()
                        }),
                    };
                    pending = Some((
                        parent,
                        FolderMeasure {
                            logical: old
                                .logical
                                .zip(measure.logical)
                                .and_then(|(a, b)| a.checked_add(b)),
                            allocated: old
                                .allocated
                                .zip(measure.allocated)
                                .and_then(|(a, b)| a.checked_add(b)),
                            unknown: old.unknown.saturating_add(measure.unknown),
                            allocation_unknown: old
                                .allocation_unknown
                                .saturating_add(measure.allocation_unknown),
                            files: old.files.saturating_add(measure.files),
                            directories: old
                                .directories
                                .saturating_add(measure.directories)
                                .saturating_add(u64::from(kind == 2)),
                            allocation_lower_bound: old.allocation_lower_bound
                                || measure.allocation_lower_bound,
                        },
                    ));
                }
            }
            if let Some((parent, measure)) = pending {
                Self::write_folder_measure(&tx, parent, measure)?;
            }
        }
        tx.commit()?;
        fold.before = after;
        fold.done = done;
        projection.complete = done;
        Ok(done)
    }
    pub(super) fn write_folder_measure(
        db: &Connection,
        id: EntryId,
        measure: FolderMeasure,
    ) -> Result<()> {
        db.prepare("INSERT OR REPLACE INTO folder_totals VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?
            .execute(params![
                unsigned(id.0),
                measure.logical.map(unsigned),
                unsigned(measure.unknown),
                unsigned(measure.files),
                unsigned(measure.directories),
                measure.allocated.map(unsigned),
                unsigned(measure.allocation_unknown),
                measure.allocation_lower_bound
            ])?;
        Ok(())
    }
    pub(super) fn folder_measure_raw(
        db: &Connection,
        id: EntryId,
    ) -> Result<Option<FolderMeasure>> {
        Ok(db
            .prepare("SELECT logical,unknown_count,files,dirs,allocated,allocation_unknown,allocation_lower_bound FROM folder_totals WHERE id=?1")?.query_row(
                [unsigned(id.0)],
                |r| {
                    Ok(FolderMeasure {
                        logical: opt_number(r, 0)?,
                        unknown: number(r, 1)?,
                        files: number(r, 2)?,
                        directories: number(r, 3)?,
                        allocated:opt_number(r,4)?,allocation_unknown:number(r,5)?,allocation_lower_bound:r.get(6)?,
                    })
                },
            )
            .optional()?)
    }
    pub fn folder_measure(
        &self,
        generation: GenerationId,
        id: EntryId,
        fold: &FolderFold,
    ) -> Result<Option<FolderMeasure>> {
        if !fold.done || fold.revision != Some((generation, self.revision(generation)?)) {
            return Ok(None);
        }
        let projection = self.folder_projection.borrow();
        let Some(projection) = projection.as_ref() else {
            return Ok(None);
        };
        if fold.owner != projection.owner || !projection.complete {
            return Ok(None);
        }
        let stamp =
            projection
                .db
                .query_row("SELECT generation,revision FROM folder_stamp", [], |r| {
                    Ok((number(r, 0)?, number(r, 1)?))
                })?;
        if stamp != (generation.0, self.revision(generation)?.0)
            || !visible(&self.db, generation, id)?
        {
            return Ok(None);
        }
        Self::folder_measure_raw(&projection.db, id)
    }
    pub fn folder_temporary_usage(&self) -> Option<queries::TemporaryUsage> {
        self.folder_projection
            .borrow()
            .as_ref()
            .map(|p| p.space.usage())
    }
    pub fn navigation_parent(
        &self,
        generation: GenerationId,
        id: EntryId,
    ) -> Result<Option<EntryId>> {
        if !visible(&self.db, generation, id)? {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT parent FROM entries WHERE generation=?1 AND id=?2",
                params![unsigned(generation.0), unsigned(id.0)],
                |r| opt_number(r, 0),
            )?
            .map(EntryId))
    }
    pub fn navigation_path(
        &self,
        generation: GenerationId,
        id: EntryId,
        root: &Path,
    ) -> Result<std::path::PathBuf> {
        if !visible(&self.db, generation, id)? {
            return Err(Error::Contract("removed navigation target"));
        }
        self.recorded_path(generation, id, root)
    }
}

#[cfg(test)]
mod tests;
