use crate::{DetailRow, TreeRow};
use sortsize_core::{Coverage, InventoryKind, InventoryReport, ScanReport};

pub(crate) const MAX_VISIBLE_ROWS: usize = 2_000;

/// Orders and labels are prepared on the worker, never sorted on the UI thread.
pub(crate) struct Tree {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    offset: usize,
    detail_offset: usize,
    selected: Option<usize>,
    sort: usize,
    truncated: bool,
    coverage: Coverage,
}
struct Node {
    name: Box<str>,
    // Display only; moved from the inventory, never used as an operation target.
    path: Box<str>,
    parent: Option<usize>,
    bytes: Option<u64>,
    kind: InventoryKind,
    directory: bool,
    incomplete: bool,
    expanded: bool,
    directory_count: usize,
    // Only nonempty parents own orders; leaf files have no four empty Vecs.
    children: Option<Box<[Box<[usize]>; 4]>>,
}
impl Node {
    fn size(&self) -> String {
        match self.bytes {
            Some(bytes) if self.incomplete => format!("≥ {}", format_bytes(bytes)),
            Some(bytes) => format_bytes(bytes),
            None => "—".into(),
        }
    }
    fn note(&self) -> &'static str {
        match self.kind {
            InventoryKind::Directory if self.incomplete => "Dossier partiel",
            InventoryKind::Directory => "Dossier",
            InventoryKind::File => "Fichier",
            InventoryKind::Symlink => "Lien ignoré",
            InventoryKind::Special => "Entrée spéciale ignorée",
        }
    }
}
impl Tree {
    /// Consume paths rather than keeping inventory and display copies alive.
    pub(crate) fn new(inventory: InventoryReport) -> (Self, ScanReport) {
        let truncated = inventory.entries_omitted > 0;
        let coverage = inventory.report.coverage;
        let mut nodes: Vec<_> = inventory
            .entries
            .into_iter()
            .map(|entry| {
                let name = if entry.parent.is_none() {
                    entry.path.as_os_str()
                } else {
                    entry.path.file_name().unwrap_or(entry.path.as_os_str())
                };
                let name = display_label(&name.to_string_lossy()).into_boxed_str();
                let mut path = entry
                    .path
                    .into_os_string()
                    .into_string()
                    .unwrap_or_else(|path| path.to_string_lossy().into_owned());
                if path.starts_with("\\\\?\\UNC\\") {
                    path.replace_range(..8, "\\\\");
                } else if path.starts_with("\\\\?\\") {
                    path.drain(..4);
                }
                if path.chars().any(char::is_control) {
                    path = display_label(&path);
                }
                Node {
                    name,
                    path: path.into_boxed_str(),
                    parent: entry.parent,
                    bytes: entry.logical_bytes,
                    kind: entry.kind,
                    directory: entry.kind == InventoryKind::Directory,
                    incomplete: entry.incomplete,
                    expanded: entry.parent.is_none(),
                    directory_count: 0,
                    children: None,
                }
            })
            .collect();
        let mut roots = Vec::new();
        let mut children = vec![Vec::new(); nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                children[parent].push(index);
            } else {
                roots.push(index);
            }
        }
        for (index, siblings) in children.into_iter().enumerate() {
            if siblings.is_empty() {
                continue;
            }
            let orders = std::array::from_fn(|order| {
                let mut sorted = siblings.clone();
                sorted.sort_unstable_by(|left, right| {
                    let a = &nodes[*left];
                    let b = &nodes[*right];
                    let metric = match order {
                        0 | 1 => a
                            .bytes
                            .is_none()
                            .cmp(&b.bytes.is_none())
                            .then_with(|| {
                                if order == 0 {
                                    b.bytes.cmp(&a.bytes)
                                } else {
                                    a.bytes.cmp(&b.bytes)
                                }
                            })
                            .then_with(|| a.name.cmp(&b.name)),
                        2 => a.name.cmp(&b.name),
                        _ => b.name.cmp(&a.name),
                    };
                    b.directory
                        .cmp(&a.directory)
                        .then(metric)
                        .then_with(|| left.cmp(right))
                });
                sorted.into_boxed_slice()
            });
            nodes[index].directory_count = orders[0]
                .iter()
                .take_while(|&&i| nodes[i].directory)
                .count();
            nodes[index].children = Some(Box::new(orders));
        }
        let selected = roots.first().copied();
        (
            Self {
                nodes,
                roots,
                offset: 0,
                detail_offset: 0,
                selected,
                sort: 0,
                truncated,
                coverage,
            },
            inventory.report,
        )
    }
    fn children(&self, index: usize, sort: usize) -> &[usize] {
        self.nodes[index]
            .children
            .as_ref()
            .map_or(&[], |orders| orders[sort].as_ref())
    }
    fn proportion(&self, index: usize) -> (String, f32) {
        let node = &self.nodes[index];
        let value = node.parent.and_then(|parent| {
            let parent = &self.nodes[parent];
            let total = parent.bytes?;
            let bytes = node.bytes?;
            if total == 0 {
                return None;
            }
            let fraction = (bytes as f64 / total as f64).clamp(0.0, 1.0);
            let prefix = if parent.incomplete { "≈ " } else { "" };
            let label = if bytes > 0 && fraction < 0.0005 {
                format!("{prefix}< 0.1 %")
            } else {
                format!("{prefix}{:.1} %", fraction * 100.0)
            };
            Some((label, fraction as f32))
        });
        value.unwrap_or_else(|| ("—".into(), 0.0))
    }
    pub(crate) fn toggle(&mut self, index: usize) {
        if let Some(node) = self.nodes.get_mut(index)
            && node.directory_count > 0
        {
            node.expanded = !node.expanded;
            self.offset = 0; // A collapse cannot strand the view on a blank last page.
        }
    }
    pub(crate) fn select(&mut self, index: usize) {
        if !self.nodes.get(index).is_some_and(|n| n.directory) {
            return;
        }
        self.selected = Some(index);
        self.detail_offset = 0;
        let mut parent = self.nodes[index].parent;
        while let Some(index) = parent {
            self.nodes[index].expanded = true;
            parent = self.nodes[index].parent;
        }
        let position = self
            .visible_directories()
            .iter()
            .position(|(i, _)| *i == index)
            .unwrap_or(0);
        self.offset = position / MAX_VISIBLE_ROWS * MAX_VISIBLE_ROWS;
    }
    pub(crate) fn parent(&mut self) {
        if let Some(parent) = self.selected.and_then(|i| self.nodes[i].parent) {
            self.select(parent);
        }
    }
    pub(crate) fn has_parent(&self) -> bool {
        self.selected
            .is_some_and(|i| self.nodes[i].parent.is_some())
    }
    pub(crate) fn path(&self) -> &str {
        self.selected.map_or("", |i| self.nodes[i].path.as_ref())
    }
    pub(crate) fn entry_path(&self, index: usize) -> Option<&str> {
        self.selected
            .filter(|p| self.nodes.get(index).is_some_and(|n| n.parent == Some(*p)))
            .map(|_| self.nodes[index].path.as_ref())
    }
    pub(crate) fn selection_summary(&self) -> String {
        self.selected.map_or_else(String::new, |i| {
            let node = &self.nodes[i];
            format!(
                "{} · {} entrées retenues{}{}",
                node.size(),
                self.children(i, 0).len(),
                if node.incomplete {
                    " · analyse partielle"
                } else {
                    ""
                },
                if self.truncated {
                    " · inventaire limité"
                } else {
                    ""
                }
            )
        })
    }
    pub(crate) fn empty_message(&self) -> &'static str {
        if self.selected.is_none() {
            match self.coverage {
                Coverage::Cancelled => {
                    "Analyse annulée avant de retenir une entrée ; aucun inventaire disponible."
                }
                Coverage::Partial => "Aucune entrée accessible ; l’analyse est partielle.",
                Coverage::Complete => "Aucune entrée retenue dans le périmètre analysé.",
            }
        } else if self.truncated {
            "Aucune entrée retenue pour ce dossier ; l’inventaire est limité."
        } else if self.selected.is_some_and(|i| self.nodes[i].incomplete) {
            "Aucune entrée accessible ; l’analyse de ce dossier est partielle."
        } else {
            "Ce dossier est vide dans le périmètre analysé."
        }
    }
    fn visible_directories(&self) -> Vec<(usize, i32)> {
        let mut visible = Vec::new();
        let mut stack = vec![(self.roots.as_slice(), 0, 0i32)];
        while let Some((siblings, cursor, depth)) = stack.last_mut() {
            let Some(&index) = siblings.get(*cursor) else {
                stack.pop();
                continue;
            };
            *cursor += 1;
            let depth = *depth;
            let node = &self.nodes[index];
            visible.push((index, depth));
            if node.expanded && node.directory_count > 0 {
                stack.push((
                    &node.children.as_ref().unwrap()[0][..node.directory_count],
                    0,
                    depth + 1,
                ));
            }
        }
        visible
    }
    pub(crate) fn rows(&self) -> (Vec<TreeRow>, bool) {
        let visible = self.visible_directories();
        let rows = visible
            .iter()
            .skip(self.offset)
            .take(MAX_VISIBLE_ROWS)
            .map(|&(index, depth)| {
                let node = &self.nodes[index];
                TreeRow {
                    node_id: index as i32,
                    depth: depth.min(12),
                    name: node.name.as_ref().into(),
                    size: node.size().into(),
                    expandable: node.directory_count > 0,
                    expanded: node.expanded,
                    selected: self.selected == Some(index),
                }
            })
            .collect();
        (rows, visible.len() > self.offset + MAX_VISIBLE_ROWS)
    }
    pub(crate) fn details(&self) -> (Vec<DetailRow>, bool) {
        let Some(index) = self.selected else {
            return (Vec::new(), false);
        };
        let children = self.children(index, self.sort);
        let rows = children
            .iter()
            .skip(self.detail_offset)
            .take(MAX_VISIBLE_ROWS)
            .map(|&index| {
                let node = &self.nodes[index];
                let (proportion, fraction) = self.proportion(index);
                DetailRow {
                    node_id: index as i32,
                    name: node.name.as_ref().into(),
                    size: node.size().into(),
                    note: node.note().into(),
                    directory: node.directory,
                    proportion: proportion.into(),
                    fraction,
                }
            })
            .collect();
        (rows, children.len() > self.detail_offset + MAX_VISIBLE_ROWS)
    }
    pub(crate) fn sort_by(&mut self, name: bool) {
        self.sort = match (name, self.sort) {
            (true, 2) => 3,
            (true, _) => 2,
            (false, 0) => 1,
            (false, _) => 0,
        };
        self.detail_offset = 0;
    }
    pub(crate) fn sort(&self) -> usize {
        self.sort
    }
    pub(crate) fn change_page(&mut self, details: bool, next: bool) {
        let limited = if details {
            self.details().1
        } else {
            self.rows().1
        };
        let offset = if details {
            &mut self.detail_offset
        } else {
            &mut self.offset
        };
        if next && limited {
            *offset += MAX_VISIBLE_ROWS;
        } else if !next {
            *offset = offset.saturating_sub(MAX_VISIBLE_ROWS);
        }
    }
    pub(crate) fn offset(&self, details: bool) -> usize {
        if details {
            self.detail_offset
        } else {
            self.offset
        }
    }
}
pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} octets");
    }
    let units = ["Kio", "Mio", "Gio", "Tio", "Pio", "Eio"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}
