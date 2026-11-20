//! Exact canonical ownership comparison; no source acquisition or lossy codec.
use super::*;
use rusqlite::{limits::Limit, types::ValueRef};
use sortsize_core::{
    CancellationToken,
    resources::{Category, Governor},
};

const PROPERTY_TRIGGERS: &str = "
CREATE TRIGGER immutable_properties_insert BEFORE INSERT ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_properties_update BEFORE UPDATE ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_properties_delete BEFORE DELETE ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_options_insert BEFORE INSERT ON collection_options WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_options_update BEFORE UPDATE ON collection_options BEGIN SELECT RAISE(ABORT,'immutable collection options'); END;
CREATE TRIGGER immutable_options_delete BEFORE DELETE ON collection_options WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
";
const ROW_BYTES: i32 = 512 * 1024;
struct LimitGuard<'a> {
    db: &'a Connection,
    previous: i32,
}
impl<'a> LimitGuard<'a> {
    fn new(db: &'a Connection) -> Result<Self> {
        let previous = db.limit(Limit::SQLITE_LIMIT_LENGTH)?;
        db.set_limit(Limit::SQLITE_LIMIT_LENGTH, previous.min(ROW_BYTES))?;
        Ok(Self { db, previous })
    }
}
impl Drop for LimitGuard<'_> {
    fn drop(&mut self) {
        let _ = self.db.set_limit(Limit::SQLITE_LIMIT_LENGTH, self.previous);
    }
}
struct Table {
    name: &'static str,
    columns: &'static str,
    key: &'static str,
    minimum: i64,
    ddl: Option<&'static str>,
}
const TABLES: &[Table] = &[
    Table {
        name: "generations",
        columns: "id,context,sealed,revision,next_entry,aggregate_cursor,aggregate_sum,aggregate_unknown,aggregate_overflow,aggregate_done,coverage",
        key: "id",
        minimum: 1,
        ddl: None,
    },
    Table {
        name: "entries",
        columns: "generation,id,parent,name,kind,logical,allocated,modified_s,modified_ns,attempt,present,metadata_known",
        key: "id",
        minimum: 1,
        ddl: None,
    },
    Table {
        name: "tasks",
        columns: "generation,directory,kind,key,attempt,owner,branch,next_sequence,parent_attempt,listing,sweep_cursor,reconciled",
        key: "directory,kind",
        minimum: 1,
        ddl: None,
    },
    Table {
        name: "contributions",
        columns: "generation,id,parent,attempt,logical,metadata_known,kind",
        key: "id",
        minimum: 1,
        ddl: None,
    },
    Table {
        name: "recovery_guard",
        columns: "generation,proof",
        key: "generation",
        minimum: 2,
        ddl: None,
    },
    Table {
        name: "measurements",
        columns: "generation,root,started,measured",
        key: "generation",
        minimum: 3,
        ddl: Some(
            "CREATE TABLE measurements(generation BLOB PRIMARY KEY REFERENCES generations(id), root BLOB NOT NULL, started INTEGER NOT NULL, measured INTEGER NOT NULL) STRICT, WITHOUT ROWID",
        ),
    },
    Table {
        name: "entry_evidence",
        columns: "generation,id,proof",
        key: "id",
        minimum: 4,
        ddl: Some(
            "CREATE TABLE entry_evidence(generation BLOB NOT NULL,id BLOB NOT NULL,proof BLOB,PRIMARY KEY(generation,id),FOREIGN KEY(generation,id) REFERENCES entries(generation,id)) STRICT, WITHOUT ROWID",
        ),
    },
    Table {
        name: "acquisition_records",
        columns: "generation,root,started",
        key: "generation",
        minimum: 5,
        ddl: Some(
            "CREATE TABLE acquisition_records(generation BLOB PRIMARY KEY REFERENCES generations(id),root BLOB NOT NULL,started INTEGER NOT NULL) STRICT, WITHOUT ROWID",
        ),
    },
    Table {
        name: "entry_properties",
        columns: "generation,id,allocated_state,accessed_state,accessed_s,accessed_ns,owner",
        key: "id",
        minimum: 6,
        ddl: Some(
            "CREATE TABLE entry_properties(generation BLOB NOT NULL,id BLOB NOT NULL,allocated_state INTEGER NOT NULL,accessed_state INTEGER NOT NULL,accessed_s INTEGER,accessed_ns INTEGER,owner BLOB NOT NULL,PRIMARY KEY(generation,id),FOREIGN KEY(generation,id) REFERENCES entries(generation,id),CHECK(allocated_state BETWEEN 0 AND 5),CHECK(accessed_state BETWEEN 0 AND 5),CHECK(length(owner) BETWEEN 1 AND 70),CHECK((accessed_s IS NULL)=(accessed_ns IS NULL)),CHECK(accessed_ns IS NULL OR accessed_ns BETWEEN 0 AND 999999999)) STRICT, WITHOUT ROWID",
        ),
    },
    Table {
        name: "collection_options",
        columns: "generation,allocation,owners",
        key: "generation",
        minimum: 6,
        ddl: Some(
            "CREATE TABLE collection_options(generation BLOB PRIMARY KEY REFERENCES generations(id),allocation INTEGER NOT NULL CHECK(allocation IN (0,1)),owners INTEGER NOT NULL CHECK(owners IN (0,1))) STRICT, WITHOUT ROWID",
        ),
    },
];

