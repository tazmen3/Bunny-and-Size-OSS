//! Explicit, bounded identity lookup, independent of scanning and presentation.
//!
//! Creating a [`SystemOwnerLookup`] requires consent to system providers, which
//! may contact NSS/Active Directory servers. Never use names as authorization.
//! Submit and poll from a background integration task: these methods do no I/O,
//! but briefly take a mutex and can copy a bounded name. No async runtime needed.
mod system;
pub use system::{SystemOwnerLookup, SystemProviderConsent};

use sortsize_core::NativeOwnerId;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

/// Lossless name supplied by the provider; display conversion is explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountName {
    UnixBytes(Box<[u8]>),
    /// Includes domain/machine and backslash when a domain is returned.
    WindowsUtf16(Box<[u16]>),
}
impl AccountName {
    pub fn byte_len(&self) -> usize {
        match self {
            Self::UnixBytes(v) => v.len(),
            Self::WindowsUtf16(v) => v.len().saturating_mul(2),
        }
    }
    pub fn to_string_lossy(&self) -> String {
        match self {
            Self::UnixBytes(v) => String::from_utf8_lossy(v).into_owned(),
            Self::WindowsUtf16(v) => String::from_utf16_lossy(v),
        }
    }
}

/// No localized messages, paths or identities stored in errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupError {
    System(i64),
    BufferLimit,
    ProviderPanicked,
    UnsupportedIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerNameResolution {
    Resolved(AccountName),
    NotFound,
    AccessDenied,
    Unavailable(LookupError),
    TemporaryFailure(LookupError),
    Cancelled,
}
impl OwnerNameResolution {
    fn name_bytes(&self) -> usize {
        match self {
            Self::Resolved(n) => n.byte_len(),
            _ => 0,
        }
    }
    fn cacheable(&self) -> bool {
        // Transient/system failures and cancellation must be retryable.
        matches!(self, Self::Resolved(_) | Self::NotFound)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOwner {
    pub id: NativeOwnerId,
    pub resolution: OwnerNameResolution,
}

/// Scratch/name limits apply to each call. Custom providers must obey them
/// while allocating (the service also rejects oversized returned names).
#[derive(Debug, Clone, Copy)]
pub struct LookupLimits {
    pub max_buffer_bytes: usize,
    pub max_name_bytes: usize,
}
pub trait OwnerLookup: Send + Sync + 'static {
    /// May block. Receives no filesystem path or inventory and must not reopen
    /// files. The service calls this outside all scheduler/cache locks.
    fn lookup(&self, id: NativeOwnerId, limits: LookupLimits) -> OwnerNameResolution;
}

#[derive(Debug, Clone, Copy)]
pub struct ResolverConfig {
    pub workers: usize,
    /// Includes queued, running AND completed-but-unconsumed work.
    pub max_pending: usize,
    pub max_cache_entries: usize,
    /// Includes the reserved cache slots and their name payloads, not allocator
    /// overhead, worker stacks, provider-internal allocations or external copies.
    pub max_cache_bytes: usize,
    pub lookup_limits: LookupLimits,
}
impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            max_pending: 128,
            max_cache_entries: 256,
            max_cache_bytes: 1024 * 1024,
            lookup_limits: LookupLimits {
                max_buffer_bytes: 64 * 1024,
                max_name_bytes: 32 * 1024,
            },
        }
    }
}
#[derive(Debug)]
pub enum ResolverStartError {
    InvalidConfig,
    Thread(std::io::Error),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitResult {
    Cached(OwnerNameResolution),
    Queued,
    AlreadyPending,
    Full,
    Cancelled,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolverStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub coalesced: u64,
    pub saturated: u64,
    pub lookup_calls: u64,
    pub evictions: u64,
    pub cache_entries: usize,
    pub cache_bytes: usize,
    pub pending: usize,
    pub running: usize,
    pub peak_pending: usize,
    pub peak_running: usize,
}

struct CacheEntry {
    value: ResolvedOwner,
}
enum Work {
    Queued,
    Running,
    Complete(OwnerNameResolution),
}
struct Slot {
    id: NativeOwnerId,
    work: Work,
}
struct State {
    slots: Vec<Option<Slot>>,
    cache: VecDeque<CacheEntry>,
    cache_base_bytes: usize,
    stats: ResolverStats,
    cancelled: bool,
    next_slot: usize,
}
impl State {
    fn cache_result(
        &mut self,
        id: NativeOwnerId,
        result: &OwnerNameResolution,
        config: ResolverConfig,
    ) {
        if !result.cacheable() || config.max_cache_entries == 0 {
            return;
        }
        let bytes = result.name_bytes();
        if bytes > config.max_cache_bytes - self.cache_base_bytes {
            return;
        }
        while self.cache.len() == config.max_cache_entries
            || bytes > config.max_cache_bytes - self.stats.cache_bytes
        {
            let old = self
                .cache
                .pop_front()
                .expect("cache has an eviction candidate");
            self.stats.cache_bytes -= old.value.resolution.name_bytes();
            self.stats.evictions += 1;
        }
        self.cache.push_back(CacheEntry {
            value: ResolvedOwner {
                id,
                resolution: result.clone(),
            },
        });
        self.stats.cache_bytes += bytes;
        self.stats.cache_entries = self.cache.len();
    }
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    workers_alive: AtomicUsize,
    config: ResolverConfig,
    lookup: Arc<dyn OwnerLookup>,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn cancel(&self) {
        let mut state = self.lock();
        state.cancelled = true;
        // Clear personal names immediately. In-progress calls release their
        // scratch and late output on return; they cannot reliably be interrupted.
        state.cache.clear();
        state.stats.cache_entries = 0;
        state.stats.cache_bytes = state.cache_base_bytes;
        for slot in state.slots.iter_mut().flatten() {
            if !matches!(slot.work, Work::Running) {
                slot.work = Work::Complete(OwnerNameResolution::Cancelled);
            }
        }
        self.changed.notify_all();
    }
}

/// One bounded lookup session for one local identity namespace. No global cache
/// or persistence. Recreate to refresh identities; negative results are LRU too.
/// Drop cancels and detaches workers, never waits for a blocking OS lookup.
/// Keep a cancelled service until [`Self::is_stopped`] before replacing it if a
/// process-wide bound is needed. Bounds otherwise apply per live session.
pub struct OwnerResolver {
    shared: Arc<Shared>,
}
impl OwnerResolver {
    pub fn new(
        config: ResolverConfig,
        lookup: Arc<dyn OwnerLookup>,
    ) -> Result<Self, ResolverStartError> {
        if !(1..=8).contains(&config.workers)
            || !(1..=4096).contains(&config.max_pending)
            || config.max_cache_entries > 65_536
            || config.max_cache_bytes > 64 * 1024 * 1024
            || !(1..=1024 * 1024).contains(&config.lookup_limits.max_buffer_bytes)
            || !(1..=64 * 1024).contains(&config.lookup_limits.max_name_bytes)
        {
            return Err(ResolverStartError::InvalidConfig);
        }
        let cache = VecDeque::with_capacity(config.max_cache_entries);
        let cache_base_bytes = cache.capacity() * size_of::<CacheEntry>();
        if cache_base_bytes > config.max_cache_bytes {
            return Err(ResolverStartError::InvalidConfig);
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                slots: (0..config.max_pending).map(|_| None).collect(),
                cache,
                cache_base_bytes,
                stats: ResolverStats {
                    cache_bytes: cache_base_bytes,
                    ..Default::default()
                },
                cancelled: false,
                next_slot: 0,
            }),
            changed: Condvar::new(),
            workers_alive: AtomicUsize::new(0),
            config,
            lookup,
        });
        for index in 0..config.workers {
            let worker = Arc::clone(&shared);
            shared.workers_alive.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = thread::Builder::new()
                .name(format!("owner-lookup-{index}"))
                .spawn(move || run(worker))
            {
                shared.workers_alive.fetch_sub(1, Ordering::Relaxed);
                shared.cancel();
                return Err(ResolverStartError::Thread(error));
            }
        }
        Ok(Self { shared })
    }
    /// Never waits for queue space or a provider. Completed work also coalesces
    /// until consumed, so a full result queue cannot cause repeated native calls.
    pub fn try_submit(&self, id: NativeOwnerId) -> SubmitResult {
        let mut state = self.shared.lock();
        if state.cancelled {
            return SubmitResult::Cancelled;
        }
        if let Some(index) = state.cache.iter().position(|entry| entry.value.id == id) {
            let entry = state.cache.remove(index).expect("existing cache entry");
            let value = entry.value.resolution.clone();
            state.cache.push_back(entry);
            state.stats.cache_hits += 1;
            return SubmitResult::Cached(value);
        }
        if state.slots.iter().flatten().any(|slot| slot.id == id) {
            state.stats.coalesced += 1;
            return SubmitResult::AlreadyPending;
        }
        if let Some(slot) = state.slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(Slot {
                id,
                work: Work::Queued,
            });
            state.stats.cache_misses += 1;
            state.stats.pending += 1;
            state.stats.peak_pending = state.stats.peak_pending.max(state.stats.pending);
            self.shared.changed.notify_one();
            SubmitResult::Queued
        } else {
            state.stats.saturated += 1;
            SubmitResult::Full
        }
    }
    /// One result per accepted unique ID; order follows available slot indices,
    /// not submission order. Caller owns and budgets any retained result copies.
    pub fn try_take(&self) -> Option<ResolvedOwner> {
        let mut state = self.shared.lock();
        let slot = state
            .slots
            .iter_mut()
            .find(|slot| {
                matches!(
                    slot,
                    Some(Slot {
                        work: Work::Complete(_),
                        ..
                    })
                )
            })?
            .take()?;
        let Work::Complete(resolution) = slot.work else {
            unreachable!()
        };
        state.stats.pending -= 1;
        Some(ResolvedOwner {
            id: slot.id,
            resolution,
        })
    }
    pub fn stats(&self) -> ResolverStats {
        self.shared.lock().stats
    }
    /// Terminal: queued jobs become Cancelled; running calls finish naturally
    /// and their results are discarded. No timeout or thread interruption claim.
    pub fn cancel(&self) {
        self.shared.cancel();
    }
    pub fn is_stopped(&self) -> bool {
        self.shared.workers_alive.load(Ordering::Acquire) == 0
    }
    /// Reserved fixed storage (excludes names, Arc/Mutex runtime and threads).
    pub fn fixed_storage_bytes(&self) -> usize {
        let state = self.shared.lock();
        state.slots.capacity() * size_of::<Option<Slot>>() + state.cache_base_bytes
    }
}
impl Drop for OwnerResolver {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}

