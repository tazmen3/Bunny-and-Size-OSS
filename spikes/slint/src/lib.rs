#[cfg(feature = "benchmarks")]
pub mod measure;

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod folder_picker;
mod path_input;
mod tree;
mod worker;

use path_input::PathInput;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use tree::Tree;
use worker::Task;

slint::slint! {
    import { Button, ComboBox, LineEdit, ListView, ScrollView, Spinner, TextEdit } from "std-widgets.slint";

    export component LegalWindow inherits Window {
        title: "SortSize — À propos / Licences";
        preferred-width: 760px; preferred-height: 560px;
        min-width: 580px; min-height: 400px;
        in property <string> notices;
        in property <string> license-text;
        private property <bool> full-license: false;
        VerticalLayout {
            padding: 16px; spacing: 12px;
            Text { text: "SortSize 0.1.0 — À propos / Licences"; font-size: 22px; }
            HorizontalLayout {
                Button { text: "Avis et droits"; clicked => { root.full-license = false; } }
                Button { text: "Lire la GPLv3 intégrale"; clicked => { root.full-license = true; } }
            }
            TextEdit {
                text: root.full-license ? root.license-text : root.notices;
                read-only: true;
                accessible-label: root.full-license ? "Texte intégral de la GPLv3" : "Avis, droits et absence de garantie";
            }
        }
    }

    export struct TreeRow {
        node-id: int, depth: int, name: string, size: string,
        expandable: bool, expanded: bool, selected: bool,
    }
    export struct DetailRow {
        node-id: int, name: string, size: string, note: string,
        directory: bool, proportion: string, fraction: float,
    }
    // A native, keyboard-focusable button with an elided display label. Full
    // names remain available through accessibility and the read-only path.
    component NameButton inherits Rectangle {
        in property <string> label;
        in property <string> action-label;
        callback clicked();
        min-width: 20px;
        Button {
            width: 100%; height: 100%;
            text: ""; accessible-label: root.action-label;
            clicked => { root.clicked(); }
        }
        Text {
            x: 8px; width: parent.width - 16px; height: 100%;
            text: root.label; color: #153550; font-size: 12px;
            overflow: elide; vertical-alignment: center;
        }
    }
    export component SortSizeWindow inherits Window {
        title: "SortSize — analyse du stockage";
        preferred-width: 1200px; preferred-height: 760px;
        min-width: 960px; min-height: 600px;
        background: #edf2f7;
        in-out property <string> scan-path;
        in-out property <int> worker-choice: 0;
        in property <bool> busy: false;
        in property <bool> cancelling: false;
        in property <bool> choosing-folder: false;
        in property <bool> folder-picker-enabled: false;
        in property <bool> scan-enabled: true;
        in property <string> status: "Choisis un dossier à analyser.";
        in property <string> summary: "Aucun inventaire";
        in property <string> errors: "";
        in property <string> inventory-warning: "";
        in property <string> view-warning: "";
        in property <string> current-path: "";
        in property <string> entry-path: "";
        in property <string> selection-summary: "";
        in property <string> empty-message: "Lance une analyse pour explorer un dossier.";
        in property <bool> has-parent: false;
        in property <bool> has-previous-page: false;
        in property <bool> has-next-page: false;
        in property <bool> has-previous-detail-page: false;
        in property <bool> has-next-detail-page: false;
        in property <string> tree-page: "";
        in property <string> detail-page: "";
        in property <int> sort-order: 0;
        in property <[TreeRow]> rows;
        in property <[DetailRow]> details;
        callback show-legal();
        callback browse-folder(); callback path-edited();
        callback start-scan(); callback cancel-scan(); callback toggle-node(int);
        callback select-node(int); callback inspect-entry(int);
        callback go-parent(); callback sort-details(bool); callback change-page(bool, bool);

        VerticalLayout {
            padding: 14px; spacing: 8px;
            HorizontalLayout {
                height: 36px; spacing: 12px;
                Text { text: "SortSize"; color: #153550; font-size: 24px; font-weight: 700; vertical-alignment: center; }
                Text { text: "Analyse du stockage"; color: #4c6175; font-size: 13px; horizontal-stretch: 1; vertical-alignment: center; }
                Text { text: "LECTURE SEULE"; color: #226747; font-size: 11px; vertical-alignment: center; }
                Button { text: "À propos / Licences"; clicked => { root.show-legal(); } }
            }
            HorizontalLayout {
                spacing: 8px;
                LineEdit {
                    text <=> root.scan-path; placeholder-text: "Chemin du dossier à analyser";
                    accessible-label: "Dossier à analyser";
                    enabled: !root.busy && !root.choosing-folder && root.scan-enabled;
                    edited => { root.path-edited(); }
                    accepted => { root.start-scan(); }
                }
                Button {
                    text: "Parcourir…"; accessible-label: "Choisir un dossier à analyser";
                    enabled: root.folder-picker-enabled && root.scan-enabled && !root.busy && !root.choosing-folder;
                    clicked => { root.browse-folder(); }
                }
                ComboBox {
                    model: ["1 dossier à la fois", "2 dossiers en parallèle", "4 dossiers en parallèle", "8 dossiers en parallèle"];
                    current-index <=> root.worker-choice;
                    accessible-label: "Dossiers analysés simultanément";
                    enabled: !root.busy && !root.choosing-folder && root.scan-enabled;
                }
                Button { text: "Analyser"; enabled: !root.busy && !root.choosing-folder && root.scan-enabled; clicked => { root.start-scan(); } }
                Button { text: root.cancelling ? "Annulation…" : "Annuler"; enabled: root.busy && !root.cancelling; clicked => { root.cancel-scan(); } }
            }
            HorizontalLayout {
                spacing: 8px;
                Button { text: "↑ Parent"; accessible-label: "Revenir au dossier parent"; enabled: root.has-parent; clicked => { root.go-parent(); } }
                LineEdit { text: root.current-path; read-only: true; placeholder-text: "Dossier consulté"; accessible-label: "Chemin du dossier consulté"; }
            }
            HorizontalLayout {
                spacing: 10px;
                // Layout stretch avoids a window-size/minimum-size binding loop.
                Rectangle {
                    horizontal-stretch: 3; min-width: 260px;
                    background: #ffffff; border-color: #cbd7e2; border-width: 1px; border-radius: 5px;
                    VerticalLayout {
                        padding: 8px; spacing: 6px;
                        HorizontalLayout {
                            height: 30px;
                            Text { text: "DOSSIERS"; color: #4c6175; font-size: 11px; font-weight: 600; horizontal-stretch: 1; vertical-alignment: center; }
                            Text { text: "TAILLE LOGIQUE"; color: #4c6175; font-size: 10px; vertical-alignment: center; }
                        }
                        ListView {
                            for row in root.rows: Rectangle {
                                height: 34px;
                                background: row.selected ? #dcecf7 : #ffffff;
                                border-radius: 3px;
                                HorizontalLayout {
                                    padding-left: 3px + row.depth * 12px; padding-right: 4px; spacing: 4px;
                                    if row.expandable: Button {
                                        width: 25px; text: row.expanded ? "−" : "+";
                                        accessible-label: (row.expanded ? "Replier " : "Déplier ") + row.name;
                                        clicked => { root.toggle-node(row.node-id); }
                                    }
                                    if !row.expandable: Text { width: 25px; text: "·"; color: #91a0af; horizontal-alignment: center; vertical-alignment: center; }
                                    NameButton {
                                        label: row.name; horizontal-stretch: 1;
                                        action-label: "Consulter " + row.name;
                                        clicked => { root.select-node(row.node-id); }
                                    }
                                    Text { text: row.size; color: #153550; width: 84px; font-size: 11px; horizontal-alignment: right; vertical-alignment: center; }
                                }
                            }
                        }
                        if root.has-previous-page || root.has-next-page: HorizontalLayout {
                            spacing: 4px;
                            Button { text: "←"; accessible-label: "Page précédente des dossiers"; enabled: root.has-previous-page; clicked => { root.change-page(false, false); } }
                            Text { text: root.tree-page; color: #4c6175; font-size: 11px; horizontal-alignment: center; vertical-alignment: center; }
                            Button { text: "→"; accessible-label: "Page suivante des dossiers"; enabled: root.has-next-page; clicked => { root.change-page(false, true); } }
                        }
                        Rectangle { height: 1px; background: #d4dfe9; }
                        Text { text: "SOURCE LOCALE"; color: #4c6175; font-size: 10px; font-weight: 600; }
                        Text { text: "Tailles logiques par chemin\nLiens non suivis"; color: #4c6175; font-size: 12px; wrap: word-wrap; }
                    }
                }
                Rectangle {
                    horizontal-stretch: 7; min-width: 560px;
                    background: #ffffff; border-color: #cbd7e2; border-width: 1px; border-radius: 5px;
                    VerticalLayout {
                        padding: 8px; spacing: 6px;
                        Text { text: "CONTENU DU DOSSIER"; color: #4c6175; font-size: 11px; font-weight: 600; }
                        Text { text: root.selection-summary == "" ? "Aucun dossier sélectionné" : root.selection-summary; color: #153550; font-size: 13px; wrap: word-wrap; }
                        HorizontalLayout {
                            spacing: 6px;
                            Button { text: "Nom" + (root.sort-order == 2 ? " ↑" : root.sort-order == 3 ? " ↓" : ""); horizontal-stretch: 1; clicked => { root.sort-details(true); } }
                            Button { text: "Taille" + (root.sort-order == 0 ? " ↓" : root.sort-order == 1 ? " ↑" : ""); width: 104px; clicked => { root.sort-details(false); } }
                            Text { text: "Part du dossier"; color: #4c6175; width: 116px; font-size: 11px; horizontal-alignment: center; vertical-alignment: center; }
                            Text { text: "Type / état"; color: #4c6175; width: 112px; font-size: 11px; vertical-alignment: center; }
                        }
                        ListView {
                            for entry[index] in root.details: Rectangle {
                                height: 34px; background: mod(index, 2) == 0 ? #f4f7fa : #ffffff; border-radius: 3px;
                                HorizontalLayout {
                                    padding-left: 2px; padding-right: 0px; spacing: 6px;
                                    NameButton {
                                        label: entry.name; horizontal-stretch: 1;
                                        action-label: (entry.directory ? "Ouvrir " : "Afficher le chemin de ") + entry.name;
                                        clicked => { if entry.directory { root.select-node(entry.node-id); } else { root.inspect-entry(entry.node-id); } }
                                    }
                                    Text { text: entry.size; color: #153550; width: 104px; font-size: 12px; horizontal-alignment: right; vertical-alignment: center; }
                                    Rectangle {
                                        width: 116px; height: 22px; background: #e8edf3; border-radius: 3px;
                                        Rectangle { x: 0px; y: 0px; width: parent.width * entry.fraction; height: parent.height; background: #afd5eb; border-radius: 3px; }
                                        Text { text: entry.proportion; color: #153550; font-size: 11px; horizontal-alignment: center; vertical-alignment: center; }
                                    }
                                    Text { text: entry.note; color: #4c6175; width: 112px; font-size: 11px; vertical-alignment: center; overflow: elide; }
                                }
                            }
                        }
                        if root.details.length == 0: Text { text: root.empty-message; color: #4c6175; wrap: word-wrap; }
                        if root.has-previous-detail-page || root.has-next-detail-page: HorizontalLayout {
                            spacing: 8px;
                            Button { text: "Précédente"; enabled: root.has-previous-detail-page; clicked => { root.change-page(true, false); } }
                            Text { text: root.detail-page; color: #4c6175; font-size: 11px; horizontal-alignment: center; vertical-alignment: center; }
                            Button { text: "Suivante"; enabled: root.has-next-detail-page; clicked => { root.change-page(true, true); } }
                        }
                        if root.entry-path != "": LineEdit { text: root.entry-path; read-only: true; accessible-label: "Chemin complet de l’entrée"; }
                    }
                }
            }
            if root.inventory-warning != "" || root.view-warning != "": Text {
                text: root.inventory-warning + (root.inventory-warning != "" && root.view-warning != "" ? "\n" : "") + root.view-warning;
                color: #845112; font-size: 12px; wrap: word-wrap;
            }
            if root.errors != "": ScrollView { height: 70px; Text { text: root.errors; color: #845112; font-size: 12px; wrap: word-wrap; } }
            HorizontalLayout {
                spacing: 8px;
                Spinner { visible: root.busy; width: 20px; height: 20px; }
                Text { text: root.status; color: #153550; font-size: 12px; font-weight: 600; wrap: word-wrap; vertical-alignment: center; }
            }
            Text { text: root.summary; color: #4c6175; font-size: 12px; wrap: word-wrap; }
        }
    }
}

#[derive(Default)]
struct Controller {
    path_input: PathInput,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    folder_picker: Option<folder_picker::FolderPicker>,
    task: Option<Task>,
    tree: Option<Tree>,
    #[cfg(feature = "benchmarks")]
    benchmark_summary: Option<worker::BenchmarkSummary>,
}

impl Controller {
    fn start(&mut self, window: &SortSizeWindow) {
        if self.task.is_some() || window.get_choosing_folder() || !window.get_scan_enabled() {
            return;
        }
        let path = self.path_input.resolve(window.get_scan_path().as_str());
        if path.as_os_str().is_empty() {
            window.set_status("Saisis le chemin d'un dossier à analyser.".into());
            return;
        }
        self.tree = None;
        #[cfg(feature = "benchmarks")]
        {
            self.benchmark_summary = None;
        }
        window.set_rows(ModelRc::default());
        window.set_details(ModelRc::default());
        window.set_current_path("".into());
        window.set_entry_path("".into());
        window.set_selection_summary("".into());
        window.set_empty_message("Analyse en cours…".into());
        window.set_has_parent(false);
        window.set_has_previous_detail_page(false);
        window.set_has_next_detail_page(false);
        window.set_errors("".into());
        window.set_inventory_warning("".into());
        window.set_view_warning("".into());
        window.set_has_previous_page(false);
        window.set_has_next_page(false);
        window.set_summary("Préparation de l'inventaire…".into());
        window.set_cancelling(false);
        let workers = match window.get_worker_choice() {
            1 => 2,
            2 => 4,
            3 => 8,
            _ => 1,
        };
        match Task::start(path, workers) {
            Ok(task) => {
                self.task = Some(task);
                window.set_busy(true);
                window.set_status("Analyse en cours…".into());
            }
            Err(error) => {
                window.set_status("Impossible de démarrer l'analyse.".into());
                window.set_errors(error.to_string().escape_debug().to_string().into());
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn browse(&mut self, window: &SortSizeWindow) {
        self.begin_browse(window, |initial| {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Choisir un dossier à analyser")
                .set_parent(&window.window().window_handle());
            if !initial.as_os_str().is_empty() {
                dialog = dialog.set_directory(initial);
            }
            folder_picker::FolderPicker::start(move || dialog.pick_folder())
        });
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn begin_browse(
        &mut self,
        window: &SortSizeWindow,
        open: impl FnOnce(std::path::PathBuf) -> std::io::Result<folder_picker::FolderPicker>,
    ) {
        if self.task.is_some()
            || self.folder_picker.is_some()
            || !window.get_scan_enabled()
            || !window.get_folder_picker_enabled()
        {
            return;
        }
        match open(self.path_input.resolve(window.get_scan_path().as_str())) {
            Ok(picker) => {
                self.folder_picker = Some(picker);
                window.set_choosing_folder(true);
            }
            Err(error) => window.set_errors(
                format!(
                    "Impossible d’ouvrir le sélecteur : {}",
                    error.to_string().escape_debug()
                )
                .into(),
            ),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn poll_folder(&mut self, window: &SortSizeWindow) {
        let Some(result) = self.folder_picker.as_ref().and_then(|picker| picker.poll()) else {
            return;
        };
        self.folder_picker = None;
        window.set_choosing_folder(false);
        match result {
            Ok(Some(path)) => {
                window.set_scan_path(self.path_input.select(path).into());
            }
            // rfd also returns None when the desktop service is unavailable.
            // Preserve the previous source and inventory in either case.
            Ok(None) => {},
            Err(_) => window.set_errors("Le sélecteur s’est arrêté sans résultat. Réessaie ou saisis le chemin manuellement.".into()),
        }
    }

    fn cancel(&self, window: &SortSizeWindow) {
        if let Some(task) = &self.task {
            task.cancellation.cancel();
            window.set_cancelling(true);
            window.set_status("Annulation demandée…".into());
        }
    }

    fn poll(&mut self, window: &SortSizeWindow) {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        self.poll_folder(window);
        let Some(task) = &self.task else {
            return;
        };
        let (progress, completion) = {
            let mut mailbox = match task.mailbox.try_lock() {
                Ok(mailbox) => mailbox,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            (mailbox.progress.take(), mailbox.completion.take())
        };
        if let Some(progress) = progress {
            window.set_summary(progress.into());
        }
        if let Some(completion) = completion {
            self.task = None;
            window.set_busy(false);
            window.set_cancelling(false);
            match completion {
                Ok(result) => {
                    #[cfg(feature = "benchmarks")]
                    {
                        self.benchmark_summary = Some(result.benchmark_summary);
                    }
                    window.set_status(result.status.into());
                    window.set_summary(result.summary.into());
                    window.set_errors(result.errors.into());
                    window.set_inventory_warning(result.warning.into());
                    self.tree = Some(result.tree);
                    self.show_rows(window);
                }
                Err(error) => {
                    window.set_status("Analyse impossible.".into());
                    window.set_summary("Aucun inventaire disponible pour ce dossier.".into());
                    window.set_errors(error.into());
                    window.set_empty_message("Aucun inventaire disponible.".into());
                }
            }
        }
    }

    fn show_rows(&self, window: &SortSizeWindow) {
        if let Some(tree) = &self.tree {
            let (rows, limited) = tree.rows();
            let first = tree.offset(false) + 1;
            let last = tree.offset(false) + rows.len();
            window.set_has_previous_page(tree.offset(false) > 0);
            window.set_has_next_page(limited);
            window.set_rows(Rc::new(VecModel::from(rows)).into());
            window.set_tree_page(format!("{first}–{last}").into());
            let (details, limited) = tree.details();
            let first = tree.offset(true) + 1;
            let last = tree.offset(true) + details.len();
            window.set_has_previous_detail_page(tree.offset(true) > 0);
            window.set_has_next_detail_page(limited);
            window.set_details(Rc::new(VecModel::from(details)).into());
            window.set_detail_page(format!("{first}–{last}").into());
            window.set_current_path(tree.path().into());
            window.set_entry_path("".into());
            window.set_selection_summary(tree.selection_summary().into());
            window.set_has_parent(tree.has_parent());
            window.set_empty_message(tree.empty_message().into());
            window.set_sort_order(tree.sort() as i32);
        }
    }
}

fn connect(window: &SortSizeWindow) -> Rc<RefCell<Controller>> {
    let controller = Rc::new(RefCell::new(Controller::default()));
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        window.set_folder_picker_enabled(true);
        let weak = window.as_weak();
        let state = controller.clone();
        window.on_browse_folder(move || {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().browse(&window);
            }
        });
    }
    let state = controller.clone();
    window.on_path_edited(move || state.borrow_mut().path_input.edited());
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_start_scan(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().start(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_cancel_scan(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow().cancel(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_toggle_node(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
        {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.toggle(index as usize);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_change_page(move |details, next| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.change_page(details, next);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_select_node(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
        {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.select(index as usize);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_go_parent(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.parent();
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_details(move |name| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            if let Some(tree) = &mut state.tree {
                tree.sort_by(name);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_inspect_entry(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
            && let Some(tree) = &state.borrow().tree
            && let Some(path) = tree.entry_path(index as usize)
        {
            window.set_entry_path(path.into());
        }
    });
    controller
}

pub fn run() -> Result<(), slint::PlatformError> {
    let window = SortSizeWindow::new()?;
    let legal = LegalWindow::new()?;
    legal.set_notices(include_str!("legal-notice.txt").into());
    legal.set_license_text(include_str!("../../../LICENSE").into());
    let weak_legal = legal.as_weak();
    window.on_show_legal(move || {
        if let Some(legal) = weak_legal.upgrade()
            && let Err(error) = legal.show()
        {
            eprintln!("SortSize — Licences : {error}");
        }
    });
    let weak_legal = legal.as_weak();
    window.window().on_close_requested(move || {
        if let Some(legal) = weak_legal.upgrade() {
            let _ = legal.hide();
        }
        slint::CloseRequestResponse::HideWindow
    });
    #[cfg(target_os = "android")]
    {
        window.set_scan_enabled(false);
        window
            .set_status("Prototype Android : la sélection SAF/MediaStore reste à intégrer.".into());
    }
    let controller = connect(&window);
    #[cfg(not(target_os = "android"))]
    if let Ok(path) = std::env::current_dir() {
        window.set_scan_path(controller.borrow_mut().path_input.select(path).into());
    }
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().poll(&window);
        }
    });
    let result = window.run();
    timer.stop();
    controller.borrow_mut().task = None;
    result
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    if let Err(error) = slint::android::init(app) {
        eprintln!("Initialisation Android impossible : {error}");
        return;
    }
    if let Err(error) = run() {
        eprintln!("SortSize : {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    use slint::platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    struct TestPlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn gui_callbacks_scan_expand_and_recover_from_a_missing_root() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
        let window = SortSizeWindow::new().unwrap();
        let controller = connect(&window);
        // A worker publishing progress must never make the UI wait for its lock.
        let mailbox = std::sync::Arc::new(std::sync::Mutex::new(worker::Mailbox {
            progress: Some("progression différée".into()),
            completion: None,
        }));
        controller.borrow_mut().task = Some(Task {
            cancellation: Default::default(),
            mailbox: mailbox.clone(),
        });
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = mailbox.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let previous = window.get_summary();
        controller.borrow_mut().poll(&window);
        assert_eq!(window.get_summary(), previous);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        controller.borrow_mut().poll(&window);
        assert_eq!(window.get_summary(), "progression différée");
        controller.borrow_mut().task = None;
        window.invoke_start_scan();
        assert!(!window.get_busy());
        let fixture = TempDir::new().unwrap();
        fs::create_dir(fixture.path().join("été")).unwrap();
        fs::create_dir(fixture.path().join("été/nested")).unwrap();
        fs::write(fixture.path().join("été/data"), b"12345").unwrap();
        fs::write(fixture.path().join("empty"), []).unwrap();
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.set_worker_choice(2);
        window.invoke_start_scan();
        assert!(window.get_busy());
        window.invoke_start_scan();
        wait(&window, &controller);
        assert_eq!(window.get_status(), "Analyse terminée");
        assert!(window.get_summary().contains("5 octets"));
        assert!(window.get_summary().contains("4 dossiers simultanés"));
        assert_eq!(window.get_rows().row_count(), 2);
        assert_eq!(window.get_details().row_count(), 2);
        assert!(!window.get_has_parent());
        let folder = window.get_rows().row_data(1).unwrap();
        assert_eq!(folder.name, "été");
        window.invoke_toggle_node(folder.node_id);
        assert_eq!(window.get_rows().row_count(), 3);
        assert_eq!(window.get_rows().row_data(2).unwrap().name, "nested");
        window.invoke_select_node(folder.node_id);
        assert!(window.get_has_parent());
        assert!(window.get_current_path().ends_with("été"));
        assert_eq!(window.get_details().row_count(), 2);
        let nested = window.get_details().row_data(0).unwrap();
        let file = window.get_details().row_data(1).unwrap();
        assert_eq!(file.name, "data");
        assert_eq!(file.proportion, "100.0 %");
        window.invoke_inspect_entry(file.node_id);
        assert!(window.get_entry_path().ends_with("data"));
        window.invoke_select_node(nested.node_id);
        assert_eq!(window.get_details().row_count(), 0);
        assert!(window.get_empty_message().contains("vide"));
        window.invoke_go_parent();
        assert_eq!(window.get_details().row_count(), 2);
        assert!(window.get_entry_path().is_empty());
        window.invoke_sort_details(true);
        assert_eq!(window.get_sort_order(), 2);
        window.invoke_sort_details(true);
        assert_eq!(window.get_sort_order(), 3);
        window.invoke_go_parent();
        assert!(!window.get_has_parent());
        assert_eq!(window.get_details().row_count(), 2);
        window.invoke_toggle_node(0);
        assert_eq!(window.get_rows().row_count(), 1);
        window.set_scan_path(fixture.path().join("missing").to_str().unwrap().into());
        window.invoke_start_scan();
        assert_eq!(window.get_rows().row_count(), 0);
        assert_eq!(window.get_details().row_count(), 0);
        assert!(window.get_current_path().is_empty());
        wait(&window, &controller);
        assert_eq!(window.get_status(), "Analyse impossible.");
        assert!(!window.get_errors().is_empty());
        assert!(!window.get_busy());
        assert!(!window.get_cancelling());
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.invoke_start_scan();
        let token = controller
            .borrow()
            .task
            .as_ref()
            .unwrap()
            .cancellation
            .clone();
        window.invoke_cancel_scan();
        assert!(token.is_cancelled());
        assert!(window.get_cancelling());
        wait(&window, &controller);
        assert!(!window.get_cancelling());
        // A tiny scan may finish before the click; completion stays authoritative.
        window.invoke_start_scan();
        wait(&window, &controller);
        assert_eq!(window.get_status(), "Analyse terminée");
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        folder_selection_regressions(&window, &controller);
        if let Ok(destination) = std::env::var("SORTSIZE_GUI_SCREENSHOT") {
            let path = std::env::var("SORTSIZE_GUI_PREVIEW_ROOT")
                .unwrap_or_else(|_| fixture.path().to_string_lossy().into_owned());
            window.set_scan_path(path.into());
            window.invoke_start_scan();
            wait(&window, &controller);
            capture(&window, &adapter, &destination, (1200, 760));
            if let Ok(destination) = std::env::var("SORTSIZE_GUI_COMPACT_SCREENSHOT") {
                capture(&window, &adapter, &destination, (960, 600));
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn folder_selection_regressions(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        use folder_picker::FolderPicker;
        let fixture = TempDir::new().unwrap();
        let mut path = fixture.path().join("été 日本語 avec espaces");
        for _ in 0..8 {
            path = path.join("long-directory-name-0123456789");
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("data"), b"1234567").unwrap();
        let previous_path = window.get_scan_path();
        let previous_summary = window.get_summary();
        let previous_rows = window.get_rows().row_count();
        let (tx, rx) = std::sync::mpsc::channel();
        controller
            .borrow_mut()
            .begin_browse(window, |_| FolderPicker::start(move || rx.recv().unwrap()));
        assert!(window.get_choosing_folder());
        // Both commands are guarded even if invoked outside their disabled buttons.
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("duplicate dialog"));
        window.invoke_start_scan();
        assert!(!window.get_busy());
        controller.borrow_mut().poll(window);
        assert_eq!(window.get_scan_path(), previous_path);
        tx.send(Some(path.clone())).unwrap();
        wait_for_folder(window, controller);
        assert_eq!(window.get_scan_path(), path.to_string_lossy().as_ref());
        assert_eq!(
            controller
                .borrow()
                .path_input
                .resolve(window.get_scan_path().as_str()),
            path
        );
        assert_eq!(window.get_summary(), previous_summary);
        assert_eq!(window.get_rows().row_count(), previous_rows);
        assert!(!window.get_busy());
        // Cancellation keeps the native source, text and existing inventory.
        controller
            .borrow_mut()
            .begin_browse(window, |_| FolderPicker::start(|| None));
        wait_for_folder(window, controller);
        assert_eq!(
            controller
                .borrow()
                .path_input
                .resolve(window.get_scan_path().as_str()),
            path
        );
        assert_eq!(window.get_summary(), previous_summary);
        window.invoke_start_scan();
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("dialog during scan"));
        wait(window, controller);
        assert!(window.get_summary().contains("7 octets"));
        assert_eq!(window.get_status(), "Analyse terminée");
        // A failed thread start must leave the commands usable and the source intact.
        controller
            .borrow_mut()
            .begin_browse(window, |_| Err(std::io::Error::other("test failure")));
        assert!(!window.get_choosing_folder());
        assert!(window.get_errors().contains("test failure"));
        // A worker that exits without sending also releases the dialog guard.
        controller.borrow_mut().begin_browse(window, |_| {
            FolderPicker::start(|| panic!("test picker failure"))
        });
        wait_for_folder(window, controller);
        assert!(window.get_errors().contains("sans résultat"));
        // Disabled platforms/scan sources cannot open a dialog.
        window.set_scan_enabled(false);
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("disabled scan"));
        window.set_scan_enabled(true);
        window.set_folder_picker_enabled(false);
        controller
            .borrow_mut()
            .begin_browse(window, |_| panic!("disabled picker"));
        window.set_folder_picker_enabled(true);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            use std::os::unix::fs::PermissionsExt;
            let native = fixture
                .path()
                .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
            fs::create_dir(&native).unwrap();
            fs::write(native.join("data"), b"12345678901").unwrap();
            let selected = native.clone();
            controller
                .borrow_mut()
                .begin_browse(window, |_| FolderPicker::start(move || Some(selected)));
            wait_for_folder(window, controller);
            // Cancel after an unrepresentable selection must preserve its exact bytes.
            controller
                .borrow_mut()
                .begin_browse(window, |_| FolderPicker::start(|| None));
            wait_for_folder(window, controller);
            window.invoke_start_scan();
            wait(window, controller);
            assert_eq!(window.get_status(), "Analyse terminée");
            assert!(window.get_summary().contains("11 octets"));
            assert_eq!(fs::read(native.join("data")).unwrap(), b"12345678901");
            // User editing invalidates the native path even if undo restores the text.
            window.invoke_path_edited();
            window.invoke_start_scan();
            wait(window, controller);
            assert_eq!(window.get_status(), "Analyse impossible.");
            let blocked = fixture.path().join("inaccessible");
            fs::create_dir(&blocked).unwrap();
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
            let denied = fs::read_dir(&blocked).is_err();
            let selected = blocked.clone();
            controller
                .borrow_mut()
                .begin_browse(window, |_| FolderPicker::start(move || Some(selected)));
            wait_for_folder(window, controller);
            window.invoke_start_scan();
            wait(window, controller);
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
            if denied {
                assert!(window.get_status().contains("Analyse partielle"));
                assert!(!window.get_errors().is_empty());
            }
        }
        // Changing the text programmatically also cannot reuse a stale native value.
        window.set_scan_path(path.to_string_lossy().as_ref().into());
        window.invoke_path_edited();
        window.invoke_start_scan();
        wait(window, controller);
        assert_eq!(window.get_status(), "Analyse terminée");
        assert!(window.get_summary().contains("7 octets"));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn wait_for_folder(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while window.get_choosing_folder() {
            assert!(Instant::now() < deadline, "folder dialog did not finish");
            controller.borrow_mut().poll(window);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while window.get_busy() {
            assert!(Instant::now() < deadline, "worker did not finish");
            controller.borrow_mut().poll(window);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn capture(
        window: &SortSizeWindow,
        adapter: &MinimalSoftwareWindow,
        destination: &str,
        (width, height): (u32, u32),
    ) {
        use slint::Rgb8Pixel;
        use std::io::Write;
        window.show().unwrap();
        adapter.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        }));
        let mut output = fs::File::create(destination).unwrap();
        output
            .write_all(format!("P6\n{width} {height}\n255\n").as_bytes())
            .unwrap();
        for pixel in pixels {
            output.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
        }
        window.hide().unwrap();
    }
}
