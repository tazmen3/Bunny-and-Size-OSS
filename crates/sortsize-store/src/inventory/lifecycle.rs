//! Opt-in retention of the canonical store. Every path here is local application
//! state. Recorded roots are labels, never capabilities or acquisition grants.
use super::*;
use std::{path::PathBuf, time::Duration};

const MIGRATION: &str = "
CREATE TABLE retention(singleton INTEGER PRIMARY KEY CHECK(singleton=1), semantic INTEGER NOT NULL, enabled INTEGER NOT NULL, days INTEGER NOT NULL, pending_purge INTEGER NOT NULL DEFAULT 0) STRICT;
INSERT INTO retention VALUES(1,1,0,30,0);
CREATE TABLE measurements(generation BLOB PRIMARY KEY REFERENCES generations(id), root BLOB NOT NULL, started INTEGER NOT NULL, measured INTEGER NOT NULL) STRICT, WITHOUT ROWID;
PRAGMA user_version=3;";
const ARTIFACTS: &[&str] = &[
    "inventory.db",
    "inventory.db-wal",
    "inventory.db-shm",
    "inventory.db-journal",
    "owner.lock",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub context: AcquisitionContext,
    pub root: PathBuf,
    pub started: i64,
    pub measured: i64,
    pub coverage: ScopeCoverage,
    pub logical: Option<u64>,
    pub unknown: u64,
}
/// Conservative lease of the entire owner. Mutation/purge requires &mut Store;
/// queries, comparisons and descriptors therefore cannot race retirement.
pub struct Lease<'a> {
    pub measurement: Measurement,
    pub(crate) store: &'a Store,
}
impl Lease<'_> {
    pub fn store(&self) -> &Store {
        self.store
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub enabled: bool,
    pub days: u32,
    pub pending_purge: bool,
}
/// Platform state directory only; never creates or probes a recorded source.
pub fn default_root() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")));
    let base = base
        .filter(|p| p.is_absolute())
        .ok_or(Error::Contract("local state directory unavailable"))?;
    Ok(base.join("bunny-and-size/inventories-v1"))
}
pub(crate) fn private_directory(path: &Path) -> Result<()> {
    sortsize_core::acquisition::local_state_location(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().mode(0o700).create(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
                DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
            },
        };
        // Protected ACL: the actual owner and SYSTEM only, inherited by artifacts.
        // OW is the Owner Rights SID, not Administrators or Everyone.
        let sddl: Vec<u16> = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)\0"
            .encode_utf16()
            .collect();
        let mut sd = std::ptr::null_mut();
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut sd,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                    as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // ACL is supplied at creation, with no permissive creation window.
            let made = windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(
                wide.as_ptr(),
                &attributes,
            );
            let error = std::io::Error::last_os_error();
            let secured = if made != 0 {
                SetFileSecurityW(
                    wide.as_ptr(),
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    sd,
                )
            } else {
                0
            };
            let secure_error = std::io::Error::last_os_error();
            LocalFree(sd);
            if made == 0 {
                return Err(error.into());
            }
            if secured == 0 {
                return Err(secure_error.into());
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(Error::Contract("private state ACL unsupported"));
    Ok(())
}
#[cfg(windows)]
fn check_private_acl(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW,
            DACL_SECURITY_INFORMATION, GetFileSecurityW,
        },
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut needed = 0;
    unsafe {
        GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 || needed > 64 * 1024 {
        return Err(Error::Contract("state ACL unavailable or oversized"));
    }
    let mut sd = vec![0u8; needed as usize];
    unsafe {
        if GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            sd.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut out = std::ptr::null_mut();
        if ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd.as_mut_ptr().cast(),
            1,
            DACL_SECURITY_INFORMATION,
            &mut out,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut len = 0;
        while *out.add(len) != 0 {
            len += 1;
            if len > 32768 {
                LocalFree(out.cast());
                return Err(Error::Contract("state ACL oversized"));
            }
        }
        let sddl = String::from_utf16(std::slice::from_raw_parts(out, len));
        LocalFree(out.cast());
        let sddl = sddl.map_err(|_| Error::Contract("invalid state ACL"))?;
        if !sddl.starts_with("D:P")
            || sddl.matches('(').count() != 2
            || !sddl.contains(";;;SY)")
            || !sddl.contains(";;;OW)")
        {
            return Err(Error::Contract(
                "state ACL must be protected, owner and SYSTEM only",
            ));
        }
    }
    Ok(())
}
pub(crate) fn check_directory(path: &Path) -> Result<()> {
    sortsize_core::acquisition::local_state_location(path)?;
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_dir() || m.is_symlink() {
        return Err(Error::Contract(
            "state directory must be a real private directory",
        ));
    }
    #[cfg(windows)]
    check_private_acl(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(Error::Contract(
                "state directory permissions are not private",
            ));
        }
    }
    Ok(())
}
/// Declared application artifacts only. Unknown files or links stop deletion.
pub(crate) fn artifact_bytes(path: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for (index, entry) in std::fs::read_dir(path)?.enumerate() {
        if index >= 1024 {
            return Err(Error::Contract("store artifact metadata budget"));
        }
        let e = entry?;
        if !ARTIFACTS.iter().any(|name| e.file_name() == *name)
            && !generation_a::canonical::is_temporary_name(&e.file_name())
        {
            return Err(Error::Contract(
                "unrecognized store artifact; preserve for inspection",
            ));
        }
        let m = std::fs::symlink_metadata(e.path())?;
        if !m.is_file() || m.is_symlink() {
            return Err(Error::Contract("unsafe store artifact"));
        }
        bytes = bytes
            .checked_add(m.len())
            .ok_or(Error::Contract("artifact size overflow"))?;
    }
    Ok(bytes)
}
/// Explicit retirement only, after SQLite closes under its exclusive lease.
/// A complete preflight preserves the whole group if any entry is foreign.
pub(crate) fn remove_canonical_temporaries(path: &Path) -> Result<()> {
    artifact_bytes(path)?;
    for (index, entry) in std::fs::read_dir(path)?.enumerate() {
        if index >= 1024 {
            return Err(Error::Contract("store artifact metadata budget"));
        }
        let entry = entry?;
        if generation_a::canonical::is_temporary_name(&entry.file_name()) {
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_symlink() || !metadata.is_file() {
                return Err(Error::Contract("unsafe canonical temporary preserved"));
            }
            std::fs::remove_file(entry.path())?;
        }
    }
    sync_directory(path)
}
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn publish_directory(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to)?;
    sync_directory(to.parent().ok_or(Error::Contract("destination parent"))?)
}
/// Creates only local application state. Existing broad Unix permissions are
/// refused rather than silently changing someone else's directory.
pub fn ensure_root(path: &Path) -> Result<()> {
    sortsize_core::acquisition::local_state_location(path)?;
    if path.exists() {
        return check_directory(path);
    }
    let parent = path
        .parent()
        .ok_or(Error::Contract("state parent missing"))?;
    std::fs::create_dir_all(parent)?;
    private_directory(path)
}
/// Reconstruct budget accounting after restart, including unfinished backups
/// and preserved corrupt groups. No database or source is opened here.
pub fn account_root(root: &Path) -> Result<u64> {
    check_directory(root)?;
    if ["IDENTITY.json", "STATE.json", "CURRENT.json"]
        .iter()
        .any(|name| std::fs::symlink_metadata(root.join(name)).is_ok())
    {
        return super::generation_a::Repository::account_directory(root);
    }
    let mut count = 0;
    let mut total = 0u64;
    for e in std::fs::read_dir(root)? {
        let e = e?;
        count += 1;
        if count > 1024 {
            return Err(Error::Contract("state catalogue budget"));
        }
        if e.file_name() == "owner.lock" {
            continue;
        }
        let name = e.file_name();
        let name = name
            .to_str()
            .ok_or(Error::Contract("foreign state entry"))?;
        if !(name.starts_with("inv-") || name.starts_with(".stage-")) {
            return Err(Error::Contract("foreign state group preserved"));
        }
        check_directory(&e.path())?;
        let repository = [
            "IDENTITY.json",
            "STATE.json",
            "CURRENT.json",
            "ORIGIN.json",
            "POLICY.json",
        ]
        .iter()
        .any(|name| std::fs::symlink_metadata(e.path().join(name)).is_ok());
        if repository {
            total = total
                .checked_add(super::generation_a::Repository::account_directory(
                    &e.path(),
                )?)
                .ok_or(Error::Contract("state repository budget overflow"))?;
            continue;
        }
        let bytes = artifact_bytes(&e.path())?;
        total = total
            .checked_add(bytes)
            .ok_or(Error::Contract("state budget overflow"))?;
        if !sortsize_core::resources::Governor::global().observe_artifact_group(
            &std::fs::canonicalize(e.path())?,
            sortsize_core::resources::native_free_disk(root).unwrap_or(0),
            bytes,
        ) {
            return Err(Error::Contract("artifact registry budget"));
        }
    }
    Ok(total)
}
impl Store {
    /// Explicit additive transaction. Legacy readers still reject schema 3.
    /// An incompatible future migration must use a budgeted coherent copy.
    pub fn enable_lifecycle(&mut self) -> Result<()> {
        let version: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if (3..=6).contains(&version) {
            self.retention()?;
            return Ok(());
        }
        if !(1..=2).contains(&version) {
            return Err(Error::Contract("future format preserved"));
        }
        let _disk = self.disk_admission(8 * 1024 * 1024)?;
        let tx = self.db.transaction()?;
        if version == 1 {
            tx.execute_batch("CREATE TABLE recovery_guard(generation BLOB PRIMARY KEY REFERENCES generations(id), proof BLOB NOT NULL) STRICT, WITHOUT ROWID;")?;
        }
        tx.execute_batch(MIGRATION)?;
        tx.commit()?;
        Ok(())
    }
    pub fn retention(&self) -> Result<Retention> {
        let version: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 3 {
            return Ok(Retention {
                enabled: false,
                days: 30,
                pending_purge: false,
            });
        }
        let (semantic, enabled, days, pending): (i64, bool, u32, bool) = self.db.query_row(
            "SELECT semantic,enabled,days,pending_purge FROM retention WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if semantic != 1 {
            return Err(Error::Contract("future retention semantics preserved"));
        }
        Ok(Retention {
            enabled,
            days,
            pending_purge: pending,
        })
    }
    /// A source opt-in is only a local retention policy, never a network grant.
    pub fn set_retention(&mut self, enabled: bool, days: u32) -> Result<Retention> {
        if !enabled && self.history_generation()?.is_some() {
            return Err(Error::Contract(
                "history-owned measure cannot be disabled as cache",
            ));
        }
        if !(1..=3650).contains(&days) {
            return Err(Error::Contract("retention days outside 1..3650"));
        }
        self.enable_lifecycle()?;
        self.db.pragma_update(None, "synchronous", "FULL")?;
        let synchronous: i64 = self
            .db
            .pragma_query_value(None, "synchronous", |r| r.get(0))?;
        if synchronous != 2 {
            return Err(Error::Contract("FULL durability unavailable"));
        }
        let _disk = self.disk_admission(8 * 1024 * 1024)?;
        self.db.execute(
            "UPDATE retention SET enabled=?1,days=?2,pending_purge=?3 WHERE singleton=1",
            params![enabled, days, !enabled],
        )?;
        let result = self.retention()?;
        if result.enabled != enabled || result.days != days {
            return Err(Error::Contract("retention readback failed"));
        }
        Ok(result)
    }
    /// Called at the actual measurement, never at cache consultation.
    pub fn record_measurement(
        &mut self,
        g: GenerationId,
        root: &Path,
        started: i64,
        measured: i64,
    ) -> Result<()> {
        if measured < started
            || !root.is_absolute()
            || root.as_os_str().as_encoded_bytes().len() > 32 * 1024
        {
            return Err(Error::Contract("invalid measurement descriptor"));
        }
        self.enable_lifecycle()?;
        let sealed: bool = self.db.query_row(
            "SELECT sealed FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| r.get(0),
        )?;
        if !sealed {
            return Err(Error::Contract("measurement must be sealed"));
        }
        let _disk = self.disk_admission(8 * 1024 * 1024)?;
        let raw = super::queries::native_path(root).encode();
        // Idempotent, and never refresh a sealed observation's timestamps/root.
        self.db.execute(
            "INSERT INTO measurements VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
            params![unsigned(g.0), raw, started, measured],
        )?;
        let actual = self.measurement(g)?;
        if actual.root != root || actual.started != started || actual.measured != measured {
            return Err(Error::Contract("measurement descriptor is immutable"));
        }
        Ok(())
    }
    pub fn measurement(&self, g: GenerationId) -> Result<Measurement> {
        let (raw,started,measured,coverage):(Vec<u8>,i64,i64,i64)=self.db.query_row("SELECT CASE WHEN length(m.root)<=65538 THEN m.root ELSE NULL END,m.started,m.measured,g.coverage FROM measurements m JOIN generations g ON g.id=m.generation WHERE g.sealed=1 AND g.id=?1",[unsigned(g.0)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let root = PathBuf::from(crate::scheduler::native_name(
            &NativeName::decode(&raw).map_err(|_| Error::Contract("invalid recorded root"))?,
        )?);
        let totals = self.totals(g)?;
        Ok(Measurement {
            context: self.acquisition_context(g)?,
            root,
            started,
            measured,
            coverage: if coverage == 2 {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
            logical: totals.logical,
            unknown: totals.unknown,
        })
    }
    pub fn lease(&self, g: GenerationId) -> Result<Lease<'_>> {
        self.require_integrity()?;
        Ok(Lease {
            measurement: self.measurement(g)?,
            store: self,
        })
    }
    pub fn measurements(&self) -> Result<Vec<Measurement>> {
        if self
            .db
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
            < 3
        {
            return Ok(vec![]);
        }
        let ids: Vec<u64> = self
            .db
            .prepare("SELECT generation FROM measurements ORDER BY generation LIMIT 4")?
            .query_map([], |r| number(r, 0))?
            .collect::<std::result::Result<_, _>>()?;
        if ids.len() > 3 {
            return Err(Error::Contract("generation admission violated"));
        }
        ids.into_iter()
            .map(|g| self.measurement(GenerationId(g)))
            .collect()
    }
    /// Online SQLite backup, validated before atomic publication. The old store
    /// remains the rollback copy until its temporary owner drops it. Interrupted
    /// staging directories are deliberately retained and charged, never opened
    /// as published inventories. Callers retry to a new destination explicitly.
    pub fn retain_copy(&mut self, destination: &Path, days: u32) -> Result<Store> {
        self.set_retention(true, days)?;
        self.copy_local(destination)
    }
    pub fn copy_local(&self, destination: &Path) -> Result<Store> {
        self.require_integrity()?;
        sortsize_core::acquisition::local_state_location(destination)?;
        for saved in self.saved_records()? {
            if sortsize_core::acquisition::contains_path(&saved.root, destination) {
                return Err(Error::Contract("cache must be outside the acquired source"));
            }
        }
        if destination.exists() {
            return Err(Error::Contract("retained destination already exists"));
        }
        let parent = destination
            .parent()
            .ok_or(Error::Contract("destination parent missing"))?;
        check_directory(parent)?;
        let _budget_lock = lock(parent)?;
        let bytes = artifact_bytes(&self.directory)?;
        // Backup + destination WAL + rollback source all admitted together.
        let _disk = self.disk_admission(
            bytes
                .checked_mul(2)
                .and_then(|n| n.checked_add(16 * 1024 * 1024))
                .ok_or(Error::Contract("copy budget overflow"))?,
        )?;
        let stage = parent.join(format!(".stage-{:032x}", self.inventory.0));
        private_directory(&stage)?;
        let mut target = crate::open(&stage.join("inventory.db"), Durability::Retained)?;
        #[cfg(test)]
        tests::copy_hook("before_backup");
        let result = (|| -> Result<()> {
            rusqlite::backup::Backup::new(&self.db, &mut target)?.run_to_completion(
                256,
                Duration::from_millis(1),
                None,
            )?;
            let integrity: String = target.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if integrity != "ok" {
                return Err(Error::Contract("backup integrity failed; preserved"));
            }
            target.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })();
        drop(target);
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(stage.join("inventory.db"))?
            .sync_all()?;
        // Register even a failed stage; it remains inspectable within quota.
        let governor = sortsize_core::resources::Governor::global();
        let _ = governor.observe_artifact_group(
            &stage,
            sortsize_core::resources::native_free_disk(parent).unwrap_or(0),
            artifact_bytes(&stage)?,
        );
        result?;
        sync_directory(&stage)?;
        #[cfg(test)]
        tests::copy_hook("before_publish");
        publish_directory(&stage, destination)?;
        let _ = governor.observe_artifact_group(
            &stage,
            sortsize_core::resources::native_free_disk(parent).unwrap_or(0),
            0,
        );
        let retained = Store::reopen(destination, Durability::Retained)?;
        if retained.inventory != self.inventory
            || retained.source != self.source
            || retained.latest_sealed()? != self.latest_sealed()?
        {
            return Err(Error::Contract("published inventory verification failed"));
        }
        retained.disk_admission(0)?;
        Ok(retained)
    }
    /// Explicitly recover a complete interrupted backup. Invalid/incomplete
    /// stages stay preserved under quota; never turn corruption into a new scan.
    pub fn recover_copy(
        parent: &Path,
        inventory: InventoryId,
        destination: &Path,
    ) -> Result<Store> {
        check_directory(parent)?;
        if destination.parent() != Some(parent) || destination.exists() {
            return Err(Error::Contract("copy recovery destination"));
        }
        let _owner = lock(parent)?;
        let stage = parent.join(format!(".stage-{:032x}", inventory.0));
        let store = Store::reopen(&stage, Durability::Retained)?;
        if store.inventory != inventory || !store.retention()?.enabled {
            return Err(Error::Contract("stage identity or retention mismatch"));
        }
        let check: String = store
            .db
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(Error::Contract("corrupt stage preserved"));
        }
        store.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(store);
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(stage.join("inventory.db"))?
            .sync_all()?;
        #[cfg(test)]
        tests::copy_hook("before_publish");
        publish_directory(&stage, destination)?;
        sortsize_core::resources::Governor::global().observe_artifact_group(
            &stage,
            sortsize_core::resources::native_free_disk(parent).unwrap_or(0),
            0,
        );
        let store = Store::reopen(destination, Durability::Retained)?;
        store.disk_admission(0)?;
        Ok(store)
    }
    /// Ordinary two generations, third only for last complete + latest partial
    /// + candidate. G0 is protected as latest/last-complete or by an active lease.
    pub fn retire_generation(&mut self, g: GenerationId) -> Result<()> {
        if self.history_generation()? == Some(g) {
            return Err(Error::Contract("history-owned generation protected"));
        }
        if self.latest_sealed()? == Some(g) || self.latest_complete()? == Some(g) {
            return Err(Error::Contract("reference generation protected"));
        }
        let _disk = self.disk_admission(8 * 1024 * 1024)?;
        let tx = self.db.transaction()?;
        tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
        // Remove the generation first in the same deferred-FK transaction so
        // the immutable-entry triggers permit retirement, never live mutation.
        tx.execute("DELETE FROM generations WHERE id=?1", [unsigned(g.0)])?;
        if tx.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= 5 {
            tx.execute(
                "DELETE FROM acquisition_records WHERE generation=?1",
                [unsigned(g.0)],
            )?;
        }

        if tx.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= 4 {
            tx.execute(
                "DELETE FROM entry_evidence WHERE generation=?1",
                [unsigned(g.0)],
            )?;
        }
        if tx.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= 6 {
            tx.execute(
                "DELETE FROM collection_options WHERE generation=?1",
                [unsigned(g.0)],
            )?;
            tx.execute(
                "DELETE FROM entry_properties WHERE generation=?1",
                [unsigned(g.0)],
            )?;
        }
        for table in ["tasks", "contributions", "recovery_guard", "entries"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE generation=?1"),
                [unsigned(g.0)],
            )?;
        }
        if tx.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= 3 {
            tx.execute(
                "DELETE FROM measurements WHERE generation=?1",
                [unsigned(g.0)],
            )?;
        }
        tx.commit()?;
        self.folder_projection.borrow_mut().take();
        self.folder_memory.borrow_mut().take();
        self.folder_cache_ready.set(false);
        Ok(())
    }
    pub fn expired(&self, now: i64) -> Result<bool> {
        let r = self.retention()?;
        let mut measured: Option<i64> =
            if self
                .db
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
                >= 3
            {
                self.db
                    .query_row("SELECT max(measured) FROM measurements", [], |r| r.get(0))?
            } else {
                None
            };
        if measured.is_none()
            && self
                .db
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
                >= 5
        {
            measured =
                self.db
                    .query_row("SELECT max(started) FROM acquisition_records", [], |r| {
                        r.get(0)
                    })?;
        }
        Ok(r.pending_purge
            || measured.is_some_and(|t| now.saturating_sub(t) >= i64::from(r.days) * 86400))
    }
    /// Explicit local forgetting. Consumes the owner and all Rust leases must
    /// already have ended. Unknown files, future formats or corruption are never
    /// removed by this path. The stable lock inode remains, preventing ABA.
    pub fn forget(self) -> Result<()> {
        if std::sync::Arc::strong_count(&self._owner) != 1 {
            return Err(Error::Contract("live preview still owns candidate lease"));
        }
        self.require_integrity()?;
        self.retention()?;
        if self.history_generation()?.is_some() {
            return Err(Error::Contract(
                "history-owned measure cannot be purged as cache",
            ));
        }
        artifact_bytes(&self.directory)?;
        let integrity: String = self.db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if integrity != "ok" {
            return Err(Error::Contract("corrupt group preserved; no purge"));
        }
        let path = self.directory.clone();
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        // Keep owner lock open until artifacts are gone; never unlink its inode.
        let Store {
            db,
            _owner,
            _canonical,
            ..
        } = self;
        db.close().map_err(|(_, e)| Error::Sql(e))?;
        // Finish physical accounting after SQLite closes and before owned
        // files are removed, while the stable owner lock is still held.
        drop(_canonical);
        remove_canonical_temporaries(&path)?;
        for name in [
            "inventory.db-wal",
            "inventory.db-shm",
            "inventory.db-journal",
            "inventory.db",
        ] {
            match std::fs::remove_file(path.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        sync_directory(&path)?;
        sortsize_core::resources::Governor::global().observe_artifact_group(
            &path,
            sortsize_core::resources::native_free_disk(&path).unwrap_or(0),
            0,
        );
        drop(_owner);
        Ok(())
    }
}
#[cfg(test)]
mod tests;
