//! Saved measures and diff share the Atelier service owner. All cache paths are
//! local state; only explicit Verify/Resume creates a source Guard.
use super::*;
use sortsize_core::{
    CancellationToken,
    resources::{Category, Governor, Reservation},
};
use sortsize_store::{
    inventory::{
        Store,
        catalogue::{self, Saved},
        differential::{Change, Diff},
        lifecycle,
    },
    scheduler::State,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
mod deletion;
mod history;
pub(super) static CLEANUP_ERROR: Mutex<Option<String>> = Mutex::new(None);
pub(super) fn seconds(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(i64::MAX as u64) as i64)
}
pub(super) fn system_time(t: i64) -> sortsize_store::inventory::Result<SystemTime> {
    let value = if t >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(t as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(t.unsigned_abs()))
    };
    value.ok_or(sortsize_store::inventory::Error::Contract(
        "recorded timestamp unsupported by platform",
    ))
}

#[derive(Clone)]
pub(super) enum Action {
    Refresh,
    Save,
    SaveFast {
        source_epoch: u64,
        context: AcquisitionContext,
    },
    Disable,
    Open(Saved),
    Verify(Saved),
    Resume(Saved),
    Compare(Saved),
    CompareSaved(Saved, Box<Saved>),
    Rank(Saved),
    Evolution(Saved),
    Forget(Saved),
}
impl Action {
    pub(super) fn is_save(&self) -> bool {
        matches!(self, Self::Save | Self::SaveFast { .. })
    }

    pub(super) fn acquisition_source(&self) -> Option<&Path> {
        match self {
            Self::Verify(saved) | Self::Resume(saved) => Some(&saved.root),
            _ => None,
        }
    }
}
/// Take and publish the cancellation handle under the same lane lock. A
/// disconnect/reconnect between dequeue and run cannot revive this request.
pub(super) fn take_request(lane: &mut service::Lane) -> Option<Request> {
    discard_obsolete_request(lane);
    let request = lane.cache.take();
    if let Some(request) = &request
        && let Some(root) = request.action.acquisition_source()
    {
        lane.source_operation = Some((root.to_owned(), request.cancel.clone()));
        lane.source_operation_owner = Some((request.id, request.epoch));
        lane.source_operation_revoked = false;
    }
    request
}
/// A superseded cache action must not retire the backend of a newer source.
/// Call while holding the lane lock, before inspecting an action's ownership.
pub(super) fn discard_obsolete_request(lane: &mut service::Lane) {
    let obsolete = lane.cache.as_ref().is_some_and(|request| {
        request.cancel.is_cancelled()
            || (matches!(
                request.action,
                Action::Open(_) | Action::Verify(_) | Action::Resume(_)
            ) && request.epoch < lane.source_epoch)
    });
    if obsolete && let Some(request) = lane.cache.take() {
        request.cancel.cancel();
        clear_source_operation(lane, &request);
    }
}
fn clear_source_operation(lane: &mut service::Lane, request: &Request) {
    if lane.source_operation_owner == Some((request.id, request.epoch)) {
        lane.source_operation = None;
        lane.source_operation_owner = None;
        lane.source_operation_revoked = false;
    }
}
pub(super) struct Request {
    pub id: u64,
    pub epoch: u64,
    pub expected: u64,
    pub action: Action,
    pub cancel: CancellationToken,
}
pub(super) struct Prepared {
    pub id: u64,
    pub epoch: u64,
    pub presentation: crate::presentation::Context,
    pub catalogue: Vec<Saved>,
    pub labels: Vec<slint::SharedString>,
    history: history::Report,
    pub rows: Vec<crate::DiffRow>,
    pub status: String,
    pub diagnostic: String,
    pub summary: String,
    pub previous: bool,
    pub next: bool,
    pub token: i32,
    pub _memory: Option<Reservation>,
    pub _page: Option<sortsize_store::inventory::differential::Page>,
}
pub(super) struct PreviewPresentation {
    pub id: u64,
    pub epoch: u64,
    pub descriptor: String,
    pub presentation: crate::presentation::Context,
    pub labels: Vec<slint::SharedString>,
    pub _prefix: sortsize_store::inventory::preview::Prefix,
    pub _memory: Reservation,
}
pub(super) enum PreviewOutput {
    Prefix(Box<PreviewPresentation>),
    End { id: u64, epoch: u64, state: i32 },
}
fn preview(
    saved: &Saved,
    request: &Request,
    output: &Arc<Mutex<Option<PreviewOutput>>>,
) -> sortsize_store::inventory::Result<Store> {
    use sortsize_store::inventory::{Error, preview::Unverified};
    let unverified =
        Unverified::open_cancellable(&saved.directory, saved.context.generation, &request.cancel)?;
    let prefix = unverified.prefix(&request.cancel)?;
    if prefix.saved.context != saved.context {
        return Err(Error::Contract("preview catalogue identity changed"));
    }
    let memory = Governor::global()
        .reserve(
            Category::Publications,
            (256 * 1024 + prefix.rows.len() * 5 * 1024) as u64,
            None,
        )
        .ok_or(Error::Contract("preview presentation memory budget"))?;
    let descriptor = preview_descriptor(&prefix.saved);
    let labels = prefix
        .rows
        .iter()
        .map(|r| {
            format!(
                "{} · {} B",
                short_label(&r.path),
                r.logical.map_or_else(|| "?".into(), |n| n.to_string())
            )
            .into()
        })
        .collect();
    *output.lock().unwrap() = Some(PreviewOutput::Prefix(Box::new(PreviewPresentation {
        id: request.id,
        epoch: request.epoch,
        descriptor,
        presentation: crate::presentation::Context::current(),
        labels,
        _prefix: prefix,
        _memory: memory,
    })));
    unverified.verify(&request.cancel)
}
fn preview_descriptor(saved: &Saved) -> String {
    format!(
        "{} · {} → {} · G{}",
        clipped(&saved.root),
        date(saved.started),
        saved.measured.map_or_else(|| tr!("Unknown").into(), date),
        saved.context.generation.0
    )
}
fn date(t: i64) -> String {
    let Ok(time) = system_time(t) else {
        return tr!("Unknown").into();
    };
    crate::date_label::format_date(sortsize_core::MetadataValue::Known {
        value: time,
        accuracy: sortsize_core::ValueAccuracy::Exact,
    })
    .replace(".000000000", "")
    .replace(",000000000", "")
}
fn clipped(p: &Path) -> String {
    label(p).chars().take(160).collect()
}
fn kind(c: Change) -> &'static str {
    match c {
        Change::Appeared => tr!("Appeared"),
        Change::NewlyObserved => tr!("Newly observed"),
        Change::Removed => tr!("Removed from path"),
        Change::Replaced => tr!("Replaced"),
        Change::FieldsChanged => tr!("Fields changed"),
        Change::FieldsUnchanged => tr!("Fields unchanged"),
        Change::Unverified => tr!("Not verified"),
        Change::ProbableMove => tr!("Probable move"),
    }
}
/// The optional attachment can be a catalogue or an exact old repository.
/// Automatic maintenance only applies to the current catalogue.
pub(super) fn legacy_catalogue() -> Option<PathBuf> {
    std::env::var_os("BUNNY_AND_SIZE_HISTORY_DIR")
        .map(PathBuf::from)
        .or_else(|| lifecycle::default_root().ok())
}