fn run(shared: Arc<Shared>) {
    struct Alive<'a>(&'a AtomicUsize);
    impl Drop for Alive<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Release);
        }
    }
    let _alive = Alive(&shared.workers_alive);
    loop {
        let (index, id) = {
            let mut state = shared.lock();
            loop {
                if state.cancelled {
                    return;
                }
                if let Some(index) = (0..state.slots.len())
                    .map(|offset| (state.next_slot + offset) % state.slots.len())
                    .find(|&index| {
                        matches!(
                            state.slots[index],
                            Some(Slot {
                                work: Work::Queued,
                                ..
                            })
                        )
                    })
                {
                    state.next_slot = (index + 1) % state.slots.len();
                    let slot = state.slots[index].as_mut().unwrap();
                    slot.work = Work::Running;
                    let id = slot.id;
                    state.stats.lookup_calls += 1;
                    state.stats.running += 1;
                    state.stats.peak_running = state.stats.peak_running.max(state.stats.running);
                    break (index, id);
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let mut result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.lookup.lookup(id, shared.config.lookup_limits)
        }))
        .unwrap_or(OwnerNameResolution::TemporaryFailure(
            LookupError::ProviderPanicked,
        ));
        if result.name_bytes() > shared.config.lookup_limits.max_name_bytes {
            result = OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        }
        let mut state = shared.lock();
        state.stats.running -= 1;
        if state.cancelled {
            result = OwnerNameResolution::Cancelled;
        } else {
            state.cache_result(id, &result, shared.config);
        }
        state.slots[index].as_mut().unwrap().work = Work::Complete(result);
    }
}

/// Stable textual fallback from the native identity alone. No system lookup.
/// The source namespace is still required to interpret this ID correctly.
/// Returns None for future NativeOwnerId variants unknown to this resolver.
pub fn owner_id_text(id: NativeOwnerId) -> Option<String> {
    Some(match id {
        NativeOwnerId::UnixUid(uid) => uid.to_string(),
        #[cfg(windows)]
        NativeOwnerId::WindowsSid(sid) => {
            use std::fmt::Write;
            let bytes = sid.as_bytes();
            let authority = bytes[2..8]
                .iter()
                .fold(0u64, |value, b| (value << 8) | u64::from(*b));
            let mut text = format!("S-{}-", bytes[0]);
            // Windows uses hexadecimal for authorities exceeding 32 bits.
            if authority > u32::MAX.into() {
                write!(text, "0x{authority:012x}").unwrap();
            } else {
                write!(text, "{authority}").unwrap();
            }
            for sub in bytes[8..].as_chunks::<4>().0 {
                write!(text, "-{}", u32::from_le_bytes(*sub)).unwrap();
            }
            text
        }
        _ => return None,
    })
}
