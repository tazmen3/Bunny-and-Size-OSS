> Version de distribution : les références internes non livrées sont indiquées en texte. Les preuves et plans détaillés restent dans le dépôt de développement ; aucun historique privé n’est fourni.

# CLI Bunny and Size

Commande distribuée : `bunny-and-size-cli` ; alias historique : `sortsize`.
Les deux commandes partagent options, codes de sortie et formats.

Depuis la racine du projet, après installation de Rust :

```bash
cargo run -p sortsize-cli -- scan /chemin/du/dossier
cargo run -p sortsize-cli -- scan /chemin/du/dossier --format json
cargo run -p sortsize-cli -- scan /chemin/du/dossier --format json --output scan.json
cargo run -p sortsize-cli -- scan /chemin/du/dossier --format csv --output resume.csv
cargo run -p sortsize-cli -- scan /chemin/du/dossier --format tsv > resume.tsv
cargo run -p sortsize-cli -- scan /chemin/du/dossier --scope entries --format csv --output inventory.csv
cargo run -p sortsize-cli -- scan /chemin/du/dossier --format csv --progress always
cargo run -p sortsize-cli -- scan /chemin/du/dossier --exclude '**/.git/**' --exclude '*.tmp'
cargo run -p sortsize-cli -- scan /chemin/du/dossier --max-depth 3
cargo run -p sortsize-cli -- scan /chemin/du/dossier --workers 4
```

Sur Windows, fournir un chemin natif tel que `D:\Donnees\Documents`. Les chemins contenant des espaces doivent être entre guillemets. Les commandes supposent Cargo disponible dans le PATH ; un nouveau terminal peut être nécessaire après installation de rustup.

