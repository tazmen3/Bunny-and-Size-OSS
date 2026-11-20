//! Persistent mode of the audited SQLite quota VFS. DB closes before this owner.
use super::*;
use queries::workspace::{TemporaryUsage, Workspace, vfs::Vfs};
use sortsize_core::resources::DiskReservation;

pub(in crate::inventory) struct CanonicalStorage {
    // Deregister callbacks only after Store.db is closed. Keep the ledger alive
    // until the registration/context and every native file handle are gone.
    vfs: Vfs,
    space: Arc<Workspace>,
}
impl CanonicalStorage {
    pub fn new(path: &Path, limit: u64, prospective: Option<DiskReservation>) -> Result<Self> {
        let space = Workspace::persistent(path, limit, prospective)?;
        let vfs = Vfs::new(space.clone())?;
        Ok(Self { vfs, space })
    }
    pub fn name(&self) -> &str {
        self.vfs.name()
    }
    pub fn prepaid_bytes(&self) -> u64 {
        self.space.prepaid_bytes()
    }
    pub fn usage(&self) -> TemporaryUsage {
        self.space.usage()
    }
}

/// Reserved namespace for quota-owned SQLite transient files on this volume.
pub(in crate::inventory) fn is_temporary_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(tail) = name.strip_prefix(".sqlite-temp-") else {
        return false;
    };
    let Some((process, nonce)) = tail.split_once('-') else {
        return false;
    };
    process.len() == 8
        && nonce.len() == 16
        && process
            .bytes()
            .chain(nonce.bytes())
            .all(|byte| byte.is_ascii_hexdigit())
}
