use crate::treemap::{self, Projection, Target};
use crate::units::Units;
use crate::{DetailRow, MapTile, TopRow, TreeRow};
use sortsize_core::Metadata;
use sortsize_core::{Coverage, DirectoryCounts, InventoryKind, InventoryReport, ScanReport};
use sortsize_core::{EntryDates, InventoryWithAllocationTotals, InventoryWithDates, MetadataValue};
use std::path::PathBuf;

pub(crate) const MAX_VISIBLE_ROWS: usize = 2_000;

/// Orders are prepared on the worker, never sorted on the UI thread.
pub(crate) struct Tree {
    nodes: Vec<Node>,
    // Indices are inventory/node identities, never positions in a sorted page.
    dates: Option<Vec<EntryDates>>,
    // Compact raw presentation values, indexed by native node identity. Only
    // the current bounded page is formatted; no filesystem access or re-sort.
    allocation_values: Option<Vec<MetadataValue<u64>>>,
    pub(crate) units: Units,
    allocation_not_collected: slint::SharedString,
    allocation_sort_available: bool,
    owner_data: Option<std::sync::Arc<crate::owner_view::OwnerData>>,
    owner_orders: Option<std::sync::Arc<crate::owner_view::OwnerOrders>>,
    owner_native_orders: Option<std::sync::Arc<crate::owner_view::OwnerOrders>>,
    owner_not_collected: slint::SharedString,
    owner_absent_labels: Option<Box<[slint::SharedString; 2]>>,
    #[cfg(feature = "benchmarks")]
    pub(crate) owner_prepare_ms: f64,
    #[cfg(feature = "benchmarks")]
    pub(crate) date_prepare_ms: f64,
    top_files: Vec<sortsize_core::TopFile>,
    pub(crate) top_rows: Vec<TopRow>,
    pub(crate) top_note: String,
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
    // Native inventory identity is retained; only presentation uses lossy labels.
    path: PathBuf,
    // Classified once on the worker, without retaining a second full label.
    // Rechecking every character of 2,000 long paths would delay UI callbacks.
    plain_path: bool,
    map: Option<Box<Projection>>,
    parent: Option<usize>,
    bytes: Option<u64>,
    kind: InventoryKind,
    directory: bool,
    incomplete: bool,
    counts: Option<DirectoryCounts>,
    expanded: bool,
    directory_count: usize,
    // Only nonempty parents own orders; no per-leaf sort storage.
    children: Option<Box<ChildOrders>>,
}
struct ChildOrders {
    standard: [Box<[usize]>; 4],
    // Opt-in: two node-index arrays, no copied paths.
    allocated: Option<Box<[Box<[usize]>; 2]>>,
    // Four additional node-index orders, built once after dates are attached.
    temporal: Option<Box<[Box<[usize]>; 4]>>,
}
impl Node {
    fn path_label(&self) -> slint::SharedString {
        match self.path.to_str() {
            Some(value) if self.plain_path => value.into(),
            _ => native_label(&self.path).into(),
        }
    }
    fn size(&self, units: Units) -> String {
        match self.bytes {
            Some(bytes) if self.incomplete => format!("≥ {}", units.format(bytes)),
            Some(bytes) => units.format(bytes),
            None => "—".into(),
        }
    }
    fn count(&self, value: impl FnOnce(DirectoryCounts) -> u64) -> String {
        self.counts.map_or_else(
            || "—".into(),
            |counts| {
                let prefix = if self.incomplete { "≥ " } else { "" };
                format!("{prefix}{}", value(counts))
            },
        )
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
    pub(crate) fn attach_owners(
        &mut self,
        owners: Vec<sortsize_core::EntryOwner>,
    ) -> Result<(), &'static str> {
        #[cfg(feature = "benchmarks")]
        let started = std::time::Instant::now();
        if owners.len() != self.nodes.len() {
            return Err("Propriétaires et inventaire incohérents");
        }
        let mut children = Vec::with_capacity(self.nodes.len());
        let ranges = self
            .nodes
            .iter()
            .map(|node| {
                let start = children.len();
                if let Some(orders) = &node.children {
                    children.extend_from_slice(&orders.standard[0]);
                }
                start..children.len()
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let data = std::sync::Arc::new(crate::owner_view::OwnerData {
            owners: owners.into_boxed_slice(),
            ranges,
            children: children.into_boxed_slice(),
            directories: self
                .nodes
                .iter()
                .map(|node| node.directory)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        });
        self.owner_orders = Some(std::sync::Arc::new(crate::owner_view::prepare_orders(
            &data,
            &[],
        )));
        self.owner_native_orders = self.owner_orders.clone();
        self.owner_data = Some(data);
        self.owner_absent_labels = Some(Box::new(["Accès refusé".into(), "Indisponible".into()]));
        #[cfg(feature = "benchmarks")]
        {
            self.owner_prepare_ms = started.elapsed().as_secs_f64() * 1000.0;
        }
        Ok(())
    }
    pub(crate) fn owner_data(&self) -> Option<std::sync::Arc<crate::owner_view::OwnerData>> {
        self.owner_data.clone()
    }
    pub(crate) fn owner_page_key(&self) -> (Option<usize>, usize, usize) {
        (self.selected, self.detail_offset, self.sort)
    }
    pub(crate) fn reset_owner_orders(&mut self) {
        self.owner_orders = self.owner_native_orders.clone();
    }
    pub(crate) fn owner_id(&self, index: usize) -> Option<sortsize_core::NativeOwnerId> {
        match self.owner_data.as_ref()?.owners.get(index)?.id {
            MetadataValue::Known { value, .. } => Some(value),
            _ => None,
        }
    }
    fn owner_label(
        &self,
        index: usize,
        labels: &[crate::owner_view::DisplayOwner],
    ) -> slint::SharedString {
        let Some(data) = &self.owner_data else {
            return self.owner_not_collected.clone();
        };
        match data.owners[index].id {
            MetadataValue::Known { value, .. } => crate::owner_view::label_for(labels, value)
                .cloned()
                .unwrap_or_else(|| crate::owner_view::fallback(data.owners[index].id)),
            MetadataValue::NotCollected => self.owner_not_collected.clone(),
            MetadataValue::AccessDenied => self.owner_absent_labels.as_ref().unwrap()[0].clone(),
            _ => self.owner_absent_labels.as_ref().unwrap()[1].clone(),
        }
    }
    fn owner_page_labels(&self, children: &[usize]) -> Vec<crate::owner_view::DisplayOwner> {
        if self.owner_data.is_none() {
            return Vec::new();
        }
        // Deduplicate this bounded page, never format/copy 50,000 owner names.
        let mut ids: Vec<_> = children
            .iter()
            .filter_map(|&node| self.owner_id(node))
            .collect();
        ids.sort_unstable_by(|a, b| crate::owner_view::compare_id(*a, *b));
        ids.dedup();
        ids.into_iter()
            .map(|id| crate::owner_view::DisplayOwner {
                id,
                label: crate::owner_view::fallback(MetadataValue::Known {
                    value: id,
                    accuracy: sortsize_core::ValueAccuracy::Exact,
                }),
            })
            .collect()
    }
    pub(crate) fn sort_owner(
        &mut self,
        orders: Option<std::sync::Arc<crate::owner_view::OwnerOrders>>,
        direction: usize,
    ) {
        if self.owner_data.is_some() {
            if let Some(orders) = orders {
                self.owner_orders = Some(orders);
            }
            self.sort = 6 + direction;
            self.detail_offset = 0;
        }
    }
    /// Reject a broken association before consuming either side. Values move
    /// unchanged; labels are produced only for the current details page.
    pub(crate) fn with_dates(
        input: InventoryWithDates,
    ) -> Result<(Self, ScanReport), &'static str> {
        if input.dates.len() != input.inventory.entries.len() {
            return Err("Dates et inventaire incohérents : nombres d’entrées différents");
        }
        let (mut tree, report) = Self::new(input.inventory);
        tree.attach_dates(input.dates);
        Ok((tree, report))
    }
    /// Validate all parallel tables before consuming paths. Retain only value
    /// and accuracy, without coverage/identities/links or copied native paths.
    pub(crate) fn with_allocations(
        input: InventoryWithAllocationTotals,
    ) -> Result<(Self, ScanReport), &'static str> {
        let count = input.metadata.inventory.entries.len();
        if input.entries_by_path.len() != count
            || input.metadata.dates.len() != count
            || (!input.metadata.allocations.is_empty() && input.metadata.allocations.len() != count)
        {
            return Err(
                "Allocations, dates et inventaire incohérents : nombres d’entrées différents",
            );
        }
        let (mut tree, report) =
            Self::build(input.metadata.inventory, Some(&input.entries_by_path));
        tree.attach_dates(input.metadata.dates);
        tree.allocation_values = Some(input.entries_by_path.into_iter().map(|m| m.value).collect());
        Ok((tree, report))
    }
    /// Worker-only preparation from the aligned table. No metadata acquisition,
    /// string formatting, copied native paths or dates stored in Node.
    fn attach_dates(&mut self, dates: Vec<EntryDates>) {
        #[cfg(feature = "benchmarks")]
        let started = std::time::Instant::now();
        self.dates = Some(dates);
        let dates = self.dates.as_ref().unwrap();
        for index in 0..self.nodes.len() {
            let Some(children) = &self.nodes[index].children else {
                continue;
            };
            let orders = std::array::from_fn(|order| {
                let mut sorted = children.standard[0].to_vec();
                sorted.sort_unstable_by(|&left, &right| {
                    let a = &self.nodes[left];
                    let b = &self.nodes[right];
                    let instant = |i: usize| {
                        let value = if order < 2 {
                            dates[i].modified
                        } else {
                            dates[i].accessed
                        };
                        match value {
                            MetadataValue::Known { value, .. } => Some(value),
                            _ => None,
                        }
                    };
                    let (av, bv) = (instant(left), instant(right));
                    b.directory
                        .cmp(&a.directory)
                        .then_with(|| av.is_none().cmp(&bv.is_none()))
                        .then_with(|| {
                            if order % 2 == 0 {
                                bv.cmp(&av)
                            } else {
                                av.cmp(&bv)
                            }
                        })
                        .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
                        .then_with(|| left.cmp(&right))
                });
                sorted.into_boxed_slice()
            });
            self.nodes[index].children.as_mut().unwrap().temporal = Some(Box::new(orders));
        }
        #[cfg(feature = "benchmarks")]
        {
            self.date_prepare_ms = started.elapsed().as_secs_f64() * 1000.0;
        }
    }
    /// Consume paths rather than keeping inventory and display copies alive.
    pub(crate) fn new(inventory: InventoryReport) -> (Self, ScanReport) {
        Self::build(inventory, None)
    }
    fn build(
        inventory: InventoryReport,
        allocations: Option<&[Metadata<u64>]>,
    ) -> (Self, ScanReport) {
        // No numeric data means no artificial order of "NotCollected" labels.
        let allocations = allocations.filter(|values| {
            values
                .iter()
                .any(|m| matches!(m.value, MetadataValue::Known { .. }))
        });
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
                let plain_path = entry
                    .path
                    .to_str()
                    .is_some_and(|value| !value.chars().any(char::is_control));
                Node {
                    name,
                    plain_path,
                    path: entry.path,
                    map: None,
                    parent: entry.parent,
                    bytes: entry.logical_bytes,
                    kind: entry.kind,
                    directory: entry.kind == InventoryKind::Directory,
                    incomplete: entry.incomplete,
                    counts: entry.counts,
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
            let allocated = allocations.map(|values| {
                Box::new(std::array::from_fn(|direction| {
                    let mut sorted = siblings.clone();
                    sorted.sort_unstable_by(|&left, &right| {
                        let a = &nodes[left];
                        let b = &nodes[right];
                        let number = |i: usize| match values[i].value {
                            MetadataValue::Known { value, .. } => Some(value),
                            _ => None,
                        };
                        let (av, bv) = (number(left), number(right));
                        b.directory
                            .cmp(&a.directory)
                            // Unavailable values stay last within each kind in BOTH directions.
                            .then_with(|| av.is_none().cmp(&bv.is_none()))
                            .then_with(|| {
                                if direction == 0 {
                                    bv.cmp(&av)
                                } else {
                                    av.cmp(&bv)
                                }
                            })
                            // Siblings share their parent: compare native basenames first
                            // to avoid parsing a long common prefix for every comparison.
                            // Lossy display names never determine this tie-break.
                            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
                            .then_with(|| a.path.as_os_str().cmp(b.path.as_os_str()))
                            .then_with(|| left.cmp(&right))
                    });
                    sorted.into_boxed_slice()
                }))
            });
            nodes[index].children = Some(Box::new(ChildOrders {
                standard: orders,
                allocated,
                temporal: None,
            }));
        }
        // Projection and grouping happen on the worker, once per inventory.
        for index in 0..nodes.len() {
            if nodes[index].directory {
                let projection = Projection::new(
                    nodes[index]
                        .children
                        .as_ref()
                        .map_or(&[][..], |o| o.standard[0].as_ref())
                        .iter()
                        .map(|&i| (i, nodes[i].bytes, nodes[i].incomplete)),
                    nodes[index].bytes,
                    nodes[index].incomplete,
                    truncated,
                );
                nodes[index].map = Some(Box::new(projection));
            }
        }
        let top_files = inventory.top_files.entries;
        let top_rows = top_files
            .iter()
            .enumerate()
            .map(|(i, f)| TopRow {
                rank: (i + 1) as i32,
                name: native_label(std::path::Path::new(
                    f.path.file_name().unwrap_or(f.path.as_os_str()),
                ))
                .into(),
                size: format_bytes(f.logical_bytes).into(),
                path: native_label(&f.path).into(),
            })
            .collect();
        let mut top_note = if coverage == Coverage::Complete {
            "Classement global du périmètre analysé.".to_owned()
        } else {
            "Analyse partielle / annulée : classement des fichiers réellement observés uniquement."
                .to_owned()
        };
        if inventory.top_files.oversized_paths > 0 {
            top_note.push_str(&format!(" Classement limité : {} chemins dépassent {} octets natifs par fichier et sont omis.", inventory.top_files.oversized_paths, sortsize_core::TOP_FILE_MAX_PATH_BYTES));
        }
        if truncated {
            top_note.push_str(" Indépendant de l’inventaire limité.");
        }
        top_note.push_str(" Tailles logiques par chemin, pas un espace récupérable.");
        let selected = roots.first().copied();
        (
            Self {
                nodes,
                dates: None,
                allocation_values: None,
                units: Units::Binary,
                allocation_not_collected: "Non collectée".into(),
                allocation_sort_available: allocations.is_some(),
                owner_data: None,
                owner_orders: None,
                owner_native_orders: None,
                owner_not_collected: "—".into(),
                owner_absent_labels: None,
                #[cfg(feature = "benchmarks")]
                owner_prepare_ms: 0.0,
                #[cfg(feature = "benchmarks")]
                date_prepare_ms: 0.0,
                top_files,
                top_rows,
                top_note,
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
    pub(crate) fn top_selection(&self, rank: usize) -> Option<(String, Option<usize>)> {
        let file = self.top_files.get(rank)?;
        Some((native_label(&file.path), file.retained_entry))
    }
    pub(crate) fn select_entry_parent(&mut self, index: usize) {
        if let Some(parent) = self.nodes.get(index).and_then(|n| n.parent) {
            self.select(parent);
        }
    }
    fn children(&self, index: usize, sort: usize) -> &[usize] {
        // Each family has an explicit range; dates never index Owner arrays.
        if matches!(sort, 6 | 7)
            && let (Some(data), Some(orders)) = (&self.owner_data, &self.owner_orders)
        {
            return &orders.directions[sort - 6][data.ranges[index].clone()];
        }
        self.nodes[index].children.as_ref().map_or(&[], |orders| {
            match sort {
                0..=3 => &orders.standard[sort],
                4 | 5 => orders
                    .allocated
                    .as_ref()
                    .map_or(&orders.standard[0], |o| &o[sort - 4]),
                8..=11 => orders
                    .temporal
                    .as_ref()
                    .map_or(&orders.standard[0], |o| &o[sort - 8]),
                // Legacy trees have no date/Owner table; retain their default order.
                _ => &orders.standard[0],
            }
        })
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
    pub(crate) fn treemap(&self, width: f64, height: f64) -> Vec<MapTile> {
        let Some(projection) = self.selected.and_then(|i| self.nodes[i].map.as_ref()) else {
            return Vec::new();
        };
        let geometry = treemap::layout(&projection.weights, width, height);
        projection
            .weights
            .iter()
            .zip(geometry)
            .map(|(weight, rect)| {
                let (node_id, name, directory, color, partial) = match weight.target {
                    Target::Node(i) => (
                        i as i32,
                        self.nodes[i].name.to_string(),
                        self.nodes[i].directory,
                        i % 6,
                        self.nodes[i].incomplete,
                    ),
                    Target::Other => (
                        -1,
                        format!("Autres ({} éléments)", projection.grouped),
                        false,
                        6,
                        projection.partial,
                    ),
                    Target::Omitted => (
                        -2,
                        "Omis de l’inventaire".to_owned(),
                        false,
                        7,
                        projection.partial,
                    ),
                };
                let size = format!(
                    "{}{}",
                    if partial { "≥ " } else { "" },
                    self.units.format(weight.bytes)
                );
                let x = rect.x as f32;
                let y = rect.y as f32;
                MapTile {
                    node_id,
                    name: name.into(),
                    size: size.into(),
                    directory,
                    color_index: color as i32,
                    x,
                    y,
                    w: (rect.width as f32).min(1.0 - x),
                    h: (rect.height as f32).min(1.0 - y),
                }
            })
            .collect()
    }
    pub(crate) fn treemap_note(&self) -> String {
        let Some(p) = self.selected.and_then(|i| self.nodes[i].map.as_ref()) else {
            return self.empty_message().to_owned();
        };
        if p.inconsistent {
            return "Agrégats incohérents : proportions indisponibles.".into();
        }
        let mut note = if p.partial || p.unknown > 0 || self.truncated {
            "Proportions des octets observés uniquement ; couverture incomplète.".to_owned()
        } else {
            "Tailles logiques · clic : dossier / inspection du fichier.".to_owned()
        };
        if self.truncated {
            note.push_str(" Inventaire limité ; omissions signalées, sans chemin individuel.");
        }
        if p.grouped > 0 {
            note.push_str(&format!(
                " Autres : {} éléments regroupés, accessibles dans le tableau.",
                p.grouped
            ));
        }
        if p.zeros > 0 {
            note.push_str(&format!(" {} taille(s) nulle(s) sans surface.", p.zeros));
        }
        if p.unknown > 0 {
            note.push_str(&format!(
                " {} taille(s) inconnue(s) / non mesurée(s), sans surface.",
                p.unknown
            ));
        }
        if p.weights.is_empty() {
            note.push_str(" Aucune taille positive représentable ; consulter le tableau.");
        }
        note
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
    pub(crate) fn path(&self) -> String {
        self.selected.map_or_else(String::new, |i| {
            display_label(&self.nodes[i].path.to_string_lossy())
        })
    }
    pub(crate) fn entry_path(&self, index: usize) -> Option<String> {
        self.selected
            .filter(|p| self.nodes.get(index).is_some_and(|n| n.parent == Some(*p)))
            .map(|_| native_label(&self.nodes[index].path))
    }
    pub(crate) fn reveal_entry(&mut self, index: usize) -> usize {
        if let Some(parent) = self.selected
            && let Some(position) = self
                .children(parent, self.sort)
                .iter()
                .position(|&i| i == index)
        {
            self.detail_offset = position / MAX_VISIBLE_ROWS * MAX_VISIBLE_ROWS;
            return position % MAX_VISIBLE_ROWS;
        }
        0
    }
    pub(crate) fn selection_summary(&self) -> String {
        self.selected.map_or_else(String::new, |i| {
            let node = &self.nodes[i];
            format!(
                "{} · fichiers directs : {} · sous-dossiers directs : {}\nfichiers récursifs : {} · sous-dossiers récursifs : {} · {} entrées retenues{}{}",
                node.size(self.units),
                node.count(|c| c.direct_files),
                node.count(|c| c.direct_directories),
                node.count(|c| c.recursive_files),
                node.count(|c| c.recursive_directories),
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
                    &node.children.as_ref().unwrap().standard[0][..node.directory_count],
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
                    size: node.size(self.units).into(),
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
        let page = &children[self.detail_offset.min(children.len())
            ..(self.detail_offset + MAX_VISIBLE_ROWS).min(children.len())];
        let owner_labels = self.owner_page_labels(page);
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
                    // Format this page only; native identity stays solely in Node.
                    path: node.path_label(),
                    size: node.size(self.units).into(),
                    allocated: self.allocation_label(index),
                    note: node.note().into(),
                    directory: node.directory,
                    proportion: proportion.into(),
                    fraction,
                    files: node.count(|c| c.recursive_files).into(),
                    directories: node.count(|c| c.recursive_directories).into(),
                    modified: crate::date_label::format_date(
                        self.dates
                            .as_ref()
                            .map_or(MetadataValue::NotCollected, |dates| dates[index].modified),
                    )
                    .into(),
                    accessed: crate::date_label::format_date(
                        self.dates
                            .as_ref()
                            .map_or(MetadataValue::NotCollected, |dates| dates[index].accessed),
                    )
                    .into(),
                    owner: self.owner_label(index, &owner_labels),
                }
            })
            .collect();
        (rows, children.len() > self.detail_offset + MAX_VISIBLE_ROWS)
    }
    pub(crate) fn size_label(&self, index: usize) -> slint::SharedString {
        self.nodes[index].size(self.units).into()
    }
    pub(crate) fn allocation_label(&self, index: usize) -> slint::SharedString {
        self.allocation_values.as_ref().map_or_else(
            || self.allocation_not_collected.clone(),
            |values| crate::allocation_label::format_value(values[index], self.units).into(),
        )
    }
    pub(crate) fn set_units(&mut self, units: Units) {
        self.units = units;
        for (row, file) in self.top_rows.iter_mut().zip(&self.top_files) {
            row.size = units.format(file.logical_bytes).into();
        }
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
    pub(crate) fn allocation_sort_available(&self) -> bool {
        self.allocation_sort_available
    }
    pub(crate) fn sort_allocated(&mut self) {
        if self.allocation_sort_available {
            self.sort = if self.sort == 4 { 5 } else { 4 };
            self.detail_offset = 0;
        }
    }
    #[cfg(feature = "benchmarks")]
    pub(crate) fn date_order_bytes(&self) -> usize {
        self.nodes
            .iter()
            .filter_map(|n| n.children.as_ref())
            .map(|c| {
                // Added Option pointer in ChildOrders, plus the boxed slice descriptors
                // and their exact index payload. Allocator bookkeeping is excluded.
                std::mem::size_of_val(&c.temporal)
                    + c.temporal.as_ref().map_or(0, |orders| {
                        std::mem::size_of_val(orders.as_ref())
                            + orders
                                .iter()
                                .map(|o| std::mem::size_of_val(o.as_ref()))
                                .sum::<usize>()
                    })
            })
            .sum()
    }
    pub(crate) fn sort_dates(&mut self, accessed: bool) {
        let recent = if accessed { 10 } else { 8 };
        self.sort = if self.sort == recent {
            recent + 1
        } else {
            recent
        };
        self.detail_offset = 0;
    }
    pub(crate) fn sort(&self) -> usize {
        self.sort
    }
    pub(crate) fn change_page(&mut self, details: bool, next: bool) {
        let limited = if details {
            self.selected.is_some_and(|i| {
                self.children(i, self.sort).len() > self.detail_offset + MAX_VISIBLE_ROWS
            })
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
    Units::Binary.format(bytes)
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
    fn counts_display_complete_partial_unavailable_and_truncated_values() {
        let fixture = TempDir::new().unwrap();
        fs::create_dir_all(fixture.path().join("folder/empty")).unwrap();
        fs::write(fixture.path().join("folder/file"), []).unwrap();
        let tree = scan(fixture.path(), 10);
        let rows = tree.details().0;
        assert_eq!(
            (rows[0].files.as_str(), rows[0].directories.as_str()),
            ("1", "1")
        );
        assert!(tree.selection_summary().contains("fichiers directs : 0"));
        assert!(
            tree.selection_summary()
                .contains("sous-dossiers récursifs : 2")
        );
        let mut tree = scan(fixture.path(), 10);
        tree.select(rows[0].node_id as usize);
        let rows = tree.details().0;
        assert_eq!(
            (rows[0].files.as_str(), rows[0].directories.as_str()),
            ("0", "0")
        );
        assert_eq!(
            (rows[1].files.as_str(), rows[1].directories.as_str()),
            ("—", "—")
        );
        let truncated = scan(fixture.path(), 1);
        assert!(
            truncated
                .selection_summary()
                .contains("fichiers récursifs : 1")
        );
        assert!(!truncated.selection_summary().contains('≥'));
        let mut inventory = scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            10,
            |_| {},
        )
        .unwrap();
        for entry in &mut inventory.entries {
            if entry.kind == InventoryKind::Directory {
                entry.incomplete = true;
            }
        }
        let tree = Tree::new(inventory).0;
        assert!(tree.selection_summary().contains("fichiers directs : ≥ 0"));
        assert_eq!(tree.details().0[0].files, "≥ 1");
    }

    #[test]
    fn detail_paths_move_native_identity_and_survive_deleted_source() {
        let fixture = TempDir::new().unwrap();
        let mut folder = fixture.path().join("été 日本語 🦀");
        for _ in 0..12 {
            folder = folder.join("long-directory-01234567890123456789");
        }
        fs::create_dir_all(&folder).unwrap();
        let name = "é".repeat(110);
        fs::write(folder.join(&name), b"12345").unwrap();
        fs::write(folder.join("small"), b"1").unwrap();
        let inventory = scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            100,
            |_| {},
        )
        .unwrap();
        let index = inventory
            .entries
            .iter()
            .position(|e| e.path.ends_with(&name))
            .unwrap();
        let pointer = inventory.entries[index]
            .path
            .as_os_str()
            .as_encoded_bytes()
            .as_ptr();
        let native = inventory.entries[index].path.clone();
        let (mut tree, _) = Tree::new(inventory);
        assert_eq!(
            tree.nodes[index]
                .path
                .as_os_str()
                .as_encoded_bytes()
                .as_ptr(),
            pointer
        );
        fixture.close().unwrap();
        tree.select_entry_parent(index);
        for name_sort in [false, false, true, true] {
            tree.sort_by(name_sort);
            let rows = tree.details().0;
            let row = rows.iter().find(|r| r.node_id == index as i32).unwrap();
            assert_eq!(row.path, native_label(&native));
            assert!(row.path.contains("été 日本語 🦀"));
            assert!(row.path.ends_with(&name));
            assert_eq!(row.size, "5 octets");
            assert_eq!(tree.entry_path(index).unwrap(), row.path.as_str());
            let top = tree.top_rows.iter().find(|r| r.name == name).unwrap();
            assert_eq!(top.path, row.path);
            assert!(
                tree.treemap(800.0, 200.0)
                    .iter()
                    .any(|t| t.node_id == row.node_id)
            );
        }
        let before = tree.sort();
        tree.parent();
        assert_eq!(tree.sort(), before);
        assert_eq!(
            tree.details().0[0].path,
            native_label(native.parent().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn detail_paths_distinguish_native_bytes_and_controls_without_parsing_labels() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = TempDir::new().unwrap();
        for name in [
            vec![b'n', 0xfe],
            vec![b'n', 0xff],
            b"line\nname".to_vec(),
            br"line\nname".to_vec(),
        ] {
            fs::write(
                fixture.path().join(std::ffi::OsString::from_vec(name)),
                b"x",
            )
            .unwrap();
        }
        let tree = scan(fixture.path(), 100);
        fixture.close().unwrap();
        let rows = tree.details().0;
        for (i, a) in rows.iter().enumerate() {
            let native = &tree.nodes[a.node_id as usize].path;
            assert_eq!(a.path, native_label(native));
            assert_eq!(
                tree.entry_path(a.node_id as usize).unwrap(),
                a.path.as_str()
            );
            for b in &rows[i + 1..] {
                assert_ne!(a.path, b.path);
            }
        }
        assert!(rows.iter().any(|r| r.path.contains("\\xFE")));
        assert!(rows.iter().any(|r| r.path.contains("\\xFF")));
    }

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
        for row in tree.details().0 {
            assert_eq!(row.path, tree.entry_path(row.node_id as usize).unwrap());
        }
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
        for row in &details {
            assert!(!row.path.is_empty());
            assert_eq!(row.path, tree.entry_path(row.node_id as usize).unwrap());
        }
        assert!(tree.selection_summary().contains("partielle"));
    }

    #[test]
    fn treemap_navigation_survives_removed_source_and_keeps_sort() {
        let fixture = TempDir::new().unwrap();
        fs::create_dir_all(fixture.path().join("folder/nested")).unwrap();
        fs::write(fixture.path().join("folder/data"), b"123456").unwrap();
        fs::write(fixture.path().join("file"), b"1234").unwrap();
        let mut tree = scan(fixture.path(), 100);
        let original = tree.treemap(800.0, 300.0);
        assert_eq!(original.len(), 2);
        assert!((original[0].w * original[0].h - 0.6).abs() < 1e-6);
        // Navigation must be entirely independent of subsequent filesystem I/O.
        fixture.close().unwrap();
        tree.sort_by(true);
        let folder = original.iter().find(|t| t.directory).unwrap().node_id as usize;
        tree.select(folder);
        assert_eq!(tree.sort(), 2);
        assert!(tree.path().ends_with("folder"));
        assert!(
            tree.rows()
                .0
                .iter()
                .any(|r| r.selected && r.node_id == folder as i32)
        );
        let file = tree.treemap(300.0, 800.0)[0].node_id as usize;
        assert!(tree.entry_path(file).unwrap().ends_with("data"));
        assert!(tree.treemap_note().contains("nulle"));
        tree.parent();
        assert_eq!(tree.treemap(800.0, 300.0)[0].node_id, original[0].node_id);
        assert_eq!(tree.details().0[0].name, "folder");
    }

    #[test]
    fn treemap_above_default_retention_cap_accounts_for_omitted_bytes() {
        let fixture = TempDir::new().unwrap();
        const FILES: u64 = 50_010;
        for i in 0..FILES {
            fs::write(fixture.path().join(format!("f{i:05}")), b"x").unwrap();
        }
        let inventory = scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            50_000,
            |_| {},
        )
        .unwrap();
        assert_eq!(inventory.report.files, FILES);
        assert_eq!(inventory.entries.len(), 50_000);
        assert_eq!(inventory.entries_omitted, 11);
        assert_eq!(inventory.entries[0].logical_bytes, Some(FILES));
        let mut tree = Tree::new(inventory).0;
        let projection = tree.nodes[0].map.as_ref().unwrap();
        assert_eq!(
            projection.weights.iter().map(|w| w.bytes).sum::<u64>(),
            FILES
        );
        assert!(projection.weights.contains(&treemap::Weight {
            target: Target::Omitted,
            bytes: 11
        }));
        assert!(tree.treemap(800.0, 300.0).len() <= treemap::MAX_TILES);
        assert!(tree.treemap_note().contains("Inventaire limité"));
        assert!(tree.details().1);
        let index = tree.children(0, tree.sort)[MAX_VISIBLE_ROWS + 1];
        tree.reveal_entry(index);
        assert_eq!(tree.offset(true), MAX_VISIBLE_ROWS);
        assert!(tree.details().0.iter().any(|r| r.node_id == index as i32));
        assert_eq!(
            tree.details()
                .0
                .iter()
                .find(|r| r.node_id == index as i32)
                .unwrap()
                .path,
            tree.entry_path(index).unwrap()
        );
    }

    #[test]
    fn treemap_partial_unknown_zero_and_path_budget_are_visible() {
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("data"), b"123").unwrap();
        // The scanner retains canonical native paths, including the Windows
        // verbatim prefix, so budget exactly the path it will observe.
        let root = fs::canonicalize(fixture.path()).unwrap();
        let inventory = sortsize_core::scan_inventory_with_limits(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            sortsize_core::InventoryLimits {
                max_entries: 100,
                max_path_bytes: root.as_os_str().as_encoded_bytes().len(),
            },
            |_| {},
        )
        .unwrap();
        assert!(inventory.path_bytes_limit_reached);
        assert_eq!(inventory.entries.len(), 1);
        assert_eq!(inventory.entries[0].path, root);
        assert_eq!(inventory.entries_omitted, 1);
        let tree = Tree::new(inventory).0;
        assert_eq!(tree.treemap(800.0, 300.0)[0].node_id, -2);
        assert_eq!(tree.treemap(800.0, 300.0)[0].size, "3 octets");
        let mut inventory = scan_inventory(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            100,
            |_| {},
        )
        .unwrap();
        inventory.entries[0].incomplete = true;
        inventory.entries[1].logical_bytes = None;
        let tree = Tree::new(inventory).0;
        assert!(tree.treemap(800.0, 300.0).is_empty());
        assert!(tree.treemap_note().contains("inconnue"));
        assert!(tree.treemap_note().contains("incomplète"));
    }

    #[cfg(unix)]
    #[test]
    fn treemap_keeps_native_paths_even_with_identical_lossy_labels() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = TempDir::new().unwrap();
        for byte in [0xfe, 0xff] {
            fs::write(
                fixture
                    .path()
                    .join(std::ffi::OsString::from_vec(vec![byte])),
                b"x",
            )
            .unwrap();
        }
        let tree = scan(fixture.path(), 10);
        let tiles = tree.treemap(800.0, 300.0);
        assert_eq!(tiles.len(), 2);
        let a = &tree.nodes[tiles[0].node_id as usize];
        let b = &tree.nodes[tiles[1].node_id as usize];
        assert_eq!(a.name, b.name);
        assert_ne!(a.path, b.path);
        assert_ne!(tiles[0].node_id, tiles[1].node_id);
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

/// Unicode stays readable; invalid native units use Rust's lossless OS-string
/// debug escapes, rather than replacement characters that alias distinct paths.
fn native_label(path: &std::path::Path) -> String {
    match path.to_str() {
        Some(value) if !value.chars().any(char::is_control) => value.to_owned(),
        _ => format!("{:?}", path.as_os_str()),
    }
}

#[cfg(test)]
mod top_tests {
    use super::*;
    use sortsize_core::{
        CancellationToken, InventoryLimits, ScanOptions, scan_inventory_with_limits,
    };
    #[test]
    fn exact_labels_preserve_prefixes_and_distinguish_control_escapes() {
        use std::path::Path;
        let prefix = r"\\?\C:\native\file";
        assert_eq!(native_label(Path::new(prefix)), prefix);
        assert_ne!(
            native_label(Path::new("a\nb")),
            native_label(Path::new(r"a\nb"))
        );
    }

    #[test]
    fn memory_limited_ranking_is_explicit_without_changing_scan_coverage() {
        let fixture = tempfile::TempDir::new().unwrap();
        let mut inventory = scan_inventory_with_limits(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        inventory.top_files.oversized_paths = 2;
        let (tree, report) = Tree::new(inventory);
        assert_eq!(report.coverage, Coverage::Complete);
        assert!(tree.top_note.contains("Classement limité : 2 chemins"));
        assert!(tree.top_rows.is_empty());
    }

    #[test]
    fn top_identity_survives_missing_nodes_native_aliases_and_deleted_source() {
        let fixture = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(fixture.path().join("nested")).unwrap();
        std::fs::write(fixture.path().join("nested/été"), b"1234").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            for byte in [0xfe, 0xff] {
                let name = std::ffi::OsString::from_vec(vec![b'n', byte]);
                std::fs::write(fixture.path().join(name), b"a").unwrap();
            }
        }
        for max_entries in [1, 100] {
            let inventory = scan_inventory_with_limits(
                fixture.path(),
                &ScanOptions::default(),
                &CancellationToken::default(),
                InventoryLimits {
                    max_entries,
                    ..Default::default()
                },
                |_| {},
            )
            .unwrap();
            let (mut tree, _) = Tree::new(inventory);
            let (path, target) = tree.top_selection(0).unwrap();
            assert!(path.ends_with("été"));
            assert_eq!(target.is_some(), max_entries == 100);
            if let Some(target) = target {
                tree.select_entry_parent(target);
                assert_eq!(tree.entry_path(target).unwrap(), path);
            }
            if max_entries == 1 {
                assert!(tree.top_note.contains("Indépendant"));
            }
            #[cfg(unix)]
            {
                assert_ne!(tree.top_rows[1].path, tree.top_rows[2].path);
                assert!(tree.top_rows[1].path.contains("\\xFE"));
                if let Some(target) = tree.top_selection(1).unwrap().1 {
                    tree.select_entry_parent(target);
                    assert_eq!(
                        tree.entry_path(target).unwrap(),
                        tree.top_rows[1].path.as_str()
                    );
                }
            }
        }
        let inventory = scan_inventory_with_limits(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            InventoryLimits::default(),
            |_| {},
        )
        .unwrap();
        drop(fixture);
        let (tree, _) = Tree::new(inventory);
        assert!(tree.top_selection(0).unwrap().0.ends_with("été"));
    }
}

#[cfg(test)]
mod date_tests;

#[cfg(test)]
mod allocation_tests;

#[cfg(test)]
mod allocated_sort_tests;

#[cfg(test)]
mod temporal_sort_tests;
