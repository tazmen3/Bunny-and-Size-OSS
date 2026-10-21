# Licences / Licensing

**Historical v0.1 documentation.** For the prepared 0.2.0-rc.1, see [current release materials](releases/v0.2.0-rc.1/RELEASE_NOTES.md) and [corresponding-source downloads](releases/v0.2.0-rc.1/DOWNLOADS.md). The buildable product snapshot on this branch is v0.2; the old statements below retain their historical scope.

SortSize est diffusé sous **GPL-3.0-only**, sans garantie dans les limites de la loi ; lire [LICENSE](../LICENSE). Le propriétaire a confirmé disposer des droits nécessaires à cette diffusion. [AUTHORS.txt](../AUTHORS.txt) conserve la liste originale de contributeurs Git, sans en déduire un titulaire exclusif ou inventer une ligne de copyright.

La GUI utilise Slint **1.18.1**, voie GPL-3.0-only. Les icônes Fluent proviennent de Microsoft sous MIT ; les compléments Material/Apache et autres mentions restent ceux des paquets. Runtime Rust et dépendances Cargo conservent leurs licences propres, y compris les obligations cumulatives AND. Les bibliothèques système Ubuntu et le runtime Microsoft sont externes, non redistribués dans les paquets.

Notices originales de plateforme : [Windows](notices/THIRD_PARTY_NOTICES_windows.txt) et [Linux](notices/THIRD_PARTY_NOTICES_linux.txt). Les chemins `licences/` cités y désignent les fichiers complets **dans les archives de la plateforme**, avec leurs sources correspondantes ; ce dépôt ne vendore pas ces dépendances. Les deux bundles source contiennent les textes et attributions complets, les dépendances verrouillées et les recettes hors ligne. Aucune licence tierce n’est remplacée par celle de SortSize.

La provenance des entrées de génération de `bindings_docs.rs` dans skia-bindings n’a pas été reproduite. Cette archive amont verrouillée et ses compléments légaux sont conservés dans le bundle source ; Skia n’est pas incorporé aux exécutables livrés. Le marqueur VCS dirty seul n’est pas traité comme une interdiction. Aucun certificat de conformité juridique globale n’est délivré.

SortSize is released under **GPL-3.0-only**, without warranty to the extent permitted by law. The owner confirmed the necessary publication rights. AUTHORS preserves the original Git contributor list; it is not an exclusive ownership statement. Slint 1.18.1 uses its GPL option. Fluent/Microsoft MIT, Material/Apache, Rust-runtime and Cargo notices retain their own terms; cumulative AND conditions remain applicable.

Original platform notices are linked above. Their `licences/` references resolve **inside each distributed archive**, where the complete texts are supplied; dependencies are not vendored into this browsing snapshot. The named source release assets include locked dependencies and offline recipes. Ubuntu system libraries and the Microsoft runtime remain external. No global legal-compliance certification is claimed; the documented optional Skia generated-input provenance limitation is retained.
