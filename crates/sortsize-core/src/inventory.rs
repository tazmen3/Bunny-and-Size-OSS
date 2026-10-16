//! Optional, bounded inventory built during the same traversal as the CLI summary.
use crate::local::Observation;
use crate::scan::scan_observed;
use crate::{CancellationToken, Coverage, Progress, ScanFailure, ScanOptions, ScanReport};
use std::collections::{HashMap, hash_map::RandomState};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryKind {
    Directory,
    File,
    Symlink,
    Special,
}

#[derive(Debug)]
pub struct InventoryEntry {
    /// Native path, never reconstructed from a display string.
    pub path: PathBuf,
    /// Index of a retained ancestor. The root has no parent.
    pub parent: Option<usize>,
    pub kind: InventoryKind,
    /// Observed logical bytes; None for ignored links and special entries.
    pub logical_bytes: Option<u64>,
    /// Errors, depth limits or cancellation make a directory's size a lower bound.
    pub incomplete: bool,
}

#[derive(Debug)]
pub struct InventoryReport {
    pub report: ScanReport,
    pub entries: Vec<InventoryEntry>,
    /// Omitted tree nodes, not unscanned objects. Totals continue to accumulate.
    pub entries_omitted: u64,
    /// Encoded native path bytes retained in entries, excluding allocation overhead.
    pub retained_path_bytes: usize,
    /// Retention stopped because the next path would exceed the byte budget.
    pub path_bytes_limit_reached: bool,
}

/// Bounds retained inventory payload, not process RSS or the walker's buffers.
#[derive(Debug, Clone, Copy)]
pub struct InventoryLimits {
    pub max_entries: usize,
    /// Sum of `OsStr::as_encoded_bytes().len()` over retained paths. Native
    /// encoding is preserved; this does not require UTF-8 or POSIX paths.
    pub max_path_bytes: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_path_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Retain at most `max_entries` nodes while scanning every accessible entry.
/// Directory totals include files whose nodes could not be retained. A truncated
/// display does not change the summary coverage. Uses the default native path
/// byte budget; see `scan_inventory_with_limits` to configure both bounds.
pub fn scan_inventory(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    max_entries: usize,
    progress: impl FnMut(&Progress),
) -> Result<InventoryReport, ScanFailure> {
    scan_inventory_with_limits(
        root,
        options,
        cancellation,
        InventoryLimits {
            max_entries,
            ..InventoryLimits::default()
        },
        progress,
    )
}

/// Scan every accessible entry while retaining only a bounded prefix of nodes.
/// Directory totals include omitted descendants. Once either bound is reached,
/// retention stays stopped: later short paths must not create holes or change
/// parent relationships. The directory lookup indexes paths owned by entries;
/// hash collisions always compare the complete native path. Traversal, errors
/// and GUI projections have separate costs.
pub fn scan_inventory_with_limits(
    root: &Path,
    options: &ScanOptions,
    cancellation: &CancellationToken,
    limits: InventoryLimits,
    progress: impl FnMut(&Progress),
) -> Result<InventoryReport, ScanFailure> {
    if limits.max_entries == 0 || limits.max_path_bytes == 0 {
        return Err(ScanFailure::InvalidOptions(
            "max_entries et max_path_bytes doivent être supérieurs à zéro".into(),
        ));
    }
    let mut builder = InventoryBuilder::new(limits);
    let report = scan_observed(
        root,
        options,
        cancellation,
        progress,
        |path, observation| {
            builder.observe(path, observation);
        },
    )?;
    Ok(builder.finish(report))
}

struct InventoryBuilder {
    entries: Vec<InventoryEntry>,
    directories: DirectoryLookup,
    limits: InventoryLimits,
    omitted: u64,
    path_bytes: usize,
    path_limit_reached: bool,
}

/// Index paths already owned by entries. Hashes only choose a bucket: native
/// equality remains authoritative, including the (rare) collision chain.
#[derive(Default)]
struct DirectoryLookup {
    hasher: RandomState,
    heads: HashMap<u64, usize>,
    collisions: HashMap<usize, usize>,
}
impl DirectoryLookup {
    fn insert(&mut self, path: &Path, index: usize) {
        self.insert_key(self.hasher.hash_one(path), index);
    }
    fn insert_key(&mut self, key: u64, index: usize) {
        if let Some(previous) = self.heads.insert(key, index) {
            self.collisions.insert(index, previous);
        }
    }
    fn get(&self, path: &Path, entries: &[InventoryEntry]) -> Option<usize> {
        self.get_key(self.hasher.hash_one(path), path, entries)
    }
    fn get_key(&self, key: u64, path: &Path, entries: &[InventoryEntry]) -> Option<usize> {
        let mut index = *self.heads.get(&key)?;
        loop {
            if entries[index].path == path {
                return Some(index);
            }
            index = *self.collisions.get(&index)?;
        }
    }
}

impl InventoryBuilder {
    fn new(limits: InventoryLimits) -> Self {
        Self {
            entries: Vec::new(),
            directories: DirectoryLookup::default(),
            limits,
            omitted: 0,
            path_bytes: 0,
            path_limit_reached: false,
        }
    }

    fn directory_ancestor(&self, path: &Path) -> Option<usize> {
        path.ancestors()
            .find_map(|p| self.directories.get(p, &self.entries))
    }

