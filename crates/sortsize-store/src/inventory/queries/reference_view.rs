//! Completed exhaustive selections over the same certified, pinned RO model.
//! Only bounded identities live in RAM; no new final result file is created.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const MAX_MEMBERS: u64 = 200;
pub(super) const MIN_MEMORY: u64 = 2 * 1024 * 1024;

pub(super) fn require_snapshot(store: &Store) -> Result<()> {
    store.require_integrity()?;
    if !store.db.is_readonly("main")?
        || store.db.is_autocommit()
        || store.db.transaction_state(Some("main"))? != rusqlite::TransactionState::Read
    {
        return Err(Error::Contract(
            "query reference requires pinned RO snapshot",
        ));
    }
    Ok(())
}

pub(super) struct ReferenceView {
    owner: PathBuf,
    nonce: u64,
    inventory: InventoryId,
    source: SourceId,
    generation: GenerationId,
    members: Vec<(u64, EntryId)>,
    counts: QueryCounts,
}
impl ReferenceView {
    pub(super) fn build(query: &Query<'_>, cancel: &CancellationToken) -> Result<Self> {
        if !query.model_source
            || !query.reference_certificate
            || !query.evaluation_done
            || query.counts.cancelled
            || query.counts.matches > MAX_MEMBERS
            || query._memory.bytes() < MIN_MEMORY
        {
            return Err(Error::Contract("unfinished or uncertified query reference"));
        }
        require_snapshot(query._store)?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let nonce = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Contract("query reference identity exhausted"))?;
        let mut statement = query
            .db
            .as_ref()
            .ok_or(Error::Contract("query reference projection missing"))?
            .prepare("SELECT sort,id FROM results ORDER BY sort,id LIMIT 201")?;
        let mut rows = statement.query([])?;
        let mut source = query
            ._store
            .db
            .prepare("SELECT id FROM generation_a_rows WHERE rank=?1")?;
        let mut members = Vec::with_capacity(query.counts.matches as usize);
        while let Some(row) = rows.next()? {
            cancelled(cancel)?;
            if members.len() as u64 >= query.counts.matches {
                return Err(Error::Contract("query reference count mismatch"));
            }
            let raw: Vec<u8> = row.get(0)?;
            let rank = u64::from_be_bytes(
                raw.try_into()
                    .map_err(|_| Error::Contract("invalid query reference rank"))?,
            );
            if members.last().is_some_and(|(last, _)| rank <= *last) {
                return Err(Error::Contract("unordered query reference"));
            }
            let id = EntryId(number(row, 1)?);
            let canonical = source.query_row([rank_value(rank)?], |row| number(row, 0))?;
            if canonical != id.0 {
                return Err(Error::Contract("query reference identity mismatch"));
            }
            members.push((rank, id));
        }
        if members.len() as u64 != query.counts.matches {
            return Err(Error::Contract("query reference count mismatch"));
        }
        cancelled(cancel)?;
        require_snapshot(query._store)?;
        Ok(Self {
            owner: query
                ._store
                .directory
                .join(format!("query-reference-{nonce:016x}")),
            nonce,
            inventory: query._store.inventory,
            source: query._store.source,
            generation: query.generation,
            members,
            counts: query.counts,
        })
    }
    fn validate_source(&self, store: &Store, generation: GenerationId) -> Result<()> {
        if generation != self.generation
            || store.inventory != self.inventory
            || store.source != self.source
        {
            return Err(Error::Contract("foreign query reference context"));
        }
        require_snapshot(store)
    }
    fn cursor(&self, position: usize) -> Cursor {
        let mut sort = self.nonce.to_be_bytes().to_vec();
        sort.extend((position as u64).to_be_bytes());
        Cursor {
            owner: self.owner.clone(),
            sort,
            id: self.members[position].1,
        }
    }
    fn anchor(&self, cursor: &Cursor) -> Result<usize> {
        if cursor.owner != self.owner
            || cursor.sort.len() != 16
            || cursor.sort[..8] != self.nonce.to_be_bytes()
        {
            return Err(Error::Contract("foreign query reference cursor"));
        }
        let position = usize::try_from(u64::from_be_bytes(cursor.sort[8..].try_into().unwrap()))
            .map_err(|_| Error::Contract("invalid query reference cursor"))?;
        if self.members.get(position).map(|m| m.1) != Some(cursor.id) {
            return Err(Error::Contract("invalid query reference cursor"));
        }
        Ok(position)
    }
    fn row(&self, source: &mut rusqlite::Statement<'_>, position: usize) -> Result<Row> {
        let (rank, id) = self.members[position];
        source
            .query_row([rank_value(rank)?], |row| {
                if number(row, 0)? != id.0 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                let raw = row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?;
                let path = decode_recorded_path(raw)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?
                    .into_owned();
                Ok(Row {
                    key: EntryKey {
                        inventory: self.inventory,
                        generation: self.generation,
                        entry: id,
                    },
                    path,
                    kind: match row.get::<_, i64>(2)? {
                        0 => Kind::Unknown,
                        1 => Kind::File,
                        2 => Kind::Directory,
                        3 => Kind::Symlink,
                        4 => Kind::Special,
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    },
                    logical: opt_number(row, 3)?,
                })
            })
            .map_err(Error::from)
    }
}
impl PersistentQuery for ReferenceView {
    fn indexed(&self) -> bool {
        false
    }
    fn evaluate(&self, store: &Store) -> Result<QueryCounts> {
        self.validate_source(store, self.generation)?;
        Ok(self.counts)
    }
    fn page(
        &self,
        store: &Store,
        generation: GenerationId,
        cursor: Option<&Cursor>,
        reverse: bool,
    ) -> Result<Page> {
        self.validate_source(store, generation)?;
        let anchor = cursor.map(|c| self.anchor(c)).transpose()?;
        let mut position = if reverse {
            anchor.and_then(|p| p.checked_sub(1))
        } else {
            Some(anchor.map_or(0, |p| p + 1))
        };
        let mut source = store
            .db
            .prepare("SELECT id,path,kind,logical FROM generation_a_rows WHERE rank=?1")?;
        let mut rows = Vec::new();
        let mut first = None;
        let mut next = None;
        let mut bytes = 0;
        let mut more = false;
        let limit = 512 * 1024 - 200 * std::mem::size_of::<Row>() - 1024;
        while let Some(index) = position.filter(|p| *p < self.members.len()) {
            let row = self.row(&mut source, index)?;
            let cost = row.path.capacity() + 128;
            if rows.len() == 200 || bytes + cost > limit {
                more = true;
                break;
            }
            bytes += cost;
            rows.push(row);
            next = Some(self.cursor(index));
            if first.is_none() {
                first = next.clone();
            }
            position = if reverse {
                index.checked_sub(1)
            } else {
                Some(index + 1)
            };
        }
        if reverse {
            rows.reverse();
        }
        Ok(Page {
            first: if reverse { next.clone() } else { first.clone() },
            rows,
            next: if reverse { first } else { next },
            more,
            accounted_bytes: bytes,
        })
    }
    fn visit(
        &self,
        store: &Store,
        generation: GenerationId,
        visitor: &mut dyn FnMut(Row) -> Result<bool>,
    ) -> Result<()> {
        self.validate_source(store, generation)?;
        let mut source = store
            .db
            .prepare("SELECT id,path,kind,logical FROM generation_a_rows WHERE rank=?1")?;
        for position in 0..self.members.len() {
            if !visitor(self.row(&mut source, position)?)? {
                break;
            }
        }
        Ok(())
    }
    fn identities(&self, store: &Store, after: Option<u64>) -> Result<Vec<(u64, EntryId)>> {
        self.validate_source(store, self.generation)?;
        Ok(self
            .members
            .iter()
            .copied()
            .filter(|(rank, _)| after.is_none_or(|after| *rank > after))
            .collect())
    }
}
fn rank_value(rank: u64) -> Result<i64> {
    i64::try_from(rank).map_err(|_| Error::Contract("query reference rank overflow"))
}
fn cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Contract("query reference creation cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::generation_a::{self, CertifiedGeneration};

    fn fixture(n: u64, long_root: bool) -> (tempfile::TempDir, Store, CertifiedGeneration) {
        let (temp, source) =
            super::super::tests::fixture_with(n, u64::MAX - 1000, |i| Observation {
                name: native_path(Path::new(&format!("file-{i:08}.TXT"))),
                kind: Kind::File,
                logical: Some(i),
                allocated: None,
                modified: None,
                metadata_known: true,
            });
        let mut root = temp.path().join("offline");
        if long_root {
            for _ in 0..16 {
                root.push("r".repeat(240));
            }
        }
        let directory = temp.path().join("published");
        generation_a::publish(
            &source,
            &directory,
            GenerationId(1),
            &root,
            16 * 1024 * 1024,
            &Default::default(),
        )
        .unwrap();
        let cap =
            CertifiedGeneration::open(&directory, GenerationId(1), &root, &Default::default())
                .unwrap();
        (temp, source, cap)
    }
    fn search() -> SearchQuery {
        SearchQuery {
            name: Some("file-".into()),
            ..Default::default()
        }
    }
    fn evaluation_only(query: &mut Query<'_>) {
        while !query.evaluation_done {
            assert!(!query.advance(&Default::default()).unwrap());
        }
        assert!(!query.evaluation_finished());
        assert!(!query.complete());
        assert!(query.db.is_some());
        assert!(query.native.is_none());
    }
    fn ids(query: &Query<'_>) -> Vec<EntryId> {
        let mut result = Vec::new();
        let mut cursor = None;
        loop {
            let page = query.page(cursor.as_ref()).unwrap();
            assert!(page.rows.len() <= 200 && page.accounted_bytes <= 512 * 1024);
            result.extend(page.rows.iter().map(|row| row.key.entry));
            if !page.more {
                return result;
            }
            cursor = page.next;
        }
    }
    #[test]
    fn exhaustive_boundaries_preserve_calls_counts_and_legacy_sync() {
        for n in [0, 1, 200, 201] {
            let (temp, source, cap) = fixture(n, false);
            let mut query = cap
                .start_query(&search(), temp.path(), 4 * 1024 * 1024)
                .unwrap();
            evaluation_only(&mut query);
            assert_eq!(query.counts.evaluated, n + 1);
            assert_eq!(query.predicate_evaluated(), n + 1);
            assert_eq!(query.counts.matches, n);
            while !query.advance(&Default::default()).unwrap() {}
            assert!(query.complete());
            assert!(!query.indexed());
            assert_eq!(query.references_generation(), n <= 200);
            assert_eq!(query.new_result_sync_required(), n > 200);
            assert_eq!(query.native.is_some(), n > 200);
            if n <= 200 {
                assert_eq!(query._memory.bytes(), MIN_MEMORY);
                assert!(query.db.is_none() && query._file.is_none() && query._vfs.is_none());
                assert_eq!(query.temporary_usage().charged, 0);
            }
            assert_eq!(ids(&query).len() as u64, n);
            let baseline = source
                .query(
                    GenerationId(1),
                    cap.root(),
                    &search(),
                    temp.path(),
                    4 * 1024 * 1024,
                    &Default::default(),
                )
                .unwrap();
            assert!(!baseline.references_generation());
            assert!(baseline.new_result_sync_required() && baseline.native.is_some());
            assert_eq!(ids(&query), ids(&baseline));
        }
    }
    #[test]
    fn long_pages_foreign_and_tampered_cursors_keep_full_width_identities() {
        let (temp, _source, cap) = fixture(200, true);
        let query = cap
            .query(&search(), temp.path(), 4 * 1024 * 1024, &Default::default())
            .unwrap();
        assert!(query.references_generation() && !query.indexed());
        let first = query.page(None).unwrap();
        assert!(first.more && !first.rows.is_empty() && first.rows.len() < 200);
        assert!(
            first
                .rows
                .iter()
                .all(|row| row.key.entry.0 > i64::MAX as u64)
        );
        let next = query.page(first.next.as_ref()).unwrap();
        let back = query.page_before(next.first.as_ref().unwrap()).unwrap();
        assert_eq!(
            first.rows.iter().map(|r| r.key.entry).collect::<Vec<_>>(),
            back.rows.iter().map(|r| r.key.entry).collect::<Vec<_>>()
        );
        assert_eq!(ids(&query).len(), 200);
        let other = cap
            .query(&search(), temp.path(), 4 * 1024 * 1024, &Default::default())
            .unwrap();
        assert!(other.page(first.next.as_ref()).is_err());
        let mut tampered = first.next.unwrap();
        tampered.sort[0] ^= 1;
        assert!(query.page(Some(&tampered)).is_err());
        tampered.sort[0] ^= 1;
        tampered.id = EntryId(0);
        assert!(query.page(Some(&tampered)).is_err());
    }
    #[test]
    fn cancellation_keeps_the_existing_partial_projection_and_preview() {
        let (temp, _source, cap) = fixture(3, false);
        let mut query = cap
            .start_query(&search(), temp.path(), 4 * 1024 * 1024)
            .unwrap();
        evaluation_only(&mut query);
        let token = CancellationToken::default();
        token.cancel();
        assert!(query.advance(&token).unwrap());
        assert!(query.counts.cancelled && !query.complete());
        assert!(!query.references_generation() && query.db.is_some());
        assert_eq!(query.preview().unwrap().rows.len(), 3);
        assert_eq!(ids(&query).len(), 3);
    }
    #[test]
    fn smaller_admitted_lease_and_canonical_control_keep_the_synced_sequence() {
        let (temp, source, cap) = fixture(1, false);
        let mut query = cap
            .start_query(&search(), temp.path(), 4 * 1024 * 1024)
            .unwrap();
        evaluation_only(&mut query);
        // Deterministic lease simulation after its SQLite cache was shrunk;
        // no global limit or memory mode is changed by this test.
        assert!(query._memory.shrink_to(1024 * 1024));
        while !query.advance(&Default::default()).unwrap() {}
        assert!(query.complete() && query.native.is_some());
        assert!(!query.references_generation() && query.new_result_sync_required());
        let mut control = cap
            .as_store()
            .start_query(
                cap.generation(),
                cap.root(),
                &search(),
                temp.path(),
                4 * 1024 * 1024,
            )
            .unwrap();
        assert!(!control.reference_certificate);
        while !control.advance(&Default::default()).unwrap() {}
        assert!(!control.references_generation() && control.native.is_some());
        assert!(control.new_result_sync_required());
        let mut legacy = source
            .start_query(
                cap.generation(),
                cap.root(),
                &search(),
                temp.path(),
                4 * 1024 * 1024,
            )
            .unwrap();
        assert!(legacy.attach_reference_certificate(&cap).is_err());
        assert!(!legacy.reference_certificate);
    }
    #[test]
    fn lost_snapshot_or_invalid_references_never_replace_the_projection() {
        let (temp, source, cap) = fixture(3, false);
        assert!(require_snapshot(&source).is_err());
        let mut query = cap
            .start_query(&search(), temp.path(), 4 * 1024 * 1024)
            .unwrap();
        evaluation_only(&mut query);
        query
            .db
            .as_ref()
            .unwrap()
            .execute(
                "UPDATE results SET id=?1 WHERE sort=(SELECT min(sort) FROM results)",
                [unsigned(42)],
            )
            .unwrap();
        assert!(query.advance(&Default::default()).is_err());
        assert!(!query.references_generation() && !query.complete() && query.db.is_some());
        cap.as_store().db.execute_batch("ROLLBACK").unwrap();
        assert!(query.advance(&Default::default()).is_err());
        assert!(!query.references_generation() && !query.complete() && query.db.is_some());
    }
}
