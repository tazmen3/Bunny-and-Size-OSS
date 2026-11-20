//! V0.5 Atelier adapter with independent, bounded acquisition and certified-read owners.
use crate::{Controller, DetailRow, MapTile, SortSizeWindow, TopRow, TreeRow};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use sortsize_core::inventory_contract::*;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
};
mod cache;
mod disconnect;
mod duplicate_ui;
mod duplicates;
mod empty_analysis;
mod folder_tree;
mod owners;
mod reader;
mod scan_p1;
mod scan_p3;
mod search;
mod server_ui;
mod service;
mod smb_server;
#[cfg(any(windows, target_os = "linux"))]
mod windows_owners;
use service::{Navigate, Output, Prepared, Service, Start};
pub(crate) fn label(path: &Path) -> String {
    let text = crate::tree::native_label(path);
    #[cfg(windows)]
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text
        .strip_prefix(r"\\?\")
        .filter(|s| s.as_bytes().get(1) == Some(&b':'))
    {
        rest.to_owned()
    } else {
        text
    };
    text
}

#[cfg(test)]
struct TestWindowVisibility(SortSizeWindow);
#[cfg(test)]
impl TestWindowVisibility {
    fn new(window: &SortSizeWindow) -> Self {
        Self(window.clone_strong())
    }
}
#[cfg(test)]
impl Drop for TestWindowVisibility {
    fn drop(&mut self) {
        // Slint keeps a strong component reference after show(). Release it on
        // success and unwinding so callbacks cannot retain old test services.
        let _ = self.0.hide();
    }
}

#[cfg(test)]
fn test_wait_state(host: &Host) -> String {
    fn page(page: &Prepared) -> String {
        format!(
            "epoch={} parent={} revision={} busy={} measured={} details={} tree={} memory={}/{}",
            page.epoch,
            page.parent_entry.0,
            page.frame.stamp.revision.0,
            page.busy,
            page.measure_ready,
            page.details.len(),
            page.folders.len(),
            page._memory.bytes(),
            page._folder_memory.as_ref().map_or(0, |m| m.bytes()),
        )
    }
    let current = host.current.as_ref().map(|p| page(p));
    let pending = host.pending.as_ref().map(|p| page(p));
    let tree_edges = host.current.as_ref().map(|p| {
        p.folders
            .iter()
            .take(3)
            .chain(p.folders.iter().rev().take(3))
            .map(|row| row.name.to_string())
            .collect::<Vec<_>>()
    });
    let lane = host.service.lane.try_lock().ok().map(|l| {
        format!(
            "source={} view={} phase={:?} writer={} reader={} navigation_epoch={:?} desired={:?}",
            l.source_epoch,
            l.view_epoch,
            l.phase,
            l.writer_running,
            l.reader_active,
            l.navigation.as_ref().map(|(epoch, _)| epoch),
            l.desired_folder,
        )
    });
    let output = host
        .service
        .output
        .try_lock()
        .ok()
        .map(|o| match o.as_ref() {
            Some(Output::Page(p)) => page(p),
            Some(Output::Error(epoch, message, _)) => format!("error epoch={epoch}: {message}"),
            None => "empty".into(),
        });
    format!(
        "host_epoch={} current={current:?} pending={pending:?} pending_folder={:?} deferred_rows={:?} tree_edges={tree_edges:?} lane={lane:?} mailbox={output:?} resources={:?}",
        host.epoch,
        host.pending_folder,
        host.deferred_tree.as_ref().map(|tree| tree.rows.len()),
        sortsize_core::resources::Governor::global().usage(),
    )
}
#[cfg(test)]
pub(super) fn placement_message(
    error: sortsize_store::inventory::placement::PlacementError,
) -> String {
    use sortsize_store::inventory::Error;
    let summary = error
        .attempts
        .iter()
        .find(|attempt| matches!(attempt.reason, Error::DiskAdmission(_)))
        .map_or_else(
            || tr!("The working folder is unavailable. Set BUNNY_AND_SIZE_STORE_DIR to an existing local folder and restart Bunny and Size.").into(),
            |attempt| disk_admission_summary(&attempt.directory),
        );
    error.attempts.iter().fold(summary, |mut message, attempt| {
        use std::fmt::Write;
        let _ = write!(
            message,
            "\n{} - {}",
            label(&attempt.directory),
            attempt.reason
        );
        message
    })
}
pub(super) fn acquisition_error_message(error: sortsize_store::inventory::Error) -> String {
    if let sortsize_store::inventory::Error::DiskAdmission(details) = error {
        format!("{}\n{details}", disk_admission_summary(&details.scratch))
    } else {
        error.to_string()
    }
}
fn disk_admission_summary(path: &Path) -> String {
    tr_format!(
        "The shared disk quota refused the working folder {}. Set BUNNY_AND_SIZE_STORE_DIR to another local folder and restart Bunny and Size.",
        label(path)
    )
}
fn short_label(path: &Path) -> String {
    let text = label(path);
    if text.chars().count() > 512 {
        format!("{}…", text.chars().take(511).collect::<String>())
    } else {
        text
    }
}
struct DeferredTree {
    rows: Vec<TreeRow>,
    keys: Vec<folder_tree::TreeTarget>,
    memory: Option<sortsize_core::resources::Reservation>,
}
struct TreeMenuTarget {
    source_epoch: u64,
    source: SourceId,
    key: EntryKey,
    path: Arc<PathBuf>,
}
struct InterruptedSource {
    root: PathBuf,
    selection: Option<crate::auto_analysis::LastSelection>,
    options: sortsize_store::scheduler::CollectionOptions,
    explicit_root: bool,
}
fn expanded_folders(
    rows: &[TreeRow],
    keys: &[folder_tree::TreeTarget],
) -> std::collections::BTreeSet<u64> {
    rows.iter()
        .zip(keys)
        .filter_map(|(row, key)| match key {
            folder_tree::TreeTarget::Folder { key, .. } if row.expanded => Some(key.entry.0),
            _ => None,
        })
        .collect()
}
pub(crate) struct Host {
    duplicate: duplicate_ui::State,
    empty: empty_analysis::State,
    disconnect: disconnect::State,
    server: server_ui::State,
    #[cfg(any(windows, target_os = "linux"))]
    windows_owners: windows_owners::VisibleOwners,
    #[cfg(any(windows, target_os = "linux"))]
    owner_live_source: bool,
    service: Service,
    owner_task: Option<crate::owner_view::Task>,
    owner_request: Option<(u64, i32, bool)>,
    owner_names: Arc<[crate::owner_view::DisplayOwner]>,
    owner_names_tag: Option<(u64, i32)>,
    epoch: u64,
    scan_metrics: Option<scan_p1::UiMetrics>,
    scan_progress_poll: Option<std::time::Instant>,
    requested: Option<PathBuf>,
    source_selection: Option<crate::auto_analysis::LastSelection>,
    collection_options: sortsize_store::scheduler::CollectionOptions,
    explicit_root: bool,
    interrupted_source: Option<InterruptedSource>,
    priority_folder: bool,
    active_service: bool,
    failed: bool,
    current: Option<Box<Prepared>>,
    pending: Option<Box<Prepared>>,
    search_current: Option<search::Prepared>,
    search_pending: Option<search::Prepared>,
    search_cancel: sortsize_core::CancellationToken,
    search_id: u64,
    cache_id: u64,
    cache_wait: Option<u64>,
    cache_autosave_revision: u64,
    cache_comparing: bool,
    cache_cancel: sortsize_core::CancellationToken,
    cache_current: Option<cache::Prepared>,
    preview_current: Option<cache::PreviewPresentation>,
    gesture: bool,
    release: bool,
    tree_focused: Option<folder_tree::TreeTarget>,
    tree_menu_target: Option<TreeMenuTarget>,
    pending_folder: Option<EntryKey>,
    tree_quiet_until: Option<std::time::Instant>,
    deferred_tree: Option<DeferredTree>,
    selected: Option<EntryKey>,
    top_selected: Option<EntryKey>,
    units: crate::units::Units,
    presentation: crate::presentation::Context,
    pub(crate) max_apply_ms: f64,
    pub(crate) last_apply_ms: f64,
    pub(crate) applied: u64,
    #[cfg(feature = "benchmarks")]
    native_probe_phases: bool,
    #[cfg(feature = "benchmarks")]
    navigation_applied_at: Option<std::time::Instant>,
    #[cfg(feature = "benchmarks")]
    duplicate_applied_at: Option<std::time::Instant>,
    #[cfg(feature = "benchmarks")]
    search_applied_at: Option<std::time::Instant>,
    #[cfg(feature = "benchmarks")]
    search_apply_ms: f64,
}
impl Host {
    fn show_cancelled(
        current: &mut Option<Box<Prepared>>,
        window: &SortSizeWindow,
        unpublished: bool,
        generation: sortsize_core::inventory_contract::GenerationId,
    ) {
        window.set_busy(false);
        window.set_cancelling(false);
        let label = if unpublished {
            tr!("Cancelled: partial inventory, not published")
        } else {
            service::Phase::Cancelled.label()
        };
        let status = format!("{label} · G{}", generation.0);
        if let Some(current) = current {
            if current.busy {
                current.status = status.clone();
            }
            current.busy = false;
        }
        window.set_status(status.clone().into());
        window.set_top_note(status.clone().into());
        window.set_treemap_note(status.into());
        if current.is_none() {
            window.set_empty_message(label.into());
        }
    }

    fn new() -> std::io::Result<Self> {
        Ok(Self {
            duplicate: Default::default(),
            empty: Default::default(),
            disconnect: Default::default(),
            server: Default::default(),
            #[cfg(any(windows, target_os = "linux"))]
            windows_owners: Default::default(),
            #[cfg(any(windows, target_os = "linux"))]
            owner_live_source: false,
            service: Service::start()?,
            owner_task: None,
            owner_request: None,
            owner_names: Arc::default(),
            owner_names_tag: None,
            epoch: 0,
            scan_metrics: None,
            scan_progress_poll: None,
            requested: None,
            source_selection: None,
            collection_options: Default::default(),
            explicit_root: false,
            interrupted_source: None,
            priority_folder: false,
            active_service: false,
            failed: false,
            current: None,
            pending: None,
            search_current: None,
            search_pending: None,
            search_cancel: Default::default(),
            search_id: 0,
            cache_id: 0,
            cache_wait: None,
            cache_autosave_revision: 0,
            cache_comparing: false,
            cache_cancel: Default::default(),
            cache_current: None,
            preview_current: None,
            gesture: false,
            release: false,
            tree_focused: None,
            tree_menu_target: None,
            pending_folder: None,
            tree_quiet_until: None,
            deferred_tree: None,
            selected: None,
            top_selected: None,
            units: Default::default(),
            presentation: Default::default(),
            max_apply_ms: 0.,
            last_apply_ms: 0.,
            applied: 0,
            #[cfg(feature = "benchmarks")]
            native_probe_phases: false,
            #[cfg(feature = "benchmarks")]
            navigation_applied_at: None,
            #[cfg(feature = "benchmarks")]
            duplicate_applied_at: None,
            #[cfg(feature = "benchmarks")]
            search_applied_at: None,
            #[cfg(feature = "benchmarks")]
            search_apply_ms: 0.,
        })
    }
    fn epoch(&mut self) -> u64 {
        self.epoch = self.epoch.checked_add(1).expect("view epoch exhausted");
        self.epoch
    }
    pub(crate) fn poll_selection(&mut self, window: &SortSizeWindow, controller: &mut Controller) {
        if window.get_choosing_folder() || !window.get_scan_enabled() {
            return;
        }
        if let Some((root, preflight)) = controller.pending_auto.take() {
            self.start(
                window,
                controller,
                Start {
                    root,
                    explicit_root: false,
                    observation: crate::workshop::service::size_observation(),
                    preflight,
                    options: sortsize_store::scheduler::CollectionOptions {
                        allocation: window.get_collect_allocation(),
                        owners: window.get_collect_owner(),
                    },
                },
            );
        } else {
            self.poll_folder_duplicates(window, controller);
        }
    }

    fn start(&mut self, window: &SortSizeWindow, controller: &mut Controller, start: Start) {
        self.start_with_mode(
            window,
            controller,
            start,
            false,
            service::StartPurpose::Regular,
        );
    }

    #[allow(unused_mut)]
    fn start_with_mode(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        mut start: Start,
        fast_local: bool,
        purpose: service::StartPurpose,
    ) {
        self.remember_duplicate_inventory();
        if controller.registry.is_disconnected(&start.root)
            || self.service.source_disconnected(&start.root)
        {
            window.set_errors(tr!("This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning.").into());
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let logical = start.root.clone();
            match crate::source_registry::scan_root(&logical) {
                Ok(root) => {
                    self.service.bind_source(logical, root.clone());
                    start.root = root;
                }
                Err(_) => {
                    window.set_errors(smb_server::native_mount_unavailable().into());
                    return;
                }
            }
        }
        if window.get_choosing_folder() || !window.get_scan_enabled() {
            return;
        }
        if self.service.smb_snapshot().is_some() {
            self.service.leave_smb_batch();
        }
        window.set_smb_server_context(false);
        window.set_smb_server_visible(false);
        let native_source = controller.sources.current.as_ref().and_then(|prepared| {
            crate::auto_analysis::LastSelection::local(start.root.clone(), &prepared.discovery)
        });
        if window.get_busy()
            && self.requested.as_ref() == Some(&start.root)
            && self.source_selection == native_source
        {
            return;
        }
        // Keep the durable reader's historical replacement policy. A
        // direct page belongs only to its original acquisition until restored
        // by the service after a rejected or cancelled replacement.
        let preserve_inventory = !cfg!(windows)
            && self.requested.as_ref() == Some(&start.root)
            && self
                .current
                .as_ref()
                .is_some_and(|page| page._direct_publication.is_none())
            && self.source_selection == native_source;
        let browsing = matches!(purpose, service::StartPurpose::BrowseInterrupted(_));
        self.priority_folder = matches!(purpose, service::StartPurpose::PriorityDuplicates);
        if self.priority_folder {
            self.interrupted_source = self
                .requested
                .as_ref()
                .filter(|root| start.root != **root && start.root.starts_with(root))
                .map(|root| InterruptedSource {
                    root: root.clone(),
                    selection: self.source_selection.clone(),
                    options: self.collection_options,
                    explicit_root: self.explicit_root,
                });
        } else if !browsing
            && self
                .interrupted_source
                .as_ref()
                .is_some_and(|source| source.root == start.root)
        {
            self.interrupted_source = None;
        }
        self.source_selection = native_source;
        self.collection_options = start.options;
        self.explicit_root = start.explicit_root;
        #[cfg(any(windows, target_os = "linux"))]
        {
            self.owner_live_source = true;
        }
        // The shared source presenter must track the Atelier intention too;
        // otherwise a later volume refresh can restore the previous source marker.
        controller.scan_root = Some(start.root.clone());
        #[cfg(any(target_os = "linux", windows))]
        {
            controller.scan_volume = controller.sources.current.as_ref().and_then(|p| {
                p.discovery.volumes.iter().find_map(|v| {
                    v.mounts
                        .iter()
                        .find(|m| m.path == start.root)
                        .map(|m| (v.identity.clone(), m.clone()))
                })
            });
            controller.show_sources(window);
        }
        window.set_scan_source(short_label(&start.root).into());
        self.close_search(window);
        self.clear_preview(window, 0);
        self.cache_cancel.cancel();
        self.cache_id += 1;
        self.cache_current = None;
        self.cache_wait = None;
        self.cache_comparing = false;
        window.set_diff_rows(ModelRc::default());
        window.set_diff_summary("".into());
        self.requested = Some(start.root.clone());
        // Automatic and normal starts must display the mode they requested.
        window.set_scan_fast_mode(fast_local);
        self.active_service = false;
        self.failed = false;
        let epoch = self.epoch();
        if let Some(trace) = &self.service.trace {
            trace.start_source(epoch);
        }
        let mut lane = self.service.lane.lock().unwrap();
        lane.detail_sort = controller.preferences.detail_sort;
        lane.detail_anchor = None;
        self.scan_metrics = scan_p1::enabled().then(|| {
            scan_p1::UiMetrics::new(
                epoch,
                self.current
                    .as_ref()
                    .filter(|_| preserve_inventory)
                    .map(|page| {
                        let context = page.frame.stamp.context;
                        (context.inventory, context.generation)
                    }),
            )
        });
        lane.start = Some((epoch, start));
        lane.fast_local = fast_local;
        lane.start_purpose = purpose;
        lane.scan_progress = None;
        lane.cancel_publication();
        lane.publication_cancel = Default::default();
        lane.source_epoch = epoch;
        lane.view_epoch = epoch;
        lane.search_current = None;
        lane.writer_running = !browsing;
        lane.navigation = None;
        lane.top_scope = None;
        lane.desired_folder = None;
        lane.desired_relative = None;
        lane.acquisition_control = None;
        lane.cancel = false;
        lane.cancel_requested_epoch = None;
        lane.cancelled_unpublished = None;
        lane.retry = false;
        lane.phase = None;
        if preserve_inventory {
            if let Some(current) = &mut self.current {
                current.epoch = epoch;
                current.busy = true;
            }
        } else {
            self.current = None;
        }
        self.pending = None;
        self.tree_focused = None;
        self.pending_folder = None;
        self.tree_quiet_until = None;
        self.deferred_tree = None;
        self.selected = None;
        self.top_selected = None;
        if !window.get_top_scope().is_empty() {
            Self::clear_top_results(window, String::new());
        }
        self.gesture = false;
        self.release = false;
        if !preserve_inventory {
            window.set_details(ModelRc::default());
            window.set_rows(ModelRc::default());
            window.set_top_rows(ModelRc::default());
            window.set_tiles(ModelRc::default());
            window.set_top_selected(-1);
            window.set_top_path("".into());
            window.set_entry_path("".into());
            window.set_current_path("".into());
            window.set_current_folder_summary("".into());
            window.set_selection_summary("".into());
            window.set_workshop_scope_detail("".into());
            window.set_workshop_selected(-1);
            window.set_inspected_node(-1);
            window.set_top_retained(false);
            window.set_has_parent(false);
            window.set_has_previous_detail_page(false);
            window.set_has_next_detail_page(false);
            window.set_has_previous_page(false);
            window.set_has_next_page(false);
            window.set_can_load_details(false);
            window.set_detail_page("".into());
            window.set_tree_page("".into());
            window.set_view_warning("".into());
            window.set_top_note(tr!("Preparing inventory…").into());
            window.set_treemap_note(tr!("Preparing inventory…").into());
            window.set_empty_message(tr!("Preparing inventory…").into());
            window.set_owner_status("".into());
        }
        window.set_cancelling(false);
        window.set_busy(true);
        window.set_errors("".into());
        window.set_status(
            if browsing {
                tr!("Preparing saved measures…")
            } else {
                tr!("Preparing inventory…")
            }
            .into(),
        );
        window.set_inventory_warning("".into());
        if !preserve_inventory {
            window.set_summary("—".into());
        }
    }
    fn frozen_unpublished_view(&self) -> bool {
        let lane = self.service.lane.lock().unwrap();
        (lane.cancel_requested_epoch == Some(lane.source_epoch)
            || lane.cancelled_unpublished == Some(lane.source_epoch))
            && !lane.reader_active
            && lane.direct_view != Some(lane.source_epoch)
    }
    fn navigate(&mut self, action: Navigate) {
        self.try_navigate(action);
    }
    fn interrupted_parent(&self) -> Option<(&Path, &Path)> {
        if !self.priority_folder {
            return None;
        }
        let source = &self.interrupted_source.as_ref()?.root;
        let relative = self
            .requested
            .as_ref()?
            .parent()?
            .strip_prefix(source)
            .ok()?;
        Some((source, relative))
    }

    fn browse_interrupted_parent(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
    ) -> bool {
        if window.get_search_visible()
            || window.get_busy()
            || self.current.as_ref().is_none_or(|page| page.parent)
        {
            return false;
        }
        let Some((root, relative)) = self.interrupted_parent() else {
            return false;
        };
        let relative = relative.to_owned();
        let root = root.to_owned();
        self.start_with_mode(
            window,
            controller,
            Start {
                root,
                explicit_root: false,
                preflight: None,
                observation: service::size_observation(),
                options: Default::default(),
            },
            false,
            service::StartPurpose::BrowseInterrupted(relative),
        );
        true
    }
    fn try_navigate(&mut self, action: Navigate) -> bool {
        // A disconnected view has no navigation intention. A late keyboard or
        // sorting callback must not attach its retained SQLite reader again.
        if self.current.is_none() && self.requested.is_none() || self.frozen_unpublished_view() {
            return false;
        }
        if matches!(&action, Navigate::Parent | Navigate::Reveal(_)) {
            self.pending_folder = None;
        }
        self.cache_cancel.cancel();
        self.cache_id += 1;
        self.cache_wait = None;
        self.cache_comparing = false;
        self.search_cancel.cancel();
        self.search_id += 1;
        self.search_current = None;
        self.search_pending = None;
        let mut lane = self.service.lane.lock().unwrap();
        lane.search = None;
        lane.search_current = None;
        drop(lane);
        let epoch = self.epoch();
        self.pending = None;
        let mut lane = self.service.lane.lock().unwrap();
        lane.view_epoch = epoch;
        match &action {
            Navigate::Top100(scope) => lane.top_scope = scope.clone(),
            Navigate::Open(key) => {
                lane.desired_folder = Some(*key);
                lane.desired_relative = None;
            }
            Navigate::Parent | Navigate::Reveal(_) => {
                lane.desired_folder = None;
                lane.desired_relative = None;
            }
            _ => {}
        }
        if let Navigate::Open(key) = &action
            && let Some((source, generation, control)) = &lane.acquisition_control
            && *source == lane.source_epoch
            && *generation == key.generation
            && (lane.smb_view.is_none() || lane.smb_view == lane.smb_acquisition_root)
        {
            control.navigate(key.entry);
        }
        lane.navigation = Some((epoch, action));
        true
    }
    /// UI-only acknowledgement. The worker owns all database and filesystem work.
    fn request_folder(&mut self, window: &SortSizeWindow, key: EntryKey, path: String) {
        // A cancelled candidate has a dated snapshot, not a certified reader.
        // Keep that snapshot visible instead of clearing it for a request the
        // frozen writer cannot serve. An existing G0 remains navigable.
        if !self.try_navigate(Navigate::Open(key)) {
            return;
        }
        self.pending_folder = Some(key);
        self.selected = None;
        self.owner_request = None;
        window.set_workshop_token(-1);
        window.set_workshop_selected(-1);
        window.set_inspected_node(-1);
        window.set_entry_path("".into());
        window.set_details(ModelRc::default());
        window.set_tiles(ModelRc::default());
        window.set_detail_scroll_y(0.);
        window.set_has_previous_detail_page(false);
        window.set_has_next_detail_page(false);
        window.set_has_previous_page(false);
        window.set_has_next_page(false);
        window.set_current_path(path.clone().into());
        window.set_current_folder_summary(tr!("Preparing inventory…").into());
        window.set_selection_summary(path.into());
        window.set_has_parent(key.entry.0 != 0);
        window.set_can_load_details(false);
        window.set_detail_page("".into());
        window.set_owner_status("".into());
        window.set_empty_message(tr!("Preparing inventory…").into());
        window.set_treemap_note(tr!("Preparing inventory…").into());
        window.set_status(tr!("Preparing inventory…").into());
        if let Some(current) = &mut self.current {
            let model = window.get_rows();
            for (index, (row, target)) in current
                .folders
                .iter_mut()
                .zip(&current.folder_keys)
                .enumerate()
            {
                row.selected = matches!(target, folder_tree::TreeTarget::Folder { key: folder, .. } if *folder == key);
                row.focused = self.tree_focused.as_ref() == Some(target);
                if let Some(model) = model.as_any().downcast_ref::<VecModel<TreeRow>>() {
                    model.set_row_data(index, row.clone());
                }
            }
        }
    }
    fn detail_row(
        &self,
        token: i32,
        index: i32,
    ) -> Option<&sortsize_store::inventory::queries::Row> {
        self.current
            .as_ref()
            .filter(|page| page.epoch == self.epoch && page.token == token)?
            .frame
            .rows
            .get(usize::try_from(index).ok()?)
    }
    fn select(&mut self, window: &SortSizeWindow, token: i32, index: i32, open: bool) {
        let Some(row) = self.detail_row(token, index) else {
            return;
        };
        let key = row.key;
        let path = label(&row.path);
        let directory = row.kind == sortsize_store::inventory::Kind::Directory;
        self.selected = Some(key);
        window.set_entry_path(path.clone().into());
        window.set_inspected_node(index);
        window.set_workshop_selected(index);
        if open && directory {
            self.close_search(window);
            self.request_folder(window, key, path);
        }
    }
    fn tree_action(&mut self, window: &SortSizeWindow, index: i32, toggle: bool) {
        let Some(target) = self
            .current
            .as_ref()
            .and_then(|p| {
                usize::try_from(index)
                    .ok()
                    .and_then(|i| p.folder_keys.get(i))
            })
            .cloned()
        else {
            return;
        };
        if toggle
            && self.current.as_ref().is_some_and(|p| {
                p.folders.get(index as usize).is_some_and(|r| !r.expanded)
                    && p.folders.iter().filter(|r| r.expanded).count() >= folder_tree::MAX_BRANCHES
            })
        {
            window.set_status(
                tr!("Folder tree display limit reached. Collapse a branch to show more folders.")
                    .into(),
            );
            return;
        }
        self.tree_focused = Some(target.clone());
        self.tree_quiet_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(400));
        match target {
            folder_tree::TreeTarget::More { parent } => self.navigate(Navigate::MoreTree(parent)),
            folder_tree::TreeTarget::Previous { parent } => {
                self.navigate(Navigate::PreviousTree(parent))
            }
            folder_tree::TreeTarget::Limit => {
                window.set_status(
                    tr!(
                        "Folder tree display limit reached. Collapse a branch to show more folders."
                    )
                    .into(),
                );
            }
            folder_tree::TreeTarget::Folder { key, .. } if toggle => {
                self.navigate(Navigate::ToggleTree(key))
            }
            folder_tree::TreeTarget::Folder { key, .. } => {
                let path = self.current.as_ref().unwrap().folders[index as usize]
                    .path
                    .to_string();
                self.close_search(window);
                self.request_folder(window, key, path);
            }
        }
    }
    fn tree_focus_index(&self, page: &Prepared) -> usize {
        self.tree_focused
            .as_ref()
            .and_then(|k| page.folder_keys.iter().position(|r| r == k))
            .or_else(|| page.folders.iter().position(|r| r.selected))
            .unwrap_or(0)
    }
    fn prepare_tree_menu(&mut self, index: i32) -> Option<usize> {
        self.tree_menu_target = None;
        let page = self.current.as_ref()?;
        let index = usize::try_from(index).unwrap_or_else(|_| self.tree_focus_index(page));
        let folder_tree::TreeTarget::Folder { key, path, .. } = page.folder_keys.get(index)? else {
            return None;
        };
        self.tree_menu_target = Some(TreeMenuTarget {
            source_epoch: self.service.lane.lock().unwrap().source_epoch,
            source: page.frame.stamp.context.source,
            key: *key,
            path: path.clone(),
        });
        Some(index)
    }
    fn take_tree_menu_target(&mut self, window: &SortSizeWindow) -> Option<TreeMenuTarget> {
        let source_epoch = self.service.lane.lock().unwrap().source_epoch;
        let target = self.tree_menu_target.take().filter(|target| {
            self.current.as_ref().is_some_and(|page| {
                let context = page.frame.stamp.context;
                target.source_epoch == source_epoch
                    && context.source == target.source
                    && context.inventory == target.key.inventory
                    && context.generation == target.key.generation
                    && page.folder_keys.iter().any(|row| {
                        matches!(row, folder_tree::TreeTarget::Folder { key, path, .. }
                            if *key == target.key && *path == target.path)
                    })
            })
        });
        if target.is_none() {
            window.set_status(
                tr!("The clicked folder can no longer be verified. Open its menu again.").into(),
            );
        }
        target
    }

    fn switch_top_scope(
        &mut self,
        window: &SortSizeWindow,
        scope: Option<service::TopScope>,
    ) -> bool {
        let path = scope
            .as_ref()
            .map_or_else(String::new, |scope| label(&scope.path));
        if !self.try_navigate(Navigate::Top100(scope)) {
            return false;
        }
        self.top_selected = None;
        Self::clear_top_results(window, path);
        true
    }

    fn clear_top_results(window: &SortSizeWindow, path: String) {
        window.set_top_rows(ModelRc::default());
        window.set_top_selected(-1);
        window.set_top_retained(false);
        window.set_top_path("".into());
        window.set_top_scroll_y(0.);
        window.set_top_scope(path.into());
        window.set_top_loading(true);
        window.set_top_note(tr!("Preparing inventory…").into());
    }

    fn tree_key(&mut self, window: &SortSizeWindow, action: i32) {
        let Some(page) = self.current.as_ref() else {
            return;
        };
        let index = self.tree_focus_index(page);
        if page.folders.is_empty() {
            return;
        }
        let row = &page.folders[index];
        let next = match action {
            0 => Some(index.saturating_sub(1)),
            1 => Some((index + 1).min(page.folders.len() - 1)),
            5 => Some(0),
            6 => Some(page.folders.len() - 1),
            2 if row.expanded => { self.tree_action(window, index as i32, true); return; }
            2 => match &page.folder_keys[index] {
                folder_tree::TreeTarget::Folder { parent: Some(parent), .. } => page.folder_keys.iter().position(|t| matches!(t, folder_tree::TreeTarget::Folder { key, .. } if key.entry == *parent)),
                _ => None,
            },
            3 if row.expandable && !row.expanded => { self.tree_action(window, index as i32, true); return; }
            3 if row.expanded && page.folders.get(index + 1).is_some_and(|r| r.depth > row.depth) => Some(index + 1),
            4 => Some(index),
            _ => None,
        };
        if let Some(next) = next {
            let depth = page.folders[next].depth;
            self.tree_action(window, next as i32, false);
            let label_start = depth as f32 * 16. + 50.;
            let horizontal = -window.get_sidebar_folder_scroll_x();
            let width = (window.get_actual_sidebar_width() - 34.).max(80.);
            if label_start < horizontal {
                window.set_sidebar_folder_scroll_x(-label_start);
            } else if label_start + 60. > horizontal + width {
                window.set_sidebar_folder_scroll_x(-(label_start + 60. - width));
            }
            let height = window.get_tree_row_height();
            let top = next as f32 * height;
            let scroll = -window.get_sidebar_folder_scroll();
            let viewport = window.get_tree_list_height();
            if top < scroll {
                window.set_sidebar_folder_scroll(-top);
            } else if top + height > scroll + viewport {
                window.set_sidebar_folder_scroll(-(top + height - viewport).max(0.));
            }
        }
    }
    fn top_row(&self, token: i32, index: i32) -> Option<&sortsize_store::inventory::queries::Row> {
        self.current
            .as_ref()
            .filter(|page| page.epoch == self.epoch && page.token == token)?
            .top
            .rows
            .get(usize::try_from(index).ok()?)
    }
    fn select_top(&mut self, window: &SortSizeWindow, index: i32) {
        let Some(row) = self.top_row(window.get_workshop_token(), index) else {
            return;
        };
        let path = label(&row.path);
        self.top_selected = Some(row.key);
        window.set_top_path(path.into());
        window.set_top_selected(index);
        window.set_top_retained(true);
    }
    fn apply(&mut self, window: &SortSizeWindow, mut page: Box<Prepared>) {
        let trace = self.service.trace.clone();
        let _binding = trace
            .as_ref()
            .map(|trace| trace.ui_sql.bind_current_thread());
        let _trace = sortsize_store::scan_trace::phase("ui_apply");
        let raw_changed = trace.is_some()
            && self
                .current
                .as_ref()
                .is_none_or(|old| old.root_measure != page.root_measure);
        self.pending_folder = None;
        // Numeric publications must not move a name under the next click.
        // Only the tree waits for this short interaction pause; the requested
        // folder's table and map continue applying immediately.
        if self
            .tree_quiet_until
            .is_some_and(|until| std::time::Instant::now() < until)
            && let Some(old) = self.current.as_mut()
            && old.frame.stamp.context.generation == page.frame.stamp.context.generation
            && old.folder_keys != page.folder_keys
            && expanded_folders(&old.folders, &old.folder_keys)
                == expanded_folders(&page.folders, &page.folder_keys)
        {
            self.deferred_tree = Some(DeferredTree {
                rows: std::mem::replace(&mut page.folders, old.folders.clone()),
                keys: std::mem::replace(&mut page.folder_keys, old.folder_keys.clone()),
                memory: page._folder_memory.take(),
            });
            page._folder_memory = old._folder_memory.take();
        } else {
            self.deferred_tree = None;
        }
        // Worker pages can arrive after an error, notably when a paused live
        // session is reformatted or navigated. Reader refusals leave a truly
        // running independent writer visible. Retry/new source clears failure.
        let (failed, direct, writer_running) = {
            let lane = self.service.lane.lock().unwrap();
            (
                self.failed && !lane.writer_running,
                lane.direct_view == Some(lane.source_epoch) && page.epoch == lane.view_epoch,
                lane.writer_running
                    && lane.reader_active
                    && page.epoch == lane.view_epoch
                    && lane.phase.is_some_and(|(epoch, phase, generation)| {
                        epoch == lane.source_epoch
                            && page.frame.stamp.context.generation != generation
                            && matches!(
                                phase,
                                service::Phase::Preparing
                                    | service::Phase::Acquiring
                                    | service::Phase::Finalizing
                                    | service::Phase::Certifying
                                    | service::Phase::Reopening
                            )
                    }),
            )
        };
        if failed {
            page.busy = false;
            page.status = tr!("Could not start scan.").into();
        } else if writer_running {
            // A certified G0 page can arrive while its replacement G1 is
            // acquiring. Its dated rows remain usable, and the current writer
            // must remain visible until the lane reaches a terminal phase.
            page.busy = true;
        }
        let begin = std::time::Instant::now();
        let anchor = self
            .current
            .as_ref()
            .filter(|p| p.previous || window.get_detail_scroll_y() < -0.01)
            .and_then(|p| {
                p.frame
                    .rows
                    .get(((-window.get_detail_scroll_y()).max(0.) / 34.) as usize)
            })
            .map(|r| r.key);
        let top_anchor = self
            .current
            .as_ref()
            .and_then(|p| {
                p.top
                    .rows
                    .get(((-window.get_top_scroll_y()).max(0.) / 34.) as usize)
            })
            .map(|r| r.key);
        let detail_offset = (-window.get_detail_scroll_y()).max(0.) % 34.;
        let top_offset = (-window.get_top_scroll_y()).max(0.) % 34.;
        window.set_workshop_token(page.token);
        // Keep the model and delegates alive across data publications.
        let model = window.get_details();
        if let Some(model) = model.as_any().downcast_ref::<VecModel<DetailRow>>() {
            for (index, row) in page.details.iter().enumerate() {
                if index >= model.row_count() {
                    model.push(row.clone());
                } else if model.row_data(index).as_ref() != Some(row) {
                    model.set_row_data(index, row.clone());
                }
            }
            while model.row_count() > page.details.len() {
                model.remove(model.row_count() - 1);
            }
        } else {
            window.set_details(Rc::new(VecModel::from(page.details.clone())).into());
        }
        let tree_height = window.get_tree_row_height();
        let tree_scroll = window.get_sidebar_folder_scroll();
        let tree_anchor = self
            .current
            .as_ref()
            .and_then(|p| {
                p.folder_keys
                    .get(((-tree_scroll).max(0.) / tree_height) as usize)
            })
            .cloned();
        let tree_offset = (-tree_scroll).max(0.) % tree_height;
        if self
            .tree_focused
            .as_ref()
            .is_none_or(|focused| !page.folder_keys.contains(focused))
        {
            self.tree_focused = page
                .folders
                .iter()
                .position(|r| r.selected)
                .and_then(|i| page.folder_keys.get(i))
                .cloned();
        }
        for (row, target) in page.folders.iter_mut().zip(&page.folder_keys) {
            row.focused = self.tree_focused.as_ref() == Some(target);
        }
        window.set_tree_max_depth(page.folders.iter().map(|r| r.depth).max().unwrap_or(0));
        let model = window.get_rows();
        if let Some(model) = model.as_any().downcast_ref::<VecModel<TreeRow>>() {
            for (index, row) in page.folders.iter().enumerate() {
                if index >= model.row_count() {
                    model.push(row.clone());
                } else if model.row_data(index).as_ref() != Some(row) {
                    model.set_row_data(index, row.clone());
                }
            }
            while model.row_count() > page.folders.len() {
                model.remove(model.row_count() - 1);
            }
        } else {
            window.set_rows(Rc::new(VecModel::from(page.folders.clone())).into());
        }
        window.set_sidebar_folder_scroll(
            tree_anchor
                .and_then(|key| page.folder_keys.iter().position(|r| *r == key))
                .map_or(tree_scroll, |index| {
                    -(index as f32 * tree_height + tree_offset)
                }),
        );

        window.set_top_rows(Rc::new(VecModel::from(page.top_rows.clone())).into());
        // A progress/status publication need not recreate unchanged rectangles.
        // Keep Slint's delegates and focus while the worker coalesces measures.
        if window.get_tiles().row_count() != page.tiles.len()
            || self
                .current
                .as_ref()
                .is_none_or(|old| old.tiles != page.tiles)
        {
            window.set_tiles(Rc::new(VecModel::from(page.tiles.clone())).into());
        }
        window.set_current_path(page.path.clone().into());
        window.set_current_folder_summary(page.current_folder_summary.clone().into());
        window.set_selection_summary(page.path.clone().into());
        window.set_workshop_scope_detail(page.scope_detail.clone().into());
        // A navigation/progress page is not an acknowledgement of Cancel.
        // The acquisition lane owns that request until a terminal phase.
        if !window.get_cancelling() {
            window.set_status(page.status.clone().into());
        }
        window.set_summary(page.summary.clone().into());
        window.set_busy(page.busy);
        window.set_sort_order(page.sort_index);
        window.set_allocation_sort_available(true);
        window.set_owner_sort_available(true);
        #[cfg(any(windows, target_os = "linux"))]
        if window.get_targeted_owner() {
            window.set_owner_sort_available(
                page.owners
                    .iter()
                    .any(|o| matches!(o.id, sortsize_core::MetadataValue::Known { .. })),
            );
        }
        window.set_has_parent(page.parent || self.interrupted_parent().is_some());
        window.set_has_previous_detail_page(page.previous);
        window.set_has_next_detail_page(page.frame.more);
        window.set_has_previous_page(false);
        window.set_has_next_page(false);
        window.set_can_load_details(false);
        window.set_detail_page("".into());
        window.set_top_scope(
            page.top_scope
                .as_deref()
                .map_or_else(String::new, |path| label(path))
                .into(),
        );
        window.set_top_loading(page.top_scope.is_some() && page.top.provisional && page.busy);
        window.set_top_note(if failed {
            tr!("Could not start scan.").into()
        } else if page.top_scope.is_some() {
            if page.top.provisional && page.busy {
                format!("{} · {}", tr!("Scan in progress…"), tr!("Partial")).into()
            } else if page.top.provisional
                || page.top.coverage != ScopeCoverage::Complete
                || page.top.byte_limited
            {
                tr!("Partial").into()
            } else {
                "".into()
            }
        } else if page.top.provisional && page.busy {
            tr!("Scan in progress…").into()
        } else {
            page.status.clone().into()
        });
        window.set_treemap_note(if failed {
            tr!("Could not start scan.").into()
        } else if page.measure_ready || !page.frame.rows.is_empty() || !page.tiles.is_empty() {
            if page.frame.provisional || page.frame.coverage != ScopeCoverage::Complete {
                format!(
                    "{} · {}",
                    tr!("Logical size · current page"),
                    tr!("Partial")
                )
                .into()
            } else {
                tr!("Logical size · current page").into()
            }
        } else {
            tr!("Preparing inventory…").into()
        });
        window.set_view_warning(if direct && page.sort_index < 0 {
            if page.frame.provisional || page.frame.coverage != ScopeCoverage::Complete {
                format!("{} · {}", tr!("Observation order"), tr!("Partial")).into()
            } else {
                tr!("Observation order").into()
            }
        } else if page.frame.provisional || page.frame.coverage != ScopeCoverage::Complete {
            tr!("Partial").into()
        } else {
            "".into()
        });
        window.set_top_selected(
            self.top_selected
                .and_then(|key| page.top.rows.iter().position(|r| r.key == key))
                .map_or(-1, |n| n as i32),
        );
        window.set_workshop_selected(
            self.selected
                .and_then(|key| page.frame.rows.iter().position(|r| r.key == key))
                .map_or(-1, |n| n as i32),
        );
        window.set_inspected_node(window.get_workshop_selected());
        window.set_empty_message(if page.busy {
            tr!("Scan in progress…").into()
        } else {
            tr!("No file observed in this scope.").into()
        });
        let previous_scroll = if self.current.as_ref().is_some_and(|old| {
            old.parent_entry == page.parent_entry
                && old.frame.stamp.context.generation == page.frame.stamp.context.generation
        }) {
            window.get_detail_scroll_y()
        } else {
            0.
        };
        window.set_detail_scroll_y(
            anchor
                .and_then(|key| page.frame.rows.iter().position(|r| r.key == key))
                .map_or(previous_scroll, |i| -(i as f32 * 34. + detail_offset)),
        );
        window.set_top_scroll_y(
            top_anchor
                .and_then(|key| page.top.rows.iter().position(|r| r.key == key))
                .map_or(0., |i| -(i as f32 * 34. + top_offset)),
        );
        if let Some(trace) = &trace {
            let models_changed = self.current.as_ref().is_none_or(|old| {
                old.details != page.details
                    || old.folders != page.folders
                    || old.top_rows != page.top_rows
                    || old.tiles != page.tiles
            });
            trace.applied(&page, raw_changed, models_changed);
            trace.visible(window);
        }
        self.current = Some(page);
        let stale = self
            .cache_current
            .as_ref()
            .is_none_or(|p| p.epoch != self.epoch || p.presentation != self.presentation);
        if window.get_cache_visible()
            && stale
            && !self.cache_comparing
            && self.cache_wait != Some(self.epoch)
        {
            self.refresh_cache(window);
        }
        self.last_apply_ms = begin.elapsed().as_secs_f64() * 1000.;
        self.max_apply_ms = self.max_apply_ms.max(self.last_apply_ms);
        self.applied += 1;
        if let Some(metrics) = &mut self.scan_metrics
            && let Some(page) = &self.current
        {
            let generation = page.frame.stamp.context.generation;
            let ready = !page.busy
                && page.measure_ready
                && self.service.lane.lock().unwrap().phase.is_some_and(
                    |(source_epoch, phase, selected)| {
                        source_epoch == metrics.source_epoch
                            && matches!(
                                phase,
                                service::Phase::Ready
                                    | service::Phase::PreviewReady
                                    | service::Phase::Saving
                            )
                            && selected == generation
                    },
                );
            let apply = begin.elapsed();
            if metrics.applied(page, ready, apply) {
                self.scan_metrics = None;
            }
        }
        #[cfg(feature = "benchmarks")]
        {
            self.navigation_applied_at = Some(std::time::Instant::now());
            if self.native_probe_phases && !window.get_search_visible() {
                i_slint_backend_winit::presentation_probe::phase(2);
            }
        }
    }
    pub(crate) fn poll(&mut self, window: &SortSizeWindow, controller: &Controller) {
        let trace = self.service.trace.clone();
        let _binding = trace
            .as_ref()
            .map(|trace| trace.ui_sql.bind_current_thread());
        let _trace = sortsize_store::scan_trace::phase("ui_poll");
        let _visible = trace
            .as_ref()
            .map(|trace| scan_p1::progress_trace::UiPoll(trace, window));
        if let Some(trace) = &trace {
            trace.ui_poll();
        }
        if !self.gesture
            && self
                .tree_quiet_until
                .is_none_or(|until| std::time::Instant::now() >= until)
            && let Some(DeferredTree {
                rows: mut folders,
                keys,
                memory,
            }) = self.deferred_tree.take()
            && let Some(current) = self.current.as_mut()
        {
            let height = window.get_tree_row_height();
            let scroll = window.get_sidebar_folder_scroll();
            let anchor = current
                .folder_keys
                .get(((-scroll).max(0.) / height) as usize)
                .cloned();
            for (row, target) in folders.iter_mut().zip(&keys) {
                row.focused = self.tree_focused.as_ref() == Some(target);
                let selected = self
                    .pending_folder
                    .map_or(current.parent_entry, |key| key.entry);
                row.selected = matches!(target, folder_tree::TreeTarget::Folder { key, .. } if key.entry == selected);
            }
            let model = window.get_rows();
            if let Some(model) = model.as_any().downcast_ref::<VecModel<TreeRow>>() {
                for (index, row) in folders.iter().enumerate() {
                    if index < model.row_count() {
                        model.set_row_data(index, row.clone());
                    } else {
                        model.push(row.clone());
                    }
                }
                while model.row_count() > folders.len() {
                    model.remove(model.row_count() - 1);
                }
            }
            window.set_sidebar_folder_scroll(
                anchor
                    .and_then(|a| keys.iter().position(|k| *k == a))
                    .map_or(scroll, |i| {
                        -(i as f32 * height + (-scroll).max(0.) % height)
                    }),
            );
            current.folders = folders;
            current.folder_keys = keys;
            current._folder_memory = memory;
        }
        let _scope = controller.presentation.enter();
        let changed = self.units != controller.preferences.units
            || self.presentation != controller.presentation;
        self.units = controller.preferences.units;
        self.presentation = controller.presentation;
        {
            let mut lane = self.service.lane.lock().unwrap();
            lane.presentation = self.presentation;
            lane.units = self.units;
            lane.memory = controller.preferences.memory;
            lane.detail_sort = controller.preferences.detail_sort;
            lane.detail_anchor = self
                .current
                .as_ref()
                .filter(|p| p.previous || window.get_detail_scroll_y() < -0.01)
                .and_then(|p| {
                    let index = ((-window.get_detail_scroll_y()).max(0.) / 34.) as usize;
                    p.frame.rows.get(index).map(|r| (p.parent_entry, r.key))
                });
        }
        if changed && matches!(window.get_preview_state(), 1 | 2) {
            // Reformat waiting text locally while the service owns the check.
            // A language change must not enqueue another catalogue/open request.
            window.set_cache_status(tr!("Preparing saved measures…").into());
            window.set_status(tr!("Preparing saved measures…").into());
        }
        if changed && self.current.is_some() {
            if window.get_search_visible() {
                self.service.lane.lock().unwrap().search_refresh = true;
            } else if self.cache_comparing {
                // The service reformats both views under the same read lease,
                // without changing the source epoch or comparison page anchor.
            } else {
                self.navigate(Navigate::Refresh);
            }
        }
        self.poll_cache(window);
        self.poll_owners(window);
        if let Some(error) = cache::CLEANUP_ERROR.lock().unwrap().take() {
            window.set_errors(format!("{} {error}", tr!("Local cache cleanup failed.")).into());
        }
        if self.release {
            self.release = false;
            self.gesture = false;
        }
        if changed
            && window.get_search_visible()
            && window.get_search_busy()
            && self.search_current.is_none()
        {
            window.set_search_status(
                tr!("Waiting for the frozen inventory; cancelling search leaves the scan running.")
                    .into(),
            );
        }
        let desired_memory = search::MODES
            .iter()
            .position(|m| *m == controller.preferences.memory)
            .unwrap_or(0) as i32;
        if window.get_memory_choice() != desired_memory {
            window.set_memory_choice(desired_memory);
        }
        if changed
            && !self.failed
            && window.get_search_visible()
            && let Some(p) = &self.current
        {
            window.set_status(
                if p.busy {
                    tr!("Scan in progress…")
                } else if p.frame.coverage == ScopeCoverage::Complete {
                    tr!("Complete")
                } else {
                    tr!("Partial")
                }
                .into(),
            );
        }
        if changed && self.failed && !self.service.lane.lock().unwrap().writer_running {
            window.set_status(tr!("Could not start scan.").into());
            window.set_top_note(tr!("Could not start scan.").into());
            window.set_treemap_note(tr!("Could not start scan.").into());
        }
        self.poll_search(window);
        self.poll_empty_analysis(window, controller);
        self.poll_duplicates(window, controller, changed);
        let output = self
            .service
            .output
            .try_lock()
            .ok()
            .and_then(|mut o| o.take());
        match output {
            Some(Output::Page(page)) if page.epoch == self.epoch && page.units != self.units => {
                // A first page may have started before the display choice changed,
                // while no current page existed to request a refresh. Discard only
                // that formatted page and reread the inventory under the new units.
                self.navigate(Navigate::Refresh);
            }
            Some(Output::Page(page))
                if page.epoch == self.epoch
                    && self.pending_folder.is_none_or(|key| {
                        key.entry == page.parent_entry
                            || key.generation != page.frame.stamp.context.generation
                    }) =>
            {
                self.active_service = true;
                if let Some(trace) = &trace {
                    use scan_p1::progress_trace::{Field, now_ns};
                    trace.ui.write(&[
                        (Field::TakenNs, now_ns()),
                        (Field::TakenToken, page.token as u64),
                    ]);
                }
                self.pending = Some(page);
            }
            Some(Output::Error(epoch, error, active)) if epoch == self.epoch => {
                self.active_service = active;
                // A reader/query refusal does not stop an independent writer.
                // A failed/paused writer publishes writer_running=false first.
                let (running, direct_preview) = {
                    let lane = self.service.lane.lock().unwrap();
                    (
                        active && lane.writer_running,
                        lane.direct_view == Some(lane.source_epoch)
                            && lane.phase.is_some_and(|(_, phase, _)| {
                                matches!(phase, service::Phase::PreviewReady)
                            }),
                    )
                };
                if direct_preview {
                    // An explicit save refusal does not invalidate the already
                    // prepared tree. Keep it navigable and allow another save.
                    if let Some(current) = &mut self.current {
                        current.busy = false;
                        window.set_status(current.status.clone().into());
                    }
                    window.set_busy(false);
                    window.set_cancelling(false);
                    window.set_errors(error.into());
                    self.poll_scan_progress(window);
                    return;
                }
                self.failed = !running;
                if let Some(current) = &mut self.current {
                    current.busy = running;
                    if !running {
                        current.status = tr!("Could not start scan.").into();
                    }
                }
                // A page frozen by a pointer gesture predates this failure.
                // Applying it afterwards would reactivate a failed acquisition.
                self.pending = None;
                window.set_busy(running);
                if !running {
                    window.set_cancelling(false);
                }
                window.set_errors(error.into());
                window.set_status(tr!("Could not start scan.").into());
                window.set_top_loading(false);
                window.set_top_note(tr!("Could not start scan.").into());
                window.set_treemap_note(tr!("Could not start scan.").into());
                if self.current.is_none() {
                    window.set_empty_message(tr!("Could not start scan.").into());
                }
            }
            _ => {}
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|page| page.epoch == self.epoch && page.units != self.units)
        {
            self.navigate(Navigate::Refresh);
        }
        if !self.gesture
            && let Some(page) = self.pending.take().filter(|p| p.epoch == self.epoch)
        {
            self.apply(window, page);
            #[cfg(any(windows, target_os = "linux"))]
            self.poll_owners(window);
        }
        let restore_frozen_snapshot =
            self.pending_folder.is_some() && self.frozen_unpublished_view();
        if restore_frozen_snapshot {
            // A navigation click accepted just before Cancel may already have
            // cleared the table. Restore only the owned dated frame, without
            // asking the writer to build another projection. Restore as soon
            // as Cancel is requested, including while native jobs still drain;
            // terminal acknowledgement must not outrun this UI restoration.
            if let Some(mut page) = self.current.take() {
                page.epoch = self.epoch;
                for (row, target) in page.folders.iter_mut().zip(&page.folder_keys) {
                    row.selected = matches!(target, folder_tree::TreeTarget::Folder { key, .. } if key.entry == page.parent_entry);
                }
                self.apply(window, page);
            } else {
                self.pending_folder = None;
            }
        }
        if let Ok(lane) = self.service.lane.try_lock()
            && let Some((epoch, phase, generation)) = lane.phase
            && epoch == lane.source_epoch
        {
            if let Some(trace) = &trace {
                trace
                    .ui
                    .write(&[(scan_p1::progress_trace::Field::Phase, phase as u64 + 1)]);
            }
            if matches!(phase, service::Phase::Cancelled) {
                Self::show_cancelled(
                    &mut self.current,
                    window,
                    lane.cancelled_unpublished == Some(epoch),
                    generation,
                );
                if let Some(trace) = &trace {
                    trace.mark_for_source(epoch, &trace.cancel_applied);
                }
            } else if matches!(phase, service::Phase::Failed) && !lane.writer_running {
                // The diagnostic can precede completion of an already admitted
                // quantum. Release the visible busy state once it is drained.
                self.failed = true;
                window.set_busy(false);
                window.set_cancelling(false);
                if let Some(current) = &mut self.current {
                    current.status = phase.label().into();
                    current.busy = false;
                }
            } else if lane.cancellation_pending() && !self.failed {
                window.set_busy(true);
                window.set_cancelling(true);
                window.set_status(tr!("Cancelling…").into());
                window.set_top_note(tr!("Cancelling…").into());
                window.set_treemap_note(tr!("Cancelling…").into());
            } else if window.get_cancelling()
                && !lane.writer_running
                && lane.cancel_requested_epoch.is_none()
                && matches!(phase, service::Phase::PreviewReady | service::Phase::Ready)
            {
                // Save has its own cancellation and can leave the fast reader active.
                let status = phase.label();
                window.set_busy(false);
                window.set_cancelling(false);
                window.set_status(status.into());
                window.set_top_note(status.into());
                window.set_treemap_note(status.into());
                if let Some(current) = &mut self.current {
                    current.busy = false;
                }
            } else if self.pending_folder.is_none() && !window.get_search_visible() {
                if lane.direct_view == Some(epoch) && matches!(phase, service::Phase::Certifying) {
                    window.set_busy(true);
                }
                let view = self
                    .current
                    .as_ref()
                    .map(|page| page.status.as_str())
                    .unwrap_or_default();
                if !window.get_errors().is_empty() {
                    // A diagnostic must not prevent cancellation from clearing busy.
                } else if !view.is_empty()
                    && (matches!(phase, service::Phase::Ready | service::Phase::Saving)
                        || (matches!(phase, service::Phase::PreviewReady)
                            && lane.direct_view == Some(epoch)))
                {
                    window.set_status(view.into());
                } else if view.is_empty() {
                    window.set_status(format!("{} · G{}", phase.label(), generation.0).into());
                } else {
                    window.set_status(
                        format!("{view} · {} · G{}", phase.label(), generation.0).into(),
                    );
                }
            }
        }
        if self.current.is_none()
            && self.requested.is_none()
            && let Ok(lane) = self.service.lane.try_lock()
            && lane.cancel_requested_epoch != Some(lane.source_epoch)
        {
            // A neutral view discards pages from its previous epoch. Observe
            // an independent writer finishing without attaching those pages.
            let busy = lane.writer_running
                || lane
                    .smb_snapshot
                    .as_ref()
                    .is_some_and(|scope| scope.running);
            window.set_busy(busy);
            if !busy {
                window.set_cancelling(false);
            }
        }
        self.poll_smb_server(window, changed);
        // The request also covers the short interval before a worker publishes
        // its first phase and neutral/search views with no navigation page.
        if let Ok(lane) = self.service.lane.try_lock()
            && lane.cancel_requested_epoch == Some(lane.source_epoch)
            && !self.failed
        {
            if lane.cancellation_pending() {
                window.set_busy(true);
                window.set_cancelling(true);
                window.set_status(tr!("Cancelling…").into());
                window.set_top_note(tr!("Cancelling…").into());
                window.set_treemap_note(tr!("Cancelling…").into());
            } else {
                window.set_cancelling(false);
                window.set_busy(false);
                if let Some((epoch, service::Phase::Cancelled, generation)) = lane.phase
                    && epoch == lane.source_epoch
                {
                    Self::show_cancelled(
                        &mut self.current,
                        window,
                        lane.cancelled_unpublished == Some(epoch),
                        generation,
                    );
                } else if lane.smb_snapshot.is_none()
                    && let Some((epoch, service::Phase::Ready, generation)) = lane.phase
                    && epoch == lane.source_epoch
                {
                    let status = format!("{} · G{}", service::Phase::Ready.label(), generation.0);
                    if let Some(current) = &mut self.current {
                        if current.busy {
                            current.status = status.clone();
                        }
                        current.busy = false;
                    }
                    window.set_status(status.clone().into());
                    window.set_top_note(status.clone().into());
                    window.set_treemap_note(status.into());
                }
            }
        }
        self.poll_single_preparation(window, controller);
        self.poll_scan_progress(window);
        let detached_save_error = self
            .service
            .lane
            .lock()
            .unwrap()
            .autosave_error
            .lock()
            .unwrap()
            .take();
        if let Some(error) = detached_save_error {
            // Persistence of an older view does not change the current acquisition.
            window.set_errors(error.into());
        }
    }

    fn poll_scan_progress(&mut self, window: &SortSizeWindow) {
        if self
            .scan_progress_poll
            .is_some_and(|last| last.elapsed() < scan_p3::DISPLAY_INTERVAL)
        {
            return;
        }
        let Ok(lane) = self.service.lane.try_lock() else {
            return;
        };
        self.scan_progress_poll = Some(std::time::Instant::now());
        let phase = lane.phase.and_then(|(epoch, phase, _)| {
            (epoch == lane.source_epoch && lane.smb_snapshot.is_none()).then_some(
                if matches!(phase, service::Phase::PreviewReady)
                    && !self.displays_fast_preview(lane.fast_preview_context)
                {
                    service::Phase::Finalizing
                } else {
                    phase
                },
            )
        });
        let can_save = matches!(phase, Some(service::Phase::PreviewReady))
            && !lane.writer_running
            && lane.cache.is_none();
        let label = if lane.cancellation_pending() {
            String::new()
        } else {
            match phase {
                Some(
                    phase @ (service::Phase::Preparing
                    | service::Phase::Acquiring
                    | service::Phase::Finalizing
                    | service::Phase::Certifying
                    | service::Phase::Reopening),
                ) => lane
                    .scan_progress
                    .as_ref()
                    .filter(|progress| progress.source_epoch == lane.source_epoch)
                    .map_or_else(
                        || phase.label().to_owned(),
                        |progress| progress.label(phase),
                    ),
                Some(service::Phase::Saving) => service::Phase::Saving.label().to_owned(),
                Some(service::Phase::PreviewReady) => {
                    service::Phase::PreviewReady.label().to_owned()
                }
                _ => String::new(),
            }
        };
        drop(lane);
        if window.get_scan_progress().as_str() != label {
            window.set_scan_progress(label.into());
        }
        if window.get_scan_fast_can_save() != can_save {
            window.set_scan_fast_can_save(can_save);
        }
    }

    fn displays_fast_preview(&self, candidate: Option<AcquisitionContext>) -> bool {
        let Some((page, candidate)) = self.current.as_ref().zip(candidate) else {
            return false;
        };
        let view = page.frame.stamp.context;
        page.epoch == self.epoch
            && !page.busy
            && view.inventory == candidate.inventory
            && view.generation == candidate.generation
            && view.source == candidate.source
            && view.scan == candidate.scan
            && view.validation == candidate.validation
    }
}
fn connect_entry_menus(
    window: &SortSizeWindow,
    host: &Rc<RefCell<Host>>,
    show: impl Fn(&SortSizeWindow, PathBuf, bool, f32, f32) + Clone + 'static,
) {
    use sortsize_store::inventory::Kind;

    let weak = window.as_weak();
    window.on_workshop_entry_menu({
        let host = host.clone();
        let weak = weak.clone();
        let show = show.clone();
        move |token, index, x, y| {
            let Some(window) = weak.upgrade() else { return };
            if window.get_windows_rename_visible() {
                return;
            }
            let mut host = host.borrow_mut();
            let Some(row) = host.detail_row(token, index) else {
                return;
            };
            let path = row.path.clone();
            let directory = row.kind == Kind::Directory;
            host.select(&window, token, index, false);
            show(&window, path, directory, x, y);
        }
    });
    window.on_top_entry_menu({
        let host = host.clone();
        let weak = weak.clone();
        let show = show.clone();
        move |token, index, x, y| {
            let Some(window) = weak.upgrade() else { return };
            if !window.get_top_visible()
                || window.get_cache_visible()
                || window.get_windows_rename_visible()
            {
                return;
            }
            let mut host = host.borrow_mut();
            let Some(row) = host
                .top_row(token, index)
                .filter(|row| row.kind == Kind::File)
            else {
                return;
            };
            let path = row.path.clone();
            host.select_top(&window, index);
            show(&window, path, false, x, y);
        }
    });
    let host = host.clone();
    window.on_duplicate_entry_menu(move |token, index, x, y| {
        let Some(window) = weak.upgrade() else { return };
        if window.get_windows_rename_visible() {
            return;
        }
        let path = host.borrow().duplicate_menu_path(&window, token, index);
        if let Some(path) = path {
            window.invoke_search_select(window.get_search_token(), index, false);
            show(&window, path, false, x, y);
        }
    });
}