/// Automatic snapshots have their own namespace. The historical family ledger
/// deliberately rejects foreign files, and explicit saved families stay intact.
pub(super) fn snapshot_root(root: &Path) -> PathBuf {
    let mut name = root.file_name().unwrap_or_default().to_os_string();
    name.push("-single-scans");
    root.with_file_name(name)
}

pub(super) fn is_snapshot(saved: &Saved) -> bool {
    saved
        .directory
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("auto-") && n.ends_with(".sqlite"))
}

fn append_snapshots(
    root: &Path,
    current: &mut catalogue::Catalogue,
) -> sortsize_store::inventory::Result<()> {
    let root = snapshot_root(root);
    if !root.exists() {
        return Ok(());
    }
    sortsize_core::acquisition::local_state_location(&root)?;
    let metadata = std::fs::symlink_metadata(&root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(sortsize_store::inventory::Error::Contract(
            "snapshot catalogue must be a local directory",
        ));
    }
    for item in std::fs::read_dir(&root)? {
        let item = item?;
        if !item
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("auto-") && n.ends_with(".sqlite"))
        {
            continue;
        }
        if current.rows.len() >= 128 {
            return Err(sortsize_store::inventory::Error::Contract(
                "cache catalogue rows/byte budget",
            ));
        }
        match sortsize_store::direct_snapshot::inspect(&item.path()) {
            Ok(meta)
                if current
                    .rows
                    .iter()
                    .any(|saved| saved.context == meta.context) => {}
            Ok(meta) => current.rows.push(Saved {
                directory: item.path(),
                context: meta.context,
                root: meta.root,
                started: meta.started,
                measured: Some(meta.measured),
                coverage: meta.summary.coverage,
                logical: meta.summary.measure.logical,
                unknown: meta.summary.measure.unknown,
                continuity: None,
            }),
            Err(_) => current.unavailable += 1,
        }
    }
    Ok(())
}

pub(super) fn list_catalogues(
    root: &Path,
    legacy: Option<&Path>,
    owners: &[&Store],
) -> sortsize_store::inventory::Result<catalogue::Catalogue> {
    let mut current = catalogue::list_open(root, owners)?;
    append_snapshots(root, &mut current)?;
    if let Some(legacy) = legacy.filter(|legacy| *legacy != root) {
        match catalogue::list_open(legacy, owners) {
            Ok(mut old) => {
                current.unavailable += old.unavailable;
                current.rows.append(&mut old.rows);
                current.rows.sort_by_key(|r| {
                    std::cmp::Reverse((
                        r.measured.unwrap_or(r.started),
                        r.context.inventory.0,
                        r.context.generation.0,
                    ))
                });
            }
            Err(_) => current.unavailable += 1,
        }
    }
    current.rows.sort_by_key(|r| {
        std::cmp::Reverse((
            r.measured.unwrap_or(r.started),
            r.context.inventory.0,
            r.context.generation.0,
        ))
    });
    Ok(current)
}

pub(super) fn prepared(
    request: &Request,
    root: &Path,
    legacy: Option<&Path>,
    active: Option<&service::Active>,
    borrowed: Option<&Store>,
    units: crate::units::Units,
) -> sortsize_store::inventory::Result<Prepared> {
    let memory = Governor::global()
        .reserve(Category::Publications, 1024 * 1024, None)
        .ok_or(sortsize_store::inventory::Error::Contract(
            "cache presentation memory; retry",
        ))?;
    let owners: Vec<&Store> = active
        .map(|a| a.session.store())
        .into_iter()
        .chain(borrowed)
        .collect();
    let catalogue = list_catalogues(root, legacy, &owners)?;
    let bytes = catalogue
        .rows
        .iter()
        .map(|r| r.root.capacity() + r.directory.capacity() + 1024)
        .sum::<usize>();
    if bytes > 512 * 1024 {
        return Err(sortsize_store::inventory::Error::Contract(
            "cache catalogue publication budget",
        ));
    }
    static TOKEN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    let token = TOKEN
        .try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| n.checked_add(1),
        )
        .map_err(|_| sortsize_store::inventory::Error::Contract("cache token exhausted"))?
        + 1;
    let labels = catalogue
        .rows
        .iter()
        .map(|r| {
            format!(
                "{} · {} · {} · {} · {}",
                date(r.measured.unwrap_or(r.started)),
                clipped(&r.root),
                history::coverage(r.coverage),
                history::size(units, r.logical),
                if r.measured.is_none() {
                    tr!("Interrupted acquisition")
                } else if is_snapshot(r) {
                    tr!("Last automatic scan")
                } else {
                    tr!("Retained date")
                }
            )
            .into()
        })
        .collect();
    Ok(Prepared {
        diagnostic: String::new(),
        id: request.id,
        epoch: request.epoch,
        presentation: crate::presentation::Context::current(),
        catalogue: catalogue.rows,
        labels,
        history: history::Report::default(),
        rows: vec![],
        status: format!(
            "{} {}",
            if active.is_some_and(
                |a| a.workspace.is_none() && matches!(a.session, service::Backend::Cached(_))
            ) {
                tr!("Saved locally. Source not queried.")
            } else {
                tr!("Local cache catalogue.")
            },
            if catalogue.unavailable > 0 {
                format!("{}: {}", tr!("Unavailable caches"), catalogue.unavailable)
            } else {
                String::new()
            }
        ),
        summary: String::new(),
        previous: false,
        next: false,
        token,
        _memory: Some(memory),
        _page: None,
    })
}
pub(super) fn error(request: &Request, message: String) -> Prepared {
    Prepared {
        id: request.id,
        epoch: request.epoch,
        presentation: crate::presentation::Context::current(),
        catalogue: vec![],
        labels: vec![],
        history: history::Report::default(),
        rows: vec![],
        status: tr!("Saved measure operation failed.").into(),
        diagnostic: message.chars().take(4096).collect(),
        summary: String::new(),
        previous: false,
        next: false,
        token: 0,
        _memory: None,
        _page: None,
    }
}

