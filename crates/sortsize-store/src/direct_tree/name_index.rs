//! A disposable candidate filter for verified direct inventory names.
//! Each block summarizes consecutive occurrence IDs. Its digest is kept in
//! memory so a damaged scratch file cannot certify absence.
use super::{
    DirectSummary, DirectTree,
    storage::{DiskBudget, DiskCharge},
};
use crate::inventory::{Error, Kind, Observation, Result};
use sha2::{Digest, Sha256};
use sortsize_core::{
    CancellationToken,
    analysis::PreparedQuery,
    inventory_contract::{EntryId, ScopeCoverage},
    resources::{Category, Governor, Reservation},
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
};

const ROWS_PER_BLOCK: usize = 512;
const GRAM_BYTES: usize = 8;
const WORDS_PER_BLOCK: usize = 128;
const PROBES: usize = 6;
const GAP_BYTES: usize = 2 * std::mem::size_of::<u16>();
const BLOCK_BYTES: usize = WORDS_PER_BLOCK * 8 + GAP_BYTES;
const BLOCKS_PER_GROUP: usize = 64;
const GROUP_BYTES: usize = BLOCKS_PER_GROUP * BLOCK_BYTES;
const MINIMUM_ROWS: u64 = 50_000;

fn unavailable() -> Error {
    Error::Contract("direct name index unavailable")
}

fn locations(gram: [u8; GRAM_BYTES]) -> impl Iterator<Item = usize> {
    let packed = u64::from_be_bytes(gram);
    let mixed = (packed ^ (packed >> 17)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let seed = (mixed ^ (mixed >> 40)) as usize;
    let step = (mixed >> 47) as usize | 1;
    (0..PROBES).map(move |probe| seed.wrapping_add(probe * step) & (WORDS_PER_BLOCK * 64 - 1))
}

struct Block {
    words: [u64; WORDS_PER_BLOCK],
    gaps: [u16; 2],
}

impl Block {
    fn new() -> Self {
        Self {
            words: [0; WORDS_PER_BLOCK],
            gaps: [0; 2],
        }
    }

    fn insert_name(&mut self, name: &str) {
        let folded;
        let name = if name.is_ascii() && !name.bytes().any(|b| b.is_ascii_uppercase()) {
            name
        } else {
            folded = name.to_lowercase();
            &folded
        };
        for gram in name.as_bytes().windows(GRAM_BYTES) {
            for bit in locations(gram.try_into().expect("fixed name gram width")) {
                self.words[bit / 64] |= 1 << (bit % 64);
            }
        }
    }

    fn write_to(&mut self, buffer: &mut Vec<u8>) {
        for gaps in self.gaps {
            buffer.extend_from_slice(&gaps.to_le_bytes());
        }
        for word in self.words {
            buffer.extend_from_slice(&word.to_le_bytes());
        }
        *self = Self::new();
    }
}

#[derive(Default)]
pub(super) struct Slot {
    writing: Option<Builder>,
    publication: OnceLock<Arc<NameIndex>>,
    preparing: Mutex<()>,
}

impl Slot {
    pub(super) fn new(governor: &Governor) -> Self {
        Self {
            writing: Builder::new(governor).ok(),
            ..Self::default()
        }
    }

    pub(super) fn observe<'row>(
        &mut self,
        observations: impl IntoIterator<Item = &'row Observation>,
        scratch: &Path,
        governor: &Governor,
        disk: &DiskBudget,
    ) {
        if let Some(writer) = &mut self.writing {
            for observation in observations {
                if writer
                    .observe(observation, scratch, governor, disk)
                    .is_err()
                {
                    self.discard();
                    break;
                }
            }
        }
    }

    pub(super) fn discard(&mut self) {
        self.writing = None;
    }

    pub(super) fn yield_disk_to_observations(
        &mut self,
        governor: &Governor,
        disk: &DiskBudget,
        headroom: u64,
    ) {
        if self
            .writing
            .as_ref()
            .is_some_and(|writer| writer.output.is_some())
        {
            let usage = governor.usage();
            let global_free = usage
                .disk_limit
                .saturating_sub(usage.disk_used.saturating_add(usage.disk_reserved));
            if global_free.min(disk.remaining()) < headroom {
                self.discard();
            }
        }
    }

    pub(super) fn finish(
        &mut self,
        summary: DirectSummary,
        revision: u64,
        scratch: &Path,
        governor: &Governor,
        disk: &DiskBudget,
    ) {
        let Some(writer) = self.writing.take() else {
            return;
        };
        if summary.coverage == ScopeCoverage::Complete
            && summary.entries >= MINIMUM_ROWS
            && let Ok(index) = writer.finish(summary.entries, revision, scratch, governor, disk)
        {
            let _ = self.publication.set(Arc::new(index));
        }
    }
}

