//! Bounded runs, eight-way merges, at most 32 levels of run descriptors.
use super::row::Record;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub(super) const MAX_RECORD_BYTES: usize = 1024 * 1024;
const FAN_IN: usize = 8;
const LEVELS: usize = 32;
const BUFFER: usize = 64 * 1024;

pub(super) struct Sorter {
    directory: PathBuf,
    prefix: &'static str,
    serial: u64,
    levels: Vec<Vec<PathBuf>>,
    records: Vec<Record>,
    bytes: usize,
    budget: usize,
}

impl Sorter {
    pub fn new(directory: &Path, prefix: &'static str, budget: usize) -> Self {
        Self {
            directory: directory.into(),
            prefix,
            serial: 0,
            levels: (0..LEVELS).map(|_| Vec::new()).collect(),
            records: Vec::new(),
            bytes: 0,
            budget,
        }
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        let cost = record.memory_bytes();
        if !self.records.is_empty()
            && (self.bytes.saturating_add(cost) > self.budget || self.records.len() >= 4096)
        {
            self.flush()?;
        }
        self.bytes = self
            .bytes
            .checked_add(cost)
            .ok_or_else(|| io::Error::other("Budget de tri dépassé"))?;
        self.records.push(record);
        Ok(())
    }
    fn next_path(&mut self) -> io::Result<PathBuf> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Trop de segments de tri"))?;
        Ok(self
            .directory
            .join(format!("{}-{}", self.prefix, self.serial)))
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.records.is_empty() {
            return Ok(());
        }
        self.records.sort_unstable();
        let path = self.next_path()?;
        #[cfg(test)]
        super::faults::check("create")?;
        let mut out = BufWriter::with_capacity(BUFFER, File::create_new(&path)?);
        for record in &self.records {
            write_record(&mut out, record)?;
        }
        #[cfg(test)]
        super::faults::check("flush")?;
        out.flush()?;
        drop(out);
        self.records.clear();
        self.bytes = 0;
        self.admit(path, 0)
    }
    fn admit(&mut self, mut path: PathBuf, mut level: usize) -> io::Result<()> {
        loop {
            if level >= LEVELS {
                return Err(io::Error::other("Trop de niveaux de tri externe"));
            }
            self.levels[level].push(path);
            if self.levels[level].len() < FAN_IN {
                return Ok(());
            }
            let paths = std::mem::take(&mut self.levels[level]);
            path = self.merge(&paths)?;
            level += 1;
        }
    }
    fn merge(&mut self, paths: &[PathBuf]) -> io::Result<PathBuf> {
        #[cfg(test)]
        super::faults::check("merge")?;
        let path = self.next_path()?;
        #[cfg(test)]
        super::faults::check("create")?;
        let mut out = BufWriter::with_capacity(BUFFER, File::create_new(&path)?);
        let mut input = Merge::new(paths)?;
        while let Some(record) = input.next_record()? {
            write_record(&mut out, &record)?;
        }
        #[cfg(test)]
        super::faults::check("flush")?;
        out.flush()?;
        drop(out);
        drop(input);
        for old in paths {
            #[cfg(test)]
            super::faults::check("remove")?;
            fs::remove_file(old)?;
        }
        Ok(path)
    }
    pub fn finish(mut self) -> io::Result<PathBuf> {
        self.flush()?;
        let mut paths: Vec<_> = self.levels.iter_mut().flat_map(std::mem::take).collect();
        while paths.len() > 1 {
            let count = paths.len().min(FAN_IN);
            let group: Vec<_> = paths.drain(..count).collect();
            paths.push(self.merge(&group)?);
        }
        match paths.pop() {
            Some(path) => Ok(path),
            None => {
                let path = self.next_path()?;
                File::create_new(&path)?;
                Ok(path)
            }
        }
    }
}

fn write_record(writer: &mut impl Write, record: &Record) -> io::Result<()> {
    #[cfg(test)]
    super::faults::check("write")?;
    let data = serde_json::to_vec(record).map_err(io::Error::other)?;
    if data.len() >= MAX_RECORD_BYTES {
        return Err(io::Error::other(
            "Observation trop grande pour le spool (1 Mio)",
        ));
    }
    writer.write_all(&data)?;
    writer.write_all(b"\n")
}

pub(super) struct Reader(BufReader<File>);
impl Reader {
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(test)]
        super::faults::check("read")?;
        Ok(Self(BufReader::with_capacity(BUFFER, File::open(path)?)))
    }
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        let mut data = Vec::new();
        self.0
            .by_ref()
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut data)?;
        if data.is_empty() {
            return Ok(None);
        }
        if data.len() > MAX_RECORD_BYTES || data.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Spool tronqué ou observation trop grande",
            ));
        }
        let record: Record = serde_json::from_slice(&data).map_err(io::Error::other)?;
        if record.cells.len() != super::COLUMNS.len() || record.rank > 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Schéma du spool invalide",
            ));
        }
        Ok(Some(record))
    }
}

struct Merge {
    readers: Vec<Reader>,
    heap: BinaryHeap<Reverse<(Record, usize)>>,
}
impl Merge {
    fn new(paths: &[PathBuf]) -> io::Result<Self> {
        let mut readers = Vec::new();
        let mut heap = BinaryHeap::new();
        for (index, path) in paths.iter().enumerate() {
            let mut reader = Reader::open(path)?;
            if let Some(record) = reader.next_record()? {
                heap.push(Reverse((record, index)));
            }
            readers.push(reader);
        }
        Ok(Self { readers, heap })
    }
    fn next_record(&mut self) -> io::Result<Option<Record>> {
        let Some(Reverse((record, index))) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(next) = self.readers[index].next_record()? {
            self.heap.push(Reverse((next, index)));
        }
        Ok(Some(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valid_json_with_invalid_record_shape_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("invalid");
        fs::write(
            &path,
            b"{\"key\":\"\",\"rank\":0,\"cells\":[],\"incomplete\":false}\n",
        )
        .unwrap();
        assert_eq!(
            Reader::open(&path)
                .unwrap()
                .next_record()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[cfg(test)]
#[test]
fn truncated_segment_during_merge_propagates_and_can_be_removed() {
    let temp = tempfile::tempdir().unwrap();
    let good = temp.path().join("good");
    let bad = temp.path().join("bad");
    let mut file = File::create_new(&good).unwrap();
    write_record(&mut file, &Record::empty("entry")).unwrap();
    drop(file);
    fs::write(&bad, b"{\"key\":\"truncated").unwrap();
    let mut sorter = Sorter::new(temp.path(), "merge", 1);
    assert_eq!(
        sorter.merge(&[good, bad]).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    temp.close().unwrap();
}
