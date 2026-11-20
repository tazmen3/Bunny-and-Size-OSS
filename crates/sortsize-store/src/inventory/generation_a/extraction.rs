//! Bounded extraction into our own canonical schema, never source-owned DDL.
use super::publication_batch::PublicationBatch;
use super::*;
use rusqlite::types::Value;

const TABLES: &[&str] = &[
    "identity",
    "generations",
    "entries",
    "tasks",
    "contributions",
    "recovery_guard",
    "retention",
    "measurements",
    "entry_evidence",
    "acquisition_records",
    "history_receipt",
    "entry_properties",
    "collection_options",
];

/// The target schema is authored by this binary. Source CHECK/generated-column
/// and trigger expressions are never executed by INSERT into the new database.
/// Every source canonical definition must match this known schema before reads.
pub(super) fn extract(
    source: &Store,
    target: &mut Store,
    g: GenerationId,
    cancel: &CancellationToken,
) -> Result<()> {
    extract_with_batches(source, target, g, cancel, PublicationBatch::new)
}

fn extract_with_batches(
    source: &Store,
    target: &mut Store,
    g: GenerationId,
    cancel: &CancellationToken,
    mut new_batch: impl FnMut() -> PublicationBatch,
) -> Result<()> {
    let _timer = Phase::Extract.start();
    let version: i64 = source
        .db
        .pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !(1..=6).contains(&version) {
        return Err(Error::Contract(
            "future canonical extraction format preserved",
        ));
    }
    for db in [&source.db, &target.db] {
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 512 * 1024)?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 256 * 1024)?;
    }
    match version {
        6 => target.enable_properties()?,
        5 => target.enable_catalogue()?,
        4 => target.enable_evidence()?,
        3 => target.enable_lifecycle()?,
        _ => (),
    }
    let mut copied = Vec::new();
    for &table in TABLES {
        let source_ddl: Option<String> = source
            .db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .optional()?;
        let minimum = match table {
            "recovery_guard" => 2,
            "retention" | "measurements" => 3,
            "entry_evidence" => 4,
            "acquisition_records" | "history_receipt" => 5,
            "entry_properties" | "collection_options" => 6,
            _ => 1,
        };
        let Some(source_ddl) = source_ddl else {
            if version >= minimum {
                return Err(Error::Contract(
                    "canonical extraction requires known tables, not views",
                ));
            }
            continue;
        };
        let expected: String = target.db.query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        )?;
        if source_ddl != expected {
            return Err(Error::Contract(
                "unrecognized canonical table definition preserved",
            ));
        }
        let mut objects=source.db.prepare("SELECT type,name,sql FROM sqlite_schema WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL")?;
        let mut rows = objects.query([table])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let name: String = row.get(1)?;
            let definition: String = row.get(2)?;
            let expected: Option<String> = target
                .db
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE type=?1 AND name=?2 AND tbl_name=?3",
                    params![kind, name, table],
                    |r| r.get(0),
                )
                .optional()?;
            if expected.as_ref() != Some(&definition) {
                return Err(Error::Contract(
                    "unrecognized canonical index or trigger preserved",
                ));
            }
        }
        // A removed canonical trigger/index changes the source contract too.
        let mut required=target.db.prepare("SELECT type,name,sql FROM sqlite_schema WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL")?;
        let mut rows = required.query([table])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let name: String = row.get(1)?;
            let definition: String = row.get(2)?;
            let actual: Option<String> = source
                .db
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE type=?1 AND name=?2 AND tbl_name=?3",
                    params![kind, name, table],
                    |r| r.get(0),
                )
                .optional()?;
            if actual.as_ref() != Some(&definition) {
                return Err(Error::Contract(
                    "missing canonical index or trigger preserved",
                ));
            }
        }
        copied.push(table);
    }
    if !copied.contains(&"generations") || !copied.contains(&"entries") {
        return Err(Error::Contract("incomplete canonical extraction schema"));
    }
    for &table in &copied {
        cancelled(cancel)?;
        if table == "identity" {
            continue;
        }
        if table == "retention" {
            target.db.execute("DELETE FROM retention", [])?;
        }
        let filter = match table {
            "retention" => "1",
            "generations" => "id=?1",
            _ => "generation=?1",
        };
        let mut statement = source
            .db
            .prepare(&format!("SELECT * FROM {table} WHERE {filter}"))?;
        let columns = statement.column_count();
        if columns > 32 {
            return Err(Error::Contract("canonical extraction column budget"));
        }
        let sql = format!(
            "INSERT INTO {table} VALUES({})",
            (0..columns).map(|_| "?").collect::<Vec<_>>().join(",")
        );
        let mut rows = if filter == "1" {
            statement.query([])?
        } else {
            statement.query([unsigned(g.0)])?
        };
        let mut done = false;
        while !done {
            cancelled(cancel)?;
            let tx = target.db.transaction()?;
            {
                let mut insert = tx.prepare(&sql)?;
                let mut batch = new_batch();
                while !done && batch.next_window() {
                    cancelled(cancel)?;
                    for _ in 0..256 {
                        cancelled(cancel)?;
                        let Some(row) = rows.next()? else {
                            done = true;
                            break;
                        };
                        let mut values = (0..columns)
                            .map(|i| row.get::<_, Value>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        if table == "generations" {
                            values[2] = Value::Integer(0);
                        }
                        let bytes = values
                            .iter()
                            .map(|v| match v {
                                Value::Blob(b) => b.len(),
                                Value::Text(s) => s.len(),
                                _ => 8,
                            })
                            .sum::<usize>();
                        if bytes > 512 * 1024 {
                            return Err(Error::Contract("canonical extraction row budget"));
                        }
                        insert.execute(rusqlite::params_from_iter(values))?;
                    }
                }
            }
            metrics::commit(tx, Phase::ExtractCommit)?;
        }
    }
    let (sealed, complete): (bool, bool) = source.db.query_row(
        "SELECT sealed,coverage=2 FROM generations WHERE id=?1",
        [unsigned(g.0)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let tx = target.db.transaction()?;
    tx.execute(
        "UPDATE generations SET sealed=?2 WHERE id=?1",
        params![unsigned(g.0), sealed],
    )?;
    tx.execute(
        "UPDATE identity SET latest=?1,latest_complete=?2",
        params![
            sealed.then(|| unsigned(g.0)),
            (sealed && complete).then(|| unsigned(g.0))
        ],
    )?;
    metrics::commit(tx, Phase::ExtractCommit)?;
    if target.db.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(Error::Contract("canonical extraction foreign key mismatch"));
    }
    let count: i64 = target
        .db
        .query_row("SELECT count(*) FROM generations", [], |r| r.get(0))?;
    if count != 1 {
        return Err(Error::Contract(
            "canonical extraction did not produce one generation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::tests::{attempt_id, batch, context_id, observation};

    #[test]
    fn late_source_constraint_failure_rolls_back_group_and_fresh_retry_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        let c = context_id(1);
        let create = |name: &str| {
            Store::create(
                &temp.path().join(name),
                c.inventory,
                c.source,
                Durability::Retained,
            )
            .unwrap()
        };
        let mut source = create("source");
        source.create_generation(c).unwrap();
        let a = attempt_id(1, 0, 1);
        source.begin_attempt(a).unwrap();
        let observations = (0..300)
            .map(|id| observation(id, Kind::File))
            .collect::<Vec<_>>();
        batch(
            &mut source,
            a,
            0,
            Some(ListingEnd::Exhausted),
            &observations,
        )
        .unwrap();
        source
            .db
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        source
            .db
            .execute(
                "UPDATE entries SET kind=99 WHERE generation=?1 AND id=?2",
                params![unsigned(1), unsigned(270)],
            )
            .unwrap();
        source
            .db
            .pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        let ids = source
            .db
            .prepare("SELECT * FROM entries WHERE generation=?1")
            .unwrap()
            .query_map([unsigned(1)], |row| number(row, 1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(ids.iter().position(|id| *id == 270).unwrap() >= 256);
        let mut failed = create("failed-stage");
        // Remove clock variance from this rollback test: the production
        // deadline is cooperative, so a slow first window may commit alone.
        // The test exercises the same transaction/window code with eight
        // windows admitted regardless of host speed, never a timing assertion.
        assert!(
            extract_with_batches(
                &source,
                &mut failed,
                c.generation,
                &Default::default(),
                PublicationBatch::without_time_limit
            )
            .is_err()
        );
        let copied: i64 = failed
            .db
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(copied, 0, "the first 256 rows must roll back with row 270");
        let sealed: bool = failed
            .db
            .query_row(
                "SELECT sealed FROM generations WHERE id=?1",
                [unsigned(1)],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sealed);
        assert_eq!(failed.latest_complete().unwrap(), None);
        let model: i64 = failed
            .db
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='generation_a_meta'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(model, 0, "failed extraction cannot expose a ready model");
        let full: i64 = failed
            .db
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(full, 2);

        source
            .db
            .execute(
                "UPDATE entries SET kind=1 WHERE generation=?1 AND id=?2",
                params![unsigned(1), unsigned(270)],
            )
            .unwrap();
        let mut retry = create("fresh-retry");
        extract(&source, &mut retry, c.generation, &Default::default()).unwrap();
        assert_eq!(retry.entry_count(c.generation).unwrap(), 301);
        assert!(
            source
                .same_generation_observation(
                    &retry,
                    c.generation,
                    c.generation,
                    &Default::default()
                )
                .unwrap()
        );
    }
}
