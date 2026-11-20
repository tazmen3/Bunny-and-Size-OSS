//! Explicit, tokenized source revocation. Opening or abandoning a dialog has no effects.
use super::*;
use crate::volume_view::ConnectionState;
use crate::{
    NetworkDisconnectWindow,
    source_registry::{self, Kind, Source},
};
use std::sync::mpsc::{self, Receiver};

#[cfg(test)]
type ReconnectProvider = Arc<dyn Fn(&Source, usize) -> Result<(), ConnectionState> + Send + Sync>;

#[cfg(any(windows, target_os = "linux"))]
type Ticket = sortsize_core::volumes::smb_disconnect::DisconnectTicket;
#[cfg(not(any(windows, target_os = "linux")))]
type Ticket = ();

struct Intent {
    source: Source,
    affected: i32,
    ticket: Option<Ticket>,
}
enum NativeResult {
    Reconnected(Source, Result<(), ConnectionState>),
    #[cfg(any(windows, target_os = "linux"))]
    Prepared(Intent),
    #[cfg(any(windows, target_os = "linux"))]
    Disconnected(bool),
}
#[derive(Default)]
pub(super) struct State {
    host: std::rc::Weak<RefCell<Host>>,
    controller: std::rc::Weak<RefCell<Controller>>,
    token: i32,
    intent: Option<Intent>,
    dialog: Option<NetworkDisconnectWindow>,
    native: Option<Receiver<NativeResult>>,
    save_pending: bool,
    save_is_disconnect: bool,
    saved: Option<bool>,
    windows_result: Option<bool>,
    reconnect_source: Option<Source>,
    #[cfg(test)]
    reconnect_provider: Option<ReconnectProvider>,
}

