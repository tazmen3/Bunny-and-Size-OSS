//! Explicit folder content comparison and its disposable pagination task.
use super::*;
use crate::number_format::integer;
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    analysis::SearchQuery,
    duplicates::DuplicateOptions,
    resources::{Category, Governor, Reservation},
};
use sortsize_store::inventory::{
    Error,
    catalogue::Saved,
    queries::{
        Row,
        duplicates::{
            self as engine, CurrentFolderResourceUsage, CurrentFolderResources,
            DuplicateResourceCause, Mode, Scope, Summary, VerificationPhase,
        },
    },
};
use std::time::{Duration, Instant};
const MAX_OPERATIONS: usize = 2;
const MAX_SELECTION_COMMANDS: usize = 256;
const CANDIDATE_DISK_BYTES: u64 = 256 * 1024 * 1024;
const MIN_CURRENT_FOLDER_RAM: u64 = 8 * 1024 * 1024;
pub(super) use engine::ReviewTarget as SelectionTarget;

#[derive(Clone, Copy)]
enum SelectionCommand {
    Reference(Option<u8>),
    Toggle(SelectionTarget, bool),
    Preview,
    Dismiss,
    Apply,
    Clear,
}

#[derive(Clone)]
pub(super) struct Inventory {
    pub saved: Saved,
    pub workspace: Option<Arc<service::RepositoryWorkspace>>,
}
#[derive(Clone)]
pub(super) struct Choice {
    pub label: String,
    pub logical_root: PathBuf,
    /// Exact native folder selected by the user, independent from inventory coverage.
    pub native_root: PathBuf,
    pub inventories: Vec<Inventory>,
}
pub(super) struct FreshInventory {
    pub profile: engine::DuplicateFileProfile,
    pub workspace: Arc<service::RepositoryWorkspace>,
    pub inventory: InventoryId,
    pub source: SourceId,
    pub generation: GenerationId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FolderPhase {
    Discovering,
    Comparing,
    Coverage,
}

impl FolderPhase {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Discovering => tr!("Discovering and checking files…"),
            Self::Comparing => tr!("Hashing file contents…"),
            Self::Coverage => tr!("Checking coverage…"),
        }
    }
}

pub(super) struct CurrentFolderRequest {
    pub profile: engine::DuplicateFileProfile,
    pub root: PathBuf,
    pub source_epoch: u64,
    pub preflight: Option<crate::directory_worker::Preflight>,
    pub options: DuplicateOptions,
    pub resources: CurrentFolderResources,
    #[cfg(test)]
    pub observe: Option<Arc<dyn Fn(FolderPhase) + Send + Sync>>,
}

enum StartKind {
    Inventories,
    Fresh(FreshInventory),
    CurrentFolder(CurrentFolderRequest),
}

#[derive(Clone, Copy)]
enum View {
    Folders,
    Files,
    Folder(u64),
}

pub(super) struct Presentation {
    pub id: u64,
    pub presentation: crate::presentation::Context,
    pub rows: Vec<Row>,
    pub model: Vec<crate::AnalysisRow>,
    /// Page-aligned display columns. Native paths and provenance stay in `rows`.
    pub duplicate_model: Vec<crate::DuplicateRow>,
    pub summary: String,
    pub warning: String,
    pub details: String,
    pub scope_summary: Vec<String>,
    pub status: String,
    pub scope: String,
    pub statistics: String,
    pub previous: bool,
    pub next: bool,
    pub complete: bool,
    pub busy: bool,
    pub results_available: bool,
    pub content_retry: bool,
    folder_cancelled: bool,
    folder_terminal: Option<FolderTerminalDetails>,
    pub groups: u64,
    pub folder_count: u64,
    pub file_count: u64,
    pub folder_view: bool,
    pub folder_detail: bool,
    pub folder_ids: Vec<u64>,
    pub reference: Option<u8>,
    pub selection_summary: String,
    pub selection_notice: String,
    pub selection_preview: String,
    pub selection_preview_open: bool,
    pub selection_ready: bool,
    pub selection_targets: Vec<Option<SelectionTarget>>,
    pub selection_epoch: u64,
    pub _memory: Option<Arc<Reservation>>,
}

enum FolderFailure {
    Cancelled,
    Failed(sortsize_store::inventory::Error),
}

#[derive(Default)]
struct FolderTelemetry {
    acquisition: engine::AcquisitionProgress,
    summary: Summary,
}

struct FolderTerminalDetails {
    failure: Option<FolderFailure>,
    telemetry: FolderTelemetry,
    options: DuplicateOptions,
    temporary_parent: PathBuf,
}

impl Presentation {
    pub(super) fn in_context(
        mut self,
        presentation: crate::presentation::Context,
        units: crate::units::Units,
    ) -> Option<Self> {
        if let Some(terminal) = &self.folder_terminal
            && let Some(failure) = &terminal.failure
        {
            let _scope = presentation.enter();
            self.presentation = presentation;
            self.summary = tr!("Incomplete coverage").into();
            let (warning, detail) = match failure {
                FolderFailure::Cancelled => (tr!("Cancelled").into(), String::new()),
                FolderFailure::Failed(error) => folder_error(error),
            };
            self.warning = warning;
            let counts = content_counts(&terminal.telemetry.summary);
            self.status = format!("{} · {}", self.warning, counts);
            self.details = format!(
                "{}\n{}\n{}\n{}\n{}",
                self.scope,
                budget_label(Some(terminal.options)),
                folder_observations(&terminal.telemetry.acquisition),
                counts,
                detail
            );
            if let Some(count) = unverified_count(&terminal.telemetry.summary) {
                self.details.push('\n');
                self.details.push_str(&count);
            }
            let resources = match &terminal.failure {
                Some(FolderFailure::Failed(Error::DuplicateResources(usage))) => {
                    Some(usage.as_ref())
                }
                _ => folder_usage(&terminal.telemetry.acquisition, &terminal.telemetry.summary),
            };
            self.details.push('\n');
            self.details.push_str(&resource_details(
                &terminal.temporary_parent,
                resources,
                units,
            ));
            self.statistics.clone_from(&self.details);
        }
        (self.presentation == presentation).then_some(self)
    }

