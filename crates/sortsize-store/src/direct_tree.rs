//! Disposable direct tree. The append-only spool retains native observations
//! for navigation and certified export. The index contains compact links and
//! directory totals. No query or database is involved in accepting a batch.
mod arena;
mod metrics;
mod seen;
pub(crate) mod spool;
use seen::SeenDisk;
pub mod query;
pub(crate) mod storage;
use arena::Index;
use sortsize_core::resources::{Category, Governor, Reservation};
pub use storage::Replay;
use storage::{DiskBudget, Spool};

use crate::{
    codec::NativeName,
    direct_save::ReplayBatch,
    inventory::{Error, Kind, Result, folder_totals::FolderMeasure},
    scheduler::{
        direct::{ObservationBatch, ObservedDirectory},
        native_name,
    },
};
use sortsize_core::{
    MetadataValue, ValueAccuracy,
    acquisition::NodeProperties,
    inventory_contract::{EntryId, ListingEnd, ScopeCoverage},
};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, hash_map::RandomState},
    fs::File,
    hash::BuildHasher,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const NONE: u32 = u32::MAX;
const INDEX_BLOCK: usize = 8 * 1024;
const TOP_FILES: usize = 100;
type TopHeap = BinaryHeap<Reverse<(u64, Reverse<u32>)>>;

fn index_limit() -> Error {
    Error::Contract("direct analysis memory quota reached")
}

#[derive(Clone, Copy)]
struct Node {
    record: u64,
    parent: u32,
    directory: u32,
    previous: u32,
    next: u32,
    same_hash: u32,
}

#[derive(Default)]
struct Seen {
    heads: HashMap<u64, u32>,
    rows: usize,
    charges: Vec<Reservation>,
    disk: Option<SeenDisk>,
}

#[derive(Clone, Copy)]
struct Directory {
    first: u32,
    last: u32,
    first_folder: u32,
    last_folder: u32,
    previous_folder: u32,
    next_folder: u32,
    sequence: u64,
    ended: bool,
    incomplete: bool,
    pending: u64,
    incomplete_descendants: u64,
    subtotal: Subtotal,
}

