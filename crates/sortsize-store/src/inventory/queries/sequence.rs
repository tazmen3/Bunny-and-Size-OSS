//! Immutable, checksummed fixed-width records. The only mutable files belong to
//! builders; unfinished files cannot be opened as sequences or published.
use super::workspace::{Artifact, Workspace};
use super::*;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicU64, Ordering},
};
const CHUNK: u64 = 1024;
const HEADER: u64 = 64;
const HASH: u64 = 32;
fn length<const W: usize>(count: u64) -> Result<u64> {
    count
        .checked_mul(W as u64)
        .and_then(|n| n.checked_add(count.div_ceil(CHUNK).checked_mul(HASH)?))
        .and_then(|n| n.checked_add(HEADER))
        .ok_or(Error::Contract("sequence length overflow"))
}
fn digest<const W: usize>(nonce: u64, block: u64, bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(nonce.to_be_bytes());
    h.update((W as u64).to_be_bytes());
    h.update(block.to_be_bytes());
    h.update(bytes);
    h.finalize().into()
}
pub(super) struct Writer<const W: usize> {
    artifact: Artifact,
    count: u64,
    nonce: u64,
    block: u64,
    written: u64,
    buffer: Vec<u8>,
}
impl<const W: usize> Writer<W> {
    pub fn new(space: &Arc<Workspace>) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let nonce = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Contract("sequence identity exhausted"))?;
        let mut artifact = space.file()?;
        let _admission = artifact.admit_growth(HEADER)?;
        artifact.write(&[0; 64])?;
        Ok(Self {
            artifact,
            count: 0,
            nonce,
            block: 0,
            written: HEADER,
            buffer: Vec::with_capacity(W * CHUNK as usize),
        })
    }
    pub fn count(&self) -> u64 {
        self.count
    }
    pub fn push(&mut self, record: &[u8; W]) -> Result<()> {
        self.buffer.extend_from_slice(record);
        self.count = self
            .count
            .checked_add(1)
            .ok_or(Error::Contract("sequence count overflow"))?;
        if self.buffer.len() == W * CHUNK as usize {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let end = self
            .written
            .checked_add(self.buffer.len() as u64 + HASH)
            .ok_or(Error::Contract("sequence length overflow"))?;
        let _admission = self.artifact.admit_growth(end)?;
        self.artifact.write(&self.buffer)?;
        self.artifact
            .write(&digest::<W>(self.nonce, self.block, &self.buffer))?;
        self.written = end;
        self.block += 1;
        self.buffer.clear();
        Ok(())
    }
    pub fn seal(mut self, durable: bool) -> Result<Sequence<W>> {
        self.flush()?;
        let mut header = [0u8; 64];
        header[..8].copy_from_slice(b"BNSSEQ01");
        header[8..16].copy_from_slice(&(W as u64).to_be_bytes());
        header[16..24].copy_from_slice(&self.count.to_be_bytes());
        header[24..32].copy_from_slice(&self.nonce.to_be_bytes());
        let hash: [u8; 32] = Sha256::digest(&header[..32]).into();
        header[32..].copy_from_slice(&hash);
        self.artifact.file_mut().seek(SeekFrom::Start(0))?;
        self.artifact.write(&header)?;
        if durable {
            self.artifact.sync()?;
        }
        let artifact = Arc::new(self.artifact);
        let reader = Reader::<W>::open(artifact.clone(), self.count, self.nonce)?;
        drop(reader);
        Ok(Sequence {
            artifact,
            count: self.count,
            nonce: self.nonce,
            cache: RefCell::new(None),
        })
    }
}
pub(super) struct Sequence<const W: usize> {
    artifact: Arc<Artifact>,
    pub count: u64,
    nonce: u64,
    cache: RefCell<Option<Reader<W>>>,
}
impl<const W: usize> Sequence<W> {
    pub fn path(&self) -> &Path {
        self.artifact.path()
    }
    pub fn nonce(&self) -> u64 {
        self.nonce
    }
    pub fn at(&self, position: u64) -> Result<[u8; W]> {
        let mut cache = self.cache.borrow_mut();
        if cache.is_none() {
            *cache = Some(self.reader()?);
        }
        cache.as_mut().unwrap().at(position)
    }
    pub fn check(&self) -> Result<()> {
        if self.artifact.file().metadata()?.len() != length::<W>(self.count)? {
            return Err(Error::Contract("invalid sequence length"));
        }
        Ok(())
    }
    pub fn reader(&self) -> Result<Reader<W>> {
        Reader::open(self.artifact.clone(), self.count, self.nonce)
    }
    pub fn planned_bytes(count: u64) -> Result<u64> {
        length::<W>(count)
    }
}
pub(super) struct Reader<const W: usize> {
    file: File,
    _artifact: Arc<Artifact>,
    count: u64,
    nonce: u64,
    block: Option<u64>,
    buffer: Vec<u8>,
    pub position: u64,
}
impl<const W: usize> Reader<W> {
    fn open(artifact: Arc<Artifact>, count: u64, nonce: u64) -> Result<Self> {
        let mut file = artifact.reopen()?;
        let mut header = [0u8; 64];
        file.read_exact(&mut header)?;
        let valid = &header[..8] == b"BNSSEQ01"
            && header[8..16] == (W as u64).to_be_bytes()
            && header[16..24] == count.to_be_bytes()
            && header[24..32] == nonce.to_be_bytes()
            && header[32..] == Sha256::digest(&header[..32])[..];
        if !valid {
            return Err(Error::Contract("invalid sequence header"));
        }
        let reader = Self {
            file,
            _artifact: artifact,
            count,
            nonce,
            block: None,
            buffer: Vec::new(),
            position: 0,
        };
        reader.check()?;
        Ok(reader)
    }
    pub fn check(&self) -> Result<()> {
        if self.file.metadata()?.len() != length::<W>(self.count)? {
            return Err(Error::Contract("invalid sequence length"));
        }
        Ok(())
    }
    pub fn at(&mut self, position: u64) -> Result<[u8; W]> {
        if position >= self.count {
            return Err(Error::Contract("sequence position outside result"));
        }
        let block = position / CHUNK;
        if self.block != Some(block) {
            // A short read must not leave partially overwritten bytes labelled
            // as the previous valid cache block.
            self.block = None;
            let n = (self.count - block * CHUNK).min(CHUNK) as usize * W;
            self.buffer.resize(n, 0);
            let offset = block
                .checked_mul(CHUNK * W as u64 + HASH)
                .and_then(|n| n.checked_add(HEADER))
                .ok_or(Error::Contract("sequence offset overflow"))?;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(&mut self.buffer)?;
            let mut stored = [0u8; 32];
            self.file.read_exact(&mut stored)?;
            if stored != digest::<W>(self.nonce, block, &self.buffer) {
                self.block = None;
                return Err(Error::Contract("sequence checksum mismatch"));
            }
            self.block = Some(block);
        }
        let start = (position % CHUNK) as usize * W;
        Ok(self.buffer[start..start + W].try_into().unwrap())
    }
    pub fn next(&mut self) -> Result<Option<[u8; W]>> {
        if self.position == self.count {
            return Ok(None);
        }
        let v = self.at(self.position)?;
        self.position += 1;
        Ok(Some(v))
    }
}
