//! V0.5 storage primitives under qualification; not yet used by the scanner/UI.
pub mod codec;
pub mod direct;
pub mod direct_save;
pub mod direct_snapshot;
pub mod direct_tree;
pub mod inventory;
mod scan_metrics;
pub mod scan_trace;
pub use scan_metrics::report_scan_metrics;

use rusqlite::{Connection, OpenFlags, Result};
use std::{path::Path, time::Duration};

#[derive(Clone, Copy, Debug)]
pub enum Durability {
    Reconstructible,
    Retained,
}

/// One owner opens a local database. Authorization/lifecycle belongs to L02/L12.
/// No source path is acquired here. Callers must supply an owned local store.
pub fn open(path: &Path, durability: Durability) -> Result<Connection> {
    open_with_vfs(path, durability, None)
}
pub(crate) fn open_with_vfs(
    path: &Path,
    durability: Durability,
    vfs: Option<&str>,
) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let db = if let Some(vfs) = vfs {
        Connection::open_with_flags_and_vfs(path, flags, vfs)?
    } else {
        Connection::open_with_flags(path, flags)?
    };
    db.busy_timeout(Duration::from_millis(100))?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA journal_mode=WAL; PRAGMA mmap_size=0; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE; PRAGMA wal_autocheckpoint=1000;")?;
    db.pragma_update(
        None,
        "synchronous",
        match durability {
            Durability::Reconstructible => "NORMAL",
            Durability::Retained => "FULL",
        },
    )?;
    Ok(db)
}

/// Qualification schema, deliberately versioned separately from the eventual
/// product schema. BLOB IDs retain the whole u64 domain; unknown size is NULL.
pub const QUALIFICATION_SCHEMA: &str = include_str!("qualification.sql");

pub mod scheduler;