impl Directory {
    fn new(previous_folder: u32) -> Self {
        Self {
            first: NONE,
            last: NONE,
            first_folder: NONE,
            last_folder: NONE,
            previous_folder,
            next_folder: NONE,
            sequence: 0,
            ended: false,
            incomplete: false,
            pending: 1,
            incomplete_descendants: 0,
            subtotal: Subtotal::default(),
        }
    }
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Subtotal {
    logical: u128,
    allocated: u128,
    files: u64,
    directories: u64,
    unknown: u64,
    allocation_unknown: u64,
    known_allocations: u64,
    allocation_lower_bound: bool,
}

impl Subtotal {
    pub(crate) fn observe(
        &mut self,
        observation: &crate::inventory::Observation,
        p: &NodeProperties,
    ) {
        self.unknown += u64::from(
            !observation.metadata_known
                || observation.kind == Kind::Unknown
                || (observation.kind == Kind::File && observation.logical.is_none()),
        );
        match observation.kind {
            Kind::Directory => self.directories += 1,
            Kind::File => {
                self.files += 1;
                self.logical += u128::from(observation.logical.unwrap_or(0));
                if let Some(size) = observation.allocated {
                    self.allocated += u128::from(size);
                    self.known_allocations += 1;
                    self.allocation_lower_bound |= matches!(
                        p.allocated,
                        MetadataValue::Known {
                            accuracy: ValueAccuracy::LowerBound,
                            ..
                        }
                    );
                } else {
                    self.allocation_unknown += 1;
                }
            }
            Kind::Unknown => {
                self.allocation_unknown += 1;
            }
            Kind::Symlink | Kind::Special => {}
        }
    }
    fn add(&mut self, delta: &Self) {
        self.logical += delta.logical;
        self.allocated += delta.allocated;
        self.files += delta.files;
        self.directories += delta.directories;
        self.unknown += delta.unknown;
        self.allocation_unknown += delta.allocation_unknown;
        self.known_allocations += delta.known_allocations;
        self.allocation_lower_bound |= delta.allocation_lower_bound;
    }
    pub(crate) fn measure(&self) -> FolderMeasure {
        FolderMeasure {
            logical: self.logical.try_into().ok(),
            unknown: self.unknown,
            allocated: if self.known_allocations == 0 && self.allocation_unknown != 0 {
                None
            } else {
                self.allocated.try_into().ok()
            },
            allocation_unknown: self.allocation_unknown,
            allocation_lower_bound: self.allocation_lower_bound
                || (self.known_allocations != 0 && self.allocation_unknown != 0),
            files: self.files,
            directories: self.directories,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DirectSummary {
    pub measure: FolderMeasure,
    pub coverage: ScopeCoverage,
    pub observations: u64,
    pub entries: u64,
    pub errors: u64,
    pub denied: u64,
    pub pending_directories: u64,
    pub incomplete_directories: u64,
    pub detached_directories: u64,
    pub duplicate_observations: u64,
    pub temporary_bytes: u64,
}

pub struct DirectRow {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub path: PathBuf,
    pub kind: Kind,
    pub logical: Option<u64>,
    pub allocated: Option<u64>,
    pub modified: Option<(i64, u32)>,
    pub properties: NodeProperties,
}

pub struct DirectPage {
    pub rows: Vec<DirectRow>,
    pub more: bool,
    pub next: Option<EntryId>,
}

struct Pending {
    row: usize,
    id: u32,
    name: Vec<u8>,
    hash: u64,
    same_hash: u32,
    position: u64,
}

pub struct DirectTree {
    root: PathBuf,
    spool: Spool,
    nodes: Index<Node>,
    directories: Index<Directory>,
    hashes: RandomState,
    active_names: usize,
    seen: HashMap<u32, Seen>,
    governor: Governor,
    disk: DiskBudget,
    _working: Reservation,
    scratch: PathBuf,
    top: TopHeap,
    rollback_top: Option<TopHeap>,
    revision: u64,
    observations: u64,
    errors: u64,
    denied: u64,
    detached: u64,
    duplicates: u64,
    finished: bool,
    failed: bool,
    write: spool::Encoder,
    metrics: Option<Box<metrics::Metrics>>,
}

impl DirectTree {
    pub fn new(root: PathBuf, scratch: &Path, disk_limit: u64) -> Result<Self> {
        Self::with_governor(root, scratch, disk_limit, Governor::global())
    }

    pub fn with_governor(
        root: PathBuf,
        scratch: &Path,
        disk_limit: u64,
        governor: Governor,
    ) -> Result<Self> {
        let disk = DiskBudget::with_governor(disk_limit, governor.clone());
        let working = governor
            .reserve(Category::Transport, (spool::MAX_BATCH * 4) as u64, None)
            .ok_or_else(index_limit)?;
        let mut nodes = Index::new(scratch, disk.clone(), governor.clone())?;
        nodes.reserve(1)?;
        nodes.push(Node {
            record: 0,
            parent: NONE,
            directory: 0,
            previous: NONE,
            next: NONE,
            same_hash: NONE,
        })?;
        let mut directories = Index::new(scratch, disk.clone(), governor.clone())?;
        directories.reserve(1)?;
        directories.push(Directory::new(NONE))?;
        Ok(Self {
            root,
            spool: Spool::new(scratch, disk.clone(), governor.clone())?,
            governor,
            disk,
            _working: working,
            scratch: scratch.to_owned(),
            seen: HashMap::new(),
            nodes,
            directories,
            hashes: RandomState::new(),
            active_names: 0,
            top: BinaryHeap::with_capacity(TOP_FILES),
            rollback_top: None,
            revision: 0,
            observations: 0,
            errors: 0,
            denied: 0,
            detached: 0,
            duplicates: 0,
            finished: false,
            failed: false,
            write: spool::Encoder(Vec::with_capacity(spool::MAX_BATCH + 4)),
            metrics: crate::scan_metrics::enabled().then(Box::default),
        })
    }

    pub fn accept(&mut self, batch: ObservationBatch<'_>) -> Result<Vec<ObservedDirectory>> {
        if self.failed {
            return Err(Error::Contract("direct analysis already failed"));
        }
        // Keep diagnostics outside the mutable tree borrow; restore on errors too.
        // Disabled instrumentation allocates nothing and reads no clock.
        let mut metrics = self.metrics.take();
        let result = self.accept_measured(batch, metrics.as_deref_mut());
        if result.is_err() {
            self.nodes.rollback();
            self.directories.rollback();
            if let Some(top) = self.rollback_top.take() {
                self.top = top;
            }
            self.failed = true;
            self.errors = self.errors.saturating_add(1);
            self.revision = self.revision.saturating_add(1);
        } else {
            self.nodes.commit();
            self.directories.commit();
            self.rollback_top = None;
        }
        self.metrics = metrics;
        result
    }

    /// Restore retained observations without inspecting the source filesystem.
    /// Children must have the exact occurrence IDs assigned by the original scan.
    pub fn accept_replay(&mut self, batch: ReplayBatch) -> Result<()> {
        let expected = batch.children;
        let children = self.accept(ObservationBatch {
            parent: batch.parent,
            sequence: batch.sequence,
            end: batch.end,
            observations: &batch.observations,
            evidence: &batch.evidence,
            properties: &batch.properties,
            directory: batch.directory.as_ref(),
            detached: false,
            errors: 0,
            denied: 0,
        })?;
        if children.len() != expected.len()
            || children
                .iter()
                .zip(&expected)
                .any(|(a, b)| a.id != b.0 || a.name != b.1)
        {
            return Err(Error::Contract(
                "saved direct inventory occurrence IDs differ",
            ));
        }
        Ok(())
    }

    /// Diagnostics are persisted separately from observations. Check all derived
    /// totals before restoring them; temporary storage is specific to this session.
    pub fn restore_diagnostics(&mut self, saved: DirectSummary) -> Result<()> {
        let current = self.summary();
        if !self.finished
            || current.entries != saved.entries
            || current.measure != saved.measure
            || current.coverage != saved.coverage
            || current.pending_directories != saved.pending_directories
            || current.incomplete_directories != saved.incomplete_directories
            || saved.observations < saved.entries
            || saved.denied > saved.errors
        {
            return Err(Error::Contract("saved direct inventory summary differs"));
        }
        self.observations = saved.observations;
        self.errors = saved.errors;
        self.denied = saved.denied;
        self.detached = saved.detached_directories;
        self.duplicates = saved.duplicate_observations;
        Ok(())
    }

    fn accept_measured(
        &mut self,
        batch: ObservationBatch<'_>,
        mut metrics: Option<&mut metrics::Metrics>,
    ) -> Result<Vec<ObservedDirectory>> {
        let prepare = metrics::Metrics::start(metrics.as_deref_mut(), metrics::Phase::Prepare);
        let parent = u32::try_from(batch.parent.0).map_err(|_| invalid_entry())?;
        let directory_id = self.node(batch.parent)?.directory;
        let directory = self.directory(batch.parent)?;
        let count = batch.observations.len();
        if self.finished
            || directory.ended
            || batch.sequence != directory.sequence
            || count > 512
            || count != batch.evidence.len()
            || count != batch.properties.len()
        {
            return Err(Error::Contract("invalid direct analysis batch"));
        }

        let mut pending: Vec<Pending> = Vec::with_capacity(count);
        let mut children = Vec::new();
        let mut subtotal = Subtotal::default();
        let mut duplicates = 0;
        for (row, observation) in batch.observations.iter().enumerate() {
            let name = observation.name.encode();
            let hash = self.hashes.hash_one(&name);
            let earlier_in_batch = pending.iter().rev().find(|p| p.hash == hash);
            let seen = self.seen.get(&directory_id);
            let earlier = match seen {
                Some(seen) => match seen.heads.get(&hash).copied() {
                    Some(id) => Some(id),
                    None => match &seen.disk {
                        Some(disk) => disk.head(hash)?,
                        None => None,
                    },
                },
                None => None,
            }
            .unwrap_or(NONE);
            if pending.iter().any(|p| p.hash == hash && p.name == name)
                || self.name_seen(earlier, &name)?
            {
                // Concurrent mutation can expose an occurrence twice. Keep the
                // first accepted observation and disallow complete coverage for
                // this listing in both the live tree and the SQLite replay.
                duplicates += 1;
                continue;
            }
            let id = u32::try_from(self.nodes.len + pending.len())
                .ok()
                .filter(|id| *id != NONE)
                .ok_or_else(index_limit)?;
            if observation.kind == Kind::Directory {
                children.push(ObservedDirectory {
                    id: EntryId(id.into()),
                    name: observation.name.clone(),
                    evidence: batch.evidence[row].clone(),
                });
            }
            subtotal.observe(observation, &batch.properties[row]);
            pending.push(Pending {
                row,
                id,
                name,
                hash,
                same_hash: earlier_in_batch.map_or(earlier, |p| p.id),
                position: 0,
            });
        }

        let incomplete = directory.incomplete || duplicates != 0 || batch.detached;
        let end = batch.end.map(|end| {
            if incomplete {
                ListingEnd::Incomplete
            } else {
                end
            }
        });
        let revision = self.revision.checked_add(1).ok_or_else(invalid_entry)?;
        let sequence = directory
            .sequence
            .checked_add(1)
            .ok_or_else(invalid_entry)?;
        self.nodes.reserve(pending.len())?;
        self.directories.reserve(children.len())?;
        if end.is_none() && !pending.is_empty() {
            let seen = self.seen.entry(directory_id).or_default();
            if seen.disk.is_none() {
                let required = seen.heads.len() + pending.len();
                if required > seen.heads.capacity() {
                    // Reserve conservatively for the hash table's growth and control bytes.
                    let capacity = required.next_power_of_two().saturating_mul(2);
                    let charge =
                        self.governor
                            .reserve(Category::Cache, (capacity * 32 + 128) as u64, None);
                    if let Some(charge) = charge {
                        seen.heads
                            .try_reserve(capacity.saturating_sub(seen.heads.len()))
                            .map_err(|_| index_limit())?;
                        seen.charges.clear();
                        seen.charges.push(charge);
                    } else {
                        // Keep the already-admitted RAM prefix until EOF. New
                        // hashes spill; no unbounded migration runs under the
                        // tree write lock and existing keys stay fully exact.
                        seen.disk = Some(SeenDisk::new(&self.scratch, self.disk.clone())?);
                    }
                }
            }
        }
        prepare.finish(count as u64);

        let encode = metrics::Metrics::start(metrics.as_deref_mut(), metrics::Phase::Encode);
        self.write.0.clear();
        self.write.0.extend_from_slice(&[0; 4]);
        let stamp = batch
            .directory
            .filter(|_| end == Some(ListingEnd::Exhausted));
        self.write
            .header(batch.parent, batch.sequence, end, stamp, pending.len())?;
        for record in &mut pending {
            record.position = self.spool.length + self.write.0.len() as u64;
            self.write.record(
                record.id,
                &record.name,
                &batch.observations[record.row],
                batch.evidence[record.row].as_ref(),
                &batch.properties[record.row],
            )?;
        }
        let length = self.write.0.len() - 4;
        if length > spool::MAX_BATCH {
            return Err(Error::Contract("direct analysis batch exceeds spool bound"));
        }
        self.write.0[..4].copy_from_slice(&(length as u32).to_le_bytes());
        encode.finish(pending.len() as u64);
        let append = metrics::Metrics::start(metrics.as_deref_mut(), metrics::Phase::Append);
        self.spool.append(&self.write.0)?;
        append.finish(self.write.0.len() as u64);

        // Publish index and aggregates only after the entire batch was accepted.
        // RAM edits cannot fail after reserve. Disk edits retain an undo overlay
        // so an I/O failure exposes the previous coherent revision, not half a lot.
        if self.nodes.has_disk()
            || self.directories.has_disk()
            || self.seen.values().any(|seen| seen.disk.is_some())
        {
            self.rollback_top = Some(self.top.clone());
            self.nodes.begin();
            self.directories.begin();
        }
        let index = metrics::Metrics::start(metrics.as_deref_mut(), metrics::Phase::Index);
        let retained = pending.len() as u64;
        let dir = self
            .directories
            .get(directory_id)?
            .ok_or_else(invalid_entry)?;
        let mut last = dir.last;
        let mut last_folder = dir.last_folder;
        self.directories.edit(directory_id, |dir| {
            if dir.first == NONE
                && let Some(record) = pending.first()
            {
                dir.first = record.id;
            }
            if dir.first_folder == NONE
                && let Some(child) = children.first()
            {
                dir.first_folder = child.id.0 as u32;
            }
        })?;
        for record in pending {
            let observation = &batch.observations[record.row];
            let directory = if observation.kind == Kind::Directory {
                let child_directory = self.directories.len as u32;
                if last_folder != NONE {
                    let previous = self
                        .nodes
                        .get(last_folder)?
                        .ok_or_else(invalid_entry)?
                        .directory;
                    self.directories
                        .edit(previous, |dir| dir.next_folder = record.id)?;
                }
                self.directories.push(Directory::new(last_folder))?;
                last_folder = record.id;
                child_directory
            } else {
                NONE
            };
            if last != NONE {
                self.nodes.edit(last, |node| node.next = record.id)?;
            }
            self.nodes.push(Node {
                record: record.position,
                parent,
                directory,
                previous: last,
                next: NONE,
                same_hash: record.same_hash,
            })?;
            last = record.id;
            if end.is_none() {
                let seen = self.seen.get_mut(&directory_id).unwrap();
                if let Some(disk) = &mut seen.disk {
                    if let Some(head) = seen.heads.get_mut(&record.hash) {
                        *head = record.id;
                    } else {
                        disk.insert(record.hash, record.id)?;
                    }
                } else {
                    seen.heads.insert(record.hash, record.id);
                }
                seen.rows += 1;
                self.active_names += 1;
            }
            if observation.kind == Kind::File
                && let Some(logical) = observation.logical
            {
                self.remember_top(record.id, logical);
            }
        }
        self.directories.edit(directory_id, |dir| {
            dir.last = last;
            dir.last_folder = last_folder;
            dir.sequence = sequence;
            dir.incomplete = incomplete;
            dir.ended = end.is_some();
        })?;
        if end.is_some()
            && let Some(seen) = self.seen.remove(&directory_id)
        {
            self.active_names -= seen.rows;
        }
        index.finish(retained);

        let propagate = metrics::Metrics::start(metrics, metrics::Phase::Propagate);
        let mut ancestor_visits = 0_u64;
        let mut ancestor = parent;
        while ancestor != NONE {
            let node = self.nodes.get(ancestor)?.ok_or_else(invalid_entry)?;
            self.directories.edit(node.directory, |dir| {
                dir.subtotal.add(&subtotal);
                dir.pending += children.len() as u64;
                dir.pending -= u64::from(end.is_some());
                dir.incomplete_descendants += u64::from(end == Some(ListingEnd::Incomplete));
            })?;
            ancestor = node.parent;
            ancestor_visits += 1;
        }
        self.observations += count as u64;
        self.errors += batch.errors;
        self.denied += batch.denied;
        self.detached += u64::from(batch.detached);
        self.duplicates += duplicates;
        self.revision = revision;
        propagate.finish(ancestor_visits);
        Ok(children)
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.failed {
            return Err(Error::Contract("direct analysis failed before completion"));
        }
        if self.directories.get(0)?.ok_or_else(invalid_entry)?.pending != 0 {
            return Err(Error::Contract(
                "direct analysis has unfinished directories",
            ));
        }
        let flush = metrics::Metrics::start(self.metrics.as_deref_mut(), metrics::Phase::Flush);
        flush.finish(0);
        self.finished = true;
        Ok(())
    }

    pub(crate) fn metrics(&self) -> Option<metrics::Metrics> {
        self.metrics.as_deref().copied()
    }

    pub fn replay(&self) -> Result<Replay> {
        if !self.finished {
            return Err(Error::Contract("direct analysis is not ready to save"));
        }
        self.spool.replay()
    }

    /// Includes the root at index zero; IDs are contiguous occurrence IDs.
    pub(crate) fn disk_budget(&self) -> DiskBudget {
        self.disk.clone()
    }

    pub fn len(&self) -> u64 {
        self.nodes.len as u64
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.len == 0
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn summary(&self) -> DirectSummary {
        let root = self
            .directories
            .get(0)
            .expect("resident root")
            .expect("root directory");
        DirectSummary {
            measure: root.subtotal.measure(),
            coverage: self.coverage(EntryId(0)).unwrap(),
            observations: self.observations,
            entries: (self.nodes.len - 1) as u64,
            errors: self.errors,
            denied: self.denied,
            pending_directories: root.pending,
            incomplete_directories: root.incomplete_descendants,
            detached_directories: self.detached,
            duplicate_observations: self.duplicates,
            temporary_bytes: self.disk.used(),
        }
    }

    pub fn coverage(&self, id: EntryId) -> Result<ScopeCoverage> {
        let dir = self.directory(id)?;
        Ok(if self.failed {
            ScopeCoverage::Partial
        } else if dir.sequence == 0 {
            ScopeCoverage::Unknown
        } else if dir.pending == 0
            && dir.incomplete_descendants == 0
            && dir.subtotal.unknown == 0
            && dir.subtotal.logical <= u128::from(u64::MAX)
        {
            ScopeCoverage::Complete
        } else {
            ScopeCoverage::Partial
        })
    }

    pub fn measure(&self, id: EntryId) -> Result<FolderMeasure> {
        Ok(self.directory(id)?.subtotal.measure())
    }

    pub fn parent(&self, id: EntryId) -> Result<Option<EntryId>> {
        Ok(entry_id(self.node(id)?.parent))
    }

    /// Name or opaque key from the provider, independent of local paths.
    pub fn name(&self, id: EntryId) -> Result<NativeName> {
        let node = self.node(id)?;
        if node.parent == NONE {
            return Ok(NativeName::Opaque(Vec::new()));
        }
        self.record_name(&node)
    }

    fn record_name(&self, node: &Node) -> Result<NativeName> {
        self.spool.with_record(node.record, |record| {
            NativeName::decode(spool::encoded_name(record)?).map_err(|_| spool::invalid())
        })
    }

    pub fn path(&self, id: EntryId) -> Result<PathBuf> {
        let mut path = self.root.clone();
        let mut names = Vec::new();
        let mut node = self.node(id)?;
        while node.parent != NONE {
            names.push(native_component(&self.record_name(&node)?)?);
            node = self.nodes.get(node.parent)?.ok_or_else(invalid_entry)?;
        }
        for name in names.into_iter().rev() {
            path.push(name);
        }
        Ok(path)
    }

    pub fn row(&self, id: EntryId) -> Result<DirectRow> {
        if let Some(parent) = self.parent(id)? {
            self.read_row(id, &self.path(parent)?)
        } else {
            let measure = self.measure(id)?;
            Ok(DirectRow {
                id,
                parent: None,
                path: self.root.clone(),
                kind: Kind::Directory,
                logical: measure.logical,
                allocated: measure.allocated,
                modified: None,
                properties: NodeProperties::default(),
            })
        }
    }

    pub fn children(
        &self,
        id: EntryId,
        after: Option<EntryId>,
        limit: usize,
        folders_only: bool,
    ) -> Result<DirectPage> {
        let directory = self.directory(id)?;
        let limit = page_size(limit)?;
        let mut entry = if let Some(after) = after {
            self.sibling(id, after, folders_only, true)?
        } else if folders_only {
            directory.first_folder
        } else {
            directory.first
        };
        let parent_id = id;
        let parent = self.path(id)?;
        let mut rows = Vec::with_capacity(limit);
        let mut bytes = 0;
        while let Some(id) = entry_id(entry).filter(|_| rows.len() < limit) {
            let row = self.read_row(id, &parent)?;
            bytes += row_bytes(&row);
            if bytes > spool::MAX_PAGE_BYTES && !rows.is_empty() {
                break;
            }
            entry = self.sibling(parent_id, id, folders_only, true)?;
            rows.push(row);
        }
        let more = entry != NONE;
        let next = rows.last().filter(|_| more).map(|row| row.id);
        Ok(DirectPage { rows, more, next })
    }

    pub fn previous_children_cursor(
        &self,
        id: EntryId,
        first: EntryId,
        limit: usize,
        folders_only: bool,
    ) -> Result<Option<EntryId>> {
        let limit = page_size(limit)?;
        let mut preceding = first;
        for _ in 0..=limit {
            let sibling = self.sibling(id, preceding, folders_only, false)?;
            let Some(previous) = entry_id(sibling) else {
                return Ok(None);
            };
            preceding = previous;
        }
        Ok(Some(preceding))
    }

    pub fn top(&self) -> Result<Vec<DirectRow>> {
        self.project_top(&self.top).map(|(rows, _)| rows)
    }

    pub fn top_within(&self, folder: EntryId) -> Result<(Vec<DirectRow>, bool)> {
        if folder == EntryId(0) {
            return self.project_top(&self.top);
        }
        let mut heap = TopHeap::with_capacity(TOP_FILES);
        let mut id = self.directory(folder)?.first;
        while let Some(entry) = entry_id(id) {
            let node = self.node(entry)?;
            if node.directory != NONE {
                let child = self.directory(entry)?.first;
                if child != NONE {
                    id = child;
                    continue;
                }
            } else {
                self.spool.with_record(node.record, |record| {
                    let row = spool::record(record)?.observation;
                    if row.kind == Kind::File
                        && let Some(size) = row.logical
                    {
                        remember_top(&mut heap, id, size);
                    }
                    Ok(())
                })?;
            }
            // Parent links keep the traversal stack-free even for deep trees.
            let mut sibling = node.next;
            let mut parent = node.parent;
            while sibling == NONE && u64::from(parent) != folder.0 {
                let ancestor = self.node(EntryId(parent.into()))?;
                sibling = ancestor.next;
                parent = ancestor.parent;
            }
            id = sibling;
        }
        self.project_top(&heap)
    }

    fn project_top(&self, heap: &TopHeap) -> Result<(Vec<DirectRow>, bool)> {
        let mut keys = heap.iter().map(|entry| entry.0).collect::<Vec<_>>();
        keys.sort_unstable_by(|a, b| b.cmp(a));
        let mut rows = Vec::with_capacity(keys.len());
        let mut bytes = 0;
        for (_, Reverse(id)) in keys {
            let row = self.row(EntryId(id.into()))?;
            bytes += row_bytes(&row);
            if bytes > spool::MAX_PAGE_BYTES && !rows.is_empty() {
                return Ok((rows, true));
            }
            rows.push(row);
        }
        Ok((rows, false))
    }

    fn node(&self, id: EntryId) -> Result<Node> {
        let id = u32::try_from(id.0).map_err(|_| invalid_entry())?;
        self.nodes.get(id)?.ok_or_else(invalid_entry)
    }

    fn directory(&self, id: EntryId) -> Result<Directory> {
        self.directories
            .get(self.node(id)?.directory)?
            .ok_or_else(invalid_entry)
    }

    fn sibling(
        &self,
        parent: EntryId,
        child: EntryId,
        folders_only: bool,
        forward: bool,
    ) -> Result<u32> {
        let node = self.node(child)?;
        if u64::from(node.parent) != parent.0 {
            return Err(Error::Contract(
                "direct analysis cursor belongs to another directory",
            ));
        }
        if folders_only {
            let folder = self.directory(child)?;
            Ok(if forward {
                folder.next_folder
            } else {
                folder.previous_folder
            })
        } else {
            Ok(if forward { node.next } else { node.previous })
        }
    }

    fn read_row(&self, id: EntryId, parent: &Path) -> Result<DirectRow> {
        let node = self.node(id)?;
        let row = self.spool.with_record(node.record, spool::record)?;
        if row.id != id {
            return Err(spool::invalid());
        }
        let observation = row.observation;
        let (logical, allocated) = if observation.kind == Kind::Directory {
            let measure = self.measure(id)?;
            (measure.logical, measure.allocated)
        } else {
            (observation.logical, observation.allocated)
        };
        Ok(DirectRow {
            id,
            parent: entry_id(node.parent),
            path: parent.join(native_component(&observation.name)?),
            kind: observation.kind,
            logical,
            allocated,
            modified: observation.modified,
            properties: row.properties,
        })
    }

    fn name_seen(&self, mut id: u32, name: &[u8]) -> Result<bool> {
        while id != NONE {
            let node = self.nodes.get(id)?.ok_or_else(invalid_entry)?;
            if self
                .spool
                .with_record(node.record, |row| Ok(spool::encoded_name(row)? == name))?
            {
                return Ok(true);
            }
            id = node.same_hash;
        }
        Ok(false)
    }

    fn remember_top(&mut self, id: u32, logical: u64) {
        remember_top(&mut self.top, id, logical);
    }
}

fn remember_top(heap: &mut TopHeap, id: u32, logical: u64) {
    let key = Reverse((logical, Reverse(id)));
    if heap.len() < TOP_FILES {
        heap.push(key);
    } else if let Some(mut least) = heap.peek_mut()
        && key < *least
    {
        *least = key;
    }
}

pub(crate) fn native_component(name: &NativeName) -> Result<std::ffi::OsString> {
    use std::path::Component;

    let name = native_name(name)?;
    let mut components = Path::new(&name).components();
    if !matches!(components.next(), Some(Component::Normal(component)) if component == name)
        || components.next().is_some()
        || name.as_encoded_bytes().contains(&0)
    {
        return Err(Error::Contract(
            "direct analysis name is not a local component",
        ));
    }
    Ok(name)
}

fn invalid_entry() -> Error {
    Error::Contract("invalid direct analysis entry")
}

fn entry_id(id: u32) -> Option<EntryId> {
    (id != NONE).then_some(EntryId(id.into()))
}

fn row_bytes(row: &DirectRow) -> usize {
    row.path.capacity() + std::mem::size_of::<DirectRow>() + 128
}

fn page_size(limit: usize) -> Result<usize> {
    if !(1..=spool::MAX_PAGE_ROWS).contains(&limit) {
        return Err(Error::Contract("direct analysis page bound"));
    }
    Ok(limit)
}

#[cfg(test)]
mod tests;