#[cfg(target_os = "linux")]
fn file_manager_uri(path: &Path) -> Option<String> {
    use std::fmt::Write;

    if !path.is_absolute() {
        return None;
    }
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match *byte {
            b'/' | b'-' | b'.' | b'_' | b'~' => uri.push(char::from(*byte)),
            byte if byte.is_ascii_alphanumeric() => uri.push(char::from(byte)),
            byte => write!(uri, "%{byte:02X}").unwrap(),
        }
    }
    Some(uri)
}

#[cfg(target_os = "linux")]
fn connect_linux_entry_menu(window: &SortSizeWindow, host: &Rc<RefCell<Host>>) {
    window.set_desktop_entry_menu(true);
    window.set_linux_entry_menu(true);
    // Keep the clicked native path independently of later publications and
    // selection changes. The menu never re-resolves a row index on activation.
    let target = Rc::new(RefCell::new(None));
    let pending = target.clone();
    connect_entry_menus(window, host, move |window, path, directory, x, y| {
        let Some(uri) = file_manager_uri(&path) else {
            return;
        };
        *pending.borrow_mut() = Some((uri, directory));
        window.invoke_show_linux_entry_menu(x, y, directory);
    });
    let weak = window.as_weak();
    window.on_linux_entry_open(move || {
        let Some((uri, directory)) = target.borrow_mut().take() else {
            return;
        };
        let Some(window) = weak.upgrade() else { return };
        window.set_windows_entry_notice("".into());
        let weak = weak.clone();
        let worker = std::thread::Builder::new()
            .name("file-manager".into())
            .spawn(move || {
                // FileManager1 only reveals items or opens folders. It never
                // runs the default application for a file or remounts a URI.
                let method = if directory {
                    "org.freedesktop.FileManager1.ShowFolders"
                } else {
                    "org.freedesktop.FileManager1.ShowItems"
                };
                let status = std::process::Command::new("gdbus")
                    .args([
                        "call",
                        "--session",
                        "--dest",
                        "org.freedesktop.FileManager1",
                        "--object-path",
                        "/org/freedesktop/FileManager1",
                        "--method",
                        method,
                        "--timeout",
                        "5",
                        &format!("['{uri}']"),
                        "",
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if !status.is_ok_and(|status| status.success()) {
                    let _ = weak.upgrade_in_event_loop(|window| {
                        window.set_windows_entry_notice(window.get_linux_entry_menu_error());
                    });
                }
            });
        if worker.is_err() {
            window.set_windows_entry_notice(window.get_linux_entry_menu_error());
        }
    });
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn file_manager_uri_preserves_native_bytes_in_one_dbus_argument() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    let path = Path::new(OsStr::from_bytes(b"/mnt/a #%'\\?\xff"));
    assert_eq!(
        file_manager_uri(path).as_deref(),
        Some("file:///mnt/a%20%23%25%27%5C%3F%FF")
    );
    assert_eq!(file_manager_uri(Path::new("smb://server/share")), None);
}

pub(crate) fn connect(
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
) -> std::io::Result<Rc<RefCell<Host>>> {
    let host = Rc::new(RefCell::new(Host::new()?));
    window.set_scan_fast_available(cfg!(any(windows, target_os = "linux")));
    sortsize_core::resources::Governor::global()
        .set_byte_limit(controller.borrow().preferences.memory_bytes);
    window.set_memory_limit(
        controller
            .borrow()
            .preferences
            .memory_bytes
            .map(|b| (b / (1024 * 1024)).to_string())
            .unwrap_or_default()
            .into(),
    );
    window.set_workshop_live(true);
    window.set_sort_order(i32::from(controller.borrow().preferences.detail_sort));
    host.borrow().service.lane.lock().unwrap().detail_sort =
        controller.borrow().preferences.detail_sort;
    window.set_preview_enabled(cfg!(feature = "experimental-preview"));
    window.set_product_version(env!("CARGO_PKG_VERSION").into());
    window.set_worker_choice(3);
    window.set_collect_owner(false);
    #[cfg(target_os = "linux")]
    window.set_local_owner_resolution(true);
    #[cfg(any(windows, target_os = "linux"))]
    {
        window.set_targeted_owner(true);
        window.set_resolve_owner(true);
    }
    window.set_collect_allocation(true);
    macro_rules! connect {($method:ident,|$w:ident,$h:ident,$c:ident $(,$arg:ident)*|$body:block)=>{{let weak=window.as_weak();let h=host.clone();let c=controller.clone();window.$method(move |$($arg),*|{if let Some($w)=weak.upgrade(){#[allow(unused_mut)] let mut $h=h.borrow_mut();#[allow(unused_mut)] let mut $c=c.borrow_mut();let _scope=$c.presentation.enter();$body}});}};}
    connect!(on_workshop_ready, |w, h, c| {
        h.poll_disconnect(&w, &mut c);
        h.poll_selection(&w, &mut c);
        h.poll(&w, &c);
    });
    // Wake the same product poll on publication instead of waiting for the
    // next 50 ms fallback tick. At most one wake is queued. Weak only: workers
    // cannot keep a closed window/controller alive or mutate UI off-thread.
    let weak = window.as_weak();
    let queued = Arc::new(std::sync::atomic::AtomicBool::new(false));
    host.borrow().service.lane.lock().unwrap().notify = Some(Arc::new(move || {
        use std::sync::atomic::Ordering;
        if queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let flag = queued.clone();
        if weak
            .upgrade_in_event_loop(move |window| {
                flag.store(false, Ordering::Release);
                window.invoke_workshop_ready();
            })
            .is_err()
        {
            queued.store(false, Ordering::Release);
        }
    }));
    connect!(on_start_scan, |w, h, c| {
        if w.get_choosing_folder() || !w.get_scan_enabled() {
            return;
        }
        w.set_windows_entry_notice("".into());
        let root = c.path_input.resolve(w.get_scan_path().as_str());
        if c.registry.is_disconnected(&root) {
            w.set_errors(tr!("This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning.").into());
            return;
        }
        let server = c
            .registry
            .find(&root)
            .filter(|s| s.is_server())
            .and_then(|s| s.server_target().map(|t| t.server_unc()));
        #[cfg(any(windows, target_os = "linux"))]
        let server = server.or_else(|| {
            root.to_str()
                .filter(|p| p.starts_with(r"\\") || p.starts_with("smb://"))
                .and_then(|p| sortsize_core::volumes::smb::parse_target(p).ok())
                .filter(|t| t.share.is_none())
                .map(|t| t.server_unc())
        });
        if let Some(server) = server {
            c.pending_auto = None;
            c.restored_selection = None;
            c.auto_analysis.manual(&server);
            c.remember_selection(&server);
            h.select_smb_server(&w, server);
            h.confirm_smb_server(&w);
            return;
        }
        if h.begin_single_smb_share(&w, &mut c, &root) {
            return;
        }
        if w.get_busy() && h.requested.as_ref() == Some(&root) {
            return;
        }
        c.pending_auto = None;
        c.restored_selection = None;
        c.auto_analysis.manual(&root);
        c.remember_selection(&root);
        let preflight = c.registry.scan_preflight(&root);
        h.start_with_mode(
            &w,
            &mut c,
            Start {
                root,
                explicit_root: true,
                observation: crate::workshop::service::size_observation(),
                preflight,
                options: sortsize_store::scheduler::CollectionOptions {
                    allocation: w.get_collect_allocation(),
                    owners: w.get_collect_owner(),
                },
            },
            w.get_scan_fast_mode(),
            service::StartPurpose::ManualScan,
        );
    });
    connect!(on_cancel_scan, |w, h, _c| {
        if h.cancel_folder_request(&w) || h.cancel_current_folder(&w) {
            return;
        }
        if let Some(trace) = &h.service.trace {
            trace.mark_first(&trace.cancel_requested);
        }
        h.mark_single_preparation_cancelled(&w);
        _c.auto_analysis.cancel();
        _c.pending_auto = None;
        _c.restored_selection = None;
        if h.service.smb_snapshot().is_some_and(|s| s.running) {
            h.service.cancel_smb_batch();
            w.set_cancelling(true);
            return;
        }
        #[cfg(any(windows, target_os = "linux"))]
        if w.get_targeted_owner() && w.get_resolve_owner() {
            w.set_resolve_owner(false);
            h.poll_owners(&w);
            if !w.get_busy() {
                return;
            }
        }
        if matches!(w.get_preview_state(), 1 | 2) {
            h.cancel_preview(&w);
            h.cache_id += 1;
            return;
        }
        if h.service.cancel_acquisition() {
            if h.service
                .lane
                .lock()
                .unwrap()
                .publication_cancel
                .is_cancelled()
            {
                h.cache_cancel.cancel();
            }
            w.set_cancelling(true);
            w.set_status(tr!("Cancelling…").into());
            w.set_top_note(tr!("Cancelling…").into());
            w.set_treemap_note(tr!("Cancelling…").into());
            if let Some(trace) = &h.service.trace {
                trace.mark_first(&trace.cancel_ui_ack);
                trace.visible(&w);
            }
        } else {
            // A stale displayed page may outlive the writer's terminal state.
            w.set_busy(false);
            w.set_cancelling(false);
            if let Some(current) = &mut h.current {
                current.busy = false;
            }
            let terminal = {
                let lane = h.service.lane.lock().unwrap();
                lane.phase.filter(|(epoch, phase, _)| {
                    *epoch == lane.source_epoch
                        && matches!(
                            phase,
                            service::Phase::Ready
                                | service::Phase::Cancelled
                                | service::Phase::PreviewReady
                        )
                })
            };
            if let Some((_, phase, generation)) = terminal {
                let status = format!("{} · G{}", phase.label(), generation.0);
                if let Some(current) = &mut h.current {
                    current.status = status.clone();
                }
                w.set_status(status.clone().into());
                w.set_top_note(status.clone().into());
                w.set_treemap_note(status.into());
            }
        }
    });
    connect!(on_cache_action, |w, h, c, kind, index, token| {
        h.cache_action(&w, kind, index, token, &mut c);
    });
    connect!(on_save_fast_inventory, |w, h, _c| {
        h.save_fast_inventory(&w);
    });
    connect!(on_cache_page, |_w, h, _c, next| {
        h.service.lane.lock().unwrap().cache_page = Some((h.cache_id, next));
    });
    connect!(on_workshop_retry, |w, h, c| {
        let root = c.path_input.resolve(w.get_scan_path().as_str());
        // A source discovery diagnostic alone is not a failed scan. Retry
        // may only act on the source of a request already made by the user.
        if h.requested.as_ref() != Some(&root) {
            return;
        }
        if c.registry.is_disconnected(&root) {
            w.set_errors(tr!("This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning.").into());
            return;
        }
        h.failed = false;
        h.service.lane.lock().unwrap().publication_cancel = Default::default();
        if cfg!(feature = "experimental-preview") && matches!(w.get_preview_state(), 4 | 5) {
            w.set_cache_visible(true);
            w.set_errors("".into());
            w.set_status(tr!("Preparing saved measures…").into());
            h.refresh_cache(&w);
            return;
        }
        let owns_candidate = h.service.lane.lock().unwrap().writer_owns_session;
        if !h.active_service && !owns_candidate {
            // A completed/offline page cannot grant a new acquisition.
            // The Scan button is the explicit command for a new request.
            return;
        }
        let mut lane = h.service.lane.lock().unwrap();
        lane.cancel_requested_epoch = None;
        lane.retry = true;
        drop(lane);
        w.set_cancelling(false);
        w.set_errors("".into());
    });
    // Only the source-card activation carries a current publication token.
    // The legacy selection callback is intentionally passive in the workshop.
    connect!(on_select_volume, |_w, _h, _c, _index| {});
    connect!(on_workshop_source, |w, h, c, token, index| {
        if c.source_publication.get() != token {
            return;
        }
        if w.get_choosing_folder() || !w.get_scan_enabled() {
            return;
        }
        h.cancel_folder_request(&w);
        let selected = usize::try_from(index)
            .ok()
            .and_then(|i| c.sources.current.as_ref()?.discovery.volumes.get(i));
        let server = selected
            .and_then(|v| v.mounts.first())
            .and_then(|m| c.registry.find(&m.path))
            .filter(|s| s.is_server())
            .map(|s| s.path.clone());
        if let Some(root) = server {
            c.pending_auto = None;
            c.restored_selection = None;
            c.auto_analysis.manual(&root);
            c.remember_selection(&root);
            w.set_selected_volume(index);
            h.select_smb_server(&w, root);
            return;
        }
        w.set_smb_server_context(false);
        w.set_smb_server_visible(false);
        let registered = selected.is_some_and(|volume| {
            volume
                .mounts
                .first()
                .and_then(|mount| c.registry.find(&mount.path))
                .is_some_and(|source| source.kind != crate::source_registry::Kind::Local)
        });
        if let Some(volume) = selected.filter(|_| !registered)
            && let Some(mount) = volume.mounts.first()
            && let Some(interrupted) = &h.interrupted_source
            && interrupted.root == mount.path
            && !w.get_busy()
            && !w.get_search_busy()
            && !c.registry.is_disconnected(&mount.path)
        {
            let allow_fresh = crate::volume_view::can_auto_scan(volume);
            let same_source = if let Some(selection) = &interrupted.selection {
                allow_fresh
                    && c.sources
                        .current
                        .as_ref()
                        .is_some_and(|prepared| selection.matches_local(&prepared.discovery))
            } else {
                // With no prior discovery, the recovery guard still verifies
                // continuity. A path-only local mapping can only resume.
                allow_fresh
                    || (cfg!(windows)
                        && matches!(
                            volume.identity,
                            sortsize_core::volumes::VolumeIdentity::WindowsPath(_)
                        )
                        && volume.kind == sortsize_core::volumes::SourceKind::Local
                        && volume.availability
                            == sortsize_core::volumes::VolumeAvailability::Available)
            };
            if same_source {
                let root = mount.path.clone();
                let options = interrupted.options;
                let explicit_root = interrupted.explicit_root;
                let preflight = allow_fresh.then(|| c.sources.scan_preflight(volume, mount));
                c.pending_auto = None;
                c.restored_selection = None;
                c.auto_analysis.manual(&root);
                c.remember_selection(&root);
                w.set_scan_path(c.path_input.select(root.clone()).into());
                w.set_selected_volume(index);
                h.start_with_mode(
                    &w,
                    &mut c,
                    Start {
                        root,
                        explicit_root: false,
                        observation: service::size_observation(),
                        preflight,
                        options: sortsize_store::scheduler::CollectionOptions {
                            allocation: w.get_collect_allocation(),
                            owners: w.get_collect_owner(),
                        },
                    },
                    false,
                    service::StartPurpose::ResumeInterrupted {
                        allow_fresh,
                        options,
                        explicit_root,
                    },
                );
                return;
            }
        }
        // Selecting a remote/uncertain source only fills the native path.
        // The existing Analyze command grants one acquisition; no grant is saved.
        // Registered sources retain that contract even if a later discovery
        // reports an ordinary local directory at a disconnected mountpoint.
        if let Some(volume) =
            selected.filter(|v| registered || !crate::volume_view::can_auto_scan(v))
            && let Some(mount) = volume.mounts.first()
        {
            let root = mount.path.clone();
            c.pending_auto = None;
            c.restored_selection = None;
            c.auto_analysis.manual(&root);
            c.remember_selection(&root);
            w.set_scan_path(c.path_input.select(root).into());
            w.set_selected_volume(index);
            c.diagnostic(&w, crate::presentation::Diagnostic::VolumeBlocked);
            return;
        }
        if let Some(volume) = selected.filter(|v| crate::volume_view::can_auto_scan(v))
            && let Some(mount) = volume.mounts.first()
        {
            let root = mount.path.clone();
            let preflight = Some(c.sources.scan_preflight(volume, mount));
            w.set_selected_volume(index);
            c.queue_automatic(&w, root, preflight, false);
            h.poll_selection(&w, &mut c);
        } else {
            w.set_errors(tr!("Source is no longer available.").into());
        }
    });
    connect!(on_workshop_action, |w, h, _c, token, index, open| {
        h.select(&w, token, index, open);
    });
    #[cfg(windows)]
    {
        use crate::windows_context_menu::{ExplorerAction, Outcome, Target};

        window.set_desktop_entry_menu(true);
        let host = host.clone();
        let weak = window.as_weak();
        let rename_target = Rc::new(RefCell::new(None::<Target>));
        let pending = rename_target.clone();
        window.on_windows_rename_submit({
            let weak = weak.clone();
            let pending = pending.clone();
            move |name| {
                let Some(window) = weak.upgrade() else { return };
                let mut pending = pending.borrow_mut();
                let Some(target) = pending.as_ref() else {
                    return;
                };
                if target.rename(name.as_str()).is_err() {
                    window.set_windows_rename_failed(true);
                    return;
                }
                pending.take();
                window.invoke_windows_rename_close();
                window.set_windows_entry_notice(window.get_windows_entry_changed());
            }
        });
        window.on_windows_rename_cancel(move || {
            pending.borrow_mut().take();
        });
        let show_menu = {
            let weak = weak.clone();
            move |path: PathBuf, explorer: Option<ExplorerAction>, x, y| {
                let weak = weak.clone();
                let rename_target = rename_target.clone();
                // Native menu tracking runs a nested message loop. Leave the Slint
                // input dispatch and release the controller before entering it.
                slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                    if let Some(window) = weak.upgrade() {
                        match crate::windows_context_menu::show(
                            window.window(),
                            &path,
                            (x, y),
                            &window.get_windows_entry_menu_error(),
                            &window.get_windows_recycle_file(),
                            &window.get_windows_recycle_folder(),
                            explorer.as_ref(),
                        ) {
                            Outcome::Unchanged => {}
                            Outcome::Rename(target) => {
                                window.set_windows_rename_path(label(target.path()).into());
                                let name = target.path().file_name().and_then(|name| name.to_str());
                                window.set_windows_rename_name(name.unwrap_or_default().into());
                                window.set_windows_rename_failed(false);
                                *rename_target.borrow_mut() = Some(target);
                                window.set_windows_rename_visible(true);
                            }
                            Outcome::Recycled { recorded } => {
                                window.set_windows_entry_notice(if recorded {
                                    window.get_windows_entry_changed()
                                } else {
                                    window.get_windows_recycle_journal_error()
                                });
                            }
                        }
                    }
                });
            }
        };
        connect_entry_menus(window, &host, move |window, path, directory, x, y| {
            let explorer = if directory {
                ExplorerAction::OpenFolder(window.get_windows_open_in_explorer())
            } else {
                ExplorerAction::RevealFile(window.get_windows_reveal_in_explorer())
            };
            show_menu(path, Some(explorer), x, y);
        });
    }
    #[cfg(target_os = "linux")]
    connect_linux_entry_menu(window, &host);
    connect!(on_workshop_gesture, |_w, h, _c, active| {
        if active {
            h.gesture = true;
            h.release = false;
        } else {
            h.release = true;
        }
    });
    connect!(on_select_node, |w, h, _c, index| {
        h.tree_action(&w, index, false);
    });
    connect!(on_inspect_entry, |w, h, _c, index| {
        h.select(&w, w.get_workshop_token(), index, false);
    });
    connect!(on_toggle_node, |w, h, _c, index| {
        h.tree_action(&w, index, true);
    });
    connect!(on_tree_key, |w, h, _c, action| {
        h.tree_key(&w, action);
    });
    window.on_prepare_tree_menu({
        let host = host.clone();
        move |index| {
            host.borrow_mut()
                .prepare_tree_menu(index)
                .map_or(-1, |i| i as i32)
        }
    });
    connect!(on_tree_find_duplicates, |w, h, c| {
        if let Some(target) = h.take_tree_menu_target(&w) {
            h.start_folder_duplicates(&w, &mut c, target.path.as_ref().clone());
        } else if w.get_search_visible() && w.get_duplicate_mode() > 0 {
            w.set_duplicate_warning(w.get_status());
        }
    });
    connect!(on_tree_find_empty, |w, h, c| {
        if let Some(target) = h.take_tree_menu_target(&w)
            && !w.get_choosing_folder()
        {
            h.open_empty_analysis(&w, &c, 1, Some(target.path.as_ref().clone()));
            h.start_empty_analysis(&w, &c);
        }
    });
    connect!(on_tree_top_files, |w, h, _c| {
        if let Some(target) = h.take_tree_menu_target(&w)
            && h.switch_top_scope(
                &w,
                Some(service::TopScope {
                    key: target.key,
                    path: target.path,
                }),
            )
        {
            h.close_search(&w);
            w.invoke_focus_top();
        }
    });
    connect!(on_show_global_top, |w, h, _c| {
        if !w.get_top_scope().is_empty() {
            h.switch_top_scope(&w, None);
        }
    });
    connect!(on_go_parent, |_w, h, _c| {
        if _w.get_smb_server_context() && h.current.as_ref().is_none_or(|p| !p.parent) {
            drop(_c);
            drop(h);
            _w.invoke_smb_server_home();
            return;
        }
        if !h.browse_interrupted_parent(&_w, &mut _c) {
            h.navigate(Navigate::Parent);
        }
    });
    connect!(on_change_page, |_w, h, _c, _details, next| {
        let observed = h.current.as_ref().is_some_and(|page| page.sort_index < 0) && {
            let lane = h.service.lane.lock().unwrap();
            lane.direct_view == Some(lane.source_epoch)
        };
        h.navigate(if observed {
            Navigate::ObservedPage(next)
        } else {
            Navigate::Page(next)
        });
    });
    connect!(on_select_top, |w, h, _c, index| {
        h.select_top(&w, index);
    });
    connect!(on_reveal_top, |w, h, _c| {
        if let Some(key) = h.top_selected {
            h.selected = Some(key);
            h.navigate(Navigate::Reveal(key));
            w.set_top_visible(false);
        }
    });
    connect!(on_activate_tile, |w, h, _c, index| {
        if let Some(p) = &h.current
            && let Some(tile) = usize::try_from(index).ok().and_then(|i| p.tiles.get(i))
        {
            let row = tile.node_id;
            h.select(&w, w.get_workshop_token(), row, true);
        }
    });
    connect!(on_sort_details, |w, h, c, name| {
        if h.current.is_none() && h.requested.is_none() {
            return;
        }
        let current = w.get_sort_order();
        let index = if name {
            if current == 2 { 3 } else { 2 }
        } else if current == 0 {
            1
        } else {
            0
        };
        h.close_search(&w);
        c.preferences.detail_sort = index as u8;
        let units = c.preferences.units;
        c.display_changed(&w, units);
        h.service.lane.lock().unwrap().detail_sort = index as u8;
        h.navigate(Navigate::Sort(index));
        w.set_busy(true);
        w.set_status(tr!("Preparing inventory…").into());
    });
    connect!(on_sort_allocated, |w, h, c| {
        if h.current.is_none() && h.requested.is_none() {
            return;
        }
        let i = if w.get_sort_order() == 4 { 5 } else { 4 };
        c.preferences.detail_sort = i as u8;
        let units = c.preferences.units;
        c.display_changed(&w, units);
        h.service.lane.lock().unwrap().detail_sort = i as u8;
        h.navigate(Navigate::Sort(i));
        w.set_busy(true);
        w.set_status(tr!("Preparing inventory…").into());
    });
    connect!(on_sort_owner, |w, h, c| {
        if h.current.is_none() && h.requested.is_none() {
            return;
        }
        let i = if w.get_sort_order() == 6 { 7 } else { 6 };
        let labels = if w.get_resolve_owner()
            && h.current
                .as_ref()
                .is_some_and(|p| h.owner_names_tag == Some((h.epoch, p.token)))
        {
            h.owner_names
                .iter()
                .map(|p| sortsize_store::inventory::ordered::OwnerLabel {
                    owner: p.id,
                    label: p.label.to_string(),
                })
                .collect()
        } else {
            Vec::new()
        };
        c.preferences.detail_sort = i as u8;
        let units = c.preferences.units;
        c.display_changed(&w, units);
        h.service.lane.lock().unwrap().detail_sort = i as u8;
        h.navigate(Navigate::OwnerSort(i, labels));
        w.set_busy(true);
        w.set_status(tr!("Preparing inventory…").into());
    });
    connect!(on_sort_dates, |w, h, c, accessed| {
        if h.current.is_none() && h.requested.is_none() {
            return;
        }
        let base = if accessed { 10 } else { 8 };
        let i = if w.get_sort_order() == base {
            base + 1
        } else {
            base
        };
        c.preferences.detail_sort = i as u8;
        let units = c.preferences.units;
        c.display_changed(&w, units);
        h.service.lane.lock().unwrap().detail_sort = i as u8;
        h.navigate(Navigate::Sort(i));
        w.set_busy(true);
        w.set_status(tr!("Preparing inventory…").into());
    });
    connect!(on_owner_resolution_changed, |w, h, _c| {
        h.owner_request = None;
        h.poll_owners(&w);
    });
    search::connect(window, controller, &host);
    duplicate_ui::connect(window, controller, &host);
    connect!(on_open_empty_analysis, |w, h, c, mode| {
        h.open_empty_analysis(&w, &c, mode, None);
    });
    connect!(on_empty_kind_changed, |w, h, c, mode| {
        h.change_empty_kind(&w, &c, mode);
    });
    connect!(on_empty_start, |w, h, c| {
        h.start_empty_analysis(&w, &c);
    });
    connect!(on_empty_browse, |w, h, _c| {
        h.pick_empty_scope(&w);
    });
    connect!(on_empty_scope_edited, |w, h, c| {
        h.edit_empty_scope(&w, &c);
    });
    connect!(on_empty_query, |w, h, _c| {
        h.query_empty_analysis(&w, None);
    });
    connect!(on_empty_page, |w, h, _c, next| {
        h.query_empty_analysis(&w, Some(next));
    });
    connect!(on_empty_cancel, |w, h, _c| {
        h.cancel_empty_analysis(&w);
    });
    server_ui::connect(window, controller, &host);
    disconnect::connect(window, controller, &host);
    Ok(host)
}

#[cfg(feature = "benchmarks")]
mod qualification;
#[cfg(feature = "benchmarks")]
pub use qualification::run as qualify;

#[cfg(feature = "benchmarks")]
mod finish_qualification;
#[cfg(feature = "benchmarks")]
pub use finish_qualification::run as qualify_finish;

#[cfg(test)]
mod progress_tests;
#[cfg(feature = "benchmarks")]
mod search_qualification;
#[cfg(test)]
mod tests;
#[cfg(feature = "benchmarks")]
pub use search_qualification::run as qualify_search;

#[cfg(feature = "benchmarks")]
mod cache_qualification;
#[cfg(feature = "benchmarks")]
pub use cache_qualification::run as qualify_cache;

#[cfg(all(feature = "benchmarks", feature = "experimental-preview"))]
mod preview_qualification;
#[cfg(all(feature = "benchmarks", feature = "experimental-preview"))]
pub use preview_qualification::run as qualify_preview;

#[cfg(feature = "benchmarks")]
mod cached_a_qualification;
#[cfg(feature = "benchmarks")]
pub use cached_a_qualification::{
    prepare_smoke as prepare_cached_a_smoke, run as qualify_cached_a,
};

#[cfg(test)]
mod compact_tree_tests;

#[cfg(test)]
mod size_sort_ui_tests;

#[cfg(test)]
mod navigation_tests;

#[cfg(test)]
mod auto_analysis_tests;

#[cfg(all(test, windows))]
mod native_duplicate_priority_test;
