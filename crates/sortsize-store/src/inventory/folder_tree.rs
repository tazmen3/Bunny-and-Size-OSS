//! Disposable, cooperative sibling orders for the desktop folder tree.
//! The canonical schema and the acquisition snapshot remain read-only. Only a
//! requested branch is materialized, with bounded RAM and VFS-accounted disk.
use super::*;
use super::{
    folder_totals::FolderMeasure,
    queries::{
        Cursor, Page, native_path, path_order, read_page,
        workspace::{Workspace, vfs::Vfs},
    },
};
use sortsize_core::resources::{Category, Governor, Reservation};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

// LIMIT bounds returned children, not work over unrelated parents. Keep both
// the continuation and the expansion hint on the existing parent/id index.
const TREE_CHILDREN: &str = "SELECT id,kind,substr(name,1,32769) FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND present=1 AND id>?3 ORDER BY id LIMIT 256";
const TREE_CHILD_KINDS: &str = "SELECT kind FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND present=1 ORDER BY id LIMIT 257";

pub struct TreeBranchOrder {
    db: Connection,
    _vfs: Vfs,
    space: Arc<Workspace>,
    owner: PathBuf,
    _memory: Reservation,
    context: AcquisitionContext,
    parent: EntryKey,
    revision: Revision,
    parent_path: PathBuf,
    parent_bytes: usize,
    parent_depth: usize,
    origin: PathBuf,
    after: EntryId,
    initialized: bool,
    done: bool,
}

impl TreeBranchOrder {
    pub fn start(
        store: &Store,
        c: AcquisitionContext,
        view: ViewContext,
        parent: EntryKey,
        root: &Path,
        scratch: &Path,
        max_bytes: u64,
    ) -> Result<Self> {
        store.view_source(c, view)?;
        if !root.is_absolute() || max_bytes < 64 * 1024 {
            return Err(Error::Contract("folder tree projection bounds"));
        }
        let target = store.navigation_target(c, view, parent)?;
        if target.is_none_or(|target| target.kind != Kind::Directory) {
            return Err(Error::Contract(
                "folder tree parent is not a visible directory",
            ));
        }
        let memory = Governor::global()
            .reserve(Category::Queries, 1024 * 1024, None)
            .ok_or(Error::Contract("folder tree memory budget"))?;
        let space = Workspace::derived(store, scratch, max_bytes)?;
        let vfs = Vfs::new(space.clone())?;
        let owner = space.path().join("folder-tree.db");
        let db = Connection::open_with_flags_and_vfs(
            &owner,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            vfs.name(),
        )?;
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=NORMAL;PRAGMA mmap_size=0;PRAGMA temp_store=FILE;PRAGMA cache_size=-256;")?;
        db.pragma_update(
            None,
            "max_page_count",
            (max_bytes / 4096).min(u64::from(u32::MAX)) as u32,
        )?;
        let (parent_path, parent_bytes, parent_depth) =
            store.recorded_path_budget(c.generation, parent.entry, root)?;
        Ok(Self {
            db,
            _vfs: vfs,
            space,
            owner,
            _memory: memory,
            context: c,
            parent,
            revision: store.revision(c.generation)?,
            parent_path,
            parent_bytes,
            parent_depth,
            origin: store.directory.clone(),
            after: EntryId(0),
            initialized: false,
            done: false,
        })
    }

    pub fn matches(&self, store: &Store, c: AcquisitionContext, parent: EntryKey) -> bool {
        self.context == c
            && self.parent == parent
            && store.directory == self.origin
            && store.inventory == c.inventory
            && store.source == c.source
            && store
                .revision(c.generation)
                .is_ok_and(|revision| revision == self.revision)
    }

