//! Complementary views never merge into the initial tree or scan report.
use crate::{
    Controller, SortSizeWindow,
    directory_worker::DirectoryWorker,
    presentation,
    tree::Tree,
    worker::{Job, Task},
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};

const PAGE_CHILDREN: usize = 4_000;
const CACHE_PAGES: usize = 4;
const CACHE_AGE: Duration = Duration::from_secs(600);
#[derive(Clone, PartialEq, Eq)]
struct Key {
    path: PathBuf,
    offset: u64,
}
struct Cached {
    key: Key,
    tree: Tree,
    loaded: u64,
    more: bool,
    previous: Option<u64>,
    at: Instant,
}
#[derive(Default)]
pub(crate) struct DirectoryState {
    pub(crate) original: Option<Tree>,
    worker: Option<DirectoryWorker>,
    task: Option<Task>,
    key: Option<Key>,
    cache: VecDeque<Cached>,
    loaded: u64,
    more: bool,
    previous: Option<u64>,
    complete: bool,
    window_ready: bool,
    completed_at: Option<Instant>,
    landing_last: Option<bool>,
    pub(crate) collect_allocation: bool,
    pub(crate) collect_owner: bool,
    #[cfg(test)]
    pub(crate) last_scan_ms: u128,
}
impl DirectoryState {
    #[cfg(test)]
    pub(crate) fn window_ready(&self) -> bool {
        self.window_ready
    }
    #[cfg(test)]
    pub(crate) fn replace_task(&mut self, task: Task) {
        self.task = Some(task);
    }

    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }
    #[cfg(test)]
    pub(crate) fn expire_cache(&mut self) {
        for cached in &mut self.cache {
            cached.at = Instant::now() - CACHE_AGE - Duration::from_secs(1);
        }
    }

    pub(crate) fn reset(&mut self, allocation: bool, owner: bool) {
        let worker = self.worker.take();
        // Replacing the state drops Task, which cancels its token without joining.
        // Keep the single worker so source changes cannot accumulate FS threads.
        *self = Self {
            worker,
            collect_allocation: allocation,
            collect_owner: owner,
            ..Default::default()
        };
    }
}
impl Controller {
    pub(crate) fn directory_controls(&self, window: &SortSizeWindow) {
        let _scope = self.presentation.enter();
        let active = self.directory.key.is_some();
        if let Some(labels) = &self.scan_labels {
            let initial = labels.get(self.preferences.units);
            window.set_summary(if active {
                tr_format!("Initial scan: {}", initial).into()
            } else {
                initial.into()
            });
        }
        window.set_can_load_details(
            self.task.is_none() && (active || self.tree.as_ref().is_some_and(Tree::needs_details)),
        );
        if let Some(key) = &self.directory.key {
            if self.tree.as_ref().and_then(Tree::selected_native).is_none() {
                window.set_current_path(crate::tree::native_label(&key.path).into());
            }
            let initial = self
                .directory
                .original
                .as_ref()
                .and_then(|t| t.known_size(&key.path, self.preferences.units))
                .unwrap_or_else(|| tr!("Unavailable").into());
            window.set_view_warning(if self.directory.task.is_some() {
                tr!("Additional folder listing in progress. Initial scan totals are unchanged.").into()
            } else {
                tr_format!("Initial folder total: {}. Additional direct-child listing starting at entry {}. Contents may have changed.", initial, crate::number_format::integer((key.offset + 1).into())).into()
            });
            if let Some(root) = self.directory.original.as_ref().and_then(Tree::root_native) {
                window.set_has_parent(
                    key.path != root || self.tree.as_ref().is_some_and(Tree::has_parent),
                );
            }
            if self.tree.as_ref().is_some_and(Tree::selected_is_root) {
                window.set_has_previous_detail_page(
                    window.get_has_previous_detail_page() || self.directory.previous.is_some(),
                );
                window.set_has_next_detail_page(
                    window.get_has_next_detail_page() || self.directory.more,
                );
            }
        }
    }
    fn initial_diagnostic(&mut self, window: &SortSizeWindow) {
        if let Some(crate::worker::DisplayLabels::Summary(data)) = &self.scan_labels {
            let data = data.clone();
            self.status(
                window,
                presentation::Status::Completed(data.report.coverage),
            );
            self.diagnostic(window, presentation::Diagnostic::Report(data));
        }
    }
    fn store_directory(&mut self) {
        if self.directory.complete
            && let (Some(key), Some(tree)) = (self.directory.key.take(), self.tree.take())
        {
            self.directory
                .cache
                .retain(|c| c.key != key && c.at.elapsed() < CACHE_AGE);
            self.directory.cache.push_back(Cached {
                key,
                tree,
                loaded: self.directory.loaded,
                more: self.directory.more,
                previous: self.directory.previous,
                at: self.directory.completed_at.unwrap_or_else(Instant::now),
            });
            while self.directory.cache.len() > CACHE_PAGES {
                self.directory.cache.pop_front();
            }
        }
    }
    pub(crate) fn load_directory(&mut self, window: &SortSizeWindow, force: bool) {
        if self.task.is_some()
            || !matches!(
                self.scan_labels,
                Some(crate::worker::DisplayLabels::Summary(_))
            )
        {
            return;
        }
        let Some(path) = self
            .tree
            .as_ref()
            .and_then(Tree::selected_native)
            .map(PathBuf::from)
            .or_else(|| self.directory.key.as_ref().map(|key| key.path.clone()))
        else {
            return;
        };
        if !force && !self.tree.as_ref().is_some_and(Tree::needs_details) {
            return;
        }
        if !force && self.directory.key.as_ref().is_some_and(|k| k.path == path) {
            return;
        }
        self.open_directory(window, Key { path, offset: 0 }, force);
    }
    fn open_directory(&mut self, window: &SortSizeWindow, key: Key, force: bool) {
        let sort = self.tree.as_ref().map_or(0, Tree::sort);
        if self.directory.original.is_none() {
            self.directory.original = self.tree.take();
        } else {
            self.store_directory();
        }
        // Task::drop cancels before the cache-hit early return below. The old
        // worker can publish only to its isolated, abandoned mailbox.
        self.directory.task = None;
        self.initial_diagnostic(window);
        self.directory.complete = false;
        self.directory.window_ready = false;
        self.directory.landing_last = None;
        self.directory.completed_at = None;
        self.directory.more = false;
        self.directory.previous = None;
        self.directory.key = Some(key.clone());
        self.generation = self.generation.wrapping_add(1);
        self.owner_page_key = None;
        self.owner_names = Default::default();
        self.cancel_owner_sort(window);
        let mut preview = Tree::preview();
        preview.inherit_sort(sort);
        self.tree = Some(preview);
        window.set_top_visible(false);
        window.set_cancelling(false);
        if !force
            && let Some(index) = self
                .directory
                .cache
                .iter()
                .position(|c| c.key == key && c.at.elapsed() < CACHE_AGE)
        {
            let cached = self.directory.cache.remove(index).unwrap();
            let mut tree = cached.tree;
            tree.prepare_presentation(self.presentation);
            tree.set_units(self.preferences.units);
            let coverage = tree.coverage();
            self.tree = Some(tree);
            self.status(window, presentation::Status::Listed(coverage));
            self.directory.loaded = cached.loaded;
            self.directory.more = cached.more;
            self.directory.previous = cached.previous;
            self.directory.complete = true;
            self.directory.window_ready = true;
            self.directory.completed_at = Some(cached.at);
            window.set_busy(false);
            self.show_rows(window);
            return;
        }
        if force {
            self.directory.cache.retain(|c| c.key.path != key.path);
        }
        if self.directory.worker.is_none() {
            match DirectoryWorker::start() {
                Ok(worker) => self.directory.worker = Some(worker),
                Err(error) => {
                    window.set_busy(false);
                    window.set_cancelling(false);
                    self.status(window, presentation::Status::ListingFailed);
                    self.diagnostic(window, presentation::Diagnostic::Native(error.to_string()));
                    self.show_rows(window);
                    return;
                }
            }
        }
        let Some(root) = self
            .directory
            .original
            .as_ref()
            .and_then(Tree::root_native)
            .map(PathBuf::from)
        else {
            return;
        };
        self.directory.task = Some(self.directory.worker.as_ref().unwrap().submit(Job {
            root: key.path.clone(),
            workers: 1,
            collect_allocation: self.directory.collect_allocation,
            collect_owner: self.directory.collect_owner,
            limits: sortsize_core::InventoryLimits {
                max_entries: PAGE_CHILDREN + 1,
                max_path_bytes: 1024 * 1024,
            },
            child_offset: key.offset,
            scope: Some(root),
        }));
        window.set_busy(true);
        self.status(window, presentation::Status::Listing);
        self.show_rows(window);
        window.set_current_path(crate::tree::native_label(&key.path).into());
    }
    pub(crate) fn load_omitted(&mut self, window: &SortSizeWindow) {
        if self.directory.window_ready
            && self.directory.more
            && self.tree.as_ref().is_some_and(Tree::selected_is_root)
        {
            if let Some(key) = self.directory.key.clone() {
                self.open_directory(
                    window,
                    Key {
                        path: key.path,
                        offset: key.offset + self.directory.loaded,
                    },
                    false,
                );
            }
        } else {
            self.load_directory(window, false);
        }
    }
    pub(crate) fn poll_directory(&mut self, window: &SortSizeWindow) {
        let Some(task) = &self.directory.task else {
            return;
        };
        let (patch, directory, completion) = {
            let Ok(mut mailbox) = task.mailbox.try_lock() else {
                return;
            };
            (
                mailbox.patch.take(),
                mailbox.directory.take(),
                mailbox.completion.take(),
            )
        };
        if completion.is_none()
            && let Some(patch) = patch
        {
            self.apply_inventory_patch(window, patch);
        }
        if completion.is_none()
            && let Some(progress) = directory
            && self
                .tree
                .as_ref()
                .is_some_and(|t| t.node_count() == progress.retained)
            && progress.closed
        {
            self.directory.window_ready = true;
            self.directory.loaded = progress.retained.saturating_sub(1) as u64;
            let offset = self.directory.key.as_ref().unwrap().offset;
            self.directory.more =
                self.directory.loaded > 0 && progress.seen > offset + self.directory.loaded;
            self.directory.previous = progress.previous;
            self.directory_controls(window);
        }
        if let Some(completion) = completion {
            self.directory.task = None;
            window.set_busy(false);
            window.set_cancelling(false);
            match completion {
                Ok(mut result) => {
                    let report = &result.presentation.report;
                    #[cfg(test)]
                    {
                        self.directory.last_scan_ms = report.duration_ms;
                    }
                    self.status(window, presentation::Status::Listed(report.coverage));
                    let total = result.direct_children_total;
                    self.directory.loaded = result.tree.node_count().saturating_sub(1) as u64;
                    let offset = self.directory.key.as_ref().unwrap().offset;
                    self.directory.more =
                        self.directory.loaded > 0 && total > offset + self.directory.loaded;
                    self.directory.previous = result.previous_child_offset;
                    // Cached windows retain the tree, not the error report. Re-read
                    // partial results so a cache hit cannot hide an access error
                    // or prevent recovery after permissions have changed.
                    self.directory.complete = report.coverage == sortsize_core::Coverage::Complete;
                    self.directory.window_ready = true;
                    self.directory.completed_at = Some(Instant::now());
                    if let Some(previous) = &self.tree {
                        result.tree.keep_navigation(previous);
                    }
                    if let Some(last) = self.directory.landing_last {
                        result.tree.detail_boundary(last);
                    }
                    result.tree.prepare_presentation(self.presentation);
                    result.tree.set_units(self.preferences.units);
                    self.tree = Some(result.tree);
                    // Initial summary, ranking and aggregates remain authoritative.
                    if report.errors_count > 0 {
                        self.diagnostic(
                            window,
                            presentation::Diagnostic::Report(result.presentation),
                        );
                    }
                }
                Err(error) => {
                    self.status(window, presentation::Status::ListingFailed);
                    self.tree = Some(Tree::preview());
                    self.diagnostic(window, presentation::Diagnostic::Failure(error));
                }
            }
            self.show_rows(window);
        }
    }
    pub(crate) fn cancel_directory(&mut self, window: &SortSizeWindow) {
        if let Some(task) = &self.directory.task {
            task.cancellation.cancel();
            window.set_cancelling(true);
        }
    }
    pub(crate) fn directory_interacted(&mut self) {
        self.directory.landing_last = None;
    }
    pub(crate) fn directory_page(
        &mut self,
        window: &SortSizeWindow,
        details: bool,
        next: bool,
    ) -> bool {
        let Some(key) = self.directory.key.clone() else {
            return false;
        };
        let Some(tree) = &self.tree else {
            return false;
        };
        if !details
            || !tree.selected_is_root()
            || (self.directory.task.is_some() && !self.directory.window_ready)
        {
            return false;
        }
        let offset = if next
            && tree.offset(true) + tree.details().0.len() >= self.directory.loaded as usize
            && self.directory.more
        {
            Some(key.offset + self.directory.loaded)
        } else if !next && tree.offset(true) == 0 {
            self.directory.previous
        } else {
            None
        };
        if let Some(offset) = offset {
            self.open_directory(
                window,
                Key {
                    path: key.path,
                    offset,
                },
                false,
            );
            self.directory.landing_last = Some(!next);
            if self.directory.task.is_none() {
                if let Some(tree) = &mut self.tree {
                    tree.detail_boundary(!next);
                }
                self.show_rows(window);
            }
            true
        } else {
            false
        }
    }
    pub(crate) fn directory_parent(&mut self, window: &SortSizeWindow) -> bool {
        if self
            .tree
            .as_ref()
            .is_some_and(|t| t.selected_native().is_some() && !t.selected_is_root())
        {
            return false;
        }
        let Some(key) = self.directory.key.clone() else {
            return false;
        };
        if self.directory.original.as_ref().and_then(Tree::root_native) == Some(key.path.as_path())
        {
            return false;
        }
        let Some(parent) = key.path.parent().map(PathBuf::from) else {
            return false;
        };
        if self
            .directory
            .original
            .as_mut()
            .is_some_and(|t| t.select_native(&parent))
        {
            self.store_directory();
            self.directory.task = None;
            self.directory.key = None;
            self.tree = self.directory.original.take();
            if let Some(tree) = &mut self.tree {
                tree.prepare_presentation(self.presentation);
                tree.set_units(self.preferences.units);
            }
            self.cancel_owner_sort(window);
            self.initial_diagnostic(window);
            window.set_busy(false);
            window.set_cancelling(false);
            window.set_view_warning("".into());
            self.generation = self.generation.wrapping_add(1);
            self.owner_page_key = None;
            self.show_rows(window);
        } else {
            self.open_directory(
                window,
                Key {
                    path: parent,
                    offset: 0,
                },
                false,
            );
        }
        true
    }
    pub(crate) fn restore_primary(&mut self, window: &SortSizeWindow) {
        if self.directory.original.is_some() {
            self.store_directory();
            self.directory.task = None;
            self.directory.key = None;
            self.tree = self.directory.original.take();
            if let Some(tree) = &mut self.tree {
                tree.prepare_presentation(self.presentation);
                tree.set_units(self.preferences.units);
            }
            self.cancel_owner_sort(window);
            self.initial_diagnostic(window);
            window.set_busy(false);
            window.set_cancelling(false);
            window.set_view_warning("".into());
            self.generation = self.generation.wrapping_add(1);
            self.owner_page_key = None;
        }
    }
}
