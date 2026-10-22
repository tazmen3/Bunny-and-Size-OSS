# Guide utilisateur / User guide

**Historical v0.1 documentation.** For 0.2.0-rc.1, see [current release materials](releases/v0.2.0-rc.1/RELEASE_NOTES.md) and [corresponding-source downloads](releases/v0.2.0-rc.1/DOWNLOADS.md). The buildable product snapshot on this branch is v0.2; the old statements below retain their historical scope.

## Français

### Windows x64

Télécharger `SortSize-v0.1.0-windows-x86_64.zip`, vérifier son SHA-256 contre `SHA256SUMS`, puis tout extraire. Installer si nécessaire le [Microsoft Visual C++ Redistributable x64 v14 officiel](https://aka.ms/vc14/vc_redist.x64.exe). Le README binaire indique au moins 14.44.35228 pour MSVC 14.44 ; cette borne n’a pas été testée isolément. Le poste validé utilise 14.50.35719.0. Microsoft recommande une version au moins aussi récente que les outils de compilation. Ne pas télécharger une DLL isolée.

Lancer `SortSize.exe`. Choisir un dossier avec **Parcourir**, puis **Analyser**. La sélection seule ne lance pas de scan. Naviguer dans l’arbre/tableau, utiliser Parent et les tris Nom/Taille ; **Annuler** conserve les résultats partiels. **À propos / Licences** présente les avis et la GPL intégrale hors ligne.

```powershell
Get-FileHash -Algorithm SHA256 -LiteralPath 'SortSize-v0.1.0-windows-x86_64.zip'
.\sortsize-cli.exe --help
.\sortsize-cli.exe scan 'C:\dossier-a-analyser' --format json
.\sortsize-cli.exe scan 'C:\dossier-a-analyser' --format json --output rapport-neuf.json
```

L’export existant n’est pas écrasé. **Windows propre sans Rust/outils de développement et runtime absent puis installé non testé.** Les EXE ne sont pas signés.

### Ubuntu 24.04 x86_64

Prérequis de cette distribution : **glibc 2.39**, libgcc, Fontconfig, FreeType/polices, bibliothèques clientes X11 et xkbcommon, environnement graphique X11. Le sélecteur requiert D-Bus et `xdg-desktop-portal` avec un backend de bureau adapté. Bibliothèques usuelles : `libfontconfig1`, `libfreetype6`, `fonts-dejavu-core`, `libx11-6`, `libx11-xcb1`, `libxcb1`, `libxcursor1`, `libxrandr2`, `libxi6`, `libxfixes3`, `libxrender1`, `libxext6`, `libxkbcommon0`, `libxkbcommon-x11-0`, `libdbus-1-3`, `xdg-desktop-portal` et le backend KDE ou GTK de la session. Aucune bibliothèque système livrée.

```sh
sha256sum -c SHA256SUMS
tar -xzf SortSize-v0.1.0-linux-x86_64.tar.gz
cd SortSize-v0.1.0-linux-x86_64
./sortsize-gui
./sortsize-cli scan /chemin/du/dossier --format json --output rapport-neuf.json
```

La commande de vérification complète suppose les quatre archives et `SHA256SUMS` dans le même dossier. Autrement, comparer le hash du seul fichier téléchargé à la ligne correspondante. Voir les limites KDE/X11 et IME avant usage.

## English

Download the binary archive for your platform, verify its SHA-256 against `SHA256SUMS` and extract all files. Windows: launch `SortSize.exe` and install the official Microsoft Visual C++ x64 v14 Redistributable if required. The package documents 14.44.35228 or later for MSVC 14.44; that minimum was not tested separately. The tested host uses 14.50.35719.0. A clean Windows host without development tools has not been tested. The executables are unsigned.

Ubuntu 24.04 x86_64 requires glibc 2.39, libgcc, Fontconfig, FreeType/fonts, X11/xkbcommon libraries and an X11 desktop. Folder selection requires session D-Bus and xdg-desktop-portal with the appropriate KDE/GTK backend. System libraries are not bundled. The package list and commands above apply; no cross-distribution compatibility is certified.

Select a folder, then start the scan. Selection alone does not start it. Use the tree/table, parent navigation and name/size sorting. Cancellation retains partial results. About / Licences displays the notices and full GPL offline. Use `sortsize-cli` (`sortsize-cli.exe` on Windows) with `scan PATH --format json`, optionally `--output NEW_FILE`; an existing export is preserved rather than overwritten. Sizes are logical; no deletion or recoverable-space estimate is provided.
