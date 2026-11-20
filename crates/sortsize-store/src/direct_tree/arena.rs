//! Fixed-width mutable arenas. Overflow blocks are private temporary files;
//! every access is fallible and never substitutes a zero/default for I/O failure.
use super::*;
use sortsize_core::resources::{Category, Governor, Reservation};
use std::collections::BTreeMap;
use storage::{DiskBudget, DiskCharge};

struct Undo<T> {
    len: usize,
    rows: BTreeMap<u32, T>,
    charges: Vec<Reservation>,
    charged: u64,
}

pub(super) trait Fixed: Copy {
    const SIZE: usize;
    fn encode(self, bytes: &mut [u8]);
    fn decode(bytes: &[u8]) -> Self;
}
enum Block<T> {
    Memory { rows: Vec<T>, _charge: Reservation },
    Disk { offset: u64 },
}
pub(super) struct Index<T> {
    blocks: Vec<Block<T>>,
    pub(super) len: usize,
    file: Mutex<File>,
    disk_length: u64,
    governor: Governor,
    metadata: Vec<Reservation>,
    undo: Option<Undo<T>>,
    failed: Option<Undo<T>>,
    disk_charge: DiskCharge,
}
impl<T: Fixed> Index<T> {
    pub(super) fn new(scratch: &Path, disk: DiskBudget, governor: Governor) -> Result<Self> {
        let owner = tempfile::Builder::new()
            .prefix("direct-arena-")
            .tempfile_in(scratch)?;
        let file = owner.reopen()?;
        let disk_charge = DiskCharge::new(disk, owner)?;
        Ok(Self {
            blocks: Vec::new(),
            len: 0,
            file: Mutex::new(file),
            disk_length: 0,
            governor,
            metadata: Vec::new(),
            undo: None,
            failed: None,
            disk_charge,
        })
    }
    pub(super) fn begin(&mut self) {
        self.undo = Some(Undo {
            len: self.len,
            rows: BTreeMap::new(),
            charges: Vec::new(),
            charged: 0,
        });
    }
    pub(super) fn has_disk(&self) -> bool {
        self.disk_length != 0
    }
    pub(super) fn commit(&mut self) {
        self.undo = None;
    }
    pub(super) fn rollback(&mut self) {
        if let Some(undo) = self.undo.take() {
            self.len = undo.len;
            // Failed disk writes cannot safely be undone by another disk write.
            // Keep the old values in RAM for all future reads of the partial view.
            self.failed = Some(undo);
        }
    }
    pub(super) fn reserve(&mut self, extra: usize) -> Result<()> {
        let length = self
            .len
            .checked_add(extra)
            .filter(|n| *n < NONE as usize)
            .ok_or_else(index_limit)?;
        while length.div_ceil(INDEX_BLOCK) > self.blocks.len() {
            if self.blocks.len() == self.blocks.capacity() {
                let charge = self
                    .governor
                    .reserve(
                        Category::Margin,
                        (128 * std::mem::size_of::<Block<T>>() + 128) as u64,
                        None,
                    )
                    .ok_or_else(index_limit)?;
                self.blocks
                    .try_reserve_exact(128)
                    .map_err(|_| index_limit())?;
                self.metadata.push(charge);
            }
            let size = (INDEX_BLOCK * std::mem::size_of::<T>()) as u64;
            let charge = self.governor.reserve(Category::Cache, size, None);
            let block = if let Some(charge) = charge {
                let mut rows = Vec::new();
                rows.try_reserve_exact(INDEX_BLOCK)
                    .map_err(|_| index_limit())?;
                Block::Memory {
                    rows,
                    _charge: charge,
                }
            } else {
                // The first block contains the root and is deliberately resident.
                if self.blocks.is_empty() {
                    return Err(index_limit());
                }
                let size = (INDEX_BLOCK * T::SIZE) as u64;
                self.disk_charge.admit(size)?;
                let offset = self.disk_length;
                let length = offset.checked_add(size).ok_or_else(index_limit)?;
                self.file.get_mut().unwrap().set_len(length)?;
                self.disk_length = length;
                Block::Disk { offset }
            };
            self.blocks.push(block);
        }
        Ok(())
    }
    pub(super) fn push(&mut self, value: T) -> Result<()> {
        let block = self
            .blocks
            .get_mut(self.len / INDEX_BLOCK)
            .ok_or_else(index_limit)?;
        match block {
            Block::Memory { rows, .. } => rows.push(value),
            Block::Disk { offset } => {
                let mut bytes = [0u8; 192];
                value.encode(&mut bytes[..T::SIZE]);
                let file = self.file.get_mut().unwrap();
                file.seek(SeekFrom::Start(
                    *offset + (self.len % INDEX_BLOCK * T::SIZE) as u64,
                ))?;
                file.write_all(&bytes[..T::SIZE])?;
            }
        }
        self.len += 1;
        Ok(())
    }
    pub(super) fn get(&self, id: u32) -> Result<Option<T>> {
        if let Some(value) = self.failed.as_ref().and_then(|undo| undo.rows.get(&id)) {
            return Ok(Some(*value));
        }
        let id = id as usize;
        if id >= self.len {
            return Ok(None);
        }
        match &self.blocks[id / INDEX_BLOCK] {
            Block::Memory { rows, .. } => Ok(rows.get(id % INDEX_BLOCK).copied()),
            Block::Disk { offset } => {
                let mut bytes = [0u8; 192];
                let mut file = self.file.lock().unwrap();
                file.seek(SeekFrom::Start(
                    *offset + (id % INDEX_BLOCK * T::SIZE) as u64,
                ))?;
                file.read_exact(&mut bytes[..T::SIZE])?;
                Ok(Some(T::decode(&bytes[..T::SIZE])))
            }
        }
    }
    pub(super) fn edit(&mut self, id: u32, edit: impl FnOnce(&mut T)) -> Result<()> {
        let index = id as usize;
        if index >= self.len {
            return Err(invalid_entry());
        }
        if self
            .undo
            .as_ref()
            .is_some_and(|undo| index < undo.len && !undo.rows.contains_key(&id))
        {
            let value = self.get(id)?.ok_or_else(invalid_entry)?;
            let undo = self.undo.as_mut().unwrap();
            let required = ((undo.rows.len() + 1) * 3 * std::mem::size_of::<(u32, T)>()) as u64;
            if required > undo.charged {
                let bytes = 64 * 1024;
                let charge = self
                    .governor
                    .reserve(Category::Transport, bytes, None)
                    .ok_or_else(index_limit)?;
                undo.charged += bytes;
                undo.charges.push(charge);
            }
            undo.rows.insert(id, value);
        }
        match &mut self.blocks[index / INDEX_BLOCK] {
            Block::Memory { rows, .. } => edit(&mut rows[index % INDEX_BLOCK]),
            Block::Disk { offset } => {
                let position = *offset + (index % INDEX_BLOCK * T::SIZE) as u64;
                let mut bytes = [0u8; 192];
                let file = self.file.get_mut().unwrap();
                file.seek(SeekFrom::Start(position))?;
                file.read_exact(&mut bytes[..T::SIZE])?;
                let mut value = T::decode(&bytes[..T::SIZE]);
                edit(&mut value);
                value.encode(&mut bytes[..T::SIZE]);
                file.seek(SeekFrom::Start(position))?;
                file.write_all(&bytes[..T::SIZE])?;
            }
        }
        Ok(())
    }
}