impl Host {
    fn source_context(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        source: Source,
        reconnect: bool,
    ) {
        let _scope = controller.presentation.enter();
        if source.kind == Kind::Local {
            return;
        }
        if self.disconnect.native.is_some() {
            window.set_errors(native_busy().into());
            return;
        }
        if reconnect {
            self.abandon_disconnect();
            // A child cannot clear a parent exclusion. Explicit child exclusions
            // also survive reconnection of their parent.
            if controller
                .registry
                .disconnected_parent(&source.path)
                .is_some()
            {
                show_parent_reconnect_required(window, controller);
                return;
            }
            if !controller.source_operations.reserve() {
                window.set_errors(native_busy().into());
                return;
            }
            #[cfg(windows)]
            let owner = crate::source_dialog::owner_handle(window.window());
            #[cfg(not(windows))]
            let owner = 0;
            #[cfg(test)]
            let provider = self.disconnect.reconnect_provider.clone();
            let pending_source = source.clone();
            let (tx, rx) = mpsc::sync_channel(1);
            let started = std::thread::Builder::new()
                .name("bunny-source-reconnect".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        #[cfg(test)]
                        if let Some(provider) = provider {
                            return provider(&pending_source, owner);
                        }
                        reconnect_native(&pending_source, owner)
                    }))
                    .unwrap_or(Err(ConnectionState::Failed));
                    let _ = tx.send(NativeResult::Reconnected(pending_source, result));
                });
            if started.is_ok() {
                self.disconnect.native = Some(rx);
                self.disconnect.reconnect_source = Some(source.clone());
                window.set_smb_reconnect_busy(true);
                controller
                    .sources
                    .set_connection_state(&source.path, ConnectionState::Checking);
                controller.show_sources(window);
                if let Some(dialog) = &controller.source_dialog {
                    dialog.set_busy(true);
                    dialog.set_reconnect_status(tr!("Checking access…").into());
                }
                window.set_errors(tr!("Checking access…").into());
            } else {
                controller.source_operations.release();
                controller
                    .sources
                    .set_connection_state(&source.path, ConnectionState::Failed);
                controller.show_sources(window);
                window.set_errors(tr!("Connection failed").into());
            }
            return;
        }
        if controller.registry.is_disconnected(&source.path) {
            return;
        }
        self.abandon_disconnect();
        let affected = if source.is_server() {
            let mut paths = std::collections::HashSet::new();
            for path in self.server.shares.iter().chain(
                controller
                    .registry
                    .entries
                    .iter()
                    .filter(|s| !s.is_server())
                    .map(|s| &s.path),
            ) {
                if source_registry::covers(&source.path, path) {
                    paths.insert(path.clone());
                }
            }
            if let Some(snapshot) = self.service.smb_snapshot() {
                for share in snapshot.shares {
                    if source_registry::covers(&source.path, &share.path) {
                        paths.insert(share.path);
                    }
                }
            }
            paths.len() as i32
        } else {
            -1
        };
        let intent = Intent {
            source,
            affected,
            ticket: None,
        };
        #[cfg(any(windows, target_os = "linux"))]
        {
            let fallback = Intent {
                source: intent.source.clone(),
                affected: intent.affected,
                ticket: None,
            };
            let (tx, rx) = mpsc::sync_channel(1);
            if std::thread::Builder::new()
                .name("bunny-disconnect-prepare".into())
                .spawn(move || {
                    let mut intent = intent;
                    intent.ticket = sortsize_core::volumes::smb_disconnect::prepare(
                        &intent.source.path,
                        intent.source.is_server(),
                    )
                    .ok();
                    let _ = tx.send(NativeResult::Prepared(intent));
                })
                .is_ok()
            {
                self.disconnect.native = Some(rx);
            } else {
                self.show_disconnect_confirmation(window, fallback);
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        self.show_disconnect_confirmation(window, intent);
    }

    fn abandon_disconnect(&mut self) {
        self.disconnect.token = self
            .disconnect
            .token
            .checked_add(1)
            .expect("disconnect token exhausted");
        self.disconnect.intent = None;
        if let Some(dialog) = self.disconnect.dialog.take() {
            let _ = dialog.hide();
        }
    }

    fn finish_reconnect(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        source: Source,
        result: Result<(), ConnectionState>,
    ) {
        controller.source_operations.release();
        self.disconnect.reconnect_source = None;
        window.set_smb_reconnect_busy(false);
        if let Some(dialog) = &controller.source_dialog {
            dialog.set_busy(false);
        }
        if let Err(state) = result {
            controller.sources.set_connection_state(&source.path, state);
            controller.show_sources(window);
            let message = format!("{}\n{}", state.text(), native_instructions());
            if let Some(dialog) = &controller.source_dialog {
                dialog.set_reconnect_status(message.clone().into());
            }
            window.set_errors(message.into());
            return;
        }
        // Native success cannot clear a child exclusion if its parent policy
        // changed while access was being checked. Keep both exclusions intact.
        if controller
            .registry
            .disconnected_parent(&source.path)
            .is_some()
        {
            controller
                .sources
                .set_connection_state(&source.path, ConnectionState::Unavailable);
            // The parent exclusion may be newer than the displayed discovery.
            // Rebuild all source rows from the current policy before reporting
            // the refusal, including siblings hidden by that parent.
            self.refresh_disconnect_sources(window, controller);
            show_parent_reconnect_required(window, controller);
            return;
        }
        // Native access is an observation, not authority to bypass an exclusion.
        // The exact exclusion is removed only for this explicit completed action.
        let changed = controller
            .registry
            .find(&source.path)
            .is_some_and(|entry| entry.disconnected);
        if changed && controller.registry.reconnect(&source.path).is_err() {
            controller
                .sources
                .set_connection_state(&source.path, ConnectionState::Failed);
            controller.show_sources(window);
            window.set_errors(tr!("Disconnection failed. The source has not been changed.").into());
            return;
        }
        if changed {
            controller.source_save_result = None;
            self.disconnect.save_pending = controller.registry.saving_available();
            self.disconnect.save_is_disconnect = false;
            self.disconnect.saved = if self.disconnect.save_pending {
                None
            } else {
                Some(false)
            };
        }
        self.disconnect.windows_result = None;
        // Older stores can contain separately excluded spelling variants of
        // one SMB scope. Clearing this exact choice must not claim visibility
        // or clear service admission while another explicit exclusion remains.
        if controller.registry.is_disconnected(&source.path) {
            controller
                .sources
                .set_connection_state(&source.path, ConnectionState::Unavailable);
            self.refresh_disconnect_sources(window, controller);
            window.set_errors(tr!("Disconnected").into());
            if let Some(remaining) =
                controller
                    .registry
                    .disconnected_sources()
                    .into_iter()
                    .find(|entry| {
                        source_registry::covers(&entry.path, &source.path)
                            && source_registry::covers(&source.path, &entry.path)
                    })
                && let Some(dialog) = &controller.source_dialog
            {
                controller.propose_source_reconnect(dialog, &remaining, 21);
                window.set_errors(dialog.get_access_status());
            }
            return;
        }
        self.service
            .reconnect_sources(std::slice::from_ref(&source.path));
        controller
            .sources
            .set_connection_state(&source.path, ConnectionState::Available);
        // Reintroducing a row invalidates any old NAS row index or scope grant.
        self.server.token = self
            .server
            .token
            .checked_add(1)
            .expect("server intent exhausted");
        if let Some(dialog) = self.server.dialog.take() {
            let _ = dialog.hide();
        }
        self.refresh_disconnect_sources(window, controller);
        if let Some(dialog) = &controller.source_dialog {
            dialog.set_reconnect_status(dialog.get_reconnected_message());
        }
        // Preserve the active local analysis and its status. Reconnection never
        // writes scan-path, publishes Start/retry, or rearms an old intention.
        if !window.get_busy() {
            window
                .set_status(tr!("Source reconnected. Click Scan to start a new analysis.").into());
        }
        if changed {
            self.render_disconnect_result(window);
        } else {
            window.set_errors("".into());
        }
    }

    fn show_disconnect_confirmation(&mut self, window: &SortSizeWindow, intent: Intent) {
        let Ok(dialog) = NetworkDisconnectWindow::new() else {
            return;
        };
        dialog
            .global::<crate::WorkshopTheme>()
            .set_mode(window.global::<crate::WorkshopTheme>().get_mode());
        dialog.set_request_token(self.disconnect.token);
        dialog.set_source_name(
            if intent.source.label.is_empty() {
                label(&intent.source.path)
            } else {
                intent.source.label.clone()
            }
            .into(),
        );
        dialog.set_source_path(label(&intent.source.path).into());
        dialog.set_affected_share_count(intent.affected);
        dialog.set_windows_platform(cfg!(windows));
        dialog.set_windows_option_available(cfg!(windows) && intent.ticket.is_some());
        dialog.set_linux_platform(cfg!(target_os = "linux"));
        dialog.set_linux_option_available(cfg!(target_os = "linux") && intent.ticket.is_some());
        dialog.set_disconnect_windows(false);
        dialog.set_disconnect_linux(false);
        let host = self.disconnect.host.clone();
        let main = window.as_weak();
        let controller = self.disconnect.controller.clone();
        dialog.on_accepted(move |token, windows| {
            if let (Some(host), Some(window), Some(controller)) =
                (host.upgrade(), main.upgrade(), controller.upgrade())
            {
                host.borrow_mut().confirm_disconnect(
                    &window,
                    &mut controller.borrow_mut(),
                    token,
                    windows,
                );
            }
        });
        let host = self.disconnect.host.clone();
        dialog.on_rejected(move |token| {
            if let Some(host) = host.upgrade() {
                let mut host = host.borrow_mut();
                if token == host.disconnect.token {
                    host.abandon_disconnect();
                }
            }
        });
        let host = self.disconnect.host.clone();
        dialog.window().on_close_requested(move || {
            if let Some(host) = host.upgrade() {
                host.borrow_mut().abandon_disconnect();
            }
            slint::CloseRequestResponse::HideWindow
        });
        self.disconnect.intent = Some(intent);
        self.disconnect.dialog = Some(dialog);
        if let Some(dialog) = &self.disconnect.dialog {
            let _ = dialog.show();
        }
    }

    fn confirm_disconnect(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        token: i32,
        windows: bool,
    ) {
        let _scope = controller.presentation.enter();
        if token != self.disconnect.token || self.disconnect.dialog.is_none() {
            return;
        }
        let Some(intent) = self.disconnect.intent.take() else {
            return;
        };
        self.abandon_disconnect();
        let root = intent.source.path.clone();
        // Persisted exclusion and live guard are revoked before cancellation or OS action.
        controller.source_save_result = None;
        if controller.registry.disconnect(intent.source).is_err() {
            window.set_errors(tr!("Disconnection failed. The source has not been changed.").into());
            return;
        }
        let cache_affected = {
            let lane = self.service.lane.lock().unwrap();
            lane.cache
                .as_ref()
                .and_then(|request| request.action.acquisition_source())
                .is_some_and(|path| source_registry::covers(&root, path))
                || lane
                    .source_operation
                    .as_ref()
                    .is_some_and(|(path, _)| source_registry::covers(&root, path))
        };
        if cache_affected {
            self.cache_cancel.cancel();
            self.cache_wait = None;
            self.cache_comparing = false;
        }
        self.service.disconnect_sources(std::slice::from_ref(&root));
        if controller
            .pending_auto
            .as_ref()
            .is_some_and(|(path, _)| source_registry::covers(&root, path))
        {
            controller.pending_auto = None;
        }
        if controller
            .restored_selection
            .as_ref()
            .is_some_and(|s| source_registry::covers(&root, &s.root))
        {
            controller.restored_selection = None;
        }
        if self
            .requested
            .as_ref()
            .is_some_and(|path| source_registry::covers(&root, path))
        {
            controller.auto_analysis.cancel();
            if window.get_busy() {
                window.set_cancelling(true);
            }
        }
        // Any previously displayed scan confirmation must lose its one-use grant.
        self.server.token = self
            .server
            .token
            .checked_add(1)
            .expect("server intent exhausted");
        if let Some(dialog) = self.server.dialog.take() {
            let _ = dialog.hide();
        }
        self.server.operations.invalidate();
        self.refresh_disconnect_sources(window, controller);
        self.neutralize_disconnected_view(window, controller, &root);
        self.disconnect.save_pending = controller.registry.saving_available();
        self.disconnect.save_is_disconnect = true;
        self.disconnect.saved = if self.disconnect.save_pending {
            None
        } else {
            Some(false)
        };
        self.disconnect.windows_result = None;
        self.render_disconnect_result(window);
        if windows {
            #[cfg(any(windows, target_os = "linux"))]
            if let Some(ticket) = intent.ticket {
                let (tx, rx) = mpsc::sync_channel(1);
                if std::thread::Builder::new()
                    .name("bunny-disconnect-windows".into())
                    .spawn(move || {
                        let succeeded =
                            sortsize_core::volumes::smb_disconnect::disconnect(&ticket).is_ok();
                        let _ = tx.send(NativeResult::Disconnected(succeeded));
                    })
                    .is_ok()
                {
                    self.disconnect.native = Some(rx);
                    return;
                }
            }
            self.disconnect.windows_result = Some(false);
            self.render_disconnect_result(window);
        }
    }

    fn neutralize_disconnected_view(
        &mut self,
        window: &SortSizeWindow,
        controller: &mut Controller,
        root: &Path,
    ) {
        let selected = controller
            .path_input
            .resolve(window.get_scan_path().as_str());
        // A virtual NAS panel may cover a retained file page. Only the visible
        // panel determines whether disconnecting this source clears the centre.
        let displayed = if window.get_smb_server_visible() {
            self.server.root.clone()
        } else {
            self.current
                .as_ref()
                .map(|page| PathBuf::from(&page.path))
                .filter(|path| !path.as_os_str().is_empty())
                .or_else(|| {
                    let path = PathBuf::from(window.get_current_path().as_str());
                    (!path.as_os_str().is_empty()).then_some(path)
                })
                .or_else(|| self.requested.clone())
        };
        let displayed_affected = displayed.as_deref().map_or_else(
            || source_registry::covers(root, &selected),
            |path| source_registry::covers(root, path),
        );
        if !displayed_affected {
            if source_registry::covers(root, &selected) {
                window.set_scan_path(controller.path_input.select(PathBuf::new()).into());
                window.set_selected_volume(-1);
            }
            return;
        }
        // Retire only presentation epochs. The revoked writer keeps its own
        // cancellation and SQLite generations; late pages cannot refill this view.
        self.epoch();
        {
            let mut lane = self.service.lane.lock().unwrap();
            lane.reader_target = Some(None);
            lane.reader_epoch = None;
            lane.reader_active = false;
            lane.navigation = None;
            lane.desired_folder = None;
            lane.desired_relative = None;
            lane.smb_open = None;
            lane.smb_open_path = None;
            // Keep the previous view_epoch: a cancelling writer may hand its
            // sealed G0/G1 to the reader, but that page remains stale for this UI.
        }

        self.requested = None;
        self.source_selection = None;
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
        self.close_search(window);
        self.clear_preview(window, 0);
        self.cache_cancel.cancel();
        self.cache_id += 1;
        self.cache_wait = None;
        self.cache_current = None;
        controller.tree = None;
        controller.scan_root = None;
        controller.scan_volume = None;
        if self
            .server
            .root
            .as_ref()
            .is_some_and(|path| source_registry::covers(root, path))
        {
            self.server.root = None;
            self.server.shares.clear();
            self.server.selected.clear();
            self.server.memory = None;
        }
        window.set_smb_server_visible(false);
        window.set_smb_server_context(false);
        window.set_smb_share_rows(ModelRc::default());
        window.set_smb_disconnected_share_rows(ModelRc::default());
        window.set_cache_visible(false);
        window.set_top_visible(false);
        window.set_scan_path(controller.path_input.select(PathBuf::new()).into());
        window.set_scan_source("".into());
        window.set_current_path("".into());
        window.set_entry_path("".into());
        window.set_current_folder_summary("".into());
        window.set_selection_summary("".into());
        window.set_summary("—".into());
        window.set_details(ModelRc::default());
        window.set_rows(ModelRc::default());
        window.set_top_rows(ModelRc::default());
        window.set_tiles(ModelRc::default());
        window.set_workshop_selected(-1);
        window.set_inspected_node(-1);
        window.set_selected_volume(-1);
        window.set_scanned_volume(-1);
        window.set_has_parent(false);
        window.set_has_next_detail_page(false);
        window.set_has_previous_detail_page(false);
        window.set_has_next_page(false);
        window.set_has_previous_page(false);
        window.set_can_load_details(false);
        window.set_detail_page("".into());
        window.set_tree_page("".into());
        window.set_view_warning("".into());
        window.set_inventory_warning("".into());
        window.set_owner_status("".into());
        window.set_empty_message(tr!("No inventory available.").into());
        window.set_top_note(tr!("No inventory available.").into());
        window.set_treemap_note(tr!("No inventory available.").into());
    }

    fn refresh_disconnect_sources(&mut self, window: &SortSizeWindow, controller: &mut Controller) {
        controller
            .sources
            .set_registered(&controller.registry.entries);
        controller.show_sources(window);
        let exclusions = controller
            .registry
            .entries
            .iter()
            .filter(|source| source.disconnected)
            .map(|source| source.path.clone())
            .collect::<Vec<_>>();
        if self.server.exclusions != exclusions {
            self.server.exclusions = exclusions;
            self.server.invalidate_share_rows();
        }
        self.server
            .selected
            .iter_mut()
            .zip(&self.server.shares)
            .for_each(|(selected, path)| {
                if controller.registry.is_disconnected(path) {
                    *selected = false;
                }
            });
        self.server.dirty = true;
        self.update_smb_confirmation();
        self.poll_smb_server(window, true);
    }

    pub(crate) fn poll_disconnect(&mut self, window: &SortSizeWindow, controller: &mut Controller) {
        let _scope = controller.presentation.enter();
        if let Some(saved) = controller.source_save_result.take()
            && self.disconnect.save_pending
        {
            self.disconnect.save_pending = false;
            self.disconnect.saved = Some(saved);
            self.render_disconnect_result(window);
            if !self.disconnect.save_is_disconnect
                && !saved
                && let Some(dialog) = &controller.source_dialog
            {
                dialog.set_reconnect_status(window.get_errors());
            }
        }
        let result = self.disconnect.native.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(result)) => {
                self.disconnect.native = None;
                match result {
                    NativeResult::Reconnected(source, result) => {
                        self.finish_reconnect(window, controller, source, result);
                    }
                    #[cfg(any(windows, target_os = "linux"))]
                    NativeResult::Prepared(intent) => {
                        self.show_disconnect_confirmation(window, intent)
                    }
                    #[cfg(any(windows, target_os = "linux"))]
                    NativeResult::Disconnected(success) => {
                        self.disconnect.windows_result = Some(success);
                        self.render_disconnect_result(window);
                    }
                }
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.disconnect.native = None;
                if let Some(source) = self.disconnect.reconnect_source.take() {
                    self.finish_reconnect(window, controller, source, Err(ConnectionState::Failed));
                }
            }
            _ => {}
        }
    }

    fn render_disconnect_result(&self, window: &SortSizeWindow) {
        let message = if self.disconnect.save_is_disconnect {
            match self.disconnect.saved {
                Some(true) => tr!(
                    "Source disconnected and saved. Cancellation is requested; a pending system call may finish later."
                ),
                Some(false) => tr!(
                    "Source disconnected for this session, but saving failed. Reconnect and retry before closing the application."
                ),
                None => tr!("Source disconnected. Saving this state…"),
            }
        } else if self.disconnect.saved == Some(false) {
            tr!("The source state could not be saved. It may differ after restart.")
        } else {
            ""
        };
        let os_message = self
            .disconnect
            .windows_result
            .map(|success| {
                if success {
                    native_disconnected(true)
                } else {
                    native_disconnected(false)
                }
            })
            .unwrap_or_default();
        window.set_errors(
            [message, os_message]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        );
    }
}

