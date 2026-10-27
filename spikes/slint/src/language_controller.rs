//! UI-thread presentation transaction. The registry owns no windows or workers.
use crate::{Controller, LegalWindow, SortSizeWindow, locale, presentation};
use locale::{Activation, Language, LanguagePreference};
use slint::{ComponentHandle, Model};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

struct Participant {
    window: slint::Weak<SortSizeWindow>,
    controller: Weak<RefCell<Controller>>,
}
thread_local! {
    static WINDOWS: RefCell<Vec<Participant>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn register(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|p| p.controller.strong_count() > 0);
        windows.push(Participant {
            window: window.as_weak(),
            controller: Rc::downgrade(controller),
        });
    });
    let weak = window.as_weak();
    let state = Rc::downgrade(controller);
    window.on_set_language(move |index| {
        let preference = match index {
            0 => LanguagePreference::Automatic,
            1 => LanguagePreference::Explicit(Language::English),
            2 => LanguagePreference::Explicit(Language::French),
            3 => LanguagePreference::Explicit(Language::German),
            4 => LanguagePreference::Explicit(Language::Spanish),
            5 => LanguagePreference::Explicit(Language::Portuguese),
            6 => LanguagePreference::Explicit(Language::Italian),
            7 => LanguagePreference::Explicit(Language::Dutch),
            _ => return,
        };
        let Some(state) = state.upgrade() else { return };
        let requested = preference.resolve(state.borrow().system_locale.as_deref());
        // Slint bindings and Rust publications happen synchronously in this
        // callback, before the event loop can draw the next frame.
        match locale::activate_bundled(requested) {
            Ok(activation) => publish_preference(preference, activation),
            Err(_) => {
                if let Some(window) = weak.upgrade() {
                    // Failed activation leaves the previous selection intact.
                    window.set_language_failed(true);
                }
            }
        }
    });
}

/// Explicit selector action: share the user's choice with all live sessions.
/// Only this operation can update and persist their language preferences.
fn publish_preference(preference: LanguagePreference, activation: Activation) {
    for (window, state) in participants() {
        state
            .borrow_mut()
            .publish_language(&window, preference, activation);
    }
}

/// Initialization follows Slint's UI-thread-global catalog, preserving each
/// session's own preference and leaving every preference store untouched.
pub(crate) fn publish_effective_language(activation: Activation) {
    for (window, state) in participants() {
        state
            .borrow_mut()
            .publish_effective_language(&window, activation);
    }
}

fn participants() -> Vec<(SortSizeWindow, Rc<RefCell<Controller>>)> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|p| p.window.upgrade().is_some() && p.controller.strong_count() > 0);
        windows
            .iter()
            .filter_map(|p| Some((p.window.upgrade()?, p.controller.upgrade()?)))
            .collect()
    })
}

#[cfg(test)]
pub(crate) fn participant_count() -> usize {
    participants().len()
}

pub(crate) fn legal_notices(legal: &LegalWindow) {
    legal.set_notices(
        tr!(include_str!("legal-notice-en.txt"))
            .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
            .into(),
    );
}