impl Store {
    /// Compare every persisted canonical observation/work-state cell for two
    /// generation keys. Retention, history receipts, derived models and global
    /// latest pointers are deliberately outside the observation. Both stores
    /// must already be certified and remain leased for this streaming call.
    pub fn same_generation_observation(
        &self,
        other: &Store,
        own: GenerationId,
        theirs: GenerationId,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        self.require_integrity()?;
        other.require_integrity()?;
        if cancel.is_cancelled() {
            return Err(Error::Contract("observation comparison cancelled"));
        }
        if self.inventory != other.inventory || self.source != other.source {
            return Ok(false);
        }
        let _memory = Governor::global()
            .reserve(Category::Queries, 2 * 1024 * 1024, None)
            .ok_or(Error::Contract("observation comparison memory admission"))?;
        let _a_limit = LimitGuard::new(&self.db)?;
        let _b_limit = LimitGuard::new(&other.db)?;
        let av = version(&self.db)?;
        let bv = version(&other.db)?;
        let mut definitions = authored_statements(SCHEMA)?;
        definitions.extend(authored_statements(PROPERTY_TRIGGERS)?);
        for store in [self, other] {
            known_identity(store, &definitions)?;
        }
        let mut presence = [[false; 2]; TABLES.len()];
        for (index, table) in TABLES.iter().enumerate() {
            presence[index] = [
                known_table(&self.db, table, av, &definitions)?,
                known_table(&other.db, table, bv, &definitions)?,
            ];
        }
        let a = self.acquisition_context(own)?;
        let b = other.acquisition_context(theirs)?;
        for (context, g) in [(a, own), (b, theirs)] {
            if context.inventory != self.inventory
                || context.source != self.source
                || context.generation != g
            {
                return Err(Error::Contract("observation comparison context binding"));
            }
        }
        if a != b {
            return Ok(false);
        }
        for (index, table) in TABLES.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(Error::Contract("observation comparison cancelled"));
            }
            let [a_exists, b_exists] = presence[index];
            if !a_exists && !b_exists {
                continue;
            }
            let filter = if table.name == "generations" {
                "id"
            } else {
                "generation"
            };
            let query = format!(
                "SELECT {} FROM {} NOT INDEXED WHERE {filter}=?1 ORDER BY {}",
                table.columns, table.name, table.key
            );
            match (a_exists, b_exists) {
                (true, true) => {
                    let mut aq = self.db.prepare(&query)?;
                    let mut bq = other.db.prepare(&query)?;
                    let columns = aq.column_count();
                    if columns != bq.column_count() || columns > 16 {
                        return Err(Error::Contract("observation column budget"));
                    }
                    let mut ar = aq.query([unsigned(own.0)])?;
                    let mut br = bq.query([unsigned(theirs.0)])?;
                    loop {
                        if cancel.is_cancelled() {
                            return Err(Error::Contract("observation comparison cancelled"));
                        }
                        match (ar.next()?, br.next()?) {
                            (None, None) => break,
                            (Some(a), Some(b)) => {
                                let mut ab = 0usize;
                                let mut bb = 0usize;
                                for i in 0..columns {
                                    let av = a.get_ref(i)?;
                                    let bv = b.get_ref(i)?;
                                    ab = ab
                                        .checked_add(cell_bytes(av))
                                        .ok_or(Error::Contract("observation row overflow"))?;
                                    bb = bb
                                        .checked_add(cell_bytes(bv))
                                        .ok_or(Error::Contract("observation row overflow"))?;
                                    if ab > ROW_BYTES as usize || bb > ROW_BYTES as usize {
                                        return Err(Error::Contract("observation row budget"));
                                    }
                                    if av != bv {
                                        return Ok(false);
                                    }
                                }
                            }
                            _ => return Ok(false),
                        }
                    }
                }
                (true, false) => {
                    if self.db.prepare(&query)?.exists([unsigned(own.0)])? {
                        return Ok(false);
                    }
                }
                (false, true) => {
                    if other.db.prepare(&query)?.exists([unsigned(theirs.0)])? {
                        return Ok(false);
                    }
                }
                _ => unreachable!(),
            }
        }
        Ok(true)
    }
}
fn cell_bytes(value: ValueRef<'_>) -> usize {
    match value {
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(b) | ValueRef::Blob(b) => b.len(),
    }
}
fn version(db: &Connection) -> Result<i64> {
    let v = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !(1..=6).contains(&v) {
        return Err(Error::Contract(
            "future observation comparison format preserved",
        ));
    }
    Ok(v)
}
fn known_table(db: &Connection, table: &Table, version: i64, definitions: &[&str]) -> Result<bool> {
    let actual: Option<(String,Option<String>)> = db.query_row("SELECT type,CASE WHEN length(sql)<=8192 THEN sql ELSE NULL END FROM sqlite_schema WHERE name=?1",[table.name],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((kind, definition)) = actual else {
        if version >= table.minimum {
            return Err(Error::Contract("required observation table missing"));
        }
        return Ok(false);
    };
    let prefix = format!("CREATE TABLE {}(", table.name);
    let expected = table
        .ddl
        .or_else(|| {
            definitions
                .iter()
                .copied()
                .find(|sql| sql.starts_with(&prefix))
        })
        .ok_or(Error::Contract("unknown authored observation schema"))?;
    if kind != "table" || definition.as_deref() != Some(expected) {
        return Err(Error::Contract(
            "unknown observation table definition preserved",
        ));
    }
    let mut objects = db.prepare("SELECT CASE WHEN length(sql)<=8192 THEN sql ELSE NULL END FROM sqlite_schema WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL LIMIT 33")?;
    let mut rows = objects.query([table.name])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > 32 {
            return Err(Error::Contract("observation schema object budget"));
        }
        let sql: String = row.get(0)?;
        if !definitions.iter().any(|known| *known == sql) {
            return Err(Error::Contract(
                "unknown observation index or trigger preserved",
            ));
        }
    }
    for known in definitions
        .iter()
        .copied()
        .filter(|sql| sql.starts_with("CREATE INDEX ") || sql.starts_with("CREATE TRIGGER "))
    {
        let object_table = known
            .split_once(" ON ")
            .map(|(_, rest)| rest.split(['(', ' ']).next().unwrap_or(""));
        if object_table != Some(table.name) {
            continue;
        }
        let name = known
            .split_whitespace()
            .nth(2)
            .ok_or(Error::Contract("authored schema object name"))?;
        let actual: Option<String> = db.query_row("SELECT CASE WHEN length(sql)<=8192 THEN sql ELSE NULL END FROM sqlite_schema WHERE tbl_name=?1 AND name=?2",params![table.name,name],|r|r.get(0)).optional()?;
        if actual.as_deref() != Some(known) {
            return Err(Error::Contract(
                "missing canonical observation index or trigger",
            ));
        }
    }

    Ok(true)
}