    fn observe(&mut self, path: &Path, observation: &Observation) {
        let parent = path.parent().and_then(|p| self.directory_ancestor(p));
        let (kind, logical_bytes, incomplete) = match observation {
            Observation::File { bytes, .. } => {
                if let Some(parent) = parent {
                    // The shared summary rejects total overflow before returning
                    // an inventory. No successful subtree sum can overflow either.
                    let total = self.entries[parent].logical_bytes.as_mut().unwrap();
                    *total = total.saturating_add(*bytes);
                }
                (InventoryKind::File, Some(*bytes), false)
            }
            Observation::Directory {
                depth_limited,
                resource_limited,
            } => (
                InventoryKind::Directory,
                Some(0),
                *depth_limited || *resource_limited,
            ),
            Observation::Symlink => (InventoryKind::Symlink, None, false),
            Observation::Special => (InventoryKind::Special, None, false),
            Observation::Error(_) => {
                if let Some(index) = self.directory_ancestor(path) {
                    self.entries[index].incomplete = true;
                }
                return;
            }
            Observation::Excluded => return,
        };
        let path_bytes = path.as_os_str().as_encoded_bytes().len();
        if self.entries.len() < self.limits.max_entries
            && path_bytes > self.limits.max_path_bytes - self.path_bytes
        {
            self.path_limit_reached = true;
        }
        if self.entries.len() == self.limits.max_entries || self.path_limit_reached {
            if incomplete && let Some(parent) = parent {
                self.entries[parent].incomplete = true;
            }
            self.omitted += 1;
            return;
        }
        self.path_bytes += path_bytes;
        if kind == InventoryKind::Directory {
            self.directories.insert(path, self.entries.len());
        }
        self.entries.push(InventoryEntry {
            path: path.to_owned(),
            parent,
            kind,
            logical_bytes,
            incomplete,
        });
    }

    fn finish(mut self, report: ScanReport) -> InventoryReport {
        // Parents precede children, so reverse propagation is linear and avoids
        // recursive calls even for very deep directory trees.
        for index in (0..self.entries.len()).rev() {
            if self.entries[index].kind != InventoryKind::Directory {
                continue;
            }
            if report.coverage == Coverage::Cancelled {
                self.entries[index].incomplete = true;
            }
            if let Some(parent) = self.entries[index].parent {
                let bytes = self.entries[index].logical_bytes.unwrap_or(0);
                let incomplete = self.entries[index].incomplete;
                let ancestor = &mut self.entries[parent];
                let total = ancestor.logical_bytes.as_mut().unwrap();
                *total = total.saturating_add(bytes);
                ancestor.incomplete |= incomplete;
            }
        }
        if report.coverage == Coverage::Partial
            && let Some(root) = self.entries.first_mut()
        {
            root.incomplete = true;
        }
        InventoryReport {
            report,
            entries: self.entries,
            entries_omitted: self.omitted,
            retained_path_bytes: self.path_bytes,
            path_bytes_limit_reached: self.path_limit_reached,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScanIssue;
    use std::io;

    #[test]
    fn directory_hash_collisions_preserve_exact_native_identity() {
        let entries: Vec<_> = ["fixture", "fixture/été", "fixture/別"]
            .into_iter()
            .map(|path| InventoryEntry {
                path: path.into(),
                parent: None,
                kind: InventoryKind::Directory,
                logical_bytes: Some(0),
                incomplete: false,
            })
            .collect();
        let mut lookup = DirectoryLookup::default();
        for index in 0..entries.len() {
            lookup.insert_key(7, index);
        }
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(lookup.get_key(7, &entry.path, &entries), Some(index));
        }
        assert_eq!(
            lookup.get_key(7, Path::new("fixture/missing"), &entries),
            None
        );
        assert_eq!(lookup.get_key(8, &entries[0].path, &entries), None);
    }

    #[test]
    fn inaccessible_directory_marks_its_ancestors_incomplete() {
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 10,
            ..InventoryLimits::default()
        });
        let root = Path::new("fixture");
        let child = root.join("blocked");
        builder.observe(
            root,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
            },
        );
        builder.observe(
            &child,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
            },
        );
        builder.observe(
            &child,
            &Observation::Error(ScanIssue::io(
                &child,
                "list",
                &io::Error::from(io::ErrorKind::PermissionDenied),
            )),
        );
        assert!(builder.entries[1].incomplete);
        assert_eq!(builder.entries[1].logical_bytes, Some(0));
    }

    #[test]
    fn path_budget_stops_retention_even_if_later_paths_are_shorter() {
        let root = Path::new("r");
        let mut builder = InventoryBuilder::new(InventoryLimits {
            max_entries: 100,
            max_path_bytes: 8,
        });
        builder.observe(
            root,
            &Observation::Directory {
                depth_limited: false,
                resource_limited: false,
            },
        );
        builder.observe(
            Path::new("r/long-name"),
            &Observation::File {
                bytes: 3,
                hardlinked: None,
            },
        );
        builder.observe(
            Path::new("r/a"),
            &Observation::File {
                bytes: 5,
                hardlinked: None,
            },
        );
        assert_eq!(builder.entries.len(), 1);
        assert_eq!(builder.entries[0].logical_bytes, Some(8));
        assert_eq!(builder.omitted, 2);
        assert!(builder.path_limit_reached);
    }
}
