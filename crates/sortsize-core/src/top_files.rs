//! Streaming Top 100, independent of tree retention and acquisition order.
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};

pub const TOP_FILES_COUNT: usize = 100;
/// Per-path admission limit: at most 100 × 64 KiB of encoded native paths.
/// Oversized paths are counted explicitly, never silently truncated.
pub const TOP_FILE_MAX_PATH_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct TopFile {
    pub path: PathBuf,
    pub logical_bytes: u64,
    /// Optional index in the entries of the same InventoryReport. This is a
    /// navigation hint only; retention never influences ranking or identity.
    pub retained_entry: Option<usize>,
}

// The worst retained candidate is at the root: smallest size, then largest path.
impl PartialEq for TopFile {
    fn eq(&self, other: &Self) -> bool {
        self.logical_bytes == other.logical_bytes && self.path == other.path
    }
}
impl Eq for TopFile {}
impl Ord for TopFile {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .logical_bytes
            .cmp(&self.logical_bytes)
            .then_with(|| self.path.cmp(&other.path))
    }
}
impl PartialOrd for TopFile {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Default)]
pub struct TopFiles {
    /// Logical size descending, then native Path order ascending on this platform.
    pub entries: Vec<TopFile>,
    pub retained_path_bytes: usize,
    /// If nonzero, ranking covers only observed paths within the admission limit.
    /// Scan coverage and totals are unaffected.
    pub oversized_paths: u64,
}

pub(crate) struct TopFilesBuilder {
    heap: BinaryHeap<TopFile>,
    oversized_paths: u64,
}
impl Default for TopFilesBuilder {
    fn default() -> Self {
        Self {
            heap: BinaryHeap::with_capacity(TOP_FILES_COUNT),
            oversized_paths: 0,
        }
    }
}
impl TopFilesBuilder {
    pub(crate) fn observe(&mut self, path: &Path, bytes: u64, retained_entry: Option<usize>) {
        if path.as_os_str().as_encoded_bytes().len() > TOP_FILE_MAX_PATH_BYTES {
            self.oversized_paths += 1;
            return;
        }
        // Compare borrowed paths first: rejected candidates never allocate a path.
        if self.heap.len() == TOP_FILES_COUNT {
            let worst = self.heap.peek().unwrap();
            if bytes < worst.logical_bytes
                || (bytes == worst.logical_bytes && path >= worst.path.as_path())
            {
                return;
            }
            // Drop the old path before allocating its replacement; no 101st path.
            self.heap.pop();
        }
        self.heap.push(TopFile {
            path: path.to_owned(),
            logical_bytes: bytes,
            retained_entry,
        });
    }
    pub(crate) fn finish(self) -> TopFiles {
        let entries = self.heap.into_sorted_vec();
        let retained_path_bytes = entries
            .iter()
            .map(|e| e.path.as_os_str().as_encoded_bytes().len())
            .sum();
        TopFiles {
            entries,
            retained_path_bytes,
            oversized_paths: self.oversized_paths,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_winners_ties_and_path_memory_limit_are_order_independent() {
        let mut candidates: Vec<_> = (0..50_010)
            .map(|i| (format!("root/{i:06}"), i as u64 / 3))
            .collect();
        candidates.push(("x".repeat(TOP_FILE_MAX_PATH_BYTES + 1), u64::MAX));
        let mut expected: Vec<_> = candidates
            .iter()
            .filter(|(p, _)| p.len() <= TOP_FILE_MAX_PATH_BYTES)
            .cloned()
            .collect();
        expected.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        expected.truncate(100);
        for reverse in [false, true] {
            let mut builder = TopFilesBuilder::default();
            if reverse {
                candidates.reverse();
            }
            for (path, bytes) in &candidates {
                builder.observe(Path::new(path), *bytes, None);
                assert!(builder.heap.len() <= 100);
            }
            let top = builder.finish();
            assert_eq!(top.oversized_paths, 1);
            assert!(top.retained_path_bytes <= 100 * TOP_FILE_MAX_PATH_BYTES);
            assert_eq!(
                top.entries
                    .iter()
                    .map(|e| (e.path.to_str().unwrap().to_owned(), e.logical_bytes))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
    #[test]
    fn exact_path_budget_boundary_and_zero_sizes() {
        let mut builder = TopFilesBuilder::default();
        let path = "x".repeat(TOP_FILE_MAX_PATH_BYTES);
        builder.observe(Path::new(&path), 0, None);
        builder.observe(Path::new("a"), 0, None);
        let top = builder.finish();
        assert_eq!(top.oversized_paths, 0);
        assert_eq!(top.entries[0].path, Path::new("a"));
        assert_eq!(top.retained_path_bytes, TOP_FILE_MAX_PATH_BYTES + 1);
    }
}
