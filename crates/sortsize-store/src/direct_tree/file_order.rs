//! Exact name orders for unfiltered searches in a finished direct tree.
//! The order belongs to that tree and is rebuilt from its recorded observations.
use super::{
    DirectTree, entry_id, native_component, spool,
    storage::{DiskCharge, RecordBuffer},
};
use crate::inventory::{Error, Kind, Observation, Result};
use sha2::{Digest, Sha256};
use sortsize_core::{
    CancellationToken,
    inventory_contract::{EntryId, ScopeCoverage},
    resources::{Category, Governor, Reservation},
};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ffi::OsString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

const BLOCK_ENTRIES: usize = 4096;
const BLOCK_BYTES: usize = BLOCK_ENTRIES * size_of::<u32>();
const FRAME_LIMIT: usize = 512;
const CHILD_LIMIT: usize = 24 * 1024;
const NAME_BYTES: usize = 1024 * 1024;
// Limit work on the query owner when the reusable order is still being built.
const FIRST_WINDOW_BUDGET: Duration = Duration::from_millis(35);
const BUILD_BYTES: usize = CHILD_LIMIT * size_of::<Child>()
    + FRAME_LIMIT * size_of::<Frame>()
    + NAME_BYTES
    + 2 * BLOCK_BYTES
    + spool::MAX_BATCH + 4
    // At most one u16-length source record and its decoded native component.
    + 4 * u16::MAX as usize;

#[derive(Clone, Copy)]
pub(super) enum Scope {
    Files,
    All,
}

fn unavailable() -> Error {
    Error::Contract("direct file order exceeds its preparation budget")
}

fn invalid_order() -> Error {
    Error::Contract("direct file order is invalid")
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Contract("direct file order cancelled"))
    } else {
        Ok(())
    }
}

pub(super) struct FileOrders {
    collecting: Option<Totals>,
    totals: OnceLock<Totals>,
    ready: OnceLock<FileOrder>,
    preparation: Mutex<()>,
}

impl Default for FileOrders {
    fn default() -> Self {
        Self {
            collecting: Some(Totals::default()),
            totals: OnceLock::new(),
            ready: OnceLock::new(),
            preparation: Mutex::new(()),
        }
    }
}

impl FileOrders {
    pub(super) fn observe(&mut self, observation: &Observation, governor: &Governor) {
        if let Some(totals) = &mut self.collecting
            && totals.observe(observation, governor).is_err()
        {
            self.collecting = None;
        }
    }

    pub(super) fn finish(&mut self, complete: bool, entries: u64) {
        if let Some(totals) = self.collecting.take()
            && complete
            && totals.evaluated == entries
        {
            let _ = self.totals.set(totals);
        }
    }
}

impl DirectTree {
    /// Attempt to prepare the optional order without blocking existing queries.
    /// A refusal leaves the exhaustive search available.
    pub fn prepare_file_order(&self, cancel: &CancellationToken) -> Result<bool> {
        if !self.finished || self.failed || self.coverage(EntryId(0))? != ScopeCoverage::Complete {
            return Ok(false);
        }
        check_cancelled(cancel)?;
        if self.file_order.ready.get().is_some() {
            return Ok(true);
        }
        let Ok(_preparation) = self.file_order.preparation.try_lock() else {
            return Ok(false);
        };
        if self.file_order.ready.get().is_none() {
            if self.file_order.totals.get().is_none() {
                let totals = Totals::read(self, cancel)?;
                self.file_order
                    .totals
                    .set(totals)
                    .map_err(|_| invalid_order())?;
            }
            let totals = self.file_order.totals.get().ok_or_else(invalid_order)?;
            let order = FileOrder::build(self, totals, cancel)?;
            check_cancelled(cancel)?;
            self.file_order
                .ready
                .set(order)
                .map_err(|_| invalid_order())?;
        }
        Ok(true)
    }

    pub(super) fn unfiltered_window(
        &self,
        revision: u64,
        window: Window<'_>,
        cancel: &CancellationToken,
    ) -> Result<(&Totals, Vec<EntryId>)> {
        if !self.finished || self.failed || self.revision != revision {
            return Err(unavailable());
        }
        let totals = self.file_order.totals.get().ok_or_else(unavailable)?;
        let ids = if let Some(order) = self.file_order.ready.get() {
            order.index(window.scope).window(
                self,
                window.descending,
                window.anchor,
                window.inclusive,
                window.limit,
                cancel,
            )?
        } else {
            self.walk_unfiltered_window(window, cancel)?
        };
        Ok((totals, ids))
    }

