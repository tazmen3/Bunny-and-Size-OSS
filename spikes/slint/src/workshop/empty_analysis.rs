use super::*;
use sortsize_core::{
    CancellationToken, MetadataValue, ValueAccuracy,
    acquisition::{Guard, Policy},
    inventory_contract::ScopeCoverage,
    resources::{disk_limit, native_free_disk},
};
use sortsize_store::inventory::{
    Error, Result,
    queries::empty_entries::{
        self, EmptyKind, EmptyParent, EmptyProgress, EmptyRow, EmptySort, EmptySummary,
    },
};
use std::{
    io,
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};

const TEMPORARY_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const MAX_FILTER_LEN: usize = 4096;
const MAX_PENDING_TASKS: usize = 2;

#[derive(Default)]
pub(super) struct State {
    input: crate::path_input::PathInput,
    picker: Option<(i32, crate::folder_picker::FolderPicker)>,
    task: Option<Task>,
    latest: Option<Publication>,
    presentation: Option<crate::presentation::Context>,
}

struct Task {
    epoch: u64,
    cancel: CancellationToken,
    commands: SyncSender<Query>,
    output: Arc<Mutex<Option<Publication>>>,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct RegisteredScope {
    id: u64,
    lane: Arc<Mutex<service::Lane>>,
}
impl Drop for RegisteredScope {
    fn drop(&mut self) {
        self.lane
            .lock()
            .unwrap()
            .duplicate_operations
            .retain(|(id, _, _)| *id != self.id);
    }
}

#[derive(Clone, Default)]
struct Query {
    filter: String,
    sort: EmptySort,
    breakdown: bool,
    offset: u64,
}

struct Page {
    query: Query,
    rows: Rows,
    previous: Option<u64>,
    next: Option<u64>,
    summary: EmptySummary,
}

enum Rows {
    Items(Vec<EmptyRow>),
    Parents(Vec<EmptyParent>),
}

enum Publication {
    Progress(EmptyProgress),
    Page(Page),
    Error(Error),
}

struct Worker {
    scope: RegisteredScope,
    preflight: Option<crate::directory_worker::Preflight>,
    root: PathBuf,
    epoch: u64,
    kind: EmptyKind,
    cancel: CancellationToken,
    commands: Receiver<Query>,
    output: Arc<Mutex<Option<Publication>>>,
}

impl Worker {
    fn publish(&self, publication: Publication) {
        *self.output.lock().unwrap() = Some(publication);
        service::notify(&self.scope.lane);
    }

    fn run(&mut self) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted).into());
        }
        if self.preflight.take().is_some_and(|check| !check()) {
            return Err(io::Error::from(io::ErrorKind::NotConnected).into());
        }
        if self.cancel.is_cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted).into());
        }
        // Resolve registered mounts without opening or reconnecting a share.
        let native = crate::source_registry::scan_root(&self.root)?;
        {
            let mut lane = self.scope.lane.lock().unwrap();
            if lane.source_epoch != self.epoch
                || lane.stop
                || service::source_disconnected(&lane, &native)
                || self.cancel.is_cancelled()
            {
                return Err(io::Error::from(io::ErrorKind::Interrupted).into());
            }
            if native != self.root
                && let Some((_, roots, _)) = lane
                    .duplicate_operations
                    .iter_mut()
                    .find(|(id, _, _)| *id == self.scope.id)
            {
                roots.push(native.clone());
            }
        }
        let location = service::work_location()?;
        let free = native_free_disk(location.path()).unwrap_or(0);
        let quota = disk_limit(free).min(TEMPORARY_LIMIT);
        let guard = Arc::new(location.scope(Guard::open(
            &native,
            Policy {
                explicit_root: true,
                ..Default::default()
            },
        )?)?);
        let mut update = Instant::now();
        let search = empty_entries::prepare(
            guard,
            location.path(),
            quota,
            self.kind,
            &self.cancel,
            |progress| {
                if update.elapsed() >= Duration::from_millis(150) {
                    self.publish(Publication::Progress(*progress));
                    update = Instant::now();
                }
            },
        )?;
        let mut query = Query::default();
        loop {
            let (rows, previous, next) = if query.breakdown {
                let page = search.parents(&query.filter, query.offset, &self.cancel)?;
                (
                    Rows::Parents(page.rows),
                    page.previous_offset,
                    page.next_offset,
                )
            } else {
                let page = search.page(&query.filter, query.sort, query.offset, &self.cancel)?;
                (
                    Rows::Items(page.rows),
                    page.previous_offset,
                    page.next_offset,
                )
            };
            if self.cancel.is_cancelled() {
                return Err(io::Error::from(io::ErrorKind::Interrupted).into());
            }
            self.publish(Publication::Page(Page {
                query,
                rows,
                previous,
                next,
                summary: *search.summary(),
            }));
            query = loop {
                if self.cancel.is_cancelled() {
                    return Err(io::Error::from(io::ErrorKind::Interrupted).into());
                }
                match self.commands.recv_timeout(Duration::from_millis(150)) {
                    Ok(query) => break query,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            };
        }
    }
}

