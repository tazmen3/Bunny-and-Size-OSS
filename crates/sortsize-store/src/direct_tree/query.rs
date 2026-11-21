//! Cooperative, bounded global ordering over the recorded in-memory tree.
//! Unprepared scopes are evaluated from the recorded observations.
use super::name_index::NameSearch;
use super::{DirectRow, DirectTree, file_order};
use crate::inventory::{Error, Kind, Result};
use sortsize_core::{
    CancellationToken, InventoryKind, MetadataValue,
    analysis::{Evaluation, PreparedQuery, SearchQuery},
    inventory_contract::EntryId,
    resources::{Category, Governor, Reservation},
};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Cursor {
    key: Arc<Key<'static>>,
    _memory: Arc<Reservation>,
}
impl Eq for Cursor {}
impl PartialEq for Cursor {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
#[derive(Eq, PartialEq)]
struct Key<'a> {
    kind_rank: u8,
    value: Value,
    missing: bool,
    descending: bool,
    path: Cow<'a, Path>,
    id: u64,
}
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
enum Value {
    Number(u64),
    Time(i128),
    Name,
    Owner(Vec<u8>),
}
impl Ord for Cursor {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.compare(&other.key)
    }
}
impl Key<'_> {
    fn compare(&self, other: &Key<'_>) -> Ordering {
        self.kind_rank
            .cmp(&other.kind_rank)
            .then_with(|| self.missing.cmp(&other.missing))
            .then_with(|| {
                let value = match (&self.value, &other.value) {
                    (Value::Name, Value::Name) => self.path.cmp(&other.path),
                    (a, b) => a.cmp(b),
                };
                if self.descending {
                    value.reverse()
                } else {
                    value
                }
            })
            .then_with(|| self.path.cmp(&other.path))
            .then(self.id.cmp(&other.id))
    }

    fn retain(self) -> Result<Cursor> {
        let memory = Governor::global()
            .reserve(
                Category::Queries,
                (self.path.as_os_str().len() * 4 + 512) as u64,
                None,
            )
            .ok_or(Error::Contract("direct query path budget"))?;
        Ok(Cursor {
            key: Arc::new(Key {
                path: Cow::Owned(self.path.into_owned()),
                kind_rank: self.kind_rank,
                value: self.value,
                missing: self.missing,
                descending: self.descending,
                id: self.id,
            }),
            _memory: Arc::new(memory),
        })
    }
}
impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
struct Kept {
    cursor: Cursor,
}
impl Eq for Kept {}
impl PartialEq for Kept {
    fn eq(&self, o: &Self) -> bool {
        self.cursor == o.cursor
    }
}
impl Ord for Kept {
    fn cmp(&self, o: &Self) -> Ordering {
        self.cursor.cmp(&o.cursor)
    }
}
impl PartialOrd for Kept {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
fn time(value: SystemTime) -> i128 {
    match value.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}
pub(super) fn size_class(logical: Option<u64>) -> usize {
    match logical {
        Some(0) => 0,
        Some(1..=1023) => 1,
        Some(1024..=1_048_575) => 2,
        Some(1_048_576..=1_073_741_823) => 3,
        Some(_) => 4,
        None => 5,
    }
}

pub(super) fn extension_bytes(extension: &OsStr) -> u64 {
    (extension.len() * 4 + 256) as u64
}

fn unfiltered_name_order(
    parent: Option<EntryId>,
    criteria: Option<&SearchQuery>,
    sort: i32,
) -> Option<file_order::Scope> {
    if parent.is_some() || !matches!(sort, 2 | 3) {
        return None;
    }
    let criteria = criteria?;
    if criteria.name.is_some()
        || criteria.path.is_some()
        || criteria.extension.is_some()
        || criteria.glob.is_some()
        || criteria.min_bytes.is_some()
        || criteria.max_bytes.is_some()
        || criteria.modified_after.is_some()
        || criteria.modified_before.is_some()
    {
        return None;
    }
    match criteria.kind {
        Some(InventoryKind::File) => Some(file_order::Scope::Files),
        None => Some(file_order::Scope::All),
        _ => None,
    }
}

fn key<'a>(
    row: &'a DirectRow,
    sort: i32,
    labels: &[crate::inventory::ordered::OwnerLabel],
    group_directories: bool,
) -> Key<'a> {
    let numeric = |n: Option<u64>| (Value::Number(n.unwrap_or(0)), n.is_none());
    let date = |n: Option<i128>| (Value::Time(n.unwrap_or(0)), n.is_none());
    let (value, missing) = match sort {
        0 | 1 => numeric(row.logical),
        4 | 5 => numeric(row.allocated),
        6 | 7 => {
            let mut bytes = Vec::new();
            match row.properties.owner.id {
                MetadataValue::Known {
                    value: sortsize_core::NativeOwnerId::UnixUid(uid),
                    ..
                } => bytes.extend(uid.to_be_bytes()),
                #[cfg(windows)]
                MetadataValue::Known {
                    value: sortsize_core::NativeOwnerId::WindowsSid(sid),
                    ..
                } => bytes.extend(sid.as_bytes()),
                _ => {}
            };
            let unknown = bytes.is_empty();
            if let MetadataValue::Known { value, .. } = row.properties.owner.id {
                if let Some(label) = labels.iter().find(|label| label.owner == value) {
                    bytes = label.label.as_bytes().to_vec();
                    bytes.insert(0, 0);
                } else {
                    bytes.insert(0, 1);
                }
            }
            (Value::Owner(bytes), unknown)
        }
        8 | 9 => date(
            row.modified
                .map(|(s, n)| i128::from(s) * 1_000_000_000 + i128::from(n)),
        ),
        10 | 11 => date(match row.properties.accessed {
            MetadataValue::Known { value, .. } => Some(time(value)),
            _ => None,
        }),
        _ => (Value::Name, false),
    };
    Key {
        kind_rank: u8::from(group_directories && row.kind != Kind::Directory),
        value,
        missing,
        descending: matches!(sort, 0 | 3 | 4 | 7 | 8 | 10),
        path: Cow::Borrowed(&row.path),
        id: row.id.0,
    }
}
/// A page and exact counts are published only after all rows of its scope were examined.
/// While a scan runs, this is a dated partial observation and may be refreshed.
pub struct PageQuery {
    parent: Option<EntryId>,
    after: Option<EntryId>,
    next_id: u64,
    end: u64,
    query: Option<PreparedQuery>,
    name_search: Option<NameSearch>,
    sort: i32,
    file_order_scope: Option<file_order::Scope>,
    anchor: Option<Cursor>,
    inclusive_anchor: bool,
    folders_only: bool,
    limit: usize,
    kept: BinaryHeap<Kept>,
    done: bool,
    pub evaluated: u64,
    pub matches: u64,
    pub gaps: u64,
    pub files: u64,
    pub logical: u128,
    pub unknown_sizes: u64,
    pub revision: u64,
    pub classes: [u64; 6],
    pub extensions: BTreeMap<std::ffi::OsString, (u64, u128)>,
    pub other_extensions: (u64, u128),
    extension_memory: Vec<Reservation>,
    root: PathBuf,
    owner_labels: Vec<crate::inventory::ordered::OwnerLabel>,
    owner_memory: Option<Reservation>,
    _memory: Reservation,
}
impl PageQuery {
    pub fn new(
        tree: &DirectTree,
        parent: Option<EntryId>,
        query: Option<&SearchQuery>,
        sort: i32,
        anchor: Option<Cursor>,
        limit: usize,
    ) -> Result<Self> {
        if limit == 0 || limit > 200 {
            return Err(Error::Contract("direct query page size"));
        }
        let memory = Governor::global()
            .reserve(Category::Queries, 64 * 1024, None)
            .ok_or(Error::Contract("direct query memory budget"))?;
        let file_order_scope = unfiltered_name_order(parent, query, sort);
        let query = query
            .map(PreparedQuery::new)
            .transpose()
            .map_err(|_| Error::Contract("invalid direct search"))?;
        let name_search = if parent.is_none() {
            query
                .as_ref()
                .and_then(|query| NameSearch::new(tree, query))
        } else {
            None
        };
        Ok(Self {
            parent,
            after: None,
            next_id: 1,
            end: tree.summary().entries.saturating_add(1),
            query,
            name_search,
            sort,
            file_order_scope,
            anchor,
            inclusive_anchor: false,
            folders_only: false,
            limit,
            kept: BinaryHeap::new(),
            done: false,
            evaluated: 0,
            matches: 0,
            gaps: 0,
            files: 0,
            logical: 0,
            unknown_sizes: 0,
            revision: tree.revision(),
            classes: [0; 6],
            extensions: BTreeMap::new(),
            other_extensions: (0, 0),
            extension_memory: Vec::new(),
            root: tree.row(EntryId(0))?.path,
            owner_labels: Vec::new(),
            owner_memory: None,
            _memory: memory,
        })
    }
    /// Restrict a sibling order to directories without materializing the scope.
    pub fn folders_only(mut self) -> Self {
        self.folders_only = true;
        self.name_search = None;
        self
    }
    /// Reveal a directory at the start of its ordered window, inclusively.
    pub fn start_at(mut self, tree: &DirectTree, entry: EntryId) -> Result<Self> {
        self.anchor = Some(
            key(
                &tree.row(entry)?,
                self.sort,
                &self.owner_labels,
                self.parent.is_some() && self.sort >= 2,
            )
            .retain()?,
        );
        self.inclusive_anchor = true;
        Ok(self)
    }
    pub fn set_owner_labels(
        &mut self,
        labels: &[crate::inventory::ordered::OwnerLabel],
    ) -> Result<()> {
        let bytes = labels.iter().map(|l| l.label.len() + 128).sum::<usize>();
        let memory = Governor::global()
            .reserve(Category::Queries, bytes as u64, None)
            .ok_or(Error::Contract("direct owner order budget"))?;
        self.owner_labels = labels.to_vec();
        self.owner_memory = Some(memory);
        Ok(())
    }
    pub fn done(&self) -> bool {
        self.done
    }
    pub fn advance(&mut self, tree: &DirectTree, cancel: &CancellationToken) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        if self
            .name_search
            .as_ref()
            .is_some_and(|search| !search.is_current(tree))
        {
            return Err(Error::Contract("direct name query revision changed"));
        }
        if self.evaluated == 0
            && !self.folders_only
            && self.anchor.as_ref().is_none_or(|anchor| {
                anchor.key.kind_rank == 0
                    && !anchor.key.missing
                    && matches!(anchor.key.value, Value::Name)
            })
            && let Some(scope) = self.file_order_scope
            // A damaged, slow or unaffordable order leaves the exhaustive search
            // available. That search also preserves cancellation semantics.
            && self.apply_file_order(tree, scope, cancel).is_ok()
        {
            return Ok(true);
        }
        let started = Instant::now();
        // The tree cannot change during this turn. Adjacent observations often
        // share a parent, so rebuild its native path only when it changes.
        let mut last_parent = None;
        let mut row_path = PathBuf::new();
        let mut remaining = 256;
        while remaining != 0 {
            if cancel.is_cancelled() {
                return Err(Error::Contract("direct query cancelled"));
            }
            if started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let row = if let Some(parent) = self.parent {
                let page = tree.children(parent, self.after, 1, self.folders_only)?;
                let Some(row) = page.rows.into_iter().next() else {
                    self.done = true;
                    break;
                };
                self.after = Some(row.id);
                if !page.more {
                    self.done = true
                }
                row
            } else {
                if self.next_id >= self.end {
                    self.done = true;
                    break;
                }
                if let Some(search) = &mut self.name_search {
                    match search.skip(tree, self.next_id) {
                        Ok(Some((next, gaps))) => {
                            self.evaluated += next - self.next_id;
                            self.gaps += gaps;
                            self.next_id = next;
                            continue;
                        }
                        Ok(None) => {}
                        Err(_) => {
                            // Earlier groups passed authentication. Fall back
                            // from the first ID of this still-unexamined block.
                            self.name_search = None;
                        }
                    }
                }
                let id = EntryId(self.next_id);
                let node = tree.node(id)?;
                let parent = super::entry_id(node.parent).ok_or_else(super::invalid_entry)?;
                let path = match &mut last_parent {
                    Some((previous, path)) if *previous == parent => path,
                    slot => &slot.insert((parent, tree.path(parent)?)).1,
                };
                let row = tree.read_row_into(id, node, path, std::mem::take(&mut row_path))?;
                self.next_id += 1;
                row
            };
            self.observe(&row)?;
            remaining -= 1;
            row_path = row.path;
            if self.done {
                break;
            }
        }
        Ok(self.done)
    }
    fn apply_file_order(
        &mut self,
        tree: &DirectTree,
        scope: file_order::Scope,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let anchor = self
            .anchor
            .as_ref()
            .map(|cursor| (cursor.key.path.as_ref(), EntryId(cursor.key.id)));
        let (totals, ids) = tree.unfiltered_window(
            self.revision,
            file_order::Window {
                scope,
                descending: self.sort == 3,
                anchor,
                inclusive: self.inclusive_anchor,
                limit: self.limit + 1,
            },
            cancel,
        )?;
        let mut kept = BinaryHeap::with_capacity(ids.len());
        for id in ids {
            if cancel.is_cancelled() {
                return Err(Error::Contract("direct query cancelled"));
            }
            let row = tree.row(id)?;
            kept.push(Kept {
                cursor: key(&row, self.sort, &[], false).retain()?,
            });
        }
        let mut extension_memory = Vec::with_capacity(totals.extensions.len());
        for extension in totals.extensions.keys() {
            extension_memory.push(
                Governor::global()
                    .reserve(Category::Queries, extension_bytes(extension), None)
                    .ok_or(Error::Contract("direct query extension budget"))?,
            );
        }
        if cancel.is_cancelled() {
            return Err(Error::Contract("direct query cancelled"));
        }
        self.kept = kept;
        self.evaluated = totals.evaluated;
        self.matches = match scope {
            file_order::Scope::Files => totals.files,
            file_order::Scope::All => totals.all_matches(),
        };
        self.gaps = totals.gaps;
        self.files = totals.files;
        self.logical = totals.logical;
        self.unknown_sizes = totals.unknown_sizes;
        self.classes = totals.classes;
        self.extensions = totals.extensions.clone();
        self.other_extensions = totals.other_extensions;
        self.extension_memory = extension_memory;
        self.done = true;
        Ok(())
    }
    fn observe(&mut self, row: &DirectRow) -> Result<()> {
        self.evaluated += 1;
        if self.folders_only && row.kind != Kind::Directory {
            return Ok(());
        }
        let matched = if let Some(query) = &self.query {
            let kind = match row.kind {
                Kind::File => InventoryKind::File,
                Kind::Directory => InventoryKind::Directory,
                Kind::Symlink => InventoryKind::Symlink,
                Kind::Special => InventoryKind::Special,
                Kind::Unknown => {
                    self.gaps += 1;
                    return Ok(());
                }
            };
            let modified = row.modified.and_then(|(s, n)| {
                if s >= 0 {
                    UNIX_EPOCH.checked_add(Duration::new(s as u64, n))
                } else {
                    UNIX_EPOCH
                        .checked_sub(Duration::from_secs(s.unsigned_abs()))
                        .and_then(|t| t.checked_add(Duration::from_nanos(u64::from(n))))
                }
            });
            match query.evaluate_path(&row.path, kind, row.logical, modified, Some(&self.root)) {
                Evaluation::Match => true,
                Evaluation::NoMatch => false,
                _ => {
                    self.gaps += 1;
                    false
                }
            }
        } else {
            true
        };
        if matched {
            self.matches += 1;
            if row.kind == Kind::File {
                self.files += 1;
                match row.logical {
                    Some(n) => self.logical += u128::from(n),
                    None => self.unknown_sizes += 1,
                }
                self.classes[size_class(row.logical)] += 1;
                let extension = row.path.extension().unwrap_or_default();
                if let Some(count) = self.extensions.get_mut(extension) {
                    count.0 += 1;
                    count.1 += u128::from(row.logical.unwrap_or(0));
                } else if self.extensions.len() < 100 {
                    if let Some(memory) = Governor::global().reserve(
                        Category::Queries,
                        extension_bytes(extension),
                        None,
                    ) {
                        self.extension_memory.push(memory);
                        self.extensions.insert(
                            extension.to_owned(),
                            (1, u128::from(row.logical.unwrap_or(0))),
                        );
                    } else {
                        self.other_extensions.0 += 1;
                        self.other_extensions.1 += u128::from(row.logical.unwrap_or(0));
                    }
                } else {
                    self.other_extensions.0 += 1;
                    self.other_extensions.1 += u128::from(row.logical.unwrap_or(0));
                }
            }
            let key = key(
                row,
                self.sort,
                &self.owner_labels,
                self.parent.is_some() && self.sort >= 2,
            );
            if self.anchor.as_ref().is_none_or(|anchor| {
                let order = key.compare(&anchor.key);
                order.is_gt() || (self.inclusive_anchor && order.is_eq())
            }) {
                let keep = self.kept.len() < self.limit + 1
                    || self
                        .kept
                        .peek()
                        .is_some_and(|last| key.compare(&last.cursor.key).is_lt());
                if keep {
                    self.kept.push(Kept {
                        cursor: key.retain()?,
                    });
                    if self.kept.len() > self.limit + 1 {
                        self.kept.pop();
                    }
                }
            }
        }
        Ok(())
    }
    pub fn rows(&self, tree: &DirectTree) -> Result<Vec<DirectRow>> {
        let mut keys = self.kept.iter().map(|k| &k.cursor).collect::<Vec<_>>();
        keys.sort();
        keys.into_iter()
            .take(self.limit)
            .map(|k| tree.row(EntryId(k.key.id)))
            .collect()
    }
    pub fn next(&self) -> Option<Cursor> {
        if self.kept.len() <= self.limit {
            return None;
        }
        let mut keys = self.kept.iter().map(|k| &k.cursor).collect::<Vec<_>>();
        keys.sort();
        Some(keys[self.limit - 1].clone())
    }
    pub fn more(&self) -> bool {
        self.kept.len() > self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{codec::NativeName, inventory::Observation, scheduler::direct::ObservationBatch};
    use sortsize_core::{acquisition::NodeProperties, inventory_contract::ListingEnd};
    fn fixture() -> (tempfile::TempDir, DirectTree) {
        fixture_with_directories(false)
    }
    fn fixture_with_directories(mixed: bool) -> (tempfile::TempDir, DirectTree) {
        let scratch = tempfile::tempdir().unwrap();
        let mut tree = DirectTree::new(
            scratch.path().join("source"),
            scratch.path(),
            16 * 1024 * 1024,
        )
        .unwrap();
        for chunk in 0..6 {
            let start = chunk * 64;
            let end = (start + 64).min(350);
            let observations = (start..end)
                .map(|n| {
                    let text = format!("échantillon-{n:04}.txt");
                    #[cfg(windows)]
                    let name = NativeName::Windows(text.encode_utf16().collect());
                    #[cfg(not(windows))]
                    let name = NativeName::Posix(text.into_bytes());
                    Observation {
                        name,
                        kind: if mixed && n % 7 == 0 {
                            Kind::Directory
                        } else {
                            Kind::File
                        },
                        logical: Some(n as u64 + 1),
                        allocated: None,
                        modified: None,
                        metadata_known: true,
                    }
                })
                .collect::<Vec<_>>();
            let evidence = vec![None; observations.len()];
            let properties = vec![NodeProperties::default(); observations.len()];
            tree.accept(ObservationBatch {
                parent: EntryId(0),
                sequence: chunk as u64,
                end: if end == 350 {
                    Some(ListingEnd::Exhausted)
                } else {
                    None
                },
                observations: &observations,
                evidence: &evidence,
                properties: &properties,
                directory: None,
                errors: 0,
                denied: 0,
                detached: false,
            })
            .unwrap();
        }
        if mixed {
            for n in (0..350).step_by(7) {
                tree.accept(ObservationBatch {
                    parent: EntryId(n + 1),
                    sequence: 0,
                    end: Some(ListingEnd::Exhausted),
                    observations: &[],
                    evidence: &[],
                    properties: &[],
                    directory: None,
                    errors: 0,
                    denied: 0,
                    detached: false,
                })
                .unwrap();
            }
        }
        tree.finish().unwrap();
        (scratch, tree)
    }
    #[test]
    fn ordered_pages_cover_entire_wide_sibling_set() {
        let (_scratch, tree) = fixture();
        let cancel = CancellationToken::default();
        let mut first = PageQuery::new(&tree, Some(EntryId(0)), None, 0, None, 200).unwrap();
        while !first.advance(&tree, &cancel).unwrap() {}
        let rows = first.rows(&tree).unwrap();
        assert_eq!(first.matches, 350);
        assert_eq!(rows.len(), 200);
        assert_eq!(rows[0].logical, Some(350));
        assert_eq!(rows[199].logical, Some(151));
        let mut second =
            PageQuery::new(&tree, Some(EntryId(0)), None, 0, first.next(), 200).unwrap();
        while !second.advance(&tree, &cancel).unwrap() {}
        let rows = second.rows(&tree).unwrap();
        assert_eq!(rows.len(), 150);
        assert_eq!(rows[0].logical, Some(150));
        assert_eq!(rows[149].logical, Some(1));
        assert!(!second.more());
    }
    #[test]
    fn search_unicode_counts_and_statistics_include_hidden_pages() {
        let (_scratch, tree) = fixture();
        let query = SearchQuery {
            name: Some("ÉCHANTILLON".into()),
            extension: Some("txt".into()),
            min_bytes: Some(101),
            ..Default::default()
        };
        let mut search = PageQuery::new(&tree, None, Some(&query), 2, None, 200).unwrap();
        while !search
            .advance(&tree, &CancellationToken::default())
            .unwrap()
        {}
        assert_eq!(search.matches, 250);
        assert_eq!(search.files, 250);
        assert_eq!(search.logical, (101u128..=350).sum());
        assert_eq!(search.extensions.values().next().unwrap().0, 250);
        assert!(search.more());
    }
    #[test]
    fn directory_orders_filter_globally_preserve_grouping_and_reveal_inclusively() {
        let (_scratch, tree) = fixture_with_directories(true);
        let cancel = CancellationToken::default();
        let mut ordered = PageQuery::new(&tree, Some(EntryId(0)), None, 2, None, 200).unwrap();
        while !ordered.advance(&tree, &cancel).unwrap() {}
        let rows = ordered.rows(&tree).unwrap();
        assert!(rows[..50].iter().all(|r| r.kind == Kind::Directory));
        assert!(rows[50..].iter().all(|r| r.kind == Kind::File));
        let mut branch = PageQuery::new(&tree, Some(EntryId(0)), None, 3, None, 20)
            .unwrap()
            .folders_only();
        while !branch.advance(&tree, &cancel).unwrap() {}
        assert_eq!(branch.matches, 50);
        assert_eq!(branch.rows(&tree).unwrap()[0].id, EntryId(344));
        let mut next = PageQuery::new(&tree, Some(EntryId(0)), None, 3, branch.next(), 20)
            .unwrap()
            .folders_only();
        while !next.advance(&tree, &cancel).unwrap() {}
        assert_eq!(next.rows(&tree).unwrap()[0].id, EntryId(204));
        let mut reveal = PageQuery::new(&tree, Some(EntryId(0)), None, 3, None, 20)
            .unwrap()
            .folders_only()
            .start_at(&tree, EntryId(22))
            .unwrap();
        while !reveal.advance(&tree, &cancel).unwrap() {}
        assert_eq!(reveal.rows(&tree).unwrap()[0].id, EntryId(22));
        assert!(
            reveal
                .rows(&tree)
                .unwrap()
                .iter()
                .all(|r| r.kind == Kind::Directory)
        );
    }
    #[test]
    fn query_cancellation_does_not_change_tree() {
        let (_scratch, tree) = fixture();
        let revision = tree.revision();
        let cancel = CancellationToken::default();
        let mut query = PageQuery::new(&tree, None, None, 0, None, 200).unwrap();
        cancel.cancel();
        assert!(query.advance(&tree, &cancel).is_err());
        assert_eq!(tree.revision(), revision);
        assert_eq!(tree.summary().measure.files, 350);
    }
}
