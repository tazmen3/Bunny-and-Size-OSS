use super::publication_batch::PublicationBatch;
use super::*;

pub(super) const SQL: &str = "
CREATE TABLE generation_a_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,generation BLOB NOT NULL,root BLOB NOT NULL,platform TEXT NOT NULL,predicate TEXT NOT NULL,rows INTEGER NOT NULL,oversized INTEGER NOT NULL,ready INTEGER NOT NULL) STRICT;
CREATE TABLE generation_a_rows(rank INTEGER PRIMARY KEY CHECK(rank>=0),id BLOB NOT NULL,path BLOB NOT NULL,kind INTEGER NOT NULL,logical BLOB,modified_s INTEGER,modified_ns INTEGER,ext_cs TEXT,ext_ci TEXT) STRICT;
CREATE TABLE generation_a_pending(sort BLOB NOT NULL,id BLOB NOT NULL,PRIMARY KEY(sort,id)) STRICT, WITHOUT ROWID;
CREATE UNIQUE INDEX generation_a_id ON generation_a_rows(id);
CREATE INDEX generation_a_kind ON generation_a_rows(kind,rank);
CREATE INDEX generation_a_ext_cs ON generation_a_rows(ext_cs,rank);
CREATE INDEX generation_a_ext_ci ON generation_a_rows(ext_ci,rank);
CREATE INDEX generation_a_size ON generation_a_rows(logical,rank);
CREATE TABLE generation_a_counts(kind INTEGER PRIMARY KEY,count INTEGER NOT NULL,unknown_size INTEGER NOT NULL,unknown_extension INTEGER NOT NULL) STRICT;
";

