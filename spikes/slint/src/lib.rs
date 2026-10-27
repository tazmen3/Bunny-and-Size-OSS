#[macro_use]
pub mod presentation;
mod catalog_generated;
#[cfg(feature = "benchmarks")]
pub mod i18n_harness;
#[cfg(feature = "benchmarks")]
pub mod measure;

mod allocation_label;
mod analysis_controller;
mod date_label;
mod directory_controller;
mod directory_worker;
mod display_preferences;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod folder_picker;
mod language_controller;
pub mod locale;
mod number_format;
mod owner_controller;
mod owner_view;
mod path_input;
#[cfg(any(target_os = "linux", windows))]
mod preferences_store;
mod tree;
mod treemap;
mod units;
#[cfg(any(target_os = "linux", windows))]
mod volume_view;
#[cfg(windows)]
mod window_placement;
mod worker;

use path_input::PathInput;
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use tree::Tree;
use worker::Task;

slint::include_modules!();

#[derive(Default)]
struct Controller {
    analysis: analysis_controller::Panel,
    presentation: presentation::Context,
    language_activation: Option<locale::Activation>,
    system_locale: Option<String>,
    inspected_group: Option<i32>,
    legal: Option<slint::Weak<LegalWindow>>,
    status: presentation::Status,
    diagnostic: presentation::Diagnostic,
    preferences: display_preferences::Preferences,
    #[cfg(any(target_os = "linux", windows))]
    preference_store: Option<preferences_store::Store>,
    scan_labels: Option<worker::DisplayLabels>,
    warning_labels: Option<worker::DisplayLabels>,
    path_input: PathInput,
    #[cfg(any(target_os = "linux", windows))]
    sources: volume_view::Sources,
    #[cfg(any(target_os = "linux", windows))]
    scan_volume: Option<(
        sortsize_core::volumes::VolumeIdentity,
        sortsize_core::volumes::VolumeMount,
    )>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    folder_picker: Option<folder_picker::FolderPicker>,
    task: Option<Task>,
    scan_worker: Option<directory_worker::DirectoryWorker>,
    scan_root: Option<std::path::PathBuf>,
    directory: directory_controller::DirectoryState,
    tree: Option<Tree>,
    generation: u64,
    owner_task: Option<owner_view::Task>,
    owner_page: u64,
    owner_page_key: Option<owner_controller::PageKey>,
    owner_names: std::sync::Arc<[owner_view::DisplayOwner]>,
    owner_sort_serial: u64,
    owner_sort_pending: Option<(u64, usize)>,
    owner_stats: sortsize_owner_resolution::ResolverStats,
    owner_status: presentation::OwnerStatus,
    #[cfg(feature = "benchmarks")]
    owner_patch_max_ms: f64,
    #[cfg(feature = "benchmarks")]
    owner_sort_prepare_ms: f64,
    #[cfg(feature = "benchmarks")]
    owner_patch_rows: usize,
    #[cfg(feature = "benchmarks")]
    benchmark_summary: Option<worker::BenchmarkSummary>,
}

impl Controller {
    fn status(&mut self, window: &SortSizeWindow, status: presentation::Status) {
        let _presentation = self.presentation.enter();
        self.status = status;
        window.set_status(self.status.render().into());
    }
    fn diagnostic(&mut self, window: &SortSizeWindow, diagnostic: presentation::Diagnostic) {
        let _presentation = self.presentation.enter();
        self.diagnostic = diagnostic;
        window.set_errors(self.diagnostic.render().into());
    }

