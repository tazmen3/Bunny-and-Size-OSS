//! Versioned SQLite snapshots of finished direct scans. SQLite is opened only
//! while saving/loading; the returned tree owns all data needed for consultation.
use crate::{
    codec::NativeName,
    direct_save::{ReplayBatch, SaveRequest, check_cancelled},
    direct_tree::{DirectSummary, DirectTree, Subtotal, spool},
    inventory::{
        DirectoryBatch, Error, Kind, Result, Store, folder_totals::FolderMeasure, lifecycle,
        snapshot_io::Staging,
    },
    scheduler::{CollectionOptions, native_name, session_proof},
};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sortsize_core::{
    CancellationToken,
    inventory_contract::*,
    resources::{Category, Governor, disk_limit, native_free_disk},
};
use std::path::{Path, PathBuf};

const VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x42534e50;
const META_LIMIT: usize = 1024 * 1024;

pub struct SavedSnapshot {
    pub path: PathBuf,
}
pub struct LoadedSnapshot {
    pub tree: DirectTree,
    pub metadata: SnapshotMetadata,
}
/// Compatibility data for explicit legacy Verify/Compare actions. Keep owner
/// alive until every repository/reader opened from directory has been dropped.
pub struct CanonicalReference {
    pub reader: (
        crate::inventory::generation_a::Repository,
        crate::inventory::generation_a::CertifiedGeneration,
    ),
    pub directory: PathBuf,
    pub owner: std::sync::Arc<SnapshotOwner>,
}
pub struct SnapshotOwner {
    temp: Option<tempfile::TempDir>,
    groups: Vec<PathBuf>,
}
impl SnapshotOwner {
    pub fn path(&self) -> &Path {
        self.temp.as_ref().expect("live snapshot owner").path()
    }
}
impl Drop for SnapshotOwner {
    fn drop(&mut self) {
        if self.temp.take().is_some_and(|temp| temp.close().is_ok()) {
            for group in &self.groups {
                Governor::global().forget_deleted_artifact_group(group);
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct SnapshotMetadata {
    pub root: PathBuf,
    pub context: AcquisitionContext,
    pub started: i64,
    pub measured: i64,
    pub options: CollectionOptions,
    pub summary: DirectSummary,
    /// Recorded acquisition evidence only; never grants access to the source.
    pub proof: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    root: Vec<u8>,
    inventory: u128,
    source: u128,
    generation: u64,
    scan: u64,
    policy: u64,
    mount: u64,
    run: u64,
    validation: u64,
    started: i64,
    measured: i64,
    allocation: bool,
    owners: bool,
    complete: bool,
    logical: Option<u64>,
    allocated: Option<u64>,
    unknown: u64,
    allocation_unknown: u64,
    allocation_lower_bound: bool,
    files: u64,
    directories: u64,
    observations: u64,
    entries: u64,
    errors: u64,
    denied: u64,
    incomplete: u64,
    detached: u64,
    duplicates: u64,
    proof: Option<Vec<u8>>,
}

fn native_path(path: &Path) -> NativeName {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        NativeName::Windows(path.as_os_str().encode_wide().collect())
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        NativeName::Posix(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(any(unix, windows)))]
    {
        NativeName::Opaque(path.as_os_str().as_encoded_bytes().to_vec())
    }
}
pub fn destination(history: &Path, root: &Path) -> PathBuf {
    history.join(format!(
        "auto-{:x}.sqlite",
        Sha256::digest(location_key(root).encode())
    ))
}
/// Normalize only equivalent admitted Windows namespace prefixes. Native names
/// remain exact (including case and unpaired UTF-16); no source I/O occurs.
fn location_key(root: &Path) -> NativeName {
    #[cfg(windows)]
    {
        use std::{
            os::windows::ffi::OsStrExt,
            path::{Component, Prefix},
        };
        let mut components = root.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return native_path(root);
        };
        let mut key = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                vec![u16::from(drive.to_ascii_uppercase()), 58]
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let mut key = vec![92, 92];
                key.extend(server.encode_wide());
                key.push(92);
                key.extend(share.encode_wide());
                key
            }
            _ => return native_path(root),
        };
        if components.next() != Some(Component::RootDir) {
            return native_path(root);
        }
        key.push(92);
        let mut first = true;
        for component in components {
            let Component::Normal(name) = component else {
                return native_path(root);
            };
            if !first {
                key.push(92);
            }
            first = false;
            key.extend(name.encode_wide());
        }
        NativeName::Windows(key)
    }
    #[cfg(not(windows))]
    {
        native_path(root)
    }
}
pub fn excluded_state_path(metadata: &SnapshotMetadata) -> Result<Option<PathBuf>> {
    metadata
        .proof
        .as_deref()
        .map_or(Ok(None), crate::scheduler::recorded_scope_path)
}
fn invalid() -> Error {
    Error::Contract("invalid direct snapshot")
}
impl Wire {
    fn new(input: &SaveRequest<'_>, summary: DirectSummary) -> Result<Self> {
        if summary.pending_directories != 0
            || summary.coverage == ScopeCoverage::Unknown
            || summary.coverage != input.coverage
        {
            return Err(invalid());
        }
        let c = input.context;
        let m = summary.measure;
        let proof = session_proof(input.guard);
        if input.guard.internal_state().is_some() && proof.is_none() {
            return Err(Error::Contract(
                "snapshot could not retain the excluded workspace proof",
            ));
        }
        Ok(Self {
            root: native_path(input.guard.root()).encode(),
            inventory: c.inventory.0,
            source: c.source.0,
            generation: c.generation.0,
            scan: c.scan.0,
            policy: c.policy.0,
            mount: c.mount.0,
            run: c.run.0,
            validation: c.validation.0,
            started: input.started,
            measured: input.measured,
            allocation: input.options.allocation,
            owners: input.options.owners,
            complete: summary.coverage == ScopeCoverage::Complete,
            logical: m.logical,
            allocated: m.allocated,
            unknown: m.unknown,
            allocation_unknown: m.allocation_unknown,
            allocation_lower_bound: m.allocation_lower_bound,
            files: m.files,
            directories: m.directories,
            observations: summary.observations,
            entries: summary.entries,
            errors: summary.errors,
            denied: summary.denied,
            incomplete: summary.incomplete_directories,
            detached: summary.detached_directories,
            duplicates: summary.duplicate_observations,
            proof,
        })
    }
    fn metadata(self) -> Result<SnapshotMetadata> {
        let root = PathBuf::from(native_name(
            &NativeName::decode(&self.root).map_err(|_| invalid())?,
        )?);
        if !root.is_absolute()
            || self.denied > self.errors
            || self.entries > self.observations
            || self
                .proof
                .as_ref()
                .is_some_and(|proof| proof.len() > 128 * 1024)
        {
            return Err(invalid());
        }
        if let Some(proof) = &self.proof {
            crate::scheduler::recorded_scope_path(proof)?;
        }
        Ok(SnapshotMetadata {
            root,
            context: AcquisitionContext {
                inventory: InventoryId(self.inventory),
                source: SourceId(self.source),
                generation: GenerationId(self.generation),
                scan: ScanId(self.scan),
                policy: PolicyId(self.policy),
                mount: MountEpoch(self.mount),
                run: RunEpoch(self.run),
                validation: ValidationEpoch(self.validation),
            },
            started: self.started,
            measured: self.measured,
            options: CollectionOptions {
                allocation: self.allocation,
                owners: self.owners,
            },
            summary: DirectSummary {
                measure: FolderMeasure {
                    logical: self.logical,
                    allocated: self.allocated,
                    unknown: self.unknown,
                    allocation_unknown: self.allocation_unknown,
                    allocation_lower_bound: self.allocation_lower_bound,
                    files: self.files,
                    directories: self.directories,
                },
                coverage: if self.complete {
                    ScopeCoverage::Complete
                } else {
                    ScopeCoverage::Partial
                },
                observations: self.observations,
                entries: self.entries,
                errors: self.errors,
                denied: self.denied,
                pending_directories: 0,
                incomplete_directories: self.incomplete,
                detached_directories: self.detached,
                duplicate_observations: self.duplicates,
                temporary_bytes: 0,
            },
            proof: self.proof,
        })
    }
}

