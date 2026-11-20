//! Fresh replay into a canonical writer during explicit save.
use super::*;
use crate::direct_save::{ReplayBatch, check_cancelled};
use sortsize_core::{
    CancellationToken,
    resources::{disk_limit, native_free_disk},
};
use std::ops::Deref;

/// A fresh writer whose entries can only be appended by the ordered replay.
/// Expose canonical reads, but never the ordinary replacement or recovery APIs.
pub(crate) struct DirectReplay(Store);

impl Deref for DirectReplay {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.0
    }
}

impl Store {
    pub(crate) fn create_direct_replay(
        directory: &Path,
        context: AcquisitionContext,
        cancel: &CancellationToken,
    ) -> Result<DirectReplay> {
        let free = native_free_disk(directory.parent().ok_or(Error::Contract("replay parent"))?)
            .ok_or(Error::Contract("replay volume free space unavailable"))?;
        let mut store = Self::create_canonical_admitted(
            directory,
            context.inventory,
            context.source,
            Durability::Reconstructible,
            Self::reserve_memory()?,
            disk_limit(free),
            None,
        )?;
        let stop = cancel.clone();
        store
            .db
            .progress_handler(1000, Some(move || stop.is_cancelled()))?;
        Ok(DirectReplay(store))
    }
}

impl DirectReplay {
    pub(crate) fn enable_properties(&mut self) -> Result<()> {
        self.0.enable_properties()
    }

    pub(crate) fn create_generation_guarded(
        &mut self,
        context: AcquisitionContext,
        proof: Option<&[u8]>,
    ) -> Result<EntryKey> {
        self.0.create_generation_guarded(context, proof)
    }

    pub(crate) fn set_collection_options(
        &mut self,
        context: AcquisitionContext,
        allocation: bool,
        owners: bool,
    ) -> Result<()> {
        self.0.set_collection_options(context, allocation, owners)
    }

    pub(crate) fn record_acquisition(
        &mut self,
        context: AcquisitionContext,
        root: &Path,
        started: i64,
    ) -> Result<()> {
        self.0.record_acquisition(context, root, started)
    }

    pub(crate) fn record_measurement(
        &mut self,
        generation: GenerationId,
        root: &Path,
        started: i64,
        measured: i64,
    ) -> Result<()> {
        self.0
            .record_measurement(generation, root, started, measured)
    }

    /// A directory is appended only below an earlier, currently owned entry.
    /// One attempt and strict inserts keep that invariant for the entire tree.
    /// There is no older branch to invalidate, even when an EOF is incomplete.
    pub(crate) fn replay_direct_chunk(
        &mut self,
        context: AcquisitionContext,
        batches: &[ReplayBatch],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _disk = self.0.disk_admission(Store::WRITE_RESERVE)?;
        let tx = self.0.db.transaction()?;
        generation(&tx, context)?;
        if tx.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? != 6 {
            return Err(Error::Contract(
                "direct replay requires canonical properties schema",
            ));
        }
        let g = unsigned(context.generation.0);
        let mut next = tx.query_row(
            "SELECT next_entry FROM generations WHERE id=?1",
            [g],
            |row| number(row, 0),
        )?;
        {
            let mut writer = ChunkWriter::new(&tx, context)?;
            for replay in batches {
                check_cancelled(cancel)?;
                next = writer.append(replay, next)?;
            }
        }
        tx.execute(
            "UPDATE generations SET next_entry=?2 WHERE id=?1",
            params![g, unsigned(next)],
        )?;
        dirty(&tx, context.generation)?;
        check_cancelled(cancel)?;
        metrics::commit(tx, Phase::IngestCommit)?;
        Ok(())
    }