/// Patch existing models, preserving delegates, selection, focus and viewport.
fn patch<T: Clone + PartialEq + 'static>(
    model: slint::ModelRc<T>,
    rows: Vec<T>,
) -> slint::ModelRc<T> {
    if model.row_count() != rows.len() {
        // The controller's raw page is authoritative. A stale UI model must not
        // panic the event loop or leave a partially translated page. No I/O.
        return Rc::new(slint::VecModel::from(rows)).into();
    }
    for (index, row) in rows.into_iter().enumerate() {
        if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
    model
}

impl Controller {
    pub(crate) fn show_inspected_group(&self, window: &SortSizeWindow) {
        let _scope = self.presentation.enter();
        if let Some(group) = self.inspected_group
            && let Some(tile) = window.get_tiles().iter().find(|tile| tile.node_id == group)
        {
            window.set_entry_path(
                format!(
                    "{} · {} · {}",
                    tile.name,
                    tile.size,
                    if group == -1 {
                        tr!("Individual entries available in the table.")
                    } else {
                        tr!("Paths not retained: no individual details available.")
                    }
                )
                .into(),
            );
        }
    }
    pub(crate) fn show_language(&self, window: &SortSizeWindow) {
        self.analysis.retranslate(self.presentation);
        let _scope = self.presentation.enter();
        window.set_language_choice(match self.preferences.language {
            LanguagePreference::Automatic => 0,
            LanguagePreference::Explicit(Language::English) => 1,
            LanguagePreference::Explicit(Language::French) => 2,
            LanguagePreference::Explicit(Language::German) => 3,
            LanguagePreference::Explicit(Language::Spanish) => 4,
            LanguagePreference::Explicit(Language::Portuguese) => 5,
            LanguagePreference::Explicit(Language::Italian) => 6,
            LanguagePreference::Explicit(Language::Dutch) => 7,
        });
        window.set_detected_language(self.system_locale.as_deref().unwrap_or("—").into());
        window.set_effective_language(
            Language::ALL
                .iter()
                .position(|language| *language == self.presentation.language)
                .unwrap_or(0) as i32,
        );
    }

    pub(crate) fn publish_language(
        &mut self,
        window: &SortSizeWindow,
        preference: LanguagePreference,
        activation: Activation,
    ) {
        let preference_changed = self.preferences.language != preference;
        self.preferences.language = preference;
        self.publish_effective_language(window, activation);
        // A preference edit is independent of a change of effective catalog.
        #[cfg(any(target_os = "linux", windows))]
        if preference_changed && let Some(store) = &mut self.preference_store {
            store.submit(self.preferences.clone());
            window.set_preference_state(1);
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = preference_changed;
    }

    pub(crate) fn publish_effective_language(
        &mut self,
        window: &SortSizeWindow,
        activation: Activation,
    ) {
        let retranslate = self
            .language_activation
            .is_none_or(|previous| previous.effective != activation.effective);
        self.language_activation = Some(activation);
        window.set_language_failed(false);
        window.set_language_fallback(activation.unavailable_catalog.is_some());
        if let Some(language) = activation.unavailable_catalog {
            window.set_unavailable_language(match language {
                Language::English => 0,
                Language::French => 1,
                Language::German => 2,
                Language::Spanish => 3,
                Language::Portuguese => 4,
                Language::Italian => 5,
                Language::Dutch => 6,
            });
        }
        // Creating another component may reinstall the bundle. Restore its
        // activation, but do not reproject unchanged pages or bump generations.
        if !retranslate {
            self.show_language(window);
            let _scope = self.presentation.enter();
            if let Some(legal) = self.legal.as_ref().and_then(slint::Weak::upgrade) {
                legal_notices(&legal);
            }
            return;
        }
        self.presentation = presentation::Context {
            language: activation.effective,
            generation: self.presentation.generation.wrapping_add(1),
        };
        let _scope = self.presentation.enter();
        self.show_language(window);
        self.show_preferences(window);
        window.set_status(self.status.render().into());
        window.set_errors(self.diagnostic.render().into());
        window.set_owner_status(self.owner_status.render().into());
        if let Some(original) = &mut self.directory.original {
            original.prepare_presentation(self.presentation);
            original.set_units(self.preferences.units);
        }
        if let Some(tree) = &mut self.tree {
            tree.prepare_presentation(self.presentation);
            window.set_rows(patch(window.get_rows(), tree.rows().0));
            window.set_details(patch(window.get_details(), tree.details().0));
            let ranking = self.directory.original.as_ref().unwrap_or(tree);
            window.set_top_rows(patch(window.get_top_rows(), ranking.top_rows.clone()));
            window.set_top_note(ranking.top_note.clone().into());
            window.set_selection_summary(tree.selection_summary().into());
            window.set_empty_message(tree.empty_message().into());
            let first = tree.offset(false) + 1;
            window.set_tree_page(
                presentation::page(first, first + window.get_rows().row_count() - 1).into(),
            );
            let first = tree.offset(true) + 1;
            window.set_detail_page(
                presentation::page(first, first + window.get_details().row_count() - 1).into(),
            );
            self.show_treemap(window);
            // Cached names are opaque provider results, never re-request them.
            self.apply_owner_names(window);
            self.show_inspected_group(window);
        } else if matches!(
            self.status,
            presentation::Status::Scanning | presentation::Status::Cancelling
        ) {
            window.set_top_note(tr!("Scan in progress…").into());
            window.set_empty_message(tr!("Scan in progress…").into());
            window.set_treemap_note(tr!("Scan in progress…").into());
            if self.scan_labels.is_none() {
                window.set_summary(tr!("Preparing inventory…").into());
            }
        } else if matches!(
            self.status,
            presentation::Status::Failed | presentation::Status::StartFailed
        ) {
            window.set_top_note(tr!("No ranking available: scan failed.").into());
            window.set_summary(tr!("No inventory available for this folder.").into());
            window.set_empty_message(tr!("No inventory available.").into());
            window.set_treemap_note(tr!("No inventory available.").into());
        }
        if let Some(labels) = &self.scan_labels {
            window.set_summary(labels.get(self.preferences.units).into());
        }
        if let Some(labels) = &self.warning_labels {
            window.set_inventory_warning(labels.get(self.preferences.units).into());
        }
        #[cfg(any(target_os = "linux", windows))]
        {
            window.set_storage_status(self.sources.status().into());
            if let Some(prepared) = &self.sources.current {
                window.set_volumes(patch(
                    window.get_volumes(),
                    prepared.rows_for(self.preferences.units),
                ));
            }
        }
        if let Some(legal) = self.legal.as_ref().and_then(slint::Weak::upgrade) {
            legal_notices(&legal);
        }
        self.directory_controls(window);
    }
}