    fn walk_unfiltered_window(
        &self,
        window: Window<'_>,
        cancel: &CancellationToken,
    ) -> Result<Vec<EntryId>> {
        let _memory = self
            .governor
            .reserve(Category::Queries, BUILD_BYTES as u64, None)
            .ok_or_else(unavailable)?;
        let mut walk = Walk::new(
            window.descending,
            Some(Instant::now() + FIRST_WINDOW_BUDGET),
        );
        walk.enter(self, EntryId(0), cancel)?;
        let mut ids = Vec::with_capacity(window.limit);
        if let Some(anchor) = window.anchor
            && let Some(id) = walk.resume(self, anchor, window.inclusive, cancel)?
            && matches!(window.scope, Scope::All)
        {
            ids.push(id);
        }
        while ids.len() < window.limit
            && let Some((id, kind)) = walk.next(self, cancel)?
        {
            if matches!(window.scope, Scope::All) || kind == Kind::File {
                ids.push(EntryId(id.into()));
            }
        }
        Ok(ids)
    }
}

pub(super) struct Window<'a> {
    pub(super) scope: Scope,
    pub(super) descending: bool,
    pub(super) anchor: Option<(&'a Path, EntryId)>,
    pub(super) inclusive: bool,
    pub(super) limit: usize,
}

#[derive(Default)]
pub(super) struct Totals {
    pub(super) evaluated: u64,
    pub(super) files: u64,
    pub(super) logical: u128,
    pub(super) gaps: u64,
    pub(super) unknown_sizes: u64,
    pub(super) classes: [u64; 6],
    pub(super) extensions: BTreeMap<OsString, (u64, u128)>,
    pub(super) other_extensions: (u64, u128),
    extension_memory: Vec<Reservation>,
}

impl Totals {
    pub(super) fn all_matches(&self) -> u64 {
        self.evaluated - self.gaps
    }

    fn observe(&mut self, observation: &Observation, governor: &Governor) -> Result<()> {
        self.evaluated += 1;
        if observation.kind == Kind::Unknown {
            self.gaps += 1;
        }
        if observation.kind != Kind::File {
            return Ok(());
        }
        self.files += 1;
        let bytes = u128::from(observation.logical.unwrap_or(0));
        self.logical += bytes;
        self.unknown_sizes += u64::from(observation.logical.is_none());
        self.classes[super::query::size_class(observation.logical)] += 1;
        let name = native_component(&observation.name)?;
        let extension = Path::new(&name).extension().unwrap_or_default();
        if let Some(count) = self.extensions.get_mut(extension) {
            count.0 += 1;
            count.1 += bytes;
        } else if self.extensions.len() < 100 {
            let memory = governor
                .reserve(
                    Category::Queries,
                    super::query::extension_bytes(extension),
                    None,
                )
                .ok_or_else(unavailable)?;
            self.extension_memory.push(memory);
            self.extensions.insert(extension.to_owned(), (1, bytes));
        } else {
            self.other_extensions.0 += 1;
            self.other_extensions.1 += bytes;
        }
        Ok(())
    }

    fn read(tree: &DirectTree, cancel: &CancellationToken) -> Result<Self> {
        let mut totals = Self::default();
        for batch in tree.replay()? {
            check_cancelled(cancel)?;
            for observation in batch?.observations {
                totals.observe(&observation, &tree.governor)?;
            }
        }
        if totals.evaluated != tree.len().saturating_sub(1) {
            return Err(invalid_order());
        }
        Ok(totals)
    }
}

struct Child {
    id: u32,
    name: OsString,
    kind: Kind,
}

struct Frame {
    start: usize,
    next: usize,
    names_before: usize,
    folder: EntryId,
}

struct Walk {
    children: Vec<Child>,
    frames: Vec<Frame>,
    records: RecordBuffer,
    name_bytes: usize,
    descending: bool,
    deadline: Option<Instant>,
}

impl Walk {
    fn new(descending: bool, deadline: Option<Instant>) -> Self {
        Self {
            children: Vec::with_capacity(CHILD_LIMIT),
            frames: Vec::with_capacity(FRAME_LIMIT),
            records: RecordBuffer::default(),
            name_bytes: 0,
            descending,
            deadline,
        }
    }