#[cfg(test)]
#[test]
fn superseded_cache_verification_cannot_retire_the_current_source() {
    use sortsize_core::inventory_contract::*;
    let saved = Saved {
        directory: PathBuf::from("snapshot.sqlite"),
        context: AcquisitionContext {
            inventory: InventoryId(1),
            generation: GenerationId(1),
            source: SourceId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        },
        root: PathBuf::from("source-a"),
        started: 0,
        measured: None,
        coverage: ScopeCoverage::Partial,
        logical: None,
        unknown: 0,
        continuity: None,
    };
    for cancelled in [false, true] {
        let cancellation = CancellationToken::default();
        if cancelled {
            cancellation.cancel();
        }
        let current_owner = CancellationToken::default();
        let mut lane = service::Lane::default();
        lane.source_epoch = 6;
        lane.cache = Some(Request {
            id: 7,
            epoch: if cancelled { 6 } else { 5 },
            expected: 4,
            action: Action::Verify(saved.clone()),
            cancel: cancellation.clone(),
        });
        lane.source_operation = Some((PathBuf::from("source-b"), current_owner.clone()));
        lane.source_operation_owner = Some((8, 6));
        discard_obsolete_request(&mut lane);
        assert!(lane.cache.is_none());
        assert!(cancellation.is_cancelled());
        assert_eq!(lane.source_operation_owner, Some((8, 6)));
        assert!(!current_owner.is_cancelled());
        lane.cache = Some(Request {
            id: 9,
            epoch: 6,
            expected: 6,
            action: Action::Verify(saved.clone()),
            cancel: Default::default(),
        });
        discard_obsolete_request(&mut lane);
        assert!(
            lane.cache.is_some(),
            "current explicit verification must remain queued"
        );
    }
}

