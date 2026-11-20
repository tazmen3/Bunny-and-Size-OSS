//! A virtual server grants acquisition for an activated share or a confirmed batch.
use super::*;
use crate::{SmbDisconnectedShareRow, SmbServerScanWindow, SmbShareChoice, SmbShareRow};

#[derive(Default)]
pub(super) struct State {
    host: std::rc::Weak<RefCell<Host>>,
    pub(super) controller: std::rc::Weak<RefCell<Controller>>,
    pub(super) root: Option<PathBuf>,
    pub(super) shares: Vec<PathBuf>,
    pub(super) selected: Vec<bool>,
    pub(super) exclusions: Vec<PathBuf>,
    uncertain: bool,
    pub(super) operations: crate::source_dialog::Operations,
    pub(super) dialog: Option<SmbServerScanWindow>,
    pub(super) token: i32,
    pub(super) share_rows_token: i32,
    error: Option<String>,
    pub(super) dirty: bool,
    last: Option<smb_server::Snapshot>,
    pub(super) memory: Option<sortsize_core::resources::Reservation>,
    #[cfg(test)]
    pub(super) discovery_fixture: Option<Vec<PathBuf>>,
}

impl State {
    fn excluded(&self, path: &Path) -> bool {
        self.exclusions
            .iter()
            .any(|root| crate::source_registry::covers(root, path))
    }
    pub(super) fn active_share_index(&self, index: usize) -> Option<usize> {
        self.shares
            .iter()
            .enumerate()
            .filter(|(_, path)| !self.excluded(path))
            .nth(index)
            .map(|(i, _)| i)
    }

    pub(super) fn disconnected_shares(&self) -> impl Iterator<Item = &PathBuf> {
        self.shares.iter().filter(|path| self.excluded(path))
    }

    pub(super) fn invalidate_share_rows(&mut self) {
        self.share_rows_token = self
            .share_rows_token
            .checked_add(1)
            .expect("share rows token exhausted");
    }
}

impl Host {
    pub(super) fn select_smb_server(&mut self, window: &SortSizeWindow, root: PathBuf) {
        if self.server.root.as_ref() != Some(&root) {
            self.server.operations.invalidate();
            if let Some(dialog) = self.server.dialog.take() {
                let _ = dialog.hide();
            }
            self.server.shares.clear();
            self.server.selected.clear();
            self.server.memory = None;
            self.server.uncertain = false;
            self.server.last = None;
            self.server.error = None;
            self.server.root = Some(root.clone());
            self.server.invalidate_share_rows();
            self.server.token = self
                .server
                .token
                .checked_add(1)
                .expect("server intent exhausted");
        }
        self.server.dirty = true;
        window.set_smb_server_context(true);
        window.set_smb_server_visible(true);
        window.set_top_visible(false);
        window.set_search_visible(false);
        window.set_cache_visible(false);
        window.set_scan_path(label(&root).into());
        window.set_current_path(label(&root).into());
        self.poll_smb_server(window, true);
    }

