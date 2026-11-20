//! Local retention and qualified legacy families. Never probes recorded sources.
use super::*;

#[derive(Serialize, Deserialize)]
struct Policy {
    version: u32,
    enabled: bool,
    days: u32,
    pending_purge: bool,
}
#[derive(Serialize, Deserialize)]
struct Anchor {
    #[serde(deserialize_with = "bounded_sequence::<_, u8, 81>")]
    context: Vec<u8>,
    #[serde(deserialize_with = "bounded_sequence::<_, u8, 65538>")]
    root: Vec<u8>,
    started: i64,
    measured: Option<i64>,
}
#[derive(Serialize, Deserialize)]
struct Origin {
    version: u32,
    #[serde(deserialize_with = "bounded_sequence::<_, u8, 65538>")]
    directory: Vec<u8>,
    inventory: [u8; 16],
    source: [u8; 16],
    #[serde(deserialize_with = "bounded_sequence::<_, u64, 3>")]
    generations: Vec<u64>,
    #[serde(deserialize_with = "bounded_sequence::<_, Anchor, 3>")]
    anchors: Vec<Anchor>,
}

impl Repository {
    pub fn retention(&self) -> Result<lifecycle::Retention> {
        let path = self.directory.join("POLICY.json");
        let policy = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Policy {
                version: REPOSITORY_VERSION,
                enabled: false,
                days: 30,
                pending_purge: false,
            },
            Err(e) => return Err(e.into()),
            Ok(_) => read_json(&path)?,
        };
        if policy.version != REPOSITORY_VERSION || !(1..=3650).contains(&policy.days) {
            return Err(Error::Contract(
                "future repository retention policy preserved",
            ));
        }
        let origin_path = self.directory.join("ORIGIN.json");
        match std::fs::symlink_metadata(&origin_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
            Ok(_) => {
                let origin: Origin = read_json(&origin_path)?;
                validate_origin_namespace(&origin)?;
                let legacy = decode_path(&origin.directory)?;
                if origin.version != REPOSITORY_VERSION
                    || origin.inventory != self.inventory.0.to_be_bytes()
                    || origin.source != self.source.0.to_be_bytes()
                    || native_companion_path(&legacy)? != self.directory
                {
                    return Err(Error::Contract(
                        "foreign/future repository origin policy preserved",
                    ));
                }
            }
        }
        Ok(lifecycle::Retention {
            enabled: policy.enabled,
            days: policy.days,
            pending_purge: policy.pending_purge,
        })
    }
    /// Header-only validation for every repository open path. It never opens
    /// or stats the origin cache and never touches the acquired source.
    pub(super) fn validate_policy_documents(&self) -> Result<()> {
        self.retention().map(|_| ())
    }
    pub fn set_retention(&mut self, enabled: bool, days: u32) -> Result<lifecycle::Retention> {
        self.require_writer()?;
        self.retention()?;
        if !(1..=3650).contains(&days) {
            return Err(Error::Contract("retention days outside 1..3650"));
        }
        write_json(
            &self.directory,
            "POLICY.json",
            &Policy {
                version: REPOSITORY_VERSION,
                enabled,
                days,
                pending_purge: !enabled,
            },
            true,
        )?;
        self.account()?;
        let actual = self.retention()?;
        if actual.enabled != enabled || actual.days != days {
            return Err(Error::Contract("repository retention readback"));
        }
        Ok(actual)
    }
    pub fn expired(&self, now: i64) -> Result<bool> {
        let policy = self.retention()?;
        let mut latest = self
            .descriptors()?
            .iter()
            .map(|d| d.measured.unwrap_or(d.started))
            .max();
        if let Some(path) = self.legacy_origin()? {
            let legacy = Store::certify_readonly(&path, &CancellationToken::default())?;
            for saved in legacy.saved_records()? {
                let date = saved.measured.unwrap_or(saved.started);
                latest = Some(latest.map_or(date, |previous| previous.max(date)));
            }
        }
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if let Some(g) = item
                .file_name()
                .to_str()
                .and_then(|name| parse_name(name, "candidate-", ""))
            {
                let candidate =
                    Store::certify_readonly(&item.path(), &CancellationToken::default())?;
                if let Ok(saved) = candidate.saved(GenerationId(g)) {
                    let date = saved.measured.unwrap_or(saved.started);
                    latest = Some(latest.map_or(date, |previous| previous.max(date)));
                }
            }
        }
        Ok(policy.pending_purge
            || latest
                .is_some_and(|date| now.saturating_sub(date) >= i64::from(policy.days) * 86400))
    }
    /// Native sibling name is only a location, never proof of family ownership.
    pub fn companion_path(legacy: &Path) -> Result<PathBuf> {
        let metadata = std::fs::symlink_metadata(legacy)?;
        if metadata.is_symlink() || !metadata.is_dir() {
            return Err(Error::Contract("unsafe legacy directory"));
        }
        let legacy = std::fs::canonicalize(legacy)?;
        native_companion_path(&legacy)
    }
    /// Explicit adoption. The old database remains read-only and is not moved.
    pub fn bind_legacy_origin(&mut self, legacy: &Store) -> Result<()> {
        self.require_writer()?;
        legacy.require_integrity()?;
        if legacy.inventory != self.inventory
            || legacy.source != self.source
            || Self::companion_path(legacy.directory())? != self.directory
        {
            return Err(Error::Contract("legacy family identity/location mismatch"));
        }
        if legacy.history_generation()?.is_some() {
            return Err(Error::Contract(
                "history-owned legacy point is not a cache family",
            ));
        }
        let path = self.directory.join("ORIGIN.json");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let origin: Origin = read_json(&path)?;
                self.validate_origin(&origin, legacy)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        self.reserve_legacy_ids(legacy)?;
        let generations = legacy_ids(legacy)?;
        let anchors = legacy
            .saved_records()?
            .into_iter()
            .map(|s| Anchor {
                context: context(s.context),
                root: queries::native_path(&s.root).encode(),
                started: s.started,
                measured: s.measured,
            })
            .collect();
        write_json(
            &self.directory,
            "ORIGIN.json",
            &Origin {
                version: REPOSITORY_VERSION,
                directory: queries::native_path(legacy.directory()).encode(),
                inventory: self.inventory.0.to_be_bytes(),
                source: self.source.0.to_be_bytes(),
                generations,
                anchors,
            },
            false,
        )?;
        self.account()?;
        Ok(())
    }
    fn validate_origin(&self, origin: &Origin, legacy: &Store) -> Result<()> {
        validate_origin_namespace(origin)?;
        if origin.version != REPOSITORY_VERSION
            || origin.inventory != self.inventory.0.to_be_bytes()
            || origin.source != self.source.0.to_be_bytes()
            || legacy.inventory != self.inventory
            || legacy.source != self.source
            || decode_path(&origin.directory)? != legacy.directory()
            || Self::companion_path(legacy.directory())? != self.directory
        {
            return Err(Error::Contract("foreign/future legacy family preserved"));
        }
        if legacy.history_generation()?.is_some() {
            return Err(Error::Contract("history-owned legacy family protected"));
        }
        let current_ids = legacy_ids(legacy)?;
        if current_ids.iter().any(|g| !origin.generations.contains(g)) {
            return Err(Error::Contract(
                "legacy changed its generation namespace after adoption",
            ));
        }
        for anchor in &origin.anchors {
            let c = decode_context(&anchor.context)?;
            let actual = legacy.saved(c.generation)?;
            if actual.root != decode_path(&anchor.root)?
                || actual.started != anchor.started
                || (anchor.measured.is_some()
                    && (actual.measured != anchor.measured || actual.context != c))
            {
                return Err(Error::Contract("legacy family observation binding changed"));
            }
        }
        Ok(())
    }
    pub fn legacy_origin(&self) -> Result<Option<PathBuf>> {
        self.validate_policy_documents()?;
        let path = self.directory.join("ORIGIN.json");
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
        let origin: Origin = read_json(&path)?;
        let path = decode_path(&origin.directory)?;
        let legacy = Store::certify_readonly(&path, &CancellationToken::default())?;
        self.validate_origin(&origin, &legacy)?;
        Ok(Some(path))
    }
    pub fn resolve_legacy(legacy: &Path, cancel: &CancellationToken) -> Result<Option<PathBuf>> {
        cancelled(cancel)?;
        let companion = Self::companion_path(legacy)?;
        match std::fs::symlink_metadata(&companion) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(metadata) if metadata.is_symlink() || !metadata.is_dir() => {
                return Err(Error::Contract("unsafe legacy companion preserved"));
            }
            Ok(_) => (),
        }
        let repository = Self::open_readonly(&companion)?;
        let origin = repository
            .legacy_origin()?
            .ok_or(Error::Contract("unbound legacy companion preserved"))?;
        if origin != std::fs::canonicalize(legacy)? {
            return Err(Error::Contract("legacy companion belongs to another cache"));
        }
        cancelled(cancel)?;
        Ok(Some(companion))
    }
    /// Independent retained copy. ORIGIN is deliberately not inherited.
    pub fn copy_to(&self, destination: &Path, cancel: &CancellationToken) -> Result<Repository> {
        self.copy_snapshot(destination, None, cancel)
    }
    /// Called by the acquisition owner between committed batches. The snapshot
    /// retains the recovery guard and context, never a native directory offset.
    pub fn copy_to_from_active(
        &self,
        destination: &Path,
        active: &Store,
        cancel: &CancellationToken,
    ) -> Result<Repository> {
        self.copy_snapshot(destination, Some(active), cancel)
    }
    fn copy_snapshot(
        &self,
        destination: &Path,
        active: Option<&Store>,
        cancel: &CancellationToken,
    ) -> Result<Repository> {
        cancelled(cancel)?;
        self.retention()?;
        self.legacy_origin()?;
        validate_layout(&self.directory)?;
        absent_destination(destination)?;
        let parent = destination
            .parent()
            .ok_or(Error::Contract("repository copy parent"))?;
        lifecycle::check_directory(parent)?;
        let name = destination
            .file_name()
            .ok_or(Error::Contract("repository copy name"))?;
        let destination = std::fs::canonicalize(parent)?.join(name);
        if destination.starts_with(&self.directory) {
            return Err(Error::Contract(
                "repository copy cannot be inside its source",
            ));
        }
        let _snapshot_owner = if self.read_only {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.directory.join("owner.lock"))?;
            file.try_lock_shared()
                .map_err(|_| Error::Contract("repository writer busy during copy"))?;
            Some(file)
        } else {
            None
        };
        let descriptors = self.descriptors()?;
        let active_generation = if let Some(store) = active {
            store.require_integrity()?;
            let ids = legacy_ids(store)?;
            if ids.len() != 1 || store.inventory != self.inventory || store.source != self.source {
                return Err(Error::Contract(
                    "active snapshot identity is not one candidate",
                ));
            }
            let g = GenerationId(ids[0]);
            self.require_allocation(g)?;
            if store.directory() != self.candidate_path(g) {
                return Err(Error::Contract(
                    "active snapshot is not the repository candidate",
                ));
            }
            Some(g)
        } else {
            None
        };
        let mut candidates = Vec::new();
        for item in std::fs::read_dir(&self.directory)?.take(4097) {
            let item = item?;
            if let Some(g) = item
                .file_name()
                .to_str()
                .and_then(|n| parse_name(n, "candidate-", ""))
            {
                let g = GenerationId(g);
                self.require_allocation(g)?;
                if !descriptors.iter().any(|d| d.generation == g) || active_generation == Some(g) {
                    candidates.push(g);
                }
            }
            if candidates.len() > 3 {
                return Err(Error::Contract("candidate copy admission exceeded"));
            }
        }
        if descriptors.is_empty() && candidates.is_empty() {
            return Err(Error::Contract(
                "no published generation or candidate to copy",
            ));
        }
        for g in &candidates {
            let owned;
            let source = if active_generation == Some(*g) {
                active.ok_or(Error::Contract("missing active snapshot owner"))?
            } else {
                owned = Store::certify_readonly(&self.candidate_path(*g), cancel)?;
                &owned
            };
            if source.inventory != self.inventory
                || source.source != self.source
                || legacy_ids(source)? != [g.0]
            {
                return Err(Error::Contract("candidate copy identity mismatch"));
            }
            let saved = source.saved(*g)?;
            if sortsize_core::acquisition::contains_path(&saved.root, &destination) {
                return Err(Error::Contract(
                    "repository copy must be outside candidate roots",
                ));
            }
        }
        for d in &descriptors {
            if sortsize_core::acquisition::contains_path(&d.root, &destination) {
                return Err(Error::Contract(
                    "repository copy must be outside acquired roots",
                ));
            }
        }
        // Serialize application publishers in this private destination parent;
        // acquisition-root validation above precedes creating its lock file.
        let _destination_owner = lock(parent)?;
        let identity: Identity = read_json(&self.directory.join("IDENTITY.json"))?;
        validate_identity(&identity, self.inventory, self.source)?;
        let state: State = read_json(&self.directory.join("STATE.json"))?;
        let current: Current = read_json(&self.directory.join("CURRENT.json"))?;
        if state.version != REPOSITORY_VERSION || current.version != REPOSITORY_VERSION {
            return Err(Error::Contract("future repository copy preserved"));
        }
        let total = self.account()?;
        let governor = Governor::global();
        let mut disk = governor
            .reserve_disk(
                total
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(16 * FRAME))
                    .ok_or(Error::Contract("repository copy byte overflow"))?,
            )
            .ok_or(Error::Contract("repository copy disk quota"))?;
        let temporary = tempfile::Builder::new()
            .prefix(".stage-repository-copy-")
            .tempdir_in(parent)?;
        let stage = temporary.path().to_owned();
        temporary.close()?;
        lifecycle::private_directory(&stage)?;
        // Repository and Store normalize their own group paths. Keep the copy
        // root in that same spelling when transferring or retiring ledgers.
        let stage = std::fs::canonicalize(&stage)?;
        let mut target = Repository::open(&stage, self.inventory, self.source)?;
        let policy = self.retention()?;
        write_json(
            &stage,
            "POLICY.json",
            &Policy {
                version: REPOSITORY_VERSION,
                enabled: policy.enabled,
                days: policy.days,
                pending_purge: policy.pending_purge,
            },
            true,
        )?;
        let buffer_memory = governor
            .reserve(Category::Transport, 64 * 1024, None)
            .ok_or(Error::Contract("repository copy buffer memory"))?;
        for d in &descriptors {
            cancelled(cancel)?;
            let cap = self.open_generation(d.generation, cancel)?;
            if cap
                .store
                .db
                .pragma_query_value::<String, _>(None, "journal_mode", |r| r.get(0))?
                != "delete"
            {
                return Err(Error::Contract(
                    "repository copy requires a standalone published generation",
                ));
            }
            let output = target.generation_path(d.generation);
            lifecycle::private_directory(&output)?;
            // This is an explicit new writer stage, not a RO repair. Publish
            // the stable owner inode before any later certified RO open.
            let owner = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(output.join("owner.lock"))?;
            owner
                .try_lock()
                .map_err(|_| Error::Contract("new copy group owner unavailable"))?;
            let mut input = File::open(d.directory.join("inventory.db"))?;
            let expected_bytes = input.metadata()?.len();
            let guard = governor
                .guard_artifact_group(
                    &output,
                    sortsize_core::resources::native_free_disk(&output)
                        .ok_or(Error::Contract("copy free disk unavailable"))?,
                    0,
                )
                .ok_or(Error::Contract("published copy artifact ledger"))?;
            if !guard.admit(expected_bytes, Some(&mut disk)) {
                return Err(Error::Contract("published copy byte admission"));
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join("inventory.db"))?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                cancelled(cancel)?;
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                out.write_all(&buffer[..count])?;
            }
            out.sync_all()?;
            drop(out);
            drop(input);
            drop(cap);
            owner.sync_all()?;
            lifecycle::sync_directory(&output)?;
            drop(owner);
            guard.finish(lifecycle::artifact_bytes(&output)?);
            let certified = CertifiedGeneration::open(&output, d.generation, &d.root, cancel)?;
            target.check_capability(d, &certified)?;
            drop(certified);
            target.write_measurement(d)?;
            target.account()?;
        }
        // Raw copies are now charged as owned bytes. Each following candidate
        // receives its own known peak envelope through the canonical VFS.
        drop(disk);
        for g in &candidates {
            cancelled(cancel)?;
            // Reserve the destination connection before a closed candidate's
            // native cache is admitted. This is planning, not a budget raise.
            let memory = small_sqlite()?;
            let owned;
            let input = if active_generation == Some(*g) {
                active.ok_or(Error::Contract("missing active snapshot owner"))?
            } else {
                owned = Store::certify_readonly(&self.candidate_path(*g), cancel)?;
                &owned
            };
            let planned = lifecycle::artifact_bytes(input.directory())?
                .checked_mul(2)
                .and_then(|n| n.checked_add(16 * FRAME))
                .ok_or(Error::Contract("candidate copy byte overflow"))?;
            let disk = input.disk_admission_at(planned, &stage)?;
            let mut snapshot = Store::create_canonical_admitted(
                &target.candidate_path(*g),
                self.inventory,
                self.source,
                Durability::Retained,
                memory,
                planned,
                Some(disk),
            )?;
            {
                let backup = rusqlite::backup::Backup::new(&input.db, &mut snapshot.db)?;
                loop {
                    cancelled(cancel)?;
                    match backup.step(256)? {
                        rusqlite::backup::StepResult::Done => break,
                        rusqlite::backup::StepResult::More
                        | rusqlite::backup::StepResult::Busy
                        | rusqlite::backup::StepResult::Locked => {
                            std::thread::sleep(std::time::Duration::from_millis(1))
                        }
                        _ => return Err(Error::Contract("unsupported SQLite backup progress")),
                    }
                }
            }
            snapshot.db.pragma_update(None, "synchronous", "FULL")?;
            check_full(&snapshot.db)?;
            let (busy, _, _): (i64, i64, i64) =
                snapshot
                    .db
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?;
            if busy != 0 {
                return Err(Error::Contract("candidate copy checkpoint busy"));
            }
            let path = snapshot.directory().to_owned();
            drop(snapshot);
            sync_group(&path)?;
            target.account()?;
        }
        drop(buffer_memory);
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            let name = item.file_name();
            if let Some(g) = name
                .to_str()
                .and_then(|n| parse_name(n, "allocation-", ".json"))
            {
                self.require_allocation(GenerationId(g))?;
                let record: Allocation = read_json(&item.path())?;
                write_json(&stage, name.to_str().unwrap(), &record, false)?;
            }
        }
        write_json(&stage, "STATE.json", &state, true)?;
        write_json(&stage, "CURRENT.json", &current, true)?;
        target.current = current;
        target.account()?;
        drop(target);
        lifecycle::sync_directory(&stage)?;
        cancelled(cancel)?;
        absent_destination(&destination)?;
        let published = publish_directory(&stage, &destination);
        if !stage.exists() && destination.exists() {
            // A rename moves owned bytes; it does not duplicate them. Rebase
            // every registration, including an active candidate, only after
            // the old filesystem names are actually gone.
            governor.forget_deleted_artifact_group(&stage);
            for descriptor in &descriptors {
                governor.forget_deleted_artifact_group(
                    &stage.join(format!("generation-{:016x}", descriptor.generation.0)),
                );
            }
            for g in &candidates {
                governor
                    .forget_deleted_artifact_group(&stage.join(format!("candidate-{:016x}", g.0)));
            }
            Repository::account_directory(&destination)?;
        }
        if let Err(error) = published {
            if destination.exists() {
                return Err(Error::Contract(
                    "repository copy published; durability/acknowledgement requires reconciliation",
                ));
            }
            return Err(error);
        }
        Repository::open_existing(&destination)
    }
    /// A recognized filename alone does not confer ownership of its database.
    /// The returned exclusive lease is retained until retirement completes.
    pub(super) fn validate_owned_group(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Store> {
        self.validate_policy_documents()?;
        if path.parent() != Some(self.directory.as_path()) {
            return Err(Error::Contract(
                "retirement group is outside its repository",
            ));
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(Error::Contract("retirement native group name"))?;
        let (g, empty_stage) = owned_generation_name(name, self.inventory)?;
        self.require_allocation(g)?;
        let store = validate_exclusive_small(path, cancel, empty_stage.then_some(g))?;
        if store.inventory != self.inventory || store.source != self.source {
            return Err(Error::Contract("foreign repository database preserved"));
        }
        let ids = legacy_ids(&store)?;
        if ids.is_empty() && empty_stage {
            return Ok(store);
        }
        if ids != [g.0] {
            return Err(Error::Contract(
                "repository group generation binding mismatch",
            ));
        }
        let c = store.acquisition_context(g)?;
        if c.inventory != self.inventory || c.source != self.source || c.generation != g {
            return Err(Error::Contract(
                "foreign repository generation context preserved",
            ));
        }
        if let Some(expected) = self.published(g)? {
            let coverage: i64 = store.db.query_row(
                "SELECT coverage FROM generations WHERE id=?1",
                [unsigned(g.0)],
                |r| r.get(0),
            )?;
            let complete = expected.coverage == ScopeCoverage::Complete;
            if c != expected.context || (coverage == 2) != complete {
                return Err(Error::Contract(
                    "repository retirement context/coverage binding mismatch",
                ));
            }
            let version: i64 = store
                .db
                .pragma_query_value(None, "user_version", |r| r.get(0))?;
            let has_measurement = version >= 3
                && store.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM measurements WHERE generation=?1)",
                    [unsigned(g.0)],
                    |r| r.get::<_, bool>(0),
                )?;
            let has_acquisition = version >= 5
                && store.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM acquisition_records WHERE generation=?1)",
                    [unsigned(g.0)],
                    |r| r.get::<_, bool>(0),
                )?;
            if has_measurement || has_acquisition {
                let saved = store.saved(g)?;
                if saved.root != expected.root
                    || saved.started != expected.started
                    || saved.measured != expected.measured
                {
                    return Err(Error::Contract(
                        "repository retirement descriptor binding mismatch",
                    ));
                }
            } else if expected.started != 0 || expected.measured.is_some() {
                return Err(Error::Contract(
                    "repository retirement missing dated descriptor",
                ));
            }
            let has_model: bool = store.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='generation_a_meta')",
                [],
                |r| r.get(0),
            )?;
            if has_model {
                let root: Vec<u8> = store.db.query_row("SELECT CASE WHEN length(root)<=65538 THEN root ELSE NULL END FROM generation_a_meta WHERE singleton=1", [], |r| r.get(0))?;
                if decode_path(&root)? != expected.root {
                    return Err(Error::Contract(
                        "repository retirement model root binding mismatch",
                    ));
                }
            }
        }
        Ok(store)
    }
    /// Preflight every family lease before deleting any family artifact.
    pub fn forget_family(self, cancel: &CancellationToken) -> Result<()> {
        self.require_writer()?;
        self.retention()?;
        let Some(origin_path) = self.legacy_origin()? else {
            return self.forget(cancel);
        };
        let origin: Origin = read_json(&self.directory.join("ORIGIN.json"))?;
        let legacy = validate_exclusive_small(&origin_path, cancel, None)?;
        self.validate_origin(&origin, &legacy)?;
        let mut groups = Vec::new();
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if item.file_type()?.is_dir() {
                groups.push((
                    item.path(),
                    self.validate_owned_group(&item.path(), cancel)?,
                ));
            }
        }
        cancelled(cancel)?;
        crate::inventory::owner::preflight_removal(&self.directory)?;
        for (path, store) in groups {
            remove_owned_group(&path, store)?;
        }
        // Keep stable family lock inodes, just as legacy Store::forget does.
        let Store { db, _owner, .. } = legacy;
        db.close().map_err(|(_, e)| Error::Sql(e))?;
        lifecycle::remove_canonical_temporaries(&origin_path)?;
        for name in [
            "inventory.db",
            "inventory.db-wal",
            "inventory.db-shm",
            "inventory.db-journal",
        ] {
            let file = origin_path.join(name);
            if file.exists() {
                std::fs::remove_file(file)?;
            }
        }
        lifecycle::sync_directory(&origin_path)?;
        Governor::global().forget_deleted_artifact_group(&origin_path);
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if item.file_name() != "owner.lock" {
                std::fs::remove_file(item.path())?;
            }
        }
        lifecycle::sync_directory(&self.directory)?;
        Governor::global().forget_deleted_artifact_group(&self.directory);
        drop(_owner);
        Ok(())
    }
    /// Account recognizable owned files even if their document version is future.
    pub fn account_directory(path: &Path) -> Result<u64> {
        validate_layout(path)?;
        let free = sortsize_core::resources::native_free_disk(path)
            .ok_or(Error::Contract("repository free capacity unavailable"))?;
        let governor = Governor::global();
        let mut metadata = 0u64;
        let mut total = 0u64;
        for item in std::fs::read_dir(path)? {
            let item = item?;
            let m = std::fs::symlink_metadata(item.path())?;
            let bytes = if m.is_dir() {
                let bytes = lifecycle::artifact_bytes(&item.path())?;
                if !governor.observe_artifact_group(&item.path(), free, bytes) {
                    return Err(Error::Contract("repository group accounting admission"));
                }
                bytes
            } else {
                metadata = metadata
                    .checked_add(m.len())
                    .ok_or(Error::Contract("repository metadata overflow"))?;
                m.len()
            };
            total = total
                .checked_add(bytes)
                .ok_or(Error::Contract("repository accounting overflow"))?;
        }
        if !governor.observe_artifact_group(path, free, metadata) {
            return Err(Error::Contract("repository metadata accounting admission"));
        }
        Ok(total)
    }
}

