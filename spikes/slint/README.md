# Prototype de faisabilité Slint

Workspace expérimental séparé pour évaluer Slint 1.18.1 sans imposer ses dépendances à la CLI. Le prototype desktop utilise maintenant `sortsize-core` : scan en lecture seule sur un worker, progression, annulation et arbre de dossiers et tableau des enfants synchronisés, tris et proportions de taille logique. Le [guide GUI](../../docs/GUI.md) décrit son utilisation et ses limites. Aucun accès SAF ni inventaire SQLite n'est implémenté.

## Desktop

```bash
cargo build --manifest-path spikes/slint/Cargo.toml
cargo run --manifest-path spikes/slint/Cargo.toml
```

Backend Winit, rendu logiciel. Linux utilise X11 (ou XWayland) dans ce prototype ; un backend Wayland natif reste à évaluer. Le lancement requiert une session graphique et les bibliothèques système correspondantes.

Pour compiler sur Ubuntu, installer les fichiers de développement de Fontconfig (`libfontconfig-dev`). Dans la session initiale, ces paquets ont été téléchargés et extraits dans `/tmp/sortsize-native-deps` sans installation système ; la compilation a utilisé `PKG_CONFIG_PATH=/tmp/sortsize-native-deps/usr/lib/x86_64-linux-gnu/pkgconfig`. Ce dossier temporaire ne fait pas partie du projet et n'est pas une dépendance de distribution.

Le [rapport natif Ubuntu/X11](../../docs/validation/issue-7-ubuntu/README.md) documente les interactions exécutées et la limite IBus/XTest. Le lanceur `scripts/run-slint-ubuntu.sh --basic-input` fournit un secours explicite pour la saisie X11 ; il désactive la composition IBus pour le processus seulement. Sans option, le lanceur conserve la saisie habituelle. Aucun test interactif Windows n’est ajouté.

## Android

```bash
rustup target add aarch64-linux-android
cargo check --manifest-path spikes/slint/Cargo.toml --lib --target aarch64-linux-android
```

Le point d'entrée `android_main` et le backend `android-activity-06` sont préparés.
La [chaîne APK épinglée](../../docs/ANDROID.md) utilise SDK/NDK/JDK et
`cargo-apk` 0.10.0, avec vérification de signature, ABI et absence de permissions :

```bash
python3 scripts/build_android.py --target aarch64-linux-android
python3 scripts/build_android.py --target x86_64-linux-android
```

Le choix de distribution, la cible SDK de publication et les permissions seront à revoir au moment de préparer l'application réelle. Aucun accès universel au stockage n'est demandé par ce prototype.

Le scan est désactivé dans l'interface Android tant que la sélection SAF/MediaStore n'est pas intégrée.
Les [preuves de #5](../../docs/validation/issue-5-android/README.md) distinguent
compilation moteur, APK ARM64/x86_64 et démarrage sur émulateur Android 15 x86_64.
Aucun appareil physique ni scan Android n'est validé ; l'adaptation tactile et
les insets de la fenêtre restent à traiter (#21).

## Licence et validation

Le paquet Slint téléchargé déclare `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`. La licence du projet et les obligations de distribution doivent être formalisées avant publication d'un binaire GUI. Voir [ADR-0001](../../docs/ADR/0001-premier-scanner.md) et [validation](../../docs/VALIDATION.md) pour les résultats effectivement obtenus.

## Treemap V0.2 — Issue #9

La Treemap Squarified des tailles logiques observées s’affiche au-dessus du
tableau : zoom dossier, retour parent, inspection fichier et clavier synchronisés
sans nouveau scan. Limite de 256 surfaces avec groupes Autres et omissions
explicites ; zéros/inconnus sans aire inventée.
[Utilisation et limites](../../docs/GUI.md#v02--treemap-interactive-9-view-002--view-008),
[preuves Ubuntu](../../docs/validation/issue-9-treemap/README.md),
[recette Windows non exécutée](../../docs/validation/issue-9-windows/README.md).
Slint reste épinglé à 1.18.1, sans dépendance supplémentaire.
