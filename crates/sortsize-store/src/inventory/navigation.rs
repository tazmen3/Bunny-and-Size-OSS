//! L08 preparation: stored pages and an identity-preserving presentation gate.
//! All Store methods run on the service thread. This module does not enable UI scanning.
use super::queries::Row;
use super::*;
use sortsize_core::resources::{Category, Governor, Reservation};
use std::sync::Arc;
#[derive(Clone)]
pub struct FolderCursor {
    acquisition: AcquisitionContext,
    parent: EntryId,
    after: EntryId,
}
pub enum Mode {
    Folder {
        parent: EntryKey,
        after: Option<FolderCursor>,
    },
    Top100,
    FolderTop100(EntryKey),
}
pub struct Target {
    pub key: EntryKey,
    pub kind: Kind,
    pub logical: Option<u64>,
}
pub struct Frame {
    pub stamp: ViewStamp,
    pub rows: Vec<Row>,
    pub next: Option<FolderCursor>,
    pub more: bool,
    pub provisional: bool,
    pub coverage: ScopeCoverage,
    pub accounted_bytes: usize,
    pub byte_limited: bool,
    _memory: Reservation,
}
impl Frame {
    /// Publish a page already resolved by the direct acquisition tree. The
    /// direct reader owns its cursor; it never creates a SQLite cursor.
    pub fn from_direct(
        stamp: ViewStamp,
        rows: Vec<Row>,
        more: bool,
        provisional: bool,
        coverage: ScopeCoverage,
    ) -> Result<Arc<Self>> {
        let overhead = rows.capacity().saturating_mul(std::mem::size_of::<Row>());
        let bytes = rows
            .iter()
            .fold(overhead.saturating_add(1024), |bytes, row| {
                bytes
                    .saturating_add(row.path.capacity())
                    .saturating_add(128)
            });
        if rows.len() > PAGE_ROWS || bytes > PUBLICATION_BYTES {
            return Err(Error::Contract("direct publication exceeds page budget"));
        }
        let memory = Governor::global()
            .reserve(Category::Publications, bytes as u64, None)
            .ok_or(Error::Contract("publication memory budget"))?;
        Ok(Arc::new(Self {
            stamp,
            rows,
            next: None,
            more,
            provisional,
            coverage,
            accounted_bytes: bytes,
            byte_limited: false,
            _memory: memory,
        }))
    }
}
impl Store {
    pub(super) fn view_source(
        &self,
        c: AcquisitionContext,
        view: ViewContext,
    ) -> Result<(bool, ScopeCoverage)> {
        self.require_integrity()?;
        if view.inventory != self.inventory
            || view.source != self.source
            || view.inventory != c.inventory
            || view.source != c.source
            || view.generation != c.generation
            || view.scan != c.scan
            || view.validation != c.validation
            || view.query.is_some()
        {
            return Err(Error::Contract("view belongs to a different acquisition"));
        }
        let (stored, sealed, coverage): (Vec<u8>, bool, i64) = self.db.query_row(
            "SELECT context,sealed,coverage FROM generations WHERE id=?1",
            [unsigned(c.generation.0)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if stored != context(c) {
            return Err(Error::Contract("stale acquisition context"));
        }
        Ok((
            sealed,
            if coverage == 2 {
                ScopeCoverage::Complete
            } else {
                ScopeCoverage::Partial
            },
        ))
    }
    /// ID order is stable observation order, not a claim of alphabetic/size sort.
    /// A live page is coherent at this read; exhaustive stable traversal requires sealing.
    pub fn navigation_frame(
        &self,
        c: AcquisitionContext,
        view: ViewContext,
        mode: Mode,
        root: &Path,
    ) -> Result<Arc<Frame>> {
        if !root.is_absolute() {
            return Err(Error::Contract("recorded root must be absolute"));
        }
        let (sealed, mut coverage) = self.view_source(c, view)?;
        let mut memory = Governor::global()
            .reserve(Category::Publications, PUBLICATION_BYTES as u64, None)
            .ok_or(Error::Contract("publication memory budget"))?;
        let _preparation = Governor::global()
            .reserve(Category::Publications, PUBLICATION_BYTES as u64, None)
            .ok_or(Error::Contract("projection preparation memory budget"))?;
        let mut rows = Vec::with_capacity(PAGE_ROWS);
        let mut bytes = PAGE_ROWS * std::mem::size_of::<Row>() + 1024;
        let mut next = None;
        let mut more = false;
        let mut byte_limited = false;
        match mode {
            Mode::Top100 | Mode::FolderTop100(_) => {
                let projection = match mode {
                    Mode::FolderTop100(folder) => {
                        if self
                            .navigation_target(c, view, folder)?
                            .is_some_and(|target| target.kind == Kind::Directory)
                        {
                            Some(self.top100_within(c.generation, folder.entry, root)?)
                        } else {
                            coverage = ScopeCoverage::Partial;
                            None
                        }
                    }
                    _ => Some(self.top100(c.generation, root)?),
                };
                byte_limited = projection.as_ref().is_some_and(|page| page.byte_limited);
                for row in projection.into_iter().flat_map(|page| page.rows) {
                    let size = row.path.capacity() + 128;
                    if bytes + size > PUBLICATION_BYTES {
                        byte_limited = true;
                        break;
                    }
                    bytes += size;
                    rows.push(row);
                }
            }
            Mode::Folder { parent, after } => {
                if parent.inventory != c.inventory
                    || parent.generation != c.generation
                    || !visible(&self.db, c.generation, parent.entry)?
                {
                    return Err(Error::Contract(
                        "folder belongs to a different or revoked generation",
                    ));
                }
                let kind: i64 = self.db.query_row(
                    "SELECT kind FROM entries WHERE generation=?1 AND id=?2",
                    params![unsigned(c.generation.0), unsigned(parent.entry.0)],
                    |r| r.get(0),
                )?;
                if kind != Kind::Directory as i64 {
                    return Err(Error::Contract("navigation target is not a directory"));
                }
                if after
                    .as_ref()
                    .is_some_and(|a| a.acquisition != c || a.parent != parent.entry)
                {
                    return Err(Error::Contract(
                        "folder cursor belongs to a different scope",
                    ));
                }
                let parent_path = self.recorded_path(c.generation, parent.entry, root)?;
                let mut statement=self.db.prepare("SELECT id,name,kind,logical FROM entries WHERE generation=?1 AND parent=?2 AND present=1 AND id>?3 ORDER BY id LIMIT 201")?;
                let mut source = statement.query(params![
                    unsigned(c.generation.0),
                    unsigned(parent.entry.0),
                    unsigned(after.as_ref().map_or(0, |a| a.after.0))
                ])?;
                while let Some(row) = source.next()? {
                    if rows.len() == PAGE_ROWS {
                        more = true;
                        break;
                    }
                    let id = EntryId(number(row, 0)?);
                    let raw: Vec<u8> = row.get(1)?;
                    let mut path = parent_path.clone();
                    path.push(crate::scheduler::native_name(
                        &NativeName::decode(&raw)
                            .map_err(|_| Error::Contract("invalid native name"))?,
                    )?);
                    if path.as_os_str().as_encoded_bytes().len() > 32 * 1024 {
                        return Err(Error::Contract("navigation path exceeds projection budget"));
                    }
                    let size = path.capacity() + 128;
                    if bytes + size > PUBLICATION_BYTES {
                        more = true;
                        byte_limited = true;
                        break;
                    }
                    bytes += size;
                    let kind = match row.get::<_, i64>(2)? {
                        1 => Kind::File,
                        2 => Kind::Directory,
                        3 => Kind::Symlink,
                        4 => Kind::Special,
                        _ => Kind::Unknown,
                    };
                    rows.push(Row {
                        key: EntryKey {
                            inventory: c.inventory,
                            generation: c.generation,
                            entry: id,
                        },
                        path,
                        kind,
                        logical: opt_number(row, 3)?,
                    });
                    next = Some(FolderCursor {
                        acquisition: c,
                        parent: parent.entry,
                        after: id,
                    });
                }
            }
        }
        // Keep only the bounded capacity retained by this immutable publication.
        // Construction still requires the full reservation and scratch allowance.
        assert!(memory.shrink_to(bytes as u64));
        Ok(Arc::new(Frame {
            stamp: ViewStamp {
                context: view,
                revision: self.revision(c.generation)?,
            },
            rows,
            next,
            more,
            provisional: !sealed,
            coverage,
            accounted_bytes: bytes,
            byte_limited,
            _memory: memory,
        }))
    }
    /// Revalidate a click/Enter target locally. No path labels or row ranks become identity.
    pub fn navigation_target(
        &self,
        c: AcquisitionContext,
        view: ViewContext,
        key: EntryKey,
    ) -> Result<Option<Target>> {
        self.view_source(c, view)?;
        if key.inventory != c.inventory
            || key.generation != c.generation
            || !visible(&self.db, c.generation, key.entry)?
        {
            return Ok(None);
        }
        let (kind, logical) = self.db.query_row(
            "SELECT kind,logical FROM entries WHERE generation=?1 AND id=?2",
            params![unsigned(c.generation.0), unsigned(key.entry.0)],
            |r| Ok((r.get::<_, i64>(0)?, opt_number(r, 1)?)),
        )?;
        Ok(Some(Target {
            key,
            kind: match kind {
                1 => Kind::File,
                2 => Kind::Directory,
                3 => Kind::Symlink,
                4 => Kind::Special,
                _ => Kind::Unknown,
            },
            logical,
        }))
    }
}
/// Small UI-side adapter. It holds at most current+latest pending bounded frames.
/// The host maps single click to select(), Enter/double click to navigation_target().
pub struct Presenter {
    expected: ViewContext,
    guard: ViewGuard,
    current: Option<Arc<Frame>>,
    pending: Option<Arc<Frame>>,
    gesture: bool,
    selected: Option<EntryKey>,
}
impl Presenter {
    pub fn new(expected: ViewContext) -> Self {
        Self {
            expected,
            guard: ViewGuard::new(expected),
            current: None,
            pending: None,
            gesture: false,
            selected: None,
        }
    }
    pub fn reset(&mut self, expected: ViewContext) {
        self.expected = expected;
        self.guard = ViewGuard::new(expected);
        self.current = None;
        self.pending = None;
        self.gesture = false;
        self.selected = self
            .selected
            .filter(|s| s.inventory == expected.inventory && s.generation == expected.generation);
    }
    pub fn publish(&mut self, frame: Arc<Frame>) -> bool {
        if !self.guard.accept_snapshot(frame.stamp) {
            return false;
        }
        if self.gesture {
            self.pending = Some(frame);
        } else {
            self.current = Some(frame);
        }
        true
    }
    pub fn gesture(&mut self, active: bool) {
        self.gesture = active;
        if !active && let Some(frame) = self.pending.take() {
            self.current = Some(frame);
        }
    }
    pub fn current(&self) -> Option<&Arc<Frame>> {
        self.current.as_ref()
    }
    pub fn selected(&self) -> Option<EntryKey> {
        self.selected
    }
    pub fn select(&mut self, stamp: ViewStamp, index: usize) -> Option<EntryKey> {
        let frame = self.current.as_ref()?;
        if frame.stamp != stamp || stamp.context != self.expected {
            return None;
        }
        let key = frame.rows.get(index)?.key;
        self.selected = Some(key);
        Some(key)
    }
}
impl Store {
    pub fn previous_folder_cursor(
        &self,
        c: AcquisitionContext,
        parent: EntryId,
        current: &FolderCursor,
    ) -> Result<Option<FolderCursor>> {
        if current.acquisition != c || current.parent != parent {
            return Err(Error::Contract("foreign folder cursor"));
        }
        let mut q=self.db.prepare("SELECT id FROM entries WHERE generation=?1 AND parent=?2 AND present=1 AND id<=?3 ORDER BY id DESC LIMIT 201")?;
        let ids = q
            .query_map(
                params![
                    unsigned(c.generation.0),
                    unsigned(parent.0),
                    unsigned(current.after.0)
                ],
                |r| number(r, 0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((ids.len() == 201).then(|| FolderCursor {
            acquisition: c,
            parent,
            after: EntryId(ids[200]),
        }))
    }
}
impl Store {
    /// A bounded preceding window reveals any stored entry without OFFSET/count scans.
    pub fn folder_cursor_for_entry(
        &self,
        c: AcquisitionContext,
        parent: EntryId,
        target: EntryId,
    ) -> Result<Option<FolderCursor>> {
        let mut q=self.db.prepare("SELECT id FROM entries WHERE generation=?1 AND parent=?2 AND present=1 AND id<?3 ORDER BY id DESC LIMIT 200")?;
        let ids = q
            .query_map(
                params![
                    unsigned(c.generation.0),
                    unsigned(parent.0),
                    unsigned(target.0)
                ],
                |r| number(r, 0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((ids.len() == 200).then(|| FolderCursor {
            acquisition: c,
            parent,
            after: EntryId(ids[199]),
        }))
    }
}

impl Store {
    pub fn ordered_navigation_frame(
        &self,
        c: AcquisitionContext,
        view: ViewContext,
        page: super::queries::Page,
    ) -> Result<Arc<Frame>> {
        self.ordered_frame(c, view, page, true)
    }
    fn ordered_frame(
        &self,
        c: AcquisitionContext,
        view: ViewContext,
        page: super::queries::Page,
        require_sealed: bool,
    ) -> Result<Arc<Frame>> {
        let (sealed, coverage) = self.view_source(c, view)?;
        if require_sealed && !sealed {
            return Err(Error::Contract("ordered frame requires sealing"));
        }
        let coverage = if require_sealed {
            coverage
        } else {
            ScopeCoverage::Partial
        };
        let mut memory = Governor::global()
            .reserve(Category::Publications, PUBLICATION_BYTES as u64, None)
            .ok_or(Error::Contract("ordered publication memory budget"))?;
        let bytes = page.accounted_bytes + PAGE_ROWS * std::mem::size_of::<Row>() + 1024;
        if bytes > PUBLICATION_BYTES {
            return Err(Error::Contract("ordered publication bytes"));
        }
        assert!(memory.shrink_to(bytes as u64));
        let byte_limited = page.more && page.rows.len() < PAGE_ROWS;
        Ok(Arc::new(Frame {
            stamp: ViewStamp {
                context: view,
                revision: self.revision(c.generation)?,
            },
            rows: page.rows,
            next: None,
            more: page.more,
            provisional: !sealed,
            coverage,
            accounted_bytes: bytes,
            byte_limited,
            _memory: memory,
        }))
    }
}

impl super::live_preview::LivePreview {
    /// Wrap a partial or completed order of this dated preview without claiming finality.
    /// Only this owned snapshot capability may publish an unsealed ordered frame.
    pub fn ordered_navigation_frame(
        &self,
        c: AcquisitionContext,
        view: ViewContext,
        page: super::queries::Page,
    ) -> Result<Arc<Frame>> {
        if c != self.acquisition_context() {
            return Err(Error::Contract(
                "ordered live frame scope or measures changed",
            ));
        }
        self.store().ordered_frame(c, view, page, false)
    }
}
