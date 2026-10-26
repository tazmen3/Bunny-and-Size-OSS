# Bunny and Size

![Bunny and Size — TazmenWorld](assets/branding/readme-banner.svg)

**v0.3.0-rc.1 — première RC publique / first public RC**, 26 octobre 2026 / 26 October 2026.
[Téléchargements et sources correspondantes / Downloads and corresponding sources](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.3.0-rc.1).
Version d'évaluation, avec réserves connues. / Evaluation prerelease with known reservations.

Analyse locale du stockage en lecture seule : arbre/tableau, Treemap, Top 100, analyse progressive et annulation, exploration complémentaire des dossiers, préférences persistantes et interface en cinq langues (EN/FR/DE/ES/PT-PT). CLI : exports de résumés JSON/CSV/TSV et d'entrées CSV/TSV. Aucune suppression ni télémétrie.

Read-only local storage analysis: tree/table, Treemap, Top 100, progressive scanning and cancellation, additional folder exploration, persistent preferences and five interface languages (EN/FR/DE/ES/PT-PT). CLI summary exports in JSON/CSV/TSV and entry exports in CSV/TSV. No deletion or telemetry.

- [Notes de publication / Release notes](docs/releases/v0.3.0-rc.1/PUBLIC_RELEASE_NOTES.md)
- [Installation Ubuntu et Windows / User guide](docs/releases/v0.3.0-rc.1/USER_GUIDE.md)
- [Limitations connues / Known limitations](docs/releases/v0.3.0-rc.1/PUBLIC_LIMITATIONS.md)
- [Clarifications des textes embarqués / Archive wording](docs/releases/v0.3.0-rc.1/RELEASE_CLARIFICATIONS.md)
- [SHA-256 des quatre archives / Archive checksums](docs/releases/v0.3.0-rc.1/SHA256SUMS)
- [Provenance](PROVENANCE.json) · [Compilation / Building](docs/BUILDING.md) · [Crédits / Credits](CREDITS.md)

Ubuntu 24.04 x86_64 : extraire toute l'archive Linux et lancer `./bunny-and-size` (X11/XWayland, prérequis dans le guide). Windows 11 x64 : extraire tout le ZIP et lancer `bunny-and-size.exe` ; [Microsoft Visual C++ Redistributable x64 v14](https://aka.ms/vc14/vc_redist.x64.exe) requis. Pas de MSI/DEB ; aucun Rust requis à l'exécution.

Ubuntu 24.04 x86_64: fully extract the Linux archive and run `./bunny-and-size` (X11/XWayland; see the guide for prerequisites). Windows 11 x64: fully extract the ZIP and run `bunny-and-size.exe`; official Microsoft Visual C++ Redistributable x64 v14 required. No MSI/DEB installer or Rust runtime requirement.

## Code signing policy / Politique de signature du code

**26 octobre 2026 :** candidature SignPath Foundation **en préparation**, non approuvée ; **signature Windows non active**. Les trois exécutables Windows publics de la v0.3.0-rc.1 sont **non signés** et peuvent afficher `Unknown publisher` / SmartScreen. Aucune ancienne archive n'est modifiée.

**26 October 2026:** SignPath Foundation application **being prepared**, not approved; **Windows signing is not active**. The current v0.3.0-rc.1 Windows executables remain **unsigned**. No existing release artifact is changed.

[**Code signing policy — conditions, rôles, confidentialité et vérification / status, roles, privacy and verification**](CODE_SIGNING.md)

**D1 Windows reste non corrigé :** après certains changements de langue, le pied peut garder temporairement l'ancienne langue ; un redimensionnement manuel a rétabli l'affichage dans les cas observés. Données cohérentes dans ces scénarios, investigation en cours. Binaires non signés, avertissement SmartScreen possible. Accessibilité humaine exhaustive, multi-écran, Wayland natif et Android non qualifiés ; CI GitHub Actions volontairement désactivée.

**Windows D1 remains unfixed:** after some language changes the footer may temporarily retain the old language; manual resizing restored it in observed cases. Data remained consistent in those scenarios; investigation continues. Unsigned binaries may trigger SmartScreen. Exhaustive human accessibility, multiple monitors, native Wayland and Android are unqualified; GitHub Actions CI is intentionally disabled.

Code **GPL-3.0-only**, sans garantie / no warranty. Visuels adaptés approuvés / approved adapted artwork: **CC BY-SA 4.0**, **Louis Lejuez / TazmenWorld**. Voir / see [BRANDING.md](BRANDING.md), [CREDITS.md](CREDITS.md) et / and [LICENSE](LICENSE). Le portrait et l'original réservé ne sont pas distribués / The portrait and reserved original artwork are not distributed. Les tiers conservent leurs licences / Third parties retain their own licenses.

Les quatre archives originales proviennent du gel `addf68704f26ef02559dd56b2e9b75b168fd0752` et restent inchangées. Les paquets source complets (dépendances Cargo et recettes hors ligne) accompagnent les binaires ; les ZIP/tar source automatiques GitHub ne les remplacent pas. L'instantané Git public sélectionne les fichiers approuvés de ces mêmes sources. Aucun historique Git privé n'est importé. Les tests privés ajoutés après le gel ne sont pas inclus. V0.4 est la prochaine étape de développement fonctionnel.

The four original archives originate from freeze `addf68704f26ef02559dd56b2e9b75b168fd0752` and remain unchanged. Dependency-complete source bundles with offline recipes accompany the binaries; GitHub's automatic source ZIP/tar downloads do not replace them. The public Git snapshot selects approved files from those same sources. Private Git history and later private tests are not imported. V0.4 is the next functional development stage.

Les releases [v0.2.0-rc.1](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.2.0-rc.1) et [v0.1.0](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.1.0) et leurs tags restent historiques. Leurs documents conservent leur portée d'origine. / Earlier releases, tags and documents retain their historical scope.