// Explicit integer encoding: no unsafe byte copies, pointers or padding on disk.
macro_rules! fixed_fields {
    ($ty:ty, $size:expr, $( $field:ident : $ft:ty ),+ $(,)?) => {
        impl Fixed for $ty {
            const SIZE: usize = $size;
            fn encode(self, bytes: &mut [u8]) {
                let mut at = 0;
                $(let encoded = (self.$field as $ft).to_le_bytes();
                bytes[at..at + encoded.len()].copy_from_slice(&encoded); at += encoded.len();)+
                debug_assert_eq!(at, Self::SIZE);
            }
            fn decode(bytes: &[u8]) -> Self {
                let mut at = 0;
                let result = Self { $($field: {
                    let n = std::mem::size_of::<$ft>();
                    let value = <$ft>::from_le_bytes(bytes[at..at+n].try_into().unwrap());
                    at += n; value
                }),+ };
                debug_assert_eq!(at, Self::SIZE); result
            }
        }
    }
}
fixed_fields!(Node, 28, record: u64, parent: u32, directory: u32, previous: u32,
    next: u32, same_hash: u32);
impl Fixed for Directory {
    const SIZE: usize = 128;
    fn encode(self, bytes: &mut [u8]) {
        let words = [
            self.first,
            self.last,
            self.first_folder,
            self.last_folder,
            self.previous_folder,
            self.next_folder,
        ];
        for (slot, value) in bytes[..24].as_chunks_mut::<4>().0.iter_mut().zip(words) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        let longs = [
            self.sequence,
            self.pending,
            self.incomplete_descendants,
            self.subtotal.files,
            self.subtotal.directories,
            self.subtotal.unknown,
            self.subtotal.allocation_unknown,
            self.subtotal.known_allocations,
        ];
        for (slot, value) in bytes[24..88].as_chunks_mut::<8>().0.iter_mut().zip(longs) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        bytes[88..104].copy_from_slice(&self.subtotal.logical.to_le_bytes());
        bytes[104..120].copy_from_slice(&self.subtotal.allocated.to_le_bytes());
        bytes[120] = u8::from(self.ended);
        bytes[121] = u8::from(self.incomplete);
        bytes[122] = u8::from(self.subtotal.allocation_lower_bound);
        bytes[123..128].fill(0);
    }
    fn decode(bytes: &[u8]) -> Self {
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u64_at = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        Self {
            first: u32_at(0),
            last: u32_at(4),
            first_folder: u32_at(8),
            last_folder: u32_at(12),
            previous_folder: u32_at(16),
            next_folder: u32_at(20),
            sequence: u64_at(24),
            pending: u64_at(32),
            incomplete_descendants: u64_at(40),
            ended: bytes[120] != 0,
            incomplete: bytes[121] != 0,
            subtotal: Subtotal {
                logical: u128::from_le_bytes(bytes[88..104].try_into().unwrap()),
                allocated: u128::from_le_bytes(bytes[104..120].try_into().unwrap()),
                files: u64_at(48),
                directories: u64_at(56),
                unknown: u64_at(64),
                allocation_unknown: u64_at(72),
                known_allocations: u64_at(80),
                allocation_lower_bound: bytes[122] != 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rollback_overlay_retains_old_links_even_if_disk_cannot_be_rewritten() {
        let scratch = tempfile::tempdir().unwrap();
        let governor = Governor::new(sortsize_core::inventory_contract::MemoryMode::Auto, None, 0);
        let mut arena = Index::new(
            scratch.path(),
            DiskBudget::new(8 * 1024 * 1024),
            governor.clone(),
        )
        .unwrap();
        arena.reserve(INDEX_BLOCK).unwrap();
        let node = Node {
            record: 77,
            parent: 0,
            directory: NONE,
            previous: NONE,
            next: NONE,
            same_hash: NONE,
        };
        for _ in 0..INDEX_BLOCK {
            arena.push(node).unwrap();
        }
        let usage = governor.usage();
        let _filled = governor
            .reserve(
                Category::Cache,
                usage.limits.categories[0] - usage.categories[0],
                None,
            )
            .unwrap();
        arena.reserve(1).unwrap();
        arena.push(node).unwrap();
        arena.begin();
        arena.edit(0, |row| row.next = INDEX_BLOCK as u32).unwrap();
        arena.edit(INDEX_BLOCK as u32, |row| row.next = 19).unwrap();
        // Simulate a disk failure after earlier writes by making the next edit unreadable.
        arena.file.get_mut().unwrap().set_len(0).unwrap();
        assert!(arena.edit(INDEX_BLOCK as u32, |row| row.next = 20).is_err());
        arena.rollback();
        assert_eq!(arena.get(0).unwrap().unwrap().next, NONE);
        assert_eq!(arena.get(INDEX_BLOCK as u32).unwrap().unwrap().next, NONE);
    }
}
