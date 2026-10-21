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

pub(crate) const LOADING: &str = "Découverte en cours…";
pub(crate) const EMPTY: &str = "Aucune source de stockage visible.";
pub(crate) const FAILED: &str = "Découverte impossible. Réessaie avec Actualiser.";
pub(crate) const READY: &str = "Mesures ponctuelles · capacités non additionnables";
pub(crate) const MAX_ROWS: usize = 256;
type Discover = Arc<dyn Fn(DiscoveryOptions) -> io::Result<VolumeDiscovery> + Send + Sync>;
struct Job {
    generation: u64,
    receiver: Receiver<Result<Prepared, ()>>,
}
pub(crate) struct Prepared {
    pub discovery: VolumeDiscovery,
    pub rows: Vec<VolumeRow>,
    pub limited: bool,
}
impl Prepared {
    pub(crate) fn rows_for(&self, units: crate::units::Units) -> Vec<VolumeRow> {
        self.discovery
            .volumes
            .iter()
            .zip(&self.rows)
            .map(|(volume, original)| {
                let mut row = original.clone();
                row.summary = summary(volume, units).into();
                row
            })
            .collect()
    }
    pub(crate) fn new(mut discovery: VolumeDiscovery) -> Self {
        let limited = discovery.volumes.len() > MAX_ROWS;
        discovery.volumes.truncate(MAX_ROWS);
        let rows = discovery.volumes.iter().map(row).collect();
        Self {
            discovery,
            rows,
            limited,
        }
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
            LOADING
        } else if self.error {
            FAILED
        } else if self.current.as_ref().is_some_and(|r| r.limited) {
            "Affichage limité à 256 sources · capacités non additionnables"
        } else if self.current.as_ref().is_some_and(|r| r.rows.is_empty()) {
            EMPTY
        } else {
            READY
        }
    }
}
fn size(value: MetadataValue<u64>, units: crate::units::Units) -> String {
    match value {
        MetadataValue::Known { value, .. } => units.format(value),
        MetadataValue::AccessDenied => "Accès refusé".into(),
        MetadataValue::Unsupported => "Non pris en charge".into(),
        MetadataValue::NotCollected => "Non interrogé".into(),
        _ => "Inconnu".into(),
    }
}
fn summary(volume: &Volume, units: crate::units::Units) -> String {
    let usage = match volume.capacity.utilization_percent() {
        MetadataValue::Known { value, .. } => format!(" · {value:.1} % utilisés"),
        _ => String::new(),
    };
    format!(
        "Total {} · Libre {}{}",
        size(volume.capacity.total, units),
        size(volume.capacity.available, units),
        usage
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
        SourceKind::Local => "Local",
        SourceKind::Removable => "Amovible",
        SourceKind::Optical => "Optique",
        SourceKind::Remote => "Réseau",
        SourceKind::Unknown => "Type inconnu",
    };
    let availability = match volume.availability {
        VolumeAvailability::Available => "Disponible",
        VolumeAvailability::NotProbed => "Non interrogé",
        VolumeAvailability::AccessDenied => "Accès refusé",
        VolumeAvailability::Unavailable => "Indisponible",
        VolumeAvailability::Unknown => "Disponibilité inconnue",
    };
    let percentage = match volume.capacity.utilization_percent() {
        MetadataValue::Known { value, .. } => Some(value),
        _ => None,
    };
    let aliases = if volume.mounts.len() > 1 {
        format!(" · {} points, même stockage", volume.mounts.len())
    } else {
        String::new()
    };
    VolumeRow {
        name: format!("{name} · {path}").into(),
        summary: summary(volume, crate::units::Units::Binary).into(),
        state: format!("{kind} · {availability}{aliases}").into(),
        fraction: (percentage.unwrap_or(0.0) / 100.0) as f32,
        has_usage: percentage.is_some(),
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
        assert_eq!(s.status(), EMPTY);
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
        assert_eq!(s.status(), EMPTY);
    }
}