fn legacy_ids(legacy: &Store) -> Result<Vec<u64>> {
    let mut statement = legacy
        .db
        .prepare("SELECT id FROM generations ORDER BY id")?;
    let result = statement
        .query_map([], |row| number(row, 0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if result.len() > 3 {
        return Err(Error::Contract("legacy generation binding budget"));
    }
    Ok(result)
}

fn small_sqlite() -> Result<Reservation> {
    let governor = Governor::global();
    governor.refresh_native();
    let usage = governor.usage();
    let total = usage
        .categories
        .iter()
        .try_fold(0u64, |sum, n| sum.checked_add(*n))
        .ok_or(Error::Contract("copy memory accounting overflow"))?;
    if usage.limits.bytes.saturating_sub(total) < FRAME {
        return Err(Error::Contract("copy memory budget"));
    }
    governor
        .reserve(Category::Sqlite, FRAME, None)
        .ok_or(Error::Contract("copy SQLite memory budget"))
}
fn validate_exclusive_small(
    path: &Path,
    cancel: &CancellationToken,
    partial: Option<GenerationId>,
) -> Result<Store> {
    let mut store = Store::inspect_admitted(path, cancel, small_sqlite()?)?;
    let version: i64 = store
        .db
        .pragma_query_value(None, "user_version", |row| row.get(0))?;
    for expected in SCHEMA
        .split(';')
        .map(str::trim)
        .filter(|sql| sql.starts_with("CREATE TABLE "))
    {
        let name = expected
            .strip_prefix("CREATE TABLE ")
            .unwrap()
            .split('(')
            .next()
            .unwrap()
            .trim();
        if name == "recovery_guard" && version < 2 {
            continue;
        }
        let actual: String = store.db.query_row(
            "SELECT CASE WHEN length(sql)<=8192 THEN sql ELSE NULL END FROM sqlite_schema WHERE type='table' AND name=?1",
            [name],
            |row| row.get(0),
        )?;
        if actual != expected {
            return Err(Error::Contract(
                "unknown canonical retirement table preserved",
            ));
        }
    }
    check_full(&store.db)?;
    if store.history_generation()?.is_some() {
        return Err(Error::Contract("history-owned family generation protected"));
    }
    if let Some(g) = partial {
        validate_partial_model(&store.db, g)?;
        store.integrity_verified = true;
        return Ok(store);
    }
    let model: bool = store.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='generation_a_meta')",
        [],
        |r| r.get(0),
    )?;
    if model {
        let (g, raw): (u64, Vec<u8>) = store.db.query_row(
            "SELECT generation,root FROM generation_a_meta WHERE singleton=1",
            [],
            |r| Ok((number(r, 0)?, r.get(1)?)),
        )?;
        let root = decode_path(&raw)?;
        return Ok(
            CertifiedGeneration::certify_store(store, GenerationId(g), &root, cancel)?.store,
        );
    }
    store.integrity_verified = true;
    Ok(store)
}

