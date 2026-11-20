//! Explicit duplicate search scopes and dispatch for the bounded worker.
use super::*;
use duplicates::{
    Choice, CurrentFolderRequest, FolderPhase, FreshInventory, Handle, Inventory, Presentation,
};
use sortsize_core::duplicates::DuplicateOptions;
use sortsize_store::inventory::{
    catalogue::Saved,
    queries::duplicates::{CurrentFolderResources, DuplicateFileProfile},
};

pub(super) fn effective_profile(w: &SortSizeWindow) -> DuplicateFileProfile {
    crate::display_preferences::effective_profile(
        w.get_duplicate_profile_choice(),
        w.get_duplicate_exhaustive(),
    )
}
use std::sync::{
    atomic::Ordering,
    mpsc::{self, Receiver},
};

const MAX_PINNED: usize = 16;
const MAX_CATALOGUE_ROWS: usize = 128;
const MAX_CATALOGUE_BYTES: usize = 512 * 1024;
pub(super) const SINGLE_MODE: i32 = 3;
const SINGLE_PICKER: usize = 3;
type Owner = Arc<service::RepositoryWorkspace>;

fn set_duplicate_mode(w: &SortSizeWindow, mode: i32) {
    w.set_duplicate_mode(mode.clamp(0, SINGLE_MODE));
    if mode == 1 && w.get_duplicate_protected_reference() == 2 {
        w.set_duplicate_protected_reference(-1);
    }
    w.set_duplicate_settings_expanded(true);
    w.set_duplicate_reveal_allowed(mode == 0);
}

#[derive(Default)]
pub(super) struct State {
    owners: Vec<Owner>,
    choices: Vec<Choice>,
    catalogue: Option<Receiver<(Vec<Choice>, bool)>>,
    catalogue_limited: bool,
    catalogue_scopes: Vec<PathBuf>,
    task: Option<Handle>,
    current: Option<Presentation>,
    comparison_scopes: Option<Vec<ScopeSignature>>,
    selected: [Option<PathBuf>; 3],
    capped: bool,
    scope_inputs: [crate::path_input::PathInput; 3],
    picker: Option<(usize, crate::folder_picker::FolderPicker)>,
    acquisition_was_busy: bool,
    // Consultation may finish before the automatic snapshot is committed.
    // Include commits from earlier sources whose view has already been closed.
    // This revision is captured before listing, so a concurrent commit causes
    // another refresh instead of leaving A/B readiness stale.
    catalogue_autosave_revision: u64,
    single_input: crate::path_input::PathInput,
    folder_request: Option<FolderRequest>,
    tree_runs: u64,
    current_folder: Option<FolderRequest>,
    single_preparation: Option<SinglePreparation>,
    #[cfg(test)]
    pub(super) force_duplicate_inventory: bool,
    #[cfg(test)]
    pub(super) folder_observer: Option<Arc<dyn Fn(FolderPhase) + Send + Sync>>,
}

struct FolderRequest {
    root: PathBuf,
    source_epoch: u64,
    search_id: u64,
    options: Option<SingleFolderOptions>,
    profile: DuplicateFileProfile,
}

struct SingleFolderOptions {
    content: DuplicateOptions,
    resources: CurrentFolderResources,
}

fn optional_resource_limit(field: &str) -> Option<Option<u64>> {
    let field = field.trim();
    if field.is_empty() {
        return Some(None);
    }
    if !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mib = field.parse::<u64>().ok()?;
    if mib == 0 {
        return None;
    }
    Some(Some(mib.checked_mul(crate::display_preferences::MIB)?))
}

impl Controller {
    pub(crate) fn show_duplicate_resource_preferences(&self, w: &SortSizeWindow) {
        let settings = &self.preferences.duplicate_resources;
        let mib = |bytes: Option<u64>| {
            bytes.map_or_else(String::new, |n| {
                (n / crate::display_preferences::MIB).to_string()
            })
        };
        w.set_duplicate_resource_memory_mib(mib(settings.memory_limit).into());
        w.set_duplicate_resource_temporary_mib(mib(settings.temporary_limit).into());
        self.show_duplicate_temporary_parent(w);
    }

    fn show_duplicate_temporary_parent(&self, w: &SortSizeWindow) {
        let parent = self
            .preferences
            .duplicate_resources
            .temporary_parent
            .as_deref();
        w.set_duplicate_resource_temporary_parent(parent.map_or_else(String::new, label).into());
        w.set_duplicate_resource_temporary_custom(parent.is_some());
    }

    fn set_duplicate_temporary_parent(&mut self, w: &SortSizeWindow, parent: Option<PathBuf>) {
        if self.preferences.duplicate_resources.temporary_parent != parent {
            self.preferences.duplicate_resources.temporary_parent = parent;
            self.show_duplicate_temporary_parent(w);
            self.save_preferences(w);
        }
    }

    pub(crate) fn poll_duplicate_temporary_picker(&mut self, w: &SortSizeWindow) {
        let Some(result) = self
            .duplicate_temporary_picker
            .as_ref()
            .and_then(|picker| picker.poll())
        else {
            return;
        };
        self.duplicate_temporary_picker = None;
        w.set_choosing_folder(false);
        match result {
            Ok(Some(Ok(path))) => self.set_duplicate_temporary_parent(w, Some(path)),
            Ok(None) => {}
            _ => w.set_duplicate_resource_picker_failed(true),
        }
    }
}

fn configured_resources(c: &Controller) -> CurrentFolderResources {
    let settings = &c.preferences.duplicate_resources;
    CurrentFolderResources {
        memory_limit: settings.memory_limit,
        temporary_limit: settings.temporary_limit,
        temporary_parent: settings.temporary_parent.clone(),
    }
}

fn content_options(total: &str, per_file: &str) -> Option<DuplicateOptions> {
    fn limit(field: &str) -> Option<Option<u64>> {
        let field = field.trim();
        if field.is_empty() {
            Some(None)
        } else if field.bytes().all(|byte| byte.is_ascii_digit()) {
            field.parse().ok().map(Some)
        } else {
            None
        }
    }

    Some(DuplicateOptions {
        max_bytes_read: limit(total)?,
        max_file_bytes: limit(per_file)?,
        ..DuplicateOptions::without_content_limits()
    })
}

fn content_options_error(w: &SortSizeWindow) {
    w.set_duplicate_content_limits_expanded(true);
    w.set_duplicate_content_limits_error(
        tr_format!(
            "Enter whole numbers from 0 to {} in bytes, or leave a field empty.",
            u64::MAX
        )
        .into(),
    );
}

struct SinglePreparation {
    epoch: u64,
    logical_root: PathBuf,
    native_root: PathBuf,
    cancelled: bool,
    profile: DuplicateFileProfile,
}

// Inventory destruction can flush or remove local files. Never perform the
// final owner drop on the Slint thread, including window destruction.
fn retire(owners: Vec<Owner>, choices: Vec<Choice>) {
    retire_state(owners, choices, None);
}
fn retire_state(
    owners: Vec<Owner>,
    choices: Vec<Choice>,
    catalogue: Option<Receiver<(Vec<Choice>, bool)>>,
) {
    let _ = std::thread::Builder::new()
        .name("duplicate-retire".into())
        .spawn(move || drop((owners, choices, catalogue)));
}
impl Drop for State {
    fn drop(&mut self) {
        self.task.take();
        retire_state(
            std::mem::take(&mut self.owners),
            std::mem::take(&mut self.choices),
            self.catalogue.take(),
        );
    }
}

#[derive(Clone)]
struct Descriptor {
    label: String,
    logical: PathBuf,
    native: PathBuf,
}

/// Provenance of the exact immutable scopes accepted by the current worker.
/// A refreshed catalogue can publish G1 while this worker still leases G0.
/// Labels, row positions and unrelated source generations are not identities.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeSignature {
    logical_root: PathBuf,
    native_root: PathBuf,
    inventories: Vec<(PathBuf, AcquisitionContext)>,
}
impl ScopeSignature {
    fn from_choice(choice: &Choice) -> Self {
        Self {
            logical_root: choice.logical_root.clone(),
            native_root: choice.native_root.clone(),
            inventories: choice
                .inventories
                .iter()
                .map(|inventory| (inventory.saved.root.clone(), inventory.saved.context))
                .collect(),
        }
    }
}

// A mounted volume can be nested below another source in the OS namespace.
// Only the inventoried, authorized roots determine whether the scopes overlap.
fn distinct_inventories(a: &Choice, b: &Choice) -> bool {
    !a.inventories.is_empty()
        && !b.inventories.is_empty()
        && !crate::source_registry::covers(&a.native_root, &b.native_root)
        && !crate::source_registry::covers(&b.native_root, &a.native_root)
}

#[cfg(test)]
fn collect_choices(
    descriptors: Vec<Descriptor>,
    owners: Vec<Owner>,
    saved: Vec<Saved>,
    cache_root: Option<PathBuf>,
) -> Vec<Choice> {
    collect_choices_for_roots(descriptors, owners, saved, cache_root, Vec::new()).0
}