    fn check(&self, cancel: &CancellationToken) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(unavailable());
        }
        check_cancelled(cancel)
    }

    fn enter(
        &mut self,
        tree: &DirectTree,
        folder: EntryId,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.frames.len() == FRAME_LIMIT {
            return Err(unavailable());
        }
        let start = self.children.len();
        let names_before = self.name_bytes;
        let mut position = tree.directory(folder)?.first;
        while let Some(id) = entry_id(position) {
            self.check(cancel)?;
            let node = tree.node(id)?;
            let record =
                tree.spool
                    .with_record_buffered(node.record, &mut self.records, spool::record)?;
            let kind = record.observation.kind;
            if kind != Kind::Unknown {
                if self.children.len() == CHILD_LIMIT {
                    return Err(unavailable());
                }
                let name = native_component(&record.observation.name)?.into_owned();
                self.name_bytes += name.capacity() + 32;
                if self.name_bytes > NAME_BYTES {
                    return Err(unavailable());
                }
                self.children.push(Child {
                    id: position,
                    name,
                    kind,
                });
            }
            position = node.next;
        }
        let siblings = &mut self.children[start..];
        siblings.sort_unstable_by(|a, b| {
            if self.descending {
                b.name.cmp(&a.name)
            } else {
                a.name.cmp(&b.name)
            }
        });
        if siblings.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(invalid_order());
        }
        self.frames.push(Frame {
            start,
            next: start,
            names_before,
            folder,
        });
        Ok(())
    }

    /// Resume at a cursor from this tree using the sorted siblings on its path.
    /// A descending directory cursor sits after its descendants in the order.
    fn resume(
        &mut self,
        tree: &DirectTree,
        (path, id): (&Path, EntryId),
        inclusive: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<EntryId>> {
        let path = path.strip_prefix(&tree.root).map_err(|_| invalid_order())?;
        let mut components = path.components().peekable();
        while let Some(Component::Normal(name)) = components.next() {
            self.check(cancel)?;
            let frame = self.frames.last_mut().ok_or_else(invalid_order)?;
            let suffix = &self.children[frame.start..];
            let at = frame.start
                + suffix
                    .binary_search_by(|child| {
                        if self.descending {
                            name.cmp(&child.name)
                        } else {
                            child.name.as_os_str().cmp(name)
                        }
                    })
                    .map_err(|_| invalid_order())?;
            let child = &self.children[at];
            let found = EntryId(u64::from(child.id));
            let directory = child.kind == Kind::Directory;
            if components.peek().is_some() {
                if !directory {
                    return Err(invalid_order());
                }
                frame.next = at + 1;
                self.enter(tree, found, cancel)?;
            } else {
                if found != id {
                    return Err(invalid_order());
                }
                if directory && self.descending {
                    frame.next = at + 1;
                    return Ok(inclusive.then_some(found));
                }
                frame.next = at + usize::from(!inclusive);
                if directory && !inclusive {
                    self.enter(tree, found, cancel)?;
                }
                return Ok(None);
            }
        }
        Err(invalid_order())
    }

    fn next(
        &mut self,
        tree: &DirectTree,
        cancel: &CancellationToken,
    ) -> Result<Option<(u32, Kind)>> {
        while !self.frames.is_empty() {
            self.check(cancel)?;
            let frame = self.frames.last_mut().unwrap();
            if let Some(child) = self.children.get(frame.next) {
                frame.next += 1;
                let id = child.id;
                let kind = child.kind;
                if kind == Kind::Directory {
                    self.enter(tree, EntryId(id.into()), cancel)?;
                    if self.descending {
                        continue;
                    }
                }
                return Ok(Some((id, kind)));
            } else {
                let frame = self.frames.pop().unwrap();
                self.children.truncate(frame.start);
                self.name_bytes = frame.names_before;
                if self.descending && frame.folder != EntryId(0) {
                    return Ok(Some((frame.folder.0 as u32, Kind::Directory)));
                }
            }
        }
        Ok(None)
    }
}

pub(super) struct FileOrder {
    files: Index,
    all: Option<Index>,
}

pub(super) struct Index {
    file: Mutex<File>,
    blocks: Vec<[u8; 32]>,
    rows: u64,
    _memory: Reservation,
    _disk: DiskCharge,
}

