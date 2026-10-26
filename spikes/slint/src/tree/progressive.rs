//! Worker-prepared patches. Native paths/names are allocated once on admission
//! and shared with the UI. No inventory snapshot or path clone per refresh.
use super::*;
use sortsize_core::{InventoryPreview, PreviewAggregate};

const MAX_ADDITIONS: usize = 2_048;

#[derive(Default)]
pub(crate) struct Projector {
    nodes: Vec<Node>,
    children: Vec<Vec<usize>>,
    aggregates: Vec<PreviewAggregate>,
}

pub(crate) struct TreePatch {
    start: usize,
    added: Vec<Node>,
    changed: Vec<(usize, NodeUpdate)>,
    truncated: bool,
}
struct NodeUpdate {
    bytes: Option<u64>,
    counts: Option<DirectoryCounts>,
    children: Option<Arc<ChildOrders>>,
    map: Option<Arc<Projection>>,
    directory_count: usize,
}
impl Projector {
    pub(crate) fn patch(&mut self, preview: InventoryPreview<'_>) -> TreePatch {
        preview.aggregate_into(&mut self.aggregates);
        let start = self.nodes.len();
        let end = preview.entries.len().min(start + MAX_ADDITIONS);
        let mut dirty = vec![false; end];
        for (index, entry) in preview.entries.iter().enumerate().take(end).skip(start) {
            let name = if entry.parent.is_none() {
                entry.path.as_os_str()
            } else {
                entry.path.file_name().unwrap_or(entry.path.as_os_str())
            };
            self.nodes.push(Node {
                name: Arc::from(display_label(&name.to_string_lossy())),
                path: Arc::new(entry.path.clone()),
                plain_path: entry
                    .path
                    .to_str()
                    .is_some_and(|p| !p.chars().any(char::is_control)),
                map: None,
                parent: entry.parent,
                bytes: entry.logical_bytes,
                kind: entry.kind,
                directory: entry.kind == InventoryKind::Directory,
                incomplete: entry.kind == InventoryKind::Directory || entry.incomplete,
                counts: entry.counts,
                expanded: entry.parent.is_none(),
                directory_count: 0,
                children: None,
            });
            self.children.push(Vec::new());
            if let Some(parent) = entry.parent {
                self.children[parent].push(index);
                dirty[parent] = true;
            }
            dirty[index] = entry.kind == InventoryKind::Directory;
        }
        for index in 0..end {
            let aggregate = self.aggregates[index];
            let node = &mut self.nodes[index];
            if node.bytes != aggregate.logical_bytes || node.counts != aggregate.counts {
                node.bytes = aggregate.logical_bytes;
                node.counts = aggregate.counts;
                dirty[index] |= node.directory;
                if let Some(parent) = node.parent {
                    dirty[parent] = true;
                }
            }
        }
        // Unpublished nodes are omissions from this preview, even if retained
        // by the engine. Their bytes must never be attributed to a visible child.
        let truncated = preview.entries_omitted > 0 || end < preview.entries.len();
        let mut changed = Vec::new();
        for (index, dirty) in dirty.into_iter().enumerate() {
            if !dirty {
                continue;
            }
            Tree::prepare_children(&mut self.nodes, index, self.children[index].clone(), None);
            let node = &self.nodes[index];
            let projection = Projection::new(
                self.children[index]
                    .iter()
                    .map(|&i| (i, self.nodes[i].bytes, self.nodes[i].incomplete)),
                node.bytes,
                true,
                truncated,
            );
            self.nodes[index].map = Some(Arc::new(projection));
            let node = &self.nodes[index];
            changed.push((
                index,
                NodeUpdate {
                    bytes: node.bytes,
                    counts: node.counts,
                    children: node.children.clone(),
                    map: node.map.clone(),
                    directory_count: node.directory_count,
                },
            ));
        }
        TreePatch {
            start,
            added: self.nodes[start..end].to_vec(),
            changed,
            truncated,
        }
    }
}

