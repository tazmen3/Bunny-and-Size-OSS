> Version de distribution : les références internes non livrées sont indiquées en texte. Les preuves et plans détaillés restent dans le dépôt de développement ; aucun historique privé n’est fourni.

# Export détaillé CSV/TSV — schéma entries v1 (#77)

Exigences REPORT-001/003/007/008, SCAN-004/009/014/015, META-001/006.
Contrat distinct du [résumé v1 à 40 colonnes](CLI.md). Décision et bornes :
ADR-0031 [référence interne].

## Invocation

Raccordement CLI (tranche B) :
`sortsize scan PATH --format csv --scope entries --output inventory.csv`.
TSV remplace csv par tsv. Le scope par défaut reste summary. Text/JSON détaillés
sont refusés. Aucun nouveau champ ni collecte dans les résumés existants.

## Enregistrements et fin de flux

52 colonnes fixes, dans cet ordre (en-tête identique CSV et TSV) :

```text
export_schema_version,export_scope,record_type,path_display,path_raw_encoding,path_raw_units_json,name_display,name_raw_encoding,name_raw_units_json,kind,depth,logical_bytes,logical_availability,logical_accuracy,allocated_bytes,allocated_availability,allocated_accuracy,direct_files,direct_files_availability,direct_files_accuracy,direct_directories,direct_directories_availability,direct_directories_accuracy,recursive_files,recursive_files_availability,recursive_files_accuracy,recursive_directories,recursive_directories_availability,recursive_directories_accuracy,modified_unix_ns,modified_availability,modified_accuracy,accessed_unix_ns,accessed_availability,accessed_accuracy,time_resolution,owner_kind,owner_id,owner_availability,owner_accuracy,depth_limited,resource_limited,coverage,operation,error_kind,error_message,records,entry_records,error_records,observations_omitted,report_json,internal_spool_excluded
```

`export_schema_version=1`, `export_scope=entries` sur chaque enregistrement.
`record_type=entry` uniquement pour un fichier régulier ou dossier observé,
racine incluse si observée ; `kind=file|directory`. Une observation échouée
n'invente aucun type. `error`, `symlink_skipped`, `special_skipped`, `excluded`
sont des types distincts à kind vide. Un dossier déjà observé peut aussi avoir
une ligne error à son chemin si son listing échoue : une entrée + un diagnostic.
Les erreurs sont toutes exportées, indépendamment de `--max-errors` (ce plafond
ne concerne que les détails du résumé dans report_json).

`end` est **le dernier et unique trailer**, sans chemin ni métrique d'objet.
`records` compte tous les enregistrements avant end, hors en-tête ;
`entry_records` compte file/directory ; `error_records` les diagnostics ;
`observations_omitted=0` signifie aucune omission par rétention de l'export, sans
promesse sur les descendants jamais visités. `report_json` est le ScanReport v2
de ce scan (paramètres effectifs, limites/pics, compteurs et couverture).
`internal_spool_excluded=true` indique que le dossier privé du spool a été
rencontré puis élagué ; il augmente excluded_entries du résumé de 1 mais ne
produit pas de ligne contenant le nom temporaire aléatoire. Les autres exclusions
produisent leur propre ligne. report_json ne constitue pas un JSON détaillé.

Un consommateur doit parser jusqu'à EOF, vérifier un unique end final, vérifier
les trois compteurs avec les lignes lues, le LF final émis, le booléen
internal_spool_excluded et la cohérence couverture/report_json, puis lire
la couverture et le code de sortie. **Sans trailer valide, transport incomplet**, même si toutes les lignes
précédentes étaient parseables. `complete` : parcours terminé dans les exclusions
déclarées ; `partial` : erreurs/profondeur/budgets ; `cancelled` : interruption.
Les lignes affichent la couverture connue lors de leur écriture ; un signal
ultérieur peut encore rendre le trailer cancelled. Le trailer fait autorité.
Un fichier modifié pendant le scan n'est pas un snapshot ; une modification non
détectée ne peut pas être annoncée comme détectée.

## Valeurs