fn show_parent_reconnect_required(window: &SortSizeWindow, controller: &Controller) {
    let message = tr!(
        "This share is unavailable because its NAS server is disconnected. Reconnect the server first; individually disconnected shares will stay disconnected."
    );
    window.set_errors(message.into());
    if let Some(dialog) = &controller.source_dialog {
        dialog.set_result_state(0);
        dialog.set_reconnect_status(message.into());
    }
}

fn reconnect_native(source: &Source, owner: usize) -> Result<(), ConnectionState> {
    #[cfg(any(windows, target_os = "linux"))]
    if matches!(source.kind, Kind::Smb | Kind::SmbServer) {
        use sortsize_core::volumes::smb::{self, Status};
        return smb::reconnect(&source.path, source.is_server(), owner).map_err(
            |error| match error.status {
                Status::AuthenticationRequired | Status::CredentialsRejected => {
                    ConnectionState::AuthenticationRequired
                }
                Status::ServerInaccessible | Status::ShareMissing => ConnectionState::Unavailable,
                Status::AccessDenied => ConnectionState::AccessDenied,
                Status::Cancelled => ConnectionState::Cancelled,
                Status::SessionConflict => ConnectionState::SessionConflict,
                #[cfg(target_os = "linux")]
                Status::DesktopConnectionPending => ConnectionState::Unavailable,
                _ => ConnectionState::Failed,
            },
        );
    }
    let _ = owner;
    let mut check = source.clone();
    check.disconnected = false;
    source_registry::preflight(&check).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            ConnectionState::AccessDenied
        } else {
            ConnectionState::Unavailable
        }
    })
}

