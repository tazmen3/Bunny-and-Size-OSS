use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A display value plus lossless native data when the path is not Unicode.
#[derive(Debug, Serialize)]
pub struct NativePath {
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<RawPath>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "encoding", content = "units", rename_all = "snake_case")]
pub enum RawPath {
    UnixBytes(Vec<u8>),
    WindowsUtf16(Vec<u16>),
}

impl From<&Path> for NativePath {
    fn from(path: &Path) -> Self {
        let raw = if path.to_str().is_some() {
            None
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                Some(RawPath::UnixBytes(path.as_os_str().as_bytes().to_vec()))
            }
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                Some(RawPath::WindowsUtf16(
                    path.as_os_str().encode_wide().collect(),
                ))
            }
            #[cfg(not(any(unix, windows)))]
            {
                None
            }
        };
        Self {
            display: path.to_string_lossy().into_owned(),
            raw,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Relative globs. Matching directories are pruned before descent.
    pub exclusions: Vec<String>,
    /// Root is depth zero. None scans every accessible level.
    pub max_depth: Option<usize>,
    /// Bound stored error details; total error count is always retained.
    pub max_errors: usize,
    /// Upper bound on simultaneously open directories; must be positive.
    /// Also caps effective concurrency; the sequential adapter uses one.
    pub max_open_directories: usize,
    /// Bounds deferred directory paths separately from the optional inventory.
    pub traversal_limits: TraversalLimits,
    pub concurrency: ConcurrencyOptions,
}

/// Bounded transfer between acquisition workers and the single aggregator.
/// Limits apply to the shared queue; each worker and the consumer additionally
/// retain at most one batch (32 observations and max_buffered_path_bytes).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConcurrencyOptions {
    pub workers: usize,
    pub max_buffered_observations: usize,
    pub max_buffered_path_bytes: usize,
}
impl Default for ConcurrencyOptions {
    fn default() -> Self {
        Self {
            workers: 1,
            max_buffered_observations: 1024,
            max_buffered_path_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TraversalLimits {
    pub max_pending_directories: usize,
    /// Sum of encoded native path lengths, excluding allocation overhead.
    pub max_pending_path_bytes: usize,
}

impl Default for TraversalLimits {
    fn default() -> Self {
        Self {
            max_pending_directories: 65_536,
            max_pending_path_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TraversalStats {
    pub peak_pending_directories: usize,
    pub peak_pending_path_bytes: usize,
    pub peak_open_directories: usize,
    pub worker_threads: usize,
    pub peak_buffered_observations: usize,
    pub peak_buffered_path_bytes: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            exclusions: Vec::new(),
            max_depth: None,
            max_errors: 100,
            max_open_directories: 16,
            traversal_limits: TraversalLimits::default(),
            concurrency: ConcurrencyOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Cancelled,
}

#[derive(Debug, Serialize)]
pub struct ScanIssue {
    pub path: NativePath,
    pub operation: &'static str,
    pub kind: String,
    pub message: String,
}

impl ScanIssue {
    pub(crate) fn io(path: &Path, operation: &'static str, error: &std::io::Error) -> Self {
        Self {
            path: path.into(),
            operation,
            kind: format!("{:?}", error.kind()),
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub schema_version: u32,
    pub provider: &'static str,
    pub root: NativePath,
    pub started_at_unix_ms: u128,
    pub duration_ms: u128,
    pub coverage: Coverage,
    pub files: u64,
    /// Includes the root and any directory whose listing failed.
    pub directories: u64,
    /// Logical lengths summed per file path, including separate hardlink paths.
    pub logical_bytes: u64,
    pub logical_size_basis: &'static str,
    pub allocated_bytes: Option<u64>,
    pub potentially_reclaimable_bytes: Option<u64>,
    pub symlinks_skipped: u64,
    pub special_entries_skipped: u64,
    pub excluded_entries: u64,
    pub depth_limited_directories: u64,
    /// Observed directories whose contents were skipped because a traversal
    /// budget was exhausted. Their unseen descendants cannot be counted.
    pub resource_limited_directories: u64,
    pub traversal_limits: TraversalLimits,
    pub traversal_stats: TraversalStats,
    pub concurrency: ConcurrencyOptions,
    /// Unix only at this stage; this is not a deduplicated size.
    pub hardlinked_file_paths: Option<u64>,
    pub errors_count: u64,
    pub errors_omitted: u64,
    pub errors: Vec<ScanIssue>,
    pub follow_symlinks: bool,
    pub exclusions: Vec<String>,
    pub max_depth: Option<usize>,
    pub mount_policy: &'static str,
}

#[derive(Debug)]
pub struct Progress {
    pub current_path: PathBuf,
    pub files: u64,
    pub directories: u64,
    pub logical_bytes: u64,
    pub errors_count: u64,
}
