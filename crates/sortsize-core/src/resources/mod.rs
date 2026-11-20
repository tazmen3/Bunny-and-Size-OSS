//! Shared V0.5 accounting governor. Accounted capacities are not process RSS.
use crate::inventory_contract::{EntryKey, MemoryMode};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock},
};
mod disk_volume;
pub use disk_volume::{DiskVolume, native_disk_volume};
const MIB: u64 = 1024 * 1024;
#[derive(Clone, Copy, Debug)]
pub struct MemoryReading {
    pub total: u64,
    pub available: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Cache,
    Sqlite,
    Transport,
    Queries,
    Publications,
    Margin,
}
impl Category {
    fn index(self) -> usize {
        self as usize
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub requested: MemoryMode,
    pub entries: Option<u64>,
    pub bytes: u64,
    pub categories: [u64; 6],
    pub fallback: bool,
}
impl Limits {
    pub fn calibrate(mode: MemoryMode, reading: Option<MemoryReading>) -> Self {
        let reading = reading.filter(|m| m.total > 0 && m.available <= m.total);
        let bytes = reading.map_or(128 * MIB, |m| {
            (m.total / 4).min(m.available / 2).min(4096 * MIB)
        });
        let weights = [160, 32, 24, 16, 8, 16];
        let categories = weights.map(|w| bytes / 256 * w);
        Self {
            requested: mode,
            entries: mode.entry_limit(categories[0] / 512),
            bytes,
            categories,
            fallback: reading.is_none(),
        }
    }
}
struct State {
    limits: Limits,
    byte_limit: Option<u64>,
    used: [u64; 6],
    entries: BTreeMap<EntryKey, u32>,
    disk_limit: u64,
    disk_used: u64,
    disk_reserved: u64,
    artifacts: BTreeMap<std::path::PathBuf, u64>,
    guarded_artifacts: BTreeSet<std::path::PathBuf>,
    closed_artifacts: BTreeSet<std::path::PathBuf>,
    disk_leases: BTreeMap<u64, DiskLease>,
    next_disk_lease: u64,
}
#[derive(Clone)]
pub struct Governor(Arc<Mutex<State>>);
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub limits: Limits,
    pub categories: [u64; 6],
    pub distinct_entries: usize,
    pub disk_used: u64,
    pub disk_reserved: u64,
    pub disk_limit: u64,
}
pub struct Reservation {
    governor: Governor,
    category: Category,
    bytes: u64,
    key: Option<EntryKey>,
}
impl Reservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    /// Return unused capacity after bounded construction; never acquire more memory.
    pub fn shrink_to(&mut self, bytes: u64) -> bool {
        if bytes > self.bytes {
            return false;
        }
        let mut state = self.governor.0.lock().unwrap();
        state.used[self.category.index()] -= self.bytes - bytes;
        self.bytes = bytes;
        true
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut s = self.governor.0.lock().unwrap();
        s.used[self.category.index()] -= self.bytes;
        if let Some(k) = self.key {
            let n = s.entries.get_mut(&k).unwrap();
            *n -= 1;
            if *n == 0 {
                s.entries.remove(&k);
            }
        }
    }
}
struct DiskLease {
    remaining: u64,
    group: Option<std::path::PathBuf>,
    volume: Option<DiskVolume>,
}
pub struct DiskReservation {
    governor: Governor,
    lease: u64,
}
/// Live owner of a local artifact ledger. There is intentionally no implicit
/// unprotection on Drop: an error path must never erase possibly live I/O.
pub struct ArtifactGuard {
    governor: Governor,
    root: std::path::PathBuf,
}
impl ArtifactGuard {
    /// Charge growth before native I/O. A prospective reservation from the same
    /// governor is atomically converted to owned bytes, preventing double charge.
    pub fn admit(&self, bytes: u64, prospective: Option<&mut DiskReservation>) -> bool {
        let mut state = self.governor.0.lock().unwrap();
        if !state.guarded_artifacts.contains(&self.root) {
            return false;
        }
        let old = state.artifacts.get(&self.root).copied().unwrap_or(0);
        if bytes <= old {
            return true;
        }
        let delta = bytes - old;
        if prospective
            .as_ref()
            .is_some_and(|token| !Arc::ptr_eq(&token.governor.0, &self.governor.0))
        {
            return false;
        }
        let explicit = prospective.as_ref().map(|token| token.lease);
        let eligible = state
            .disk_leases
            .iter()
            .filter(|(id, lease)| {
                Some(**id) == explicit || lease.group.as_deref() == Some(self.root.as_path())
            })
            .map(|(id, lease)| (*id, lease.remaining))
            .collect::<Vec<_>>();
        let available = eligible
            .iter()
            .try_fold(0u64, |n, (_, bytes)| n.checked_add(*bytes))
            .unwrap_or(u64::MAX);
        let consumed = delta.min(available);
        let Some(used) = state.disk_used.checked_add(delta) else {
            return false;
        };
        let Some(reserved) = state.disk_reserved.checked_sub(consumed) else {
            return false;
        };
        if used
            .checked_add(reserved)
            .is_none_or(|total| total > state.disk_limit)
        {
            return false;
        }
        state.artifacts.insert(self.root.clone(), bytes);
        state.disk_used = used;
        state.disk_reserved = reserved;
        let mut left = consumed;
        for (id, bytes) in eligible {
            let consumed = bytes.min(left);
            if let Some(lease) = state.disk_leases.get_mut(&id) {
                lease.remaining -= consumed;
            }
            left -= consumed;
            if left == 0 {
                break;
            }
        }
        true
    }
    /// Owner calls only after every native handle has closed successfully and
    /// filesystem accounting has succeeded. A retained group stays charged.
    pub fn finish(self, bytes: u64) {
        let mut state = self.governor.0.lock().unwrap();
        let old = state
            .artifacts
            .insert(self.root.clone(), bytes)
            .unwrap_or(0);
        state.disk_used = state.disk_used.saturating_sub(old).saturating_add(bytes);
        state.guarded_artifacts.remove(&self.root);
        state.closed_artifacts.insert(self.root.clone());
    }
}
impl DiskReservation {
    pub fn bytes(&self) -> u64 {
        self.governor
            .0
            .lock()
            .unwrap()
            .disk_leases
            .get(&self.lease)
            .map_or(0, |lease| lease.remaining)
    }
}
impl Drop for DiskReservation {
    fn drop(&mut self) {
        let mut state = self.governor.0.lock().unwrap();
        if let Some(lease) = state.disk_leases.remove(&self.lease) {
            state.disk_reserved -= lease.remaining;
        }
    }
}
impl Governor {
    pub fn new(mode: MemoryMode, reading: Option<MemoryReading>, free_disk: u64) -> Self {
        Self(Arc::new(Mutex::new(State {
            limits: Limits::calibrate(mode, reading),
            byte_limit: None,
            used: [0; 6],
            entries: BTreeMap::new(),
            disk_limit: disk_limit(free_disk),
            disk_used: 0,
            disk_reserved: 0,
            artifacts: BTreeMap::new(),
            guarded_artifacts: BTreeSet::new(),
            closed_artifacts: BTreeSet::new(),
            disk_leases: BTreeMap::new(),
            next_disk_lease: 1,
        })))
    }
    pub fn global() -> Self {
        static GLOBAL: OnceLock<Governor> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Self::new(MemoryMode::Auto, native_memory(), 0))
            .clone()
    }
    pub fn configure(&self, mode: MemoryMode, reading: Option<MemoryReading>) {
        let mut state = self.0.lock().unwrap();
        state.limits = Self::limited(Limits::calibrate(mode, reading), state.byte_limit);
    }
    /// Advanced ceiling in bytes, independent of the legacy entry-cache setting.
    /// Existing allocations stay charged; subsequent allocations obey the ceiling.
    pub fn set_byte_limit(&self, bytes: Option<u64>) {
        let mut state = self.0.lock().unwrap();
        state.byte_limit = bytes;
        state.limits = Self::limited(
            Limits::calibrate(state.limits.requested, native_memory()),
            bytes,
        );
    }
    fn limited(mut limits: Limits, ceiling: Option<u64>) -> Limits {
        if let Some(bytes) = ceiling.filter(|bytes| *bytes < limits.bytes) {
            limits.bytes = bytes;
            limits.categories = [160, 32, 24, 16, 8, 16].map(|w| bytes / 256 * w);
        }
        limits
    }
    /// Explicit service-side refresh. Existing reservations stay charged after shrink.
    pub fn refresh_native(&self) {
        let reading = native_memory();
        let mut s = self.0.lock().unwrap();
        s.limits = Self::limited(Limits::calibrate(s.limits.requested, reading), s.byte_limit);
    }
    /// Reserve a bounded working set atomically; never borrow from other categories.
    pub fn reserve_up_to(
        &self,
        category: Category,
        minimum: u64,
        preferred: u64,
    ) -> Option<Reservation> {
        if minimum == 0 || minimum > preferred || category == Category::Cache {
            return None;
        }
        let mut s = self.0.lock().unwrap();
        let i = category.index();
        let used = s.used.iter().try_fold(0u64, |n, v| n.checked_add(*v))?;
        let bytes = preferred
            .min(s.limits.categories[i].saturating_sub(s.used[i]))
            .min(s.limits.bytes.saturating_sub(used));
        if bytes < minimum {
            return None;
        }
        s.used[i] += bytes;
        Some(Reservation {
            governor: self.clone(),
            category,
            bytes,
            key: None,
        })
    }
    /// Admit one set of distinct, non-cache capacities under one lock.
    /// The process ceiling also bounds the minima, which may borrow unused
    /// process capacity. Optional growth still stays within category targets.
    pub fn reserve_partition<const N: usize>(
        &self,
        ceiling: u64,
        parts: [(Category, u64, u64); N],
    ) -> (u64, Option<[Reservation; N]>) {
        let mut state = self.0.lock().unwrap();
        let total = state.used.iter().copied().sum::<u64>();
        let mut seen = [false; 6];
        let mut minimum = 0u64;
        let mut possible = 0u64;
        let mut room = [0u64; N];
        for (slot, (category, low, high)) in parts.iter().copied().enumerate() {
            let i = category.index();
            if seen[i] || category == Category::Cache || low > high {
                return (0, None);
            }
            seen[i] = true;
            room[slot] = state.limits.categories[i]
                .saturating_sub(state.used[i])
                .clamp(low, high);
            possible = possible.saturating_add(room[slot]);
            minimum = minimum.saturating_add(low);
        }
        let available = possible.min(state.limits.bytes.saturating_sub(total));
        let Some(mut extra) = available.min(ceiling).checked_sub(minimum) else {
            return (available, None);
        };
        let reservations = std::array::from_fn(|slot| {
            let (category, low, _) = parts[slot];
            let added = extra.min(room[slot] - low);
            let bytes = low + added;
            extra -= added;
            state.used[category.index()] += bytes;
            Reservation {
                governor: self.clone(),
                category,
                bytes,
                key: None,
            }
        });
        (available, Some(reservations))
    }
    pub fn usage(&self) -> Usage {
        let s = self.0.lock().unwrap();
        Usage {
            limits: s.limits,
            categories: s.used,
            distinct_entries: s.entries.len(),
            disk_used: s.disk_used,
            disk_reserved: s.disk_reserved,
            disk_limit: s.disk_limit,
        }
    }
    /// Include allocated capacities/struct/map overhead supplied by the owner.
    /// Multiple copies of one EntryKey charge bytes twice, count the key once.
    pub fn reserve(
        &self,
        category: Category,
        bytes: u64,
        key: Option<EntryKey>,
    ) -> Option<Reservation> {
        if key.is_some() && category != Category::Cache {
            return None;
        }
        let mut s = self.0.lock().unwrap();
        let i = category.index();
        let total = s.used[i].checked_add(bytes)?;
        let used = s.used.iter().try_fold(0u64, |n, v| n.checked_add(*v))?;
        if total > s.limits.categories[i] || used.checked_add(bytes)? > s.limits.bytes {
            return None;
        }
        if let Some(k) = key {
            let new = !s.entries.contains_key(&k);
            if new
                && s.limits
                    .entries
                    .is_some_and(|n| s.entries.len() as u64 >= n)
            {
                return None;
            }
            let count = s.entries.get(&k).copied().unwrap_or(0).checked_add(1)?;
            s.entries.insert(k, count);
        }
        s.used[i] = total;
        Some(Reservation {
            governor: self.clone(),
            category,
            bytes,
            key,
        })
    }
    /// Caller measures *all* owned DB/WAL/SHM/results/backup/quarantine files.
    /// This is a monitored quota with margin, not a physical filesystem limit.
    pub fn observe_disk(&self, free: u64, owned_bytes: u64) {
        let mut s = self.0.lock().unwrap();
        s.disk_limit = disk_limit(free);
        let protected = s
            .artifacts
            .values()
            .try_fold(0u64, |n, v| n.checked_add(*v))
            .unwrap_or(u64::MAX);
        s.disk_used = owned_bytes.max(protected);
    }
    pub fn observe_artifact_group(&self, root: &std::path::Path, free: u64, bytes: u64) -> bool {
        let mut s = self.0.lock().unwrap();
        if !s.artifacts.contains_key(root) {
            if s.artifacts.len() >= 1024 {
                return false;
            }
            let metadata = s.artifacts.keys().try_fold(0usize, |n, p| {
                n.checked_add(p.as_os_str().as_encoded_bytes().len() + 128)
            });
            if metadata
                .and_then(|n| n.checked_add(root.as_os_str().as_encoded_bytes().len() + 128))
                .is_none_or(|bytes| bytes > 256 * 1024)
            {
                return false;
            }
        }
        if bytes == 0 && s.closed_artifacts.contains(root) && artifact_confirmed_gone(root) {
            s.closed_artifacts.remove(root);
        }
        let bytes = if s.guarded_artifacts.contains(root) || s.closed_artifacts.contains(root) {
            bytes.max(s.artifacts.get(root).copied().unwrap_or(0))
        } else {
            bytes
        };
        s.artifacts.insert(root.into(), bytes);
        s.disk_used = s
            .artifacts
            .values()
            .try_fold(0u64, |a, b| a.checked_add(*b))
            .unwrap_or(u64::MAX);
        s.disk_limit = disk_limit(free);
        true
    }
    /// Owner calls this only after deleting its complete local artifact group.
    /// Closing a retained database is not deletion and must not uncharge it.
    pub fn forget_deleted_artifact_group(&self, root: &std::path::Path) {
        let mut s = self.0.lock().unwrap();
        if s.guarded_artifacts.contains(root)
            || (s.closed_artifacts.contains(root) && !artifact_confirmed_gone(root))
        {
            return;
        }
        s.closed_artifacts.remove(root);
        if let Some(bytes) = s.artifacts.remove(root) {
            s.disk_used = s.disk_used.saturating_sub(bytes);
        }
    }
    /// Install one authoritative live ledger. Ordinary observation and deletion
    /// notifications cannot reduce this group's charge until the owner confirms
    /// all native handles are closed. Dropping the guard without finish is sticky.
    pub fn guard_artifact_group(
        &self,
        root: &std::path::Path,
        free: u64,
        bytes: u64,
    ) -> Option<ArtifactGuard> {
        let mut state = self.0.lock().unwrap();
        if state.guarded_artifacts.contains(root)
            || (!state.artifacts.contains_key(root) && state.artifacts.len() >= 1024)
        {
            return None;
        }
        let metadata = state
            .guarded_artifacts
            .iter()
            .chain(state.closed_artifacts.iter())
            .filter(|p| p.as_path() != root)
            .try_fold(0usize, |n, p| {
                n.checked_add(p.as_os_str().as_encoded_bytes().len() + 128)
            })?;
        if metadata.checked_add(root.as_os_str().as_encoded_bytes().len() + 128)? > 256 * 1024 {
            return None;
        }
        let old = state.artifacts.get(root).copied().unwrap_or(0);
        let bytes = bytes.max(old);
        let used = state.disk_used.checked_sub(old)?.checked_add(bytes)?;
        state.disk_limit = disk_limit(free);
        if used.checked_add(state.disk_reserved)? > state.disk_limit {
            return None;
        }
        state.artifacts.insert(root.into(), bytes);
        state.disk_used = used;
        state.closed_artifacts.remove(root);
        state.guarded_artifacts.insert(root.into());
        Some(ArtifactGuard {
            governor: self.clone(),
            root: root.into(),
        })
    }
    /// Transfer a closed, durable group after its filesystem rename without a
    /// zero-charge window. Live/sticky ledgers cannot be relocated.
    pub fn relocate_artifact_group(&self, from: &std::path::Path, to: &std::path::Path) -> bool {
        if from == to {
            return true;
        }
        if !matches!(std::fs::symlink_metadata(from),Err(error) if error.kind()==std::io::ErrorKind::NotFound)
            || !std::fs::symlink_metadata(to).is_ok_and(|m| !m.is_symlink())
        {
            return false;
        }
        let mut state = self.0.lock().unwrap();
        if state.guarded_artifacts.contains(from)
            || state.guarded_artifacts.contains(to)
            || state.artifacts.get(to).is_some_and(|bytes| *bytes != 0)
        {
            return false;
        }
        let Some(bytes) = state.artifacts.remove(from) else {
            return false;
        };
        state.artifacts.insert(to.into(), bytes);
        if state.closed_artifacts.remove(from) {
            state.closed_artifacts.insert(to.into());
        }
        true
    }
    pub fn reserve_disk(&self, bytes: u64) -> Option<DiskReservation> {
        self.reserve_disk_inner(bytes, None, None)
    }
    pub fn reserve_disk_at(&self, path: &std::path::Path, bytes: u64) -> Option<DiskReservation> {
        self.reserve_disk_inner(bytes, None, native_disk_volume(path))
    }
    pub fn reserve_disk_for_group(
        &self,
        root: &std::path::Path,
        bytes: u64,
    ) -> Option<DiskReservation> {
        self.reserve_disk_inner(bytes, Some(root), native_disk_volume(root))
    }
    /// Only still-prospective bytes need to be subtracted from a current OS
    /// free-space reading. Anonymous legacy leases remain conservative.
    pub fn prospective_disk_bytes(&self, volume: Option<&DiskVolume>) -> u64 {
        self.0
            .lock()
            .unwrap()
            .disk_leases
            .values()
            .filter(|lease| {
                volume.is_none() || lease.volume.is_none() || lease.volume.as_ref() == volume
            })
            .map(|lease| lease.remaining)
            .sum()
    }
    fn reserve_disk_inner(
        &self,
        bytes: u64,
        group: Option<&std::path::Path>,
        volume: Option<DiskVolume>,
    ) -> Option<DiskReservation> {
        let mut state = self.0.lock().unwrap();
        if state.disk_leases.len() >= 1024 {
            return None;
        }
        let metadata = state
            .disk_leases
            .values()
            .filter_map(|lease| lease.group.as_ref())
            .try_fold(0usize, |n, p| {
                n.checked_add(p.as_os_str().as_encoded_bytes().len() + 128)
            })?;
        if group.is_some_and(|p| {
            metadata
                .checked_add(p.as_os_str().as_encoded_bytes().len() + 128)
                .is_none_or(|size| size > 256 * 1024)
        }) {
            return None;
        }
        if state
            .disk_used
            .checked_add(state.disk_reserved)?
            .checked_add(bytes)?
            > state.disk_limit
        {
            return None;
        }
        let id = state.next_disk_lease;
        state.next_disk_lease = state.next_disk_lease.checked_add(1)?;
        state.disk_reserved += bytes;
        state.disk_leases.insert(
            id,
            DiskLease {
                remaining: bytes,
                group: group.map(std::path::Path::to_owned),
                volume,
            },
        );
        Some(DiskReservation {
            governor: self.clone(),
            lease: id,
        })
    }
}
fn artifact_confirmed_gone(root: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(root) {
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {
            let Ok(entries) = std::fs::read_dir(root) else {
                return false;
            };
            for (index, item) in entries.enumerate() {
                if index >= 1024 {
                    return false;
                }
                let Ok(item) = item else {
                    return false;
                };
                let Ok(metadata) = std::fs::symlink_metadata(item.path()) else {
                    return false;
                };
                if !metadata.is_file() || metadata.is_symlink() || metadata.len() != 0 {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}
pub fn disk_limit(free: u64) -> u64 {
    (16 * 1024 * MIB)
        .min(free / 4)
        .min(free.saturating_sub(512 * MIB))
}
/// One service cache, keyed by qualified EntryKey; eviction never alters Store.
/// Payload capacities must be reported by the owner, including shared buffers.
pub struct Cache<T> {
    governor: Governor,
    items: BTreeMap<EntryKey, Item<T>>,
    lru: BTreeMap<u64, EntryKey>,
    clock: u64,
}
struct Item<T> {
    value: T,
    _reservation: Reservation,
    stamp: u64,
    pinned: bool,
}
impl<T> Cache<T> {
    pub fn new(governor: Governor) -> Self {
        Self {
            governor,
            items: BTreeMap::new(),
            lru: BTreeMap::new(),
            clock: 0,
        }
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    fn evict(&mut self) -> bool {
        let selected = self
            .lru
            .iter()
            .find(|(_, k)| !self.items[k].pinned)
            .map(|(s, k)| (*s, *k));
        if let Some((s, k)) = selected {
            self.lru.remove(&s);
            self.items.remove(&k);
            true
        } else {
            false
        }
    }
    pub fn trim(&mut self) {
        loop {
            let u = self.governor.usage();
            if u.categories[0] <= u.limits.categories[0]
                && u.limits
                    .entries
                    .is_none_or(|n| u.distinct_entries as u64 <= n)
            {
                break;
            }
            if !self.evict() {
                break;
            }
        }
    }
    pub fn insert(
        &mut self,
        key: EntryKey,
        value: T,
        payload_capacity: u64,
        pinned: bool,
    ) -> Result<(), T> {
        if self.items.contains_key(&key) {
            return Err(value);
        }
        let Some(bytes) = payload_capacity.checked_add(256 + std::mem::size_of::<T>() as u64)
        else {
            return Err(value);
        };
        let reservation = loop {
            if let Some(r) = self.governor.reserve(Category::Cache, bytes, Some(key)) {
                break r;
            }
            if !self.evict() {
                return Err(value);
            }
        };
        let Some(stamp) = self.clock.checked_add(1) else {
            return Err(value);
        };
        self.clock = stamp;
        self.lru.insert(stamp, key);
        self.items.insert(
            key,
            Item {
                value,
                _reservation: reservation,
                stamp,
                pinned,
            },
        );
        Ok(())
    }
    pub fn get(&mut self, key: EntryKey) -> Option<&T> {
        let stamp = self.clock.checked_add(1)?;
        self.clock = stamp;
        let item = self.items.get_mut(&key)?;
        self.lru.remove(&item.stamp);
        item.stamp = stamp;
        self.lru.insert(stamp, key);
        Some(&item.value)
    }
    pub fn pin(&mut self, key: EntryKey, pinned: bool) {
        if let Some(i) = self.items.get_mut(&key) {
            i.pinned = pinned;
        }
    }
}
#[cfg(target_os = "linux")]
pub fn native_memory() -> Option<MemoryReading> {
    let mut info = std::mem::MaybeUninit::<libc::sysinfo>::uninit();
    if unsafe { libc::sysinfo(info.as_mut_ptr()) } != 0 {
        return None;
    }
    let i = unsafe { info.assume_init() };
    Some(MemoryReading {
        total: i.totalram.checked_mul(u64::from(i.mem_unit))?,
        available: i
            .freeram
            .checked_add(i.bufferram)?
            .checked_mul(u64::from(i.mem_unit))?,
    })
}
#[cfg(windows)]
pub fn native_memory() -> Option<MemoryReading> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut i: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    i.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut i) } == 0 {
        return None;
    }
    Some(MemoryReading {
        total: i.ullTotalPhys,
        available: i.ullAvailPhys,
    })
}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn native_memory() -> Option<MemoryReading> {
    None
}
#[cfg(test)]
mod tests;

/// Probe only the already-authorized local store volume; never a source path.
#[cfg(target_os = "linux")]
pub fn native_free_disk(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut v = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(p.as_ptr(), v.as_mut_ptr()) } != 0 {
        return None;
    }
    let v = unsafe { v.assume_init() };
    v.f_bavail.checked_mul(v.f_frsize)
}
#[cfg(windows)]
pub fn native_free_disk(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut p = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if p.contains(&0) {
        return None;
    }
    p.push(0);
    let mut free = 0;
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            p.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return None;
    }
    Some(free)
}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn native_free_disk(_: &std::path::Path) -> Option<u64> {
    None
}