struct IndexFile {
    file: File,
    disk: DiskCharge,
}

struct Builder {
    offset: usize,
    block: Block,
    buffer: Vec<u8>,
    digests: Vec<[u8; 32]>,
    digest_memory: Option<Reservation>,
    output: Option<IndexFile>,
    _working: Reservation,
}

struct NameIndex {
    file: Mutex<File>,
    digests: Vec<[u8; 32]>,
    blocks: usize,
    revision: u64,
    _memory: Reservation,
    _disk: DiskCharge,
}

impl DirectTree {
    /// Completed trees normally publish the accelerator during ingestion.
    /// A caller may retry preparation later if the temporary index was refused.
    pub fn prepare_name_search(&self, cancel: &CancellationToken) -> Result<bool> {
        if !self.finished
            || self.failed
            || self.len().saturating_sub(1) < MINIMUM_ROWS
            || self.coverage(EntryId(0))? != ScopeCoverage::Complete
            || cancel.is_cancelled()
        {
            return Ok(false);
        }
        if self.name_index.publication.get().is_some() {
            return Ok(true);
        }
        let Ok(_preparing) = self.name_index.preparing.try_lock() else {
            return Ok(false);
        };
        if self.name_index.publication.get().is_some() {
            return Ok(true);
        }
        let index = NameIndex::build(self, cancel)?;
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let _ = self.name_index.publication.set(Arc::new(index));
        Ok(true)
    }
}

impl NameIndex {
    fn build(tree: &DirectTree, cancel: &CancellationToken) -> Result<Self> {
        let mut writer = Builder::new(&tree.governor)?;
        for batch in tree.replay()? {
            if cancel.is_cancelled() {
                return Err(Error::Contract("direct name index cancelled"));
            }
            for observation in batch?.observations {
                writer.observe(&observation, &tree.scratch, &tree.governor, &tree.disk)?;
            }
        }
        writer.finish(
            tree.len() - 1,
            tree.revision(),
            &tree.scratch,
            &tree.governor,
            &tree.disk,
        )
    }
}

impl Builder {
    fn new(governor: &Governor) -> Result<Self> {
        // Buffer the first two groups to keep smaller inventories off disk.
        let working = governor
            .reserve(Category::Queries, (2 * GROUP_BYTES + 2048) as u64, None)
            .ok_or_else(unavailable)?;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(2 * GROUP_BYTES)
            .map_err(|_| unavailable())?;
        Ok(Self {
            offset: 0,
            block: Block::new(),
            buffer,
            digests: Vec::new(),
            digest_memory: None,
            output: None,
            _working: working,
        })
    }

    fn observe(
        &mut self,
        observation: &Observation,
        scratch: &Path,
        governor: &Governor,
        disk: &DiskBudget,
    ) -> Result<()> {
        let name = super::native_component(&observation.name)?;
        match (observation.kind, name.to_str()) {
            (Kind::Unknown, _) => {
                self.block.gaps[0] += 1;
                self.block.gaps[1] += 1;
            }
            (_, Some(name)) => self.block.insert_name(name),
            (kind, None) => {
                self.block.gaps[0] += 1;
                self.block.gaps[1] += u16::from(kind == Kind::File);
            }
        }
        self.offset += 1;
        if self.offset.is_multiple_of(ROWS_PER_BLOCK) {
            self.block.write_to(&mut self.buffer);
            if self.buffer.len() == 2 * GROUP_BYTES {
                self.flush(scratch, governor, disk)?;
            }
        }
        Ok(())
    }

