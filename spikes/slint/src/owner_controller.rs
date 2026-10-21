//! UI adapter: bounded page requests and generation-checked row patches only.
use crate::{Controller, SortSizeWindow, owner_view};
use slint::Model;
use std::sync::Arc;

pub(crate) type PageKey = (u64, Option<usize>, usize, usize, bool, bool);

impl Controller {
    pub(crate) fn cancel_owner_sort(&mut self, window: &SortSizeWindow) {
        self.owner_sort_serial = self.owner_sort_serial.wrapping_add(1);
        self.owner_sort_pending = None;
        window.set_owner_sort_preparing(false);
    }
    pub(crate) fn request_owners(&mut self, window: &SortSizeWindow, sort: Option<(u64, usize)>) {
        if sort.is_some() {
            self.owner_sort_pending = sort;
        }
        let sort = self.owner_sort_pending;
        let data = self.tree.as_ref().and_then(|tree| tree.owner_data());
        let enabled = window.get_resolve_owner() && data.is_some();
        let visible_page = !window.get_top_visible();
        let (selected, offset, order) = self
            .tree
            .as_ref()
            .map_or((None, 0, 0), |tree| tree.owner_page_key());
        let key = (
            self.generation,
            selected,
            offset,
            order,
            enabled,
            visible_page,
        );
        if self.owner_page_key == Some(key) && sort.is_none() {
            return;
        }
        if self.owner_page_key != Some(key) {
            self.owner_page = self.owner_page.wrapping_add(1);
            self.owner_names = Arc::default();
        }
        self.owner_page_key = Some(key);
        if enabled && self.owner_task.is_none() {
            match owner_view::Task::system() {
                Ok(task) => self.owner_task = Some(task),
                Err(_) => {
                    window.set_owner_status(
                        "Résolution indisponible ; identifiants natifs conservés.".into(),
                    );
                    return;
                }
            }
        }
        let visible = if enabled && visible_page {
            window
                .get_details()
                .iter()
                .filter_map(|row| self.tree.as_ref()?.owner_id(row.node_id as usize))
                .collect()
        } else {
            Vec::new()
        };
        if let Some(task) = &mut self.owner_task {
            task.request(owner_view::Request {
                generation: self.generation,
                page: self.owner_page,
                enabled,
                data: if enabled { data } else { None },
                visible,
                sort,
            });
        }
        window.set_owner_status(
            if enabled {
                "Résolution des noms de la page…"
            } else {
                ""
            }
            .into(),
        );
    }
    pub(crate) fn apply_owner_names(&mut self, window: &SortSizeWindow) {
        if self.owner_names.is_empty() {
            return;
        }
        let Some(tree) = &self.tree else {
            return;
        };
        #[cfg(feature = "benchmarks")]
        let started = std::time::Instant::now();
        let model = window.get_details();
        for index in 0..model.row_count() {
            let Some(mut row) = model.row_data(index) else {
                continue;
            };
            if let Some(id) = tree.owner_id(row.node_id as usize)
                && let Some(name) = owner_view::label_for(&self.owner_names, id)
                && row.owner != *name
            {
                row.owner = name.clone();
                model.set_row_data(index, row);
                #[cfg(feature = "benchmarks")]
                {
                    self.owner_patch_rows += 1;
                }
            }
        }
        #[cfg(feature = "benchmarks")]
        {
            self.owner_patch_max_ms = self
                .owner_patch_max_ms
                .max(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    pub(crate) fn resolution_changed(&mut self, window: &SortSizeWindow) {
        self.generation = self.generation.wrapping_add(1);
        self.owner_sort_serial = self.owner_sort_serial.wrapping_add(1);
        self.owner_sort_pending = None;
        self.owner_page_key = None;
        self.owner_names = Arc::default();
        window.set_owner_sort_preparing(false);
        if !window.get_resolve_owner()
            && let Some(tree) = &mut self.tree
        {
            tree.reset_owner_orders();
        }
        if self.tree.is_some() {
            self.show_owner_sort(window);
        } else {
            self.request_owners(window, None);
        }
    }
    pub(crate) fn sort_owners(&mut self, window: &SortSizeWindow) {
        let Some(tree) = &mut self.tree else {
            return;
        };
        if tree.owner_data().is_none() {
            return;
        }
        let direction = usize::from(tree.sort() == 6);
        if window.get_resolve_owner() && self.owner_task.is_some() {
            self.owner_sort_serial = self.owner_sort_serial.wrapping_add(1);
            window.set_owner_sort_preparing(true);
            self.request_owners(window, Some((self.owner_sort_serial, direction)));
        } else {
            tree.sort_owner(None, direction);
            self.show_owner_sort(window);
        }
    }
    fn show_owner_sort(&mut self, window: &SortSizeWindow) {
        let inspected = window.get_inspected_node();
        if inspected >= 0 {
            self.inspect(window, inspected as usize);
        } else {
            self.show_rows(window);
        }
    }
    pub(crate) fn poll_owners(&mut self, window: &SortSizeWindow) {
        if self.owner_task.is_none() {
            return;
        }
        self.request_owners(window, None);
        let Some(task) = &mut self.owner_task else {
            return;
        };
        let updates = task.poll();
        if let Some(update) = updates.page
            && update.generation == self.generation
            && update.page == self.owner_page
            && window.get_resolve_owner()
        {
            self.owner_names = update.names;
            self.owner_stats = update.stats;
            self.apply_owner_names(window);
            let status = if update.waiting_for_previous {
                "Résolution suspendue : attente du fournisseur précédent ou service indisponible."
                    .to_owned()
            } else if update.pending > 0 {
                format!(
                    "Résolution des noms : {} identités restantes sur cette page.",
                    update.pending
                )
            } else {
                "Noms de la page actualisés ; identifiants natifs conservés en secours.".into()
            };
            window.set_owner_status(status.into());
        }
        if let Some(update) = updates.sort
            && update.generation == self.generation
            && update.request == self.owner_sort_serial
        {
            self.owner_sort_pending = None;
            #[cfg(feature = "benchmarks")]
            {
                self.owner_sort_prepare_ms = update.preparation_ms;
            }
            if let Some(tree) = &mut self.tree {
                tree.sort_owner(Some(update.orders), update.direction);
            }
            window.set_owner_sort_preparing(false);
            self.show_owner_sort(window);
        }
    }
}
