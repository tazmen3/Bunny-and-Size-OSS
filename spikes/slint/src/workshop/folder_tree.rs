//! Bounded, demand-loaded folder navigation. No filesystem traversal or UI ownership.
use super::*;
use sortsize_store::inventory::{
    Store, folder_totals::FolderMeasure, folder_tree::TreeBranchOrder,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_BRANCHES: usize = 32;
const MAX_ROWS: usize = 2048;
const ORDER_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TreeTarget {
    Folder {
        key: EntryKey,
        parent: Option<EntryId>,
        path: Arc<PathBuf>,
    },
    More {
        parent: EntryKey,
    },
    Previous {
        parent: EntryKey,
    },
    Limit,
}

struct Branch {
    order: TreeBranchOrder,
    anchor: Option<EntryId>,
    failed: bool,
    started: bool,
}
pub(super) struct FolderTree {
    expanded: BTreeSet<u64>,
    branches: BTreeMap<u64, Branch>,
    // Revision changes replace only the bounded derivative orders, never the
    // expansion state. A completing live fold changes measures at one revision.
    stamp: Option<(GenerationId, Revision, bool)>,
    round: u64,
    refused: BTreeSet<u64>,
    positions: BTreeMap<u64, EntryId>,
    // A retained page remains interactive while its replacement order builds.
    // Keep at most one latest page intent per expanded branch.
    pending_pages: BTreeMap<u64, bool>,
    retained: BTreeMap<u64, Vec<sortsize_store::inventory::queries::Row>>,
    _memory: Option<sortsize_core::resources::Reservation>,
    pub dirty: bool,
}
impl Default for FolderTree {
    fn default() -> Self {
        Self {
            expanded: BTreeSet::from([0]),
            branches: BTreeMap::new(),
            stamp: None,
            round: 0,
            refused: BTreeSet::new(),
            positions: BTreeMap::new(),
            pending_pages: BTreeMap::new(),
            retained: BTreeMap::new(),
            _memory: None,
            dirty: true,
        }
    }
}
impl FolderTree {
    pub fn copy_navigation(&self) -> sortsize_store::inventory::Result<Self> {
        let mut copy = Self {
            expanded: self.expanded.clone(),
            positions: self.positions.clone(),
            pending_pages: self.pending_pages.clone(),
            stamp: self.stamp,
            ..Self::default()
        };
        copy._memory = sortsize_core::resources::Governor::global().reserve_up_to(
            sortsize_core::resources::Category::Queries,
            32 * 1024,
            1024 * 1024,
        );
        let Some(memory) = copy._memory.as_ref() else {
            return Ok(copy);
        };
        let cache_limit = (memory.bytes() as usize / 2).min(512 * 1024);
        let mut remaining = MAX_ROWS;
        let mut bytes = 0usize;
        for (id, branch) in &self.branches {
            let rows = if branch.started {
                Self::page(branch)?.map(|p| p.rows).unwrap_or_default()
            } else {
                self.retained.get(id).cloned().unwrap_or_default()
            };
            let mut saved = Vec::new();
            for row in rows {
                let cost = row.path.as_os_str().as_encoded_bytes().len() + 128;
                if remaining == 0 || bytes + cost > cache_limit {
                    break;
                }
                bytes += cost;
                remaining -= 1;
                saved.push(row);
            }
            if !saved.is_empty() {
                copy.retained.insert(*id, saved);
            }
        }
        Ok(copy)
    }
    fn page(
        branch: &Branch,
    ) -> sortsize_store::inventory::Result<Option<sortsize_store::inventory::queries::Page>> {
        if !branch.started {
            return Ok(None);
        }
        if let Some(id) = branch.anchor {
            branch.order.page_containing(id)
        } else {
            branch.order.page(None).map(Some)
        }
    }
    fn retain_pages(&mut self) -> sortsize_store::inventory::Result<()> {
        if self.branches.is_empty() {
            return Ok(());
        }
        let copy = self.copy_navigation()?;
        self.retained = copy.retained;
        self._memory = copy._memory;
        Ok(())
    }
    pub fn toggle(&mut self, id: EntryId) {
        if !self.expanded.remove(&id.0) && self.expanded.len() < MAX_BRANCHES {
            self.expanded.insert(id.0);
        }
        self.refused.remove(&id.0);
        self.pending_pages.remove(&id.0);
        self.dirty = true;
    }
    pub fn page_branch(
        &mut self,
        id: EntryId,
        forward: bool,
    ) -> sortsize_store::inventory::Result<()> {
        if self.expanded.contains(&id.0)
            && self
                .branches
                .get(&id.0)
                .is_none_or(|branch| !branch.failed && !branch.order.ready())
        {
            self.pending_pages.insert(id.0, forward);
            self.dirty = true;
            return Ok(());
        }
        self.pending_pages.remove(&id.0);
        if let Some(branch) = self.branches.get_mut(&id.0)
            && let Some(page) = Self::page(branch)?
        {
            let next = if forward {
                page.next
                    .as_ref()
                    .map(|cursor| branch.order.page(Some(cursor)))
                    .transpose()?
            } else {
                page.first
                    .as_ref()
                    .map(|cursor| branch.order.previous(cursor))
                    .transpose()?
            };
            if let Some(next) = next
                && let Some(row) = next.rows.first()
            {
                branch.anchor = Some(row.key.entry);
                self.positions.insert(id.0, row.key.entry);
            }
        }
        self.dirty = true;
        Ok(())
    }
    pub fn reveal(
        &mut self,
        store: &Store,
        c: AcquisitionContext,
        selected: EntryId,
    ) -> sortsize_store::inventory::Result<()> {
        let mut id = selected;
        // Only ancestors of a requested selection are touched. A malformed or
        // over-budget chain never causes an unbounded walk or RAM allocation.
        for _ in 0..MAX_BRANCHES {
            let Some(parent) = store.navigation_parent(c.generation, id)? else {
                break;
            };
            if self.expanded.len() < MAX_BRANCHES {
                self.expanded.insert(parent.0);
            }
            let visible = self
                .branches
                .get(&parent.0)
                .map(Self::page)
                .transpose()?
                .flatten()
                .is_some_and(|page| page.rows.iter().any(|row| row.key.entry == id));
            if !visible {
                self.pending_pages.remove(&parent.0);
                self.positions.insert(parent.0, id);
                if let Some(branch) = self.branches.get_mut(&parent.0) {
                    branch.anchor = Some(id);
                }
            }
            id = parent;
        }
        self.dirty = true;
        Ok(())
    }
    pub fn has_work(&self) -> bool {
        self.branches
            .values()
            .any(|b| !b.failed && !b.order.ready())
    }
    pub fn advance(
        &mut self,
        store: &Store,
        c: AcquisitionContext,
        folded: bool,
        mut measure: impl FnMut(EntryId) -> sortsize_store::inventory::Result<Option<FolderMeasure>>,
    ) -> sortsize_store::inventory::Result<()> {
        self.synchronize(store, c, folded)?;
        let next = self
            .branches
            .range(self.round..)
            .find(|(_, b)| !b.failed && !b.order.ready())
            .map(|(id, _)| *id)
            .or_else(|| {
                self.branches
                    .iter()
                    .find(|(_, b)| !b.failed && !b.order.ready())
                    .map(|(id, _)| *id)
            });
        if let Some(id) = next {
            let branch = self.branches.get_mut(&id).unwrap();
            match branch.order.step(store, &mut measure) {
                Ok(_) => branch.started = true,
                Err(_) => branch.failed = true,
            }
            if branch.failed {
                self.pending_pages.remove(&id);
            } else if branch.order.ready()
                && let Some(forward) = self.pending_pages.remove(&id)
            {
                self.page_branch(EntryId(id), forward)?;
            }
            self.round = id.saturating_add(1);
            self.dirty = true;
        }
        Ok(())
    }
    fn synchronize(
        &mut self,
        store: &Store,
        c: AcquisitionContext,
        folded: bool,
    ) -> sortsize_store::inventory::Result<()> {
        let stamp = (c.generation, store.revision(c.generation)?, folded);
        if self.stamp != Some(stamp)
            || self.branches.iter().any(|(id, b)| {
                !b.order.matches(
                    store,
                    c,
                    EntryKey {
                        inventory: c.inventory,
                        generation: c.generation,
                        entry: EntryId(*id),
                    },
                )
            })
        {
            if self.stamp.is_some_and(|old| old.0 == c.generation) {
                self.retain_pages()?;
            } else {
                self.retained.clear();
                self.pending_pages.clear();
            }
            self.branches.clear();
            self.refused.clear();
            self.stamp = Some(stamp);
            self.dirty = true;
        }
        Ok(())
    }
    fn limit(rows: &mut Vec<TreeRow>, targets: &mut Vec<TreeTarget>, depth: usize) {
        let index = targets.len() as i32;
        targets.push(TreeTarget::Limit);
        rows.push(TreeRow {
            node_id: index,
            depth: depth as i32,
            name: tr!("Folder tree display limit reached. Collapse a branch to show more folders.")
                .into(),
            ..Default::default()
        });
    }
    #[allow(clippy::too_many_arguments)]
    pub fn rows(
        &mut self,
        store: &Store,
        c: AcquisitionContext,
        view: ViewContext,
        root: &Path,
        scratch: &Path,
        selected: EntryKey,
        provisional: bool,
        folded: bool,
        units: crate::units::Units,
        mut measure: impl FnMut(EntryId) -> sortsize_store::inventory::Result<Option<FolderMeasure>>,
    ) -> sortsize_store::inventory::Result<(
        Vec<TreeRow>,
        Vec<TreeTarget>,
        Option<sortsize_core::resources::Reservation>,
    )> {
        use sortsize_core::resources::{Category, Governor};
        if self._memory.is_none() {
            self._memory =
                Governor::global().reserve_up_to(Category::Queries, 32 * 1024, 1024 * 1024);
        }
        let Some(mut publication_memory) =
            Governor::global().reserve_up_to(Category::Publications, 64 * 1024, 8 * 1024 * 1024)
        else {
            let mut rows = Vec::new();
            let mut targets = Vec::new();
            Self::limit(&mut rows, &mut targets, 0);
            return Ok((rows, targets, None));
        };
        if self._memory.is_none() {
            let mut rows = Vec::new();
            let mut targets = Vec::new();
            Self::limit(&mut rows, &mut targets, 0);
            publication_memory.shrink_to(16 * 1024);
            return Ok((rows, targets, Some(publication_memory)));
        }
        let text_limit = (publication_memory.bytes() as usize / 3).min(2 * 1024 * 1024);
        let stack_limit = (publication_memory.bytes() as usize / 8).min(512 * 1024);
        self.synchronize(store, c, folded)?;
        let root_key = EntryKey {
            inventory: c.inventory,
            generation: c.generation,
            entry: EntryId(0),
        };
        let mut rows = Vec::new();
        let mut targets = Vec::new();
        let mut stack = vec![(
            root_key,
            None,
            root.to_owned(),
            0usize,
            measure(EntryId(0))?.and_then(|m| m.logical),
            0u8,
            false,
        )];
        let mut visible = BTreeSet::new();
        let mut text_bytes = 0usize;
        let mut stack_bytes = root.as_os_str().as_encoded_bytes().len() + 128;
        while let Some((key, parent, path, depth, ordered_logical, continuation, retained_metric)) =
            stack.pop()
        {
            stack_bytes =
                stack_bytes.saturating_sub(path.as_os_str().as_encoded_bytes().len() + 128);
            if continuation != 0 {
                if rows.len() < MAX_ROWS {
                    let index = targets.len() as i32;
                    targets.push(if continuation == 1 {
                        TreeTarget::More { parent: key }
                    } else {
                        TreeTarget::Previous { parent: key }
                    });
                    rows.push(TreeRow {
                        node_id: index,
                        depth: depth as i32,
                        name: if continuation == 1 {
                            tr!("Load more folders")
                        } else {
                            tr!("Previous folders")
                        }
                        .into(),
                        path: label(&path).into(),
                        ..Default::default()
                    });
                }
                continue;
            }
            if rows.len() >= MAX_ROWS - 1 {
                Self::limit(&mut rows, &mut targets, depth);
                break;
            }
            let cost = path.as_os_str().as_encoded_bytes().len().saturating_mul(8) + 2048;
            if text_bytes + cost > text_limit {
                Self::limit(&mut rows, &mut targets, depth);
                break;
            }
            text_bytes += cost;
            let own_measure = measure(key.entry)?;
            let logical = if parent.is_some() {
                ordered_logical
            } else {
                own_measure.and_then(|m| m.logical)
            };
            let parent_measure = parent.map(&mut measure).transpose()?.flatten();
            let parent_logical = parent_measure.and_then(|m| m.logical);
            let partial = provisional
                || retained_metric
                || own_measure.is_some_and(|m| m.unknown > 0)
                || parent_measure.is_some_and(|m| m.unknown > 0);
            let (fraction, has_weight, proportion) = crate::tree::folder_tree_weight(
                logical,
                if parent.is_none() {
                    logical
                } else {
                    parent_logical
                },
                partial,
            );
            let expandable =
                store.folder_has_children(c, key.entry)?.unwrap_or(true) || provisional;
            let expanded = self.expanded.contains(&key.entry.0);
            let index = targets.len() as i32;
            targets.push(TreeTarget::Folder {
                key,
                parent,
                path: Arc::new(path.clone()),
            });
            rows.push(TreeRow {
                node_id: index,
                depth: depth as i32,
                name: crate::tree::native_label(path.file_name().map(Path::new).unwrap_or(&path))
                    .into(),
                path: label(&path).into(),
                size: logical
                    .map_or_else(
                        || "—".into(),
                        |n| {
                            if partial {
                                format!("≥ {}", units.format(n))
                            } else {
                                units.format(n)
                            }
                        },
                    )
                    .into(),
                expandable,
                expanded,
                selected: key == selected,
                focused: false,
                fraction,
                has_weight,
                proportion,
                weight_provisional: partial,
                color_index: (key.entry.0 % 8) as i32,
            });
            if !expanded || !expandable || visible.len() >= MAX_BRANCHES {
                continue;
            }
            visible.insert(key.entry.0);
            if !self.branches.contains_key(&key.entry.0) {
                if self.refused.contains(&key.entry.0) {
                    Self::limit(&mut rows, &mut targets, depth + 1);
                    continue;
                }
                let order =
                    match TreeBranchOrder::start(store, c, view, key, root, scratch, ORDER_BYTES) {
                        Ok(order) => order,
                        Err(_) => {
                            self.refused.insert(key.entry.0);
                            Self::limit(&mut rows, &mut targets, depth + 1);
                            continue;
                        }
                    };
                self.branches.insert(
                    key.entry.0,
                    Branch {
                        order,
                        anchor: self.positions.get(&key.entry.0).copied(),
                        failed: false,
                        started: false,
                    },
                );
            }
            let branch = &self.branches[&key.entry.0];
            if branch.failed {
                let index = targets.len() as i32;
                targets.push(TreeTarget::Limit);
                rows.push(TreeRow {
                    node_id: index,
                    depth: (depth + 1) as i32,
                    name: tr!("This folder could not be expanded within the tree budget.").into(),
                    ..Default::default()
                });
                continue;
            }
            let page = Self::page(branch)?;
            let mut more = page.as_ref().is_some_and(|p| p.more);
            let previous = if branch.anchor.is_some() {
                page.as_ref()
                    .and_then(|p| p.first.as_ref())
                    .map(|cursor| branch.order.previous(cursor).map(|p| !p.rows.is_empty()))
                    .transpose()?
                    .unwrap_or(false)
            } else {
                false
            };
            let mut children = page.map(|p| p.rows).unwrap_or_default();
            let mut retained_children = false;
            if !branch.order.ready() {
                if let Some(old) = self
                    .retained
                    .get(&key.entry.0)
                    .filter(|old| old.len() > children.len())
                {
                    children = old.clone();
                    retained_children = true;
                    more = true;
                }
            } else {
                self.retained.remove(&key.entry.0);
            }
            // Continuations stay attached to their parent, and never replace
            // the primary table's independent pagination or selected folder.
            if more {
                stack_bytes += path.as_os_str().as_encoded_bytes().len() + 128;
                stack.push((key, None, path.clone(), depth + 1, None, 1u8, false));
            }
            // The displayed rows alone are bounded, even if SQLite indexes a
            // huge fratrie. Only admitted paths enter the publication.
            let mut limited = false;
            let mut admitted = Vec::new();
            let mut admitted_bytes = stack_bytes;
            for child in children {
                let bytes = child.path.as_os_str().as_encoded_bytes().len() + 128;
                if admitted_bytes + bytes > stack_limit
                    || admitted.len() + rows.len() + stack.len() >= MAX_ROWS - 2
                {
                    limited = true;
                    break;
                }
                admitted_bytes += bytes;
                admitted.push(child);
            }
            if limited {
                Self::limit(&mut rows, &mut targets, depth + 1);
            }
            for child in admitted.into_iter().rev() {
                let bytes = child.path.as_os_str().as_encoded_bytes().len() + 128;
                if stack_bytes + bytes > stack_limit {
                    continue;
                }
                stack_bytes += bytes;
                stack.push((
                    child.key,
                    Some(key.entry),
                    child.path,
                    depth + 1,
                    child.logical,
                    0u8,
                    retained_metric || retained_children,
                ));
            }
            if previous {
                stack_bytes += path.as_os_str().as_encoded_bytes().len() + 128;
                stack.push((key, None, path, depth + 1, None, 2u8, false));
            }
        }
        self.branches.retain(|id, _| visible.contains(id));
        self.retained.retain(|id, _| visible.contains(id));
        self.positions.retain(|id, _| self.expanded.contains(id));
        self.pending_pages.retain(|id, _| visible.contains(id));
        self.refused.retain(|id| visible.contains(id));
        self.dirty = false;
        let retained_bytes = text_bytes
            + rows.capacity() * std::mem::size_of::<TreeRow>() * 2
            + targets.capacity() * std::mem::size_of::<TreeTarget>() * 2
            + 16 * 1024;
        publication_memory.shrink_to((retained_bytes as u64).min(publication_memory.bytes()));
        Ok((rows, targets, Some(publication_memory)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_page_navigation_waits_for_replacement_order() {
        use sortsize_store::{
            Durability,
            inventory::{Kind, Observation},
        };
        let governor = sortsize_core::resources::Governor::global();
        governor.configure(MemoryMode::Entries50k, None);
        governor.refresh_native();
        let temp = tempfile::tempdir().unwrap();
        let c = AcquisitionContext {
            inventory: InventoryId(77),
            source: SourceId(88),
            generation: GenerationId(3),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let mut store = Store::create(
            &temp.path().join("store"),
            c.inventory,
            c.source,
            Durability::Reconstructible,
        )
        .unwrap();
        store.create_generation(c).unwrap();
        let attempt = AttemptKey {
            task: TaskKey {
                context: c,
                directory: EntryId(0),
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(1),
            owner: OwnerEpoch(1),
            branch: BranchRevision(1),
        };
        store.begin_attempt(attempt).unwrap();
        let observations = (0..231)
            .map(|index| {
                let name = format!("folder-{index:03}");
                #[cfg(unix)]
                let name = sortsize_store::codec::NativeName::Posix(name.into_bytes());
                #[cfg(windows)]
                let name =
                    sortsize_store::codec::NativeName::Windows(name.encode_utf16().collect());
                Observation {
                    name,
                    kind: Kind::Directory,
                    logical: None,
                    allocated: None,
                    modified: None,
                    metadata_known: true,
                }
            })
            .collect::<Vec<_>>();
        store
            .ingest(
                BatchHeader {
                    attempt,
                    sequence: 0,
                    end: None,
                },
                &observations,
            )
            .unwrap();
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
        let root = EntryKey {
            inventory: c.inventory,
            generation: c.generation,
            entry: EntryId(0),
        };
        let rows = |tree: &mut FolderTree| {
            tree.rows(
                &store,
                c,
                view,
                temp.path(),
                temp.path(),
                root,
                false,
                false,
                crate::units::Units::default(),
                |_| Ok(None),
            )
            .unwrap()
            .0
        };
        let mut tree = FolderTree::default();
        rows(&mut tree);
        for _ in 0..16 {
            if !tree.has_work() {
                break;
            }
            tree.advance(&store, c, false, |_| Ok(None)).unwrap();
        }
        assert!(!tree.has_work());
        assert!(
            rows(&mut tree)
                .iter()
                .any(|row| row.name == tr!("Load more folders"))
        );
        let mut replacement = tree.copy_navigation().unwrap();
        let retained = rows(&mut replacement);
        assert!(!replacement.branches[&0].started);
        assert!(retained.iter().any(|row| row.name == "folder-199"));
        assert!(
            retained
                .iter()
                .any(|row| row.name == tr!("Load more folders"))
        );
        replacement.page_branch(EntryId(0), true).unwrap();
        // Another snapshot handoff must preserve the accepted click too.
        let mut replacement = replacement.copy_navigation().unwrap();
        rows(&mut replacement);
        for _ in 0..16 {
            if !replacement.has_work() {
                break;
            }
            replacement.advance(&store, c, false, |_| Ok(None)).unwrap();
        }
        assert!(!replacement.has_work());
        assert!(replacement.pending_pages.is_empty());
        let next = rows(&mut replacement);
        assert!(next.iter().any(|row| row.name == "folder-230"));
        assert!(!next.iter().any(|row| row.name == "folder-000"));
        replacement.page_branch(EntryId(0), false).unwrap();
        assert!(
            rows(&mut replacement)
                .iter()
                .any(|row| row.name == "folder-000")
        );
        let mut waiting = replacement.copy_navigation().unwrap();
        waiting.page_branch(EntryId(0), true).unwrap();
        assert_eq!(waiting.pending_pages.len(), 1);
        waiting.toggle(EntryId(0));
        assert!(waiting.pending_pages.is_empty());
        waiting.page_branch(EntryId(0), true).unwrap();
        assert!(waiting.pending_pages.is_empty());
    }
}