impl FileOrder {
    fn build(tree: &DirectTree, totals: &Totals, cancel: &CancellationToken) -> Result<Self> {
        let _build = tree
            .governor
            .reserve(Category::Queries, BUILD_BYTES as u64, None)
            .ok_or_else(unavailable)?;
        let mut files = Writer::new(tree, totals.files)?;
        let mut all = (totals.all_matches() != totals.files)
            .then(|| Writer::new(tree, totals.all_matches()))
            .transpose()?;
        let mut walk = Walk::new(false, None);
        // All siblings are native normal components, so sorted depth-first
        // traversal is exactly the global Path order. On equal components the
        // accelerator refuses to rely on reversing the order for descending.
        walk.enter(tree, EntryId(0), cancel)?;
        while let Some((id, kind)) = walk.next(tree, cancel)? {
            if kind == Kind::File {
                files.write(id)?;
            }
            if let Some(all) = &mut all {
                all.write(id)?;
            }
        }
        Ok(Self {
            files: files.finish()?,
            all: all.map(Writer::finish).transpose()?,
        })
    }

    pub(super) fn index(&self, scope: Scope) -> &Index {
        match scope {
            Scope::Files => &self.files,
            Scope::All => self.all.as_ref().unwrap_or(&self.files),
        }
    }
}

struct Writer {
    index: Index,
    block: [u8; BLOCK_BYTES],
    offset: usize,
    written: u64,
}

impl Writer {
    fn new(tree: &DirectTree, rows: u64) -> Result<Self> {
        let block_count = rows.div_ceil(BLOCK_ENTRIES as u64) as usize;
        let memory = tree
            .governor
            .reserve(Category::Queries, (block_count * 32 + 256) as u64, None)
            .ok_or_else(unavailable)?;
        let owner = tempfile::Builder::new()
            .prefix("direct-file-order-")
            .tempfile_in(&tree.scratch)?;
        let disk;
        let file = owner.reopen()?;
        disk = DiskCharge::new(tree.disk.clone(), owner)?;
        disk.admit(rows * size_of::<u32>() as u64)?;
        Ok(Self {
            index: Index {
                file: Mutex::new(file),
                blocks: Vec::with_capacity(block_count),
                rows,
                _memory: memory,
                _disk: disk,
            },
            block: [0; BLOCK_BYTES],
            offset: 0,
            written: 0,
        })
    }

