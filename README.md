# SortSize

**V0.1.0 — première préversion open source / first open-source prerelease.**

Scanner local en lecture seule : moteur Rust partagé, GUI Slint et CLI avec JSON. Windows x64 et Ubuntu 24.04 x86_64. Aucun compte, télémétrie ou suppression implicite.

Read-only local storage scanner with a shared Rust engine, Slint GUI and JSON CLI. Windows x64 and Ubuntu 24.04 x86_64. No account, telemetry or implicit deletion.

**[Télécharger / Downloads](https://github.com/tazmen3/SortSize-OSS/releases/tag/v0.1.0)** · [Guide FR/EN](docs/USER_GUIDE.md) · [Compilation / Building](docs/BUILDING.md) · [Limites / Limitations](docs/LIMITATIONS.md) · [Provenance](docs/PROVENANCE.md) · [Licences / Licensing](docs/LICENSING.md)

## Utilisation / Usage

Extraire toute l’archive de son système. Windows : lancer `SortSize.exe` ; installer si nécessaire le Microsoft Visual C++ Redistributable x64 v14 officiel. Ubuntu : lancer `./sortsize-gui` ; glibc 2.39, Fontconfig, bibliothèques X11 et portail de bureau requis. Rust et les outils de développement ne sont pas nécessaires pour lancer les binaires prévus.

Extract the complete archive for your platform. On Windows, run `SortSize.exe` and install the official Microsoft Visual C++ x64 v14 Redistributable if needed. On Ubuntu, run `./sortsize-gui`; glibc 2.39, Fontconfig, X11 libraries and a desktop portal are required. Running the supplied binaries is intended to require no Rust or development tools.

La GUI propose sélection de dossier, arbre/tableau, tris, progression, annulation et **À propos / Licences**. La CLI séparée `sortsize-cli` (`sortsize-cli.exe` sous Windows) exporte du JSON. Les tailles sont logiques et ne représentent pas un gain récupérable.

The GUI provides folder selection, tree/table navigation, sorting, progress, cancellation and **About / Licences**. The separate `sortsize-cli` (`sortsize-cli.exe` on Windows) exports JSON. Sizes are logical and do not estimate recoverable storage.

## Limites importantes / Important limitations

- Windows propre sans outils de développement et runtime absent puis installé **non testé**. Binaires Windows non signés ; aucune certification SmartScreen.
- Blocage ponctuel sous KDE/X11 pendant des injections clavier rapides : cause non établie. IME, clavier matériel et Wayland/XWayland non certifiés.
- Aucune compatibilité générale avec toutes les distributions ou versions de Windows. Pas de Treemap, doublons, suppression, SQLite, cloud/NAS ou livraison Android dans cette préversion.

- A clean Windows system without development tools, and the missing-runtime-then-installed scenario, **have not been tested**. Windows binaries are unsigned; no SmartScreen certification is claimed.
- An occasional KDE/X11 hang was observed during rapid keyboard injection; its cause is undetermined. IME, physical keyboard and Wayland/XWayland are not certified.
- No general compatibility certification across distributions or Windows versions. No Treemap, duplicate management, deletion, SQLite, cloud/NAS or Android release in this preview.

## Sources et licence / Source and licence

**GPL-3.0-only**, sans garantie / without warranty; voir / see [LICENSE](LICENSE), [AUTHORS.txt](AUTHORS.txt) et / and [Licensing](docs/LICENSING.md).

Les deux bundles source correspondant aux binaires sont joints à la même Release, sans authentification ni surcoût. Ils contiennent les dépendances Cargo verrouillées et les recettes hors ligne. **Les archives automatiques GitHub “Source code” ne les remplacent pas.**

The two corresponding-source bundles are attached to the same Release, without authentication or additional charge. They include locked Cargo dependencies and offline rebuild recipes. **GitHub’s automatic “Source code” downloads do not replace them.**

Les archives proviennent du commit produit `fbf46e3d94451b821cfb31fb6228aaea60258045`. Ce dépôt contient un instantané indépendant aux mêmes sources produit ; son commit et le tag public sont différents du commit d’origine. Aucun historique, ancienne PR, journal interne ou workflow de développement n’est importé. Les chemins déjà incorporés aux archives demeurent dans ces fichiers inchangés, avec acceptation du propriétaire.

The archives originate from product commit `fbf46e3d94451b821cfb31fb6228aaea60258045`. This repository is an independent snapshot of the same product sources; its commit and public tag differ from the origin commit. No private history, old PR, internal log or development workflow is imported. Previously embedded workstation paths remain in the unchanged archives, as accepted by the owner.

Le README historique du prototype sous `spikes/slint/` est conservé octet pour octet pour la provenance ; les présents guides publics sont la référence de cette diffusion. The historical prototype README is retained byte for byte; these public guides describe this release.