    fn flush(&mut self, scratch: &Path, governor: &Governor, disk: &DiskBudget) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let needed = self.digests.len() + self.buffer.len().div_ceil(GROUP_BYTES);
        if needed > self.digests.capacity() {
            let capacity = self.digests.capacity().saturating_mul(2).max(8).max(needed);
            let memory = governor
                .reserve(Category::Queries, (capacity * 32 + 256) as u64, None)
                .ok_or_else(unavailable)?;
            self.digests
                .try_reserve_exact(capacity - self.digests.len())
                .map_err(|_| unavailable())?;
            self.digest_memory = Some(memory);
        }
        if self.output.is_none() {
            let owner = tempfile::Builder::new()
                .prefix("direct-name-index-")
                .tempfile_in(scratch)?;
            let file = owner.reopen()?;
            self.output = Some(IndexFile {
                file,
                disk: DiskCharge::new(disk.clone(), owner)?,
            });
        }
        let output = self.output.as_mut().ok_or_else(unavailable)?;
        output.disk.admit(self.buffer.len() as u64)?;
        output.file.write_all(&self.buffer)?;
        self.digests.extend(
            self.buffer
                .chunks(GROUP_BYTES)
                .map(|group| <[u8; 32]>::from(Sha256::digest(group))),
        );
        self.buffer.clear();
        Ok(())
    }

    fn finish(
        mut self,
        entries: u64,
        revision: u64,
        scratch: &Path,
        governor: &Governor,
        disk: &DiskBudget,
    ) -> Result<NameIndex> {
        if self.offset as u64 != entries {
            return Err(unavailable());
        }
        if !self.offset.is_multiple_of(ROWS_PER_BLOCK) {
            self.block.write_to(&mut self.buffer);
        }
        self.flush(scratch, governor, disk)?;
        let blocks = self.offset.div_ceil(ROWS_PER_BLOCK);
        let bytes = blocks.checked_mul(BLOCK_BYTES).ok_or_else(unavailable)?;
        let output = self.output.as_mut().ok_or_else(unavailable)?;
        if self.digests.len() != blocks.div_ceil(BLOCKS_PER_GROUP)
            || output.file.stream_position()? != bytes as u64
        {
            return Err(unavailable());
        }
        let memory = self.digest_memory.ok_or_else(unavailable)?;
        let IndexFile { file, disk } = self.output.ok_or_else(unavailable)?;
        Ok(NameIndex {
            file: Mutex::new(file),
            digests: self.digests,
            blocks,
            revision,
            _memory: memory,
            _disk: disk,
        })
    }
}

pub(super) struct NameSearch {
    index: Weak<NameIndex>,
    grams: [[u8; GRAM_BYTES]; 2],
    files_only: bool,
    group: Option<usize>,
    bytes: Vec<u8>,
    _memory: Reservation,
}

impl NameSearch {
    pub(super) fn new(tree: &DirectTree, query: &PreparedQuery) -> Option<Self> {
        let index = tree.name_index.publication.get()?;
        let (needle, files_only) = query.folded_name_prefilter()?;
        let needle = needle.as_bytes();
        let grams = [
            needle.get(..GRAM_BYTES)?.try_into().ok()?,
            needle
                .get(needle.len().checked_sub(GRAM_BYTES)?..)?
                .try_into()
                .ok()?,
        ];
        let memory = tree
            .governor
            .reserve(Category::Queries, (GROUP_BYTES + 256) as u64, None)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(GROUP_BYTES).ok()?;
        Some(Self {
            index: Arc::downgrade(index),
            grams,
            files_only,
            group: None,
            bytes,
            _memory: memory,
        })
    }

    pub(super) fn is_current(&self, tree: &DirectTree) -> bool {
        !tree.failed
            && tree.name_index.publication.get().is_some_and(|index| {
                std::ptr::eq(self.index.as_ptr(), Arc::as_ptr(index))
                    && index.revision == tree.revision()
            })
    }

