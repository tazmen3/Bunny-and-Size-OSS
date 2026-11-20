//! One admission for every file of a query, including SQLite's rollback journal.
//! Charges precede growth. Failed writes retain their charge until deletion.
use super::*;
use sortsize_core::resources::{ArtifactGuard, DiskReservation, Governor};
use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

pub(crate) mod adaptive;
pub(in crate::inventory) mod vfs;
use super::duplicates::DuplicateResourceCause;
use adaptive::{AdaptiveSpace, ArtifactIdentity, SpacePolicy};

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct TemporaryUsage {
    pub limit: u64,
    pub charged: u64,
    pub peak: u64,
}
struct Ledger {
    files: BTreeMap<PathBuf, Charge>,
    usage: TemporaryUsage,
}
#[derive(Default)]
struct Charge {
    bytes: u64,
    open_vfs_handles: u32,
    temporary: bool,
    owned: Option<ArtifactIdentity>,
}
enum Directory {
    Temporary(tempfile::TempDir),
    Persistent(PathBuf),
    Derived(PathBuf),
    Adaptive(Box<AdaptiveSpace>),
}
impl Directory {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(directory) => directory.path(),
            Self::Persistent(path) | Self::Derived(path) => path,
            Self::Adaptive(space) => &space.path,
        }
    }
}
pub(crate) struct WorkspaceGrowth {
    _admission: Option<MutexGuard<'static, ()>>,
}
pub(crate) struct Workspace {
    directory: Directory,
    ledger: Mutex<Ledger>,
    unit: u64,
    pub(super) mapping_unit: u64,
    #[cfg(test)]
    fault: Mutex<Option<&'static str>>,
    #[cfg(test)]
    transients_created: std::sync::atomic::AtomicU64,
    disk: Mutex<Option<DiskReservation>>,
    guard: Option<ArtifactGuard>,
}
impl Workspace {
    pub fn new(store: &Store, scratch: &Path, limit: u64) -> Result<Arc<Self>> {
        let unit = sortsize_core::resources::native_allocation_unit(scratch)
            .filter(|n| n.is_power_of_two() && *n <= 64 * 1024)
            .ok_or(Error::Contract("unsupported temporary allocation geometry"))?
            .max(4096);
        let mapping_unit = sortsize_core::resources::native_mapping_page_size()
            .filter(|n| n.is_power_of_two() && *n <= 64 * 1024)
            .ok_or(Error::Contract("unsupported temporary mapping geometry"))?
            .max(32 * 1024);
        let disk = store.disk_admission_at(limit, scratch)?;
        let directory = tempfile::Builder::new()
            .prefix("query-")
            .tempdir_in(scratch.canonicalize()?)?;
        // Private scratch is an existing API precondition; TempDir further
        // isolates this disposable job. Canonical owners use persistent().
        Ok(Arc::new(Self {
            directory: Directory::Temporary(directory),
            unit,
            mapping_unit,
            #[cfg(test)]
            fault: Mutex::new(None),
            #[cfg(test)]
            transients_created: std::sync::atomic::AtomicU64::new(0),
            ledger: Mutex::new(Ledger {
                files: BTreeMap::new(),
                usage: TemporaryUsage {
                    limit,
                    charged: 0,
                    peak: 0,
                },
            }),
            disk: Mutex::new(Some(disk)),
            guard: None,
        }))
    }
    /// Derived connections live outside the canonical authority and own one
    /// reservation for their entire lifetime, including all SQLite auxiliaries.
    pub(in crate::inventory) fn derived(
        store: &Store,
        scratch: &Path,
        limit: u64,
    ) -> Result<Arc<Self>> {
        Self::private_derived(scratch, limit, Some(store))
    }
    pub(super) fn current_folder(scratch: &Path, limit: u64) -> Result<Arc<Self>> {
        Self::private_derived(scratch, limit, None)
    }
    pub(super) fn current_folder_adaptive(
        scratch: &Path,
        free: u64,
        policy: SpacePolicy,
    ) -> Result<Arc<Self>> {
        let (unit, mapping_unit) = Self::derived_geometry(scratch)?;
        let directory = AdaptiveSpace::create(scratch, free, policy, |parent| {
            Self::create_derived_directory(parent, None)
        })?;
        Ok(Self::derived_workspace(
            Directory::Adaptive(Box::new(directory)),
            policy.limit,
            unit,
            mapping_unit,
            None,
        ))
    }
    fn private_derived(scratch: &Path, limit: u64, store: Option<&Store>) -> Result<Arc<Self>> {
        let scratch = scratch.canonicalize()?;
        sortsize_core::acquisition::local_state_location(&scratch)?;
        if store.is_some_and(|store| {
            sortsize_core::acquisition::contains_path(store.directory(), &scratch)
        }) {
            return Err(Error::Contract(
                "derived scratch is inside canonical authority",
            ));
        }
        let (unit, mapping_unit) = Self::derived_geometry(&scratch)?;
        let disk = if let Some(store) = store {
            store.disk_admission_at(limit, &scratch)?
        } else {
            let free = sortsize_core::resources::native_free_disk(&scratch)
                .ok_or(Error::Contract("duplicate scratch free space unavailable"))?;
            let governor = Governor::global();
            governor.observe_disk(free, 0);
            governor
                .reserve_disk_at(&scratch, limit)
                .ok_or(Error::DuplicateTemporaryLimit { limit })?
        };
        let directory = Self::create_derived_directory(&scratch, store)?;
        Ok(Self::derived_workspace(
            Directory::Derived(directory),
            limit,
            unit,
            mapping_unit,
            Some(disk),
        ))
    }
    fn derived_geometry(scratch: &Path) -> Result<(u64, u64)> {
        let unit = sortsize_core::resources::native_allocation_unit(scratch)
            .filter(|n| n.is_power_of_two() && *n <= 64 * 1024)
            .ok_or(Error::Contract("unsupported derived allocation geometry"))?
            .max(4096);
        let mapping_unit = sortsize_core::resources::native_mapping_page_size()
            .filter(|n| n.is_power_of_two() && *n <= 64 * 1024)
            .ok_or(Error::Contract("unsupported derived mapping geometry"))?
            .max(32 * 1024);
        Ok((unit, mapping_unit))
    }
    fn create_derived_directory(scratch: &Path, store: Option<&Store>) -> Result<PathBuf> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..32 {
            let nonce = NEXT
                .try_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |n| n.checked_add(1),
                )
                .map_err(|_| Error::Contract("derived scratch identity exhausted"))?;
            let clock = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::Contract("derived scratch clock unavailable"))?
                .as_nanos();
            let path = scratch.join(format!(
                ".bunny-derived-{:08x}-{clock:032x}-{nonce:016x}",
                std::process::id()
            ));
            if store.is_some_and(|store| {
                sortsize_core::acquisition::contains_path(store.directory(), &path)
            }) {
                return Err(Error::Contract("derived scratch authority collision"));
            }
            match super::super::lifecycle::private_directory(&path) {
                Ok(()) => return Ok(path),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::Contract("derived scratch collision budget"))
    }
    fn derived_workspace(
        directory: Directory,
        limit: u64,
        unit: u64,
        mapping_unit: u64,
        disk: Option<DiskReservation>,
    ) -> Arc<Self> {
        Arc::new(Self {
            directory,
            unit,
            mapping_unit,
            ledger: Mutex::new(Ledger {
                files: BTreeMap::new(),
                usage: TemporaryUsage {
                    limit,
                    charged: 0,
                    peak: 0,
                },
            }),
            disk: Mutex::new(disk),
            guard: None,
            #[cfg(test)]
            fault: Mutex::new(None),
            #[cfg(test)]
            transients_created: std::sync::atomic::AtomicU64::new(0),
        })
    }
    pub(in crate::inventory) fn persistent(
        directory: &Path,
        limit: u64,
        prospective: Option<DiskReservation>,
    ) -> Result<Arc<Self>> {
        let directory = directory.canonicalize()?;
        let unit = sortsize_core::resources::native_allocation_unit(&directory)
            .filter(|n| n.is_power_of_two() && *n <= 64 * 1024)
            .ok_or(Error::Contract("unsupported canonical allocation geometry"))?
            .max(4096);
        let mapping_unit = sortsize_core::resources::native_mapping_page_size()
            .filter(|n| n.is_power_of_two() && *n <= 64 * 1024)
            .ok_or(Error::Contract("unsupported canonical mapping geometry"))?
            .max(32 * 1024);
        let free = sortsize_core::resources::native_free_disk(&directory)
            .ok_or(Error::Contract("canonical free space unavailable"))?;
        let guard = Governor::global()
            .guard_artifact_group(&directory, free, 0)
            .ok_or(Error::Contract("canonical artifact ledger admission"))?;
        let space = Arc::new(Self {
            directory: Directory::Persistent(directory.clone()),
            unit,
            mapping_unit,
            ledger: Mutex::new(Ledger {
                files: BTreeMap::new(),
                usage: TemporaryUsage {
                    limit,
                    charged: 0,
                    peak: 0,
                },
            }),
            disk: Mutex::new(prospective),
            guard: Some(guard),
            #[cfg(test)]
            fault: Mutex::new(None),
            #[cfg(test)]
            transients_created: std::sync::atomic::AtomicU64::new(0),
        });
        for (index, item) in std::fs::read_dir(&directory)?.enumerate() {
            if index >= 1024 {
                return Err(Error::Contract("canonical artifact count budget"));
            }
            let item = item?;
            let name = item.file_name();
            let path = item.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.is_symlink() {
                return Err(Error::Contract("unsafe canonical artifact"));
            }
            if name == "owner.lock" {
                if metadata.len() != 0 {
                    return Err(Error::Contract("canonical lock file must be empty"));
                }
                continue;
            }
            if super::super::generation_a::canonical::is_temporary_name(&name) {
                space.register_transient(&path)?;
            }
            if !space.charge(&path, metadata.len()) {
                return Err(Error::Contract(
                    "existing canonical artifacts exceed admitted quota",
                ));
            }
        }
        Ok(space)
    }
    #[cfg(test)]
    pub fn inject(&self, fault: &'static str) {
        *self.fault.lock().unwrap() = Some(fault);
    }
    #[cfg(test)]
    fn take_fault(&self, fault: &'static str) -> bool {
        let mut value = self.fault.lock().unwrap();
        if *value == Some(fault) {
            *value = None;
            true
        } else {
            false
        }
    }
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
    #[cfg(test)]
    pub(in crate::inventory) fn transients_created(&self) -> u64 {
        self.transients_created
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn usage(&self) -> TemporaryUsage {
        self.ledger.lock().unwrap().usage
    }
    pub(super) fn adaptive_observation(&self) -> (Option<u64>, Option<DuplicateResourceCause>) {
        self.adaptive()
            .map_or((None, None), AdaptiveSpace::observation)
    }
    fn adaptive(&self) -> Option<&AdaptiveSpace> {
        match &self.directory {
            Directory::Adaptive(adaptive) => Some(adaptive),
            _ => None,
        }
    }
    pub(super) fn adaptive_mode(&self) -> bool {
        self.adaptive().is_some()
    }
    pub(super) fn owns_adaptive_file(&self, path: &Path) -> bool {
        self.ledger
            .lock()
            .unwrap()
            .files
            .get(path)
            .is_some_and(|charge| charge.owned.is_some())
    }
    fn claim_adaptive_file(&self, path: &Path, identity: ArtifactIdentity) -> bool {
        let mut ledger = self.ledger.lock().unwrap();
        let Some(charge) = ledger.files.get_mut(path) else {
            return false;
        };
        *charge.owned.get_or_insert(identity) == identity
    }
    pub(super) fn remove_adaptive_file(&self, path: &Path) -> std::io::Result<()> {
        let ledger = self.ledger.lock().unwrap();
        let identity = ledger
            .files
            .get(path)
            .filter(|charge| charge.open_vfs_handles == 0)
            .and_then(|charge| charge.owned)
            .ok_or(std::io::ErrorKind::PermissionDenied)?;
        identity.remove(path)
    }
    fn reject_growth(&self, cause: DuplicateResourceCause) {
        if let Some(adaptive) = self.adaptive() {
            adaptive.reject(cause);
        }
    }
    pub(super) fn note_native_space_error(&self) {
        self.reject_growth(DuplicateResourceCause::TemporarySpace);
    }
    pub(in crate::inventory) fn prepaid_bytes(&self) -> u64 {
        self.disk
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, DiskReservation::bytes)
    }
    pub(super) fn persistent_mode(&self) -> bool {
        matches!(self.directory, Directory::Persistent(_))
    }
    pub(super) fn allows_transients(&self) -> bool {
        matches!(
            self.directory,
            Directory::Persistent(_) | Directory::Derived(_) | Directory::Adaptive(_)
        )
    }
    fn allowed_path(&self, path: &Path) -> bool {
        if path.parent() != Some(self.path()) {
            return false;
        }
        if !self.persistent_mode() {
            return true;
        }
        if path.file_name().is_some_and(|name| {
            [
                "inventory.db",
                "inventory.db-wal",
                "inventory.db-shm",
                "inventory.db-journal",
            ]
            .iter()
            .any(|allowed| name == *allowed)
        }) {
            return true;
        }
        self.ledger
            .lock()
            .unwrap()
            .files
            .get(path)
            .is_some_and(|entry| entry.temporary)
    }
    fn register_transient(&self, path: &Path) -> Result<()> {
        if path.parent() != Some(self.path())
            || !path
                .file_name()
                .is_some_and(super::super::generation_a::canonical::is_temporary_name)
        {
            return Err(Error::Contract("invalid canonical transient name"));
        }
        let mut ledger = self.ledger.lock().unwrap();
        if ledger.files.contains_key(path) {
            return Err(Error::Contract(
                "canonical transient name already registered",
            ));
        }
        let metadata = ledger
            .files
            .keys()
            .map(|name| name.as_os_str().as_encoded_bytes().len() + 256)
            .sum::<usize>();
        if ledger.files.len() >= 1024
            || metadata + path.as_os_str().as_encoded_bytes().len() + 256 > 256 * 1024
        {
            return Err(Error::Contract("canonical transient metadata budget"));
        }
        ledger.files.insert(
            path.into(),
            Charge {
                temporary: true,
                ..Default::default()
            },
        );
        Ok(())
    }
    pub(super) fn transient_path(&self) -> Result<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        if !self.allows_transients() {
            return Err(Error::Contract(
                "query scratch rejects unnamed SQLite files",
            ));
        }
        for _ in 0..1024 {
            let nonce = NEXT
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| Error::Contract("canonical transient identity exhausted"))?;
            let path = self.path().join(format!(
                ".sqlite-temp-{:08x}-{nonce:016x}",
                std::process::id()
            ));
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Ok(_) => continue,
                Err(error) => return Err(error.into()),
            }
            self.register_transient(&path)?;
            if self.charge(&path, 0) {
                #[cfg(test)]
                self.transients_created
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(path);
            }
            self.ledger.lock().unwrap().files.remove(&path);
            return Err(Error::Contract("canonical transient refused before create"));
        }
        Err(Error::Contract("canonical transient collision budget"))
    }
    pub fn charge(&self, path: &Path, length: u64) -> bool {
        self.admit_growth(path, length).is_some()
    }
    pub(crate) fn admit_growth(&self, path: &Path, length: u64) -> Option<WorkspaceGrowth> {
        self.admit_extent(path, length, None)
    }
    pub(super) fn admit_extent(
        &self,
        path: &Path,
        length: u64,
        previous: Option<u64>,
    ) -> Option<WorkspaceGrowth> {
        if !self.allowed_path(path) {
            self.reject_growth(DuplicateResourceCause::TemporaryAccess);
            return None;
        }
        let charge = length
            .checked_add(self.unit - 1)
            .map(|n| n / self.unit * self.unit)
            .and_then(|n| n.checked_add(self.unit))?;
        let mut ledger = self.ledger.lock().unwrap();
        let old = ledger.files.get(path).map_or(0, |v| v.bytes);
        let charge = charge.max(old);
        let total = ledger
            .usage
            .charged
            .checked_sub(old)
            .and_then(|n| n.checked_add(charge))?;
        // Keep a separate margin for filesystem metadata. This is not a claim
        // that arbitrary compressed/COW filesystem allocation is bounded.
        let margin = (ledger.usage.limit / 64).min(4 * 1024 * 1024);
        if total > ledger.usage.limit.saturating_sub(margin) {
            self.reject_growth(
                if self.adaptive().is_some_and(|a| a.policy.limited_by_space) {
                    DuplicateResourceCause::TemporarySpace
                } else {
                    DuplicateResourceCause::TemporaryLimit
                },
            );
            return None;
        }
        if !ledger.files.contains_key(path)
            && (ledger.files.len() >= 1024
                || ledger
                    .files
                    .keys()
                    .map(|p| p.as_os_str().as_encoded_bytes().len() + 256)
                    .sum::<usize>()
                    + path.as_os_str().as_encoded_bytes().len()
                    + 256
                    > 256 * 1024)
        {
            self.reject_growth(DuplicateResourceCause::TemporaryLimit);
            return None;
        }
        if let Some(guard) = &self.guard {
            let mut prospective = self.disk.lock().unwrap();
            if !guard.admit(total, prospective.as_mut()) {
                return None;
            }
        }
        // A successful native shrink leaves the historical charge intact.
        // Check physical regrowth anyway, using the known native extent.
        let growth = if let Some(previous) = previous {
            let rounded = |n: u64| n.div_ceil(self.unit);
            rounded(length).saturating_sub(rounded(previous)) * self.unit
        } else {
            charge - old
        };
        let admission = if let Some(adaptive) = self.adaptive()
            && growth > 0
        {
            Some(adaptive.admit(growth)?)
        } else {
            None
        };
        ledger.files.entry(path.to_owned()).or_default().bytes = charge;
        ledger.usage.charged = total;
        ledger.usage.peak = ledger.usage.peak.max(total);
        Some(WorkspaceGrowth {
            _admission: admission,
        })
    }
    pub fn forget_deleted(&self, path: &Path) {
        let mut ledger = self.ledger.lock().unwrap();
        if ledger
            .files
            .get(path)
            .is_some_and(|v| v.open_vfs_handles != 0)
        {
            return;
        }
        if let Some(n) = ledger.files.remove(path) {
            ledger.usage.charged -= n.bytes;
        }
    }
    pub(super) fn vfs_opened(&self, path: &Path) -> bool {
        let mut ledger = self.ledger.lock().unwrap();
        let Some(charge) = ledger.files.get_mut(path) else {
            return false;
        };
        let Some(n) = charge.open_vfs_handles.checked_add(1) else {
            return false;
        };
        charge.open_vfs_handles = n;
        true
    }
    pub(super) fn vfs_closed(&self, path: &Path) {
        let mut ledger = self.ledger.lock().unwrap();
        if let Some(charge) = ledger.files.get_mut(path) {
            charge.open_vfs_handles = charge.open_vfs_handles.saturating_sub(1);
        }
    }
    pub(super) fn may_delete(&self, path: &Path) -> bool {
        self.allowed_path(path)
            && self
                .ledger
                .lock()
                .unwrap()
                .files
                .get(path)
                .map_or(self.adaptive().is_none(), |v| v.open_vfs_handles == 0)
    }
    pub(crate) fn file(self: &Arc<Self>) -> Result<Artifact> {
        if matches!(self.directory, Directory::Persistent(_)) {
            return Err(Error::Contract(
                "canonical VFS rejects arbitrary temporary filenames",
            ));
        }
        let admission = if let Some(adaptive) = self.adaptive() {
            Some(adaptive.admit(self.unit).ok_or(Error::Contract(
                "query workspace quota: refuse before write",
            ))?)
        } else {
            None
        };
        let mut file = tempfile::NamedTempFile::new_in(self.path())?;
        drop(admission);
        let charged = if self.adaptive_mode() {
            file.disable_cleanup(true);
            let identity = ArtifactIdentity::from_file(file.as_file())?;
            if self.charge(file.path(), 0) {
                self.claim_adaptive_file(file.path(), identity)
            } else {
                let path = file.path().to_owned();
                drop(file);
                let _ = identity.remove(&path);
                return Err(Error::Contract(
                    "query workspace quota: refuse before write",
                ));
            }
        } else {
            self.charge(file.path(), 0)
        };
        if !charged {
            return Err(Error::Contract(
                "query workspace quota: refuse before write",
            ));
        }
        Ok(Artifact {
            file: Some(file),
            space: self.clone(),
        })
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Directory::Persistent(path) = &self.directory {
            // Never delete canonical data. A failed close/read leaves its live
            // guard sticky and prospective capacity charged until process exit.
            let closed = self
                .ledger
                .get_mut()
                .unwrap()
                .files
                .values()
                .all(|charge| charge.open_vfs_handles == 0);
            let bytes = super::super::lifecycle::artifact_bytes(path);
            if closed && let Ok(bytes) = bytes {
                if let Some(guard) = self.guard.take() {
                    guard.finish(bytes);
                }
            } else {
                std::mem::forget(self.guard.take());
                std::mem::forget(self.disk.get_mut().unwrap().take());
            }
            return;
        }
        if self.adaptive_mode() {
            // Adaptive leases reflect native capacity. A failed or foreign
            // removal leaves its bytes visible in subsequent OS observations.
            for (path, charge) in &self.ledger.get_mut().unwrap().files {
                if charge.open_vfs_handles == 0
                    && let Some(identity) = charge.owned
                {
                    let _ = identity.remove(path);
                }
            }
            return;
        }
        // TempDir attempts cleanup again. Should removal fail (e.g. Windows
        // sharing violation), retain those bytes in the global registry.
        // Iterate the bounded ledger, not a fallible directory enumeration.
        // A metadata/read_dir failure must never turn a residual charge into 0.
        let remaining = self
            .ledger
            .get_mut()
            .unwrap()
            .files
            .iter()
            .filter(|(path, charge)| {
                charge.open_vfs_handles != 0
                    || std::fs::remove_file(path)
                        .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
            })
            .fold(0u64, |sum, (_, charge)| sum.saturating_add(charge.bytes));
        if let Directory::Derived(path) = &self.directory {
            if remaining == 0 && std::fs::remove_dir(path).is_ok() {
                return;
            }
            // No recursive cleanup on uncertain handles or unexpected files.
            // Preserve the directory and the complete admission until restart.
            std::mem::forget(self.disk.get_mut().unwrap().take());
            return;
        }
        if remaining != 0 {
            if let Directory::Temporary(directory) = &mut self.directory {
                directory.disable_cleanup(true);
            }
            let free = sortsize_core::resources::native_free_disk(self.path()).unwrap_or(0);
            if !Governor::global().observe_artifact_group(self.path(), free, remaining) {
                // A full global residue registry must not release the admission.
                // Fail closed until process exit; no unaccounted future writes.
                std::mem::forget(self.disk.get_mut().unwrap().take());
            }
        }
    }
}
pub(crate) struct Artifact {
    file: Option<tempfile::NamedTempFile>,
    pub space: Arc<Workspace>,
}
impl Artifact {
    pub fn path(&self) -> &Path {
        self.file.as_ref().unwrap().path()
    }
    pub fn file(&self) -> &std::fs::File {
        self.file.as_ref().unwrap().as_file()
    }
    pub fn file_mut(&mut self) -> &mut std::fs::File {
        self.file.as_mut().unwrap().as_file_mut()
    }
    pub fn reopen(&self) -> Result<std::fs::File> {
        Ok(self.file.as_ref().unwrap().reopen()?)
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        #[cfg(test)]
        if self.space.take_fault("write") {
            self.file_mut().write_all(&bytes[..bytes.len() / 2])?;
            return Err(std::io::Error::other("injected short write failure").into());
        }
        Ok(self.file_mut().write_all(bytes)?)
    }
    pub fn sync(&self) -> Result<()> {
        #[cfg(test)]
        if self.space.take_fault("sync") {
            return Err(std::io::Error::other("injected sync failure").into());
        }
        Ok(self.file().sync_all()?)
    }
    pub fn admit_growth(&self, length: u64) -> Result<WorkspaceGrowth> {
        self.space
            .admit_growth(self.path(), length)
            .ok_or(Error::Contract(
                "query workspace quota: refuse before write",
            ))
    }
    #[cfg(test)]
    pub fn grow(&self, length: u64) -> Result<()> {
        self.admit_growth(length).map(drop)
    }
}
impl Drop for Artifact {
    fn drop(&mut self) {
        if let Some(mut file) = self.file.take() {
            let path = file.path().to_owned();
            if self.space.adaptive_mode() {
                drop(file);
                if self.space.remove_adaptive_file(&path).is_ok() {
                    self.space.forget_deleted(&path);
                }
            } else if !self.space.may_delete(&path) {
                // A native close failure can leave an OS handle alive. Keep
                // the name and charge, even where Unix would allow unlink.
                // keep() can fail while changing Windows attributes, and its
                // error owns a TempFile that would retry deletion on drop.
                // Disabling cleanup is infallible and needs no filesystem I/O.
                file.disable_cleanup(true);
            } else if file.close().is_ok() {
                self.space.forget_deleted(&path);
            }
        }
    }
}
