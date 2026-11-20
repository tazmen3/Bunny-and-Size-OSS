//! L10: disk-owned candidates and groups. Preparation is metadata-only;
//! content is read exclusively by the explicit `verify` operation.
use super::*;
mod current;
pub mod folders;
pub mod selection;
pub use current::{
    AcquisitionProgress, CURRENT_FOLDER_PUBLICATION_PAGE_BYTES, CurrentFolderResourceUsage,
    CurrentFolderResources, DuplicateResourceCause, prepare_current_folder,
    prepare_current_folder_with_profile, prepare_current_folder_with_resources,
};
pub use selection::{ReviewReason, ReviewState, ReviewSummary, ReviewTarget};
use sortsize_core::{
    acquisition::{Attachment, Guard, contains_path},
    duplicates::{
        ContentLimit, DuplicateOptions, DuplicateProgress,
        stream::{Proof, Verifier},
    },
    resources::{Category, Governor, Reservation},
};
use std::{io, sync::Mutex};
static CONTENT_JOB: Mutex<()> = Mutex::new(());
type CandidateRecord = (
    u64,
    Vec<u8>,
    u64,
    usize,
    Option<i64>,
    Option<u32>,
    Option<Vec<u8>>,
);
type VerificationRecord = (u64, Vec<u8>, Vec<u8>, u64, usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationPhase {
    Hashing,
    Comparing,
}

/// Minimum logical size for an interactive file search. The historical
/// comparison entry points remain available for whole-folder equality.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateFileProfile {
    General,
    Optimized,
    #[default]
    LargeFiles,
    Exhaustive,
}

impl DuplicateFileProfile {
    pub const fn minimum_bytes(self) -> u64 {
        match self {
            Self::General => 256 * 1024,
            Self::Optimized => 1024 * 1024,
            Self::LargeFiles => 10 * 1024 * 1024,
            Self::Exhaustive => 1,
        }
    }

    fn excludes(self, size: u64, excluded_by_size: &mut u64, excluded_empty: &mut u64) -> bool {
        if size == 0 {
            *excluded_empty += 1;
        } else if size < self.minimum_bytes() {
            *excluded_by_size += 1;
        } else {
            return false;
        }
        true
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Summary {
    pub selected_files: u64,
    /// Number of regular-file observations, including unknown and excluded sizes.
    #[serde(skip)]
    pub examined_files: u64,
    /// Non-empty files below the selected minimum logical size.
    #[serde(skip)]
    pub excluded_by_size: u64,
    /// Zero-byte files omitted from the interactive duplicate search.
    #[serde(skip)]
    pub excluded_empty: u64,
    /// Sum of known logical sizes of admitted files in a live or profiled search.
    #[serde(skip)]
    pub selected_logical_bytes: u128,
    pub unknown_sizes: u64,
    pub groups: u64,
    /// Highest fully verified A/B folder pairs. Their descendants are not added again.
    pub folder_groups: u64,
    /// Files represented by folder pairs, counted once per pair.
    pub folder_files: u64,
    /// Logical content bytes represented by folder pairs, never physical savings.
    pub folder_logical_bytes: u64,
    /// Directory locations whose recursive coverage or freshness is not proven.
    /// This does not invalidate individually verified file-content groups.
    pub folder_unverified: u64,
    /// Inventoried file locations omitted from the explicit file query.
    /// A filtered file result cannot certify the enclosing whole folder.
    pub folder_filtered: u64,
    /// Bounded failure reason for optional folder derivation; exact file
    /// results remain available when folder presentation cannot be completed.
    pub folder_diagnostic: Option<String>,
    pub independent_members: u64,
    /// Observed native aliases, including paths suppressed from cross-source groups.
    pub aliases: u64,
    /// Native identity matches across inventories are conservatively aliases.
    pub ambiguous_aliases: u64,
    /// Scopes whose inventory/source continuity is unproven. Current file
    /// equality is still verifiable, but coverage and copy independence are not.
    pub unverified_sources: u64,
    /// Scopes whose saved policy permits directory-index metadata. Size
    /// preselection can omit changed files before the content verifier sees
    /// them; verified positive groups remain valid, but coverage is unproven.
    pub directory_listing_sources: u64,
    /// Future assisted operations must also require complete coverage.
    pub source_independence_verified: bool,
    pub files_hashed: u64,
    pub bytes_read: u128,
    pub errors: u64,
    pub cancelled: bool,
    pub budget_limited: bool,
    #[serde(skip)]
    pub total_read_limit: Option<u64>,
    #[serde(skip)]
    pub file_size_limit: Option<u64>,
    #[serde(skip)]
    pub temporary_limit: Option<u64>,
    #[serde(skip)]
    pub resources: Option<CurrentFolderResourceUsage>,
    #[serde(skip)]
    pub error_limit: Option<usize>,
    /// Known comparable file occurrences still lacking a content classification.
    /// An incomplete acquisition or unproven attachment leaves the count unknown.
    #[serde(skip)]
    pub unverified_files: Option<u64>,
    #[serde(skip)]
    pub verification_phase: Option<VerificationPhase>,
    pub verified: bool,
    pub complete: bool,
    pub potentially_reclaimable_bytes: Option<u64>,
}
impl Summary {
    fn update_content(&mut self, progress: DuplicateProgress) {
        self.bytes_read = progress.bytes_read;
        self.files_hashed = progress.files_hashed;
    }

    fn update_content_in_flight(
        &mut self,
        update: DuplicateProgress,
        last: &mut Option<Instant>,
        progress: &mut impl FnMut(&Summary),
    ) {
        let now = Instant::now();
        if last.is_none_or(|last| now.duration_since(last) >= Duration::from_millis(200)) {
            *last = Some(now);
            self.update_content(update);
            progress(self);
        }
    }
}
#[derive(Debug, serde::Serialize)]
pub struct Group {
    pub id: u64,
    pub logical_bytes: u64,
    pub independent_members: u64,
    pub paths: u64,
}
#[derive(Debug, serde::Serialize)]
pub struct Member {
    /// Actual chosen folder side, independent of source and inventory identity.
    pub side: u8,
    #[serde(skip)]
    pub source: SourceId,
    #[serde(skip)]
    pub key: EntryKey,
    pub entry: u64,
    /// Temporary job key used by members(group, after), separate from EntryKey.
    pub cursor: u64,
    #[serde(skip)]
    composite: bool,
    pub path: sortsize_core::NativePath,
    pub representative: u64,
}
impl Member {
    /// Encode native units directly; no intermediate JSON value per path byte.
    pub fn json_record(&self, group: u64) -> Result<Vec<u8>> {
        #[derive(serde::Serialize)]
        struct Data<'a> {
            entry: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            side: Option<u8>,
            #[serde(skip_serializing_if = "Option::is_none")]
            source: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            inventory: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            generation: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cursor: Option<String>,
            path: &'a sortsize_core::NativePath,
            representative: String,
        }
        #[derive(serde::Serialize)]
        struct Record<'a> {
            duplicates_schema: u8,
            record: &'static str,
            group: String,
            member: Data<'a>,
        }
        serde_json::to_vec(&Record {
            duplicates_schema: if self.composite { 2 } else { 1 },
            record: "member",
            group: group.to_string(),
            member: Data {
                entry: self.entry.to_string(),
                side: self.composite.then_some(self.side),
                source: self.composite.then(|| self.source.0.to_string()),
                inventory: self.composite.then(|| self.key.inventory.0.to_string()),
                generation: self.composite.then(|| self.key.generation.0.to_string()),
                cursor: self.composite.then(|| self.cursor.to_string()),
                path: &self.path,
                representative: self.representative.to_string(),
            },
        })
        .map_err(|e| Error::Io(io::Error::other(e)))
    }
}
/// Metadata-only selection. Roots remain native and each receives its own guard.
pub struct Scope<'q, 's> {
    pub query: &'q Query<'s>,
    pub roots: Vec<PathBuf>,
    /// Explicit selected side: 0 for selection/A, 1 for B, 2 for C.
    pub side: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    WithinSelection,
    BetweenSources,
    BetweenThreeSources,
}
impl Mode {
    pub fn is_cross_source(self) -> bool {
        self != Self::WithinSelection
    }
    pub fn side_count(self) -> u8 {
        match self {
            Self::WithinSelection => 1,
            Self::BetweenSources => 2,
            Self::BetweenThreeSources => 3,
        }
    }
}
pub struct DuplicateJob<'q, 's> {
    scopes: Vec<Scope<'q, 's>>,
    current: Option<current::Acquisition>,
    guard_roots: Vec<PathBuf>,
    mode: Mode,
    rank_by_gain: bool,
    max_errors: usize,
    root_issues: Vec<(usize, String)>,
    root_warnings: Vec<(usize, String)>,
    db: Connection,
    _vfs: workspace::vfs::Vfs,
    space: Arc<workspace::Workspace>,
    owner: PathBuf,
    _memory: Reservation,
    _sqlite_memory: Option<Reservation>,
    publication_memory: Option<Arc<Reservation>>,
    summary: Summary,
}
fn path(bytes: Vec<u8>) -> Result<PathBuf> {
    let n = NativeName::decode(&bytes).map_err(|_| Error::Contract("duplicate path codec"))?;
    Ok(crate::scheduler::native_name(&n)?.into())
}
impl<'s> Query<'s> {
    /// No stat, open, hashing or boundary discovery. All regular-file matches,
    /// including candidates beyond the visible/cache prefix, are retained.
    pub fn prepare_duplicates<'q>(
        &'q self,
        scratch: &Path,
        max_db_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<DuplicateJob<'q, 's>> {
        self.prepare_duplicate_selection(scratch, max_db_bytes, None, cancel)
    }