fn collect_choices_for_roots(
    descriptors: Vec<Descriptor>,
    owners: Vec<Owner>,
    mut saved: Vec<Saved>,
    cache_root: Option<PathBuf>,
    preferred: Vec<PathBuf>,
) -> (Vec<Choice>, bool) {
    let legacy = cache_root
        .is_none()
        .then(super::cache::legacy_catalogue)
        .flatten();
    let mut limited = false;
    if let Some(root) = cache_root.or_else(|| {
        super::service::work_location()
            .ok()
            .map(|location| location.catalogue())
    }) {
        match super::cache::list_catalogues(&root, legacy.as_deref(), &[]) {
            Ok(catalogue) => {
                // Successful listing is authoritative for replaced/forgotten
                // snapshots. Cached fallback applies only when listing fails.
                saved.retain(|record| !super::cache::is_snapshot(record));
                saved.extend(catalogue.rows);
                limited |= catalogue.unavailable > 0;
            }
            Err(_) => limited = true,
        }
        // The global catalogue can exceed its admission budget. Its failure
        // must not hide the known sources selected for this comparison. Probe
        // only deterministic local snapshot names, never the acquired source.
        let history = super::cache::snapshot_root(&root);
        let mut probed = std::collections::HashSet::new();
        for native in preferred
            .iter()
            .chain(descriptors.iter().map(|d| &d.native))
        {
            let destination = sortsize_store::direct_snapshot::destination(&history, native);
            if probed.contains(&destination) {
                continue;
            }
            if probed.len() == MAX_CATALOGUE_ROWS {
                limited = true;
                break;
            }
            probed.insert(destination.clone());
            // A missing/invalid selected snapshot must not be resurrected by
            // an older cached descriptor when the global catalogue is full.
            saved.retain(|old| {
                !super::cache::is_snapshot(old)
                    || old.directory.file_name() != destination.file_name()
            });
            if let Ok(meta) = sortsize_store::direct_snapshot::inspect(&destination)
                && sortsize_store::direct_snapshot::destination(&history, &meta.root) == destination
            {
                // An explicit archive of this same context remains preferred.
                if !saved.iter().any(|old| old.context == meta.context) {
                    saved.push(Saved {
                        directory: destination,
                        context: meta.context,
                        root: meta.root,
                        started: meta.started,
                        measured: Some(meta.measured),
                        coverage: meta.summary.coverage,
                        logical: meta.summary.measure.logical,
                        unknown: meta.summary.measure.unknown,
                        continuity: None,
                    });
                }
            }
        }
    }
    let mut inventories = Vec::new();
    for owner in &owners {
        if let Ok(records) = owner.saved_records(None) {
            for saved in records {
                inventories.push(Inventory {
                    saved,
                    workspace: Some(owner.clone()),
                });
            }
        }
    }
    for record in saved {
        if !inventories
            .iter()
            .any(|i: &Inventory| i.saved.context == record.context)
        {
            inventories.push(Inventory {
                saved: record,
                workspace: None,
            });
        }
    }
    // Prefer the newest measured generation for each native allowed root.
    inventories.sort_by_key(|i| {
        (
            !preferred.iter().any(|root| root.starts_with(&i.saved.root)),
            std::cmp::Reverse(i.saved.measured),
            std::cmp::Reverse(i.saved.context.generation.0),
        )
    });
    let mut seen = std::collections::HashSet::new();
    inventories.retain(|i| i.saved.measured.is_some() && seen.insert(i.saved.root.clone()));
    let mut rows = 0usize;
    let mut bytes = 0usize;
    inventories.retain(|i| {
        let size = i
            .saved
            .root
            .capacity()
            .saturating_add(i.saved.directory.capacity())
            .saturating_add(1024);
        if rows == MAX_CATALOGUE_ROWS || bytes.saturating_add(size) > MAX_CATALOGUE_BYTES {
            limited = true;
            return false;
        }
        rows += 1;
        bytes += size;
        true
    });
    let mut choices: Vec<Choice> = descriptors
        .iter()
        .map(|d| Choice {
            label: d.label.clone(),
            logical_root: d.logical.clone(),
            native_root: d.native.clone(),
            inventories: Vec::new(),
        })
        .collect();
    for inventory in inventories {
        // Do not infer that an inventory above a source covers its mount. The
        // scan's same-volume policy may have deliberately skipped that source.
        // Reuse a broader inventory only inside the source that owns it.
        let target = descriptors
            .iter()
            .enumerate()
            .filter(|(_, d)| inventory.saved.root.starts_with(&d.native))
            .max_by_key(|(_, d)| d.native.components().count())
            .map(|(i, _)| i);
        if let Some(index) = target {
            choices[index].inventories.push(inventory);
        } else {
            choices.push(Choice {
                label: short_label(&inventory.saved.root),
                logical_root: inventory.saved.root.clone(),
                native_root: inventory.saved.root.clone(),
                inventories: vec![inventory],
            });
        }
    }
    choices.sort_by(|a, b| a.label.cmp(&b.label));
    limited |= choices.len() > crate::source_registry::MAX_SOURCES;
    choices.truncate(crate::source_registry::MAX_SOURCES);
    (choices, limited)
}

fn scope_choice(mut choice: Choice, root: PathBuf) -> Choice {
    // A single sealed ancestor inventory supplies metadata for the exact folder.
    // Other roots associated with this source are never unioned.
    choice
        .inventories
        .retain(|i| root.starts_with(&i.saved.root));
    choice.inventories.sort_by_key(|i| {
        (
            std::cmp::Reverse(i.saved.root.components().count()),
            std::cmp::Reverse(i.saved.measured),
            std::cmp::Reverse(i.saved.context.generation.0),
        )
    });
    choice.inventories.truncate(1);
    choice.native_root = root;
    choice
}

fn show_duplicate_folder_link(w: &SortSizeWindow, folder_ids: &[u64], index: i32) {
    let group = usize::try_from(index)
        .ok()
        .and_then(|index| folder_ids.get(index))
        .copied()
        .filter(|id| *id != 0);
    w.set_duplicate_reference_available(group.is_some());
    w.set_duplicate_reference_label(
        group
            .map_or_else(String::new, |id| tr_format!("Open folder group {}", id))
            .into(),
    );
}

impl Host {
    /// Clear only review presentation; the explicit reference remains visible.
    /// Context owners close the worker before calling this helper.
    pub(super) fn reset_duplicate_review(&self, w: &SortSizeWindow, notice: bool) {
        w.set_duplicate_selection_ready(false);
        let rows = w
            .get_duplicate_rows()
            .iter()
            .map(|mut row| {
                row.selectable = false;
                row.selected = false;
                row.partial = false;
                row
            })
            .collect::<Vec<_>>();
        w.set_duplicate_rows(Rc::new(VecModel::from(rows)).into());
        w.set_duplicate_selection_preview_open(false);
        w.set_duplicate_selection_preview_text("".into());
        w.set_duplicate_selection_summary("".into());
        w.set_duplicate_selection_notice(if notice {
            tr!("Selection cleared because the comparison context changed.").into()
        } else {
            "".into()
        });
    }

    pub(super) fn remember_duplicate_inventory(&mut self) {
        let owner = self.service.lane.lock().unwrap().reader_workspace.clone();
        let Some(owner) = owner else { return };
        if self
            .duplicate
            .owners
            .iter()
            .any(|old| Arc::ptr_eq(old, &owner))
        {
            return;
        }
        if let Some(index) = self
            .duplicate
            .owners
            .iter()
            .position(|old| old.root() == owner.root())
        {
            let previous = std::mem::replace(&mut self.duplicate.owners[index], owner);
            retire(vec![previous], vec![]);
        } else if self.duplicate.owners.len() < MAX_PINNED {
            self.duplicate.owners.push(owner);
        } else {
            self.duplicate.capped = true;
        }
    }

    pub(super) fn refresh_duplicate_choices(&mut self, w: &SortSizeWindow, c: &Controller) {
        if self.duplicate.catalogue.is_some() {
            return;
        }
        self.remember_duplicate_inventory();
        let lane = self.service.lane.lock().unwrap();
        let bindings = lane.source_bindings.clone();
        let cache_root = lane.cache_root.clone();
        let autosave_revision = lane.autosave_revision.load(Ordering::Acquire);
        drop(lane);
        let mut descriptors = Vec::new();
        // Logical servers are intentionally not selectable as OS directories.
        // Their explicitly scanned shares appear individually in this lot.
        for source in &c.registry.entries {
            if source.is_server() {
                continue;
            }
            let native = bindings
                .iter()
                .find(|(logical, _)| *logical == source.path)
                .map_or_else(|| source.path.clone(), |(_, native)| native.clone());
            descriptors.push(Descriptor {
                label: format!(
                    "{} · {}",
                    match source.kind {
                        crate::source_registry::Kind::Local => tr!("Local"),
                        crate::source_registry::Kind::Smb => "SMB",
                        crate::source_registry::Kind::Nfs => "NFS",
                        crate::source_registry::Kind::SmbServer => unreachable!(),
                    },
                    if source.label.is_empty() {
                        short_label(&source.path)
                    } else {
                        source.label.clone()
                    }
                ),
                logical: source.path.clone(),
                native,
            });
        }
        if let Some(current) = &c.sources.current {
            for volume in &current.discovery.volumes {
                for mount in &volume.mounts {
                    if descriptors
                        .iter()
                        .any(|d| d.logical == mount.path || d.native == mount.path)
                    {
                        continue;
                    }
                    descriptors.push(Descriptor {
                        label: short_label(&mount.path),
                        logical: mount.path.clone(),
                        native: mount.path.clone(),
                    });
                }
            }
        }
        let owners = self.duplicate.owners.clone();
        let saved = self
            .cache_current
            .as_ref()
            .map_or_else(Vec::new, |p| p.catalogue.clone());
        let preferred = (0..3)
            .filter_map(|side| self.duplicate_scope(w, side))
            .chain(self.requested.iter().cloned())
            .collect::<Vec<_>>();
        let captured_scopes = preferred.clone();
        let (send, receive) = mpsc::channel();
        let result = std::thread::Builder::new()
            .name("duplicate-catalogue".into())
            .spawn(move || {
                let choices =
                    collect_choices_for_roots(descriptors, owners, saved, cache_root, preferred);
                let _ = send.send(choices);
            });
        if result.is_ok() {
            self.duplicate.catalogue = Some(receive);
            self.duplicate.catalogue_autosave_revision = autosave_revision;
            self.duplicate.catalogue_scopes = captured_scopes;
            w.set_duplicate_ready(false);
            w.set_search_status(tr!("Preparing saved measures…").into());
        } else {
            w.set_search_status(tr!("Could not start search.").into());
        }
    }

    fn duplicate_source_for_path(&self, root: &std::path::Path) -> Option<&Choice> {
        self.duplicate
            .choices
            .iter()
            .filter(|choice| {
                choice.native_root.is_absolute()
                    && crate::source_registry::covers(&choice.native_root, root)
            })
            .max_by_key(|choice| choice.native_root.components().count())
    }

    fn duplicate_scope(&self, w: &SortSizeWindow, side: usize) -> Option<PathBuf> {
        let text = match side {
            0 => w.get_duplicate_scope_a(),
            1 => w.get_duplicate_scope_b(),
            _ => w.get_duplicate_scope_c(),
        };
        let root = self.duplicate.scope_inputs[side].resolve(text.as_str());
        (root.is_absolute()
            && !root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            && self.duplicate_source_for_path(&root).is_some())
        .then_some(root)
    }

    fn scoped_duplicate_choice(&self, w: &SortSizeWindow, side: usize) -> Option<Choice> {
        let root = self.duplicate_scope(w, side)?;
        let choice = self.duplicate_source_for_path(&root)?.clone();
        Some(scope_choice(choice, root))
    }

    fn duplicate_scope_signatures(&self, w: &SortSizeWindow) -> Option<Vec<ScopeSignature>> {
        (0..if w.get_duplicate_mode() == 2 { 3 } else { 2 })
            .map(|side| {
                self.scoped_duplicate_choice(w, side)
                    .map(|choice| ScopeSignature::from_choice(&choice))
            })
            .collect()
    }

    fn set_duplicate_scope(&mut self, w: &SortSizeWindow, side: usize, root: PathBuf) {
        let display = self.duplicate.scope_inputs[side].select(root).into();
        match side {
            0 => w.set_duplicate_scope_a(display),
            1 => w.set_duplicate_scope_b(display),
            _ => w.set_duplicate_scope_c(display),
        }
    }

    fn single_root(&self, w: &SortSizeWindow) -> Option<PathBuf> {
        let root = self
            .duplicate
            .single_input
            .resolve(w.get_duplicate_single_root().as_str());
        (root.is_absolute()
            && !root
                .components()
                .any(|part| part == std::path::Component::ParentDir))
        .then_some(root)
    }