| Famille | Sémantique |
|---|---|
| path/name | chemin absolu canonisé, nom natif final (racine sans nom : chemin natif) ; présentation UTF-8, raw de secours si non Unicode |
| raw | cellules vides si Unicode ; sinon unix_bytes + tableau JSON d'octets, ou windows_utf16 + tableau JSON d'unités u16, sans normalisation |
| depth | racine zéro, nombre de composantes relatives |
| logical_bytes | fichier : longueur observée ; dossier : somme des fichiers descendants observés par chemin, hardlinks compris séparément |
| allocated_bytes | fichier régulier seulement, valeur/état acquis par l'adaptateur ; dossier : vide/not_collected, pas la taille physique de son sous-arbre |
| direct_* | fichiers réguliers/dossiers immédiatement enfants dans le périmètre, sous-dossiers exclus non comptés |
| recursive_* | descendants observés, dossier lui-même exclu du nombre ; racine comprise dans directories du report seulement |
| modified/accessed_unix_ns | entier signé décimal de nanosecondes depuis Unix UTC ; avant 1970 préservé exactement, pas de passage par flottant |
| time_resolution | native_resolution_unknown : encodage à la nanoseconde, résolution effective du FS inconnue ; atime dépend aussi de relatime/noatime et des politiques Windows |
| owner_kind/id | unix_uid décimal Linux, windows_sid_hex : octets binaires SID exacts encodés hex ; aucune résolution NSS/AD ni nom de compte |
| *_availability | known, unknown, unsupported, access_denied, not_collected ; known zéro = 0, aucune valeur inconnue transformée en zéro |
| *_accuracy | exact ou lower_bound seulement si known ; vide autrement |
| depth_limited/resource_limited | booléens sur dossier ; descendants jamais visités absents, compteurs globaux dans report_json |
| operation/error_kind/error_message | diagnostics observés, pas d'exception transformée en type fichier |

Exact qualifie la valeur observée, sans certifier la cohérence temporelle du
volume. Lower_bound qualifie les cumuls non négatifs affectés par des descendants
inconnus/limités/annulés. Les comptes directs sont alors aussi conservativement
qualifiés. Fichiers connus gardent leurs propriétés exactes. Une exclusion
explicite ne rend pas le périmètre partiel. Symlinks réels et pendants restent
ignorés, jamais suivis. Spécial n'est ni fichier régulier ni dossier.

## Ordre et reproductibilité

Racine puis composantes natives relatives triées lexicographiquement : bytes
Unix / unités UTF-16 Windows, ordre non signé, préfixe avant descendants, sans
collation ni conversion Unicode. Au même chemin : directory, file, error,
symlink_skipped, special_skipped, excluded, puis cellules complètes comme départage.
Ce tri par composantes peut différer d'un tri brut du chemin contenant les
séparateurs. Le trailer reste toujours dernier.

Sur un jeu figé sans saturation ni annulation, comparer les lignes après parsing.
Les mtime/atime du système peuvent varier entre scans ; ne pas confondre effets
relatime et défaut de tri. report_json contient horloge, durée, workers, pics et
sélection bornée d'erreurs variables. L'ordre complet des lignes reste garanti,
le sous-ensemble effectivement acquis peut varier quand le scan est limité.

## Convention et confidentialité

UTF-8 sans BOM, LF, virgule ou tabulation ; quoting CSV dans les deux formats :
champ contenant séparateur/guillemet/CR/LF entre guillemets, guillemets doublés.
CR/LF internes conservés. Nombres décimaux ASCII sans locale ni suffixe ; cellules
JSON compactes parsées dans un second temps. Une ligne logique peut occuper
plusieurs lignes physiques : ne jamais compter avec un simple découpage LF.

Les noms/erreurs peuvent commencer par `=`, `+`, `-`, `@` ou des contrôles.
**Le quoting CSV ne neutralise pas les formules des tableurs.** Les données
sources sont préservées sans apostrophe ni autre transformation silencieuse.
Importer les colonnes textuelles explicitement comme texte, désactiver
l'interprétation de formules et de liens ; employer un parseur CSV pour les
consommations automatiques. Raw fait autorité en cas de libellé lossy et permet
la reconstruction native seulement sur la plateforme compatible. Ne jamais
utiliser la présentation pour une opération sur le système de fichiers.

Chemins, noms, UID/SID et erreurs sont sensibles. Export et spool restent locaux,
sans télémétrie ni réseau. Le spool est privé et nettoyé à la sortie normale ;
une interruption forcée peut le laisser. Le destinataire de l'export contrôle
ensuite son accès. Les permissions du fichier publié sont précisées par la CLI.

## Transport et ressources


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

L'API Rust `sortsize_core::entries::export_entries` écrit vers `Write` faillible.
Tout échec d'écriture/spool/tri/overflow/limite dure est fatal, sans panic de
production. L'appelant doit flush la destination ; le trailer ne remplace pas
la vérification d'erreur du transport. Annulation arrête l'acquisition puis trie
et finalise les observations consommées ; cette finalisation coûte O(N log N).
Pas de promesse de sortie immédiate pendant un appel OS ou un tri externe.
Les bornes de structures, coût disque et alternatives sont dans ADR-0031.

## Validation native

Ubuntu et Windows 11/MSVC/NTFS ont été exécutés : preuves et réserves [référence interne].
Sous NTFS, l’énumération peut modifier l’atime des dossiers entre deux scans ;
la valeur exportée reste celle observée, sans promesse de snapshot. Les tests
Windows incluent UTF-16 invalide, SID/ACL et CTRL_C_EVENT automatisé ; disque
plein réel Windows et performances physiques restent hors de cette preuve VM.