    /// Keep the original query intact and omit files below the profile's size
    /// from the disposable duplicate store.
    pub fn prepare_duplicates_with_profile<'q>(
        &'q self,
        scratch: &Path,
        max_db_bytes: u64,
        profile: DuplicateFileProfile,
        cancel: &CancellationToken,
    ) -> Result<DuplicateJob<'q, 's>> {
        self.prepare_duplicate_selection(scratch, max_db_bytes, Some(profile), cancel)
    }

    fn prepare_duplicate_selection<'q>(
        &'q self,
        scratch: &Path,
        max_db_bytes: u64,
        profile: Option<DuplicateFileProfile>,
        cancel: &CancellationToken,
    ) -> Result<DuplicateJob<'q, 's>> {
        prepare_scopes_impl(
            vec![Scope {
                query: self,
                roots: vec![self.root.clone()],
                side: 0,
            }],
            Mode::WithinSelection,
            scratch,
            max_db_bytes,
            profile,
            cancel,
        )
    }
}
/// Prepare one disposable result database while borrowing every immutable query.
/// No source metadata or content I/O occurs here.
pub fn prepare_scopes<'q, 's>(
    scopes: Vec<Scope<'q, 's>>,
    mode: Mode,
    scratch: &Path,
    max_db_bytes: u64,
    cancel: &CancellationToken,
) -> Result<DuplicateJob<'q, 's>> {
    prepare_scopes_impl(scopes, mode, scratch, max_db_bytes, None, cancel)
}

/// Prepare an interactive file search without changing the source inventories.
pub fn prepare_scopes_with_profile<'q, 's>(
    scopes: Vec<Scope<'q, 's>>,
    mode: Mode,
    scratch: &Path,
    max_db_bytes: u64,
    profile: DuplicateFileProfile,
    cancel: &CancellationToken,
) -> Result<DuplicateJob<'q, 's>> {
    if mode.is_cross_source() {
        return Err(Error::Contract(
            "whole-folder comparison requires an unfiltered file profile",
        ));
    }
    prepare_scopes_impl(scopes, mode, scratch, max_db_bytes, Some(profile), cancel)
}

fn prepare_scopes_impl<'q, 's>(
    mut scopes: Vec<Scope<'q, 's>>,
    mode: Mode,
    scratch: &Path,
    max_db_bytes: u64,
    profile: Option<DuplicateFileProfile>,
    cancel: &CancellationToken,
) -> Result<DuplicateJob<'q, 's>> {
    if scopes.is_empty() || scopes.len() > 128 || max_db_bytes < 64 * 1024 {
        return Err(Error::Contract("duplicate scope/disk budget"));
    }
    let query = scopes[0].query;
    let mut guard_roots = Vec::new();
    for scope in &mut scopes {
        if !scope.query.finished
            || scope.side >= mode.side_count()
            || scope.roots.is_empty()
            || scope.roots.len() > 128
        {
            return Err(Error::Contract("unfinished duplicate scope or root budget"));
        }
        scope.roots.sort_by_key(|p| p.components().count());
        let mut roots = Vec::<PathBuf>::new();
        for root in std::mem::take(&mut scope.roots) {
            if !root.is_absolute()
                || !root.starts_with(&scope.query.root)
                || root
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(Error::Contract("duplicate root outside inventory"));
            }
            if !roots.iter().any(|parent| root.starts_with(parent)) {
                roots.push(root);
            }
        }
        if guard_roots.len() + roots.len() > 128
            || guard_roots
                .iter()
                .chain(&roots)
                .map(|p| p.as_os_str().as_encoded_bytes().len())
                .sum::<usize>()
                > 256 * 1024
        {
            return Err(Error::Contract("duplicate total root/path budget"));
        }
        guard_roots.extend(roots.iter().cloned());
        scope.roots = roots;
    }
    if mode.is_cross_source() {
        for side in 0..mode.side_count() {
            let source = scopes
                .iter()
                .find(|scope| scope.side == side)
                .ok_or(Error::Contract("missing chosen comparison folder"))?
                .query
                ._store
                .source;
            if scopes
                .iter()
                .any(|scope| scope.side == side && scope.query._store.source != source)
            {
                return Err(Error::Contract(
                    "each chosen comparison folder must have a consistent source identity",
                ));
            }
        }
        for (index, a) in scopes.iter().enumerate() {
            if scopes.iter().skip(index + 1).any(|b| {
                a.side != b.side
                    && a.roots.iter().any(|ra| {
                        b.roots
                            .iter()
                            .any(|rb| contains_path(ra, rb) || contains_path(rb, ra))
                    })
            }) {
                return Err(Error::Contract(
                    "chosen comparison folders are identical or nested",
                ));
            }
        }
    }
    let memory = Governor::global()
        .reserve(Category::Queries, 1024 * 1024, None)
        .ok_or(Error::Contract("duplicate working memory budget"))?;
    let space = workspace::Workspace::derived(query._store, scratch, max_db_bytes)?;
    let rank_by_gain = profile.is_some();
    let (db, vfs, owner) = open_results(&space, max_db_bytes, rank_by_gain)?;
    let mut summary = Summary::default();
    let mut tx = Some(db.unchecked_transaction()?);
    let mut batch = 0;
    let mut serial = 0u64;
    let mut guard_base = 0usize;
    for (index, scope) in scopes.iter().enumerate() {
        if scope
            .query
            ._store
            .acquisition_context(scope.query.generation)?
            .policy
            == crate::scheduler::DIRECTORY_LISTING_POLICY
        {
            summary.directory_listing_sources += 1;
        }
        scope.query.visit_matches(|r| {
            if cancel.is_cancelled() { summary.cancelled = true; return Ok(false); }
            let Some(guard) = scope.roots.iter().position(|root| r.path.starts_with(root)) else { return Ok(true); };
            if r.kind == Kind::Unknown { summary.unknown_sizes += 1; return Ok(true); }
            if r.kind != Kind::File { return Ok(true); }
            summary.examined_files += 1;
            if let Some(profile) = profile {
                let Some(size) = r.logical else {
                    summary.unknown_sizes += 1;
                    return Ok(true);
                };
                if profile.excludes(size, &mut summary.excluded_by_size, &mut summary.excluded_empty) {
                    return Ok(true);
                }
            }
            let duplicate: bool = tx.as_ref().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM candidates WHERE source=?1 AND inventory=?2 AND generation=?3 AND entry=?4)",params![scope.query._store.source.0.to_be_bytes().as_slice(),r.key.inventory.0.to_be_bytes().as_slice(),unsigned(r.key.generation.0),unsigned(r.key.entry.0)],|r|r.get(0))?;
            let same_path: bool=tx.as_ref().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM candidates WHERE path=?1)",[native_path(&r.path).encode()],|r|r.get(0))?;
            if duplicate || same_path { return Ok(true); }
            summary.selected_files += 1;
            let Some(size) = r.logical else { summary.unknown_sizes += 1; return Ok(true); };
            if profile.is_some() {
                summary.selected_logical_bytes += u128::from(size);
            }
            let (modified_s,modified_ns):(Option<i64>,Option<u32>) = scope.query._store.db.query_row("SELECT modified_s,modified_ns FROM entries WHERE generation=?1 AND id=?2",params![unsigned(r.key.generation.0),unsigned(r.key.entry.0)],|r|Ok((r.get(0)?,r.get(1)?)))?;
            serial = serial.checked_add(1).ok_or(Error::Contract("duplicate id exhausted"))?;
            // Preserve the historical single-query entry cursor contract.
            let id = if scopes.len() == 1 { r.key.entry.0 } else { serial };
            tx.as_ref().unwrap().execute("INSERT INTO candidates(id,scope,guard,side,source,inventory,generation,entry,path,size,modified_s,modified_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![unsigned(id),index as i64,(guard_base+guard) as i64,scope.side,scope.query._store.source.0.to_be_bytes().as_slice(),r.key.inventory.0.to_be_bytes().as_slice(),unsigned(r.key.generation.0),unsigned(r.key.entry.0),native_path(&r.path).encode(),unsigned(size),modified_s,modified_ns])?;
            batch += 1;
            if batch == 512 { tx.take().unwrap().commit()?; tx = Some(db.unchecked_transaction()?); batch=0; }
            Ok(true)
        })?;
        guard_base += scope.roots.len();
    }
    tx.take().unwrap().commit()?;
    drop(tx);
    let mut job = DuplicateJob {
        scopes,
        current: None,
        guard_roots,
        mode,
        rank_by_gain,
        max_errors: 100,
        root_issues: vec![],
        root_warnings: vec![],
        db,
        _vfs: vfs,
        space,
        owner,
        _memory: memory,
        _sqlite_memory: None,
        publication_memory: None,
        summary,
    };
    folders::prepare(&mut job, cancel)?;
    Ok(job)
}

fn open_results(
    space: &Arc<workspace::Workspace>,
    max_db_bytes: u64,
    rank_by_gain: bool,
) -> Result<(Connection, workspace::vfs::Vfs, PathBuf)> {
    open_results_with(space, max_db_bytes, None, rank_by_gain)
}

fn open_current_results(
    space: &Arc<workspace::Workspace>,
    sqlite_cache: u64,
    rank_by_gain: bool,
) -> Result<(Connection, workspace::vfs::Vfs, PathBuf)> {
    open_results_with(space, space.usage().limit, Some(sqlite_cache), rank_by_gain)
}

