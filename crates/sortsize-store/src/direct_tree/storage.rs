//! Bounded, reconstructible storage for a single in-process inventory.
//! RAM is admitted before allocation. Only overflow is written to temporary files.
use super::*;
use sortsize_core::resources::{Category, Governor, Reservation};

#[derive(Clone)]
pub(crate) struct DiskBudget {
    state: Arc<Mutex<(u64, u64)>>,
    governor: Option<Governor>,
}
impl DiskBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new((0, limit))),
            governor: None,
        }
    }
    pub(super) fn with_governor(limit: u64, governor: Governor) -> Self {
        let mut budget = Self::new(limit);
        budget.governor = Some(governor);
        budget
    }
    fn admit(&self, extra: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let used = state
            .0
            .checked_add(extra)
            .filter(|n| *n <= state.1)
            .ok_or(Error::Contract(
                "direct analysis temporary disk quota reached",
            ))?;
        state.0 = used;
        Ok(())
    }
    fn release(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        state.0 = state
            .0
            .checked_sub(bytes)
            .expect("owned direct disk charge");
    }
    pub(crate) fn used(&self) -> u64 {
        self.state.lock().unwrap().0
    }
    #[cfg(test)]
    pub(super) fn cap_remaining_for_test(&self, extra: u64) {
        let mut state = self.state.lock().unwrap();
        state.1 = state.0.checked_add(extra).unwrap();
    }
}

/// Declared after file handles and delete-on-drop owners. Charges are released
/// only after the native artifact is confirmed gone; a failed deletion stays charged.
pub(crate) struct DiskCharge {
    budget: DiskBudget,
    bytes: Mutex<u64>,
    guard: Option<sortsize_core::resources::ArtifactGuard>,
    path: PathBuf,
    owner: Option<tempfile::NamedTempFile>,
}
impl DiskCharge {
    pub(crate) fn new(budget: DiskBudget, owner: tempfile::NamedTempFile) -> Result<Self> {
        let path = owner.path();
        let guard = if let Some(governor) = &budget.governor {
            let free = path
                .parent()
                .and_then(sortsize_core::resources::native_free_disk)
                .ok_or(Error::Contract("direct temporary volume unavailable"))?;
            Some(
                governor
                    .guard_artifact_group(path, free, 0)
                    .ok_or(Error::Contract("global direct temporary disk budget"))?,
            )
        } else {
            None
        };
        Ok(Self {
            budget,
            bytes: Mutex::new(0),
            guard,
            path: path.to_owned(),
            owner: Some(owner),
        })
    }
    pub(crate) fn admit(&self, extra: u64) -> Result<()> {
        let mut bytes = self.bytes.lock().unwrap();
        let total = bytes.checked_add(extra).ok_or_else(index_limit)?;
        self.budget.admit(extra)?;
        if self
            .guard
            .as_ref()
            .is_some_and(|guard| !guard.admit(total, None))
        {
            self.budget.release(extra);
            return Err(Error::Contract(
                "global direct temporary disk quota reached",
            ));
        }
        *bytes = total;
        Ok(())
    }
}
impl Drop for DiskCharge {
    fn drop(&mut self) {
        let bytes = *self.bytes.get_mut().unwrap();
        if let Some(owner) = self.owner.take() {
            let _ = owner.close();
        }
        let gone = matches!(std::fs::symlink_metadata(&self.path), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        if let Some(guard) = self.guard.take() {
            guard.finish(if gone { 0 } else { bytes });
            if gone && let Some(governor) = &self.budget.governor {
                governor.forget_deleted_artifact_group(&self.path);
            }
        }
        if gone {
            self.budget.release(bytes);
        }
    }
}

struct MemoryChunk {
    bytes: Vec<u8>,
    _charge: Reservation,
}
#[derive(Clone)]
enum Chunk {
    Memory { start: u64, data: Arc<MemoryChunk> },
    Disk { start: u64, offset: u64 },
}
impl Chunk {
    fn start(&self) -> u64 {
        match self {
            Self::Memory { start, .. } | Self::Disk { start, .. } => *start,
        }
    }
}

pub(super) struct Spool {
    chunks: Vec<Chunk>,
    metadata: Vec<Reservation>,
    file: Arc<Mutex<File>>,
    pub(super) length: u64,
    disk_length: u64,
    memory_bytes: u64,
    governor: Governor,
    disk_charge: Arc<DiskCharge>,
}
impl Spool {
    pub(super) fn new(scratch: &Path, disk: DiskBudget, governor: Governor) -> Result<Self> {
        let owner = tempfile::Builder::new()
            .prefix("direct-records-")
            .tempfile_in(scratch)?;
        let file = owner.reopen()?;
        let disk_charge = Arc::new(DiskCharge::new(disk, owner)?);
        Ok(Self {
            chunks: Vec::new(),
            metadata: Vec::new(),
            file: Arc::new(Mutex::new(file)),
            length: 0,
            disk_length: 0,
            memory_bytes: 0,
            governor,
            disk_charge,
        })
    }
    pub(super) fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let length = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or_else(spool::invalid)?;
        if self.chunks.len() == self.chunks.capacity() {
            let extra = 1024;
            let charge = self
                .governor
                .reserve(
                    Category::Margin,
                    (extra * std::mem::size_of::<Chunk>() + 128) as u64,
                    None,
                )
                .ok_or_else(index_limit)?;
            self.chunks
                .try_reserve_exact(extra)
                .map_err(|_| index_limit())?;
            self.metadata.push(charge);
        }
        // Leave room for mutable arènes, name indexes and other live inventories.
        let charge = (self.memory_bytes + bytes.len() as u64
            <= self.governor.usage().limits.categories[0] / 2)
            .then(|| {
                self.governor
                    .reserve(Category::Cache, (bytes.len() + 128) as u64, None)
            })
            .flatten();
        let chunk = if let Some(charge) = charge {
            let mut data = Vec::new();
            data.try_reserve_exact(bytes.len())
                .map_err(|_| index_limit())?;
            data.extend_from_slice(bytes);
            self.memory_bytes += charge.bytes();
            Chunk::Memory {
                start: self.length,
                data: Arc::new(MemoryChunk {
                    bytes: data,
                    _charge: charge,
                }),
            }
        } else {
            self.disk_charge.admit(bytes.len() as u64)?;
            let mut file = self.file.lock().unwrap();
            file.seek(SeekFrom::Start(self.disk_length))?;
            file.write_all(bytes)?;
            let chunk = Chunk::Disk {
                start: self.length,
                offset: self.disk_length,
            };
            self.disk_length += bytes.len() as u64;
            chunk
        };
        self.chunks.push(chunk);
        self.length = length;
        Ok(())
    }
    pub(super) fn with_record<T>(
        &self,
        position: u64,
        read: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let index = self
            .chunks
            .partition_point(|chunk| chunk.start() <= position)
            .checked_sub(1)
            .ok_or_else(spool::invalid)?;
        let chunk = &self.chunks[index];
        let relative = position - chunk.start();
        match chunk {
            Chunk::Memory { data, .. } => {
                let offset = usize::try_from(relative).map_err(|_| spool::invalid())?;
                let prefix = data
                    .bytes
                    .get(offset..offset + 2)
                    .ok_or_else(spool::invalid)?;
                let length = usize::from(u16::from_le_bytes(prefix.try_into().unwrap()));
                read(
                    data.bytes
                        .get(offset + 2..offset + 2 + length)
                        .ok_or_else(spool::invalid)?,
                )
            }
            Chunk::Disk { offset, .. } => {
                let mut file = self.file.lock().unwrap();
                file.seek(SeekFrom::Start(offset + relative))?;
                let mut bytes = Vec::new();
                spool::read_record(&mut *file, &mut bytes)?;
                read(&bytes)
            }
        }
    }
    pub(super) fn replay(&self) -> Result<Replay> {
        let charge = self
            .governor
            .reserve(
                Category::Transport,
                (self.chunks.len() * std::mem::size_of::<Chunk>() + spool::MAX_BATCH + 128) as u64,
                None,
            )
            .ok_or_else(index_limit)?;
        Ok(Replay {
            chunks: self.chunks.clone(),
            file: self.file.clone(),
            next: 0,
            length: self.length,
            bytes: Vec::new(),
            _charge: charge,
            _disk_charge: self.disk_charge.clone(),
        })
    }
}

