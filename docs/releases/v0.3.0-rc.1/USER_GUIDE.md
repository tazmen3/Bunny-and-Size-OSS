# Bunny and Size v0.3.0-rc.1 — guide / user guide

## Français

Télécharger le binaire de votre système et l'archive source correspondante depuis [la release](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.3.0-rc.1), puis vérifier `SHA256SUMS`. Extraire intégralement dans un dossier accessible. Pas d'installateur MSI/DEB. Lire [les limites](PUBLIC_LIMITATIONS.md), dont D1 Windows non corrigé, avant l'essai.

**Ubuntu 24.04 x86_64 :** lancer `./bunny-and-size`. En terminal : `./bunny-and-size-cli --help`. Prérequis : glibc, libgcc, Fontconfig (`libfontconfig1`), FreeType (`libfreetype6`), polices (`fonts-dejavu-core`), X11 ou XWayland, `libx11-6`, `libx11-xcb1`, `libxcb1`, `libxcursor1`, `libxrandr2`, `libxi6`, `libxfixes3`, `libxrender1`, `libxext6`, `libxkbcommon0`, `libxkbcommon-x11-0`. Le sélecteur de dossiers requiert un bus D-Bus de session et `xdg-desktop-portal` avec backend adapté (GTK/KDE). La saisie manuelle du chemin reste possible. Aucune bibliothèque système n'est incluse ; Wayland natif n'est pas activé.

**Windows 11 x64 :** lancer `bunny-and-size.exe`. Installer si nécessaire le [Microsoft Visual C++ Redistributable x64 v14 officiel](https://aka.ms/vc14/vc_redist.x64.exe). La version 14.44.35211.0 a été testée ; le minimum absolu n'est pas établi. Utiliser la dernière version officielle compatible au moins aussi récente que les outils MSVC, selon [Microsoft](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist). Ne pas télécharger une DLL isolée. Le runtime n'est pas fourni et son installation peut demander une élévation ou un redémarrage. Les exécutables sont non signés : SmartScreen peut avertir ; ne pas désactiver globalement les protections Windows.

**Utilisation :** Parcourir, sélectionner un dossier, puis Analyser. Annuler conserve un résultat partiel. Explorer l'arbre/tableau, la Treemap et le Top 100 ; ajuster langues, colonnes et unités dans les préférences. Les préférences existantes restent dans `sortsize/display-v1.conf`, sans migration au lancement. Les analyses lisent les métadonnées sans supprimer de fichiers. La résolution distante des propriétaires est une option à consentement séparé.

Exemple CLI Linux : `./bunny-and-size-cli scan /chemin/dossier --format json --output rapport.json`. Windows : `.\bunny-and-size-cli.exe scan 'C:\chemin\dossier' --format json`. Pour les entrées : `--scope entries --format csv`. Exports GUI non livrés. Choisir une destination de rapport adaptée ; la CLI écrit explicitement le fichier demandé.

## English

Download your platform's binary package and its matching source archive from [the release](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.3.0-rc.1), verify `SHA256SUMS`, and fully extract into an accessible folder. No MSI/DEB installer. Read [the limitations](PUBLIC_LIMITATIONS.md), including unfixed Windows D1, before evaluation.

**Ubuntu 24.04 x86_64:** run `./bunny-and-size`; terminal help: `./bunny-and-size-cli --help`. Required libraries and packages are listed in the French section above. Use an X11 or XWayland session. The folder picker requires session D-Bus and `xdg-desktop-portal` with a suitable GTK/KDE backend; manual path entry remains available. System libraries are not bundled. Native Wayland is not enabled.

**Windows 11 x64:** run `bunny-and-size.exe`. Install the official [Microsoft Visual C++ Redistributable x64 v14](https://aka.ms/vc14/vc_redist.x64.exe) if needed. Version 14.44.35211.0 was tested, not established as a universal minimum. Follow [Microsoft guidance](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist) and use a supported official runtime at least as recent as the build tools. Do not download standalone DLL files. The runtime is not bundled and installation may require elevation or restart. Executables are unsigned and SmartScreen may warn; do not globally disable Windows protections.

**Usage:** Browse, select a folder, then Scan. Cancel retains partial results. Explore the tree/table, Treemap and Top 100; select language, columns and units in Preferences. Existing preferences remain in `sortsize/display-v1.conf`, with no startup migration. Scanning reads metadata without deleting files. Remote owner resolution requires separate opt-in.

CLI examples appear above; summary formats are JSON/CSV/TSV, entry formats CSV/TSV. GUI exports are not delivered. Select a suitable output destination; the CLI explicitly writes the requested report file.

Code GPL-3.0-only ; visuels approuvés / approved artwork CC BY-SA 4.0, Louis Lejuez / TazmenWorld. Conserver LICENSE et les avis tiers / Keep LICENSE and third-party notices. No warranty / Aucune garantie.
