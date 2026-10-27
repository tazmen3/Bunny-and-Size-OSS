//! One explicit analysis job per panel; no joins or content reads in callbacks.
use crate::{AnalysisRow, AnalysisWindow, Controller, SortSizeWindow, presentation, tree};
use slint::{ComponentHandle, Model, VecModel};
use sortsize_core::{
    CancellationToken,
    analysis::{self, AnalysisSnapshot, SearchQuery, SearchResult},
    duplicates::{self, DuplicateReport},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
const PAGE_ROWS: usize = 200;
#[derive(Clone, Copy)]
enum Notice {
    Searching,
    Reading,
    Cancelling,
    Waiting,
    InvalidBytes,
    InvalidRange,
    InvalidQuery,
    WorkerStopped,
    Unavailable,
}
impl Notice {
    fn label(self) -> &'static str {
        match self {
            Self::Searching => tr!("Searching retained entries…"),
            Self::Reading => tr!("Reading candidate files…"),
            Self::Cancelling => {
                tr!("Cancellation requested. A system or network read may take time to return.")
            }
            Self::Waiting => tr!(
                "Waiting for the previous read to finish. Reopen Search / statistics afterwards."
            ),
            Self::InvalidBytes => {
                tr!("Invalid filter: sizes must be whole bytes from 0 to 18446744073709551615.")
            }
            Self::InvalidRange => tr!("Invalid filter: minimum size must not exceed maximum size."),
            Self::InvalidQuery => {
                tr!("Invalid filter: check the glob and the limit of 4096 bytes per field.")
            }
            Self::WorkerStopped => tr!("Analysis worker stopped"),
            Self::Unavailable => tr!("Unavailable"),
        }
    }
}
fn read_error(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind::*;
    match kind {
        PermissionDenied => tr!("Access denied. Check operating-system permissions."),
        NotFound => tr!("File no longer accessible. Check the path or network connection."),
        ConnectionAborted | ConnectionRefused | ConnectionReset | NotConnected | TimedOut
        | BrokenPipe | NetworkDown | NetworkUnreachable | HostUnreachable => {
            tr!("Network read failed. Check the connection and retry.")
        }
        Unsupported => tr!("File identity is unavailable on this storage."),
        _ => tr!("Read failed or file changed. Rescan before retrying."),
    }
}