    fn update_single_selection(&self, w: &SortSizeWindow, c: &Controller) {
        w.set_duplicate_content_limits_supported(true);
        let lane = self.service.lane.lock().unwrap();
        let batch = lane.smb_snapshot.as_ref().filter(|batch| batch.running);
        let reason = if w.get_search_busy() {
            String::new()
        } else if lane.writer_running
            || lane.start.is_some()
            || lane.preparing_source.is_some()
            || batch.is_some()
            || w.get_busy()
            || w.get_cancelling()
        {
            let path = lane
                .acquisition_root
                .as_deref()
                .or_else(|| lane.start.as_ref().map(|(_, start)| start.root.as_path()))
                .or_else(|| batch.map(|batch| batch.server.as_path()))
                .or(self.requested.as_deref())
                .map_or_else(|| w.get_scan_source().to_string(), label);
            format!(
                "{}\n{}",
                tr!("An analysis is active. Wait for completion or cancel it."),
                path
            )
        } else if lane.duplicate_operations.iter().any(|(id, _, _)| {
            self.duplicate
                .task
                .as_ref()
                .is_none_or(|task| task.id != *id)
        }) {
            tr!("Another duplicate task is still closing. Retry shortly.").to_owned()
        } else if let Some(root) = self.single_root(w) {
            if c.registry.is_disconnected(&root) || service::source_disconnected(&lane, &root) {
                tr!("Disconnected by user").to_owned()
            } else {
                String::new()
            }
        } else {
            tr!("Choose an absolute folder.").to_owned()
        };
        w.set_duplicate_ready(
            reason.is_empty()
                && self.duplicate.single_preparation.is_none()
                && !w.get_search_busy()
                && !w.get_choosing_folder()
                && w.get_scan_enabled(),
        );
        w.set_duplicate_disabled_reason(reason.into());
    }

    fn start_single_search(&mut self, w: &SortSizeWindow, c: &mut Controller) {
        self.update_single_selection(w, c);
        let Some(logical_root) = self.single_root(w).filter(|_| w.get_duplicate_ready()) else {
            return;
        };
        let Ok(options) = self.selected_folder_options(w, c, &logical_root) else {
            return;
        };
        let profile = effective_profile(w);
        self.run_single_search(w, c, logical_root, false, options, profile);
    }

    fn selected_folder_options(
        &self,
        w: &SortSizeWindow,
        c: &Controller,
        root: &Path,
    ) -> Result<Option<SingleFolderOptions>, ()> {
        let total = w.get_duplicate_content_total_bytes();
        let per_file = w.get_duplicate_content_file_bytes();
        let Some(options) = content_options(total.as_str(), per_file.as_str()) else {
            content_options_error(w);
            return Err(());
        };
        if self.use_current_folder(root) {
            return Ok(Some(SingleFolderOptions {
                content: options,
                resources: configured_resources(c),
            }));
        }
        if options.max_bytes_read.is_some() || options.max_file_bytes.is_some() {
            w.set_duplicate_content_limits_expanded(true);
            w.set_duplicate_content_limits_error(
                tr!("Content limits are available for local folders.").into(),
            );
            return Err(());
        }
        Ok(None)
    }

    fn run_single_search(
        &mut self,
        w: &SortSizeWindow,
        c: &mut Controller,
        root: PathBuf,
        preserve_parent: bool,
        options: Option<SingleFolderOptions>,
        profile: DuplicateFileProfile,
    ) {
        if let Some(options) = options {
            self.start_current_folder(w, c, root, options, profile);
            return;
        }
        self.prepare_single_inventory(w, c, root, preserve_parent, profile);
    }

    fn use_current_folder(&self, root: &Path) -> bool {
        #[cfg(test)]
        if self.duplicate.force_duplicate_inventory {
            return false;
        }
        matches!(
            sortsize_core::acquisition::source_class(root),
            Ok(sortsize_core::acquisition::SourceClass::Local)
        )
    }

    fn start_current_folder(
        &mut self,
        w: &SortSizeWindow,
        c: &mut Controller,
        root: PathBuf,
        options: SingleFolderOptions,
        profile: DuplicateFileProfile,
    ) {
        self.close_search(w);
        c.pending_auto = None;
        c.restored_selection = None;
        c.auto_analysis.cancel();
        let source_epoch = self.service.lane.lock().unwrap().source_epoch;
        let task = Handle::start_current_folder(
            self.search_id,
            CurrentFolderRequest {
                preflight: c.registry.scan_preflight(&root),
                root: root.clone(),
                source_epoch,
                options: options.content,
                resources: options.resources,
                profile,
                #[cfg(test)]
                observe: self.duplicate.folder_observer.clone(),
            },
            self.service.lane.clone(),
            self.units,
            self.presentation,
        );
        w.set_search_visible(true);
        w.set_duplicate_scope_summary(Rc::new(VecModel::from(vec![label(&root).into()])).into());
        match task {
            Ok(task) => {
                self.duplicate.task = Some(task);
                self.duplicate.current_folder = Some(FolderRequest {
                    root,
                    source_epoch,
                    search_id: self.search_id,
                    options: None,
                    profile,
                });
                w.set_search_busy(true);
                w.set_duplicate_ready(false);
                w.set_duplicate_summary(FolderPhase::Discovering.label().into());
            }
            Err(error) => {
                w.set_duplicate_summary(tr!("Incomplete coverage").into());
                w.set_duplicate_warning(error.to_string().into());
                self.update_single_selection(w, c);
            }
        }
    }

    fn prepare_single_inventory(
        &mut self,
        w: &SortSizeWindow,
        c: &mut Controller,
        logical_root: PathBuf,
        interrupted_scan: bool,
        profile: DuplicateFileProfile,
    ) {
        let preflight = c.registry.scan_preflight(&logical_root);
        c.pending_auto = None;
        c.restored_selection = None;
        if !interrupted_scan {
            c.auto_analysis.manual(&logical_root);
        }
        let previous_epoch = self.epoch;
        self.start_with_mode(
            w,
            c,
            Start {
                root: logical_root.clone(),
                explicit_root: true,
                observation: sortsize_core::acquisition::ScanObservation::Strict,
                preflight,
                // Duplicate detection needs file metadata and contents only.
                options: sortsize_store::scheduler::CollectionOptions::default(),
            },
            false,
            if interrupted_scan {
                service::StartPurpose::PriorityDuplicates
            } else {
                service::StartPurpose::DuplicateInventory
            },
        );
        if self.epoch == previous_epoch {
            w.set_duplicate_summary(tr!("Incomplete coverage").into());
            w.set_duplicate_warning(tr!("Could not start scan.").into());
            return;
        }
        let native_root = self.requested.clone().expect("accepted acquisition root");
        w.set_duplicate_scope_summary(
            Rc::new(VecModel::from(vec![label(&native_root).into()])).into(),
        );
        self.duplicate.single_preparation = Some(SinglePreparation {
            epoch: self.epoch,
            logical_root,
            native_root,
            cancelled: false,
            profile,
        });
        w.set_search_visible(true);
        w.set_search_busy(true);
        w.set_duplicate_summary(tr!("Preparing inventory…").into());
    }

    fn mark_duplicate_tree_run(&mut self, w: &SortSizeWindow) {
        self.duplicate.tree_runs += 1;
        w.set_duplicate_tree_run(
            crate::number_format::integer(self.duplicate.tree_runs.into()).into(),
        );
    }

    pub(super) fn start_folder_duplicates(
        &mut self,
        w: &SortSizeWindow,
        c: &mut Controller,
        root: PathBuf,
    ) {
        if w.get_search_busy() && self.duplicate.folder_request.is_none() {
            let notice: slint::SharedString =
                tr!("An analysis is active. Wait for completion or cancel it.").into();
            w.set_status(notice.clone());
            if w.get_search_visible() && w.get_duplicate_mode() > 0 {
                w.set_duplicate_warning(notice);
            }
            return;
        }
        set_duplicate_mode(w, SINGLE_MODE);
        w.set_duplicate_single_root(self.duplicate.single_input.select(root.clone()).into());
        if w.get_ribbon_tab() != 1 {
            w.invoke_activate_ribbon(1);
        }
        self.show_analysis(w, c);
        let profile = effective_profile(w);
        let (source_epoch, active, batch, retiring_read) = {
            let lane = self.service.lane.lock().unwrap();
            let batch = lane.smb_begin.is_some()
                || lane
                    .smb_snapshot
                    .as_ref()
                    .is_some_and(|batch| batch.running);
            let previous_task = self.duplicate.task.as_ref().map(|task| task.id);
            // A completed result keeps its worker available for pagination.
            // Retire that worker and any cancelled readers before reusing their
            // resources. An independent active read still follows normal admission.
            let retiring_read = !lane.duplicate_operations.is_empty()
                && lane
                    .duplicate_operations
                    .iter()
                    .all(|(id, _, cancel)| Some(*id) == previous_task || cancel.is_cancelled());
            (
                lane.source_epoch,
                lane.acquisition_in_progress(),
                batch,
                retiring_read,
            )
        };
        // A terminal worker may already have posted an acknowledgement while
        // the visible scan state still reflects its last running page.
        let waiting_for_scan = active || w.get_busy() || w.get_cancelling();
        let was_queued = self.duplicate.folder_request.is_some();
        // Clear the old report before publishing a refusal for the new scope.
        self.close_search(w);
        w.set_search_visible(true);
        let Ok(options) = self.selected_folder_options(w, c, &root) else {
            w.set_duplicate_summary(tr!("Could not start search.").into());
            return;
        };
        if !waiting_for_scan && !retiring_read && !was_queued {
            self.update_single_selection(w, c);
            if let Some(root) = self.single_root(w).filter(|_| w.get_duplicate_ready()) {
                // A completed parent tree still owns the return destination.
                // Independent autosave is not acquisition activity, so preserving
                // that tree must not depend on whether persistence is running.
                let preserve_parent = self
                    .requested
                    .as_ref()
                    .is_some_and(|parent| root != *parent && root.starts_with(parent));
                self.run_single_search(w, c, root, preserve_parent, options, profile);
                if w.get_search_busy() {
                    self.mark_duplicate_tree_run(w);
                }
            } else {
                w.set_duplicate_summary(tr!("Could not start search.").into());
            }
            return;
        }
        if !w.get_scan_enabled()
            || w.get_choosing_folder()
            || c.registry.is_disconnected(&root)
            || self.service.source_disconnected(&root)
        {
            w.set_duplicate_summary(tr!("Could not start search.").into());
            self.update_single_selection(w, c);
            return;
        }

        c.pending_auto = None;
        c.restored_selection = None;
        c.auto_analysis.cancel();
        self.duplicate.folder_request = Some(FolderRequest {
            root,
            source_epoch,
            search_id: self.search_id,
            options,
            profile,
        });
        w.set_search_visible(true);
        w.set_search_busy(true);
        w.set_duplicate_waiting_for_scan(true);
        w.set_duplicate_ready(false);
        w.set_duplicate_disabled_reason("".into());
        self.mark_duplicate_tree_run(w);
        w.set_duplicate_summary(
            if waiting_for_scan {
                tr!("Cancelling…")
            } else {
                tr!("Waiting")
            }
            .into(),
        );
        if waiting_for_scan {
            if batch {
                self.service.cancel_smb_batch();
                w.set_cancelling(true);
            } else if self.service.cancel_acquisition() {
                w.set_cancelling(true);
            }
        }
    }

