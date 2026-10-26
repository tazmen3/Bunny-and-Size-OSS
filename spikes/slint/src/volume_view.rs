//! Bounded single-flight worker. No native call, wait, join or scan on the UI.
use crate::VolumeRow;
use sortsize_core::{MetadataValue, volumes::*};
use std::{
    io,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
};

pub(crate) const LOADING: &str = "Discovering…";
pub(crate) const EMPTY: &str = "No storage source visible.";
pub(crate) const FAILED: &str = "Discovery failed. Try Refresh.";
pub(crate) const READY: &str = "Point-in-time measurements · capacities cannot be summed";
pub(crate) const MAX_ROWS: usize = 256;
type Discover = Arc<dyn Fn(DiscoveryOptions) -> io::Result<VolumeDiscovery> + Send + Sync>;
struct Job {
    generation: u64,
    receiver: Receiver<Result<Prepared, ()>>,
}
pub(crate) struct Prepared {
    pub discovery: VolumeDiscovery,
    pub limited: bool,
}
impl Prepared {
    pub(crate) fn rows_for(&self, units: crate::units::Units) -> Vec<VolumeRow> {
        self.discovery
            .volumes
            .iter()
            .map(|volume| {
                let mut row = row(volume);
                row.summary = summary(volume, units).into();
                row
            })
            .collect()
    }
    pub(crate) fn new(mut discovery: VolumeDiscovery) -> Self {
        let limited = discovery.volumes.len() > MAX_ROWS;
        discovery.volumes.truncate(MAX_ROWS);
        Self { discovery, limited }
    }
}
/// One running native operation and one latest coalesced request per window.
pub(crate) struct Sources {
    discover: Discover,
    generation: u64,
    active: Option<Job>,
    queued: Option<DiscoveryOptions>,
    pub current: Option<Prepared>,
    pub error: bool,
}
impl Default for Sources {
    fn default() -> Self {
        Self::with_provider(Arc::new(discover_volumes))
    }
}
impl Sources {
    pub(crate) fn with_provider(discover: Discover) -> Self {
        Self {
            discover,
            generation: 0,
            active: None,
            queued: None,
            current: None,
            error: false,
        }
    }
    /// Revalidate native identity and exact mount on the scan worker. A stale
    /// Linux mountpoint may remain a readable directory after unmounting.
    pub(crate) fn scan_preflight(
        &self,
        volume: &Volume,
        mount: &VolumeMount,
    ) -> crate::directory_worker::Preflight {
        let discover = self.discover.clone();
        let identity = volume.identity.clone();
        let mount = mount.clone();
        Box::new(move || {
            discover(DiscoveryOptions::default()).is_ok_and(|fresh| {
                fresh.volumes.iter().any(|volume| {
                    volume.identity == identity
                        && can_auto_scan(volume)
                        && volume.mounts.contains(&mount)
                })
            })
        })
    }
    pub fn loading(&self) -> bool {
        self.active.is_some()
    }
    pub fn refresh(&mut self, options: DiscoveryOptions) {
        self.generation = self.generation.wrapping_add(1);
        self.error = false;
        if self.active.is_some() {
            self.queued = Some(options);
        } else {
            self.launch(options);
        }
    }
    fn launch(&mut self, options: DiscoveryOptions) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let discover = self.discover.clone();
        let generation = self.generation;
        match std::thread::Builder::new()
            .name("sortsize-volumes".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    discover(options).map(Prepared::new)
                }));
                let result = result.ok().and_then(Result::ok).ok_or(());
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.active = Some(Job {
                    generation,
                    receiver,
                })
            }
            Err(_) => {
                self.error = true;
            }
        }
    }
    /// Returns true only when the visible result/loading state changed.
    pub fn poll(&mut self) -> bool {
        let Some(job) = &self.active else {
            return false;
        };
        let result = match job.receiver.try_recv() {
            Err(TryRecvError::Empty) => return false,
            Ok(result) => result,
            Err(TryRecvError::Disconnected) => Err(()),
        };
        let current = job.generation == self.generation;
        self.active = None;
        if current {
            match result {
                Ok(result) => {
                    self.current = Some(result);
                    self.error = false;
                }
                Err(_) => self.error = true,
            }
        }
        if let Some(options) = self.queued.take() {
            self.launch(options);
        }
        true
    }
    pub fn status(&self) -> &'static str {
        if self.loading() {
            tr!(LOADING)
        } else if self.error {
            tr!(FAILED)
        } else if self.current.as_ref().is_some_and(|r| r.limited) {
            tr!("Display limited to 256 sources · capacities cannot be summed")
        } else if self
            .current
            .as_ref()
            .is_some_and(|r| r.discovery.volumes.is_empty())
        {
            tr!(EMPTY)
        } else {
            tr!(READY)
        }
    }
}
/// Only positively identified, accessible local sources can start implicitly.
pub(crate) fn can_auto_scan(volume: &Volume) -> bool {
    matches!(
        volume.kind,
        SourceKind::Local | SourceKind::Removable | SourceKind::Optical
    ) && volume.availability == VolumeAvailability::Available
        && !volume.mounts.is_empty()
        // WindowsPath identifies only a location (for example SUBST), whose
        // target can change without changing this identity or its mounts.
        && !matches!(volume.identity, VolumeIdentity::WindowsPath(_))
}
fn size(value: MetadataValue<u64>, units: crate::units::Units) -> String {
    match value {
        MetadataValue::Known { value, .. } => units.format(value),
        MetadataValue::AccessDenied => tr!("Access denied").into(),
        MetadataValue::Unsupported => tr_in!("capacity", "Unsupported").into(),
        MetadataValue::NotCollected => tr!("Not queried").into(),
        _ => tr_in!("capacity", "Unknown").into(),
    }
}
// Thresholds use exact byte ratios, independent of unit and display rounding.
const OCCUPANCY_WARNING: u128 = 70;
const OCCUPANCY_CRITICAL: u128 = 90;
fn usage(volume: &Volume) -> Option<(f32, i32, f64)> {
    if volume.availability != VolumeAvailability::Available {
        return None;
    }
    let (MetadataValue::Known { value: total, .. }, MetadataValue::Known { value: used, .. }) =
        (volume.capacity.total, volume.capacity.used)
    else {
        return None;
    };
    if total == 0 || used > total {
        return None;
    }
    let scaled = u128::from(used) * 100;
    let total128 = u128::from(total);
    let level = if scaled >= total128 * OCCUPANCY_CRITICAL {
        3
    } else if scaled >= total128 * OCCUPANCY_WARNING {
        2
    } else {
        1
    };
    // Floor to one decimal so 69.99 never reads 70.0 beside a green bar,
    // and a non-full volume never reads 100.0. Geometry uses the full ratio.
    let displayed = (u128::from(used) * 1000 / total128) as f64 / 10.;
    Some((used as f32 / total as f32, level, displayed))
}
fn summary(volume: &Volume, units: crate::units::Units) -> String {
    if volume.availability != VolumeAvailability::Available {
        return tr!("Capacity unavailable").into();
    }
    tr_format!(
        "Total {} · Used {} · Free {} · Available {}",
        size(volume.capacity.total, units),
        size(volume.capacity.used, units),
        size(volume.capacity.free_total, units),
        size(volume.capacity.available, units)
    )
}
fn display(value: &std::ffi::OsStr) -> String {
    // Human display only. Escape controls; native identity/path stays in core.
    value
        .to_string_lossy()
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn row(volume: &Volume) -> VolumeRow {
    let path = volume
        .mounts
        .first()
        .map(|m| display(m.path.as_os_str()))
        .unwrap_or_default();
    let name = match &volume.label {
        MetadataValue::Known { value, .. } if !value.is_empty() => display(value),
        _ => path.clone(),
    };
    let kind = match volume.kind {
        SourceKind::Local => tr!("Local"),
        SourceKind::Removable => tr!("Removable"),
        SourceKind::Optical => tr!("Optical"),
        SourceKind::Remote => tr!("Network"),
        SourceKind::Unknown => tr!("Unknown type"),
    };
    let availability = match volume.availability {
        VolumeAvailability::Available => tr!("Available"),
        VolumeAvailability::NotProbed => tr!("Not queried"),
        VolumeAvailability::AccessDenied => tr!("Access denied"),
        VolumeAvailability::Unavailable => tr!("Unavailable"),
        VolumeAvailability::Unknown => tr!("Unknown availability"),
    };
    let usage = usage(volume);
    let aliases = if volume.mounts.len() > 1 {
        tr_count!(
            " · {n} mount, same storage",
            " · {n} mounts, same storage",
            volume.mounts.len() as u64
        )
    } else {
        String::new()
    };
    VolumeRow {
        name: format!("{name} · {path}").into(),
        summary: summary(volume, crate::units::Units::Binary).into(),
        state: format!("{kind} · {availability}{aliases}").into(),
        fraction: usage.map_or(0., |u| u.0),
        // Accessibility follows the textual tenth, not the rounded f32 geometry.
        usage_percent: usage.map_or(0., |u| u.2 as f32),
        has_usage: usage.is_some(),
        usage_level: usage.map_or(0, |u| u.1),
        usage: usage
            .map(|u| tr_format!("{} % used", crate::number_format::decimal(u.2)))
            .unwrap_or_else(|| tr!("Usage unavailable").into())
            .into(),
        selectable: volume.availability == VolumeAvailability::Available,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    fn result() -> VolumeDiscovery {
        VolumeDiscovery {
            volumes: vec![],
            enumeration_time: Duration::ZERO,
            capacity_time: Duration::ZERO,
            capacity_calls: 0,
        }
    }
    #[test]
    fn fallback_path_identity_cannot_authorize_a_retargeted_virtual_drive() {
        let volume = Volume {
            identity: VolumeIdentity::WindowsPath("Q:\\".into()),
            label: MetadataValue::Unknown,
            filesystem: MetadataValue::Unknown,
            kind: SourceKind::Local,
            mounts: vec![VolumeMount {
                path: "Q:\\".into(),
                mount_id: None,
                root: None,
            }],
            capacity: VolumeCapacity::absent(MetadataValue::Unknown),
            availability: VolumeAvailability::Available,
        };
        // A SUBST target may change while this location-only identity stays equal.
        let fresh = volume.clone();
        let sources = Sources::with_provider(Arc::new(move |_| {
            Ok(VolumeDiscovery {
                volumes: vec![fresh.clone()],
                ..result()
            })
        }));
        assert!(!sources.scan_preflight(&volume, &volume.mounts[0])());
        assert!(!can_auto_scan(&volume));
    }
    #[test]
    fn refresh_coalesces_rejects_late_result_and_drop_does_not_wait() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new(AtomicUsize::new(0));
        let g = gate.clone();
        let e = entered.clone();
        let mut s = Sources::with_provider(Arc::new(move |_| {
            let n = e.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                let (lock, cv) = &*g;
                let _guard = cv
                    .wait_while(lock.lock().unwrap(), |ready| !*ready)
                    .unwrap();
            }
            Ok(result())
        }));
        s.refresh(DiscoveryOptions::default());
        let start = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        for _ in 0..100 {
            s.refresh(DiscoveryOptions::default());
        }
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        assert!(!s.poll());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        while s.loading() {
            s.poll();
            if s.loading() {
                assert!(s.current.is_none());
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        assert_eq!(entered.load(Ordering::SeqCst), 2);
        assert_eq!(s.status(), tr!(EMPTY));
        let (send, recv) = mpsc::channel();
        let recv = Mutex::new(recv);
        let (started, ready) = mpsc::channel();
        let mut s = Sources::with_provider(Arc::new(move |_| {
            started.send(()).unwrap();
            recv.lock().unwrap().recv().unwrap();
            Ok(result())
        }));
        s.refresh(DiscoveryOptions::default());
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let now = Instant::now();
        drop(s);
        assert!(now.elapsed() < Duration::from_millis(100));
        send.send(()).unwrap();
    }
    #[test]
    fn errors_panics_and_empty_are_distinct_and_refresh_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut s =
            Sources::with_provider(Arc::new(move |_| match c.fetch_add(1, Ordering::SeqCst) {
                0 => Err(io::ErrorKind::PermissionDenied.into()),
                1 => panic!("injected"),
                _ => Ok(result()),
            }));
        for error in [true, true, false] {
            s.refresh(DiscoveryOptions::default());
            let now = Instant::now();
            while s.loading() {
                s.poll();
                assert!(now.elapsed() < Duration::from_secs(5));
                std::thread::yield_now();
            }
            assert_eq!(s.error, error);
        }
        assert_eq!(s.status(), tr!(EMPTY));
    }
}
