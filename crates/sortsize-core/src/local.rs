//! Streaming native acquisition: one listing and a bounded stack of directories.
//! No buffered file siblings or temporary on-disk inventory.
#[cfg(not(windows))]
use crate::EntryOwner;
use crate::{
    CancellationToken, EntryAllocation, EntryDates, ScanFailure, ScanIssue, ScanOptions,
    TraversalLimits, TraversalStats,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::{FileType, ReadDir};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub(crate) struct CollectFields {
    pub dates: bool,
    pub allocation: bool,
    pub owner: bool,
    // Private export spool: prune by native identity, never a lossy glob.
    pub private_exclusion: Option<std::sync::Arc<Path>>,
}

pub(crate) enum Observation {
    File {
        bytes: u64,
        hardlinked: Option<bool>,
        dates: Option<EntryDates>,
        owner: Option<crate::owner::ObservedOwner>,
        allocation: Option<EntryAllocation>,
    },
    Directory {
        depth_limited: bool,
        resource_limited: bool,
        dates: Option<EntryDates>,
        owner: Option<crate::owner::ObservedOwner>,
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
    fields: CollectFields,
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
    pub(crate) fn new(
        root: &Path,
        options: &ScanOptions,
        fields: CollectFields,
    ) -> Result<Self, ScanFailure> {
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
            fields,
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
        if self.fields.private_exclusion.as_deref() == Some(path) {
            return true;
        }
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
            #[cfg(windows)]
            let (metadata, owner) = if self.fields.owner {
                let (metadata, _, owner) =
                    crate::allocation::platform::collect_node(path, metadata, false, true);
                (metadata, owner.map(crate::owner::observed))
            } else {
                (metadata, None)
            };
            #[cfg(not(windows))]
            let owner = self
                .fields
                .owner
                .then(|| crate::owner::observed(EntryOwner::collect(&metadata)));
            Observation::Directory {
                depth_limited,
                resource_limited: false,
                dates: self.fields.dates.then(|| EntryDates::collect(&metadata)),
                owner,
            }
        } else if kind.is_file() {
            #[cfg(unix)]
            let hardlinked = {
                use std::os::unix::fs::MetadataExt;
                Some(metadata.nlink() > 1)
            };
            #[cfg(not(unix))]
            let hardlinked = None;
            #[cfg(windows)]
            let (metadata, allocation, owner) = if self.fields.owner {
                let (metadata, allocation, owner) = crate::allocation::platform::collect_node(
                    path,
                    metadata,
                    self.fields.allocation,
                    true,
                );
                (
                    metadata,
                    self.fields.allocation.then_some(allocation),
                    owner.map(crate::owner::observed),
                )
            } else if self.fields.allocation {
                let (metadata, allocation) = crate::allocation::collect_file(path, metadata);
                (metadata, Some(allocation), None)
            } else {
                (metadata, None, None)
            };
            #[cfg(not(windows))]
            let (metadata, allocation) = if self.fields.allocation {
                let (metadata, allocation) = crate::allocation::collect_file(path, metadata);
                (metadata, Some(allocation))
            } else {
                (metadata, None)
            };
            #[cfg(not(windows))]
            let owner = self
                .fields
                .owner
                .then(|| crate::owner::observed(EntryOwner::collect(&metadata)));
            Observation::File {
                bytes: metadata.len(),
                hardlinked,
                dates: self.fields.dates.then(|| EntryDates::collect(&metadata)),
                owner,
                allocation,
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
        fields: CollectFields,
    ) -> Result<Self, ScanFailure> {
        Ok(Self {
            source: LocalSource::new(root, options, fields)?,
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
            ..
        } = &mut observation
        {
            *resource_limited = !self.defer(path, depth);
        }
        observation
    }
}

pub(crate) fn open_directory(path: &Path) -> Result<ReadDir, ScanIssue> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::read_dir(path).map_err(|error| ScanIssue::io(path, "list", &error))
        }
        Ok(_) => Err(changed_issue(path)),
        Err(error) => Err(ScanIssue::io(path, "list", &error)),
    }
}

fn changed(path: &Path) -> Observation {
    Observation::Error(changed_issue(path))
}
fn changed_issue(path: &Path) -> ScanIssue {
    ScanIssue {
        path: path.into(),
        operation: "metadata",
        kind: "ChangedDuringScan".into(),
        message: "Le type de l'entrée a changé pendant l'analyse".into(),
    }
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
                    ..
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
                Err(issue) => {
                    return Some((directory.path.into_path_buf(), Observation::Error(issue)));
                }
            }
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryOwner;
    #[test]
    fn metadata_transport_sizes_are_measured_per_platform() {
        eprintln!(
            "Observation={} EntryAllocation={} EntryDates={} Event={} EntryOwner={} NativeOwnerId={} OptionalOwner={}",
            std::mem::size_of::<Observation>(),
            std::mem::size_of::<EntryAllocation>(),
            std::mem::size_of::<EntryDates>(),
            std::mem::size_of::<(PathBuf, Observation)>(),
            std::mem::size_of::<EntryOwner>(),
            std::mem::size_of::<crate::NativeOwnerId>(),
            std::mem::size_of::<Option<EntryOwner>>()
        );
        assert!(!std::mem::needs_drop::<EntryAllocation>());
    }

    #[test]
    fn deleted_before_observation_is_an_error_without_owner() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("gone");
        std::fs::write(&path, []).unwrap();
        let expected = std::fs::symlink_metadata(&path).unwrap().file_type();
        std::fs::remove_file(&path).unwrap();
        let source = LocalSource::new(
            temp.path(),
            &ScanOptions::default(),
            CollectFields {
                owner: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            source.observe(&path, 1, expected),
            Observation::Error(_)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_linux_scan_prunes_pseudo_filesystems_before_descent() {
        let source = LocalSource::new(
            Path::new("/"),
            &ScanOptions::default(),
            CollectFields::default(),
        )
        .unwrap();
        for path in ["/proc", "/sys", "/dev"] {
            assert!(source.excluded(Path::new(path), true));
        }
        assert!(!source.excluded(Path::new("/home"), true));
        assert_eq!(source.effective_exclusions().len(), 3);
    }
}