fn checked_file(path: &Path) -> Result<()> {
    lifecycle::check_directory(path.parent().ok_or_else(invalid)?)?;
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_file() || m.is_symlink() {
        return Err(invalid());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Err(invalid());
        }
    }
    Ok(())
}
fn open_read(path: &Path) -> Result<Connection> {
    checked_file(path)?;
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
        META_LIMIT as i32,
    )?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; PRAGMA mmap_size=0; PRAGMA cache_size=-1024;")?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if version != VERSION || app != APPLICATION_ID {
        return Err(invalid());
    }
    Ok(db)
}
fn metadata_bytes(db: &Connection) -> Result<Vec<u8>> {
    let bytes: Vec<u8> = db.query_row("SELECT data FROM metadata WHERE id=1", [], |r| r.get(0))?;
    if bytes.len() > META_LIMIT {
        return Err(invalid());
    }
    Ok(bytes)
}
pub fn inspect(path: &Path) -> Result<SnapshotMetadata> {
    let _memory = Governor::global()
        .reserve(Category::Sqlite, 2 * 1024 * 1024, None)
        .ok_or(Error::Contract("snapshot reader memory budget"))?;
    let db = open_read(path)?;
    serde_json::from_slice::<Wire>(&metadata_bytes(&db)?)
        .map_err(|_| invalid())?
        .metadata()
}