fn absent_destination(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(Error::Contract(
            "repository copy destination already exists",
        )),
    }
}

fn native_companion_path(legacy: &Path) -> Result<PathBuf> {
    if !legacy.is_absolute() {
        return Err(Error::Contract(
            "legacy origin must have an absolute native path",
        ));
    }
    let mut name = legacy
        .file_name()
        .ok_or(Error::Contract("legacy directory has no native name"))?
        .to_os_string();
    name.push("-generations");
    Ok(legacy.with_file_name(name))
}

fn owned_generation_name(name: &str, inventory: InventoryId) -> Result<(GenerationId, bool)> {
    if let Some(g) =
        parse_name(name, "candidate-", "").or_else(|| parse_name(name, "generation-", ""))
    {
        return Ok((GenerationId(g), false));
    }
    let stage = format!(".stage-a-{:032x}-", inventory.0);
    if let Some(g) = parse_name(name, &stage, "") {
        return Ok((GenerationId(g), true));
    }
    let quarantine = format!(".quarantine-a-{:032x}-", inventory.0);
    if let Some(tail) = name.strip_prefix(&quarantine)
        && tail.len() == 49
        && tail.as_bytes()[16] == b'-'
        && tail[17..].bytes().all(|b| b.is_ascii_hexdigit())
        && let Some(g) = parse_name(&tail[..16], "", "")
    {
        return Ok((GenerationId(g), true));
    }
    Err(Error::Contract(
        "foreign stage or quarantine name preserved",
    ))
}