/// Allocation unit of the already-authorized local temporary volume. A query
/// refuses unsupported geometry instead of assuming 4 KiB on every filesystem.
#[cfg(target_os = "linux")]
pub fn native_allocation_unit(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut v = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: valid NUL-terminated path and writable statvfs output.
    if unsafe { libc::statvfs(p.as_ptr(), v.as_mut_ptr()) } != 0 {
        return None;
    }
    Some(unsafe { v.assume_init() }.f_frsize)
}
#[cfg(windows)]
pub fn native_allocation_unit(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};
    let p: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = vec![0u16; 32768];
    let (mut sectors, mut bytes, mut free, mut total) = (0, 0, 0, 0);
    // SAFETY: input and output buffers are valid and correctly sized.
    unsafe {
        if GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0
            || GetDiskFreeSpaceW(
                root.as_ptr(),
                &mut sectors,
                &mut bytes,
                &mut free,
                &mut total,
            ) == 0
        {
            return None;
        }
    }
    u64::from(sectors).checked_mul(u64::from(bytes))
}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn native_allocation_unit(_: &std::path::Path) -> Option<u64> {
    None
}

/// Native mapping page size, needed to account SQLite WAL mappings on systems
/// whose pages are larger than SQLite's 32 KiB shared-memory region.
#[cfg(target_os = "linux")]
pub fn native_mapping_page_size() -> Option<u64> {
    // SAFETY: sysconf takes this constant and has no pointer arguments.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()
}
#[cfg(windows)]
pub fn native_mapping_page_size() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    let mut info = std::mem::MaybeUninit::<SYSTEM_INFO>::uninit();
    // SAFETY: valid writable SYSTEM_INFO storage.
    unsafe {
        GetSystemInfo(info.as_mut_ptr());
        // winShmMap aligns the view offset to AllocationGranularity but grows
        // the file only to the requested region end, rounded to memory pages.
        Some(u64::from(info.assume_init().dwPageSize))
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn native_mapping_page_size() -> Option<u64> {
    None
}