fn digest_batch(hash: &mut Sha256, sequence: u64, bytes: &[u8]) {
    hash.update(sequence.to_le_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
fn verify(
    db: &Connection,
    cancel: &CancellationToken,
    mut batch: impl FnMut(ReplayBatch) -> Result<()>,
) -> Result<SnapshotMetadata> {
    if db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))? != "ok" {
        return Err(invalid());
    }
    let meta = metadata_bytes(db)?;
    let metadata = serde_json::from_slice::<Wire>(&meta)
        .map_err(|_| invalid())?
        .metadata()?;
    let mut hash = Sha256::new();
    hash.update(&meta);
    let mut query = db.prepare("SELECT sequence,data FROM batches ORDER BY sequence")?;
    let mut rows = query.query([])?;
    let mut next = 0u64;
    while let Some(row) = rows.next()? {
        check_cancelled(cancel)?;
        let sequence: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        if sequence < 0 || sequence as u64 != next || bytes.len() > spool::MAX_BATCH {
            return Err(invalid());
        }
        digest_batch(&mut hash, next, &bytes);
        let retained = spool::batch(&bytes)?;
        validate_batch(metadata.context, &retained)?;
        batch(retained)?;
        next += 1;
    }
    let (count, checksum): (i64, Vec<u8>) =
        db.query_row("SELECT batches,checksum FROM seal WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if count < 0 || count as u64 != next || checksum.as_slice() != hash.finalize().as_slice() {
        return Err(invalid());
    }
    Ok(metadata)
}

