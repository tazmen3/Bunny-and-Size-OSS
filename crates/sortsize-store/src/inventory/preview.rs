//! A dated, explicitly unverified prefix. It is never a navigation/query capability.
//! Certification checks the same pinned read transaction and retains its owner.
use super::*;
use catalogue::Saved;
use sortsize_core::{
    CancellationToken,
    resources::{Category, Governor, Reservation},
};
use std::path::{Component, PathBuf};

pub struct Unverified {
    store: Store,
    saved: Saved,
    cancel: CancellationToken,
}
pub struct RecordedRow {
    pub path: PathBuf,
    pub kind: Kind,
    /// Recorded file size only. No directory total or current-source assertion.
    pub logical: Option<u64>,
}
pub struct Prefix {
    pub saved: Saved,
    pub rows: Vec<RecordedRow>,
    pub accounted_bytes: usize,
    _memory: Reservation,
}
pub(super) fn require_tables(db: &Connection, version: i64) -> Result<()> {
    for (name, minimum) in [
        ("identity", 1),
        ("generations", 1),
        ("entries", 1),
        ("retention", 3),
        ("measurements", 3),
        ("acquisition_records", 5),
        ("entry_properties", 6),
        ("collection_options", 6),
    ] {
        if version < minimum {
            continue;
        }
        let (kind, definition): (String, String) = db.query_row(
            "SELECT type,substr(sql,1,64) FROM sqlite_schema WHERE name=?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if kind != "table"
            || !definition
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE TABLE")
        {
            return Err(Error::Contract("preview requires canonical tables"));
        }
    }
    Ok(())
}

impl Unverified {
    pub fn open(directory: &Path, generation: GenerationId) -> Result<Self> {
        Self::open_cancellable(directory, generation, &Default::default())
    }
    pub fn open_cancellable(
        directory: &Path,
        generation: GenerationId,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let store = Store::inspect_cancellable(directory, cancel)?;
        // Inspection pins before even identity/retention are read. Keep that
        // transaction through the full check AND certified RO use.
        debug_assert!(!store.db.is_autocommit());
        let app: i64 = store
            .db
            .pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = store
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if app != APPLICATION_ID || !(1..=6).contains(&version) {
            return Err(Error::Contract("unrecognized or future preview format"));
        }
        require_tables(&store.db, version)?;
        if version >= 3
            && store.db.query_row(
                "SELECT semantic FROM retention WHERE singleton=1",
                [],
                |r| r.get::<_, i64>(0),
            )? != 1
        {
            return Err(Error::Contract("future preview retention semantics"));
        }
        let identity: (Vec<u8>, Vec<u8>) = store.db.query_row(
            "SELECT substr(inventory,1,17),substr(source,1,17) FROM identity",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if identity.0 != store.inventory.0.to_be_bytes()
            || identity.1 != store.source.0.to_be_bytes()
        {
            return Err(Error::Contract("preview identity changed before snapshot"));
        }
        let saved = store.saved(generation)?;
        if saved.context.inventory != store.inventory
            || saved.context.source != store.source
            || !saved.root.is_absolute()
            || saved.root.capacity() > 64 * 1024
        {
            return Err(Error::Contract("invalid preview descriptor"));
        }
        Ok(Self {
            store,
            saved,
            cancel: cancel.clone(),
        })
    }
    pub fn prefix(&self, cancel: &CancellationToken) -> Result<Prefix> {
        const BYTES: usize = 512 * 1024;
        let memory = Governor::global()
            .reserve(Category::Publications, BYTES as u64, None)
            .ok_or(Error::Contract("unverified preview publication budget"))?;
        let mut rows = Vec::with_capacity(200);
        // Leave room inside the reservation for raw/native decoding and one
        // candidate path that may be rejected at the publication boundary.
        let mut bytes = 256 * 1024
            + 2 * (self.saved.root.capacity() + self.saved.directory.capacity())
            + rows.capacity() * std::mem::size_of::<RecordedRow>();
        let mut stmt = self.store.db.prepare("SELECT CASE WHEN length(name)<=32768 THEN name ELSE NULL END,kind,logical FROM entries WHERE generation=?1 AND parent=?2 AND id>?2 AND present=1 ORDER BY id LIMIT 200")?;
        let mut source = stmt.query(params![
            unsigned(self.saved.context.generation.0),
            unsigned(0)
        ])?;
        while let Some(row) = source.next()? {
            if cancel.is_cancelled() || self.cancel.is_cancelled() {
                return Err(Error::Contract("inventory verification cancelled"));
            }
            let raw: Vec<u8> = row.get(0)?;
            let name = crate::scheduler::native_name(
                &NativeName::decode(&raw)
                    .map_err(|_| Error::Contract("preview native name codec"))?,
            )?;
            let component = Path::new(&name);
            let mut parts = component.components();
            if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
                return Err(Error::Contract("invalid preview name component"));
            }
            let path = self.saved.root.join(name);
            if path.as_os_str().as_encoded_bytes().len() > 32 * 1024 {
                return Err(Error::Contract("preview path byte budget"));
            }
            let cost = path.capacity() + 128;
            if bytes + cost > BYTES {
                break;
            }
            bytes += cost;
            let kind = match row.get::<_, i64>(1)? {
                0 => Kind::Unknown,
                1 => Kind::File,
                2 => Kind::Directory,
                3 => Kind::Symlink,
                4 => Kind::Special,
                _ => return Err(Error::Contract("preview entry kind")),
            };
            rows.push(RecordedRow {
                path,
                kind,
                logical: if kind == Kind::File {
                    opt_number(row, 2)?
                } else {
                    None
                },
            });
        }
        Ok(Prefix {
            saved: self.saved.clone(),
            rows,
            accounted_bytes: bytes,
            _memory: memory,
        })
    }
    /// Consume the unverified capability. On error/cancel the RO connection is
    /// closed and its snapshot released; no RW connection/checkpoint is opened.
    pub fn verify(mut self, cancel: &CancellationToken) -> Result<Store> {
        if cancel.is_cancelled() || self.cancel.is_cancelled() {
            return Err(Error::Contract("inventory verification cancelled"));
        }
        let stop = cancel.clone();
        let original = self.cancel.clone();
        self.store.db.progress_handler(
            1000,
            Some(move || stop.is_cancelled() || original.is_cancelled()),
        )?;
        let result = self
            .store
            .db
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0));
        self.store.db.progress_handler(0, None::<fn() -> bool>)?;
        if cancel.is_cancelled() || self.cancel.is_cancelled() {
            return Err(Error::Contract("inventory verification cancelled"));
        }
        if result? != "ok" {
            return Err(Error::Contract(
                "corrupt inventory group preserved; preview invalidated",
            ));
        }
        let saved = self.store.saved(self.saved.context.generation)?;
        if saved.context != self.saved.context
            || saved.root != self.saved.root
            || saved.started != self.saved.started
            || saved.measured != self.saved.measured
        {
            return Err(Error::Contract("preview generation changed"));
        }
        self.store.integrity_verified = true;
        Ok(self.store)
    }
}

// TEMP projection steps may run inside the pinned, certified RO transaction.
// Only this internal constant savepoint is admitted; failures roll it back.
pub(super) struct ProjectionStep<'a> {
    db: &'a Connection,
    finished: bool,
}
impl<'a> ProjectionStep<'a> {
    pub fn begin(db: &'a Connection) -> Result<Self> {
        db.execute_batch("SAVEPOINT bunny_folder_step;")?;
        Ok(Self {
            db,
            finished: false,
        })
    }
    pub fn commit(mut self) -> Result<()> {
        self.db.execute_batch("RELEASE bunny_folder_step;")?;
        self.finished = true;
        Ok(())
    }
}
impl std::ops::Deref for ProjectionStep<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.db
    }
}
impl Drop for ProjectionStep<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self
                .db
                .execute_batch("ROLLBACK TO bunny_folder_step; RELEASE bunny_folder_step;");
        }
    }
}
#[cfg(test)]
mod tests;
