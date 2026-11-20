//! Frozen metadata queries: persistent or materialized results, keyset pages.
use super::*;
use sortsize_core::{
    CancellationToken, InventoryKind,
    analysis::{Evaluation, PreparedQuery, SearchQuery},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
pub mod duplicates;
pub mod empty_entries;
pub mod ordering;
pub(super) mod paths;
mod reference_view;
mod sequence;
pub(crate) mod workspace;
pub use workspace::TemporaryUsage;
pub mod reports;

#[derive(Debug, Default, Clone, Copy)]
pub struct QueryCounts {
    pub evaluated: u64,
    pub matches: u64,
    pub unknown_sizes: u64,
    pub unknown_dates: u64,
    pub native_text: u64,
    pub oversized_paths: u64,
    pub unknown_kinds: u64,
    pub cancelled: bool,
}
#[derive(Debug, Clone)]
pub struct Row {
    pub key: EntryKey,
    pub path: PathBuf,
    pub kind: Kind,
    pub logical: Option<u64>,
}
#[derive(Debug, Clone)]
pub struct Cursor {
    pub(super) owner: PathBuf,
    pub(super) sort: Vec<u8>,
    pub(super) id: EntryId,
}
pub struct Page {
    pub first: Option<Cursor>,
    pub rows: Vec<Row>,
    pub next: Option<Cursor>,
    pub more: bool,
    pub accounted_bytes: usize,
}
pub struct Preview {
    pub rows: Vec<Row>,
    pub counts: QueryCounts,
    pub evaluation_finished: bool,
    pub complete: bool,
    pub accounted_bytes: usize,
}
/// Holding this query borrows the store, protecting its generation. L12 may
/// replace this conservative whole-store lease with a generation lease.
pub struct Query<'a> {
    _store: &'a Store,
    initial_rows: rusqlite::Statement<'a>,
    following_rows: rusqlite::Statement<'a>,
    db: Option<Connection>,
    _file: Option<workspace::Artifact>,
    _vfs: Option<workspace::vfs::Vfs>,
    result: ResultStorage,
    model_source: bool,
    reference_certificate: bool,
    model_oversized: u64,
    predicate_calls: u64,
    query_cancel: Option<QueryCancellation>,
    workspace: Arc<workspace::Workspace>,
    native: Option<sequence::Sequence<8>>,
    conversion: Option<Conversion>,
    evaluation_done: bool,
    _memory: sortsize_core::resources::Reservation,
    generation: GenerationId,
    pub counts: QueryCounts,
    pub source_coverage: ScopeCoverage,
    root: PathBuf,
    prepared: PreparedQuery,
    kind_filter: bool,
    after: Option<u64>,
    finished: bool,
    paths: paths::Paths,
}
pub(crate) type QueryCancellation = Arc<std::sync::Mutex<Option<CancellationToken>>>;
struct ActiveCancellation(QueryCancellation);
impl ActiveCancellation {
    fn install(
        cell: Option<&QueryCancellation>,
        cancel: &CancellationToken,
    ) -> Result<Option<Self>> {
        let Some(cell) = cell else {
            return Ok(None);
        };
        *cell
            .lock()
            .map_err(|_| Error::Contract("query cancellation state poisoned"))? =
            Some(cancel.clone());
        Ok(Some(Self(cell.clone())))
    }
}
impl Drop for ActiveCancellation {
    fn drop(&mut self) {
        if let Ok(mut cell) = self.0.lock() {
            *cell = None;
        }
    }
}
pub(crate) trait PersistentQuery {
    fn indexed(&self) -> bool {
        true
    }
    fn evaluate(&self, store: &Store) -> Result<QueryCounts>;
    fn page(
        &self,
        store: &Store,
        generation: GenerationId,
        after: Option<&Cursor>,
        reverse: bool,
    ) -> Result<Page>;
    fn visit(
        &self,
        store: &Store,
        generation: GenerationId,
        visitor: &mut dyn FnMut(Row) -> Result<bool>,
    ) -> Result<()>;
    fn identities(&self, store: &Store, after: Option<u64>) -> Result<Vec<(u64, EntryId)>>;
}
enum ResultStorage {
    Temporary,
    Persistent(Box<dyn PersistentQuery>),
}
struct Conversion {
    writer: sequence::Writer<8>,
    after: Option<(Vec<u8>, EntryId)>,
}
/// Borrow POSIX bytes straight from the certified SQLite row. Windows keeps
/// its explicit UTF-16 codec; no unchecked platform string conversion is used.
pub(crate) fn decode_recorded_path(raw: &[u8]) -> Result<std::borrow::Cow<'_, Path>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if let [1, 0, bytes @ ..] = raw {
            return Ok(std::borrow::Cow::Borrowed(Path::new(
                std::ffi::OsStr::from_bytes(bytes),
            )));
        }
        Err(Error::Contract("foreign recorded path namespace"))
    }
    #[cfg(not(unix))]
    {
        let name = NativeName::decode(raw)
            .map_err(|_| Error::Contract("generation A native path codec"))?;
        Ok(std::borrow::Cow::Owned(PathBuf::from(
            crate::scheduler::native_name(&name)?,
        )))
    }
}
pub(super) fn native_path(path: &Path) -> NativeName {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        NativeName::Posix(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        NativeName::Windows(path.as_os_str().encode_wide().collect())
    }
    #[cfg(not(any(unix, windows)))]
    {
        NativeName::Opaque(path.as_os_str().as_encoded_bytes().to_vec())
    }
}
pub(super) fn path_order(path: &Path) -> Vec<u8> {
    // Path::cmp orders components, not a slash-separated string. Zero terminates
    // each component; byte+1 retains native OsStr order, including invalid text.
    let mut key = Vec::new();
    for c in path.components() {
        for b in c.as_os_str().as_encoded_bytes() {
            key.extend((u16::from(*b) + 1).to_be_bytes());
        }
        key.extend([0, 0]);
    }
    key
}
/// Component order with zero escaped and zero-zero as terminator. The common
/// recorded root is omitted; only the query owns it. Never a display/UTF-8 key.
pub(super) fn compact_path_order(path: &Path, root: &Path) -> Result<Vec<u8>> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| Error::Contract("query path outside recorded root"))?;
    let mut key = Vec::new();
    for component in relative.components() {
        for byte in component.as_os_str().as_encoded_bytes() {
            if *byte == 0 {
                key.extend([0, 255]);
            } else {
                key.push(*byte);
            }
        }
        key.extend([0, 0]);
    }
    Ok(key)
}
pub(super) fn time(s: Option<i64>, ns: Option<u32>) -> Option<SystemTime> {
    match (s, ns) {
        (Some(s), Some(n)) if s >= 0 => {
            SystemTime::UNIX_EPOCH.checked_add(Duration::new(s as u64, n))
        }
        (Some(s), Some(n)) => SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(s.unsigned_abs()))?
            .checked_add(Duration::from_nanos(u64::from(n))),
        _ => None,
    }
}
impl Store {
    pub(super) fn recorded_path(
        &self,
        g: GenerationId,
        id: EntryId,
        root: &Path,
    ) -> Result<PathBuf> {
        self.recorded_path_budget(g, id, root).map(|v| v.0)
    }
    pub(super) fn recorded_path_budget(
        &self,
        g: GenerationId,
        mut id: EntryId,
        root: &Path,
    ) -> Result<(PathBuf, usize, usize)> {
        let mut parts = Vec::new();
        let mut bytes = root.as_os_str().as_encoded_bytes().len();
        while id.0 != 0 {
            let (parent, name): (u64, Vec<u8>) = self.db.query_row(
                "SELECT parent,substr(name,1,32769) FROM entries WHERE generation=?1 AND id=?2",
                params![unsigned(g.0), unsigned(id.0)],
                |r| Ok((number(r, 0)?, r.get(1)?)),
            )?;
            bytes += name.len();
            if bytes > 32 * 1024 || parts.len() >= 1024 || parent >= id.0 {
                return Err(Error::Contract("recorded path exceeds projection budget"));
            }
            parts.push(crate::scheduler::native_name(
                &NativeName::decode(&name).map_err(|_| Error::Contract("invalid name"))?,
            )?);
            id = EntryId(parent);
        }
        let depth = parts.len();
        let mut path = root.to_owned();
        for n in parts.into_iter().rev() {
            path.push(n);
        }
        Ok((path, bytes, depth))
    }
    /// `root` is the recorded root, never canonicalized or probed. `scratch`
    /// must be a caller-selected local private store directory. Quota covers
    /// this result DB; aggregate disk governor is L06.
    pub fn query<'a>(
        &'a self,
        g: GenerationId,
        root: &Path,
        search: &SearchQuery,
        scratch: &Path,
        max_db_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<Query<'a>> {
        let mut query = self.start_query(g, root, search, scratch, max_db_bytes)?;
        while !query.advance(cancel)? {}
        Ok(query)
    }
    /// Allocate a frozen query without evaluating the whole generation. Advance
    /// it on the service thread; provisional previews never carry page cursors.
    pub fn start_query<'a>(
        &'a self,
        g: GenerationId,
        root: &Path,
        search: &SearchQuery,
        scratch: &Path,
        max_db_bytes: u64,
    ) -> Result<Query<'a>> {
        self.require_integrity()?;
        let governor = sortsize_core::resources::Governor::global();
        governor.refresh_native();
        self.start_query_admitted(
            g,
            root,
            search,
            scratch,
            max_db_bytes,
            Self::query_memory_in(&governor)?,
        )
    }
    fn query_memory_in(
        governor: &sortsize_core::resources::Governor,
    ) -> Result<sortsize_core::resources::Reservation> {
        use sortsize_core::resources::Category;
        governor
            .reserve_up_to(Category::Queries, 1024 * 1024, 4 * 1024 * 1024)
            .ok_or_else(|| {
                let usage = governor.usage();
                let limit = usage.limits.categories[Category::Queries as usize];
                Error::MemoryAdmission {
                    minimum: 1024 * 1024,
                    available: limit.saturating_sub(usage.categories[Category::Queries as usize]),
                    category_limit: limit,
                }
            })
    }
    fn start_query_admitted<'a>(
        &'a self,
        g: GenerationId,
        root: &Path,
        search: &SearchQuery,
        scratch: &Path,
        max_db_bytes: u64,
        memory: sortsize_core::resources::Reservation,
    ) -> Result<Query<'a>> {
        self.require_integrity()?;
        if !root.is_absolute() {
            return Err(Error::Contract("recorded root must be absolute"));
        }
        let (sealed, coverage): (bool, i64) = self.db.query_row(
            "SELECT sealed,coverage FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !sealed {
            return Err(Error::Contract("frozen query requires a sealed generation"));
        }
        let prepared =
            PreparedQuery::new(search).map_err(|_| Error::Contract("invalid search query"))?;
        if max_db_bytes < 64 * 1024 {
            return Err(Error::Contract("query disk budget too small"));
        }
        let cache_bytes = memory.bytes() / 2;
        let paths = paths::Paths::new(
            memory
                .bytes()
                .saturating_sub(cache_bytes + 512 * 1024)
                .min(1024 * 1024) as usize,
        );
        let workspace = workspace::Workspace::new(self, scratch, max_db_bytes)?;
        let vfs = workspace::vfs::Vfs::new(workspace.clone())?;
        let file = workspace.file()?;
        let db = Connection::open_with_flags_and_vfs(
            file.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
            vfs.name(),
        )?;
        // This private intermediate projection is never recovered after a
        // process/OS restart. Keep WAL transactions and the quota VFS, without
        // forcing disposable pages to stable storage. Configure this before
        // journal_mode can initialize the empty database. Canonical stores
        // retain their durability policy; conversion still synchronizes the
        // validated final sequence before publishing it.
        db.execute_batch("PRAGMA page_size=4096;PRAGMA synchronous=OFF;PRAGMA journal_mode=WAL;PRAGMA mmap_size=0;PRAGMA temp_store=FILE;CREATE TABLE results(sort BLOB NOT NULL,id BLOB NOT NULL,PRIMARY KEY(sort,id)) WITHOUT ROWID;")?;
        db.pragma_update(None, "cache_size", -((cache_bytes / 1024) as i64))?;
        db.pragma_update(
            None,
            "max_page_count",
            (max_db_bytes / 4096).min(u64::from(u32::MAX)) as u32,
        )?;
        Ok(Query {
            _memory: memory,
            _store: self,
            initial_rows: self.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND id>=?2 ORDER BY id LIMIT 256")?,
            following_rows: self.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND id>?2 ORDER BY id LIMIT 256")?,
            db: Some(db),
            _file: Some(file),
            _vfs: Some(vfs),
            result: ResultStorage::Temporary,
            model_source: false,
            reference_certificate: false,
            model_oversized: 0,
            predicate_calls: 0,
            query_cancel: None,
            workspace,
            native: None,
            conversion: None,
            evaluation_done: false,
            generation: g,
            counts: QueryCounts::default(),
            root: root.to_owned(),
            prepared,
            kind_filter: search.kind.is_some(),
            after: None,
            finished: false,
            paths,
            source_coverage: if coverage == 2 {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
        })
    }
}
impl Store {
    pub(crate) fn start_model_query<'a>(
        &'a self,
        g: GenerationId,
        root: &Path,
        search: &SearchQuery,
        scratch: &Path,
        max_bytes: u64,
        oversized: u64,
    ) -> Result<Query<'a>> {
        let mut query = self.start_query(g, root, search, scratch, max_bytes)?;
        // Certified ranks provide native order and ready paths. Keep the same
        // bounded transactions, common predicate, quota VFS and final sync.
        query.initial_rows=self.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,path,rank,1 FROM generation_a_rows WHERE rank>=?2 ORDER BY rank LIMIT 256")?;
        query.following_rows=self.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,path,rank,1 FROM generation_a_rows WHERE rank>?2 ORDER BY rank LIMIT 256")?;
        query.model_source = true;
        query.model_oversized = oversized;
        Ok(query)
    }
    pub(crate) fn start_persistent_query<'a>(
        &'a self,
        g: GenerationId,
        root: &Path,
        search: &SearchQuery,
        scratch: &Path,
        max_bytes: u64,
        result: Box<dyn PersistentQuery>,
    ) -> Result<Query<'a>> {
        self.require_integrity()?;
        if max_bytes < 64 * 1024 {
            return Err(Error::Contract("query disk budget too small"));
        }
        let (sealed, coverage): (bool, i64) = self.db.query_row(
            "SELECT sealed,coverage FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !sealed {
            return Err(Error::Contract("frozen query requires a sealed generation"));
        }
        let governor = sortsize_core::resources::Governor::global();
        governor.refresh_native();
        let memory = Self::query_memory_in(&governor)?;
        let workspace = workspace::Workspace::new(self, scratch, max_bytes)?;
        Ok(Query{
            _store:self,
            initial_rows:self.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND id>=?2 ORDER BY id LIMIT 256")?,
            following_rows:self.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND id>?2 ORDER BY id LIMIT 256")?,
            db:None,_file:None,_vfs:None,result:ResultStorage::Persistent(result),model_source:false,reference_certificate:false,model_oversized:0,predicate_calls:0,query_cancel:None,
            workspace,native:None,conversion:None,evaluation_done:false,_memory:memory,generation:g,counts:QueryCounts::default(),
            source_coverage:if coverage==2{ScopeCoverage::Complete}else{ScopeCoverage::Partial},root:root.into(),
            prepared:PreparedQuery::new(search).map_err(|_|Error::Contract("invalid search query"))?,kind_filter:search.kind.is_some(),after:None,finished:false,paths:paths::Paths::new(0),
        })
    }
}
impl Query<'_> {
    fn can_reference_result(&self) -> bool {
        self.model_source
            && self.reference_certificate
            && self.counts.matches <= reference_view::MAX_MEMBERS
            && self._memory.bytes() >= reference_view::MIN_MEMORY
    }
    pub(crate) fn attach_reference_certificate(
        &mut self,
        generation: &super::generation_a::CertifiedGeneration,
    ) -> Result<()> {
        if !std::ptr::eq(self._store, generation.as_store())
            || self.generation != generation.generation()
            || self.root != generation.root()
        {
            return Err(Error::Contract("foreign query reference certificate"));
        }
        reference_view::require_snapshot(self._store)?;
        self.reference_certificate = true;
        Ok(())
    }
    pub(crate) fn attach_cancellation(&mut self, cell: QueryCancellation) {
        self.query_cancel = Some(cell);
    }
    pub fn indexed(&self) -> bool {
        match &self.result {
            ResultStorage::Persistent(result) => result.indexed(),
            ResultStorage::Temporary => false,
        }
    }
    /// Members refer to the same retained generation; this is separate from
    /// indexed coverage of the predicate, which an exhaustive scan does not have.
    pub fn references_generation(&self) -> bool {
        matches!(self.result, ResultStorage::Persistent(_))
    }
    /// Storage policy for a completed result; no new sequence is created for
    /// a certified reference view. Any sequence actually created keeps its sync.
    pub fn new_result_sync_required(&self) -> bool {
        !self.references_generation()
    }
    /// Actual predicate calls, including work from a quantum later rolled back.
    /// QueryCounts remain the separately committed classification/coverage.
    pub fn predicate_evaluated(&self) -> u64 {
        self.predicate_calls
    }
    pub fn proof_covered(&self) -> u64 {
        self.counts.evaluated
    }
    pub fn evaluation_finished(&self) -> bool {
        self.finished
    }
    /// One short transaction: <=256 examined rows and ~20 ms between rows.
    /// A blocked SQLite call is not interruptible by this time budget. No UI use.
    /// Counts and the cursor advance only after commit, including cancellation.
    pub fn advance(&mut self, cancel: &CancellationToken) -> Result<bool> {
        let _scope = ActiveCancellation::install(self.query_cancel.as_ref(), cancel)?;
        let result = self.advance_inner(cancel);
        if result.is_err() && cancel.is_cancelled() {
            // SQLite interrupted a quantum: its transaction rolled back; keep
            // the previously committed partial view and publish no new bytes.
            self.conversion = None;
            self.counts.cancelled = true;
            self.finished = true;
            Ok(true)
        } else {
            result
        }
    }
    fn advance_inner(&mut self, cancel: &CancellationToken) -> Result<bool> {
        if self.finished {
            return Ok(true);
        }
        if let ResultStorage::Persistent(result) = &self.result {
            if cancel.is_cancelled() {
                self.counts.cancelled = true;
                self.finished = true;
                return Ok(true);
            }
            let evaluated = result.evaluate(self._store);
            if cancel.is_cancelled() {
                self.counts.cancelled = true;
                self.finished = true;
                return Ok(true);
            }
            self.counts = evaluated?;
            self.evaluation_done = true;
            self.finished = true;
            assert!(self._memory.shrink_to(1024 * 1024));
            return Ok(true);
        }
        if self.evaluation_done {
            let result = self.advance_conversion(cancel);
            if result.is_err() {
                self.conversion = None;
            }
            return result;
        }
        let start = Instant::now();
        // Reuse the two bounded scan statements across quanta. Their lifetime is
        // the existing Store lease; no additional connection or statement cache.
        let statement = if self.after.is_some() {
            &mut self.following_rows
        } else {
            &mut self.initial_rows
        };
        let mut rows = statement.query(params![
            unsigned(self.generation.0),
            if self.model_source {
                rusqlite::types::Value::Integer(
                    i64::try_from(self.after.unwrap_or(0))
                        .map_err(|_| Error::Contract("generation A rank overflow"))?,
                )
            } else {
                rusqlite::types::Value::Blob(unsigned(self.after.unwrap_or(0)).to_vec())
            }
        ])?;
        let mut counts = self.counts;
        if self.model_source && self.after.is_none() {
            counts.evaluated = self.model_oversized;
            counts.oversized_paths = self.model_oversized;
        }
        let mut after = self.after;
        let mut examined = 0;
        let mut finished = false;
        let tx = self.db.as_mut().unwrap().transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO results VALUES(?1,?2)")?;
            loop {
                if cancel.is_cancelled() {
                    counts.cancelled = true;
                    finished = true;
                    break;
                }
                if examined >= 256 || (examined > 0 && start.elapsed() >= Duration::from_millis(20))
                {
                    break;
                }
                let Some(row) = rows.next()? else {
                    finished = true;
                    break;
                };
                let id = EntryId(number(row, 0)?);
                let kind: i64 = row.get(1)?;
                after = Some(if self.model_source {
                    u64::try_from(row.get::<_, i64>(6)?)
                        .map_err(|_| Error::Contract("generation A negative rank"))?
                } else {
                    id.0
                });
                examined += 1;
                let model_path;
                let path = if self.model_source {
                    let raw = row.get_ref(5)?.as_blob().map_err(rusqlite::Error::from)?;
                    model_path = decode_recorded_path(raw)?;
                    model_path.as_ref()
                } else {
                    match self
                        .paths
                        .row(self._store, self.generation, id, &self.root, row)
                    {
                        Ok(Some(p)) => p,
                        Ok(None) => continue,
                        Err(Error::Contract(_)) => {
                            counts.evaluated += 1;
                            counts.oversized_paths += 1;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                };
                counts.evaluated += 1;
                if kind == 0 && self.kind_filter {
                    counts.unknown_kinds += 1;
                    continue;
                }
                let logical = opt_number(row, 2)?;
                self.predicate_calls = self
                    .predicate_calls
                    .checked_add(1)
                    .ok_or(Error::Contract("query predicate work overflow"))?;
                let evaluation = self.prepared.evaluate_path(
                    path,
                    match kind {
                        1 => InventoryKind::File,
                        2 => InventoryKind::Directory,
                        3 => InventoryKind::Symlink,
                        _ => InventoryKind::Special,
                    },
                    logical,
                    time(row.get(3)?, row.get(4)?),
                    Some(&self.root),
                );
                match evaluation {
                    Evaluation::Match => {
                        insert.execute(params![
                            if self.model_source {
                                unsigned(
                                    u64::try_from(row.get::<_, i64>(6)?).map_err(|_| {
                                        Error::Contract("generation A negative rank")
                                    })?,
                                )
                                .to_vec()
                            } else {
                                compact_path_order(path, &self.root)?
                            },
                            unsigned(id.0)
                        ])?;
                        counts.matches += 1;
                    }
                    Evaluation::UnknownSize => counts.unknown_sizes += 1,
                    Evaluation::UnknownDate => counts.unknown_dates += 1,
                    Evaluation::NativeText => counts.native_text += 1,
                    Evaluation::NoMatch => {}
                }
            }
        }
        tx.commit()?;
        self.counts = counts;
        self.after = after;
        self.evaluation_done = finished;
        if finished {
            // Evaluation caches are not retained while reports/exports consume the result.
            self.paths = paths::Paths::new(0);
            self.db
                .as_ref()
                .unwrap()
                .pragma_update(None, "cache_size", -128_i64)?;
            self.db
                .as_ref()
                .unwrap()
                .execute_batch("PRAGMA shrink_memory;")?;
            let retained = if self.model_source
                && self.reference_certificate
                && self.counts.matches <= reference_view::MAX_MEMBERS
                && self._memory.bytes() >= reference_view::MIN_MEMORY
            {
                reference_view::MIN_MEMORY
            } else {
                1024 * 1024
            };
            assert!(self._memory.shrink_to(retained));
        }
        if self.counts.cancelled {
            self.finished = true;
        }
        Ok(self.finished)
    }
    pub fn preparation_phase(&self) -> &'static str {
        if self.finished {
            "ready"
        } else if self.evaluation_done {
            "conversion"
        } else {
            "evaluation"
        }
    }
    pub fn temporary_usage(&self) -> TemporaryUsage {
        self.workspace.usage()
    }
    /// Current real lease, including a retained bounded reference selection.
    pub fn working_memory_bytes(&self) -> u64 {
        self._memory.bytes()
    }
    fn advance_conversion(&mut self, cancel: &CancellationToken) -> Result<bool> {
        if cancel.is_cancelled() {
            // The old SQLite view remains intact and readable as a partial
            // query. No unfinished sequence can replace it.
            self.conversion = None;
            self.counts.cancelled = true;
            self.finished = true;
            return Ok(true);
        }
        if self.can_reference_result() {
            let result = reference_view::ReferenceView::build(self, cancel)?;
            // The SQLite projection is reconstructible, never a published
            // result file. Keep it intact until every reference is validated.
            drop(self.db.take());
            drop(self._file.take());
            drop(self._vfs.take());
            self.result = ResultStorage::Persistent(Box::new(result));
            self.finished = true;
            return Ok(true);
        }
        if self.conversion.is_none() {
            let planned = sequence::Sequence::<8>::planned_bytes(self.counts.matches)?;
            let usage = self.workspace.usage();
            if usage.charged.checked_add(planned).is_none_or(|n| {
                n > usage
                    .limit
                    .saturating_sub((usage.limit / 64).min(4 * 1024 * 1024))
            }) {
                return Err(Error::Contract(
                    "query conversion quota: original view preserved",
                ));
            }
            self.conversion = Some(Conversion {
                writer: sequence::Writer::new(&self.workspace)?,
                after: None,
            });
        }
        let conversion = self.conversion.as_mut().unwrap();
        let sql = if conversion.after.is_some() {
            "SELECT sort,id FROM results WHERE (sort,id)>(?1,?2) ORDER BY sort,id LIMIT 256"
        } else {
            "SELECT sort,id FROM results WHERE (sort,id)>=(?1,?2) ORDER BY sort,id LIMIT 256"
        };
        let mut statement = self.db.as_ref().unwrap().prepare(sql)?;
        let mut rows = statement.query(params![
            conversion
                .after
                .as_ref()
                .map_or(&[][..], |a| a.0.as_slice()),
            unsigned(conversion.after.as_ref().map_or(0, |a| a.1.0))
        ])?;
        let started = Instant::now();
        let mut examined = 0;
        let mut done = false;
        while examined < 256 && (examined == 0 || started.elapsed() < Duration::from_millis(20)) {
            if cancel.is_cancelled() {
                break;
            }
            let Some(row) = rows.next()? else {
                done = true;
                break;
            };
            let id = EntryId(number(row, 1)?);
            conversion.writer.push(&unsigned(id.0))?;
            conversion.after = Some((row.get(0)?, id));
            examined += 1;
        }
        drop(rows);
        drop(statement);
        if done {
            let conversion = self.conversion.take().unwrap();
            if conversion.writer.count() != self.counts.matches {
                return Err(Error::Contract("conversion count mismatch"));
            }
            let native = conversion.writer.seal(true)?;
            // Publish only after header, exact length, checksums and sync. Drop
            // the old DB/OS handles before removing/uncharging its file.
            self.native = Some(native);
            drop(self.db.take());
            drop(self._file.take());
            self.finished = true;
        }
        Ok(self.finished)
    }
    /// Matches are exact individually; membership/order/counts remain provisional
    /// until evaluation finishes. No cursor can escape this preview.
    pub fn preview(&self) -> Result<Preview> {
        let page = self.page_direction(None, false)?;
        Ok(Preview {
            rows: page.rows,
            counts: self.counts,
            evaluation_finished: self.finished,
            complete: self.complete(),
            accounted_bytes: page.accounted_bytes,
        })
    }

    pub fn complete(&self) -> bool {
        self.finished
            && self.source_coverage == ScopeCoverage::Complete
            && !self.counts.cancelled
            && self.counts.unknown_dates == 0
            && self.counts.unknown_sizes == 0
            && self.counts.native_text == 0
            && self.counts.oversized_paths == 0
            && self.counts.unknown_kinds == 0
    }
    pub fn page(&self, after: Option<&Cursor>) -> Result<Page> {
        if !self.finished {
            return Err(Error::Contract(
                "exact pages require finished evaluation; use preview",
            ));
        }
        self.page_direction(after, false)
    }
    pub fn page_before(&self, before: &Cursor) -> Result<Page> {
        if !self.finished {
            return Err(Error::Contract(
                "exact pages require finished evaluation; use preview",
            ));
        }
        self.page_direction(Some(before), true)
    }
    fn source_statement(&self) -> Result<rusqlite::Statement<'_>> {
        Ok(self._store.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND id=?2")?)
    }
    fn hydrate(
        &self,
        id: EntryId,
        source: &mut rusqlite::Statement<'_>,
        paths: &mut paths::Paths,
    ) -> Result<Row> {
        let mut rows = source.query(params![unsigned(self.generation.0), unsigned(id.0)])?;
        let row = rows
            .next()?
            .ok_or(Error::Contract("missing projected entry"))?;
        let path = paths
            .row(self._store, self.generation, id, &self.root, row)?
            .ok_or(Error::Contract("invisible projected entry"))?
            .to_owned();
        let kind = match row.get::<_, i64>(1)? {
            1 => Kind::File,
            2 => Kind::Directory,
            3 => Kind::Symlink,
            4 => Kind::Special,
            _ => Kind::Unknown,
        };
        Ok(Row {
            key: EntryKey {
                inventory: self._store.inventory,
                generation: self.generation,
                entry: id,
            },
            path,
            kind,
            logical: opt_number(row, 2)?,
        })
    }
    /// One row at a time under the original Store/generation lease. The callback
    /// returns false to stop. No second source connection or unbounded page list.
    fn visit_matches(&self, mut visit: impl FnMut(Row) -> Result<bool>) -> Result<()> {
        if let ResultStorage::Persistent(result) = &self.result {
            if !self.finished || self.counts.cancelled {
                return Ok(());
            }
            return result.visit(self._store, self.generation, &mut visit);
        }
        if let Some(native) = &self.native {
            let mut reader = native.reader()?;
            let mut source = self.source_statement()?;
            let mut paths = paths::Paths::new(128 * 1024);
            while let Some(id) = reader.next()? {
                if !visit(self.hydrate(
                    EntryId(u64::from_be_bytes(id)),
                    &mut source,
                    &mut paths,
                )?)? {
                    break;
                }
            }
            return Ok(());
        }
        let mut statement = self
            .db
            .as_ref()
            .unwrap()
            .prepare("SELECT id FROM results ORDER BY sort,id")?;
        let mut rows = statement.query([])?;
        let mut source = self.source_statement()?;
        let mut paths = paths::Paths::new(128 * 1024);
        while let Some(row) = rows.next()? {
            if !visit(self.hydrate(EntryId(number(row, 0)?), &mut source, &mut paths)?)? {
                break;
            }
        }
        Ok(())
    }
    fn page_direction(&self, after: Option<&Cursor>, reverse: bool) -> Result<Page> {
        if let ResultStorage::Persistent(result) = &self.result {
            if !self.finished || self.counts.cancelled {
                return Ok(Page {
                    first: None,
                    rows: Vec::new(),
                    next: None,
                    more: false,
                    accounted_bytes: 0,
                });
            }
            return result.page(self._store, self.generation, after, reverse);
        }
        if let Some(native) = &self.native {
            return self.sequence_page(native, after, reverse);
        }
        self.compact_page(
            self.db.as_ref().unwrap(),
            self._file.as_ref().unwrap().path(),
            after,
            reverse,
        )
    }
    fn sequence_page(
        &self,
        sequence: &sequence::Sequence<8>,
        cursor: Option<&Cursor>,
        reverse: bool,
    ) -> Result<Page> {
        sequence.check()?;
        let anchor = if let Some(c) = cursor {
            if c.owner != sequence.path()
                || c.sort.len() != 16
                || c.sort[..8] != sequence.nonce().to_be_bytes()
            {
                return Err(Error::Contract("cursor belongs to another query order"));
            }
            let position = u64::from_be_bytes(c.sort[8..].try_into().unwrap());
            if sequence.at(position)? != unsigned(c.id.0) {
                return Err(Error::Contract("invalid sequence cursor"));
            }
            Some(position)
        } else {
            None
        };
        let mut position = if reverse {
            anchor.and_then(|index| index.checked_sub(1))
        } else {
            Some(anchor.map_or(0, |a| a + 1))
        };
        let mut source = self.source_statement()?;
        let mut paths = paths::Paths::new(128 * 1024);
        let mut result = Vec::new();
        let mut first = None;
        let mut next = None;
        let mut bytes = 0;
        let mut more = false;
        let byte_limit = 512 * 1024 - 200 * std::mem::size_of::<Row>() - 1024;
        while let Some(index) = position.filter(|i| *i < sequence.count) {
            if result.len() == 200 {
                more = true;
                break;
            }
            let id = EntryId(u64::from_be_bytes(sequence.at(index)?));
            let row = self.hydrate(id, &mut source, &mut paths)?;
            let size = row.path.capacity() + 128;
            if bytes + size > byte_limit {
                more = true;
                break;
            }
            bytes += size;
            result.push(row);
            let mut sort = sequence.nonce().to_be_bytes().to_vec();
            sort.extend(index.to_be_bytes());
            next = Some(Cursor {
                owner: sequence.path().into(),
                sort,
                id,
            });
            if first.is_none() {
                first = next.clone();
            }
            position = if reverse {
                index.checked_sub(1)
            } else {
                index.checked_add(1)
            };
        }
        if reverse {
            result.reverse();
        }
        Ok(Page {
            first: if reverse { next.clone() } else { first.clone() },
            rows: result,
            next: if reverse { first } else { next },
            more,
            accounted_bytes: bytes,
        })
    }
    fn compact_page(
        &self,
        db: &Connection,
        owner: &Path,
        after: Option<&Cursor>,
        reverse: bool,
    ) -> Result<Page> {
        if after.is_some_and(|c| c.owner != owner) {
            return Err(Error::Contract("cursor belongs to another query"));
        }
        let sql = if reverse {
            "SELECT sort,id FROM results WHERE (sort,id)<(?1,?2) ORDER BY sort DESC,id DESC LIMIT 201"
        } else if after.is_some() {
            "SELECT sort,id FROM results WHERE (sort,id)>(?1,?2) ORDER BY sort,id LIMIT 201"
        } else {
            // The root has the empty relative key; it must not be skipped.
            "SELECT sort,id FROM results WHERE (sort,id)>=(?1,?2) ORDER BY sort,id LIMIT 201"
        };
        let mut statement = db.prepare(sql)?;
        let mut rows = statement.query(params![
            after.map_or(&[][..], |c| c.sort.as_slice()),
            unsigned(after.map_or(0, |c| c.id.0))
        ])?;
        let mut source = self.source_statement()?;
        let mut paths = paths::Paths::new(128 * 1024);
        let mut result = Vec::new();
        let mut first = None;
        let mut next = None;
        let mut bytes = 0;
        let mut more = false;
        let byte_limit = 512 * 1024 - 200 * std::mem::size_of::<Row>() - 1024;
        while let Some(r) = rows.next()? {
            if result.len() == 200 {
                more = true;
                break;
            }
            let id = EntryId(number(r, 1)?);
            let row = self.hydrate(id, &mut source, &mut paths)?;
            let size = row.path.capacity() + 128;
            if bytes + size > byte_limit {
                more = true;
                break;
            }
            bytes += size;
            result.push(row);
            next = Some(Cursor {
                owner: owner.into(),
                sort: r.get(0)?,
                id,
            });
            if first.is_none() {
                first = next.clone();
            }
        }
        if reverse {
            result.reverse();
        }
        Ok(Page {
            first: if reverse { next.clone() } else { first.clone() },
            rows: result,
            next: if reverse { first } else { next },
            more,
            accounted_bytes: bytes,
        })
    }
}
pub(super) fn read_page(
    db: &Connection,
    inventory: InventoryId,
    generation: GenerationId,
    owner: &Path,
    after: Option<&Cursor>,
    reverse: bool,
) -> Result<Page> {
    if after.is_some_and(|c| c.owner != owner) {
        return Err(Error::Contract("foreign ordered cursor"));
    }
    let sql = if reverse {
        "SELECT sort,id,path,kind,logical FROM results WHERE (sort,id)<(?1,?2) ORDER BY sort DESC,id DESC LIMIT 201"
    } else {
        "SELECT sort,id,path,kind,logical FROM results WHERE (sort,id)>(?1,?2) ORDER BY sort,id LIMIT 201"
    };
    let mut q = db.prepare(sql)?;
    let mut rows = q.query(params![
        after.map_or(&[][..], |c| c.sort.as_slice()),
        unsigned(after.map_or(0, |c| c.id.0))
    ])?;
    let mut result = Vec::new();
    let mut next = None;
    let mut first = None;
    let mut bytes = 0;
    // Reserve frame headers and the bounded row table within the same 512 KiB
    // publication ceiling; byte-limited pages retain exact continuation cursors.
    let byte_limit = 512 * 1024 - 200 * std::mem::size_of::<Row>() - 1024;
    let mut more = false;
    while let Some(r) = rows.next()? {
        let raw: Vec<u8> = r.get(2)?;
        let path: PathBuf = crate::scheduler::native_name(
            &NativeName::decode(&raw).map_err(|_| Error::Contract("invalid query path"))?,
        )?
        .into();
        // WTF-8 capacity may exceed UTF-16 serialized length on Windows.
        let size = path.capacity().max(raw.len()) + 128;
        if result.len() == 200 || bytes + size > byte_limit {
            more = true;
            break;
        }
        let id = EntryId(number(r, 1)?);
        let kind = match r.get::<_, i64>(3)? {
            1 => Kind::File,
            2 => Kind::Directory,
            3 => Kind::Symlink,
            4 => Kind::Special,
            _ => Kind::Unknown,
        };
        bytes += size;
        result.push(Row {
            key: EntryKey {
                inventory,
                generation,
                entry: id,
            },
            path,
            kind,
            logical: opt_number(r, 4)?,
        });
        next = Some(Cursor {
            owner: owner.into(),
            sort: r.get(0)?,
            id,
        });
        if first.is_none() {
            first = next.clone();
        }
    }
    if reverse {
        result.reverse();
    }
    Ok(Page {
        first: if reverse { next.clone() } else { first.clone() },
        rows: result,
        next: if reverse { first } else { next },
        more,
        accounted_bytes: bytes,
    })
}
#[cfg(test)]
mod tests;