fn kind(mode: i32) -> Option<EmptyKind> {
    match mode {
        1 => Some(EmptyKind::Files),
        2 => Some(EmptyKind::Directories),
        _ => None,
    }
}

fn counts(progress: &EmptyProgress) -> String {
    use crate::number_format::integer;
    tr_format!(
        "Matches: {} · Folders examined: {} · Entries observed: {} · Errors: {}.",
        integer(progress.matches.into()),
        integer(progress.directories_scanned.into()),
        integer(progress.entries_seen.into()),
        integer(progress.errors.into()),
    )
}

fn scan_error(error: &Error) -> String {
    match error {
        Error::Io(error) if error.kind() == io::ErrorKind::Interrupted => tr!("Cancelled").into(),
        Error::Io(error) if error.kind() == io::ErrorKind::NotConnected => {
            tr!("Disconnected by user").into()
        }
        Error::Io(error) if error.kind() == io::ErrorKind::QuotaExceeded => {
            format!("{} {error}", tr!("Temporary storage limit reached."))
        }
        Error::Contract("source root already has an active acquisition") => {
            tr!("An analysis is active. Wait for completion or cancel it.").into()
        }
        _ => format!("{} {}", tr!("Could not start search."), error),
    }
}

fn clear_page(w: &SortSizeWindow) {
    w.set_empty_rows(ModelRc::default());
    w.set_empty_selected(-1);
    w.set_empty_previous(false);
    w.set_empty_next(false);
    w.set_empty_can_query(false);
}

impl Host {
    fn empty_root(&self, w: &SortSizeWindow) -> Option<PathBuf> {
        let root = self.empty.input.resolve(w.get_empty_scope().as_str());
        let logical_smb = cfg!(target_os = "linux")
            && root
                .to_str()
                .is_some_and(|p| p.starts_with("smb://") || p.starts_with(r"\\"));
        ((root.is_absolute() || logical_smb)
            && !root
                .components()
                .any(|part| part == std::path::Component::ParentDir))
        .then_some(root)
    }

    pub(super) fn open_empty_analysis(
        &mut self,
        w: &SortSizeWindow,
        c: &Controller,
        mode: i32,
        scope: Option<PathBuf>,
    ) {
        if kind(mode).is_none() || w.get_choosing_folder() {
            return;
        }
        self.close_search(w);
        self.show_analysis(w, c);
        w.set_empty_mode(mode);
        w.set_empty_sort(0);
        w.set_empty_breakdown(false);
        w.set_empty_filter("".into());
        let selected_folder = self.current.as_ref().and_then(|page| {
            let (_, target) = page
                .folders
                .iter()
                .zip(&page.folder_keys)
                .find(|(row, _)| row.selected)?;
            match target {
                folder_tree::TreeTarget::Folder { path, .. } => Some(path.as_ref().clone()),
                _ => None,
            }
        });
        if let Some(path) = scope
            .or(selected_folder)
            .or_else(|| self.requested.clone())
            .or_else(|| c.scan_root.clone())
        {
            w.set_empty_scope(self.empty.input.select(path).into());
        }
        self.update_empty_ready(w, c);
        w.invoke_focus_empty_analysis();
    }

    pub(super) fn change_empty_kind(&mut self, w: &SortSizeWindow, c: &Controller, mode: i32) {
        if kind(mode).is_none()
            || w.get_empty_busy()
            || w.get_choosing_folder()
            || w.get_empty_mode() == mode
            || w.get_empty_mode() == 0
        {
            return;
        }
        self.empty.task = None;
        self.empty.latest = None;
        clear_page(w);
        w.set_empty_mode(mode);
        w.set_empty_sort(0);
        w.set_empty_breakdown(false);
        w.set_empty_filter("".into());
        w.set_empty_status("".into());
        self.update_empty_ready(w, c);
    }

    pub(super) fn close_empty_analysis(&mut self, w: &SortSizeWindow) {
        if kind(w.get_empty_mode()).is_some() {
            self.cancel_empty_analysis(w);
            w.set_empty_mode(0);
            w.set_empty_scope("".into());
            w.set_empty_status("".into());
            w.set_empty_ready(false);
            self.empty.input.edited();
        }
    }

    pub(super) fn cancel_empty_analysis(&mut self, w: &SortSizeWindow) {
        self.empty.task = None;
        self.empty.latest = None;
        w.set_empty_busy(false);
        clear_page(w);
        w.set_empty_status(tr!("Cancelled").into());
    }