    pub(crate) fn finish_direct_replay(
        &mut self,
        context: AcquisitionContext,
        cancel: &CancellationToken,
    ) -> Result<ScopeCoverage> {
        check_cancelled(cancel)?;
        let unfinished: bool = self.0.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE generation=?1 AND kind=0 AND (key IS NULL OR listing NOT IN (1,2)))",
            [unsigned(context.generation.0)],
            |r| r.get(0),
        )?;
        if unfinished {
            return Err(Error::Contract("direct replay has unfinished directories"));
        }
        // The append-only replay proved ownership before every write. The
        // canonical fold's joins retain every contribution from that tree.
        loop {
            check_cancelled(cancel)?;
            if self.0.consolidate_step_inner(context, MAX_BATCH, false)? {
                break;
            }
        }
        check_cancelled(cancel)?;
        self.0.seal(context)
    }

    pub(crate) fn checkpoint_direct_replay(&self) -> Result<()> {
        let _disk = self.disk_admission(Store::WRITE_RESERVE)?;
        let busy: i64 = self
            .db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(Error::Contract("direct replay checkpoint busy"));
        }
        Ok(())
    }
}

struct ChunkWriter<'a> {
    context: AcquisitionContext,
    directory: rusqlite::Statement<'a>,
    begin: rusqlite::Statement<'a>,
    entry: rusqlite::Statement<'a>,
    contribution: rusqlite::Statement<'a>,
    evidence: rusqlite::Statement<'a>,
    properties: rusqlite::Statement<'a>,
    enqueue: rusqlite::Statement<'a>,
    progress: rusqlite::Statement<'a>,
    modified: rusqlite::Statement<'a>,
    eof_evidence: rusqlite::Statement<'a>,
}

impl<'a> ChunkWriter<'a> {
    fn new(db: &'a Connection, context: AcquisitionContext) -> Result<Self> {
        Ok(Self {
            context,
            directory: db.prepare(
                "SELECT e.parent,t.key,t.next_sequence,t.listing,p.key FROM tasks t
                 JOIN entries e ON e.generation=t.generation AND e.id=t.directory
                 LEFT JOIN tasks p ON p.generation=e.generation AND p.directory=e.parent AND p.kind=0 AND p.attempt=e.attempt
                 WHERE t.generation=?1 AND t.directory=?2 AND t.kind=0 AND e.present=1 AND e.kind=2 AND t.parent_attempt=e.attempt
                   AND ((e.id=?3 AND e.parent IS NULL AND e.attempt=?3) OR (e.parent<e.id AND e.attempt=?4))",
            )?,
            begin: db.prepare(
                "UPDATE tasks SET key=?3,attempt=?4,owner=?4,branch=?4 WHERE generation=?1 AND directory=?2 AND kind=0 AND key IS NULL",
            )?,
            entry: db.prepare(
                "INSERT INTO entries(generation,id,parent,name,kind,logical,allocated,modified_s,modified_ns,attempt,present,metadata_known) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,?11)",
            )?,
            contribution: db.prepare("INSERT INTO contributions VALUES(?1,?2,?3,?4,?5,?6,?7)")?,
            evidence: db.prepare("INSERT INTO entry_evidence VALUES(?1,?2,?3)")?,
            properties: db.prepare(properties::INSERT)?,
            enqueue: db.prepare(
                "INSERT INTO tasks(generation,directory,next_sequence,parent_attempt) VALUES(?1,?2,?3,?4)",
            )?,
            progress: db.prepare(
                "UPDATE tasks SET next_sequence=?3,listing=?4,reconciled=?5 WHERE generation=?1 AND directory=?2 AND kind=0",
            )?,
            modified: db.prepare("UPDATE entries SET modified_s=?3,modified_ns=?4 WHERE generation=?1 AND id=?2")?,
            eof_evidence: db.prepare(
                "INSERT INTO entry_evidence VALUES(?1,?2,?3) ON CONFLICT(generation,id) DO UPDATE SET proof=excluded.proof",
            )?,
        })
    }

