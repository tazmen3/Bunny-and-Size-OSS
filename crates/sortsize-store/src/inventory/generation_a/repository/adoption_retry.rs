//! Explicit retry of a legacy import, preserving its previous physical copy.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

impl Repository {
    pub(super) fn prepare_adoption_retry(
        &mut self,
        legacy: &Store,
        g: GenerationId,
        plan: &mut Pipeline,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let marker = self.directory.join(format!(".pending-adopt-{:016x}", g.0));
        match std::fs::symlink_metadata(&marker) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
        cancelled(cancel)?;
        self.require_allocation(g)?;
        let record: Allocation = read_json(&marker)?;
        if record.version != REPOSITORY_VERSION || record.generation != g.0 {
            return Err(Error::Contract(
                "future or foreign adoption marker preserved",
            ));
        }
        if !legacy.db.is_readonly("main")? {
            return Err(Error::Contract(
                "adoption retry requires certified read-only origin",
            ));
        }
        let origin = self.directory.join("ORIGIN.json");
        let metadata = std::fs::symlink_metadata(&origin)?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Err(Error::Contract("adoption retry origin binding missing"));
        }
        // Existing binding validation performs no write and uses the supplied
        // certified source, avoiding an additional unplanned SQLite connection.
        self.bind_legacy_origin(legacy)?;
        let context = legacy.acquisition_context(g)?;
        let sealed: bool = legacy.db.query_row(
            "SELECT sealed FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| r.get(0),
        )?;
        if sealed || self.generation_path(g).exists() || self.published(g)?.is_some() {
            return Err(Error::Contract(
                "adoption retry is not an unpublished candidate",
            ));
        }
        let path = self.candidate_path(g);
        match std::fs::symlink_metadata(&path) {
            // A kill between quarantine and recreation leaves just the marker.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e.into()),
            Ok(m) if m.is_symlink() || !m.is_dir() => {
                return Err(Error::Contract("unsafe adoption copy preserved"));
            }
            Ok(_) => (),
        }
        lifecycle::check_directory(&path)?;
        lifecycle::artifact_bytes(&path)?;
        let owner = std::fs::symlink_metadata(path.join("owner.lock"))?;
        if !owner.is_file() || owner.is_symlink() || owner.len() != 0 {
            return Err(Error::Contract("adoption copy owner metadata preserved"));
        }
        let source_version: i64 = legacy
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        let partial = self.validate_adoption_copy(
            &path,
            g,
            context,
            source_version,
            cancel,
            plan.publication()?,
        )?;
        #[cfg(any(windows, test))]
        let partial = {
            // Installing an alias is explicit and only follows full validation.
            // Close the first SQLite snapshot AND its inside-directory lock handle,
            // then fully revalidate under a fresh lease opened through that alias.
            // No earlier certificate crosses the handoff interval.
            crate::inventory::owner::install(&path, &partial._owner)?;
            plan.restore_publication(release_store(partial)?)?;
            #[cfg(test)]
            tests::after_anchor_handoff(&path);
            cancelled(cancel)?;
            let partial = self.validate_adoption_copy(
                &path,
                g,
                context,
                source_version,
                cancel,
                plan.publication()?,
            )?;
            crate::inventory::owner::verify_installed(&path, &partial._owner)?;
            partial
        };
        let Store {
            db,
            _canonical,
            _memory,
            _owner,
            ..
        } = partial;
        db.close().map_err(|(_, e)| Error::Sql(e))?;
        drop(_canonical);
        plan.restore_publication(_memory)?;
        cancelled(cancel)?;
        let bytes = lifecycle::artifact_bytes(&path)?;
        let governor = Governor::global();
        let free = sortsize_core::resources::native_free_disk(&path)
            .ok_or(Error::Contract("adoption retry capacity unavailable"))?;
        let closed = governor
            .guard_artifact_group(&path, free, bytes)
            .ok_or(Error::Contract(
                "adoption copy still owns native handles or quota",
            ))?;
        closed.finish(bytes);
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Contract("adoption quarantine clock unavailable"))?
            .as_nanos();
        let nonce = clock
            ^ (u128::from(std::process::id()) << 96)
            ^ u128::from(NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let quarantine = self.directory.join(format!(
            ".quarantine-a-{:032x}-{:016x}-{nonce:032x}",
            self.inventory.0, g.0
        ));
        if std::fs::symlink_metadata(&quarantine).is_ok() {
            return Err(Error::Contract("adoption quarantine destination occupied"));
        }
        let moved = publish_directory(&path, &quarantine);
        if !path.exists()
            && quarantine.is_dir()
            && !governor.relocate_artifact_group(&path, &quarantine)
        {
            return Err(Error::Contract(
                "adoption copy moved; accounting reconciliation required",
            ));
        }
        moved?;
        drop(_owner);
        self.account()?;
        Ok(true)
    }
    fn validate_adoption_copy(
        &self,
        path: &Path,
        g: GenerationId,
        context: AcquisitionContext,
        source_version: i64,
        cancel: &CancellationToken,
        memory: Reservation,
    ) -> Result<Store> {
        lifecycle::check_directory(path)?;
        let mut partial = Store::inspect_admitted(path, cancel, memory)?;
        if partial.inventory != self.inventory || partial.source != self.source {
            return Err(Error::Contract("foreign interrupted adoption preserved"));
        }
        known_schema(&partial.db, source_version)?;
        check_full(&partial.db)?;
        if partial.db.prepare("PRAGMA foreign_key_check")?.exists([])? {
            return Err(Error::Contract(
                "interrupted adoption foreign keys preserved",
            ));
        }
        let identity_rows: i64 =
            partial
                .db
                .query_row("SELECT count(*) FROM identity", [], |r| r.get(0))?;
        let selected: bool = partial.db.query_row(
            "SELECT latest IS NOT NULL OR latest_complete IS NOT NULL FROM identity",
            [],
            |r| r.get(0),
        )?;
        if identity_rows != 1 || selected || partial.history_generation()?.is_some() {
            return Err(Error::Contract(
                "interrupted adoption identity or history mismatch",
            ));
        }
        let ids = partial
            .db
            .prepare("SELECT id FROM generations LIMIT 2")?
            .query_map([], |row| number(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len() > 1 || ids.first().is_some_and(|id| *id != g.0) {
            return Err(Error::Contract("interrupted adoption generation mismatch"));
        }
        if !ids.is_empty() {
            let partial_sealed: bool = partial.db.query_row(
                "SELECT sealed FROM generations WHERE id=?1",
                [unsigned(g.0)],
                |r| r.get(0),
            )?;
            if partial_sealed || partial.acquisition_context(g)? != context {
                return Err(Error::Contract("interrupted adoption context mismatch"));
            }
        }
        partial.integrity_verified = true;
        Ok(partial)
    }
}

#[derive(Clone)]
struct Definition {
    version: i64,
    kind: &'static str,
    sql: String,
}

// Snapshot of our migrations only, never SQL loaded from a source inventory.
// The test compares it with stores produced by every real migration API.
fn definitions() -> Result<BTreeMap<String, Definition>> {
    let mut definitions = BTreeMap::new();
    let mut sources = vec![(1, SCHEMA)];
    // Rust source strings use LF even when the checkout uses CRLF. Match
    // that convention for our embedded snapshot, leaving source DDL intact.
    let migrations = include_str!("adoption_migrations.sql").replace("\r\n", "\n");
    for chunk in migrations.split("-- VERSION ").skip(1) {
        let (version, sql) = chunk
            .split_once('\n')
            .ok_or(Error::Contract("known migration header"))?;
        sources.push((
            version
                .parse::<i64>()
                .map_err(|_| Error::Contract("known migration version"))?,
            sql,
        ));
    }
    for (version, sql) in sources {
        let mut start = 0;
        for (at, character) in sql.char_indices() {
            if character != ';' {
                continue;
            }
            let statement = sql[start..=at].trim();
            let text = std::ffi::CString::new(statement)
                .map_err(|_| Error::Contract("known SQL contains NUL"))?;
            // SAFETY: complete only parses this terminated, binary-authored SQL.
            if unsafe { rusqlite::ffi::sqlite3_complete(text.as_ptr()) } == 0 {
                continue;
            }
            start = at + 1;
            let statement = statement.trim_end_matches(';');
            for (prefix, kind) in [
                ("CREATE TABLE ", "table"),
                ("CREATE INDEX ", "index"),
                ("CREATE TRIGGER ", "trigger"),
            ] {
                if let Some(rest) = statement.strip_prefix(prefix) {
                    let name = rest
                        .split(|c: char| c == '(' || c.is_whitespace())
                        .next()
                        .unwrap();
                    let version = if name == "recovery_guard" { 2 } else { version };
                    if definitions
                        .insert(
                            name.into(),
                            Definition {
                                version,
                                kind,
                                sql: statement.into(),
                            },
                        )
                        .is_some()
                    {
                        return Err(Error::Contract("duplicate known migration object"));
                    }
                }
            }
        }
        if !sql[start..].trim().is_empty() {
            return Err(Error::Contract("incomplete known migration SQL"));
        }
    }
    Ok(definitions)
}
fn known_schema(db: &Connection, source_version: i64) -> Result<()> {
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !(1..=6).contains(&source_version) || !(1..=source_version.max(2)).contains(&version) {
        return Err(Error::Contract(
            "future interrupted adoption format preserved",
        ));
    }
    let expected = definitions()?;
    let mut present = BTreeSet::new();
    let mut objects = db.prepare("SELECT CASE WHEN length(type)<=32 THEN type ELSE NULL END,CASE WHEN length(name)<=128 THEN name ELSE NULL END,CASE WHEN length(tbl_name)<=128 THEN tbl_name ELSE NULL END,CASE WHEN length(sql)<=8192 THEN sql ELSE NULL END FROM sqlite_schema LIMIT 129")?;
    let mut rows = objects.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > 128 {
            return Err(Error::Contract("interrupted adoption schema budget"));
        }
        let (kind, name, table, sql): (String, String, String, Option<String>) =
            (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        if sql.is_none()
            && kind == "index"
            && name.starts_with(&format!("sqlite_autoindex_{table}_"))
            && expected
                .get(&table)
                .is_some_and(|d| d.kind == "table" && d.version <= source_version.max(2))
        {
            continue;
        }
        let definition = expected.get(&name).ok_or(Error::Contract(
            "foreign interrupted adoption object preserved",
        ))?;
        if definition.version > source_version.max(2)
            || definition.kind != kind
            || sql.as_deref() != Some(definition.sql.as_str())
        {
            return Err(Error::Contract(
                "unknown interrupted adoption DDL preserved",
            ));
        }
        present.insert(name);
    }
    if expected
        .iter()
        .any(|(name, d)| d.version <= version && !present.contains(name))
    {
        return Err(Error::Contract(
            "incomplete interrupted adoption core schema",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
