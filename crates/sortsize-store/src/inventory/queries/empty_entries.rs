//! Live, read-only searches for zero-byte files or directories with no entries.
//! The native scheduler supplies guarded metadata without opening file contents.
use super::{
    Error, Result, ScopeCoverage, TemporaryUsage, decode_recorded_path, native_path, time,
    workspace::{Workspace, vfs::Vfs},
};
use crate::{
    inventory::Kind,
    scheduler::{direct, native_name},
};
use rusqlite::{Connection, OpenFlags, Statement, Transaction, params};
use sortsize_core::{
    CancellationToken,
    acquisition::{Attachment, Guard, ScanObservation, Stamp},
    inventory_contract::{EntryId, ListingEnd},
    resources::{Category, Governor, Reservation},
};
use std::{
    cell::RefCell,
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

const MAX_ROWS: usize = 100;
const PAGE_BYTES: usize = 512 * 1024;
const QUERY_MEMORY: u64 = 2 * 1024 * 1024;
const SQLITE_MEMORY: u64 = 2 * 1024 * 1024;
const MAX_FILTER_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyKind {
    Files,
    Directories,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmptySort {
    #[default]
    PathAscending,
    PathDescending,
    ModifiedNewest,
    ModifiedOldest,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmptyProgress {
    /// Raw listing entries, including links and exclusions.
    pub entries_seen: u64,
    /// Directory attempts concluded, including incomplete listings.
    pub directories_scanned: u64,
    /// Observations during discovery. Final results exclude invalidated subtrees.
    pub matches: u64,
    pub errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmptySummary {
    pub progress: EmptyProgress,
    pub coverage: ScopeCoverage,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyRow {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug)]
pub struct EmptyPage {
    pub rows: Vec<EmptyRow>,
    pub previous_offset: Option<u64>,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyParent {
    pub path: Option<PathBuf>,
    pub count: u64,
}

#[derive(Debug)]
pub struct EmptyParentPage {
    pub rows: Vec<EmptyParent>,
    pub previous_offset: Option<u64>,
    pub next_offset: Option<u64>,
}

/// A disposable result set. Rows report observations from this scan interval.
/// The native checks validate each listing while it is acquired. Files can
/// subsequently change in place without a directory timestamp change.
pub struct EmptySearch {
    db: RefCell<Connection>,
    _vfs: Vfs,
    space: Arc<Workspace>,
    _query_memory: Reservation,
    _sqlite_memory: Reservation,
    summary: EmptySummary,
    page_limits: PageLimits,
}

struct PageLimits {
    rows: usize,
    parents: usize,
}

/// Scan an authorized root and retain the result pages under one disk limit.
/// Run on a worker. Cancellation returns Interrupted and drops the workspace.
pub fn prepare(
    guard: Arc<Guard>,
    scratch: &Path,
    temporary_limit: u64,
    kind: EmptyKind,
    cancel: &CancellationToken,
    mut progress: impl FnMut(&EmptyProgress),
) -> Result<EmptySearch> {
    let result = (|| {
        check_cancel(cancel)?;
        if guard.scan_observation() != ScanObservation::Strict || !guard.policy().same_volume {
            return Err(Error::Contract(
                "empty analysis requires a strict source guard",
            ));
        }
        if !guard.work_path_allowed(scratch)? {
            return Err(Error::Contract(
                "empty analysis scratch overlaps selected source",
            ));
        }
        if temporary_limit < 64 * 1024 {
            return Err(quota_error(temporary_limit));
        }
        let governor = Governor::global();
        let reserve = |category, bytes| {
            governor
                .reserve(category, bytes, None)
                .ok_or_else(|| Error::memory_admission(&governor, category, bytes))
        };
        let query_memory = reserve(Category::Queries, QUERY_MEMORY)?;
        let sqlite_memory = reserve(Category::Sqlite, SQLITE_MEMORY)?;
        let space = Workspace::current_folder(scratch, temporary_limit)?;
        if !guard.work_path_allowed(space.path())? {
            return Err(Error::Contract(
                "empty analysis scratch overlaps selected source",
            ));
        }
        let (mut db, vfs) = open_results(&space)?;
        let (summary, page_limits) = with_cancellation(&mut db, cancel, |db| {
            let mut sink = ScanSink::new(db, kind)?;
            direct::run_candidates(guard.clone(), cancel, space.file()?, None, |event| {
                let children = sink.accept(event, cancel)?;
                progress(&sink.progress);
                Ok(children)
            })?;
            check_cancel(cancel)?;
            // Do not publish observations of a scope that ceased to be
            // attached after its initial listing completed.
            let root = guard.inspect(guard.root())?;
            let attachment = guard.evidence().1.attachment(&root);
            if attachment == Attachment::Different {
                return Err(io::Error::other("The selected folder is no longer attached").into());
            }
            if sink.root_at_eof.as_ref().is_some_and(|stamp| {
                stamp.identity != root.identity
                    || stamp.created != root.created
                    || stamp.modified != root.modified
            }) {
                sink.incomplete = true;
                sink.progress.errors += 1;
                sink.exclude_subtree(guard.root())?;
            }
            let page_limits = PageLimits {
                rows: page_limit(sink.max_row_bytes),
                parents: page_limit(sink.max_parent_bytes),
            };
            let summary = sink.finish(attachment, cancel)?;
            progress(&summary.progress);
            Ok((summary, page_limits))
        })?;
        Ok(EmptySearch {
            db: RefCell::new(db),
            _vfs: vfs,
            space,
            _query_memory: query_memory,
            _sqlite_memory: sqlite_memory,
            summary,
            page_limits,
        })
    })();
    result.map_err(|error| temporary_error(error, temporary_limit))
}

fn open_results(space: &Arc<Workspace>) -> Result<(Connection, Vfs)> {
    let vfs = Vfs::new(space.clone())?;
    let db = Connection::open_with_flags_and_vfs(
        space.path().join("empty-entries.db"),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        vfs.name(),
    )?;
    db.execute_batch(
        "PRAGMA page_size=4096; PRAGMA synchronous=OFF; PRAGMA journal_mode=DELETE;
         PRAGMA temp_store=FILE; PRAGMA mmap_size=0; PRAGMA threads=0;
         PRAGMA cache_size=-1024; PRAGMA temp.cache_size=-256; PRAGMA cache_spill=ON;",
    )?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 256 * 1024)?;
    db.pragma_update(
        None,
        "max_page_count",
        (space.usage().limit / 4096).min(u64::from(u32::MAX) - 1) as u32,
    )?;
    db.execute_batch(
        "BEGIN;
         CREATE TABLE results(
            path BLOB PRIMARY KEY, parent BLOB, display TEXT NOT NULL,
            modified_s INTEGER, modified_ns INTEGER
         ) WITHOUT ROWID;
         CREATE TABLE invalidated(
            path BLOB PRIMARY KEY, lower BLOB NOT NULL, upper BLOB NOT NULL
         ) WITHOUT ROWID;
         COMMIT;",
    )?;
    Ok((db, vfs))
}

struct ScanSink<'a> {
    db: &'a Connection,
    insert: Statement<'a>,
    exclude: Statement<'a>,
    tx: Option<Transaction<'a>>,
    kind: EmptyKind,
    serial: u64,
    mutations: usize,
    incomplete: bool,
    root_at_eof: Option<Stamp>,
    max_row_bytes: usize,
    max_parent_bytes: usize,
    progress: EmptyProgress,
}

impl<'a> ScanSink<'a> {
    fn new(db: &'a Connection, kind: EmptyKind) -> Result<Self> {
        Ok(Self {
            db,
            insert: db.prepare(
                "INSERT INTO results(path,parent,display,modified_s,modified_ns)
                 VALUES(?1,?2,?3,?4,?5) ON CONFLICT(path) DO NOTHING",
            )?,
            exclude: db
                .prepare("INSERT INTO invalidated VALUES(?1,?2,?3) ON CONFLICT(path) DO NOTHING")?,
            tx: Some(db.unchecked_transaction()?),
            kind,
            serial: 0,
            mutations: 0,
            incomplete: false,
            root_at_eof: None,
            max_row_bytes: 0,
            max_parent_bytes: 0,
            progress: EmptyProgress::default(),
        })
    }

    fn accept(
        &mut self,
        event: direct::DirectBatch<'_>,
        cancel: &CancellationToken,
    ) -> Result<Vec<direct::ObservedDirectory>> {
        check_cancel(cancel)?;
        let batch = event.batch;
        self.progress.entries_seen += event.entries_seen;
        self.progress.errors += batch.errors;
        self.progress.directories_scanned += u64::from(batch.end.is_some());
        if batch.parent == EntryId(0) && batch.end == Some(ListingEnd::Exhausted) {
            self.root_at_eof = batch.directory.cloned();
        }
        if batch.detached || batch.end == Some(ListingEnd::Incomplete) {
            self.incomplete = true;
            self.exclude_subtree(event.parent_path)?;
            return Ok(Vec::new());
        }
        if self.kind == EmptyKind::Directories
            && batch.sequence == 0
            && batch.end == Some(ListingEnd::Exhausted)
            && event.entries_seen == 0
            && batch.errors == 0
        {
            // EOF is accompanied by fresh directory metadata. Require enough
            // evidence to detect identity and mtime changes across the listing.
            if let Some(stamp) = batch
                .directory
                .filter(|s| s.directory && s.identity.is_some() && s.modified.is_some())
            {
                self.insert(
                    event.parent_path,
                    stamp.modified.and_then(crate::scheduler::timestamp),
                )?;
            } else {
                self.incomplete = true;
            }
        }
        let mut children = Vec::new();
        for (row, evidence) in batch.observations.iter().zip(batch.evidence) {
            check_cancel(cancel)?;
            if row.kind == Kind::Directory {
                self.serial = self
                    .serial
                    .checked_add(1)
                    .ok_or(Error::Contract("empty analysis directory ID exhausted"))?;
                children.push(direct::ObservedDirectory {
                    id: EntryId(self.serial),
                    name: row.name.clone(),
                    evidence: evidence.clone(),
                });
            } else if self.kind == EmptyKind::Files
                && row.kind == Kind::File
                && row.logical == Some(0)
                && row.metadata_known
            {
                let name = native_name(&row.name)?;
                let mut components = Path::new(&name).components();
                if !matches!(components.next(), Some(Component::Normal(_)))
                    || components.next().is_some()
                {
                    return Err(Error::Contract("invalid empty file name"));
                }
                let path = event.parent_path.join(name);
                if path.parent() != Some(event.parent_path) {
                    return Err(Error::Contract("invalid empty file scope"));
                }
                self.insert(&path, row.modified)?;
            }
        }
        Ok(children)
    }

    fn insert(&mut self, path: &Path, modified: Option<(i64, u32)>) -> Result<()> {
        let (seconds, nanos) = modified.unzip();
        let display = path.to_string_lossy().to_lowercase();
        let parent_bytes = path.parent().map_or(0, path_bytes);
        self.max_row_bytes = self
            .max_row_bytes
            .max(2 * (path_bytes(path) + parent_bytes) + std::mem::size_of::<EmptyRow>());
        self.max_parent_bytes = self
            .max_parent_bytes
            .max(2 * parent_bytes + std::mem::size_of::<EmptyParent>());
        let parent = path.parent().map(|parent| native_path(parent).encode());
        let inserted = self.insert.execute(params![
            native_path(path).encode(),
            parent,
            display,
            seconds,
            nanos
        ])?;
        self.progress.matches += inserted as u64;
        self.progress.errors += u64::from(inserted == 0);
        self.advance_batch()
    }

    fn exclude_subtree(&mut self, path: &Path) -> Result<()> {
        // Append a native separator to the exact occurrence spelling (the
        // separator is already present for a drive or filesystem root).
        // The codec retains a literal ASCII separator as the last byte,
        // including its big-endian Windows UTF-16 representation.
        let lower = native_path(&path.join("")).encode();
        let mut upper = lower.clone();
        let separator = upper.last_mut().unwrap();
        if *separator != std::path::MAIN_SEPARATOR as u8 {
            return Err(Error::Contract("invalid empty analysis directory prefix"));
        }
        *separator += 1;
        self.exclude
            .execute(params![native_path(path).encode(), lower, upper])?;
        self.advance_batch()
    }

    fn advance_batch(&mut self) -> Result<()> {
        self.mutations += 1;
        if self.mutations == 512 {
            self.tx.take().unwrap().commit()?;
            self.tx = Some(self.db.unchecked_transaction()?);
            self.mutations = 0;
        }
        Ok(())
    }

    fn finish(
        mut self,
        attachment: Attachment,
        cancel: &CancellationToken,
    ) -> Result<EmptySummary> {
        self.tx.take().unwrap().commit()?;
        if self.incomplete {
            // The scheduler may have visited a child before detecting a
            // mutation of its parent at EOF. Prune the entire uncertain
            // subtree, including results from earlier quanta.
            let mut excluded = self
                .db
                .prepare("SELECT path,lower,upper FROM invalidated")?;
            let mut rows = excluded.query([])?;
            let mut remove = self
                .db
                .prepare("DELETE FROM results WHERE path=?1 OR (path>=?2 AND path<?3)")?;
            while let Some(row) = rows.next()? {
                check_cancel(cancel)?;
                let path: Vec<u8> = row.get(0)?;
                let lower: Vec<u8> = row.get(1)?;
                let upper: Vec<u8> = row.get(2)?;
                self.progress.matches -= remove.execute(params![path, lower, upper])? as u64;
            }
        }
        // Building these once avoids maintaining secondary indexes on each
        // acquisition write. The disk-backed work and indexes share the quota.
        self.db.execute_batch(
            "CREATE INDEX by_parent ON results(parent);
             CREATE INDEX by_recent ON results(modified_s IS NULL,modified_s DESC,modified_ns DESC,path);
             CREATE INDEX by_oldest ON results(modified_s IS NULL,modified_s,modified_ns,path);
             PRAGMA query_only=ON;",
        )?;
        Ok(EmptySummary {
            progress: self.progress,
            coverage: if !self.incomplete
                && self.progress.errors == 0
                && attachment == Attachment::Compatible
            {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
            cancelled: false,
        })
    }
}

impl EmptySearch {
    pub fn summary(&self) -> &EmptySummary {
        &self.summary
    }

    pub fn temporary_usage(&self) -> TemporaryUsage {
        self.space.usage()
    }

    /// Filter matches a case-insensitive substring of the display path.
    /// The native path itself is retained losslessly. Unknown dates sort last.
    pub fn page(
        &self,
        filter: &str,
        sort: EmptySort,
        offset: u64,
        cancel: &CancellationToken,
    ) -> Result<EmptyPage> {
        let order = match sort {
            EmptySort::PathAscending => "path",
            EmptySort::PathDescending => "path DESC",
            EmptySort::ModifiedNewest => "modified_s IS NULL,modified_s DESC,modified_ns DESC,path",
            EmptySort::ModifiedOldest => "modified_s IS NULL,modified_s,modified_ns,path",
        };
        let sql = format!(
            "SELECT path,parent,modified_s,modified_ns FROM results
             WHERE ?1='' OR instr(display,?1)>0 ORDER BY {order} LIMIT ?2 OFFSET ?3",
        );
        let step = self.page_limits.rows;
        let (rows, next_offset) = self.query_page(&sql, filter, step, offset, cancel, |row| {
            let path = decode_recorded_path(&row.get::<_, Vec<u8>>(0)?)?.into_owned();
            let parent = optional_path(row, 1)?;
            Ok(EmptyRow {
                path,
                parent,
                modified: time(row.get(2)?, row.get(3)?),
            })
        })?;
        Ok(EmptyPage {
            rows,
            previous_offset: previous_offset(offset, step),
            next_offset,
        })
    }

    pub fn parents(
        &self,
        filter: &str,
        offset: u64,
        cancel: &CancellationToken,
    ) -> Result<EmptyParentPage> {
        let step = self.page_limits.parents;
        let (rows, next_offset) = self.query_page(
            "SELECT parent,count(*) FROM results WHERE ?1='' OR instr(display,?1)>0
             GROUP BY parent ORDER BY parent LIMIT ?2 OFFSET ?3",
            filter,
            step,
            offset,
            cancel,
            |row| {
                let path = optional_path(row, 0)?;
                Ok(EmptyParent {
                    path,
                    count: row.get::<_, i64>(1)? as u64,
                })
            },
        )?;
        Ok(EmptyParentPage {
            rows,
            previous_offset: previous_offset(offset, step),
            next_offset,
        })
    }

    fn query_page<T>(
        &self,
        sql: &str,
        filter: &str,
        step: usize,
        offset: u64,
        cancel: &CancellationToken,
        mut decode: impl FnMut(&rusqlite::Row<'_>) -> Result<T>,
    ) -> Result<(Vec<T>, Option<u64>)> {
        if filter.len() > MAX_FILTER_BYTES {
            return Err(Error::Contract(
                "empty analysis filter exceeds input budget",
            ));
        }
        let native_offset = i64::try_from(offset)
            .map_err(|_| Error::Contract("empty analysis page offset overflow"))?;
        with_cancellation(&mut self.db.borrow_mut(), cancel, |db| {
            let mut statement = db.prepare(sql)?;
            let mut result = statement.query(params![
                filter.to_lowercase(),
                step as i64 + 1,
                native_offset
            ])?;
            let mut rows = Vec::with_capacity(step);
            while let Some(row) = result.next()? {
                check_cancel(cancel)?;
                if rows.len() == step {
                    let next = offset + step as u64;
                    return Ok((rows, Some(next)));
                }
                rows.push(decode(row)?);
            }
            Ok((rows, None))
        })
        .map_err(|error| temporary_error(error, self.space.usage().limit))
    }
}

fn page_limit(max_bytes: usize) -> usize {
    (PAGE_BYTES / max_bytes.max(1)).clamp(1, MAX_ROWS)
}

fn previous_offset(offset: u64, step: usize) -> Option<u64> {
    (offset > 0).then_some(offset.saturating_sub(step as u64))
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn optional_path(row: &rusqlite::Row<'_>, index: usize) -> Result<Option<PathBuf>> {
    row.get::<_, Option<Vec<u8>>>(index)?
        .as_deref()
        .map(|raw| decode_recorded_path(raw).map(std::borrow::Cow::into_owned))
        .transpose()
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(io::Error::from(io::ErrorKind::Interrupted).into());
    }
    Ok(())
}

fn with_cancellation<T>(
    db: &mut Connection,
    cancel: &CancellationToken,
    work: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    check_cancel(cancel)?;
    let token = cancel.clone();
    db.progress_handler(1024, Some(move || token.is_cancelled()))?;
    let result = work(db);
    db.progress_handler(0, None::<fn() -> bool>)?;
    check_cancel(cancel)?;
    result
}

fn temporary_error(error: Error, limit: u64) -> Error {
    match &error {
        Error::DuplicateTemporaryLimit { .. }
        | Error::Contract("query workspace quota: refuse before write") => quota_error(limit),
        Error::Sql(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DiskFull =>
        {
            quota_error(limit)
        }
        _ => error,
    }
}

fn quota_error(limit: u64) -> Error {
    io::Error::new(
        io::ErrorKind::QuotaExceeded,
        format!("Empty-entry analysis temporary storage limit ({limit} bytes)"),
    )
    .into()
}

#[cfg(test)]
mod tests;
