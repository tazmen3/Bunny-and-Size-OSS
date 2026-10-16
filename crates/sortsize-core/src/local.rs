//! Streaming native acquisition: one listing and a bounded stack of directories.
//! No buffered file siblings or temporary on-disk inventory.
use crate::{
    CancellationToken, ScanFailure, ScanIssue, ScanOptions, TraversalLimits, TraversalStats,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::{FileType, ReadDir};
use std::path::{Path, PathBuf};

pub(crate) enum Observation {
    File {
        bytes: u64,
        hardlinked: Option<bool>,
    },
    Directory {
        depth_limited: bool,
        resource_limited: bool,
    },
    Symlink,
    Special,
    Excluded,
    Error(ScanIssue),
}

pub(crate) struct PendingDirectory {
    pub(crate) path: Box<Path>,
    pub(crate) depth: usize,
}

pub(crate) struct LocalSource {
    root: PathBuf,
    exclusions: GlobSet,
    effective_exclusions: Vec<String>,
    max_depth: Option<usize>,
}

pub(crate) struct LocalWalker {
    source: LocalSource,
    root_pending: bool,
    pending: Vec<PendingDirectory>,
    pending_bytes: usize,
    current: Option<(PendingDirectory, ReadDir)>,
    limits: TraversalLimits,
    pub(crate) stats: TraversalStats,
    cancellation: CancellationToken,
}

impl LocalSource {
    pub(crate) fn new(root: &Path, options: &ScanOptions) -> Result<Self, ScanFailure> {
        if options.max_open_directories == 0
            || options.traversal_limits.max_pending_directories == 0
            || options.traversal_limits.max_pending_path_bytes == 0
            || !(1..=32).contains(&options.concurrency.workers)
            || options.concurrency.max_buffered_observations == 0
            || options.concurrency.max_buffered_path_bytes == 0
        {
            return Err(ScanFailure::InvalidOptions(
                "Les limites du parcours doivent être positives et le nombre de travailleurs compris entre 1 et 32".into(),
            ));
        }
        let metadata = std::fs::symlink_metadata(root).map_err(|error| ScanFailure::RootIo {
            path: root.to_owned(),
            error,
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ScanFailure::InvalidRoot(root.to_owned()));
        }
        let root = std::fs::canonicalize(root).map_err(|error| ScanFailure::RootIo {
            path: root.to_owned(),
            error,
        })?;
        let effective_exclusions = platform_exclusions(&root, &options.exclusions);
        let mut builder = GlobSetBuilder::new();
        for pattern in &effective_exclusions {
            builder
                .add(Glob::new(pattern).map_err(|e| ScanFailure::InvalidOptions(e.to_string()))?);
        }
        let exclusions = builder
            .build()
            .map_err(|e| ScanFailure::InvalidOptions(e.to_string()))?;
        Ok(Self {
            root,
            exclusions,
            effective_exclusions,
            max_depth: options.max_depth,
        })
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn effective_exclusions(&self) -> &[String] {
        &self.effective_exclusions
    }
    pub(crate) fn root_observation(&self) -> Observation {
        match std::fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                self.observe(&self.root, 0, metadata.file_type())
            }
            Ok(_) => changed(&self.root),
            Err(error) => Observation::Error(ScanIssue::io(&self.root, "metadata", &error)),
        }
    }
    fn excluded(&self, path: &Path, directory: bool) -> bool {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        if self.exclusions.is_match(relative) {
            return true;
        }
        if directory {
            let mut with_separator = relative.as_os_str().to_owned();
            with_separator.push(std::path::MAIN_SEPARATOR_STR);
            return self.exclusions.is_match(Path::new(&with_separator));
        }
        false
    }
    pub(crate) fn observe(&self, path: &Path, depth: usize, expected: FileType) -> Observation {
        if depth > 0 && self.excluded(path, expected.is_dir()) {
            return Observation::Excluded;
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => return Observation::Error(ScanIssue::io(path, "metadata", &error)),
        };
        let kind = metadata.file_type();
        if kind != expected {
            return changed(path);
        }
        if kind.is_symlink() {
            Observation::Symlink
        } else if kind.is_dir() {
            let depth_limited = self.max_depth == Some(depth);
            Observation::Directory {
                depth_limited,
                resource_limited: false,
            }
        } else if kind.is_file() {
            #[cfg(unix)]
            let hardlinked = {
                use std::os::unix::fs::MetadataExt;
                Some(metadata.nlink() > 1)
            };
            #[cfg(not(unix))]
            let hardlinked = None;
            Observation::File {
                bytes: metadata.len(),
                hardlinked,
            }
        } else {
            Observation::Special
        }
    }
}
impl LocalWalker {
    pub(crate) fn new(
        root: &Path,
        options: &ScanOptions,
        cancellation: &CancellationToken,
    ) -> Result<Self, ScanFailure> {
        Ok(Self {
            source: LocalSource::new(root, options)?,
            root_pending: true,
            pending: Vec::new(),
            pending_bytes: 0,
            current: None,
            limits: options.traversal_limits,
            stats: TraversalStats {
                worker_threads: 1,
                ..Default::default()
            },
            cancellation: cancellation.clone(),
        })
    }
    pub(crate) fn root(&self) -> &Path {
        self.source.root()
    }
    pub(crate) fn effective_exclusions(&self) -> &[String] {
        self.source.effective_exclusions()
    }
    fn defer(&mut self, path: &Path, depth: usize) -> bool {
        let bytes = path.as_os_str().as_encoded_bytes().len();
        if self.pending.len() == self.limits.max_pending_directories
            || bytes > self.limits.max_pending_path_bytes - self.pending_bytes
        {
            return false;
        }
        self.pending.push(PendingDirectory {
            path: path.to_owned().into_boxed_path(),
            depth,
        });
        self.pending_bytes += bytes;
        self.stats.peak_pending_directories =
            self.stats.peak_pending_directories.max(self.pending.len());
        self.stats.peak_pending_path_bytes =
            self.stats.peak_pending_path_bytes.max(self.pending_bytes);
        true
    }