    fn cursor(&mut self, header: BatchHeader) -> Result<BatchCursor> {
        let a = header.attempt;
        let g = unsigned(self.context.generation.0);
        let directory = unsigned(a.task.directory.0);
        let zero = unsigned(0);
        let one = unsigned(a.attempt.0);
        let task = self
            .directory
            .query_row(params![g, directory, zero, one], |row| {
                Ok((
                    opt_number(row, 0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    number(row, 2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })
            .optional()?;
        let Some((parent, key, sequence, listing, parent_key)) = task else {
            return Err(Error::Batch(BatchRejection::StaleAttempt));
        };
        if let Some(parent) = parent
            && parent_key.as_deref()
                != Some(attempt(direct_attempt(self.context, EntryId(parent))).as_slice())
        {
            return Err(Error::Batch(BatchRejection::StaleAttempt));
        }
        let expected_key = attempt(a);
        if key.is_none() && header.sequence == 0 && sequence == 0 && listing == 0 {
            if self
                .begin
                .execute(params![g, directory, expected_key, one])?
                == 1
            {
                return Ok(BatchCursor::start(a));
            }
            return Err(Error::Batch(BatchRejection::StaleAttempt));
        }
        if key.as_deref() != Some(expected_key.as_slice()) {
            return Err(Error::Batch(BatchRejection::StaleAttempt));
        }
        let end = match listing {
            0 => None,
            1 => Some(ListingEnd::Exhausted),
            2 => Some(ListingEnd::Incomplete),
            _ => return Err(Error::Contract("invalid direct replay listing state")),
        };
        BatchCursor::from_committed(a, sequence, end)
            .ok_or(Error::Contract("invalid direct replay sequence"))
    }

    fn append(&mut self, replay: &ReplayBatch, first: u64) -> Result<u64> {
        let count = replay.observations.len();
        if replay.parent.0 >= first
            || (replay.end == Some(ListingEnd::Exhausted)) != replay.directory.is_some()
            || replay.evidence.len() != count
            || replay.properties.len() != count
        {
            return Err(Error::Contract("direct replay parent or evidence mismatch"));
        }
        let end_id = first
            .checked_add(count as u64)
            .ok_or(Error::Contract("direct replay entry identity exhausted"))?;
        let children = replay
            .observations
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind == Kind::Directory)
            .map(|(index, row)| (EntryId(first + index as u64), &row.name));
        if !children.eq(replay.children.iter().map(|(id, name)| (*id, name))) {
            return Err(Error::Contract("direct replay directory identity mismatch"));
        }
        let a = direct_attempt(self.context, replay.parent);
        let header = BatchHeader {
            attempt: a,
            sequence: replay.sequence,
            end: replay.end,
        };
        let cursor = self.cursor(header)?;
        let BatchDecision::Apply { next } = cursor.prepare(header).map_err(Error::Batch)? else {
            return Err(Error::Contract("duplicate direct replay batch"));
        };
        let batch = DirectoryBatch {
            header,
            observations: &replay.observations,
            evidence: &replay.evidence,
            properties: &replay.properties,
            directory: replay.directory.as_ref(),
        };
        Store::validate_directory_batch(&batch, true, true)?;

        let generation = self.context.generation;
        let g = unsigned(generation.0);
        let parent = unsigned(replay.parent.0);
        let one = unsigned(a.attempt.0);
        let zero = unsigned(0);
        for (index, ((row, evidence), properties)) in replay
            .observations
            .iter()
            .zip(&replay.evidence)
            .zip(&replay.properties)
            .enumerate()
        {
            let id = first + index as u64;
            let kind = row.kind as i64;
            let logical = row.logical.filter(|_| row.metadata_known).map(unsigned);
            let allocated = row.allocated.filter(|_| row.metadata_known);
            let modified = row.modified.filter(|_| row.metadata_known);
            // A duplicate name or ID violates the canonical UNIQUE/PK and
            // rolls back the chunk. No observation can replace an older one.
            self.entry.execute(params![
                g,
                unsigned(id),
                parent,
                row.name.encode(),
                kind,
                logical,
                allocated.map(unsigned),
                modified.map(|x| x.0),
                modified.map(|x| i64::from(x.1)),
                one,
                row.metadata_known
            ])?;
            self.contribution.execute(params![
                g,
                unsigned(id),
                parent,
                one,
                logical,
                row.metadata_known,
                kind
            ])?;
            let proof = evidence
                .as_ref()
                .filter(|_| row.metadata_known)
                .map(|proof| proof.encode());
            self.evidence.execute(params![g, unsigned(id), proof])?;
            properties::write(
                &mut self.properties,
                generation,
                EntryId(id),
                Some(properties),
                allocated,
            )?;
            if row.kind == Kind::Directory {
                self.enqueue.execute(params![g, unsigned(id), zero, one])?;
            }
        }
        let end = match next.listing_end() {
            None => 0,
            Some(ListingEnd::Exhausted) => 1,
            Some(ListingEnd::Incomplete) => 2,
        };
        // A stable EOF can reconcile immediately. Every child of this parent
        // was inserted by its one attempt, so there are no stale rows to sweep.
        self.progress.execute(params![
            g,
            parent,
            unsigned(next.next_sequence()),
            end,
            batch.directory.is_some()
        ])?;
        if let Some(stamp) = batch.directory {
            let modified = stamp.modified.and_then(properties::instant);
            self.modified.execute(params![
                g,
                parent,
                modified.map(|x| x.0),
                modified.map(|x| x.1)
            ])?;
            self.eof_evidence.execute(params![
                g,
                parent,
                differential::NativeEvidence::from_stamp(stamp).encode()
            ])?;
        }
        Ok(end_id)
    }
}

fn direct_attempt(context: AcquisitionContext, directory: EntryId) -> AttemptKey {
    AttemptKey {
        task: TaskKey {
            context,
            directory,
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    }
}

pub(crate) fn install_family(from: &Path, to: &Path) -> Result<()> {
    match std::fs::symlink_metadata(to) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            generation_a::publish_directory(from, to)
        }
        Err(error) => Err(error.into()),
        Ok(_) => Err(Error::Contract("direct save destination already exists")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::acquisition::{Guard, NodeProperties};

    #[test]
    fn fresh_replay_checks_ownership_and_rolls_back_before_reconciling_each_eof() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join("child")).unwrap();
        let guard = Guard::open(&source, Default::default()).unwrap();
        let context = AcquisitionContext {
            inventory: InventoryId(301),
            source: SourceId(302),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let cancel = CancellationToken::default();
        let mut replay =
            Store::create_direct_replay(&temp.path().join("candidate"), context, &cancel).unwrap();
        replay.enable_properties().unwrap();
        replay.create_generation_guarded(context, None).unwrap();
        let child = NativeName::Opaque(b"child".to_vec());
        let root_batch = || ReplayBatch {
            parent: EntryId(0),
            sequence: 0,
            end: None,
            observations: vec![Observation {
                name: child.clone(),
                kind: Kind::Directory,
                logical: None,
                allocated: None,
                modified: None,
                metadata_known: true,
            }],
            evidence: vec![None],
            properties: vec![NodeProperties::default()],
            directory: None,
            children: vec![(EntryId(1), child.clone())],
        };
        let child_batch = |sequence| ReplayBatch {
            parent: EntryId(1),
            sequence,
            end: Some(ListingEnd::Exhausted),
            observations: vec![Observation {
                name: NativeName::Opaque(b"file".to_vec()),
                kind: Kind::File,
                logical: Some(9),
                allocated: None,
                modified: None,
                metadata_known: true,
            }],
            evidence: vec![None],
            properties: vec![NodeProperties::default()],
            directory: Some(guard.inspect(&source.join("child")).unwrap()),
            children: Vec::new(),
        };
        assert!(
            replay
                .replay_direct_chunk(context, &[root_batch(), child_batch(1)], &cancel)
                .is_err()
        );
        assert_eq!(replay.entry_count(context.generation).unwrap(), 1);
        replay
            .replay_direct_chunk(context, &[root_batch(), child_batch(0)], &cancel)
            .unwrap();
        assert!(replay.finish_direct_replay(context, &cancel).is_err());
        let end = ReplayBatch {
            parent: EntryId(0),
            sequence: 1,
            end: Some(ListingEnd::Exhausted),
            observations: Vec::new(),
            evidence: Vec::new(),
            properties: Vec::new(),
            directory: Some(guard.evidence().1.clone()),
            children: Vec::new(),
        };
        replay
            .replay_direct_chunk(context, &[end], &cancel)
            .unwrap();
        assert_eq!(
            replay.finish_direct_replay(context, &cancel).unwrap(),
            ScopeCoverage::Complete
        );
        assert_eq!(replay.entry_count(context.generation).unwrap(), 3);
        assert_eq!(replay.totals(context.generation).unwrap().logical, Some(9));
    }
}