/// Bounded live projection. `provisional` is always true for an unsealed source;
/// missing metadata has no invented area. Native names are never reconstructed
/// from a truncated display label.
pub struct Projection {
    pub rows: Vec<Row>,
    pub provisional: bool,
    pub revision: Revision,
    pub accounted_bytes: usize,
    pub byte_limited: bool,
}
impl Store {
    pub fn top100(&self, g: GenerationId, root: &Path) -> Result<Projection> {
        self.project(g, root, None, false, 100)
    }
    pub(super) fn top100_within(
        &self,
        g: GenerationId,
        folder: EntryId,
        root: &Path,
    ) -> Result<Projection> {
        self.project(g, root, Some(folder), true, 100)
    }
    pub fn treemap(&self, g: GenerationId, parent: EntryId, root: &Path) -> Result<Projection> {
        self.project(g, root, Some(parent), false, 256)
    }
    fn project(
        &self,
        g: GenerationId,
        root: &Path,
        parent: Option<EntryId>,
        recursive: bool,
        limit: usize,
    ) -> Result<Projection> {
        self.require_integrity()?;
        let sealed: bool = self.db.query_row(
            "SELECT sealed FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| r.get(0),
        )?;
        let sql = if recursive {
            // Follow inventory edges only through present directories, then rank
            // the files inside that scope before applying the row limit.
            "WITH RECURSIVE folders(id) AS (
                 VALUES(?2)
                 UNION ALL
                 SELECT e.id FROM folders f JOIN entries e
                   ON e.generation=?1 AND e.parent=f.id AND e.id>f.id
                 WHERE e.present=1 AND e.kind=2
             ) SELECT e.id,e.logical FROM folders f JOIN entries e
                 ON e.generation=?1 AND e.parent=f.id AND e.id>f.id
               WHERE e.present=1 AND e.kind=1 AND e.logical IS NOT NULL
               ORDER BY e.logical DESC,e.id LIMIT 100"
        } else if parent.is_some() {
            "SELECT id,logical FROM entries WHERE generation=?1 AND parent=?2 AND kind=1 AND logical IS NOT NULL AND present=1 ORDER BY logical DESC,id"
        } else {
            "SELECT id,logical FROM entries WHERE generation=?1 AND kind=1 AND logical IS NOT NULL AND present=1 ORDER BY logical DESC,id"
        };
        let mut q = self.db.prepare(sql)?;
        let trace_started =
            (parent.is_none() && crate::scan_trace::enabled()).then(std::time::Instant::now);
        let projected = (|| -> Result<Projection> {
            let mut rows = if let Some(parent) = parent {
                q.query(params![unsigned(g.0), unsigned(parent.0)])?
            } else {
                q.query([unsigned(g.0)])?
            };
            let mut result = Vec::new();
            let mut bytes = 0;
            let mut byte_limited = false;
            while let Some(row) = rows.next()? {
                let id = EntryId(number(row, 0)?);
                if !recursive && !visible(&self.db, g, id)? {
                    continue;
                }
                let path = self.recorded_path(g, id, root)?;
                let size = path.as_os_str().as_encoded_bytes().len() + 128;
                if result.len() == limit {
                    break;
                }
                if bytes + size > 512 * 1024 {
                    byte_limited = true;
                    break;
                }
                bytes += size;
                result.push(Row {
                    key: EntryKey {
                        inventory: self.inventory,
                        generation: g,
                        entry: id,
                    },
                    path,
                    kind: Kind::File,
                    logical: Some(number(row, 1)?),
                });
            }
            Ok(Projection {
                rows: result,
                provisional: !sealed,
                revision: self.revision(g)?,
                accounted_bytes: bytes,
                byte_limited,
            })
        })();
        // Includes visibility/path work while walking the unchanged statement;
        // VM counters refer only to this SELECT, not those nested lookups.
        crate::scan_trace::statement("top100", &q, trace_started);
        projected
    }
}
