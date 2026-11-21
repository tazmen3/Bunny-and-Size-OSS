//! Linear hashing for unfinished listings that exceed the RAM name-index budget.
//!
//! The head table and records have separate, admitted temporary files. At load
//! above four records per bucket, one bucket is split. There is no whole-table
//! rebuild, fixed entry ceiling, or ever-growing average chain on large folders.
use super::*;

const INITIAL_BUCKETS: u64 = 1024;
const LOAD: u64 = 4;
const RECORD_BYTES: u64 = 20;
// A fresh head table and at most one record and bucket split per observation.
pub(super) const MAX_BATCH_DISK_BYTES: usize =
    (INITIAL_BUCKETS * 8 + 512 * (RECORD_BYTES + 8)) as usize;

pub(super) struct SeenDisk {
    heads: Mutex<File>,
    records: Mutex<File>,
    base: u64,
    split: u64,
    entries: u64,
    heads_charge: storage::DiskCharge,
    records_charge: storage::DiskCharge,
}
impl SeenDisk {
    pub(super) fn new(scratch: &Path, disk: DiskBudget) -> Result<Self> {
        let heads_owner = tempfile::Builder::new()
            .prefix("direct-name-heads-")
            .tempfile_in(scratch)?;
        let heads = heads_owner.reopen()?;
        let heads_charge = storage::DiskCharge::new(disk.clone(), heads_owner)?;
        heads_charge.admit(INITIAL_BUCKETS * 8)?;
        heads.set_len(INITIAL_BUCKETS * 8)?;
        let records_owner = tempfile::Builder::new()
            .prefix("direct-name-records-")
            .tempfile_in(scratch)?;
        let records = records_owner.reopen()?;
        let records_charge = storage::DiskCharge::new(disk, records_owner)?;
        Ok(Self {
            heads: Mutex::new(heads),
            records: Mutex::new(records),
            base: INITIAL_BUCKETS,
            split: 0,
            entries: 0,
            heads_charge,
            records_charge,
        })
    }
    fn bucket(&self, hash: u64) -> u64 {
        let bucket = hash & (self.base - 1);
        if bucket < self.split {
            hash & (self.base * 2 - 1)
        } else {
            bucket
        }
    }
    fn read_record(file: &mut File, pointer: u64, entries: u64) -> Result<[u8; 20]> {
        let position = pointer
            .checked_sub(1)
            .filter(|p| p.is_multiple_of(RECORD_BYTES) && *p / RECORD_BYTES < entries)
            .ok_or_else(spool::invalid)?;
        file.seek(SeekFrom::Start(position))?;
        let mut record = [0u8; 20];
        file.read_exact(&mut record)?;
        Ok(record)
    }
    fn find(&self, hash: u64) -> Result<Option<(u64, u32)>> {
        let mut head = [0u8; 8];
        {
            let mut heads = self.heads.lock().unwrap();
            heads.seek(SeekFrom::Start(self.bucket(hash) * 8))?;
            heads.read_exact(&mut head)?;
        }
        let mut pointer = u64::from_le_bytes(head);
        let mut remaining = self.entries;
        let mut records = self.records.lock().unwrap();
        while pointer != 0 {
            if remaining == 0 {
                return Err(spool::invalid());
            }
            remaining -= 1;
            let record = Self::read_record(&mut records, pointer, self.entries)?;
            if u64::from_le_bytes(record[..8].try_into().unwrap()) == hash {
                return Ok(Some((
                    pointer,
                    u32::from_le_bytes(record[8..12].try_into().unwrap()),
                )));
            }
            pointer = u64::from_le_bytes(record[12..].try_into().unwrap());
        }
        Ok(None)
    }
    pub(super) fn head(&self, hash: u64) -> Result<Option<u32>> {
        Ok(self.find(hash)?.map(|(_, id)| id))
    }
    pub(super) fn insert(&mut self, hash: u64, id: u32) -> Result<()> {
        // Full hashes are unique in this table. Distinct names sharing a full
        // hash still chain through Node::same_hash, so changing the head is exact.
        if let Some((pointer, _)) = self.find(hash)? {
            let records = self.records.get_mut().unwrap();
            records.seek(SeekFrom::Start(pointer - 1 + 8))?;
            records.write_all(&id.to_le_bytes())?;
            return Ok(());
        }
        let bucket = self.bucket(hash);
        let heads = self.heads.get_mut().unwrap();
        let records = self.records.get_mut().unwrap();
        heads.seek(SeekFrom::Start(bucket * 8))?;
        let mut previous = [0u8; 8];
        heads.read_exact(&mut previous)?;
        let mut bytes = [0u8; 20];
        bytes[..8].copy_from_slice(&hash.to_le_bytes());
        bytes[8..12].copy_from_slice(&id.to_le_bytes());
        bytes[12..].copy_from_slice(&previous);
        let position = self
            .entries
            .checked_mul(RECORD_BYTES)
            .ok_or_else(spool::invalid)?;
        self.records_charge.admit(RECORD_BYTES)?;
        records.seek(SeekFrom::Start(position))?;
        records.write_all(&bytes)?;
        heads.seek(SeekFrom::Start(bucket * 8))?;
        heads.write_all(&(position + 1).to_le_bytes())?;
        self.entries += 1;
        if self.entries > LOAD * (self.base + self.split) {
            self.split_one()?;
        }
        Ok(())
    }
    fn split_one(&mut self) -> Result<()> {
        let doubled = self.base.checked_mul(2).ok_or_else(spool::invalid)?;
        let new_bucket = self.base + self.split;
        // Admission precedes every change to this split. Errors subsequently
        // poison acquisition; the tree's RAM undo overlay retains its old view.
        self.heads_charge.admit(8)?;
        let heads = self.heads.get_mut().unwrap();
        let records = self.records.get_mut().unwrap();
        heads.seek(SeekFrom::Start(new_bucket * 8))?;
        heads.write_all(&0u64.to_le_bytes())?;
        heads.seek(SeekFrom::Start(self.split * 8))?;
        let mut bytes = [0u8; 8];
        heads.read_exact(&mut bytes)?;
        let mut pointer = u64::from_le_bytes(bytes);
        let mut partition = [0u64; 2];
        let mut remaining = self.entries;
        while pointer != 0 {
            if remaining == 0 {
                return Err(spool::invalid());
            }
            remaining -= 1;
            let record = Self::read_record(records, pointer, self.entries)?;
            let hash = u64::from_le_bytes(record[..8].try_into().unwrap());
            let next = u64::from_le_bytes(record[12..].try_into().unwrap());
            let side = usize::from(hash & self.base != 0);
            records.seek(SeekFrom::Start(pointer - 1 + 12))?;
            records.write_all(&partition[side].to_le_bytes())?;
            partition[side] = pointer;
            pointer = next;
        }
        heads.seek(SeekFrom::Start(self.split * 8))?;
        heads.write_all(&partition[0].to_le_bytes())?;
        heads.seek(SeekFrom::Start(new_bucket * 8))?;
        heads.write_all(&partition[1].to_le_bytes())?;
        self.split += 1;
        if self.split == self.base {
            self.base = doubled;
            self.split = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_rounds_keep_all_hash_heads_and_update_full_hash_collisions() {
        let scratch = tempfile::tempdir().unwrap();
        let budget = DiskBudget::new(4 * 1024 * 1024);
        let mut seen = SeenDisk::new(scratch.path(), budget.clone()).unwrap();
        let hash = |i: u64| i.wrapping_mul(0x9e3779b97f4a7c15);
        for i in 0..20_000u32 {
            seen.insert(hash(u64::from(i)), i).unwrap();
        }
        assert_eq!(seen.base, 4096);
        assert!(seen.split > 0);
        let unique = seen.entries;
        for i in (0..20_000u32).step_by(257) {
            seen.insert(hash(u64::from(i)), i + 20_000).unwrap();
        }
        assert_eq!(seen.entries, unique);
        for i in 0..20_000u32 {
            assert_eq!(
                seen.head(hash(u64::from(i))).unwrap(),
                Some(if i % 257 == 0 { i + 20_000 } else { i })
            );
        }
        assert_eq!(seen.head(hash(20_001)).unwrap(), None);
        assert_eq!(
            budget.used(),
            8 * (seen.base + seen.split) + RECORD_BYTES * seen.entries
        );
        drop(seen);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn colliding_initial_buckets_split_without_losing_exact_hashes() {
        let scratch = tempfile::tempdir().unwrap();
        let mut seen = SeenDisk::new(scratch.path(), DiskBudget::new(1024 * 1024)).unwrap();
        for i in 0..64u32 {
            seen.insert(u64::from(i) * INITIAL_BUCKETS, i).unwrap();
        }
        for i in 1..4500u64 {
            seen.insert(i.wrapping_mul(0x9e3779b97f4a7c15), i as u32 + 1000)
                .unwrap();
        }
        assert!(seen.split > 0);
        for i in 0..64u32 {
            assert_eq!(seen.head(u64::from(i) * INITIAL_BUCKETS).unwrap(), Some(i));
        }
    }
    #[test]
    fn split_quota_failure_preserves_existing_hash_heads() {
        let scratch = tempfile::tempdir().unwrap();
        let limit = INITIAL_BUCKETS * 8 + (INITIAL_BUCKETS * LOAD + 1) * RECORD_BYTES;
        let mut seen = SeenDisk::new(scratch.path(), DiskBudget::new(limit)).unwrap();
        for i in 0..INITIAL_BUCKETS * LOAD {
            seen.insert(i, i as u32).unwrap();
        }
        assert!(seen.insert(INITIAL_BUCKETS * LOAD, 99999).is_err());
        assert_eq!(seen.base, INITIAL_BUCKETS);
        assert_eq!(seen.split, 0);
        for i in 0..INITIAL_BUCKETS * LOAD {
            assert_eq!(seen.head(i).unwrap(), Some(i as u32));
        }
    }
}