/// Only complete traversal sessions are accepted. A finished partial traversal
/// is retained with its diagnostics; cancellation never replaces a prior scan.
pub fn save(
    input: SaveRequest<'_>,
    summary: DirectSummary,
    batches: impl IntoIterator<Item = Result<ReplayBatch>>,
    cancel: &CancellationToken,
) -> Result<SavedSnapshot> {
    check_cancelled(cancel)?;
    let parent = input.destination.parent().ok_or_else(invalid)?;
    if !input.guard.work_path_allowed(parent)? {
        return Err(Error::Contract("snapshot placement overlaps source"));
    }
    lifecycle::ensure_root(parent)?;
    if input.destination != destination(parent, input.guard.root()) {
        return Err(Error::Contract(
            "snapshot destination is not the automatic inventory",
        ));
    }
    if input.destination.exists() {
        checked_file(input.destination)?;
    }
    let _memory = Governor::global()
        .reserve(Category::Sqlite, 8 * 1024 * 1024, None)
        .ok_or(Error::Contract("snapshot writer memory budget"))?;
    let _transport = Governor::global()
        .reserve(Category::Transport, 1024 * 1024, None)
        .ok_or(Error::Contract("snapshot transport memory budget"))?;
    let wire = Wire::new(&input, summary)?;
    let meta = serde_json::to_vec(&wire).map_err(|_| invalid())?;
    let limit = native_free_disk(parent)
        .map(disk_limit)
        .ok_or(Error::Contract("snapshot free space unavailable"))?;
    let mut stage = Staging::new(parent, limit)?;
    let mut db = stage.open()?;
    let progress_cancel = cancel.clone();
    db.progress_handler(4096, Some(move || progress_cancel.is_cancelled()))?;
    db.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-4096; PRAGMA temp_store=MEMORY;")?;
    db.pragma_update(
        None,
        "max_page_count",
        (limit / 4096).min(i64::MAX as u64) as i64,
    )?;
    db.pragma_update(None, "application_id", APPLICATION_ID)?;
    db.pragma_update(None, "user_version", VERSION)?;
    let tx = db.transaction()?;
    tx.execute_batch("CREATE TABLE metadata(id INTEGER PRIMARY KEY CHECK(id=1),data BLOB NOT NULL); CREATE TABLE batches(sequence INTEGER PRIMARY KEY,data BLOB NOT NULL); CREATE TABLE seal(id INTEGER PRIMARY KEY CHECK(id=1),batches INTEGER NOT NULL,checksum BLOB NOT NULL); CREATE TABLE pending(id INTEGER PRIMARY KEY,sequence INTEGER NOT NULL); INSERT INTO pending VALUES(0,0);")?;
    tx.execute("INSERT INTO metadata VALUES(1,?1)", [&meta])?;
    let mut hash = Sha256::new();
    hash.update(&meta);
    let mut next_id = 1u64;
    let mut sequence = 0u64;
    let mut subtotal = Subtotal::default();
    let mut incomplete = 0u64;
    {
        let mut insert = tx.prepare("INSERT INTO batches VALUES(?1,?2)")?;
        let mut directory_sequence = tx.prepare("SELECT sequence FROM pending WHERE id=?1")?;
        let mut directory_insert = tx.prepare("INSERT INTO pending VALUES(?1,0)")?;
        let mut directory_end = tx.prepare("DELETE FROM pending WHERE id=?1")?;
        let mut directory_next =
            tx.prepare("UPDATE pending SET sequence=sequence+1 WHERE id=?1")?;
        for retained in batches {
            check_cancelled(cancel)?;
            let retained = retained?;
            validate_batch(input.context, &retained)?;
            if directory_sequence.query_row([retained.parent.0 as i64], |r| r.get::<_, i64>(0))?
                as u64
                != retained.sequence
                || retained.observations.len() != retained.evidence.len()
                || retained.observations.len() != retained.properties.len()
                || retained.observations.len() > 512
            {
                return Err(invalid());
            }
            if retained.end.is_some() {
                directory_end.execute([retained.parent.0 as i64])?;
            } else {
                directory_next.execute([retained.parent.0 as i64])?;
            }
            incomplete += u64::from(retained.end == Some(ListingEnd::Incomplete));
            let mut encoded = spool::Encoder(Vec::new());
            encoded.header(
                retained.parent,
                retained.sequence,
                retained.end,
                retained.directory.as_ref(),
                retained.observations.len(),
            )?;
            let mut children = Vec::new();
            for ((row, proof), properties) in retained
                .observations
                .iter()
                .zip(&retained.evidence)
                .zip(&retained.properties)
            {
                subtotal.observe(row, properties);
                if row.kind == Kind::Directory {
                    directory_insert.execute([next_id as i64])?;
                    children.push((EntryId(next_id), row.name.clone()));
                }
                encoded.record(
                    u32::try_from(next_id).map_err(|_| invalid())?,
                    &row.name.encode(),
                    row,
                    proof.as_ref(),
                    properties,
                )?;
                next_id = next_id.checked_add(1).ok_or_else(invalid)?;
            }
            if retained.children != children || encoded.0.len() > spool::MAX_BATCH {
                return Err(invalid());
            }
            digest_batch(&mut hash, sequence, &encoded.0);
            insert.execute(params![
                i64::try_from(sequence).map_err(|_| invalid())?,
                encoded.0
            ])?;
            sequence += 1;
        }
    }
    let pending: i64 = tx.query_row("SELECT count(*) FROM pending", [], |r| r.get(0))?;
    let measure = subtotal.measure();
    let coverage = if incomplete == 0 && measure.unknown == 0 && measure.logical.is_some() {
        ScopeCoverage::Complete
    } else {
        ScopeCoverage::Partial
    };
    if pending != 0
        || next_id - 1 != summary.entries
        || measure != summary.measure
        || incomplete != summary.incomplete_directories
        || coverage != summary.coverage
    {
        return Err(invalid());
    }
    tx.execute_batch("DROP TABLE pending;")?;
    tx.execute(
        "INSERT INTO seal VALUES(1,?1,?2)",
        params![sequence as i64, hash.finalize().as_slice()],
    )?;
    check_cancelled(cancel)?;
    tx.commit()?;
    verify(&db, cancel, |_| Ok(()))?;
    db.close().map_err(|(_, error)| Error::Sql(error))?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(stage.path())?
        .sync_all()?;
    stage.close();
    check_cancelled(cancel)?;
    if !input.guard.work_path_allowed(parent)? {
        return Err(Error::Contract("snapshot placement changed"));
    }
    let free = native_free_disk(parent).unwrap_or(0);
    let bytes = std::fs::metadata(stage.path())?.len();
    check_cancelled(cancel)?;
    publish(stage.path(), input.destination)?;
    Governor::global().observe_artifact_group(input.destination, free, bytes);
    Ok(SavedSnapshot {
        path: input.destination.to_owned(),
    })
}

