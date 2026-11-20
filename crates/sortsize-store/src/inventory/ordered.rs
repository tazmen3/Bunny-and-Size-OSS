//! Bounded disk materialization for global sibling sorts. No source I/O.
use super::*;
use super::{
    folder_totals::{FolderFold, FolderMeasure},
    queries::workspace::{Workspace, vfs::Vfs},
    queries::{Cursor, Page, native_path, path_order, read_page},
};
use sortsize_core::{
    MetadataValue, NativeOwnerId,
    resources::{Category, Governor, Reservation},
};
const ENRICHED_SIBLINGS: &str = "SELECT e.id,e.kind,e.logical,substr(e.name,1,32769),e.modified_s,e.modified_ns,e.allocated,p.allocated_state,p.accessed_state,p.accessed_s,p.accessed_ns,p.owner FROM entries e LEFT JOIN entry_properties p ON p.generation=e.generation AND p.id=e.id WHERE e.generation=?1 AND e.parent=?2 AND e.present=1 AND e.id>?3 ORDER BY e.id LIMIT 256";
pub(super) const SIBLINGS: &str = "SELECT id,kind,logical,substr(name,1,32769),modified_s,modified_ns,allocated FROM entries WHERE generation=?1 AND parent=?2 AND present=1 AND id>?3 ORDER BY id LIMIT 256";
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    SizeDown,
    SizeUp,
    NameUp,
    NameDown,
    AllocatedDown,
    AllocatedUp,
    OwnerUp,
    OwnerDown,
    ModifiedDown,
    ModifiedUp,
    AccessedDown,
    AccessedUp,
}
impl Sort {
    pub fn from_index(i: i32) -> Option<Self> {
        Some(match i {
            0 => Self::SizeDown,
            1 => Self::SizeUp,
            2 => Self::NameUp,
            3 => Self::NameDown,
            4 => Self::AllocatedDown,
            5 => Self::AllocatedUp,
            6 => Self::OwnerUp,
            7 => Self::OwnerDown,
            8 => Self::ModifiedDown,
            9 => Self::ModifiedUp,
            10 => Self::AccessedDown,
            11 => Self::AccessedUp,
            _ => return None,
        })
    }
}
#[derive(Clone)]
pub struct OwnerLabel {
    pub owner: NativeOwnerId,
    pub label: String,
}
pub struct OrderedFolder {
    db: Connection,
    _vfs: Vfs,
    space: Arc<Workspace>,
    owner: PathBuf,
    _memory: Reservation,
    context: AcquisitionContext,
    live: bool,
    parent: EntryId,
    revision: Revision,
    parent_path: PathBuf,
    parent_bytes: usize,
    parent_depth: usize,
    origin: PathBuf,
    sort: Sort,
    schema: i64,
    owner_labels: Vec<(Vec<u8>, Vec<u8>)>,
    initialized: bool,
    after: Option<EntryId>,
    done: bool,
}
fn numeric(v: Option<u64>, descending: bool) -> Vec<u8> {
    let mut b = vec![u8::from(v.is_none())];
    if let Some(n) = v {
        b.extend(if descending { !n } else { n }.to_be_bytes());
    }
    b
}
fn moment(v: MetadataValue<std::time::SystemTime>, down: bool) -> Vec<u8> {
    let MetadataValue::Known { value, .. } = v else {
        return vec![1];
    };
    let ns = match value.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    let mut raw = (ns as u128 ^ (1u128 << 127)).to_be_bytes();
    if down {
        raw.iter_mut().for_each(|b| *b = !*b)
    }
    let mut key = vec![0];
    key.extend(raw);
    key
}
impl Store {
    pub fn start_ordered_folder(
        &self,
        c: AcquisitionContext,
        parent: EntryId,
        root: &Path,
        sort: Sort,
        scratch: &Path,
        max_bytes: u64,
    ) -> Result<OrderedFolder> {
        self.start_ordered_folder_inner(c, parent, root, sort, scratch, max_bytes, true)
    }
    #[allow(clippy::too_many_arguments)]
    fn start_ordered_folder_inner(
        &self,
        c: AcquisitionContext,
        parent: EntryId,
        root: &Path,
        sort: Sort,
        scratch: &Path,
        max_bytes: u64,
        require_sealed: bool,
    ) -> Result<OrderedFolder> {
        self.require_integrity()?;
        if max_bytes < 64 * 1024 {
            return Err(Error::Contract("ordered folder disk budget"));
        }
        if !root.is_absolute() {
            return Err(Error::Contract("recorded root must be absolute"));
        }
        let (stored, sealed): (Vec<u8>, bool) = self.db.query_row(
            "SELECT context,sealed FROM generations WHERE id=?1",
            [unsigned(c.generation.0)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if stored != context(c)
            || (require_sealed && !sealed)
            || !visible(&self.db, c.generation, parent)?
        {
            return Err(Error::Contract(
                "ordered folder requires its sealed generation",
            ));
        }
        let memory = Governor::global()
            .reserve(Category::Queries, 2 * 1024 * 1024, None)
            .ok_or(Error::Contract("ordered folder memory budget"))?;
        let space = Workspace::derived(self, scratch, max_bytes)?;
        let vfs = Vfs::new(space.clone())?;
        let owner = space.path().join("ordered.db");
        let db = Connection::open_with_flags_and_vfs(
            &owner,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            vfs.name(),
        )?;
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=NORMAL;PRAGMA mmap_size=0;PRAGMA temp_store=FILE;PRAGMA cache_size=-512;")?;
        db.pragma_update(
            None,
            "max_page_count",
            (max_bytes / 4096).min(u64::from(u32::MAX)) as u32,
        )?;
        let (parent_path, parent_bytes, parent_depth) =
            self.recorded_path_budget(c.generation, parent, root)?;
        Ok(OrderedFolder {
            db,
            _vfs: vfs,
            space,
            owner,
            _memory: memory,
            context: c,
            live: !require_sealed,
            parent,
            revision: self.revision(c.generation)?,
            parent_path,
            parent_bytes,
            parent_depth,
            origin: self.directory.clone(),
            sort,
            schema: self
                .db
                .pragma_query_value(None, "user_version", |r| r.get(0))?,
            owner_labels: Vec::new(),
            initialized: false,
            after: None,
            done: false,
        })
    }
}
impl live_preview::LivePreview {
    /// Build a global sibling order from this pinned acquisition snapshot.
    /// Before the derivative completes, known directory sizes are lower bounds
    /// and untouched ones remain unknown. Callers replace this dated order when
    /// the selected scope finishes, and label intermediate pages as partial.
    pub fn start_ordered_folder(
        &self,
        c: AcquisitionContext,
        parent: EntryId,
        root: &Path,
        sort: Sort,
        scratch: &Path,
        max_bytes: u64,
    ) -> Result<OrderedFolder> {
        if c != self.acquisition_context() {
            return Err(Error::Contract(
                "ordered live snapshot scope or measures changed",
            ));
        }
        self.store()
            .start_ordered_folder_inner(c, parent, root, sort, scratch, max_bytes, false)
    }
}
fn sequence(raw: &[u8], down: bool) -> Vec<u8> {
    let mut key = Vec::with_capacity(raw.len() * 2 + 1);
    for b in raw {
        key.extend([1, *b]);
    }
    key.push(0);
    if down {
        key.iter_mut().for_each(|b| *b = !*b)
    }
    key
}
impl OrderedFolder {
    pub fn temporary_usage(&self) -> queries::TemporaryUsage {
        self.space.usage()
    }
    /// Freeze only the bounded names available on the requesting page. No lookup
    /// or live reordering: the same explicit snapshot rule as the historical UI.
    pub fn set_owner_labels(&mut self, labels: &[OwnerLabel]) -> Result<()> {
        if self.after.is_some() || self.done || labels.len() > 200 {
            return Err(Error::Contract("owner sort snapshot bounds"));
        }
        if labels.iter().any(|p| p.label.len() > 1024) {
            return Err(Error::Contract("owner sort label budget"));
        }
        self.owner_labels = labels
            .iter()
            .map(|p| (properties::owner(p.owner), p.label.as_bytes().to_vec()))
            .collect();
        self.owner_labels.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.owner_labels.dedup_by(|a, b| a.0 == b.0);
        Ok(())
    }
    pub fn ready(&self) -> bool {
        self.done
    }
    pub fn step(&mut self, store: &Store, fold: &FolderFold) -> Result<bool> {
        if self.live {
            return Err(Error::Contract("live order requires its pinned preview"));
        }
        let generation = self.context.generation;
        self.step_with_measure(store, |id| store.folder_measure(generation, id, fold))
    }
    pub fn step_live(&mut self, preview: &live_preview::LivePreview) -> Result<bool> {
        if !self.live || self.context != preview.acquisition_context() {
            return Err(Error::Contract(
                "ordered live snapshot scope or measures changed",
            ));
        }
        self.step_with_measure(preview.store(), |id| preview.measure(id))
    }
    fn step_with_measure(
        &mut self,
        store: &Store,
        mut measure: impl FnMut(EntryId) -> Result<Option<FolderMeasure>>,
    ) -> Result<bool> {
        if store.directory != self.origin
            || store.inventory != self.context.inventory
            || store.revision(self.context.generation)? != self.revision
            || store
                .db
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
                != self.schema
        {
            return Err(Error::Contract("ordered folder revision changed"));
        }
        if self.done {
            return Ok(true);
        }
        let started = Instant::now();
        let enriched = self.schema >= 6
            && matches!(
                self.sort,
                Sort::AllocatedDown
                    | Sort::AllocatedUp
                    | Sort::OwnerUp
                    | Sort::OwnerDown
                    | Sort::AccessedDown
                    | Sort::AccessedUp
            );
        let mut stmt = store.db.prepare(if enriched {
            ENRICHED_SIBLINGS
        } else {
            SIBLINGS
        })?;
        let mut rows = stmt.query(params![
            unsigned(self.context.generation.0),
            unsigned(self.parent.0),
            unsigned(self.after.map_or(0, |x| x.0))
        ])?;
        let tx = self.db.transaction()?;
        if !self.initialized {
            // Schema and the first quantum share one transaction: a small
            // admitted workspace can be opened, and FULL rolls back all work
            // without publishing a ready result or a partially created schema.
            tx.execute_batch("CREATE TABLE results(sort BLOB NOT NULL,id BLOB NOT NULL,path BLOB NOT NULL,kind INTEGER NOT NULL,logical BLOB,PRIMARY KEY(sort,id)) WITHOUT ROWID;CREATE UNIQUE INDEX result_entry ON results(id);")?;
        }
        let mut insert = tx.prepare("INSERT INTO results VALUES(?1,?2,?3,?4,?5)")?;
        let mut last = self.after;
        let mut done = false;
        let mut n = 0;
        while n < 256 && (n == 0 || started.elapsed() < Duration::from_millis(20)) {
            let Some(r) = rows.next()? else {
                done = true;
                break;
            };
            let id = EntryId(number(r, 0)?);
            last = Some(id);
            n += 1;
            let kind: i64 = r.get(1)?;
            let name: Vec<u8> = r.get(3)?;
            if self.parent.0 >= id.0
                || self.parent_depth >= 1024
                || self.parent_bytes + name.len() > 32 * 1024
            {
                return Err(Error::Contract("recorded path exceeds projection budget"));
            }
            let mut path = self.parent_path.clone();
            path.push(crate::scheduler::native_name(
                &NativeName::decode(&name).map_err(|_| Error::Contract("invalid name"))?,
            )?);
            let measure: Option<FolderMeasure> = if kind == 2 { measure(id)? } else { None };
            let logical = if kind == 2 {
                measure.and_then(|m| m.logical)
            } else {
                opt_number(r, 2)?
            };
            let props = if matches!(
                self.sort,
                Sort::AllocatedDown
                    | Sort::AllocatedUp
                    | Sort::OwnerUp
                    | Sort::OwnerDown
                    | Sort::AccessedDown
                    | Sort::AccessedUp
            ) {
                if self.schema >= 6 {
                    match r.get::<_, Option<i64>>(7)? {
                        Some(a) => properties::decode(
                            opt_number(r, 6)?,
                            a,
                            r.get(8)?,
                            r.get(9)?,
                            r.get(10)?,
                            &r.get::<_, Vec<u8>>(11)?,
                        )?,
                        None => Default::default(),
                    }
                } else {
                    sortsize_core::acquisition::NodeProperties {
                        allocated: opt_number(r, 6)?.map_or(MetadataValue::NotCollected, |value| {
                            MetadataValue::Known {
                                value,
                                accuracy: sortsize_core::ValueAccuracy::Exact,
                            }
                        }),
                        ..Default::default()
                    }
                }
            } else {
                sortsize_core::acquisition::NodeProperties::default()
            };
            let allocated = if kind == 2 {
                measure.and_then(|m| m.allocated)
            } else {
                match props.allocated {
                    MetadataValue::Known { value, .. } => Some(value),
                    _ => None,
                }
            };
            let mut sort = match self.sort {
                Sort::SizeDown | Sort::SizeUp => numeric(logical, self.sort == Sort::SizeDown),
                Sort::AllocatedDown | Sort::AllocatedUp => {
                    numeric(allocated, self.sort == Sort::AllocatedDown)
                }
                Sort::NameUp | Sort::NameDown => {
                    let mut k = path_order(&path);
                    if self.sort == Sort::NameDown {
                        k.iter_mut().for_each(|b| *b = !*b)
                    }
                    k
                }
                Sort::ModifiedDown | Sort::ModifiedUp => moment(
                    queries::time(r.get(4)?, r.get(5)?).map_or(MetadataValue::Unknown, |value| {
                        MetadataValue::Known {
                            value,
                            accuracy: sortsize_core::ValueAccuracy::Exact,
                        }
                    }),
                    self.sort == Sort::ModifiedDown,
                ),
                Sort::AccessedDown | Sort::AccessedUp => {
                    moment(props.accessed, self.sort == Sort::AccessedDown)
                }
                Sort::OwnerUp | Sort::OwnerDown => {
                    let down = self.sort == Sort::OwnerDown;
                    let id = match props.owner.id {
                        MetadataValue::Known { value, .. } => Some(properties::owner(value)),
                        _ => None,
                    };
                    let label = id.as_ref().and_then(|id| {
                        self.owner_labels
                            .binary_search_by(|(key, _)| key.cmp(id))
                            .ok()
                            .map(|i| &self.owner_labels[i].1)
                    });
                    let class = if label.is_some() {
                        0
                    } else if id.is_some() {
                        1
                    } else {
                        match props.owner.id {
                            MetadataValue::AccessDenied => 2,
                            MetadataValue::NotCollected => 4,
                            _ => 3,
                        }
                    };
                    let mut key = vec![class];
                    if let Some(label) = label {
                        key.extend(sequence(label, down));
                    } else if let Some(id) = &id {
                        key.extend(sequence(id, down));
                    }
                    if let Some(id) = &id {
                        key.extend(sequence(id, false));
                    }
                    key
                }
            };
            if !matches!(self.sort, Sort::SizeDown | Sort::SizeUp) {
                // Preserve the historical grouping for other explicit sorts.
                sort.insert(0, u8::from(kind != 2));
            }
            // Logical sizes mix files and directories. Stable native-path tie
            // breaker, then result ID; never a translated display label.
            sort.extend(path_order(&path));
            insert.execute(params![
                sort,
                unsigned(id.0),
                native_path(&path).encode(),
                kind,
                logical.map(unsigned)
            ])?;
        }
        drop(insert);
        tx.commit()?;
        self.initialized = true;
        self.after = last;
        self.done = done;
        Ok(done)
    }
    /// Globally sorted among materialized siblings. Until `ready()`, more
    /// siblings may still enter this order, so the UI must mark it partial.
    pub fn page(&self, after: Option<&Cursor>) -> Result<Page> {
        if !self.initialized || !self.live && !self.done {
            return Err(Error::Contract("ordered folder not complete"));
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
    pub fn locate(&self, id: EntryId) -> Result<Option<Page>> {
        self.page_containing(id)
    }
    /// Preserve an identity as the first row after a dated order is replaced.
    pub fn page_containing(&self, id: EntryId) -> Result<Option<Page>> {
        if !self.initialized || !self.live && !self.done {
            return Err(Error::Contract("ordered folder not complete"));
        }
        let Some(sort) = self
            .db
            .query_row(
                "SELECT sort FROM results WHERE id=?1",
                [unsigned(id.0)],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let before=self.db.query_row("SELECT sort,id FROM results WHERE (sort,id)<(?1,?2) ORDER BY sort DESC,id DESC LIMIT 1",params![sort,unsigned(id.0)],|r|Ok(Cursor{owner:self.owner.clone(),sort:r.get(0)?,id:EntryId(number(r,1)?)})).optional()?;
        self.page(before.as_ref()).map(Some)
    }
    pub fn previous(&self, before: &Cursor) -> Result<Page> {
        if !self.initialized || !self.live && !self.done {
            return Err(Error::Contract("ordered folder not complete"));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};

    #[test]
    fn live_size_order_mixes_kinds_across_pages_and_keeps_native_ties() {
        use crate::inventory::generation_a::Pipeline;
        let temp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let mut store = Store::create(
            &temp.path().join("store"),
            c.inventory,
            c.source,
            Durability::Retained,
        )
        .unwrap();
        store.create_generation(c).unwrap();
        let root = attempt_id(0, 0, 1);
        store.begin_attempt(root).unwrap();
        let observation = |name: &str, kind, logical| Observation {
            name: native_path(Path::new(name)),
            kind,
            logical,
            allocated: None,
            modified: None,
            metadata_known: true,
        };
        batch(
            &mut store,
            root,
            0,
            None,
            &[
                observation("a-directory", Kind::Directory, None),
                observation("unknown-directory", Kind::Directory, None),
                observation("zero-directory", Kind::Directory, None),
            ],
        )
        .unwrap();
        for (parent, size) in [(1, Some(1000)), (2, None), (3, Some(0))] {
            let attempt = attempt_id(0, parent, 1);
            store.begin_attempt(attempt).unwrap();
            batch(
                &mut store,
                attempt,
                0,
                None,
                &[observation("nested", Kind::File, size)],
            )
            .unwrap();
        }
        let mut files = (0..430)
            .map(|i| observation(&format!("file-{i:04}"), Kind::File, Some(i)))
            .collect::<Vec<_>>();
        files.push(observation("z-tied-file", Kind::File, Some(1000)));
        files.push(observation("late-largest", Kind::File, Some(2000)));
        files.push(observation("unknown-file", Kind::File, None));
        batch(&mut store, root, 1, None, &files).unwrap();
        let mut pipeline = Pipeline::plan().unwrap();
        let mut preview = pipeline.live_preview(&store, c).unwrap();
        // A first bounded page is available before any global folder fold.
        let mut first_order = preview
            .start_ordered_folder(
                c,
                EntryId(0),
                temp.path(),
                Sort::SizeDown,
                temp.path(),
                8 * 1024 * 1024,
            )
            .unwrap();
        assert!(first_order.page(None).is_err());
        assert!(!first_order.step_live(&preview).unwrap());
        assert!(!first_order.ready());
        let partial = first_order.page(None).unwrap();
        assert!(!partial.rows.is_empty());
        for pair in partial.rows.windows(2) {
            if let (Some(a), Some(b)) = (pair[0].logical, pair[1].logical) {
                assert!(a >= b);
            }
            assert!(!(pair[0].logical.is_none() && pair[1].logical.is_some()));
        }
        assert!(!preview.complete());
        drop(first_order);
        while !preview.step(temp.path()).unwrap() {}
        assert!(
            preview
                .start_ordered_folder(
                    context_id(1),
                    EntryId(0),
                    temp.path(),
                    Sort::SizeDown,
                    temp.path(),
                    8 * 1024 * 1024
                )
                .is_err()
        );
        let view = ViewContext {
            inventory: c.inventory,
            source: c.source,
            generation: c.generation,
            scan: c.scan,
            validation: c.validation,
            view: ViewId(1),
            epoch: ViewEpoch(1),
            query: None,
            presentation: PresentationEpoch(1),
        };
        assert!(
            store
                .start_ordered_folder(
                    c,
                    EntryId(0),
                    temp.path(),
                    Sort::SizeDown,
                    temp.path(),
                    8 * 1024 * 1024
                )
                .is_err()
        );
        for sort in [Sort::SizeDown, Sort::SizeUp] {
            let mut order = preview
                .start_ordered_folder(
                    c,
                    EntryId(0),
                    temp.path(),
                    sort,
                    temp.path(),
                    8 * 1024 * 1024,
                )
                .unwrap();
            assert!(order.step(preview.store(), &FolderFold::default()).is_err());
            while !order.step_live(&preview).unwrap() {}
            assert!(
                store
                    .ordered_navigation_frame(c, view, order.page(None).unwrap())
                    .is_err()
            );
            assert!(
                preview
                    .ordered_navigation_frame(context_id(1), view, order.page(None).unwrap())
                    .is_err()
            );
            let frame = preview
                .ordered_navigation_frame(c, view, order.page(None).unwrap())
                .unwrap();
            assert!(frame.provisional);
            assert_eq!(frame.coverage, ScopeCoverage::Partial);
            drop(frame);
            let first = order.page(None).unwrap();
            assert!(first.more);
            let second = order.page(first.next.as_ref()).unwrap();
            let previous = order.previous(second.first.as_ref().unwrap()).unwrap();
            assert_eq!(
                first.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
                previous.rows.iter().map(|r| r.key).collect::<Vec<_>>()
            );
            let anchor = second.rows[10].key.entry;
            assert_eq!(
                order.page_containing(anchor).unwrap().unwrap().rows[0]
                    .key
                    .entry,
                anchor
            );
            let mut all = Vec::new();
            let mut page = first;
            loop {
                all.extend(page.rows);
                if !page.more {
                    break;
                }
                page = order.page(page.next.as_ref()).unwrap();
            }
            assert_eq!(all.len(), 436);
            for pair in all.windows(2) {
                let a = &pair[0];
                let b = &pair[1];
                match (a.logical, b.logical) {
                    (Some(a), Some(b)) if sort == Sort::SizeDown => assert!(a >= b),
                    (Some(a), Some(b)) => assert!(a <= b),
                    (None, Some(_)) => panic!("unknown size precedes known size"),
                    _ => {}
                }
                if a.logical == b.logical {
                    assert!(a.path < b.path);
                }
            }
            let tied = all
                .iter()
                .filter(|r| r.logical == Some(1000))
                .collect::<Vec<_>>();
            assert_eq!(tied[0].kind, Kind::Directory);
            assert_eq!(tied[1].kind, Kind::File);
            assert!(all[all.len() - 2..].iter().all(|r| r.logical.is_none()));
            if sort == Sort::SizeDown {
                assert_eq!(all[0].path.file_name().unwrap(), "late-largest");
            }
        }
        // Writer mutations cannot change the pinned order's paths or sums.
        batch(
            &mut store,
            root,
            2,
            None,
            &[observation("new-largest", Kind::File, Some(3000))],
        )
        .unwrap();
        let mut old = preview
            .start_ordered_folder(
                c,
                EntryId(0),
                temp.path(),
                Sort::SizeDown,
                temp.path(),
                8 * 1024 * 1024,
            )
            .unwrap();
        while !old.step_live(&preview).unwrap() {}
        assert_eq!(old.page(None).unwrap().rows[0].logical, Some(2000));
        drop(old);
        pipeline.restore_preview(preview).unwrap();
        let mut refreshed = pipeline.live_preview(&store, c).unwrap();
        while !refreshed.step(temp.path()).unwrap() {}
        let mut current = refreshed
            .start_ordered_folder(
                c,
                EntryId(0),
                temp.path(),
                Sort::SizeDown,
                temp.path(),
                8 * 1024 * 1024,
            )
            .unwrap();
        while !current.step_live(&refreshed).unwrap() {}
        assert_eq!(current.page(None).unwrap().rows[0].logical, Some(3000));
        drop(current);
        pipeline.restore_preview(refreshed).unwrap();
        drop(pipeline);
        for parent in 1..=3 {
            let attempt = attempt_id(0, parent, 1);
            batch(&mut store, attempt, 1, Some(ListingEnd::Exhausted), &[]).unwrap();
            reconcile(&mut store, attempt);
        }
        batch(&mut store, root, 3, Some(ListingEnd::Exhausted), &[]).unwrap();
        reconcile(&mut store, root);
        fold(&mut store, 0);
        store.seal(c).unwrap();
        let mut folded = FolderFold::default();
        while !store
            .folder_fold_step_at(c.generation, &mut folded, temp.path())
            .unwrap()
        {}
        let mut sealed = store
            .start_ordered_folder(
                c,
                EntryId(0),
                temp.path(),
                Sort::SizeDown,
                temp.path(),
                8 * 1024 * 1024,
            )
            .unwrap();
        while !sealed.step(&store, &folded).unwrap() {}
        let sealed_page = sealed.page(None).unwrap();
        assert_eq!(sealed_page.rows[0].logical, Some(3000));
        assert_eq!(sealed_page.rows[1].logical, Some(2000));
        assert_eq!(sealed_page.rows[2].kind, Kind::Directory);
        assert_eq!(sealed_page.rows[3].kind, Kind::File);
        let frame = store
            .ordered_navigation_frame(c, view, sealed_page)
            .unwrap();
        assert!(!frame.provisional);
    }

    #[test]
    fn small_ordered_budgets_open_and_refuse_work_without_publishing_partial_schema() {
        let temp = tempfile::tempdir().unwrap();
        let c = context_id(1);
        let mut store = Store::create(
            &temp.path().join("store"),
            c.inventory,
            c.source,
            Durability::Retained,
        )
        .unwrap();
        store.create_generation(c).unwrap();
        let attempt = attempt_id(1, 0, 1);
        store.begin_attempt(attempt).unwrap();
        let observations = (0..32)
            .map(|id| Observation {
                name: native_path(Path::new(&format!("file-{id:04}-{}", "x".repeat(4096)))),
                kind: Kind::File,
                logical: Some(id),
                allocated: None,
                modified: None,
                metadata_known: true,
            })
            .collect::<Vec<_>>();
        batch(
            &mut store,
            attempt,
            0,
            Some(ListingEnd::Exhausted),
            &observations,
        )
        .unwrap();
        reconcile(&mut store, attempt);
        fold(&mut store, 1);
        store.seal(c).unwrap();
        for quota in [64 * 1024, 128 * 1024] {
            let mut order = store
                .start_ordered_folder(c, EntryId(0), temp.path(), Sort::NameUp, temp.path(), quota)
                .unwrap();
            for _ in 0..2 {
                let error = order.step(&store, &FolderFold::default()).unwrap_err();
                assert!(
                    matches!(error, Error::Sql(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull))
                );
                assert!(!order.ready());
                assert!(order.page(None).is_err());
                assert!(order.db.is_autocommit());
                assert_eq!(
                    order
                        .db
                        .query_row::<i64, _, _>("SELECT count(*) FROM sqlite_schema", [], |r| r
                            .get(0))
                        .unwrap(),
                    0
                );
                assert!(order.temporary_usage().peak <= quota);
            }
        }
    }
}