    pub(super) fn edit_empty_scope(&mut self, w: &SortSizeWindow, c: &Controller) {
        self.empty.input.edited();
        self.empty.task = None;
        self.empty.latest = None;
        clear_page(w);
        w.set_empty_status("".into());
        self.update_empty_ready(w, c);
    }

    fn update_empty_ready(&self, w: &SortSizeWindow, c: &Controller) {
        let root = self.empty_root(w);
        let disconnected = root.as_ref().is_some_and(|root| {
            c.registry.is_disconnected(root) || self.service.source_disconnected(root)
        });
        w.set_empty_ready(
            root.is_some() && !disconnected && !w.get_empty_busy() && !w.get_choosing_folder(),
        );
    }

    pub(super) fn pick_empty_scope(&mut self, w: &SortSizeWindow) {
        if w.get_choosing_folder() || w.get_empty_busy() || kind(w.get_empty_mode()).is_none() {
            return;
        }
        #[cfg(any(target_os = "linux", windows))]
        {
            let mut dialog = rfd::FileDialog::new()
                .set_title(tr!("Choose a folder to scan"))
                .set_parent(&w.window().window_handle());
            if let Some(root) = self.empty_root(w) {
                dialog = dialog.set_directory(root);
            }
            if let Ok(picker) =
                crate::folder_picker::FolderPicker::start(move || dialog.pick_folder())
            {
                self.empty.picker = Some((w.get_empty_mode(), picker));
                w.set_choosing_folder(true);
                w.set_empty_ready(false);
            }
        }
    }

    pub(super) fn start_empty_analysis(&mut self, w: &SortSizeWindow, c: &Controller) {
        if !w.get_empty_ready() || w.get_empty_busy() {
            return;
        }
        let (Some(kind), Some(root)) = (kind(w.get_empty_mode()), self.empty_root(w)) else {
            return;
        };
        let preflight = c.registry.scan_preflight(&root);
        self.search_id += 1;
        let id = self.search_id;
        let cancel = CancellationToken::default();
        let lane = Arc::clone(&self.service.lane);
        let epoch = {
            let mut l = lane.lock().unwrap();
            if c.registry.is_disconnected(&root) || service::source_disconnected(&l, &root) {
                w.set_empty_status(tr!("Disconnected by user").into());
                return;
            }
            if l.duplicate_operations.len() >= MAX_PENDING_TASKS || l.acquisition_in_progress() {
                w.set_empty_status(
                    tr!("An analysis is active. Wait for completion or cancel it.").into(),
                );
                return;
            }
            l.duplicate_operations
                .push((id, vec![root.clone()], cancel.clone()));
            l.source_epoch
        };
        let output = Arc::new(Mutex::new(None));
        let (commands, incoming) = mpsc::sync_channel(1);
        let mut worker = Worker {
            scope: RegisteredScope { id, lane },
            preflight,
            root,
            epoch,
            kind,
            cancel: cancel.clone(),
            output: output.clone(),
            commands: incoming,
        };
        let presentation = c.presentation;
        let started = std::thread::Builder::new()
            .name("bunny-empty-analysis".into())
            .spawn(move || {
                let _scope = presentation.enter();
                if let Err(error) = worker.run() {
                    worker.publish(Publication::Error(error));
                }
            });
        if started.is_err() {
            w.set_empty_status(tr!("Could not start search.").into());
            return;
        }
        self.empty.latest = None;
        clear_page(w);
        self.empty.task = Some(Task {
            epoch,
            cancel,
            commands,
            output,
        });
        w.set_empty_busy(true);
        w.set_empty_ready(false);
        w.set_empty_breakdown(false);
        w.set_empty_sort(0);
        w.set_empty_filter("".into());
        w.set_empty_status(tr!("Search in progress…").into());
        w.invoke_focus_empty_results();
    }

    pub(super) fn query_empty_analysis(&mut self, w: &SortSizeWindow, direction: Option<bool>) {
        if !w.get_empty_can_query() || w.get_empty_busy() {
            return;
        }
        let (Some(task), Some(Publication::Page(page))) = (&self.empty.task, &self.empty.latest)
        else {
            return;
        };
        let query = if let Some(next) = direction {
            let Some(offset) = (if next { page.next } else { page.previous }) else {
                return;
            };
            Query {
                offset,
                ..page.query.clone()
            }
        } else {
            let filter = w.get_empty_filter().to_string();
            if filter.len() > MAX_FILTER_LEN {
                w.set_empty_status(tr!("The path filter is too long.").into());
                return;
            }
            Query {
                filter,
                sort: match w.get_empty_sort() {
                    1 => EmptySort::PathDescending,
                    2 => EmptySort::ModifiedNewest,
                    3 => EmptySort::ModifiedOldest,
                    _ => EmptySort::PathAscending,
                },
                breakdown: w.get_empty_breakdown(),
                offset: 0,
            }
        };
        if task.commands.try_send(query).is_ok() {
            w.set_empty_busy(true);
            w.set_empty_ready(false);
            w.set_empty_previous(false);
            w.set_empty_next(false);
            w.set_empty_can_query(false);
            w.set_empty_status(tr!("Search in progress…").into());
        }
    }