    pub(super) fn poll_folder_duplicates(&mut self, w: &SortSizeWindow, c: &mut Controller) {
        let Some(request) = &self.duplicate.folder_request else {
            return;
        };
        let _scope = c.presentation.enter();
        let lane = self.service.lane.lock().unwrap();
        if lane.source_epoch != request.source_epoch
            || self.search_id != request.search_id
            || w.get_duplicate_mode() != SINGLE_MODE
            || !w.get_search_visible()
            || self.single_root(w).as_ref() != Some(&request.root)
        {
            drop(lane);
            self.close_search(w);
            return;
        }
        if c.registry.is_disconnected(&request.root)
            || service::source_disconnected(&lane, &request.root)
        {
            drop(lane);
            self.cancel_folder_request(w);
            w.set_duplicate_warning(tr!("Disconnected by user").into());
            return;
        }
        if lane.acquisition_in_progress()
            || !lane.duplicate_operations.is_empty()
            || w.get_busy()
            || w.get_cancelling()
        {
            return;
        }
        drop(lane);
        let request = self.duplicate.folder_request.take().unwrap();
        w.set_duplicate_waiting_for_scan(false);
        w.set_search_busy(false);
        let run = w.get_duplicate_tree_run();
        self.run_single_search(w, c, request.root, true, request.options, request.profile);
        w.set_duplicate_tree_run(run);
    }

    pub(super) fn mark_single_preparation_cancelled(&mut self, w: &SortSizeWindow) -> bool {
        let Some(pending) = &mut self.duplicate.single_preparation else {
            return false;
        };
        pending.cancelled = true;
        w.set_duplicate_summary(tr!("Cancelling…").into());
        true
    }

    pub(super) fn poll_single_preparation(&mut self, w: &SortSizeWindow, c: &Controller) {
        let Some(pending) = self.duplicate.single_preparation.as_ref() else {
            return;
        };
        let fresh = {
            let lane = self.service.lane.lock().unwrap();
            if lane.source_epoch != pending.epoch || w.get_duplicate_mode() != SINGLE_MODE {
                drop(lane);
                self.close_search(w);
                return;
            }
            if lane.writer_running || lane.start.is_some() || w.get_cancelling() {
                return;
            }
            let cancelled = pending.cancelled || lane.cancel_requested_epoch == Some(pending.epoch);
            if !cancelled
                && !self.failed
                && lane.phase.is_some_and(|(epoch, phase, _)| {
                    epoch == pending.epoch
                        && matches!(
                            phase,
                            service::Phase::Preparing
                                | service::Phase::Acquiring
                                | service::Phase::Finalizing
                                | service::Phase::Certifying
                                | service::Phase::Reopening
                        )
                })
            {
                // Acquisition may have stopped while the reader and writer
                // cleanup still prepare the certified view. Only its terminal
                // phase can launch duplicate verification or report a failure.
                return;
            }
            if !cancelled
                && let Some((epoch, service::Phase::Ready, generation)) = lane.phase
                && epoch == pending.epoch
            {
                // Ready follows the reader page and writer cleanup. The UI
                // can still be one poll behind that certified publication.
                let from_handoff = lane
                    .reader_target
                    .as_ref()
                    .and_then(Option::as_ref)
                    .filter(|active| {
                        active.source_epoch == epoch
                            && active.root == pending.native_root
                            && active.c.generation == generation
                    })
                    .and_then(|active| {
                        Some(FreshInventory {
                            workspace: active.workspace.clone()?,
                            inventory: active.c.inventory,
                            source: active.c.source,
                            generation,
                            profile: pending.profile,
                        })
                    });
                let fresh = from_handoff.or_else(|| {
                    let page = self.current.as_ref().filter(|page| {
                        page.epoch == self.epoch
                            && !page.busy
                            && !page.frame.provisional
                            && page.frame.stamp.context.generation == generation
                    })?;
                    let workspace = lane
                        .reader_workspace
                        .as_ref()
                        .filter(|workspace| workspace.root() == pending.native_root)?;
                    Some(FreshInventory {
                        workspace: workspace.clone(),
                        inventory: page.frame.stamp.context.inventory,
                        source: page.frame.stamp.context.source,
                        generation,
                        profile: pending.profile,
                    })
                });
                if fresh.is_none() && !self.failed {
                    return;
                }
                (fresh, false)
            } else {
                (
                    None,
                    cancelled || matches!(lane.phase, Some((_, service::Phase::Cancelled, _))),
                )
            }
        };
        let pending = self.duplicate.single_preparation.take().unwrap();
        let result = fresh.0.map(|inventory| {
            Handle::start_single(
                self.search_id,
                pending.logical_root,
                pending.native_root,
                inventory,
                self.service.lane.clone(),
                self.units,
                self.presentation,
            )
        });
        match result {
            Some(Ok(task)) => {
                self.duplicate.task = Some(task);
                w.set_duplicate_summary(tr!("Verifying files…").into());
            }
            failure => {
                w.set_search_busy(false);
                w.set_search_complete(false);
                w.set_duplicate_summary(tr!("Incomplete coverage").into());
                w.set_duplicate_warning(
                    match failure {
                        Some(Err(error)) => error.to_string(),
                        _ if fresh.1 => tr!("Cancelled").to_owned(),
                        _ => tr!("Could not start scan.").to_owned(),
                    }
                    .into(),
                );
                self.update_single_selection(w, c);
            }
        }
    }

    fn update_duplicate_selection(&mut self, w: &SortSizeWindow, c: &Controller) {
        if w.get_duplicate_mode() == SINGLE_MODE {
            self.update_single_selection(w, c);
            return;
        }
        let a = self.scoped_duplicate_choice(w, 0);
        let b = self.scoped_duplicate_choice(w, 1);
        let third = (w.get_duplicate_mode() == 2)
            .then(|| self.scoped_duplicate_choice(w, 2))
            .flatten();
        let info = |choice: Option<&Choice>| {
            let Some(choice) = choice else {
                return tr!("Choose an absolute folder.").to_owned();
            };
            let active = self.requested.as_ref() == Some(&choice.native_root) && w.get_busy();
            let state = if c.registry.is_disconnected(&choice.logical_root) {
                tr!("Disconnected by user")
            } else if active {
                if w.get_cancelling() {
                    tr!("Cancelling…")
                } else {
                    tr!("Scan in progress…")
                }
            } else if choice.inventories.is_empty() {
                tr!("Inventory absent")
            } else if choice
                .inventories
                .iter()
                .any(|i| i.saved.coverage != ScopeCoverage::Complete)
            {
                tr!("Incomplete coverage")
            } else {
                tr!("Inventory available")
            };
            format!("{state} · {}", choice.label)
        };
        w.set_duplicate_scope_a_valid(
            a.as_ref()
                .is_some_and(|s| !c.registry.is_disconnected(&s.logical_root)),
        );
        w.set_duplicate_scope_b_valid(
            b.as_ref()
                .is_some_and(|s| !c.registry.is_disconnected(&s.logical_root)),
        );
        w.set_duplicate_source_a_info(info(a.as_ref()).into());
        w.set_duplicate_source_b_info(info(b.as_ref()).into());
        w.set_duplicate_scope_c_valid(
            third
                .as_ref()
                .is_some_and(|s| !c.registry.is_disconnected(&s.logical_root)),
        );
        w.set_duplicate_source_c_info(if w.get_duplicate_mode() == 2 {
            info(third.as_ref()).into()
        } else {
            "".into()
        });
        w.set_duplicate_c_needs_inventory(third.as_ref().is_some_and(|s| {
            s.inventories.is_empty() && !c.registry.is_disconnected(&s.logical_root)
        }));
        w.set_duplicate_a_needs_inventory(a.as_ref().is_some_and(|s| {
            s.inventories.is_empty() && !c.registry.is_disconnected(&s.logical_root)
        }));
        w.set_duplicate_b_needs_inventory(b.as_ref().is_some_and(|s| {
            s.inventories.is_empty() && !c.registry.is_disconnected(&s.logical_root)
        }));
        let choices = [a.as_ref(), b.as_ref(), third.as_ref()];
        let count = if w.get_duplicate_mode() == 2 { 3 } else { 2 };
        let mut reason = String::new();
        if w.get_busy() || w.get_cancelling() {
            reason = tr!("An analysis is active. Wait for completion or cancel it.").to_owned();
        } else if self.duplicate.catalogue.is_some() {
            reason = tr!("Preparing saved measures…").to_owned();
        } else {
            for (side, choice) in choices[..count].iter().enumerate() {
                let side = ["A", "B", "C"][side];
                let failure = match choice {
                    None => Some(tr!(
                        "Folder missing or invalid: choose an absolute folder on an available source."
                    )),
                    Some(choice) if c.registry.is_disconnected(&choice.logical_root) => {
                        Some(tr!("Disconnected by user"))
                    }
                    Some(choice) if choice.inventories.is_empty() => Some(tr!("Inventory absent")),
                    _ => None,
                };
                if let Some(failure) = failure {
                    reason = format!("{side}: {failure}");
                    break;
                }
            }
            if reason.is_empty() {
                for left in 0..count {
                    for right in left + 1..count {
                        if !distinct_inventories(choices[left].unwrap(), choices[right].unwrap()) {
                            reason = tr!("Folders are identical or nested. Choose separate, disjoint folders.").to_owned();
                        }
                    }
                }
            }
        }
        w.set_duplicate_ready(reason.is_empty());
        w.set_duplicate_disabled_reason(reason.into());
    }

    pub(super) fn cancel_duplicate_task(&mut self) {
        self.duplicate.folder_request = None;
        self.duplicate.current_folder = None;
        if let Some(pending) = self.duplicate.single_preparation.take() {
            let owns_writer = self.service.lane.lock().unwrap().source_epoch == pending.epoch;
            if owns_writer {
                self.service.cancel_acquisition();
            }
        }
        if let Some(task) = &self.duplicate.task {
            task.cancel();
        }
        self.duplicate.task = None;
        self.duplicate.current = None;
        self.duplicate.comparison_scopes = None;
    }

    pub(super) fn cancel_duplicate_verification(&mut self, w: &SortSizeWindow) {
        if self.cancel_folder_request(w) {
            return;
        }
        if self.mark_single_preparation_cancelled(w) {
            if self.service.cancel_acquisition() {
                w.set_cancelling(true);
            }
        } else if w.get_search_busy()
            && let Some(task) = &self.duplicate.task
        {
            task.cancel();
            if self.duplicate.current_folder.is_some() {
                w.set_duplicate_summary(tr!("Cancelling…").into());
            }
        }
    }