    fn observe(&mut self, path: &Path, depth: usize, expected: FileType) -> Observation {
        let mut observation = self.source.observe(path, depth, expected);
        if let Observation::Directory {
            depth_limited: false,
            resource_limited,
        } = &mut observation
        {
            *resource_limited = !self.defer(path, depth);
        }
        observation
    }
}

pub(crate) fn open_directory(path: &Path) -> Result<ReadDir, Observation> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::read_dir(path)
                .map_err(|error| Observation::Error(ScanIssue::io(path, "list", &error)))
        }
        Ok(_) => Err(changed(path)),
        Err(error) => Err(Observation::Error(ScanIssue::io(path, "list", &error))),
    }
}

fn changed(path: &Path) -> Observation {
    Observation::Error(ScanIssue {
        path: path.into(),
        operation: "metadata",
        kind: "ChangedDuringScan".into(),
        message: "Le type de l'entrée a changé pendant l'analyse".into(),
    })
}
fn platform_exclusions(root: &Path, user_exclusions: &[String]) -> Vec<String> {
    #[cfg(target_os = "linux")]
    if root == Path::new("/") {
        return user_exclusions
            .iter()
            .cloned()
            .chain(["proc", "sys", "dev"].map(String::from))
            .collect();
    }
    #[cfg(not(target_os = "linux"))]
    let _ = root;
    user_exclusions.to_vec()
}
impl Iterator for LocalWalker {
    type Item = (PathBuf, Observation);
    fn next(&mut self) -> Option<Self::Item> {
        // Check while opening empty directories too: a wide empty set must not
        // become one uninterruptible call to next().
        while !self.cancellation.is_cancelled() {
            if self.root_pending {
                self.root_pending = false;
                let path = self.source.root.clone();
                let mut observation = self.source.root_observation();
                if let Observation::Directory {
                    depth_limited: false,
                    resource_limited,
                } = &mut observation
                {
                    *resource_limited = !self.defer(&path, 0);
                }
                return Some((path, observation));
            }
            if let Some((directory, listing)) = &mut self.current {
                let item = listing.next();
                let depth = directory.depth + 1;
                match item {
                    Some(Ok(entry)) => {
                        let path = entry.path();
                        let observation = match entry.file_type() {
                            Ok(expected) => self.observe(&path, depth, expected),
                            Err(error) => {
                                Observation::Error(ScanIssue::io(&path, "metadata", &error))
                            }
                        };
                        return Some((path, observation));
                    }
                    Some(Err(error)) => {
                        let path = directory.path.to_path_buf();
                        return Some((
                            path.clone(),
                            Observation::Error(ScanIssue::io(&path, "list", &error)),
                        ));
                    }
                    None => self.current = None,
                }
            }
            let directory = self.pending.pop()?;
            self.pending_bytes -= directory.path.as_os_str().as_encoded_bytes().len();
            // Re-check deferred directories before opening: an already-replaced
            // symlink must not be followed. This is not an atomic filesystem snapshot.
            let listing = open_directory(&directory.path);
            match listing {
                Ok(listing) => {
                    self.current = Some((directory, listing));
                    self.stats.peak_open_directories = 1;
                }
                Err(observation) => return Some((directory.path.into_path_buf(), observation)),
            }
        }
        None
    }
}
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn global_linux_scan_prunes_pseudo_filesystems_before_descent() {
        let source = LocalSource::new(Path::new("/"), &ScanOptions::default()).unwrap();
        for path in ["/proc", "/sys", "/dev"] {
            assert!(source.excluded(Path::new(path), true));
        }
        assert!(!source.excluded(Path::new("/home"), true));
        assert_eq!(source.effective_exclusions().len(), 3);
    }
}