## Build et contrôles

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --release -p sortsize-cli
```

Les exécutables produits sont `target/release/bunny-and-size-cli` et l’alias `target/release/sortsize` sous Linux (suffixe `.exe` sous Windows). La CLI ne dépend pas du prototype Slint.

## Contrat de scan

- Lecture des métadonnées et des dossiers ; le contenu des fichiers n'est pas lu ni modifié par le scanner.
- La racine doit être un dossier réel. Un fichier ou un lien symbolique fourni comme racine est refusé. La racine est ensuite canonisée et exportée sous forme absolue.
- Les liens symboliques sont comptés comme ignorés. `--no-follow-symlinks` explicite la politique par défaut ; aucune option de suivi n'est actuellement proposée.
- Les exclusions sont des globs relatifs à la racine. Un dossier exclu est élagué ; `excluded_entries` compte les entrées exclues observées, pas les descendants non visités.
- Les montages accessibles sont traversés. Pour un scan global Linux de `/`, `proc`, `sys` et `dev` sont automatiquement exclus et figurent dans le rapport. Un scan ciblé explicitement sur une de ces sources reste possible et peut être incomplet ou instable.
- `directories` inclut la racine et les dossiers observés dont le listing a échoué.
- `logical_bytes` est la somme des longueurs par chemin de fichier. Les hardlinks ne sont pas dédupliqués. `allocated_bytes` et `potentially_reclaimable_bytes` sont inconnus (`null`).
- `hardlinked_file_paths` signale sous Unix les chemins dont le nombre de liens dépasse un. Sous Windows cette métrique est inconnue à ce stade.
- JSON : schéma version 2, date Unix en millisecondes, source, durée, couverture, paramètres et erreurs. Pour les chemins non Unicode, `raw` préserve les octets Unix ou unités UTF-16 Windows.
- Par défaut, 100 détails d'erreurs au maximum sont conservés. `--max-errors` modifie cette limite ; `errors_count` et `errors_omitted` gardent la trace des autres erreurs.
- Le parcours séquentiel par défaut garde une seule liste ouverte et au plus 65 536 dossiers / 8 Mio de chemins natifs en attente. `--max-pending-directories` et `--max-pending-path-bytes` règlent ces budgets (valeurs positives). Une saturation augmente `resource_limited_directories`, rend les tailles inférieures ou égales au total réel et produit `partial` / code 2 ; les descendants non visités ne sont pas comptés. `traversal_limits` et `traversal_stats` distinguent limites et pics observés. Ces bornes de structures ne sont pas un plafond de RSS.
- `--workers 1` conserve le mode séquentiel par défaut ; `--workers 2` à `32` active le pool partagé (au plus 16 listes ouvertes avec les options CLI actuelles). Les budgets de dossiers et d’inventaire restent globaux. Les gains varient selon l’arborescence et le support ; 8 peut être plus lent que 4.
- La file commune du pool garde au plus 1 024 observations et 1 Mio de chemins encodés, réglables par `--max-buffered-observations` et `--max-buffered-path-bytes` (positifs). Chaque travailleur et l’agrégateur peuvent en plus détenir un lot d’au plus 32 observations et ce budget de chemins. Voir ADR-0007 [référence interne] pour la borne complète de structures. Un chemin isolé plus long que le budget de transmission provoque une erreur fatale ; relever le budget ou choisir 1 travailleur.
- Les ajouts JSON v2 `concurrency` et `traversal_stats.worker_threads` distinguent demande et nombre effectif ; les pics `peak_buffered_observations` / `peak_buffered_path_bytes` décrivent seulement la file partagée. En séquentiel, ils valent zéro et `worker_threads` vaut 1. Les consommateurs doivent accepter les champs supplémentaires.
- Le JSON v2 conserve les champs v1 et ajoute ces informations de parcours ; un consommateur imposant v1 doit accepter explicitement cette évolution. Les exclusions et les erreurs retenues sont désormais ordonnées à l'export (tous formats), sans changer le schéma ou les métriques. En cas de saturation, le sous-ensemble retenu peut varier ; tout résultat limité reste explicitement partiel.
- Ctrl+C demande l'annulation et produit le résumé déjà acquis. L'arrêt attend la fin d'un éventuel appel OS bloqué.
- stdout contient uniquement le rapport choisi (`text` par défaut, `json`, `csv`, `tsv`). Avec `--output`, stdout est vide. La progression et les diagnostics fatals vont sur stderr. `--progress auto` (défaut) affiche la progression si stderr est un terminal ; `always` l'active aussi dans un pipeline/journal, `never` la masque. `--quiet` prime sur ces trois modes. Les diagnostics fatals et erreurs d'arguments restent visibles, même avec `--quiet` ; les détails d'erreurs de scan font aussi partie des données du rapport. Quand activée, la progression annonce le démarrage après installation du gestionnaire Ctrl+C, puis les compteurs au plus une fois toutes les 250 ms ; un scan court peut n'afficher que le démarrage.
- Le fichier `--output` est créé après le scan. Un fichier déjà existant n'est jamais remplacé. L'export n'est pas transactionnel en cas de coupure d'alimentation ou de disque plein ; une écriture échouée peut laisser un fichier incomplet.

## Couverture et codes de sortie

| Code | Résultat |
|---|---|
| 0 | Scan complet dans les exclusions déclarées, rapport écrit |
| 1 | Erreur fatale : racine, options, sérialisation ou écriture |
| 2 | Scan partiel (erreurs, profondeur ou ressources limitées), rapport écrit ; également erreur d'arguments Clap avant tout scan |
| 130 | Scan annulé, rapport partiel écrit |

Une racine manquante/invalide produit un message sur stderr et aucun rapport. `coverage` distingue `complete`, `partial` et `cancelled`. Un état complet signifie que le parcours prévu est terminé ; il ne garantit pas un snapshot cohérent d'un volume en cours de modification.

L’export détaillé CSV/TSV est disponible explicitement avec `--scope entries` (contrat ci-dessous). La restriction à un volume, l’historique, la recherche et les doublons restent à développer.

## CSV/TSV — résumé versionné, pas inventaire détaillé (#10)

`--format csv` et `--format tsv` projettent **le même résumé `ScanReport` que JSON v2** :
un en-tête et **une seule ligne de données** pour la racine analysée. Une ligne
peut occuper plusieurs lignes physiques si un chemin contient un retour à la
ligne. Ni les fichiers ni les dossiers individuels, dates, allocations ou Owner
de l'API d'inventaire ne sont énumérés. Cet export ne constitue donc **pas un
export complet de l'arborescence**, même si `coverage=complete`.

Le schéma tabulaire est `export_schema_version=1`, `export_scope=summary` ;
`schema_version=2` désigne le modèle de résumé JSON source, toujours conservé.
CSV et TSV ont exactement les mêmes **40 colonnes dans l'ordre suivant** :

```text
export_schema_version,export_scope,schema_version,provider,root_display,root_raw_encoding,root_raw_units_json,started_at_unix_ms,duration_ms,coverage,files,directories,logical_bytes,logical_size_basis,allocated_bytes,potentially_reclaimable_bytes,symlinks_skipped,special_entries_skipped,excluded_entries,depth_limited_directories,resource_limited_directories,max_pending_directories,max_pending_path_bytes,peak_pending_directories,peak_pending_path_bytes,peak_open_directories,worker_threads,peak_buffered_observations,peak_buffered_path_bytes,workers,max_buffered_observations,max_buffered_path_bytes,hardlinked_file_paths,errors_count,errors_omitted,errors_json,follow_symlinks,exclusions_json,max_depth,mount_policy
```

| Colonnes | Valeur / unité |
|---|---|
| `export_schema_version`, `schema_version` | Entiers de version, respectivement 1 et 2 |
| `export_scope`, `provider` | `summary`, `local` |
| `root_display` | Chemin absolu canonisé par l'OS, représentation UTF-8 de présentation |
| `root_raw_encoding`, `root_raw_units_json` | Vides si Unicode ; sinon `unix_bytes` + tableau JSON d'octets 0–255, ou `windows_utf16` + tableau JSON d'unités 0–65535 |
| `started_at_unix_ms` | Millisecondes entières depuis l'époque Unix (UTC), horloge système |
| `duration_ms` | Millisecondes entières écoulées (horloge monotone), inclut le scan, exclut l'export |
| `coverage` | `complete`, `partial`, `cancelled` ; mêmes sens que JSON |
| `files`, `directories` | Comptages de chemins ; dossiers racine incluse, fichiers hardlinkés comptés séparément |
| `logical_bytes`, `allocated_bytes`, `potentially_reclaimable_bytes` | Octets, base 1 ; logique par chemin, allouée/récupérable inconnues dans cette CLI |
| `logical_size_basis` | `per_file_path` |
| `symlinks_skipped`, `special_entries_skipped`, `excluded_entries` | Nombres d'entrées observées ignorées ; les descendants élagués ne sont pas comptés |
| `depth_limited_directories`, `resource_limited_directories` | Nombres de dossiers dont le contenu n'a pas été parcouru pour ce motif |
| `max_pending_directories`, `peak_pending_directories`, `peak_open_directories` | Nombres de dossiers : limite en attente, pic en attente, pic ouvert |
| `max_pending_path_bytes`, `peak_pending_path_bytes` | Octets de chemins natifs encodés, hors surcoût d'allocation |
| `worker_threads`, `workers` | Nombres de travailleurs effectifs et demandés |
| `peak_buffered_observations`, `max_buffered_observations` | Nombres d'observations dans la file partagée (pic, limite) |
| `peak_buffered_path_bytes`, `max_buffered_path_bytes` | Octets de chemins encodés dans la file partagée (pic, limite) |
| `hardlinked_file_paths` | Nombre de chemins de fichiers à plusieurs liens sous Unix ; inconnu sous Windows |
| `errors_count`, `errors_omitted` | Nombre total d'erreurs observées et nombre de détails non conservés |
| `errors_json` | Tableau JSON compact de détails `path` (`display`, `raw` si nécessaire), `operation`, `kind`, `message` ; même structure que JSON v2 |
| `follow_symlinks` | Booléen ASCII `false` (politique actuelle) |
| `exclusions_json` | Tableau JSON compact des globs effectifs, exclusions automatiques comprises |
| `max_depth` | Profondeur entière, racine = 0 ; vide signifie sans limite |
| `mount_policy` | `cross_accessible_mounts` |

Les nombres sont des entiers décimaux ASCII sans séparateur de milliers,
sans suffixe ni influence de la locale. **Une métrique inconnue (`null` en JSON)
est une cellule vide**, tandis qu'une valeur connue nulle est `0`. Cela concerne
`allocated_bytes`, `potentially_reclaimable_bytes`, `hardlinked_file_paths`.
Un tableau vide est `[]`. Pour `max_depth` et les deux colonnes raw, le vide
signale l'absence de limite ou d'encodage de secours, selon le tableau ci-dessus.
Les colonnes temporelles peuvent dépasser un entier 64 bits : ne pas les lire
via un flottant si l'exactitude entière est nécessaire.

Les fichiers sont en **UTF-8 sans BOM**, séparateur virgule pour CSV et
tabulation pour TSV, fin d'enregistrement **LF**, y compris sous Windows.
Un champ contenant son séparateur, un guillemet double, CR ou LF est entouré
de guillemets doubles ; chaque guillemet interne est doublé. Les retours à la
ligne internes restent intacts. TSV utilise exactement cette convention de
guillemets CSV avec un autre séparateur ; ce n'est pas un TSV à échappements
`\\t`/`\\n`. Utiliser un parseur CSV configuré avec le bon séparateur, puis
un parseur JSON pour les cellules `*_json` (deux couches distinctes).

`root_display` garde la syntaxe native (barres obliques inverses et éventuel
préfixe Windows de canonisation compris), sans normalisation Unicode. Pour un
chemin non Unicode, sa présentation contient des caractères de remplacement :
**les unités raw font autorité** et préservent aussi les surrogates UTF-16
isolés. Même contrat pour les chemins des erreurs dans `errors_json`. Les noms
non Unicode des fichiers sont acceptés et comptés, mais ne sont pas listés dans
ce résumé. Les arguments `PATH` et `--output` restent des chemins OS natifs.

## Ordre et reproductibilité

L'en-tête et l'ordre des champs sont fixes. Tous les formats trient les globs
effectifs par chaîne UTF-8 croissante (doublons conservés), et les détails
d'erreurs par `(path.display, type raw absent/unix_bytes/windows_utf16,
unités raw, operation, kind, message)`, en ordre croissant lexicographique.
Le départage raw évite de confondre deux chemins dont la présentation lossy
est identique. Cela modifie uniquement l'ordre des listes du JSON v2, pas son
schéma ni ses valeurs ; le mode texte bénéficie du même ordre d'erreurs.
Pour des valeurs scalaires et des listes retenues équivalentes, chaque format
produit les mêmes octets après cet ordonnancement.

Deux scans indépendants ne sont **pas** nécessairement identiques octet par octet :

- `started_at_unix_ms` et `duration_ms` varient naturellement ;
- les cinq champs `peak_*` dépendent du parcours et de l'ordonnancement ;
- racine canonisée, support et plateforme, permissions, messages OS/localisés,
  options et exclusions automatiques peuvent différer ;
- les modifications concurrentes du système de fichiers ne sont pas figées ;
- l'annulation, les budgets saturés et la troncature `--max-errors` peuvent
  retenir des sous-ensembles différents ; trier ne reconstitue pas les éléments omis.

Pour comparer des scans d'une fixture stable sans saturation, comparer les
données après parsing, à options et plateforme équivalentes, en excluant dates,
durée et pics. Vérifier aussi couverture et compteurs d'omission. Le banc
`validate_cli_exports.py` [référence interne] applique ce
contrat et compare les trois formats ; les tests Rust figent les octets d'un
même rapport aux listes permutées. Aucun mode ne promet un snapshot ni un
inventaire exhaustif. Décision technique [référence interne],
preuves et matrice native [référence interne].

### V0.2 — collecte d'allocation du moteur (#14, tranche A)

La nouvelle API Rust optionnelle d'inventaire combine dates et allocations sans
second parcours. Elle ne change aucune option CLI ni le schéma JSON v2 :
`allocated_bytes` et `potentially_reclaimable_bytes` restent `null`.
Aucun cumul physique ou gain récupérable n'est déduit des hardlinks/sparse.
Contrat et limites [référence interne], tests [référence interne].

### V0.2 — cumuls de tranche B (#14)

L'API Rust opt-in d'inventaire expose des cumuls alloués par chemin et un total
borné distinct par identité. Aucune option ni clé CLI nouvelle : JSON v2 conserve
`allocated_bytes: null` et `potentially_reclaimable_bytes: null`.
Contrat [référence interne], preuves de non-régression [référence interne].


## CSV/TSV — inventaire détaillé entries v1 (#77)

`--scope summary` est le défaut et conserve les résumés précédents. Utiliser
`--scope entries --format csv|tsv` pour le nouveau schéma indépendant :
**52 colonnes**, une ligne par fichier/dossier observé, enregistrements distincts
pour erreurs/liens ignorés/exclusions, puis trailer `end` obligatoire. JSON et
text avec entries, ou options de tri externe avec summary, sont refusés avec
code 1 ; les erreurs de syntaxe Clap conservent le code 2.

[Contrat complet, colonnes et parsing](ENTRIES_EXPORT.md) : chemins/noms natifs
sans perte, profondeur zéro à la racine, tailles logiques par chemin, allocation
fichier connue seulement, comptes directs/récursifs, dates UTC signées en ns,
UID Linux/SID Windows binaire, cinq disponibilités et exact/lower_bound séparés.
Aucune résolution de noms ni nouvelle collecte sur le chemin summary. L’inventaire
GUI à 50 000 nœuds ne limite pas cet export.


**Espace temporaire à prévoir avant un gros export :** la campagne historique
à 1 001 001 entrées a observé jusqu'à **1 147 Mio de spool logique sous Windows**
pour environ **368,5 Mio de sortie** (Ubuntu : ~1 009 Mio de spool). Ces pics
échantillonnés ne sont pas des plafonds. Prévoir une marge pour les segments
anciens et nouveaux de fusion, le deuxième tri et le staging de destination ;
plusieurs millions d'entrées peuvent nécessiter plusieurs Gio. Choisir un
volume adapté avec `--temporary-directory PATH`. Une insuffisance d'espace
est fatale (code 1) ; aucun export fichier incomplet n'est publié comme terminé.
Le spool et l'export contiennent chemins et UID/SID sensibles ; vérifier les
permissions/ACL de la destination. Une interruption forcée peut laisser le spool.

`--sort-memory-bytes N` règle le budget de lignes de chaque tri externe
(8 Mio par défaut ; positif), et `--temporary-directory PATH` choisit le parent
du spool privé. La pile d’agrégation reste limitée à 4096 dossiers/8 Mio ; une
observation sérialisée à 1 Mio. Dépassement dur/écriture impossible : code 1,
sans succès tronqué. Le scanner garde ses limites/progression/exclusions
historiques ; un périmètre réellement limité donne partial/code 2. Les bornes
ne constituent pas un plafond de RSS. Coûts/alternatives [référence interne],
mesures reproductibles [référence interne].

Avec `--output`, l’export détaillé utilise un staging dans le parent de la
destination, créé lors de la première écriture **après le scan**, donc absent des
observations. Le fichier demandé est publié après écritures, flush et sync_all
réussis, avec persist_noclobber : aucune destination existante ou créée entre-temps,
même symlink pendant, n’est remplacée. En erreur le staging est nettoyé par RAII ;
une interruption forcée peut laisser un staging. Export partiel/annulé acquis
publié avec son trailer explicite et code 2/130. Sous Unix le staging/fichier
publié est 0600 (umask peut encore restreindre) ; sous Windows les ACL du parent
s’appliquent. Ne pas assimiler cette publication à une garantie universelle
d’atomicité ou de durabilité après coupure : parent non fsync, sémantique du FS.
Le mode summary conserve sa création historique sans staging.

Vers stdout, données seules, erreur fatale sur stderr et code 1 pour pipe rompu
ou ENOSPC. Un consommateur doit vérifier l’EOF après le trailer `end`, ses comptes
et sa couverture : l’absence de trailer invalide le flux. Ctrl+C arrête le scan
puis finalise les observations acquises ; le tri externe et la sortie prennent
encore du temps. Un signal pendant ces passes rend aussi le trailer cancelled.
Aucun snapshot n’est promis. Formules des tableurs : quoting insuffisant,
**importer les textes en texte**, jamais préfixer/altérer les chemins sources en
silence. Raw, UID/SID et diagnostics sont sensibles ; partage à contrôler.

Windows : **testé nativement dans la VM Windows 11/MSVC/NTFS**, avec réserves ;
rapport, ACL/SID/UTF-16, annulations et mesures [référence interne].
La compilation GNU sous Ubuntu reste une preuve distincte.
