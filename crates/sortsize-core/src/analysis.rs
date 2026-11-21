//! Bounded queries over a retained inventory. No filesystem operations.
use crate::{
    CancellationToken, Coverage, EntryDates, InventoryKind, InventoryReport, MetadataValue,
    NativePath,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::SystemTime};
pub const MAX_ANALYSIS_ENTRIES: usize = 50_000;
pub const MAX_QUERY_BYTES: usize = 4_096;
#[derive(Debug, Clone)]
pub struct AnalysisEntry {
    pub path: Arc<PathBuf>,
    pub kind: InventoryKind,
    pub logical_bytes: Option<u64>,
    pub modified: Option<SystemTime>,
}
#[derive(Debug, Clone)]
pub struct AnalysisSnapshot {
    pub entries: Vec<AnalysisEntry>,
    pub scan_coverage: Coverage,
    pub entries_omitted: u64,
}
impl AnalysisSnapshot {
    pub fn from_inventory(
        inventory: &InventoryReport,
        dates: &[EntryDates],
    ) -> Result<Self, &'static str> {
        if inventory.entries.len() > MAX_ANALYSIS_ENTRIES
            || (!dates.is_empty() && dates.len() != inventory.entries.len())
        {
            return Err("Inventory exceeds analysis limits or date table is inconsistent");
        }
        Ok(Self {
            entries: inventory
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| AnalysisEntry {
                    path: Arc::new(e.path.clone()),
                    kind: e.kind,
                    logical_bytes: e.logical_bytes,
                    modified: dates.get(i).and_then(|d| match d.modified {
                        MetadataValue::Known { value, .. } => Some(value),
                        _ => None,
                    }),
                })
                .collect(),
            scan_coverage: inventory.report.coverage,
            entries_omitted: inventory.entries_omitted,
        })
    }
    pub fn scope(&self) -> AnalysisScope {
        AnalysisScope {
            source: "retained_inventory",
            scan_coverage: self.scan_coverage,
            retained_entries: self.entries.len(),
            omitted_inventory_entries: self.entries_omitted,
            exhaustive: self.scan_coverage == Coverage::Complete && self.entries_omitted == 0,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisScope {
    pub source: &'static str,
    pub scan_coverage: Coverage,
    pub retained_entries: usize,
    pub omitted_inventory_entries: u64,
    pub exhaustive: bool,
}
#[derive(Debug, Default, Clone)]
pub struct SearchQuery {
    pub name: Option<String>,
    pub path: Option<String>,
    pub extension: Option<String>,
    /// Relative to the root; native globset semantics, no content search.
    pub glob: Option<String>,
    pub kind: Option<InventoryKind>,
    pub min_bytes: Option<u64>,
    pub max_bytes: Option<u64>,
    pub modified_after: Option<SystemTime>,
    pub modified_before: Option<SystemTime>,
    pub case_sensitive: bool,
}
#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub scope: AnalysisScope,
    pub indices: Vec<usize>,
    pub matching_entries: usize,
    pub omitted_results: usize,
    pub unknown_dates: usize,
    pub unknown_sizes: usize,
    pub complete: bool,
    pub unsearchable_native_text: usize,
    pub cancelled: bool,
    /// All observed matches, before limiting the result page.
    pub statistics: Statistics,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Counts {
    pub files: u64,
    pub logical_bytes: u64,
    pub unknown_sizes: u64,
    pub overflow: bool,
}
impl Counts {
    fn add(&mut self, bytes: Option<u64>) {
        self.files += 1;
        match bytes {
            Some(bytes) => {
                if let Some(sum) = self.logical_bytes.checked_add(bytes) {
                    self.logical_bytes = sum;
                } else {
                    self.overflow = true;
                }
            }
            None => self.unknown_sizes += 1,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct ExtensionBucket {
    pub extension: NativePath,
    pub counts: Counts,
}
#[derive(Debug, Serialize)]
pub struct Statistics {
    pub basis: &'static str,
    pub totals: Counts,
    pub extensions: Vec<ExtensionBucket>,
    /// [empty, 1..1023 B, 1 KiB..<1 MiB, 1 MiB..<1 GiB, >=1 GiB, unknown].
    pub size_classes: [Counts; 6],
}
impl Statistics {
    fn from_indices(snapshot: &AnalysisSnapshot, indices: &[usize]) -> Self {
        let mut totals = Counts::default();
        let mut sizes: [Counts; 6] = Default::default();
        let mut extensions = BTreeMap::<std::ffi::OsString, Counts>::new();
        for &i in indices {
            let e = &snapshot.entries[i];
            if e.kind != InventoryKind::File {
                continue;
            }
            totals.add(e.logical_bytes);
            let class = match e.logical_bytes {
                Some(0) => 0,
                Some(1..=1023) => 1,
                Some(1024..=1_048_575) => 2,
                Some(1_048_576..=1_073_741_823) => 3,
                Some(_) => 4,
                None => 5,
            };
            sizes[class].add(e.logical_bytes);
            extensions
                .entry(e.path.extension().unwrap_or_default().to_owned())
                .or_default()
                .add(e.logical_bytes);
        }
        Self {
            basis: "matched_regular_file_paths_logical_bytes",
            totals,
            extensions: extensions
                .into_iter()
                .map(|(extension, counts)| ExtensionBucket {
                    extension: NativePath::from(std::path::Path::new(&extension)),
                    counts,
                })
                .collect(),
            size_classes: sizes,
        }
    }
}
/// AND criteria and native path order. Invalid native text is skipped only when
/// a text predicate needs it. Locale-independent Unicode lowercase is used.
pub fn search(
    snapshot: &AnalysisSnapshot,
    query: &SearchQuery,
    max_results: usize,
    cancel: &CancellationToken,
) -> Result<SearchResult, String> {
    if snapshot.entries.len() > MAX_ANALYSIS_ENTRIES
        || max_results > MAX_ANALYSIS_ENTRIES
        || max_results == 0
    {
        return Err("Analysis/result limit must be between 1 and 50000".into());
    }
    let prepared = PreparedQuery::new(query)?;
    let mut indices = Vec::new();
    let mut unknown_dates = 0;
    let mut unknown_sizes = 0;
    let mut unsearchable_native_text = 0;
    let root = snapshot.entries.first().map(|e| e.path.as_path());
    for (i, e) in snapshot.entries.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        match prepared.evaluate(e, root) {
            Evaluation::Match => {}
            Evaluation::NoMatch => continue,
            Evaluation::UnknownSize => {
                unknown_sizes += 1;
                continue;
            }
            Evaluation::UnknownDate => {
                unknown_dates += 1;
                continue;
            }
            Evaluation::NativeText => {
                unsearchable_native_text += 1;
                continue;
            }
        }
        indices.push(i);
    }
    indices.sort_unstable_by(|&a, &b| {
        snapshot.entries[a]
            .path
            .cmp(&snapshot.entries[b].path)
            .then(a.cmp(&b))
    });
    let statistics = Statistics::from_indices(snapshot, &indices);
    let matching_entries = indices.len();
    indices.truncate(max_results);
    let complete = snapshot.scope().exhaustive
        && !cancel.is_cancelled()
        && matching_entries == indices.len()
        && unknown_dates == 0
        && unknown_sizes == 0
        && unsearchable_native_text == 0;
    Ok(SearchResult {
        scope: snapshot.scope(),
        omitted_results: matching_entries - indices.len(),
        indices,
        matching_entries,
        unknown_dates,
        unknown_sizes,
        complete,
        unsearchable_native_text,
        cancelled: cancel.is_cancelled(),
        statistics,
    })
}

/// One compiled predicate for retained and persisted inventories. No source I/O.
pub struct PreparedQuery {
    query: SearchQuery,
    glob: Option<globset::GlobMatcher>,
    name: Option<String>,
    path: Option<String>,
    extension: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evaluation {
    Match,
    NoMatch,
    UnknownSize,
    UnknownDate,
    NativeText,
}
impl PreparedQuery {
    pub fn new(query: &SearchQuery) -> Result<Self, String> {
        if query
            .min_bytes
            .zip(query.max_bytes)
            .is_some_and(|(a, b)| a > b)
            || query
                .modified_after
                .zip(query.modified_before)
                .is_some_and(|(a, b)| a > b)
        {
            return Err("Invalid inclusive range".into());
        }
        for s in [&query.name, &query.path, &query.extension, &query.glob]
            .into_iter()
            .flatten()
        {
            if s.len() > MAX_QUERY_BYTES {
                return Err("Query exceeds 4096 bytes".into());
            }
        }
        let glob = query
            .glob
            .as_ref()
            .map(|g| {
                globset::GlobBuilder::new(g)
                    .case_insensitive(!query.case_sensitive)
                    .literal_separator(true)
                    .build()
                    .map(|g| g.compile_matcher())
            })
            .transpose()
            .map_err(|e| e.to_string())?;
        let folded = |s: &str| {
            if query.case_sensitive {
                s.to_owned()
            } else {
                s.to_lowercase()
            }
        };
        let name = query.name.as_deref().map(folded);
        let path = query.path.as_deref().map(folded);
        let extension = query
            .extension
            .as_deref()
            .map(|s| folded(s.trim_start_matches('.')));
        Ok(Self {
            query: query.clone(),
            glob,
            name,
            path,
            extension,
        })
    }
    pub fn evaluate(&self, e: &AnalysisEntry, root: Option<&std::path::Path>) -> Evaluation {
        self.evaluate_path(&e.path, e.kind, e.logical_bytes, e.modified, root)
    }
    /// Necessary folded text for the simple name predicate. The exact
    /// predicate still applies to every candidate, including case-sensitive
    /// ASCII matches. The returned flag restricts the gap count to files.
    /// A case-sensitive Unicode substring can't use this
    /// filter because folding can depend on surrounding characters.
    pub fn folded_name_prefilter(&self) -> Option<(std::borrow::Cow<'_, str>, bool)> {
        let SearchQuery {
            name: Some(_),
            path: None,
            extension: None,
            glob: None,
            kind: kind @ (Some(InventoryKind::File) | None),
            min_bytes: None,
            max_bytes: None,
            modified_after: None,
            modified_before: None,
            case_sensitive,
        } = &self.query
        else {
            return None;
        };
        let needle = self.name.as_deref()?;
        let folded = if *case_sensitive {
            if !needle.is_ascii() {
                return None;
            }
            std::borrow::Cow::Owned(needle.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(needle)
        };
        Some((folded, kind.is_some()))
    }
    /// Evaluate a borrowed recorded path with the same predicate as retained
    /// entries. Disk-backed scans can reuse their bounded path buffer.
    pub fn evaluate_path(
        &self,
        entry_path: &std::path::Path,
        kind: InventoryKind,
        logical_bytes: Option<u64>,
        modified: Option<SystemTime>,
        root: Option<&std::path::Path>,
    ) -> Evaluation {
        let query = &self.query;
        let (name, path, extension, glob) = (&self.name, &self.path, &self.extension, &self.glob);
        fn folded(s: &str, case_sensitive: bool) -> std::borrow::Cow<'_, str> {
            if case_sensitive || (s.is_ascii() && !s.bytes().any(|b| b.is_ascii_uppercase())) {
                std::borrow::Cow::Borrowed(s)
            } else {
                std::borrow::Cow::Owned(s.to_lowercase())
            }
        }
        if query.kind.is_some_and(|k| k != kind) {
            return Evaluation::NoMatch;
        }
        if (query.min_bytes.is_some() || query.max_bytes.is_some()) && logical_bytes.is_none() {
            return Evaluation::UnknownSize;
        }
        if query
            .min_bytes
            .is_some_and(|v| logical_bytes.is_none_or(|b| b < v))
            || query
                .max_bytes
                .is_some_and(|v| logical_bytes.is_none_or(|b| b > v))
        {
            return Evaluation::NoMatch;
        }
        if query.modified_after.is_some() || query.modified_before.is_some() {
            let Some(modified) = modified else {
                return Evaluation::UnknownDate;
            };
            if query.modified_after.is_some_and(|v| modified < v)
                || query.modified_before.is_some_and(|v| modified > v)
            {
                return Evaluation::NoMatch;
            }
        }
        let mut matches = true;
        for (expected, actual, equal) in [
            (
                name,
                entry_path.file_name().unwrap_or(entry_path.as_os_str()),
                false,
            ),
            (path, entry_path.as_os_str(), false),
            (extension, entry_path.extension().unwrap_or_default(), true),
        ] {
            if let Some(expected) = expected {
                let Some(actual) = actual.to_str() else {
                    return Evaluation::NativeText;
                };
                let actual = folded(actual, query.case_sensitive);
                if if equal {
                    actual.as_ref() != expected.as_str()
                } else {
                    !actual.contains(expected)
                } {
                    matches = false;
                    break;
                }
            }
        }
        if !matches {
            return Evaluation::NoMatch;
        }
        if glob.as_ref().is_some_and(|g| {
            !g.is_match(
                root.and_then(|r| entry_path.strip_prefix(r).ok())
                    .unwrap_or(entry_path),
            )
        }) {
            return Evaluation::NoMatch;
        }
        Evaluation::Match
    }
}