fn authored_statements(sql: &'static str) -> Result<Vec<&'static str>> {
    let mut result = Vec::new();
    let mut start = 0;
    for (index, byte) in sql.bytes().enumerate() {
        if byte != b';' {
            continue;
        }
        let fragment = &sql[start..=index];
        let encoded =
            std::ffi::CString::new(fragment).map_err(|_| Error::Contract("authored SQL NUL"))?;
        // SAFETY: the audited static SQL is NUL-terminated and lives through
        // this parser call. sqlite3_complete parses; it executes no SQL.
        if unsafe { libsqlite3_sys::sqlite3_complete(encoded.as_ptr()) } == 1 {
            result.push(fragment.trim().trim_end_matches(';').trim());
            start = index + 1;
        }
    }
    if !sql[start..].trim().is_empty() {
        return Err(Error::Contract("incomplete authored observation schema"));
    }
    Ok(result)
}
fn known_identity(store: &Store, definitions: &[&str]) -> Result<()> {
    let table = Table {
        name: "identity",
        columns: "inventory,source",
        key: "inventory,source",
        minimum: 1,
        ddl: None,
    };
    known_table(&store.db, &table, 1, definitions)?;
    let mut statement = store.db.prepare("SELECT CASE WHEN length(inventory)=16 THEN inventory ELSE NULL END,CASE WHEN length(source)=16 THEN source ELSE NULL END FROM identity LIMIT 2")?;
    let mut rows = statement.query([])?;
    let row = rows
        .next()?
        .ok_or(Error::Contract("missing canonical comparison identity"))?;
    if row.get_ref(0)? != ValueRef::Blob(&store.inventory.0.to_be_bytes())
        || row.get_ref(1)? != ValueRef::Blob(&store.source.0.to_be_bytes())
    {
        return Err(Error::Contract("canonical comparison identity mismatch"));
    }
    if rows.next()?.is_some() {
        return Err(Error::Contract("duplicate canonical comparison identity"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{
        differential::NativeEvidence,
        tests::{attempt_id, context_id, fold, reconcile},
    };
    use sortsize_core::{EntryOwner, LocalFileId, MetadataValue, acquisition::NodeProperties};
    fn fixture(path: &Path, root: &Path, variant: u8) -> Store {
        let c = context_id(0);
        let memory = Governor::global()
            .reserve(Category::Sqlite, 1536 * 1024, None)
            .unwrap();
        let mut s =
            Store::create_admitted(path, c.inventory, c.source, Durability::Retained, memory)
                .unwrap();
        s.enable_properties().unwrap();
        s.create_generation_guarded(
            c,
            Some(if variant == 3 {
                b"different-guard"
            } else {
                b"same-guard"
            }),
        )
        .unwrap();
        s.set_collection_options(c, true, variant != 4).unwrap();
        s.record_acquisition(c, root, 100).unwrap();
        let a = attempt_id(0, 0, 1);
        s.begin_attempt(a).unwrap();
        #[cfg(windows)]
        let name = NativeName::Windows(vec![0xd800, if variant == 1 { 0x42 } else { 0x41 }]);
        #[cfg(not(windows))]
        let name = NativeName::Posix(vec![0xff, if variant == 1 { b'b' } else { b'a' }]);
        let observation = Observation {
            name,
            kind: Kind::File,
            logical: Some(240),
            allocated: None,
            modified: None,
            metadata_known: true,
        };
        let properties = NodeProperties {
            allocated: MetadataValue::Unknown,
            accessed: if variant == 2 {
                MetadataValue::NotCollected
            } else {
                MetadataValue::Unknown
            },
            owner: EntryOwner {
                id: MetadataValue::AccessDenied,
            },
        };
        let evidence = NativeEvidence {
            identity: Some(LocalFileId::Unix {
                device: 1,
                inode: if variant == 5 { 2 } else { 1 },
            }),
            created: Some((-1, 999999999)),
        };
        s.ingest_properties(
            BatchHeader {
                attempt: a,
                sequence: 0,
                end: Some(ListingEnd::Exhausted),
            },
            &[observation],
            &[Some(evidence)],
            &[properties],
        )
        .unwrap();
        reconcile(&mut s, a);
        fold(&mut s, 0);
        s.seal(c).unwrap();
        s.record_measurement(c.generation, root, 100, 200).unwrap();
        s
    }
    #[test]
    fn handoff_rejects_fork_members_with_equal_identity_dates_totals_and_preserves_owned_bytes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("offline-native-root");
        let a = fixture(&t.path().join("a"), &root, 0);
        let b = fixture(&t.path().join("b"), &root, 1);
        assert_eq!(
            a.measurement(GenerationId(0)).unwrap(),
            b.measurement(GenerationId(0)).unwrap()
        );
        let consumer = t.path().join("history");
        let first = handoff(&a.lease(GenerationId(0)).unwrap(), &consumer).unwrap();
        let before = std::fs::read(first.directory.join("inventory.db")).unwrap();
        let error = handoff(&b.lease(GenerationId(0)).unwrap(), &consumer).unwrap_err();
        assert!(matches!(
            error,
            Error::Contract("history observation collision; owned point preserved")
        ));
        assert_eq!(
            std::fs::read(first.directory.join("inventory.db")).unwrap(),
            before
        );
        let replay = handoff(&a.lease(GenerationId(0)).unwrap(), &consumer).unwrap();
        assert!(replay.replay);
        assert_eq!(
            std::fs::read(first.directory.join("inventory.db")).unwrap(),
            before
        );
        assert!(!root.exists());
    }
    #[test]
    fn published_copy_without_history_receipt_is_reconciled_after_exact_comparison() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("offline");
        let a = fixture(&t.path().join("a"), &root, 0);
        let consumer = t.path().join("history");
        lifecycle::ensure_root(&consumer).unwrap();
        let c = context_id(0);
        let destination = consumer.join(format!(
            "inv-{:032x}-{:016x}",
            c.inventory.0, c.generation.0
        ));
        let partial = a.copy_local(&destination).unwrap();
        assert_eq!(partial.history_generation().unwrap(), None);
        drop(partial);
        let receipt = handoff(&a.lease(c.generation).unwrap(), &consumer).unwrap();
        assert!(receipt.replay);
        let owned = Store::reopen(&destination, Durability::Retained).unwrap();
        assert_eq!(owned.history_generation().unwrap(), Some(c.generation));
        assert!(
            a.same_generation_observation(&owned, c.generation, c.generation, &Default::default())
                .unwrap()
        );
    }
    #[test]
    fn absent_old_guard_table_is_equivalent_to_an_empty_current_guard_table() {
        let t = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let mut stores = Vec::new();
        for suffix in ["old", "current"] {
            let memory = Governor::global()
                .reserve(Category::Sqlite, 1536 * 1024, None)
                .unwrap();
            let mut store = Store::create_admitted(
                &t.path().join(suffix),
                c.inventory,
                c.source,
                Durability::Retained,
                memory,
            )
            .unwrap();
            store.create_generation(c).unwrap();
            stores.push(store);
        }
        stores[0]
            .db
            .execute_batch("DROP TABLE recovery_guard; PRAGMA user_version=1;")
            .unwrap();
        assert!(
            stores[0]
                .same_generation_observation(
                    &stores[1],
                    c.generation,
                    c.generation,
                    &Default::default()
                )
                .unwrap()
        );
    }
    #[test]
    fn native_properties_guard_options_and_evidence_are_part_of_the_observation() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("offline");
        let a = fixture(&t.path().join("a"), &root, 0);
        for variant in 0..=5 {
            let b = fixture(&t.path().join(format!("b-{variant}")), &root, variant);
            assert_eq!(
                a.measurement(GenerationId(0)).unwrap(),
                b.measurement(GenerationId(0)).unwrap()
            );
            assert_eq!(
                a.same_generation_observation(
                    &b,
                    GenerationId(0),
                    GenerationId(0),
                    &Default::default()
                )
                .unwrap(),
                variant == 0,
                "variant {variant}"
            );
        }
    }
    #[test]
    fn persisted_work_state_acquisition_and_aggregate_cells_are_not_ignored() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("offline");
        let a = fixture(&t.path().join("a"), &root, 0);
        for (n, sql) in [
            "UPDATE generations SET revision=x'00000000000000aa'",
            "UPDATE tasks SET listing=listing+1",
            "UPDATE contributions SET attempt=x'00000000000000bb'",
            "UPDATE acquisition_records SET started=started+1",
            "UPDATE measurements SET measured=measured+1",
        ]
        .iter()
        .enumerate()
        {
            let b = fixture(&t.path().join(format!("b-{n}")), &root, 0);
            b.db.execute_batch(sql).unwrap();
            assert!(
                !a.same_generation_observation(
                    &b,
                    GenerationId(0),
                    GenerationId(0),
                    &Default::default()
                )
                .unwrap(),
                "{sql}"
            );
        }
    }
    #[test]
    fn comparison_refuses_unknown_schema_and_oversized_rows_and_restores_connection_limits() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("offline");
        let a = fixture(&t.path().join("a"), &root, 0);
        let b = fixture(&t.path().join("b"), &root, 0);
        let previous = b.db.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap();
        b.db.execute(
            "UPDATE recovery_guard SET proof=?1",
            [vec![7u8; ROW_BYTES as usize + 1]],
        )
        .unwrap();
        assert!(
            a.same_generation_observation(
                &b,
                GenerationId(0),
                GenerationId(0),
                &Default::default()
            )
            .is_err()
        );
        assert_eq!(b.db.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(), previous);
        let c = fixture(&t.path().join("c"), &root, 0);
        c.db.execute_batch("DROP TABLE entry_evidence; CREATE VIEW entry_evidence AS SELECT generation,id,zeroblob(1000000000) AS proof FROM entries;").unwrap();
        assert!(matches!(
            a.same_generation_observation(
                &c,
                GenerationId(0),
                GenerationId(0),
                &Default::default()
            ),
            Err(Error::Contract(
                "unknown observation table definition preserved"
            ))
        ));
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(
            a.same_generation_observation(&a, GenerationId(0), GenerationId(0), &cancel)
                .is_err()
        );
    }
}