#[cfg(test)]
#[path = "disconnect_ui_tests.rs"]
mod tests;

fn from_volume(controller: &Controller, index: i32) -> Option<Source> {
    let volume = controller
        .sources
        .current
        .as_ref()?
        .discovery
        .volumes
        .get(usize::try_from(index).ok()?)?;
    let path = volume.mounts.first()?.path.clone();
    if let Some(source) = controller.registry.find(&path) {
        return (source.kind != Kind::Local).then(|| source.clone());
    }
    if volume.kind != sortsize_core::volumes::SourceKind::Remote {
        return None;
    }
    let kind = match &volume.identity {
        sortsize_core::volumes::VolumeIdentity::Linux { filesystem, .. }
            if filesystem == "nfs" || filesystem == "nfs4" =>
        {
            Kind::Nfs
        }
        _ => Kind::Smb,
    };
    #[cfg(target_os = "linux")]
    let path = if kind == Kind::Smb {
        sortsize_core::volumes::smb::mounted_uri(&path).unwrap_or(path)
    } else {
        path
    };
    Some(Source {
        kind,
        path,
        label: String::new(),
        disconnected: false,
    })
}

pub(super) fn connect(
    window: &SortSizeWindow,
    controller: &Rc<RefCell<Controller>>,
    host: &Rc<RefCell<Host>>,
) {
    host.borrow_mut().disconnect.host = Rc::downgrade(host);
    host.borrow_mut().disconnect.controller = Rc::downgrade(controller);
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    controller.borrow_mut().source_reconnect = Some(Rc::new(move |source| {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            host.borrow_mut()
                .source_context(&window, &mut controller.borrow_mut(), source, true);
        }
    }));
    {
        let controller = controller.borrow();
        let paths = controller
            .registry
            .entries
            .iter()
            .filter(|source| source.disconnected)
            .map(|source| source.path.clone())
            .collect::<Vec<_>>();
        host.borrow_mut().server.exclusions = paths.clone();
        host.borrow().service.disconnect_sources(&paths);
    }
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    window.on_source_context_action(move |token, index, reconnect| {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            let mut controller = controller.borrow_mut();
            if token != controller.source_publication.get() {
                return;
            }
            if let Some(source) = from_volume(&controller, index) {
                host.borrow_mut()
                    .source_context(&window, &mut controller, source, reconnect);
            }
        }
    });
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    window.on_smb_share_context_action(move |token, index, reconnect| {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            let mut host = host.borrow_mut();
            if token != host.server.share_rows_token {
                return;
            }
            let Some(path) = usize::try_from(index)
                .ok()
                .and_then(|i| host.server.active_share_index(i))
                .and_then(|i| host.server.shares.get(i))
                .cloned()
            else {
                return;
            };
            let source = Source {
                kind: Kind::Smb,
                path,
                label: String::new(),
                disconnected: false,
            };
            host.source_context(&window, &mut controller.borrow_mut(), source, reconnect);
        }
    });
    let weak = Rc::downgrade(host);
    let main = window.as_weak();
    let control = Rc::downgrade(controller);
    window.on_smb_disconnected_share_reconnect(move |token, index| {
        if let (Some(host), Some(window), Some(controller)) =
            (weak.upgrade(), main.upgrade(), control.upgrade())
        {
            let mut host = host.borrow_mut();
            if !window.get_smb_server_visible() || token != host.server.share_rows_token {
                return;
            }
            let Some(path) = usize::try_from(index)
                .ok()
                .and_then(|i| host.server.disconnected_shares().nth(i))
            else {
                return;
            };
            let mut controller = controller.borrow_mut();
            let _scope = controller.presentation.enter();
            if controller.registry.disconnected_parent(path).is_some() {
                show_parent_reconnect_required(&window, &controller);
                return;
            }
            // Discovery can use a different spelling from the persisted key.
            // Use the registered Source so success clears the exact exclusion.
            if let Some(source) = controller
                .registry
                .entries
                .iter()
                .filter(|source| {
                    source.kind == Kind::Smb
                        && source.disconnected
                        && source_registry::covers(&source.path, path)
                        && source_registry::covers(path, &source.path)
                })
                .min_by_key(|source| source.path != *path)
                .cloned()
            {
                host.source_context(&window, &mut controller, source, true);
            }
        }
    });
}

fn native_busy() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        tr!("A desktop source operation is already running. Wait for it to finish, then retry.")
    }
    #[cfg(not(target_os = "linux"))]
    {
        tr!("A Windows source operation is already running. Wait for it to finish, then retry.")
    }
}
fn native_instructions() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        tr!(
            "Open this source in Files, connect it there, then click Reconnect again. A usable native mount is required for scanning."
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        tr!(
            "Open this source in Windows File Explorer, sign in there, then click Reconnect again. Existing Windows sessions are preserved."
        )
    }
}
fn native_disconnected(success: bool) -> &'static str {
    #[cfg(target_os = "linux")]
    {
        if success {
            tr!("Source disconnected in Bunny and Size and Ubuntu.")
        } else {
            tr!("Source disconnected in Bunny and Size. The Ubuntu mount is still active.")
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if success {
            tr!("Source disconnected in Bunny and Size and Windows.")
        } else {
            tr!("Source disconnected in Bunny and Size. The Windows connection is still active.")
        }
    }
}