fn open_results_with(
    space: &Arc<workspace::Workspace>,
    max_db_bytes: u64,
    sqlite_cache: Option<u64>,
    rank_by_gain: bool,
) -> Result<(Connection, workspace::vfs::Vfs, PathBuf)> {
    let vfs = workspace::vfs::Vfs::new(space.clone())?;
    let owner = space.path().join("duplicates.db");
    let db = Connection::open_with_flags_and_vfs(
        &owner,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        vfs.name(),
    )?;
    db.execute_batch(
        "PRAGMA page_size=4096;PRAGMA synchronous=OFF;PRAGMA temp_store=FILE;PRAGMA mmap_size=0;",
    )?;
    if let Some(cache) = sqlite_cache {
        // Disk-backed rollback preserves committed proofs on SQLITE_FULL or
        // interruption. SQLite work files and the journal share the workspace.
        db.execute_batch(
            "PRAGMA journal_mode=DELETE;PRAGMA cache_spill=ON;PRAGMA threads=0;PRAGMA automatic_index=OFF;",
        )?;
        let temp_kib = (cache / 1024 / 4) as i64;
        db.pragma_update(None, "cache_size", -(cache as i64 / 1024 - temp_kib))?;
        db.execute_batch(&format!("PRAGMA temp.cache_size=-{temp_kib}"))?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 256 * 1024)?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    } else {
        db.execute_batch("PRAGMA journal_mode=OFF;PRAGMA cache_size=-512;")?;
    }
    db.pragma_update(
        None,
        "max_page_count",
        (max_db_bytes / 4096).min(u64::from(u32::MAX) - 1) as u32,
    )?;
    db.execute_batch("BEGIN;
      CREATE TABLE candidates(id BLOB PRIMARY KEY,scope INTEGER NOT NULL,guard INTEGER NOT NULL,side INTEGER NOT NULL,source BLOB NOT NULL,inventory BLOB NOT NULL,generation BLOB NOT NULL,entry BLOB NOT NULL,path BLOB NOT NULL,size BLOB NOT NULL,modified_s INTEGER,modified_ns INTEGER,identity BLOB,proof BLOB,hash BLOB,representative BLOB,group_id BLOB,acquired BLOB) WITHOUT ROWID;
      CREATE UNIQUE INDEX by_entry ON candidates(source,inventory,generation,entry);
      CREATE UNIQUE INDEX by_path ON candidates(path);
      CREATE INDEX by_size ON candidates(size,id);
      CREATE INDEX by_group ON candidates(group_id,id);
      CREATE INDEX by_side_group ON candidates(side,group_id,id);
      CREATE TABLE groups(id BLOB PRIMARY KEY,size BLOB NOT NULL,members BLOB NOT NULL,paths BLOB NOT NULL) WITHOUT ROWID;
      CREATE TABLE issues(id BLOB PRIMARY KEY,reason TEXT NOT NULL) WITHOUT ROWID;
      CREATE TABLE folder_covered(candidate BLOB PRIMARY KEY,folder BLOB NOT NULL) WITHOUT ROWID;
      CREATE TABLE review_control(reference INTEGER); INSERT INTO review_control VALUES(NULL);
      CREATE TABLE review_selected(candidate BLOB PRIMARY KEY) WITHOUT ROWID;
      CREATE TABLE review_empty_directories(node INTEGER PRIMARY KEY);")?;
    if sqlite_cache.is_some() {
        db.execute_batch(
            "CREATE INDEX by_identity ON candidates(identity,id) WHERE hash IS NOT NULL;
             CREATE INDEX by_hash ON candidates(size,hash,id) WHERE hash IS NOT NULL;",
        )?;
    } else {
        db.execute_batch(
            "CREATE INDEX by_identity ON candidates(identity,id);
             CREATE INDEX by_hash ON candidates(size,hash,id);",
        )?;
    }
    if rank_by_gain {
        db.execute_batch(
            "ALTER TABLE groups ADD COLUMN gain_rank BLOB;
             CREATE INDEX by_gain ON groups(gain_rank,id);",
        )?;
    }
    db.execute_batch("COMMIT")?;
    Ok((db, vfs, owner))
}

