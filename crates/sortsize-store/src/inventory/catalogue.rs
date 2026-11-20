//! Reconstructible local cache catalogue and leased handoff to a future history
//! consumer. Listing/opening a cache never creates a history point.
use super::lifecycle::{Lease, Measurement};
mod observation;
use super::*;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
#[derive(Clone, Debug)]
pub struct Saved {
    pub directory: PathBuf,
    pub context: AcquisitionContext,
    pub root: PathBuf,
    pub started: i64,
    pub measured: Option<i64>,
    pub coverage: ScopeCoverage,
    pub logical: Option<u64>,
    pub unknown: u64,
    /// Fingerprint of recorded native scope evidence. Absence cannot prove continuity.
    pub continuity: Option<[u8; 32]>,
}
#[derive(Default)]
pub struct Catalogue {
    pub rows: Vec<Saved>,
    pub unavailable: usize,
}
impl Store {
    pub fn history_generation(&self) -> Result<Option<GenerationId>> {
        let v: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v < 5 {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT generation FROM history_receipt WHERE singleton=1",
                [],
                |r| number(r, 0),
            )
            .optional()?
            .map(GenerationId))
    }
    pub fn enable_catalogue(&mut self) -> Result<()> {
        self.enable_evidence()?;
        let v: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if (5..=6).contains(&v) {
            return Ok(());
        }
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let tx = self.db.transaction()?;
        tx.execute_batch("CREATE TABLE acquisition_records(generation BLOB PRIMARY KEY REFERENCES generations(id),root BLOB NOT NULL,started INTEGER NOT NULL) STRICT, WITHOUT ROWID; CREATE TABLE history_receipt(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation BLOB NOT NULL REFERENCES generations(id)) STRICT; PRAGMA user_version=5;")?;
        tx.commit()?;
        Ok(())
    }
    pub fn record_acquisition(
        &mut self,
        c: AcquisitionContext,
        root: &Path,
        started: i64,
    ) -> Result<()> {
        if !root.is_absolute() || root.as_os_str().as_encoded_bytes().len() > 32 * 1024 {
            return Err(Error::Contract("invalid recorded root"));
        }
        self.enable_catalogue()?;
        generation(&self.db, c)?;
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        self.db.execute(
            "INSERT INTO acquisition_records VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
            params![
                unsigned(c.generation.0),
                super::queries::native_path(root).encode(),
                started
            ],
        )?;
        let saved = self.saved(c.generation)?;
        if saved.root != root || saved.started != started {
            return Err(Error::Contract("acquisition descriptor immutable"));
        }
        Ok(())
    }
    pub fn saved(&self, g: GenerationId) -> Result<Saved> {
        let v: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        let continuity = self
            .recorded_recovery_guard(g)?
            .filter(|proof| !proof.is_empty())
            .map(|proof| Sha256::digest(proof).into());
        if v < 5 {
            let m = self.measurement(g)?;
            return Ok(Saved::from_measurement(
                self.directory.clone(),
                m,
                continuity,
            ));
        }
        let (root,started,measured,coverage):(Vec<u8>,i64,Option<i64>,i64)=self.db.query_row("SELECT CASE WHEN length(coalesce(m.root,a.root))<=65538 THEN coalesce(m.root,a.root) ELSE NULL END,coalesce(m.started,a.started),m.measured,g.coverage FROM generations g LEFT JOIN measurements m ON m.generation=g.id LEFT JOIN acquisition_records a ON a.generation=g.id WHERE g.id=?1",[unsigned(g.0)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let root = PathBuf::from(crate::scheduler::native_name(
            &NativeName::decode(&root).map_err(|_| Error::Contract("recorded root codec"))?,
        )?);
        let t = self.totals(g)?;
        Ok(Saved {
            directory: self.directory.clone(),
            context: self.acquisition_context(g)?,
            root,
            started,
            measured,
            coverage: if coverage == 2 {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
            logical: t.logical,
            unknown: t.unknown,
            continuity,
        })
    }
    pub fn saved_records(&self) -> Result<Vec<Saved>> {
        let ids: Vec<u64> = self
            .db
            .prepare("SELECT id FROM generations ORDER BY id LIMIT 4")?
            .query_map([], |r| number(r, 0))?
            .collect::<std::result::Result<_, _>>()?;
        if ids.len() > 3 {
            return Err(Error::Contract("generation admission exceeded"));
        }
        ids.into_iter()
            .map(|id| self.saved(GenerationId(id)))
            .collect()
    }
}
impl Saved {
    fn from_measurement(directory: PathBuf, m: Measurement, continuity: Option<[u8; 32]>) -> Self {
        Self {
            directory,
            context: m.context,
            root: m.root,
            started: m.started,
            measured: Some(m.measured),
            coverage: m.coverage,
            logical: m.logical,
            unknown: m.unknown,
            continuity,
        }
    }

    /// Catalogue-level scope matching for display. Comparing requires leased measurements.
    pub fn same_scope(&self, other: &Self) -> bool {
        self.continuity.is_some()
            && self.continuity == other.continuity
            && same_recorded_scope(self.context, &self.root, other.context, &other.root)
    }
}

pub(super) fn same_recorded_scope(
    a: AcquisitionContext,
    a_root: &Path,
    b: AcquisitionContext,
    b_root: &Path,
) -> bool {
    a.source == b.source && a.policy == b.policy && a_root.as_os_str() == b_root.as_os_str()
}
/// An identifier based on native root bytes is only a catalogue hint. Comparison
/// and resume additionally require qualified native continuity evidence.
pub fn source_hint(root: &Path) -> SourceId {
    let mut h = 0x6c62272e07bb014262b821756295c58du128;
    for b in super::queries::native_path(root).encode() {
        h ^= u128::from(b);
        h = h.wrapping_mul(0x1000000000000000000013b);
    }
    SourceId(h)
}
pub fn list(root: &Path, active: Option<&Store>) -> Result<Catalogue> {
    match active {
        Some(s) => list_open(root, &[s]),
        None => list_open(root, &[]),
    }
}
fn repository_marker(path: &Path) -> bool {
    [
        "IDENTITY.json",
        "STATE.json",
        "CURRENT.json",
        "ORIGIN.json",
        "POLICY.json",
    ]
    .iter()
    .any(|name| std::fs::symlink_metadata(path.join(name)).is_ok())
}
fn inspect_metadata_shared(path: &Path) -> Result<Store> {
    let governor = sortsize_core::resources::Governor::global();
    governor.refresh_native();
    let memory = governor
        .reserve(
            sortsize_core::resources::Category::Sqlite,
            1024 * 1024,
            None,
        )
        .ok_or(Error::Contract("catalogue SQLite metadata memory"))?;
    Store::inspect_admitted_shared(path, &Default::default(), memory)
}
fn repository_rows(path: &Path, owners: &[&Store]) -> Result<(Vec<Saved>, usize)> {
    let repository = super::generation_a::Repository::open_readonly(path)?;
    repository.retention()?;
    repository.legacy_origin()?;
    let mut rows = Vec::new();
    let mut unavailable = 0;
    for descriptor in repository.descriptors()? {
        let saved = if let Some(owner) = owners
            .iter()
            .find(|owner| owner.directory() == descriptor.directory)
        {
            owner.saved(descriptor.generation)
        } else {
            inspect_metadata_shared(&descriptor.directory)
                .and_then(|store| store.saved(descriptor.generation))
        };
        match saved {
            Ok(mut saved) => {
                saved.directory = repository.directory().to_owned();
                rows.push(saved);
            }
            Err(_) => unavailable += 1,
        }
    }
    for item in std::fs::read_dir(repository.directory())?.take(4097) {
        let item = item?;
        let name = item.file_name();
        let Some(hex) = name
            .to_str()
            .and_then(|name| name.strip_prefix("candidate-"))
        else {
            continue;
        };
        if hex.len() != 16 {
            return Err(Error::Contract("candidate catalogue identity"));
        }
        let g = GenerationId(
            u64::from_str_radix(hex, 16)
                .map_err(|_| Error::Contract("candidate catalogue identity"))?,
        );
        // A completed snapshot can retain its acquisition candidate. Publish
        // only one descriptor for that logical generation, never a duplicate.
        if rows.iter().any(|saved| saved.context.generation == g) {
            continue;
        }
        let saved =
            if let Some(owner) = owners.iter().find(|owner| owner.directory() == item.path()) {
                owner.saved(g)
            } else {
                inspect_metadata_shared(&item.path()).and_then(|store| store.saved(g))
            };
        match saved {
            Ok(mut saved) => {
                saved.directory = repository.directory().to_owned();
                rows.push(saved);
            }
            Err(_) => unavailable += 1,
        }
    }
    if let Some(origin) = repository.legacy_origin()? {
        let legacy_rows =
            if let Some(owner) = owners.iter().find(|owner| owner.directory() == origin) {
                owner.saved_records()
            } else {
                inspect_metadata_shared(&origin).and_then(|store| store.saved_records())
            };
        match legacy_rows {
            Ok(legacy_rows) => {
                for saved in legacy_rows {
                    if !rows
                        .iter()
                        .any(|row| row.context.generation == saved.context.generation)
                    {
                        // An origin record that has not been extracted remains
                        // explicitly attached to its original physical store.
                        rows.push(saved);
                    }
                }
            }
            Err(_) => unavailable += 1,
        }
    }
    Ok((rows, unavailable))
}
pub fn list_open(root: &Path, owners: &[&Store]) -> Result<Catalogue> {
    sortsize_core::acquisition::local_state_location(root)?;
    if !root.exists() {
        return Ok(Catalogue::default());
    }
    lifecycle::account_root(root)?;
    let directories = if repository_marker(root) {
        vec![std::fs::canonicalize(root)?]
    } else {
        std::fs::read_dir(root)?
            .filter_map(|item| match item {
                Ok(item)
                    if item
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("inv-")) =>
                {
                    Some(Ok(item.path()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<std::io::Result<Vec<_>>>()?
    };
    let mut out = Catalogue::default();
    for directory in directories {
        let result = if repository_marker(&directory) {
            repository_rows(&directory, owners)
        } else if directory.join("inventory.db").is_file() {
            match super::generation_a::Repository::resolve_legacy(&directory, &Default::default()) {
                Ok(Some(_)) => continue, // Its qualified companion is listed once.
                Err(_) => {
                    out.unavailable += 1;
                    continue;
                }
                Ok(None) => (),
            }
            if let Some(store) = owners.iter().find(|s| s.directory() == directory) {
                store.saved_records().map(|rows| (rows, 0))
            } else {
                Store::inspect(&directory)
                    .and_then(|s| s.saved_records())
                    .map(|rows| (rows, 0))
            }
        } else {
            continue;
        };
        match result {
            Ok((mut rows, unavailable)) => {
                out.unavailable += unavailable;
                let bytes = out
                    .rows
                    .iter()
                    .chain(rows.iter())
                    .map(|r| r.root.capacity() + r.directory.capacity() + 512)
                    .sum::<usize>();
                if bytes > 512 * 1024 || out.rows.len() + rows.len() > 128 {
                    return Err(Error::Contract("cache catalogue rows/byte budget"));
                }
                out.rows.append(&mut rows);
            }
            Err(_) => out.unavailable += 1,
        }
    }
    out.rows.sort_by_key(|r| {
        std::cmp::Reverse((
            r.measured.unwrap_or(r.started),
            r.context.inventory.0,
            r.context.generation.0,
        ))
    });
    Ok(out)
}
/// Explicit maintenance. A qualified legacy family is one indivisible ownership
/// unit for preflight; foreign, future or busy companions never authorize purge.
pub fn prune(root: &Path, now: i64) -> Result<usize> {
    sortsize_core::acquisition::local_state_location(root)?;
    if !root.exists() {
        return Ok(0);
    }
    lifecycle::account_root(root)?;
    let direct = repository_marker(root);
    let _budget = if direct { None } else { Some(lock(root)?) };
    let directories = if direct {
        vec![std::fs::canonicalize(root)?]
    } else {
        std::fs::read_dir(root)?
            .filter_map(|item| match item {
                Ok(item)
                    if item
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("inv-")) =>
                {
                    Some(Ok(item.path()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<std::io::Result<Vec<_>>>()?
    };
    let mut handled = std::collections::BTreeSet::new();
    let mut removed = 0;
    for directory in directories {
        let directory = std::fs::canonicalize(directory)?;
        let repository_path = if repository_marker(&directory) {
            Some(directory.clone())
        } else if directory.join("inventory.db").is_file() {
            match super::generation_a::Repository::resolve_legacy(&directory, &Default::default()) {
                Ok(path) => path,
                Err(error) if maintenance_busy(&error) => continue,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        if let Some(repository_path) = repository_path {
            if !handled.insert(repository_path.clone()) {
                continue;
            }
            // Consultation and a live acquisition may retain the repository
            // owner. Read policy/age first; only actual retirement needs its
            // exclusive lock. Busy ownership defers maintenance, never refresh.
            let inspection = super::generation_a::Repository::open_readonly(&repository_path)?;
            let expired = match inspection.expired(now) {
                Ok(expired) => expired,
                Err(error) if maintenance_busy(&error) => continue,
                Err(error) => return Err(error),
            };
            drop(inspection);
            if expired {
                let repository =
                    match super::generation_a::Repository::open_existing(&repository_path) {
                        Ok(repository) => repository,
                        Err(error) if maintenance_busy(&error) => continue,
                        Err(error) => return Err(error),
                    };
                match repository.forget_family(&Default::default()) {
                    Ok(()) => removed += 1,
                    Err(error) if maintenance_busy(&error) => (),
                    Err(error) => return Err(error),
                }
            }
        } else if directory.join("inventory.db").is_file() {
            let expired = match Store::inspect(&directory) {
                Ok(store) => store.history_generation()?.is_none() && store.expired(now)?,
                Err(_) => false,
            };
            if expired && let Ok(store) = Store::reopen(&directory, Durability::Retained) {
                store.forget()?;
                removed += 1;
            }
        }
    }
    lifecycle::account_root(root)?;
    Ok(removed)
}

fn maintenance_busy(error: &Error) -> bool {
    match error {
        Error::Contract(
            "inventory already owned or native lock unavailable"
            | "repository already owned or native lock unavailable"
            | "inventory exclusive writer or retirement is active",
        ) => true,
        Error::Sql(rusqlite::Error::SqliteFailure(code, _)) => matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ),
        _ => false,
    }
}

#[derive(Debug)]
pub struct Receipt {
    pub directory: PathBuf,
    pub inventory: InventoryId,
    pub generation: GenerationId,
    pub replay: bool,
}
/// The receiver owns a coherent FULL copy, never a pointer to purgeable cache.
/// Destination identity makes retries idempotent even after an acknowledgement
/// loss. Only explicitly handing off a measured lease creates a history point.
pub fn handoff(lease: &Lease<'_>, consumer_root: &Path) -> Result<Receipt> {
    lifecycle::ensure_root(consumer_root)?;
    let m = &lease.measurement;
    if lease
        .store
        .history_generation()?
        .is_some_and(|g| g != m.context.generation)
    {
        return Err(Error::Contract(
            "history-owned point differs from requested handoff",
        ));
    }
    let destination = consumer_root.join(format!(
        "inv-{:032x}-{:016x}",
        m.context.inventory.0, m.context.generation.0
    ));
    let replay = destination.exists();
    // Serialize the RO-to-RW recovery handoff in the consumer directory. The
    // first-copy path takes this same lock inside copy_local, so do not nest it.
    let _replay_owner = if replay {
        Some(lock(consumer_root)?)
    } else {
        None
    };
    if replay {
        let existing = super::preview::Unverified::open(&destination, m.context.generation)?
            .verify(&Default::default())?;
        if existing.measurement(m.context.generation)? != *m
            || !lease.store.same_generation_observation(
                &existing,
                m.context.generation,
                m.context.generation,
                &Default::default(),
            )?
        {
            return Err(Error::Contract(
                "history observation collision; owned point preserved",
            ));
        }
        match existing.history_generation()? {
            Some(g) if g == m.context.generation => {
                return Ok(Receipt {
                    directory: destination,
                    inventory: m.context.inventory,
                    generation: g,
                    replay: true,
                });
            }
            Some(_) => return Err(Error::Contract("history-owned point differs from replay")),
            None => (),
        }
        drop(existing);
    }
    let mut owned = if replay {
        Store::reopen(&destination, Durability::Retained)?
    } else {
        lease.store.copy_local(&destination)?
    };
    if owned.measurement(m.context.generation)? != *m
        || !lease.store.same_generation_observation(
            &owned,
            m.context.generation,
            m.context.generation,
            &Default::default(),
        )?
    {
        return Err(Error::Contract(
            "history observation collision; owned point preserved",
        ));
    }
    owned.enable_catalogue()?;
    owned.set_retention(true, 3650)?;
    let _receipt_disk = owned.disk_admission(8 * 1024 * 1024)?;
    owned.db.execute(
        "INSERT INTO history_receipt VALUES(1,?1) ON CONFLICT DO NOTHING",
        [unsigned(m.context.generation.0)],
    )?;
    let generation = GenerationId(owned.db.query_row(
        "SELECT generation FROM history_receipt",
        [],
        |r| number(r, 0),
    )?);
    if generation != m.context.generation {
        return Err(Error::Contract("history receipt identity mismatch"));
    }
    Ok(Receipt {
        directory: destination,
        inventory: m.context.inventory,
        generation,
        replay,
    })
}
#[cfg(test)]
mod tests;