    /// The caller is at the next unexamined ID. Return its block's exclusive
    /// end and native gaps only when the verified summary excludes the name.
    pub(super) fn skip(&mut self, tree: &DirectTree, id: u64) -> Result<Option<(u64, u64)>> {
        let offset = usize::try_from(id - 1).map_err(|_| unavailable())?;
        if !offset.is_multiple_of(ROWS_PER_BLOCK) {
            return Ok(None);
        }
        let index = tree.name_index.publication.get().ok_or_else(unavailable)?;
        let number = offset / ROWS_PER_BLOCK;
        let group = number / BLOCKS_PER_GROUP;
        if self.group != Some(group) {
            let digest = index.digests.get(group).ok_or_else(unavailable)?;
            let first = group * BLOCKS_PER_GROUP;
            let length = (index.blocks - first).min(BLOCKS_PER_GROUP) * BLOCK_BYTES;
            self.bytes.resize(length, 0);
            let mut file = index.file.lock().unwrap();
            file.seek(SeekFrom::Start((first * BLOCK_BYTES) as u64))?;
            file.read_exact(&mut self.bytes)?;
            drop(file);
            let actual: [u8; 32] = Sha256::digest(&self.bytes).into();
            if digest != &actual {
                return Err(unavailable());
            }
            self.group = Some(group);
        }
        let start = (number % BLOCKS_PER_GROUP) * BLOCK_BYTES;
        let block = self
            .bytes
            .get(start..start + BLOCK_BYTES)
            .ok_or_else(unavailable)?;
        let excluded = self
            .grams
            .iter()
            .copied()
            .flat_map(locations)
            .any(|bit| block[GAP_BYTES + bit / 8] & (1 << (bit % 8)) == 0);
        Ok(excluded.then(|| {
            let end = (id + ROWS_PER_BLOCK as u64).min(tree.len());
            let start = usize::from(self.files_only) * 2;
            let gaps = u16::from_le_bytes([block[start], block[start + 1]]);
            (end, u64::from(gaps))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{codec::NativeName, direct_tree::query::PageQuery, inventory::Observation};
    use sortsize_core::{
        InventoryKind, acquisition::NodeProperties, analysis::SearchQuery,
        inventory_contract::ListingEnd,
    };

    fn observation(name: NativeName, kind: Kind) -> Observation {
        Observation {
            name,
            kind,
            logical: Some(5),
            allocated: None,
            modified: None,
            metadata_known: true,
        }
    }

    fn name(name: &str) -> NativeName {
        #[cfg(windows)]
        return NativeName::Windows(name.encode_utf16().collect());
        #[cfg(not(windows))]
        NativeName::Posix(name.as_bytes().to_vec())
    }

    fn unreadable(discriminator: u8) -> NativeName {
        #[cfg(windows)]
        return NativeName::Windows(vec![0xd800, u16::from(discriminator)]);
        #[cfg(not(windows))]
        NativeName::Posix(vec![0xff, discriminator])
    }

    fn fixture() -> (tempfile::TempDir, DirectTree) {
        let scratch = tempfile::tempdir().unwrap();
        let mut tree =
            DirectTree::new(scratch.path().join("offline"), scratch.path(), 8 << 20).unwrap();
        let batches = [
            (0..ROWS_PER_BLOCK - 2)
                .map(|n| observation(name(&format!("sample-{n:04}.txt")), Kind::File))
                .chain([
                    observation(unreadable(b'1'), Kind::File),
                    observation(unreadable(b'2'), Kind::Symlink),
                ])
                .collect::<Vec<_>>(),
            vec![
                observation(name("LATE-ÉCHANTILLON.txt"), Kind::File),
                observation(name("late-échantillon.link"), Kind::Symlink),
            ],
        ];
        for (sequence, rows) in batches.iter().enumerate() {
            tree.accept(crate::scheduler::direct::ObservationBatch {
                parent: EntryId(0),
                sequence: sequence as u64,
                end: (sequence == 1).then_some(ListingEnd::Exhausted),
                observations: rows,
                evidence: &vec![None; rows.len()],
                properties: &vec![NodeProperties::default(); rows.len()],
                directory: None,
                detached: false,
                errors: 0,
                denied: 0,
            })
            .unwrap();
        }
        tree.finish().unwrap();
        (scratch, tree)
    }

    fn append_large_fixture(tree: &mut DirectTree) -> u64 {
        // Exactly two complete groups so the writer has already reserved disk
        // before the test finalizes the listing or forces quota pressure.
        let batches = 2 * BLOCKS_PER_GROUP;
        for sequence in 0..batches {
            let start = sequence * ROWS_PER_BLOCK;
            let rows = (start..start + ROWS_PER_BLOCK)
                .map(|n| observation(name(&format!("sample-{n:06}.txt")), Kind::File))
                .collect::<Vec<_>>();
            tree.accept(crate::scheduler::direct::ObservationBatch {
                parent: EntryId(0),
                sequence: sequence as u64,
                end: None,
                observations: &rows,
                evidence: &vec![None; rows.len()],
                properties: &vec![NodeProperties::default(); rows.len()],
                directory: None,
                detached: false,
                errors: 0,
                denied: 0,
            })
            .unwrap();
        }
        batches as u64
    }

    fn finish_large_fixture(tree: &mut DirectTree, sequence: u64) {
        tree.accept(crate::scheduler::direct::ObservationBatch {
            parent: EntryId(0),
            sequence,
            end: Some(ListingEnd::Exhausted),
            observations: &[],
            evidence: &[],
            properties: &[],
            directory: None,
            detached: false,
            errors: 0,
            denied: 0,
        })
        .unwrap();
        tree.finish().unwrap();
    }

    fn large_fixture_tree(scratch: &Path) -> DirectTree {
        DirectTree::with_governor(
            scratch.join("offline"),
            scratch,
            1 << 30,
            Governor::new(
                sortsize_core::inventory_contract::MemoryMode::Auto,
                None,
                32 << 30,
            ),
        )
        .unwrap()
    }

    #[test]
    fn exact_name_index_is_published_at_first_completed_view() {
        let scratch = tempfile::tempdir().unwrap();
        let mut tree = large_fixture_tree(scratch.path());
        let sequence = append_large_fixture(&mut tree);
        assert!(tree.name_index.publication.get().is_none());
        finish_large_fixture(&mut tree, sequence);
        assert!(tree.name_index.publication.get().is_some());

        let last = tree.len() - 2;
        let found = search(
            &tree,
            &SearchQuery {
                name: Some(format!("sample-{last:06}.txt")),
                ..Default::default()
            },
        );
        assert_eq!(found.rows, [EntryId(last + 1)]);
        assert_eq!((found.matches, found.gaps, found.logical), (1, 0, 5));
        assert_eq!(found.evaluated, tree.len() - 1);
    }

    #[test]
    fn in_progress_name_index_releases_disk_before_a_tight_batch() {
        let scratch = tempfile::tempdir().unwrap();
        let mut tree = large_fixture_tree(scratch.path());
        let sequence = append_large_fixture(&mut tree);
        let used = tree.disk.used();
        assert!(used >= (2 * GROUP_BYTES) as u64);
        tree.disk.cap_remaining_for_test(0);

        finish_large_fixture(&mut tree, sequence);
        assert!(tree.name_index.publication.get().is_none());
        assert_eq!(tree.disk.used(), used - (2 * GROUP_BYTES) as u64);
        assert_eq!(tree.summary().coverage, ScopeCoverage::Complete);
    }

    fn publish_index(tree: &DirectTree) {
        let index = NameIndex::build(tree, &CancellationToken::default()).unwrap();
        assert!(tree.name_index.publication.set(Arc::new(index)).is_ok());
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ResultSummary {
        rows: Vec<EntryId>,
        evaluated: u64,
        matches: u64,
        files: u64,
        gaps: u64,
        logical: u128,
        classes: [u64; 6],
    }

    fn search(tree: &DirectTree, query: &SearchQuery) -> ResultSummary {
        let mut page = PageQuery::new(tree, None, Some(query), 2, None, 200).unwrap();
        while !page.advance(tree, &CancellationToken::default()).unwrap() {}
        ResultSummary {
            rows: page
                .rows(tree)
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect(),
            evaluated: page.evaluated,
            matches: page.matches,
            files: page.files,
            gaps: page.gaps,
            logical: page.logical,
            classes: page.classes,
        }
    }

    #[test]
    fn folded_candidates_preserve_native_text_gaps_and_exact_predicates() {
        let (_scratch, tree) = fixture();
        let queries = [
            SearchQuery {
                name: Some("late-échantillon".into()),
                kind: Some(InventoryKind::File),
                ..Default::default()
            },
            SearchQuery {
                name: Some("late-échantillon".into()),
                ..Default::default()
            },
            SearchQuery {
                name: Some("no-such-name".into()),
                ..Default::default()
            },
            SearchQuery {
                name: Some("SAMPLE-0000".into()),
                case_sensitive: true,
                ..Default::default()
            },
            SearchQuery {
                name: Some("LATE-ÉCHANTILLON".into()),
                case_sensitive: true,
                ..Default::default()
            },
        ];
        let expected = queries
            .iter()
            .map(|query| search(&tree, query))
            .collect::<Vec<_>>();
        assert_eq!((expected[0].matches, expected[0].gaps), (1, 1));
        assert_eq!((expected[1].matches, expected[1].gaps), (2, 2));
        assert_eq!((expected[2].matches, expected[2].gaps), (0, 2));
        assert_eq!(expected[3].matches, 0);
        assert_eq!(expected[4].matches, 1);
        publish_index(&tree);
        for (query, expected) in queries.iter().zip(expected) {
            assert_eq!(search(&tree, query), expected);
        }
    }

    #[test]
    fn damaged_index_falls_back_and_a_different_tree_cannot_reuse_it() {
        let (_scratch, tree) = fixture();
        let (_other_scratch, other) = fixture();
        let query = SearchQuery {
            name: Some("late-échantillon".into()),
            ..Default::default()
        };
        let expected = search(&tree, &query);
        publish_index(&tree);
        let mut page = PageQuery::new(&tree, None, Some(&query), 2, None, 200).unwrap();
        assert!(page.advance(&other, &CancellationToken::default()).is_err());
        let index = tree.name_index.publication.get().unwrap();
        {
            let mut file = index.file.lock().unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&[0xFF; BLOCK_BYTES]).unwrap();
        }
        assert_eq!(search(&tree, &query), expected);
    }
}
