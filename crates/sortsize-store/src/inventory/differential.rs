//! Conservative metadata comparison, materialized on local disk in bounded
//! quanta. Parent/name joins do not assume stable EntryIds across generations.
use super::lifecycle::{Lease, Measurement};
use super::queries::workspace::{Workspace, vfs::Vfs};
use super::*;
use sortsize_core::{
    CancellationToken, LocalFileId,
    resources::{Category, Governor, Reservation},
};
use std::{path::PathBuf, sync::Arc};

mod root_sizes;
pub use root_sizes::{RootChange, RootSize, largest_root_entries};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeEvidence {
    pub identity: Option<LocalFileId>,
    pub created: Option<(i64, u32)>,
}
impl NativeEvidence {
    pub fn from_stamp(s: &sortsize_core::acquisition::Stamp) -> Self {
        let created = s.created.and_then(|t| {
            let ns = match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => i128::try_from(d.as_nanos()).ok()?,
                Err(e) => -i128::try_from(e.duration().as_nanos()).ok()?,
            };
            Some((
                i64::try_from(ns.div_euclid(1_000_000_000)).ok()?,
                ns.rem_euclid(1_000_000_000) as u32,
            ))
        });
        Self {
            identity: s.identity,
            created,
        }
    }
    /// Only a qualified identity + creation proof can correlate generations.
    pub(crate) fn encode(&self) -> Vec<u8> {
        if self.created.is_none_or(|(_, n)| n >= 1_000_000_000) {
            return vec![];
        }
        self.encode_occurrence()
    }
    /// Within one live job, retain the native identity even on filesystems
    /// that supply no creation time. An empty token means no usable identity.
    pub(crate) fn encode_occurrence(&self) -> Vec<u8> {
        let mut b = vec![1];
        match self.identity {
            Some(LocalFileId::Unix { device, inode }) => {
                b.push(0);
                b.extend(device.to_be_bytes());
                b.extend(inode.to_be_bytes());
            }
            Some(LocalFileId::Windows {
                volume_serial,
                file_id,
            }) => {
                b.push(1);
                b.extend(volume_serial.to_be_bytes());
                b.extend(file_id);
            }
            Some(LocalFileId::WindowsLegacy { volume, index }) if index != 0 => {
                b.push(2);
                b.extend(volume.to_be_bytes());
                b.extend(index.to_be_bytes());
            }
            _ => return vec![],
        }
        if let Some((s, n)) = self.created.filter(|(_, n)| *n < 1_000_000_000) {
            b.extend(s.to_be_bytes());
            b.extend(n.to_be_bytes());
        }
        b
    }
}
impl Store {
    pub fn enable_evidence(&mut self) -> Result<()> {
        self.enable_lifecycle()?;
        let version: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if (4..=6).contains(&version) {
            return Ok(());
        }
        let _disk = self.disk_admission(8 * 1024 * 1024)?;
        let tx = self.db.transaction()?;
        tx.execute_batch("CREATE TABLE entry_evidence(generation BLOB NOT NULL,id BLOB NOT NULL,proof BLOB,PRIMARY KEY(generation,id),FOREIGN KEY(generation,id) REFERENCES entries(generation,id)) STRICT, WITHOUT ROWID; PRAGMA user_version=4;")?;
        tx.commit()?;
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Appeared = 0,
    NewlyObserved = 1,
    Removed = 2,
    Replaced = 3,
    FieldsChanged = 4,
    FieldsUnchanged = 5,
    Unverified = 6,
    ProbableMove = 7,
}
fn change(n: i64) -> Result<Change> {
    Ok(match n {
        0 => Change::Appeared,
        1 => Change::NewlyObserved,
        2 => Change::Removed,
        3 => Change::Replaced,
        4 => Change::FieldsChanged,
        5 => Change::FieldsUnchanged,
        6 => Change::Unverified,
        7 => Change::ProbableMove,
        _ => return Err(Error::Contract("diff event kind")),
    })
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct Summary {
    pub old_logical: Option<u64>,
    pub new_logical: Option<u64>,
    pub old_unknown: u64,
    pub new_unknown: u64,
    /// Decimal signed value, only for complete comparable scopes. No f64/u64 truncation.
    pub comparable_delta: Option<String>,
    pub compatible: bool,
    pub finished: bool,
    pub cancelled: bool,
    pub counts: [u64; 8],
    pub visited: u64,
}
#[derive(Debug, Clone)]
pub struct Event {
    pub sequence: u64,
    pub change: Change,
    pub old: Option<PathBuf>,
    pub new: Option<PathBuf>,
    pub old_entry: Option<EntryId>,
    pub new_entry: Option<EntryId>,
}
#[derive(Clone, Debug)]
pub struct Cursor {
    owner: PathBuf,
    sequence: u64,
    priority: Option<i64>,
}
pub struct Page {
    pub rows: Vec<Event>,
    pub first: Option<Cursor>,
    pub next: Option<Cursor>,
    pub more: bool,
    pub accounted_bytes: usize,
    _memory: Reservation,
}
pub struct Diff<'a> {
    old: Lease<'a>,
    new: Lease<'a>,
    db: Connection,
    _vfs: Vfs,
    space: Arc<Workspace>,
    owner: PathBuf,
    _memory: Reservation,
    summary: Summary,
    changes_first: bool,
}
#[derive(Clone)]
struct Item {
    id: u64,
    name: Vec<u8>,
    kind: i64,
    logical: Option<u64>,
    allocated: Option<u64>,
    modified: Option<(i64, u32)>,
    known: bool,
    attempt: u64,
    proof: Option<Vec<u8>>,
}
#[derive(Clone, Copy, Default)]
struct Directory {
    current: bool,
    complete: bool,
    attempt: Option<u64>,
}
fn directory(s: &Store, g: GenerationId, id: Option<u64>, inherited: bool) -> Result<Directory> {
    let Some(id) = id else {
        return Ok(Directory::default());
    };
    let epoch = context(s.acquisition_context(g)?);
    type DirectoryRow = (Option<Vec<u8>>, Option<u64>, u64, u64, i64, bool);
    let row:Option<DirectoryRow>=s.db.query_row("SELECT t.key,t.attempt,t.parent_attempt,e.attempt,t.listing,t.reconciled FROM tasks t JOIN entries e ON e.generation=t.generation AND e.id=t.directory WHERE t.generation=?1 AND t.directory=?2 AND t.kind=0 AND e.present=1 AND e.kind=2",params![unsigned(g.0),unsigned(id)],|r|Ok((r.get(0)?,opt_number(r,1)?,number(r,2)?,number(r,3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((key, attempt, parent, seen, listing, reconciled)) = row else {
        return Ok(Directory::default());
    };
    let current = inherited && parent == seen && key.is_some_and(|k| k.starts_with(&epoch));
    Ok(Directory {
        current,
        attempt,
        complete: current && listing == 1 && reconciled,
    })
}
struct Batch {
    rows: Vec<Item>,
    more: bool,
}
fn items(s: &Store, g: GenerationId, parent: Option<u64>, after: &[u8]) -> Result<Batch> {
    let Some(parent) = parent else {
        return Ok(Batch {
            rows: vec![],
            more: false,
        });
    };
    let evidence =
        s.db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= 4;
    let sql = if evidence {
        "SELECT e.id,substr(e.name,1,262145),e.kind,e.logical,e.allocated,e.modified_s,e.modified_ns,e.metadata_known,e.attempt,CASE WHEN length(p.proof)<=64 THEN p.proof ELSE NULL END FROM entries e LEFT JOIN entry_evidence p ON p.generation=e.generation AND p.id=e.id WHERE e.generation=?1 AND e.parent=?2 AND e.present=1 AND e.name>?3 ORDER BY e.name LIMIT 129"
    } else {
        "SELECT id,substr(name,1,262145),kind,logical,allocated,modified_s,modified_ns,metadata_known,attempt,NULL FROM entries WHERE generation=?1 AND parent=?2 AND present=1 AND name>?3 ORDER BY name LIMIT 129"
    };
    let mut stmt = s.db.prepare(sql)?;
    let mut rows = stmt.query(params![unsigned(g.0), unsigned(parent), after])?;
    let mut result = Vec::with_capacity(128);
    let mut bytes = 0;
    let mut more = false;
    while let Some(r) = rows.next()? {
        let raw = r.get_ref(1)?;
        let name = match raw {
            rusqlite::types::ValueRef::Blob(b) => b,
            _ => return Err(Error::Contract("invalid native name type")),
        };
        if result.len() == 128 || bytes + name.len() + 256 > 256 * 1024 {
            if result.is_empty() {
                return Err(Error::Contract("diff native name exceeds batch budget"));
            }
            more = true;
            break;
        }
        bytes += name.len() + 256;
        result.push(Item {
            id: number(r, 0)?,
            name: name.to_vec(),
            kind: r.get(2)?,
            logical: opt_number(r, 3)?,
            allocated: opt_number(r, 4)?,
            modified: r.get::<_, Option<i64>>(5)?.zip(r.get(6)?),
            known: r.get(7)?,
            attempt: number(r, 8)?,
            proof: r.get::<_, Option<Vec<u8>>>(9)?.filter(|p| !p.is_empty()),
        });
    }
    Ok(Batch { rows: result, more })
}

fn continuity(old: &Lease<'_>, new: &Lease<'_>) -> Result<bool> {
    let a = &old.measurement;
    let b = &new.measurement;
    if !super::catalogue::same_recorded_scope(a.context, &a.root, b.context, &b.root) {
        return Ok(false);
    }
    let proof = |lease: &Lease<'_>| -> Result<Option<Vec<u8>>> {
        lease
            .store
            .recorded_recovery_guard(lease.measurement.context.generation)
    };
    let a = proof(old)?;
    let b = proof(new)?;
    Ok(a.as_ref().is_some_and(|proof| !proof.is_empty()) && a == b)
}
impl<'a> Diff<'a> {
    /// No source filesystem metadata, owner/capacity lookup, canonicalization,
    /// Guard or source IO. Only the selected local scratch is validated.
    pub fn start(old: Lease<'a>, new: Lease<'a>, scratch: &Path, max_bytes: u64) -> Result<Self> {
        if max_bytes < 1024 * 1024 {
            return Err(Error::Contract("diff disk budget too small"));
        }
        let scratch = scratch.canonicalize()?;
        if sortsize_core::acquisition::contains_path(new.store.directory(), &scratch) {
            return Err(Error::Contract("diff scratch is inside new authority"));
        }
        let compatible = continuity(&old, &new)?;
        let governor = Governor::global();
        governor.refresh_native();
        let memory = governor
            .reserve(Category::Queries, 2 * 1024 * 1024, None)
            .ok_or(Error::Contract("diff working memory admission"))?;
        let limit = max_bytes
            .checked_mul(2)
            .and_then(|n| n.checked_add(8 * 1024 * 1024))
            .ok_or(Error::Contract("diff disk overflow"))?;
        let space = Workspace::derived(old.store, &scratch, limit)?;
        let vfs = Vfs::new(space.clone())?;
        let owner = space.path().join("diff.db");
        let db = Connection::open_with_flags_and_vfs(
            &owner,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            vfs.name(),
        )?;
        db.execute_batch("PRAGMA page_size=4096;PRAGMA journal_mode=WAL;PRAGMA synchronous=NORMAL;PRAGMA temp_store=FILE;PRAGMA cache_size=-512;PRAGMA mmap_size=0;PRAGMA wal_autocheckpoint=256;
          CREATE TABLE work(id INTEGER PRIMARY KEY,old BLOB,new BLOB,oc INTEGER NOT NULL,nc INTEGER NOT NULL,oa INTEGER NOT NULL,na INTEGER NOT NULL,cursor BLOB NOT NULL,done INTEGER NOT NULL DEFAULT 0);
          CREATE INDEX pending_work ON work(done,id);
          INSERT INTO work(old,new,oc,nc,oa,na,cursor) VALUES(x'0000000000000000',x'0000000000000000',1,1,0,0,x'');
          CREATE TABLE events(id INTEGER PRIMARY KEY,class INTEGER NOT NULL,old BLOB,new BLOB,suppressed INTEGER NOT NULL DEFAULT 0,
            priority INTEGER GENERATED ALWAYS AS (CASE WHEN class=5 THEN 2 WHEN class IN (1,6) THEN 1 ELSE 0 END) VIRTUAL);
          CREATE TABLE identities(proof BLOB NOT NULL,side INTEGER NOT NULL,entry BLOB NOT NULL,event INTEGER NOT NULL,PRIMARY KEY(proof,side,entry)) WITHOUT ROWID;")?;
        db.pragma_update(
            None,
            "max_page_count",
            (max_bytes / 4096).min(i64::MAX as u64) as i64,
        )?;
        let a = &old.measurement;
        let b = &new.measurement;
        let delta = if compatible
            && a.coverage == ScopeCoverage::Complete
            && b.coverage == ScopeCoverage::Complete
        {
            a.logical
                .zip(b.logical)
                .map(|(a, b)| (i128::from(b) - i128::from(a)).to_string())
        } else {
            None
        };
        let summary = Summary {
            old_logical: a.logical,
            new_logical: b.logical,
            old_unknown: a.unknown,
            new_unknown: b.unknown,
            comparable_delta: delta,
            compatible,
            finished: false,
            cancelled: false,
            counts: [0; 8],
            visited: 0,
        };
        Ok(Self {
            old,
            new,
            db,
            _vfs: vfs,
            space,
            owner,
            _memory: memory,
            summary,
            changes_first: false,
        })
    }
    pub fn summary(&self) -> &Summary {
        &self.summary
    }
    pub fn temporary_usage(&self) -> queries::TemporaryUsage {
        self.space.usage()
    }
    pub fn measurements(&self) -> (&Measurement, &Measurement) {
        (&self.old.measurement, &self.new.measurement)
    }
    /// At most 128 rows from each side and one short destination transaction.
    /// Every stored entry is visited once; B-tree joins cost logarithmic seeks.
    pub fn advance(&mut self, cancel: &CancellationToken) -> Result<bool> {
        if self.summary.finished {
            return Ok(true);
        }
        if cancel.is_cancelled() {
            self.summary.cancelled = true;
            self.summary.comparable_delta = None;
            return Ok(true);
        }
        type Work = (
            i64,
            Option<u64>,
            Option<u64>,
            bool,
            bool,
            bool,
            bool,
            Vec<u8>,
        );
        let work: Option<Work> = self
            .db
            .query_row(
                "SELECT id,old,new,oc,nc,oa,na,cursor FROM work WHERE done=0 ORDER BY id LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        opt_number(r, 1)?,
                        opt_number(r, 2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((work, old, new, oc, nc, oa, na, cursor)) = work else {
            let token = cancel.clone();
            self.db
                .progress_handler(4096, Some(move || token.is_cancelled()))?;
            let result = self.correlate();
            self.db.progress_handler(0, None::<fn() -> bool>)?;
            if cancel.is_cancelled() {
                self.summary.cancelled = true;
                self.summary.comparable_delta = None;
                return Ok(true);
            }
            result?;
            self.summary.finished = true;
            return Ok(true);
        };
        let og = self.old.measurement.context.generation;
        let ng = self.new.measurement.context.generation;
        let od = directory(self.old.store, og, old, oc)?;
        let nd = directory(self.new.store, ng, new, nc)?;
        let a = items(self.old.store, og, old, &cursor)?;
        let b = items(self.new.store, ng, new, &cursor)?;
        let (am, bm) = (a.more, b.more);
        let (a, b) = (a.rows, b.rows);
        let mut i = 0;
        let mut j = 0;
        let mut last = cursor;
        let mut counts = self.summary.counts;
        let mut visited = self.summary.visited;
        let tx = self.db.transaction()?;
        while i < a.len() || j < b.len() {
            // A full batch can hide the next key. Stop before comparing it with
            // the other side, and fetch from the shared key on the next quantum.
            if i == a.len() && am || j == b.len() && bm {
                break;
            }
            let order = match (a.get(i), b.get(j)) {
                (Some(a), Some(b)) => a.name.cmp(&b.name),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => break,
            };
            let (left, right) = match order {
                std::cmp::Ordering::Less => {
                    let x = &a[i];
                    i += 1;
                    (Some(x), None)
                }
                std::cmp::Ordering::Greater => {
                    let x = &b[j];
                    j += 1;
                    (None, Some(x))
                }
                std::cmp::Ordering::Equal => {
                    let x = &a[i];
                    let y = &b[j];
                    i += 1;
                    j += 1;
                    (Some(x), Some(y))
                }
            };
            last = left.or(right).unwrap().name.clone();
            visited += 1;
            let lv = left.is_some_and(|x| od.current && Some(x.attempt) == od.attempt && x.known);
            let rv = right.is_some_and(|x| nd.current && Some(x.attempt) == nd.attempt && x.known);
            let old_absent = oa || od.complete;
            let new_absent = na || nd.complete;
            let class = if !self.summary.compatible {
                Change::Unverified
            } else {
                match (left, right) {
                    (Some(a), Some(b)) if lv && rv => {
                        if a.kind != b.kind
                            || a.proof
                                .as_ref()
                                .zip(b.proof.as_ref())
                                .is_some_and(|(a, b)| a != b)
                        {
                            Change::Replaced
                        } else if a.modified.is_none()
                            || b.modified.is_none()
                            || (a.kind == 1 && (a.logical.is_none() || b.logical.is_none()))
                        {
                            Change::Unverified
                        } else if a.logical.zip(b.logical).is_some_and(|(a, b)| a != b)
                            || a.allocated.zip(b.allocated).is_some_and(|(a, b)| a != b)
                            || a.modified != b.modified
                        {
                            Change::FieldsChanged
                        } else {
                            Change::FieldsUnchanged
                        }
                    }
                    (Some(_), None) if lv && new_absent => Change::Removed,
                    (None, Some(_)) if rv && old_absent => Change::Appeared,
                    (None, Some(_)) if rv => Change::NewlyObserved,
                    _ => Change::Unverified,
                }
            };
            counts[class as usize] += 1;
            tx.execute(
                "INSERT INTO events(class,old,new) VALUES(?1,?2,?3)",
                params![
                    class as i64,
                    left.map(|x| unsigned(x.id)),
                    right.map(|x| unsigned(x.id))
                ],
            )?;
            let event = tx.last_insert_rowid();
            for (side, row, valid) in [(0, left, lv), (1, right, rv)] {
                if valid
                    && let Some(row) = row
                    && let Some(proof) = &row.proof
                {
                    tx.execute(
                        "INSERT INTO identities VALUES(?1,?2,?3,?4)",
                        params![proof, side, unsigned(row.id), event],
                    )?;
                }
            }
            let ld = left.filter(|x| x.kind == 2);
            let rd = right.filter(|x| x.kind == 2);
            if ld.is_some() || rd.is_some() {
                tx.execute(
                    "INSERT INTO work(old,new,oc,nc,oa,na,cursor) VALUES(?1,?2,?3,?4,?5,?6,x'')",
                    params![
                        ld.map(|x| unsigned(x.id)),
                        rd.map(|x| unsigned(x.id)),
                        lv,
                        rv,
                        ld.is_none() && (old_absent || left.is_some_and(|x| x.kind != 2 && lv)),
                        rd.is_none() && (new_absent || right.is_some_and(|x| x.kind != 2 && rv))
                    ],
                )?;
            }
        }
        tx.execute(
            "UPDATE work SET cursor=?2,done=?3 WHERE id=?1",
            params![work, last, i == a.len() && !am && j == b.len() && !bm],
        )?;
        tx.commit()?;
        self.summary.counts = counts;
        self.summary.visited = visited;
        Ok(false)
    }
    fn correlate(&mut self) -> Result<()> {
        // Complete identity/creation proof, one path on each side. A hardlink
        // set never picks an arbitrary representative or invents a rename.
        let tx = self.db.transaction()?;
        tx.execute_batch("CREATE TABLE moves AS SELECT min(CASE WHEN side=0 THEN event END) old_event,min(CASE WHEN side=1 THEN event END) new_event FROM identities GROUP BY proof HAVING sum(side=0)=1 AND sum(side=1)=1;
          DELETE FROM moves WHERE (SELECT class FROM events WHERE id=old_event)!=2 OR (SELECT class FROM events WHERE id=new_event)!=0;
          UPDATE events SET class=7,new=(SELECT n.new FROM moves m JOIN events n ON n.id=m.new_event WHERE m.old_event=events.id) WHERE id IN (SELECT old_event FROM moves);
          UPDATE events SET suppressed=1 WHERE id IN (SELECT new_event FROM moves);")?;
        let moved: u64 =
            tx.query_row("SELECT count(*) FROM moves", [], |r| r.get::<_, i64>(0))? as u64;
        tx.commit()?;
        self.summary.counts[0] -= moved;
        self.summary.counts[2] -= moved;
        self.summary.counts[7] += moved;
        Ok(())
    }
    /// Put changes first, then uncertain observations, then unchanged fields.
    /// Ordinary traversal and its event sequence remain the default for exports.
    pub fn prioritize_changes(&mut self, cancel: &CancellationToken) -> Result<()> {
        if !self.summary.finished || cancel.is_cancelled() {
            return Err(Error::Contract("diff not ready for display order"));
        }
        let token = cancel.clone();
        self.db
            .progress_handler(4096, Some(move || token.is_cancelled()))?;
        let result = self.db.execute_batch(
            "CREATE INDEX IF NOT EXISTS events_display ON events(priority,id) WHERE suppressed=0;",
        );
        self.db.progress_handler(0, None::<fn() -> bool>)?;
        result?;
        self.changes_first = true;
        Ok(())
    }
    pub fn page(&self, after: Option<&Cursor>, reverse: bool) -> Result<Page> {
        if !self.summary.finished {
            return Err(Error::Contract("diff not finished"));
        }
        if after
            .is_some_and(|c| c.owner != self.owner || c.priority.is_some() != self.changes_first)
        {
            return Err(Error::Contract("foreign diff cursor"));
        }
        let memory = Governor::global()
            .reserve(Category::Publications, 512 * 1024, None)
            .ok_or(Error::Contract("diff publication budget"))?;
        let sql = match (self.changes_first, reverse) {
            (true, false) => {
                "SELECT id,class,old,new,priority FROM events WHERE suppressed=0 AND (priority,id)>(?2,?1) ORDER BY priority,id LIMIT 201"
            }
            (true, true) => {
                "SELECT id,class,old,new,priority FROM events WHERE suppressed=0 AND (priority,id)<(?2,?1) ORDER BY priority DESC,id DESC LIMIT 201"
            }
            (false, false) => {
                "SELECT id,class,old,new,NULL FROM events WHERE suppressed=0 AND id>?1 ORDER BY id LIMIT 201"
            }
            (false, true) => {
                "SELECT id,class,old,new,NULL FROM events WHERE suppressed=0 AND id<?1 ORDER BY id DESC LIMIT 201"
            }
        };
        let mut stmt = self.db.prepare(sql)?;
        let sequence = after.map_or(if reverse { i64::MAX } else { 0 }, |c| c.sequence as i64);
        let mut rows = if self.changes_first {
            let priority = after
                .and_then(|c| c.priority)
                .unwrap_or(if reverse { 2 } else { 0 });
            stmt.query(params![sequence, priority])?
        } else {
            stmt.query([sequence])?
        };
        let mut out = Vec::with_capacity(200);
        let mut bytes = 32 * 1024;
        let mut more = false;
        let mut first = None;
        let mut next = None;
        while let Some(row) = rows.next()? {
            if out.len() == 200 {
                more = true;
                break;
            }
            let old = opt_number(row, 2)?.map(EntryId);
            let new = opt_number(row, 3)?.map(EntryId);
            let old_path = old
                .map(|id| {
                    self.old.store.recorded_path(
                        self.old.measurement.context.generation,
                        id,
                        &self.old.measurement.root,
                    )
                })
                .transpose()?;
            let new_path = new
                .map(|id| {
                    self.new.store.recorded_path(
                        self.new.measurement.context.generation,
                        id,
                        &self.new.measurement.root,
                    )
                })
                .transpose()?;
            let cost = old_path.as_ref().map_or(0, |p| p.capacity())
                + new_path.as_ref().map_or(0, |p| p.capacity())
                + 256;
            if bytes + cost > 512 * 1024 {
                more = true;
                break;
            }
            bytes += cost;
            let sequence = row.get::<_, i64>(0)? as u64;
            let cursor = Cursor {
                owner: self.owner.clone(),
                sequence,
                priority: row.get(4)?,
            };
            if first.is_none() {
                first = Some(cursor.clone());
            }
            next = Some(cursor);
            out.push(Event {
                sequence,
                change: change(row.get(1)?)?,
                old: old_path,
                new: new_path,
                old_entry: old,
                new_entry: new,
            });
        }
        if reverse {
            out.reverse();
            std::mem::swap(&mut first, &mut next);
        }
        Ok(Page {
            rows: out,
            first,
            next,
            more,
            accounted_bytes: bytes,
            _memory: memory,
        })
    }
}
impl Drop for Diff<'_> {
    fn drop(&mut self) {
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}
#[cfg(test)]
mod tests;