fn display_label(value: &str) -> String {
    let value = if let Some(unc) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        value.strip_prefix("\\\\?\\").unwrap_or(value).to_owned()
    };
    // Append characters directly instead of allocating a String per character.
    // Long native paths otherwise cause millions of transient allocations.
    let mut label = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() {
            label.extend(ch.escape_default());
        } else {
            label.push(ch);
        }
    }
    label
}
#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::{CancellationToken, ScanOptions, scan_inventory};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn labels_preserve_unicode_and_windows_prefixes_while_escaping_controls() {
        assert_eq!(display_label("été/日本語\nfile\t"), "été/日本語\\nfile\\t");
        assert_eq!(
            display_label("\\\\?\\UNC\\server\\share"),
            "\\\\server\\share"
        );
        assert_eq!(display_label("\\\\?\\C:\\été"), "C:\\été");
    }
    fn scan(root: &std::path::Path, limit: usize) -> Tree {
        Tree::new(
            scan_inventory(
                root,
                &ScanOptions::default(),
                &CancellationToken::default(),
                limit,
                |_| {},
            )
            .unwrap(),
        )
        .0
    }
    #[test]
    fn navigation_sort_and_proportions_share_one_inventory() {
        let fixture = TempDir::new().unwrap();
        fs::create_dir_all(fixture.path().join("folder/nested")).unwrap();
        fs::write(fixture.path().join("folder/nested/child"), b"abc").unwrap();
        fs::write(fixture.path().join("small"), b"1").unwrap();
        fs::write(fixture.path().join("large"), b"12345").unwrap();
        let mut tree = scan(fixture.path(), 100);
        let (rows, _) = tree.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].size, "9 octets");
        let folder = rows[1].node_id as usize;
        assert!(!rows[1].expanded);
        let (details, _) = tree.details();
        assert_eq!(
            details.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["folder", "large", "small"]
        );
        assert_eq!(details[1].proportion, "55.6 %");
        tree.sort_by(false);
        assert_eq!(tree.details().0[1].name, "small");
        tree.sort_by(true);
        assert_eq!(tree.details().0[1].name, "large");
        tree.sort_by(true);
        assert_eq!(tree.details().0[1].name, "small");
        tree.select(folder);
        assert!(!tree.rows().0[1].expanded);
        let nested = tree.details().0[0].node_id as usize;
        tree.select(nested);
        assert!(tree.rows().0[1].expanded);
        assert!(
            tree.rows()
                .0
                .iter()
                .any(|r| r.selected && r.node_id == nested as i32)
        );
        assert_eq!(tree.details().0[0].name, "child");
        assert_eq!(tree.details().0[0].proportion, "100.0 %");
        assert!(tree.has_parent());
        tree.parent();
        assert!(tree.path().ends_with("folder"));
        tree.parent();
        assert!(!tree.has_parent());
        tree.select(usize::MAX);
        tree.select(details[1].node_id as usize);
        assert!(!tree.has_parent());
    }
    #[test]
    fn independent_pages_and_collapse_never_leave_a_blank_page() {
        let fixture = TempDir::new().unwrap();
        for index in 0..MAX_VISIBLE_ROWS {
            fs::create_dir(fixture.path().join(index.to_string())).unwrap();
        }
        let mut tree = scan(fixture.path(), MAX_VISIBLE_ROWS + 1);
        assert!(tree.rows().1);
        assert!(!tree.details().1);
        tree.change_page(false, true);
        assert_eq!(tree.rows().0.len(), 1);
        assert_eq!(tree.offset(true), 0);
        tree.toggle(0);
        assert_eq!(tree.offset(false), 0);
        assert_eq!(tree.rows().0.len(), 1);
        tree.toggle(0);
        tree.select(tree.rows().0[1].node_id as usize);
        assert!(tree.details().0.is_empty());
        tree.parent();
        assert_eq!(tree.offset(true), 0);
        fs::write(fixture.path().join("another"), b"x").unwrap();
        let mut tree = scan(fixture.path(), MAX_VISIBLE_ROWS + 2);
        tree.change_page(true, true);
        assert_eq!(tree.details().0.len(), 1);
        tree.sort_by(true);
        assert_eq!(tree.offset(true), 0);
        tree.change_page(true, true);
        tree.change_page(true, false);
        assert_eq!(tree.offset(true), 0);
    }
    #[test]
    fn zero_unknown_partial_and_omitted_entries_are_explicit() {
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("empty"), []).unwrap();
        let tree = scan(fixture.path(), 10);
        assert_eq!(tree.details().0[0].proportion, "—");
        let tree = scan(fixture.path(), 1);
        assert!(tree.details().0.is_empty());
        assert!(tree.empty_message().contains("limité"));
        fs::write(fixture.path().join("data"), b"123").unwrap();
        let mut inventory = scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            10,
            |_| {},
        )
        .unwrap();
        inventory.entries[0].incomplete = true;
        inventory.entries[1].logical_bytes = None;
        let tree = Tree::new(inventory).0;
        let details = tree.details().0;
        assert!(details.iter().any(|r| r.proportion.starts_with("≈ ")));
        assert!(details.iter().any(|r| r.proportion == "—"));
        assert!(tree.selection_summary().contains("partielle"));
    }

    #[test]
    fn cancellation_before_the_first_entry_does_not_look_like_an_unstarted_scan() {
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("data"), b"123").unwrap();
        let token = CancellationToken::default();
        token.cancel();
        let inventory =
            scan_inventory(fixture.path(), &ScanOptions::default(), &token, 10, |_| {}).unwrap();
        let mut tree = Tree::new(inventory).0;
        assert!(tree.rows().0.is_empty());
        assert!(tree.details().0.is_empty());
        assert!(tree.empty_message().contains("annulée"));
        assert!(tree.empty_message().contains("aucun inventaire"));
        assert!(!tree.has_parent());
        tree.parent();
        tree.change_page(false, true);
        tree.change_page(true, true);
        assert_eq!(tree.offset(false), 0);
        assert_eq!(tree.offset(true), 0);
        assert!(tree.path().is_empty());
    }
}
