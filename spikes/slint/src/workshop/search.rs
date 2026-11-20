//! One replaceable explicit query on the certified read owner.
//! Its immutable generation stays pinned while the independent acquisition owner advances.
use super::*;
use service::Lane;
pub(super) struct Frozen<'a> {
    pub store: &'a sortsize_store::inventory::Store,
    pub certified: Option<&'a sortsize_store::inventory::generation_a::CertifiedGeneration>,
    pub c: AcquisitionContext,
    pub root: &'a Path,
    pub scratch: &'a Path,
    pub started: std::time::SystemTime,
    pub sealed: std::time::SystemTime,
}

use sortsize_core::{
    CancellationToken,
    analysis::SearchQuery,
    resources::{Category, Governor, Reservation},
};
use sortsize_store::inventory::queries::{Query, Row, duplicates::DuplicateFileProfile};
use std::time::{Duration, Instant};

pub(super) const MODES: [MemoryMode; 6] = [
    MemoryMode::Auto,
    MemoryMode::Entries50k,
    MemoryMode::Entries250k,
    MemoryMode::Entries1m,
    MemoryMode::Entries5m,
    MemoryMode::UnlimitedCount,
];
pub(super) struct Request {
    pub id: u64,
    pub query: SearchQuery,
    pub cancel: CancellationToken,
}
pub(super) struct Prepared {
    pub id: u64,
    #[cfg(feature = "benchmarks")]
    pub counts: sortsize_store::inventory::queries::QueryCounts,
    #[cfg(feature = "benchmarks")]
    pub indexed: bool,
    #[cfg(feature = "benchmarks")]
    pub predicate_evaluated: u64,
    #[cfg(feature = "benchmarks")]
    pub source_context: Option<AcquisitionContext>,
    pub presentation: crate::presentation::Context,
    pub token: i32,
    pub rows: Vec<Row>,
    pub model: Vec<crate::AnalysisRow>,
    pub status: String,
    pub scope: String,
    pub statistics: String,
    pub busy: bool,
    pub previous: bool,
    pub next: bool,
    pub complete: bool,
    pub matches: u64,
    pub duplicates: bool,
    pub duplicate_group_first: Vec<bool>,
    #[allow(dead_code)] // Native rendered qualification receipt.
    pub duplicate_groups: Option<u64>,
    pub sort_index: i32,
    pub _memory: Option<Reservation>,
}
impl Prepared {
    pub(super) fn error(id: u64, message: String) -> Self {
        Self {
            id,
            #[cfg(feature = "benchmarks")]
            counts: Default::default(),
            #[cfg(feature = "benchmarks")]
            indexed: false,
            #[cfg(feature = "benchmarks")]
            predicate_evaluated: 0,
            #[cfg(feature = "benchmarks")]
            source_context: None,
            presentation: crate::presentation::Context::current(),
            token: 0,
            rows: vec![],
            model: vec![],
            status: message,
            scope: String::new(),
            statistics: String::new(),
            busy: false,
            previous: false,
            next: false,
            complete: false,
            matches: 0,
            duplicates: false,
            duplicate_group_first: Vec::new(),
            duplicate_groups: None,
            sort_index: 0,
            _memory: None,
        }
    }
}
#[derive(Default)]
pub(super) struct Meter {
    last: Option<Instant>,
    mode: Option<MemoryMode>,
    presentation: Option<crate::presentation::Context>,
}
impl Meter {
    pub fn update(
        &mut self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<(crate::presentation::Context, String)>>,
        units: crate::units::Units,
    ) {
        let mode = lane.lock().unwrap().memory;
        let changed = self.mode != Some(mode);
        let presentation = crate::presentation::Context::current();
        let translated = self.presentation != Some(presentation);
        self.presentation = Some(presentation);
        if changed {
            Governor::global().configure(mode, sortsize_core::resources::native_memory());
            self.mode = Some(mode);
        }
        if changed
            || translated
            || self
                .last
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
        {
            let u = Governor::global().usage();
            let count = u
                .limits
                .entries
                .map_or_else(|| tr!("Unlimited count").to_owned(), |n| n.to_string());
            *output.lock().unwrap() = Some((
                presentation,
                format!(
                    "{} {} · {} {} / {} · {}",
                    tr!("Effective entry ceiling:"),
                    count,
                    tr!("Accounted memory:"),
                    units.format(u.categories.iter().sum()),
                    units.format(u.limits.bytes),
                    tr!("Not process RSS; byte limits always apply.")
                ),
            ));
            self.last = Some(Instant::now());
        }
    }
}
pub(super) fn token() -> sortsize_store::inventory::Result<i32> {
    use std::sync::atomic::{AtomicI32, Ordering};
    static TOKEN: AtomicI32 = AtomicI32::new(0);
    TOKEN
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map(|n| n + 1)
        .map_err(|_| sortsize_store::inventory::Error::Contract("search token exhausted"))
}
fn scope(a: &Frozen<'_>) -> String {
    let date = |t| {
        crate::date_label::format_date(sortsize_core::MetadataValue::Known {
            value: t,
            accuracy: sortsize_core::ValueAccuracy::Exact,
        })
    };
    format!(
        "{} {} · {}\n{} → {}",
        tr!("Frozen inventory, generation"),
        a.c.generation.0,
        short_label(a.root),
        date(a.started),
        date(a.sealed)
    )
}
fn prepare(
    a: &Frozen<'_>,
    id: u64,
    q: &Query<'_>,
    rows: Vec<Row>,
    pages: (bool, bool),
    statistics: &str,
    units: crate::units::Units,
) -> sortsize_store::inventory::Result<Prepared> {
    let longest = rows
        .iter()
        .map(|r| r.path.as_os_str().as_encoded_bytes().len())
        .max()
        .unwrap_or(0);
    let bytes = 64 * 1024
        + longest * 8
        + rows.capacity() * std::mem::size_of::<Row>()
        + rows
            .iter()
            .map(|r| {
                let n = r.path.as_os_str().as_encoded_bytes().len();
                n * 2 + 2 * (n * 4).min(2048) + 512
            })
            .sum::<usize>();
    let memory = Governor::global()
        .reserve(Category::Publications, bytes as u64, None)
        .ok_or(sortsize_store::inventory::Error::Contract(
            "search publication memory unavailable",
        ))?;
    let done = q.evaluation_finished();
    let mut status = format!(
        "{} · {} / {}",
        if !done {
            if q.indexed() {
                tr!("Preparing the search results…")
            } else {
                tr!("Searching the entire inventory…")
            }
        } else if q.counts.cancelled {
            tr!("Cancelled")
        } else if q.complete() {
            tr!("Complete")
        } else {
            tr!("Partial")
        },
        q.counts.matches,
        q.counts.evaluated
    );
    if done && q.counts.matches == 0 {
        status.push_str(if q.complete() {
            tr!(" · No match in this inventory.")
        } else {
            tr!(" · No match observed; coverage is partial.")
        });
    }
    status.push_str(&format!(
        " · {} {} / {} / {} / {} / {}",
        tr!("Gaps (size/date/text/path/kind):"),
        q.counts.unknown_sizes,
        q.counts.unknown_dates,
        q.counts.native_text,
        q.counts.oversized_paths,
        q.counts.unknown_kinds
    ));
    let model = rows
        .iter()
        .enumerate()
        .map(|(i, r)| crate::AnalysisRow {
            id: i as i32,
            label: short_label(&r.path).into(),
            size: r
                .logical
                .map_or_else(|| "—".into(), |n| units.format(n))
                .into(),
            fraction: 0.,
        })
        .collect();
    Ok(Prepared {
        id,
        #[cfg(feature = "benchmarks")]
        counts: q.counts,
        #[cfg(feature = "benchmarks")]
        indexed: q.indexed(),
        #[cfg(feature = "benchmarks")]
        predicate_evaluated: q.predicate_evaluated(),
        #[cfg(feature = "benchmarks")]
        source_context: Some(a.c),
        presentation: crate::presentation::Context::current(),
        token: token()?,
        rows,
        model,
        status,
        scope: scope(a),
        statistics: statistics.into(),
        busy: !done,
        previous: pages.0,
        next: pages.1,
        complete: q.complete(),
        matches: q.counts.matches,
        duplicates: false,
        duplicate_group_first: Vec::new(),
        duplicate_groups: None,
        sort_index: 0,
        _memory: Some(memory),
    })
}
fn statistics_text(report: &sortsize_store::inventory::queries::reports::Statistics) -> String {
    // Whole-query logical file totals, not a reduction of the visible page.
    let mut statistics = format!(
        "{}: {} · {}: {} · {}: {}\n{}: {}",
        tr!("Files"),
        report.totals.files,
        tr!("Known logical bytes"),
        report.totals.known_bytes,
        tr!("Unknown sizes"),
        report.totals.unknown_sizes,
        tr!("Size classes (0 / <1 KiB / <1 MiB / <1 GiB / ≥1 GiB / unknown)"),
        report.size_classes.map(|c| c.files.to_string()).join(" / ")
    );
    statistics.push_str(if report.complete {
        tr!(" · Complete")
    } else {
        tr!(" · Partial")
    });
    for group in &report.extensions {
        let extension: String = group.extension.display.chars().take(512).collect();
        statistics.push_str(&format!(
            "\n{}: {} / {}",
            extension, group.counts.files, group.counts.known_bytes
        ));
    }
    statistics.push_str(&format!(
        "\n{}: {} / {}",
        tr!("Other extensions"),
        report.others.files,
        report.others.known_bytes
    ));

    statistics
}
pub(super) fn run(
    a: &Frozen<'_>,
    request: Request,
    lane: &Mutex<Lane>,
    output: &Mutex<Option<Prepared>>,
    resources: &Mutex<Option<(crate::presentation::Context, String)>>,
) {
    run_mode(a, request, lane, output, resources, RunMode::default());
}
#[derive(Clone, Copy, Default)]
struct RunMode {
    initial_duplicates: Option<DuplicateFileProfile>,
    epoch: Option<(u64, u64)>,
}
impl RunMode {
    fn publish(
        &self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<Prepared>>,
        page: Prepared,
    ) -> bool {
        let l = lane.lock().unwrap();
        if self.superseded(&l, page.id) {
            return false;
        }
        *output.lock().unwrap() = Some(page);
        true
    }
    fn superseded(&self, lane: &Lane, id: u64) -> bool {
        superseded(lane, id)
            || self
                .epoch
                .is_some_and(|epoch| epoch != (lane.source_epoch, lane.view_epoch))
    }
}
pub(super) fn run_explicit_duplicates(
    a: &Frozen<'_>,
    request: Request,
    lane: &Mutex<Lane>,
    output: &Mutex<Option<Prepared>>,
    resources: &Mutex<Option<(crate::presentation::Context, String)>>,
    epoch: (u64, u64),
    profile: DuplicateFileProfile,
) {
    run_mode(
        a,
        request,
        lane,
        output,
        resources,
        RunMode {
            initial_duplicates: Some(profile),
            epoch: Some(epoch),
        },
    );
}
fn run_mode(
    a: &Frozen<'_>,
    request: Request,
    lane: &Mutex<Lane>,
    output: &Mutex<Option<Prepared>>,
    resources: &Mutex<Option<(crate::presentation::Context, String)>>,
    mode: RunMode,
) {
    {
        let mut l = lane.lock().unwrap();
        if mode.superseded(&l, request.id) {
            return;
        }
        l.query_cancel = Some(request.cancel.clone());
    }
    let result = (|| -> sortsize_store::inventory::Result<()> {
        let scratch = a.scratch;
        let _pages = Governor::global()
            .reserve(Category::Queries, 1024 * 1024, None)
            .ok_or(sortsize_store::inventory::Error::Contract(
                "search page memory unavailable",
            ))?;
        let mut q = if let Some(certified) = a.certified {
            certified.start_query(&request.query, scratch, 512 * 1024 * 1024)?
        } else {
            a.store.start_query(
                a.c.generation,
                a.root,
                &request.query,
                scratch,
                512 * 1024 * 1024,
            )?
        };
        let mut meter = Meter::default();
        let mut last: Option<Instant> = None;
        while !q.evaluation_finished() {
            let (units, presentation) = {
                let l = lane.lock().unwrap();
                if mode.superseded(&l, request.id) {
                    return Ok(());
                }
                (l.units, l.presentation)
            };
            let _scope = presentation.enter();
            meter.update(lane, resources, units);
            q.advance(&request.cancel)?;
            // A completed preview has no stable paging flags. Only results()
            // may publish the terminal page, including partial terminal queries.
            if !q.evaluation_finished()
                && last.is_none_or(|t| t.elapsed() >= Duration::from_millis(100))
            {
                let prepared = prepare(
                    a,
                    request.id,
                    &q,
                    q.preview()?.rows,
                    (false, false),
                    "",
                    units,
                )?;
                if !mode.publish(lane, output, prepared) {
                    return Ok(());
                }
                service::notify(lane);
                last = Some(Instant::now());
            }
            std::thread::yield_now();
        }
        results(a, &request, &q, lane, output, resources, mode)
    })();
    if let Err(e) = result
        && mode.publish(lane, output, Prepared::error(request.id, e.to_string()))
    {
        service::notify(lane);
    }
}
fn results(
    a: &Frozen<'_>,
    request: &Request,
    q: &Query<'_>,
    lane: &Mutex<Lane>,
    output: &Mutex<Option<Prepared>>,
    resources: &Mutex<Option<(crate::presentation::Context, String)>>,
    mut mode: RunMode,
) -> sortsize_store::inventory::Result<()> {
    use sortsize_store::inventory::queries::duplicates::DuplicateJob;
    let mut meter = Meter::default();
    let mut page = q.page(None)?;
    let mut current_token = 0;
    let mut sort_index = 0;
    let mut statistics = None;
    let mut error = None;
    let mut duplicates: Option<DuplicateJob<'_, '_>> = None;
    let mut duplicate_page = None;
    let mut duplicate_profile = None;
    let mut force = true;
    let mut order: Option<sortsize_store::inventory::queries::ordering::SizeOrder<'_, '_>> = None;
    loop {
        let (next, stats, dupes, sort, refresh, units, presentation) = {
            let mut l = lane.lock().unwrap();
            if mode.superseded(&l, request.id) {
                return Ok(());
            }
            (
                l.search_page.take(),
                l.search_statistics.take(),
                l.search_duplicates.take(),
                l.search_sort.take(),
                std::mem::take(&mut l.search_refresh),
                l.units,
                l.presentation,
            )
        };
        let _scope = presentation.enter();
        meter.update(lane, resources, units);
        force |= refresh;
        if let Some((_, index)) = sort.filter(|(t, _)| *t == current_token && duplicates.is_none())
        {
            // Build beside the current immutable view under the same workspace
            // quota. Errors and cancellation never discard the previous order.
            let attempt = (|| -> sortsize_store::inventory::Result<_> {
                if index == 0 {
                    return Ok(Some((None, q.page(None)?)));
                }
                let mut new_order = q.order_by_size(index == 2, a.scratch, 512 * 1024 * 1024)?;
                while !new_order.advance(&request.cancel)? {
                    let l = lane.lock().unwrap();
                    if mode.superseded(&l, request.id) {
                        return Ok(None);
                    }
                    std::thread::yield_now();
                }
                let new_page = new_order.page(None, false)?;
                Ok(Some((Some(new_order), new_page)))
            })();
            match attempt {
                Ok(Some((new_order, new_page))) => {
                    order = new_order;
                    page = new_page;
                    sort_index = index;
                    error = None;
                }
                Ok(None) => return Ok(()),
                Err(e) => error = Some(e.to_string()),
            }
            force = true;
        }
        let requested_profile = mode.initial_duplicates.take().or_else(|| {
            dupes
                .filter(|(token, _)| *token == current_token)
                .map(|(_, profile)| profile)
        });
        if let Some(profile) = requested_profile
            && duplicates.is_none()
            && !request.cancel.is_cancelled()
        {
            order = None;
            page = q.page(None)?;
            sort_index = 0;
            let attempt = (|| -> sortsize_store::inventory::Result<_> {
                let mut job = q.prepare_duplicates_with_profile(
                    a.scratch,
                    512 * 1024 * 1024,
                    profile,
                    &request.cancel,
                )?;
                let guard = sortsize_core::acquisition::Guard::open(
                    a.root,
                    sortsize_core::acquisition::Policy {
                        explicit_root: true,
                        ..Default::default()
                    },
                )?;
                job.verify(
                    &guard,
                    sortsize_core::duplicates::DuplicateOptions::default(),
                    &request.cancel,
                )?;
                let p = job.page(None, false)?;
                Ok((job, p))
            })();
            match attempt {
                Ok((job, p)) => {
                    duplicates = Some(job);
                    duplicate_profile = Some(profile);
                    duplicate_page = Some(p);
                    error = None;
                }
                Err(e) => error = Some(e.to_string()),
            }
            force = true;
        }
        if let Some((_, forward)) = next.filter(|(t, _)| *t == current_token) {
            if let (Some(job), Some(p)) = (&duplicates, &duplicate_page) {
                if forward && p.more {
                    duplicate_page = Some(job.page(p.last.as_ref(), false)?);
                    force = true;
                } else if !forward {
                    let prev = job.page(p.first.as_ref(), true)?;
                    if !prev.rows.is_empty() {
                        duplicate_page = Some(prev);
                        force = true;
                    }
                }
            } else if forward && page.more {
                page = if let Some(o) = &order {
                    o.page(page.next.as_ref(), false)?
                } else {
                    q.page(page.next.as_ref())?
                };
                force = true;
            } else if !forward && let Some(first) = &page.first {
                let prev = if let Some(o) = &order {
                    o.page(Some(first), true)?
                } else {
                    q.page_before(first)?
                };
                if !prev.rows.is_empty() {
                    page = prev;
                    force = true;
                }
            }
        }
        if stats == Some(current_token) && duplicates.is_none() && !request.cancel.is_cancelled() {
            match q.statistics(a.scratch, 512 * 1024 * 1024, &request.cancel) {
                Ok(report) => {
                    statistics = Some(report);
                    error = None;
                }
                Err(e) => error = Some(e.to_string()),
            }
            force = true;
        }
        if force {
            let previous = if let (Some(job), Some(p)) = (&duplicates, &duplicate_page) {
                p.first
                    .as_ref()
                    .map(|c| job.page(Some(c), true).map(|p| !p.rows.is_empty()))
                    .transpose()?
                    .unwrap_or(false)
            } else {
                page.first
                    .as_ref()
                    .map(|c| {
                        if let Some(o) = &order {
                            o.page(Some(c), true).map(|p| !p.rows.is_empty())
                        } else {
                            q.page_before(c).map(|p| !p.rows.is_empty())
                        }
                    })
                    .transpose()?
                    .unwrap_or(false)
            };
            let (rows, next) = duplicate_page
                .as_ref()
                .map(|p| (p.rows.clone(), p.more))
                .unwrap_or_else(|| (page.rows.clone(), page.more));
            let mut p = prepare(
                a,
                request.id,
                q,
                rows,
                (previous, next),
                &error.clone().unwrap_or_else(|| {
                    statistics.as_ref().map(statistics_text).unwrap_or_default()
                }),
                units,
            )?;
            if let (Some(job), Some(dp)) = (&duplicates, &duplicate_page) {
                let s = job.summary();
                p.duplicates = true;
                p.duplicate_groups = Some(s.groups);
                p.complete = s.complete;
                p.matches = s.independent_members + s.aliases;
                p.status = format!(
                    "{} {}",
                    if s.complete {
                        tr!("Complete")
                    } else {
                        tr!("Incomplete coverage")
                    },
                    tr_format!(
                        "Exact groups: {} · bytes read: {} · hardlink aliases: {} · errors: {}.",
                        s.groups,
                        s.bytes_read,
                        s.aliases,
                        s.errors
                    )
                );
                let profile = duplicate_profile.expect("verified duplicate search profile");
                let counts = crate::display_preferences::profile_counts(
                    profile,
                    s.examined_files.into(),
                    s.excluded_by_size.into(),
                    s.excluded_empty.into(),
                );
                p.statistics = format!(
                    "{counts} {}",
                    tr!(
                        "SHA-256 candidates are compared byte for byte. Hardlink aliases are the same file."
                    )
                );
                if s.directory_listing_sources > 0 {
                    let warning = tr!(
                        "Archived directory sizes may be outdated. Some duplicates may be missed. Verify the saved inventories before searching again."
                    );
                    p.statistics.push(' ');
                    p.statistics.push_str(warning);
                }
                if s.budget_limited {
                    p.statistics.push(' ');
                    p.statistics.push_str(tr!(
                        "Read budget or file-size limit reached. Results are incomplete."
                    ));
                }
                if s.cancelled {
                    p.statistics.push(' ');
                    p.statistics
                        .push_str(tr!("Cancelled. Only finished groups are shown."));
                }
                if s.unverified_sources > 0 {
                    p.statistics.push_str(&format!(
                        "\n{} {}",
                        tr_format!("Unverified sources: {}.", s.unverified_sources),
                        tr!("Source continuity is unverified. Current file contents can be compared, but inventory coverage and independent copies are not proven.")
                    ));
                }
                p.duplicate_group_first = (0..p.model.len())
                    .map(|index| dp.starts_group_at(index))
                    .collect();
                for (row, (group, copy, alias)) in p.model.iter_mut().zip(&dp.labels) {
                    row.label = format!(
                        "{} {}{}",
                        if s.unverified_sources > 0 {
                            tr_format!("Group {} · location {} ·", group, copy)
                        } else {
                            tr_format!("Group {} · copy {} ·", group, copy)
                        },
                        if *alias {
                            format!("{} · ", tr!("Hardlink alias"))
                        } else {
                            String::new()
                        },
                        row.label
                    )
                    .into();
                }
            } else if let Some(e) = &error {
                p.status = e.clone();
            }
            p.sort_index = sort_index;
            current_token = p.token;
            if !mode.publish(lane, output, p) {
                return Ok(());
            }
            service::notify(lane);
            force = false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
impl Host {
    pub(super) fn close_search(&mut self, w: &SortSizeWindow) {
        self.close_empty_analysis(w);
        self.cancel_duplicate_task();
        self.reset_duplicate_review(w, true);
        self.search_cancel.cancel();
        self.search_id += 1;
        self.search_current = None;
        self.search_pending = None;
        let mut l = self.service.lane.lock().unwrap();
        l.search = None;
        l.search_current = None;
        l.search_page = None;
        l.search_statistics = None;
        l.search_duplicates = None;
        l.search_sort = None;
        w.set_search_visible(false);
        w.set_search_busy(false);
        w.set_duplicate_waiting_for_scan(false);
        w.set_search_token(0);
        w.set_search_scope("".into());
        w.set_search_statistics("".into());
        w.set_search_complete(false);
        w.set_search_duplicates_visible(false);
        w.set_search_order(0);
        w.set_search_matches("".into());
        w.set_search_scroll_y(0.);
        w.set_search_statistics_visible(false);
        w.set_search_results(ModelRc::default());
        w.set_search_selected(-1);
        w.set_search_full_path("".into());
        w.set_search_previous(false);
        w.set_search_next(false);
        w.set_duplicate_rows(ModelRc::default());
        w.set_duplicate_folder_view(false);
        w.set_duplicate_folder_detail(false);
        w.set_duplicate_reference_available(false);
        w.set_duplicate_folder_count("0".into());
        w.set_duplicate_file_count("0".into());
        w.set_duplicate_summary("".into());
        w.set_duplicate_tree_run("".into());
        w.set_duplicate_warning("".into());
        w.set_duplicate_details("".into());
        w.set_duplicate_content_retry(false);
        w.set_duplicate_content_limits_expanded(false);
        w.set_duplicate_content_limits_error("".into());
        w.set_duplicate_scope_summary(Default::default());
    }
    pub(super) fn show_analysis(&mut self, w: &SortSizeWindow, c: &Controller) {
        self.close_empty_analysis(w);
        if matches!(w.get_preview_state(), 1 | 2) {
            w.set_busy(false);
        }
        self.clear_preview(w, 0);
        self.cache_cancel.cancel();
        self.cache_id += 1;
        self.cache_wait = None;
        self.cache_comparing = false;
        w.set_cache_visible(false);
        w.set_smb_server_visible(false);
        w.set_search_visible(true);
        if matches!(w.get_duplicate_mode(), 1 | 2) {
            self.refresh_duplicate_choices(w, c);
        }
        w.set_top_visible(false);
        w.invoke_focus_search();
    }
    fn begin_search(&mut self, w: &SortSizeWindow) {
        self.cancel_duplicate_task();
        self.reset_duplicate_review(w, true);
        {
            let lane = self.service.lane.lock().unwrap();
            if lane.direct_view.is_none()
                && !lane.reader_active
                && lane
                    .phase
                    .is_some_and(|(_, phase, _)| matches!(phase, service::Phase::Cancelled))
            {
                w.set_search_status(tr!("Cancelled").into());
                w.set_search_busy(false);
                return;
            }
        }
        let parse = |s: slint::SharedString| {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                s.trim().parse::<u64>().map(Some).map_err(|_| ())
            }
        };
        let (Ok(min_bytes), Ok(max_bytes)) = (parse(w.get_search_min()), parse(w.get_search_max()))
        else {
            w.set_search_status(tr!("Invalid byte count.").into());
            return;
        };
        if min_bytes.zip(max_bytes).is_some_and(|(a, b)| a > b) {
            w.set_search_status(tr!("Invalid size range.").into());
            return;
        }
        let text = |s: slint::SharedString| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };
        let query = SearchQuery {
            name: text(w.get_search_name()),
            path: text(w.get_search_path()),
            extension: text(w.get_search_extension()),
            glob: text(w.get_search_glob()),
            min_bytes,
            max_bytes,
            case_sensitive: w.get_search_case(),
            ..Default::default()
        };
        if sortsize_core::analysis::PreparedQuery::new(&query).is_err() {
            w.set_search_status(tr!("Invalid search query.").into());
            return;
        }
        if self.requested.is_none() {
            w.set_search_status(tr!("Choose a source and start an inventory first.").into());
            return;
        }
        self.search_cancel.cancel();
        self.search_cancel = Default::default();
        self.search_id += 1;
        self.search_current = None;
        self.search_pending = None;
        let mut l = self.service.lane.lock().unwrap();
        l.search_current = Some(self.search_id);
        l.search = Some(Request {
            id: self.search_id,
            query,
            cancel: self.search_cancel.clone(),
        });
        l.search_page = None;
        l.search_statistics = None;
        l.search_duplicates = None;
        l.search_sort = None;
        w.set_search_token(0);
        w.set_search_scope("".into());
        w.set_search_statistics("".into());
        w.set_search_complete(false);
        w.set_search_duplicates_visible(false);
        w.set_search_order(0);
        w.set_search_matches("".into());
        w.set_search_scroll_y(0.);
        w.set_search_statistics_visible(false);
        w.set_search_results(ModelRc::default());
        w.set_search_statistics("".into());
        w.set_search_selected(-1);
        w.set_search_full_path("".into());
        w.set_search_busy(true);
        w.set_search_previous(false);
        w.set_search_next(false);
        w.set_search_status(
            tr!("Waiting for the frozen inventory; cancelling search leaves the scan running.")
                .into(),
        );
    }
    pub(super) fn poll_search(&mut self, w: &SortSizeWindow) {
        if let Ok(mut r) = self.service.resources.try_lock()
            && let Some((_, text)) = r
                .take()
                .filter(|(context, _)| *context == self.presentation)
        {
            w.set_memory_status(text.into());
        }
        if let Ok(mut o) = self.service.search_output.try_lock()
            && let Some(p) = o
                .take()
                .filter(|p| p.id == self.search_id && p.presentation == self.presentation)
        {
            self.search_pending = Some(p);
        }
        if !self.gesture
            && let Some(p) = self
                .search_pending
                .take()
                .filter(|p| p.id == self.search_id && p.presentation == self.presentation)
        {
            #[cfg(feature = "benchmarks")]
            let apply_begin = Instant::now();
            let selected = self
                .search_current
                .as_ref()
                .and_then(|old| {
                    usize::try_from(w.get_search_selected())
                        .ok()
                        .and_then(|i| old.rows.get(i))
                })
                .map(|r| r.key);
            let anchor = self
                .search_current
                .as_ref()
                .and_then(|old| {
                    old.rows
                        .get(((-w.get_search_scroll_y()).max(0.) / 34.) as usize)
                })
                .map(|r| r.key);
            let offset = (-w.get_search_scroll_y()).max(0.) % 34.;
            let index = selected.and_then(|key| p.rows.iter().position(|r| r.key == key));
            let scroll = anchor
                .and_then(|key| p.rows.iter().position(|r| r.key == key))
                .map_or(0., |i| -(i as f32 * 34. + offset));
            w.set_search_complete(p.complete);
            w.set_search_duplicates_visible(p.duplicates);
            w.set_search_order(p.sort_index);
            w.set_search_matches(p.matches.to_string().into());
            w.set_search_token(p.token);
            if p.duplicates {
                w.set_duplicate_menu_token(w.get_duplicate_menu_token().wrapping_add(1));
                w.set_search_group_first(
                    Rc::new(VecModel::from(p.duplicate_group_first.clone())).into(),
                );
            }
            w.set_search_results(Rc::new(VecModel::from(p.model.clone())).into());
            w.set_search_status(p.status.clone().into());
            w.set_search_scope(p.scope.clone().into());
            w.set_search_statistics(p.statistics.clone().into());
            w.set_search_busy(p.busy);
            w.set_search_previous(p.previous);
            w.set_search_next(p.next);
            w.set_search_selected(index.map_or(-1, |i| i as i32));
            w.set_search_full_path(
                index
                    .map_or_else(String::new, |i| label(&p.rows[i].path))
                    .into(),
            );
            w.set_search_scroll_y(scroll);
            self.search_current = Some(p);
            #[cfg(feature = "benchmarks")]
            {
                self.search_applied_at = Some(Instant::now());
                self.search_apply_ms = apply_begin.elapsed().as_secs_f64() * 1000.;
                if self.native_probe_phases
                    && self
                        .search_current
                        .as_ref()
                        .is_some_and(|p| !p.busy && p.token > 0)
                {
                    i_slint_backend_winit::presentation_probe::phase(3);
                }
            }
        }
    }
}
fn superseded(lane: &Lane, id: u64) -> bool {
    lane.stop
        || lane.start.is_some()
        || lane.navigation.is_some()
        || lane.search.is_some()
        || lane
            .cache
            .as_ref()
            .is_some_and(|request| !matches!(request.action, super::cache::Action::Refresh))
        || lane.search_current != Some(id)
}
impl Drop for Host {
    fn drop(&mut self) {
        self.cache_cancel.cancel();
        self.search_cancel.cancel();
    }
}
pub(super) fn connect(w: &SortSizeWindow, c: &Rc<RefCell<Controller>>, h: &Rc<RefCell<Host>>) {
    macro_rules! connect {($method:ident,|$w:ident,$h:ident,$c:ident $(,$arg:ident)*|$body:block)=>{{let weak=w.as_weak();let h=h.clone();let c=c.clone();w.$method(move |$($arg),*|{if let Some($w)=weak.upgrade(){#[allow(unused_mut)]let mut $h=h.borrow_mut();#[allow(unused_mut)]let mut $c=c.borrow_mut();let _scope=$c.presentation.enter();$body}});}};}
    connect!(on_show_analysis, |w, h, c| {
        h.show_analysis(&w, &c);
    });
    connect!(on_close_search, |w, h, _c| {
        h.close_search(&w);
        h.navigate(Navigate::Refresh);
    });
    connect!(on_search_submit, |w, h, _c| {
        if w.get_duplicate_mode() == 0 {
            h.begin_search(&w);
        }
    });
    connect!(on_search_cancel, |w, h, _c| {
        if w.get_duplicate_mode() > 0 {
            h.cancel_duplicate_verification(&w);
            return;
        }
        h.search_cancel.cancel();
        let mut l = h.service.lane.lock().unwrap();
        if l.search.take().is_some() {
            w.set_search_busy(false);
            w.set_search_status(tr!("Search cancelled; the scan continues.").into());
        }
    });
    connect!(on_search_page, |_w, h, _c, token, next| {
        if _w.get_duplicate_mode() > 0 {
            h.page_duplicates(next);
            return;
        }
        if h.search_current.as_ref().is_some_and(|p| p.token == token) {
            h.service.lane.lock().unwrap().search_page = Some((token, next));
        }
    });
    connect!(on_search_sort, |w, h, _c, token, index| {
        if !h.search_cancel.is_cancelled()
            && h.search_current
                .as_ref()
                .is_some_and(|p| p.token == token && !p.busy && !p.duplicates)
            && (0..3).contains(&index)
        {
            h.service.lane.lock().unwrap().search_sort = Some((token, index));
            w.set_search_busy(true);
            w.set_search_status(tr!("Preparing inventory…").into());
        }
    });
    connect!(on_search_duplicates, |w, h, _c, token| {
        if !h.search_cancel.is_cancelled()
            && h.search_current
                .as_ref()
                .is_some_and(|p| p.token == token && !p.busy && !p.duplicates)
        {
            h.service.lane.lock().unwrap().search_duplicates =
                Some((token, duplicate_ui::effective_profile(&w)));
            w.set_search_busy(true);
            w.set_search_status(tr!("Preparing inventory…").into());
        }
    });
    connect!(on_search_stats, |w, h, _c, token| {
        if !h.search_cancel.is_cancelled()
            && h.search_current.as_ref().is_some_and(|p| p.token == token)
        {
            h.service.lane.lock().unwrap().search_statistics = Some(token);
            w.set_search_busy(true);
        }
    });
    connect!(on_search_select, |w, h, _c, token, index, reveal| {
        if w.get_duplicate_mode() > 0 {
            h.select_duplicate(&w, index);
            return;
        }
        if let Some(p) = h
            .search_current
            .as_ref()
            .filter(|p| p.token == token && p.id == h.search_id)
            && let Some(row) = usize::try_from(index).ok().and_then(|i| p.rows.get(i))
        {
            let key = row.key;
            w.set_search_selected(index);
            w.set_search_full_path(label(&row.path).into());
            if reveal {
                w.set_entry_path(label(&row.path).into());
                h.selected = Some(key);
                h.close_search(&w);
                h.navigate(Navigate::Reveal(key));
            }
        }
    });
    connect!(on_memory_limit_apply, |w, _h, c| {
        let text = w.get_memory_limit();
        let bytes = if text.trim().is_empty() {
            None
        } else {
            let Some(bytes) = text
                .trim()
                .parse::<u64>()
                .ok()
                .and_then(crate::display_preferences::memory_limit_bytes)
            else {
                w.set_memory_status(
                    tr!("Enter at least 128 MiB, or leave empty for automatic.").into(),
                );
                return;
            };
            Some(bytes)
        };
        c.preferences.memory_bytes = bytes;
        Governor::global().set_byte_limit(bytes);
        let units = c.preferences.units;
        c.display_changed(&w, units);
    });
    connect!(on_memory_select, |w, _h, c, index| {
        if let Some(mode) = usize::try_from(index).ok().and_then(|i| MODES.get(i)) {
            let previous = c.preferences.units;
            c.preferences.memory = *mode;
            c.display_changed(&w, previous);
        }
    });
}

#[cfg(test)]
#[test]
fn explicit_duplicate_late_publication_preserves_new_mailbox_and_controls() {
    // The old worker has already prepared its response before another UI
    // intention wins. Exercise that interleaving without a scheduling delay.
    for changed in 0..4 {
        let mode = RunMode {
            initial_duplicates: Some(DuplicateFileProfile::Exhaustive),
            epoch: Some((10, 12)),
        };
        let old = Prepared::error(7, "old completed response".into());
        let mut state = Lane::default();
        state.source_epoch = 10;
        state.view_epoch = 12;
        state.search_current = Some(7);
        let lane = Mutex::new(state);
        let output = Mutex::new(None);
        {
            let mut current = lane.lock().unwrap();
            match changed {
                0 => current.search_current = Some(8),
                1 => current.source_epoch = 11,
                2 => current.view_epoch = 13,
                _ => current.stop = true,
            }
            current.search_page = Some((91, true));
            current.search_statistics = Some(91);
            current.search_duplicates = Some((91, DuplicateFileProfile::Exhaustive));
            current.search_sort = Some((91, 2));
            current.search_refresh = true;
            *output.lock().unwrap() = Some(Prepared::error(8, "new response".into()));
        }
        assert!(!mode.publish(&lane, &output, old));
        let page = output.lock().unwrap();
        assert_eq!(page.as_ref().unwrap().id, 8);
        assert_eq!(page.as_ref().unwrap().status, "new response");
        drop(page);
        let current = lane.lock().unwrap();
        assert_eq!(current.search_page, Some((91, true)));
        assert_eq!(current.search_statistics, Some(91));
        assert_eq!(
            current.search_duplicates,
            Some((91, DuplicateFileProfile::Exhaustive))
        );
        assert_eq!(current.search_sort, Some((91, 2)));
        assert!(current.search_refresh);
    }
    let mut state = Lane::default();
    state.source_epoch = 10;
    state.view_epoch = 12;
    state.search_current = Some(7);
    let lane = Mutex::new(state);
    let output = Mutex::new(None);
    let mode = RunMode {
        initial_duplicates: Some(DuplicateFileProfile::Exhaustive),
        epoch: Some((10, 12)),
    };
    assert!(mode.publish(
        &lane,
        &output,
        Prepared::error(7, "current response".into())
    ));
    assert_eq!(output.lock().unwrap().as_ref().unwrap().id, 7);
}