    pub(super) fn cancel_current_folder(&mut self, w: &SortSizeWindow) -> bool {
        if self.duplicate.current_folder.is_none() || !w.get_search_busy() {
            return false;
        }
        self.cancel_duplicate_verification(w);
        true
    }

    pub(super) fn cancel_folder_request(&mut self, w: &SortSizeWindow) -> bool {
        if self.duplicate.folder_request.take().is_none() {
            return false;
        }
        w.set_duplicate_waiting_for_scan(false);
        w.set_search_busy(false);
        w.set_duplicate_summary(tr!("Incomplete coverage").into());
        w.set_duplicate_warning(tr!("Cancelled").into());
        true
    }

    pub(super) fn page_duplicates(&self, next: bool) {
        if let Some(task) = &self.duplicate.task {
            task.page(next);
        }
    }

    pub(super) fn duplicate_menu_path(
        &self,
        w: &SortSizeWindow,
        token: i32,
        index: i32,
    ) -> Option<PathBuf> {
        if !w.get_search_visible()
            || !w.get_search_duplicates_visible()
            || token != w.get_duplicate_menu_token()
        {
            return None;
        }
        let rows = if w.get_duplicate_mode() == 0 {
            &self
                .search_current
                .as_ref()
                .filter(|page| page.duplicates && page.id == self.search_id)?
                .rows
        } else {
            &self
                .duplicate
                .current
                .as_ref()
                .filter(|page| {
                    page.id == self.search_id && (!page.folder_view || page.folder_detail)
                })?
                .rows
        };
        let row = rows.get(usize::try_from(index).ok()?)?;
        (row.kind == sortsize_store::inventory::Kind::File).then(|| row.path.clone())
    }

    pub(super) fn select_duplicate(&self, w: &SortSizeWindow, index: i32) {
        if let Some(page) = self
            .duplicate
            .current
            .as_ref()
            .filter(|p| p.id == self.search_id)
            && let Ok(position) = usize::try_from(index)
            && let Some(row) = page.rows.get(position)
        {
            w.set_search_selected(index);
            if let Some(model) = page.duplicate_model.get(position).filter(|row| {
                !row.selection_label.is_empty()
                    && (w.get_duplicate_protected_reference() >= 0
                        || row.selection_label
                            != tr!("Choose a protected reference before selecting copies."))
            }) {
                // The same explanation is available visually and to screen readers.
                w.set_duplicate_selection_notice(model.selection_label.clone());
            }
            w.set_search_full_path(label(&row.path).into());
            show_duplicate_folder_link(w, &page.folder_ids, index);
        }
    }

    pub(super) fn poll_duplicates(&mut self, w: &SortSizeWindow, c: &Controller, changed: bool) {
        if self
            .duplicate
            .current_folder
            .as_ref()
            .is_some_and(|request| {
                request.search_id != self.search_id
                    || request.source_epoch != self.service.lane.lock().unwrap().source_epoch
                    || w.get_duplicate_mode() != SINGLE_MODE
                    || !w.get_search_visible()
                    || self.single_root(w).as_ref() != Some(&request.root)
            })
        {
            self.close_search(w);
        }
        self.remember_duplicate_inventory();
        if let Some((side, result)) = self
            .duplicate
            .picker
            .as_ref()
            .and_then(|(side, picker)| picker.poll().map(|result| (*side, result)))
        {
            self.duplicate.picker = None;
            w.set_choosing_folder(false);
            if let Ok(Some(root)) = &result
                && side == SINGLE_PICKER
                && w.get_duplicate_mode() == SINGLE_MODE
            {
                self.close_search(w);
                w.set_search_visible(true);
                w.set_duplicate_single_root(
                    self.duplicate.single_input.select(root.clone()).into(),
                );
            } else if let Ok(Some(root)) = result
                && side
                    < if w.get_duplicate_mode() == 2 {
                        3
                    } else if w.get_duplicate_mode() == 1 {
                        2
                    } else {
                        0
                    }
            {
                self.close_search(w);
                w.set_search_visible(true);
                self.set_duplicate_scope(w, side, root);
                w.set_search_status("".into());
            }
        }
        // Legacy acquisition commits before its terminal UI state. Direct
        // consultation precedes autosave: refresh again after that commit,
        // even if the UI has remained non-busy throughout persistence.
        let autosave_revision = self
            .service
            .lane
            .lock()
            .unwrap()
            .autosave_revision
            .load(Ordering::Acquire);
        if matches!(w.get_duplicate_mode(), 1 | 2)
            && ((self.duplicate.acquisition_was_busy && !w.get_busy())
                || autosave_revision != self.duplicate.catalogue_autosave_revision
                || self.duplicate.catalogue_limited
                    && (0..3)
                        .filter_map(|side| self.duplicate_scope(w, side))
                        .chain(self.requested.iter().cloned())
                        .collect::<Vec<_>>()
                        != self.duplicate.catalogue_scopes)
        {
            self.refresh_duplicate_choices(w, c);
        }
        self.duplicate.acquisition_was_busy = w.get_busy();
        self.update_duplicate_selection(w, c);
        let catalogue = self.duplicate.catalogue.as_ref().map(|r| r.try_recv());
        if matches!(&catalogue, Some(Err(mpsc::TryRecvError::Disconnected))) {
            self.duplicate.catalogue = None;
            w.set_search_status(tr!("Could not start search.").into());
            w.set_duplicate_ready(false);
        }
        if let Some(Ok((choices, limited))) = catalogue {
            self.duplicate.catalogue = None;
            self.duplicate.catalogue_limited = limited;
            let previous = std::mem::replace(&mut self.duplicate.choices, choices);
            retire(vec![], previous);
            let labels = self
                .duplicate
                .choices
                .iter()
                .map(|s| s.label.clone().into())
                .collect::<Vec<_>>();
            w.set_duplicate_source_labels(Rc::new(VecModel::from(labels)).into());
            let selected = |side: usize| {
                self.duplicate.selected[side]
                    .as_ref()
                    .and_then(|p| {
                        self.duplicate
                            .choices
                            .iter()
                            .position(|s| s.logical_root == *p)
                    })
                    .map_or(-1, |i| i as i32)
            };
            w.set_duplicate_source_a(selected(0));
            w.set_duplicate_source_b(selected(1));
            w.set_duplicate_source_c(selected(2));
            if self
                .duplicate
                .comparison_scopes
                .as_ref()
                .is_some_and(|accepted| {
                    self.duplicate_scope_signatures(w).as_ref() != Some(accepted)
                })
            {
                // Compare only the selected scoped provenance, after the local
                // catalogue response. An independent acquisition may now have
                // published a new generation. Drop the old selection owner and
                // its queued replies; never recompare or probe sources here.
                self.close_search(w);
                w.set_search_visible(true);
            }
            self.update_duplicate_selection(w, c);
            if matches!(w.get_duplicate_mode(), 1 | 2) {
                if self.duplicate.catalogue_limited {
                    w.set_search_status(
                        format!("{} · {}", tr!("Local cache catalogue."), tr!("Partial")).into(),
                    );
                } else {
                    w.set_search_status(if self.duplicate.capped {
                    tr!("At most 16 session inventories are retained for comparison. Save further inventories to use them.")
                } else if w.get_duplicate_mode() == 2 { tr!("Choose three disjoint folders, on the same or different drives. Content reads start with Find duplicates.") } else { tr!("Choose two disjoint folders, on the same or different drives. Content reads start with Find duplicates.") } .into());
                }
            }
        }
        if changed {
            self.update_duplicate_selection(w, c);
            if let Some(task) = &self.duplicate.task {
                task.refresh(self.units, self.presentation);
            }
        }
        if let Some(task) = &self.duplicate.task
            && let Some(p) = task
                .take()
                .filter(|p| p.id == self.search_id)
                .and_then(|p| p.in_context(self.presentation, self.units))
        {
            let same_page = self.duplicate.current.as_ref().is_some_and(|old| {
                old.id == p.id
                    && old.reference == p.reference
                    && old.folder_view == p.folder_view
                    && old.folder_detail == p.folder_detail
                    && old
                        .rows
                        .iter()
                        .map(|row| row.key)
                        .eq(p.rows.iter().map(|row| row.key))
            });
            let selected = if same_page {
                w.get_search_selected()
            } else {
                -1
            };
            let scroll = if same_page {
                w.get_search_scroll_y()
            } else {
                0.
            };
            if w.get_duplicate_mode() != SINGLE_MODE {
                w.set_duplicate_protected_reference(p.reference.map_or(-1, i32::from));
            }
            w.set_duplicate_selection_summary(p.selection_summary.clone().into());
            w.set_duplicate_selection_notice(
                if p.reference.is_none()
                    && p.selection_notice
                        == tr!("Choose a protected reference before selecting copies.")
                {
                    // The reference chooser already presents this instruction.
                    "".into()
                } else {
                    p.selection_notice.clone().into()
                },
            );
            w.set_duplicate_selection_preview_text(p.selection_preview.clone().into());
            w.set_duplicate_selection_preview_open(p.selection_preview_open);
            w.set_duplicate_selection_ready(p.selection_ready);
            w.set_search_complete(p.complete);
            w.set_search_duplicates_visible(p.results_available);
            w.set_search_statistics_visible(false);
            w.set_search_matches(p.groups.to_string().into());
            if !same_page {
                // Reject a release over a different page than the one pressed.
                w.set_duplicate_menu_token(w.get_duplicate_menu_token().wrapping_add(1));
            }
            w.set_search_results(Rc::new(VecModel::from(p.model.clone())).into());
            w.set_duplicate_rows(Rc::new(VecModel::from(p.duplicate_model.clone())).into());
            w.set_duplicate_folder_view(p.folder_view);
            w.set_duplicate_folder_detail(p.folder_detail);
            show_duplicate_folder_link(w, &p.folder_ids, selected);
            w.set_duplicate_folder_count(
                crate::number_format::integer(p.folder_count.into()).into(),
            );
            w.set_duplicate_file_count(crate::number_format::integer(p.file_count.into()).into());
            w.set_duplicate_category_index(if p.folder_view { 0 } else { 1 });
            w.set_duplicate_summary(p.summary.clone().into());
            w.set_duplicate_warning(p.warning.clone().into());
            w.set_duplicate_details(p.details.clone().into());
            w.set_duplicate_content_retry(p.content_retry);
            w.set_duplicate_scope_summary(
                Rc::new(VecModel::from_iter(
                    p.scope_summary.iter().map(|path| path.as_str().into()),
                ))
                .into(),
            );
            w.set_search_status(p.status.clone().into());
            w.set_search_scope(p.scope.clone().into());
            w.set_search_statistics(p.statistics.clone().into());
            w.set_search_busy(p.busy);
            w.set_search_previous(p.previous);
            w.set_search_next(p.next);
            w.set_search_selected(selected);
            w.set_search_full_path(
                usize::try_from(selected)
                    .ok()
                    .and_then(|index| p.rows.get(index))
                    .map_or_else(String::new, |row| label(&row.path))
                    .into(),
            );
            w.set_search_scroll_y(scroll);
            #[cfg(feature = "benchmarks")]
            if p.results_available && !p.duplicate_model.is_empty() {
                self.duplicate_applied_at = Some(std::time::Instant::now());
                i_slint_backend_winit::presentation_probe::phase(4);
            }
            self.duplicate.current = Some(p);
        }
    }
}

