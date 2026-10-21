//! Read-only desktop sources, independent of scanning and Slint.
//!
//! No summed capacity: aliases, subvolumes and storage pools may share space.
//! A measurement is not a snapshot, an access guarantee, or a file-size total.
use crate::{MetadataValue, ValueAccuracy};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
pub use linux::NativeVolumes;
#[cfg(windows)]
pub use windows::{NativeVolumes, unc_source};

/// Native namespace identity, not a physical disk or a persistent hardware ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VolumeIdentity {
    Linux {
        major: u32,
        minor: u32,
        filesystem: OsString,
        source: OsString,
    },
    WindowsGuid(OsString),
    /// Fallback location identity; aliases are not proven distinct storage.
    WindowsPath(PathBuf),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Local,
    Removable,
    Optical,
    Remote,
    Unknown,
}
impl SourceKind {
    pub fn may_contact_network(self) -> bool {
        matches!(self, Self::Remote | Self::Unknown)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeAvailability {
    NotProbed,
    Available,
    AccessDenied,
    Unavailable,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeMount {
    pub path: PathBuf,
    /// Linux mount ID; absent on Windows.
    pub mount_id: Option<u64>,
    /// Linux root inside the filesystem, including bind/subvolume roots.
    pub root: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub identity: VolumeIdentity,
    pub label: MetadataValue<OsString>,
    pub filesystem: MetadataValue<OsString>,
    pub kind: SourceKind,
    pub mounts: Vec<VolumeMount>,
    pub capacity: VolumeCapacity,
    pub availability: VolumeAvailability,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeCapacity {
    /// Whole filesystem capacity, never a caller quota substituted silently.
    pub total: MetadataValue<u64>,
    pub free_total: MetadataValue<u64>,
    pub available: MetadataValue<u64>,
    /// Windows caller quota total, where provided separately by the OS.
    pub caller_total: MetadataValue<u64>,
    /// total - free_total; reserved free space is not counted as used.
    pub used: MetadataValue<u64>,
}
pub(crate) fn known<T>(value: T) -> MetadataValue<T> {
    MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    }
}
impl VolumeCapacity {
    pub fn absent(state: MetadataValue<u64>) -> Self {
        Self {
            total: state,
            free_total: state,
            available: state,
            caller_total: state,
            used: state,
        }
    }
    /// Reject contradictory properties without turning missing values into zero.
    pub fn from_bytes(
        total: MetadataValue<u64>,
        free_total: MetadataValue<u64>,
        available: MetadataValue<u64>,
    ) -> Self {
        let mut result = Self {
            total,
            free_total,
            available,
            caller_total: MetadataValue::NotCollected,
            used: MetadataValue::Unknown,
        };
        if let (MetadataValue::Known { value: t, .. }, MetadataValue::Known { value: f, .. }) =
            (total, free_total)
        {
            if let Some(used) = t.checked_sub(f) {
                result.used = known(used);
            } else {
                result.free_total = MetadataValue::Unknown;
            }
        }
        if let MetadataValue::Known { value: a, .. } = available {
            let exceeds_total = matches!(total, MetadataValue::Known { value: t, .. } if a > t);
            let exceeds_free = matches!(free_total, MetadataValue::Known { value: f, .. } if a > f);
            if exceeds_total || exceeds_free {
                result.available = MetadataValue::Unknown;
            }
        }
        result
    }
    pub fn utilization_percent(self) -> MetadataValue<f64> {
        match (self.total, self.used) {
            (
                MetadataValue::Known { value: total, .. },
                MetadataValue::Known { value: used, .. },
            ) if total > 0 && used <= total => known(used as f64 / total as f64 * 100.0),
            _ => MetadataValue::Unknown,
        }
    }
}
#[derive(Clone, Debug)]
pub struct VolumeProbe {
    pub capacity: VolumeCapacity,
    pub availability: VolumeAvailability,
    pub label: MetadataValue<OsString>,
    pub filesystem: MetadataValue<OsString>,
}
impl VolumeProbe {
    pub fn error(error: &io::Error) -> Self {
        #[cfg(windows)]
        let absent_media = matches!(error.raw_os_error(), Some(15 | 21 | 1112 | 1167));
        #[cfg(not(windows))]
        let absent_media = matches!(
            error.raw_os_error(),
            Some(libc::ENODEV | libc::ENXIO | libc::ESTALE)
        );
        let (availability, state) = match error.kind() {
            _ if absent_media => (VolumeAvailability::Unavailable, MetadataValue::Unknown),
            io::ErrorKind::PermissionDenied => (
                VolumeAvailability::AccessDenied,
                MetadataValue::AccessDenied,
            ),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
                (VolumeAvailability::Unavailable, MetadataValue::Unknown)
            }
            io::ErrorKind::Unsupported => (VolumeAvailability::Unknown, MetadataValue::Unsupported),
            _ => (VolumeAvailability::Unknown, MetadataValue::Unknown),
        };
        Self {
            capacity: VolumeCapacity::absent(state),
            availability,
            label: MetadataValue::Unknown,
            filesystem: MetadataValue::Unknown,
        }
    }
}
/// Injection boundary: enumeration must not probe remote paths or read contents.
pub trait VolumeProvider {
    fn enumerate(&self) -> io::Result<Vec<Volume>>;
    fn probe(&self, path: &Path) -> VolumeProbe;
}
#[derive(Clone, Debug, Default)]
pub struct DiscoveryOptions {
    /// Explicit caller authorization for potentially remote native queries.
    pub probe_remote: bool,
    /// One explicitly supplied UNC path. Requires probe_remote; no UNC listing.
    pub explicit_unc: Option<PathBuf>,
}
#[derive(Debug)]
pub struct VolumeDiscovery {
    pub volumes: Vec<Volume>,
    pub enumeration_time: Duration,
    pub capacity_time: Duration,
    pub capacity_calls: usize,
}
/// Blocking operation: desktop callers must use a dedicated worker.
pub fn discover_volumes(options: DiscoveryOptions) -> io::Result<VolumeDiscovery> {
    discover_with(&NativeVolumes, options)
}
pub fn discover_with(
    provider: &impl VolumeProvider,
    options: DiscoveryOptions,
) -> io::Result<VolumeDiscovery> {
    let start = Instant::now();
    let mut volumes = provider.enumerate()?;
    if let Some(path) = options.explicit_unc {
        if !options.probe_remote {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        #[cfg(windows)]
        if !volumes
            .iter()
            .any(|v| v.mounts.iter().any(|m| m.path == path))
        {
            volumes.push(unc_source(&path)?);
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            return Err(io::ErrorKind::Unsupported.into());
        }
    }
    let enumeration_time = start.elapsed();
    let start = Instant::now();
    let mut capacity_calls = 0;
    for volume in &mut volumes {
        if volume.kind.may_contact_network() && !options.probe_remote {
            continue;
        }
        // Prefer the first reachable alias. A denied or disappeared preferred
        // mount must not conceal another accessible path to the same storage.
        let mut selected = None;
        for (index, mount) in volume.mounts.iter().enumerate() {
            let probe = provider.probe(&mount.path);
            capacity_calls += 1;
            let available = probe.availability == VolumeAvailability::Available;
            if selected.is_none()
                || available
                || probe.availability == VolumeAvailability::AccessDenied
            {
                selected = Some((index, probe));
            }
            if available {
                break;
            }
        }
        if let Some((index, probe)) = selected {
            volume.mounts.swap(0, index);
            volume.capacity = probe.capacity;
            volume.availability = probe.availability;
            if matches!(probe.label, MetadataValue::Known { .. }) {
                volume.label = probe.label;
            }
            if matches!(probe.filesystem, MetadataValue::Known { .. }) {
                volume.filesystem = probe.filesystem;
            }
        }
    }
    Ok(VolumeDiscovery {
        volumes,
        enumeration_time,
        capacity_time: start.elapsed(),
        capacity_calls,
    })
}
#[cfg(test)]
mod tests;
