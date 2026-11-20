//! Quota-controlled SQLite staging for a sealed direct snapshot.
use super::{
    Error, Result,
    queries::workspace::{Workspace, vfs::Vfs},
};
use rusqlite::{Connection, OpenFlags};
use sortsize_core::resources::Governor;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Staging {
    vfs: Option<Vfs>,
    space: Option<Arc<Workspace>>,
    temp: Option<tempfile::TempDir>,
    path: PathBuf,
}
impl Staging {
    pub fn new(parent: &Path, limit: u64) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix(".pending-snapshot-")
            .tempdir_in(parent)?;
        let directory = std::fs::canonicalize(temp.path())?;
        let space = Workspace::persistent(&directory, limit, None)?;
        let vfs = Vfs::new(space.clone())?;
        Ok(Self {
            vfs: Some(vfs),
            space: Some(space),
            temp: Some(temp),
            path: directory.join("inventory.db"),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn open(&self) -> Result<Connection> {
        Ok(Connection::open_with_flags_and_vfs(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            self.vfs
                .as_ref()
                .ok_or(Error::Contract("snapshot staging closed"))?
                .name(),
        )?)
    }
    /// Caller has closed the SQLite connection before publishing the file.
    pub fn close(&mut self) {
        self.vfs.take();
        self.space.take();
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        self.close();
        if let Some(temp) = self.temp.take() {
            let path = std::fs::canonicalize(temp.path()).ok();
            if temp.close().is_ok()
                && let Some(path) = path
            {
                Governor::global().forget_deleted_artifact_group(&path);
            }
        }
    }
}