struct Outcome {
    search: SearchResult,
    duplicates: Option<DuplicateReport>,
}
#[derive(Default)]
pub(crate) struct Panel {
    window: Option<AnalysisWindow>,
    snapshot: Option<Arc<AnalysisSnapshot>>,
    receiver: Option<mpsc::Receiver<Result<Outcome, String>>>,
    cancellation: CancellationToken,
    progress: Arc<AtomicU64>,
    files_hashed: Arc<AtomicU64>,
    reading: bool,
    result: Option<Outcome>,
    order: Vec<usize>,
    offset: usize,
    duplicate_labels: BTreeMap<usize, (usize, usize, bool)>,
    units: crate::units::Units,
    notice: Option<Notice>,
}
impl Panel {
    #[cfg(test)]
    pub(crate) fn test_window(&self) -> Option<AnalysisWindow> {
        self.window.as_ref().map(ComponentHandle::clone_strong)
    }
    #[cfg(test)]
    pub(crate) fn test_busy(&self) -> bool {
        self.receiver.is_some()
    }
    pub(crate) fn invalidate(&mut self) {
        self.cancellation.cancel();
        self.snapshot = None;
        self.notice = self.receiver.as_ref().map(|_| Notice::Waiting);
        self.result = None;
        self.order.clear();
        self.offset = 0;
        if let Some(w) = &self.window {
            w.set_results(Default::default());
            w.set_statistics(Default::default());
            w.set_scope_note("".into());
            w.set_result_note("".into());
            w.set_source_path("".into());
            w.set_diagnostic_text("".into());
            w.set_empty_message("".into());
            w.set_page_label("".into());
            w.set_selected_id(-1);
            let _ = w.hide();
        }
    }
    fn start(&mut self, read: bool) {
        let Some(w) = &self.window else {
            return;
        };
        if self.receiver.is_some() {
            return;
        }
        let parse = |s: slint::SharedString| {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                s.trim().parse::<u64>().map(Some).map_err(|_| ())
            }
        };
        let (Ok(min_bytes), Ok(max_bytes)) = (parse(w.get_min_filter()), parse(w.get_max_filter()))
        else {
            self.reject(Notice::InvalidBytes);
            return;
        };
        let text = |s: slint::SharedString| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };
        let query = SearchQuery {
            name: text(w.get_name_filter()),
            path: text(w.get_path_filter()),
            extension: text(w.get_extension_filter()),
            glob: text(w.get_glob_filter()),
            min_bytes,
            max_bytes,
            case_sensitive: w.get_case_sensitive(),
            ..Default::default()
        };
        if min_bytes.zip(max_bytes).is_some_and(|(a, b)| a > b) {
            self.reject(Notice::InvalidRange);
            return;
        }
        if analysis::search(
            &AnalysisSnapshot {
                entries: vec![],
                scan_coverage: sortsize_core::Coverage::Complete,
                entries_omitted: 0,
            },
            &query,
            1,
            &Default::default(),
        )
        .is_err()
        {
            self.reject(Notice::InvalidQuery);
            return;
        }
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        self.result = None;
        w.set_results(Default::default());
        w.set_statistics(Default::default());
        w.set_page_label("".into());
        w.set_result_note("".into());
        w.set_diagnostic_text("".into());
        w.set_empty_message("".into());
        w.set_errors_visible(false);
        w.set_duplicates_visible(false);
        w.set_has_previous(false);
        w.set_has_next(false);
        self.cancellation = CancellationToken::default();
        let cancel = self.cancellation.clone();
        self.progress.store(0, Ordering::Relaxed);
        self.files_hashed.store(0, Ordering::Relaxed);
        let files_hashed = self.files_hashed.clone();
        let progress = self.progress.clone();
        self.reading = read;
        self.notice = Some(if read {
            Notice::Reading
        } else {
            Notice::Searching
        });
        let (tx, rx) = mpsc::sync_channel(1);
        self.receiver = Some(rx);
        w.set_busy(true);
        w.set_status(self.notice.unwrap().label().into());
        std::thread::spawn(move || {
            let result = (|| {
                let search =
                    analysis::search(&snapshot, &query, analysis::MAX_ANALYSIS_ENTRIES, &cancel)?;
                let duplicates = if read {
                    Some(duplicates::find(
                        &snapshot,
                        &search,
                        Default::default(),
                        &cancel,
                        |p| {
                            progress.store(p.bytes_read, Ordering::Relaxed);
                            files_hashed.store(p.files_hashed, Ordering::Relaxed);
                        },
                    )?)
                } else {
                    None
                };
                Ok(Outcome { search, duplicates })
            })();
            let _ = tx.send(result);
        });
    }
    fn reject(&mut self, notice: Notice) {
        self.notice = Some(notice);
        self.result = None;
        self.order.clear();
        self.duplicate_labels.clear();
        self.offset = 0;
        if let Some(w) = &self.window {
            w.set_status(notice.label().into());
            w.set_results(Default::default());
            w.set_statistics(Default::default());
            w.set_result_note("".into());
            w.set_diagnostic_text("".into());
            w.set_empty_message("".into());
            w.set_page_label("".into());
            w.set_has_previous(false);
            w.set_has_next(false);
            w.set_selected_id(-1);
        }
    }
    fn cancel(&mut self) {
        self.cancellation.cancel();
        if self.receiver.is_some() {
            self.notice = Some(Notice::Cancelling);
            if let Some(w) = &self.window {
                w.set_status(Notice::Cancelling.label().into());
            }
        }
    }
    fn prepare_order(&mut self, keep_selection: bool) {
        let (Some(w), Some(snapshot), Some(outcome)) = (&self.window, &self.snapshot, &self.result)
        else {
            return;
        };
        self.duplicate_labels.clear();
        w.set_duplicates_visible(outcome.duplicates.is_some());
        if let Some(d) = &outcome.duplicates {
            self.order = d
                .groups
                .iter()
                .enumerate()
                .flat_map(|(group, g)| {
                    g.members.iter().enumerate().flat_map(move |(copy, m)| {
                        m.entry_indices
                            .iter()
                            .enumerate()
                            .map(move |(alias, &i)| (i, group + 1, copy + 1, alias > 0))
                    })
                })
                .map(|(i, group, copy, alias)| {
                    self.duplicate_labels.insert(i, (group, copy, alias));
                    i
                })
                .collect();
            self.offset = 0;
            if keep_selection
                && let Some(position) = self
                    .order
                    .iter()
                    .position(|&id| id as i32 == w.get_selected_id())
            {
                self.offset = position / PAGE_ROWS * PAGE_ROWS;
            } else {
                w.set_selected_id(-1);
            }
            return;
        }
        self.order = outcome.search.indices.clone();
        let sort = w.get_sort_order();
        self.order.sort_unstable_by(|&a, &b| {
            let (a, b) = (&snapshot.entries[a], &snapshot.entries[b]);
            let by_size = match (a.logical_bytes, b.logical_bytes) {
                (Some(a), Some(b)) if sort == 2 => b.cmp(&a),
                (Some(a), Some(b)) => a.cmp(&b),
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            };
            if sort == 0 {
                a.path.cmp(&b.path)
            } else {
                by_size.then_with(|| a.path.cmp(&b.path))
            }
        });
        self.offset = 0;
        if keep_selection
            && let Some(position) = self
                .order
                .iter()
                .position(|&id| id as i32 == w.get_selected_id())
        {
            self.offset = position / PAGE_ROWS * PAGE_ROWS;
        } else {
            w.set_selected_id(-1);
        }
    }
    fn page(&mut self, next: bool, context: presentation::Context) {
        if self.receiver.is_some() {
            return;
        }
        if next && self.offset + PAGE_ROWS < self.order.len() {
            self.offset += PAGE_ROWS;
        }
        if !next {
            self.offset = self.offset.saturating_sub(PAGE_ROWS);
        }
        self.retranslate(context);
    }
    fn show_duplicate_details(&self) {
        let Some(w) = &self.window else {
            return;
        };
        let Some(d) = self.result.as_ref().and_then(|r| r.duplicates.as_ref()) else {
            w.set_result_note("".into());
            w.set_diagnostic_text("".into());
            return;
        };
        let mut note = tr!(
            "SHA-256 candidates are compared byte for byte. Hardlink aliases are the same file."
        )
        .to_owned();
        if let Some(&(group, _, _)) = usize::try_from(w.get_selected_id())
            .ok()
            .and_then(|i| self.duplicate_labels.get(&i))
        {
            let g = &d.groups[group - 1];
            note = tr_format!(
                "Group {} · independent copies: {} · SHA-256: {} · byte-for-byte verified.",
                crate::number_format::integer(group as u128),
                crate::number_format::integer(g.members.len() as u128),
                &g.sha256
            );
        }
        if d.budget_limited {
            note.push(' ');
            note.push_str(tr!(
                "Read budget or file-size limit reached. Results are incomplete."
            ));
        }
        if d.cancelled {
            note.push(' ');
            note.push_str(tr!("Cancelled. Only finished groups are shown."));
        }
        w.set_result_note(note.into());
        w.set_diagnostic_text(
            d.errors
                .iter()
                .map(|e| {
                    format!(
                        "{}: {}",
                        e.path.display.escape_debug(),
                        read_error(e.error_kind)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        );
    }
    pub(crate) fn set_units(&mut self, units: crate::units::Units, context: presentation::Context) {
        self.units = units;
        self.retranslate(context);
    }
    pub(crate) fn retranslate(&self, context: presentation::Context) {
        let _scope = context.enter();
        let (Some(w), Some(snapshot), Some(outcome)) = (&self.window, &self.snapshot, &self.result)
        else {
            if let (Some(w), Some(notice)) = (&self.window, self.notice) {
                w.set_status(notice.label().into());
            }
            return;
        };
        let result = &outcome.search;
        let integer = |n: usize| crate::number_format::integer(n as u128);
        w.set_status(tr_format!("Matches: {} · retained results: {} · omitted inventory entries: {} · unknown dates: {}. Logical file sizes per path.",integer(result.matching_entries),integer(result.indices.len()),crate::number_format::integer(result.scope.omitted_inventory_entries as u128),integer(result.unknown_dates)).into());
        let rows = self
            .order
            .iter()
            .skip(self.offset)
            .take(PAGE_ROWS)
            .map(|&i| {
                let e = &snapshot.entries[i];
                AnalysisRow {
                    id: i as i32,
                    label: if let Some(&(group, copy, alias)) = self.duplicate_labels.get(&i) {
                        format!(
                            "{} {}{}",
                            tr_format!("Group {} · copy {} ·", integer(group), integer(copy)),
                            if alias {
                                format!("{} · ", tr!("Hardlink alias"))
                            } else {
                                String::new()
                            },
                            tree::native_label(&e.path)
                        )
                        .into()
                    } else {
                        tree::native_label(&e.path).into()
                    },
                    size: e
                        .logical_bytes
                        .map_or_else(|| "—".into(), |n| self.units.format(n))
                        .into(),
                    fraction: 0.0,
                }
            })
            .collect::<Vec<_>>();
        if let Some(d) = &outcome.duplicates {
            w.set_status(
                tr_format!(
                    "Exact groups: {} · bytes read: {} · hardlink aliases: {} · errors: {}.",
                    integer(d.groups.len()),
                    crate::number_format::integer(d.bytes_read as u128),
                    crate::number_format::integer(d.hardlink_aliases as u128),
                    crate::number_format::integer(d.errors_count as u128)
                )
                .into(),
            );
            if !d.complete {
                w.set_status(format!("{} {}", tr!("Incomplete coverage"), w.get_status()).into());
            }
        } else if !result.scope.exhaustive || result.cancelled {
            w.set_status(format!("{} {}", tr!("Incomplete coverage"), w.get_status()).into());
        }
        if result.cancelled && outcome.duplicates.is_none() {
            w.set_status(
                format!(
                    "{} {}",
                    tr!("Cancelled. Only observed matches are shown."),
                    w.get_status()
                )
                .into(),
            );
        }
        if result.statistics.totals.overflow
            || result.statistics.totals.unknown_sizes > 0
            || result.unknown_sizes > 0
            || result.unsearchable_native_text > 0
        {
            w.set_status(
                format!("{} {}", tr!("Some values are unavailable"), w.get_status()).into(),
            );
        }
        w.set_has_previous(self.offset > 0);
        w.set_has_next(self.offset + PAGE_ROWS < self.order.len());
        w.set_page_label(
            tr_format!(
                "Rows {}–{} of {}",
                integer(if rows.is_empty() { 0 } else { self.offset + 1 }),
                integer(self.offset + rows.len()),
                integer(self.order.len())
            )
            .into(),
        );
        w.set_empty_message(
            if rows.is_empty() {
                if outcome.duplicates.is_some() {
                    tr!("No exact duplicate group in the examined files.")
                } else {
                    tr!("No matching retained entry.")
                }
            } else {
                ""
            }
            .into(),
        );
        self.show_duplicate_details();
        w.set_results(Rc::new(VecModel::from(rows)).into());
        let statistics = &result.statistics;
        let total = statistics.totals.logical_bytes;
        let count_label = |counts: &analysis::Counts| {
            format!(
                "{} · {}{}",
                crate::number_format::integer(counts.files.into()),
                if counts.unknown_sizes > 0 && !counts.overflow {
                    "≥"
                } else {
                    ""
                },
                if counts.overflow {
                    "—".into()
                } else {
                    self.units.format(counts.logical_bytes)
                }
            )
        };
        w.set_scope_note(
            format!(
                "{} {}",
                if result.scope.exhaustive {
                    tr!("Retained inventory")
                } else {
                    tr!("Incomplete coverage")
                },
                tr_format!(
                    "Retained entries: {} · omitted: {}. Filters never rescan the source.",
                    integer(result.scope.retained_entries),
                    crate::number_format::integer(result.scope.omitted_inventory_entries.into())
                )
            )
            .into(),
        );
        w.set_statistics_note(tr_format!("Counts · known logical sizes per path. Unknown file sizes: {}. Hardlinks count as paths.", crate::number_format::integer(statistics.totals.unknown_sizes.into())).into());
        let mut extensions = statistics.extensions.iter().collect::<Vec<_>>();
        extensions.sort_by(|a, b| {
            b.counts
                .logical_bytes
                .cmp(&a.counts.logical_bytes)
                .then_with(|| a.extension.display.cmp(&b.extension.display))
        });
        let mut rows = vec![
            AnalysisRow {
                id: -1,
                label: tr!("All matched files").into(),
                size: count_label(&statistics.totals).into(),
                fraction: 0.0,
            },
            AnalysisRow {
                id: -1,
                label: tr!("Extensions: largest 32 · bars: known bytes").into(),
                ..Default::default()
            },
        ];
        rows.extend(extensions.iter().take(32).map(|e| AnalysisRow {
            id: -1,
            label: if let Some(raw) = &e.extension.raw {
                let units = match raw {
                    sortsize_core::RawPath::UnixBytes(units) => units
                        .iter()
                        .map(|v| format!("{v:02x}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    sortsize_core::RawPath::WindowsUtf16(units) => units
                        .iter()
                        .map(|v| format!("{v:04x}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                format!("{} ({})", tr!("Non-Unicode extension"), units).into()
            } else if e.extension.display.is_empty() {
                tr!("No extension").into()
            } else {
                e.extension.display.escape_debug().to_string().into()
            },
            size: count_label(&e.counts).into(),
            fraction: if total == 0 || statistics.totals.overflow || e.counts.overflow {
                0.0
            } else {
                (e.counts.logical_bytes as f64 / total as f64).clamp(0.0, 1.0) as f32
            },
        }));
        if extensions.len() > 32 {
            rows.push(AnalysisRow {
                id: -1,
                label: tr_format!(
                    "Other extension groups: {} (included in totals)",
                    integer(extensions.len() - 32)
                )
                .into(),
                ..Default::default()
            });
        }
        rows.push(AnalysisRow {
            id: -1,
            label: tr!("Size classes · bars: file counts").into(),
            ..Default::default()
        });
        let labels = [
            tr!("Empty files"),
            tr!("1–1023 bytes"),
            tr!("1 KiB–<1 MiB"),
            tr!("1 MiB–<1 GiB"),
            tr!("At least 1 GiB"),
            tr!("Unknown size"),
        ];
        rows.extend(
            statistics
                .size_classes
                .iter()
                .zip(labels)
                .map(|(counts, label)| AnalysisRow {
                    id: -1,
                    label: label.into(),
                    size: count_label(counts).into(),
                    fraction: if statistics.totals.files == 0 {
                        0.0
                    } else {
                        counts.files as f32 / statistics.totals.files as f32
                    },
                }),
        );
        w.set_statistics(Rc::new(VecModel::from(rows)).into());
        if let Some(notice) = self.notice {
            w.set_status(notice.label().into());
        }
    }

    pub(crate) fn poll(&mut self, context: presentation::Context) {
        let _scope = context.enter();
        let Some(receiver) = &self.receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
                if matches!(self.notice, Some(Notice::Cancelling | Notice::Waiting)) {
                    if let Some(w) = &self.window {
                        w.set_status(self.notice.unwrap().label().into());
                    }
                    return;
                }
                if self.reading
                    && let Some(w) = &self.window
                {
                    w.set_status(
                        tr_format!(
                            "Reading contents: {} bytes · {} files hashed",
                            crate::number_format::integer(
                                self.progress.load(Ordering::Relaxed) as u128
                            ),
                            crate::number_format::integer(
                                self.files_hashed.load(Ordering::Relaxed) as u128
                            )
                        )
                        .into(),
                    );
                }
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.notice = Some(Notice::WorkerStopped);
                Err(String::new())
            }
        };
        self.receiver = None;
        let Some(w) = &self.window else {
            return;
        };
        w.set_busy(false);
        if self.snapshot.is_none() {
            return;
        }
        match result {
            Err(_) => {
                self.notice.get_or_insert(Notice::WorkerStopped);
                w.set_status(self.notice.unwrap().label().into());
            }
            Ok(result) => {
                self.notice = None;
                self.result = Some(result);
                self.prepare_order(true);
                self.retranslate(context);
            }
        }
    }
}
pub(crate) fn connect(window: &SortSizeWindow, state: &Rc<RefCell<Controller>>) {
    let weak = window.as_weak();
    let owner = Rc::downgrade(state);
    window.on_show_analysis(move || {
        let (Some(window), Some(owner)) = (weak.upgrade(), owner.upgrade()) else {
            return;
        };
        if window.get_busy() {
            return;
        }
        let mut state = owner.borrow_mut();
        let _scope = state.presentation.enter();
        if state.analysis.receiver.is_some() {
            if let Some(panel) = &state.analysis.window {
                let _ = panel.show();
            }
            state.analysis.retranslate(state.presentation);
            return;
        }
        let Some(tree) = &state.tree else {
            return;
        };
        let snapshot = Arc::new(tree.analysis_snapshot());
        if state.analysis.window.is_none() {
            let Ok(panel) = AnalysisWindow::new() else {
                return;
            };
            let weak_owner = Rc::downgrade(&owner);
            panel.on_search(move || {
                if let Some(owner) = weak_owner.upgrade() {
                    let mut s = owner.borrow_mut();
                    let _scope = s.presentation.enter();
                    s.analysis.start(false);
                }
            });
            let weak_owner = Rc::downgrade(&owner);
            panel.on_duplicates(move || {
                if let Some(owner) = weak_owner.upgrade() {
                    let mut s = owner.borrow_mut();
                    let _scope = s.presentation.enter();
                    s.analysis.start(true);
                }
            });
            let weak_owner = Rc::downgrade(&owner);
            panel.on_change_page(move |next| {
                if let Some(owner) = weak_owner.upgrade() {
                    let mut s = owner.borrow_mut();
                    let context = s.presentation;
                    s.analysis.page(next, context);
                }
            });
            let weak_owner = Rc::downgrade(&owner);
            panel.on_change_sort(move || {
                if let Some(owner) = weak_owner.upgrade() {
                    let mut s = owner.borrow_mut();
                    let context = s.presentation;
                    s.analysis.prepare_order(true);
                    s.analysis.retranslate(context);
                }
            });
            let weak_owner = Rc::downgrade(&owner);
            panel.on_cancel(move || {
                if let Some(owner) = weak_owner.upgrade() {
                    let mut state = owner.borrow_mut();
                    let _scope = state.presentation.enter();
                    state.analysis.cancel();
                }
            });
            let weak_owner = Rc::downgrade(&owner);
            let weak_window = window.as_weak();
            panel.on_reveal(move |id| {
                if id >= 0
                    && let (Some(owner), Some(window)) =
                        (weak_owner.upgrade(), weak_window.upgrade())
                {
                    let mut s = owner.borrow_mut();
                    let _scope = s.presentation.enter();
                    if !window.get_busy()
                        && s.analysis.receiver.is_none()
                        && s.analysis
                            .window
                            .as_ref()
                            .is_some_and(|p| p.get_results().iter().any(|row| row.id == id))
                    {
                        let path = s
                            .analysis
                            .snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.entries.get(id as usize))
                            .map(|entry| entry.path.clone());
                        let target = path.and_then(|path| {
                            let current = s
                                .tree
                                .as_mut()
                                .and_then(|tree| tree.select_analysis_path(&path));
                            if current.is_some() {
                                return current;
                            }
                            let original = s
                                .directory
                                .original
                                .as_mut()
                                .and_then(|tree| tree.select_analysis_path(&path));
                            if original.is_some() {
                                s.restore_primary(&window);
                            }
                            original
                        });
                        if let Some(target) = target {
                            s.show_rows(&window);
                            s.inspect(&window, target);
                            if let Some(cursor) = window
                                .get_tiles()
                                .iter()
                                .position(|tile| tile.node_id == target as i32)
                            {
                                window.set_map_cursor(cursor as i32);
                            }
                            s.analysis.notice = None;
                            if let Some(panel) = &s.analysis.window {
                                panel.set_selected_id(id);
                                s.analysis.show_duplicate_details();
                            }
                        } else if let Some(panel) = &s.analysis.window {
                            panel.set_status(Notice::Unavailable.label().into());
                            s.analysis.notice = Some(Notice::Unavailable);
                        }
                    }
                }
            });
            let weak_owner = Rc::downgrade(&owner);
            panel.window().on_close_requested(move || {
                if let Some(owner) = weak_owner.upgrade() {
                    let mut state = owner.borrow_mut();
                    let _scope = state.presentation.enter();
                    state.analysis.cancel();
                }
                slint::CloseRequestResponse::HideWindow
            });
            state.analysis.window = Some(panel);
            let _ = crate::locale::activate_bundled(state.presentation.language);
        }
        if let Some(panel) = &state.analysis.window {
            panel.set_source_path(
                snapshot
                    .entries
                    .first()
                    .map_or_else(String::new, |e| tree::native_label(&e.path))
                    .into(),
            );
        }
        state.analysis.units = state.preferences.units;
        state.analysis.snapshot = Some(snapshot);
        if let Some(panel) = &state.analysis.window {
            let _ = panel.show();
        }
        state.analysis.start(false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use sortsize_core::{Coverage, InventoryKind, analysis::AnalysisEntry};
    struct Platform;
    impl slint::platform::Platform for Platform {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }
    #[test]
    fn statistics_show_classes_unknowns_partial_scope_units_and_overflow() {
        slint::platform::set_platform(Box::new(Platform)).unwrap();
        let context = presentation::Context {
            language: crate::locale::Language::English,
            generation: 0,
        };
        let _scope = context.enter();
        let snapshot = Arc::new(AnalysisSnapshot {
            entries: [Some(999), Some(0), Some(1000), None, Some(1_073_741_824)]
                .into_iter()
                .enumerate()
                .map(|(i, bytes)| AnalysisEntry {
                    path: Arc::new(format!("root/{i}.txt").into()),
                    kind: if i == 0 {
                        InventoryKind::Directory
                    } else {
                        InventoryKind::File
                    },
                    logical_bytes: bytes,
                    modified: None,
                })
                .collect(),
            scan_coverage: Coverage::Partial,
            entries_omitted: 42,
        });
        let search = analysis::search(
            &snapshot,
            &SearchQuery::default(),
            50_000,
            &Default::default(),
        )
        .unwrap();
        let window = AnalysisWindow::new().unwrap();
        let mut panel = Panel {
            window: Some(window.clone_strong()),
            snapshot: Some(snapshot),
            result: Some(Outcome {
                search,
                duplicates: None,
            }),
            units: crate::units::Units::Decimal,
            ..Default::default()
        };
        panel.prepare_order(false);
        panel.retranslate(context);
        assert!(window.get_scope_note().contains("Incomplete coverage"));
        assert!(window.get_scope_note().contains("42"));
        assert!(
            window
                .get_statistics_note()
                .contains("Unknown file sizes: 1")
        );
        let rows = window.get_statistics();
        assert!(rows.row_data(0).unwrap().size.starts_with("4 · ≥"));
        assert!(
            rows.iter()
                .any(|r| r.label == "1–1023 bytes" && r.size == "1 · 1.0 kB")
        );
        assert!(
            rows.iter()
                .any(|r| r.label == "Empty files" && r.size.starts_with("1 · 0"))
        );
        assert!(
            rows.iter()
                .all(|r| r.fraction.is_finite() && (0.0..=1.0).contains(&r.fraction))
        );
        panel
            .result
            .as_mut()
            .unwrap()
            .search
            .statistics
            .totals
            .overflow = true;
        panel.retranslate(context);
        assert_eq!(window.get_statistics().row_data(0).unwrap().size, "4 · —");
        assert_eq!(window.get_statistics().row_data(2).unwrap().fraction, 0.0);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn invalidation_cancels_without_joining_and_discards_a_late_result() {
        struct Platform;
        impl slint::platform::Platform for Platform {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(
                    slint::platform::software_renderer::MinimalSoftwareWindow::new(
                        slint::platform::software_renderer::RepaintBufferType::NewBuffer,
                    ),
                )
            }
        }
        slint::platform::set_platform(Box::new(Platform)).unwrap();
        let window = AnalysisWindow::new().unwrap();
        let snapshot = Arc::new(AnalysisSnapshot {
            entries: vec![],
            scan_coverage: sortsize_core::Coverage::Complete,
            entries_omitted: 0,
        });
        let search =
            analysis::search(&snapshot, &Default::default(), 1, &Default::default()).unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut panel = Panel {
            window: Some(window.clone_strong()),
            snapshot: Some(snapshot),
            receiver: Some(receiver),
            ..Default::default()
        };
        window.set_busy(true);
        panel.cancel();
        assert!(panel.cancellation.is_cancelled());
        panel.invalidate();
        assert!(panel.receiver.is_some()); // Do not admit another blocked content worker.
        panel.poll(Default::default()); // An empty channel must return immediately.
        sender
            .send(Ok(Outcome {
                search,
                duplicates: None,
            }))
            .unwrap();
        panel.poll(Default::default());
        assert!(panel.result.is_none());
        assert_eq!(window.get_results().row_count(), 0);
        assert!(!window.get_busy());
    }
}
