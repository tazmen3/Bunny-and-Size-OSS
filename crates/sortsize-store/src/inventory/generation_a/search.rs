//! Persistent exact selections and a shared Query API for every consumer.
use super::*;
use queries::{Cursor, Page, Query, QueryCounts};
use sortsize_core::{
    InventoryKind,
    analysis::{PreparedQuery, SearchQuery},
};
use std::sync::atomic::{AtomicU64, Ordering};

pub type GenerationQuery<'a> = Query<'a>;
impl CertifiedGeneration {
    pub fn start_query<'a>(
        &'a self,
        search: &SearchQuery,
        scratch: &Path,
        max_query_bytes: u64,
    ) -> Result<Query<'a>> {
        PreparedQuery::new(search).map_err(|_| Error::Contract("invalid search query"))?;
        let mut query = if PersistentResult::supports(search) {
            let result = PersistentResult::new(&self.store, search, self.rows, self.oversized)?;
            self.store.start_persistent_query(
                self.generation,
                &self.root,
                search,
                scratch,
                max_query_bytes,
                Box::new(result),
            )
        } else {
            self.store.start_model_query(
                self.generation,
                &self.root,
                search,
                scratch,
                max_query_bytes,
                self.oversized,
            )
        }?;
        query.attach_cancellation(self.query_cancel.clone());
        query.attach_reference_certificate(self)?;
        Ok(query)
    }
    pub fn query<'a>(
        &'a self,
        search: &SearchQuery,
        scratch: &Path,
        max_query_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<Query<'a>> {
        cancelled(cancel)?;
        let mut query = self.start_query(search, scratch, max_query_bytes)?;
        while !query.advance(cancel)? {}
        Ok(query)
    }
}