pub(super) fn connect(w: &SortSizeWindow, c: &Rc<RefCell<Controller>>, h: &Rc<RefCell<Host>>) {
    macro_rules! connect {($method:ident,|$w:ident,$h:ident,$c:ident $(,$arg:ident)*|$body:block)=>{{let weak=w.as_weak();let h=h.clone();let c=c.clone();w.$method(move |$($arg),*|{if let Some($w)=weak.upgrade(){#[allow(unused_mut)]let mut $h=h.borrow_mut();#[allow(unused_mut)]let mut $c=c.borrow_mut();let _scope=$c.presentation.enter();$body}});}};}
    connect!(on_duplicate_reference_changed, |w, h, _c, side| {
        if !matches!(w.get_duplicate_mode(), 1 | 2)
            || !(-1..=if w.get_duplicate_mode() == 2 { 2 } else { 1 }).contains(&side)
            || w.get_search_busy()
        {
            return;
        }
        if w.get_duplicate_protected_reference() != side {
            w.set_duplicate_protected_reference(side);
            h.reset_duplicate_review(&w, false);
            w.set_duplicate_selection_notice(
                tr!("Selection cleared after the protected reference changed.").into(),
            );
            if let Some(task) = &h.duplicate.task {
                task.reference(u8::try_from(side).ok());
            }
        }
    });
    connect!(on_duplicate_toggle_copy, |w, h, _c, index, selected| {
        if !w.get_duplicate_selection_ready() || w.get_search_busy() {
            return;
        }
        let target = h
            .duplicate
            .current
            .as_ref()
            .filter(|p| {
                p.id == h.search_id
                    && p.selection_ready
                    && p.reference == u8::try_from(w.get_duplicate_protected_reference()).ok()
            })
            .and_then(|p| {
                usize::try_from(index)
                    .ok()
                    .and_then(|i| p.selection_targets.get(i))
                    .and_then(|target| target.map(|target| (target, p.reference)))
            });
        if let Some((target, reference)) = target
            && let Some(task) = &h.duplicate.task
        {
            task.toggle(target, selected, reference);
        }
    });
    connect!(on_duplicate_selection_preview, |w, h, _c| {
        if !w.get_duplicate_selection_ready() || w.get_search_busy() {
            return;
        }
        if let Some(task) = &h.duplicate.task {
            task.preview();
        }
    });
    connect!(on_duplicate_selection_apply, |w, h, _c| {
        if w.get_duplicate_selection_preview_open()
            && let Some(task) = &h.duplicate.task
        {
            w.set_duplicate_selection_preview_open(false);
            task.apply();
        }
    });
    connect!(on_duplicate_selection_dismiss, |w, h, _c| {
        w.set_duplicate_selection_preview_open(false);
        if let Some(task) = &h.duplicate.task {
            task.dismiss();
        }
    });
    connect!(on_duplicate_selection_clear, |_w, h, _c| {
        if let Some(task) = &h.duplicate.task {
            task.clear();
        }
    });
    connect!(on_duplicate_category_changed, |_w, h, _c, category| {
        if let Some(task) = &h.duplicate.task {
            task.category(category == 0);
        }
    });
    connect!(on_duplicate_open_folder, |_w, h, _c, index| {
        if let Some(id) = h
            .duplicate
            .current
            .as_ref()
            .and_then(|p| {
                usize::try_from(index)
                    .ok()
                    .and_then(|index| p.folder_ids.get(index))
            })
            .filter(|id| **id != 0)
            && let Some(task) = &h.duplicate.task
        {
            task.open_folder(*id);
        }
    });
    connect!(on_duplicate_mode_changed, |w, h, c, mode| {
        h.close_search(&w);
        w.set_search_visible(true);
        set_duplicate_mode(&w, mode);
        if matches!(mode, 1 | 2) {
            h.refresh_duplicate_choices(&w, &c);
        }
        h.update_duplicate_selection(&w, &c);
    });
    connect!(on_duplicate_select, |w, h, c, side, index| {
        if matches!(w.get_duplicate_mode(), 1 | 2)
            && let Ok(side @ 0..=2) = usize::try_from(side)
            && side < if w.get_duplicate_mode() == 2 { 3 } else { 2 }
        {
            h.close_search(&w);
            w.set_search_visible(true);
            h.duplicate.selected[side] = usize::try_from(index)
                .ok()
                .and_then(|i| h.duplicate.choices.get(i))
                .map(|s| s.logical_root.clone());
            if let Some(root) = usize::try_from(index)
                .ok()
                .and_then(|i| h.duplicate.choices.get(i))
                .map(|s| s.native_root.clone())
            {
                h.set_duplicate_scope(&w, side, root);
            }
            w.set_search_status("".into());
            h.update_duplicate_selection(&w, &c);
        }
    });
    connect!(on_duplicate_run, |w, h, c| {
        if w.get_duplicate_mode() == SINGLE_MODE {
            h.start_single_search(&w, &mut c);
            return;
        }
        h.update_duplicate_selection(&w, &c);
        if !w.get_duplicate_ready() {
            w.set_duplicate_settings_expanded(true);
            return;
        }
        let choice = |side: usize| h.scoped_duplicate_choice(&w, side);
        let (Some(a), Some(b)) = (choice(0), choice(1)) else {
            return;
        };
        let mode = w.get_duplicate_mode();
        let third = if mode == 2 { choice(2) } else { None };
        if mode == 2 && third.is_none() {
            return;
        }
        let comparison_scopes = h.duplicate_scope_signatures(&w);
        h.close_search(&w);
        w.set_search_visible(true);
        w.set_duplicate_mode(mode);
        w.set_duplicate_reveal_allowed(false);
        let task = if let Some(third) = third {
            Handle::start_three(
                h.search_id,
                a,
                b,
                third,
                h.service.lane.clone(),
                h.units,
                h.presentation,
            )
        } else {
            Handle::start(
                h.search_id,
                a,
                b,
                h.service.lane.clone(),
                h.units,
                h.presentation,
            )
        };
        match task {
            Ok(task) => {
                if let Ok(side) = u8::try_from(w.get_duplicate_protected_reference())
                    && side < if mode == 2 { 3 } else { 2 }
                {
                    task.reference(Some(side));
                }
                h.duplicate.task = Some(task);
                h.duplicate.comparison_scopes = comparison_scopes;
                w.set_duplicate_settings_expanded(false);
                let mut paths = vec![w.get_duplicate_scope_a(), w.get_duplicate_scope_b()];
                if mode == 2 {
                    paths.push(w.get_duplicate_scope_c());
                }
                w.set_duplicate_scope_summary(Rc::new(VecModel::from(paths)).into());
                w.set_duplicate_summary(tr!("Preparing inventory…").into());
                w.set_search_busy(true);
                w.set_search_status(tr!("Preparing inventory…").into());
            }
            Err(e) => {
                w.set_duplicate_settings_expanded(true);
                w.set_search_busy(false);
                w.set_search_status(format!("{} {e}", tr!("Incomplete coverage")).into());
            }
        }
    });
    connect!(on_duplicate_content_limits_edited, |w, _h, _c| {
        w.set_duplicate_content_limits_error("".into());
    });
    connect!(on_duplicate_resource_apply, |w, _h, c| {
        let memory = optional_resource_limit(w.get_duplicate_resource_memory_mib().as_str());
        let temporary = optional_resource_limit(w.get_duplicate_resource_temporary_mib().as_str());
        let (Some(memory), Some(temporary)) = (memory, temporary) else {
            w.set_duplicate_resource_invalid(true);
            return;
        };
        w.set_duplicate_resource_invalid(false);
        w.set_duplicate_resource_dirty(false);
        let settings = &mut c.preferences.duplicate_resources;
        if (settings.memory_limit, settings.temporary_limit) != (memory, temporary) {
            settings.memory_limit = memory;
            settings.temporary_limit = temporary;
            c.save_preferences(&w);
        }
        c.show_duplicate_resource_preferences(&w);
    });
    connect!(on_duplicate_resource_browse, |w, _h, c| {
        if w.get_choosing_folder() || !w.get_folder_picker_enabled() {
            return;
        }
        let mut dialog = rfd::FileDialog::new()
            .set_title(tr!("Choose a local temporary folder"))
            .set_parent(&w.window().window_handle());
        if let Some(parent) = &c.preferences.duplicate_resources.temporary_parent {
            dialog = dialog.set_directory(parent);
        }
        w.set_duplicate_resource_picker_failed(false);
        let pick = move || {
            let path = dialog.pick_folder()?;
            Some(
                if path.is_absolute()
                    && matches!(
                        sortsize_core::acquisition::source_class(&path),
                        Ok(sortsize_core::acquisition::SourceClass::Local)
                    )
                {
                    Ok(path)
                } else {
                    Err(())
                },
            )
        };
        match crate::folder_picker::FolderPicker::start(pick) {
            Ok(picker) => {
                c.duplicate_temporary_picker = Some(picker);
                w.set_choosing_folder(true);
            }
            Err(_) => w.set_duplicate_resource_picker_failed(true),
        }
    });
    connect!(on_duplicate_resource_reset_parent, |w, _h, c| {
        if !w.get_choosing_folder() {
            w.set_duplicate_resource_picker_failed(false);
            c.set_duplicate_temporary_parent(&w, None);
        }
    });
    connect!(on_duplicate_scope_edited, |w, h, c, side| {
        if w.get_duplicate_mode() == SINGLE_MODE {
            if side == SINGLE_PICKER as i32
                && (!w.get_search_busy() || w.get_duplicate_waiting_for_scan())
            {
                h.duplicate.single_input.edited();
                h.close_search(&w);
                w.set_search_visible(true);
                h.update_single_selection(&w, &c);
            }
            return;
        }
        if let Ok(side @ 0..=2) = usize::try_from(side)
            && side < if w.get_duplicate_mode() == 2 { 3 } else { 2 }
        {
            h.duplicate.scope_inputs[side].edited();
            h.close_search(&w);
            w.set_search_visible(true);
            w.set_search_status("".into());
            h.update_duplicate_selection(&w, &c);
        }
    });
    connect!(on_duplicate_browse_scope, |w, h, c, side| {
        let Ok(side) = usize::try_from(side) else {
            return;
        };
        let root = match w.get_duplicate_mode() {
            SINGLE_MODE if side == SINGLE_PICKER => h.single_root(&w),
            1 if side < 2 => h.duplicate_scope(&w, side),
            2 if side < 3 => h.duplicate_scope(&w, side),
            _ => return,
        };
        if w.get_choosing_folder() || (w.get_search_busy() && !w.get_duplicate_waiting_for_scan()) {
            return;
        }
        #[cfg(any(target_os = "linux", windows))]
        {
            let dialog = rfd::FileDialog::new()
                .set_title(tr!("Choose a folder to scan"))
                .set_parent(&w.window().window_handle());
            let dialog = if let Some(root) = root {
                dialog.set_directory(root)
            } else {
                dialog
            };
            if let Ok(picker) =
                crate::folder_picker::FolderPicker::start(move || dialog.pick_folder())
            {
                h.cancel_folder_request(&w);
                h.duplicate.picker = Some((side, picker));
                w.set_choosing_folder(true);
            }
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = root;
        h.update_duplicate_selection(&w, &c);
    });
    connect!(on_duplicate_inventory, |w, h, c, side| {
        if !matches!(w.get_duplicate_mode(), 1 | 2) {
            return;
        }
        let choice = usize::try_from(side)
            .ok()
            .filter(|side| *side < if w.get_duplicate_mode() == 2 { 3 } else { 2 })
            .and_then(|side| h.scoped_duplicate_choice(&w, side));
        if let Some(choice) = choice.filter(|s| !c.registry.is_disconnected(&s.logical_root)) {
            if w.get_busy() || w.get_cancelling() {
                return;
            }
            let mode = w.get_duplicate_mode();
            let root = choice.native_root.clone();
            let preflight = c.registry.scan_preflight(&choice.logical_root);
            // This explicit intention replaces both queued and not-yet-resolved
            // startup restoration. It must not be superseded by a parent scan
            // when native discovery finishes on a later UI poll.
            c.pending_auto = None;
            c.restored_selection = None;
            c.auto_analysis.manual(&root);
            h.start_with_mode(
                &w,
                &mut c,
                Start {
                    root,
                    explicit_root: true,
                    observation: sortsize_core::acquisition::ScanObservation::Strict,
                    preflight,
                    options: sortsize_store::scheduler::CollectionOptions {
                        allocation: w.get_collect_allocation(),
                        owners: w.get_collect_owner(),
                    },
                },
                false,
                service::StartPurpose::DuplicateInventory,
            );
            // Stay in the panel that launched this exact-folder acquisition.
            w.set_search_visible(true);
            w.set_duplicate_mode(mode);
            h.duplicate.acquisition_was_busy = w.get_busy();
            w.set_search_status(tr!("Scan in progress…").into());
            h.update_duplicate_selection(&w, &c);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_content_options_use_empty_fields_and_exact_unsigned_bytes() {
        let defaults = content_options("", " ").unwrap();
        assert_eq!(defaults.max_bytes_read, None);
        assert_eq!(defaults.max_file_bytes, None);
        assert_eq!(defaults.max_errors, 100);
        let maximum = u64::MAX.to_string();
        let limits = content_options(" 0 ", &maximum).unwrap();
        assert_eq!(limits.max_bytes_read, Some(0));
        assert_eq!(limits.max_file_bytes, Some(u64::MAX));
        for invalid in ["18446744073709551616", "-1", "+1", "1,024", "1.5", "2 MiB"] {
            assert!(content_options(invalid, "").is_none(), "{invalid}");
            assert!(content_options("", invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn duplicate_resource_limits_distinguish_automatic_zero_and_overflow() {
        use crate::display_preferences::MIB;

        assert_eq!(optional_resource_limit(" "), Some(None));
        assert_eq!(optional_resource_limit(" 70 "), Some(Some(70 * MIB)));
        let max_mib = u64::MAX / MIB;
        assert_eq!(
            optional_resource_limit(&max_mib.to_string()),
            Some(Some(max_mib * MIB))
        );
        for invalid in ["0", "-1", "+1", "1.5", "1 MiB", &(max_mib + 1).to_string()] {
            assert_eq!(optional_resource_limit(invalid), None);
        }
    }

    fn choice(logical: &str, root: &str, id: u128) -> Choice {
        Choice {
            label: id.to_string(),
            logical_root: logical.into(),
            native_root: root.into(),
            inventories: vec![Inventory {
                workspace: None,
                saved: Saved {
                    directory: PathBuf::new(),
                    context: AcquisitionContext {
                        inventory: InventoryId(id),
                        source: SourceId(id),
                        generation: GenerationId(1),
                        scan: ScanId(1),
                        policy: PolicyId(1),
                        mount: MountEpoch(1),
                        run: RunEpoch(1),
                        validation: ValidationEpoch(1),
                    },
                    root: root.into(),
                    started: 0,
                    measured: Some(1),
                    coverage: ScopeCoverage::Complete,
                    logical: None,
                    unknown: 0,
                    continuity: None,
                },
            }],
        }
    }

    #[test]
    fn duplicate_review_provenance_changes_only_for_the_selected_scopes() {
        let root = std::env::temp_dir().join("bunny-review-provenance");
        let a_root = root.join("source-a");
        let b_root = root.join("source-b");
        let a_scope = a_root.join("chosen");
        let a = choice(&a_root.to_string_lossy(), &a_root.to_string_lossy(), 11);
        let b = choice(&b_root.to_string_lossy(), &b_root.to_string_lossy(), 12);
        let signatures = |a: Choice, b: Choice| {
            [
                ScopeSignature::from_choice(&scope_choice(a, a_scope.clone())),
                ScopeSignature::from_choice(&scope_choice(b, b_root.clone())),
            ]
        };
        let accepted = signatures(a.clone(), b.clone());
        let mut refreshed = a.clone();
        refreshed.label = "translated display label".into();
        assert_eq!(accepted, signatures(refreshed.clone(), b.clone()));
        // New data from an unselected sibling is excluded before identifying
        // the comparison, just as during the actual launch.
        let mut sibling = refreshed.inventories[0].clone();
        sibling.saved.root = a_root.join("other-folder");
        sibling.saved.context.generation = GenerationId(99);
        sibling.saved.measured = Some(99);
        refreshed.inventories.push(sibling);
        assert_eq!(accepted, signatures(refreshed.clone(), b.clone()));
        let mut next = refreshed.inventories[0].clone();
        next.saved.context.generation = GenerationId(2);
        next.saved.measured = Some(2);
        refreshed.inventories.push(next);
        assert_ne!(accepted, signatures(refreshed, b.clone()));
        let mut changed_identity = a.clone();
        changed_identity.inventories[0].saved.context.source = SourceId(111);
        assert_ne!(accepted, signatures(changed_identity, b.clone()));
        let mut changed_proof_context = b.clone();
        changed_proof_context.inventories[0]
            .saved
            .context
            .validation = ValidationEpoch(2);
        assert_ne!(accepted, signatures(a.clone(), changed_proof_context));
        let mut remapped = accepted.clone();
        remapped[0].native_root = a_root.join("different-folder");
        assert_ne!(accepted, remapped);
        let mut rebound = accepted.clone();
        rebound[0].inventories[0].0 = root.clone();
        assert_ne!(accepted, rebound);
    }

    #[test]
    fn duplicate_review_catalogue_generation_switch_clears_review_and_keeps_inputs() {
        use slint::platform::{
            Platform, WindowAdapter,
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        };
        struct ReviewPlatform(Rc<MinimalSoftwareWindow>);
        impl Platform for ReviewPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        slint::platform::set_platform(Box::new(ReviewPlatform(MinimalSoftwareWindow::new(
            RepaintBufferType::NewBuffer,
        ))))
        .unwrap();
        let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
        let mut host = Host::new().unwrap();
        let root = std::env::temp_dir().join("bunny-review-catalogue");
        let a_root = root.join("a");
        let b_root = root.join("b");
        let a = choice(&a_root.to_string_lossy(), &a_root.to_string_lossy(), 21);
        let b = choice(&b_root.to_string_lossy(), &b_root.to_string_lossy(), 22);
        host.duplicate.choices = vec![a.clone(), b.clone()];
        host.duplicate.selected = [Some(a_root.clone()), Some(b_root.clone()), None];
        host.set_duplicate_scope(&w, 0, a_root.clone());
        host.set_duplicate_scope(&w, 1, b_root.clone());
        host.duplicate.comparison_scopes = host.duplicate_scope_signatures(&w);
        assert!(host.duplicate.comparison_scopes.is_some());
        w.set_duplicate_mode(1);
        w.set_duplicate_protected_reference(0);
        w.set_duplicate_selection_ready(true);
        w.set_duplicate_selection_summary("selected from G1".into());
        w.set_duplicate_selection_preview_open(true);
        let old_id = host.search_id;
        // An unrelated new source and a translated label do not reset checks.
        let c_root = root.join("unselected-c");
        let unrelated = choice(&c_root.to_string_lossy(), &c_root.to_string_lossy(), 23);
        let (send, receive) = mpsc::channel();
        send.send((vec![a.clone(), b.clone(), unrelated.clone()], false))
            .unwrap();
        host.duplicate.catalogue = Some(receive);
        host.poll_duplicates(&w, &c.borrow(), false);
        assert_eq!(host.search_id, old_id);
        assert!(w.get_duplicate_selection_ready());
        assert!(w.get_duplicate_selection_preview_open());
        // Publication of a selected scope's next immutable generation revokes
        // the old owner and all late replies before any further UI polling.
        let mut next = a;
        next.inventories[0].saved.context.generation = GenerationId(2);
        next.inventories[0].saved.measured = Some(2);
        let (send, receive) = mpsc::channel();
        send.send((vec![next, b, unrelated], false)).unwrap();
        host.duplicate.catalogue = Some(receive);
        host.poll_duplicates(&w, &c.borrow(), false);
        assert!(host.search_id > old_id);
        assert!(host.duplicate.comparison_scopes.is_none());
        assert!(!w.get_duplicate_selection_ready());
        assert!(!w.get_duplicate_selection_preview_open());
        assert!(w.get_duplicate_selection_summary().is_empty());
        assert!(!w.get_duplicate_selection_notice().is_empty());
        assert_eq!(w.get_duplicate_protected_reference(), 0);
        assert_eq!(host.duplicate_scope(&w, 0), Some(a_root));
        assert_eq!(host.duplicate_scope(&w, 1), Some(b_root));
        assert!(w.get_search_visible());
    }

    #[test]
    fn duplicate_three_source_settings_and_ab_provenance_exclude_old_c() {
        use slint::platform::{
            Platform, WindowAdapter,
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        };
        struct TestPlatform(Rc<MinimalSoftwareWindow>);
        impl Platform for TestPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
            RepaintBufferType::NewBuffer,
        ))))
        .unwrap();
        let (w, c) = crate::new_session(Default::default(), Some("en".into())).unwrap();
        let host = Rc::new(RefCell::new(Host::new().unwrap()));
        connect(&w, &c, &host);
        let root = std::env::temp_dir().join("bunny-three-ui");
        {
            let mut h = host.borrow_mut();
            h.duplicate.choices = (0..3)
                .map(|side| {
                    let root = root.join(side.to_string());
                    choice(
                        &root.to_string_lossy(),
                        &root.to_string_lossy(),
                        5000 + side,
                    )
                })
                .collect();
        }
        w.invoke_duplicate_mode_changed(2);
        for side in 0..3 {
            w.invoke_duplicate_select(side, side);
        }
        assert_eq!(w.get_duplicate_mode(), 2);
        assert!(
            !w.get_duplicate_ready(),
            "catalogue preparation disables content reads"
        );
        assert!(
            w.get_duplicate_disabled_reason()
                .contains("Preparing saved measures")
        );
        // Deliver a deterministic metadata catalogue instead of racing the
        // asynchronous catalogue started by the real mode-change callback.
        let choices = host.borrow().duplicate.choices.clone();
        let (send, receive) = mpsc::channel();
        send.send((choices, false)).unwrap();
        host.borrow_mut().duplicate.catalogue = Some(receive);
        host.borrow_mut().poll_duplicates(&w, &c.borrow(), false);
        assert!(w.get_duplicate_ready());
        assert!(w.get_duplicate_scope_c().contains("2"));
        let signatures = host.borrow().duplicate_scope_signatures(&w).unwrap();
        assert_eq!(signatures.len(), 3);
        assert!(
            !w.get_busy() && !w.get_search_busy(),
            "choosing sources and mode cannot scan"
        );
        w.invoke_duplicate_reference_changed(2);
        assert_eq!(w.get_duplicate_protected_reference(), 2);
        w.set_duplicate_selection_ready(true);
        w.set_duplicate_selection_preview_open(true);
        w.invoke_duplicate_mode_changed(1);
        assert_eq!(w.get_duplicate_protected_reference(), -1);
        assert!(!w.get_duplicate_selection_ready() && !w.get_duplicate_selection_preview_open());
        let ab = host.borrow().duplicate_scope_signatures(&w).unwrap();
        assert_eq!(ab, signatures[..2]);
        host.borrow_mut().duplicate.choices[2].inventories[0]
            .saved
            .context
            .generation = GenerationId(99);
        assert_eq!(host.borrow().duplicate_scope_signatures(&w).unwrap(), ab);
        assert!(!w.get_busy() && !w.get_search_busy());
    }

    #[cfg(windows)]
    #[test]
    fn duplicate_selected_snapshots_remain_available_beyond_catalogue_limit_and_forget() {
        use sortsize_core::{acquisition::Guard, inventory_contract::ListingEnd};
        use sortsize_store::{
            direct_save::{ReplayBatch, SaveRequest},
            direct_tree::DirectTree,
        };
        let temporary = tempfile::tempdir().unwrap();
        let catalogue = temporary.path().join("catalogue");
        let history = super::super::cache::snapshot_root(&catalogue);
        sortsize_store::inventory::lifecycle::ensure_root(&catalogue).unwrap();
        sortsize_store::inventory::lifecycle::ensure_root(&history).unwrap();
        let mut paths = Vec::new();
        let mut selected = Vec::new();
        for index in 0..130 {
            let source = temporary.path().join(format!("source-{index:03}"));
            std::fs::create_dir(&source).unwrap();
            let guard = Guard::open(&source, Default::default()).unwrap();
            let mut tree =
                DirectTree::new(source.clone(), temporary.path(), 32 * 1024 * 1024).unwrap();
            tree.accept_replay(ReplayBatch {
                parent: EntryId(0),
                sequence: 0,
                end: Some(ListingEnd::Exhausted),
                observations: Vec::new(),
                evidence: Vec::new(),
                properties: Vec::new(),
                directory: Some(guard.evidence().1.clone()),
                children: Vec::new(),
            })
            .unwrap();
            tree.finish().unwrap();
            let path = sortsize_store::direct_snapshot::destination(&history, &source);
            let context = choice("", "", index + 1).inventories[0].saved.context;
            let summary = tree.summary();
            sortsize_store::direct_snapshot::save(
                SaveRequest {
                    context,
                    guard: &guard,
                    scratch: temporary.path(),
                    destination: &path,
                    started: 0,
                    measured: 1,
                    options: Default::default(),
                    coverage: summary.coverage,
                },
                summary,
                tree.replay().unwrap(),
                &Default::default(),
            )
            .unwrap();
            paths.push(path);
            if index >= 128 {
                selected.push(source);
            }
        }
        assert!(super::super::cache::list_catalogues(&catalogue, None, &[]).is_err());
        let descriptors = || {
            selected
                .iter()
                .map(|root| Descriptor {
                    label: short_label(root),
                    logical: root.clone(),
                    native: root.clone(),
                })
                .collect::<Vec<_>>()
        };
        let (choices, limited) = collect_choices_for_roots(
            descriptors(),
            vec![],
            vec![],
            Some(catalogue.clone()),
            selected.clone(),
        );
        assert!(limited);
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|choice| choice.inventories.len() == 1));
        let stale = choices[0].inventories[0].saved.clone();
        let forgotten = std::fs::canonicalize(&stale.directory).unwrap();
        std::fs::remove_file(&stale.directory).unwrap();
        sortsize_core::resources::Governor::global().forget_deleted_artifact_group(&forgotten);
        let (after, limited) = collect_choices_for_roots(
            descriptors(),
            vec![],
            vec![stale.clone()],
            Some(catalogue),
            selected,
        );
        assert!(
            limited,
            "129 remaining snapshots still exceed the global cap"
        );
        assert!(
            after
                .iter()
                .find(|choice| choice.native_root == stale.root)
                .unwrap()
                .inventories
                .is_empty()
        );
        assert_eq!(
            after
                .iter()
                .map(|choice| choice.inventories.len())
                .sum::<usize>(),
            1
        );
        let governor = sortsize_core::resources::Governor::global();
        for path in paths {
            let canonical = std::fs::canonicalize(&path).ok();
            if path.exists() {
                std::fs::remove_file(&path).unwrap();
            }
            if let Some(path) = canonical {
                governor.forget_deleted_artifact_group(&path);
            }
            governor.forget_deleted_artifact_group(&path);
        }
    }

    #[test]
    fn duplicate_catalogue_does_not_restore_a_forgotten_snapshot_from_cached_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("source");
        let catalogue = temporary.path().join("catalogue");
        sortsize_store::inventory::lifecycle::ensure_root(&catalogue).unwrap();
        assert!(
            super::super::cache::list_catalogues(&catalogue, None, &[])
                .unwrap()
                .rows
                .is_empty()
        );
        let mut stale = choice("source", "recorded", 1).inventories.remove(0).saved;
        stale.root = root.clone();
        stale.directory = super::super::cache::snapshot_root(&catalogue).join("auto-old.sqlite");
        let choices = collect_choices(
            vec![Descriptor {
                label: "source".into(),
                logical: root.clone(),
                native: root,
            }],
            vec![],
            vec![stale],
            Some(catalogue),
        );
        assert_eq!(choices.len(), 1);
        assert!(choices[0].inventories.is_empty());
    }

    #[test]
    fn duplicate_catalogue_parent_inventory_does_not_cover_another_source_mount() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        let other_mount = root.join("media").join("DATA-Linux");
        let mut saved = choice("parent", "recorded", 1).inventories.remove(0).saved;
        saved.root = root.clone();
        let choices = collect_choices(
            vec![
                Descriptor {
                    label: "parent".into(),
                    logical: root.clone(),
                    native: root.clone(),
                },
                Descriptor {
                    label: "other-mount".into(),
                    logical: other_mount.clone(),
                    native: other_mount.clone(),
                },
            ],
            vec![],
            vec![saved],
            Some(t.path().join("catalogue")),
        );
        assert_eq!(
            choices
                .iter()
                .find(|c| c.logical_root == root)
                .unwrap()
                .inventories
                .len(),
            1
        );
        assert!(
            choices
                .iter()
                .find(|c| c.logical_root == other_mount)
                .unwrap()
                .inventories
                .is_empty()
        );
    }

    #[test]
    fn duplicate_scope_uses_one_ancestor_and_never_adds_sibling_inventories() {
        let root = std::env::temp_dir().join("bunny-scope-source");
        let mut source = choice("source", "recorded", 1);
        source.native_root = root.clone();
        source.inventories[0].saved.root = root.clone();
        let mut sibling = source.inventories[0].clone();
        sibling.saved.root = root.join("other");
        sibling.saved.context.source = SourceId(2);
        let mut narrower = source.inventories[0].clone();
        narrower.saved.root = root.join("selected");
        narrower.saved.context.generation = GenerationId(2);
        source.inventories.extend([sibling, narrower]);
        let scope = root.join("selected");
        let selected = scope_choice(source.clone(), scope.clone());
        assert_eq!(selected.native_root, scope);
        assert_eq!(selected.inventories.len(), 1);
        assert_eq!(
            selected.inventories[0].saved.context.generation,
            GenerationId(2)
        );
        let selected = scope_choice(source.clone(), root.join("another"));
        assert_eq!(selected.inventories.len(), 1);
        assert_eq!(selected.inventories[0].saved.root, root);
        source.inventories.remove(0);
        let absent = scope_choice(source, root.join("another"));
        assert!(
            absent.inventories.is_empty(),
            "sibling coverage is not coverage of the selected folder"
        );
    }

    #[cfg(windows)]
    #[test]
    fn duplicate_scope_requires_exact_native_windows_spelling() {
        let source = choice(r"C:\Source", r"C:\Source\ChosenFolder", 1);
        let changed_case = scope_choice(
            source.clone(),
            PathBuf::from(r"c:\source\chosenfolder\child"),
        );
        assert!(changed_case.inventories.is_empty());
        let exact = scope_choice(source, PathBuf::from(r"C:\Source\ChosenFolder\child"));
        assert_eq!(exact.inventories.len(), 1);
        assert_eq!(
            exact.native_root,
            PathBuf::from(r"C:\Source\ChosenFolder\child")
        );
    }

    #[test]
    fn duplicate_source_availability_uses_inventory_roots_not_mount_ancestry() {
        #[cfg(not(windows))]
        let (a, mut b) = (
            choice("/", "/opt/bunny-corpus-a", 1),
            choice("/media/DATA", "/media/DATA/bunny-corpus-b", 2),
        );
        #[cfg(windows)]
        let (a, mut b) = (
            choice(r"C:\", r"C:\BunnyCorpusA", 1),
            choice(r"D:\", r"D:\BunnyCorpusB", 2),
        );
        assert!(distinct_inventories(&a, &b));
        assert!(distinct_inventories(&b, &a));
        let other_root = b.inventories[0].saved.root.clone();
        b.native_root = a.native_root.join("nested");
        assert!(!distinct_inventories(&a, &b));
        assert!(!distinct_inventories(&b, &a));
        b.native_root = other_root;
        b.inventories[0].saved.context.source = a.inventories[0].saved.context.source;
        assert!(distinct_inventories(&a, &b));
        b.inventories[0].saved.context.source = SourceId(2);
        b.logical_root = a.logical_root.clone();
        assert!(distinct_inventories(&a, &b));
        b.inventories.clear();
        assert!(!distinct_inventories(&a, &b));
    }
}
