# V0.4 — recherche, statistiques et doublons

Après une analyse desktop terminée, **Recherche / statistiques** ouvre une fenêtre
sur l’inventaire retenu. La source est affichée ; le périmètre peut correspondre à
une fenêtre complémentaire et reste borné à 50 000 nœuds / 8 Mio de chemins.
Ce n’est pas un index complet du disque. Les omissions et la couverture restent
visibles. Les filtres combinés par ET et les changements de langue ou d’unité
ne déclenchent aucun nouveau scan ni accès de contenu.

Nom/chemin/extension et glob relatif, minimum/maximum inclusifs en octets entiers.
La casse est ignorée par défaut sur les deux OS ; **Respecter la casse** l’active
explicitement. Les filtres texte utilisent Unicode lowercase, sans collation locale ; les globs
suivent les règles de globset.
Valider avec Entrée depuis un champ ou cliquer Rechercher. Un critère invalide
retire les anciens résultats et explique le problème.

Tous les résultats retenus sont accessibles par pages de **200 lignes**. Tri par
chemin natif ou taille (les inconnus restent en fin) ; les statistiques portent
sur toutes les correspondances, avant pagination. **Afficher** sélectionne le
fichier dans son parent, synchronise le tableau et le Treemap et conserve son
identité. L’inventaire initial peut être restauré depuis une vue complémentaire
sans relire le disque. Un chemin devenu absent de l’arbre est déclaré indisponible.
Une nouvelle analyse invalide les résultats précédents.

**Statistiques** affiche les comptes de chemins de fichiers réguliers, les tailles
logiques connues, les 32 extensions les plus volumineuses et six classes de taille.
Les barres d’extensions représentent les octets connus ; celles des classes, les
nombres de fichiers. Les seuils de classes restent binaires ; les valeurs suivent
le choix d’unités binaire/décimal. `≥` indique une somme connue avec des tailles
manquantes ; `—` une absence ou un débordement. Les hardlinks comptent par chemin.
Les autres groupes d’extensions restent inclus dans les totaux.

**Chercher les doublons exacts** lit volontairement les candidats de tous les
résultats retenus correspondant aux critères, pas seulement la page visible.
Présélection par taille, SHA-256 puis comparaison binaire ; au plus 1 Gio de lectures
et 512 Mio/fichier par défaut. Les copies indépendantes et les alias hardlink sont
distingués. **Afficher** expose le SHA-256 et le groupe vérifié ; les erreurs et
limites sont consultables. Les groupes observés sont paginés. Une taille de fichier
inconnue rend la couverture des doublons incomplète. Aucun espace récupérable
n’est estimé ; aucune suppression, déplacement ou modification de source.

Annuler ou fermer le panneau demande l’annulation coopérative. Un appel système
ou SMB déjà bloqué peut retarder le retour. Le changement de source abandonne les
anciens résultats ; aucun nouveau worker de contenu n’est admis avant le retour
du précédent. Les filtres, messages, pages et résultats suivent les sept langues.
Les filtres et diagnostics défilent séparément des résultats en petite fenêtre.

CLI (nouvelles fonctions, anciens `scan` et exports conservés) :

```sh
bunny-and-size-cli analyze /chemin --extension txt --min-bytes 0 --max-bytes 1048576
bunny-and-size-cli analyze /chemin --glob '**/*.rs' --kind file --format tsv --output recherche.tsv
bunny-and-size-cli analyze /chemin --duplicates --max-read-bytes 1073741824 --output doublons.json
bunny-and-size-cli analyze /chemin --modified-after 1704067200 --exclude '**/.git/**'
```

Dates CLI : secondes UTC depuis 1970, bornes inclusives ; dates absentes non
assimilées à zéro. Texte : Unicode lowercase, pas de collation locale, glob selon
la syntaxe native globset ; les noms non Unicode restent exportables, un prédicat
texte impossible à évaluer est compté séparément. Chemins utilisés nativement.

## Export d'analyse v1

JSON : `analysis_schema_version=1`, `search` (couverture, indices locaux, limites,
statistiques), `entries` (chemin `NativePath`, type, octets facultatifs), `duplicates`
facultatif. Les statistiques comptent uniquement les chemins de fichiers réguliers
correspondants, jamais les dossiers ; hardlinks par chemin. `overflow=true` invalide
la somme associée ; `unknown_sizes>0` signale une somme connue seulement partielle.
Une extension sans nom et une extension non Unicode restent distinguées.

CSV/TSV : trois colonnes `analysis_schema_version,record_type,data_json` ; lignes
`entry`, éventuellement `duplicates`, puis `end` contenant les métadonnées et
statistiques de recherche. Les cellules JSON conservent les chemins natifs et
commencent par `{` : aucune formule provenant d'un nom de fichier. Guillemets
échappés, fin de ligne CRLF. Export local après acquisition ; staging et publication
sans écrasement, destination existante refusée même en cas de course. Budget nul
refusé avant accès source. L’annulation est contrôlée pendant l’écriture et juste
avant publication après synchronisation ; le staging est abandonné via le mécanisme de nettoyage de tempfile.
Un rapport d’acquisition déjà annulée peut être exporté avec son statut explicite.
Un nouveau signal pendant l’export arrête l’écriture, code 130 ; **stdout peut être
tronqué et doit être rejeté par le consommateur**, même s’il contient un préfixe
JSON/CSV/TSV. Une sortie fichier locale abandonnée n’est pas publiée. Un appel OS
de sortie déjà bloqué reste soumis aux limites de l’annulation coopérative.

Code 0 : opération complète dans son périmètre explicite ; 2 : couverture/limite
incomplète, 130 : annulation, 1 : erreur fatale. Une vue de 50 000 entrées/8 Mio ne
couvre pas nécessairement le disque : lire `scope.exhaustive` et les omissions.
Une recherche ciblée définit un sous-périmètre, jamais une garantie de tous les
fichiers du volume. Les groupes sont des observations, pas un snapshot atomique.

SMB : fournir explicitement un chemin UNC/lecteur déjà connecté Windows ou un
répertoire CIFS déjà monté Linux. Les droits et l'identité sont ceux du système.
Aucun mot de passe demandé ou conservé par l'application, aucune découverte NAS.
Les capacités, allocations et identités offertes par le serveur peuvent manquer ;
les erreurs et résultats partiels restent visibles. Une opération réseau bloquée
ne garantit pas une annulation immédiate.