    /// One quantum visits at most 256 direct entries, including files. The
    /// caller supplies measurements from exactly this Store's dated snapshot.
    /// It must keep those measurements fixed until this order is discarded.
    pub fn step(
        &mut self,
        store: &Store,
        mut measure: impl FnMut(EntryId) -> Result<Option<FolderMeasure>>,
    ) -> Result<bool> {
        if !self.matches(store, self.context, self.parent) {
            return Err(Error::Contract("folder tree revision changed"));
        }
        if self.done {
            return Ok(true);
        }
        let started = Instant::now();
        let mut statement = store.db.prepare(TREE_CHILDREN)?;
        let mut rows = statement.query(params![
            unsigned(self.context.generation.0),
            unsigned(self.parent.entry.0),
            unsigned(self.after.0)
        ])?;
        let tx = self.db.transaction()?;
        if !self.initialized {
            tx.execute_batch("CREATE TABLE results(sort BLOB NOT NULL,id BLOB NOT NULL,path BLOB NOT NULL,kind INTEGER NOT NULL,logical BLOB,PRIMARY KEY(sort,id)) WITHOUT ROWID;CREATE UNIQUE INDEX result_entry ON results(id);")?;
        }
        let mut insert = tx.prepare("INSERT INTO results VALUES(?1,?2,?3,2,?4)")?;
        let mut last = self.after;
        let mut done = false;
        let mut count = 0;
        while count < 256 && (count == 0 || started.elapsed() < Duration::from_millis(20)) {
            let Some(row) = rows.next()? else {
                done = true;
                break;
            };
            let id = EntryId(number(row, 0)?);
            last = id;
            count += 1;
            if row.get::<_, i64>(1)? != Kind::Directory as i64 {
                continue;
            }
            let name: Vec<u8> = row.get(2)?;
            if self.parent.entry.0 >= id.0
                || self.parent_depth >= 1024
                || self.parent_bytes + name.len() > 32 * 1024
            {
                return Err(Error::Contract(
                    "folder tree path exceeds projection budget",
                ));
            }
            let mut path = self.parent_path.clone();
            path.push(crate::scheduler::native_name(
                &NativeName::decode(&name)
                    .map_err(|_| Error::Contract("invalid folder tree native name"))?,
            )?);
            let logical = measure(id)?.and_then(|m| m.logical);
            let mut key = vec![u8::from(logical.is_none())];
            if let Some(logical) = logical {
                key.extend((!logical).to_be_bytes());
            }
            // Equal measured sizes and unknown sizes use native paths and IDs,
            // never translated labels, discovery timing, or rounded UI sizes.
            key.extend(path_order(&path));
            insert.execute(params![
                key,
                unsigned(id.0),
                native_path(&path).encode(),
                logical.map(unsigned)
            ])?;
        }
        drop(insert);
        tx.commit()?;
        self.after = last;
        self.initialized = true;
        self.done = done;
        Ok(done)
    }

    pub fn ready(&self) -> bool {
        self.done
    }

    /// A partial order is usable immediately. `ready()` distinguishes the
    /// eventual exact sibling order from the directories visited so far.
    pub fn page(&self, after: Option<&Cursor>) -> Result<Page> {
        if !self.initialized {
            return Err(Error::Contract("folder tree order has not started"));
        }
        read_page(
            &self.db,
            self.context.inventory,
            self.context.generation,
            &self.owner,
            after,
            false,
        )
    }

    pub fn previous(&self, before: &Cursor) -> Result<Page> {
        if !self.initialized {
            return Err(Error::Contract("folder tree order has not started"));
        }
        read_page(
            &self.db,
            self.context.inventory,
            self.context.generation,
            &self.owner,
            Some(before),
            true,
        )
    }

    /// Locate one identity without scanning all preceding pages. The requested
    /// entry starts the returned bounded page, so a selection outside the
    /// currently displayed sibling page can be revealed without retaining it.
    pub fn page_containing(&self, entry: EntryId) -> Result<Option<Page>> {
        if !self.initialized {
            return Ok(None);
        }
        let Some(sort) = self
            .db
            .query_row(
                "SELECT sort FROM results WHERE id=?1",
                [unsigned(entry.0)],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let before = self.db.query_row(
            "SELECT sort,id FROM results WHERE (sort,id)<(?1,?2) ORDER BY sort DESC,id DESC LIMIT 1",
            params![sort, unsigned(entry.0)],
            |row| Ok(Cursor { owner: self.owner.clone(), sort: row.get(0)?, id: EntryId(number(row, 1)?) }),
        ).optional()?;
        self.page(before.as_ref()).map(Some)
    }

    pub fn temporary_usage(&self) -> queries::TemporaryUsage {
        self.space.usage()
    }
}

impl Store {
    /// Bounded expansion hint. None means the first 256 direct entries contain
    /// only files and more entries remain; the UI may offer tentative expansion.
    pub fn folder_has_children(
        &self,
        c: AcquisitionContext,
        parent: EntryId,
    ) -> Result<Option<bool>> {
        self.require_integrity()?;
        // This is a reader, including on a certified immutable generation.
        // The acquisition `generation` guard is intentionally write-only and
        // rejects sealing; revalidate identity and context without that guard.
        if c.inventory != self.inventory || c.source != self.source {
            return Err(Error::Contract("folder tree belongs to another inventory"));
        }
        let stored: Vec<u8> = self.db.query_row(
            "SELECT context FROM generations WHERE id=?1",
            [unsigned(c.generation.0)],
            |row| row.get(0),
        )?;
        if stored != context(c) {
            return Err(Error::Contract("stale folder tree acquisition context"));
        }
        if !visible(&self.db, c.generation, parent)? {
            return Ok(Some(false));
        }
        let mut statement = self.db.prepare(TREE_CHILD_KINDS)?;
        let mut rows = statement.query(params![unsigned(c.generation.0), unsigned(parent.0)])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            if count == 256 {
                return Ok(None);
            }
            if row.get::<_, i64>(0)? == Kind::Directory as i64 {
                return Ok(Some(true));
            }
            count += 1;
        }
        Ok(Some(false))
    }
}

#[cfg(test)]
mod tests;