// Ownership of an interrupted builder is weaker than a READY capability. Every
// object already present must be known, while an absent suffix and ready=0 are
// normal interruption states. No result is exposed from this certificate.
fn validate_partial_model(db: &Connection, generation: GenerationId) -> Result<()> {
    let mut statement = db.prepare("SELECT CASE WHEN length(type)<=32 THEN type ELSE NULL END,CASE WHEN length(name)<=128 THEN name ELSE NULL END,CASE WHEN length(tbl_name)<=128 THEN tbl_name ELSE NULL END,CASE WHEN length(sql)<=8192 THEN sql ELSE NULL END FROM sqlite_schema WHERE name GLOB 'generation_a_*' OR tbl_name GLOB 'generation_a_*' LIMIT 33")?;
    let mut rows = statement.query([])?;
    let mut count = 0;
    let mut meta = false;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > 32 {
            return Err(Error::Contract("partial model object admission exceeded"));
        }
        let (kind, name, table, definition): (String, String, String, Option<String>) =
            (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        if kind == "index"
            && definition.is_none()
            && table == "generation_a_pending"
            && name == "sqlite_autoindex_generation_a_pending_1"
        {
            continue;
        }
        let definition = definition.ok_or(Error::Contract("unknown partial model object"))?;
        if !SQL
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .any(|expected| expected == definition.trim().trim_end_matches(';'))
        {
            return Err(Error::Contract(
                "unknown partial model definition preserved",
            ));
        }
        meta |= name == "generation_a_meta" && kind == "table";
    }
    drop(rows);
    drop(statement);
    if meta {
        type ModelHeader = (i64, Vec<u8>, Vec<u8>, String, String, i64, i64, i64);
        let header: Option<ModelHeader> = db.query_row("SELECT version,CASE WHEN length(generation)=8 THEN generation ELSE NULL END,CASE WHEN length(root)<=65538 THEN root ELSE NULL END,CASE WHEN length(platform)<=128 THEN platform ELSE NULL END,CASE WHEN length(predicate)<=128 THEN predicate ELSE NULL END,rows,oversized,ready FROM generation_a_meta WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
        if let Some((version, g, root, platform, predicate, rows, oversized, ready)) = header
            && (version != VERSION
                || g != unsigned(generation.0)
                || !decode_path(&root)?.is_absolute()
                || platform != PLATFORM
                || predicate != "rust-analysis-v1"
                || rows < 0
                || oversized < 0
                || !(0..=1).contains(&ready))
        {
            return Err(Error::Contract("partial model context/version preserved"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod owned_stage_tests {
    use super::*;
    use crate::inventory::tests::context_id;
    #[test]
    fn interrupted_stage_can_be_owned_without_becoming_ready_but_unknown_model_is_preserved() {
        let t = tempfile::tempdir().unwrap();
        let template = context_id(1);
        let mut repository = Repository::open(
            &t.path().join("repository"),
            template.inventory,
            template.source,
        )
        .unwrap();
        let context = repository.next_context(context_id(1)).unwrap();
        let stage = repository.directory().join(format!(
            ".stage-a-{:032x}-{:016x}",
            context.inventory.0, context.generation.0
        ));
        let mut store = Store::create(
            &stage,
            context.inventory,
            context.source,
            Durability::Retained,
        )
        .unwrap();
        store.create_generation(context).unwrap();
        store.record_acquisition(context, t.path(), 10).unwrap();
        store.db.execute_batch(SQL).unwrap();
        store
            .db
            .execute(
                "INSERT INTO generation_a_meta VALUES(1,?1,?2,?3,?4,'rust-analysis-v1',0,0,0)",
                params![
                    VERSION,
                    unsigned(context.generation.0),
                    queries::native_path(t.path()).encode(),
                    PLATFORM
                ],
            )
            .unwrap();
        drop(store);
        let lease = repository
            .validate_owned_group(&stage, &Default::default())
            .unwrap();
        assert!(
            CertifiedGeneration::certify_store(
                lease,
                context.generation,
                t.path(),
                &Default::default()
            )
            .is_err()
        );
        let store = Store::reopen(&stage, Durability::Retained).unwrap();
        store.db.execute_batch("DROP INDEX generation_a_kind; CREATE INDEX generation_a_kind ON generation_a_rows(id);").unwrap();
        drop(store);
        let before = std::fs::read(stage.join("inventory.db")).unwrap();
        assert!(
            repository
                .validate_owned_group(&stage, &Default::default())
                .is_err()
        );
        assert_eq!(std::fs::read(stage.join("inventory.db")).unwrap(), before);
    }
}

fn bounded_sequence<'de, D, T, const N: usize>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    struct Bounded<T, const N: usize>(std::marker::PhantomData<T>);
    impl<'de, T: serde::Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for Bounded<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "an array with at most {N} elements")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Vec<T>, A::Error> {
            let mut values = Vec::with_capacity(N);
            for _ in 0..N {
                match sequence.next_element()? {
                    Some(value) => values.push(value),
                    None => return Ok(values),
                }
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("bounded origin array exceeded"));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, N>(std::marker::PhantomData))
}
fn validate_origin_namespace(origin: &Origin) -> Result<()> {
    if origin.generations.len() > 3 || origin.anchors.len() > 3 || origin.directory.len() > 65538 {
        return Err(Error::Contract("legacy origin namespace budget"));
    }
    let mut ids = std::collections::BTreeSet::new();
    for g in &origin.generations {
        if !ids.insert(*g) {
            return Err(Error::Contract("duplicate legacy origin identity"));
        }
    }
    let mut anchored = std::collections::BTreeSet::new();
    for anchor in &origin.anchors {
        let c = decode_context(&anchor.context)?;
        if c.inventory.0.to_be_bytes() != origin.inventory
            || c.source.0.to_be_bytes() != origin.source
            || !ids.contains(&c.generation.0)
            || !anchored.insert(c.generation.0)
            || anchor.root.len() > 65538
            || !decode_path(&anchor.root)?.is_absolute()
        {
            return Err(Error::Contract("legacy origin anchor namespace mismatch"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod origin_bounds_tests {
    use super::*;
    #[test]
    fn origin_arrays_reject_fourth_identity_or_anchor_and_oversized_native_payload() {
        let native = queries::native_path(std::env::temp_dir().as_path()).encode();
        let origin = Origin {
            version: 1,
            directory: native.clone(),
            inventory: 1u128.to_be_bytes(),
            source: 2u128.to_be_bytes(),
            generations: vec![1],
            anchors: vec![],
        };
        let valid = serde_json::to_value(origin).unwrap();
        let c = crate::inventory::tests::context_id(1);
        let anchor = serde_json::to_value(Anchor {
            context: context(c),
            root: native,
            started: 0,
            measured: None,
        })
        .unwrap();
        for field in ["generations", "anchors", "directory"] {
            let mut invalid = valid.clone();
            invalid[field] = match field {
                "generations" => serde_json::json!([1, 2, 3, 4]),
                "anchors" => serde_json::json!([
                    anchor.clone(),
                    anchor.clone(),
                    anchor.clone(),
                    anchor.clone()
                ]),
                _ => serde_json::json!(vec![1u8; 65539]),
            };
            assert!(
                serde_json::from_value::<Origin>(invalid).is_err(),
                "{field}"
            );
        }
    }
}