fn validate_batch(context: AcquisitionContext, retained: &ReplayBatch) -> Result<()> {
    if (retained.end == Some(ListingEnd::Exhausted)) != retained.directory.is_some() {
        return Err(Error::Contract("snapshot stable EOF evidence mismatch"));
    }
    for row in &retained.observations {
        crate::direct_tree::native_component(&row.name)?;
    }
    let header = BatchHeader {
        attempt: AttemptKey {
            task: TaskKey {
                context,
                directory: retained.parent,
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(1),
            owner: OwnerEpoch(1),
            branch: BranchRevision(1),
        },
        sequence: retained.sequence,
        end: retained.end,
    };
    Store::validate_directory_batch(
        &DirectoryBatch {
            header,
            observations: &retained.observations,
            evidence: &retained.evidence,
            properties: &retained.properties,
            directory: retained.directory.as_ref(),
        },
        true,
        true,
    )
}

fn publish(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_NORMAL, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            SetFileAttributesW,
        };
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // Both paths are private application state; no scanned entry is mutated.
        if unsafe { SetFileAttributesW(from.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0
            || unsafe {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(temporary, destination)?;
    }
    lifecycle::sync_directory(destination.parent().ok_or_else(invalid)?)
}

pub fn load(
    path: &Path,
    scratch: &Path,
    disk_limit: u64,
    cancel: &CancellationToken,
) -> Result<LoadedSnapshot> {
    check_cancelled(cancel)?;
    let _memory = Governor::global()
        .reserve(Category::Sqlite, 2 * 1024 * 1024, None)
        .ok_or(Error::Contract("snapshot reader memory budget"))?;
    let mut db = open_read(path)?;
    let progress_cancel = cancel.clone();
    db.progress_handler(4096, Some(move || progress_cancel.is_cancelled()))?;
    let transaction = db.transaction()?;
    let metadata = serde_json::from_slice::<Wire>(&metadata_bytes(&transaction)?)
        .map_err(|_| invalid())?
        .metadata()?;
    let mut tree = DirectTree::new(metadata.root.clone(), scratch, disk_limit)?;
    verify(&transaction, cancel, |batch| tree.accept_replay(batch))?;
    check_cancelled(cancel)?;
    tree.finish()?;
    tree.restore_diagnostics(metadata.summary)?;
    Ok(LoadedSnapshot { tree, metadata })
}

/// Materialize a legacy canonical reference only for an explicitly requested
/// compatibility operation. No source Guard is opened: retained proof remains
/// evidence about the old acquisition, never authorization for a new one.
pub fn canonical_reference(
    path: &Path,
    scratch: &Path,
    cancel: &CancellationToken,
) -> Result<CanonicalReference> {
    use crate::direct_save::{RecordedSaveRequest, save_recorded};
    use sortsize_core::acquisition::{Attachment, Guard, contains_path, local_state_location};
    check_cancelled(cancel)?;
    lifecycle::check_directory(scratch)?;
    let mut owner = SnapshotOwner {
        temp: Some(
            tempfile::Builder::new()
                .prefix("snapshot-reference-")
                .tempdir_in(scratch)?,
        ),
        groups: Vec::new(),
    };
    let private = owner.path().join("private");
    lifecycle::private_directory(&private)?;
    let private = std::fs::canonicalize(private)?;
    let guard = Guard::open(&private, Default::default())?;
    let limit = native_free_disk(&private)
        .map(disk_limit)
        .ok_or(Error::Contract("snapshot reference free space unavailable"))?;
    let LoadedSnapshot { tree, metadata } = load(path, &private, limit, cancel)?;
    let destination = private.join("inv-snapshot-reference");
    owner
        .groups
        .push(destination.join(format!("generation-{:016x}", metadata.context.generation.0)));
    owner.groups.push(destination.clone());
    let saved = save_recorded(
        RecordedSaveRequest {
            context: metadata.context,
            root: &metadata.root,
            scratch: &private,
            destination: &destination,
            started: metadata.started,
            measured: metadata.measured,
            options: metadata.options,
            coverage: metadata.summary.coverage,
            proof: metadata.proof.as_deref(),
        },
        tree.replay()?,
        cancel,
        |candidate| {
            local_state_location(candidate)?;
            let current = Guard::open(&private, Default::default())?;
            let (boundary, stamp) = guard.evidence();
            let (new_boundary, new_stamp) = current.evidence();
            Ok(contains_path(&private, candidate)
                && boundary == new_boundary
                && stamp.attachment(new_stamp) == Attachment::Compatible)
        },
    )?;
    Ok(CanonicalReference {
        reader: saved.reader?,
        directory: destination,
        owner: std::sync::Arc::new(owner),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Observation, differential::NativeEvidence};
    use sortsize_core::{
        MetadataValue, ValueAccuracy,
        acquisition::{Guard, NodeProperties},
    };

    #[cfg(windows)]
    #[test]
    fn automatic_location_key_unifies_native_prefixes_without_folding_names() {
        let history = Path::new(r"C:\private\snapshots");
        assert_eq!(
            destination(history, Path::new("C:/Folder/é")),
            destination(history, Path::new(r"\\?\C:\Folder\é"))
        );
        assert_eq!(
            destination(history, Path::new("c:/")),
            destination(history, Path::new(r"\\?\C:\"))
        );
        assert_eq!(
            destination(history, Path::new(r"\\server\share\Folder")),
            destination(history, Path::new(r"\\?\UNC\server\share\Folder"))
        );
        assert_ne!(
            destination(history, Path::new(r"C:\Folder\name")),
            destination(history, Path::new(r"C:\folder\name"))
        );
        assert_ne!(
            destination(history, Path::new(r"C:Folder")),
            destination(history, Path::new(r"C:\Folder"))
        );
    }

    fn context() -> AcquisitionContext {
        AcquisitionContext {
            inventory: InventoryId(301),
            source: SourceId(302),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(2),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        }
    }
    fn tree(guard: &Guard, scratch: &Path, partial: bool) -> DirectTree {
        let mut tree = DirectTree::new(guard.root().to_owned(), scratch, 32 * 1024 * 1024).unwrap();
        let name = native_path(Path::new("échantillon-🐇.txt"));
        tree.accept_replay(ReplayBatch {
            parent: EntryId(0),
            sequence: 0,
            end: Some(if partial {
                ListingEnd::Incomplete
            } else {
                ListingEnd::Exhausted
            }),
            observations: vec![Observation {
                name,
                kind: Kind::File,
                logical: Some(12345),
                allocated: Some(4096),
                modified: Some((-4, 12)),
                metadata_known: true,
            }],
            evidence: vec![Some(NativeEvidence::from_stamp(guard.evidence().1))],
            properties: vec![NodeProperties {
                allocated: MetadataValue::Known {
                    value: 4096,
                    accuracy: ValueAccuracy::LowerBound,
                },
                ..Default::default()
            }],
            directory: if partial {
                None
            } else {
                Some(guard.evidence().1.clone())
            },
            children: Vec::new(),
        })
        .unwrap();
        tree.finish().unwrap();
        tree
    }
    fn request<'a>(
        guard: &'a Guard,
        scratch: &'a Path,
        destination: &'a Path,
        summary: DirectSummary,
    ) -> SaveRequest<'a> {
        SaveRequest {
            context: context(),
            guard,
            scratch,
            destination,
            started: 10,
            measured: 20,
            options: CollectionOptions::default(),
            coverage: summary.coverage,
        }
    }

    #[test]
    fn snapshot_roundtrip_without_source_and_atomic_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        lifecycle::private_directory(&source).unwrap();
        let history = temp.path().join("snapshots");
        lifecycle::private_directory(&history).unwrap();
        let guard = Guard::open(&source, Default::default()).unwrap();
        let direct = tree(&guard, temp.path(), false);
        let summary = direct.summary();
        let path = destination(&history, &source);
        std::fs::remove_dir(&source).unwrap(); // No source access to save or reload.
        save(
            request(&guard, temp.path(), &path, summary),
            summary,
            direct.replay().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let first = std::fs::read(&path).unwrap();
        let loaded = load(&path, temp.path(), 32 * 1024 * 1024, &Default::default()).unwrap();
        assert_eq!(loaded.metadata.context, context());
        assert_eq!(loaded.tree.summary().measure, summary.measure);
        let row = loaded.tree.row(EntryId(1)).unwrap();
        assert_eq!(row.path, source.join("échantillon-🐇.txt"));
        assert_eq!(row.modified, Some((-4, 12)));
        assert_eq!(
            row.properties.allocated,
            MetadataValue::Known {
                value: 4096,
                accuracy: ValueAccuracy::LowerBound
            }
        );
        assert!(
            loaded
                .tree
                .replay()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .evidence[0]
                .is_some()
        );
        let disk_before = Governor::global().usage().disk_used;
        let canonical = canonical_reference(&path, &history, &Default::default()).unwrap();
        assert_eq!(
            canonical
                .reader
                .1
                .as_store()
                .entry_count(context().generation)
                .unwrap(),
            2
        );
        assert_eq!(
            canonical
                .reader
                .1
                .as_store()
                .recorded_recovery_guard(context().generation)
                .unwrap(),
            loaded.metadata.proof
        );
        let owned = canonical.owner.path().to_owned();
        drop(canonical);
        assert!(!owned.exists());
        assert_eq!(Governor::global().usage().disk_used, disk_before);
        let cancel = CancellationToken::default();
        let mut batches = direct.replay().unwrap();
        let interrupted = std::iter::from_fn(|| {
            let next = batches.next();
            if next.is_none() {
                cancel.cancel();
            }
            next
        });
        assert!(
            save(
                request(&guard, temp.path(), &path, summary),
                summary,
                interrupted,
                &cancel
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert!(
            save(
                request(&guard, temp.path(), &path, summary),
                summary,
                [Err(Error::Contract("simulated input failure"))],
                &Default::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), first);
        let mut wrong = summary;
        wrong.measure.logical = Some(999);
        assert!(
            save(
                request(&guard, temp.path(), &path, wrong),
                wrong,
                direct.replay().unwrap(),
                &Default::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), first);
        let explicit = history.join("user-explicit.sqlite");
        std::fs::write(&explicit, b"previous explicit inventory").unwrap();
        assert!(
            save(
                request(&guard, temp.path(), &explicit, summary),
                summary,
                direct.replay().unwrap(),
                &Default::default()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(&explicit).unwrap(),
            b"previous explicit inventory"
        );
        save(
            request(&guard, temp.path(), &path, summary),
            summary,
            direct.replay().unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(&history).unwrap().count(), 2);
    }

    #[test]
    fn partial_diagnostics_and_corruption_are_not_silently_lost() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        lifecycle::private_directory(&source).unwrap();
        let history = temp.path().join("snapshots");
        lifecycle::private_directory(&history).unwrap();
        let guard = Guard::open(&source, Default::default()).unwrap();
        let direct = tree(&guard, temp.path(), true);
        let mut summary = direct.summary();
        summary.errors = 2;
        summary.denied = 1;
        summary.detached_directories = 1;
        let path = destination(&history, &source);
        save(
            request(&guard, temp.path(), &path, summary),
            summary,
            direct.replay().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let loaded = load(&path, temp.path(), 32 * 1024 * 1024, &Default::default()).unwrap();
        assert_eq!(loaded.tree.summary().coverage, ScopeCoverage::Partial);
        assert_eq!(loaded.tree.summary().errors, 2);
        assert_eq!(loaded.tree.summary().denied, 1);
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE seal SET checksum=zeroblob(32)", [])
            .unwrap();
        drop(db);
        assert!(load(&path, temp.path(), 32 * 1024 * 1024, &Default::default()).is_err());
    }

    #[test]
    fn recorded_scope_survives_offline_reload_and_invalid_components_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        lifecycle::private_directory(&source).unwrap();
        let scope = source.join("excluded-original");
        lifecycle::private_directory(&scope).unwrap();
        let history = temp.path().join("snapshots");
        lifecycle::private_directory(&history).unwrap();
        let guard = Guard::open(&source, Default::default())
            .unwrap()
            .with_internal_state(Guard::open(&scope, Default::default()).unwrap())
            .unwrap();
        let direct = tree(&guard, temp.path(), false);
        let summary = direct.summary();
        let path = destination(&history, &source);
        save(
            request(&guard, temp.path(), &path, summary),
            summary,
            direct.replay().unwrap(),
            &Default::default(),
        )
        .unwrap();
        std::fs::remove_dir(&scope).unwrap();
        std::fs::remove_dir(&source).unwrap();
        let loaded = load(&path, temp.path(), 32 * 1024 * 1024, &Default::default()).unwrap();
        assert_eq!(excluded_state_path(&loaded.metadata).unwrap(), Some(scope));
        // An internally consistent checksum is not authority to accept unsafe
        // path components from a supplied snapshot.
        let adversarial = history.join("adversarial.sqlite");
        std::fs::copy(&path, &adversarial).unwrap();
        let db = Connection::open(&adversarial).unwrap();
        let bytes: Vec<u8> = db
            .query_row("SELECT data FROM batches WHERE sequence=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        let mut batch = spool::batch(&bytes).unwrap();
        #[cfg(windows)]
        {
            batch.observations[0].name = NativeName::Windows("C:relative".encode_utf16().collect());
        }
        #[cfg(not(windows))]
        {
            batch.observations[0].name = NativeName::Posix(b"../relative".to_vec());
        }
        let mut encoded = spool::Encoder(Vec::new());
        encoded
            .header(
                batch.parent,
                batch.sequence,
                batch.end,
                batch.directory.as_ref(),
                1,
            )
            .unwrap();
        encoded
            .record(
                1,
                &batch.observations[0].name.encode(),
                &batch.observations[0],
                batch.evidence[0].as_ref(),
                &batch.properties[0],
            )
            .unwrap();
        db.execute("UPDATE batches SET data=?1 WHERE sequence=0", [&encoded.0])
            .unwrap();
        let mut hash = Sha256::new();
        hash.update(metadata_bytes(&db).unwrap());
        digest_batch(&mut hash, 0, &encoded.0);
        db.execute("UPDATE seal SET checksum=?1", [hash.finalize().as_slice()])
            .unwrap();
        drop(db);
        assert!(
            load(
                &adversarial,
                temp.path(),
                32 * 1024 * 1024,
                &Default::default()
            )
            .is_err()
        );
        assert!(load(&path, temp.path(), 32 * 1024 * 1024, &Default::default()).is_ok());
    }
}
