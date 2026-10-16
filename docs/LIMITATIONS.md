# Limites connues / Known limitations

## Français

- **Préversion expérimentale**, aucune stabilité ni conformité juridique certifiée annoncée.
- Windows propre sans Rust/Visual Studio : **non testé** ; runtime absent puis installé : non testé. Les EXE sont non signés ; téléchargement/SmartScreen non certifié.
- **KDE/X11** : blocage ponctuel sous injections clavier rapides et interactions de champs texte, cause non établie. Des sessions indépendantes et la saisie temporisée ont réussi avec IBus actif ; ces succès ne prouvent pas la résolution du blocage. IBus n’est pas présenté comme la cause.
- **Composition IME, clavier matériel, accessibilité complète et Wayland/XWayland non certifiés.** Backend livré X11, pas Wayland natif activé. Aucun contournement IBus n’est annoncé comme solution démontrée.
- Le dialogue KDE peut altérer les noms Unix non UTF-8 ; le backend GTK a été validé séparément pour ce cas. Tous les cas de chemins longs Windows, notamment sans alias NTFS, ne sont pas certifiés.
- Ubuntu 24.04 x86_64, glibc 2.39, Fontconfig et environnement X11 ciblés ; pas de certification générale de toutes distributions/versions de Windows.
- Analyse en lecture seule, non transactionnelle : pas de snapshot de fichiers modifiés pendant le scan. Refus d’accès et limites de parcours produisent des résultats partiels explicites. Un appel système bloqué peut retarder l’annulation.
- L’affichage retient au plus 50 000 entrées / 8 Mio de chemins ; le scan peut continuer au-delà de cette limite de rétention. Symlinks non suivis par défaut. Tailles logiques par chemin, sans estimation de l’espace récupérable ou garantie de déduplication des hardlinks.
- Pas de Treemap, recherche avancée, détection/gestion de doublons, suppression, historique SQLite, connecteurs cloud/NAS ou paquet Android dans cette livraison.
- Les archives inchangées contiennent des chemins de compilation/provenance acceptés par le propriétaire. La copie publique sans historique n’enlève pas ces chaînes.

## English

- **Experimental prerelease**; no stable-release or certified legal-compliance claim.
- A clean Windows system without Rust/Visual Studio and the missing-runtime-then-installed scenario **have not been tested**. EXEs are unsigned; the download/SmartScreen experience is not certified.
- **KDE/X11**: an occasional hang occurred during rapid keyboard injection and text-field interaction. Its cause is undetermined. Independent sessions and paced typing succeeded with IBus active, which does not establish that the hang is fixed or caused by IBus.
- **IME composition, physical keyboard, complete accessibility and Wayland/XWayland are not certified.** The shipped backend is X11; no native Wayland backend or proven IBus workaround is claimed.
- KDE folder selection may alter non-UTF-8 Unix names; GTK was tested separately for that case. Not all Windows long-path cases, including paths without NTFS aliases, are certified.
- Target: Ubuntu 24.04 x86_64 with glibc 2.39, Fontconfig and X11. No general compatibility certification for all distributions or Windows versions.
- Read-only, non-transactional scans do not provide a snapshot of concurrently changing files. Access errors, cancellation and traversal limits can yield explicit partial results. A blocked system call can delay cancellation.
- Display retention is limited to 50,000 entries / 8 MiB of native paths; counting may continue beyond the display limit. Symlinks are not followed by default. Sizes are logical per path, with no recoverable-space or hardlink-deduplication guarantee.
- No Treemap, advanced search, duplicate management, deletion, SQLite history, cloud/NAS connectors or Android package in this release.
- Unchanged archives retain previously accepted build/provenance paths; removing Git history does not remove those strings.
