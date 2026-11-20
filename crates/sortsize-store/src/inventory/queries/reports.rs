//! L09: full frozen-query statistics and a separate versioned JSONL export.
//! No source filesystem reads; the Query borrow retains the generation lease.
use super::*;
use sortsize_core::{
    NativePath,
    resources::{Category, Governor, Reservation},
};
use std::io::{BufWriter, Write};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub files: u64,
    pub known_bytes: u128,
    pub unknown_sizes: u64,
}
impl Counts {
    fn add(&mut self, size: Option<u64>) -> Result<()> {
        self.files = self
            .files
            .checked_add(1)
            .ok_or(Error::Contract("file count overflow"))?;
        if let Some(n) = size {
            self.known_bytes += u128::from(n);
        } else {
            self.unknown_sizes += 1;
        }
        Ok(())
    }
    fn subtract(&mut self, rhs: Self) {
        self.files -= rhs.files;
        self.known_bytes -= rhs.known_bytes;
        self.unknown_sizes -= rhs.unknown_sizes;
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"files":self.files.to_string(),"known_bytes":self.known_bytes.to_string(),"unknown_sizes":self.unknown_sizes.to_string()})
    }
}
pub struct ExtensionGroup {
    pub extension: NativePath,
    pub counts: Counts,
}
pub struct Statistics {
    pub inventory: InventoryId,
    pub source: SourceId,
    pub generation: GenerationId,
    pub query_counts: QueryCounts,
    pub totals: Counts,
    pub size_classes: [Counts; 6],
    pub extensions: Vec<ExtensionGroup>,
    pub others: Counts,
    pub extensions_byte_limited: bool,
    pub complete: bool,
    pub cancelled: bool,
    _memory: Reservation,
}
impl Statistics {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"inventory_statistics_schema":1,"inventory":self.inventory.0.to_string(),"source":self.source.0.to_string(),"generation":self.generation.0.to_string(),"query_matches":self.query_counts.matches.to_string(),"query_evaluated":self.query_counts.evaluated.to_string(),"query_gaps":{"cancelled":self.query_counts.cancelled,"unknown_sizes":self.query_counts.unknown_sizes.to_string(),"unknown_dates":self.query_counts.unknown_dates.to_string(),"native_text":self.query_counts.native_text.to_string(),"oversized_paths":self.query_counts.oversized_paths.to_string(),"unknown_kinds":self.query_counts.unknown_kinds.to_string()},"basis":"matched_regular_file_paths_logical_bytes","complete":self.complete,"cancelled":self.cancelled,"totals":self.totals.json(),"size_classes":self.size_classes.map(|c|c.json()),"extensions":self.extensions.iter().map(|g|serde_json::json!({"extension":g.extension,"counts":g.counts.json()})).collect::<Vec<_>>(),"others":self.others.json(),"extensions_byte_limited":self.extensions_byte_limited})
    }
}
#[derive(Debug)]
pub struct ExportSummary {
    pub records: u64,
    pub complete: bool,
    pub cancelled: bool,
    pub quota: bool,
}
#[derive(serde::Serialize)]
struct ExportHeader {
    inventory_export_schema: u8,
    record: &'static str,
    recorded_root: NativePath,
    root_supplied_by_caller: bool,
    inventory: String,
    source: String,
    generation: String,
    query_matches: String,
    query_complete: bool,
    native_paths: bool,
    basis: &'static str,
}
#[derive(serde::Serialize)]
struct ExportEntry {
    inventory_export_schema: u8,
    record: &'static str,
    entry: String,
    path: NativePath,
    kind: &'static str,
    logical_bytes: Option<String>,
}
fn group_counts(r: &rusqlite::Row<'_>) -> rusqlite::Result<Counts> {
    let raw: Vec<u8> = r.get(1)?;
    let bytes: [u8; 16] = raw.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Counts {
        files: number(r, 0)?,
        known_bytes: u128::from_be_bytes(bytes),
        unknown_sizes: number(r, 2)?,
    })
}
fn json_line(value: impl serde::Serialize) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
    out.push(b'\n');
    Ok(out)
}
impl Query<'_> {
    /// Groups spill to their own bounded database. All matches are streamed,
    /// including matches beyond every UI page or RAM retention cap.
    pub fn statistics(
        &self,
        scratch: &Path,
        max_db_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<Statistics> {
        if !self.finished {
            return Err(Error::Contract(
                "statistics require finished query evaluation",
            ));
        }
        if max_db_bytes < 64 * 1024 {
            return Err(Error::Contract("statistics disk budget too small"));
        }
        let memory = Governor::global()
            .reserve(Category::Queries, 1024 * 1024, None)
            .ok_or(Error::Contract("statistics memory budget"))?;
        let space = workspace::Workspace::derived(self._store, scratch, max_db_bytes)?;
        let vfs = workspace::vfs::Vfs::new(space.clone())?;
        let mut groups = Connection::open_with_flags_and_vfs(
            space.path().join("statistics.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            vfs.name(),
        )?;
        groups.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=FILE; PRAGMA cache_size=-512; CREATE TABLE groups(ext BLOB PRIMARY KEY,n BLOB NOT NULL,bytes BLOB NOT NULL,unknown BLOB NOT NULL) WITHOUT ROWID;")?;
        groups.pragma_update(
            None,
            "max_page_count",
            (max_db_bytes / 4096).min(u64::from(u32::MAX)) as u32,
        )?;
        let mut totals = Counts::default();
        let mut size_classes = [Counts::default(); 6];
        let mut cancelled = false;
        {
            let tx = groups.transaction()?;
            let mut lookup = tx.prepare("SELECT n,bytes,unknown FROM groups WHERE ext=?1")?;
            let mut put = tx.prepare("INSERT OR REPLACE INTO groups VALUES(?1,?2,?3,?4)")?;
            self.visit_matches(|r| {
                if cancel.is_cancelled() {
                    cancelled = true;
                    return Ok(false);
                }
                if r.kind != Kind::File {
                    return Ok(true);
                }
                let ext = native_path(Path::new(r.path.extension().unwrap_or_default())).encode();
                let logical = r.logical;
                let mut count = lookup
                    .query_row([&ext], group_counts)
                    .optional()?
                    .unwrap_or_default();
                count.add(logical)?;
                put.execute(params![
                    ext,
                    unsigned(count.files),
                    count.known_bytes.to_be_bytes().as_slice(),
                    unsigned(count.unknown_sizes)
                ])?;
                totals.add(logical)?;
                let class = match logical {
                    Some(0) => 0,
                    Some(1..=1023) => 1,
                    Some(1024..=1_048_575) => 2,
                    Some(1_048_576..=1_073_741_823) => 3,
                    Some(_) => 4,
                    None => 5,
                };
                size_classes[class].add(logical)?;
                Ok(true)
            })?;
            drop(put);
            drop(lookup);
            tx.commit()?;
        }
        let mut extensions = Vec::with_capacity(32);
        let mut others = totals;
        let mut projection_bytes = 0;
        let mut extensions_byte_limited = false;
        let mut q = groups.prepare(
            "SELECT n,bytes,unknown,ext FROM groups ORDER BY bytes DESC,n DESC,ext LIMIT 32",
        )?;
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? {
            let counts = group_counts(r)?;
            let raw: Vec<u8> = r.get(3)?;
            let bound = raw.len() * 40 + 1024;
            if projection_bytes + bound > 512 * 1024 {
                extensions_byte_limited = true;
                break;
            }
            projection_bytes += bound;
            let native = crate::scheduler::native_name(
                &NativeName::decode(&raw)
                    .map_err(|_| Error::Contract("invalid extension codec"))?,
            )?;
            others.subtract(counts);
            extensions.push(ExtensionGroup {
                extension: NativePath::from(Path::new(&native)),
                counts,
            });
        }
        cancelled |= cancel.is_cancelled();
        Ok(Statistics {
            inventory: self._store.inventory,
            source: self._store.source,
            generation: self.generation,
            query_counts: self.counts,
            totals,
            size_classes,
            extensions,
            others,
            extensions_byte_limited,
            complete: self.complete() && !cancelled,
            cancelled,
            _memory: memory,
        })
    }
    /// New schema; never replaces the historical CSV/TSV contracts. A quota or
    /// cancellation yields an explicit partial trailer. I/O failure publishes nothing.
    pub fn export_jsonl(
        &self,
        destination: &Path,
        max_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<ExportSummary> {
        if !self.finished {
            return Err(Error::Contract("export requires finished query evaluation"));
        }
        if max_bytes < 4096 {
            return Err(Error::Contract(
                "export budget needs 4096 bytes for its envelope",
            ));
        }
        let _memory = Governor::global()
            .reserve(Category::Queries, 1024 * 1024, None)
            .ok_or(Error::Contract("export memory budget"))?;
        let _disk = self._store.disk_admission(max_bytes)?;
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut stage = tempfile::NamedTempFile::new_in(parent)?;
        let mut summary = ExportSummary {
            records: 0,
            complete: false,
            cancelled: false,
            quota: false,
        };
        {
            let mut output = BufWriter::with_capacity(64 * 1024, stage.as_file_mut());
            let header = json_line(ExportHeader {
                inventory_export_schema: 1,
                record: "header",
                recorded_root: NativePath::from(self.root.as_path()),
                root_supplied_by_caller: true,
                inventory: self._store.inventory.0.to_string(),
                source: self._store.source.0.to_string(),
                generation: self.generation.0.to_string(),
                query_matches: self.counts.matches.to_string(),
                query_complete: self.complete(),
                native_paths: true,
                basis: "observed_entry_metadata",
            })?;
            let mut written = header.len() as u64;
            output.write_all(&header)?;
            drop(header);
            self.visit_matches(|r| {
                if cancel.is_cancelled() {
                    summary.cancelled = true;
                    return Ok(false);
                }
                let kind = match r.kind {
                    Kind::File => "file",
                    Kind::Directory => "directory",
                    Kind::Symlink => "symlink",
                    Kind::Special => "special",
                    _ => "unknown",
                };
                let row = json_line(ExportEntry {
                    inventory_export_schema: 1,
                    record: "entry",
                    entry: r.key.entry.0.to_string(),
                    path: NativePath::from(r.path.as_path()),
                    kind,
                    logical_bytes: r.logical.map(|n| n.to_string()),
                })?;
                if written
                    .saturating_add(row.len() as u64)
                    .saturating_add(2048)
                    > max_bytes
                {
                    summary.quota = true;
                    return Ok(false);
                }
                output.write_all(&row)?;
                written += row.len() as u64;
                summary.records += 1;
                Ok(true)
            })?;
            summary.cancelled |= cancel.is_cancelled();
            summary.complete = self.complete()
                && !summary.cancelled
                && !summary.quota
                && summary.records == self.counts.matches;
            let trailer = json_line(
                serde_json::json!({"inventory_export_schema":1,"record":"end","records":summary.records.to_string(),"matched_not_exported":(self.counts.matches-summary.records).to_string(),"coverage":if summary.complete{"complete"}else{"partial"},"cancelled":summary.cancelled,"quota":summary.quota,"query_evaluated":self.counts.evaluated.to_string(),"query_complete":self.complete(),"unknown_sizes":self.counts.unknown_sizes.to_string(),"unknown_dates":self.counts.unknown_dates.to_string(),"native_text_not_evaluable":self.counts.native_text.to_string(),"oversized_paths":self.counts.oversized_paths.to_string(),"unknown_kinds":self.counts.unknown_kinds.to_string()}),
            )?;
            if written.saturating_add(trailer.len() as u64) > max_bytes {
                return Err(Error::Contract("export trailer exceeds reserved envelope"));
            }
            output.write_all(&trailer)?;
            output.flush()?;
        }
        stage.as_file().sync_all()?;
        stage
            .persist_noclobber(destination)
            .map_err(|e| Error::Io(e.error))?;
        Ok(summary)
    }
}
