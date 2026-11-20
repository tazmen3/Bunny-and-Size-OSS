//! Bounded single-flight worker. No native call, wait, join or scan on the UI.
use crate::VolumeRow;
use sortsize_core::{MetadataValue, volumes::*};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
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
/// Session observations only: never persisted as an authorization or restored
/// as evidence of access after restarting Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    Checking,
    Available,
    #[cfg_attr(not(windows), allow(dead_code))]
    AuthenticationRequired,
    Unavailable,
    AccessDenied,
    #[cfg_attr(not(windows), allow(dead_code))]
    Cancelled,
    #[cfg_attr(not(windows), allow(dead_code))]
    SessionConflict,
    Failed,
}
impl ConnectionState {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Checking => tr!("Checking access…"),
            Self::Available => tr!("Available"),
            Self::AuthenticationRequired => tr!("Authentication required"),
            Self::Unavailable => tr!("Unavailable"),
            Self::AccessDenied => tr!("Access denied"),
            Self::Cancelled => tr!("Connection cancelled"),
            Self::SessionConflict => tr!("Windows session conflict"),
            Self::Failed => tr!("Connection failed"),
        }
    }
}
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
                row.compact_summary = compact_summary(volume, units).into();
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
    registered: Vec<crate::source_registry::Source>,
    connections: HashMap<PathBuf, ConnectionState>,
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
            registered: Vec::new(),
            connections: HashMap::new(),
            current: None,
            error: false,
        }
    }
    pub(crate) fn set_connection_state(&mut self, path: &Path, state: ConnectionState) {
        // Only visible/registered sources can create observations, keeping the
        // session map bounded by the same source limits as the visible model.
        let known = self.registered.iter().any(|source| source.path == path)
            || self.current.as_ref().is_some_and(|current| {
                current
                    .discovery
                    .volumes
                    .iter()
                    .any(|volume| volume.mounts.iter().any(|mount| mount.path == path))
            });
        if known
            && (self.connections.len() < MAX_ROWS + crate::source_registry::MAX_SOURCES
                || self.connections.contains_key(path))
        {
            self.connections.insert(path.to_owned(), state);
        }
    }
    pub(crate) fn apply_connection_state(&self, path: &Path, row: &mut VolumeRow) {
        if let Some(state) = self.connections.get(path).copied() {
            row.reconnect_busy = state == ConnectionState::Checking;
            row.reconnect_needed = state != ConnectionState::Available;
            if !row.disconnected {
                row.state = state.text().into();
            }
            // A successful access check does not collect capacities. Previous
            // point-in-time capacity text may remain, but no stale live gauge.
            if state != ConnectionState::Available {
                row.has_usage = false;
            }
        }
        if row.disconnected {
            row.reconnect_needed = true;
            row.has_usage = false;
        }
    }
    /// Merge bookmarks without native queries, content access or scan intents.
    pub(crate) fn set_registered(&mut self, entries: &[crate::source_registry::Source]) {
        self.registered = entries.to_vec();
        if self.current.is_none() {
            self.current = Some(Prepared::new(VolumeDiscovery {
                volumes: Vec::new(),
                enumeration_time: Default::default(),
                capacity_time: Default::default(),
                capacity_calls: 0,
            }));
        }
        if let Some(current) = &mut self.current {
            crate::source_registry::merge(&mut current.discovery, &self.registered);
        }
        self.retain_connections();
    }
    fn retain_connections(&mut self) {
        self.connections.retain(|path, _| {
            self.registered.iter().any(|source| &source.path == path)
                || self.current.as_ref().is_some_and(|current| {
                    current
                        .discovery
                        .volumes
                        .iter()
                        .any(|volume| volume.mounts.iter().any(|mount| &mount.path == path))
                })
        });
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
        // A newer explicit native measurement supersedes an old connection
        // observation. Keep only an operation that is currently in flight.
        self.connections
            .retain(|_, state| *state == ConnectionState::Checking);
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
                Ok(mut result) => {
                    self.connections
                        .retain(|_, state| *state == ConnectionState::Checking);
                    crate::source_registry::merge(&mut result.discovery, &self.registered);
                    self.current = Some(result);
                    self.retain_connections();
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
fn compact_summary(volume: &Volume, units: crate::units::Units) -> String {
    if volume.availability != VolumeAvailability::Available {
        return String::new();
    }
    match volume.capacity.total {
        MetadataValue::Known { value, .. } => units.format(value),
        _ => String::new(),
    }
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
        network: volume.kind == SourceKind::Remote,
        disconnected: false,
        name: format!("{name} · {path}").into(),
        display_name: name.into(),
        path: path.into(),
        compact_summary: compact_summary(volume, crate::units::Units::Binary).into(),
        reconnect_needed: volume.kind == SourceKind::Remote
            && volume.availability != VolumeAvailability::Available,
        reconnect_busy: false,
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
        selectable: !volume.mounts.is_empty(),
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
    fn connection_observations_are_not_restored_and_new_native_results_supersede_them() {
        let source = crate::source_registry::Source {
            kind: crate::source_registry::Kind::Smb,
            path: std::env::temp_dir().join("synthetic-source-no-access"),
            label: "NAS".into(),
            disconnected: false,
        };
        let mut original = result();
        crate::source_registry::merge(&mut original, std::slice::from_ref(&source));
        let mut measured = original.volumes[0].clone();
        measured.availability = VolumeAvailability::Unavailable;
        let mut sources = Sources::with_provider(Arc::new(move |_| {
            Ok(VolumeDiscovery {
                volumes: vec![measured.clone()],
                ..result()
            })
        }));
        sources.set_registered(std::slice::from_ref(&source));
        let row = sources
            .current
            .as_ref()
            .unwrap()
            .rows_for(crate::units::Units::Binary)[0]
            .clone();
        assert_eq!(
            sources.current.as_ref().unwrap().discovery.volumes[0].availability,
            VolumeAvailability::NotProbed
        );
        assert!(row.state.contains(tr!("Not queried")));
        assert!(row.reconnect_needed);
        assert!(row.compact_summary.is_empty());
        assert!(!row.has_usage);
        sources.set_connection_state(&source.path, ConnectionState::Available);
        sources.refresh(DiscoveryOptions::default());
        // An old positive observation must not mask a later native failure.
        sources.set_connection_state(&source.path, ConnectionState::Available);
        let start = Instant::now();
        while sources.loading() {
            sources.poll();
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        let mut row = sources
            .current
            .as_ref()
            .unwrap()
            .rows_for(crate::units::Units::Binary)[0]
            .clone();
        sources.apply_connection_state(&source.path, &mut row);
        assert_eq!(
            sources.current.as_ref().unwrap().discovery.volumes[0].availability,
            VolumeAvailability::Unavailable
        );
        assert!(row.state.contains(tr!("Unavailable")));
        assert!(row.reconnect_needed);
        let mut restarted = Sources::with_provider(Arc::new(|_| Ok(result())));
        restarted.set_registered(std::slice::from_ref(&source));
        assert!(restarted.connections.is_empty());
        assert_eq!(
            restarted.current.as_ref().unwrap().discovery.volumes[0].availability,
            VolumeAvailability::NotProbed
        );
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
    fn disconnect_hides_rows_immediately_and_in_flight_discovery_cannot_restore_them() {
        let temp = tempfile::tempdir().unwrap();
        let mut source = crate::source_registry::Source {
            kind: crate::source_registry::Kind::Smb,
            path: temp.path().join("offline-share"),
            label: "NAS".into(),
            disconnected: false,
        };
        let mut measured = result();
        crate::source_registry::merge(&mut measured, std::slice::from_ref(&source));
        let volumes = measured.volumes;
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_gate = gate.clone();
        let worker_calls = calls.clone();
        let mut sources = Sources::with_provider(Arc::new(move |_| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            let _guard = worker_gate
                .1
                .wait_while(worker_gate.0.lock().unwrap(), |ready| !*ready)
                .unwrap();
            Ok(VolumeDiscovery {
                volumes: volumes.clone(),
                ..result()
            })
        }));
        sources.set_registered(std::slice::from_ref(&source));
        assert_eq!(sources.current.as_ref().unwrap().discovery.volumes.len(), 1);
        sources.refresh(DiscoveryOptions::default());
        source.disconnected = true;
        sources.set_registered(std::slice::from_ref(&source));
        assert!(
            sources
                .current
                .as_ref()
                .unwrap()
                .discovery
                .volumes
                .is_empty()
        );
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        for _ in 0..2 {
            let start = Instant::now();
            while sources.loading() {
                sources.poll();
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::yield_now();
            }
            assert!(
                sources
                    .current
                    .as_ref()
                    .unwrap()
                    .discovery
                    .volumes
                    .is_empty()
            );
            sources.refresh(DiscoveryOptions::default());
        }
        let start = Instant::now();
        while sources.loading() {
            sources.poll();
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        let before_reconnect = calls.load(Ordering::SeqCst);
        source.disconnected = false;
        sources.set_registered(std::slice::from_ref(&source));
        assert!(!sources.loading());
        assert_eq!(calls.load(Ordering::SeqCst), before_reconnect);
        let restored = &sources.current.as_ref().unwrap().discovery.volumes[0];
        assert_eq!(restored.mounts[0].path, source.path);
        assert_eq!(restored.availability, VolumeAvailability::NotProbed);
        assert!(!can_auto_scan(restored));
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
