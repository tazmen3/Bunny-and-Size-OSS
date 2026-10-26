# Limitations — Bunny and Size v0.3.0-rc.1

26 octobre 2026 / 26 October 2026. Réserves acceptées par le propriétaire pour cette RC publique ; elles restent présentes. / The owner accepted these reservations for this public RC; they remain applicable.

## Français

- **D1 Windows, non corrigé :** après certains changements de langue, le pied peut conserver temporairement la langue précédente tandis que UIA expose la nouvelle. Les données restent cohérentes dans les scénarios observés. Un redimensionnement manuel a rétabli l'affichage. Cause inconnue, investigation en cours ; aucune correction ni extension à Ubuntu démontrée.
- **#94 :** la transparence générale historique n'a pas été reproduite lors des dernières campagnes. Cela ne clôt pas les réserves matérielles ni D1.
- **Windows :** exécutables non signés, avertissement SmartScreen possible, réputation non qualifiée. Microsoft Visual C++ Redistributable x64 v14 requis. Distribution portable par extraction, sans MSI/DEB. Aucune désactivation globale des protections n'est recommandée.
- **Accessibilité :** UIA Windows et AT-SPI Ubuntu exposent des textes ; l'écoute humaine du Narrateur et une certification exhaustive (focus, raccourcis, contrastes) restent non réalisées.
- **Matériel :** preuves VM/RDP et physiques distinctes ; tous les pilotes, le multi-écran et les transitions DPI entre moniteurs ne sont pas couverts.
- **Plateformes :** Ubuntu 24.04 x86_64 sous X11 et Windows 11 x64 qualifiés dans les scénarios documentés ; Wayland natif, Android et autres environnements non qualifiés pour cette livraison.
- **CI :** GitHub Actions volontairement désactivé ; les tests natifs locaux ne sont pas des exécutions de CI distante.
- **Inventaire :** rétention bornée à 50 000 entrées et 8 MiB de chemins natifs ; omissions signalées, totaux de scan conservés. Exploration complémentaire hors cache O(N) par demande, O(P×N) pour P pages ; le contenu peut avoir changé depuis le scan initial.
- **Données :** tailles logiques et allouées distinctes ; hardlinks comptés par chemin selon l'indication affichée. Inconnu n'est pas zéro. Les capacités des sources ne sont pas additionnables.
- **Périmètre :** exports GUI, historique SQLite, analyse Android SAF sur appareil et fonctions NAS/cloud/transfert non livrés par cette RC desktop.

## English

- **Windows D1, not fixed:** after some language changes, the footer may temporarily retain the previous language while UIA exposes the new one. Data remained consistent in observed scenarios. Manual resizing restored the display. The cause is unknown and investigation continues; no fix or extension to Ubuntu is established.
- **#94:** historical general transparency was not reproduced in the latest campaigns. Hardware reservations and D1 remain open.
- **Windows:** unsigned executables, possible SmartScreen warning, unqualified reputation. Microsoft Visual C++ Redistributable x64 v14 required. Portable extraction, no MSI/DEB installer. Globally disabling protections is not recommended.
- **Accessibility:** Windows UIA and Ubuntu AT-SPI expose text; human Narrator listening and exhaustive certification (focus, shortcuts, contrast) have not been performed.
- **Hardware:** VM/RDP and physical evidence are distinct; qualification does not cover all drivers, multiple monitors or cross-monitor DPI changes.
- **Platforms:** Ubuntu 24.04 x86_64 under X11 and Windows 11 x64 were qualified in the documented scenarios; native Wayland, Android and other environments are unqualified for this release.
- **CI:** GitHub Actions is intentionally disabled; local native tests are not remote CI runs.
- **Inventory:** retention is bounded at 50,000 entries and 8 MiB of native paths; omissions are reported and scan totals retained. Uncached additional exploration costs O(N) per request, O(P×N) for P pages; content can change after the initial scan.
- **Data:** logical and allocated sizes differ; hardlinks are counted by path as indicated in the UI. Unknown is not zero. Source capacities cannot be added together.
- **Scope:** GUI exports, SQLite history, on-device Android SAF scans and NAS/cloud/transfer functions are not delivered by this desktop RC.

Qualification is limited to the executed scenarios. Ignored tests were not counted as passes. / La qualification se limite aux scénarios exécutés. Les tests ignorés ne sont pas comptés comme réussis.
