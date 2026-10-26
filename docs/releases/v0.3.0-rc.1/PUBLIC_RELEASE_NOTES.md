# Bunny and Size v0.3.0-rc.1 — Release Candidate

## Français

Première RC publique de Bunny and Size, autorisée par le propriétaire le **26 octobre 2026** avec les réserves ci-dessous. Version d'évaluation, pas une version stable. V0.4 est la prochaine étape de développement fonctionnel.

### Nouveautés V0.3

- SortSize devient **Bunny and Size**, avec la mascotte et la signature **TazmenWorld**.
- Interface en cinq langues : anglais, français, allemand, espagnol et portugais du Portugal.
- Analyse progressive avec résultat partiel et annulation ; exploration complémentaire des dossiers au-delà de l'inventaire retenu en mémoire.
- Navigation et panneaux améliorés, corrections du rendu Windows et améliorations des textes accessibles par UIA/AT-SPI. Ces améliorations ne constituent pas une certification d'accessibilité.

Fonctions également livrées, déjà présentes avant V0.3 pour certaines : arbre/tableau, Treemap, Top 100, colonnes et unités configurables, préférences persistantes, tailles logiques/allouées, dates UTC et propriétaires optionnels. La CLI exporte les résumés en JSON/CSV/TSV et les entrées en CSV/TSV. Les exports restent réservés à la CLI. L'analyse est en lecture seule, sans suppression ni télémétrie ; la résolution distante des propriétaires requiert un consentement séparé.

### Installation et provenance

Ubuntu 24.04 x86_64 : extraire intégralement l'archive Linux et lancer `./bunny-and-size`. Windows 11 x64 : extraire intégralement le ZIP Windows et lancer `bunny-and-size.exe`. Aucun MSI/DEB. Windows requiert le **Microsoft Visual C++ Redistributable x64 v14** officiel ; les prérequis Linux sont détaillés dans [le guide](USER_GUIDE.md). Rust n'est pas requis pour exécuter les binaires.

Télécharger le paquet binaire et **son archive source correspondante**. Les quatre archives originales sont inchangées et proviennent du gel produit `addf68704f26ef02559dd56b2e9b75b168fd0752`. Leurs empreintes sont dans [SHA256SUMS](SHA256SUMS), leur provenance dans [PROVENANCE.json](PROVENANCE.json). Les archives source comprennent les dépendances Cargo et les recettes hors ligne ; les téléchargements source automatiques GitHub ne les remplacent pas. Les anciennes mentions « candidate interne » embarquées sont expliquées dans [les clarifications](RELEASE_CLARIFICATIONS.md).

### Limitations connues

**D1 — non corrigé :** Sous Windows, après certains changements de langue, le pied de fenêtre peut conserver temporairement la langue précédente. Les données restent cohérentes dans les scénarios observés. Un redimensionnement manuel a permis de rétablir l'affichage. Le problème reste en cours d'investigation.

Les textes accessibles peuvent avoir changé alors que les pixels sont anciens. La cause exacte et un lien causal avec le défaut historique #94 ne sont pas démontrés. Le défaut général de transparence #94 n'a pas été reproduit dans les dernières campagnes ; les réserves de qualification plus larges restent ouvertes.

- Exécutables Windows **non signés** : avertissement SmartScreen possible ; réputation et parcours SmartScreen non qualifiés. Ne pas désactiver globalement les protections Windows.
- Écoute humaine du Narrateur et certification exhaustive de l'accessibilité non réalisées.
- Qualification matérielle incomplète : multi-écran et changements de DPI entre moniteurs non certifiés.
- CI GitHub Actions volontairement désactivée ; résultats issus de contrôles natifs locaux.
- Ubuntu qualifié sous X11 ; Wayland natif et Android non qualifiés pour cette livraison. Autres environnements non certifiés.

Lire [toutes les limites](PUBLIC_LIMITATIONS.md), notamment les bornes d'inventaire et le coût de l'exploration complémentaire.

Code **GPL-3.0-only**. Visuels adaptés approuvés **CC BY-SA 4.0**, attribution **Louis Lejuez / TazmenWorld** ; adaptations incorporées sous GPLv3 selon [BRANDING.md](../../../BRANDING.md). Portrait personnel et visuels originaux réservés exclus. Conserver les licences et avis des tiers. Aucune garantie.

## English

First public Bunny and Size RC, authorized by the owner on **26 October 2026** with the reservations below. This is an evaluation prerelease, not a stable version. V0.4 is the next functional development stage.

### New in V0.3

- SortSize becomes **Bunny and Size**, with its mascot and **TazmenWorld** identity.
- Five interface languages: English, French, German, Spanish and European Portuguese.
- Progressive scanning with partial results and cancellation; additional folder exploration beyond the inventory retained in memory.
- Navigation and panel improvements, Windows rendering fixes and improved accessible text through UIA/AT-SPI. These changes do not certify accessibility.

Also delivered, with some features predating V0.3: tree/table views, Treemap, Top 100, configurable columns and units, persistent preferences, logical/allocated sizes, UTC dates and optional owners. The CLI exports summaries as JSON/CSV/TSV and entries as CSV/TSV. Exports are CLI-only. Scanning is read-only, with no deletion or telemetry; remote owner resolution requires separate consent.

### Installation and provenance

Ubuntu 24.04 x86_64: fully extract the Linux archive and run `./bunny-and-size`. Windows 11 x64: fully extract the Windows ZIP and run `bunny-and-size.exe`. No MSI/DEB installer. Windows requires the official **Microsoft Visual C++ Redistributable x64 v14**; Linux requirements appear in [the guide](USER_GUIDE.md). Running the binaries does not require Rust.

Download the binary package and **its matching corresponding-source archive**. All four original archives are unchanged and originate from frozen product source `addf68704f26ef02559dd56b2e9b75b168fd0752`. See [SHA256SUMS](SHA256SUMS) and [PROVENANCE.json](PROVENANCE.json). The source bundles include Cargo dependencies and offline build recipes; GitHub's automatic source downloads do not replace these bundles. Historical “internal candidate” wording inside the archives is explained by [the clarifications](RELEASE_CLARIFICATIONS.md).

### Known limitations

**D1 — not fixed:** On Windows, after some language changes, the window footer may temporarily retain the previous language. Data remained consistent in the observed scenarios. Manually resizing the window restored the display in the observed cases. Investigation remains ongoing.

Accessible text may already have changed while old pixels remain. The root cause and a causal relationship with historical issue #94 are unproven. The general transparency defect #94 was not reproduced in the latest campaigns; broader qualification reservations remain open.

- **Unsigned Windows executables**: SmartScreen warnings are possible; reputation and the SmartScreen download experience are unqualified. Do not globally disable Windows protections.
- Human Narrator listening and exhaustive accessibility certification have not been performed.
- Hardware qualification is incomplete: multiple monitors and DPI changes between monitors are not certified.
- GitHub Actions CI is intentionally disabled; evidence comes from local native checks.
- Ubuntu was qualified under X11; native Wayland and Android are unqualified for this release. Other environments are not certified.

Read [all limitations](PUBLIC_LIMITATIONS.md), including bounded inventory and additional exploration costs.

Code: **GPL-3.0-only**. Approved adapted artwork: **CC BY-SA 4.0**, attribution **Louis Lejuez / TazmenWorld**; incorporated adaptations use GPLv3 as explained in [BRANDING.md](../../../BRANDING.md). The personal portrait and reserved original artwork are excluded. Keep all licenses and third-party notices. No warranty.