impl Tree {
    pub(crate) fn preview() -> Self {
        Self {
            nodes: Vec::new(),
            dates: None,
            allocation_values: None,
            units: Units::Binary,
            allocation_not_collected: tr!("Not collected").into(),
            allocation_sort_available: false,
            owner_data: None,
            owner_orders: None,
            owner_native_orders: None,
            owner_not_collected: "—".into(),
            owner_absent_labels: None,
            #[cfg(feature = "benchmarks")]
            owner_prepare_ms: 0.0,
            #[cfg(feature = "benchmarks")]
            date_prepare_ms: 0.0,
            top_files: Vec::new(),
            top_rows: Vec::new(),
            top_note: tr!("Scan in progress…").into(),
            top_oversized_paths: 0,
            presentation: crate::presentation::Context::current(),
            roots: Vec::new(),
            offset: 0,
            detail_offset: 0,
            selected: None,
            sort: 0,
            truncated: false,
            coverage: Coverage::Partial,
        }
    }
    pub(crate) fn apply_patch(&mut self, patch: TreePatch) {
        // A patch belongs to one ordered stream. Independent task mailboxes
        // prevent old generations from writing into the current tree.
        assert_eq!(self.nodes.len(), patch.start);
        for (index, node) in patch.added.into_iter().enumerate() {
            if node.parent.is_none() {
                self.roots.push(patch.start + index);
            }
            self.nodes.push(node);
        }
        for (index, update) in patch.changed {
            let node = &mut self.nodes[index];
            node.bytes = update.bytes;
            node.counts = update.counts;
            node.children = update.children;
            node.map = update.map;
            node.directory_count = update.directory_count;
        }
        self.truncated = patch.truncated;
        self.selected = self.selected.or_else(|| self.roots.first().copied());
    }
    pub(crate) fn keep_navigation(&mut self, previous: &Self) {
        for (current, old) in self.nodes.iter_mut().zip(&previous.nodes) {
            if current.path == old.path {
                current.expanded = old.expanded;
            }
        }
        if let Some(index) = previous.selected
            && self
                .nodes
                .get(index)
                .zip(previous.nodes.get(index))
                .is_some_and(|(a, b)| a.path == b.path)
        {
            self.selected = Some(index);
            self.offset = previous.offset;
            self.detail_offset = previous.detail_offset;
            self.sort = previous.sort;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::*;

    #[test]
    fn patches_admit_paths_once_preserve_selection_and_match_final_totals() {
        let fixture = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        for i in 0..2_110 {
            std::fs::write(fixture.path().join("child").join(i.to_string()), [0; 3]).unwrap();
        }
        let mut projector = Projector::default();
        let mut tree = Tree::preview();
        let mut last_count = 0;
        let mut pointer = None;
        let result = scan_inventory_progressive(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            ProgressiveInventoryOptions::default(),
            |_, preview| {
                // Exercise several worker/UI handoffs without wall-clock sleeps.
                if preview.entries.len() == 1 || preview.entries.len() >= last_count + 500 {
                    let patch = projector.patch(preview);
                    assert!(patch.added.len() <= MAX_ADDITIONS);
                    tree.apply_patch(patch);
                    last_count = tree.nodes.len();
                    if tree.nodes.len() > 1 {
                        tree.select(1);
                    }
                    let path = Arc::as_ptr(&tree.nodes[0].path);
                    if let Some(previous) = pointer {
                        assert_eq!(path, previous);
                    }
                    pointer = Some(path);
                    assert!(tree.nodes[0].size(Units::Binary).starts_with("≥ "));
                }
            },
        )
        .unwrap();
        let selected = tree.selected;
        let (mut final_tree, report) = Tree::new(result.collected.metadata.inventory);
        final_tree.keep_navigation(&tree);
        assert_eq!(final_tree.selected, selected);
        assert_eq!(report.logical_bytes, 6_330);
        assert_eq!(final_tree.nodes[0].bytes, Some(6_330));
        assert!(!final_tree.nodes[0].incomplete);
        assert!(tree.nodes[0].incomplete);
        assert_eq!(final_tree.nodes[1].path, tree.nodes[1].path);
    }
}
