//! Exact global size order: bounded segments of (known, size, native rank),
//! followed by an immutable sequence of full-width EntryIds. No source FS I/O.
use super::sequence::{Reader, Sequence, Writer};
use super::*;
use sortsize_core::resources::{Category, Governor, Reservation};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
};
const BLOCK: usize = 32_768;
const FANIN: usize = 16;
type Key = [u8; 17];
struct Build {
    next: u64,
    records: Vec<Key>,
    runs: VecDeque<Sequence<17>>,
}
struct Merge {
    pending: VecDeque<Sequence<17>>,
    outputs: VecDeque<Sequence<17>>,
    active: Option<Group>,
}
struct Group {
    readers: Vec<Reader<17>>,
    _sources: Vec<Sequence<17>>,
    heap: BinaryHeap<Reverse<(Key, usize)>>,
    writer: Writer<17>,
}
struct Publish {
    reader: Reader<17>,
    _source: Sequence<17>,
    writer: Writer<8>,
}
struct Collect {
    after: Option<u64>,
    writer: Writer<8>,
}
enum Phase {
    Collect(Collect),
    Build(Build),
    Merge(Merge),
    Publish(Publish),
}
pub struct SizeOrder<'q, 's> {
    query: &'q Query<'s>,
    sizes: rusqlite::Statement<'q>,
    _memory: Reservation,
    descending: bool,
    phase: Option<Phase>,
    result: Option<Sequence<8>>,
    materialized: Option<Sequence<8>>,
    failed: bool,
}
impl<'s> Query<'s> {
    pub fn order_by_size<'q>(
        &'q self,
        descending: bool,
        scratch: &Path,
        max_bytes: u64,
    ) -> Result<SizeOrder<'q, 's>> {
        if !self.finished
            || (self.native.is_none() && (!self.references_generation() || self.counts.cancelled))
        {
            return Err(Error::Contract(
                "size order requires a finished ranked query",
            ));
        }
        if max_bytes != self.workspace.usage().limit
            || scratch.canonicalize()?.as_path() != self.workspace.path().parent().unwrap()
        {
            return Err(Error::Contract(
                "size order must share its query workspace and quota",
            ));
        }
        let memory = Governor::global()
            .reserve(Category::Queries, 2 * 1024 * 1024, None)
            .ok_or(Error::Contract("size order memory budget"))?;
        let sql = format!(
            "SELECT id,logical,present FROM entries WHERE generation=?1 AND id IN ({})",
            (2..=257)
                .map(|n| format!("?{n}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let sizes = self._store.db.prepare(&sql)?;
        Ok(SizeOrder {
            query: self,
            sizes,
            _memory: memory,
            descending,
            phase: Some(if self.references_generation() {
                Phase::Collect(Collect {
                    after: None,
                    writer: Writer::new(&self.workspace)?,
                })
            } else {
                Phase::Build(Build {
                    next: 0,
                    records: Vec::with_capacity(BLOCK),
                    runs: VecDeque::new(),
                })
            }),
            result: None,
            materialized: None,
            failed: false,
        })
    }
}
impl SizeOrder<'_, '_> {
    pub fn preparation_phase(&self) -> &'static str {
        match &self.phase {
            Some(Phase::Collect(_)) => "native-identities",
            Some(Phase::Build(_)) => "segments",
            Some(Phase::Merge(_)) => "merge",
            Some(Phase::Publish(_)) => "publication",
            None if self.result.is_some() => "ready",
            None => "failed",
        }
    }
    pub fn temporary_usage(&self) -> TemporaryUsage {
        self.query.temporary_usage()
    }
    pub fn advance(&mut self, cancel: &CancellationToken) -> Result<bool> {
        if self.result.is_some() {
            return Ok(true);
        }
        if self.failed {
            return Err(Error::Contract("failed size order cannot resume"));
        }
        let result = self.step(cancel);
        if matches!(result, Ok(true)) {
            assert!(self._memory.shrink_to(128 * 1024));
        }
        if result.is_err() {
            self.phase = None;
            self.failed = true;
        }
        result
    }
    fn step(&mut self, cancel: &CancellationToken) -> Result<bool> {
        if cancel.is_cancelled() {
            return Err(Error::Contract("size order cancelled"));
        }
        let phase = self
            .phase
            .take()
            .ok_or(Error::Contract("missing size order phase"))?;
        let space = &self.query.workspace;
        let phase = match phase {
            Phase::Collect(mut collect) => {
                let ResultStorage::Persistent(result) = &self.query.result else {
                    return Err(Error::Contract("missing persistent query source"));
                };
                let ids = result.identities(self.query._store, collect.after)?;
                if ids.is_empty() {
                    if collect.writer.count() != self.query.counts.matches {
                        return Err(Error::Contract(
                            "persistent query materialization count mismatch",
                        ));
                    }
                    self.materialized = Some(collect.writer.seal(true)?);
                    Phase::Build(Build {
                        next: 0,
                        records: Vec::with_capacity(BLOCK),
                        runs: VecDeque::new(),
                    })
                } else {
                    for (rank, id) in ids {
                        if cancel.is_cancelled() {
                            return Err(Error::Contract("size order cancelled"));
                        }
                        collect.writer.push(&unsigned(id.0))?;
                        collect.after = Some(rank);
                    }
                    Phase::Collect(collect)
                }
            }
            Phase::Build(mut build) => {
                let native = self
                    .query
                    .native
                    .as_ref()
                    .or(self.materialized.as_ref())
                    .ok_or(Error::Contract("missing native identity sequence"))?;
                let count = (native.count - build.next).min(256) as usize;
                if count > 0 {
                    // Fetch only ID and size through the original source connection.
                    // Bounded IN parameters avoid one statement/ancestor walk per row.
                    let mut wanted = Vec::with_capacity(count);
                    for i in 0..count {
                        wanted.push((u64::from_be_bytes(native.at(build.next + i as u64)?), i));
                    }
                    wanted.sort_unstable();
                    let mut values = Vec::with_capacity(257);
                    values.push(rusqlite::types::Value::Blob(
                        unsigned(self.query.generation.0).to_vec(),
                    ));
                    for (id, _) in &wanted {
                        values.push(rusqlite::types::Value::Blob(unsigned(*id).to_vec()));
                    }
                    values.resize(257, rusqlite::types::Value::Null);
                    let mut rows = self.sizes.query(rusqlite::params_from_iter(values))?;
                    let mut keys = vec![[0u8; 17]; count];
                    let mut seen = vec![false; count];
                    while let Some(row) = rows.next()? {
                        if cancel.is_cancelled() {
                            return Err(Error::Contract("size order cancelled"));
                        }
                        let id = number(row, 0)?;
                        let index = wanted
                            .binary_search_by_key(&id, |x| x.0)
                            .map_err(|_| Error::Contract("unexpected sort identity"))?;
                        let index = wanted[index].1;
                        if seen[index] || !row.get::<_, bool>(2)? {
                            return Err(Error::Contract("invalid projected sort identity"));
                        }
                        seen[index] = true;
                        let logical = opt_number(row, 1)?;
                        keys[index][0] = u8::from(logical.is_none());
                        if let Some(n) = logical {
                            keys[index][1..9].copy_from_slice(
                                &if self.descending { !n } else { n }.to_be_bytes(),
                            );
                        }
                        keys[index][9..]
                            .copy_from_slice(&(build.next + index as u64).to_be_bytes());
                    }
                    if seen.iter().any(|v| !*v) {
                        return Err(Error::Contract("missing projected sort identity"));
                    }
                    build.records.extend(keys);
                    build.next += count as u64;
                }
                if build.records.len() == BLOCK
                    || (build.next == native.count && !build.records.is_empty())
                {
                    build.records.sort_unstable();
                    let mut writer = Writer::new(space)?;
                    for key in &build.records {
                        if cancel.is_cancelled() {
                            return Err(Error::Contract("size order cancelled"));
                        }
                        writer.push(key)?;
                    }
                    build.runs.push_back(writer.seal(false)?);
                    build.records.clear();
                }
                if build.next == native.count {
                    if build.runs.is_empty() {
                        self.result = Some(Writer::new(space)?.seal(true)?);
                        return Ok(true);
                    }
                    Phase::Merge(Merge {
                        pending: build.runs,
                        outputs: VecDeque::new(),
                        active: None,
                    })
                } else {
                    Phase::Build(build)
                }
            }
            Phase::Merge(mut merge) => {
                if merge.active.is_none() {
                    if merge.pending.is_empty() {
                        merge.pending = std::mem::take(&mut merge.outputs);
                    }
                    if merge.pending.len() == 1 && merge.outputs.is_empty() {
                        let source = merge.pending.pop_front().unwrap();
                        let reader = source.reader()?;
                        self.phase = Some(Phase::Publish(Publish {
                            reader,
                            _source: source,
                            writer: Writer::new(space)?,
                        }));
                        return Ok(false);
                    }
                    let mut sources = Vec::with_capacity(FANIN);
                    for _ in 0..FANIN {
                        if let Some(s) = merge.pending.pop_front() {
                            sources.push(s);
                        } else {
                            break;
                        }
                    }
                    let mut readers = Vec::with_capacity(sources.len());
                    let mut heap = BinaryHeap::new();
                    for source in &sources {
                        let mut r = source.reader()?;
                        if let Some(k) = r.next()? {
                            heap.push(Reverse((k, readers.len())));
                        }
                        readers.push(r);
                    }
                    merge.active = Some(Group {
                        readers,
                        _sources: sources,
                        heap,
                        writer: Writer::new(space)?,
                    });
                }
                let group = merge.active.as_mut().unwrap();
                let started = Instant::now();
                for _ in 0..4096 {
                    if cancel.is_cancelled() {
                        return Err(Error::Contract("size order cancelled"));
                    }
                    let Some(Reverse((key, index))) = group.heap.pop() else {
                        break;
                    };
                    group.writer.push(&key)?;
                    if let Some(next) = group.readers[index].next()? {
                        group.heap.push(Reverse((next, index)));
                    }
                    if started.elapsed() >= Duration::from_millis(20) {
                        break;
                    }
                }
                if group.heap.is_empty() {
                    let group = merge.active.take().unwrap();
                    let output = group.writer.seal(false)?;
                    // Readers keep artifacts charged. Close them before deleting
                    // the consumed segments, on Windows as well as Unix.
                    drop(group.readers);
                    drop(group._sources);
                    merge.outputs.push_back(output);
                }
                Phase::Merge(merge)
            }
            Phase::Publish(mut publish) => {
                let native = self
                    .query
                    .native
                    .as_ref()
                    .or(self.materialized.as_ref())
                    .ok_or(Error::Contract("missing native identity sequence"))?;
                let started = Instant::now();
                let mut done = false;
                for _ in 0..256 {
                    if cancel.is_cancelled() {
                        return Err(Error::Contract("size order cancelled"));
                    }
                    let Some(key) = publish.reader.next()? else {
                        done = true;
                        break;
                    };
                    let rank = u64::from_be_bytes(key[9..].try_into().unwrap());
                    if key[0] > 1 {
                        return Err(Error::Contract("invalid size order key"));
                    }
                    publish.writer.push(&native.at(rank)?)?;
                    if started.elapsed() >= Duration::from_millis(20) {
                        break;
                    }
                }
                if done {
                    if publish.writer.count() != native.count {
                        return Err(Error::Contract("size order count mismatch"));
                    }
                    let result = publish.writer.seal(true)?;
                    if cancel.is_cancelled() {
                        return Err(Error::Contract("size order cancelled"));
                    }
                    self.result = Some(result);
                    return Ok(true);
                }
                Phase::Publish(publish)
            }
        };
        self.phase = Some(phase);
        Ok(false)
    }
    pub fn page(&self, cursor: Option<&Cursor>, reverse: bool) -> Result<Page> {
        self.query.sequence_page(
            self.result
                .as_ref()
                .ok_or(Error::Contract("size order not complete"))?,
            cursor,
            reverse,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::queries::tests::fixture;
    fn collect(order: &SizeOrder<'_, '_>) -> Vec<EntryId> {
        let mut ids = Vec::new();
        let mut page = order.page(None, false).unwrap();
        loop {
            ids.extend(page.rows.iter().map(|r| r.key.entry));
            if !page.more {
                break;
            }
            page = order.page(page.next.as_ref(), false).unwrap();
        }
        ids
    }
    #[test]
    fn external_merge_preserves_previous_order_at_every_cancelled_phase() {
        let (tmp, store) = fixture(33_001);
        let q = store
            .query(
                GenerationId(1),
                &tmp.path().join("offline"),
                &Default::default(),
                tmp.path(),
                16 * 1024 * 1024,
                &Default::default(),
            )
            .unwrap();
        let mut old = q
            .order_by_size(false, tmp.path(), 16 * 1024 * 1024)
            .unwrap();
        while !old.advance(&Default::default()).unwrap() {}
        let expected: Vec<_> = (1..=33_001).chain(Some(0)).map(EntryId).collect();
        assert_eq!(collect(&old), expected);
        let retained = q.temporary_usage().charged;
        for target in [0, 1, 2] {
            let mut new = q.order_by_size(true, tmp.path(), 16 * 1024 * 1024).unwrap();
            loop {
                let phase = match new.phase.as_ref().unwrap() {
                    Phase::Collect(_) => 0,
                    Phase::Build(_) => 0,
                    Phase::Merge(_) => 1,
                    Phase::Publish(_) => 2,
                };
                if phase == target {
                    break;
                }
                assert!(!new.advance(&Default::default()).unwrap());
            }
            let cancel = CancellationToken::default();
            cancel.cancel();
            assert!(new.advance(&cancel).is_err());
            assert!(new.page(None, false).is_err());
            assert!(new.advance(&Default::default()).is_err());
            assert_eq!(q.temporary_usage().charged, retained);
            assert_eq!(collect(&old), expected);
        }
        let mut new = q.order_by_size(true, tmp.path(), 16 * 1024 * 1024).unwrap();
        while !new.advance(&Default::default()).unwrap() {}
        assert_eq!(
            collect(&new),
            (1..=33_001)
                .rev()
                .chain(Some(0))
                .map(EntryId)
                .collect::<Vec<_>>()
        );
        assert!(
            new.page(old.page(None, false).unwrap().next.as_ref(), false)
                .is_err()
        );
        assert_eq!(collect(&old), expected);
        assert!(q.temporary_usage().peak <= 16 * 1024 * 1024);
        assert_eq!(store.entry_count(GenerationId(1)).unwrap(), 33_002);
    }
    #[test]
    fn quota_short_write_and_sync_failures_preserve_old_view() {
        let (tmp, store) = fixture(900);
        let q = store
            .query(
                GenerationId(1),
                &tmp.path().join("offline"),
                &Default::default(),
                tmp.path(),
                8 * 1024 * 1024,
                &Default::default(),
            )
            .unwrap();
        let mut old = q.order_by_size(false, tmp.path(), 8 * 1024 * 1024).unwrap();
        while !old.advance(&Default::default()).unwrap() {}
        let expected = collect(&old);
        let retained = q.temporary_usage().charged;
        for fault in ["write", "sync", "quota"] {
            let mut new = q.order_by_size(true, tmp.path(), 8 * 1024 * 1024).unwrap();
            let held = if fault == "quota" {
                let artifact = q.workspace.file().unwrap();
                let available =
                    8 * 1024 * 1024 - (8 * 1024 * 1024 / 64) - q.temporary_usage().charged;
                artifact.grow(available).unwrap();
                Some(artifact)
            } else {
                q.workspace.inject(fault);
                None
            };
            loop {
                match new.advance(&Default::default()) {
                    Err(_) => break,
                    Ok(false) => (),
                    Ok(true) => panic!("injected failure did not refuse"),
                }
            }
            drop(held);
            drop(new);
            assert_eq!(q.temporary_usage().charged, retained);
            assert_eq!(collect(&old), expected);
            assert_eq!(store.entry_count(GenerationId(1)).unwrap(), 901);
        }
    }
}