pub struct Replay {
    chunks: Vec<Chunk>,
    file: Arc<Mutex<File>>,
    next: usize,
    length: u64,
    bytes: Vec<u8>,
    _charge: Reservation,
    _disk_charge: Arc<DiskCharge>,
}
impl Iterator for Replay {
    type Item = Result<ReplayBatch>;
    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.chunks.get(self.next)?;
        let end = self
            .chunks
            .get(self.next + 1)
            .map_or(self.length, Chunk::start);
        let result = (|| {
            let bytes = match chunk {
                Chunk::Memory { data, .. } => &data.bytes[..],
                Chunk::Disk { offset, start } => {
                    let length = usize::try_from(end - start).map_err(|_| spool::invalid())?;
                    if length > spool::MAX_BATCH + 4 {
                        return Err(spool::invalid());
                    }
                    self.bytes.resize(length, 0);
                    let mut file = self.file.lock().unwrap();
                    file.seek(SeekFrom::Start(*offset))?;
                    file.read_exact(&mut self.bytes)?;
                    &self.bytes[..]
                }
            };
            let prefix: [u8; 4] = bytes
                .get(..4)
                .ok_or_else(spool::invalid)?
                .try_into()
                .unwrap();
            if u32::from_le_bytes(prefix) as usize != bytes.len() - 4 {
                return Err(spool::invalid());
            }
            spool::batch(&bytes[4..])
        })();
        self.next = if result.is_err() {
            self.chunks.len()
        } else {
            self.next + 1
        };
        Some(result)
    }
}