    fn write(&mut self, id: u32) -> Result<()> {
        if self.written >= self.index.rows {
            return Err(invalid_order());
        }
        let end = self.offset + size_of::<u32>();
        self.block[self.offset..end].copy_from_slice(&id.to_le_bytes());
        self.offset = end;
        self.written += 1;
        if self.offset == BLOCK_BYTES {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<()> {
        let complete = &self.block[..self.offset];
        self.index.file.get_mut().unwrap().write_all(complete)?;
        self.index.blocks.push(Sha256::digest(complete).into());
        self.offset = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<Index> {
        if self.written != self.index.rows {
            return Err(invalid_order());
        }
        if self.offset != 0 {
            self.flush_block()?;
        }
        Ok(self.index)
    }
}

impl Index {
    pub(super) fn window(
        &self,
        tree: &DirectTree,
        descending: bool,
        anchor: Option<(&Path, EntryId)>,
        inclusive: bool,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<EntryId>> {
        let Ok(file) = self.file.try_lock() else {
            return Err(unavailable());
        };
        let mut reads = Reads {
            order: self,
            file,
            block: [0; BLOCK_BYTES],
            cached: None,
            descending,
        };
        let files = self.rows;
        let mut start = 0;
        if let Some((path, anchor_id)) = anchor {
            let mut end = files;
            while start < end {
                check_cancelled(cancel)?;
                let mid = start + (end - start) / 2;
                let id = reads.entry(mid)?;
                let comparison = tree.path(id)?.as_path().cmp(path);
                let comparison = if descending {
                    comparison.reverse()
                } else {
                    comparison
                }
                .then_with(|| id.0.cmp(&anchor_id.0));
                if comparison == Ordering::Less || (!inclusive && comparison == Ordering::Equal) {
                    start = mid + 1;
                } else {
                    end = mid;
                }
            }
        }
        (start..files.min(start + limit as u64))
            .map(|rank| {
                check_cancelled(cancel)?;
                reads.entry(rank)
            })
            .collect()
    }
}

struct Reads<'a> {
    order: &'a Index,
    file: MutexGuard<'a, File>,
    block: [u8; BLOCK_BYTES],
    cached: Option<usize>,
    descending: bool,
}

impl Reads<'_> {
    fn entry(&mut self, rank: u64) -> Result<EntryId> {
        let files = self.order.rows;
        if rank >= files {
            return Err(invalid_order());
        }
        let position = if self.descending {
            files - rank - 1
        } else {
            rank
        };
        let start = position / BLOCK_ENTRIES as u64 * BLOCK_ENTRIES as u64;
        let index = (start / BLOCK_ENTRIES as u64) as usize;
        if self.cached != Some(index) {
            let bytes = (files - start).min(BLOCK_ENTRIES as u64) as usize * size_of::<u32>();
            self.file
                .seek(SeekFrom::Start(start * size_of::<u32>() as u64))?;
            self.file.read_exact(&mut self.block[..bytes])?;
            let checksum: [u8; 32] = Sha256::digest(&self.block[..bytes]).into();
            if self.order.blocks.get(index) != Some(&checksum) {
                return Err(invalid_order());
            }
            self.cached = Some(index);
        }
        let offset = (position - start) as usize * size_of::<u32>();
        let id = u32::from_le_bytes(
            self.block[offset..offset + size_of::<u32>()]
                .try_into()
                .unwrap(),
        );
        Ok(EntryId(id.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{codec::NativeName, inventory::Observation, scheduler::direct::ObservationBatch};
    use sortsize_core::{
        InventoryKind, acquisition::NodeProperties, analysis::SearchQuery,
        inventory_contract::ListingEnd,
    };

    fn observation(name: &str, kind: Kind, size: u64) -> Observation {
        #[cfg(unix)]
        let name = NativeName::Posix(name.as_bytes().to_vec());
        #[cfg(windows)]
        let name = NativeName::Windows(name.encode_utf16().collect());
        Observation {
            name,
            kind,
            logical: Some(size),
            allocated: None,
            modified: None,
            metadata_known: true,
        }
    }

    fn push(tree: &mut DirectTree, parent: u64, observations: &[Observation]) {
        tree.accept(ObservationBatch {
            parent: EntryId(parent),
            sequence: 0,
            end: Some(ListingEnd::Exhausted),
            observations,
            evidence: &vec![None; observations.len()],
            properties: &vec![NodeProperties::default(); observations.len()],
            directory: None,
            detached: false,
            errors: 0,
            denied: 0,
        })
        .unwrap();
    }

    fn fixture() -> (tempfile::TempDir, DirectTree) {
        let scratch = tempfile::tempdir().unwrap();
        let mut tree =
            DirectTree::new(scratch.path().join("source"), scratch.path(), 8 << 20).unwrap();
        let mut root = vec![
            observation("é", Kind::Directory, 0),
            observation("a", Kind::Directory, 0),
            observation("a.txt", Kind::File, 2),
            observation("b", Kind::Symlink, 0),
        ];
        root.extend((0..290).rev().map(|n| {
            observation(
                &format!("x{n:03}.e{:03}", n % 145),
                Kind::File,
                (n as u64) << (n % 5 * 10),
            )
        }));
        push(&mut tree, 0, &root);
        push(
            &mut tree,
            1,
            &[
                observation("zzz.e000", Kind::File, 333),
                observation("aaa.e144", Kind::File, 1),
            ],
        );
        push(
            &mut tree,
            2,
            &[
                observation("b", Kind::Directory, 0),
                observation("a.e140", Kind::File, 0),
            ],
        );
        push(&mut tree, 297, &[observation("zzz.e000", Kind::File, 12)]);
        tree.finish().unwrap();
        (scratch, tree)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Pages {
        entries: Vec<EntryId>,
        matches: u64,
        files: u64,
        logical: u128,
        evaluated: u64,
        gaps: u64,
        unknown_sizes: u64,
        classes: [u64; 6],
        extensions: BTreeMap<OsString, (u64, u128)>,
        other: (u64, u128),
    }

    fn pages(tree: &DirectTree, sort: i32, kind: Option<InventoryKind>, indexed: bool) -> Pages {
        use super::super::query::PageQuery;
        let cancel = CancellationToken::default();
        let criteria = SearchQuery {
            kind,
            ..Default::default()
        };
        let mut anchor = None;
        let mut entries = Vec::new();
        loop {
            let mut query = PageQuery::new(tree, None, Some(&criteria), sort, anchor, 31).unwrap();
            if indexed {
                assert!(query.advance(tree, &cancel).unwrap());
            }
            while !query.advance(tree, &cancel).unwrap() {}
            entries.extend(query.rows(tree).unwrap().into_iter().map(|row| row.id));
            anchor = query.next();
            if anchor.is_none() {
                return Pages {
                    entries,
                    matches: query.matches,
                    files: query.files,
                    logical: query.logical,
                    evaluated: query.evaluated,
                    gaps: query.gaps,
                    unknown_sizes: query.unknown_sizes,
                    classes: query.classes,
                    extensions: query.extensions,
                    other: query.other_extensions,
                };
            }
        }
    }

    #[test]
    fn native_pagination_and_statistics_match_before_and_after_order_preparation() {
        let (_scratch, mut tree) = fixture();
        let scopes = [None, Some(InventoryKind::File)];
        let orders: Vec<_> = (2..=3)
            .flat_map(|sort| scopes.map(|scope| (sort, scope)))
            .collect();
        let totals = tree.file_order.totals.take().unwrap();
        let expected = orders
            .iter()
            .map(|&(sort, scope)| pages(&tree, sort, scope, false))
            .collect::<Vec<_>>();
        assert!(tree.file_order.totals.set(totals).is_ok());
        assert!(tree.file_order.ready.get().is_none());
        for (&(sort, scope), expected) in orders.iter().zip(&expected) {
            assert_ne!(expected.other.0, 0);
            assert_eq!(&pages(&tree, sort, scope, true), expected);
        }
        assert!(
            tree.prepare_file_order(&CancellationToken::default())
                .unwrap()
        );
        for ((sort, scope), expected) in orders.into_iter().zip(expected) {
            assert_eq!(pages(&tree, sort, scope, true), expected);
        }
    }

    #[test]
    fn an_inclusive_directory_cursor_matches_the_exhaustive_order() {
        use super::super::query::PageQuery;
        let (_scratch, mut tree) = fixture();
        let cancel = CancellationToken::default();
        for sort in [2, 3] {
            let criteria = SearchQuery::default();
            let collect = |tree: &DirectTree, fast: bool| {
                let mut query = PageQuery::new(tree, None, Some(&criteria), sort, None, 31)
                    .unwrap()
                    .start_at(tree, EntryId(2))
                    .unwrap();
                if fast {
                    assert!(query.advance(tree, &cancel).unwrap());
                }
                while !query.advance(tree, &cancel).unwrap() {}
                query
                    .rows(tree)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.id)
                    .collect::<Vec<_>>()
            };
            let totals = tree.file_order.totals.take().unwrap();
            let expected = collect(&tree, false);
            assert!(tree.file_order.totals.set(totals).is_ok());
            assert_eq!(expected.first(), Some(&EntryId(2)));
            assert_eq!(collect(&tree, true), expected);
        }
    }

    #[test]
    fn an_anchor_from_a_size_order_matches_the_exhaustive_query() {
        use super::super::query::PageQuery;
        let (_scratch, tree) = fixture();
        let cancel = CancellationToken::default();
        let criteria = SearchQuery::default();
        let mut size_order = PageQuery::new(&tree, None, Some(&criteria), 1, None, 31).unwrap();
        while !size_order.advance(&tree, &cancel).unwrap() {}
        let anchor = size_order.next().unwrap();
        let collect = || {
            let mut query =
                PageQuery::new(&tree, None, Some(&criteria), 2, Some(anchor.clone()), 31).unwrap();
            while !query.advance(&tree, &cancel).unwrap() {}
            (
                query.matches,
                query
                    .rows(&tree)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.id)
                    .collect::<Vec<_>>(),
            )
        };
        let expected = collect();
        assert!(tree.prepare_file_order(&cancel).unwrap());
        assert_eq!(collect(), expected);
    }

    #[test]
    fn cancelled_preparation_and_forged_file_order_keep_exhaustive_search() {
        let (_scratch, tree) = fixture();
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(tree.prepare_file_order(&cancel).is_err());
        assert!(tree.file_order.ready.get().is_none());
        let expected = pages(&tree, 2, Some(InventoryKind::File), false);
        assert!(
            tree.prepare_file_order(&CancellationToken::default())
                .unwrap()
        );
        {
            let order = tree.file_order.ready.get().unwrap();
            let mut file = order.files.file.lock().unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&3u32.to_le_bytes()).unwrap();
        }
        assert_eq!(pages(&tree, 2, Some(InventoryKind::File), false), expected);
    }

    #[test]
    fn a_refused_disk_order_keeps_the_same_direct_tree() {
        let (_scratch, tree) = fixture();
        let expected = pages(&tree, 3, None, false);
        let disk_before = tree.disk.used();
        tree.disk.cap_remaining_for_test(0);
        assert!(
            tree.prepare_file_order(&CancellationToken::default())
                .is_err()
        );
        assert!(tree.file_order.ready.get().is_none());
        assert_eq!(tree.disk.used(), disk_before);
        assert_eq!(pages(&tree, 3, None, false), expected);
    }
}