    pub(super) fn poll_empty_analysis(&mut self, w: &SortSizeWindow, c: &Controller) {
        if let Some((mode, outcome)) = self
            .empty
            .picker
            .as_ref()
            .and_then(|(mode, picker)| picker.poll().map(|outcome| (*mode, outcome)))
        {
            self.empty.picker = None;
            w.set_choosing_folder(false);
            if w.get_empty_mode() == mode
                && let Ok(Some(path)) = outcome
            {
                self.empty.task = None;
                self.empty.latest = None;
                clear_page(w);
                w.set_empty_scope(self.empty.input.select(path).into());
                w.set_empty_status("".into());
            }
        }
        if kind(w.get_empty_mode()).is_none() {
            return;
        }
        self.update_empty_ready(w, c);
        let mut changed = false;
        if let Some(task) = &self.empty.task {
            if self.service.lane.lock().unwrap().source_epoch != task.epoch
                || !w.get_search_visible()
            {
                self.close_empty_analysis(w);
                return;
            }
            if task.cancel.is_cancelled() {
                self.cancel_empty_analysis(w);
                return;
            }
            if let Ok(mut latest) = task.output.try_lock()
                && let Some(publication) = latest.take()
            {
                self.empty.latest = Some(publication);
                changed = true;
            }
        }
        if !changed && self.empty.presentation == Some(self.presentation) {
            return;
        }
        self.empty.presentation = Some(self.presentation);
        let _scope = self.presentation.enter();
        match &self.empty.latest {
            None => {}
            Some(Publication::Progress(progress)) => {
                w.set_empty_status(
                    format!("{} · {}", tr!("Search in progress…"), counts(progress)).into(),
                );
            }
            Some(Publication::Page(page)) => {
                if !changed && w.get_empty_busy() {
                    return;
                }
                self.show_empty_page(w, page, changed);
            }
            Some(Publication::Error(error)) => {
                self.empty.task = None;
                w.set_empty_busy(false);
                clear_page(w);
                w.set_empty_status(scan_error(error).into());
            }
        }
        self.update_empty_ready(w, c);
    }

    fn show_empty_page(&self, w: &SortSizeWindow, page: &Page, changed: bool) {
        let files = kind(w.get_empty_mode()) == Some(EmptyKind::Files);
        let model = match &page.rows {
            Rows::Items(rows) => rows
                .iter()
                .map(|row| crate::EmptyResultRow {
                    name: row
                        .path
                        .file_name()
                        .map_or_else(|| label(&row.path), |name| label(Path::new(name)))
                        .into(),
                    path: label(&row.path).into(),
                    parent: row.parent.as_deref().map(label).unwrap_or_default().into(),
                    detail: if files {
                        crate::date_label::format_date(row.modified.map_or(
                            MetadataValue::Unknown,
                            |value| MetadataValue::Known {
                                value,
                                accuracy: ValueAccuracy::Exact,
                            },
                        ))
                        .into()
                    } else {
                        "".into()
                    },
                })
                .collect::<Vec<_>>(),
            Rows::Parents(rows) => rows
                .iter()
                .map(|row| {
                    let path = row.path.as_deref().map(label).unwrap_or_default();
                    let name = row
                        .path
                        .as_deref()
                        .and_then(Path::file_name)
                        .map(|name| label(Path::new(name)))
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| path.clone());
                    crate::EmptyResultRow {
                        name: name.into(),
                        path: path.into(),
                        parent: "".into(),
                        detail: crate::number_format::integer(row.count.into()).into(),
                    }
                })
                .collect(),
        };
        w.set_empty_busy(false);
        let has_rows = !model.is_empty();
        if changed {
            w.set_empty_scroll_y(0.);
            w.set_empty_selected(if has_rows { 0 } else { -1 });
        }
        w.set_empty_rows(Rc::new(VecModel::from(model)).into());
        w.set_empty_previous(page.previous.is_some());
        w.set_empty_next(page.next.is_some());
        w.set_empty_can_query(true);
        let coverage = match page.summary.coverage {
            ScopeCoverage::Complete => tr!("Complete coverage"),
            _ => tr!("Incomplete coverage"),
        };
        let status = format!("{coverage} · {}", counts(&page.summary.progress));
        w.set_empty_status(if has_rows {
            status.into()
        } else {
            format!("{status} {}", tr!("No match on this page.")).into()
        });
    }
}
