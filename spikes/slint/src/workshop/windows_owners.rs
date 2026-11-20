//! Current native owners are a transient visible-row enrichment, never a
//! mutation of an inventory, saved measurement, acquisition or certified read.
use super::*;
use slint::Model;
use sortsize_core::{EntryOwner, MetadataValue, NativeOwnerId};
use sortsize_owner_resolution::{LookupLimits, OwnerLookup, OwnerNameResolution, owner_id_text};
use std::{
    collections::VecDeque,
    sync::{
        Condvar,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(windows)]
use sortsize_owner_resolution::{SystemOwnerLookup, SystemProviderConsent};

const MAX_ROWS: usize = 128;
const PATH_BYTES: usize = 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
const NAME_BYTES: usize = 4096;

#[derive(Clone, Debug)]
enum Value {
    Identified(String),
    Unresolved(NativeOwnerId),
    Denied,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    fn key(n: u64) -> EntryKey {
        EntryKey {
            inventory: InventoryId(1),
            generation: GenerationId(1),
            entry: EntryId(n),
        }
    }
    fn wait(task: &Task, n: usize) -> Update {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(update) = task
                .shared
                .mail
                .lock()
                .unwrap()
                .update
                .as_ref()
                .filter(|u| u.values.len() == n)
            {
                return update.clone();
            }
            assert!(Instant::now() < end, "owner worker timeout");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    struct Lookup(AtomicUsize);
    #[cfg(windows)]
    #[test]
    fn targeted_owner_system_account_is_resolved() {
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let oracle = std::process::Command::new("powershell.exe")
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-NonInteractive", "-Command", "[Console]::OutputEncoding=[Text.Encoding]::UTF8; (Get-Acl -LiteralPath $env:SystemRoot).Owner"])
            .output().unwrap();
        assert!(oracle.status.success());
        let expected = String::from_utf8_lossy(&oracle.stdout)
            .trim()
            .trim_start_matches('\u{feff}')
            .to_owned();
        let task = Task::start().unwrap();
        task.request(1, vec![(key(1), system)]);
        let update = wait(&task, 1);
        assert!(matches!(&update.values[0].1, Value::Identified(name) if *name == expected));
        println!(
            "Windows system directory account matches independent Get-Acl; no directory enumeration"
        );
    }
    impl OwnerLookup for Lookup {
        fn lookup(&self, _: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
            self.0.fetch_add(1, Ordering::Relaxed);
            OwnerNameResolution::NotFound
        }
    }
    #[test]
    fn targeted_owner_worker_caches_sid_and_rejects_late_result() {
        #[cfg(windows)]
        let id = NativeOwnerId::WindowsSid(
            sortsize_core::WindowsSid::from_bytes(&[1, 1, 0, 0, 0, 0, 0, 5, 255, 255, 255, 255])
                .unwrap(),
        );
        #[cfg(target_os = "linux")]
        let id = NativeOwnerId::UnixUid(424242);
        let lookup = Arc::new(Lookup(AtomicUsize::new(0)));
        let (began, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let read: Arc<ReadOwner> = Arc::new(move |path| {
            if path == Path::new("slow") {
                began.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
            if path == Path::new("denied") {
                EntryOwner {
                    id: MetadataValue::AccessDenied,
                }
            } else if path == Path::new("missing") {
                EntryOwner {
                    id: MetadataValue::Unknown,
                }
            } else {
                EntryOwner {
                    id: MetadataValue::Known {
                        value: id,
                        accuracy: sortsize_core::ValueAccuracy::Exact,
                    },
                }
            }
        });
        let shared = Arc::new(Shared::default());
        let task = Task {
            shared: shared.clone(),
        };
        let provider = lookup.clone();
        let worker = std::thread::spawn(move || run(shared, provider, read));
        task.request(
            1,
            vec![
                (key(1), "first".into()),
                (key(2), "second".into()),
                (key(3), "denied".into()),
                (key(4), "missing".into()),
            ],
        );
        let update = wait(&task, 4);
        assert!(matches!(update.values[0].1, Value::Unresolved(_)));
        assert!(matches!(update.values[2].1, Value::Denied));
        assert!(matches!(update.values[3].1, Value::Unavailable));
        assert_eq!(
            lookup.0.load(Ordering::Relaxed),
            1,
            "shared SID resolved once"
        );
        let _scope = crate::presentation::Context {
            language: crate::locale::Language::French,
            generation: 1,
        }
        .enter();
        #[cfg(windows)]
        assert!(update.values[0].1.label().contains("SID non résolu"));
        #[cfg(target_os = "linux")]
        assert!(update.values[0].1.label().contains("UID"));
        assert_eq!(update.values[2].1.label(), "Accès refusé");
        task.request(2, vec![(key(5), "slow".into())]);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        task.request(3, vec![(key(6), "fresh".into())]);
        release.send(()).unwrap();
        let update = wait(&task, 1);
        assert_eq!(update.serial, task.shared.serial.load(Ordering::Acquire));
        assert_eq!(update.values[0].0, key(6));
        drop(task);
        worker.join().unwrap();
    }
}
fn unresolved_label() -> String {
    #[cfg(windows)]
    {
        tr!("Unresolved SID").into()
    }
    #[cfg(target_os = "linux")]
    {
        "UID".into()
    }
}
impl Value {
    fn label(&self) -> slint::SharedString {
        match self {
            Self::Identified(name) => name.as_str().into(),
            Self::Unresolved(id) => format!(
                "{} ({})",
                owner_id_text(*id).unwrap_or_default(),
                unresolved_label()
            )
            .into(),
            Self::Denied => tr!("Access denied").into(),
            Self::Unavailable => tr!("Unavailable").into(),
        }
    }
}
struct Request {
    serial: u64,
    epoch: u64,
    rows: Vec<(EntryKey, PathBuf)>,
}
#[derive(Clone)]
struct Update {
    serial: u64,
    values: Vec<(EntryKey, Value)>,
}
#[derive(Default)]
struct Mail {
    request: Option<Request>,
    update: Option<Update>,
}
#[derive(Default)]
struct Shared {
    mail: Mutex<Mail>,
    wake: Condvar,
    serial: AtomicU64,
    stop: AtomicBool,
}
struct Task {
    shared: Arc<Shared>,
}
impl Drop for Task {
    fn drop(&mut self) {
        // Native security and identity calls cannot be interrupted safely. Never join on the UI;
        // the sole detached worker owns its reservation until the call returns.
        let _mail = self.shared.mail.lock().unwrap();
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }
}
impl Task {
    fn start() -> std::io::Result<Self> {
        use sortsize_core::resources::{Category, Governor};
        let memory = Governor::global()
            .reserve(Category::Queries, 4 * 1024 * 1024, None)
            .ok_or_else(|| std::io::Error::other("owner memory unavailable"))?;
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("visible-native-owners".into())
            .spawn(move || {
                let _memory = memory;
                #[cfg(windows)]
                run(
                    worker,
                    Arc::new(SystemOwnerLookup::new(
                        SystemProviderConsent::AllowPotentialNetworkLookup,
                    )),
                    Arc::new(EntryOwner::query_windows_path),
                );
                #[cfg(target_os = "linux")]
                run(
                    worker,
                    Arc::new(sortsize_owner_resolution::LocalLinuxOwnerLookup),
                    Arc::new(read_linux_owner),
                );
            })?;
        Ok(Self { shared })
    }
    fn request(&self, epoch: u64, rows: Vec<(EntryKey, PathBuf)>) {
        let serial = self.shared.serial.fetch_add(1, Ordering::AcqRel) + 1;
        let mut mail = self.shared.mail.lock().unwrap();
        mail.update = None;
        mail.request = Some(Request {
            serial,
            epoch,
            rows,
        });
        self.shared.wake.notify_one();
    }
}
type ReadOwner = dyn Fn(&Path) -> EntryOwner + Send + Sync;
#[cfg(target_os = "linux")]
fn read_linux_owner(path: &Path) -> EntryOwner {
    use std::os::unix::fs::MetadataExt;
    EntryOwner {
        id: match std::fs::symlink_metadata(path) {
            Ok(metadata) => MetadataValue::Known {
                value: NativeOwnerId::UnixUid(metadata.uid()),
                accuracy: sortsize_core::ValueAccuracy::Exact,
            },
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                MetadataValue::AccessDenied
            }
            Err(_) => MetadataValue::Unknown,
        },
    }
}

fn run(shared: Arc<Shared>, lookup: Arc<dyn OwnerLookup>, read: Arc<ReadOwner>) {
    let mut names: VecDeque<(NativeOwnerId, Value)> = VecDeque::new();
    let mut paths: VecDeque<(PathBuf, Value)> = VecDeque::new();
    let mut path_bytes = 0;
    let mut epoch = None;
    loop {
        let request = {
            let mut mail = shared.mail.lock().unwrap();
            while mail.request.is_none() && !shared.stop.load(Ordering::Acquire) {
                mail = shared.wake.wait(mail).unwrap();
            }
            if shared.stop.load(Ordering::Acquire) {
                return;
            }
            mail.request.take().unwrap()
        };
        if epoch != Some(request.epoch) || request.rows.is_empty() {
            epoch = Some(request.epoch);
            paths.clear();
            path_bytes = 0;
        }
        let current = || {
            !shared.stop.load(Ordering::Acquire)
                && shared.serial.load(Ordering::Acquire) == request.serial
        };
        let mut values = Vec::new();
        for (key, path) in request.rows {
            if !current() {
                break;
            }
            let value = if let Some((_, value)) = paths.iter().find(|(p, _)| *p == path) {
                value.clone()
            } else {
                let owner = read(&path);
                if !current() {
                    break;
                }
                match owner.id {
                    MetadataValue::Known { value: id, .. } => {
                        if let Some((_, value)) = names.iter().find(|(cached, _)| *cached == id) {
                            value.clone()
                        } else {
                            let resolution = lookup.lookup(
                                id,
                                LookupLimits {
                                    max_buffer_bytes: 64 * 1024,
                                    max_name_bytes: NAME_BYTES,
                                },
                            );
                            let cacheable = matches!(
                                resolution,
                                OwnerNameResolution::Resolved(_) | OwnerNameResolution::NotFound
                            );
                            let value = match resolution {
                                OwnerNameResolution::Resolved(name) => {
                                    let name = crate::owner_view::display_name(name).to_string();
                                    if name.is_empty() {
                                        Value::Unavailable
                                    } else {
                                        Value::Identified(name)
                                    }
                                }
                                OwnerNameResolution::NotFound => Value::Unresolved(id),
                                OwnerNameResolution::AccessDenied => Value::Denied,
                                _ => Value::Unavailable,
                            };
                            if cacheable {
                                if names.len() == CACHE_ENTRIES {
                                    names.pop_front();
                                }
                                names.push_back((id, value.clone()));
                            }
                            value
                        }
                    }
                    MetadataValue::AccessDenied => Value::Denied,
                    _ => Value::Unavailable,
                }
            };
            if !current() {
                break;
            }
            // Only successful observations survive progressive republication.
            // Navigation/refresh changes epoch, forcing a fresh security read.
            if matches!(value, Value::Identified(_) | Value::Unresolved(_))
                && !paths.iter().any(|(p, _)| *p == path)
            {
                let bytes = path.as_os_str().as_encoded_bytes().len() + NAME_BYTES * 2;
                while !paths.is_empty()
                    && (paths.len() == CACHE_ENTRIES || path_bytes + bytes > PATH_BYTES)
                {
                    let (old, _) = paths.pop_front().unwrap();
                    path_bytes -= old.as_os_str().as_encoded_bytes().len() + NAME_BYTES * 2;
                }
                if bytes <= PATH_BYTES {
                    path_bytes += bytes;
                    paths.push_back((path, value.clone()));
                }
            }
            values.push((key, value));
            let mut mail = shared.mail.lock().unwrap();
            if current() {
                mail.update = Some(Update {
                    serial: request.serial,
                    values: values.clone(),
                });
            }
        }
    }
}

#[derive(Default)]
pub(super) struct VisibleOwners {
    task: Option<Task>,
    key: Option<(u64, Vec<EntryKey>)>,
    latest: Option<Update>,
    last_token: Option<i32>,
    submitted: Vec<EntryKey>,
}
impl Host {
    pub(super) fn poll_windows_owners(&mut self, w: &SortSizeWindow) {
        let state = &mut self.windows_owners;
        let page = self.current.as_ref().filter(|p| p.epoch == self.epoch);
        let visible = w.get_resolve_owner()
            && self.owner_live_source
            && !w.get_top_visible()
            && !w.get_search_visible()
            && w.get_column_settings().row_data(3).is_some_and(|c| c.shown);
        let Some(page) = page.filter(|_| visible) else {
            if state.key.take().is_some() {
                if let Some(task) = &state.task {
                    task.request(self.epoch, Vec::new());
                }
                state.latest = None;
                state.last_token = None;
                w.set_owner_status(crate::presentation::OwnerStatus::Idle.render().into());
                // Restore raw inventory values when resolution is stopped.
                if let Some(page) = page {
                    let model = w.get_details();
                    for (i, owner) in page.owners.iter().enumerate() {
                        if let Some(mut row) = model.row_data(i) {
                            row.owner = crate::owner_view::fallback(owner.id);
                            model.set_row_data(i, row);
                        }
                    }
                }
            }
            return;
        };
        if state.task.is_none() {
            match Task::start() {
                Ok(task) => state.task = Some(task),
                Err(_) => {
                    w.set_owner_status(tr!("Unavailable").into());
                    let model = w.get_details();
                    for i in 0..model.row_count() {
                        if let Some(mut row) = model.row_data(i) {
                            row.owner = tr!("Unavailable").into();
                            model.set_row_data(i, row);
                        }
                    }
                    return;
                }
            }
        }
        let first = ((-w.get_detail_scroll_y()).max(0.) / 34.) as usize;
        let count =
            ((w.get_detail_viewport_height().max(34.) / 34.).ceil() as usize + 2).min(MAX_ROWS);
        let end = first.saturating_add(count).min(page.frame.rows.len());
        let first = first.min(end);
        let rows = &page.frame.rows[first..end];
        let key = (self.epoch, rows.iter().map(|r| r.key).collect::<Vec<_>>());
        let changed = state.key.as_ref() != Some(&key);
        if changed {
            state.key = Some(key);
            state.latest = None;
            let mut bytes = 0;
            let request: Vec<_> = rows
                .iter()
                .filter(|r| {
                    matches!(
                        r.kind,
                        sortsize_store::inventory::Kind::File
                            | sortsize_store::inventory::Kind::Directory
                    )
                })
                .take_while(|r| {
                    bytes += r.path.as_os_str().as_encoded_bytes().len();
                    bytes <= PATH_BYTES
                })
                .map(|r| (r.key, r.path.clone()))
                .collect();
            state.submitted = request.iter().map(|(key, _)| *key).collect();
            state.task.as_ref().unwrap().request(self.epoch, request);
        }
        let task = state.task.as_ref().unwrap();
        let mut updated = false;
        // No waiting behind the worker on the UI thread.
        if let Ok(mut mail) = task.shared.mail.try_lock()
            && let Some(update) = mail
                .update
                .take()
                .filter(|u| u.serial == task.shared.serial.load(Ordering::Acquire))
        {
            state.latest = Some(update);
            updated = true;
        }
        if changed || updated || state.last_token != Some(page.token) {
            state.last_token = Some(page.token);
            let model = w.get_details();
            for (i, source) in rows.iter().enumerate() {
                if let Some(mut row) = model.row_data(first + i) {
                    row.owner = state
                        .latest
                        .as_ref()
                        .and_then(|u| u.values.iter().find(|(key, _)| *key == source.key))
                        .map(|(_, value)| value.label())
                        .unwrap_or_else(|| {
                            if state.submitted.contains(&source.key) {
                                tr!("Loading…").into()
                            } else {
                                tr!("Unavailable").into()
                            }
                        });
                    model.set_row_data(first + i, row);
                }
            }
            let mut names: Vec<_> = state
                .latest
                .as_ref()
                .into_iter()
                .flat_map(|u| &u.values)
                .filter_map(|(key, value)| {
                    let index = page.frame.rows.iter().position(|r| r.key == *key)?;
                    let MetadataValue::Known { value: id, .. } = page.owners.get(index)?.id else {
                        return None;
                    };
                    if let Value::Identified(_) = value {
                        Some(crate::owner_view::DisplayOwner {
                            id,
                            label: value.label(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            names.sort_by(|a, b| crate::owner_view::compare_id(a.id, b.id));
            names.dedup_by_key(|n| n.id);
            self.owner_names = names.into();
            self.owner_names_tag = Some((self.epoch, page.token));
            w.set_owner_status(
                crate::presentation::OwnerStatus::Page {
                    pending: state
                        .submitted
                        .len()
                        .saturating_sub(state.latest.as_ref().map_or(0, |u| u.values.len())),
                    waiting: false,
                }
                .render()
                .into(),
            );
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    #[test]
    fn linux_visible_owner_uses_link_metadata_and_preserves_unknown() {
        use std::os::unix::{fs::MetadataExt, fs::symlink};
        let fixture = tempfile::tempdir().unwrap();
        let link = fixture.path().join("link");
        symlink(fixture.path().join("absent-target"), &link).unwrap();
        let owner = read_linux_owner(&link);
        let expected = std::fs::symlink_metadata(&link).unwrap().uid();
        assert!(matches!(owner.id, MetadataValue::Known {
            value: NativeOwnerId::UnixUid(uid), ..
        } if uid == expected));
        assert_eq!(
            read_linux_owner(&fixture.path().join("absent")).id,
            MetadataValue::Unknown
        );
    }
}
