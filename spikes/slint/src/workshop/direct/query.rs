//! Cooperative query turns share the tree, without waiting for persistence.
use super::*;
use sortsize_store::direct_tree::query::{Cursor, PageQuery};

pub(super) struct DetailQuery {
    pub job: PageQuery,
    pub retained: Option<PageQuery>,
    pub retained_sort: Option<i32>,
    pub previous: Vec<Option<Cursor>>,
    pub anchor: Option<Cursor>,
    pub revision: u64,
}
pub(super) struct SearchWork {
    request: search::Request,
    job: PageQuery,
    previous: Vec<Option<Cursor>>,
    anchor: Option<Cursor>,
    sort: i32,
    token: i32,
    last: Instant,
    dirty: bool,
}
impl SearchWork {
    pub(super) fn request_id(&self) -> u64 {
        self.request.id
    }
}
impl DirectActive {
    pub(in super::super) fn pending_search_progress(&self) -> Option<(u64, u64)> {
        self.search
            .as_ref()
            .filter(|work| !work.job.done())
            .map(|work| (work.request_id(), work.job.evaluated))
    }

    pub(super) fn query_turn(
        &mut self,
        lane: &Mutex<Lane>,
        output: &Mutex<Option<search::Prepared>>,
        units: crate::units::Units,
    ) -> Result<()> {
        let tree_owner = self.scan.tree().clone();
        let Ok(tree) = tree_owner.try_read() else {
            return Ok(());
        };
        let cancel = CancellationToken::default();
        self.branch_turn(&tree, &cancel)?;
        // A completed empty order can otherwise remain frozen throughout a
        // busy scan: unrelated forced tree publications keep resetting the
        // publication clock. Serve the bounded observation window while the
        // tree is changing, and build the global order only once it is stable.
        if self.scan.status() != DirectStatus::Running
            && !self.observation_pending
            && self.detail_query.is_none()
        {
            self.detail_query = Some(DetailQuery {
                job: PageQuery::new(
                    &tree,
                    Some(self.parent),
                    None,
                    self.sort_index,
                    None,
                    DETAIL_LIMIT,
                )?,
                retained: None,
                retained_sort: None,
                previous: Vec::new(),
                anchor: None,
                revision: tree.revision(),
            });
        }
        if let Some(detail) = &mut self.detail_query {
            if detail.job.evaluated == 0 && matches!(self.sort_index, 6 | 7) {
                detail.job.set_owner_labels(&self.owner_labels)?;
            }
            if !detail.job.done() && detail.job.advance(&tree, &cancel)? {
                detail.retained = None;
                detail.retained_sort = None;
                self.force = true;
            }
            // Refresh a completed order only between pages, without starving an
            // in-flight sort when native observations arrive continuously.
            if detail.job.done()
                && detail.revision != tree.revision()
                && (self.scan.status() == DirectStatus::Finished
                    || self
                        .last
                        .is_none_or(|(t, _)| t.elapsed() >= PREVIEW_INTERVAL))
            {
                let next = PageQuery::new(
                    &tree,
                    Some(self.parent),
                    None,
                    self.sort_index,
                    detail.anchor.clone(),
                    DETAIL_LIMIT,
                )?;
                detail.retained = Some(std::mem::replace(&mut detail.job, next));
                detail.retained_sort = Some(self.sort_index);
                detail.revision = tree.revision();
            }
        }
        let (request, current, page, sort, stats, refresh) = {
            let mut l = lane.lock().unwrap();
            // An explicit duplicate worker owns its own query controls.
            if self.search.is_none() && l.search.is_none() {
                return Ok(());
            }
            (
                l.search.take(),
                l.search_current,
                l.search_page.take(),
                l.search_sort.take(),
                l.search_statistics.take(),
                std::mem::take(&mut l.search_refresh),
            )
        };
        if self
            .search
            .as_ref()
            .is_some_and(|s| Some(s.request.id) != current)
        {
            self.search = None;
        }
        if let Some(request) = request {
            let job = match PageQuery::new(&tree, None, Some(&request.query), 2, None, DETAIL_LIMIT)
            {
                Ok(job) => job,
                Err(error) => {
                    *output.lock().unwrap() =
                        Some(search::Prepared::error(request.id, error.to_string()));
                    drop(tree);
                    notify(lane);
                    return Ok(());
                }
            };
            self.search = Some(SearchWork {
                request,
                job,
                previous: Vec::new(),
                anchor: None,
                sort: 0,
                token: 0,
                last: Instant::now() - PREVIEW_INTERVAL,
                dirty: true,
            });
        }
        let Some(work) = &mut self.search else {
            return Ok(());
        };
        if work.request.cancel.is_cancelled() {
            let id = work.request.id;
            self.search = None;
            *output.lock().unwrap() =
                Some(search::Prepared::error(id, tr!("Cancelled").to_owned()));
            drop(tree);
            notify(lane);
            return Ok(());
        }
        let mut reset =
            self.scan.status() == DirectStatus::Finished && work.job.revision != tree.revision();
        if reset {
            work.anchor = None;
            work.previous.clear();
        }

        if let Some((token, index)) = sort.filter(|(token, _)| *token == work.token) {
            let _ = token;
            work.sort = index;
            work.anchor = None;
            work.previous.clear();
            reset = true;
        }
        if let Some((_, forward)) = page.filter(|(token, _)| *token == work.token) {
            if forward && work.job.done() {
                if let Some(next) = work.job.next() {
                    if work.previous.len() == 128 {
                        work.previous.remove(0);
                    }
                    work.previous.push(work.anchor.take());
                    work.anchor = Some(next);
                    reset = true;
                }
            } else if !forward && let Some(previous) = work.previous.pop() {
                work.anchor = previous;
                reset = true;
            }
        }
        if reset {
            work.job = PageQuery::new(
                &tree,
                None,
                Some(&work.request.query),
                match work.sort {
                    1 => 1,
                    2 => 0,
                    _ => 2,
                },
                work.anchor.clone(),
                DETAIL_LIMIT,
            )?;
            work.dirty = true;
        }
        if !work.job.done() {
            work.job.advance(&tree, &work.request.cancel)?;
            work.dirty = true;
        }
        work.dirty |= refresh || stats == Some(work.token);
        if !work.dirty || (!work.job.done() && work.last.elapsed() < PREVIEW_INTERVAL) {
            return Ok(());
        }
        let rows = work
            .job
            .rows(&tree)?
            .into_iter()
            .map(|r| Row {
                key: EntryKey {
                    inventory: self.context.inventory,
                    generation: self.context.generation,
                    entry: r.id,
                },
                path: r.path,
                kind: r.kind,
                logical: r.logical,
            })
            .collect::<Vec<_>>();
        let bytes = 64 * 1024
            + rows
                .iter()
                .map(|r| r.path.capacity() * 12 + 1024)
                .sum::<usize>();
        let memory = Governor::global()
            .reserve(Category::Publications, bytes as u64, None)
            .ok_or(Error::Contract("direct search publication budget"))?;
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
        let complete = work.job.done()
            && tree.summary().coverage == ScopeCoverage::Complete
            && work.job.gaps == 0
            && self.scan.status() == DirectStatus::Finished
            && work.job.revision == tree.revision();
        let next_token = search::token()?;
        {
            let mut l = lane.lock().unwrap();
            if l.search_current == Some(work.request.id)
                && let Some((token, _)) = &mut l.search_duplicates
                && *token == work.token
            {
                *token = next_token;
            }
        }
        work.token = next_token;
        let status = format!(
            "{} · {} / {}",
            if !work.job.done() {
                tr!("Searching the entire inventory…")
            } else if complete {
                tr!("Complete")
            } else {
                tr!("Partial")
            },
            work.job.matches,
            work.job.evaluated
        );
        let mut statistics = format!(
            "{}: {} · {}: {} · {}: {}\n{}: {}",
            tr!("Files"),
            work.job.files,
            tr!("Known logical bytes"),
            work.job.logical,
            tr!("Unknown sizes"),
            work.job.unknown_sizes,
            tr!("Size classes (0 / <1 KiB / <1 MiB / <1 GiB / ≥1 GiB / unknown)"),
            work.job.classes.map(|n| n.to_string()).join(" / ")
        );
        for (extension, counts) in &work.job.extensions {
            statistics.push_str(&format!(
                "\n{}: {} / {}",
                short_label(Path::new(extension)),
                counts.0,
                counts.1
            ));
        }
        statistics.push_str(&format!(
            "\n{}: {} / {}",
            tr!("Other extensions"),
            work.job.other_extensions.0,
            work.job.other_extensions.1
        ));
        *output.lock().unwrap() = Some(search::Prepared {
            id: work.request.id,
            #[cfg(feature = "benchmarks")]
            counts: Default::default(),
            #[cfg(feature = "benchmarks")]
            indexed: false,
            #[cfg(feature = "benchmarks")]
            predicate_evaluated: work.job.evaluated,
            #[cfg(feature = "benchmarks")]
            source_context: Some(self.context),
            presentation: crate::presentation::Context::current(),
            token: work.token,
            rows,
            model,
            status,
            scope: format!(
                "{} · {}",
                short_label(&self.root),
                if complete {
                    tr!("Complete")
                } else {
                    tr!("Partial")
                }
            ),
            statistics,
            busy: !work.job.done(),
            previous: !work.previous.is_empty(),
            next: work.job.done() && work.job.more(),
            complete,
            matches: work.job.matches,
            duplicates: false,
            duplicate_group_first: Vec::new(),
            duplicate_groups: None,
            sort_index: work.sort,
            _memory: Some(memory),
        });
        work.last = Instant::now();
        work.dirty = false;
        drop(tree);
        notify(lane);
        Ok(())
    }
    pub(in super::super) fn spawn_explicit_duplicates(
        &mut self,
        lane: &Arc<Mutex<Lane>>,
        output: &Arc<Mutex<Option<search::Prepared>>>,
        resources: &Arc<Mutex<Option<(crate::presentation::Context, String)>>>,
    ) {
        let Some(work) = self.search.as_ref() else {
            return;
        };
        let profile = {
            let mut l = lane.lock().unwrap();
            let Some((token, profile)) = l.search_duplicates else {
                return;
            };
            if token != work.token || l.search_current != Some(work.request.id) {
                l.search_duplicates = None;
                return;
            }
            if !work.job.done()
                || (!self.saved
                    && self.scan.status() == DirectStatus::Finished
                    && (self.saving.is_some() || !self.autosave_attempted))
            {
                return;
            }
            l.search_duplicates = None;
            profile
        };
        let saved = match self.saved_record() {
            Ok(saved) => saved,
            Err(error) => {
                *output.lock().unwrap() =
                    Some(search::Prepared::error(work.request.id, error.to_string()));
                notify(lane);
                return;
            }
        };
        let work = self.search.take().unwrap();
        let id = work.request.id;
        let epoch = (self.source_epoch, self.epoch);
        {
            let mut l = lane.lock().unwrap();
            if l.stop || l.search_current != Some(id) || (l.source_epoch, l.view_epoch) != epoch {
                return;
            }
            l.query_cancel = Some(work.request.cancel.clone());
        }
        let presentation = crate::presentation::Context::current();
        let worker_lane = Arc::clone(lane);
        let worker_output = Arc::clone(output);
        let worker_resources = Arc::clone(resources);
        let spawned = std::thread::Builder::new()
            .name("bunny-search-duplicates".into())
            .spawn(move || {
                let _presentation = presentation.enter();
                let result = (|| -> Result<()> {
                    {
                        let l = worker_lane.lock().unwrap();
                        if l.stop
                            || l.search_current != Some(id)
                            || (l.source_epoch, l.view_epoch) != epoch
                            || work.request.cancel.is_cancelled()
                        {
                            return Ok(());
                        }
                    }
                    let owner = Active::open_saved(epoch.0, &saved, None, &work.request.cancel)?;
                    search::run_explicit_duplicates(
                        &search::Frozen {
                            store: owner.session.store(),
                            certified: owner.certified_generation(),
                            c: owner.c,
                            root: &owner.root,
                            scratch: owner._temporary.path(),
                            started: owner.started,
                            sealed: owner.sealed.unwrap_or(owner.started),
                        },
                        work.request,
                        &worker_lane,
                        &worker_output,
                        &worker_resources,
                        epoch,
                        profile,
                    );
                    Ok(())
                })();
                if let Err(error) = result {
                    let l = worker_lane.lock().unwrap();
                    if !l.stop
                        && l.search_current == Some(id)
                        && (l.source_epoch, l.view_epoch) == epoch
                    {
                        *worker_output.lock().unwrap() =
                            Some(search::Prepared::error(id, error.to_string()));
                    }
                    drop(l);
                    notify(&worker_lane);
                }
            });
        if let Err(error) = spawned {
            *output.lock().unwrap() = Some(search::Prepared::error(id, error.to_string()));
            notify(lane);
        }
    }
    pub(super) fn query_navigation(
        &mut self,
        action: &Navigate,
        tree: &DirectTree,
    ) -> Result<bool> {
        match action {
            Navigate::ObservedPage(_) => {
                self.detail_query = None;
                Ok(false)
            }
            Navigate::Sort(sort) | Navigate::OwnerSort(sort, _) => {
                let next = if self
                    .detail_query
                    .as_ref()
                    .is_some_and(|detail| detail.job.done() || detail.retained.is_some())
                {
                    Some(PageQuery::new(
                        tree,
                        Some(self.parent),
                        None,
                        *sort,
                        None,
                        DETAIL_LIMIT,
                    )?)
                } else {
                    None
                };
                if let Navigate::OwnerSort(_, labels) = action {
                    self.owner_labels = labels.clone();
                }
                let old_sort = self.sort_index;
                let retained = self.detail_query.take().and_then(|detail| {
                    if detail.job.done() {
                        Some((detail.job, old_sort))
                    } else {
                        detail.retained.zip(detail.retained_sort)
                    }
                });
                self.sort_index = *sort;
                if let (Some((old, old_sort)), Some(next)) = (retained, next) {
                    self.detail_query = Some(DetailQuery {
                        job: next,
                        retained: Some(old),
                        retained_sort: Some(old_sort),
                        previous: Vec::new(),
                        anchor: None,
                        revision: tree.revision(),
                    });
                }
                self.after = None;
                self.native_previous.clear();
                Ok(true)
            }
            Navigate::Page(forward) => {
                if self.detail_query.is_none() {
                    return Ok(false);
                }
                if self
                    .detail_query
                    .as_ref()
                    .is_some_and(|q| !q.job.done() && q.retained.is_none())
                {
                    self.detail_query = None;
                    return Ok(false);
                }
                if let Some(detail) = &mut self.detail_query {
                    if !detail.job.done() {
                        return Ok(true);
                    }
                    let anchor = if *forward {
                        let Some(next) = detail.job.next() else {
                            return Ok(true);
                        };
                        Some(next)
                    } else {
                        let Some(previous) = detail.previous.last() else {
                            return Ok(true);
                        };
                        previous.clone()
                    };
                    let next = PageQuery::new(
                        tree,
                        Some(self.parent),
                        None,
                        self.sort_index,
                        anchor.clone(),
                        DETAIL_LIMIT,
                    )?;
                    if *forward {
                        if detail.previous.len() == 128 {
                            detail.previous.remove(0);
                        }
                        detail.previous.push(detail.anchor.take());
                    } else {
                        detail.previous.pop();
                    }
                    detail.anchor = anchor;
                    detail.retained = Some(std::mem::replace(&mut detail.job, next));
                    detail.retained_sort = Some(self.sort_index);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