    fn show_preferences(&self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        let labels = window.get_column_labels();
        let columns = (0..11)
            .map(|i| ColumnSetting {
                label: labels.row_data(i).unwrap_or_default(),
                shown: i == 0 || self.preferences.visible[i],
                pixels: f32::from(display_preferences::WIDTHS[i]),
            })
            .collect::<Vec<_>>();
        let model = window.get_column_settings();
        if model.row_count() == columns.len() {
            // Keep checkbox instances/focus while a keyboard user toggles them.
            for (index, column) in columns.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&column) {
                    model.set_row_data(index, column);
                }
            }
        } else {
            window.set_column_settings(Rc::new(VecModel::from(columns)).into());
        }
        window.set_table_pixels(f32::from(self.preferences.width()));
        window.set_decimal_units(self.preferences.units == units::Units::Decimal);
        window.set_sidebar_ratio(i32::from(self.preferences.sidebar_ratio));
    }
    fn display_changed(&mut self, window: &SortSizeWindow, previous: units::Units) {
        let _presentation = self.presentation.enter();
        self.show_preferences(window);
        if previous != self.preferences.units {
            self.update_units(window);
        }
        #[cfg(any(target_os = "linux", windows))]
        if let Some(store) = &mut self.preference_store {
            store.submit(self.preferences.clone());
            window.set_preference_state(1);
        }
    }
    fn update_units(&mut self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        let units = self.preferences.units;
        self.analysis.set_units(units, self.presentation);
        if let Some(original) = &mut self.directory.original {
            original.set_units(units);
        }
        if let Some(tree) = &mut self.tree {
            tree.set_units(units);
            // Patch size fields only. Do not reset pagination, selection, scroll,
            // owner generations, names, native identities or path strings.
            let rows = window.get_rows();
            for i in 0..rows.row_count() {
                if let Some(mut row) = rows.row_data(i) {
                    row.size = tree.size_label(row.node_id as usize);
                    rows.set_row_data(i, row);
                }
            }
            let details = window.get_details();
            for i in 0..details.row_count() {
                if let Some(mut row) = details.row_data(i) {
                    row.size = tree.size_label(row.node_id as usize);
                    row.allocated = tree.allocation_label(row.node_id as usize);
                    details.set_row_data(i, row);
                }
            }
            window.set_top_rows(
                Rc::new(VecModel::from(
                    self.directory
                        .original
                        .as_ref()
                        .unwrap_or(tree)
                        .top_rows
                        .clone(),
                ))
                .into(),
            );
            window.set_selection_summary(tree.selection_summary().into());
            self.show_treemap(window);
        }
        if let Some(labels) = &self.scan_labels {
            window.set_summary(labels.get(units).into());
        }
        if let Some(labels) = &self.warning_labels {
            window.set_inventory_warning(labels.get(units).into());
        }
        #[cfg(any(target_os = "linux", windows))]
        self.show_sources(window);
        self.directory_controls(window);
    }
    #[cfg(any(target_os = "linux", windows))]
    fn refresh_sources(&mut self, window: &SortSizeWindow, remote: bool) {
        let _presentation = self.presentation.enter();
        self.sources
            .refresh(sortsize_core::volumes::DiscoveryOptions {
                probe_remote: remote,
                explicit_unc: {
                    #[cfg(windows)]
                    {
                        let path = self.path_input.resolve(window.get_scan_path().as_str());
                        if remote && sortsize_core::volumes::unc_source(&path).is_ok() {
                            Some(path)
                        } else {
                            None
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        None
                    }
                },
            });
        self.show_sources(window);
    }
    #[cfg(any(target_os = "linux", windows))]
    fn show_sources(&self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        window.set_storage_loading(self.sources.loading());
        window.set_storage_status(self.sources.status().into());
        if let Some(prepared) = &self.sources.current {
            window.set_volumes(
                Rc::new(VecModel::from(prepared.rows_for(self.preferences.units))).into(),
            );
            let path = self.path_input.resolve(window.get_scan_path().as_str());
            let selected = prepared
                .discovery
                .volumes
                .iter()
                .position(|v| v.mounts.first().is_some_and(|m| m.path == path));
            window.set_selected_volume(selected.map(|i| i as i32).unwrap_or(-1));
            let scanned = prepared.discovery.volumes.iter().position(|v| {
                self.scan_volume.as_ref().is_some_and(|(identity, mount)| {
                    &v.identity == identity
                        && v.mounts.contains(mount)
                        && self.scan_root.as_ref() == Some(&mount.path)
                })
            });
            window.set_scanned_volume(scanned.map(|i| i as i32).unwrap_or(-1));
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn select_volume(&mut self, window: &SortSizeWindow, index: i32) {
        let _presentation = self.presentation.enter();
        if window.get_choosing_folder() || !window.get_scan_enabled() {
            return;
        }
        let Some(volume) = usize::try_from(index)
            .ok()
            .and_then(|i| self.sources.current.as_ref()?.discovery.volumes.get(i))
        else {
            return;
        };
        if !volume_view::can_auto_scan(volume) {
            self.diagnostic(window, presentation::Diagnostic::VolumeBlocked);
            return;
        }
        let Some(mount) = volume.mounts.first() else {
            return;
        };
        let preflight = self.sources.scan_preflight(volume, mount);
        let path = mount.path.clone();
        // Same source while running is idempotent. A completed source can be rescanned.
        let identity = (volume.identity.clone(), mount.clone());
        if self
            .task
            .as_ref()
            .is_some_and(|task| !task.cancellation.is_cancelled())
            && self.scan_volume.as_ref() == Some(&identity)
        {
            return;
        }
        // Dropping the only visible mailbox cancels the old generation. Its late
        // progress, patches and completion are unreachable from the next poll.
        self.task = None;
        self.scan_volume = Some(identity);
        window.set_scan_path(self.path_input.select(path).into());
        window.set_selected_volume(index);
        window.set_resolve_owner(false);
        self.resolution_changed(window);
        self.start_checked(window, Some(preflight));
    }
    fn start(&mut self, window: &SortSizeWindow) {
        self.start_checked(window, None);
    }
    fn start_checked(
        &mut self,
        window: &SortSizeWindow,
        preflight: Option<directory_worker::Preflight>,
    ) {
        self.analysis.invalidate();
        let _presentation = self.presentation.enter();
        if self.task.is_some() || window.get_choosing_folder() || !window.get_scan_enabled() {
            return;
        }
        let path = self.path_input.resolve(window.get_scan_path().as_str());
        if path.as_os_str().is_empty() {
            self.status(window, presentation::Status::EnterPath);
            return;
        }
        #[cfg(any(target_os = "linux", windows))]
        if preflight.is_none() {
            // Bind a manual scan to the identity known at its start. A later
            // discovery at the same pathname must never acquire its old results.
            self.scan_volume = self.sources.current.as_ref().and_then(|prepared| {
                prepared
                    .discovery
                    .volumes
                    .iter()
                    .filter(|volume| volume_view::can_auto_scan(volume))
                    .find_map(|volume| {
                        volume
                            .mounts
                            .iter()
                            .find(|mount| mount.path == path)
                            .map(|mount| (volume.identity.clone(), mount.clone()))
                    })
            });
        }
        self.directory
            .reset(window.get_collect_allocation(), window.get_collect_owner());
        window.set_can_load_details(false);
        self.tree = None;
        self.inspected_group = None;
        self.scan_labels = None;
        self.warning_labels = None;
        self.generation = self.generation.wrapping_add(1);
        self.owner_page_key = None;
        self.owner_names = Default::default();
        self.owner_sort_pending = None;
        self.owner_sort_serial = self.owner_sort_serial.wrapping_add(1);
        window.set_owner_sort_preparing(false);
        self.request_owners(window, None);
        #[cfg(feature = "benchmarks")]
        {
            self.benchmark_summary = None;
        }
        window.set_top_rows(ModelRc::default());
        window.set_top_selected(-1);
        window.set_top_path("".into());
        window.set_top_retained(false);
        window.set_top_note(tr!("Scan in progress…").into());
        window.set_top_scroll_y(0.0);
        window.set_detail_scroll_y(0.0);
        window.set_map_cursor(0);
        window.set_rows(ModelRc::default());
        window.set_details(ModelRc::default());
        window.set_detail_scroll_x(0.0);
        window.set_tiles(ModelRc::default());
        window.set_treemap_note(tr!("Scan in progress…").into());
        window.set_inspected_node(-1);
        self.scan_root = Some(path.clone());
        window.set_scan_source(tree::native_label(&path).into());
        window.set_current_path("".into());
        #[cfg(any(target_os = "linux", windows))]
        self.show_sources(window);
        window.set_entry_path("".into());
        window.set_selection_summary("".into());
        window.set_empty_message(tr!("Scan in progress…").into());
        window.set_has_parent(false);
        window.set_has_previous_detail_page(false);
        window.set_has_next_detail_page(false);
        self.diagnostic(window, presentation::Diagnostic::None);
        window.set_inventory_warning("".into());
        window.set_view_warning("".into());
        window.set_has_previous_page(false);
        window.set_has_next_page(false);
        window.set_summary(tr!("Preparing inventory…").into());
        window.set_cancelling(false);
        let workers = match window.get_worker_choice() {
            1 => 2,
            2 => 4,
            3 => 8,
            _ => 1,
        };
        // One persistent worker and one replaceable pending request per window.
        // A blocked OS call delays the new scan but cannot accumulate threads.
        let task = (|| {
            if self.scan_worker.is_none() {
                self.scan_worker = Some(directory_worker::DirectoryWorker::named("sortsize-scan")?);
            }
            Ok::<_, std::io::Error>(self.scan_worker.as_ref().unwrap().submit_checked(
                worker::Job {
                    root: path,
                    workers,
                    collect_allocation: window.get_collect_allocation(),
                    collect_owner: window.get_collect_owner(),
                    limits: sortsize_core::InventoryLimits::default(),
                    child_offset: 0,
                    scope: None,
                },
                preflight,
            ))
        })();
        match task {
            Ok(task) => {
                self.task = Some(task);
                window.set_busy(true);
                self.status(window, presentation::Status::Scanning);
            }
            Err(error) => {
                window.set_busy(false);
                self.status(window, presentation::Status::StartFailed);
                self.diagnostic(
                    window,
                    presentation::Diagnostic::Native(error.to_string().escape_debug().to_string()),
                );
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn browse(&mut self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        self.begin_browse(window, |initial| {
            let mut dialog = rfd::FileDialog::new()
                .set_title(tr!("Choose a folder to scan"))
                .set_parent(&window.window().window_handle());
            if !initial.as_os_str().is_empty() {
                dialog = dialog.set_directory(initial);
            }
            folder_picker::FolderPicker::start(move || dialog.pick_folder())
        });
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn begin_browse(
        &mut self,
        window: &SortSizeWindow,
        open: impl FnOnce(std::path::PathBuf) -> std::io::Result<folder_picker::FolderPicker>,
    ) {
        let _presentation = self.presentation.enter();
        if self.task.is_some()
            || self.folder_picker.is_some()
            || !window.get_scan_enabled()
            || !window.get_folder_picker_enabled()
        {
            return;
        }
        match open(self.path_input.resolve(window.get_scan_path().as_str())) {
            Ok(picker) => {
                self.folder_picker = Some(picker);
                window.set_choosing_folder(true);
            }
            Err(error) => self.diagnostic(
                window,
                presentation::Diagnostic::PickerStart(error.to_string().escape_debug().to_string()),
            ),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn poll_folder(&mut self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        let Some(result) = self.folder_picker.as_ref().and_then(|picker| picker.poll()) else {
            return;
        };
        self.folder_picker = None;
        window.set_choosing_folder(false);
        match result {
            Ok(Some(path)) => {
                window.set_scan_path(self.path_input.select(path).into());
                window.set_selected_volume(-1);
                window.set_resolve_owner(false);
                self.resolution_changed(window);
            }
            // rfd also returns None when the desktop service is unavailable.
            // Preserve the previous source and inventory in either case.
            Ok(None) => {}
            Err(_) => self.diagnostic(window, presentation::Diagnostic::PickerStopped),
        }
    }

    fn cancel(&mut self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        window.set_resolve_owner(false);
        self.resolution_changed(window);
        self.cancel_directory(window);
        if let Some(task) = &self.task {
            task.cancellation.cancel();
            window.set_cancelling(true);
            self.status(window, presentation::Status::Cancelling);
        }
    }

    fn poll(&mut self, window: &SortSizeWindow) {
        self.analysis.poll(self.presentation);
        let _presentation = self.presentation.enter();
        #[cfg(any(target_os = "linux", windows))]
        if let Some(success) = self
            .preference_store
            .as_ref()
            .and_then(|store| store.poll())
        {
            window.set_preference_state(if success { 2 } else { 3 });
        }
        #[cfg(any(target_os = "linux", windows))]
        if self.sources.poll() {
            self.show_sources(window);
        }
        self.poll_owners(window);
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        self.poll_folder(window);
        self.poll_directory(window);
        let Some(task) = &self.task else {
            return;
        };
        let (progress, patch, completion) = {
            let mut mailbox = match task.mailbox.try_lock() {
                Ok(mailbox) => mailbox,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            (
                mailbox.progress.take(),
                mailbox.patch.take(),
                mailbox.completion.take(),
            )
        };
        if let Some(progress) = progress {
            window.set_summary(progress.get(self.preferences.units).into());
            self.scan_labels = Some(progress);
        }
        if completion.is_none()
            && let Some(patch) = patch
        {
            self.apply_inventory_patch(window, patch);
        }
        if let Some(completion) = completion {
            self.task = None;
            window.set_busy(false);
            window.set_cancelling(false);
            match completion {
                Ok(mut result) => {
                    if let Some(previous) = &self.tree {
                        result.tree.keep_navigation(previous);
                    }
                    result.tree.prepare_presentation(self.presentation);
                    result.tree.set_units(self.preferences.units);
                    #[cfg(feature = "benchmarks")]
                    {
                        self.benchmark_summary = Some(result.benchmark_summary);
                    }
                    self.status(
                        window,
                        presentation::Status::Completed(result.presentation.report.coverage),
                    );
                    window.set_summary(result.summary.get(self.preferences.units).into());
                    self.scan_labels = Some(result.summary);
                    self.diagnostic(
                        window,
                        presentation::Diagnostic::Report(result.presentation.clone()),
                    );
                    window.set_inventory_warning(result.warning.get(self.preferences.units).into());
                    self.warning_labels = Some(result.warning);
                    window
                        .set_top_rows(Rc::new(VecModel::from(result.tree.top_rows.clone())).into());
                    window.set_top_note(result.tree.top_note.clone().into());
                    self.tree = Some(result.tree);
                    self.show_rows(window);
                }
                Err(error) => {
                    self.tree = None;
                    window.set_rows(ModelRc::default());
                    window.set_details(ModelRc::default());
                    window.set_tiles(ModelRc::default());
                    self.scan_labels = None;
                    window.set_top_note(tr!("No ranking available: scan failed.").into());
                    self.status(window, presentation::Status::Failed);
                    window.set_summary(tr!("No inventory available for this folder.").into());
                    self.diagnostic(window, presentation::Diagnostic::Failure(error));
                    window.set_empty_message(tr!("No inventory available.").into());
                    window.set_treemap_note(tr!("No inventory available.").into());
                }
            }
        }
    }

    fn apply_inventory_patch(&mut self, window: &SortSizeWindow, patch: tree::TreePatch) {
        let tree = self.tree.get_or_insert_with(Tree::preview);
        tree.apply_patch(patch);
        tree.prepare_presentation(self.presentation);
        tree.set_units(self.preferences.units);
        let scroll = window.get_detail_scroll_y();
        let inspected = window.get_inspected_node();
        let entry_path = window.get_entry_path();
        let cursor = window.get_map_cursor();
        self.show_rows(window);
        window.set_detail_scroll_y(scroll);
        window.set_inspected_node(inspected);
        window.set_entry_path(entry_path);
        window.set_map_cursor(cursor);
    }
    fn select_top(&self, window: &SortSizeWindow, rank: i32) {
        let _presentation = self.presentation.enter();
        if rank >= 0
            && let Some((path, node)) = self
                .directory
                .original
                .as_ref()
                .or(self.tree.as_ref())
                .and_then(|t| t.top_selection(rank as usize))
        {
            window.set_top_selected(rank);
            window.set_top_path(path.into());
            window.set_top_retained(node.is_some());
        }
    }
    fn reveal_top(&mut self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        let rank = window.get_top_selected();
        let selected = usize::try_from(rank).ok().and_then(|rank| {
            self.directory
                .original
                .as_ref()
                .or(self.tree.as_ref())
                .and_then(|tree| tree.top_selection(rank))
        });
        if let Some((_, Some(index))) = selected {
            self.restore_primary(window);
            self.tree.as_mut().unwrap().select_entry_parent(index);
            window.set_top_visible(false);
            self.inspect(window, index);
            if let Some(cursor) = window
                .get_tiles()
                .iter()
                .position(|tile| tile.node_id == index as i32)
            {
                window.set_map_cursor(cursor as i32);
            }
        }
    }
    fn show_rows(&mut self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        self.inspected_group = None;
        if let Some(tree) = &self.tree {
            let (rows, limited) = tree.rows();
            let first = tree.offset(false) + 1;
            let last = tree.offset(false) + rows.len();
            window.set_has_previous_page(tree.offset(false) > 0);
            window.set_has_next_page(limited);
            window.set_rows(Rc::new(VecModel::from(rows)).into());
            window.set_tree_page(presentation::page(first, last).into());
            let (details, limited) = tree.details();
            let first = tree.offset(true) + 1;
            let last = tree.offset(true) + details.len();
            window.set_has_previous_detail_page(tree.offset(true) > 0);
            window.set_has_next_detail_page(limited);
            window.set_details(Rc::new(VecModel::from(details)).into());
            window.set_detail_page(presentation::page(first, last).into());
            window.set_current_path(tree.path().into());
            window.set_entry_path("".into());
            window.set_inspected_node(-1);
            window.set_detail_scroll_y(0.0);
            window.set_map_cursor(0);
            self.show_treemap(window);
            window.set_selection_summary(tree.selection_summary().into());
            window.set_has_parent(tree.has_parent());
            window.set_empty_message(tree.empty_message().into());
            window.set_sort_order(tree.sort() as i32);
            window.set_allocation_sort_available(tree.allocation_sort_available());
            window.set_owner_sort_available(tree.owner_data().is_some());
        }
        self.request_owners(window, None);
        self.apply_owner_names(window);
        self.directory_controls(window);
    }
    fn show_treemap(&self, window: &SortSizeWindow) {
        let _presentation = self.presentation.enter();
        if let Some(tree) = &self.tree {
            window.set_tiles(
                Rc::new(VecModel::from(tree.treemap(
                    f64::from(window.get_map_width()),
                    f64::from(window.get_map_height()),
                )))
                .into(),
            );
            window.set_treemap_note(tree.treemap_note().into());
        }
    }
    fn inspect(&mut self, window: &SortSizeWindow, index: usize) {
        self.directory_interacted();
        let _presentation = self.presentation.enter();
        if let Some(tree) = &mut self.tree
            && let Some(path) = tree.entry_path(index)
        {
            let row = tree.reveal_entry(index);
            let cursor = window.get_map_cursor();
            self.show_rows(window);
            window.set_map_cursor(cursor);
            window.set_detail_scroll_y(-(row as f32 * 34.0));
            window.set_inspected_node(index as i32);
            window.set_entry_path(path.into());
        }
    }
}

fn connect(window: &SortSizeWindow) -> Rc<RefCell<Controller>> {
    let controller = Rc::new(RefCell::new(Controller {
        presentation: presentation::Context::current(),
        system_locale: sys_locale::get_locale(),
        ..Controller::default()
    }));
    analysis_controller::connect(window, &controller);
    controller.borrow().show_preferences(window);
    controller.borrow().show_language(window);
    language_controller::register(window, &controller);
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_column(move |index, visible| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            if let Ok(index) = usize::try_from(index) {
                state.preferences.set_column(index, visible);
            }
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_units(move |decimal| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            state.preferences.units = if decimal {
                units::Units::Decimal
            } else {
                units::Units::Binary
            };
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_reset_display(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            state.preferences = display_preferences::Preferences {
                language: state.preferences.language,
                ..Default::default()
            };
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_resize_sidebar(move |ratio| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let ratio = ratio.clamp(100, 900) as u16;
            if state.preferences.sidebar_ratio != ratio {
                state.preferences.sidebar_ratio = ratio;
                // I/O remains on the coalescing preferences worker.
                let units = state.preferences.units;
                state.display_changed(&window, units);
            }
        }
    });
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        window.set_folder_picker_enabled(true);
        let weak = window.as_weak();
        let state = controller.clone();
        window.on_browse_folder(move || {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().browse(&window);
            }
        });
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        window.set_storage_enabled(true);
        let weak = window.as_weak();
        let state = controller.clone();
        window.on_refresh_volumes(move |remote| {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().refresh_sources(&window, remote);
            }
        });
        let weak = window.as_weak();
        let state = controller.clone();
        window.on_select_volume(move |index| {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().select_volume(&window, index);
            }
        });
    }
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_load_folder_details(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().load_directory(&window, true);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_select_top(move |rank| {
        if let Some(window) = weak.upgrade() {
            state.borrow().select_top(&window, rank);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_reveal_top(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().reveal_top(&window);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_path_edited(move || {
        let mut state = state.borrow_mut();
        state.path_input.edited();
        if let Some(window) = weak.upgrade() {
            window.set_selected_volume(-1);
        }
        if let Some(window) = weak.upgrade() {
            window.set_resolve_owner(false);
            state.resolution_changed(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_start_scan(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().start(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_cancel_scan(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().cancel(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_owner_resolution_changed(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().resolution_changed(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_owner(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().sort_owners(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_toggle_node(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
        {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.toggle(index as usize);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_change_page(move |details, next| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            if state.directory_page(&window, details, next) {
                return;
            }
            state.directory_interacted();
            if let Some(tree) = &mut state.tree {
                tree.change_page(details, next);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_select_node(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
        {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.select(index as usize);
            }
            state.show_rows(&window);
            state.load_directory(&window, false);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_go_parent(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            if state.directory_parent(&window) {
                return;
            }
            if let Some(tree) = &mut state.tree {
                tree.parent();
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_details(move |name| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            state.directory_interacted();
            state.cancel_owner_sort(&window);
            if let Some(tree) = &mut state.tree {
                tree.sort_by(name);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_allocated(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            state.directory_interacted();
            state.cancel_owner_sort(&window);
            if let Some(tree) = &mut state.tree {
                tree.sort_allocated();
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_dates(move |accessed| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            state.directory_interacted();
            state.cancel_owner_sort(&window);
            if let Some(tree) = &mut state.tree {
                tree.sort_dates(accessed);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_inspect_entry(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
        {
            state.borrow_mut().inspect(&window, index as usize);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_resize_treemap(move |_, _| {
        if let Some(window) = weak.upgrade() {
            state.borrow().show_treemap(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_activate_tile(move |index| {
        use slint::Model;
        if let Some(window) = weak.upgrade()
            && index >= 0
            && let Some(tile) = window.get_tiles().row_data(index as usize)
        {
            let mut state = state.borrow_mut();
            if tile.node_id >= 0 {
                if tile.directory {
                    if let Some(tree) = &mut state.tree {
                        tree.select(tile.node_id as usize);
                    }
                    state.show_rows(&window);
                    state.load_directory(&window, false);
                } else {
                    state.inspect(&window, tile.node_id as usize);
                }
            } else if tile.node_id == -2 && state.tree.as_ref().is_some_and(Tree::needs_details) {
                state.load_omitted(&window);
            } else {
                state.inspected_group = Some(tile.node_id);
                state.show_inspected_group(&window);
                window.set_inspected_node(-1);
            }
        }
    });
    controller
}

/// Construct a bare component using the scoped presentation language. Restores
/// the global bundle and managed sessions' effective language, without editing
/// their preferences. Use the session boundary for a managed product window.
pub fn new_window() -> Result<SortSizeWindow, slint::PlatformError> {
    let window = SortSizeWindow::new()?;
    let language = presentation::Context::current().language;
    let activation = locale::activate_bundled(language).map_err(|error| {
        slint::PlatformError::Other(format!("Translation activation failed: {error}"))
    })?;
    language_controller::publish_effective_language(activation);
    Ok(window)
}

/// Production startup boundary; locale injection keeps tests out of user config.
/// The last activation governs all managed windows on this UI thread. Existing
/// sessions retain their own preferences until an explicit selector action.
fn new_session(
    preferences: display_preferences::Preferences,
    system_locale: Option<String>,
) -> Result<(SortSizeWindow, Rc<RefCell<Controller>>), slint::PlatformError> {
    let window = SortSizeWindow::new()?;
    let activation =
        locale::activate_bundled(preferences.language.resolve(system_locale.as_deref())).map_err(
            |error| slint::PlatformError::Other(format!("Translation activation failed: {error}")),
        )?;
    let context = presentation::Context {
        language: activation.effective,
        generation: 0,
    };
    let _scope = context.enter();
    let controller = connect(&window);
    {
        let mut state = controller.borrow_mut();
        state.system_locale = system_locale;
        state.preferences = preferences;
    }
    language_controller::publish_effective_language(activation);
    Ok((window, controller))
}

pub fn run() -> Result<(), slint::PlatformError> {
    // Read before the first component; activate after construction but before
    // any rendering or Rust model publication. No English startup frame.
    #[cfg(any(target_os = "linux", windows))]
    let path = preferences_store::config_path();
    #[cfg(any(target_os = "linux", windows))]
    let (preferences, message) = path
        .as_deref()
        .map(preferences_store::load)
        .unwrap_or_else(|| (Default::default(), 7));
    #[cfg(not(any(target_os = "linux", windows)))]
    let preferences = display_preferences::Preferences::default();
    let (window, controller) = new_session(preferences, sys_locale::get_locale())?;
    let _presentation = controller.borrow().presentation.enter();
    let legal = LegalWindow::new()?;
    window.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_notices(
        tr!(include_str!("legal-notice-en.txt"))
            .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
            .into(),
    );
    legal.set_license_text(include_str!("../../../LICENSE").into());
    let weak_legal = legal.as_weak();
    window.on_show_legal(move || {
        if let Some(legal) = weak_legal.upgrade()
            && let Err(error) = legal.show()
        {
            eprintln!("Bunny and Size — Licences : {error}");
        }
    });
    let weak_legal = legal.as_weak();
    let weak_controller = Rc::downgrade(&controller);
    window.window().on_close_requested(move || {
        if let Some(controller) = weak_controller.upgrade() {
            controller.borrow_mut().analysis.invalidate();
        }
        if let Some(legal) = weak_legal.upgrade() {
            let _ = legal.hide();
        }
        slint::CloseRequestResponse::HideWindow
    });
    #[cfg(target_os = "android")]
    {
        window.set_scan_enabled(false);
        window.set_status(
            tr!("Android prototype: SAF/MediaStore selection is not yet integrated.").into(),
        );
    }
    {
        let mut state = controller.borrow_mut();
        state.legal = Some(legal.as_weak());
        #[cfg(any(target_os = "linux", windows))]
        {
            window.set_preference_state(message);
            if let Some(path) = path {
                match preferences_store::Store::start(path) {
                    Ok(store) => state.preference_store = Some(store),
                    Err(_) => window.set_preference_state(6),
                }
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    if let Ok(path) = std::env::current_dir() {
        window.set_scan_path(controller.borrow_mut().path_input.select(path).into());
    }
    #[cfg(any(target_os = "linux", windows))]
    controller.borrow_mut().refresh_sources(&window, false);
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    #[cfg(windows)]
    let mut placement_attempts = 0;
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        if let Some(window) = weak.upgrade() {
            // Native handles can arrive after the first event-loop iteration.
            // Reuse the existing poll and stop after success or ten attempts.
            #[cfg(windows)]
            if placement_attempts < 10 {
                placement_attempts += 1;
                if window_placement::fit_to_work_area(window.window()) {
                    placement_attempts = 10;
                }
            }
            state.borrow_mut().poll(&window);
        }
    });
    let result = window.run();
    timer.stop();
    controller.borrow_mut().analysis.invalidate();
    controller.borrow_mut().task = None;
    controller.borrow_mut().scan_worker = None;
    controller.borrow_mut().owner_task = None;
    #[cfg(any(target_os = "linux", windows))]
    {
        controller.borrow_mut().sources = volume_view::Sources::default();
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        controller.borrow_mut().preference_store = None;
    }
    result
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    if let Err(error) = slint::android::init(app) {
        eprintln!("Initialisation Android impossible : {error}");
        return;
    }
    if let Err(error) = run() {
        eprintln!("Bunny and Size : {error}");
    }
}

#[cfg(test)]
mod tests {
    mod analysis_gui_tests;
    mod branding_gui_tests;
    mod display_gui_tests;
    mod i18n_gui_tests;
    mod language_gui_tests;
    mod ondemand_gui_tests;
    mod owner_gui_tests;
    mod progressive_gui_tests;
    mod temporal_gui_tests;
    #[cfg(windows)]
    mod windows_redraw_product;
    use super::*;
    #[cfg(any(target_os = "linux", windows))]
    mod volume_gui_tests;
    use slint::Model;
    use slint::platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    struct TestPlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn allocation_checkbox_applies_only_to_the_next_explicit_scan() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
        let window = new_window().unwrap();
        let controller = connect(&window);
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("file"), [1; 4096]).unwrap();
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        assert!(!window.get_collect_allocation());
        for enabled in [false, true] {
            window.set_collect_allocation(enabled);
            assert!(!window.get_busy());
            assert!(controller.borrow().task.is_none());
            window.invoke_start_scan();
            let start = Instant::now();
            while window.get_busy() {
                controller.borrow_mut().poll(&window);
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
            let row = window.get_details().row_data(0).unwrap();
            assert_eq!(row.allocated == "Non collectée", !enabled);
            assert_eq!(window.get_allocation_sort_available(), enabled);
            window.invoke_sort_allocated();
            assert_eq!(window.get_sort_order(), if enabled { 4 } else { 0 });
            window.invoke_sort_allocated();
            assert_eq!(window.get_sort_order(), if enabled { 5 } else { 0 });
            assert!(row.modified.ends_with(" UTC"));
            window.set_collect_allocation(!enabled);
            assert_eq!(
                window.get_details().row_data(0).unwrap().allocated,
                row.allocated
            );
            assert_eq!(window.get_allocation_sort_available(), enabled);
            assert!(controller.borrow().task.is_none());
        }
    }

    #[test]
    fn gui_callbacks_scan_expand_and_recover_from_a_missing_root() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
        let window = new_window().unwrap();
        let controller = connect(&window);
        // A worker publishing progress must never make the UI wait for its lock.
        let mailbox = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox {
            directory: None,
            patch: None,
            progress: Some("progression différée".into()),
            completion: None,
        }));
        controller.borrow_mut().task = Some(Task {
            cancellation: Default::default(),
            mailbox: mailbox.clone(),
        });
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = mailbox.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let previous = window.get_summary();
        controller.borrow_mut().poll(&window);
        assert_eq!(window.get_summary(), previous);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        controller.borrow_mut().poll(&window);
        assert_eq!(window.get_summary(), "progression différée");
        controller.borrow_mut().task = None;
        window.invoke_start_scan();
        assert!(!window.get_busy());
        let fixture = TempDir::new().unwrap();
        fs::create_dir(fixture.path().join("été")).unwrap();
        fs::create_dir(fixture.path().join("été/nested")).unwrap();
        fs::write(fixture.path().join("été/data"), b"12345").unwrap();
        fs::write(fixture.path().join("empty"), []).unwrap();
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.set_worker_choice(2);
        window.invoke_start_scan();
        assert!(window.get_busy());
        window.invoke_start_scan();
        wait(&window, &controller);
        assert_eq!(window.get_status(), "Analyse terminée");
        assert!(window.get_summary().contains("5 octets"));
        assert!(window.get_summary().contains("4 dossiers simultanés"));
        // Language storage is independent from display reset and scan state.
        let language = locale::LanguagePreference::Explicit(locale::Language::German);
        controller.borrow_mut().preferences.language = language;
        let generation = controller.borrow().generation;
        let summary = window.get_summary();
        window.invoke_reset_display();
        assert_eq!(controller.borrow().preferences.language, language);
        assert_eq!(controller.borrow().generation, generation);
        assert!(controller.borrow().task.is_none());
        assert_eq!(window.get_summary(), summary);

        assert_eq!(window.get_rows().row_count(), 2);
        assert_eq!(window.get_details().row_count(), 2);
        for row in window.get_details().iter() {
            assert!(row.modified.ends_with(" UTC"));
            assert!(row.accessed.ends_with(" UTC"));
        }
        assert!(!window.get_has_parent());
        assert_eq!(window.get_top_rows().row_count(), 2);
        assert_eq!(window.get_top_rows().row_data(0).unwrap().name, "data");
        window.set_top_visible(true);
        window.invoke_select_top(0);
        assert!(window.get_top_path().ends_with("data"));
        assert!(window.get_top_retained());
        window.invoke_reveal_top();
        assert!(!window.get_top_visible());
        assert!(window.get_current_path().ends_with("été"));
        assert!(window.get_entry_path().ends_with("data"));
        assert!(window.get_inspected_node() >= 0);
        window.invoke_go_parent();
        let folder = window.get_rows().row_data(1).unwrap();
        assert_eq!(folder.name, "été");
        assert_eq!(window.get_details().row_data(0).unwrap().files, "1");
        assert_eq!(window.get_details().row_data(0).unwrap().directories, "1");
        assert!(
            window
                .get_selection_summary()
                .contains("fichiers directs : 1")
        );
        assert!(
            window
                .get_selection_summary()
                .contains("fichiers récursifs : 2")
        );
        window.invoke_toggle_node(folder.node_id);
        assert_eq!(window.get_rows().row_count(), 3);
        assert_eq!(window.get_rows().row_data(2).unwrap().name, "nested");
        window.set_map_width(800.0);
        window.set_map_height(300.0);
        controller.borrow().show_treemap(&window);
        let tile = window
            .get_tiles()
            .iter()
            .position(|t| t.node_id == folder.node_id)
            .unwrap();
        window.invoke_activate_tile(tile as i32);
        assert!(window.get_has_parent());
        assert!(window.get_current_path().ends_with("été"));
        assert_eq!(window.get_details().row_count(), 2);
        let nested = window.get_details().row_data(0).unwrap();
        let file = window.get_details().row_data(1).unwrap();
        assert!(!file.path.is_empty());
        let context = window.get_current_path();
        window.invoke_inspect_entry(nested.node_id);
        assert_eq!(window.get_entry_path(), nested.path);
        assert_eq!(window.get_current_path(), context);
        window.invoke_inspect_entry(file.node_id);
        assert_eq!(window.get_entry_path(), file.path);
        assert_eq!(file.name, "data");
        assert_eq!(file.files, "—");
        assert_eq!(nested.files, "0");
        assert!(
            window
                .get_selection_summary()
                .contains("sous-dossiers directs : 1")
        );
        assert_eq!(file.proportion, "100,0 %");
        let file_tile = window
            .get_tiles()
            .iter()
            .position(|t| t.node_id == file.node_id)
            .unwrap();
        window.invoke_activate_tile(file_tile as i32);
        assert!(window.get_entry_path().ends_with("data"));
        assert_eq!(window.get_inspected_node(), file.node_id);
        window.invoke_inspect_entry(file.node_id);
        assert_eq!(window.get_inspected_node(), file.node_id);
        window.invoke_activate_tile(-1);
        window.invoke_activate_tile(1000);
        assert_eq!(window.get_inspected_node(), file.node_id);
        window.invoke_select_node(nested.node_id);
        assert_eq!(window.get_details().row_count(), 0);
        assert_eq!(window.get_tiles().row_count(), 0);
        assert!(window.get_treemap_note().contains("Aucune taille positive"));
        assert!(window.get_empty_message().contains("vide"));
        window.invoke_go_parent();
        assert_eq!(window.get_details().row_count(), 2);
        assert!(window.get_entry_path().is_empty());
        window.invoke_sort_details(true);
        assert_eq!(window.get_sort_order(), 2);
        window.invoke_sort_details(true);
        assert_eq!(window.get_sort_order(), 3);
        window.invoke_go_parent();
        assert!(!window.get_has_parent());
        assert_eq!(window.get_details().row_count(), 2);
        window.invoke_toggle_node(0);
        assert_eq!(window.get_rows().row_count(), 1);
        window.set_scan_path(fixture.path().join("missing").to_str().unwrap().into());
        window.invoke_start_scan();
        assert_eq!(window.get_top_rows().row_count(), 0);
        assert_eq!(window.get_top_selected(), -1);
        assert!(!window.get_top_retained());
        assert_eq!(window.get_rows().row_count(), 0);
        assert_eq!(window.get_tiles().row_count(), 0);
        assert_eq!(window.get_details().row_count(), 0);
        assert!(window.get_current_path().is_empty());
        wait(&window, &controller);
        assert_eq!(window.get_status(), "Analyse impossible.");
        assert!(!window.get_errors().is_empty());
        assert!(!window.get_busy());
        assert!(!window.get_cancelling());
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.invoke_start_scan();
        let token = controller
            .borrow()
            .task
            .as_ref()
            .unwrap()
            .cancellation
            .clone();
        window.invoke_cancel_scan();
        assert!(token.is_cancelled());
        assert!(window.get_cancelling());
        wait(&window, &controller);
        assert!(!window.get_cancelling());
        // A tiny scan may finish before the click; completion stays authoritative.
        window.invoke_start_scan();
        wait(&window, &controller);
        assert_eq!(window.get_status(), "Analyse terminée");
        top_callbacks_regressions(&window, &controller);
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        folder_selection_regressions(&window, &controller);
        if let Ok(destination) = std::env::var("SORTSIZE_GUI_SCREENSHOT") {
            let path = std::env::var("SORTSIZE_GUI_PREVIEW_ROOT")
                .unwrap_or_else(|_| fixture.path().to_string_lossy().into_owned());
            window.set_scan_path(path.into());
            window.invoke_start_scan();
            wait(&window, &controller);
            if let Ok(top_destination) = std::env::var("SORTSIZE_TOP_SCREENSHOT") {
                window.set_top_visible(true);
                window.invoke_select_top(0);
                capture(&window, &adapter, &top_destination, (960, 600));
                window.set_top_visible(false);
            }
            capture(&window, &adapter, &destination, (1200, 760));
            if let Ok(destination) = std::env::var("SORTSIZE_GUI_COMPACT_SCREENSHOT") {
                capture(&window, &adapter, &destination, (960, 600));
                if let Ok(destination) = std::env::var("SORTSIZE_DETAIL_SCREENSHOT") {
                    window.set_detail_scroll_x(-600.0);
                    capture(&window, &adapter, &destination, (960, 600));
                }
            }
        }
    }

    fn top_callbacks_regressions(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("not-retained"), b"1234").unwrap();
        let inventory = sortsize_core::scan_inventory(
            fixture.path(),
            &Default::default(),
            &Default::default(),
            1,
            |_| {},
        )
        .unwrap();
        let (tree, _) = Tree::new(inventory);
        window.set_top_rows(Rc::new(VecModel::from(tree.top_rows.clone())).into());
        controller.borrow_mut().tree = Some(tree);
        controller.borrow_mut().show_rows(window);
        window.set_top_visible(true);
        window.invoke_select_top(0);
        assert!(window.get_top_path().ends_with("not-retained"));
        assert!(!window.get_top_retained());
        let previous_path = window.get_current_path();
        window.invoke_reveal_top();
        assert!(window.get_top_visible());
        assert_eq!(window.get_current_path(), previous_path);
        assert_eq!(window.get_details().row_count(), 0);
        window.invoke_select_top(-1);
        window.invoke_select_top(100);
        assert_eq!(window.get_top_selected(), 0);
        // Revealing a retained file must also place keyboard focus on its tile.
        fs::write(fixture.path().join("larger"), b"12345678").unwrap();
        let inventory = sortsize_core::scan_inventory(
            fixture.path(),
            &Default::default(),
            &Default::default(),
            100,
            |_| {},
        )
        .unwrap();
        let (tree, _) = Tree::new(inventory);
        window.set_top_rows(Rc::new(VecModel::from(tree.top_rows.clone())).into());
        controller.borrow_mut().tree = Some(tree);
        window.set_map_width(600.0);
        window.set_map_height(120.0);
        controller.borrow_mut().show_rows(window);
        window.invoke_select_top(1);
        window.invoke_reveal_top();
        assert_eq!(
            window
                .get_tiles()
                .row_data(window.get_map_cursor() as usize)
                .unwrap()
                .node_id,
            window.get_inspected_node()
        );
        window.set_top_visible(false);
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn folder_selection_regressions(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        use folder_picker::FolderPicker;
        let fixture = TempDir::new().unwrap();
        let mut path = fixture.path().join("été 日本語 avec espaces");
        for _ in 0..8 {
            path = path.join("long-directory-name-0123456789");
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("data"), b"1234567").unwrap();
        let previous_path = window.get_scan_path();
        let previous_summary = window.get_summary();
        let previous_rows = window.get_rows().row_count();
        let (tx, rx) = std::sync::mpsc::channel();
        controller
            .borrow_mut()
            .begin_browse(window, |_| FolderPicker::start(move || rx.recv().unwrap()));
        assert!(window.get_choosing_folder());
        // Both commands are guarded even if invoked outside their disabled buttons.
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("duplicate dialog"));
        window.invoke_start_scan();
        assert!(!window.get_busy());
        controller.borrow_mut().poll(window);
        assert_eq!(window.get_scan_path(), previous_path);
        tx.send(Some(path.clone())).unwrap();
        wait_for_folder(window, controller);
        assert_eq!(window.get_scan_path(), path.to_string_lossy().as_ref());
        assert_eq!(
            controller
                .borrow()
                .path_input
                .resolve(window.get_scan_path().as_str()),
            path
        );
        assert_eq!(window.get_summary(), previous_summary);
        assert_eq!(window.get_rows().row_count(), previous_rows);
        assert!(!window.get_busy());
        // Cancellation keeps the native source, text and existing inventory.
        controller
            .borrow_mut()
            .begin_browse(window, |_| FolderPicker::start(|| None));
        wait_for_folder(window, controller);
        assert_eq!(
            controller
                .borrow()
                .path_input
                .resolve(window.get_scan_path().as_str()),
            path
        );
        assert_eq!(window.get_summary(), previous_summary);
        window.invoke_start_scan();
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("dialog during scan"));
        wait(window, controller);
        assert!(window.get_summary().contains("7 octets"));
        assert_eq!(window.get_status(), "Analyse terminée");
        // A failed thread start must leave the commands usable and the source intact.
        controller
            .borrow_mut()
            .begin_browse(window, |_| Err(std::io::Error::other("test failure")));
        assert!(!window.get_choosing_folder());
        assert!(window.get_errors().contains("test failure"));
        // A worker that exits without sending also releases the dialog guard.
        controller.borrow_mut().begin_browse(window, |_| {
            FolderPicker::start(|| panic!("test picker failure"))
        });
        wait_for_folder(window, controller);
        assert!(window.get_errors().contains("sans résultat"));
        // Disabled platforms/scan sources cannot open a dialog.
        window.set_scan_enabled(false);
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("disabled scan"));
        window.set_scan_enabled(true);
        window.set_folder_picker_enabled(false);
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("disabled picker"));
        window.set_folder_picker_enabled(true);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            use std::os::unix::fs::PermissionsExt;
            let native = fixture
                .path()
                .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
            fs::create_dir(&native).unwrap();
            fs::write(native.join("data"), b"12345678901").unwrap();
            let selected = native.clone();
            controller
                .borrow_mut()
                .begin_browse(window, |_| FolderPicker::start(move || Some(selected)));
            wait_for_folder(window, controller);
            // Cancel after an unrepresentable selection must preserve its exact bytes.
            controller
                .borrow_mut()
                .begin_browse(window, |_| FolderPicker::start(|| None));
            wait_for_folder(window, controller);
            window.invoke_start_scan();
            wait(window, controller);
            assert_eq!(window.get_status(), "Analyse terminée");
            assert!(window.get_summary().contains("11 octets"));
            assert_eq!(fs::read(native.join("data")).unwrap(), b"12345678901");
            // User editing invalidates the native path even if undo restores the text.
            window.invoke_path_edited();
            window.invoke_start_scan();
            wait(window, controller);
            assert_eq!(window.get_status(), "Analyse impossible.");
            let blocked = fixture.path().join("inaccessible");
            fs::create_dir(&blocked).unwrap();
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
            let denied = fs::read_dir(&blocked).is_err();
            let selected = blocked.clone();
            controller
                .borrow_mut()
                .begin_browse(window, |_| FolderPicker::start(move || Some(selected)));
            wait_for_folder(window, controller);
            window.invoke_start_scan();
            wait(window, controller);
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
            if denied {
                assert!(window.get_status().contains("Analyse partielle"));
                assert!(!window.get_errors().is_empty());
                assert!(
                    window
                        .get_selection_summary()
                        .contains("fichiers récursifs : ≥ 0")
                );
            }
        }
        // Changing the text programmatically also cannot reuse a stale native value.
        window.set_scan_path(path.to_string_lossy().as_ref().into());
        window.invoke_path_edited();
        window.invoke_start_scan();
        wait(window, controller);
        assert_eq!(window.get_status(), "Analyse terminée");
        assert!(window.get_summary().contains("7 octets"));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn wait_for_folder(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while window.get_choosing_folder() {
            assert!(Instant::now() < deadline, "folder dialog did not finish");
            controller.borrow_mut().poll(window);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while window.get_busy() {
            assert!(Instant::now() < deadline, "worker did not finish");
            controller.borrow_mut().poll(window);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn capture(
        window: &SortSizeWindow,
        adapter: &MinimalSoftwareWindow,
        destination: &str,
        (width, height): (u32, u32),
    ) {
        use slint::Rgb8Pixel;
        use std::io::Write;
        window.show().unwrap();
        adapter.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        }));
        let mut output = fs::File::create(destination).unwrap();
        output
            .write_all(format!("P6\n{width} {height}\n255\n").as_bytes())
            .unwrap();
        for pixel in pixels {
            output.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
        }
        window.hide().unwrap();
    }
}