    fn mark_folder_cancelled(&mut self) {
        self.complete = false;
        self.content_retry = true;
        if let Some(terminal) = &mut self.folder_terminal {
            terminal.failure = Some(FolderFailure::Cancelled);
        } else if !self.folder_cancelled {
            let _scope = self.presentation.enter();
            let incomplete = format!(
                "{} · {}",
                tr!("Incomplete coverage"),
                tr_format!("Exact file groups: {}.", self.file_count)
            );
            let status = format!(
                "{} · {}",
                tr!("Cancelled"),
                self.status.replacen(&self.summary, &incomplete, 1)
            );
            let previous_status = std::mem::replace(&mut self.status, status);
            self.details = self.details.replacen(&previous_status, &self.status, 1);
            self.summary = incomplete;
            let message = tr!("Cancelled. Only finished groups are shown.");
            self.warning = [message, self.warning.as_str()]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
        }
        self.folder_cancelled = true;
        self.results_available &= self.file_count > 0;
    }
}
struct Control {
    closed: bool,
    page: Option<bool>,
    view: Option<View>,
    refresh: bool,
    selection_commands: std::collections::VecDeque<SelectionCommand>,
    selection_epoch: u64,
    reference: Option<u8>,
    side_count: u8,
    units: crate::units::Units,
    presentation: crate::presentation::Context,
}
pub(super) struct Handle {
    pub id: u64,
    cancel: CancellationToken,
    control: Arc<Mutex<Control>>,
    output: Arc<Mutex<Option<Presentation>>>,
    current_folder: bool,
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.control.lock().unwrap().closed = true;
    }
}
impl Handle {
    pub fn start(
        id: u64,
        a: Choice,
        b: Choice,
        lane: Arc<Mutex<service::Lane>>,
        units: crate::units::Units,
        presentation: crate::presentation::Context,
    ) -> std::io::Result<Self> {
        Self::start_choices(
            id,
            a,
            Some((b, None)),
            StartKind::Inventories,
            lane,
            units,
            presentation,
        )
    }
    pub fn start_three(
        id: u64,
        a: Choice,
        b: Choice,
        c: Choice,
        lane: Arc<Mutex<service::Lane>>,
        units: crate::units::Units,
        presentation: crate::presentation::Context,
    ) -> std::io::Result<Self> {
        Self::start_choices(
            id,
            a,
            Some((b, Some(c))),
            StartKind::Inventories,
            lane,
            units,
            presentation,
        )
    }
    pub fn start_single(
        id: u64,
        logical_root: PathBuf,
        native_root: PathBuf,
        fresh: FreshInventory,
        lane: Arc<Mutex<service::Lane>>,
        units: crate::units::Units,
        presentation: crate::presentation::Context,
    ) -> std::io::Result<Self> {
        let choice = Choice {
            label: short_label(&native_root),
            logical_root,
            native_root,
            inventories: Vec::new(),
        };
        Self::start_choices(
            id,
            choice,
            None,
            StartKind::Fresh(fresh),
            lane,
            units,
            presentation,
        )
    }
    pub fn start_current_folder(
        id: u64,
        request: CurrentFolderRequest,
        lane: Arc<Mutex<service::Lane>>,
        units: crate::units::Units,
        presentation: crate::presentation::Context,
    ) -> std::io::Result<Self> {
        let choice = Choice {
            label: short_label(&request.root),
            logical_root: request.root.clone(),
            native_root: request.root.clone(),
            inventories: Vec::new(),
        };
        Self::start_choices(
            id,
            choice,
            None,
            StartKind::CurrentFolder(request),
            lane,
            units,
            presentation,
        )
    }
    fn start_choices(
        id: u64,
        a: Choice,
        additional: Option<(Choice, Option<Choice>)>,
        mut kind: StartKind,
        lane: Arc<Mutex<service::Lane>>,
        units: crate::units::Units,
        presentation: crate::presentation::Context,
    ) -> std::io::Result<Self> {
        let current_folder = matches!(&kind, StartKind::CurrentFolder(_));
        let folder_options = match &kind {
            StartKind::CurrentFolder(request) => Some(request.options),
            _ => None,
        };
        let (b, c) = additional.map_or((None, None), |(b, c)| (Some(b), c));
        let choices = || std::iter::once(&a).chain(b.as_ref()).chain(c.as_ref());
        if choices()
            .map(|choice| choice.inventories.len())
            .sum::<usize>()
            > 128
        {
            return Err(std::io::Error::other("duplicate inventory count budget"));
        }
        let cancel = CancellationToken::default();
        let roots = choices()
            .flat_map(|c| {
                std::iter::once(c.logical_root.clone())
                    .chain(std::iter::once(c.native_root.clone()))
                    .chain(c.inventories.iter().map(|i| i.saved.root.clone()))
            })
            .collect::<Vec<_>>();
        {
            let mut l = lane.lock().unwrap();
            if let StartKind::CurrentFolder(request) = &kind
                && (l.source_epoch != request.source_epoch || l.acquisition_in_progress())
            {
                return Err(std::io::Error::other(tr!(
                    "An analysis is active. Wait for completion or cancel it."
                )));
            }
            if l.duplicate_operations.len() >= MAX_OPERATIONS {
                return Err(std::io::Error::other(tr!(
                    "Another duplicate task is still closing. Retry shortly."
                )));
            }
            if roots
                .iter()
                .any(|root| service::source_disconnected(&l, root))
            {
                return Err(std::io::Error::other(tr!(
                    "This source is disconnected. Open + Add a source > Disconnected sources and click Reconnect before scanning."
                )));
            }
            l.duplicate_operations.push((id, roots, cancel.clone()));
        }
        let control = Arc::new(Mutex::new(Control {
            closed: false,
            page: None,
            view: None,
            refresh: false,
            selection_commands: Default::default(),
            selection_epoch: 0,
            reference: None,
            side_count: choices().count() as u8,
            units,
            presentation,
        }));
        let output = Arc::new(Mutex::new(None));
        let worker_control = control.clone();
        let worker_output = output.clone();
        let worker_cancel = cancel.clone();
        let worker_lane = lane.clone();
        let spawn = std::thread::Builder::new()
            .name("bunny-duplicates".into())
            .spawn(move || {
                let _scope = presentation.enter();
                let folder_temporary_parent = if let StartKind::CurrentFolder(request) = &mut kind {
                    Some(
                        request
                            .resources
                            .temporary_parent
                            .get_or_insert_with(std::env::temp_dir)
                            .clone(),
                    )
                } else {
                    None
                };
                let profile = match &kind {
                    StartKind::Inventories => None,
                    StartKind::Fresh(fresh) => Some(fresh.profile),
                    StartKind::CurrentFolder(request) => Some(request.profile),
                };
                let mut context = Context {
                    profile,
                    id,
                    a,
                    b,
                    c,
                    lane: worker_lane.clone(),
                    control: worker_control,
                    output: worker_output,
                    cancel: worker_cancel,
                    folder_options,
                    folder_temporary_parent,
                };
                let mut telemetry = FolderTelemetry::default();
                let result = (|| {
                    if let StartKind::CurrentFolder(request) = kind {
                        return run_current_folder(&context, request, &mut telemetry);
                    }
                    if let StartKind::Fresh(fresh) = kind {
                        let saved = fresh
                            .workspace
                            .saved_records(Some(fresh.generation))?
                            .into_iter()
                            .find(|saved| {
                                saved.root == context.a.native_root
                                    && saved.context.inventory == fresh.inventory
                                    && saved.context.source == fresh.source
                            })
                            .ok_or(sortsize_store::inventory::Error::Contract(
                                "the inventory prepared for this folder is unavailable",
                            ))?;
                        context.a.inventories.push(Inventory {
                            saved,
                            workspace: Some(fresh.workspace),
                        });
                    }
                    run(&context)
                })();
                if let Err(error) = result {
                    let current = context.control.lock().unwrap().presentation;
                    let _scope = current.enter();
                    let mut p = context.busy(&telemetry.summary);
                    p.busy = false;
                    if let Some(options) = folder_options
                        && let Some(temporary_parent) = context.folder_temporary_parent.clone()
                    {
                        p.content_retry = true;
                        p.folder_terminal = Some(FolderTerminalDetails {
                            failure: Some(FolderFailure::Failed(error)),
                            telemetry,
                            options,
                            temporary_parent,
                        });
                    } else {
                        p.status = format!("{}: {}", tr!("Incomplete coverage"), error);
                        p.summary = tr!("Incomplete coverage").into();
                        p.warning = p.status.clone();
                        if context.cancel.is_cancelled() {
                            p.warning.push('\n');
                            p.warning
                                .push_str(tr!("Cancelled. Only finished groups are shown."));
                        }
                    }
                    context.publish(p);
                }
                worker_lane
                    .lock()
                    .unwrap()
                    .duplicate_operations
                    .retain(|(task, _, _)| *task != id);
            });
        if let Err(error) = spawn {
            lane.lock()
                .unwrap()
                .duplicate_operations
                .retain(|(task, _, _)| *task != id);
            return Err(error);
        }
        Ok(Self {
            id,
            cancel,
            control,
            output,
            current_folder,
        })
    }
    pub fn take(&self) -> Option<Presentation> {
        let control = self.control.lock().unwrap();
        let mut output = self.output.lock().unwrap().take()?;
        if control.closed {
            return None;
        }
        let cancelled = self.cancel.is_cancelled();
        if self.current_folder && cancelled {
            if output.busy {
                return None;
            }
            output.mark_folder_cancelled();
            output.selection_epoch = control.selection_epoch;
        }
        (output.selection_epoch == control.selection_epoch
            && (!cancelled || !output.selection_ready))
            .then_some(output)
    }
    fn selection_command(&self, command: SelectionCommand) {
        self.selection_command_for_reference(command, None);
    }
    fn selection_command_for_reference(
        &self,
        command: SelectionCommand,
        expected: Option<Option<u8>>,
    ) {
        let mut control = self.control.lock().unwrap();
        if control.side_count == 1
            || expected.is_some_and(|reference| reference != control.reference)
        {
            return;
        }
        if let SelectionCommand::Reference(side) = command {
            if side.is_some_and(|side| side >= control.side_count) || control.reference == side {
                return;
            }
            control.reference = side;
        }
        control.selection_epoch = control.selection_epoch.wrapping_add(1);
        // Bound a pathological event burst. Clear unknown old checks rather
        // than silently applying only part of the queued review intentions.
        if control.selection_commands.len() >= MAX_SELECTION_COMMANDS {
            control.selection_commands.clear();
            let reference = control.reference;
            control
                .selection_commands
                .push_back(SelectionCommand::Reference(reference));
            control
                .selection_commands
                .push_back(SelectionCommand::Clear);
        }
        control.selection_commands.push_back(command);
    }
    pub fn reference(&self, side: Option<u8>) {
        self.selection_command(SelectionCommand::Reference(side));
    }
    pub fn toggle(&self, target: SelectionTarget, selected: bool, expected_reference: Option<u8>) {
        self.selection_command_for_reference(
            SelectionCommand::Toggle(target, selected),
            Some(expected_reference),
        );
    }
    pub fn preview(&self) {
        self.selection_command(SelectionCommand::Preview);
    }
    pub fn dismiss(&self) {
        self.selection_command(SelectionCommand::Dismiss);
    }
    pub fn apply(&self) {
        self.selection_command(SelectionCommand::Apply);
    }
    pub fn clear(&self) {
        self.selection_command(SelectionCommand::Clear);
    }
    pub fn page(&self, forward: bool) {
        self.control.lock().unwrap().page = Some(forward);
    }
    pub fn category(&self, folders: bool) {
        self.control.lock().unwrap().view = Some(if folders { View::Folders } else { View::Files });
    }
    pub fn open_folder(&self, id: u64) {
        self.control.lock().unwrap().view = Some(View::Folder(id));
    }
    pub fn refresh(&self, units: crate::units::Units, presentation: crate::presentation::Context) {
        let mut c = self.control.lock().unwrap();
        c.units = units;
        c.presentation = presentation;
        c.refresh = true;
    }
    pub fn cancel(&self) {
        let mut control = self.control.lock().unwrap();
        if self.current_folder && self.cancel.is_cancelled() {
            return;
        }
        self.cancel.cancel();
        control.selection_epoch = control.selection_epoch.wrapping_add(1);
        control.refresh = true;
    }
}
struct Context {
    profile: Option<engine::DuplicateFileProfile>,
    id: u64,
    a: Choice,
    b: Option<Choice>,
    c: Option<Choice>,
    lane: Arc<Mutex<service::Lane>>,
    control: Arc<Mutex<Control>>,
    output: Arc<Mutex<Option<Presentation>>>,
    cancel: CancellationToken,
    folder_options: Option<DuplicateOptions>,
    folder_temporary_parent: Option<PathBuf>,
}
fn side_label(side: u8) -> &'static str {
    match side {
        0 => "A",
        1 => "B",
        _ => "C",
    }
}
fn row_name(row: &Row) -> String {
    row.path
        .file_name()
        .map_or_else(|| label(&row.path), |name| label(Path::new(name)))
}
fn ordered_locations<'a>(
    a: (u8, &'a Row),
    b: (u8, &'a Row),
    c: Option<(u8, &'a Row)>,
    reference: Option<u8>,
) -> Vec<(u8, &'a Row)> {
    let mut locations: Vec<_> = [a, b].into_iter().chain(c).collect();
    locations.sort_by_key(|(side, _)| (Some(*side) != reference, *side));
    locations
}
impl Context {
    fn choices(&self) -> impl Iterator<Item = &Choice> {
        std::iter::once(&self.a)
            .chain(self.b.as_ref())
            .chain(self.c.as_ref())
    }
    fn choice(&self, side: u8) -> &Choice {
        match side {
            0 => &self.a,
            1 => self.b.as_ref().expect("selected side B"),
            _ => self.c.as_ref().expect("selected side C"),
        }
    }
    fn single(&self) -> bool {
        self.b.is_none()
    }
    fn publish(&self, mut p: Presentation) {
        let control = self.control.lock().unwrap();
        if self.folder_options.is_some() && self.cancel.is_cancelled() {
            if p.busy {
                return;
            }
            p.selection_epoch = control.selection_epoch;
            p.mark_folder_cancelled();
        } else if self.single() && !p.busy && self.cancel.is_cancelled() {
            let _scope = control.presentation.enter();
            p.selection_epoch = control.selection_epoch;
            p.complete = false;
            p.summary = tr!("Incomplete coverage").into();
            p.status = tr!("Cancelled").into();
            p.warning = tr!("Cancelled. Only finished groups are shown.").into();
        }
        if control.closed
            || p.selection_epoch != control.selection_epoch
            || (self.cancel.is_cancelled() && p.selection_ready)
        {
            return;
        }
        *self.output.lock().unwrap() = Some(p);
        drop(control);
        service::notify(&self.lane);
    }
    fn scope(&self) -> String {
        if self.folder_options.is_some() {
            return label(&self.a.native_root);
        }
        self.choices()
            .enumerate()
            .map(|(side, c)| {
                let inventories = c
                    .inventories
                    .iter()
                    .map(|i| {
                        format!(
                            "{} · G{} · {} · {}",
                            short_label(&i.saved.root),
                            i.saved.context.generation.0,
                            if i.saved.coverage == ScopeCoverage::Complete {
                                tr!("Complete")
                            } else {
                                tr!("Incomplete coverage")
                            },
                            i.saved.measured.map_or_else(
                                || tr!("Unknown").into(),
                                |t| cache::system_time(t)
                                    .map(|date| crate::date_label::format_date(
                                        sortsize_core::MetadataValue::Known {
                                            value: date,
                                            accuracy: sortsize_core::ValueAccuracy::Exact
                                        }
                                    ))
                                    .unwrap_or_else(|_| tr!("Unknown").into())
                            )
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if self.single() {
                    format!("{}\n{}", label(&c.native_root), inventories)
                } else {
                    format!(
                        "{}: {} · {}\n{}",
                        side_label(side as u8),
                        c.label,
                        short_label(&c.native_root),
                        inventories
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn scope_summary(&self) -> Vec<String> {
        self.choices()
            .map(|choice| {
                if self.single() {
                    label(&choice.native_root)
                } else {
                    short_label(&choice.native_root)
                }
            })
            .collect()
    }
    fn busy(&self, s: &Summary) -> Presentation {
        let (presentation, selection_epoch, reference) = {
            let control = self.control.lock().unwrap();
            (
                control.presentation,
                control.selection_epoch,
                control.reference,
            )
        };
        let _scope = presentation.enter();
        Presentation {
            id: self.id,
            presentation: crate::presentation::Context::current(),
            rows: vec![],
            model: vec![],
            duplicate_model: vec![],
            summary: with_profile_counts(summary_label(s, true), self.profile, s),
            warning: warning_label(s, true, self.folder_options.is_some()),
            details: format!("{}\n{}", self.scope(), budget_label(self.folder_options)),
            scope_summary: self.scope_summary(),
            status: tr_format!(
                "Exact groups: {} · bytes read: {} · hardlink aliases: {} · errors: {}.",
                s.groups,
                s.bytes_read,
                s.aliases,
                s.errors
            ),
            scope: self.scope(),
            statistics: if s.unverified_sources > 0 {
                format!(
                    "{}\n{} {}",
                    budget_label(self.folder_options),
                    tr_format!("Unverified sources: {}.", s.unverified_sources),
                    tr!(
                        "Source continuity is unverified. Current file contents can be compared, but inventory coverage and independent copies are not proven."
                    )
                )
            } else {
                budget_label(self.folder_options)
            },
            previous: false,
            next: false,
            complete: false,
            busy: true,
            results_available: self.folder_options.is_none(),
            content_retry: false,
            folder_cancelled: false,
            folder_terminal: None,
            groups: s.groups,
            folder_count: s.folder_groups,
            file_count: s.groups,
            folder_view: false,
            folder_detail: false,
            folder_ids: vec![],
            reference,
            selection_summary: String::new(),
            selection_notice: String::new(),
            selection_preview: String::new(),
            selection_preview_open: false,
            selection_ready: false,
            selection_targets: vec![],
            selection_epoch,
            _memory: None,
        }
    }

    fn folder_progress(
        &self,
        phase: FolderPhase,
        acquisition: &engine::AcquisitionProgress,
        summary: &Summary,
    ) {
        let mut p = self.busy(summary);
        let _scope = p.presentation.enter();
        let label = if phase == FolderPhase::Comparing
            && summary.verification_phase == Some(VerificationPhase::Comparing)
        {
            tr!("Comparing file contents…")
        } else {
            phase.label()
        };
        p.summary = if let Some(profile) = self.profile {
            let counts = crate::display_preferences::profile_counts(
                profile,
                acquisition.examined_files.into(),
                acquisition.excluded_by_size.into(),
                acquisition.excluded_empty.into(),
            );
            format!("{label} · {counts}")
        } else {
            label.into()
        };
        p.status = p.summary.clone();
        p.details.push('\n');
        p.details.push_str(&folder_observations(acquisition));
        if let Some(parent) = &self.folder_temporary_parent {
            p.details.push('\n');
            p.details.push_str(&resource_details(
                parent,
                folder_usage(acquisition, summary),
                self.control.lock().unwrap().units,
            ));
        }
        if phase != FolderPhase::Discovering {
            p.details.push('\n');
            let counts = content_counts(summary);
            p.summary.push_str(" · ");
            p.summary.push_str(&counts);
            p.status.clone_from(&p.summary);
            p.details.push_str(&counts);
        }
        self.publish(p);
    }
}

fn folder_observations(progress: &engine::AcquisitionProgress) -> String {
    format!(
        "{}\n{}",
        tr_format!(
            "Observed entries: {} · selected files: {} · discovery errors: {}.",
            integer(progress.observations.into()),
            integer(progress.selected_files.into()),
            integer(progress.errors.into())
        ),
        tr_format!(
            "Logical size of selected files with known sizes: {} bytes.",
            integer(progress.selected_logical_bytes)
        )
    )
}

fn with_profile_counts(
    text: String,
    profile: Option<engine::DuplicateFileProfile>,
    summary: &Summary,
) -> String {
    let Some(profile) = profile else { return text };
    let counts = crate::display_preferences::profile_counts(
        profile,
        summary.examined_files.into(),
        summary.excluded_by_size.into(),
        summary.excluded_empty.into(),
    );
    format!("{text} · {counts}")
}

fn content_counts(summary: &Summary) -> String {
    tr_format!(
        "Content bytes read (hashing and comparison): {}.",
        integer(summary.bytes_read)
    )
}

fn unverified_count(summary: &Summary) -> Option<String> {
    summary.unverified_files.map(|files| {
        tr_format!(
            "Candidate files not fully verified: {}.",
            integer(files.into())
        )
    })
}

fn folder_usage<'a>(
    acquisition: &'a engine::AcquisitionProgress,
    summary: &'a Summary,
) -> Option<&'a CurrentFolderResourceUsage> {
    summary
        .resources
        .as_ref()
        .or(acquisition.resources.as_ref())
}

fn resource_warning(cause: DuplicateResourceCause) -> &'static str {
    match cause {
        DuplicateResourceCause::Memory => tr!("Search RAM budget unavailable."),
        DuplicateResourceCause::TemporaryLimit => tr!("Temporary quota reached."),
        DuplicateResourceCause::TemporarySpace => {
            tr!("Not enough free space on the temporary volume.")
        }
        DuplicateResourceCause::TemporaryAccess => {
            tr!("Could not access the local temporary folder.")
        }
        DuplicateResourceCause::TemporaryPathEncoding => {
            tr!("This temporary folder path is not supported. Choose another local folder.")
        }
    }
}

fn resource_details(
    parent: &Path,
    usage: Option<&CurrentFolderResourceUsage>,
    units: crate::units::Units,
) -> String {
    let location = tr_format!("Temporary folder: {}.", label(parent));
    let Some(usage) = usage else {
        return location;
    };
    let quantity = |bytes| {
        let human = units.with_size_unit(0).format_file(bytes);
        if bytes < if units.is_decimal() { 1000 } else { 1024 } {
            human
        } else {
            format!(
                "{human} ({})",
                tr_format!("{} bytes", integer(bytes.into()))
            )
        }
    };
    let known = |bytes: Option<u64>| bytes.map_or_else(|| tr!("Unknown").into(), quantity);
    let requested = |bytes: Option<u64>| bytes.map_or_else(|| tr!("Automatic").into(), quantity);
    [
        location,
        tr_format!(
            "Temporary storage charged: {} · peak: {} · effective quota: {} · requested quota: {}.",
            quantity(usage.temporary_charged),
            quantity(usage.temporary_peak),
            known(
                (usage.temporary_limit > 0 || usage.temporary_free.is_some())
                    .then_some(usage.temporary_limit)
            ),
            requested(usage.temporary_requested)
        ),
        tr_format!(
            "Free on temporary volume: {} · space kept free: {}.",
            known(usage.temporary_free),
            known(usage.temporary_free.map(|_| usage.temporary_reserve))
        ),
        tr_format!(
            "Search RAM limit: {} · reserved peak: {} · shared RAM available at admission: {}.",
            requested(usage.memory_limit),
            quantity(usage.memory_reserved),
            known(
                (usage.memory_reserved > 0 || usage.cause == Some(DuplicateResourceCause::Memory))
                    .then_some(usage.memory_available)
            )
        ),
        tr_format!(
            "SQLite cache: {} · discovery workers: {} · result pages reserved: {}.",
            known((usage.sqlite_cache > 0).then_some(usage.sqlite_cache)),
            if usage.discovery_workers > 0 {
                integer(usage.discovery_workers as u128)
            } else {
                tr!("Unknown").into()
            },
            known((usage.publication_reserved > 0).then_some(usage.publication_reserved))
        ),
    ]
    .join("\n")
}

fn folder_error(error: &Error) -> (String, String) {
    match error {
        Error::DuplicateResources(usage)
            if usage.cause == Some(DuplicateResourceCause::Memory)
                && usage
                    .memory_limit
                    .is_some_and(|limit| limit < MIN_CURRENT_FOLDER_RAM) =>
        {
            (
                tr!("Search RAM budget unavailable. Set at least 8 MiB or use Automatic.").into(),
                String::new(),
            )
        }
        Error::DuplicateResources(usage) => (
            usage
                .cause
                .map_or_else(|| tr!("Could not search this folder."), resource_warning)
                .into(),
            String::new(),
        ),
        Error::DuplicateTemporaryLimit { limit } => (
            tr_format!(
                "Insufficient temporary storage. Search quota: {} bytes.",
                integer((*limit).into())
            ),
            String::new(),
        ),
        Error::DiskAdmission(disk) => (
            tr_format!(
                "Temporary storage quota unavailable: {} bytes requested, {} bytes quota.",
                integer(disk.requested.into()),
                integer(disk.limit.into())
            ),
            error.to_string(),
        ),
        Error::MemoryAdmission {
            minimum,
            available,
            category_limit,
        } => (
            tr_format!(
                "Memory budget unavailable: {} bytes required, {} bytes available, {} bytes category limit.",
                integer((*minimum).into()),
                integer((*available).into()),
                integer((*category_limit).into())
            ),
            String::new(),
        ),
        Error::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => (
            tr!("Could not access the scanned folder.").into(),
            error.to_string(),
        ),
        Error::Contract("duplicate folder preflight failed") => (
            tr!("Could not confirm access to the scanned folder.").into(),
            String::new(),
        ),
        _ => (
            tr!("Could not search this folder.").into(),
            error.to_string(),
        ),
    }
}
fn summary_label(summary: &Summary, busy: bool) -> String {
    format!(
        "{} · {}",
        if busy {
            tr!("Verifying files…")
        } else if summary.complete {
            tr!("Complete")
        } else {
            tr!("Incomplete coverage")
        },
        tr_format!(
            "Exact groups: {} · errors: {}.",
            summary.groups,
            summary.errors
        )
    )
}
/// Limits are independent of the technical detail disclosure and row labels.
fn warning_label(summary: &Summary, busy: bool, current_folder: bool) -> String {
    let mut warnings = Vec::new();
    if !busy && !summary.complete && !current_folder {
        warnings.push(tr!("Incomplete coverage").to_owned());
    }
    if summary.cancelled {
        warnings.push(tr!("Cancelled. Only finished groups are shown.").to_owned());
    }
    if current_folder {
        if let Some(limit) = summary.total_read_limit {
            warnings.push(tr_format!(
                "Total content read limit reached: {} bytes.",
                integer(limit.into())
            ));
        }
        if let Some(limit) = summary.file_size_limit {
            warnings.push(tr_format!(
                "Files exceeded the size limit of {} bytes.",
                integer(limit.into())
            ));
        }
        if let Some(cause) = summary.resources.and_then(|usage| usage.cause) {
            warnings.push(resource_warning(cause).into());
        } else if let Some(limit) = summary.temporary_limit {
            warnings.push(tr_format!(
                "Insufficient temporary storage. Search quota: {} bytes.",
                integer(limit.into())
            ));
        }
        if let Some(limit) = summary.error_limit {
            warnings.push(tr_format!(
                "Error limit reached: {}.",
                integer(limit as u128)
            ));
        } else if summary.errors > 0 {
            warnings.push(tr_format!(
                "Errors during search: {}.",
                integer(summary.errors.into())
            ));
        }
        if !busy
            && summary.unverified_files.is_some_and(|count| count > 0)
            && let Some(count) = unverified_count(summary)
        {
            warnings.push(count);
        }
    } else if summary.budget_limited {
        warnings.push(
            tr!("Read budget or file-size limit reached. Results are incomplete.").to_owned(),
        );
    }
    if summary.ambiguous_aliases > 0 {
        warnings.push(
            tr!("Possible source aliases remain unverified. Results are incomplete.").to_owned(),
        );
    }
    if summary.unverified_sources > 0 {
        warnings.push(tr_format!(
            "Unverified sources: {}.",
            summary.unverified_sources
        ));
    }
    if summary.directory_listing_sources > 0 {
        warnings.push(tr!("Archived directory sizes may be outdated. Some duplicates may be missed. Verify the saved inventories before searching again.").to_owned());
    }
    if summary.folder_unverified > 0
        || summary.folder_filtered > 0
        || summary.folder_diagnostic.is_some()
    {
        warnings.push(tr!("Some folders lack complete, fresh verification. Only verified file duplicates are retained for them.").to_owned());
    }
    warnings.join(" · ")
}
#[derive(Clone, Copy)]
enum SelectionNotice {
    ReferenceChanged,
    Cleared,
    Applied,
    Invalidated,
    Excluded(engine::ReviewReason),
}
impl SelectionNotice {
    fn label(self) -> &'static str {
        match self {
            Self::ReferenceChanged => {
                tr!("Selection cleared after the protected reference changed.")
            }
            Self::Cleared => tr!("Selection cleared."),
            Self::Applied => tr!("Copies selected for review. No files were changed."),
            Self::Invalidated => tr!(
                "Comparison was cancelled or a source was disconnected. Compare explicitly again before selecting copies."
            ),
            Self::Excluded(reason) => selection_reason(reason),
        }
    }
}
fn selection_reason(reason: engine::ReviewReason) -> &'static str {
    match reason {
        engine::ReviewReason::ChooseReference => {
            tr!("Choose a protected reference before selecting copies.")
        }
        engine::ReviewReason::ProtectedReference => {
            tr!("Protected reference: this location cannot be selected.")
        }
        engine::ReviewReason::IncompleteEvidence => tr!(
            "Incomplete or insufficient verification. Compare explicitly again before selecting copies."
        ),
        engine::ReviewReason::Alias => {
            tr!("Hardlink or source alias: this is not an independent copy.")
        }
        engine::ReviewReason::Unverified => {
            tr!("This result lacks sufficient verified evidence for selection.")
        }
        engine::ReviewReason::MissingReference => tr!(
            "No verified matching copy in the protected reference. This group cannot be selected with assistance."
        ),
        engine::ReviewReason::Eligible => "",
    }
}
impl Context {
    fn duplicate_row(
        &self,
        row: &Row,
        group: u64,
        alias: bool,
        side: u8,
        units: crate::units::Units,
        reference: Option<u8>,
    ) -> crate::DuplicateRow {
        let protected = reference == Some(side);
        let copy = reference.is_some() && !protected && !alias;
        let directory = row.kind == sortsize_store::inventory::Kind::Directory;
        let location = if directory {
            Some(row.path.as_path())
        } else {
            row.path.parent()
        };
        crate::DuplicateRow {
            group: crate::number_format::integer(group.into()).into(),
            side: self.display_side(side).into(),
            scope_label: if self.single() {
                "".into()
            } else {
                self.scope_label(side).into()
            },
            protected,
            copy,
            role_label: if protected {
                tr!("Protected reference")
            } else if copy {
                tr!("Copy")
            } else {
                ""
            }
            .into(),
            name: row_name(row).into(),
            location: location.map_or_else(String::new, label).into(),
            size: if directory {
                tr!("Folder").into()
            } else {
                row.logical
                    .map_or_else(|| tr!("Unknown").into(), |n| units.format_file(n))
            }
            .into(),
            selectable: false,
            selected: false,
            partial: false,
            selection_label: "".into(),
            verification: if alias {
                format!("{} · {}", tr!("Verified exact"), tr!("Hardlink alias"))
            } else {
                tr!("Verified exact").into()
            }
            .into(),
            ..Default::default()
        }
    }
    fn display_side(&self, side: u8) -> &'static str {
        if self.single() { "" } else { side_label(side) }
    }
    fn scope_label(&self, side: u8) -> String {
        let choice = self.choice(side);
        choice
            .native_root
            .file_name()
            .map_or_else(|| choice.label.clone(), |name| label(Path::new(name)))
    }
    fn reference_note(&self, side: u8, present: bool) -> String {
        if present {
            tr_format!(
                "Protected reference: {} · {}",
                side_label(side),
                self.scope_label(side)
            )
        } else {
            tr_format!(
                "No matching location in reference folder {}",
                side_label(side)
            )
        }
    }
}
fn budget_label(folder_options: Option<DuplicateOptions>) -> String {
    let budgets = if let Some(options) = folder_options {
        let limit = |bytes: Option<u64>| {
            bytes.map_or_else(
                || tr!("No limit").into(),
                |bytes| tr_format!("{} bytes", integer(bytes.into())),
            )
        };
        tr_format!(
            "Content read limit: {} · file size limit: {} · error limit: {}.",
            limit(options.max_bytes_read),
            limit(options.max_file_bytes),
            integer(options.max_errors as u128)
        )
    } else {
        let options = DuplicateOptions::default();
        tr_format!(
            "Effective duplicate budgets: {} bytes read; {} bytes per file; {} errors; {} bytes temporary state total; {} roots maximum.",
            options.max_bytes_read.unwrap_or(u64::MAX),
            options.max_file_bytes.unwrap_or(u64::MAX),
            options.max_errors,
            2 * CANDIDATE_DISK_BYTES,
            128
        )
    };
    format!(
        "{} {}",
        budgets,
        tr_format!(
            "Maximum pending duplicate tasks, including cancelled tasks awaiting the system: {}.",
            MAX_OPERATIONS
        )
    )
}

fn trace_candidate_preparation(job: &engine::DuplicateJob<'_, '_>, route: &str, start: Instant) {
    if scan_p1::enabled() {
        let wall_ns = start.elapsed().as_nanos();
        let temporary = job.temporary_usage();
        let candidate_count = job
            .candidate_count()
            .map_or_else(|_| "unknown".to_owned(), |count| count.to_string());
        eprintln!(
            "duplicate_candidates route={route} wall_ns={wall_ns} candidate_count={candidate_count} temp_charged={} temp_peak={}",
            temporary.charged, temporary.peak,
        );
    }
}

struct VerificationTrace {
    started: Instant,
    enabled: bool,
    first_hash_ns: Option<u128>,
}

impl VerificationTrace {
    fn new(started: Instant, enabled: bool) -> Self {
        Self {
            started,
            enabled: enabled && scan_p1::enabled(),
            first_hash_ns: None,
        }
    }

    fn observe(&mut self, summary: &Summary) {
        if self.enabled && self.first_hash_ns.is_none() && summary.files_hashed > 0 {
            self.first_hash_ns = Some(self.started.elapsed().as_nanos());
        }
    }

    fn finish(&self, job: &engine::DuplicateJob<'_, '_>, route: &str) {
        if self.enabled {
            let summary = job.summary();
            let temporary = job.temporary_usage();
            let first_hash_ns = self
                .first_hash_ns
                .map_or_else(|| "none".to_owned(), |elapsed| elapsed.to_string());
            eprintln!(
                "duplicate_verify route={route} wall_ns={} first_hash_ns={first_hash_ns} files_hashed={} bytes_read={} groups={} complete={} temp_charged={} temp_peak={}",
                self.started.elapsed().as_nanos(),
                summary.files_hashed,
                summary.bytes_read,
                summary.groups,
                summary.complete,
                temporary.charged,
                temporary.peak,
            );
        }
    }
}

fn run_current_folder(
    context: &Context,
    request: CurrentFolderRequest,
    telemetry: &mut FolderTelemetry,
) -> sortsize_store::inventory::Result<()> {
    use sortsize_store::inventory::Error;

    let phase =
        |phase: FolderPhase, acquisition: &engine::AcquisitionProgress, summary: &Summary| {
            context.folder_progress(phase, acquisition, summary);
            #[cfg(test)]
            if let Some(observe) = &request.observe {
                observe(phase);
            }
            if phase != FolderPhase::Coverage && context.cancel.is_cancelled() {
                return Err(Error::Contract("duplicate comparison cancelled"));
            }
            Ok(())
        };
    let empty = Summary::default();
    phase(FolderPhase::Discovering, &telemetry.acquisition, &empty)?;
    if request.preflight.is_some_and(|check| !check()) {
        return Err(Error::Contract("duplicate folder preflight failed"));
    }
    if context.cancel.is_cancelled() {
        return Err(Error::Contract("duplicate comparison cancelled"));
    }
    let location = service::work_location()?;
    let guard = Guard::open(
        &request.root,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
    )?;
    let guard = Arc::new(location.scope(guard)?);
    let start_preparation = Instant::now();
    let mut last = start_preparation;
    let mut job = engine::prepare_current_folder_with_profile(
        guard,
        &request.resources,
        request.profile,
        &context.cancel,
        |progress| {
            telemetry.acquisition = *progress;
            if last.elapsed() >= Duration::from_millis(200) {
                context.folder_progress(FolderPhase::Discovering, progress, &empty);
                last = Instant::now();
            }
        },
    )?;
    trace_candidate_preparation(&job, "current-folder", start_preparation);
    telemetry.summary.clone_from(job.summary());
    phase(
        FolderPhase::Comparing,
        &telemetry.acquisition,
        job.summary(),
    )?;
    last = Instant::now();
    let mut trace = VerificationTrace::new(last, true);
    let result =
        job.verify_current_folder_with_progress(request.options, &context.cancel, |summary| {
            trace.observe(summary);
            if summary.verification_phase.is_some() && last.elapsed() >= Duration::from_millis(200)
            {
                context.folder_progress(FolderPhase::Comparing, &telemetry.acquisition, summary);
                last = Instant::now();
            }
        });
    telemetry.summary.clone_from(job.summary());
    result?;
    trace.finish(&job, "current-folder");
    phase(FolderPhase::Coverage, &telemetry.acquisition, job.summary())?;
    show_results(
        context,
        &mut job,
        &[(0, request.root.as_path())],
        &[],
        Some(&telemetry.acquisition),
    )
}

fn run(context: &Context) -> sortsize_store::inventory::Result<()> {
    use sortsize_store::inventory::Error;
    let choices = context.choices().collect::<Vec<_>>();
    if choices.iter().any(|choice| choice.inventories.is_empty()) {
        return Err(Error::Contract("inventories required for selected folders"));
    }
    if choices.iter().enumerate().any(|(i, a)| {
        choices[..i].iter().any(|b| {
            sortsize_core::acquisition::contains_path(&a.native_root, &b.native_root)
                || sortsize_core::acquisition::contains_path(&b.native_root, &a.native_root)
        })
    }) {
        return Err(Error::Contract(
            "selected folders must be distinct and not nested",
        ));
    }
    let inventories_with_sides = choices
        .iter()
        .enumerate()
        .flat_map(|(side, choice)| {
            choice
                .inventories
                .iter()
                .map(move |inventory| (side as u8, inventory))
        })
        .collect::<Vec<_>>();
    let inventories = inventories_with_sides
        .iter()
        .map(|(_, inventory)| *inventory)
        .collect::<Vec<_>>();
    let sides = inventories_with_sides
        .iter()
        .map(|(side, _)| *side)
        .collect::<Vec<_>>();
    if inventories.len() > 128 {
        return Err(Error::Contract("duplicate inventory count budget"));
    }
    context.publish(context.busy(&Summary::default()));
    // Open only local canonical snapshots. This phase never opens a NAS file.
    let mut active = Vec::new();
    for (index, inventory) in inventories.iter().enumerate() {
        if context.cancel.is_cancelled() {
            return Err(Error::Contract("duplicate comparison cancelled"));
        }
        active.push(
            service::Active::open_saved(
                context.id,
                &inventory.saved,
                inventory.workspace.clone(),
                &context.cancel,
            )
            .map_err(|error| {
                std::io::Error::other(format!(
                    "{} · {}: {}",
                    context.display_side(sides[index]),
                    short_label(&inventory.saved.root),
                    error
                ))
            })?,
        );
    }
    let mut guards = Vec::new();
    for (i, _inventory) in inventories.iter().enumerate() {
        if context.cancel.is_cancelled() {
            return Err(Error::Contract("duplicate comparison cancelled"));
        }
        let root = &context.choice(sides[i]).native_root;
        guards.push(
            Guard::open(
                root,
                Policy {
                    explicit_root: true,
                    ..Default::default()
                },
            )
            .ok(),
        );
    }
    let mut source_failures = Vec::new();
    for (index, (source, inventory)) in active.iter().zip(&inventories).enumerate() {
        if guards[index].is_none() {
            continue;
        }
        // A reused parent inventory is metadata coverage, not permission to
        // reopen that parent. The verifier proves its recorded subroot against
        // the selected guard (identity, source boundary and mutation checks).
        if inventory.saved.root != context.choice(sides[index]).native_root {
            continue;
        }
        match Guard::open(
            &inventory.saved.root,
            Policy {
                explicit_root: true,
                ..Default::default()
            },
        ) {
            Ok(guard) => {
                if sortsize_store::scheduler::Session::source_attachment(
                    source.session.store(),
                    source.c.generation,
                    &guard,
                )? == sortsize_core::acquisition::Attachment::Different
                {
                    guards[index] = None;
                    source_failures.push((index, tr!("This source differs from its recorded inventory. Analyze it separately before comparing.").to_owned()));
                }
            }
            Err(error) => {
                guards[index] = None;
                source_failures.push((index, error.to_string()));
            }
        }
    }
    let location = super::service::work_location()?;
    let scratch = location.temporary("bunny-duplicates-ab-")?;
    for guard in guards.iter().flatten() {
        let scoped = location.scope(Guard::open(guard.root(), guard.policy())?)?;
        if !scoped.work_path_allowed(scratch.path())? {
            return Err(Error::Contract(
                "duplicate scratch overlaps a selected source",
            ));
        }
    }
    // One unchanged 512 MiB job ceiling shared between queries and result DB.
    let per_query_budget = CANDIDATE_DISK_BYTES / inventories.len() as u64;
    let mut queries = Vec::new();
    for source in &active {
        let query = SearchQuery::default();
        let mut q = if let Some(g) = source.certified_generation() {
            g.start_query(&query, scratch.path(), per_query_budget)?
        } else {
            source.session.store().start_query(
                source.c.generation,
                &source.root,
                &query,
                scratch.path(),
                per_query_budget,
            )?
        };
        while !q.evaluation_finished() {
            q.advance(&context.cancel)?;
        }
        queries.push(q);
    }
    let scopes = queries
        .iter()
        .zip(&inventories)
        .enumerate()
        .map(|(i, (query, _inventory))| Scope {
            query,
            roots: vec![context.choice(sides[i]).native_root.clone()],
            side: sides[i],
        })
        .collect();
    let start_preparation = Instant::now();
    let mut job = if let Some(profile) = context.profile {
        engine::prepare_scopes_with_profile(
            scopes,
            Mode::WithinSelection,
            scratch.path(),
            CANDIDATE_DISK_BYTES,
            profile,
            &context.cancel,
        )
    } else {
        engine::prepare_scopes(
            scopes,
            if context.c.is_some() {
                Mode::BetweenThreeSources
            } else {
                Mode::BetweenSources
            },
            scratch.path(),
            CANDIDATE_DISK_BYTES,
            &context.cancel,
        )
    }?;
    if context.single() {
        trace_candidate_preparation(&job, "strict-inventory", start_preparation);
    }
    if context.cancel.is_cancelled() {
        return Err(Error::Contract("duplicate comparison cancelled"));
    }
    let mut last = Instant::now();
    let mut trace = VerificationTrace::new(last, context.single());
    job.verify_available_scoped_with_progress(
        &guards,
        DuplicateOptions::default(),
        &context.cancel,
        |summary| {
            trace.observe(summary);
            if last.elapsed() >= Duration::from_millis(100) {
                context.publish(context.busy(summary));
                last = Instant::now();
            }
        },
    )?;
    trace.finish(&job, "strict-inventory");
    let scopes = sides
        .iter()
        .copied()
        .zip(inventories.iter().map(|i| i.saved.root.as_path()))
        .collect::<Vec<_>>();
    show_results(context, &mut job, &scopes, &source_failures, None)
}

fn show_results(
    context: &Context,
    job: &mut engine::DuplicateJob<'_, '_>,
    scopes: &[(u8, &Path)],
    source_failures: &[(usize, String)],
    acquisition: Option<&engine::AcquisitionProgress>,
) -> sortsize_store::inventory::Result<()> {
    use sortsize_store::inventory::Error;

    let scope_label = |index: usize| {
        let (side, root) = scopes[index];
        if acquisition.is_some() {
            label(root)
        } else {
            format!("{} · {}", side_label(side), short_label(root))
        }
    };
    let mut page = job.review_file_page(None, false)?;
    let mut file_has_next = page.more;
    let file_count = job.file_group_count()?;
    let mut folders = job.folder_groups(None)?;
    let mut folder_detail = None;
    let mut detail_has_next = false;
    let mut view = if folders.is_empty() {
        View::Files
    } else {
        View::Folders
    };
    let mut force = true;
    let mut selection_notice = None;
    let mut selection_preview_open = false;
    let mut selection_invalidated = false;
    #[cfg(all(windows, test))]
    let mut dump = context
        .single()
        .then(|| std::env::var_os("BUNNY_DUPLICATE_MEASURE_OUTPUT"))
        .flatten();
    loop {
        let (
            direction,
            requested_view,
            refresh,
            closed,
            units,
            presentation,
            selection_commands,
            selection_epoch,
        ) = {
            let mut c = context.control.lock().unwrap();
            (
                c.page.take(),
                c.view.take(),
                std::mem::take(&mut c.refresh),
                c.closed,
                c.units,
                c.presentation,
                std::mem::take(&mut c.selection_commands),
                c.selection_epoch,
            )
        };
        if closed {
            return Ok(());
        }
        let _scope = presentation.enter();
        let mut reference_changed = false;
        force |= refresh || !selection_commands.is_empty();
        for command in selection_commands {
            if context.cancel.is_cancelled() {
                continue;
            }
            match command {
                SelectionCommand::Reference(side) => {
                    job.set_protected_reference(side)?;
                    reference_changed = true;
                    selection_preview_open = false;
                    selection_notice = Some(SelectionNotice::ReferenceChanged);
                }
                SelectionCommand::Toggle(target, selected) => {
                    let state = job.review_state(target)?;
                    if state.eligible {
                        job.set_review_target(target, selected)?;
                        selection_notice = None;
                    } else {
                        // An old visible page can send a toggle after a
                        // reference change. Refuse it without closing the job.
                        selection_notice = Some(SelectionNotice::Excluded(state.reason));
                    }
                    selection_preview_open = false;
                }
                SelectionCommand::Preview => {
                    selection_preview_open = true;
                    selection_notice = None;
                }
                SelectionCommand::Dismiss => {
                    selection_preview_open = false;
                }
                SelectionCommand::Apply if selection_preview_open => {
                    let reason = job.review_summary()?.reason;
                    if reason == engine::ReviewReason::Eligible {
                        job.select_all_review_copies()?;
                        selection_notice = Some(SelectionNotice::Applied);
                    } else {
                        selection_notice = Some(SelectionNotice::Excluded(reason));
                    }
                    selection_preview_open = false;
                }
                SelectionCommand::Apply => {}
                SelectionCommand::Clear => {
                    job.clear_review_selection()?;
                    selection_preview_open = false;
                    selection_notice = Some(SelectionNotice::Cleared);
                }
            }
        }
        if context.cancel.is_cancelled() && !selection_invalidated {
            job.set_protected_reference(None)?;
            reference_changed = true;
            selection_preview_open = false;
            selection_notice = Some(SelectionNotice::Invalidated);
            selection_invalidated = true;
            force = true;
        }
        if reference_changed {
            page = job.review_file_page(None, false)?;
            file_has_next = page.more;
        }
        if let Some(requested) = requested_view.filter(|_| !context.single()) {
            match requested {
                View::Folders => {
                    folders = job.folder_groups(None)?;
                    view = View::Folders;
                }
                View::Files => {
                    page = job.review_file_page(None, false)?;
                    file_has_next = page.more;
                    view = View::Files;
                }
                View::Folder(id)
                    if folders.iter().any(|group| group.id == id)
                        || page.folder_references.contains(&Some(id)) =>
                {
                    let details = job.folder_page(id, None, false)?;
                    detail_has_next = details.more;
                    folder_detail = Some(details);
                    view = View::Folder(id);
                }
                View::Folder(_) => {}
            }
            force = true;
        }
        if let Some(forward) = direction.filter(|_| !reference_changed) {
            match view {
                View::Files => {
                    if forward && file_has_next {
                        page = job.review_file_page(page.last.as_ref(), false)?;
                        file_has_next = page.more;
                    } else if !forward {
                        let previous = job.review_file_page(page.first.as_ref(), true)?;
                        if !previous.rows.is_empty() {
                            file_has_next = true;
                            page = previous;
                        }
                    }
                }
                View::Folders => {
                    let next = if forward {
                        job.folder_groups(folders.last().map(|group| group.id))?
                    } else {
                        job.folder_groups_before(folders.first().map(|group| group.id))?
                    };
                    if !next.is_empty() {
                        folders = next;
                    }
                }
                View::Folder(id) => {
                    if let Some(current) = folder_detail.as_ref() {
                        let next = job.folder_page(
                            id,
                            if forward {
                                current.last.as_ref()
                            } else {
                                current.first.as_ref()
                            },
                            !forward,
                        )?;
                        if !next.rows.is_empty() {
                            detail_has_next = !forward || next.more;
                            folder_detail = Some(next);
                        }
                    }
                }
            }
            force = true;
        }
        if force {
            let s = job.summary();
            let review = job.review_summary()?;
            let mut detail_roots = Vec::new();
            if context.c.is_some()
                && let View::Folder(id) = view
            {
                let previous = folder_detail
                    .as_ref()
                    .and_then(|details| details.first.as_ref())
                    .map(|cursor| {
                        job.folder_page(id, Some(cursor), true)
                            .map(|page| !page.rows.is_empty())
                    })
                    .transpose()?
                    .unwrap_or(false);
                if !previous && let Some(group) = job.folder_group(id)? {
                    detail_roots.extend(
                        std::iter::once((group.a_side, group.a))
                            .chain(std::iter::once((group.b_side, group.b)))
                            .chain(group.c_side.zip(group.c)),
                    );
                    detail_roots.sort_by_key(|(side, _)| (Some(*side) != review.reference, *side));
                }
            }
            let folder_locations: Vec<_> = folders
                .iter()
                .map(|group| {
                    ordered_locations(
                        (group.a_side, &group.a),
                        (group.b_side, &group.b),
                        group.c_side.zip(group.c.as_ref()),
                        review.reference,
                    )
                })
                .collect();
            let detail_locations: Vec<_> = folder_detail
                .as_ref()
                .into_iter()
                .flat_map(|details| &details.rows)
                .flat_map(|pair| {
                    ordered_locations(
                        (pair.a_side, &pair.a),
                        (pair.b_side, &pair.b),
                        pair.c_side.zip(pair.c.as_ref()),
                        review.reference,
                    )
                })
                .collect();
            let row_bytes = |row: &Row| {
                row.path.as_os_str().as_encoded_bytes().len() * 4
                    + std::mem::size_of::<Row>()
                    + 4096
            };
            let publication_bytes = 128 * 1024
                + folders
                    .iter()
                    .map(|group| {
                        group.b.path.as_os_str().as_encoded_bytes().len() * 4
                            + group.c.as_ref().map_or(0, |row| {
                                row.path.as_os_str().as_encoded_bytes().len() * 4 + 4096
                            })
                            + 4096
                    })
                    .sum::<usize>()
                + match view {
                    View::Files => page.rows.iter().map(row_bytes).sum::<usize>(),
                    View::Folders => folder_locations
                        .iter()
                        .map(|rows| row_bytes(rows[0].1))
                        .sum(),
                    View::Folder(_) => detail_roots
                        .iter()
                        .map(|(_, row)| row_bytes(row))
                        .chain(detail_locations.iter().map(|(_, row)| row_bytes(row)))
                        .sum(),
                };
            let memory = if acquisition.is_some() {
                // Each worker, mailbox and visible page uses at most one share
                // of the lease acquired before the search. Keeping a clone on
                // the published page extends the lease across worker shutdown.
                debug_assert!(publication_bytes <= engine::CURRENT_FOLDER_PUBLICATION_PAGE_BYTES);
                job.current_folder_publication_reservation()
                    .ok_or(Error::Contract("duplicate publication lease missing"))?
            } else {
                Arc::new(
                    Governor::global()
                        .reserve(Category::Publications, publication_bytes as u64, None)
                        .ok_or(Error::Contract("duplicate publication memory budget"))?,
                )
            };
            let visible_rows: Vec<Row> = match view {
                View::Files => page.rows.clone(),
                View::Folders => folder_locations
                    .iter()
                    .map(|rows| rows[0].1.clone())
                    .collect(),
                View::Folder(_) => detail_roots
                    .iter()
                    .map(|(_, row)| row.clone())
                    .chain(detail_locations.iter().map(|(_, row)| (*row).clone()))
                    .collect(),
            };
            let mut p = context.busy(s);
            p.busy = false;
            p.results_available = true;
            p.complete = s.complete && s.folder_unverified == 0 && s.folder_filtered == 0;
            let summary = if acquisition.is_some() {
                format!(
                    "{} · {}",
                    if p.complete {
                        tr!("Complete coverage")
                    } else {
                        tr!("Incomplete coverage")
                    },
                    tr_format!("Exact file groups: {}.", file_count)
                )
            } else if context.single() {
                summary_label(s, false)
            } else {
                format!(
                    "{} · {}",
                    if p.complete {
                        tr!("Complete")
                    } else {
                        tr!("Incomplete coverage")
                    },
                    tr_format!(
                        "{} folder groups · {} file groups.",
                        s.folder_groups,
                        file_count
                    )
                )
            };
            p.groups = s.folder_groups.saturating_add(file_count);
            p.warning = warning_label(s, false, acquisition.is_some());
            p.summary = with_profile_counts(summary, context.profile, s);
            p.content_retry = acquisition.is_some()
                && (s.total_read_limit.is_some()
                    || s.file_size_limit.is_some()
                    || s.temporary_limit.is_some()
                    || s.error_limit.is_some()
                    || s.resources.and_then(|usage| usage.cause).is_some());
            p.status = format!(
                "{} · {}",
                if p.complete {
                    tr!("Complete")
                } else {
                    tr!("Incomplete coverage")
                },
                tr_format!(
                    "Exact groups: {} · bytes read: {} · hardlink aliases: {} · errors: {}.",
                    p.groups,
                    s.bytes_read,
                    s.aliases,
                    s.errors
                )
            );
            p.statistics = format!(
                "{}\n{}",
                tr!(
                    "SHA-256 candidates are compared byte for byte. Hardlink aliases are the same file."
                ),
                budget_label(context.folder_options)
            );
            if let Some(acquisition) = acquisition {
                p.status = format!("{} · {}", p.summary, content_counts(s));
                p.statistics
                    .push_str(&format!("\n{}", folder_observations(acquisition)));
                if let Some(parent) = &context.folder_temporary_parent {
                    p.statistics.push('\n');
                    p.statistics.push_str(&resource_details(
                        parent,
                        folder_usage(acquisition, s),
                        units,
                    ));
                }
                if let Some(count) = unverified_count(s) {
                    p.statistics.push('\n');
                    p.statistics.push_str(&count);
                }
                if s.unknown_sizes > 0 {
                    p.statistics.push_str(&format!(
                        "\n{}: {}",
                        tr!("Unknown sizes"),
                        s.unknown_sizes
                    ));
                }
                p.statistics.push('\n');
                p.statistics.push_str(tr!(
                    "Coverage describes the folders examined and the checks made during this search. Files can change during the search."
                ));
            }
            if acquisition.is_none() && s.cancelled {
                p.statistics
                    .push_str(tr!("Cancelled. Only finished groups are shown."));
            }
            if acquisition.is_none() && s.budget_limited {
                p.statistics.push_str(tr!(
                    "Read budget or file-size limit reached. Results are incomplete."
                ));
            }
            if s.ambiguous_aliases > 0 {
                p.statistics.push_str(tr!(
                    "Possible source aliases remain unverified. Results are incomplete."
                ));
            }
            if s.unverified_sources > 0 {
                p.statistics.push_str(&format!("\n{} {}",tr_format!("Unverified sources: {}.",s.unverified_sources),tr!("Source continuity is unverified. Current file contents can be compared, but inventory coverage and independent copies are not proven.")));
                for (index, _) in job.warnings() {
                    p.statistics.push_str(&format!(
                        "\n{} · {}",
                        scope_label(*index),
                        tr_format!("Unverified sources: {}.", 1)
                    ));
                }
            }
            let errors = job.scope_errors()?;
            for (i, count) in errors.iter().enumerate().filter(|(_, n)| **n > 0) {
                p.statistics
                    .push_str(&format!("\n{}: {}", scope_label(i), count));
            }
            for (index, reason) in job.diagnostics()? {
                p.statistics
                    .push_str(&format!("\n{}: {}", scope_label(index), reason));
            }
            for (index, diagnostic) in source_failures {
                p.statistics
                    .push_str(&format!("\n{}: {}", scope_label(*index), diagnostic));
            }
            if let Some(diagnostic) = &s.folder_diagnostic {
                p.statistics.push_str(&format!("\n{diagnostic}"));
            }
            p.details = format!("{}\n{}\n{}", p.scope, p.status, p.statistics);
            p.rows = visible_rows;
            p.folder_count = s.folder_groups;
            p.file_count = file_count;
            p.folder_view = !matches!(view, View::Files);
            p.folder_detail = matches!(view, View::Folder(_));
            p.duplicate_model = page
                .rows
                .iter()
                .zip(&page.labels)
                .zip(&page.sides)
                .enumerate()
                .map(|(index, ((row, (group, _, alias)), side))| {
                    let mut result =
                        context.duplicate_row(row, *group, *alias, *side, units, review.reference);
                    result.group_start = index == 0 || page.starts_group_at(index);
                    result.group_first_file = page.starts_group_at(index);
                    if let Some(reference) = review.reference
                        && result.group_start
                        && !result.protected
                    {
                        result.group_note = context
                            .reference_note(reference, page.has_reference[index])
                            .into();
                    }
                    result
                })
                .collect();
            p.model = page
                .rows
                .iter()
                .zip(&page.labels)
                .zip(&page.sides)
                .map(|((row, (group, representative, alias)), side)| {
                    let provenance = if context.single() {
                        String::new()
                    } else {
                        format!("{} · ", side_label(*side))
                    };
                    crate::AnalysisRow {
                        id: 0,
                        label: format!(
                            "{provenance}{} {} {} · {}",
                            if s.unverified_sources > 0 {
                                tr_format!("Group {} · location {} ·", group, representative)
                            } else {
                                tr_format!("Group {} · copy {} ·", group, representative)
                            },
                            tr!("Verified exact"),
                            if *alias { tr!("Hardlink alias") } else { "" },
                            short_label(&row.path)
                        )
                        .into(),
                        size: row
                            .logical
                            .map_or_else(|| tr!("Unknown").into(), |n| units.format_file(n))
                            .into(),
                        fraction: 0.,
                    }
                })
                .enumerate()
                .map(|(i, mut row)| {
                    row.id = i as i32;
                    row
                })
                .collect();
            p.previous = page
                .first
                .as_ref()
                .map(|c| {
                    job.review_file_page(Some(c), true)
                        .map(|p| !p.rows.is_empty())
                })
                .transpose()?
                .unwrap_or(false);
            p.next = file_has_next;
            match view {
                View::Files => {
                    p.folder_ids = page
                        .folder_references
                        .iter()
                        .map(|reference| reference.unwrap_or(0))
                        .collect();
                    for (index, reference) in page.folder_references.iter().enumerate() {
                        if let Some(folder) = reference {
                            let mut reference_label = tr_format!("Matches folder group {}", folder);
                            if page.folder_reference_counts[index] > 1 {
                                reference_label.push_str(&format!(
                                    " (+{})",
                                    page.folder_reference_counts[index] - 1
                                ));
                            }
                            p.duplicate_model[index].verification = format!(
                                "{}\n{}",
                                p.duplicate_model[index].verification, reference_label
                            )
                            .into();
                            p.model[index].label =
                                format!("{} · {}", p.model[index].label, reference_label).into();
                        }
                    }
                }
                View::Folders => {
                    p.folder_ids = folders.iter().map(|group| group.id).collect();
                    p.duplicate_model = folders
                        .iter()
                        .zip(&folder_locations)
                        .map(|(group, locations)| {
                            let has_reference = Some(locations[0].0) == review.reference;
                            let provenance = |side, row: &Row| {
                                format!(
                                    "{} · {} · {}",
                                    side_label(side),
                                    context.scope_label(side),
                                    label(&row.path)
                                )
                            };
                            let (protected, copies) =
                                locations.split_at(usize::from(has_reference));
                            let protected_location =
                                protected.first().map_or_else(String::new, |(side, row)| {
                                    format!(
                                        "{} · {}",
                                        tr!("Protected reference"),
                                        provenance(*side, row)
                                    )
                                });
                            let location = copies
                                .iter()
                                .map(|(side, row)| {
                                    if review.reference.is_some() {
                                        format!("{} · {}", tr!("Copy"), provenance(*side, row))
                                    } else {
                                        provenance(*side, row)
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            crate::DuplicateRow {
                                group: crate::number_format::integer(group.id.into()).into(),
                                side: locations
                                    .iter()
                                    .map(|(side, _)| side_label(*side))
                                    .collect::<Vec<_>>()
                                    .join(" / ")
                                    .into(),
                                name: row_name(locations[0].1).into(),
                                name_detail: tr_format!(
                                    "{} files · {} locations",
                                    group.files,
                                    locations.len()
                                )
                                .into(),
                                location: location.into(),
                                protected_location: protected_location.into(),
                                group_start: true,
                                group_note: review
                                    .reference
                                    .filter(|_| !has_reference)
                                    .map_or_else(String::new, |reference| {
                                        context.reference_note(reference, false)
                                    })
                                    .into(),
                                size: units.format_file(group.logical_bytes).into(),
                                verification: if group.verified {
                                    tr!("Verified exact")
                                } else {
                                    tr!("Incomplete coverage")
                                }
                                .into(),
                                ..Default::default()
                            }
                        })
                        .collect();
                    p.model = folders
                        .iter()
                        .enumerate()
                        .map(|(index, group)| crate::AnalysisRow {
                            id: index as i32,
                            label: format!(
                                "{} · {}",
                                tr!("Identical folders"),
                                std::iter::once((group.a_side, &group.a))
                                    .chain(std::iter::once((group.b_side, &group.b)))
                                    .chain(
                                        group
                                            .c
                                            .as_ref()
                                            .zip(group.c_side)
                                            .map(|(row, side)| (side, row))
                                    )
                                    .map(|(side, row)| format!(
                                        "{}: {}",
                                        side_label(side),
                                        short_label(&row.path)
                                    ))
                                    .collect::<Vec<_>>()
                                    .join(" · ")
                            )
                            .into(),
                            size: units.format_file(group.logical_bytes).into(),
                            fraction: 0.,
                        })
                        .collect();
                    p.previous = !job
                        .folder_groups_before(folders.first().map(|group| group.id))?
                        .is_empty()
                        && !folders.is_empty();
                    p.next = !folders.is_empty()
                        && !job
                            .folder_groups(folders.last().map(|group| group.id))?
                            .is_empty();
                }
                View::Folder(id) => {
                    let details = folder_detail.as_ref().expect("folder view has a page");
                    p.duplicate_model = detail_roots
                        .iter()
                        .map(|(side, row)| (*side, row))
                        .chain(detail_locations.iter().copied())
                        .enumerate()
                        .map(|(index, (side, row))| {
                            let mut result = context.duplicate_row(
                                row,
                                id,
                                false,
                                side,
                                units,
                                review.reference,
                            );
                            result.group_start = index == 0;
                            if let Some(reference) = review.reference
                                && result.group_start
                                && !result.protected
                            {
                                result.group_note = context.reference_note(reference, false).into();
                            }
                            result
                        })
                        .collect();
                    p.model = p
                        .rows
                        .iter()
                        .enumerate()
                        .map(|(index, row)| crate::AnalysisRow {
                            id: index as i32,
                            label: short_label(&row.path).into(),
                            size: row
                                .logical
                                .map_or_else(|| tr!("Folder").into(), |n| units.format_file(n))
                                .into(),
                            fraction: 0.,
                        })
                        .collect();
                    p.previous = details
                        .first
                        .as_ref()
                        .map(|cursor| {
                            job.folder_page(id, Some(cursor), true)
                                .map(|page| !page.rows.is_empty())
                        })
                        .transpose()?
                        .unwrap_or(false);
                    p.next = detail_has_next;
                }
            }
            p.selection_epoch = selection_epoch;
            p.reference = review.reference;
            p.selection_ready = review.reference.is_some()
                && matches!(review.reason, engine::ReviewReason::Eligible);
            p.selection_summary = tr_format!(
                "Selected for review: {} files · {} folders · {} logical.",
                crate::number_format::integer(review.files.into()),
                crate::number_format::integer(review.folders.into()),
                units.format_file(review.logical_bytes)
            );
            p.selection_notice = if let Some(notice) = selection_notice {
                notice.label().into()
            } else if !p.selection_ready {
                selection_reason(review.reason).into()
            } else {
                String::new()
            };
            p.selection_preview_open = selection_preview_open;
            if selection_preview_open {
                let preview = job.review_preview()?;
                let outside = context
                    .choices()
                    .enumerate()
                    .filter(|(side, _)| Some(*side as u8) != review.reference)
                    .map(|(side, choice)| {
                        tr_format!(
                            "For review on side {}: {}",
                            side_label(side as u8),
                            format!("{} · {}", choice.label, short_label(&choice.native_root))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                p.selection_preview = format!(
                    "{}\n{}\n{}\n{}",
                    outside,
                    tr_format!(
                        "Preview: {} files · {} folders · {} logical.",
                        crate::number_format::integer(preview.eligible_files.into()),
                        crate::number_format::integer(preview.eligible_folders.into()),
                        units.format_file(preview.eligible_logical_bytes)
                    ),
                    tr!(
                        "All eligible results in this comparison, including pages not displayed. Logical size is not recoverable space."
                    ),
                    tr!(
                        "Selection uses the recorded comparison evidence; compare explicitly again to refresh it."
                    )
                );
            }
            p.selection_targets = match view {
                _ if context.single() => Vec::new(),
                View::Folders => folders
                    .iter()
                    .map(|group| Some(SelectionTarget::Folder(group.id)))
                    .collect(),
                _ => p
                    .rows
                    .iter()
                    .map(|row| job.review_row_target(row))
                    .collect::<sortsize_store::inventory::Result<Vec<_>>>()?,
            };
            for (row, target) in p.duplicate_model.iter_mut().zip(&p.selection_targets) {
                let Some(target) = target else {
                    row.selection_label = selection_reason(engine::ReviewReason::Unverified).into();
                    continue;
                };
                let state = job.review_state(*target)?;
                row.selectable = state.eligible;
                row.selected = state.selected;
                row.partial = state.partial;
                if state.reason == engine::ReviewReason::Alias && row.copy {
                    row.copy = false;
                    row.role_label = "".into();
                }
                row.selection_label = if state.eligible {
                    let description = match (*target, view) {
                        (SelectionTarget::Folder(id), View::Folders) => folders
                            .iter()
                            .find(|group| group.id == id)
                            .map(|group| {
                                std::iter::once((group.a_side, &group.a))
                                    .chain(std::iter::once((group.b_side, &group.b)))
                                    .chain(
                                        group
                                            .c
                                            .as_ref()
                                            .zip(group.c_side)
                                            .map(|(row, side)| (side, row)),
                                    )
                                    .filter(|(side, _)| Some(*side) != review.reference)
                                    .map(|(side, row)| {
                                        tr_format!(
                                            "For review on side {}: {}",
                                            side_label(side),
                                            short_label(&row.path)
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default(),
                        _ => tr_format!("For review on side {}: {}", row.side, row.name),
                    };
                    description.into()
                } else {
                    selection_reason(state.reason).into()
                };
            }
            p._memory = Some(memory);
            p.folder_cancelled = acquisition.is_some() && s.cancelled;
            if let (Some(acquisition), Some(options), Some(temporary_parent), 0) = (
                acquisition,
                context.folder_options,
                context.folder_temporary_parent.as_ref(),
                file_count,
            ) {
                // An empty cancelled search releases its worker. Keep the raw
                // counts so a language change can still apply the last reply.
                p.folder_terminal = Some(FolderTerminalDetails {
                    failure: None,
                    telemetry: FolderTelemetry {
                        acquisition: *acquisition,
                        summary: s.clone(),
                    },
                    options,
                    temporary_parent: temporary_parent.clone(),
                });
            }
            context.publish(p);
            #[cfg(all(windows, test))]
            if let Some(path) = dump.take() {
                dump_measurement(job, Path::new(&path))?;
            }
            if acquisition.is_some() && context.cancel.is_cancelled() && file_count == 0 {
                return Ok(());
            }
            force = false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(all(windows, test))]
fn dump_measurement(
    job: &engine::DuplicateJob<'_, '_>,
    path: &Path,
) -> sortsize_store::inventory::Result<()> {
    use serde_json::json;
    use std::io::{BufWriter, Write};

    let staging = path.with_extension("pending");
    let mut output = BufWriter::new(std::fs::File::create(&staging)?);
    let mut write = |value: serde_json::Value| -> std::io::Result<()> {
        serde_json::to_writer(&mut output, &value).map_err(std::io::Error::other)?;
        output.write_all(b"\n")
    };
    let summary = job.summary();
    // Qualification diagnostics are intentionally absent from the product export.
    let mut measured = serde_json::to_value(summary).map_err(std::io::Error::other)?;
    measured["selected_logical_bytes"] =
        serde_json::to_value(summary.selected_logical_bytes).map_err(std::io::Error::other)?;
    measured["total_read_limit"] = json!(summary.total_read_limit);
    measured["file_size_limit"] = json!(summary.file_size_limit);
    measured["temporary_limit"] = json!(summary.temporary_limit);
    measured["error_limit"] = json!(summary.error_limit);
    measured["unverified_files"] = json!(summary.unverified_files);
    write(json!({
        "record": "summary",
        "summary": measured,
        "temporary_usage": job.temporary_usage(),
    }))?;
    let mut after = None;
    loop {
        let groups = job.groups(after)?;
        write(json!({"record": "group_page", "after": after, "groups": groups}))?;
        for group in &groups {
            let mut member_after = None;
            loop {
                let members = job.members(group.id, member_after)?;
                write(
                    json!({"record": "member_page", "group": group.id, "after": member_after, "members": members}),
                )?;
                let Some(last) = members.last() else { break };
                member_after = Some(last.cursor);
            }
        }
        let Some(last) = groups.last() else { break };
        after = Some(last.id);
    }
    output.flush()?;
    drop(output);
    std::fs::rename(staging, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_store::{
        Durability,
        inventory::Store,
        scheduler::{Session, State},
    };

    fn inventory(root: &Path, directory: &Path, id: u128) -> Inventory {
        inventory_with_source(root, directory, id, id)
    }
    fn inventory_with_source(root: &Path, directory: &Path, id: u128, source: u128) -> Inventory {
        let c = AcquisitionContext {
            inventory: InventoryId(id),
            source: SourceId(source),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let mut session = Session::new(
            Store::create(
                directory,
                c.inventory,
                c.source,
                Durability::Reconstructible,
            )
            .unwrap(),
            c,
            Guard::open(root, Policy::default()).unwrap(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while session.tick().unwrap().state != State::Idle {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        while session.finalize_step().unwrap().is_none() {}
        session
            .store_mut()
            .record_measurement(c.generation, root, 1, 2)
            .unwrap();
        Inventory {
            saved: session.store().saved(c.generation).unwrap(),
            workspace: None,
        }
    }
    fn fixture() -> (tempfile::TempDir, Choice, Choice) {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("source-a");
        let b = t.path().join("source-b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("renamed"), b"identical").unwrap();
        std::fs::write(b.join("one"), b"identical").unwrap();
        std::fs::write(b.join("two"), b"identical").unwrap();
        let ai = inventory(&a, &t.path().join("inventory-a"), 701);
        let bi = inventory(&b, &t.path().join("inventory-b"), 702);
        (
            t,
            Choice {
                label: "A".into(),
                logical_root: a.clone(),
                native_root: a,
                inventories: vec![ai],
            },
            Choice {
                label: "B".into(),
                logical_root: b.clone(),
                native_root: b,
                inventories: vec![bi],
            },
        )
    }
    #[track_caller]
    fn terminal(handle: &Handle) -> Presentation {
        terminal_within(handle, Duration::from_secs(30))
    }

    fn terminal_within(handle: &Handle, timeout: Duration) -> Presentation {
        let deadline = Instant::now() + timeout;
        let mut last_status = None;
        loop {
            if let Some(p) = handle.take() {
                if !p.busy {
                    return p;
                }
                last_status = Some(p.status);
            }
            assert!(
                Instant::now() < deadline,
                "duplicate worker did not settle: {last_status:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn settled(lane: &Mutex<service::Lane>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !lane.lock().unwrap().duplicate_operations.is_empty() {
            assert!(
                Instant::now() < deadline,
                "duplicate cleanup owner did not release"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn duplicate_review_keeps_protected_locations_and_group_context_across_pages() {
        let presentation = crate::presentation::Context {
            language: crate::locale::Language::French,
            ..Default::default()
        };
        let t = tempfile::tempdir().unwrap();
        let choices: Vec<_> = (0..3)
            .map(|side| {
                let root = t.path().join(format!("dossier-{}", side_label(side)));
                std::fs::create_dir(&root).unwrap();
                for i in 0..[102, 101, 1][usize::from(side)] {
                    std::fs::write(root.join(format!("{side}-{i}.bin")), b"large group").unwrap();
                }
                if side != 0 {
                    std::fs::write(root.join("orphan.bin"), b"orphan").unwrap();
                }
                let saved = inventory(
                    &root,
                    &t.path().join(format!("inventory-{side}")),
                    3_100 + u128::from(side),
                );
                Choice {
                    label: "Same volume".into(),
                    logical_root: root.clone(),
                    native_root: root,
                    inventories: vec![saved],
                }
            })
            .collect();
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let handle = Handle::start_three(
            894,
            choices[0].clone(),
            choices[1].clone(),
            choices[2].clone(),
            lane.clone(),
            Default::default(),
            presentation,
        )
        .unwrap();
        let initial = terminal(&handle);
        assert!(initial.complete, "{}", initial.warning);
        assert_eq!((initial.folder_count, initial.file_count), (0, 2));
        assert!(initial.reference.is_none());
        assert!(
            initial
                .duplicate_model
                .iter()
                .all(|row| !row.selectable && !row.protected)
        );
        handle.reference(Some(0));
        let first = terminal(&handle);
        assert!(first.next && !first.previous);
        assert_eq!(first.rows.len(), 200);
        let protected: Vec<_> = first
            .duplicate_model
            .iter()
            .filter(|row| row.protected)
            .collect();
        assert_eq!(protected.len(), 102);
        assert!(protected.iter().all(|row| row.side == "A"
            && row.scope_label == "dossier-A"
            && row.role_label == "Référence protégée"
            && row.verification == "Contenu identique vérifié"
            && !row.selectable
            && !row.selected));
        let protected_group = &protected[0].group;
        let grouped_rows: Vec<_> = first
            .duplicate_model
            .iter()
            .filter(|row| row.group == *protected_group)
            .collect();
        assert!(grouped_rows[..102].iter().all(|row| row.protected));
        assert!(
            grouped_rows[102..]
                .iter()
                .all(|row| row.copy && row.selectable && !row.selected)
        );
        handle.page(true);
        let second = terminal(&handle);
        assert!(second.previous && !second.next);
        assert_eq!(second.rows.len(), 6);
        assert!(first.duplicate_model[0].group_first_file);
        assert!(second.duplicate_model[0].group_start);
        assert!(!second.duplicate_model[0].group_first_file);
        assert!(
            second.duplicate_model[0]
                .group_note
                .contains("A · dossier-A")
        );
        assert!(
            second
                .duplicate_model
                .iter()
                .filter(|row| row.group == *protected_group)
                .all(|row| row.copy && row.selectable && !row.selected)
        );
        let orphan: Vec<_> = first
            .duplicate_model
            .iter()
            .chain(&second.duplicate_model)
            .filter(|row| row.name == "orphan.bin")
            .collect();
        assert_eq!(orphan.len(), 2);
        assert!(orphan.iter().all(|row| !row.selectable));
        assert!(orphan.iter().any(|row| row.group_start
            && row.group_note == "Aucun exemplaire dans le dossier de référence A"));
        handle.toggle(second.selection_targets[0].unwrap(), true, second.reference);
        let marked = terminal(&handle);
        assert!(marked.duplicate_model[0].selected);
        handle.page(false);
        let returned = terminal(&handle);
        assert!(returned.next && !returned.previous);
        assert_eq!(
            returned
                .rows
                .iter()
                .map(|row| &row.path)
                .collect::<Vec<_>>(),
            first.rows.iter().map(|row| &row.path).collect::<Vec<_>>()
        );
        assert_eq!(returned.selection_summary, marked.selection_summary);
        handle.reference(Some(2));
        let changed = terminal(&handle);
        assert_eq!(changed.reference, Some(2));
        assert!(changed.duplicate_model.iter().all(|row| !row.selected));
        for row in changed.duplicate_model.iter().filter(|row| row.group_start) {
            assert!(row.protected);
            assert_eq!(row.side, "C");
        }
        drop(handle);
        settled(&lane);
    }

    #[test]
    fn duplicate_worker_same_volume_shared_parent_preserves_sides_and_reference() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        let t = tempfile::tempdir().unwrap();
        let parent = t.path().join("folders");
        let roots = [parent.join("A"), parent.join("B"), parent.join("C")];
        for (side, root) in roots.iter().enumerate() {
            std::fs::create_dir_all(root.join("identical")).unwrap();
            std::fs::write(root.join("shared.bin"), b"shared across three folders").unwrap();
            std::fs::write(root.join("unique.bin"), [side as u8]).unwrap();
            std::fs::write(root.join("identical/child.bin"), b"identical folder child").unwrap();
        }
        // The parent can cover all three sides without being their comparison root.
        std::fs::write(parent.join("outside.bin"), b"shared across three folders").unwrap();
        let shared = inventory(&parent, &t.path().join("inventory"), 791);
        let choice = |side: usize| Choice {
            label: side_label(side as u8).into(),
            logical_root: parent.clone(),
            native_root: roots[side].clone(),
            inventories: vec![shared.clone()],
        };
        for third in [false, true] {
            let lane = Arc::new(Mutex::new(service::Lane::default()));
            let handle = if third {
                Handle::start_three(
                    891,
                    choice(0),
                    choice(1),
                    choice(2),
                    lane.clone(),
                    Default::default(),
                    crate::presentation::Context::current(),
                )
            } else {
                Handle::start(
                    891,
                    choice(0),
                    choice(1),
                    lane.clone(),
                    Default::default(),
                    crate::presentation::Context::current(),
                )
            }
            .unwrap();
            let folders = terminal(&handle);
            assert!(folders.complete, "{}", folders.warning);
            assert_eq!(folders.folder_count, 1);
            assert_eq!(folders.file_count, 1);
            assert_eq!(
                folders.duplicate_model[0].side,
                if third { "A / B / C" } else { "A / B" }
            );
            handle.open_folder(folders.folder_ids[0]);
            let details = terminal(&handle);
            assert!(details.folder_detail);
            for (side, root) in roots.iter().enumerate().take(if third { 3 } else { 2 }) {
                let expected = side_label(side as u8);
                let rows = details
                    .rows
                    .iter()
                    .zip(&details.duplicate_model)
                    .filter(|(row, _)| row.path.starts_with(root))
                    .collect::<Vec<_>>();
                assert_eq!(
                    rows.len(),
                    if third { 2 } else { 1 },
                    "child and optional ABC root for side {expected}"
                );
                assert!(rows.iter().all(|(_, model)| model.side == expected));
            }
            handle.category(false);
            let files = terminal(&handle);
            assert_eq!(files.rows.len(), if third { 3 } else { 2 });
            assert!(
                files
                    .rows
                    .iter()
                    .all(|row| row.path != parent.join("outside.bin"))
            );
            handle.reference(Some(0));
            let reference_a = terminal(&handle);
            assert!(reference_a.selection_ready);
            for (row, model) in reference_a.rows.iter().zip(&reference_a.duplicate_model) {
                let side = roots
                    .iter()
                    .position(|root| row.path.starts_with(root))
                    .unwrap();
                assert_eq!(model.side, side_label(side as u8));
                assert_eq!(model.selectable, side != 0);
                assert_eq!(row.key.inventory, shared.saved.context.inventory);
            }
            let b_index = reference_a
                .duplicate_model
                .iter()
                .position(|row| row.side == "B")
                .unwrap();
            let b_target = reference_a.selection_targets[b_index].unwrap();
            handle.toggle(b_target, true, Some(0));
            let selected = terminal(&handle);
            assert!(selected.duplicate_model[b_index].selected);
            handle.reference(Some(1));
            handle.toggle(b_target, true, Some(0));
            let reference_b = terminal(&handle);
            assert!(reference_b.duplicate_model.iter().all(|row| !row.selected));
            assert!(
                !reference_b
                    .duplicate_model
                    .iter()
                    .find(|row| row.side == "B")
                    .unwrap()
                    .selectable
            );
            drop(handle);
            settled(&lane);
        }
    }
    #[test]
    fn duplicate_worker_same_source_separate_inventories_and_zero_groups() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        for identical in [true, false] {
            let t = tempfile::tempdir().unwrap();
            let a = t.path().join("A");
            let b = t.path().join("B");
            std::fs::create_dir(&a).unwrap();
            std::fs::create_dir(&b).unwrap();
            std::fs::write(a.join("one.bin"), b"one content").unwrap();
            std::fs::write(
                b.join("one.bin"),
                if identical {
                    b"one content"
                } else {
                    b"two content"
                },
            )
            .unwrap();
            let ai = inventory_with_source(&a, &t.path().join("inventory-A"), 792, 794);
            let bi = inventory_with_source(&b, &t.path().join("inventory-B"), 793, 794);
            let lane = Arc::new(Mutex::new(service::Lane::default()));
            let choice = |root: PathBuf, inventory: Inventory| Choice {
                label: short_label(&root),
                logical_root: t.path().to_owned(),
                native_root: root,
                inventories: vec![inventory],
            };
            let handle = Handle::start(
                892,
                choice(a, ai),
                choice(b, bi),
                lane.clone(),
                Default::default(),
                crate::presentation::Context::current(),
            )
            .unwrap();
            let result = terminal(&handle);
            assert!(result.complete, "{}", result.warning);
            assert_eq!(result.groups, u64::from(identical));
            if identical {
                assert_eq!(result.folder_count, 1);
                handle.reference(Some(0));
                let reference = terminal(&handle);
                assert!(reference.selection_ready);
                assert_eq!(reference.duplicate_model[0].side, "A / B");
                assert!(!reference.duplicate_model[0].protected_location.is_empty());
                assert!(reference.duplicate_model[0].selectable);
                handle.toggle(reference.selection_targets[0].unwrap(), true, Some(0));
                assert!(terminal(&handle).duplicate_model[0].selected);
            } else {
                assert!(result.rows.is_empty());
                assert_eq!(result.folder_count, 0);
                assert_eq!(result.file_count, 0);
            }
            drop(handle);
            settled(&lane);
        }
    }
    #[test]
    fn duplicate_worker_identical_or_nested_folders_are_rejected_before_content() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        for nested in [false, true] {
            let (_t, a, mut b) = fixture();
            b.native_root = if nested {
                a.native_root.join("child")
            } else {
                a.native_root.clone()
            };
            let lane = Arc::new(Mutex::new(service::Lane::default()));
            let handle = Handle::start(
                893,
                a,
                b,
                lane.clone(),
                Default::default(),
                crate::presentation::Context::current(),
            )
            .unwrap();
            let rejected = terminal(&handle);
            assert!(!rejected.complete);
            assert!(rejected.warning.contains("distinct and not nested"));
            assert!(rejected.rows.is_empty());
            assert!(!rejected.selection_ready);
            drop(handle);
            settled(&lane);
        }
    }
    #[test]
    fn duplicate_worker_broad_inventory_reads_only_explicit_folders() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            generation: 0,
        }
        .enter();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("wide-a");
        let b = t.path().join("wide-b");
        let selected_a = a.join("selected");
        let selected_b = b.join("selected");
        for path in [&selected_a, &selected_b] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("chosen.bin"), b"chosen identical bytes").unwrap();
        }
        for path in [&a, &b] {
            std::fs::write(path.join("excluded.bin"), b"outside identical bytes").unwrap();
        }
        let ai = inventory(&a, &t.path().join("wide-inventory-a"), 711);
        let bi = inventory(&b, &t.path().join("wide-inventory-b"), 712);
        // If the worker silently broadens its roots, these stale records would
        // incur source errors in addition to the extra duplicate group.
        std::fs::remove_file(a.join("excluded.bin")).unwrap();
        std::fs::remove_file(b.join("excluded.bin")).unwrap();
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let handle = Handle::start(
            811,
            Choice {
                label: "A".into(),
                logical_root: a,
                native_root: selected_a.clone(),
                inventories: vec![ai],
            },
            Choice {
                label: "B".into(),
                logical_root: b,
                native_root: selected_b.clone(),
                inventories: vec![bi],
            },
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        let result = terminal(&handle);
        assert_eq!(result.groups, 1, "{} {}", result.status, result.statistics);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.folder_count, 1);
        assert_eq!(result.file_count, 0);
        assert!(result.complete, "{}", result.warning);
        assert!(result.warning.is_empty());
        handle.open_folder(result.folder_ids[0]);
        let result = terminal(&handle);
        assert_eq!(result.rows.len(), 2);
        assert!(
            result
                .rows
                .iter()
                .all(|r| r.path.starts_with(&selected_a) || r.path.starts_with(&selected_b))
        );
        assert!(
            result
                .rows
                .iter()
                .all(|r| r.path.file_name().unwrap() == "chosen.bin")
        );
        assert!(result.status.contains("errors: 0"), "{}", result.status);
        assert!(
            result.complete,
            "subroot proofs remain fresh despite mutations outside the selected scopes"
        );
        assert!(result.warning.is_empty());
        assert!(
            !result
                .statistics
                .contains("Source continuity is unverified")
        );
        assert_eq!(result.duplicate_model.len(), result.rows.len());
        drop(handle);
        settled(&lane);
    }

    #[test]
    fn duplicate_worker_three_sources_one_group_and_two_copies_selected_offline() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..crate::presentation::Context::current()
        }
        .enter();
        let t = tempfile::tempdir().unwrap();
        let choices = (0..3)
            .map(|side| {
                let root = t.path().join(format!("root-{}", side_label(side)));
                std::fs::create_dir(&root).unwrap();
                for i in 0..10 {
                    std::fs::write(root.join(format!("file-{i}")), format!("content-{i}")).unwrap();
                }
                let saved = inventory(
                    &root,
                    &t.path().join(format!("inventory-{side}")),
                    3000 + side as u128,
                );
                Choice {
                    label: side_label(side).into(),
                    logical_root: root.clone(),
                    native_root: root,
                    inventories: vec![saved],
                }
            })
            .collect::<Vec<_>>();
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let handle = Handle::start_three(
            3000,
            choices[0].clone(),
            choices[1].clone(),
            choices[2].clone(),
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        let initial = terminal(&handle);
        assert!(
            initial.complete,
            "{} {}",
            initial.warning, initial.statistics
        );
        assert_eq!(
            (initial.folder_count, initial.file_count, initial.rows.len()),
            (1, 0, 1)
        );
        assert!(
            initial.duplicate_model[0]
                .name_detail
                .contains("10 files · 3 locations")
        );
        assert_eq!(initial.duplicate_model[0].side, "A / B / C");
        let group = initial.folder_ids[0];
        // All presentation and review actions now consult only recorded results.
        for (side, choice) in choices.iter().enumerate() {
            std::fs::rename(
                &choice.native_root,
                t.path().join(format!("offline-{side}")),
            )
            .unwrap();
        }
        handle.reference(Some(0));
        let protected = terminal(&handle);
        assert!(protected.selection_ready);
        assert!(protected.selection_summary.contains("0 files"));
        handle.preview();
        let preview = terminal(&handle);
        assert!(preview.selection_preview.contains("20 files · 2 folders"));
        assert!(
            preview.selection_preview.contains("side B")
                && preview.selection_preview.contains("side C")
        );
        handle.apply();
        let selected = terminal(&handle);
        assert!(selected.selection_summary.contains("20 files · 2 folders"));
        assert!(selected.duplicate_model[0].selected);
        handle.open_folder(group);
        let details = terminal(&handle);
        assert_eq!(details.rows.len(), 33);
        for (row, native) in details.duplicate_model.iter().zip(&details.rows) {
            let expected = if native.kind == sortsize_store::inventory::Kind::Directory {
                native.path.as_path()
            } else {
                native.path.parent().unwrap()
            };
            assert_eq!(row.location, label(expected));
        }
        assert_eq!(
            details
                .duplicate_model
                .iter()
                .zip(&details.rows)
                .filter(|(row, native)| row.selected
                    && native.kind == sortsize_store::inventory::Kind::File)
                .count(),
            20
        );
        assert!(
            details
                .duplicate_model
                .iter()
                .filter(|row| row.side == "A")
                .all(|row| !row.selected && !row.selectable)
        );
        let b_root = details
            .rows
            .iter()
            .position(|row| row.path == choices[1].native_root)
            .expect("B root is individually shown");
        handle.toggle(details.selection_targets[b_root].unwrap(), false, Some(0));
        let only_c = terminal(&handle);
        assert!(only_c.selection_summary.contains("10 files · 1 folders"));
        handle.category(true);
        assert!(terminal(&handle).duplicate_model[0].partial);
        handle.open_folder(group);
        terminal(&handle);
        handle.reference(Some(2));
        handle.toggle(SelectionTarget::Folder(group), true, Some(0));
        let changed = terminal(&handle);
        assert_eq!(changed.reference, Some(2));
        assert!(changed.selection_summary.contains("0 files"));
        assert!(
            changed
                .duplicate_model
                .iter()
                .filter(|row| row.side == "C")
                .all(|row| !row.selected && !row.selectable)
        );
        handle.preview();
        terminal(&handle);
        handle.apply();
        let selected = terminal(&handle);
        assert_eq!(
            selected
                .duplicate_model
                .iter()
                .zip(&selected.rows)
                .filter(|(row, native)| row.selected
                    && native.kind == sortsize_store::inventory::Kind::File)
                .count(),
            20
        );
        assert!(
            selected
                .duplicate_model
                .iter()
                .filter(|row| row.side == "C")
                .all(|row| !row.selected)
        );
        handle.clear();
        assert!(terminal(&handle).selection_summary.contains("0 files"));
        drop(handle);
        settled(&lane);
    }

    #[test]
    fn duplicate_worker_presentation_is_offline_after_exact_results() {
        let (t, a, b) = fixture();
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let handle = Handle::start(
            801,
            a.clone(),
            b.clone(),
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        let p = terminal(&handle);
        assert!(p.complete, "{} {}", p.status, p.statistics);
        assert_eq!(p.groups, 1);
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.duplicate_model.len(), p.rows.len());
        for (columns, native) in p.duplicate_model.iter().zip(&p.rows) {
            assert_eq!(columns.group, "1");
            assert_eq!(
                columns.name,
                label(Path::new(native.path.file_name().unwrap())).as_str()
            );
            assert_eq!(
                columns.location,
                label(native.path.parent().unwrap()).as_str()
            );
            assert_eq!(
                columns.side,
                if native.path.starts_with(&a.native_root) {
                    "A"
                } else {
                    "B"
                }
            );
            assert_eq!(columns.size, crate::units::Units::Binary.format(9).as_str());
        }
        assert!(p.warning.is_empty(), "{}", p.warning);
        assert_eq!(
            p.scope_summary,
            [label(&a.native_root), label(&b.native_root)]
        );
        assert!(p.details.contains("G1"));
        std::fs::rename(&a.logical_root, t.path().join("offline-a")).unwrap();
        std::fs::rename(&b.logical_root, t.path().join("offline-b")).unwrap();
        let units = crate::units::Units::BinaryFixed(3);
        handle.refresh(units, crate::presentation::Context::current());
        let refreshed = terminal(&handle);
        assert!(refreshed.complete);
        assert_eq!(refreshed.rows.len(), 3);
        assert_eq!(refreshed.id, 801);
        assert_eq!(
            refreshed.duplicate_model[0].size,
            units.format_file(9).as_str()
        );
        assert!(refreshed.duplicate_model[0].size.starts_with("< "));
        drop(handle);
        settled(&lane);
    }
    #[test]
    fn duplicate_worker_cancel_replacement_and_disconnected_admission_are_honest() {
        let (_t, a, b) = fixture();
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let old = Handle::start(
            802,
            a.clone(),
            b.clone(),
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        old.cancel();
        let cancelled = terminal(&old);
        assert!(!cancelled.complete);
        assert_eq!(cancelled.id, 802);
        drop(old);
        settled(&lane);
        let new = Handle::start(
            803,
            a.clone(),
            b.clone(),
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        let p = terminal(&new);
        assert!(p.complete, "{}", p.status);
        assert_eq!(p.id, 803);
        assert_eq!(p.groups, 1);
        drop(new);
        settled(&lane);
        lane.lock()
            .unwrap()
            .disconnected_sources
            .push(b.logical_root.clone());
        assert!(
            Handle::start(
                804,
                a,
                b,
                lane.clone(),
                Default::default(),
                crate::presentation::Context::current()
            )
            .is_err()
        );
        assert!(lane.lock().unwrap().duplicate_operations.is_empty());
    }
    #[test]
    fn duplicate_worker_1240_files_are_one_folder_with_offline_bounded_details() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            generation: 0,
        }
        .enter();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("root-name-A");
        let b = t.path().join("other-root-name-B");
        for root in [&a, &b] {
            std::fs::create_dir_all(root.join("empty")).unwrap();
            for index in 0..1240 {
                std::fs::write(
                    root.join(format!("file-{index:04}.bin")),
                    format!("exact content {index:04}"),
                )
                .unwrap();
            }
        }
        let ai = inventory(&a, &t.path().join("inventory-a"), 751);
        let bi = inventory(&b, &t.path().join("inventory-b"), 752);
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let handle = Handle::start(
            851,
            Choice {
                label: "A".into(),
                logical_root: a.clone(),
                native_root: a.clone(),
                inventories: vec![ai],
            },
            Choice {
                label: "B".into(),
                logical_root: b.clone(),
                native_root: b.clone(),
                inventories: vec![bi],
            },
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        // This fixture verifies more than two thousand native files. The
        // deadline bounds the test harness, independent of the page oracle.
        let summary = terminal_within(&handle, Duration::from_secs(120));
        assert!(summary.complete, "{} {}", summary.status, summary.warning);
        assert_eq!(summary.folder_count, 1);
        assert_eq!(summary.file_count, 0);
        assert_eq!(summary.groups, 1);
        assert!(summary.folder_view && !summary.folder_detail);
        assert_eq!(summary.model.len(), 1);
        assert_eq!(summary.duplicate_model.len(), 1);
        assert!(
            summary.duplicate_model[0]
                .name_detail
                .contains("1240 files")
        );
        assert!(summary.duplicate_model[0].location.contains(&label(&a)));
        assert!(summary.duplicate_model[0].location.contains(&label(&b)));
        let group = summary.folder_ids[0];
        // From this point every category, disclosure and page is served by the
        // leased SQLite result. Both live source paths have disappeared.
        std::fs::rename(&a, t.path().join("offline-a")).unwrap();
        std::fs::rename(&b, t.path().join("offline-b")).unwrap();
        // No availability probe is implicit: only recorded evidence is used.
        // A known revocation is checked separately below through cancellation.
        let check_totals = |page: &Presentation, files: u64, folders: u64| {
            assert_eq!(
                page.selection_summary,
                tr_format!(
                    "Selected for review: {} files · {} folders · {} logical.",
                    crate::number_format::integer(files.into()),
                    crate::number_format::integer(folders.into()),
                    crate::units::Units::default().format_file(files * 18)
                )
            );
            assert_eq!(
                page.status, summary.status,
                "presentation performs no new reads"
            );
        };
        assert_eq!(summary.reference, None);
        assert!(!summary.duplicate_model[0].selectable);
        handle.reference(Some(0));
        let protected_a = terminal(&handle);
        check_totals(&protected_a, 0, 0);
        assert_eq!(protected_a.reference, Some(0));
        assert!(protected_a.duplicate_model[0].selectable);
        assert!(!protected_a.duplicate_model[0].selected);
        assert_eq!(protected_a.duplicate_model[0].side, "A / B");
        assert_eq!(protected_a.duplicate_model[0].name, "root-name-A");
        assert!(
            protected_a.duplicate_model[0]
                .location
                .contains("other-root-name-B")
        );
        assert!(
            protected_a.duplicate_model[0]
                .protected_location
                .contains(&label(&a))
        );
        assert_eq!(protected_a.rows[0].path, a);
        assert!(!protected_a.selection_notice.is_empty());
        handle.toggle(SelectionTarget::Folder(group), true, protected_a.reference);
        check_totals(&terminal(&handle), 1240, 1);
        handle.open_folder(group);
        let first = terminal(&handle);
        assert!(first.rows.len() <= 200);
        assert!(
            first
                .duplicate_model
                .iter()
                .filter(|row| row.side == "A")
                .all(|row| !row.selectable && !row.selected)
        );
        assert!(
            first
                .duplicate_model
                .iter()
                .filter(|row| row.side == "B")
                .all(|row| row.selectable && row.selected)
        );
        let child = first
            .rows
            .iter()
            .position(|row| {
                row.kind == sortsize_store::inventory::Kind::File && row.path.starts_with(&b)
            })
            .unwrap();
        let child_path = first.rows[child].path.clone();
        let child_target = first.selection_targets[child].unwrap();
        handle.toggle(child_target, false, first.reference);
        let partial = terminal(&handle);
        check_totals(&partial, 1239, 0);
        assert!(!partial.duplicate_model[child].selected);
        handle.page(true);
        let second = terminal(&handle);
        check_totals(&second, 1239, 0);
        assert!(
            second
                .duplicate_model
                .iter()
                .filter(|row| row.side == "B")
                .all(|row| row.selected)
        );
        handle.page(false);
        let returned = terminal(&handle);
        let returned_child = returned
            .rows
            .iter()
            .position(|row| row.path == child_path)
            .unwrap();
        assert!(!returned.duplicate_model[returned_child].selected);
        handle.category(true);
        let partial_parent = terminal(&handle);
        assert!(
            partial_parent.duplicate_model[0].partial
                && !partial_parent.duplicate_model[0].selected
        );
        check_totals(&partial_parent, 1239, 0);
        // A stale B toggle queued after switching to reference B is refused,
        // and no presentation from the earlier reference can restore marks.
        handle.reference(Some(1));
        handle.toggle(child_target, true, partial_parent.reference);
        let protected_b = terminal(&handle);
        assert_eq!(protected_b.reference, Some(1));
        assert_eq!(protected_b.duplicate_model[0].side, "B / A");
        assert_eq!(protected_b.duplicate_model[0].name, "other-root-name-B");
        assert!(
            protected_b.duplicate_model[0]
                .location
                .contains("root-name-A")
        );
        assert!(
            protected_b.duplicate_model[0]
                .protected_location
                .contains(&label(&b))
        );
        check_totals(&protected_b, 0, 0);
        handle.preview();
        let preview = terminal(&handle);
        assert!(preview.selection_preview_open);
        assert!(preview.selection_preview.contains("pages not displayed"));
        assert!(preview.selection_preview.contains("side A"));
        assert!(preview.selection_preview.contains(&short_label(&a)));
        check_totals(&preview, 0, 0);
        handle.dismiss();
        assert!(!terminal(&handle).selection_preview_open);
        handle.apply();
        check_totals(&terminal(&handle), 0, 0); // No application without preview.
        handle.preview();
        assert!(terminal(&handle).selection_preview_open);
        handle.apply();
        let assisted = terminal(&handle);
        check_totals(&assisted, 1240, 1);
        assert!(assisted.duplicate_model[0].selected);
        handle.clear();
        check_totals(&terminal(&handle), 0, 0);
        handle.reference(None);
        let none = terminal(&handle);
        assert_eq!(none.reference, None);
        assert!(!none.duplicate_model[0].selectable);
        handle.open_folder(group);
        let mut seen = std::collections::HashSet::new();
        let mut pages = 0;
        loop {
            let details = terminal(&handle);
            assert!(details.complete, "{}", details.warning);
            assert!(details.folder_detail);
            assert!(!details.rows.is_empty() && details.rows.len() <= 200);
            assert_eq!(details.rows.len(), details.model.len());
            assert_eq!(details.rows.len(), details.duplicate_model.len());
            for row in &details.rows {
                assert!(seen.insert(row.path.clone()), "duplicate detail row");
            }
            pages += 1;
            if !details.next {
                break;
            }
            assert!(pages < 20, "folder pagination must be bounded");
            handle.page(true);
        }
        assert!(pages > 1);
        assert_eq!(
            seen.iter()
                .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
                .count(),
            2480
        );
        assert!(seen.contains(&a.join("empty")) && seen.contains(&b.join("empty")));
        handle.page(false);
        assert!(terminal(&handle).next);
        handle.category(true);
        let collapsed = terminal(&handle);
        assert!(!collapsed.folder_detail && collapsed.folder_view);
        assert_eq!(collapsed.model.len(), 1);
        handle.category(false);
        let files = terminal(&handle);
        assert!(!files.folder_view);
        assert!(
            files.rows.is_empty(),
            "folder members are not counted again"
        );
        assert_eq!(files.file_count, 0);
        handle.reference(Some(0));
        check_totals(&terminal(&handle), 0, 0);
        handle.preview();
        assert!(terminal(&handle).selection_preview_open);
        handle.apply();
        check_totals(&terminal(&handle), 1240, 1);
        handle.cancel();
        let revoked = terminal(&handle);
        check_totals(&revoked, 0, 0);
        assert!(!revoked.selection_ready);
        assert!(revoked.selection_notice.contains("disconnected"));
        handle.reference(Some(1));
        let still_revoked = terminal(&handle);
        assert!(!still_revoked.selection_ready);
        check_totals(&still_revoked, 0, 0);
        drop(handle);
        settled(&lane);
    }

    #[test]
    fn duplicate_worker_residual_file_references_its_folder_without_repeating_members() {
        let _presentation = crate::presentation::Context {
            language: crate::locale::Language::English,
            generation: 0,
        }
        .enter();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("source-a");
        let b = t.path().join("source-b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir_all(b.join("mirror")).unwrap();
        for path in [a.join("item"), b.join("mirror/item"), b.join("extra-copy")] {
            std::fs::write(path, b"one content in three locations").unwrap();
        }
        let ai = inventory(&a, &t.path().join("inventory-a"), 761);
        let bi = inventory(&b, &t.path().join("inventory-b"), 762);
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let handle = Handle::start(
            861,
            Choice {
                label: "A".into(),
                logical_root: a.clone(),
                native_root: a.clone(),
                inventories: vec![ai],
            },
            Choice {
                label: "B".into(),
                logical_root: b.clone(),
                native_root: b.clone(),
                inventories: vec![bi],
            },
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        )
        .unwrap();
        let folders = terminal(&handle);
        assert!(folders.complete, "{}", folders.warning);
        assert_eq!(folders.folder_count, 1);
        assert_eq!(folders.file_count, 1);
        let group = folders.folder_ids[0];
        handle.category(false);
        let files = terminal(&handle);
        assert_eq!(
            files.rows.len(),
            1,
            "covered members remain in the folder only"
        );
        assert_eq!(files.rows[0].path, b.join("extra-copy"));
        assert_eq!(files.folder_ids, vec![group]);
        assert!(
            files.duplicate_model[0]
                .verification
                .contains(&format!("Matches folder group {group}"))
        );
        std::fs::rename(&a, t.path().join("offline-a")).unwrap();
        std::fs::rename(&b, t.path().join("offline-b")).unwrap();
        handle.open_folder(files.folder_ids[0]);
        let details = terminal(&handle);
        assert!(details.folder_detail && details.complete);
        assert_eq!(details.rows.len(), 2);
        assert!(details.rows.iter().any(|row| row.path == a.join("item")));
        assert!(
            details
                .rows
                .iter()
                .any(|row| row.path == b.join("mirror/item"))
        );
        drop(handle);
        settled(&lane);
    }

    #[test]
    fn duplicate_worker_pending_task_budget_is_explicit() {
        let lane = Arc::new(Mutex::new(service::Lane::default()));
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        lane.lock().unwrap().duplicate_operations = (0..MAX_OPERATIONS)
            .map(|i| (i as u64, vec![PathBuf::from("pending")], cancelled.clone()))
            .collect();
        let choice = Choice {
            label: "admission".into(),
            logical_root: PathBuf::from("unopened"),
            native_root: PathBuf::from("unopened"),
            inventories: vec![],
        };
        let result = Handle::start(
            900,
            choice.clone(),
            choice,
            lane.clone(),
            Default::default(),
            crate::presentation::Context::current(),
        );
        assert!(result.is_err());
        assert_eq!(
            lane.lock().unwrap().duplicate_operations.len(),
            MAX_OPERATIONS
        );
        assert!(budget_label(None).contains(&MAX_OPERATIONS.to_string()));
    }
    #[test]
    fn duplicate_worker_progress_uses_latest_presentation() {
        let initial = crate::presentation::Context {
            language: crate::locale::Language::French,
            generation: 71,
        };
        let latest = crate::presentation::Context {
            language: crate::locale::Language::English,
            generation: 72,
        };
        let _scope = initial.enter();
        let choice = Choice {
            label: "source".into(),
            logical_root: PathBuf::from("source"),
            native_root: PathBuf::from("source"),
            inventories: vec![],
        };
        let context = Context {
            profile: None,
            id: 990,
            a: choice.clone(),
            b: Some(choice),
            c: None,
            lane: Arc::new(Mutex::new(service::Lane::default())),
            control: Arc::new(Mutex::new(Control {
                closed: false,
                page: None,
                view: None,
                refresh: false,
                selection_commands: Default::default(),
                selection_epoch: 0,
                reference: None,
                side_count: 2,
                units: Default::default(),
                presentation: initial,
            })),
            output: Arc::new(Mutex::new(None)),
            cancel: Default::default(),
            folder_options: None,
            folder_temporary_parent: None,
        };
        // A source call returns after the UI has switched language/presentation.
        context.control.lock().unwrap().presentation = latest;
        let progress = context.busy(&Summary::default());
        assert_eq!(progress.presentation, latest);
        assert!(progress.busy);
        assert!(progress.status.starts_with("Exact groups:"));
        assert!(progress.summary.starts_with("Verifying files…"));
        assert!(progress.warning.is_empty());
        assert_eq!(crate::presentation::Context::current(), initial);

        let handle = Handle {
            id: 0,
            cancel: context.cancel.clone(),
            control: context.control.clone(),
            output: context.output.clone(),
            current_folder: false,
        };
        context.publish(progress);
        handle.reference(Some(0));
        assert!(handle.take().is_none(), "queued old output is discarded");
        let obsolete = context.busy(&Summary::default());
        handle.reference(Some(1));
        context.publish(obsolete);
        assert!(handle.take().is_none(), "late old output is discarded");
        let epoch = handle.control.lock().unwrap().selection_epoch;
        handle.toggle(SelectionTarget::Folder(1), true, Some(0));
        assert_eq!(
            handle.control.lock().unwrap().selection_epoch,
            epoch,
            "old folder reference cannot select the new opposite side"
        );
        for _ in 0..1000 {
            handle.clear();
        }
        assert!(handle.control.lock().unwrap().selection_commands.len() <= MAX_SELECTION_COMMANDS);
        let mut marked = context.busy(&Summary::default());
        marked.selection_ready = true;
        handle.cancel();
        context.publish(marked);
        assert!(handle.take().is_none(), "revoked checks never return");
    }

    #[test]
    fn duplicate_limits_remain_in_the_main_warning() {
        let _scope = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        let summary = Summary {
            groups: 3,
            errors: 2,
            cancelled: true,
            budget_limited: true,
            ambiguous_aliases: 1,
            unverified_sources: 1,
            directory_listing_sources: 1,
            ..Default::default()
        };
        assert_eq!(
            summary_label(&summary, false),
            "Incomplete coverage · Exact groups: 3 · errors: 2."
        );
        let warning = warning_label(&summary, false, false);
        for visible in [
            "Incomplete coverage",
            "Cancelled.",
            "Read budget or file-size limit reached.",
            "Possible source aliases remain unverified.",
            "Unverified sources: 1.",
            "Archived directory sizes may be outdated.",
            "Some duplicates may be missed.",
        ] {
            assert!(warning.contains(visible), "{visible}: {warning}");
        }
    }

    #[test]
    fn current_folder_duplicate_diagnostics_distinguish_read_limits_and_resources() {
        let _scope = crate::presentation::Context {
            language: crate::locale::Language::English,
            ..Default::default()
        }
        .enter();
        let summary = Summary {
            bytes_read: 1_073_741_825,
            total_read_limit: Some(1_073_741_825),
            file_size_limit: Some(536_870_912),
            temporary_limit: Some(268_435_456),
            error_limit: Some(100),
            unverified_files: Some(32),
            cancelled: true,
            ..Default::default()
        };
        assert_eq!(
            content_counts(&summary),
            "Content bytes read (hashing and comparison): 1,073,741,825."
        );
        let warning = warning_label(&summary, false, true);
        for expected in [
            "Cancelled",
            "Total content read limit reached: 1,073,741,825 bytes.",
            "Files exceeded the size limit of 536,870,912 bytes.",
            "Insufficient temporary storage. Search quota: 268,435,456 bytes.",
            "Error limit reached: 100.",
            "Candidate files not fully verified: 32.",
        ] {
            assert!(warning.contains(expected), "{warning}");
        }
        let (memory, _) = folder_error(&Error::MemoryAdmission {
            minimum: 1_048_576,
            available: 8,
            category_limit: 4_194_304,
        });
        assert_eq!(
            memory,
            "Memory budget unavailable: 1,048,576 bytes required, 8 bytes available, 4,194,304 bytes category limit."
        );
        let blank = budget_label(Some(DuplicateOptions::without_content_limits()));
        assert!(blank.contains("Content read limit: No limit · file size limit: No limit"));
    }
}