    fn discover_smb_server(&mut self, window: &SortSizeWindow) {
        if self.server.operations.busy() || self.service.smb_snapshot().is_some_and(|s| s.running) {
            return;
        }
        let Some(root) = self.server.root.clone() else {
            return;
        };
        #[cfg(test)]
        if let Some(shares) = self.server.discovery_fixture.clone() {
            let _ = self
                .server
                .operations
                .start(move || crate::source_dialog::Outcome {
                    state: 18,
                    shares: Some(shares),
                    limited: false,
                });
            self.server.dirty = true;
            self.update_smb_confirmation();
            self.poll_smb_server(window, true);
            return;
        }
        #[cfg(any(windows, target_os = "linux"))]
        {
            let Some(target) = root
                .to_str()
                .and_then(|p| sortsize_core::volumes::smb::parse_target(p).ok())
                .filter(|t| t.share.is_none())
            else {
                return;
            };
            self.server.error = None;
            self.server.dirty = true;
            let result = self.server.operations.start(move || {
                match sortsize_core::volumes::smb::enumerate_shares(&target) {
                    Ok(list) => crate::source_dialog::Outcome {
                        state: 18,
                        shares: Some(
                            list.shares
                                .into_iter()
                                .take(smb_server::MAX_SHARES)
                                .map(|s| s.path)
                                .collect(),
                        ),
                        limited: list.limited,
                    },
                    Err(_) => crate::source_dialog::Outcome::state(16),
                }
            });
            if result.is_err() {
                self.server.error = Some(discovery_failure().into());
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = root;
            self.server.error = Some(discovery_failure().into());
        }
        self.update_smb_confirmation();
        self.poll_smb_server(window, true);
    }

    pub(super) fn confirm_smb_server(&mut self, window: &SortSizeWindow) {
        let (Some(host), Some(controller)) =
            (self.server.host.upgrade(), self.server.controller.upgrade())
        else {
            return;
        };
        if self.server.operations.busy() || self.service.smb_snapshot().is_some_and(|s| s.running) {
            return;
        }
        if let Some(old) = self.server.dialog.take() {
            let _ = old.hide();
        }
        self.server.token = self
            .server
            .token
            .checked_add(1)
            .expect("server intent exhausted");
        let Ok(dialog) = SmbServerScanWindow::new() else {
            return;
        };
        dialog
            .global::<crate::WorkshopTheme>()
            .set_mode(window.global::<crate::WorkshopTheme>().get_mode());
        let _ = crate::locale::activate_bundled(self.presentation.language);
        dialog.set_token(self.server.token);
        let weak = Rc::downgrade(&host);
        dialog.on_toggle_share(move |token, index, selected| {
            if let Some(host) = weak.upgrade() {
                let mut host = host.borrow_mut();
                if token != host.server.token || host.server.operations.busy() {
                    return;
                }
                let Some(index) = usize::try_from(index)
                    .ok()
                    .and_then(|i| host.server.active_share_index(i))
                else {
                    return;
                };
                if host.server.shares.get(index).is_some_and(|path| {
                    host.server
                        .exclusions
                        .iter()
                        .any(|root| crate::source_registry::covers(root, path))
                }) {
                    return;
                }
                if let Some(value) = host.server.selected.get_mut(index) {
                    *value = selected;
                }
                host.update_smb_confirmation();
            }
        });
        let weak = Rc::downgrade(&host);
        dialog.on_select_all(move |token, selected| {
            if let Some(host) = weak.upgrade() {
                let mut host = host.borrow_mut();
                if token != host.server.token || host.server.operations.busy() {
                    return;
                }
                let exclusions = host.server.exclusions.clone();
                let shares = host.server.shares.clone();
                for (value, path) in host.server.selected.iter_mut().zip(&shares) {
                    *value = selected
                        && !exclusions
                            .iter()
                            .any(|root| crate::source_registry::covers(root, path));
                }
                host.update_smb_confirmation();
            }
        });
        let weak = Rc::downgrade(&host);
        dialog.on_cancel(move |token| {
            if let Some(host) = weak.upgrade() {
                let mut host = host.borrow_mut();
                if token != host.server.token {
                    return;
                }
                host.server.token = host
                    .server
                    .token
                    .checked_add(1)
                    .expect("server intent exhausted");
                host.server.dirty = true;
                host.server.operations.invalidate();
                if let Some(dialog) = host.server.dialog.take() {
                    let _ = dialog.hide();
                }
            }
        });
        let weak = Rc::downgrade(&host);
        let main = window.as_weak();
        let control = Rc::downgrade(&controller);
        dialog.on_confirm(move |token| {
            if let (Some(host), Some(window), Some(controller)) =
                (weak.upgrade(), main.upgrade(), control.upgrade())
            {
                let mut host = host.borrow_mut();
                if token != host.server.token
                    || host.server.operations.busy()
                    || host
                        .server
                        .dialog
                        .as_ref()
                        .is_none_or(|d| d.get_token() != token)
                    || host.service.smb_snapshot().is_some_and(|s| s.running)
                {
                    return;
                }
                let shares = host
                    .server
                    .shares
                    .iter()
                    .zip(&host.server.selected)
                    .filter(|(_, selected)| **selected)
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>();
                if shares.is_empty() {
                    return;
                }
                host.server.token = host
                    .server
                    .token
                    .checked_add(1)
                    .expect("server intent exhausted");
                if let Some(dialog) = host.server.dialog.take() {
                    let _ = dialog.hide();
                }
                host.begin_smb_operation(&window, &mut controller.borrow_mut(), shares);
            }
        });
        // Closing the confirmation window never authorizes an acquisition.
        let weak = Rc::downgrade(&host);
        dialog.window().on_close_requested(move || {
            if let Some(host) = weak.upgrade() {
                let mut host = host.borrow_mut();
                host.server.operations.invalidate();
                host.server.token = host
                    .server
                    .token
                    .checked_add(1)
                    .expect("server intent exhausted");
                host.server.dialog = None;
                host.server.dirty = true;
            }
            slint::CloseRequestResponse::HideWindow
        });
        self.server.dialog = Some(dialog);
        self.update_smb_confirmation();
        if let Some(dialog) = &self.server.dialog {
            let _ = dialog.show();
        }
        // A fresh explicit discovery precedes every confirmation. The operation
        // subsequently uses only the exact paths selected in this dialog.
        self.discover_smb_server(window);
    }

    pub(super) fn update_smb_confirmation(&self) {
        let Some(dialog) = &self.server.dialog else {
            return;
        };
        dialog.set_server_path(
            self.server
                .root
                .as_ref()
                .map_or_else(String::new, |p| label(p))
                .into(),
        );
        dialog.set_busy(self.server.operations.busy());
        dialog.set_discovery_uncertain(self.server.uncertain);
        dialog.set_status(self.server.error.clone().unwrap_or_default().into());
        dialog.set_shares(patch_model(
            dialog.get_shares(),
            self.server
                .shares
                .iter()
                .zip(&self.server.selected)
                .filter(|(path, _)| !self.server.excluded(path))
                .map(|(path, selected)| SmbShareChoice {
                    name: share_name(path).into(),
                    path: label(path).into(),
                    selected: *selected
                        && !self
                            .server
                            .exclusions
                            .iter()
                            .any(|root| crate::source_registry::covers(root, path)),
                    disconnected: self
                        .server
                        .exclusions
                        .iter()
                        .any(|root| crate::source_registry::covers(root, path)),
                })
                .collect::<Vec<_>>(),
        ));
        dialog.set_selected_count(
            self.server
                .selected
                .iter()
                .zip(&self.server.shares)
                .filter(|(s, path)| {
                    **s && !self
                        .server
                        .exclusions
                        .iter()
                        .any(|root| crate::source_registry::covers(root, path))
                })
                .count() as i32,
        );
    }

    fn begin_smb_operation(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        shares: Vec<PathBuf>,
    ) -> bool {
        let Some(server) = self.server.root.clone() else {
            return false;
        };
        if controller.registry.is_disconnected(&server) {
            window.set_errors(tr!("This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning.").into());
            return false;
        }
        let shares = shares
            .into_iter()
            .filter(|path| !controller.registry.is_disconnected(path))
            .collect::<Vec<_>>();
        if shares.is_empty() {
            return false;
        }
        let epoch = self.epoch();
        let options = sortsize_store::scheduler::CollectionOptions {
            allocation: window.get_collect_allocation(),
            owners: window.get_collect_owner(),
        };
        // A scan can be confirmed before the first presentation poll.
        // Publish the initiating UI language before the worker sees the batch.
        self.presentation = controller.presentation;
        self.service.lane.lock().unwrap().presentation = self.presentation;
        if let Err(error) = self.service.begin_smb_batch(
            epoch,
            server.clone(),
            shares,
            self.server.uncertain,
            options,
        ) {
            window.set_errors(error.into());
            return false;
        }
        controller.pending_auto = None;
        controller.restored_selection = None;
        controller.auto_analysis.manual(&server);
        controller.scan_root = Some(server.clone());
        self.requested = Some(server.clone());
        self.source_selection = None;
        self.current = None;
        self.pending = None;
        self.pending_folder = None;
        self.active_service = true;
        self.failed = false;
        self.close_search(window);
        self.clear_preview(window, 0);
        self.cache_cancel.cancel();
        self.cache_wait = None;
        self.server.dirty = true;
        window.set_busy(true);
        window.set_cancelling(false);
        window.set_errors("".into());
        window.set_scan_source(label(&server).into());
        self.poll_smb_server(window, true);
        true
    }

    fn start_smb_share(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        path: &Path,
    ) -> bool {
        if !self.begin_smb_operation(window, controller, vec![path.to_owned()]) {
            return false;
        }
        let epoch = self.epoch();
        self.service.view_smb_share(epoch, 0);
        true
    }

    pub(super) fn begin_single_smb_share(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        path: &Path,
    ) -> bool {
        if !window.get_smb_server_context()
            || window.get_smb_server_visible()
            || !self.server.shares.iter().any(|p| p == path)
        {
            return false;
        }
        self.start_smb_share(window, controller, path);
        true
    }

    fn open_smb_share_ui(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        index: usize,
    ) {
        let Some(path) = self
            .server
            .active_share_index(index)
            .and_then(|i| self.server.shares.get(i))
            .cloned()
        else {
            return;
        };
        let snapshot = self
            .service
            .smb_snapshot()
            .filter(|s| Some(&s.server) == self.server.root.as_ref());
        let stored_index = snapshot
            .as_ref()
            .and_then(|s| s.shares.iter().position(|share| share.path == path));
        let epoch = self.epoch();
        // Reuse the displayed inventory whenever it can be opened.
        self.requested = Some(path.clone());
        self.current = None;
        self.pending = None;
        self.pending_folder = None;
        self.selected = None;
        self.tree_focused = None;
        window.set_smb_server_visible(false);
        window.set_scan_path(controller.path_input.select(path.clone()).into());
        window.set_current_path(label(&path).into());
        window.set_current_folder_summary("".into());
        window.set_details(ModelRc::default());
        window.set_rows(ModelRc::default());
        window.set_tiles(ModelRc::default());
        window.set_has_parent(true);
        window.set_has_next_detail_page(false);
        window.set_has_previous_detail_page(false);
        window.set_errors("".into());
        let opened = stored_index.is_some_and(|i| {
            if snapshot.as_ref().is_some_and(|s| s.running) {
                self.service.view_smb_share(epoch, i)
            } else {
                self.service.open_smb_share(epoch, i)
            }
        }) || self.service.open_smb_path(epoch, &path);
        let can_start = !opened
            && window.get_scan_enabled()
            && !window.get_busy()
            && !window.get_choosing_folder()
            && !self.server.operations.busy()
            && self.server.dialog.is_none()
            // A completed SMB batch may still be finalizing its reader
            // presentation until a page opens. Only live intentions block this click.
            && !self.service.lane.lock().unwrap().has_acquisition_intent();
        if opened || (can_start && self.start_smb_share(window, controller, &path)) {
            window.set_empty_message(tr!("Preparing inventory…").into());
        } else {
            window.set_empty_message(
                tr!("Share not inventoried. Click Scan to analyze this share.").into(),
            );
            window
                .set_status(tr!("Share not inventoried. Click Scan to analyze this share.").into());
        }
        self.server.dirty = true;
    }

    pub(super) fn poll_smb_server(&mut self, window: &SortSizeWindow, changed: bool) {
        if let Some(operation) = self.service.smb_snapshot() {
            window.set_busy(operation.running);
            if !operation.running {
                window.set_cancelling(false);
            }
        }
        if self.server.root.is_none() {
            return;
        }
        if let Some(result) = self.server.operations.poll() {
            if let Some(outcome) = result {
                if let Some(shares) = outcome.shares {
                    let bytes = shares.iter().fold(64 * 1024u64, |sum, p| {
                        sum.saturating_add(
                            (p.as_os_str().as_encoded_bytes().len() as u64)
                                .saturating_mul(32)
                                .saturating_add(1024),
                        )
                    });
                    let memory = sortsize_core::resources::Governor::global().reserve(
                        sortsize_core::resources::Category::Publications,
                        bytes,
                        None,
                    );
                    if let Some(memory) = memory {
                        if self.server.shares != shares {
                            self.server.invalidate_share_rows();
                        }
                        self.server.selected = shares
                            .iter()
                            .map(|path| !self.server.excluded(path))
                            .collect();
                        self.server.shares = shares;
                        self.server.memory = Some(memory);
                        self.server.uncertain = outcome.limited;
                        self.server.error = None;
                    } else {
                        self.server.selected.clear();
                        self.server.uncertain = true;
                        self.server.error = Some(
                            tr!("This folder could not be expanded within the tree budget.").into(),
                        );
                    }
                } else {
                    // Retain the last displayed catalogue. Only a fresh discovery
                    // can supply the choices for a new server-wide operation.
                    self.server.selected.clear();
                    self.server.uncertain = true;
                    self.server.error = Some(discovery_failure().into());
                }
            }
            self.server.dirty = true;
            self.update_smb_confirmation();
        }
        if !window.get_smb_server_context() {
            return;
        }
        window.set_has_parent(!window.get_smb_server_visible());
        let snapshot = self
            .service
            .smb_snapshot()
            .filter(|s| Some(&s.server) == self.server.root.as_ref());
        if let Some(snapshot) = &snapshot {
            // A child page finishing must not mark the whole operation idle.
            window.set_busy(snapshot.running);
            if !snapshot.running {
                window.set_cancelling(false);
            }
        }
        if !changed && !self.server.dirty && self.server.last == snapshot {
            return;
        }
        self.server.last = snapshot.clone();
        self.server.dirty = false;
        let title = format!(
            "NAS - {}",
            self.server
                .root
                .as_ref()
                .map_or_else(String::new, |p| label(p))
        );
        window.set_smb_server_title(title.into());
        let mut summary = Vec::new();
        if self.server.operations.busy() {
            summary.push(tr!("Looking for shares on this server…").to_owned());
        } else if let Some(error) = &self.server.error {
            summary.push(error.clone());
        } else if self.server.memory.is_none() {
            summary.push(tr!("Discover shares explicitly before browsing this server.").into());
        } else if self.server.uncertain {
            summary.push(tr!("Share discovery is limited or uncertain.").to_owned());
        }
        let scan_summary = snapshot.as_ref().map(|snapshot| {
            let totals = snapshot.totals();
            let measures = if totals.coverage == ScopeCoverage::Unknown {
                tr!("Unknown").to_owned()
            } else {
                crate::current_folder_summary::render(
                    totals.logical,
                    totals.files,
                    totals.directories,
                    totals.coverage != ScopeCoverage::Complete,
                    self.units,
                )
            };
            let scan = if snapshot.running {
                tr!("Current scan")
            } else {
                tr!("Last scan")
            };
            let completed = tr_format!(
                "Shares completed: {} / {}",
                crate::number_format::integer(snapshot.completed() as u128),
                crate::number_format::integer(snapshot.shares.len() as u128)
            );
            format!("{scan} · {completed} · {measures}")
        });
        let retained = self.service.retained_smb_shares();
        let rows = self
            .server
            .shares
            .iter()
            .filter(|path| !self.server.excluded(path))
            .map(|path| {
                let selected_share = snapshot
                    .as_ref()
                    .and_then(|s| s.shares.iter().find(|share| &share.path == path));
                let observed = selected_share.filter(|s| s.current_observation);
                let saved = retained.iter().find(|share| &share.path == path);
                let share = observed.or(saved);
                let partial = share.is_none_or(|s| {
                    s.coverage != ScopeCoverage::Complete
                        || s.phase != smb_server::SharePhase::Complete
                });
                let value = |n: Option<u64>, folders: bool| {
                    n.map_or_else(
                        || "—".to_owned(),
                        |n| {
                            format!(
                                "{}{}",
                                if partial { "≥ " } else { "" },
                                if folders {
                                    crate::presentation::folders(n)
                                } else {
                                    crate::presentation::files(n)
                                }
                            )
                        },
                    )
                };
                let size = share.and_then(|s| s.logical).map_or_else(
                    || "—".into(),
                    |n| {
                        format!(
                            "{}{}",
                            if partial { "≥ " } else { "" },
                            self.units.format(n)
                        )
                    },
                );
                let state = selected_share
                    .or(share)
                    .map_or(tr!("Unknown"), |s| match s.phase {
                        smb_server::SharePhase::Queued => tr!("Waiting"),
                        smb_server::SharePhase::Running => tr!("Scanning"),
                        smb_server::SharePhase::Complete if partial => tr!("Partial"),
                        smb_server::SharePhase::Complete => tr!("Complete"),
                        smb_server::SharePhase::Failed => tr!("Failed"),
                        smb_server::SharePhase::Cancelled => tr!("Cancelled"),
                    });
                let mut state = selected_share
                    .or(share)
                    .and_then(|s| s.error.as_ref())
                    .map_or_else(|| state.to_owned(), |error| format!("{state}: {error}"));
                if observed.is_none()
                    && let Some(measured) = saved.and_then(|s| s.measured)
                {
                    let date =
                        crate::date_label::format_date(sortsize_core::MetadataValue::Known {
                            value: measured,
                            accuracy: sortsize_core::ValueAccuracy::Exact,
                        });
                    state = format!("{state} · {} · {date}", tr!("Saved measures"));
                }
                SmbShareRow {
                    disconnected: false,
                    name: share_name(path).into(),
                    path: label(path).into(),
                    size: size.into(),
                    files: value(share.and_then(|s| s.files), false).into(),
                    directories: value(share.and_then(|s| s.directories), true).into(),
                    state: state.into(),
                    selected: selected_share.is_some(),
                }
            })
            .collect::<Vec<_>>();
        let disconnected_rows = self
            .server
            .disconnected_shares()
            .map(|path| SmbDisconnectedShareRow {
                name: share_name(path).into(),
                path: label(path).into(),
            })
            .collect::<Vec<_>>();
        if self.server.memory.is_some() {
            let visible = crate::number_format::integer(rows.len() as u128);
            let mut discovery = tr_format!("Visible shares: {}", visible);
            if !disconnected_rows.is_empty() {
                let excluded = crate::number_format::integer(disconnected_rows.len() as u128);
                let disconnected = tr_format!("Shares disconnected in Bunny: {}", excluded);
                discovery = format!("{discovery} · {disconnected}");
            }
            summary.push(discovery);
        }
        if let Some(scan_summary) = scan_summary {
            summary.push(scan_summary);
        }
        let summary = summary.join(" · ");
        window.set_smb_server_summary(summary.clone().into());
        // Replace row delegates when their identities change. A button
        // pressed before rediscovery must not activate a different share.
        if window.get_smb_server_token() == self.server.share_rows_token {
            window.set_smb_share_rows(patch_model(window.get_smb_share_rows(), rows));
        } else {
            window.set_smb_server_token(self.server.share_rows_token);
            window.set_smb_share_rows(Rc::new(VecModel::from(rows)).into());
        }
        window.set_smb_disconnected_share_token(self.server.share_rows_token);
        window.set_smb_disconnected_share_rows(patch_model(
            window.get_smb_disconnected_share_rows(),
            disconnected_rows,
        ));
        if window.get_smb_server_visible() {
            window.set_status(summary.clone().into());
            window.set_summary(summary.into());
            window.set_current_path(
                self.server
                    .root
                    .as_ref()
                    .map_or_else(String::new, |p| label(p))
                    .into(),
            );
        }
    }
}

fn share_name(path: &Path) -> String {
    // A UNC share is a native prefix, not a normal Path::file_name component.
    label(path)
        .rsplit(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn patch_model<T: Clone + PartialEq + 'static>(current: ModelRc<T>, rows: Vec<T>) -> ModelRc<T> {
    if let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() {
        for (index, row) in rows.iter().enumerate() {
            if index >= model.row_count() {
                model.push(row.clone());
            } else if model.row_data(index).as_ref() != Some(row) {
                model.set_row_data(index, row.clone());
            }
        }
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        current
    } else {
        Rc::new(VecModel::from(rows)).into()
    }
}

pub(super) fn connect(
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
    host: &Rc<RefCell<Host>>,
) {
    host.borrow_mut().server.host = Rc::downgrade(host);
    host.borrow_mut().server.controller = Rc::downgrade(controller);
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    window.on_smb_server_discover(move || {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            let _scope = controller.borrow().presentation.enter();
            host.borrow_mut().discover_smb_server(&window);
        }
    });
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    window.on_smb_server_home(move || {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            let _scope = controller.borrow().presentation.enter();
            let root = host.borrow().server.root.clone();
            if let Some(root) = root {
                host.borrow_mut().select_smb_server(&window, root);
            }
        }
    });
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    window.on_smb_share_open(move |token, index| {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            let _scope = controller.borrow().presentation.enter();
            let mut host = host.borrow_mut();
            if window.get_smb_server_visible()
                && token == host.server.share_rows_token
                && let Ok(index) = usize::try_from(index)
            {
                host.open_smb_share_ui(&window, &mut controller.borrow_mut(), index);
            }
        }
    });
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use std::time::{Duration, Instant};
    struct Platform;
    impl slint::platform::Platform for Platform {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }

    fn wait(host: &Rc<RefCell<Host>>, window: &SortSizeWindow) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            host.borrow_mut().poll_smb_server(window, false);
            if !host.borrow().server.operations.busy() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn server(name: &str) -> PathBuf {
        #[cfg(target_os = "linux")]
        {
            PathBuf::from(format!("smb://{name}/"))
        }
        #[cfg(windows)]
        {
            PathBuf::from(format!(r"\\{name}"))
        }
    }

    fn share(name: &str, share: &str) -> PathBuf {
        #[cfg(target_os = "linux")]
        {
            PathBuf::from(format!("smb://{name}/{share}"))
        }
        #[cfg(windows)]
        {
            PathBuf::from(format!(r"\\{name}\{share}"))
        }
    }

    #[test]
    fn confirmation_cancel_empty_selection_and_stale_tokens_never_grant_acquisition() {
        slint::platform::set_platform(Box::new(Platform)).unwrap();
        let (window, controller) =
            crate::new_session(Default::default(), Some("en".into())).unwrap();
        let host = super::super::connect(&window, &controller).unwrap();
        {
            let mut host = host.borrow_mut();
            host.server.discovery_fixture = Some(vec![
                share("example.invalid", "first"),
                share("example.invalid", "second"),
            ]);
            host.select_smb_server(&window, server("example.invalid"));
            host.confirm_smb_server(&window);
        }
        wait(&host, &window);
        let dialog = host.borrow().server.dialog.as_ref().unwrap().clone_strong();
        let token = dialog.get_token();
        assert_eq!(dialog.get_selected_count(), 2);
        assert!(host.borrow().service.smb_snapshot().is_none());
        dialog.invoke_confirm(token + 1);
        assert!(host.borrow().service.smb_snapshot().is_none());
        dialog.invoke_select_all(token, false);
        assert_eq!(dialog.get_selected_count(), 0);
        dialog.invoke_confirm(token);
        assert!(host.borrow().service.smb_snapshot().is_none());
        dialog.invoke_toggle_share(token, 1, true);
        assert_eq!(dialog.get_selected_count(), 1);
        dialog.invoke_cancel(token);
        dialog.invoke_confirm(token); // A queued callback from a closed dialog is inert.
        assert!(host.borrow().service.smb_snapshot().is_none());
        assert!(controller.borrow().task.is_none());
        assert!(controller.borrow().pending_auto.is_none());
        assert_eq!(host.borrow().service.lane.lock().unwrap().source_epoch, 0);
    }

    fn begin_fixture_batch(
        host: &mut Host,
        rows: Vec<(PathBuf, Result<PathBuf, &'static str>)>,
    ) -> u64 {
        let epoch = host.epoch();
        let mut request = smb_server::Request::new(
            epoch,
            host.server.root.clone().unwrap(),
            rows.iter().map(|(path, _)| path.clone()).collect(),
            false,
            Default::default(),
        )
        .unwrap();
        // Keep the logical SMB paths used by the UI. At the execution boundary,
        // give the existing worker tiny local trees and a simulated access error.
        for (share, (_, native)) in request.snapshot.shares.iter_mut().zip(rows) {
            match native {
                Ok(root) => share.native_root = Some(root),
                Err(error) => {
                    share.native_root = None;
                    share.phase = smb_server::SharePhase::Failed;
                    share.error = Some(error.into());
                }
            }
        }
        let mut lane = host.service.lane.lock().unwrap();
        lane.source_epoch = epoch;
        lane.view_epoch = epoch;
        lane.publication_cancel.cancel();
        lane.publication_cancel = Default::default();
        lane.smb_snapshot = Some(request.snapshot.clone());
        lane.smb_begin = Some(request);
        lane.smb_view = None;
        lane.reader_target = Some(None);
        epoch
    }

    fn wait_for_fixture_batch(
        host: &Rc<RefCell<Host>>,
        window: &SortSizeWindow,
        epoch: u64,
    ) -> smb_server::Snapshot {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut host = host.borrow_mut();
            host.poll_smb_server(window, false);
            if let Some(snapshot) = host.service.smb_snapshot()
                && snapshot.epoch == epoch
                && !snapshot.running
            {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "fixture scan did not finish");
            drop(host);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn host_keeps_discovered_shares_during_targeted_scan_and_refresh_failure() {
        slint::platform::set_platform(Box::new(Platform)).unwrap();
        let (window, controller) =
            crate::new_session(Default::default(), Some("en".into())).unwrap();
        let context = controller.borrow().presentation;
        let _scope = context.enter();
        let host = super::super::connect(&window, &controller).unwrap();
        let name = "six-shares.invalid";
        let shares = [
            "bouquins",
            "home",
            "homes",
            "PlexMediaServer",
            "tv",
            "video",
        ]
        .map(|label| share(name, label));
        {
            let mut host = host.borrow_mut();
            host.server.discovery_fixture = Some(shares.to_vec());
            host.select_smb_server(&window, server(name));
        }
        window.invoke_smb_server_discover();
        wait(&host, &window);
        assert_eq!(window.get_smb_share_rows().row_count(), 6);
        assert!(host.borrow().service.smb_snapshot().is_none());

        let temp = tempfile::tempdir().unwrap();
        let saved = temp.path().join("saved");
        let targeted = temp.path().join("targeted");
        for (path, bytes) in [(&saved, &[1; 31][..]), (&targeted, &[2; 17][..])] {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("fixture"), bytes).unwrap();
        }
        let epoch = begin_fixture_batch(
            &mut host.borrow_mut(),
            vec![
                (shares[0].clone(), Ok(saved)),
                (shares[3].clone(), Err("Access denied")),
            ],
        );
        let first = wait_for_fixture_batch(&host, &window, epoch);
        assert_eq!(first.shares[0].logical, Some(31));
        let row = |name: &str| {
            window
                .get_smb_share_rows()
                .iter()
                .enumerate()
                .find(|(_, row)| row.name.as_str() == name)
                .unwrap_or_else(|| panic!("missing share {name}"))
        };
        let (_, denied) = row("PlexMediaServer");
        assert!(denied.state.contains("Access denied"));
        assert_eq!(denied.size.as_str(), "—");

        let (index, video) = row("video");
        assert_eq!(video.state.as_str(), "Unknown");
        {
            let host = host.borrow();
            host.service.lane.lock().unwrap().smb_native_fixture =
                Some((shares[5].clone(), targeted));
        }
        let old_token = window.get_smb_server_token();
        host.borrow_mut().server.discovery_fixture = Some(shares.iter().rev().cloned().collect());
        window.invoke_smb_server_discover();
        wait(&host, &window);
        let token = window.get_smb_server_token();
        assert_ne!(token, old_token);
        window.invoke_smb_share_open(old_token, index as i32);
        assert!(window.get_smb_server_visible());
        assert_eq!(host.borrow().service.smb_snapshot(), Some(first.clone()));
        let (index, _) = row("video");
        window.invoke_smb_share_open(token, index as i32);
        assert!(!window.get_smb_server_visible());
        assert_eq!(window.get_scan_path(), video.path);
        let pending = host.borrow().service.smb_snapshot().unwrap();
        assert_ne!(pending.epoch, first.epoch);
        assert_eq!(pending.shares.len(), 1);
        assert_eq!(pending.shares[0].path, shares[5]);
        window.invoke_smb_share_open(token, index as i32);
        assert_eq!(
            host.borrow().service.smb_snapshot().unwrap().epoch,
            pending.epoch
        );
        let targeted = wait_for_fixture_batch(&host, &window, pending.epoch);
        assert_eq!(targeted.shares.len(), 1);
        assert_eq!(targeted.shares[0].path, shares[5]);
        assert_eq!(targeted.shares[0].logical, Some(17));
        window.invoke_go_parent();
        assert!(window.get_smb_server_visible());
        assert_eq!(window.get_smb_share_rows().row_count(), 6);
        // An unsuccessful observation belongs to its scan. Without a fresh
        // observation or a saved measure, the next scan leaves this row unknown.
        for name in ["home", "homes", "PlexMediaServer", "tv"] {
            let (_, unknown) = row(name);
            assert_eq!(unknown.state.as_str(), "Unknown");
            assert_eq!(unknown.size.as_str(), "—");
        }
        let (_, measured) = row("bouquins");
        assert!(measured.state.contains("Saved measures"));
        assert_ne!(measured.size.as_str(), "—");
        assert_eq!(row("video").1.state.as_str(), "Complete");
        {
            let _french = crate::presentation::Context {
                language: crate::locale::Language::French,
                ..context
            }
            .enter();
            host.borrow_mut().poll_smb_server(&window, true);
            assert!(row("bouquins").1.state.contains("Mesures enregistrées"));
        }
        host.borrow_mut().poll_smb_server(&window, true);
        let summary = window.get_smb_server_summary();
        assert!(summary.contains("Visible shares: 6"), "{summary}");
        assert!(summary.contains("Last scan"), "{summary}");
        assert!(summary.contains("Shares completed: 1 / 1"), "{summary}");

        window.invoke_smb_server_discover();
        wait(&host, &window);
        assert_eq!(window.get_smb_share_rows().row_count(), 6);
        assert_eq!(row("bouquins").1, measured);
        let (index, saved) = row("bouquins");
        window.invoke_smb_share_open(window.get_smb_server_token(), index as i32);
        assert!(!window.get_smb_server_visible());
        assert_eq!(window.get_scan_path(), saved.path);
        window.invoke_go_parent();
        assert_eq!(host.borrow().service.smb_snapshot(), Some(targeted.clone()));

        window.set_busy(true);
        let (index, tv) = row("tv");
        window.invoke_smb_share_open(window.get_smb_server_token(), index as i32);
        assert_eq!(window.get_scan_path(), tv.path);
        assert_eq!(host.borrow().service.smb_snapshot(), Some(targeted));
        window.invoke_go_parent();

        assert!(
            host.borrow_mut()
                .server
                .operations
                .start(|| crate::source_dialog::Outcome::state(16))
                .unwrap()
        );
        wait(&host, &window);
        assert_eq!(window.get_smb_share_rows().row_count(), 6);
        assert_eq!(row("bouquins").1, measured);
        let summary = window.get_smb_server_summary();
        assert!(summary.contains(discovery_failure()), "{summary}");
        assert!(summary.contains("Shares completed: 1 / 1"), "{summary}");
    }

    #[test]
    fn late_discovery_from_another_server_cannot_replace_the_selected_scope() {
        slint::platform::set_platform(Box::new(Platform)).unwrap();
        let (window, controller) =
            crate::new_session(Default::default(), Some("en".into())).unwrap();
        let host = super::super::connect(&window, &controller).unwrap();
        host.borrow_mut()
            .select_smb_server(&window, server("old.invalid"));
        let (sender, receiver) = std::sync::mpsc::channel();
        host.borrow_mut()
            .server
            .operations
            .start(move || {
                receiver.recv_timeout(Duration::from_secs(5)).unwrap();
                crate::source_dialog::Outcome {
                    state: 18,
                    shares: Some(vec![share("old.invalid", "stale")]),
                    limited: false,
                }
            })
            .unwrap();
        host.borrow_mut()
            .select_smb_server(&window, server("new.invalid"));
        sender.send(()).unwrap();
        wait(&host, &window);
        assert_eq!(
            host.borrow().server.root.as_deref(),
            Some(server("new.invalid").as_path())
        );
        assert!(host.borrow().server.shares.is_empty());
        assert!(host.borrow().service.smb_snapshot().is_none());
        assert!(window.get_smb_share_rows().row_count() == 0);
    }
}

fn discovery_failure() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        tr!(
            "Share discovery failed. Open the server in Files, then try again or enter a known share."
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        tr!("Share discovery failed. Connect with Windows, then try again.")
    }
}
