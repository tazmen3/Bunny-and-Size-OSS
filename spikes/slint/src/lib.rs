#[macro_use]
pub mod presentation;
mod catalog_generated;
#[cfg(feature = "benchmarks")]
pub mod i18n_harness;
#[cfg(feature = "benchmarks")]
pub mod measure;

mod allocation_label;
mod analysis_controller;
#[cfg(any(target_os = "linux", windows))]
mod auto_analysis;
mod current_folder_summary;
mod date_label;
mod directory_controller;
mod directory_worker;
mod display_preferences;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod folder_picker;
mod hover;
mod language_controller;
pub mod locale;
mod number_format;
mod owner_controller;
mod owner_view;
mod path_input;
#[cfg(any(target_os = "linux", windows))]
mod preferences_store;
#[cfg(any(target_os = "linux", windows))]
mod source_dialog;
#[cfg(any(target_os = "linux", windows))]
mod source_registry;
mod tree;
mod treemap;
mod units;
#[cfg(any(target_os = "linux", windows))]
mod volume_view;
#[cfg(windows)]
mod window_placement;
#[cfg(windows)]
mod windows_context_menu;
mod worker;
#[cfg(any(target_os = "linux", windows))]
mod workshop;

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
    registry: source_registry::Registry,
    #[cfg(any(target_os = "linux", windows))]
    source_dialog: Option<AddSourceWindow>,
    #[cfg(any(target_os = "linux", windows))]
    source_reconnect: Option<Rc<dyn Fn(source_registry::Source)>>,
    #[cfg(any(target_os = "linux", windows))]
    source_operations: source_dialog::Operations,
    #[cfg(any(target_os = "linux", windows))]
    source_save_result: Option<bool>,
    #[cfg(any(target_os = "linux", windows))]
    source_shares: Vec<std::path::PathBuf>,
    #[cfg(any(target_os = "linux", windows))]
    source_picker_serial: u64,
    #[cfg(any(target_os = "linux", windows))]
    source_picker: Option<folder_picker::FolderPicker>,
    #[cfg(any(target_os = "linux", windows))]
    source_path_input: PathInput,
    #[cfg(any(target_os = "linux", windows))]
    auto_analysis: auto_analysis::AutoAnalysis,
    #[cfg(any(target_os = "linux", windows))]
    last_auto_selection: Option<auto_analysis::LastSelection>,
    #[cfg(any(target_os = "linux", windows))]
    restored_selection: Option<auto_analysis::LastSelection>,
    #[cfg(any(target_os = "linux", windows))]
    selection_store: Option<auto_analysis::SelectionStore>,
    #[cfg(any(target_os = "linux", windows))]
    pending_remember: Option<std::path::PathBuf>,
    #[cfg(any(target_os = "linux", windows))]
    pending_auto: Option<(std::path::PathBuf, Option<directory_worker::Preflight>)>,
    #[cfg(any(target_os = "linux", windows))]
    source_publication: std::cell::Cell<i32>,
    #[cfg(any(target_os = "linux", windows))]
    scan_volume: Option<(
        sortsize_core::volumes::VolumeIdentity,
        sortsize_core::volumes::VolumeMount,
    )>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    folder_picker: Option<folder_picker::FolderPicker>,
    #[cfg(any(target_os = "linux", windows))]
    duplicate_temporary_picker: Option<folder_picker::FolderPicker<Result<std::path::PathBuf, ()>>>,
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
        window.set_memory_choice(
            [
                "auto",
                "50000",
                "250000",
                "1000000",
                "5000000",
                "unlimited-count",
            ]
            .iter()
            .position(|key| *key == self.preferences.memory.key())
            .unwrap_or(0) as i32,
        );
        window.set_theme_choice(i32::from(self.preferences.theme));
        window.set_dense(self.preferences.dense);
        window.set_duplicate_profile_choice(display_preferences::profile_index(
            self.preferences.duplicate_profile,
        ));
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
        window.set_decimal_units(self.preferences.units.is_decimal());
        window.set_size_unit_choice(i32::from(self.preferences.units.size_unit()));
        window.set_sidebar_ratio(i32::from(self.preferences.sidebar_ratio));
        window.set_sidebar_width(f32::from(self.preferences.sidebar_width));
        window.set_sidebar_collapsed(self.preferences.sidebar_collapsed);
        window.set_treemap_mode(i32::from(self.preferences.treemap_mode));
        window.set_treemap_height_choice(i32::from(self.preferences.treemap_height));
    }
    fn display_changed(&mut self, window: &SortSizeWindow, previous: units::Units) {
        let _presentation = self.presentation.enter();
        self.show_preferences(window);
        if previous != self.preferences.units {
            self.update_units(window);
        }
        self.save_preferences(window);
    }
    fn save_preferences(&mut self, window: &SortSizeWindow) {
        #[cfg(any(target_os = "linux", windows))]
        if let Some(store) = &mut self.preference_store {
            store.submit(self.preferences.clone());
            window.set_preference_state(1);
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = window;
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
            window.set_current_folder_summary(tree.current_folder_summary().into());
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
        self.show_disconnected_sources();
        window.set_storage_loading(self.sources.loading());
        window.set_storage_status(self.sources.status().into());
        if let Some(prepared) = &self.sources.current {
            let Some(token) = self.source_publication.get().checked_add(1) else {
                return;
            };
            self.source_publication.set(token);
            window.set_storage_token(token);
            let mut rows = prepared.rows_for(self.preferences.units);
            for (row, volume) in rows.iter_mut().zip(&prepared.discovery.volumes) {
                if let Some(mount) = volume.mounts.first() {
                    row.network |= self
                        .registry
                        .find(&mount.path)
                        .is_some_and(|s| s.kind != source_registry::Kind::Local);
                    row.disconnected = row.network && self.registry.is_disconnected(&mount.path);
                    if row.disconnected {
                        row.state = tr!("Disconnected deliberately").into();
                    }
                    self.sources.apply_connection_state(&mount.path, row);
                }
            }
            window.set_volumes(Rc::new(VecModel::from(rows)).into());
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
    fn source_from_dialog(
        &self,
        dialog: &AddSourceWindow,
    ) -> std::io::Result<source_registry::Source> {
        if dialog.get_source_kind() == 3 {
            // A server is a logical source, not a relative filesystem path.
            return source_registry::Source::new(
                3,
                std::path::PathBuf::from(dialog.get_source_path().as_str()),
                dialog.get_display_name().as_str(),
                None,
            );
        }
        #[cfg(target_os = "linux")]
        if dialog.get_source_kind() == 0 {
            let input = dialog.get_source_path();
            let input = input.as_str();
            if input.starts_with("smb://") || input.starts_with(r"\\") || !input.starts_with('/') {
                let target = self
                    .smb_target_from_dialog(dialog)
                    .map_err(|_| std::io::ErrorKind::InvalidInput)?;
                return source_registry::Source::new(
                    0,
                    target.share.ok_or(std::io::ErrorKind::WouldBlock)?,
                    dialog.get_display_name().as_str(),
                    self.sources.current.as_ref().map(|p| &p.discovery),
                );
            }
        }
        let path = self
            .source_path_input
            .resolve(dialog.get_source_path().as_str());
        #[cfg(windows)]
        let path = if dialog.get_source_kind() == 0
            && !matches!(path.components().next(), Some(std::path::Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
            && path.to_str().is_some()
        {
            let target = self
                .smb_target_from_dialog(dialog)
                .map_err(|_| std::io::ErrorKind::InvalidInput)?;
            let normalized = target.share.ok_or(std::io::ErrorKind::WouldBlock)?;
            if path.is_absolute() && sortsize_core::volumes::unc_source(&path).is_ok() {
                path
            } else {
                normalized
            }
        } else {
            path
        };
        source_registry::Source::new(
            dialog.get_source_kind(),
            path,
            dialog.get_display_name().as_str(),
            self.sources.current.as_ref().map(|p| &p.discovery),
        )
    }
    #[cfg(any(target_os = "linux", windows))]
    fn smb_target_from_dialog(
        &self,
        dialog: &AddSourceWindow,
    ) -> Result<sortsize_core::volumes::smb::Target, sortsize_core::volumes::smb::SmbError> {
        #[allow(unused_mut)]
        let mut target =
            sortsize_core::volumes::smb::parse_target(dialog.get_source_path().as_str())?;
        #[cfg(windows)]
        let native = self
            .source_path_input
            .resolve(dialog.get_source_path().as_str());
        #[cfg(windows)]
        if target.share.is_some()
            && native.is_absolute()
            && sortsize_core::volumes::unc_source(&native).is_ok()
        {
            target.share = Some(native);
        }
        Ok(target)
    }
    #[cfg(any(target_os = "linux", windows))]
    fn add_source(&mut self, window: &SortSizeWindow, dialog: &AddSourceWindow) {
        if self.source_operations.busy() || self.source_picker.is_some() {
            return;
        }
        let source = match self.source_from_dialog(dialog) {
            Ok(source) => source,
            Err(error) => {
                dialog.set_result_state(if error.kind() == std::io::ErrorKind::WouldBlock {
                    7
                } else {
                    4
                });
                return;
            }
        };
        match self.registry.add(source) {
            Ok(source_registry::AddOutcome::Added) => {}
            Ok(source_registry::AddOutcome::ReconnectRequired(source)) => {
                self.propose_source_reconnect(dialog, &source, 21);
                return;
            }
            Ok(source_registry::AddOutcome::ParentDisconnected(parent)) => {
                self.propose_source_reconnect(dialog, &parent, 22);
                return;
            }
            Err(_) => {
                dialog.set_result_state(4);
                return;
            }
        }
        self.sources.set_registered(&self.registry.entries);
        self.show_sources(window);
        dialog.set_result_state(if self.registry.saving_available() {
            5
        } else {
            6
        });
        // Deliberately preserve scan-path, active acquisition, pending local
        // intention, and selected inventory. Selecting and Analyze are separate.
    }
    #[cfg(any(target_os = "linux", windows))]
    fn propose_source_reconnect(
        &self,
        dialog: &AddSourceWindow,
        source: &source_registry::Source,
        state: i32,
    ) {
        self.show_disconnected_sources();
        let index = self
            .registry
            .disconnected_sources()
            .iter()
            .position(|entry| entry.path == source.path);
        dialog.set_selected_disconnected(index.map(|i| i as i32).unwrap_or(-1));
        dialog.set_reconnect_status("".into());
        dialog.set_result_state(state);
        dialog.set_disconnected_view(true);
        // The existing Reconnect button is the explicit user decision. Merely
        // adding a disabled source never checks access or changes exclusions.
    }
    #[cfg(any(target_os = "linux", windows))]
    fn check_source(&mut self, dialog: &AddSourceWindow) {
        if self.source_operations.busy() || self.source_picker.is_some() {
            return;
        }
        let source = match self.source_from_dialog(dialog) {
            Ok(source) => source,
            Err(error) => {
                dialog.set_result_state(if error.kind() == std::io::ErrorKind::WouldBlock {
                    7
                } else {
                    4
                });
                return;
            }
        };
        let started = self.source_operations.start(move || {
            let state = match source_registry::preflight(&source) {
                Ok(()) => 1,
                Err(error) => source_dialog::access_state(&error),
            };
            source_dialog::Outcome::state(state)
        });
        match started {
            Ok(true) => {
                dialog.set_busy(true);
                dialog.set_operation(1);
                dialog.set_result_state(0);
            }
            Ok(false) => {}
            Err(_) => dialog.set_result_state(2),
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn source_input_changed(&mut self, dialog: &AddSourceWindow, edited: bool) {
        if edited {
            self.source_path_input.edited();
        }
        self.source_operations.invalidate();
        self.source_shares.clear();
        dialog.set_shares(ModelRc::default());
        dialog.set_selected_share(-1);
        dialog.set_shares_limited(false);
        dialog.set_result_state(0);
        dialog.set_smb_server_only(false);
        #[cfg(any(target_os = "linux", windows))]
        if matches!(dialog.get_source_kind(), 0 | 3)
            && let Ok(target) =
                sortsize_core::volumes::smb::parse_target(dialog.get_source_path().as_str())
        {
            dialog.set_smb_server_only(target.share.is_none());
            if target.share.is_none() {
                dialog.set_result_state(7);
            }
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn smb_source_operation(&mut self, dialog: &AddSourceWindow, connect: bool) {
        if !matches!(dialog.get_source_kind(), 0 | 3)
            || self.source_operations.busy()
            || self.source_picker.is_some()
        {
            return;
        }
        let target = match self.smb_target_from_dialog(dialog) {
            Ok(target) => target,
            Err(error) => {
                dialog.set_result_state(source_dialog::native_state(error.status));
                return;
            }
        };
        #[cfg(windows)]
        let owner = source_dialog::owner_handle(dialog.window());
        #[cfg(target_os = "linux")]
        let owner = 0;
        self.source_shares.clear();
        dialog.set_shares(ModelRc::default());
        dialog.set_selected_share(-1);
        dialog.set_shares_limited(false);
        let started = self.source_operations.start(move || {
            if connect {
                return source_dialog::Outcome::state(
                    match sortsize_core::volumes::smb::connect_target(&target, owner) {
                        Ok(()) => 13,
                        Err(error) => source_dialog::native_state(error.status),
                    },
                );
            }
            match sortsize_core::volumes::smb::enumerate_shares(&target) {
                Ok(list) => {
                    let limited = list.limited || list.shares.len() > 256;
                    let shares: Vec<_> =
                        list.shares.into_iter().take(256).map(|s| s.path).collect();
                    source_dialog::Outcome {
                        state: if shares.is_empty() { 9 } else { 18 },
                        shares: Some(shares),
                        limited,
                    }
                }
                Err(error) => {
                    source_dialog::Outcome::state(source_dialog::enumeration_state(error.status))
                }
            }
        });
        match started {
            Ok(true) => {
                dialog.set_busy(true);
                dialog.set_operation(if connect { 3 } else { 2 });
                dialog.set_result_state(0);
            }
            Ok(false) => {}
            Err(_) => dialog.set_result_state(16),
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn select_source_share(&mut self, dialog: &AddSourceWindow, index: i32) {
        if dialog.get_source_kind() == 3 {
            return;
        }
        if self.source_operations.busy() || self.source_picker.is_some() {
            return;
        }
        let Some(path) = source_dialog::share_index(index)
            .and_then(|i| self.source_shares.get(i))
            .cloned()
        else {
            return;
        };
        self.source_operations.invalidate();
        dialog.set_source_path(self.source_path_input.select(path).into());
        dialog.set_smb_server_only(false);
        dialog.set_selected_share(index);
        dialog.set_result_state(0);
    }
    #[cfg(any(target_os = "linux", windows))]
    fn poll_source_dialog(&mut self) {
        if let Some(result) = self.source_operations.poll()
            && let Some(dialog) = &self.source_dialog
        {
            dialog.set_busy(false);
            dialog.set_operation(0);
            if let Some(outcome) = result {
                dialog.set_result_state(outcome.state);
                if let Some(shares) = outcome.shares {
                    let labels = std::iter::once(dialog.get_share_placeholder())
                        .chain(
                            shares
                                .iter()
                                .map(|p| p.to_string_lossy().into_owned().into()),
                        )
                        .collect::<Vec<slint::SharedString>>();
                    self.source_shares = shares;
                    dialog.set_shares(Rc::new(VecModel::from(labels)).into());
                    dialog.set_selected_share(0);
                    dialog.set_shares_limited(outcome.limited);
                }
            }
        }
        if let Some(result) = self.source_picker.as_ref().and_then(|p| p.poll()) {
            self.source_picker = None;
            if let Some(dialog) = &self.source_dialog {
                dialog.set_busy(false);
                if self.source_picker_serial == self.source_operations.revision()
                    && let Ok(Some(path)) = result
                {
                    dialog.set_source_path(self.source_path_input.select(path).into());
                    dialog.set_result_state(0);
                }
            }
        }
        if let Some(saved) = self.registry.poll_saved() {
            self.source_save_result = Some(saved);
            if !saved
                && let Some(dialog) = &self.source_dialog
                && matches!(dialog.get_result_state(), 5 | 6)
                && self
                    .source_from_dialog(dialog)
                    .is_ok_and(|source| !self.registry.is_disconnected(&source.path))
            {
                dialog.set_result_state(6);
            }
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn show_disconnected_sources(&self) {
        let Some(dialog) = &self.source_dialog else {
            return;
        };
        // This window survives closing. A later disconnection from the main
        // window invalidates its old registration success, even while hidden.
        if matches!(dialog.get_result_state(), 5 | 6)
            && self
                .source_from_dialog(dialog)
                .is_ok_and(|source| self.registry.is_disconnected(&source.path))
        {
            dialog.set_result_state(0);
            dialog.set_reconnect_status("".into());
        }
        let labels = self
            .registry
            .disconnected_sources()
            .iter()
            .map(|source| {
                let path = source.path.to_string_lossy();
                if source.label.is_empty() {
                    path.into_owned()
                } else {
                    format!("{} · {path}", source.label)
                }
                .into()
            })
            .collect::<Vec<slint::SharedString>>();
        let current = dialog.get_disconnected_sources();
        if current.row_count() != labels.len()
            || labels
                .iter()
                .enumerate()
                .any(|(i, label)| current.row_data(i).as_ref() != Some(label))
        {
            dialog.set_disconnected_sources(Rc::new(VecModel::from(labels)).into());
            dialog.set_selected_disconnected(-1);
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn remember_selection(&mut self, root: &std::path::Path) {
        if self.sources.current.is_none() {
            // An explicit choice may precede discovery. Keep that intention,
            // and classify it once without ever scheduling another scan.
            self.pending_remember = Some(root.to_owned());
            if let Some(store) = &mut self.selection_store {
                store.clear();
            }
            return;
        }
        self.pending_remember = None;
        let selection = self.sources.current.as_ref().and_then(|prepared| {
            (!self
                .registry
                .find(root)
                .is_some_and(|s| s.kind != source_registry::Kind::Local))
            .then(|| auto_analysis::LastSelection::local(root.to_owned(), &prepared.discovery))
            .flatten()
        });
        self.last_auto_selection = selection.clone();
        if let Some(store) = &mut self.selection_store {
            match selection {
                Some(selection) => store.submit(selection),
                None => store.clear(),
            }
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    fn queue_automatic(
        &mut self,
        window: &SortSizeWindow,
        root: std::path::PathBuf,
        preflight: Option<directory_worker::Preflight>,
        restored: bool,
    ) {
        self.restored_selection = None;
        if self.registry.is_disconnected(&root) {
            self.pending_auto = None;
            return;
        }
        let selection = self.sources.current.as_ref().and_then(|prepared| {
            auto_analysis::LastSelection::local(root.clone(), &prepared.discovery)
        });
        // A replacement mounted at the same pathname is a different source.
        if self.last_auto_selection != selection {
            self.auto_analysis.different_source();
        }
        self.last_auto_selection = selection;
        let accepted = if restored {
            self.auto_analysis.restored(&root)
        } else {
            self.auto_analysis.select(&root)
        };
        if !restored {
            self.remember_selection(&root);
        }
        // A repeated selection still restores its native path in the editor;
        // only the acquisition request is deduplicated.
        window.set_scan_path(self.path_input.select(root.clone()).into());
        if accepted {
            self.pending_auto = Some((root, preflight));
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    fn restore_selection(&mut self, window: &SortSizeWindow) {
        if self.sources.loading() {
            return;
        }
        let Some(prepared) = &self.sources.current else {
            return;
        };
        let Some(selection) = self.restored_selection.take() else {
            return;
        };
        if self
            .registry
            .find(&selection.root)
            .is_some_and(|s| s.kind != source_registry::Kind::Local)
        {
            return;
        }
        if !selection.matches_local(&prepared.discovery) {
            self.diagnostic(window, presentation::Diagnostic::VolumeBlocked);
            return;
        }
        // The saved path was displayed at startup. Discovery must not rewrite
        // a later user choice or an offline inventory opened in the meantime.
        self.show_sources(window);
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
        if !volume_view::can_auto_scan(volume)
            || volume.mounts.first().is_some_and(|m| {
                self.registry
                    .find(&m.path)
                    .is_some_and(|s| s.kind != source_registry::Kind::Local)
            })
        {
            if let Some(mount) = volume.mounts.first() {
                window.set_scan_path(self.path_input.select(mount.path.clone()).into());
                window.set_selected_volume(index);
            }
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
        window.set_current_folder_summary("".into());
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
                self.restored_selection = None;
                window.set_scan_path(self.path_input.select(path).into());
                window.set_selected_volume(-1);
                let root = self.path_input.resolve(window.get_scan_path().as_str());
                self.remember_selection(&root);
                if !window.get_local_owner_resolution() {
                    window.set_resolve_owner(false);
                    self.resolution_changed(window);
                }
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
        self.poll_source_dialog();
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
        #[cfg(any(target_os = "linux", windows))]
        {
            if self.sources.current.is_some()
                && let Some(root) = self.pending_remember.take()
            {
                self.remember_selection(&root);
            }
            self.restore_selection(window);
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
            window.set_current_folder_summary(tree.current_folder_summary().into());
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

#[cfg(any(target_os = "linux", windows))]
fn show_add_source(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
    controller.borrow().show_disconnected_sources();
    if let Some(dialog) = &controller.borrow().source_dialog {
        dialog
            .global::<WorkshopTheme>()
            .set_mode(i32::from(controller.borrow().preferences.theme));
        let _ = dialog.show();
        return;
    }
    let Ok(dialog) = AddSourceWindow::new() else {
        return;
    };
    dialog.set_windows_platform(cfg!(windows));
    dialog.set_linux_platform(cfg!(target_os = "linux"));
    dialog
        .global::<WorkshopTheme>()
        .set_mode(i32::from(controller.borrow().preferences.theme));
    let weak = dialog.as_weak();
    let state = Rc::downgrade(controller);
    dialog.on_cancel(move || {
        if let Some(dialog) = weak.upgrade() {
            if let Some(state) = state.upgrade() {
                let mut state = state.borrow_mut();
                state.source_operations.invalidate();
                if state.source_operations.busy() {
                    dialog.set_result_state(14);
                }
            }
            let _ = dialog.hide();
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_input_changed(move || {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            state.borrow_mut().source_input_changed(&dialog, true);
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_type_changed(move || {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            state.borrow_mut().source_input_changed(&dialog, false);
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    let main = window.as_weak();
    dialog.on_add_source(move || {
        if let (Some(state), Some(dialog), Some(main)) =
            (state.upgrade(), weak.upgrade(), main.upgrade())
        {
            state.borrow_mut().add_source(&main, &dialog);
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_check_access(move || {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            state.borrow_mut().check_source(&dialog);
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_find_shares(move || {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            #[cfg(any(target_os = "linux", windows))]
            state.borrow_mut().smb_source_operation(&dialog, false);
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                let _ = state;
                dialog.set_result_state(17);
            }
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_connect_share(move || {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            #[cfg(any(target_os = "linux", windows))]
            state.borrow_mut().smb_source_operation(&dialog, true);
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                let _ = state;
                dialog.set_result_state(17);
            }
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_select_share(move |index| {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            state.borrow_mut().select_source_share(&dialog, index);
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    dialog.on_browse_source(move || {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            let mut state = state.borrow_mut();
            if state.source_picker.is_some() || state.source_operations.busy() {
                return;
            }
            // Native picker has its own mailbox and never routes through the
            // existing browse-and-auto-analyze callback.
            let picker = rfd::FileDialog::new().set_parent(&dialog.window().window_handle());
            match folder_picker::FolderPicker::start(move || picker.pick_folder()) {
                Ok(picker) => {
                    state.source_picker = Some(picker);
                    state.source_operations.invalidate();
                    state.source_picker_serial = state.source_operations.revision();
                    dialog.set_busy(true);
                }
                Err(_) => dialog.set_result_state(2),
            }
        }
    });
    let state = Rc::downgrade(controller);
    let weak = dialog.as_weak();
    let main = window.as_weak();
    dialog.on_reconnect_source(move |index| {
        if let (Some(state), Some(dialog)) = (state.upgrade(), weak.upgrade()) {
            let request = {
                let state = state.borrow();
                if state.source_operations.busy() {
                    return;
                }
                usize::try_from(index)
                    .ok()
                    .and_then(|i| state.registry.disconnected_sources().get(i).cloned())
                    .zip(state.source_reconnect.clone())
            };
            if let Some((source, reconnect)) = request {
                dialog.set_result_state(0);
                dialog.set_reconnect_status("".into());
                reconnect(source);
                if !state.borrow().source_operations.busy()
                    && let Some(main) = main.upgrade()
                {
                    dialog.set_reconnect_status(main.get_errors());
                }
            }
        }
    });
    let state = Rc::downgrade(controller);
    dialog.window().on_close_requested(move || {
        if let Some(state) = state.upgrade() {
            state.borrow_mut().source_operations.invalidate();
        }
        slint::CloseRequestResponse::HideWindow
    });
    let _ = dialog.show();
    controller.borrow_mut().source_dialog = Some(dialog);
    controller.borrow().show_disconnected_sources();
}

fn connect(window: &SortSizeWindow) -> Rc<RefCell<Controller>> {
    let controller = Rc::new(RefCell::new(Controller {
        presentation: presentation::Context::current(),
        system_locale: sys_locale::get_locale(),
        ..Controller::default()
    }));
    analysis_controller::connect(window, &controller);
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_theme(move |theme| {
        if let Some(window) = weak.upgrade() {
            let mut s = state.borrow_mut();
            s.preferences.theme = theme.clamp(0, 2) as u8;
            let units = s.preferences.units;
            s.display_changed(&window, units);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_density(move |dense| {
        if let Some(window) = weak.upgrade() {
            let mut s = state.borrow_mut();
            s.preferences.dense = dense;
            let units = s.preferences.units;
            s.display_changed(&window, units);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_sidebar_width(move |width| {
        if let Some(w) = weak.upgrade() {
            let mut s = state.borrow_mut();
            s.preferences.sidebar_width = width.clamp(220., 600.) as u16;
            let units = s.preferences.units;
            s.display_changed(&w, units);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_sidebar_collapsed(move |collapsed| {
        if let Some(w) = weak.upgrade() {
            let mut s = state.borrow_mut();
            s.preferences.sidebar_collapsed = collapsed;
            let units = s.preferences.units;
            s.display_changed(&w, units);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_treemap_mode(move |mode| {
        if let Some(w) = weak.upgrade() {
            let mut s = state.borrow_mut();
            s.preferences.treemap_mode = u8::from(mode == 1);
            let units = s.preferences.units;
            s.display_changed(&w, units);
        }
    });
    window.on_open_project(|| {
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open")
            .arg("https://github.com/tazmen3/Bunny-and-Size-OSS")
            .spawn();
        #[cfg(windows)]
        let _ = std::process::Command::new("rundll32.exe")
            .args([
                "url.dll,FileProtocolHandler",
                "https://github.com/tazmen3/Bunny-and-Size-OSS",
            ])
            .spawn();
    });
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
            state.preferences.units = previous.with_decimal(decimal);
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_size_unit(move |choice| {
        if let Some(window) = weak.upgrade()
            && let Ok(choice @ 0..=4) = u8::try_from(choice)
        {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            state.preferences.units = previous.with_size_unit(choice);
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_duplicate_profile_changed({
        let state = state.clone();
        let weak = weak.clone();
        move |index| {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().change_duplicate_profile(&window, index);
            }
        }
    });
    window.on_reset_display(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            state.preferences = display_preferences::Preferences {
                language: state.preferences.language,
                duplicate_profile: state.preferences.duplicate_profile,
                #[cfg(any(target_os = "linux", windows))]
                duplicate_resources: state.preferences.duplicate_resources.clone(),
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
        let state = Rc::downgrade(&controller);
        window.on_show_add_source(move || {
            if let (Some(window), Some(state)) = (weak.upgrade(), state.upgrade()) {
                show_add_source(&window, &state);
            }
        });
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
        #[cfg(any(target_os = "linux", windows))]
        {
            state.restored_selection = None;
            state.pending_auto = None;
        }
        if let Some(window) = weak.upgrade() {
            window.set_selected_volume(-1);
        }
        if let Some(window) = weak.upgrade()
            && !window.get_local_owner_resolution()
        {
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
    window.on_set_treemap_height({
        let state = state.clone();
        let weak = weak.clone();
        move |height| {
            if let Some(window) = weak.upgrade() {
                let height = if height == 0 {
                    0
                } else {
                    height.clamp(80, i32::from(u16::MAX)) as u16
                };
                {
                    let mut state = state.borrow_mut();
                    if state.preferences.treemap_height == height {
                        return;
                    }
                    state.preferences.treemap_height = height;
                    state.save_preferences(&window);
                }
                window.set_treemap_height_choice(i32::from(height));
            }
        }
    });
    window.on_resize_treemap(move |_, _| {
        if let Some(window) = weak.upgrade() {
            state.borrow().show_treemap(&window);
        }
    });
    let weak = window.as_weak();
    window.on_map_hover(move |x, y, inside| {
        if let Some(window) = weak.upgrade() {
            let index = inside
                .then(|| {
                    hover::hit_test(
                        &window.get_tiles(),
                        (window.get_map_width(), window.get_map_height()),
                        (x, y),
                        window.get_treemap_mode() == 1,
                    )
                })
                .flatten()
                .map_or(-1, |index| index as i32);
            // Pointer motion reads the existing geometry only. It never changes
            // the keyboard cursor, inspected entry or inventory presentation.
            if window.get_map_hover_index() != index {
                window.set_map_hover_index(index);
            }
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
    window.set_accessibility_description_values(cfg!(target_os = "linux"));
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
        // Restore presentation choices directly at the startup boundary. They
        // must not depend on a language change to reach the first rendered frame.
        // This only publishes settings; it never schedules a preference write.
        state.show_preferences(&window);
        #[cfg(any(target_os = "linux", windows))]
        state.show_duplicate_resource_preferences(&window);
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
    #[cfg(any(target_os = "linux", windows))]
    let workshop = workshop::connect(&window, &controller)
        .map_err(|e| slint::PlatformError::Other(e.to_string()))?;
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
            eprintln!("Bunny and Size: Licences : {error}");
        }
    });
    let weak_legal = legal.as_weak();
    let weak_controller = Rc::downgrade(&controller);
    window.window().on_close_requested(move || {
        if let Some(controller) = weak_controller.upgrade() {
            controller.borrow_mut().analysis.invalidate();
            #[cfg(any(target_os = "linux", windows))]
            if let Some(dialog) = &controller.borrow().source_dialog {
                let _ = dialog.hide();
            }
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
    #[cfg(any(target_os = "linux", windows))]
    if let Some(path) = auto_analysis::config_path() {
        let selection = auto_analysis::load(&path);
        let mut state = controller.borrow_mut();
        if let Some(selection) = selection {
            window.set_scan_path(state.path_input.select(selection.root.clone()).into());
            state.restored_selection = Some(selection);
        }
        state.selection_store = auto_analysis::SelectionStore::start(path).ok();
    }
    #[cfg(any(target_os = "linux", windows))]
    if let Some(path) = source_registry::config_path() {
        let mut state = controller.borrow_mut();
        state.registry = source_registry::Registry::open(path);
        let entries = state.registry.entries.clone();
        if !entries.is_empty() {
            state.sources.set_registered(&entries);
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    controller.borrow_mut().refresh_sources(&window, false);
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    #[cfg(windows)]
    let mut placement_attempts = 0;
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
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
            #[cfg(any(target_os = "linux", windows))]
            state.borrow_mut().poll_duplicate_temporary_picker(&window);
            #[cfg(any(target_os = "linux", windows))]
            {
                let mut controller = state.borrow_mut();
                let mut host = workshop.borrow_mut();
                host.poll_disconnect(&window, &mut controller);
                host.poll_selection(&window, &mut controller);
                host.poll(&window, &controller);
            }
        }
    });
    let result = window.run();
    timer.stop();
    #[cfg(any(target_os = "linux", windows))]
    if let Some(dialog) = &controller.borrow().source_dialog {
        let _ = dialog.hide();
    }
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

#[cfg(all(feature = "benchmarks", any(target_os = "linux", windows)))]
pub use workshop::qualify_finish;
#[cfg(all(feature = "benchmarks", any(target_os = "linux", windows)))]
pub use workshop::{qualify as qualify_workshop, qualify_cache, qualify_search};

#[cfg(all(
    feature = "benchmarks",
    feature = "experimental-preview",
    any(target_os = "linux", target_os = "windows")
))]
pub use workshop::qualify_preview;

#[cfg(all(feature = "benchmarks", any(target_os = "linux", windows)))]
pub use workshop::{prepare_cached_a_smoke, qualify_cached_a};

#[cfg(test)]
mod tests {
    mod analysis_gui_tests;
    mod branding_gui_tests;
    mod display_gui_tests;
    mod gauge_gui_tests;
    mod hover_callback_tests;
    mod hover_gui_tests;
    mod i18n_gui_tests;
    mod language_gui_tests;
    mod ondemand_gui_tests;
    mod owner_gui_tests;
    mod progressive_gui_tests;
    #[cfg(any(target_os = "linux", windows))]
    mod size_units_gui_tests;
    #[cfg(windows)]
    mod smb_server_gui_tests;
    #[cfg(any(target_os = "linux", windows))]
    mod source_gui_tests;
    mod temporal_gui_tests;
    mod toolbar_gui_tests;
    mod ux_final_gui_tests;
    #[cfg(any(target_os = "linux", windows))]
    mod view_preferences_gui_tests;
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_smb_dialog_registers_logical_targets_without_acquisition_or_mount() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
        let window = new_window().unwrap();
        let controller = connect(&window);
        let dialog = AddSourceWindow::new().unwrap();
        dialog.set_linux_platform(true);
        dialog.set_display_name("Offline NAS".into());
        dialog.set_source_kind(3);
        for input in ["192.0.2.204", "smb://192.0.2.204/", r"\\192.0.2.204\"] {
            dialog.set_source_path(input.into());
            let source = controller.borrow().source_from_dialog(&dialog).unwrap();
            assert!(source.is_server());
            assert_eq!(source.path, std::path::PathBuf::from("smb://192.0.2.204/"));
        }
        dialog.set_source_kind(0);
        for input in ["smb://192.0.2.204/Photos", r"\\192.0.2.204\Photos"] {
            dialog.set_source_path(input.into());
            controller.borrow_mut().add_source(&window, &dialog);
        }
        assert_eq!(controller.borrow().registry.entries.len(), 1);
        assert_eq!(
            controller.borrow().registry.entries[0].path,
            std::path::PathBuf::from("smb://192.0.2.204/Photos")
        );
        assert!(!window.get_busy());
        assert!(controller.borrow().task.is_none());
        assert!(controller.borrow().pending_auto.is_none());
        assert!(!controller.borrow().source_operations.busy());
        assert!(window.get_scan_path().is_empty());
        dialog.set_source_path("smb://user:secret@192.0.2.204/Photos".into());
        assert!(controller.borrow().source_from_dialog(&dialog).is_err());
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