pub(crate) struct PersistentResult {
    selection: Selection,
    owner: PathBuf,
    population: u64,
    oversized: u64,
}
impl PersistentResult {
    pub(crate) fn supports(search: &SearchQuery) -> bool {
        search.name.is_none()
            && search.path.is_none()
            && search.glob.is_none()
            && search.modified_after.is_none()
            && search.modified_before.is_none()
    }
    fn new(store: &Store, search: &SearchQuery, population: u64, oversized: u64) -> Result<Self> {
        static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);
        let nonce = NEXT_QUERY
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Contract("generation A query identity exhausted"))?;
        Ok(Self {
            selection: Selection::new(search),
            owner: store.directory.join(format!("query-a-{nonce}")),
            population,
            oversized,
        })
    }
}
impl queries::PersistentQuery for PersistentResult {
    fn evaluate(&self, store: &Store) -> Result<QueryCounts> {
        let selection = &self.selection;
        let mut counts = QueryCounts {
            evaluated: self
                .population
                .checked_add(self.oversized)
                .ok_or(Error::Contract("generation A population overflow"))?,
            oversized_paths: self.oversized,
            ..Default::default()
        };
        if selection.kind.is_some() {
            counts.unknown_kinds = store.db.query_row(
                "SELECT coalesce(sum(count),0) FROM generation_a_counts WHERE kind=0",
                [],
                |r| count(r, 0),
            )?;
        }
        counts.matches = if selection.min.is_none()
            && selection.max.is_none()
            && selection.extension.is_none()
        {
            if selection.kind.is_none() {
                self.population
            } else {
                store.db.query_row(
                    &format!(
                        "SELECT coalesce(sum(count),0) FROM generation_a_counts WHERE {}",
                        selection.kind_condition()
                    ),
                    [],
                    |r| count(r, 0),
                )?
            }
        } else {
            store.db.query_row(
                &format!(
                    "SELECT count(*) FROM generation_a_rows WHERE {}",
                    selection.condition()
                ),
                rusqlite::params_from_iter(selection.parameters()),
                |r| count(r, 0),
            )?
        };
        if selection.min.is_some() || selection.max.is_some() {
            counts.unknown_sizes = store.db.query_row(
                &format!(
                    "SELECT coalesce(sum(unknown_size),0) FROM generation_a_counts WHERE {}",
                    selection.kind_condition()
                ),
                [],
                |r| count(r, 0),
            )?;
        }
        if selection.extension.is_some() {
            let mut native = selection.clone();
            native.extension = None;
            counts.native_text = store.db.query_row(
                &format!(
                    "SELECT count(*) FROM generation_a_rows WHERE {} AND ext_cs IS NULL",
                    native.condition()
                ),
                rusqlite::params_from_iter(native.parameters()),
                |r| count(r, 0),
            )?;
        }
        Ok(counts)
    }
    fn page(
        &self,
        store: &Store,
        generation: GenerationId,
        after: Option<&Cursor>,
        reverse: bool,
    ) -> Result<Page> {
        let anchor = if let Some(cursor) = after {
            if cursor.owner != self.owner || cursor.sort.len() != 8 {
                return Err(Error::Contract("foreign generation A cursor"));
            }
            let rank = u64::from_be_bytes(cursor.sort.as_slice().try_into().unwrap());
            let mut params = self.selection.parameters();
            params.push(rank_i64(rank)?.into());
            let anchored: Vec<u8> = store.db.query_row(
                &format!(
                    "SELECT id FROM generation_a_rows WHERE {} AND rank=?",
                    self.selection.condition()
                ),
                rusqlite::params_from_iter(params),
                |r| r.get(0),
            )?;
            if anchored != unsigned(cursor.id.0) {
                return Err(Error::Contract("invalid generation A cursor anchor"));
            }
            Some(rank)
        } else {
            None
        };
        let mut params = self.selection.parameters();
        let mut condition = self.selection.condition();
        if let Some(rank) = anchor {
            condition.push_str(if reverse {
                " AND rank<?"
            } else {
                " AND rank>?"
            });
            params.push(rank_i64(rank)?.into());
        }
        let sql = format!(
            "SELECT rank,id,path,kind,logical FROM generation_a_rows {} WHERE {} ORDER BY rank {} LIMIT 201",
            self.selection.page_index(),
            condition,
            if reverse { "DESC" } else { "ASC" }
        );
        let mut stmt = store.db.prepare(&sql)?;
        let mut source = stmt.query(rusqlite::params_from_iter(params))?;
        let mut rows = Vec::with_capacity(200);
        let mut first = None;
        let mut next = None;
        let mut more = false;
        let mut bytes = 200 * std::mem::size_of::<queries::Row>() + 1024;
        while let Some(row) = source.next()? {
            let item = decode_row(store, generation, row, 1)?;
            let cost = item.path.capacity() + 128;
            if rows.len() == 200 || bytes + cost > 512 * 1024 {
                more = true;
                break;
            }
            bytes += cost;
            let cursor = Cursor {
                owner: self.owner.clone(),
                sort: unsigned(count(row, 0)?).to_vec(),
                id: item.key.entry,
            };
            if first.is_none() {
                first = Some(cursor.clone());
            }
            next = Some(cursor);
            rows.push(item);
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
        visitor: &mut dyn FnMut(queries::Row) -> Result<bool>,
    ) -> Result<()> {
        let sql = format!(
            "SELECT id,path,kind,logical FROM generation_a_rows {} WHERE {} ORDER BY rank",
            self.selection.page_index(),
            self.selection.condition()
        );
        let mut stmt = store.db.prepare(&sql)?;
        let mut source = stmt.query(rusqlite::params_from_iter(self.selection.parameters()))?;
        while let Some(row) = source.next()? {
            if !visitor(decode_row(store, generation, row, 0)?)? {
                break;
            }
        }
        Ok(())
    }
    fn identities(&self, store: &Store, after: Option<u64>) -> Result<Vec<(u64, EntryId)>> {
        let mut condition = self.selection.condition();
        let mut params = self.selection.parameters();
        if let Some(rank) = after {
            condition.push_str(" AND rank>?");
            params.push(rank_i64(rank)?.into());
        }
        let mut stmt = store.db.prepare(&format!(
            "SELECT rank,id FROM generation_a_rows {} WHERE {} ORDER BY rank LIMIT 256",
            self.selection.page_index(),
            condition
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let mut result = Vec::with_capacity(256);
        while let Some(row) = rows.next()? {
            result.push((count(row, 0)?, EntryId(number(row, 1)?)));
        }
        Ok(result)
    }
}
fn rank_i64(rank: u64) -> Result<i64> {
    i64::try_from(rank).map_err(|_| Error::Contract("generation A rank overflow"))
}
fn decode_row(
    store: &Store,
    generation: GenerationId,
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> Result<queries::Row> {
    let raw = row
        .get_ref(offset + 1)?
        .as_blob()
        .map_err(rusqlite::Error::from)?;
    let path = queries::decode_recorded_path(raw)?.into_owned();
    Ok(queries::Row {
        key: EntryKey {
            inventory: store.inventory,
            generation,
            entry: EntryId(number(row, offset)?),
        },
        path,
        kind: match row.get::<_, i64>(offset + 2)? {
            1 => Kind::File,
            2 => Kind::Directory,
            3 => Kind::Symlink,
            4 => Kind::Special,
            _ => Kind::Unknown,
        },
        logical: opt_number(row, offset + 3)?,
    })
}
#[derive(Clone)]
pub(crate) struct Selection {
    kind: Option<i64>,
    min: Option<u64>,
    max: Option<u64>,
    extension: Option<String>,
    case_sensitive: bool,
}
impl Selection {
    fn new(search: &SearchQuery) -> Self {
        Self {
            kind: search.kind.map(|kind| match kind {
                InventoryKind::File => 1,
                InventoryKind::Directory => 2,
                InventoryKind::Symlink => 3,
                InventoryKind::Special => 4,
            }),
            min: search.min_bytes,
            max: search.max_bytes,
            extension: search.extension.as_deref().map(|s| {
                if search.case_sensitive {
                    s.trim_start_matches('.').into()
                } else {
                    s.trim_start_matches('.').to_lowercase()
                }
            }),
            case_sensitive: search.case_sensitive,
        }
    }
    fn kind_condition(&self) -> String {
        self.kind
            .map_or_else(|| "1".into(), |kind| format!("kind={kind}"))
    }
    fn condition(&self) -> String {
        let mut terms = vec![self.kind_condition()];
        if self.min.is_some() && self.min == self.max {
            terms.push("logical=?".into());
        } else {
            if self.min.is_some() {
                terms.push("logical>=?".into());
            }
            if self.max.is_some() {
                terms.push("logical<=?".into());
            }
        }
        if self.extension.is_some() {
            terms.push(
                if self.case_sensitive {
                    "ext_cs=?"
                } else {
                    "ext_ci=?"
                }
                .into(),
            );
        }
        terms.join(" AND ")
    }
    fn page_index(&self) -> &'static str {
        if self.extension.is_some() {
            if self.case_sensitive {
                "INDEXED BY generation_a_ext_cs"
            } else {
                "INDEXED BY generation_a_ext_ci"
            }
        } else if self.min.is_some() && self.min == self.max {
            "INDEXED BY generation_a_size"
        } else if self.kind.is_some() {
            "INDEXED BY generation_a_kind"
        } else {
            "NOT INDEXED"
        }
    }
    fn parameters(&self) -> Vec<rusqlite::types::Value> {
        let mut result = Vec::new();
        if let Some(min) = self.min {
            result.push(unsigned(min).to_vec().into());
        }
        if let Some(max) = self.max.filter(|_| self.min != self.max) {
            result.push(unsigned(max).to_vec().into());
        }
        if let Some(extension) = &self.extension {
            result.push(extension.clone().into());
        }
        result
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