#[cfg(test)]
#[test]
fn technical_cache_diagnostics_do_not_replace_localized_product_status() {
    let request = Request {
        id: 1,
        epoch: 2,
        expected: 0,
        action: Action::Refresh,
        cancel: Default::default(),
    };
    for language in crate::locale::Language::ALL {
        let context = crate::presentation::Context {
            language,
            generation: 1,
        };
        let _scope = context.enter();
        let failure = error(
            &request,
            "SQLite technical diagnostic / private-native-name".into(),
        );
        assert!(!failure.status.contains("SQLite"));
        assert_eq!(
            failure.diagnostic,
            "SQLite technical diagnostic / private-native-name"
        );
        assert!(!failure.status.is_empty());
        if language != crate::locale::Language::English {
            assert_ne!(failure.status, "Saved measure operation failed.");
        }
    }
    assert_eq!(
        error(&request, "é".repeat(10000))
            .diagnostic
            .chars()
            .count(),
        4096
    );
}
pub(super) fn run(
    request: Request,
    active: &mut Option<service::Active>,
    lane: &Arc<Mutex<service::Lane>>,
    output: &Arc<Mutex<Option<Prepared>>>,
    navigation: &Arc<Mutex<Option<service::Output>>>,
    preview_output: &Arc<Mutex<Option<PreviewOutput>>>,
) {
    if matches!(request.action, Action::Verify(_) | Action::Resume(_))
        && lane.lock().unwrap().writer_owns_session
    {
        let mut result = error(&request, String::new());
        result.status =
            tr!("Wait for the current acquisition to finish before verifying or resuming.").into();
        clear_source_operation(&mut lane.lock().unwrap(), &request);
        *output.lock().unwrap() = Some(result);
        return;
    }
    {
        let mut l = lane.lock().unwrap();
        if let Some(root) = request.action.acquisition_source() {
            if service::source_disconnected(&l, root) || request.cancel.is_cancelled() {
                let mut result = error(&request, String::new());
                result.status = if request.cancel.is_cancelled() {
                    tr!("Cancelled")
                } else {
                    tr!("This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning.")
                }.into();
                clear_source_operation(&mut l, &request);
                *output.lock().unwrap() = Some(result);
                return;
            }
            // take_request may have registered this operation before the
            // worker entered run. Preserve any revocation during that window.
            if l.source_operation_owner != Some((request.id, request.epoch)) {
                l.source_operation = Some((root.to_owned(), request.cancel.clone()));
                l.source_operation_owner = Some((request.id, request.epoch));
                l.source_operation_revoked = false;
            }
        }
        l.operation_cancel = Some(request.cancel.clone());
    }
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let configured = lane.lock().unwrap().cache_root.clone();
        let legacy = configured.is_none().then(legacy_catalogue).flatten();
        let mut root = configured.map_or_else(
            || service::work_location().map(|location| location.catalogue()),
            Ok,
        )?;
        sortsize_core::acquisition::local_state_location(&root)?;
        if root.exists() {
            sortsize_core::acquisition::local_state_location(&root)?;
            root = std::fs::canonicalize(&root)?;
        }
        if let Action::Verify(saved) | Action::Resume(saved) = &request.action {
            let location = service::work_location()?;
            // Automatic snapshots are regular files. Their parent is the
            // workspace to check here. open_saved verifies the file itself.
            let directory = if is_snapshot(saved) {
                saved
                    .directory
                    .parent()
                    .ok_or("snapshot has no catalogue directory")?
            } else {
                &saved.directory
            };
            service::require_work_location(&location, directory)?;
        }
        let reused_workspace = active.as_ref().and_then(|a| a.workspace.clone());
        if matches!(
            request.action,
            Action::Open(_) | Action::Verify(_) | Action::Resume(_)
        ) && (matches!(request.action, Action::Open(_))
            || active.as_ref().is_none_or(|a| a.epoch == request.expected))
            && let Some(a) = active.take()
        {
            a.session.control().cancel();
            drop(a);
        }
        if request.cancel.is_cancelled() {
            return Ok(());
        }
        if let Action::SaveFast {
            source_epoch,
            context,
        } = &request.action
        {
            let l = lane.lock().unwrap();
            if l.source_epoch != *source_epoch
                || l.fast_preview_context != Some(*context)
                || !active
                    .as_ref()
                    .is_some_and(|a| a.source_epoch == *source_epoch && a.c == *context)
            {
                return Ok(());
            }
        }
        if !matches!(
            request.action,
            Action::Refresh
                | Action::Open(_)
                | Action::Rank(_)
                | Action::Evolution(_)
                | Action::CompareSaved(..)
        ) && active.as_ref().is_some_and(|a| {
            a.epoch != request.expected
                && !(request.action.is_save()
                    && a.source_epoch == lane.lock().unwrap().source_epoch)
        }) {
            return Ok(());
        }
        if matches!(request.action, Action::Save | Action::Disable)
            && let Some(a) = active.as_ref()
            && a.workspace.is_none()
            && matches!(a.session, service::Backend::Cached(_))
        {
            let location = service::work_location()?;
            service::require_work_location(&location, a.session.store().directory())?;
            *active = Some(active.take().unwrap().writable()?);
        }
        if request.cancel.is_cancelled() {
            return Ok(());
        }
        let mut deletion_journal_failed = false;
        match &request.action {
            Action::Refresh => {
                if root.exists() && !cfg!(feature = "experimental-preview") {
                    catalogue::prune(&root, seconds(SystemTime::now()))?;
                }
            }
            Action::Save | Action::SaveFast { .. } => {
                let location = service::work_location()?;
                service::require_work_location(&location, &root)?;
                if let Some(a) = active.as_ref()
                    && (!a.session.can_place_store(&root)?
                        || sortsize_core::acquisition::contains_path(&a.root, &root)
                            && !location.contains(&root)?)
                {
                    return Err(
                        "cache overlaps the acquired source outside its verified workspace".into(),
                    );
                }
                lifecycle::ensure_root(&root)?;
                root = std::fs::canonicalize(&root)?;
                lifecycle::account_root(&root)?;
                let a = active
                    .as_mut()
                    .ok_or(tr!("No measured inventory selected."))?;
                // Direct consultation creates a temporary compatibility reader
                // for each explicit Save. Reuse the already retained immutable
                // acquisition instead of appending another copy on every click.
                let already_retained = if a.is_snapshot_reference() {
                    let catalog = catalogue::list_open(&root, &[])?;
                    if let Some(saved) = catalog
                        .rows
                        .iter()
                        .find(|saved| saved.context == a.c && saved.root == a.root)
                    {
                        let verified = service::Active::open_saved(
                            request.epoch,
                            saved,
                            None,
                            &request.cancel,
                        )?;
                        if verified.c != a.c
                            || verified.root != a.root
                            || !matches!(verified.session, service::Backend::Certified(_))
                        {
                            return Err(
                                "retained acquisition is not a matching certified generation"
                                    .into(),
                            );
                        }
                        true
                    } else {
                        let prefix = format!("inv-{:032x}", a.c.inventory.0);
                        for entry in std::fs::read_dir(&root)? {
                            let entry = entry?;
                            if entry
                                .file_name()
                                .to_str()
                                .is_some_and(|name| name.starts_with(&prefix))
                            {
                                return Err("existing retained acquisition is unavailable; preserve it and inspect the cache".into());
                            }
                        }
                        false
                    }
                } else {
                    false
                };
                if already_retained {
                    // The generation was opened through the certified factory.
                    // A repeated Save leaves its bytes and retention unchanged.
                } else if a.workspace.is_some() {
                    a.retain(&root, &request.cancel)?;
                } else if a.session.store().directory().parent() == Some(root.as_path()) {
                    a.session.store_mut()?.set_retention(true, 30)?;
                } else {
                    let destination = root.join(format!("inv-{:032x}", a.c.inventory.0));
                    let retained = a.session.store_mut()?.retain_copy(&destination, 30)?;
                    a.replace_store(retained)?;
                }
                a.force = true;
            }
            Action::Disable => {
                let a = active
                    .as_mut()
                    .ok_or(tr!("No measured inventory selected."))?;
                if let Some(workspace) = &a.workspace {
                    workspace.require_writer()?;
                    let owner = workspace.repository();
                    let mut repository = owner.lock().unwrap();
                    let days = repository.retention()?.days;
                    repository.set_retention(false, days)?;
                } else {
                    let days = a.session.store().retention()?.days;
                    a.session.store_mut()?.set_retention(false, days)?;
                }
            }
            Action::Open(saved) => {
                lane.lock().unwrap().phase = Some((
                    request.epoch,
                    service::Phase::Reopening,
                    saved.context.generation,
                ));
                if let Some(a) = active.take() {
                    a.session.control().cancel();
                    drop(a);
                }
                // Legacy previews remain explicitly provisional; final reads use
                // the certified factory, including saved repository generations.
                if cfg!(feature = "experimental-preview")
                    && saved.directory.join("inventory.db").exists()
                {
                    drop(preview(saved, &request, preview_output)?);
                }
                if request.cancel.is_cancelled() {
                    return Ok(());
                }
                #[cfg(feature = "benchmarks")]
                let worker_open = std::time::Instant::now();
                *active = Some(service::Active::open_saved(
                    request.epoch,
                    saved,
                    reused_workspace.clone(),
                    &request.cancel,
                )?);
                #[cfg(feature = "benchmarks")]
                {
                    lane.lock().unwrap().cache_open_timing = Some((
                        request.id,
                        std::time::Instant::now(),
                        worker_open.elapsed().as_secs_f64() * 1000.,
                    ));
                }
                if cfg!(feature = "experimental-preview") {
                    *preview_output.lock().unwrap() = Some(PreviewOutput::End {
                        id: request.id,
                        epoch: request.epoch,
                        state: 3,
                    });
                }
            }
            Action::Verify(saved) => {
                if let Some(a) = active.take() {
                    a.session.control().cancel();
                    drop(a);
                }
                let reference = service::Active::open_saved(
                    request.epoch,
                    saved,
                    reused_workspace.clone(),
                    &request.cancel,
                )?;
                let after =
                    list_catalogues(&root, legacy.as_deref(), &[reference.session.store()])?
                        .rows
                        .iter()
                        .filter(|record| {
                            record.context.inventory == saved.context.inventory
                                && record.context.source == saved.context.source
                        })
                        .map(|record| record.context.generation)
                        .max()
                        .unwrap_or(saved.context.generation);
                let publication_cancel = lane.lock().unwrap().publication_cancel.clone();
                let (writer, reader) = service::Active::verified(
                    request.epoch,
                    reference,
                    after,
                    &request.cancel,
                    publication_cancel,
                )?;
                *active = Some(writer);
                let mut l = lane.lock().unwrap();
                let previous = l.reader_target.replace(Some(Box::new(reader)));
                l.reader_active = true;
                drop(l);
                drop(previous);
            }
            Action::Resume(saved) => {
                if saved.measured.is_some() {
                    return Err(tr!("This measure is sealed. Verify it explicitly.").into());
                }
                if let Some(a) = active.take() {
                    a.session.control().cancel();
                    drop(a);
                }
                let publication_cancel = lane.lock().unwrap().publication_cancel.clone();
                *active = Some(service::Active::resume_saved(
                    request.epoch,
                    sortsize_core::acquisition::Policy {
                        explicit_root: true,
                        ..Default::default()
                    },
                    saved,
                    reused_workspace.clone(),
                    &request.cancel,
                    publication_cancel,
                )?);
            }
            Action::Forget(saved) => {
                let journal = deletion::Journal::open(&root)?;
                journal.record("requested")?;
                let deleted = (|| -> Result<(), Box<dyn std::error::Error>> {
                    if is_snapshot(saved) {
                        let _pending = service::lock_snapshot_deletion(saved).map_err(|_| {
                            tr!("Select another inventory before forgetting this cache.")
                        })?;
                        let snapshot = sortsize_store::direct_snapshot::inspect(&saved.directory)?;
                        if snapshot.context != saved.context
                            || snapshot.root != saved.root
                            || Some(snapshot.measured) != saved.measured
                        {
                            return Err("saved snapshot changed; refresh the catalogue".into());
                        }
                        let allowed = snapshot_root(&root);
                        let parent = saved
                            .directory
                            .parent()
                            .ok_or("snapshot has no catalogue directory")?;
                        if std::fs::canonicalize(parent)? != std::fs::canonicalize(&allowed)? {
                            return Err(
                                "snapshot is outside the current automatic catalogue".into()
                            );
                        }
                        let canonical = std::fs::canonicalize(&saved.directory)?;
                        if request.cancel.is_cancelled() {
                            return Err("saved measure deletion cancelled".into());
                        }
                        std::fs::remove_file(&saved.directory)?;
                        let resources = sortsize_core::resources::Governor::global();
                        resources.forget_deleted_artifact_group(&canonical);
                        resources.forget_deleted_artifact_group(&saved.directory);
                        if let Some(name) = saved.directory.file_name() {
                            resources.forget_deleted_artifact_group(&allowed.join(name));
                        }
                    } else {
                        if active.as_ref().is_some_and(|a| {
                            a.session.store().directory() == saved.directory
                                || a.retained_copy.as_ref() == Some(&saved.directory)
                                || a.workspace
                                    .as_ref()
                                    .is_some_and(|owner| owner.path() == saved.directory)
                        }) {
                            return Err(tr!(
                                "Select another inventory before forgetting this cache."
                            )
                            .into());
                        }
                        deletion::forget_inventory(saved, &request.cancel)?;
                    }
                    Ok(())
                })();
                let outcome = journal.record(if deleted.is_ok() {
                    "deleted"
                } else {
                    "refused-or-failed"
                });
                deleted?;
                deletion_journal_failed = outcome.is_err();
            }
            Action::Rank(_) | Action::Evolution(_) => {}
            Action::Compare(saved) | Action::CompareSaved(saved, _) => {
                let pair = if let Action::CompareSaved(_, newer) = &request.action {
                    Some(newer.as_ref())
                } else {
                    None
                };
                let scratch = pair
                    .map(|_| service::work_location()?.temporary("bunny-history-diff-"))
                    .transpose()?;
                let newer = pair
                    .map(|s| history::open_selected(&request, s))
                    .transpose()?;
                let a = newer
                    .as_ref()
                    .or_else(|| {
                        active.as_ref().filter(|a| {
                            a.sealed.is_some() && a.session.status().state == State::Sealed
                        })
                    })
                    .ok_or(tr!("Wait for a measured generation before comparing."))?;
                let prior = if pair.is_some() {
                    Some(history::open_selected(&request, saved)?)
                } else if a.session.store().directory() == saved.directory {
                    None
                } else {
                    Some(service::Active::open_saved(
                        request.epoch,
                        saved,
                        a.workspace.clone(),
                        &request.cancel,
                    )?)
                };
                let old = prior
                    .as_ref()
                    .map_or(a.session.store(), |prior| prior.session.store());
                let path = scratch
                    .as_ref()
                    .map_or(a._temporary.path(), |scratch| scratch.path());
                let mut diff = Diff::start(
                    old.lease(saved.context.generation)?,
                    a.session.store().lease(a.c.generation)?,
                    path,
                    1024 * 1024 * 1024,
                )?;
                while !diff.advance(&request.cancel)? {
                    if lane.lock().unwrap().stop {
                        return Ok(());
                    }
                }
                if !diff.summary().finished {
                    return Ok(());
                }
                diff.prioritize_changes(&request.cancel)?;
                let mut cursor = None;
                let mut reverse = false;
                let changes = pair
                    .map(|_| diff.root_folder_changes(path, &request.cancel))
                    .transpose()?;
                loop {
                    if request.cancel.is_cancelled() || lane.lock().unwrap().stop {
                        return Ok(());
                    }
                    let (presentation, units) = {
                        let l = lane.lock().unwrap();
                        (l.presentation, l.units)
                    };
                    let _scope = presentation.enter();
                    let mut result = prepared(
                        &request,
                        &root,
                        legacy.as_deref(),
                        active.as_ref().or(pair.map(|_| a)),
                        prior.as_ref().map(|prior| prior.session.store()),
                        units,
                    )?;
                    if pair.is_none() {
                        let scope = lane.lock().unwrap().top_scope.clone();
                        let page = a.prepare(units, scope)?;
                        *navigation.lock().unwrap() = Some(service::Output::Page(Box::new(page)));
                    }
                    if let Some(changes) = &changes {
                        result.history = history::contributions(&diff, changes, units);
                    }
                    let page = diff.page(cursor.as_ref(), reverse)?;
                    let summary = diff.summary();
                    let (old, new) = diff.measurements();
                    let delta = summary
                        .comparable_delta
                        .as_ref()
                        .and_then(|s| s.parse::<i128>().ok())
                        .map_or_else(|| tr!("Unavailable").into(), |n| history::delta(units, n));
                    result.summary = format!(
                        "{}: {} · G{} · {} · {}\n{}: {} · G{} · {} · {}\n{}: {}",
                        tr!("Previous measure"),
                        date(old.measured),
                        old.context.generation.0,
                        history::coverage(old.coverage),
                        history::size(units, summary.old_logical),
                        if pair.is_some() {
                            tr!("Later measure")
                        } else {
                            tr!("Current measure")
                        },
                        date(new.measured),
                        new.context.generation.0,
                        history::coverage(new.coverage),
                        history::size(units, summary.new_logical),
                        tr!("Comparable delta"),
                        delta
                    );
                    result.rows = page
                        .rows
                        .iter()
                        .map(|e| crate::DiffRow {
                            change: kind(e.change).into(),
                            old_path: e.old.as_deref().map_or_else(|| "".into(), clipped).into(),
                            new_path: e.new.as_deref().map_or_else(|| "".into(), clipped).into(),
                        })
                        .collect();
                    result.previous = if reverse { page.more } else { cursor.is_some() };
                    result.next = if reverse { cursor.is_some() } else { page.more };
                    let first = page.first.clone();
                    let next_cursor = page.next.clone();
                    result._page = Some(page);
                    *output.lock().unwrap() = Some(result);
                    loop {
                        if request.cancel.is_cancelled() || lane.lock().unwrap().stop {
                            return Ok(());
                        }
                        if let Some((id, next)) = lane.lock().unwrap().cache_page.take()
                            && id == request.id
                        {
                            reverse = !next;
                            cursor = if next {
                                next_cursor.clone()
                            } else {
                                first.clone()
                            };
                            break;
                        }
                        let changed = {
                            let l = lane.lock().unwrap();
                            l.presentation != presentation || l.units != units
                        };
                        if changed {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
        let (presentation, units) = {
            let l = lane.lock().unwrap();
            (l.presentation, l.units)
        };
        let _scope = presentation.enter();
        let mut result = prepared(
            &request,
            &root,
            legacy.as_deref(),
            active.as_ref(),
            None,
            units,
        )?;
        match &request.action {
            Action::Forget(_) => {
                result.status = if deletion_journal_failed {
                    tr!("Saved measure deleted. Journal update failed.")
                } else {
                    tr!("Saved measure deleted.")
                }
                .into();
            }
            Action::Rank(saved) => result.history = history::ranking(&request, saved, units)?,
            Action::Evolution(saved) => {
                result.history = history::evolution(saved, &result.catalogue, units)
            }
            Action::Save | Action::SaveFast { .. } => {
                if let Some(a) = active.as_ref()
                    && let Some(saved) = result
                        .catalogue
                        .iter()
                        .find(|s| s.context == a.c && history::retained(s))
                {
                    result.status = format!(
                        "{} · {}",
                        tr!("Date retained locally"),
                        date(saved.measured.unwrap_or(saved.started))
                    );
                }
            }
            _ => {}
        }
        if matches!(request.action, Action::Disable) {
            result.status =
                tr!("Retention disabled. Local purge waits for readers to close.").into();
        }
        if !request.cancel.is_cancelled() {
            *output.lock().unwrap() = Some(result);
        }
        Ok(())
    })();
    if request.action.acquisition_source().is_some() {
        let mut l = lane.lock().unwrap();
        if let Some(a) = active
            .as_mut()
            .filter(|a| matches!(a.session, service::Backend::Live(_)))
        {
            let revoked = service::source_disconnected(&l, &a.root)
                || (l.source_operation_owner == Some((request.id, request.epoch))
                    && l.source_operation_revoked);
            if revoked {
                a.request_cancel();
            } else if request.cancel.is_cancelled() {
                a.cancel_acquisition();
            }
            l.acquisition_root = Some(a.root.clone());
            l.acquisition_control = Some((a.source_epoch, a.c.generation, a.session.control()));
            l.acquisition_publication = Some((a.root.clone(), a.publication_token()));
        }
        clear_source_operation(&mut l, &request);
    }
    if let Err(e) = result {
        if matches!(request.action, Action::Open(_)) && cfg!(feature = "experimental-preview") {
            *preview_output.lock().unwrap() = Some(PreviewOutput::End {
                id: request.id,
                epoch: request.epoch,
                state: if request.cancel.is_cancelled() { 5 } else { 4 },
            });
        }
        if request.cancel.is_cancelled() {
            return;
        }
        if matches!(
            request.action,
            Action::Open(_)
                | Action::Verify(_)
                | Action::Resume(_)
                | Action::Save
                | Action::SaveFast { .. }
                | Action::Disable
        ) {
            let epoch = if let Action::SaveFast { source_epoch, .. } = request.action {
                let l = lane.lock().unwrap();
                if l.source_epoch != source_epoch {
                    return;
                }
                l.view_epoch
            } else {
                request.epoch
            };
            *navigation.lock().unwrap() = Some(service::Output::Error(
                epoch,
                e.to_string(),
                active.is_some(),
            ));
        }
        *output.lock().unwrap() = Some(error(&request, e.to_string()));
    }
}
impl Host {
    pub(super) fn cancel_preview(&mut self, w: &SortSizeWindow) {
        self.cache_cancel.cancel();
        let retired = self.epoch;
        self.epoch();
        let mut lane = self.service.lane.lock().unwrap();
        lane.retired_through = lane.retired_through.max(retired);
        lane.navigation = None;
        drop(lane);
        self.current = None;
        self.pending = None;
        self.active_service = false;
        self.cache_wait = None;
        self.clear_preview(w, 5);
        w.set_busy(false);
        w.set_status(tr!("Cancelled").into());
    }
    pub(super) fn clear_preview(&mut self, w: &SortSizeWindow, state: i32) {
        self.preview_current = None;
        w.set_preview_rows(ModelRc::default());
        w.set_preview_descriptor("".into());
        w.set_preview_state(state);
        #[cfg(feature = "benchmarks")]
        i_slint_backend_winit::presentation_probe::phase(state as u8);
    }
    pub(super) fn refresh_cache(&mut self, w: &SortSizeWindow) {
        if w.get_preview_state() != 3 {
            self.clear_preview(w, 0);
        }
        self.cache_cancel.cancel();
        self.cache_cancel = CancellationToken::default();
        self.cache_id += 1;
        self.cache_wait = Some(self.epoch);
        self.cache_comparing = false;
        clear_report(w);
        w.set_cache_token(0);
        w.set_cache_diagnostic("".into());
        w.set_cache_diagnostic_visible(false);
        w.set_cache_status(tr!("Preparing saved measures…").into());
        let mut lane = self.service.lane.lock().unwrap();
        self.cache_autosave_revision = lane
            .autosave_revision
            .load(std::sync::atomic::Ordering::Acquire);
        lane.cache = Some(Request {
            id: self.cache_id,
            epoch: self.epoch,
            expected: self.epoch,
            action: Action::Refresh,
            cancel: self.cache_cancel.clone(),
        });
    }

    pub(super) fn save_fast_inventory(&mut self, w: &SortSizeWindow) {
        let mut lane = self.service.lane.lock().unwrap();
        let Some(context) = lane.fast_preview_context else {
            return;
        };
        if !w.get_scan_fast_can_save()
            || lane.writer_running
            || lane.cache.is_some()
            || !matches!(lane.phase, Some((epoch, service::Phase::PreviewReady, _))
                if epoch == lane.source_epoch && self.displays_fast_preview(lane.fast_preview_context))
        {
            return;
        }
        self.cache_cancel.cancel();
        self.cache_id += 1;
        self.cache_wait = Some(self.epoch);
        lane.cache = Some(Request {
            id: self.cache_id,
            epoch: self.epoch,
            expected: self.epoch,
            action: Action::SaveFast {
                source_epoch: lane.source_epoch,
                context,
            },
            cancel: CancellationToken::default(),
        });
        drop(lane);
        w.set_errors("".into());
        w.set_scan_fast_can_save(false);
    }

    pub(super) fn cache_action(
        &mut self,
        w: &SortSizeWindow,
        kind: i32,
        index: i32,
        token: i32,
        controller: &mut Controller,
    ) {
        if kind == 7 {
            if matches!(w.get_preview_state(), 1 | 2) {
                self.cancel_preview(w);
            }
            self.clear_preview(w, 5);
            self.cache_cancel.cancel();
            self.cache_id += 1;
            self.cache_wait = None;
            self.cache_comparing = false;
            w.set_cache_visible(false);
            return;
        }
        if kind != 0
            && self
                .cache_current
                .as_ref()
                .is_none_or(|p| p.token != token || p.epoch != self.epoch || p.id != self.cache_id)
        {
            return;
        }
        let legacy_fast_preview = {
            let lane = self.service.lane.lock().unwrap();
            lane.direct_view.is_none() && self.displays_fast_preview(lane.fast_preview_context)
        };
        if kind == 1 && legacy_fast_preview {
            self.save_fast_inventory(w);
            return;
        }
        if kind == 0 && matches!(w.get_preview_state(), 1 | 2) {
            self.cancel_preview(w);
        }
        let saved = usize::try_from(index)
            .ok()
            .and_then(|i| self.cache_current.as_ref()?.catalogue.get(i))
            .cloned();
        let action = match kind {
            0 => Action::Refresh,
            1 => Action::Save,
            8 => Action::Disable,
            2 => {
                if let Some(s) = saved {
                    Action::Open(s)
                } else {
                    return;
                }
            }
            3 => {
                if let Some(s) = saved {
                    Action::Verify(s)
                } else {
                    return;
                }
            }
            4 => {
                if let Some(s) = saved {
                    Action::Resume(s)
                } else {
                    return;
                }
            }
            5 => {
                if let Some(s) = saved {
                    Action::Compare(s)
                } else {
                    return;
                }
            }
            9 => match saved.filter(|s| s.measured.is_some()) {
                Some(s) => Action::Rank(s),
                None => return,
            },
            10 => {
                let Some(mut earlier) = saved.filter(history::retained) else {
                    return;
                };
                let Some(mut later) = w
                    .get_cache_other()
                    .checked_sub(1)
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| {
                        self.cache_current
                            .as_ref()?
                            .catalogue
                            .iter()
                            .filter(|s| history::retained(s))
                            .nth(i)
                    })
                    .cloned()
                else {
                    return;
                };
                if earlier.context == later.context {
                    w.set_cache_status(tr!("Choose two different retained dates.").into());
                    return;
                }
                if (earlier.measured, earlier.started) > (later.measured, later.started) {
                    std::mem::swap(&mut earlier, &mut later);
                }
                Action::CompareSaved(earlier, Box::new(later))
            }
            11 => match saved.filter(history::retained) {
                Some(s) => Action::Evolution(s),
                None => return,
            },
            6 if w.get_cache_manage()
                && w.get_cache_confirm()
                && index == w.get_cache_choice()
                && !w.get_busy() =>
            {
                if let Some(s) = saved.filter(|s| s.measured.is_some()) {
                    Action::Forget(s)
                } else {
                    return;
                }
            }
            _ => return,
        };
        if let Some(root) = action.acquisition_source()
            && (controller.registry.is_disconnected(root) || self.service.source_disconnected(root))
        {
            w.set_cache_status(tr!("This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning.").into());
            return;
        }
        let acquisition_pending = {
            let lane = self.service.lane.lock().unwrap();
            lane.writer_owns_session || lane.writer_epoch.is_some() || lane.start.is_some()
        };
        if matches!(action, Action::Verify(_) | Action::Resume(_)) && acquisition_pending {
            w.set_cache_status(
                tr!("Wait for the current acquisition to finish before verifying or resuming.")
                    .into(),
            );
            return;
        }
        self.cache_cancel.cancel();
        self.clear_preview(
            w,
            if matches!(action, Action::Open(_)) && cfg!(feature = "experimental-preview") {
                1
            } else {
                0
            },
        );
        self.cache_cancel = CancellationToken::default();
        self.cache_id += 1;
        self.close_search(w);
        let expected = self.epoch;
        if let Action::Open(saved) | Action::Verify(saved) | Action::Resume(saved) = &action {
            #[cfg(any(windows, target_os = "linux"))]
            {
                self.owner_live_source = false;
            }
            self.requested = Some(saved.root.clone());
            w.set_scan_path(controller.path_input.select(saved.root.clone()).into());
            controller.scan_root = None;
            #[cfg(any(target_os = "linux", windows))]
            {
                controller.scan_volume = None;
            }
            w.set_scan_source("".into());
            w.set_selected_volume(-1);
            w.set_scanned_volume(-1);
        }
        if matches!(
            action,
            Action::Open(_) | Action::Verify(_) | Action::Resume(_)
        ) {
            self.failed = false;
            self.epoch();
            let mut lane = self.service.lane.lock().unwrap();
            lane.source_epoch = self.epoch;
            lane.view_epoch = self.epoch;
            lane.navigation = None;
            lane.desired_folder = None;
            lane.desired_relative = None;
            lane.acquisition_control = None;
            lane.cancel_publication();
            lane.publication_cancel = Default::default();
            drop(lane);
            self.current = None;
            self.pending = None;
            self.pending_folder = None;
            self.tree_focused = None;
            self.tree_quiet_until = None;
            self.deferred_tree = None;
            self.selected = None;
            self.top_selected = None;
            self.gesture = false;
            self.release = false;
            w.set_details(ModelRc::default());
            w.set_rows(ModelRc::default());
            w.set_top_rows(ModelRc::default());
            w.set_tiles(ModelRc::default());
            w.set_summary("?".into());
            w.set_entry_path("".into());
            w.set_current_path("".into());
            w.set_current_folder_summary("".into());
            w.set_busy(true);
            w.set_status(tr!("Preparing saved measures…").into());
            w.set_inventory_warning("".into());
        }
        if kind != 0 && index >= 0 {
            w.set_cache_choice(index);
        }
        w.set_errors("".into());
        self.cache_wait = Some(self.epoch);
        self.cache_comparing = matches!(action, Action::Compare(_) | Action::CompareSaved(..));
        clear_report(w);
        w.set_cache_confirm(false);
        w.set_cache_token(0);
        w.set_cache_diagnostic("".into());
        w.set_cache_diagnostic_visible(false);
        w.set_cache_status(tr!("Preparing saved measures…").into());
        let mut lane = self.service.lane.lock().unwrap();
        self.cache_autosave_revision = lane
            .autosave_revision
            .load(std::sync::atomic::Ordering::Acquire);
        lane.cache = Some(Request {
            id: self.cache_id,
            epoch: self.epoch,
            expected,
            action,
            cancel: self.cache_cancel.clone(),
        });
    }
    pub(super) fn poll_cache(&mut self, w: &SortSizeWindow) {
        let preview = self.service.preview_output.lock().unwrap().take();
        match preview {
            Some(PreviewOutput::Prefix(mut p))
                if p.id == self.cache_id && p.epoch == self.epoch =>
            {
                let _scope = self.presentation.enter();
                if p.presentation != self.presentation {
                    p.descriptor = preview_descriptor(&p._prefix.saved);
                    p.presentation = self.presentation;
                }
                w.set_preview_descriptor(p.descriptor.clone().into());
                w.set_preview_rows(Rc::new(VecModel::from(p.labels.clone())).into());
                w.set_preview_state(2);
                #[cfg(feature = "benchmarks")]
                i_slint_backend_winit::presentation_probe::phase(2);
                self.preview_current = Some(*p);
            }
            Some(PreviewOutput::End { id, epoch, state })
                if id == self.cache_id && epoch == self.epoch =>
            {
                self.clear_preview(w, state)
            }
            _ => {}
        }
        if let Some(p) = self.preview_current.as_mut()
            && p.presentation != self.presentation
        {
            let _scope = self.presentation.enter();
            p.descriptor = preview_descriptor(&p._prefix.saved);
            p.presentation = self.presentation;
            w.set_preview_descriptor(p.descriptor.clone().into());
        }
        let out = self.service.cache_output.lock().unwrap().take();
        if let Some(p) = out.filter(|p| p.id == self.cache_id && p.epoch == self.epoch) {
            if p.presentation != self.presentation && p.token != 0 {
                if !self.cache_comparing {
                    self.cache_wait = None;
                    self.refresh_cache(w);
                }
                return;
            }
            self.cache_wait = None;
            if p.token == 0 {
                self.cache_comparing = false;
            }
            let selected = self
                .cache_current
                .as_ref()
                .and_then(|old| {
                    usize::try_from(w.get_cache_choice())
                        .ok()
                        .and_then(|i| old.catalogue.get(i))
                })
                .map(|s| s.context);
            let previous_other = w
                .get_cache_other()
                .checked_sub(1)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| {
                    self.cache_current
                        .as_ref()?
                        .catalogue
                        .iter()
                        .filter(|s| history::retained(s))
                        .nth(i)
                        .map(|s| s.context)
                });
            let selected_index =
                selected.and_then(|key| p.catalogue.iter().position(|s| s.context == key));
            let archives = p
                .catalogue
                .iter()
                .enumerate()
                .filter(|(_, s)| history::retained(s));
            let other =
                previous_other.and_then(|key| archives.clone().position(|(_, s)| s.context == key));
            w.set_cache_archive_indices(
                Rc::new(VecModel::from(
                    std::iter::once(-1)
                        .chain(archives.clone().map(|(i, _)| i as i32))
                        .collect::<Vec<_>>(),
                ))
                .into(),
            );
            w.set_cache_archive_labels(
                Rc::new(VecModel::from(
                    std::iter::once(tr!("Choose another retained date…").into())
                        .chain(archives.map(|(i, _)| p.labels[i].clone()))
                        .collect::<Vec<_>>(),
                ))
                .into(),
            );
            w.set_cache_other(other.map_or(0, |i| i as i32 + 1));
            w.set_cache_retained(
                Rc::new(VecModel::from(
                    p.catalogue
                        .iter()
                        .map(history::retained)
                        .collect::<Vec<_>>(),
                ))
                .into(),
            );
            w.set_cache_measured(
                Rc::new(VecModel::from(
                    p.catalogue
                        .iter()
                        .map(|s| s.measured.is_some())
                        .collect::<Vec<_>>(),
                ))
                .into(),
            );
            w.set_cache_token(p.token);
            w.set_cache_labels(Rc::new(VecModel::from(p.labels.clone())).into());
            if let Some(index) = selected_index {
                w.set_cache_choice(index as i32);
            }
            w.set_diff_rows(Rc::new(VecModel::from(p.rows.clone())).into());
            w.set_cache_status(p.status.clone().into());
            w.set_cache_diagnostic(p.diagnostic.clone().into());
            w.set_diff_summary(p.summary.clone().into());
            w.set_cache_previous(p.previous);
            w.set_cache_next(p.next);
            w.set_history_heading(p.history.heading.clone().into());
            w.set_history_scope(p.history.scope.clone().into());
            w.set_history_rows(Rc::new(VecModel::from(p.history.rows.clone())).into());
            if let Some(chart) = &p.history.timeline {
                w.set_history_points(Rc::new(VecModel::from(chart.points.clone())).into());
                w.set_history_zero(chart.zero.clone().into());
                w.set_history_middle(chart.middle.clone().into());
                w.set_history_maximum(chart.maximum.clone().into());
                w.set_history_first(chart.first.clone().into());
                w.set_history_last(chart.last.clone().into());
            } else {
                w.set_history_points(ModelRc::default());
            }
            self.cache_current = Some(p);
        }

        // Direct acquisition makes its page consultable before the automatic
        // snapshot commits. A catalogue already loaded in this epoch can be old.
        if w.get_cache_visible()
            && !w.get_busy()
            && !w.get_cache_confirm()
            && !self.cache_comparing
            && w.get_history_heading().is_empty()
            && w.get_diff_summary().is_empty()
            && self.cache_wait.is_none()
            && self
                .cache_current
                .as_ref()
                .is_some_and(|p| p.epoch == self.epoch)
            && self
                .service
                .lane
                .lock()
                .unwrap()
                .autosave_revision
                .load(std::sync::atomic::Ordering::Acquire)
                != self.cache_autosave_revision
        {
            self.refresh_cache(w);
        }
    }
}

fn clear_report(w: &SortSizeWindow) {
    w.set_history_heading("".into());
    w.set_history_points(ModelRc::default());
    w.set_history_rows(ModelRc::default());
    w.set_diff_summary("".into());
    w.set_diff_rows(ModelRc::default());
}

/// Benchmark fixture bootstrap deliberately excludes production retention purge.
#[cfg(feature = "benchmarks")]
pub(super) fn readonly_fixture_catalogue(
    request: &Request,
    root: &Path,
) -> sortsize_store::inventory::Result<Prepared> {
    prepared(
        request,
        root,
        None,
        None,
        None,
        crate::units::Units::default(),
    )
}