pub(super) fn require_model_tables(db: &Connection) -> Result<()> {
    for name in [
        "generation_a_meta",
        "generation_a_rows",
        "generation_a_counts",
    ] {
        let kind: String = db.query_row(
            "SELECT type FROM sqlite_schema WHERE name=?1",
            [name],
            |r| r.get(0),
        )?;
        if kind != "table" {
            return Err(Error::Contract(
                "generation A requires canonical model tables",
            ));
        }
    }
    for (name, expected) in [
        (
            "generation_a_id",
            "CREATE UNIQUE INDEX generation_a_id ON generation_a_rows(id)",
        ),
        (
            "generation_a_kind",
            "CREATE INDEX generation_a_kind ON generation_a_rows(kind,rank)",
        ),
        (
            "generation_a_ext_cs",
            "CREATE INDEX generation_a_ext_cs ON generation_a_rows(ext_cs,rank)",
        ),
        (
            "generation_a_ext_ci",
            "CREATE INDEX generation_a_ext_ci ON generation_a_rows(ext_ci,rank)",
        ),
        (
            "generation_a_size",
            "CREATE INDEX generation_a_size ON generation_a_rows(logical,rank)",
        ),
    ] {
        let (kind, table, definition): (String, String, String) = db.query_row(
            "SELECT type,tbl_name,sql FROM sqlite_schema WHERE name=?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if kind != "index"
            || table != "generation_a_rows"
            || definition.trim_end_matches(';') != expected
        {
            return Err(Error::Contract("generation A index definition mismatch"));
        }
    }
    Ok(())
}
// Only the temporary preparation map contains native sort keys. Persistent
// secondary indexes contain scalar values and an eight-byte-or-smaller rank,
// never a repeated native path. EntryId remains a full-width unsigned BLOB.
pub(super) fn build_rows(
    source: &Store,
    target: &mut Store,
    generation: GenerationId,
    root: &Path,
    baseline: u64,
    budget: PublicationGrowth,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let max_growth = budget.bytes();
    let build_keys = Phase::BuildKeys.start();
    let mut paths = queries::paths::Paths::new(512 * 1024);
    let mut source_rows = source.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 ORDER BY id")?;
    let mut rows = source_rows.query([unsigned(generation.0)])?;
    let mut oversized = 0_u64;
    let mut done = false;
    while !done {
        let tx = target.db.unchecked_transaction()?;
        let mut batch = PublicationBatch::new();
        let mut group_oversized = oversized;
        {
            let mut insert = tx.prepare("INSERT INTO generation_a_pending VALUES(?1,?2)")?;
            while !done && batch.next_window() {
                growth(target, baseline, max_growth, cancel)?;
                for _ in 0..256 {
                    cancelled(cancel)?;
                    let Some(row) = rows.next()? else {
                        done = true;
                        break;
                    };
                    let id = EntryId(number(row, 0)?);
                    match paths.row(source, generation, id, root, row) {
                        Ok(Some(path)) => {
                            insert.execute(params![
                                queries::compact_path_order(path, root)?,
                                unsigned(id.0)
                            ])?;
                        }
                        Ok(None) => {}
                        Err(Error::Contract(_)) => {
                            group_oversized = group_oversized
                                .checked_add(1)
                                .ok_or(Error::Contract("generation A count overflow"))?
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        metrics::commit(tx, Phase::BuildKeysCommit)?;
        oversized = group_oversized;
    }
    drop(rows);
    drop(source_rows);
    drop(build_keys);
    // Secondary indexes already exist, so SQLite maintains them during each
    // row INSERT; this interval includes both row projection and indexing.
    let _timer = Phase::BuildRowsAndIndexes.start();
    let mut canonical=source.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND id=?2")?;
    let mut count = 0_u64;
    let mut after = None;
    // A target cursor held across these commits pins the WAL readmark. Close
    // each bounded read window before writing, so SQLite can checkpoint/reuse
    // WAL normally even when native EntryIds are unrelated to lexical order.
    drop(paths);
    let mut rows_done = false;
    loop {
        let tx = target.db.unchecked_transaction()?;
        let mut batch = PublicationBatch::new();
        let mut group_after = after.take();
        let mut group_count = count;
        while batch.next_window() {
            growth(target, baseline, max_growth, cancel)?;
            let window = pending_window(&tx, group_after.take(), cancel)?;
            if window.ids.is_empty() {
                rows_done = true;
                break;
            }
            let footprint = window
                .key
                .capacity()
                .checked_add(window.ids.capacity() * std::mem::size_of::<EntryId>())
                .and_then(|n| n.checked_add(256 * 1024))
                .ok_or(Error::Contract("generation A key window memory overflow"))?;
            let path_budget = (FRAME as usize)
                .checked_sub(footprint)
                .ok_or(Error::Contract("generation A key window memory budget"))?
                .min(512 * 1024);
            let mut paths = queries::paths::Paths::new(path_budget);
            if budget.consumes_keys() {
                // Consume only the first pending window. Deletion and model
                // inserts share one rollback boundary, so a failed/cancelled
                // window cannot lose keys or advance ranks.
                consume_pending(&tx, &window)?;
            }
            {
                let mut insert =
                    tx.prepare("INSERT INTO generation_a_rows VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
                for id in &window.ids {
                    cancelled(cancel)?;
                    let mut canonical_rows =
                        canonical.query(params![unsigned(generation.0), unsigned(id.0)])?;
                    let row = canonical_rows
                        .next()?
                        .ok_or(Error::Contract("generation A missing ranked identity"))?;
                    let path = paths
                        .row(source, generation, *id, root, row)?
                        .ok_or(Error::Contract("generation A invisible ranked identity"))?;
                    let extension = path.extension().unwrap_or_default().to_str();
                    let rank = i64::try_from(group_count)
                        .map_err(|_| Error::Contract("generation A rank overflow"))?;
                    insert.execute(params![
                        rank,
                        unsigned(id.0),
                        queries::native_path(path).encode(),
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<u32>>(4)?,
                        extension,
                        extension.map(str::to_lowercase)
                    ])?;
                    group_count = group_count
                        .checked_add(1)
                        .ok_or(Error::Contract("generation A count overflow"))?;
                }
            }
            if !budget.consumes_keys() {
                // This continuation is local to the transaction. The durable
                // build continuation advances only after the whole group commits.
                let last = *window
                    .ids
                    .last()
                    .ok_or(Error::Contract("generation A empty key window"))?;
                group_after = Some((window.key, last));
            }
        }
        if group_count == count {
            break;
        }
        metrics::commit(tx, Phase::BuildRowsAndIndexesCommit)?;
        count = group_count;
        after = group_after;
        if rows_done {
            break;
        }
    }
    target
        .db
        .execute_batch("DROP TABLE generation_a_pending;")?;
    growth(target, baseline, max_growth, cancel)?;
    Ok((count, oversized))
}
struct PendingWindow {
    ids: Vec<EntryId>,
    key: Vec<u8>,
}

fn consume_pending(tx: &Transaction<'_>, window: &PendingWindow) -> Result<()> {
    let last = window
        .ids
        .last()
        .ok_or(Error::Contract("generation A empty key window"))?;
    let removed = tx.execute(
        "DELETE FROM generation_a_pending WHERE (sort,id)<=(?1,?2)",
        params![&window.key, unsigned(last.0)],
    )?;
    if removed != window.ids.len() {
        return Err(Error::Contract("generation A pending window changed"));
    }
    Ok(())
}

fn pending_window(
    db: &Connection,
    after: Option<(Vec<u8>, EntryId)>,
    cancel: &CancellationToken,
) -> Result<PendingWindow> {
    let mut statement = db.prepare(if after.is_some() {
        "SELECT sort,id FROM generation_a_pending WHERE (sort,id)>(?1,?2) ORDER BY sort,id LIMIT 256"
    } else {
        "SELECT sort,id FROM generation_a_pending WHERE (sort,id)>=(?1,?2) ORDER BY sort,id LIMIT 256"
    })?;
    let mut rows = statement.query(params![
        after.as_ref().map_or(&[][..], |a| a.0.as_slice()),
        unsigned(after.as_ref().map_or(0, |a| a.1.0))
    ])?;
    // rusqlite binds owned SQLite copies. Do not retain the previous Rust key
    // while allocating its replacement; at most one last key lives in this
    // build lease, not 256 native keys. No smaller path/key limit is added.
    drop(after);
    let mut ids = Vec::new();
    ids.try_reserve_exact(256)
        .map_err(|_| Error::Contract("generation A identity window allocation"))?;
    let mut key = Vec::new();
    while let Some(row) = rows.next()? {
        cancelled(cancel)?;
        let raw = row.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?;
        if raw.len() > key.capacity() {
            // Free first rather than reallocating with old+new capacities live.
            key = Vec::new();
            key.try_reserve_exact(raw.len())
                .map_err(|_| Error::Contract("generation A key window allocation"))?;
        }
        key.clear();
        key.extend_from_slice(raw);
        ids.push(EntryId(number(row, 1)?));
    }
    // Rows AND its prepared statement are finalized before the caller writes.
    // No target read cursor survives a window or a committing transaction.
    Ok(PendingWindow { ids, key })
}
#[cfg(test)]
static OBSERVED_GROWTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn growth(
    target: &Store,
    baseline: u64,
    max_growth: u64,
    cancel: &CancellationToken,
) -> Result<()> {
    cancelled(cancel)?;
    let used = lifecycle::artifact_bytes(&target.directory)?.saturating_sub(baseline);
    #[cfg(test)]
    OBSERVED_GROWTH.fetch_max(used, std::sync::atomic::Ordering::Relaxed);
    if used >= max_growth.saturating_sub(256 * 1024) {
        return Err(Error::Contract("generation A index growth budget"));
    }
    Ok(())
}

pub(super) fn validate_semantics(
    store: &Store,
    generation: GenerationId,
    root: &Path,
    expected_rows: u64,
    expected_oversized: u64,
    cancel: &CancellationToken,
) -> Result<()> {
    let semantic_rows = Phase::CertifySemanticRows.start();
    let mut paths = queries::paths::Paths::new(512 * 1024);
    let mut stmt = store.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 ORDER BY id")?;
    let mut source = stmt.query([unsigned(generation.0)])?;
    let mut lookup = store.db.prepare("SELECT rank,path,kind,logical,modified_s,modified_ns,ext_cs,ext_ci FROM generation_a_rows WHERE id=?1")?;
    let mut total = 0;
    let mut oversized = 0;
    while let Some(row) = source.next()? {
        cancelled(cancel)?;
        let id = EntryId(number(row, 0)?);
        let path = match paths.row(store, generation, id, root, row) {
            Ok(Some(path)) => path,
            Ok(None) => continue,
            Err(Error::Contract(_)) => {
                oversized += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let actual = lookup.query_row([unsigned(id.0)], |r| {
            Ok((
                count(r, 0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<u32>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let extension = path.extension().unwrap_or_default().to_str();
        let expected = (
            actual.0,
            queries::native_path(path).encode(),
            row.get::<_, i64>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<u32>>(4)?,
            extension.map(str::to_owned),
            extension.map(str::to_lowercase),
        );
        if actual != expected {
            return Err(Error::Contract("generation A semantic index mismatch"));
        }
        total += 1;
    }
    drop(semantic_rows);
    let rank_order = Phase::CertifyRankOrder.start();
    let mut ranked = store
        .db
        .prepare("SELECT rank,id,path FROM generation_a_rows ORDER BY rank")?;
    let mut ranked_rows = ranked.query([])?;
    let mut previous: Option<(Vec<u8>, u64)> = None;
    let mut rank = 0_u64;
    while let Some(row) = ranked_rows.next()? {
        cancelled(cancel)?;
        if count(row, 0)? != rank {
            return Err(Error::Contract("generation A rank gap"));
        }
        let id = number(row, 1)?;
        let raw: Vec<u8> = row.get(2)?;
        let path: PathBuf = crate::scheduler::native_name(
            &NativeName::decode(&raw).map_err(|_| Error::Contract("generation A path codec"))?,
        )?
        .into();
        let key = (queries::compact_path_order(&path, root)?, id);
        if previous.as_ref().is_some_and(|old| old >= &key) {
            return Err(Error::Contract("generation A rank order mismatch"));
        }
        previous = Some(key);
        rank = rank
            .checked_add(1)
            .ok_or(Error::Contract("generation A rank overflow"))?;
    }
    if rank != total {
        return Err(Error::Contract("generation A rank count mismatch"));
    }
    drop(rank_order);
    let _population_counts = Phase::CertifyPopulationCounts.start();
    let indexed: u64 = store
        .db
        .query_row("SELECT count(*) FROM generation_a_rows", [], |r| {
            count(r, 0)
        })?;
    let discrepancy:u64=store.db.query_row("SELECT count(*) FROM (SELECT kind,count(*) AS count,sum(logical IS NULL) AS unknown_size,sum(ext_cs IS NULL) AS unknown_extension FROM generation_a_rows GROUP BY kind EXCEPT SELECT kind,count,unknown_size,unknown_extension FROM generation_a_counts)",[],|r|count(r,0))?;
    let extra:u64=store.db.query_row("SELECT count(*) FROM (SELECT kind,count,unknown_size,unknown_extension FROM generation_a_counts EXCEPT SELECT kind,count(*),sum(logical IS NULL),sum(ext_cs IS NULL) FROM generation_a_rows GROUP BY kind)",[],|r|count(r,0))?;
    if total != expected_rows
        || total != indexed
        || oversized != expected_oversized
        || discrepancy != 0
        || extra != 0
    {
        return Err(Error::Contract("generation A semantic population mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod growth_tests;
