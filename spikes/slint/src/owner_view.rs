//! Bounded page projection and one coordinator per window. No UI/system lookup.
use slint::SharedString;
use sortsize_core::{EntryOwner, MetadataValue, NativeOwnerId};
use sortsize_owner_resolution::{
    OwnerLookup, OwnerNameResolution, OwnerResolver, ResolverConfig, ResolverStats, SubmitResult,
    SystemOwnerLookup, SystemProviderConsent, owner_id_text,
};
use std::{
    cmp::Ordering,
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

pub(crate) const PAGE_NAME_BUDGET: usize = 1024 * 1024;
pub(crate) const DISPLAY_NAME_BYTES: usize = 1024;

pub(crate) fn compare_id(a: NativeOwnerId, b: NativeOwnerId) -> Ordering {
    match (a, b) {
        (NativeOwnerId::UnixUid(a), NativeOwnerId::UnixUid(b)) => a.cmp(&b),
        #[cfg(windows)]
        (NativeOwnerId::WindowsSid(a), NativeOwnerId::WindowsSid(b)) => {
            a.as_bytes().cmp(b.as_bytes())
        }
        #[cfg(windows)]
        (NativeOwnerId::UnixUid(_), NativeOwnerId::WindowsSid(_)) => Ordering::Less,
        #[cfg(windows)]
        (NativeOwnerId::WindowsSid(_), NativeOwnerId::UnixUid(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

pub(crate) fn fallback(owner: MetadataValue<NativeOwnerId>) -> SharedString {
    match owner {
        MetadataValue::Known { value, .. } => owner_id_text(value)
            .unwrap_or_else(|| tr!("Unavailable").into())
            .into(),
        MetadataValue::NotCollected => "—".into(),
        MetadataValue::AccessDenied => tr!("Access denied").into(),
        _ => tr!("Unavailable").into(),
    }
}

fn display_name(name: sortsize_owner_resolution::AccountName) -> SharedString {
    let text = name.to_string_lossy();
    let mut result = String::with_capacity(text.len().min(DISPLAY_NAME_BYTES));
    let mut push = |character: char| {
        if result.len() + character.len_utf8() > DISPLAY_NAME_BYTES - 3 {
            result.push('…');
            false
        } else {
            result.push(character);
            true
        }
    };
    'characters: for character in text.chars() {
        if character.is_control() {
            for escaped in character.escape_default() {
                if !push(escaped) {
                    break 'characters;
                }
            }
        } else if !push(character) {
            break;
        }
    }
    result.into()
}

/// Immutable node-index tables shared with the coordinator, never copied per page.
pub(crate) struct OwnerData {
    pub(crate) owners: Box<[EntryOwner]>,
    pub(crate) ranges: Box<[Range<usize>]>,
    pub(crate) children: Box<[usize]>,
    pub(crate) directories: Box<[bool]>,
}
pub(crate) struct OwnerOrders {
    pub(crate) directions: [Box<[usize]>; 2],
}
#[derive(Clone)]
pub(crate) struct DisplayOwner {
    pub(crate) id: NativeOwnerId,
    pub(crate) label: SharedString,
}
pub(crate) fn label_for(names: &[DisplayOwner], id: NativeOwnerId) -> Option<&SharedString> {
    names
        .binary_search_by(|entry| compare_id(entry.id, id))
        .ok()
        .map(|i| &names[i].label)
}

/// Called on scan/coordinator workers only. Equal labels keep ID/node tie breaks.
pub(crate) fn prepare_orders(data: &OwnerData, names: &[DisplayOwner]) -> OwnerOrders {
    let labels: Vec<_> = data
        .owners
        .iter()
        .map(|entry| match entry.id {
            MetadataValue::Known { value, .. } => label_for(names, value),
            _ => None,
        })
        .collect();
    let class = |index: usize| match data.owners[index].id {
        MetadataValue::Known { .. } if labels[index].is_some() => 0,
        MetadataValue::Known { .. } => 1,
        MetadataValue::AccessDenied => 2,
        MetadataValue::NotCollected => 4,
        _ => 3,
    };
    OwnerOrders {
        directions: std::array::from_fn(|direction| {
            let mut order = data.children.to_vec();
            for range in &data.ranges {
                order[range.clone()].sort_unstable_by(|&left, &right| {
                    let ids = match (data.owners[left].id, data.owners[right].id) {
                        (
                            MetadataValue::Known { value: a, .. },
                            MetadataValue::Known { value: b, .. },
                        ) => compare_id(a, b),
                        _ => Ordering::Equal,
                    };
                    let value = if class(left) == 0 && class(right) == 0 {
                        labels[left]
                            .unwrap()
                            .as_str()
                            .cmp(labels[right].unwrap().as_str())
                    } else {
                        ids
                    };
                    data.directories[right]
                        .cmp(&data.directories[left])
                        .then_with(|| class(left).cmp(&class(right)))
                        .then(if direction == 0 {
                            value
                        } else {
                            value.reverse()
                        })
                        .then(ids)
                        .then(left.cmp(&right))
                });
            }
            order.into_boxed_slice()
        }),
    }
}

pub(crate) struct Request {
    pub(crate) generation: u64,
    pub(crate) page: u64,
    pub(crate) enabled: bool,
    pub(crate) data: Option<Arc<OwnerData>>,
    pub(crate) visible: Vec<NativeOwnerId>,
    pub(crate) sort: Option<(u64, usize)>,
}
pub(crate) struct PageUpdate {
    pub(crate) generation: u64,
    pub(crate) page: u64,
    pub(crate) names: Arc<[DisplayOwner]>,
    pub(crate) pending: usize,
    pub(crate) waiting_for_previous: bool,
    pub(crate) stats: ResolverStats,
}
pub(crate) struct SortUpdate {
    pub(crate) generation: u64,
    pub(crate) request: u64,
    pub(crate) direction: usize,
    pub(crate) orders: Arc<OwnerOrders>,
    #[cfg(feature = "benchmarks")]
    pub(crate) preparation_ms: f64,
}
#[derive(Default)]
pub(crate) struct Updates {
    pub(crate) page: Option<PageUpdate>,
    pub(crate) sort: Option<SortUpdate>,
}

pub(crate) struct Task {
    inbox: Arc<Mutex<Option<Request>>>,
    outbox: Arc<Mutex<Updates>>,
    stop: Arc<AtomicBool>,
    pending: Option<Request>,
    // Process-local validation diagnostic; not exposed in product UI/logs.
    #[cfg_attr(not(any(test, feature = "benchmarks")), allow(dead_code))]
    pub(crate) pools_started: Arc<AtomicUsize>,
    #[cfg_attr(not(feature = "benchmarks"), allow(dead_code))]
    native_provider: Option<Arc<SystemOwnerLookup>>,
}
impl Task {
    pub(crate) fn system() -> std::io::Result<Self> {
        let provider = Arc::new(SystemOwnerLookup::new(
            SystemProviderConsent::AllowPotentialNetworkLookup,
        ));
        let mut task = Self::start(provider.clone())?;
        task.native_provider = Some(provider);
        Ok(task)
    }
    pub(crate) fn start(provider: Arc<dyn OwnerLookup>) -> std::io::Result<Self> {
        let inbox = Arc::new(Mutex::new(None));
        let outbox = Arc::new(Mutex::new(Updates::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let pools_started = Arc::new(AtomicUsize::new(0));
        let (input, output, stopped, starts) = (
            inbox.clone(),
            outbox.clone(),
            stop.clone(),
            pools_started.clone(),
        );
        std::thread::Builder::new()
            .name("sortsize-owner-view".into())
            .spawn(move || {
                coordinate(input, output, stopped, starts, provider);
            })?;
        Ok(Self {
            inbox,
            outbox,
            stop,
            pending: None,
            pools_started,
            native_provider: None,
        })
    }
    pub(crate) fn request(&mut self, mut request: Request) {
        request.visible.truncate(crate::tree::MAX_VISIBLE_ROWS);
        self.pending = Some(request);
        self.flush();
    }
    fn flush(&mut self) {
        if self.pending.is_some()
            && let Ok(mut inbox) = self.inbox.try_lock()
        {
            *inbox = self.pending.take();
        }
    }
    #[cfg(test)]
    pub(crate) fn inject_sort_update(&self, update: SortUpdate) {
        self.outbox.lock().unwrap().sort = Some(update);
    }
    pub(crate) fn poll(&mut self) -> Updates {
        self.flush();
        self.outbox
            .try_lock()
            .map(|mut output| std::mem::take(&mut *output))
            .unwrap_or_default()
    }
    #[cfg(feature = "benchmarks")]
    pub(crate) fn diagnostics(&self) -> (usize, usize) {
        (
            self.pools_started.load(AtomicOrdering::Relaxed),
            self.native_provider
                .as_ref()
                .map_or(0, |p| p.native_calls()),
        )
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Release);
    }
}
struct Item {
    id: NativeOwnerId,
    state: u8,
    name: Option<SharedString>,
}

fn coordinate(
    inbox: Arc<Mutex<Option<Request>>>,
    outbox: Arc<Mutex<Updates>>,
    stop: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    provider: Arc<dyn OwnerLookup>,
) {
    let mut current: Option<Request> = None;
    let mut resolver: Option<OwnerResolver> = None;
    let mut retiring = false;
    let mut items: Vec<Item> = Vec::new();
    let mut name_bytes = 0;
    let mut dirty = false;
    let mut last_publish = Instant::now() - Duration::from_secs(1);
    let mut sorted_request = 0;
    let mut failed_start = false;
    while !stop.load(AtomicOrdering::Acquire) {
        let command = inbox.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut request) = command {
            let restart = current.as_ref().is_some_and(|old| {
                old.generation != request.generation || (old.enabled && !request.enabled)
            });
            if restart {
                if let Some(service) = &resolver {
                    service.cancel();
                    retiring = true;
                }
                failed_start = false;
            }
            let new_page = current.as_ref().is_none_or(|old| {
                old.generation != request.generation
                    || old.page != request.page
                    || old.enabled != request.enabled
            });
            if new_page {
                request.visible.sort_unstable_by(|a, b| compare_id(*a, *b));
                request.visible.dedup();
                items = request
                    .visible
                    .iter()
                    .map(|&id| Item {
                        id,
                        state: 0,
                        name: None,
                    })
                    .collect();
                name_bytes = 0;
                dirty = true;
            }
            current = Some(request);
        }
        if retiring && resolver.as_ref().is_some_and(OwnerResolver::is_stopped) {
            resolver = None;
            retiring = false;
            dirty = true;
        }
        if let Some(request) = &current {
            if request.enabled
                && !items.is_empty()
                && request.data.is_some()
                && resolver.is_none()
                && !failed_start
            {
                match OwnerResolver::new(ResolverConfig::default(), provider.clone()) {
                    Ok(service) => {
                        resolver = Some(service);
                        starts.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                    Err(_) => {
                        failed_start = true;
                        dirty = true;
                    }
                }
            }
            if request.enabled
                && !retiring
                && let Some(service) = &resolver
            {
                for _ in 0..128 {
                    let Some(result) = service.try_take() else {
                        break;
                    };
                    if let Ok(index) = items.binary_search_by(|item| compare_id(item.id, result.id))
                    {
                        finish(&mut items[index], result.resolution, &mut name_bytes);
                        dirty = true;
                    }
                }
                for item in items.iter_mut().filter(|i| i.state == 0).take(32) {
                    match service.try_submit(item.id) {
                        SubmitResult::Queued | SubmitResult::AlreadyPending => {
                            item.state = 1;
                            dirty = true;
                        }
                        SubmitResult::Cached(value) => {
                            finish(item, value, &mut name_bytes);
                            dirty = true;
                        }
                        SubmitResult::Full => break,
                        SubmitResult::Cancelled => break,
                    }
                }
            }
            if let Some((serial, direction)) = request.sort
                && serial != sorted_request
            {
                if let Some(data) = &request.data {
                    let names: Vec<_> = items
                        .iter()
                        .filter_map(|i| {
                            i.name.as_ref().map(|name| DisplayOwner {
                                id: i.id,
                                label: name.clone(),
                            })
                        })
                        .collect();
                    #[cfg(feature = "benchmarks")]
                    let started = Instant::now();
                    let orders = Arc::new(prepare_orders(data, &names));
                    outbox.lock().unwrap_or_else(|e| e.into_inner()).sort = Some(SortUpdate {
                        generation: request.generation,
                        request: serial,
                        direction,
                        orders,
                        #[cfg(feature = "benchmarks")]
                        preparation_ms: started.elapsed().as_secs_f64() * 1000.0,
                    });
                }
                sorted_request = serial;
            }
            if dirty && last_publish.elapsed() >= Duration::from_millis(50) {
                let names = items
                    .iter()
                    .filter_map(|i| {
                        i.name.as_ref().map(|name| DisplayOwner {
                            id: i.id,
                            label: name.clone(),
                        })
                    })
                    .collect::<Vec<_>>()
                    .into();
                let update = PageUpdate {
                    generation: request.generation,
                    page: request.page,
                    names,
                    pending: items.iter().filter(|i| i.state != 2).count(),
                    waiting_for_previous: retiring || failed_start,
                    stats: resolver
                        .as_ref()
                        .map_or_else(ResolverStats::default, OwnerResolver::stats),
                };
                outbox.lock().unwrap_or_else(|e| e.into_inner()).page = Some(update);
                dirty = false;
                last_publish = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(service) = resolver {
        service.cancel();
    }
}
fn finish(item: &mut Item, resolution: OwnerNameResolution, bytes: &mut usize) {
    if item.state == 2 {
        return;
    }
    item.state = 2;
    if let OwnerNameResolution::Resolved(name) = resolution {
        let name = display_name(name);
        if !name.is_empty() && name.len() <= PAGE_NAME_BUDGET.saturating_sub(*bytes) {
            *bytes += name.len();
            item.name = Some(name);
        }
    }
}

#[cfg(test)]
mod tests;