fn is_temporary_limit(error: &Error) -> bool {
    matches!(
        error,
        Error::DuplicateTemporaryLimit { .. }
            | Error::Contract("query workspace quota: refuse before write")
    ) || matches!(
        error,
        Error::Io(error) if matches!(
            error.kind(), io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
        )
    ) || matches!(
        error,
        Error::Sql(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DiskFull
    )
}

impl DuplicateJob<'_, '_> {
    fn provenance(&self, scope: usize, entry: EntryId) -> (u8, SourceId, EntryKey) {
        let (side, source, inventory, generation) = if let Some(current) = &self.current {
            (0, current.source, InventoryId(0), current.generation)
        } else {
            let scope = &self.scopes[scope];
            (
                scope.side,
                scope.query._store.source,
                scope.query._store.inventory,
                scope.query.generation,
            )
        };
        (
            side,
            source,
            EntryKey {
                inventory,
                generation,
                entry,
            },
        )
    }

    fn guard_scope(&self, guard: usize) -> Result<usize> {
        if self.current.is_some() && guard == 0 {
            return Ok(0);
        }
        self.scopes
            .iter()
            .enumerate()
            .flat_map(|(index, scope)| std::iter::repeat_n(index, scope.roots.len()))
            .nth(guard)
            .ok_or(Error::Contract("unknown comparison guard"))
    }

    fn discard_groups(&mut self) -> Result<()> {
        self.db.execute("DELETE FROM groups", [])?;
        self.summary.groups = 0;
        self.summary.independent_members = 0;
        self.summary.complete = false;
        self.summary.source_independence_verified = false;
        Ok(())
    }

    pub fn temporary_usage(&self) -> TemporaryUsage {
        self.space.usage()
    }
    pub fn current_folder_publication_reservation(&self) -> Option<Arc<Reservation>> {
        self.publication_memory.clone()
    }
    pub fn summary(&self) -> &Summary {
        &self.summary
    }
    pub fn candidate_count(&self) -> Result<u64> {
        Ok(self
            .db
            .prepare("SELECT count(*) FROM candidates")?
            .query_row([], |r| r.get::<_, i64>(0).map(|n| n as u64))?)
    }
    fn issue(&mut self, id: u64, error: io::Error, cancel: &CancellationToken) -> Result<bool> {
        Self::record_issue(
            &self.db,
            &mut self.summary,
            self.current.as_ref().map(|current| current.errors),
            self.max_errors,
            id,
            error,
            cancel,
        )
    }

    fn record_issue(
        db: &Connection,
        summary: &mut Summary,
        acquisition_errors: Option<u64>,
        max_errors: usize,
        id: u64,
        e: io::Error,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        if e.kind() == io::ErrorKind::Interrupted && cancel.is_cancelled() {
            summary.cancelled = true;
            return Ok(true);
        }
        if let Some(limit) = ContentLimit::from_io(&e) {
            summary.budget_limited = true;
            return match limit {
                ContentLimit::TotalRead(limit) => {
                    summary.total_read_limit = Some(limit);
                    Ok(true)
                }
                ContentLimit::FileSize(limit) => {
                    summary.file_size_limit = Some(limit);
                    Ok(acquisition_errors.is_none())
                }
            };
        }
        summary.errors += 1;
        db.execute(
            "INSERT OR REPLACE INTO issues VALUES(?1,?2)",
            params![
                unsigned(id),
                e.to_string().chars().take(512).collect::<String>()
            ],
        )?;
        if summary.errors - acquisition_errors.unwrap_or(0) >= max_errors as u64 {
            summary.error_limit = Some(max_errors);
        }
        Ok(summary.error_limit.is_some())
    }
    /// Explicit content action. One global verifier owns at most two comparison
    /// handles plus bounded validation/path-walk handles, never one per member.
    /// A stopped job is final/partial; creating a new job is explicit.
    pub fn verify(
        &mut self,
        guard: &Guard,
        options: DuplicateOptions,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.current.is_some() {
            return self.verify_scoped(std::slice::from_ref(guard), options, cancel);
        }
        if self.summary.verified {
            return Err(Error::Contract("duplicate job already verified"));
        }
        if self.guard_roots.len() != 1 || guard.root() != self.guard_roots[0] {
            return Err(Error::Contract("duplicate source root mismatch"));
        }
        let _permit = CONTENT_JOB
            .try_lock()
            .map_err(|_| Error::Contract("content verifier already active"))?;
        self.max_errors = options.max_errors;
        let mut native = NativeVerification {
            verifier: Verifier::new(options, cancel),
            guards: &[Some(guard)],
        };
        if !self.validate_roots(native.guards, cancel) {
            self.summary.verified = true;
            return Ok(());
        }
        self.verify_with(&mut native, cancel)?;
        self.validate_roots(native.guards, cancel);
        folders::verify(self, native.guards, cancel)?;
        Ok(())
    }
    fn validate_roots(&mut self, guards: &[Option<&Guard>], cancel: &CancellationToken) -> bool {
        if cancel.is_cancelled() {
            self.summary.cancelled = true;
            self.summary.complete = false;
            self.summary.source_independence_verified = false;
            if self.current.is_none() {
                return false;
            }
        }
        if let Some(current) = &self.current {
            let attachment = match guards.first().copied().flatten() {
                Some(guard) if std::ptr::eq(guard, current.guard.as_ref()) => {
                    current::check_attachment(guard).map_err(|error| error.to_string())
                }
                Some(_) => Err("The selected folder guard differs from the acquired source".into()),
                None => Err("The selected folder source is unavailable".into()),
            };
            return match attachment {
                Ok(Attachment::Compatible) => true,
                Ok(Attachment::Uncertain) => {
                    if self.root_warnings.is_empty() {
                        self.root_warnings.push((0,"Source continuity is unverified; current contents do not prove coverage or independent copies".into()));
                    }
                    self.summary.unverified_sources = 1;
                    self.summary.complete = false;
                    self.summary.source_independence_verified = false;
                    true
                }
                Err(reason) => {
                    if self.root_issues.is_empty() {
                        self.root_issues
                            .push((0, reason.chars().take(512).collect()));
                        self.summary.errors += 1;
                    }
                    self.summary.complete = false;
                    self.summary.source_independence_verified = false;
                    false
                }
                Ok(Attachment::Different) => {
                    unreachable!("attachment check refuses different roots")
                }
            };
        }
        let mut valid = true;
        let slots = self
            .scopes
            .iter()
            .enumerate()
            .flat_map(|(index, s)| std::iter::repeat_n(index, s.roots.len()))
            .collect::<Vec<_>>();
        for (guard, scope) in guards.iter().zip(slots) {
            let query = self.scopes[scope].query;
            let mut uncertain = false;
            let issue = match guard.map(|guard| guard.inspect(guard.root())) {
                None => {
                    valid = false;
                    Some("This chosen source is unavailable; its coverage was not verified".into())
                }
                Some(Err(error)) => {
                    valid = false;
                    Some(error.to_string())
                }
                Some(Ok(_)) if guard.unwrap().root() == query.root => {
                    let guard = guard.unwrap();
                    match crate::scheduler::Session::source_attachment(
                        query._store,
                        query.generation,
                        guard,
                    ) {
                        Ok(Attachment::Compatible) => None,
                        Ok(Attachment::Different) => {
                            valid = false;
                            Some("This source differs from its recorded inventory; comparison was refused".into())
                        }
                        Ok(Attachment::Uncertain) => {
                            uncertain = true;
                            None
                        }
                        Err(error) => {
                            valid = false;
                            Some(error.to_string())
                        }
                    }
                }
                Some(Ok(stamp)) if self.mode.is_cross_source() => {
                    match Self::subroot_attachment(query, guard.unwrap(), &stamp) {
                        Ok(Attachment::Compatible) => None,
                        Ok(Attachment::Uncertain) => {
                            uncertain = true;
                            None
                        }
                        Ok(Attachment::Different) => {
                            valid = false;
                            Some("This selected folder differs from its recorded inventory; comparison was refused".into())
                        }
                        Err(error) => {
                            valid = false;
                            Some(error.to_string())
                        }
                    }
                }
                Some(Ok(_)) => {
                    uncertain = true;
                    None
                }
            };
            if uncertain && !self.root_warnings.iter().any(|(index, _)| *index == scope) {
                self.root_warnings.push((scope,"Source continuity is unverified; current contents do not prove inventory coverage or independent copies".into()));
                self.summary.unverified_sources = self.root_warnings.len() as u64;
            }
            if let Some(reason) = issue {
                if !self.root_issues.iter().any(|(index, _)| *index == scope) {
                    self.root_issues
                        .push((scope, reason.chars().take(512).collect()));
                    self.summary.errors += 1;
                }
                self.summary.complete = false;
            }
        }
        if self.summary.unverified_sources > 0 || !valid {
            self.summary.complete = false;
            self.summary.source_independence_verified = false;
        }
        valid
    }
    /// Prove only the selected recorded child, under its existing exact guard.
    /// The parent source is never opened to establish this local correlation.
    fn subroot_attachment(
        query: &Query<'_>,
        guard: &Guard,
        stamp: &sortsize_core::acquisition::Stamp,
    ) -> Result<Attachment> {
        let boundary = crate::scheduler::Session::subroot_boundary_attachment(
            query._store,
            query.generation,
            &query.root,
            guard,
        )?;
        if boundary != Attachment::Compatible {
            return Ok(boundary);
        }
        let current = crate::inventory::differential::NativeEvidence::from_stamp(stamp).encode();
        if !stamp.directory || current.is_empty() {
            return Ok(Attachment::Uncertain);
        }
        let has_evidence: bool = query._store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='entry_evidence')", [], |r| r.get(0),
        )?;
        if !has_evidence {
            return Ok(Attachment::Uncertain);
        }
        let relative = guard
            .root()
            .strip_prefix(&query.root)
            .map_err(|_| Error::Contract("selected folder outside recorded root"))?;
        let mut entry = 0u64;
        let mut depth = 0usize;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Ok(Attachment::Uncertain);
            };
            depth += 1;
            if depth > 1024 {
                return Ok(Attachment::Uncertain);
            }
            let next: Option<u64> = query._store.db.query_row(
                "SELECT id FROM entries WHERE generation=?1 AND parent=?2 AND name=?3 AND kind=2 AND present=1",
                params![unsigned(query.generation.0), unsigned(entry), native_path(Path::new(name)).encode()],
                |r| number(r, 0),
            ).optional()?;
            let Some(next) = next else {
                return Ok(Attachment::Uncertain);
            };
            entry = next;
        }
        let recorded: Option<Vec<u8>> = query._store.db.query_row(
            "SELECT CASE WHEN length(proof)<=64 THEN proof ELSE NULL END FROM entry_evidence WHERE generation=?1 AND id=?2",
            params![unsigned(query.generation.0), unsigned(entry)], |r| r.get(0),
        ).optional()?.flatten();
        let Some(recorded) = recorded.filter(|proof| !proof.is_empty()) else {
            return Ok(Attachment::Uncertain);
        };
        Ok(if recorded == current {
            Attachment::Compatible
        } else {
            Attachment::Different
        })
    }
    fn require_guards(&self, guards: &[Option<&Guard>]) -> Result<()> {
        if guards.len() != self.guard_roots.len()
            || guards
                .iter()
                .zip(&self.guard_roots)
                .any(|(g, r)| g.is_some_and(|g| g.root() != r))
        {
            return Err(Error::Contract("duplicate source guard mismatch"));
        }
        if self.mode.is_cross_source() {
            let sides = self
                .scopes
                .iter()
                .flat_map(|s| std::iter::repeat_n(s.side, s.roots.len()))
                .collect::<Vec<_>>();
            for (i, a) in guards.iter().enumerate() {
                for (j, b) in guards.iter().enumerate().skip(i + 1) {
                    let (Some(a), Some(b)) = (a, b) else { continue };
                    if sides[i] != sides[j]
                        && a.evidence().1.identity.is_some()
                        && a.evidence().1.identity == b.evidence().1.identity
                    {
                        return Err(Error::Contract(
                            "Chosen sources are known or possible native root aliases",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    /// Explicit action only. Every selected root keeps its own access guard.
    pub fn verify_scoped(
        &mut self,
        guards: &[Guard],
        options: DuplicateOptions,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.verify_scoped_with_progress(guards, options, cancel, |_| {})
    }
    /// Verify using exactly the acquisition guard retained by the live job.
    pub fn verify_current_folder_with_progress(
        &mut self,
        options: DuplicateOptions,
        cancel: &CancellationToken,
        progress: impl FnMut(&Summary),
    ) -> Result<()> {
        let guard = self
            .current
            .as_ref()
            .ok_or(Error::Contract("not a current-folder duplicate job"))?
            .guard
            .clone();
        self.verify_available_borrowed(&[Some(&guard)], options, cancel, progress)
    }
    pub fn guard_roots(&self) -> &[PathBuf] {
        &self.guard_roots
    }
    pub fn verify_scoped_with_progress(
        &mut self,
        guards: &[Guard],
        options: DuplicateOptions,
        cancel: &CancellationToken,
        progress: impl FnMut(&Summary),
    ) -> Result<()> {
        let borrowed = guards.iter().map(Some).collect::<Vec<_>>();
        self.verify_available_borrowed(&borrowed, options, cancel, progress)
    }
    /// A missing chosen source remains part of the global coverage. The other
    /// sources may still publish independently verified groups in three-source
    /// mode. No replacement guard or fabricated source identity is introduced.
    pub fn verify_available_scoped_with_progress(
        &mut self,
        guards: &[Option<Guard>],
        options: DuplicateOptions,
        cancel: &CancellationToken,
        progress: impl FnMut(&Summary),
    ) -> Result<()> {
        let borrowed = guards.iter().map(Option::as_ref).collect::<Vec<_>>();
        self.verify_available_borrowed(&borrowed, options, cancel, progress)
    }
    fn verify_available_borrowed(
        &mut self,
        guards: &[Option<&Guard>],
        options: DuplicateOptions,
        cancel: &CancellationToken,
        mut progress: impl FnMut(&Summary),
    ) -> Result<()> {
        self.require_guards(guards)?;
        let _permit = CONTENT_JOB
            .try_lock()
            .map_err(|_| Error::Contract("content verifier already active"))?;
        self.max_errors = options.max_errors;
        let mut native = NativeVerification {
            verifier: Verifier::new(options, cancel),
            guards,
        };
        if !self.validate_roots(native.guards, cancel)
            && (self.mode != Mode::BetweenThreeSources || cancel.is_cancelled())
        {
            self.summary.verified = true;
            if self.current.is_some() {
                progress(&self.summary);
            }
            return Ok(());
        }
        if self.current.is_some() {
            self.verify_current_with(&mut native, options, cancel, &mut progress)?;
        } else {
            self.verify_with_progress(&mut native, options, cancel, &mut progress)?;
        }
        let attached = self.validate_roots(native.guards, cancel);
        if self.current.is_some() && !attached {
            // The file groups have only job-local addresses. A detached root
            // cannot present these as groups at its former paths.
            self.discard_groups()?;
        }
        folders::verify(self, native.guards, cancel)?;
        if let Some(current) = &self.current {
            self.summary.unverified_files = if attached
                && current.complete
                && current.errors == 0
                && self.summary.unknown_sizes == 0
                && self.summary.unverified_sources == 0
                && (self.summary.resources.is_none() || !cancel.is_cancelled())
            {
                Some(if self.summary.complete {
                    0
                } else {
                    self.count_unverified_files()?
                })
            } else {
                None
            };
            self.summary.verification_phase = None;
            progress(&self.summary);
        }
        Ok(())
    }
    fn verify_current_with(
        &mut self,
        verifier: &mut impl Verification,
        options: DuplicateOptions,
        cancel: &CancellationToken,
        progress: &mut impl FnMut(&Summary),
    ) -> Result<()> {
        let token = cancel.clone();
        self.db
            .progress_handler(4096, Some(move || token.is_cancelled()))?;
        let result = self.verify_with_progress(verifier, options, cancel, progress);
        self.db.progress_handler(0, None::<fn() -> bool>)?;
        // SQLITE_FULL may roll back the current transaction automatically.
        // Roll back explicitly if it did not, before querying retained groups.
        if !self.db.is_autocommit() {
            self.db.execute_batch("ROLLBACK")?;
        }
        self.summary.update_content(verifier.progress());
        self.refresh_resources();
        match result {
            Ok(()) => {}
            Err(Error::Sql(rusqlite::Error::SqliteFailure(error, _)))
                if error.code == rusqlite::ErrorCode::OperationInterrupted
                    && cancel.is_cancelled() =>
            {
                self.summary.cancelled = true;
            }
            Err(error) if is_temporary_limit(&error) => {
                self.summary.temporary_limit = Some(self.space.usage().limit);
                self.summary.budget_limited = true;
                if let Some(usage) = &mut self.summary.resources {
                    usage.cause = usage.cause.or(Some(current::native_resource_cause(&error)));
                }
            }
            Err(error) => return Err(error),
        }
        if self.summary.resources.is_none() {
            self.update_aliases()?;
        }
        Ok(())
    }
    fn refresh_resources(&mut self) {
        if let Some(usage) = &mut self.summary.resources {
            usage.refresh(&self.space);
        }
    }
    fn refresh_resources_if_due(
        resources: &mut Option<CurrentFolderResourceUsage>,
        space: &workspace::Workspace,
        last: &mut Instant,
    ) {
        if let Some(usage) = resources
            && last.elapsed() >= Duration::from_millis(200)
        {
            usage.refresh(space);
            *last = Instant::now();
        }
    }
    fn count_unverified_files(&self) -> Result<u64> {
        // Size-unique occurrences need no content read. A missing hash leaves
        // the containing size bucket unclassified; otherwise only a repeated
        // digest with independent identities needs a final comparison.
        let count: i64 = self.db.query_row(
            "SELECT
                (SELECT count(*) FROM candidates c WHERE c.hash IS NULL AND EXISTS(
                    SELECT 1 FROM candidates b WHERE b.size=c.size AND b.id!=c.id))
                +
                (SELECT coalesce(sum(n),0) FROM (
                    SELECT count(*) AS n FROM candidates c INDEXED BY by_hash
                    LEFT JOIN groups g ON g.id=c.group_id WHERE c.hash IS NOT NULL
                    GROUP BY c.size,c.hash
                    HAVING max(g.id) IS NULL AND (
                      sum(c.id=c.representative)>1 OR EXISTS(
                        SELECT 1 FROM candidates b WHERE b.size=c.size AND b.hash IS NULL))))",
            [],
            |r| r.get(0),
        )?;
        u64::try_from(count).map_err(|_| Error::Contract("negative unverified duplicate count"))
    }
    fn update_aliases(&mut self) -> Result<()> {
        self.summary.aliases = self.db.query_row(
            "SELECT count(*) FROM candidates WHERE representative IS NOT NULL AND representative!=id",
            [],
            |r| r.get::<_, i64>(0).map(|n| n as u64),
        )?;
        Ok(())
    }
    fn verify_with(
        &mut self,
        verifier: &mut impl Verification,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.verify_with_progress(verifier, DuplicateOptions::default(), cancel, &mut |_| {})
    }
    fn verify_with_progress(
        &mut self,
        verifier: &mut impl Verification,
        options: DuplicateOptions,
        cancel: &CancellationToken,
        progress: &mut impl FnMut(&Summary),
    ) -> Result<()> {
        if self.summary.verified {
            return Err(Error::Contract("duplicate job already verified"));
        }
        self.summary.verified = true;
        if self.summary.cancelled {
            return Ok(());
        }
        let publish_blocks = self.current.is_some();
        let batch_hash_updates = self.summary.resources.is_some();
        let mut batch_count = 0;
        let mut batch_aliases = 0;
        let mut last_resources = Instant::now();
        let mut last_block = None;
        self.summary.verification_phase = Some(VerificationPhase::Hashing);
        let acquisition_errors = self.current.as_ref().map(|current| current.errors);
        let issue = |summary: &mut Summary, id, error| {
            Self::record_issue(
                &self.db,
                summary,
                acquisition_errors,
                self.max_errors,
                id,
                error,
                cancel,
            )
        };
        // Keyset traversal: at most one path/proof in RAM. Size counts use the
        // on-disk index, including arbitrarily large equal-size/empty buckets.
        let candidate_sql = if self.mode.is_cross_source() {
            "SELECT c.id,c.path,c.size,c.guard,c.modified_s,c.modified_ns,c.acquired FROM candidates c WHERE c.id>?1 AND EXISTS(SELECT 1 FROM candidates b WHERE b.size=c.size AND b.side!=c.side) ORDER BY c.id LIMIT 1"
        } else {
            "SELECT c.id,c.path,c.size,c.guard,c.modified_s,c.modified_ns,c.acquired FROM candidates c WHERE c.id>?1 AND EXISTS(SELECT 1 FROM candidates b WHERE b.size=c.size AND b.id!=c.id) ORDER BY c.id LIMIT 1"
        };
        let mut candidate = self.db.prepare(candidate_sql)?;
        let mut hashed_identity = self.db.prepare(
            "SELECT id,proof,hash FROM candidates
             WHERE identity=?1 AND hash IS NOT NULL ORDER BY id LIMIT 1",
        )?;
        #[cfg(windows)]
        let mut alias_scopes = self.db.prepare(
            "SELECT a.scope,b.scope FROM candidates a,candidates b WHERE a.id=?1 AND b.id=?2",
        )?;
        let mut save_hash = self.db.prepare(
            "UPDATE candidates SET identity=?2,proof=?3,hash=?4,representative=?5 WHERE id=?1",
        )?;
        let mut after = None;
        loop {
            if cancel.is_cancelled() {
                self.summary.cancelled = true;
                break;
            }
            let row: Option<CandidateRecord> = candidate
                .query_row([unsigned(after.unwrap_or(0))], |r| {
                    Ok((
                        number(r, 0)?,
                        r.get(1)?,
                        number(r, 2)?,
                        r.get::<_, i64>(3)? as usize,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .optional()?;
            let Some((id, raw, size, guard, modified_s, modified_ns, acquired)) = row else {
                break;
            };
            after = Some(id);
            let scope = self.guard_scope(guard)?;
            if self.root_issues.iter().any(|(index, _)| *index == scope) {
                continue;
            }
            if publish_blocks && let Some(limit) = options.file_size_limit(size) {
                // Strictly acquired sizes can exclude candidates without
                // opening their contents or charging an access error.
                self.summary.file_size_limit = Some(limit);
                self.summary.budget_limited = true;
                continue;
            }
            let p = path(raw)?;
            let observed = time(modified_s, modified_ns);
            if batch_hash_updates && batch_count == 0 {
                self.db.execute_batch("BEGIN")?;
            }
            let result = (|| -> Result<bool> {
                let proof = if let Some(acquired) = acquired {
                    verifier.observe_current(guard, &p, size, observed, &acquired)?
                } else {
                    verifier.observe(guard, &p, size, observed)?
                };
                let prior: Option<(u64, Vec<u8>, Vec<u8>)> = hashed_identity
                    .query_row([proof.identity()], |r| {
                        Ok((number(r, 0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()?;
                #[cfg(windows)]
                if let Some((rep, _, _)) = &prior {
                    let (old_scope, this_scope): (i64, i64) = alias_scopes
                        .query_row(params![unsigned(*rep), unsigned(id)], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })?;
                    if old_scope != this_scope {
                        self.summary.ambiguous_aliases += 1;
                        return Err(io::Error::other("Possible cross-source native identity alias; independence is unverified").into());
                    }
                }
                let (representative, hash) = if let Some((rep, old, hash)) = prior {
                    if Proof::decode(&old)? != proof {
                        return Err(io::Error::other("Hardlink changed").into());
                    }
                    (rep, hash)
                } else {
                    let mut notify = |update| {
                        if publish_blocks {
                            self.summary.update_content_in_flight(
                                update,
                                &mut last_block,
                                progress,
                            );
                        }
                    };
                    (id, verifier.hash(guard, &p, &proof, &mut notify)?.to_vec())
                };
                save_hash.execute(params![
                    unsigned(id),
                    proof.identity(),
                    proof.encode()?,
                    hash,
                    unsigned(representative)
                ])?;
                Ok(representative != id)
            })();
            match result {
                Ok(alias) => batch_aliases += u64::from(alias),
                Err(Error::Io(e)) => {
                    if issue(&mut self.summary, id, e)? {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
            if batch_hash_updates {
                batch_count += 1;
                if batch_count == 128 {
                    self.db.execute_batch("COMMIT")?;
                    self.summary.aliases += batch_aliases;
                    batch_aliases = 0;
                    batch_count = 0;
                }
            }
            self.summary.update_content(verifier.progress());
            Self::refresh_resources_if_due(
                &mut self.summary.resources,
                &self.space,
                &mut last_resources,
            );
            progress(&self.summary);
        }
        drop((candidate, hashed_identity, save_hash));
        #[cfg(windows)]
        drop(alias_scopes);
        if batch_hash_updates && !self.db.is_autocommit() {
            self.db.execute_batch("COMMIT")?;
            self.summary.aliases += batch_aliases;
        }
        // Compare a hash bucket without ever loading its members. A bucket is
        // published only when every member and alias was revalidated. Failed
        // buckets stay unclassified, never silently 'unique'.
        let mut next: Option<(Vec<u8>, Vec<u8>)> = None;
        if !self.summary.cancelled
            && self.summary.total_read_limit.is_none()
            && self.summary.error_limit.is_none()
            && (!self.summary.budget_limited || publish_blocks)
        {
            self.summary.verification_phase = Some(VerificationPhase::Comparing);
            last_block = None;
            let mut next_bucket = self.db.prepare(
                "SELECT size,hash FROM candidates WHERE hash IS NOT NULL AND (size,hash)>(?1,?2)
                 GROUP BY size,hash HAVING min(identity)<>max(identity) ORDER BY size,hash LIMIT 1",
            )?;
            let mut bucket_anchor = self.db.prepare(
                "SELECT id,path,proof,guard FROM candidates
                 WHERE size=?1 AND hash=?2 ORDER BY id LIMIT 1",
            )?;
            let mut bucket_member = self.db.prepare(
                "SELECT id,path,proof,representative,guard,side FROM candidates
                 WHERE size=?1 AND hash=?2 AND id>?3 ORDER BY id LIMIT 1",
            )?;
            let mut member_batch = self.db.prepare(
                "SELECT id FROM candidates WHERE size=?1 AND hash=?2 AND id>?3
                 ORDER BY id LIMIT 512",
            )?;
            let mut assign_group = self
                .db
                .prepare("UPDATE candidates SET group_id=?2 WHERE id=?1")?;
            let mut publish_group = self.db.prepare(if self.rank_by_gain {
                "INSERT INTO groups VALUES(?1,?2,?3,?4,?5)"
            } else {
                "INSERT INTO groups VALUES(?1,?2,?3,?4)"
            })?;
            loop {
                if cancel.is_cancelled() {
                    self.summary.cancelled = true;
                    break;
                }
                let (last_size, last_hash) =
                    next.as_ref().map_or((&[][..], &[][..]), |(size, hash)| {
                        (size.as_slice(), hash.as_slice())
                    });
                let bucket: Option<(Vec<u8>, Vec<u8>)> = next_bucket
                    .query_row(params![last_size, last_hash], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .optional()?;
                let Some((size, hash)) = bucket else { break };
                next = Some((size.clone(), hash.clone()));
                let expected_hash = hash
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Contract("invalid comparison hash"))?;
                let (first, raw, bytes, anchor_guard): (u64, Vec<u8>, Vec<u8>, usize) =
                    bucket_anchor.query_row(params![size, hash], |r| {
                        Ok((
                            number(r, 0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get::<_, i64>(3)? as usize,
                        ))
                    })?;
                let anchor = path(raw)?;
                let anchor_proof = Proof::decode(&bytes)?;
                let mut last = None;
                let mut valid = true;
                let mut members = 0;
                let mut paths = 0;
                let mut sides = [false; 3];
                loop {
                    let r: Option<VerificationRecord> = bucket_member
                        .query_row(params![size, hash, unsigned(last.unwrap_or(0))], |r| {
                            Ok((
                                number(r, 0)?,
                                r.get(1)?,
                                r.get(2)?,
                                number(r, 3)?,
                                r.get::<_, i64>(4)? as usize,
                                r.get::<_, i64>(5)? as usize,
                            ))
                        })
                        .optional()?;
                    let Some((id, raw, bytes, rep, guard, side)) = r else {
                        break;
                    };
                    last = Some(id);
                    let p = path(raw)?;
                    let proof = Proof::decode(&bytes)?;
                    let checked = if rep == first || rep != id {
                        verifier.validate(guard, &p, &proof).map(|()| true)
                    } else {
                        let mut notify = |update| {
                            if publish_blocks {
                                self.summary.update_content_in_flight(
                                    update,
                                    &mut last_block,
                                    progress,
                                );
                            }
                        };
                        verifier.equal(
                            (anchor_guard, &anchor, &anchor_proof),
                            (guard, &p, &proof),
                            expected_hash,
                            &mut notify,
                        )
                    };
                    match checked {
                        Ok(true) => {}
                        Ok(false) => {
                            issue(
                                &mut self.summary,
                                id,
                                io::Error::other("Hash collision or concurrent mutation"),
                            )?;
                            valid = false;
                            break;
                        }
                        Err(e) => {
                            issue(&mut self.summary, id, e)?;
                            valid = false;
                            break;
                        }
                    }
                    self.summary.update_content(verifier.progress());
                    Self::refresh_resources_if_due(
                        &mut self.summary.resources,
                        &self.space,
                        &mut last_resources,
                    );
                    progress(&self.summary);
                    paths += 1;
                    if rep == id {
                        sides[side] = true;
                        members += 1;
                    }
                }
                if valid
                    && members > 1
                    && (self.mode == Mode::WithinSelection
                        || sides.iter().filter(|s| **s).count() >= 2)
                {
                    // SQLite update uses bounded batches instead of a huge group transaction.
                    let mut last = None;
                    loop {
                        if cancel.is_cancelled() {
                            self.summary.cancelled = true;
                            break;
                        }
                        let ids: Vec<u64> = member_batch
                            .query_map(params![size, hash, unsigned(last.unwrap_or(0))], |r| {
                                number(r, 0)
                            })?
                            .collect::<rusqlite::Result<_>>()?;
                        if ids.is_empty() {
                            break;
                        }
                        last = ids.last().copied();
                        // A shared connection keeps the statements reusable. The hash
                        // batch is closed, and collecting ids resets the read cursor.
                        let tx = self.db.unchecked_transaction()?;
                        for id in ids {
                            assign_group.execute(params![unsigned(id), unsigned(first)])?;
                        }
                        tx.commit()?;
                    }
                    if cancel.is_cancelled() {
                        self.summary.cancelled = true;
                        break;
                    }
                    if self.rank_by_gain {
                        let logical_size = u64::from_be_bytes(
                            size.as_slice()
                                .try_into()
                                .map_err(|_| Error::Contract("invalid duplicate size"))?,
                        );
                        let logical_gain = u128::from(logical_size) * u128::from(members - 1);
                        // Fixed-width, inverted unsigned key: largest gain first.
                        let gain_rank = (u128::MAX - logical_gain).to_be_bytes();
                        publish_group.execute(params![
                            unsigned(first),
                            size,
                            unsigned(members),
                            unsigned(paths),
                            gain_rank,
                        ])?;
                    } else {
                        publish_group.execute(params![
                            unsigned(first),
                            size,
                            unsigned(members),
                            unsigned(paths),
                        ])?;
                    }
                    self.summary.groups += 1;
                    self.summary.independent_members += members;
                }
                if self.summary.cancelled
                    || self.summary.total_read_limit.is_some()
                    || self.summary.error_limit.is_some()
                {
                    break;
                }
            }
        }
        self.summary.update_content(verifier.progress());
        self.summary.verification_phase = None;
        if self.current.is_none() {
            self.update_aliases()?;
        }
        self.summary.complete = self.current.as_ref().map_or_else(
            || self.scopes.iter().all(|s| s.query.complete()),
            |current| current.complete,
        ) && self.summary.unknown_sizes == 0
            && self.summary.errors == 0
            && self.summary.unverified_sources == 0
            && self.summary.directory_listing_sources == 0
            && !self.summary.cancelled
            && !self.summary.budget_limited;
        self.summary.source_independence_verified =
            self.summary.complete && self.summary.ambiguous_aliases == 0;
        Ok(())
    }
    /// Errors are attributed to the original selected scope, without source I/O.
    pub fn scope_errors(&self) -> Result<Vec<u64>> {
        if self.current.is_some() {
            return Ok(vec![self.summary.errors]);
        }
        self.scopes.iter().enumerate().map(|(index,_)| self.db.query_row("SELECT count(*) FROM issues i JOIN candidates c ON c.id=i.id WHERE c.scope=?1",[index as i64],|r|r.get::<_,i64>(0).map(|n|n as u64)).map(|count|count+self.root_issues.iter().filter(|(i,_)|*i==index).count() as u64).map_err(Error::from)).collect()
    }
    /// Metadata warnings do not consume the I/O-error budget.
    pub fn warnings(&self) -> &[(usize, String)] {
        &self.root_warnings
    }
    pub fn diagnostics(&self) -> Result<Vec<(usize, String)>> {
        let mut rows=self.db.prepare("SELECT c.scope,i.reason FROM issues i JOIN candidates c ON c.id=i.id ORDER BY i.id LIMIT 100")?.query_map([],|r|Ok((r.get::<_,i64>(0)? as usize,r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.extend(self.root_issues.iter().cloned());
        if let Some(current) = &self.current {
            rows.extend(current.diagnostics());
        }
        Ok(rows)
    }
    pub fn groups(&self, after: Option<u64>) -> Result<Vec<Group>> {
        let mut statement = self.db.prepare(if self.rank_by_gain {
            "SELECT id,size,members,paths FROM groups
             WHERE (gain_rank,id)>(?1,?2) ORDER BY gain_rank,id LIMIT 100"
        } else {
            "SELECT id,size,members,paths FROM groups WHERE id>?1 ORDER BY id LIMIT 100"
        })?;
        let mut rows = if self.rank_by_gain {
            statement.query(params![
                self.group_rank(after)?,
                unsigned(after.unwrap_or(0))
            ])?
        } else {
            statement.query([unsigned(after.unwrap_or(0))])?
        };
        let mut groups = Vec::new();
        while let Some(row) = rows.next()? {
            groups.push(Group {
                id: number(row, 0)?,
                logical_bytes: number(row, 1)?,
                independent_members: number(row, 2)?,
                paths: number(row, 3)?,
            });
        }
        Ok(groups)
    }

    fn group_rank(&self, group: Option<u64>) -> Result<[u8; 16]> {
        group.map_or(Ok([0; 16]), |group| {
            self.db
                .query_row(
                    "SELECT gain_rank FROM groups WHERE id=?1",
                    [unsigned(group)],
                    |r| r.get(0),
                )
                .map_err(Into::into)
        })
    }
    pub fn members(&self, group: u64, after: Option<u64>) -> Result<Vec<Member>> {
        let mut q=self.db.prepare("SELECT c.id,c.path,c.representative,c.scope,c.entry FROM candidates c JOIN groups g ON g.id=c.group_id WHERE g.id=?1 AND c.id>?2 ORDER BY c.id LIMIT 200")?;
        let mut rows = q.query(params![unsigned(group), unsigned(after.unwrap_or(0))])?;
        let mut out = Vec::new();
        let mut used = 0;
        while let Some(r) = rows.next()? {
            let raw: Vec<u8> = r.get(1)?;
            let p = path(raw)?;
            let bytes = p.as_os_str().as_encoded_bytes().len() * 2 + 256;
            if used + bytes > 512 * 1024 {
                break;
            }
            used += bytes;
            let entry = number(r, 4)?;
            let (side, source, key) = self.provenance(r.get::<_, i64>(3)? as usize, EntryId(entry));
            out.push(Member {
                side,
                source,
                key,
                entry,
                cursor: number(r, 0)?,
                composite: self.scopes.len() > 1,
                path: sortsize_core::NativePath::from(p.as_path()),
                representative: number(r, 2)?,
            });
        }
        Ok(out)
    }
}

// Private injection boundary: production always constructs NativeVerification.
// Tests can qualify disk grouping cardinality without pretending to read files.
trait Verification {
    fn observe(
        &self,
        guard: usize,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
    ) -> io::Result<Proof>;
    fn observe_current(
        &self,
        _guard: usize,
        _path: &Path,
        _length: u64,
        _modified: Option<SystemTime>,
        _evidence: &[u8],
    ) -> io::Result<Proof> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn hash(
        &mut self,
        guard: usize,
        path: &Path,
        proof: &Proof,
        progress: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<[u8; 32]>;
    fn validate(&self, guard: usize, path: &Path, proof: &Proof) -> io::Result<()>;
    fn equal(
        &mut self,
        a: (usize, &Path, &Proof),
        b: (usize, &Path, &Proof),
        expected_hash: &[u8; 32],
        progress: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<bool>;
    fn progress(&self) -> sortsize_core::duplicates::DuplicateProgress;
}
struct NativeVerification<'a, 'b> {
    verifier: Verifier<'a>,
    guards: &'b [Option<&'b Guard>],
}
impl NativeVerification<'_, '_> {
    fn guard(&self, index: usize) -> io::Result<&Guard> {
        self.guards
            .get(index)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::other("Chosen source is unavailable"))
    }
}
impl Verification for NativeVerification<'_, '_> {
    fn observe(
        &self,
        guard: usize,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
    ) -> io::Result<Proof> {
        self.verifier
            .observe(self.guard(guard)?, path, length, modified)
    }
    fn observe_current(
        &self,
        guard: usize,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
        evidence: &[u8],
    ) -> io::Result<Proof> {
        self.verifier
            .observe_acquired(self.guard(guard)?, path, length, modified, |stamp| {
                !evidence.is_empty()
                    && crate::inventory::differential::NativeEvidence::from_stamp(stamp)
                        .encode_occurrence()
                        == evidence
            })
    }
    fn hash(
        &mut self,
        guard: usize,
        path: &Path,
        proof: &Proof,
        progress: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<[u8; 32]> {
        let guard = self
            .guards
            .get(guard)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::other("Chosen source is unavailable"))?;
        self.verifier
            .hash_with_progress(guard, path, proof, progress)
    }
    fn validate(&self, guard: usize, path: &Path, proof: &Proof) -> io::Result<()> {
        self.verifier.validate(self.guard(guard)?, path, proof)
    }
    fn equal(
        &mut self,
        a: (usize, &Path, &Proof),
        b: (usize, &Path, &Proof),
        expected_hash: &[u8; 32],
        progress: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<bool> {
        let a_guard = self
            .guards
            .get(a.0)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::other("Chosen source is unavailable"))?;
        let b_guard = self
            .guards
            .get(b.0)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::other("Chosen source is unavailable"))?;
        self.verifier.equal_hashed_between_with_progress(
            (a_guard, a.1, a.2),
            (b_guard, b.1, b.2),
            expected_hash,
            progress,
        )
    }
    fn progress(&self) -> sortsize_core::duplicates::DuplicateProgress {
        self.verifier.progress()
    }
}
#[cfg(test)]
mod ab_tests;
#[cfg(test)]
mod tests;

/// Stable bounded pages over all verified groups, including hardlink aliases.
/// A cursor belongs to one disposable job; it never grants source access.
#[derive(Clone)]
pub struct DuplicateCursor {
    owner: PathBuf,
    group: u64,
    entry: u64,
    reference: Option<u8>,
    role: u8,
}
pub struct DuplicatePage {
    pub sources: Vec<SourceId>,
    /// Aligned with rows; never infer A/B/C from the shared inventory identity.
    pub sides: Vec<u8>,
    pub rows: Vec<Row>,
    pub labels: Vec<(u64, u64, bool)>,
    first_continues_group: bool,
    /// A residual file location may match members already represented by a
    /// folder pair. Reference their first folder without repeating its paths.
    pub folder_references: Vec<Option<u64>>,
    /// Number of distinct folder pairs represented by each reference, not copies.
    pub folder_reference_counts: Vec<u64>,
    /// Group-wide presence, including members shown through folder groups.
    pub has_reference: Vec<bool>,
    pub first: Option<DuplicateCursor>,
    pub last: Option<DuplicateCursor>,
    pub more: bool,
}
impl DuplicatePage {
    pub fn starts_group_at(&self, index: usize) -> bool {
        if index == 0 {
            !self.first_continues_group && !self.labels.is_empty()
        } else {
            self.labels[index - 1].0 != self.labels[index].0
        }
    }
}

impl DuplicateJob<'_, '_> {
    pub fn page(&self, cursor: Option<&DuplicateCursor>, reverse: bool) -> Result<DuplicatePage> {
        self.page_filtered(cursor, reverse, false, None)
    }
    /// File results outside the highest verified folder pairs. Reading this page
    /// only consults the leased disposable SQLite result, never a source path.
    pub fn file_page(
        &self,
        cursor: Option<&DuplicateCursor>,
        reverse: bool,
    ) -> Result<DuplicatePage> {
        self.page_filtered(cursor, reverse, true, None)
    }
    /// Stable review order: group, protected locations, then other locations.
    /// A change of protected side starts a new traversal.
    pub fn review_file_page(
        &self,
        cursor: Option<&DuplicateCursor>,
        reverse: bool,
    ) -> Result<DuplicatePage> {
        self.page_filtered(cursor, reverse, true, self.review_reference()?)
    }
    /// Number of file-content groups still represented outside folder pairs.
    pub fn file_group_count(&self) -> Result<u64> {
        if !self.mode.is_cross_source() {
            return Ok(self.summary.groups);
        }
        Ok(self.db.query_row(
            "SELECT count(DISTINCT c.group_id) FROM candidates c JOIN groups g ON g.id=c.group_id WHERE NOT EXISTS(SELECT 1 FROM folder_covered f WHERE f.candidate=c.id)",
            [], |r| r.get::<_, i64>(0).map(|n| n as u64),
        )?)
    }
    fn page_filtered(
        &self,
        cursor: Option<&DuplicateCursor>,
        reverse: bool,
        exclude_folders: bool,
        reference: Option<u8>,
    ) -> Result<DuplicatePage> {
        if !self.summary.verified {
            return Err(Error::Contract("unverified duplicate job"));
        }
        if cursor.is_some_and(|c| c.owner != self.owner || c.reference != reference) {
            return Err(Error::Contract("foreign duplicate cursor"));
        }
        let mut rows = Vec::new();
        let mut labels = Vec::new();
        let mut folder_references = Vec::new();
        let mut folder_reference_counts = Vec::new();
        let mut has_reference = Vec::new();
        let mut reference_group: Option<(u64, Option<u64>, u64, bool)> = None;
        let mut sources = Vec::new();
        let mut sides = Vec::new();
        let mut cursors = Vec::new();
        let mut used = 0usize;
        let mut publication_bytes = 128 * 1024;
        let mut more = false;
        let mut visit = |r: &rusqlite::Row<'_>| -> Result<bool> {
            let p = path(r.get(2)?)?;
            let bytes = p.capacity() * 2 + 256;
            let publication_row = p.capacity() * 4 + 4096 + std::mem::size_of::<Row>();
            if rows.len() == 200
                || used + bytes > 512 * 1024
                || (self.publication_memory.is_some()
                    && publication_bytes + publication_row > CURRENT_FOLDER_PUBLICATION_PAGE_BYTES)
            {
                if rows.is_empty() {
                    return Err(Error::Contract("duplicate row exceeds page budget"));
                }
                more = true;
                return Ok(false);
            }
            used += bytes;
            publication_bytes += publication_row;
            let group = number(r, 0)?;
            let entry = number(r, 1)?;
            let representative = number(r, 4)?;
            let (side, source, key) =
                self.provenance(r.get::<_, i64>(5)? as usize, EntryId(number(r, 6)?));
            rows.push(Row {
                key,
                path: p,
                kind: Kind::File,
                logical: Some(number(r, 3)?),
            });
            labels.push((group, representative, entry != representative));
            let (folder_reference, folder_count, reference_present) = if exclude_folders
                && self.mode.is_cross_source()
            {
                if let Some((_, first, count, present)) =
                    reference_group.filter(|(previous, _, _, _)| *previous == group)
                {
                    (first, count, present)
                } else {
                    let value = self.db.query_row(
                            "SELECT min(f.folder),count(DISTINCT f.folder),
                                    EXISTS(SELECT 1 FROM candidates p WHERE p.group_id=?1 AND p.side=?2)
                             FROM folder_covered f JOIN candidates covered ON covered.id=f.candidate
                             WHERE covered.group_id=?1",
                            params![unsigned(group), reference],
                            |r| Ok((opt_number(r, 0)?, r.get::<_, i64>(1)? as u64, r.get(2)?)),
                        )?;
                    reference_group = Some((group, value.0, value.1, value.2));
                    value
                }
            } else {
                (None, 0, false)
            };
            folder_references.push(folder_reference);
            folder_reference_counts.push(folder_count);
            has_reference.push(reference_present);
            sources.push(source);
            sides.push(side);
            cursors.push(DuplicateCursor {
                owner: self.owner.clone(),
                group,
                entry,
                reference,
                role: u8::from(reference.is_some_and(|reference| reference != side)),
            });
            Ok(true)
        };
        if self.rank_by_gain {
            self.visit_gain_order(cursor, reverse, &mut visit)?;
        } else {
            self.visit_id_order(cursor, reverse, exclude_folders, reference, &mut visit)?;
        }
        if reverse {
            rows.reverse();
            labels.reverse();
            folder_references.reverse();
            folder_reference_counts.reverse();
            has_reference.reverse();
            sources.reverse();
            sides.reverse();
            cursors.reverse();
        }
        let mut first_continues_group = false;
        if (cursor.is_some() || reverse)
            && let Some(first) = cursors.first()
        {
            // Seek one row back using the same visible order, including the
            // reference sort and exclusions used to build this page.
            let mut preceding = |row: &rusqlite::Row<'_>| {
                first_continues_group = number(row, 0)? == first.group;
                Ok(false)
            };
            if self.rank_by_gain {
                self.visit_gain_order(Some(first), true, &mut preceding)?;
            } else {
                self.visit_id_order(
                    Some(first),
                    true,
                    exclude_folders,
                    reference,
                    &mut preceding,
                )?;
            }
        }
        Ok(DuplicatePage {
            sources,
            sides,
            rows,
            labels,
            first_continues_group,
            folder_references,
            folder_reference_counts,
            has_reference,
            first: cursors.first().cloned(),
            last: cursors.last().cloned(),
            more,
        })
    }

    fn visit_gain_order(
        &self,
        cursor: Option<&DuplicateCursor>,
        reverse: bool,
        mut visit: impl FnMut(&rusqlite::Row<'_>) -> Result<bool>,
    ) -> Result<()> {
        // Separate seeks on by_gain and by_group avoid a sort of all the paths
        // within a large group. The caller stops after at most one page plus one.
        let (comparison, order) = if reverse { ("<", "DESC") } else { (">", "ASC") };
        let mut members = self.db.prepare(&format!(
            "SELECT c.group_id,c.id,c.path,g.size,c.representative,c.scope,c.entry
             FROM groups g JOIN candidates c INDEXED BY by_group ON c.group_id=g.id
             WHERE g.id=?1 AND c.id{comparison}?2 ORDER BY c.id {order}"
        ))?;
        let mut visit_group = |group: u64, boundary: &[u8]| -> Result<bool> {
            let mut members = members.query(params![unsigned(group), boundary])?;
            while let Some(member) = members.next()? {
                if !visit(member)? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        if let Some(cursor) = cursor
            && !visit_group(cursor.group, &unsigned(cursor.entry))?
        {
            return Ok(());
        }
        let group = cursor.map(|c| c.group);
        let mut groups = self.db.prepare(&format!(
            "SELECT id FROM groups INDEXED BY by_gain
             WHERE (gain_rank,id){comparison}(?1,?2) ORDER BY gain_rank {order},id {order}"
        ))?;
        let mut groups = groups.query(params![
            self.group_rank(group)?,
            unsigned(group.unwrap_or(0))
        ])?;
        let boundary: &[u8] = if reverse { &[u8::MAX; 9] } else { &[0; 8] };
        while let Some(group) = groups.next()? {
            if !visit_group(number(group, 0)?, boundary)? {
                break;
            }
        }
        Ok(())
    }

    fn visit_id_order(
        &self,
        cursor: Option<&DuplicateCursor>,
        reverse: bool,
        exclude_folders: bool,
        reference: Option<u8>,
        mut visit: impl FnMut(&rusqlite::Row<'_>) -> Result<bool>,
    ) -> Result<()> {
        let sql: String = if let Some(reference) = reference {
            // Each side seeks on by_side_group and emits at most one page. Merge
            // these bounded streams so even one huge group does not require
            // an unbounded sort to show the protected locations first.
            let (comparison, order) = if reverse { ("<", "DESC") } else { (">", "ASC") };
            let arm = |side| {
                let role = u8::from(side != reference);
                let entry_param = role + 2;
                format!(
                    "SELECT * FROM (
                     SELECT c.group_id,c.id,{role} AS role
                     FROM candidates c INDEXED BY by_side_group JOIN groups g ON c.group_id=g.id
                     WHERE (c.group_id,c.id){comparison}(?1,?{entry_param})
                       AND c.side={side}
                       AND NOT EXISTS(SELECT 1 FROM folder_covered f WHERE f.candidate=c.id)
                     ORDER BY c.group_id {order},c.id {order} LIMIT 201)"
                )
            };
            let arms = (0..self.mode.side_count())
                .map(arm)
                .collect::<Vec<_>>()
                .join(" UNION ALL ");
            format!(
                "WITH page AS ({arms} ORDER BY 1 {order},3 {order},2 {order} LIMIT 201)
                 SELECT p.group_id,p.id,c.path,g.size,c.representative,c.scope,c.entry
                 FROM page p JOIN candidates c ON c.id=p.id JOIN groups g ON g.id=p.group_id
                 ORDER BY p.group_id {order},p.role {order},p.id {order}"
            )
        } else if reverse && exclude_folders {
            "SELECT c.group_id,c.id,c.path,g.size,c.representative,c.scope,c.entry FROM candidates c JOIN groups g ON c.group_id=g.id WHERE (c.group_id,c.id)<(?1,?2) AND NOT EXISTS(SELECT 1 FROM folder_covered f WHERE f.candidate=c.id) ORDER BY c.group_id DESC,c.id DESC LIMIT 201".into()
        } else if exclude_folders {
            "SELECT c.group_id,c.id,c.path,g.size,c.representative,c.scope,c.entry FROM candidates c JOIN groups g ON c.group_id=g.id WHERE (c.group_id,c.id)>(?1,?2) AND NOT EXISTS(SELECT 1 FROM folder_covered f WHERE f.candidate=c.id) ORDER BY c.group_id,c.id LIMIT 201".into()
        } else if reverse {
            "SELECT c.group_id,c.id,c.path,g.size,c.representative,c.scope,c.entry FROM candidates c JOIN groups g ON c.group_id=g.id WHERE (c.group_id,c.id)<(?1,?2) ORDER BY c.group_id DESC,c.id DESC LIMIT 201".into()
        } else {
            "SELECT c.group_id,c.id,c.path,g.size,c.representative,c.scope,c.entry FROM candidates c JOIN groups g ON c.group_id=g.id WHERE (c.group_id,c.id)>(?1,?2) ORDER BY c.group_id,c.id LIMIT 201".into()
        };
        let (g, id) = cursor.map(|c| (c.group, c.entry)).unwrap_or((0, 0));
        let mut statement = self.db.prepare(&sql)?;
        let mut query = if reference.is_some() {
            let boundary = |role: u8| {
                cursor.map_or(0, |c| match role.cmp(&c.role) {
                    std::cmp::Ordering::Equal => c.entry,
                    std::cmp::Ordering::Less => u64::MAX,
                    std::cmp::Ordering::Greater => 0,
                })
            };
            statement.query(params![
                unsigned(g),
                unsigned(boundary(0)),
                unsigned(boundary(1))
            ])?
        } else {
            statement.query(params![unsigned(g), unsigned(id)])?
        };
        while let Some(row) = query.next()? {
            if !visit(row)? {
                break;
            }
        }
        Ok(())
    }
}
