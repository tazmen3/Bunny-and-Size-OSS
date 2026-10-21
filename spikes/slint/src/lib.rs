#[cfg(feature = "benchmarks")]
pub mod measure;

mod allocation_label;
mod date_label;
mod display_preferences;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod folder_picker;
mod owner_controller;
mod owner_view;
mod path_input;
#[cfg(any(target_os = "linux", windows))]
mod preferences_store;
mod tree;
mod treemap;
mod units;
#[cfg(any(target_os = "linux", windows))]
mod volume_view;
#[cfg(windows)]
mod window_placement;
mod worker;

use path_input::PathInput;
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use tree::Tree;
use worker::Task;

slint::slint! {
    import { Button, CheckBox, ComboBox, LineEdit, ListView, ScrollView, Spinner, TextEdit } from "std-widgets.slint";

    export component LegalWindow inherits Window {
        title: "SortSize — À propos / Licences";
        preferred-width: 760px; preferred-height: 560px;
        min-width: 580px; min-height: 400px;
        in property <string> product-version;
        in property <string> notices;
        in property <string> license-text;
        private property <bool> full-license: false;
        VerticalLayout {
            padding: 16px; spacing: 12px;
            Text { text: "SortSize " + root.product-version + " — À propos / Licences"; font-size: 22px; }
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

    export struct ColumnSetting { label: string, shown: bool, pixels: float, }
    export struct VolumeRow {
        name: string, summary: string, state: string,
        fraction: float, has-usage: bool, selectable: bool,
    }
    export struct TreeRow {
        node-id: int, depth: int, name: string, size: string,
        expandable: bool, expanded: bool, selected: bool,
    }
    export struct DetailRow {
        node-id: int, name: string, path: string, size: string, note: string,
        directory: bool, proportion: string, fraction: float,
        files: string, directories: string, modified: string, accessed: string, allocated: string, owner: string,
    }
    export struct TopRow { rank: int, name: string, size: string, path: string, }
    export struct MapTile {
        node-id: int, name: string, size: string, directory: bool,
        color-index: int, x: float, y: float, w: float, h: float,
    }
    component TreemapTile inherits Rectangle {
        in property <MapTile> tile;
        in property <bool> inspected;
        in property <bool> focused;
        callback activate();
        clip: true;
        background: tile.color-index == 0 ? #23658a : tile.color-index == 1 ? #386b47 :
            tile.color-index == 2 ? #704e89 : tile.color-index == 3 ? #8a4b35 :
            tile.color-index == 4 ? #356b72 : tile.color-index == 5 ? #805d20 :
            tile.color-index == 6 ? #4f5c70 : #6e532b;
        border-width: focused || inspected ? 3px : 1px;
        border-color: focused ? #ffdd75 : inspected ? #ffffff : #ffffff80;
        accessible-role: button;
        accessible-label: (tile.directory ? "Ouvrir le dossier " : tile.node-id >= 0 ? "Inspecter le fichier " : "Groupe ") + tile.name + ", " + tile.size;
        accessible-action-default => { root.activate(); }
        TouchArea { mouse-cursor: pointer; clicked => { root.activate(); } }
        if root.width > 70px && root.height > 24px: Text {
            x: 7px; y: 4px; width: max(0px, parent.width - 14px); height: 20px;
            text: (root.tile.directory ? "▸ " : "") + root.tile.name;
            color: white; font-size: 12px; font-weight: 600; overflow: elide;
        }
        if root.width > 70px && root.height > 48px: Text {
            x: 7px; y: 26px; width: max(0px, parent.width - 14px); height: 18px;
            text: root.tile.size; color: white; font-size: 11px; overflow: elide;
        }
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
        in property <string> product-version;
        title: "SortSize " + root.product-version + " — analyse du stockage";
        preferred-width: 1200px; preferred-height: 760px;
        min-width: 960px; min-height: 600px;
        background: #edf2f7;
        private property <bool> compact-view: false;
        changed height => { root.compact-view = root.height < 680px; }
        in-out property <bool> preferences-open: false;
        in property <[string]> column-labels: [@tr("Nom (toujours visible)"), @tr("Taille logique"), @tr("Taille allouée"), @tr("Propriétaire"), @tr("Part du dossier"), @tr("Fichiers récursifs"), @tr("Dossiers récursifs"), @tr("Type / état"), @tr("Dernière modification"), @tr("Dernier accès"), @tr("Chemin complet")];
        in property <[ColumnSetting]> column-settings;
        in property <float> table-pixels: 1946;
        in property <length> table-width: root.table-pixels * 1px;
        changed table-pixels => { root.detail-scroll-x = clamp(root.detail-scroll-x, min(0px, detail-columns.visible-width - max(detail-columns.visible-width, root.table-width)), 0px); }
        in property <bool> decimal-units: false;
        in property <int> preference-state: 0;
        in property <string> preference-status: ["", @tr("Enregistrement…"), @tr("Préférences enregistrées."), @tr("Sauvegarde impossible ; préférences conservées pour cette session."), @tr("Préférences illisibles ou version non prise en charge ; valeurs par défaut."), @tr("Lecture des préférences impossible ; valeurs par défaut."), @tr("Sauvegarde indisponible pour cette session."), @tr("Répertoire utilisateur indisponible ; préférences de session.")][root.preference-state];
        callback set-column(int, bool);
        callback set-units(bool);
        callback reset-display();
        in-out property <string> scan-path;
        in-out property <int> worker-choice: 0;
        in-out property <bool> collect-allocation: false;
        in-out property <bool> collect-owner: false;
        in-out property <bool> resolve-owner: false;
        private property <bool> owner-options: false;
        changed collect-owner => { if !root.collect-owner { root.resolve-owner = false; } }
        in property <bool> owner-sort-available: false;
        in property <bool> owner-sort-preparing: false;
        in property <string> owner-status: "";
        callback owner-resolution-changed();
        callback sort-owner();
        in property <bool> storage-enabled: false;
        in property <bool> storage-loading: false;
        in property <[VolumeRow]> volumes;
        in property <string> storage-status;
        in property <int> selected-volume: -1;
        callback refresh-volumes(bool);
        callback select-volume(int);
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
        in property <bool> allocation-sort-available: false;
        callback sort-allocated();
        callback sort-dates(bool);
        in property <string> modified-heading: "Dernière modification" + (root.sort-order == 8 ? " ↓" : root.sort-order == 9 ? " ↑" : "");
        in property <string> accessed-heading: "Dernier accès (UTC)" + (root.sort-order == 10 ? " ↓" : root.sort-order == 11 ? " ↑" : "");
        in-out property <bool> top-visible: false;
        in property <[TopRow]> top-rows;
        in property <string> top-note: "Lance une analyse pour afficher le Top 100.";
        in property <string> top-path;
        in property <int> top-selected: -1;
        in property <bool> top-retained: false;
        in-out property <length> top-scroll-y <=> top-view.content-y;
        callback select-top(int); callback reveal-top();
        in property <[TreeRow]> rows;
        in property <[DetailRow]> details;
        in-out property <length> detail-scroll-y <=> details-view.content-y;
        in-out property <length> detail-scroll-x <=> detail-columns.content-x;
        in property <[MapTile]> tiles;
        in property <string> treemap-note: "Lance une analyse pour afficher la Treemap.";
        in property <int> inspected-node: -1;
        in-out property <int> map-cursor: 0;
        in-out property <float> map-width: map-area.width / 1px;
        in-out property <float> map-height: map-area.height / 1px;
        changed map-width => { root.resize-treemap(root.map-width, root.map-height); }
        changed map-height => { root.resize-treemap(root.map-width, root.map-height); }
        callback resize-treemap(float, float);
        callback activate-tile(int);
        callback show-legal();
        callback browse-folder(); callback path-edited();
        callback start-scan(); callback cancel-scan(); callback toggle-node(int);
        callback select-node(int); callback inspect-entry(int);
        callback go-parent(); callback sort-details(bool); callback change-page(bool, bool);

        VerticalLayout {
            padding: root.compact-view ? 10px : 14px; spacing: root.compact-view ? 6px : 8px;
            HorizontalLayout {
                height: 36px; spacing: 12px;
                Text { text: "SortSize"; color: #153550; font-size: 24px; font-weight: 700; vertical-alignment: center; }
                Text { text: "Analyse du stockage"; color: #4c6175; font-size: 13px; horizontal-stretch: 1; vertical-alignment: center; }
                Text { text: "LECTURE SEULE"; color: #226747; font-size: 11px; vertical-alignment: center; }
                Button { text: @tr("Colonnes et affichage"); accessible-label: @tr("Configurer les colonnes et les unités"); clicked => { root.preferences-open = !root.preferences-open; } }
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
                Button { text: root.cancelling ? "Annulation…" : "Annuler"; accessible-label: "Annuler le scan ou la résolution des propriétaires"; enabled: (root.busy || root.resolve-owner) && !root.cancelling; clicked => { root.cancel-scan(); } }
            }
            HorizontalLayout {
                spacing: 8px;
                Button { text: root.top-visible ? "Arbre / Treemap" : "Top 100"; accessible-label: "Basculer entre Top 100 et exploration"; clicked => { root.top-visible = !root.top-visible; if root.top-visible { top-focus.focus(); } } }
                Button { text: "Treemap"; accessible-label: "Placer le focus sur la Treemap"; enabled: root.tiles.length > 0; clicked => { root.top-visible = false; map-focus.focus(); } }
                Button { text: "↑ Parent"; accessible-label: "Revenir au dossier parent"; enabled: root.has-parent; clicked => { root.go-parent(); } }
                CheckBox {
                    text: "Collecter les tailles allouées";
                    checked <=> root.collect-allocation;
                    accessible-label: "Collecter les tailles allouées lors du prochain scan";
                    enabled: !root.busy && !root.choosing-folder && root.scan-enabled;
                }
                Button { text: "Propriétaires" + (root.collect-owner ? " ✓" : "");
                    accessible-label: "Afficher ou masquer les options de collecte et de résolution des propriétaires";
                    clicked => { root.owner-options = !root.owner-options; }
                }
                LineEdit { text: root.current-path; read-only: true; placeholder-text: "Dossier consulté"; accessible-label: "Chemin du dossier consulté"; }
            }
            if root.owner-options: HorizontalLayout {
                spacing: 8px;
                CheckBox {
                    text: "Collecter les propriétaires"; checked <=> root.collect-owner;
                    toggled => { if !root.collect-owner { root.resolve-owner = false; root.owner-resolution-changed(); } }
                    accessible-label: "Collecter les propriétaires natifs lors du prochain scan";
                    enabled: !root.busy && !root.choosing-folder && root.scan-enabled;
                }
                CheckBox {
                    text: "Résoudre les noms (NSS / AD : réseau possible)"; checked <=> root.resolve-owner;
                    toggled => { root.owner-resolution-changed(); }
                    accessible-label: "Autoriser explicitement la résolution des noms ; NSS ou Active Directory peuvent accéder au réseau";
                    enabled: root.collect-owner && !root.busy && root.scan-enabled;
                }
                Button { text: "Arrêter la résolution"; enabled: root.resolve-owner;
                    clicked => { root.resolve-owner = false; root.owner-resolution-changed(); }
                }
            }
            Rectangle {
                HorizontalLayout {
                    width: 100%; height: 100%; visible: !root.top-visible;
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
                            if root.storage-enabled: VerticalLayout {
                                spacing: 3px;
                                HorizontalLayout {
                                    Text { text: @tr("SOURCES DE STOCKAGE"); font-size: 10px; color: #4c6175; font-weight: 600; vertical-alignment: center; horizontal-stretch: 1; }
                                    Button { text: @tr("Actualiser"); enabled: !root.storage-loading; clicked => { root.refresh-volumes(false); } }
                                }
                                Text { text: root.storage-status; font-size: 10px; color: #4c6175; overflow: elide; }
                                ListView {
                                    height: root.compact-view ? 112px : 164px;
                                    vertical-stretch: 0;
                                    for volume[index] in root.volumes: Rectangle {
                                        height: 78px; background: root.selected-volume == index ? #dcecf7 : #f4f7fa;
                                        VerticalLayout {
                                            padding: 3px; spacing: 2px;
                                            Rectangle { height: 25px;
                                                Button { width: 100%; height: 100%; text: "";
                                                    enabled: volume.selectable && !root.busy && !root.choosing-folder && root.scan-enabled;
                                                    accessible-label: @tr("Sélectionner comme prochaine source : ") + volume.name;
                                                    clicked => { root.select-volume(index); }
                                                }
                                                Text { x: 5px; width: parent.width - 10px; height: 100%; text: volume.name; font-size: 11px; color: #153550; overflow: elide; vertical-alignment: center; }
                                            }
                                            Text { text: volume.summary; color: #153550; font-size: 10px; overflow: elide; }
                                            Rectangle { height: 4px; background: #d4dfe9; visible: volume.has-usage;
                                                Rectangle { height: 100%; width: parent.width * volume.fraction; background: #397dab; }
                                            }
                                            Text { text: volume.state; color: #4c6175; font-size: 10px; overflow: elide; }
                                        }
                                    }
                                }
                                Button { text: @tr("Interroger réseau / type inconnu"); enabled: !root.storage-loading;
                                    accessible-label: @tr("Interroger les capacités des sources réseau ou de type inconnu ; accès réseau possible");
                                    clicked => { root.refresh-volumes(true); }
                                }
                            }
                            if !root.compact-view: Text { text: "SOURCE LOCALE"; color: #4c6175; font-size: 10px; font-weight: 600; }
                            if root.owner-status != "": Text { text: root.owner-status; color: #4c6175; font-size: 11px; wrap: word-wrap; }
                            Text { text: (root.compact-view ? @tr("Logique par chemin · liens non suivis\nAlloué : hardlinks répétés") : "Tailles logiques par chemin\nAlloué : par chemin, hardlinks répétés\nLiens non suivis") + (root.sort-order == 4 || root.sort-order == 5 ? "\nTri alloué : ≥ = borne connue\nClassement provisoire" : ""); color: #4c6175; font-size: root.compact-view ? 10px : 12px; wrap: word-wrap; }
                        }
                    }
                    Rectangle {
                        horizontal-stretch: 7; min-width: 560px;
                        background: #ffffff; border-color: #cbd7e2; border-width: 1px; border-radius: 5px;
                        VerticalLayout {
                            padding: 8px; spacing: root.compact-view ? 4px : 6px;
                            Text { text: "CONTENU DU DOSSIER · " + @tr("Tri : ") + [@tr("Taille ↓"), @tr("Taille ↑"), @tr("Nom ↑"), @tr("Nom ↓"), @tr("Alloué ↓"), @tr("Alloué ↑"), @tr("Propriétaire ↑"), @tr("Propriétaire ↓"), @tr("Modification récente"), @tr("Modification ancienne"), @tr("Accès récent"), @tr("Accès ancien")][root.sort-order]; color: #4c6175; font-size: 11px; font-weight: 600; }
                            Text { text: root.selection-summary == "" ? "Aucun dossier sélectionné" : root.selection-summary; color: #153550; font-size: 13px; wrap: word-wrap; }
                            HorizontalLayout {
                                height: 24px;
                                Text { text: "TREEMAP · TAILLE LOGIQUE"; color: #4c6175; font-size: 11px; font-weight: 600; horizontal-stretch: 1; }
                                Text { text: "Flèches · Entrée · Retour arrière"; color: #4c6175; font-size: 10px; }
                            }
                            map-area := Rectangle {
                                min-height: 80px; preferred-height: root.compact-view ? 100px : 190px; max-height: root.compact-view ? 100px : 230px;
                                vertical-stretch: 0; background: #edf2f7; clip: true;
                                accessible-role: groupbox; accessible-label: "Treemap des tailles logiques observées";
                                map-focus := FocusScope {
                                    width: 100%; height: 100%;
                                    accessible-role: groupbox;
                                    accessible-label: root.tiles.length > 0 ? root.tiles[root.map-cursor].name + ", " + root.tiles[root.map-cursor].size : "Aucune surface mesurée";
                                    key-pressed(event) => {
                                        if event.text == Key.RightArrow || event.text == Key.DownArrow {
                                            root.map-cursor = mod(root.map-cursor + 1, max(1, root.tiles.length));
                                            return accept;
                                        }
                                        if event.text == Key.LeftArrow || event.text == Key.UpArrow {
                                            root.map-cursor = mod(root.map-cursor + max(1, root.tiles.length) - 1, max(1, root.tiles.length));
                                            return accept;
                                        }
                                        if event.text == Key.Return || event.text == Key.Space {
                                            root.activate-tile(root.map-cursor); return accept;
                                        }
                                        if event.text == Key.Backspace { root.go-parent(); return accept; }
                                        return reject;
                                    }
                                    for tile[index] in root.tiles: TreemapTile {
                                        x: parent.width * tile.x; y: parent.height * tile.y;
                                        width: parent.width * tile.w; height: parent.height * tile.h;
                                        tile: tile; inspected: tile.node-id >= 0 && tile.node-id == root.inspected-node;
                                        focused: map-focus.has-focus && root.map-cursor == index;
                                        activate => { root.map-cursor = index; map-focus.focus(); root.activate-tile(index); }
                                    }
                                    if root.tiles.length == 0: Text {
                                        text: "Aucune surface mesurée"; color: #4c6175; horizontal-alignment: center; vertical-alignment: center;
                                    }
                                }
                            }
                            if map-focus.has-focus && root.tiles.length > 0: Text {
                                text: root.tiles[root.map-cursor].name + " · " + root.tiles[root.map-cursor].size;
                                color: #153550; font-size: 12px; overflow: elide;
                            }
                            Text { text: root.treemap-note; color: #4c6175; font-size: 11px; wrap: word-wrap; }
                            // One horizontal viewport owns both header and rows. The inner
                            // ListView retains vertical virtualization and 34px reveal offsets.
                            detail-columns := ScrollView {
                                min-width: 0px;
                                content-width: max(self.visible-width, root.table-width);
                                content-height: self.visible-height;
                                vertical-scrollbar-policy: always-off;
                                Rectangle {
                                    width: detail-columns.content-width;
                                    height: detail-columns.visible-height;
                                    VerticalLayout {
                                        spacing: 6px;
                                        Rectangle {
                                            height: 34px;
                                            HorizontalLayout {
                                                x: 0px; height: 34px; width: root.table-width; padding-left: 2px; padding-right: 0px; spacing: 6px;
                                                Button { text: "Nom" + (root.sort-order == 2 ? " ↑" : root.sort-order == 3 ? " ↓" : ""); width: root.column-settings[0].pixels * 1px; clicked => { root.sort-details(true); } }
                                                if root.column-settings[1].shown: Button { text: "Taille" + (root.sort-order == 0 ? " ↓" : root.sort-order == 1 ? " ↑" : ""); width: root.column-settings[1].pixels * 1px; clicked => { root.sort-details(false); } }
                                                if root.column-settings[2].shown: Button {
                                                    text: "Alloué / Par chemin" + (root.sort-order == 4 ? " ↓" : root.sort-order == 5 ? " ↑" : "");
                                                    width: root.column-settings[2].pixels * 1px; enabled: root.allocation-sort-available;
                                                    clicked => { root.sort-allocated(); }
                                                }
                                                if root.column-settings[3].shown: Button {
                                                    text: "Propriétaire" + (root.sort-order == 6 ? " ↑" : root.sort-order == 7 ? " ↓" : "");
                                                    width: root.column-settings[3].pixels * 1px; enabled: root.owner-sort-available && !root.owner-sort-preparing;
                                                    accessible-label: "Trier les propriétaires ; appliquer les noms disponibles à cet instant";
                                                    clicked => { root.sort-owner(); }
                                                }
                                                if root.column-settings[4].shown: Text { text: "Part du dossier"; color: #4c6175; width: root.column-settings[4].pixels * 1px; font-size: 11px; horizontal-alignment: center; vertical-alignment: center; }
                                                if root.column-settings[5].shown: Text { text: "Fichiers\nrécursifs"; color: #4c6175; width: root.column-settings[5].pixels * 1px; font-size: 11px; horizontal-alignment: right; vertical-alignment: center; }
                                                if root.column-settings[6].shown: Text { text: "Dossiers\nrécursifs"; color: #4c6175; width: root.column-settings[6].pixels * 1px; font-size: 11px; horizontal-alignment: right; vertical-alignment: center; }
                                                if root.column-settings[7].shown: Text { text: "Type / état"; color: #4c6175; width: root.column-settings[7].pixels * 1px; font-size: 11px; vertical-alignment: center; }
                                                if root.column-settings[8].shown: Button {
                                                    text: root.modified-heading; width: root.column-settings[8].pixels * 1px;
                                                    accessible-label: root.modified-heading + "; trier du plus récent au plus ancien, puis inverser";
                                                    clicked => { root.sort-dates(false); }
                                                }
                                                if root.column-settings[9].shown: Button {
                                                    text: root.accessed-heading; width: root.column-settings[9].pixels * 1px;
                                                    accessible-label: root.accessed-heading + "; selon la politique du système ; trier du plus récent au plus ancien, puis inverser";
                                                    clicked => { root.sort-dates(true); }
                                                }
                                                if root.column-settings[10].shown: Text { text: "Chemin complet"; color: #4c6175; width: root.column-settings[10].pixels * 1px; font-size: 11px; vertical-alignment: center; }
                                            }
                                        }
                                        details-view := ListView {
                                            horizontal-scrollbar-policy: always-off;
                                            for entry[index] in root.details: Rectangle {
                                                height: 34px; background: entry.node-id == root.inspected-node ? #dcecf7 : mod(index, 2) == 0 ? #f4f7fa : #ffffff; border-radius: 3px;
                                                HorizontalLayout {
                                                    x: 0px; width: root.table-width; padding-left: 2px; padding-right: 0px; spacing: 6px;
                                                    NameButton {
                                                        label: entry.name; width: root.column-settings[0].pixels * 1px;
                                                        action-label: (entry.directory ? "Ouvrir " : "Afficher le chemin de ") + entry.name;
                                                        clicked => { if entry.directory { root.select-node(entry.node-id); } else { root.inspect-entry(entry.node-id); } }
                                                    }
                                                    if root.column-settings[1].shown: Text { text: entry.size; color: #153550; width: root.column-settings[1].pixels * 1px; font-size: 12px; horizontal-alignment: right; vertical-alignment: center; }
                                                    if root.column-settings[2].shown: Text { text: entry.allocated; color: #153550; width: root.column-settings[2].pixels * 1px; font-size: 11px; horizontal-alignment: right; vertical-alignment: center; }
                                                    if root.column-settings[3].shown: NameButton {
                                                        label: entry.owner; width: root.column-settings[3].pixels * 1px;
                                                        action-label: "Propriétaire de " + entry.name + " : " + entry.owner;
                                                        clicked => { root.inspect-entry(entry.node-id); }
                                                    }
                                                    if root.column-settings[4].shown: Rectangle {
                                                        width: root.column-settings[4].pixels * 1px; height: 22px; background: #e8edf3; border-radius: 3px;
                                                        Rectangle { x: 0px; y: 0px; width: parent.width * entry.fraction; height: parent.height; background: #afd5eb; border-radius: 3px; }
                                                        Text { text: entry.proportion; color: #153550; font-size: 11px; horizontal-alignment: center; vertical-alignment: center; }
                                                    }
                                                    if root.column-settings[5].shown: Text { text: entry.files; color: #153550; width: root.column-settings[5].pixels * 1px; font-size: 11px; horizontal-alignment: right; vertical-alignment: center; }
                                                    if root.column-settings[6].shown: Text { text: entry.directories; color: #153550; width: root.column-settings[6].pixels * 1px; font-size: 11px; horizontal-alignment: right; vertical-alignment: center; }
                                                    if root.column-settings[7].shown: Text { text: entry.note; color: #4c6175; width: root.column-settings[7].pixels * 1px; font-size: 11px; vertical-alignment: center; overflow: elide; }
                                                    if root.column-settings[8].shown: Text { text: entry.modified; color: #153550; width: root.column-settings[8].pixels * 1px; font-size: 11px; vertical-alignment: center; }
                                                    if root.column-settings[9].shown: Text { text: entry.accessed; color: #153550; width: root.column-settings[9].pixels * 1px; font-size: 11px; vertical-alignment: center; }
                                                    if root.column-settings[10].shown: NameButton {
                                                        label: entry.path; width: root.column-settings[10].pixels * 1px;
                                                        action-label: "Afficher le chemin complet : " + entry.path;
                                                        clicked => { root.inspect-entry(entry.node-id); }
                                                    }
                                                }
                                            }
                                        }
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
                            LineEdit { text: root.entry-path; placeholder-text: "Sélectionne un chemin pour le lire en entier"; read-only: true; accessible-label: "Chemin complet de l’entrée"; }
                        }
                    }
                }
                Rectangle {
                    width: 100%; height: 100%; visible: root.top-visible;
                    background: white; border-width: 1px; border-color: #cbd7e2; border-radius: 5px;
                    VerticalLayout {
                        padding: 8px; spacing: 6px;
                        Text { text: "TOP 100 · PLUS GROS FICHIERS"; color: #153550; font-size: 15px; font-weight: 600; }
                        Text { text: root.top-note; color: #4c6175; font-size: 12px; wrap: word-wrap; }
                        HorizontalLayout {
                            Text { text: "Rang"; width: 42px; color: #4c6175; }
                            Text { text: "Nom"; width: 200px; color: #4c6175; }
                            Text { text: "Taille logique"; width: 110px; color: #4c6175; }
                            Text { text: "Chemin complet"; horizontal-stretch: 1; color: #4c6175; }
                        }
                        top-focus := FocusScope {
                            accessible-role: groupbox;
                            accessible-label: root.top-selected >= 0 ? "Top 100, rang " + root.top-rows[root.top-selected].rank + ", " + root.top-rows[root.top-selected].name : "Top 100, aucun fichier sélectionné";
                            key-pressed(event) => {
                                if root.top-rows.length > 0 {
                                    if event.text == Key.DownArrow || event.text == Key.UpArrow || event.text == Key.Home || event.text == Key.End {
                                        root.select-top(event.text == Key.Home ? 0 : event.text == Key.End ? root.top-rows.length - 1 : clamp(root.top-selected + (event.text == Key.UpArrow ? -1 : 1), 0, root.top-rows.length - 1));
                                        root.top-scroll-y = min(0px, max(top-view.visible-height - top-view.content-height, -max(0, root.top-selected - 2) * 34px));
                                        return accept;
                                    }
                                    if event.text == Key.Return { root.reveal-top(); if !root.top-visible { map-focus.focus(); } return accept; }
                                }
                                return reject;
                            }
                            top-view := ListView {
                                width: 100%; height: 100%;
                                // Fixed-height rows: avoid a virtualized content-height
                                // estimate leaving the final row clipped on Windows.
                                content-height: root.top-rows.length * 34px;
                                for item[index] in root.top-rows: Rectangle {
                                    height: 34px;
                                    background: root.top-selected == index ? #dcecf7 : mod(index, 2) == 0 ? #f4f7fa : white;
                                    border-width: top-focus.has-focus && root.top-selected == index ? 1px : 0px;
                                    border-color: #23658a;
                                    accessible-role: button; accessible-label: item.rank + ", " + item.name + ", " + item.size + ", " + item.path;
                                    accessible-action-default => { root.select-top(index); }
                                    HorizontalLayout {
                                        spacing: 8px; padding-left: 4px; padding-right: 4px;
                                        Text { text: item.rank; width: 38px; color: #153550; vertical-alignment: center; }
                                        Text { text: item.name; width: 192px; color: #153550; vertical-alignment: center; overflow: elide; }
                                        Text { text: item.size; width: 102px; color: #153550; vertical-alignment: center; }
                                        Text { text: item.path; horizontal-stretch: 1; color: #153550; vertical-alignment: center; overflow: elide; }
                                    }
                                    TouchArea { clicked => { root.select-top(index); top-focus.focus(); } }
                                }
                            }
                        }
                        if root.top-rows.length == 0: Text { text: "Aucun fichier observé dans ce périmètre."; color: #4c6175; }
                        Text { text: "↑ ↓ · Début / Fin : sélection · Entrée : révéler dans le tableau"; color: #4c6175; font-size: 11px; }
                        LineEdit { text: root.top-path; read-only: true; accessible-label: "Chemin natif complet du fichier classé, unités non Unicode échappées"; }
                        HorizontalLayout {
                            Button { text: "Révéler dans le tableau"; enabled: root.top-retained; clicked => { root.reveal-top(); if !root.top-visible { map-focus.focus(); } } }
                            Text { text: root.top-selected < 0 ? "Sélectionne un fichier." : root.top-retained ? "Fichier retenu dans l’inventaire." : "Chemin absent de l’inventaire retenu ; aucun nœud à révéler."; color: #4c6175; font-size: 12px; wrap: word-wrap; }
                        }
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
        if root.preferences-open: Rectangle {
            x: max(16px, (root.width - self.width) / 2); y: 70px;
            width: min(620px, root.width - 32px); height: min(480px, root.height - 90px);
            background: #ffffff; border-width: 2px; border-color: #397dab; border-radius: 6px;
            VerticalLayout {
                padding: 12px; spacing: 8px;
                Text { text: @tr("Colonnes et affichage"); font-size: 18px; color: #153550; }
                Text { text: @tr("Le nom reste visible. Le tri actif est conservé."); font-size: 12px; color: #4c6175; }
                ListView {
                    for column[index] in root.column-settings: CheckBox {
                        height: 26px; text: column.label; checked: column.shown;
                        enabled: index != 0; accessible-label: column.label;
                        toggled => { root.set-column(index, self.checked); }
                    }
                }
                HorizontalLayout {
                    Text { text: @tr("Unités"); vertical-alignment: center; }
                    ComboBox {
                        model: [@tr("Binaire (KiB, MiB, GiB)"), @tr("Décimal (kB, MB, GB)")];
                        current-index: root.decimal-units ? 1 : 0;
                        accessible-label: @tr("Unités des tailles affichées");
                        selected(value) => { root.set-units(self.current-index == 1); }
                    }
                }
                Text { text: root.preference-status; font-size: 11px; color: #845112; wrap: word-wrap; }
                HorizontalLayout {
                    Button { text: @tr("Réinitialiser l’affichage"); clicked => { root.reset-display(); } }
                    Button { text: @tr("Fermer"); clicked => { root.preferences-open = false; } }
                }
            }
        }

    }
}

#[derive(Default)]
struct Controller {
    preferences: display_preferences::Preferences,
    #[cfg(any(target_os = "linux", windows))]
    preference_store: Option<preferences_store::Store>,
    scan_labels: Option<worker::DisplayLabels>,
    warning_labels: Option<worker::DisplayLabels>,
    path_input: PathInput,
    #[cfg(any(target_os = "linux", windows))]
    sources: volume_view::Sources,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    folder_picker: Option<folder_picker::FolderPicker>,
    task: Option<Task>,
    tree: Option<Tree>,
    generation: u64,
    owner_task: Option<owner_view::Task>,
    owner_page: u64,
    owner_page_key: Option<owner_controller::PageKey>,
    owner_names: std::sync::Arc<[owner_view::DisplayOwner]>,
    owner_sort_serial: u64,
    owner_sort_pending: Option<(u64, usize)>,
    owner_stats: sortsize_owner_resolution::ResolverStats,
    #[cfg(feature = "benchmarks")]
    owner_patch_max_ms: f64,
    #[cfg(feature = "benchmarks")]
    owner_sort_prepare_ms: f64,
    #[cfg(feature = "benchmarks")]
    owner_patch_rows: usize,
    #[cfg(feature = "benchmarks")]
    benchmark_summary: Option<worker::BenchmarkSummary>,
}

impl Controller {
    fn show_preferences(&self, window: &SortSizeWindow) {
        let labels = window.get_column_labels();
        let columns = (0..11)
            .map(|i| ColumnSetting {
                label: labels.row_data(i).unwrap_or_default(),
                shown: i == 0 || self.preferences.visible[i],
                pixels: f32::from(display_preferences::WIDTHS[i]),
            })
            .collect::<Vec<_>>();
        let model = window.get_column_settings();
        if model.row_count() == columns.len() {
            // Keep checkbox instances/focus while a keyboard user toggles them.
            for (index, column) in columns.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&column) {
                    model.set_row_data(index, column);
                }
            }
        } else {
            window.set_column_settings(Rc::new(VecModel::from(columns)).into());
        }
        window.set_table_pixels(f32::from(self.preferences.width()));
        window.set_decimal_units(self.preferences.units == units::Units::Decimal);
    }
    fn display_changed(&mut self, window: &SortSizeWindow, previous: units::Units) {
        self.show_preferences(window);
        if previous != self.preferences.units {
            self.update_units(window);
        }
        #[cfg(any(target_os = "linux", windows))]
        if let Some(store) = &mut self.preference_store {
            store.submit(self.preferences.clone());
            window.set_preference_state(1);
        }
    }
    fn update_units(&mut self, window: &SortSizeWindow) {
        let units = self.preferences.units;
        if let Some(tree) = &mut self.tree {
            tree.set_units(units);
            // Patch size fields only. Do not reset pagination, selection, scroll,
            // owner generations, names, native identities or path strings.
            let rows = window.get_rows();
            for i in 0..rows.row_count() {
                if let Some(mut row) = rows.row_data(i) {
                    row.size = tree.size_label(row.node_id as usize);
                    rows.set_row_data(i, row);
                }
            }
            let details = window.get_details();
            for i in 0..details.row_count() {
                if let Some(mut row) = details.row_data(i) {
                    row.size = tree.size_label(row.node_id as usize);
                    row.allocated = tree.allocation_label(row.node_id as usize);
                    details.set_row_data(i, row);
                }
            }
            window.set_top_rows(Rc::new(VecModel::from(tree.top_rows.clone())).into());
            window.set_selection_summary(tree.selection_summary().into());
            self.show_treemap(window);
        }
        if let Some(labels) = &self.scan_labels {
            window.set_summary(labels.get(units).into());
        }
        if let Some(labels) = &self.warning_labels {
            window.set_inventory_warning(labels.get(units).into());
        }
        #[cfg(any(target_os = "linux", windows))]
        self.show_sources(window);
    }
    #[cfg(any(target_os = "linux", windows))]
    fn refresh_sources(&mut self, window: &SortSizeWindow, remote: bool) {
        self.sources
            .refresh(sortsize_core::volumes::DiscoveryOptions {
                probe_remote: remote,
                explicit_unc: {
                    #[cfg(windows)]
                    {
                        let path = self.path_input.resolve(window.get_scan_path().as_str());
                        if remote && sortsize_core::volumes::unc_source(&path).is_ok() {
                            Some(path)
                        } else {
                            None
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        None
                    }
                },
            });
        self.show_sources(window);
    }
    #[cfg(any(target_os = "linux", windows))]
    fn show_sources(&self, window: &SortSizeWindow) {
        window.set_storage_loading(self.sources.loading());
        window.set_storage_status(self.sources.status().into());
        if let Some(prepared) = &self.sources.current {
            window.set_volumes(
                Rc::new(VecModel::from(prepared.rows_for(self.preferences.units))).into(),
            );
            let path = self.path_input.resolve(window.get_scan_path().as_str());
            let selected = prepared
                .discovery
                .volumes
                .iter()
                .position(|v| v.mounts.first().is_some_and(|m| m.path == path));
            window.set_selected_volume(selected.map(|i| i as i32).unwrap_or(-1));
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    fn select_volume(&mut self, window: &SortSizeWindow, index: i32) {
        if self.task.is_some()
            || window.get_busy()
            || window.get_choosing_folder()
            || !window.get_scan_enabled()
        {
            return;
        }
        let Some(volume) = usize::try_from(index)
            .ok()
            .and_then(|i| self.sources.current.as_ref()?.discovery.volumes.get(i))
        else {
            return;
        };
        if volume.availability != sortsize_core::volumes::VolumeAvailability::Available {
            return;
        }
        let Some(mount) = volume.mounts.first() else {
            return;
        };
        window.set_scan_path(self.path_input.select(mount.path.clone()).into());
        window.set_selected_volume(index);
        window.set_resolve_owner(false);
        self.resolution_changed(window);
    }
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
        self.scan_labels = None;
        self.warning_labels = None;
        self.generation = self.generation.wrapping_add(1);
        self.owner_page_key = None;
        self.owner_names = Default::default();
        self.owner_sort_pending = None;
        self.owner_sort_serial = self.owner_sort_serial.wrapping_add(1);
        window.set_owner_sort_preparing(false);
        self.request_owners(window, None);
        #[cfg(feature = "benchmarks")]
        {
            self.benchmark_summary = None;
        }
        window.set_top_rows(ModelRc::default());
        window.set_top_selected(-1);
        window.set_top_path("".into());
        window.set_top_retained(false);
        window.set_top_note("Analyse en cours…".into());
        window.set_top_scroll_y(0.0);
        window.set_rows(ModelRc::default());
        window.set_details(ModelRc::default());
        window.set_detail_scroll_x(0.0);
        window.set_tiles(ModelRc::default());
        window.set_treemap_note("Analyse en cours…".into());
        window.set_inspected_node(-1);
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
        match Task::start(
            path,
            workers,
            window.get_collect_allocation(),
            window.get_collect_owner(),
        ) {
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
                window.set_selected_volume(-1);
                window.set_resolve_owner(false);
                self.resolution_changed(window);
            }
            // rfd also returns None when the desktop service is unavailable.
            // Preserve the previous source and inventory in either case.
            Ok(None) => {},
            Err(_) => window.set_errors("Le sélecteur s’est arrêté sans résultat. Réessaie ou saisis le chemin manuellement.".into()),
        }
    }

    fn cancel(&mut self, window: &SortSizeWindow) {
        window.set_resolve_owner(false);
        self.resolution_changed(window);
        if let Some(task) = &self.task {
            task.cancellation.cancel();
            window.set_cancelling(true);
            window.set_status("Annulation demandée…".into());
        }
    }

    fn poll(&mut self, window: &SortSizeWindow) {
        #[cfg(any(target_os = "linux", windows))]
        if let Some(success) = self
            .preference_store
            .as_ref()
            .and_then(|store| store.poll())
        {
            window.set_preference_state(if success { 2 } else { 3 });
        }
        #[cfg(any(target_os = "linux", windows))]
        if self.sources.poll() {
            self.show_sources(window);
        }
        self.poll_owners(window);
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
            window.set_summary(progress.get(self.preferences.units).into());
            self.scan_labels = Some(progress);
        }
        if let Some(completion) = completion {
            self.task = None;
            window.set_busy(false);
            window.set_cancelling(false);
            match completion {
                Ok(mut result) => {
                    result.tree.set_units(self.preferences.units);
                    #[cfg(feature = "benchmarks")]
                    {
                        self.benchmark_summary = Some(result.benchmark_summary);
                    }
                    window.set_status(result.status.into());
                    window.set_summary(result.summary.get(self.preferences.units).into());
                    self.scan_labels = Some(result.summary);
                    window.set_errors(result.errors.into());
                    window.set_inventory_warning(result.warning.get(self.preferences.units).into());
                    self.warning_labels = Some(result.warning);
                    window
                        .set_top_rows(Rc::new(VecModel::from(result.tree.top_rows.clone())).into());
                    window.set_top_note(result.tree.top_note.clone().into());
                    self.tree = Some(result.tree);
                    self.show_rows(window);
                }
                Err(error) => {
                    window.set_top_note("Aucun classement disponible : analyse impossible.".into());
                    window.set_status("Analyse impossible.".into());
                    window.set_summary("Aucun inventaire disponible pour ce dossier.".into());
                    window.set_errors(error.into());
                    window.set_empty_message("Aucun inventaire disponible.".into());
                    window.set_treemap_note("Aucun inventaire disponible.".into());
                }
            }
        }
    }

    fn select_top(&self, window: &SortSizeWindow, rank: i32) {
        if rank >= 0
            && let Some((path, node)) = self
                .tree
                .as_ref()
                .and_then(|t| t.top_selection(rank as usize))
        {
            window.set_top_selected(rank);
            window.set_top_path(path.into());
            window.set_top_retained(node.is_some());
        }
    }
    fn reveal_top(&mut self, window: &SortSizeWindow) {
        let rank = window.get_top_selected();
        if rank >= 0
            && let Some((_, Some(index))) = self
                .tree
                .as_ref()
                .and_then(|t| t.top_selection(rank as usize))
        {
            self.tree.as_mut().unwrap().select_entry_parent(index);
            window.set_top_visible(false);
            self.inspect(window, index);
            if let Some(cursor) = window
                .get_tiles()
                .iter()
                .position(|tile| tile.node_id == index as i32)
            {
                window.set_map_cursor(cursor as i32);
            }
        }
    }
    fn show_rows(&mut self, window: &SortSizeWindow) {
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
            window.set_inspected_node(-1);
            window.set_detail_scroll_y(0.0);
            window.set_map_cursor(0);
            self.show_treemap(window);
            window.set_selection_summary(tree.selection_summary().into());
            window.set_has_parent(tree.has_parent());
            window.set_empty_message(tree.empty_message().into());
            window.set_sort_order(tree.sort() as i32);
            window.set_allocation_sort_available(tree.allocation_sort_available());
            window.set_owner_sort_available(tree.owner_data().is_some());
        }
        self.request_owners(window, None);
        self.apply_owner_names(window);
    }
    fn show_treemap(&self, window: &SortSizeWindow) {
        if let Some(tree) = &self.tree {
            window.set_tiles(
                Rc::new(VecModel::from(tree.treemap(
                    f64::from(window.get_map_width()),
                    f64::from(window.get_map_height()),
                )))
                .into(),
            );
            window.set_treemap_note(tree.treemap_note().into());
        }
    }
    fn inspect(&mut self, window: &SortSizeWindow, index: usize) {
        if let Some(tree) = &mut self.tree
            && let Some(path) = tree.entry_path(index)
        {
            let row = tree.reveal_entry(index);
            let cursor = window.get_map_cursor();
            self.show_rows(window);
            window.set_map_cursor(cursor);
            window.set_detail_scroll_y(-(row as f32 * 34.0));
            window.set_inspected_node(index as i32);
            window.set_entry_path(path.into());
        }
    }
}

fn connect(window: &SortSizeWindow) -> Rc<RefCell<Controller>> {
    let controller = Rc::new(RefCell::new(Controller::default()));
    controller.borrow().show_preferences(window);
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_column(move |index, visible| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            if let Ok(index) = usize::try_from(index) {
                state.preferences.set_column(index, visible);
            }
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_set_units(move |decimal| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            state.preferences.units = if decimal {
                units::Units::Decimal
            } else {
                units::Units::Binary
            };
            state.display_changed(&window, previous);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_reset_display(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            let previous = state.preferences.units;
            state.preferences = Default::default();
            state.display_changed(&window, previous);
        }
    });
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
    #[cfg(any(target_os = "linux", windows))]
    {
        window.set_storage_enabled(true);
        let weak = window.as_weak();
        let state = controller.clone();
        window.on_refresh_volumes(move |remote| {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().refresh_sources(&window, remote);
            }
        });
        let weak = window.as_weak();
        let state = controller.clone();
        window.on_select_volume(move |index| {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().select_volume(&window, index);
            }
        });
    }
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_select_top(move |rank| {
        if let Some(window) = weak.upgrade() {
            state.borrow().select_top(&window, rank);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_reveal_top(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().reveal_top(&window);
        }
    });
    let state = controller.clone();
    let weak = window.as_weak();
    window.on_path_edited(move || {
        let mut state = state.borrow_mut();
        state.path_input.edited();
        if let Some(window) = weak.upgrade() {
            window.set_selected_volume(-1);
        }
        if let Some(window) = weak.upgrade() {
            window.set_resolve_owner(false);
            state.resolution_changed(&window);
        }
    });
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
            state.borrow_mut().cancel(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_owner_resolution_changed(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().resolution_changed(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_owner(move || {
        if let Some(window) = weak.upgrade() {
            state.borrow_mut().sort_owners(&window);
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
            state.cancel_owner_sort(&window);
            if let Some(tree) = &mut state.tree {
                tree.sort_by(name);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_allocated(move || {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            state.cancel_owner_sort(&window);
            if let Some(tree) = &mut state.tree {
                tree.sort_allocated();
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_sort_dates(move |accessed| {
        if let Some(window) = weak.upgrade() {
            let mut state = state.borrow_mut();
            state.cancel_owner_sort(&window);
            if let Some(tree) = &mut state.tree {
                tree.sort_dates(accessed);
            }
            state.show_rows(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_inspect_entry(move |index| {
        if index >= 0
            && let Some(window) = weak.upgrade()
        {
            state.borrow_mut().inspect(&window, index as usize);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_resize_treemap(move |_, _| {
        if let Some(window) = weak.upgrade() {
            state.borrow().show_treemap(&window);
        }
    });
    let weak = window.as_weak();
    let state = controller.clone();
    window.on_activate_tile(move |index| {
        use slint::Model;
        if let Some(window) = weak.upgrade()
            && index >= 0
            && let Some(tile) = window.get_tiles().row_data(index as usize)
        {
            let mut state = state.borrow_mut();
            if tile.node_id >= 0 {
                if tile.directory {
                    if let Some(tree) = &mut state.tree {
                        tree.select(tile.node_id as usize);
                    }
                    state.show_rows(&window);
                } else {
                    state.inspect(&window, tile.node_id as usize);
                }
            } else {
                window.set_entry_path(
                    format!(
                        "{} · {} · {}",
                        tile.name,
                        tile.size,
                        if tile.node_id == -1 {
                            "Éléments individuels accessibles dans le tableau."
                        } else {
                            "Chemins non retenus : aucun détail individuel disponible."
                        }
                    )
                    .into(),
                );
                window.set_inspected_node(-1);
            }
        }
    });
    controller
}

pub fn run() -> Result<(), slint::PlatformError> {
    let window = SortSizeWindow::new()?;
    let legal = LegalWindow::new()?;
    window.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_product_version(env!("CARGO_PKG_VERSION").into());
    legal.set_notices(
        include_str!("legal-notice.txt")
            .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
            .into(),
    );
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
    #[cfg(any(target_os = "linux", windows))]
    {
        if let Some(path) = preferences_store::config_path() {
            let (preferences, message) = preferences_store::load(&path);
            controller.borrow_mut().preferences = preferences;
            controller.borrow().show_preferences(&window);
            window.set_preference_state(message);
            match preferences_store::Store::start(path) {
                Ok(store) => controller.borrow_mut().preference_store = Some(store),
                Err(_) => window.set_preference_state(6),
            }
        } else {
            window.set_preference_state(7);
        }
    }
    #[cfg(not(target_os = "android"))]
    if let Ok(path) = std::env::current_dir() {
        window.set_scan_path(controller.borrow_mut().path_input.select(path).into());
    }
    #[cfg(any(target_os = "linux", windows))]
    controller.borrow_mut().refresh_sources(&window, false);
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    #[cfg(windows)]
    let mut placement_attempts = 0;
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        if let Some(window) = weak.upgrade() {
            // Native handles can arrive after the first event-loop iteration.
            // Reuse the existing poll and stop after success or ten attempts.
            #[cfg(windows)]
            if placement_attempts < 10 {
                placement_attempts += 1;
                if window_placement::fit_to_work_area(window.window()) {
                    placement_attempts = 10;
                }
            }
            state.borrow_mut().poll(&window);
        }
    });
    let result = window.run();
    timer.stop();
    controller.borrow_mut().task = None;
    controller.borrow_mut().owner_task = None;
    #[cfg(any(target_os = "linux", windows))]
    {
        controller.borrow_mut().sources = volume_view::Sources::default();
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        controller.borrow_mut().preference_store = None;
    }
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
    mod display_gui_tests;
    mod owner_gui_tests;
    mod temporal_gui_tests;
    use super::*;
    #[cfg(any(target_os = "linux", windows))]
    mod volume_gui_tests;
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
    fn allocation_checkbox_applies_only_to_the_next_explicit_scan() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
        let window = SortSizeWindow::new().unwrap();
        let controller = connect(&window);
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("file"), [1; 4096]).unwrap();
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        assert!(!window.get_collect_allocation());
        for enabled in [false, true] {
            window.set_collect_allocation(enabled);
            assert!(!window.get_busy());
            assert!(controller.borrow().task.is_none());
            window.invoke_start_scan();
            let start = Instant::now();
            while window.get_busy() {
                controller.borrow_mut().poll(&window);
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
            let row = window.get_details().row_data(0).unwrap();
            assert_eq!(row.allocated == "Non collectée", !enabled);
            assert_eq!(window.get_allocation_sort_available(), enabled);
            window.invoke_sort_allocated();
            assert_eq!(window.get_sort_order(), if enabled { 4 } else { 0 });
            window.invoke_sort_allocated();
            assert_eq!(window.get_sort_order(), if enabled { 5 } else { 0 });
            assert!(row.modified.ends_with(" UTC"));
            window.set_collect_allocation(!enabled);
            assert_eq!(
                window.get_details().row_data(0).unwrap().allocated,
                row.allocated
            );
            assert_eq!(window.get_allocation_sort_available(), enabled);
            assert!(controller.borrow().task.is_none());
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
        for row in window.get_details().iter() {
            assert!(row.modified.ends_with(" UTC"));
            assert!(row.accessed.ends_with(" UTC"));
        }
        assert!(!window.get_has_parent());
        assert_eq!(window.get_top_rows().row_count(), 2);
        assert_eq!(window.get_top_rows().row_data(0).unwrap().name, "data");
        window.set_top_visible(true);
        window.invoke_select_top(0);
        assert!(window.get_top_path().ends_with("data"));
        assert!(window.get_top_retained());
        window.invoke_reveal_top();
        assert!(!window.get_top_visible());
        assert!(window.get_current_path().ends_with("été"));
        assert!(window.get_entry_path().ends_with("data"));
        assert!(window.get_inspected_node() >= 0);
        window.invoke_go_parent();
        let folder = window.get_rows().row_data(1).unwrap();
        assert_eq!(folder.name, "été");
        assert_eq!(window.get_details().row_data(0).unwrap().files, "1");
        assert_eq!(window.get_details().row_data(0).unwrap().directories, "1");
        assert!(
            window
                .get_selection_summary()
                .contains("fichiers directs : 1")
        );
        assert!(
            window
                .get_selection_summary()
                .contains("fichiers récursifs : 2")
        );
        window.invoke_toggle_node(folder.node_id);
        assert_eq!(window.get_rows().row_count(), 3);
        assert_eq!(window.get_rows().row_data(2).unwrap().name, "nested");
        window.set_map_width(800.0);
        window.set_map_height(300.0);
        controller.borrow().show_treemap(&window);
        let tile = window
            .get_tiles()
            .iter()
            .position(|t| t.node_id == folder.node_id)
            .unwrap();
        window.invoke_activate_tile(tile as i32);
        assert!(window.get_has_parent());
        assert!(window.get_current_path().ends_with("été"));
        assert_eq!(window.get_details().row_count(), 2);
        let nested = window.get_details().row_data(0).unwrap();
        let file = window.get_details().row_data(1).unwrap();
        assert!(!file.path.is_empty());
        let context = window.get_current_path();
        window.invoke_inspect_entry(nested.node_id);
        assert_eq!(window.get_entry_path(), nested.path);
        assert_eq!(window.get_current_path(), context);
        window.invoke_inspect_entry(file.node_id);
        assert_eq!(window.get_entry_path(), file.path);
        assert_eq!(file.name, "data");
        assert_eq!(file.files, "—");
        assert_eq!(nested.files, "0");
        assert!(
            window
                .get_selection_summary()
                .contains("sous-dossiers directs : 1")
        );
        assert_eq!(file.proportion, "100.0 %");
        let file_tile = window
            .get_tiles()
            .iter()
            .position(|t| t.node_id == file.node_id)
            .unwrap();
        window.invoke_activate_tile(file_tile as i32);
        assert!(window.get_entry_path().ends_with("data"));
        assert_eq!(window.get_inspected_node(), file.node_id);
        window.invoke_inspect_entry(file.node_id);
        assert_eq!(window.get_inspected_node(), file.node_id);
        window.invoke_activate_tile(-1);
        window.invoke_activate_tile(1000);
        assert_eq!(window.get_inspected_node(), file.node_id);
        window.invoke_select_node(nested.node_id);
        assert_eq!(window.get_details().row_count(), 0);
        assert_eq!(window.get_tiles().row_count(), 0);
        assert!(window.get_treemap_note().contains("Aucune taille positive"));
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
        assert_eq!(window.get_top_rows().row_count(), 0);
        assert_eq!(window.get_top_selected(), -1);
        assert!(!window.get_top_retained());
        assert_eq!(window.get_rows().row_count(), 0);
        assert_eq!(window.get_tiles().row_count(), 0);
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
        top_callbacks_regressions(&window, &controller);
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        folder_selection_regressions(&window, &controller);
        if let Ok(destination) = std::env::var("SORTSIZE_GUI_SCREENSHOT") {
            let path = std::env::var("SORTSIZE_GUI_PREVIEW_ROOT")
                .unwrap_or_else(|_| fixture.path().to_string_lossy().into_owned());
            window.set_scan_path(path.into());
            window.invoke_start_scan();
            wait(&window, &controller);
            if let Ok(top_destination) = std::env::var("SORTSIZE_TOP_SCREENSHOT") {
                window.set_top_visible(true);
                window.invoke_select_top(0);
                capture(&window, &adapter, &top_destination, (960, 600));
                window.set_top_visible(false);
            }
            capture(&window, &adapter, &destination, (1200, 760));
            if let Ok(destination) = std::env::var("SORTSIZE_GUI_COMPACT_SCREENSHOT") {
                capture(&window, &adapter, &destination, (960, 600));
                if let Ok(destination) = std::env::var("SORTSIZE_DETAIL_SCREENSHOT") {
                    window.set_detail_scroll_x(-600.0);
                    capture(&window, &adapter, &destination, (960, 600));
                }
            }
        }
    }

    fn top_callbacks_regressions(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) {
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("not-retained"), b"1234").unwrap();
        let inventory = sortsize_core::scan_inventory(
            fixture.path(),
            &Default::default(),
            &Default::default(),
            1,
            |_| {},
        )
        .unwrap();
        let (tree, _) = Tree::new(inventory);
        window.set_top_rows(Rc::new(VecModel::from(tree.top_rows.clone())).into());
        controller.borrow_mut().tree = Some(tree);
        controller.borrow_mut().show_rows(window);
        window.set_top_visible(true);
        window.invoke_select_top(0);
        assert!(window.get_top_path().ends_with("not-retained"));
        assert!(!window.get_top_retained());
        let previous_path = window.get_current_path();
        window.invoke_reveal_top();
        assert!(window.get_top_visible());
        assert_eq!(window.get_current_path(), previous_path);
        assert_eq!(window.get_details().row_count(), 0);
        window.invoke_select_top(-1);
        window.invoke_select_top(100);
        assert_eq!(window.get_top_selected(), 0);
        // Revealing a retained file must also place keyboard focus on its tile.
        fs::write(fixture.path().join("larger"), b"12345678").unwrap();
        let inventory = sortsize_core::scan_inventory(
            fixture.path(),
            &Default::default(),
            &Default::default(),
            100,
            |_| {},
        )
        .unwrap();
        let (tree, _) = Tree::new(inventory);
        window.set_top_rows(Rc::new(VecModel::from(tree.top_rows.clone())).into());
        controller.borrow_mut().tree = Some(tree);
        window.set_map_width(600.0);
        window.set_map_height(120.0);
        controller.borrow_mut().show_rows(window);
        window.invoke_select_top(1);
        window.invoke_reveal_top();
        assert_eq!(
            window
                .get_tiles()
                .row_data(window.get_map_cursor() as usize)
                .unwrap()
                .node_id,
            window.get_inspected_node()
        );
        window.set_top_visible(false);
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
                assert!(
                    window
                        .get_selection_summary()
                        .contains("fichiers récursifs : ≥ 0")
                );
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
