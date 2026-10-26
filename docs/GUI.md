> Version de distribution : les références internes non livrées sont indiquées en texte. Les preuves et plans détaillés restent dans le dépôt de développement ; aucun historique privé n’est fourni.

# Interface de scan à deux panneaux

En v0.3, l’exécutable GUI est `bunny-and-size` (`bunny-and-size.exe` sous
Windows), la CLI `bunny-and-size-cli`, avec l’alias `sortsize`. Les sections
datées V0.1/V0.2 ci-dessous conservent les noms des candidats historiques.

Le prototype `spikes/slint` utilise maintenant le même scanner que la CLI. Il reste dans son workspace expérimental : aucun composant graphique n'est ajouté à la CLI. Exigences concernées : `SCAN-001`, `SCAN-014`, `SCAN-004`, `SCAN-009`, `SCAN-010`, `SCAN-015`, `SCAN-011`, `VIEW-001`, `VIEW-008`, `VIEW-009`.

## V0.2 — colonnes, unités et préférences (#18)

**Colonnes et affichage** ouvre un panneau utilisable au clavier. Les onze
colonnes sont cochées par défaut ; **Nom** reste toujours visible. Les autres
peuvent être masquées, sans réserver d'espace : taille logique, taille allouée,
propriétaire, part du dossier, fichiers récursifs, dossiers récursifs, type/état,
modification, accès et chemin complet. La largeur partagée en-têtes/cellules
s'adapte et le défilement horizontal se réajuste si le contenu rétrécit.

Masquer la colonne du tri actif **conserve ce tri** ; le titre du tableau le
rappelle en permanence. Réafficher la colonne pour changer son tri. Page,
sélection, navigation et données collectées restent conservées.

Le choix **Binaire** (défaut, KiB/MiB/GiB/TiB) ou **Décimal** (kB/MB/GB/TB)
s'applique immédiatement à l'arbre, au tableau logique/alloué, aux résumés,
à la Treemap, au Top 100 et aux volumes. Une décimale arrondie ; valeurs brutes,
proportions, dates UTC et tris inchangés. Les absences restent explicites et
`≥` reste une borne inférieure connue. Aucun nouveau scan ou accès de métadonnées,
aucune résolution Owner ou découverte de volumes n'est déclenché.

**Réinitialiser l’affichage** restaure toutes les colonnes et les unités binaires.
Ces seuls réglages sont sauvegardés localement :
`$XDG_CONFIG_HOME/sortsize/display-v1.conf` ou
`$HOME/.config/sortsize/display-v1.conf` sous Linux ;
`%APPDATA%\sortsize\display-v1.conf` sous Windows. Aucun chemin analysé ni
consentement réseau n'est mémorisé. Une erreur de sauvegarde est indiquée dans
le panneau ; l'affichage reste utilisable pour la session.

ADR-0029 [référence interne] ·
preuves Ubuntu, mesures et limites [référence interne] ·
Windows natif restant [référence interne].
Les mentions de colonnes/préférences « hors périmètre » dans les anciennes
tranches ci-dessous décrivent leurs jalons historiques.


## Sources et compilation de la vue Slint

Le markup est dans `spikes/slint/ui/main.slint`. Le `build.rs` du prototype
compile cette vue avec `slint-build =1.18.1`, après la génération existante des
ressources de version Windows. `src/lib.rs` importe les types et composants
exportés avec `slint::include_modules!()` ; les contrôleurs restent en Rust.
La CLI et ses contrats d’export restent dans le workspace racine indépendant.

```bash
cargo build --workspace --release --locked
cargo build --manifest-path spikes/slint/Cargo.toml --release --locked
```

Pour vérifier l’extraction des `@tr` existants avec `slint-tr-extractor` 1.18.1 :

```bash
slint-tr-extractor spikes/slint/ui/main.slint -o /tmp/sortsize-ui.pot
```

Cette extraction ne fournit aucun catalogue traduit ni changement de langue.

## Lancer et utiliser

```bash
cargo run --locked --manifest-path spikes/slint/Cargo.toml
```

Sous Ubuntu, la compilation requiert les fichiers de développement Fontconfig (`libfontconfig-dev`), pkg-config et un linker C. Le lanceur `scripts/run-slint-ubuntu.sh` exécute la même commande Cargo avec le lockfile.

La validation native Ubuntu du 14 octobre 2026 [référence interne] couvre les interactions dans une vraie fenêtre X11, avec une limite de saisie observée avec IBus et XTest. Si la saisie se dérègle, lancer explicitement `scripts/run-slint-ubuntu.sh --basic-input` : ce secours désactive la composition IBus pour ce processus seulement. L'affichage Unicode et le collage Unicode sont vérifiés dans ce mode. Un complément au clavier physique avec IBus valide la courte séquence de saisie accentuée et Retour arrière (`/tmp/été` → `/tmp/ét`) ; la composition IME et les autres entrées IBus restent à caractériser. Le lancement habituel conserve l'environnement de saisie.

Cliquer sur **Parcourir…** pour choisir un dossier local, ou saisir son chemin, puis cliquer sur **Analyser** (ou Entrée). Le choix seul ne lance aucune analyse ; annuler le dialogue conserve la source et l’inventaire affiché. Le chemin du dossier courant est proposé au démarrage. Le bouton **Annuler** demande l'arrêt entre les entrées du scan ; un appel système bloqué peut retarder cet arrêt. Une seule analyse peut être active. Fermer la fenêtre demande aussi l'annulation, sans attendre le worker sur le thread graphique.

### Propriétaires (V0.2 PR D)

Ouvrir **Propriétaires** pour afficher les deux options distinctes, désactivées
par défaut. **Collecter les propriétaires** s'applique au prochain scan et affiche
les UID Linux ou SID Windows dans la colonne **Propriétaire**. La collecte partage
le parcours des dates, cumuls alloués et Top 100 ; aucun deuxième scan.

**Résoudre les noms (NSS / AD : réseau possible)** autorise explicitement les
fournisseurs système, potentiellement distants. Les noms arrivent en arrière-plan
pour la page visible uniquement ; les identifiants connus restent affichés en cas
d'échec. Un refus de collecte indique « Accès refusé », une identité indisponible
« Indisponible », et une collecte non demandée « — ». Aucun propriétaire ou droit
de fichier n'est modifié. **Annuler** arrête aussi la résolution ; un appel système
déjà engagé peut continuer sans bloquer la fenêtre. Le panneau d'options peut être
refermé pour conserver la hauteur du tableau à 960 × 600.

Le clic sur **Propriétaire** trie en ascendant puis descendant. Avec résolution,
le tri utilise un instantané des noms disponibles sur la page à cet instant ; il
ne résout pas tous les comptes de l'inventaire. Les cellules se mettent à jour sans
réordonner automatiquement les lignes : cliquer à nouveau pour appliquer les noms
nouvellement disponibles. Dossiers d'abord, puis noms résolus, ID connus, refus,
indisponibles et non collectés ; égalités départagées par ID natif puis de nœud.
Les noms affichés sont comparés lexicalement et sensibles à la casse, sans
collation locale ; UID numériques, SID sur leurs octets. Les six tris historiques,
les pages de 2 000 lignes, le défilement horizontal/vertical et les identifiants de
sélection restent conservés. Un clic sur un autre tri invalide un tri Owner en cours.

La génération d'inventaire et la révision de page empêchent un ancien résultat de
nommer une ligne d'un nouveau scan. Les deux workers, 128 demandes, 256 entrées et
1 Mio de cache sont réutilisés, avec budget d'affichage séparé de 1 Mio par page.
Les labels d'affichage sont limités à 1 024 octets avec ellipse ; les UID/SID natifs
restent disponibles. Cette donnée d'affichage n'est jamais une autorisation.

Architecture et limites [référence interne] ·
Preuves Windows et recette Ubuntu [référence interne].

L’arbre de gauche affiche les dossiers et leurs tailles cumulées. Les boutons `+` / `−` déplient ou replient un dossier sans changer le dossier consulté. Cliquer sur son nom sélectionne le dossier et remplit le tableau de droite avec ses enfants directs. Cliquer sur un dossier du tableau le consulte ; **↑ Parent** revient au parent dans le même inventaire, sans relancer le scan. Les ancêtres et la page de l’arbre sont révélés pour la nouvelle sélection.

Les en-têtes **Nom**, **Taille** et **Alloué / Par chemin** changent le tri et inversent son sens au second clic. Les dossiers restent en tête ; les tailles inconnues restent en fin de leur groupe lors d’un tri numérique. Le tri des noms est lexical, sensible à la casse, sans collation locale pour ce jalon. L’arbre conserve son ordre par taille décroissante. Arbre et tableau disposent chacun de pages de 2 000 lignes ; une sélection ou un changement de tri revient à la première page du tableau.

Le premier clic **Alloué / Par chemin** trie par allocation décroissante, le
second par allocation croissante ; une flèche indique le sens. Les comparaisons
utilisent les `u64` Rust, jamais les chaînes arrondies. Les valeurs Exact et
LowerBound sont classées sur leur nombre connu ; `≥` est conservé et le panneau
Source locale indique que ce classement est provisoire. Inconnue, Non prise en
charge, Accès refusé et Non collectée restent en fin de chaque groupe
(dossiers avant fichiers), dans les deux sens. Les égalités sont départagées par
nom natif puis chemin natif et identifiant, indépendamment du libellé Unicode
avec remplacement.

Le bouton est désactivé si l'inventaire courant n'a pas d'allocation numérique,
notamment après une analyse sans collecte. La case de collecte configure le
**prochain** clic Analyser : la changer seule conserve le résultat courant,
son tri disponible et ses valeurs, sans scan supplémentaire. Les deux ordres
alloués sont préparés sur le worker ; changer le tri ne fait que sélectionner un
ordre d'identifiants et projeter au plus 2 000 lignes. Les limites de 50 000 nœuds
et 8 Mio de chemins sont conservées. Preuves V0.2 [référence interne].

Les noms longs sont abrégés à l’affichage, avec le libellé complet pour l’accessibilité. Le chemin du dossier consulté est visible dans un champ en lecture seule, sélectionnable et copiable. Cliquer sur un fichier ou un lien affiche son chemin complet dans le tableau, sans ouvrir le contenu. Les noms Unicode s’affichent et les caractères de contrôle sont échappés ; les libellés ne servent jamais à retrouver une cible d’opération.

La colonne **Part du dossier** et ses barres utilisent la taille logique connue du parent. Les proportions sur un parent partiel portent `≈` ; un total nul ou inconnu, ou une entrée ignorée, affiche `—`. Une proportion positive trop petite pour l’arrondi à une décimale affiche `< 0.1 %`. Le résumé du dossier affiche le nombre d’enfants retenus, avec les indications d’analyse partielle ou d’inventaire limité. Une liste vide distingue le périmètre vide des données inaccessibles ou omises.

La fenêtre affiche les comptages, la taille logique totale, la durée, les liens ignorés et les erreurs. Pendant l'analyse, elle affiche les derniers compteurs reçus ; l'arbre apparaît après le scan et sa préparation. Un dossier inaccessible déclenche un message d'erreur, et un résultat partiel ou annulé est identifié explicitement. `≥` signale une taille de dossier partielle. Les liens et fichiers spéciaux ignorés ont une taille `—`, jamais un zéro présenté comme mesuré.

Si l’annulation arrive avant la première entrée, le tableau indique explicitement qu’aucun inventaire n’est disponible à cause de l’annulation. Il ne présente pas ce résultat comme une analyse non démarrée ou un dossier vide.

## Limites et sécurité

- Lecture seule : aucune suppression, modification, ouverture de contenu, téléversement ou télémétrie.
- Taille logique par chemin : chaque hardlink contribue séparément ; aucune estimation d'espace alloué ou récupérable.
- Au plus 50 000 entrées et 8 Mio de chemins encodés natifs retenus pour l’arbre. Au premier plafond atteint, le scan et les sommes continuent ; un avertissement indique les entrées non affichées et le budget atteint. Les coûts du parcours et du rendu restent distincts ; aucun plafond absolu de RSS n’est garanti.
- Sélecteur natif Linux/Windows et saisie manuelle ; pas de réglage d’exclusions dans la GUI pour ce jalon. Les règles du scanner s'appliquent, notamment les exclusions Linux d'un scan de `/`.
- La [Treemap V0.2](#v02--treemap-interactive-9-view-002--view-008) complète le tableau. Aucun index SQLite ni export depuis la GUI ; l’export JSON reste disponible dans la CLI.
- Interface desktop : adaptation tactile et navigation accessible complète restent à valider.
- Android : bouton de scan désactivé jusqu'à l'intégration SAF/MediaStore.
  Les captures sur émulateur de #5 [référence interne]
  montrent les contrôles grisés, mais le bas de la fenêtre contenant le statut
  SAF/MediaStore est masqué ; adaptation/insets et tactile restent dans #21.
  Aucun accès aux données privées des autres applications n'est promis.

Le sélecteur **Dossiers analysés simultanément** propose 1, 2, 4 ou 8 avant le
lancement ; il est désactivé pendant l’analyse. Le défaut est 1. Le résultat indique
la capacité choisie ; le nombre de listes réellement actives dépend de l’arbre.
Les 50 000 entrées et 8 Mio de chemins de l’inventaire restent un budget unique.
Le pipeline concurrent [référence interne] ajoute une file partagée
bornée et un lot borné par travailleur ; il ne garantit pas un plafond absolu de
mémoire. Les mesures Ubuntu du produit [référence interne]
comparent les réglages dans la vraie boucle native, avec inventaire et navigation.
Le sélecteur de dossier, la progression et l'annulation sont exercés dans la
recette native Windows MSVC du 16 octobre [référence interne].

## Vérifications

```bash
cargo fmt --manifest-path spikes/slint/Cargo.toml --check
cargo clippy --locked --manifest-path spikes/slint/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path spikes/slint/Cargo.toml
cargo build --locked --manifest-path spikes/slint/Cargo.toml
```

Les tests utilisent un adaptateur logiciel Slint public, sans session graphique ni dépendance de test additionnelle à Slint. Ils vérifient les callbacks GUI, le scan réel d'une fixture, les tailles, la sélection entre panneaux, le retour au parent, les tris, les proportions, le dépliage/repliage, l'annulation demandée, la relance après une erreur et la pagination d'un dossier large. La CI les exécute nativement sur Linux et Windows.

Pour une vérification visuelle facultative, le test GUI peut écrire un rendu logiciel PPM de 1200 × 760 pixels si `SORTSIZE_GUI_SCREENSHOT` contient sa destination. `SORTSIZE_GUI_PREVIEW_ROOT` permet de choisir un petit dossier de référence ; sans cette variable, la fixture de test est utilisée. `SORTSIZE_GUI_COMPACT_SCREENSHOT` ajoute un rendu de 960 × 600 pixels lorsque le rendu principal est demandé. Aucun fichier n’est rendu par défaut.

Les preuves réellement obtenues figurent dans VALIDATION.md [référence interne]. Une capture logicielle et les tests des callbacks ne certifient pas tous les comportements de Winit, du clavier, du lecteur d'écran ou des permissions sur appareil réel.

La recette native Windows du 15 octobre 2026 [référence interne]
ajoute les preuves souris/clavier, navigation, tris, pages, erreurs NTFS,
annulation, redimensionnement en cours de scan, fermeture et relance. Les preuves
de #7 sont prêtes pour revue ; aucun statut d'accessibilité n'en est déduit. Le générateur
`scripts/gui_validation_windows.py` prépare des fixtures neuves sans modifier
les ACL ni exiger la création de symlinks.

## Direction de la prochaine interface

Le prototype valide le raccordement au moteur. L’organisation desktop visée est décrite dans DESKTOP_UX.md [référence interne] ; les deux panneaux synchronisés, le tableau des enfants, le retour au parent et les proportions sont implémentés dans le prototype. Les volumes, métadonnées enrichies et visualisations restent à développer. Les comptages par sous-dossier sont ajoutés dans la tranche V0.2 de l’Issue #11, décrite ci-dessous.


## Budget de rétention et mesures natives

Le prototype conserve au plus 50 000 nœuds et **8 Mio de chemins encodés natifs**, avec des pages de 2 000 lignes. Le premier plafond atteint arrête la rétention des nœuds, mais le scan continue ses comptages et tailles ; un inventaire limité n’est pas une analyse partielle. L’avertissement indique combien de nœuds sont omis et si le budget de chemins est atteint. Il peut donc y avoir moins de 50 000 nœuds visibles avec des chemins longs.

La tranche Windows [référence interne] complète les mesures Ubuntu : 144 essais avec oracle numérique GUI, annulation et compteurs Win32 réservés au banc. Le pic de boucle isolé de 234,999 ms et les buffers du parcours restent à traiter. Les variantes de budgets et d'itérateurs concernent les modes moteur ; la fenêtre conserve les paramètres du produit. Aucun résultat ne certifie une latence physique des entrées ni un plafond absolu de mémoire.

Le rapport de performance Ubuntu [référence interne] détaille les mesures, les objectifs et les limites du RSS. Le banc se compile explicitement avec `--features benchmarks --bin sortsize-measure` ; le lancement Cargo habituel reste l’application grâce à `default-run`. Aucun journal de mesure ni télémétrie n’est activé au lancement normal. L’API `scan_inventory_with_limits` permet de régler les limites pour les autres clients ; la GUI utilise les valeurs par défaut.

## Limite du parcours local

Le moteur conserve une liste ouverte et une pile limitée à 65 536 dossiers / 8 Mio de chemins natifs, distincte des budgets de la vue. Une saturation affiche le nombre de dossiers observés non parcourus et l’état partiel ; les descendants inconnus ne sont pas comptés et les tailles sont des bornes inférieures. Les rendus et mesures Ubuntu [référence interne] vérifient cet avertissement et l’annulation ; la revalidation Windows reste distincte.


## Sélecteur de dossier — #46, V0.1

`rfd` **0.17.2**, MIT, utilise le **XDG Desktop Portal** sous Linux et
`IFileDialog` sous Windows. Aucun toolkit GTK/Qt ni runtime async n’est ajouté
au binaire. Slint reste en **1.18.1** ; sa feature publique
`raw-window-handle-06` rattache le dialogue à la fenêtre.
Voir ADR-0011 [référence interne] et
preuves Ubuntu [référence interne] et
preuves Windows MSVC [référence interne].

Le dialogue attend sur un thread dédié ; le timer graphique reçoit son résultat
sans attente de verrou ni `join`. **Parcourir…** est désactivé pendant un scan.
Pendant le dialogue, chemin, parallélisme, Parcourir et Analyser sont désactivés ;
les gardes Rust empêchent aussi un second dialogue ou scan déclenché par callback.
L’ancien inventaire reste visible jusqu’au prochain clic sur Analyser.

Le `PathBuf` reçu du dialogue est conservé séparément de son libellé Slint UTF-8.
`Task::start` reçoit ce chemin natif. Toute édition utilisateur (y compris un
retour au texte initial) passe en saisie manuelle ; une mise à jour programmatique
avec un texte différent ne réutilise pas un ancien chemin natif. Le dossier courant
initial est également conservé sans conversion avec perte vers le moteur.

### Prérequis et limites Linux

- Session graphique X11 et bus D-Bus de session ; `libdbus-1.so.3`,
  `xdg-desktop-portal` et un backend fournissant `FileChooser`
  (`xdg-desktop-portal-gtk`, `xdg-desktop-portal-kde`, selon le bureau).
  La sélection Wayland native n’est pas validée : le prototype Slint utilise X11.
- `rfd` peut essayer **Zenity** si le portail est absent ou échoue. Ce secours
  dépend de l’exécutable système, n’est pas certifié ici et ne garantit pas les
  noms non UTF-8 ni les espaces en début/fin de nom.
- `rfd` renvoie `None` pour une annulation et pour certaines erreurs de service.
  Dans les deux cas, Bunny and Size conserve la source et l’inventaire ; la saisie
  manuelle reste disponible. Impossible de distinguer ces causes avec cette API.
- Le portail doit lui-même préserver les noms natifs. **GTK 1.15.1** conserve
  les octets non UTF-8 dans la recette. **KDE 5.27.11** renvoie un caractère de
  remplacement pour le nom `native-<FF>` : l’analyse de cette sélection échoue
  avec « chemin inexistant ». Bunny and Size ne tente aucune reconstruction ambiguë.
  Le choix d’un backend GTK capable de conserver ces octets relève de la
  configuration du bureau ; aucune préférence système n’est changée par Bunny and Size.

Les tests contrôlent les chemins accentués/Unicode, espaces, chemins longs,
non UTF-8 Unix, UTF-16 non apparié Windows (test natif du contrôleur réussi),
annulation, édition manuelle, gardes d’activité, échec du thread et accès refusé.
La recette Windows MSVC du 16 octobre valide le vrai dialogue, Unicode/espaces,
annulation, navigation, progression et arrêt du scan. Le chemin long est choisi
par navigation dans le Shell et transmis ici sous forme d'alias NTFS 8.3 ; son
analyse réussit. Un essai de saisie intégrale du chemin long dans le dialogue
échoue, sans cause isolée. Les variantes sans alias 8.3, UNC et les noms UTF-16
non appariés choisis dans le Shell ne sont pas certifiés par cette recette.
Android est hors V0.1, sans intégration `rfd`.

## Candidat portable Windows de #47

Sous Windows, le binaire graphique utilise maintenant le sous-système Windows
GUI ; son lancement par double-clic ne crée pas de console. La CLI reste un
exécutable console distinct. Le packaging renomme seulement les fichiers en
`SortSize.exe` et `sortsize-cli.exe`, sans changer les crates ou fonctionnalités.
Procédure et runtime [référence interne],
preuves du ZIP extrait [référence interne]. Le code source
et les notices candidates accompagnent le ZIP dans une archive séparée.
L'essai Windows propre et la conformité finale restent à valider.

## V0.2 — comptages par dossier (#11, SCAN-004)

La sélection présente les fichiers et sous-dossiers **directs** (enfants immédiats)
et **récursifs** (tous les descendants, enfants directs inclus). Le dossier lui-même
est exclu : à la racine, les sous-dossiers récursifs valent le total CLI moins un.
Le tableau ajoute les colonnes « Fichiers récursifs » et « Dossiers récursifs ».
Un dossier vide analysé complètement affiche zéro ; fichiers, liens et entrées
spéciales affichent `—` dans ces colonnes, auxquelles le comptage ne s’applique pas.
Les chemins de hardlinks sont comptés séparément ; exclusions et liens ignorés ne
contribuent pas aux nombres de fichiers/dossiers.

Les quatre compteurs portent `≥` lorsque le dossier est incomplet : erreur,
limite de profondeur/ressources ou annulation. Cette règle est conservatrice :
une erreur dans un descendant rend également les compteurs directs du parent
partiels. Les enfants inconnus ne deviennent pas des zéros exacts. Un inventaire
tronqué seul ne rend pas les compteurs partiels : les descendants non retenus
continuent de contribuer sans second parcours. L’absence d’inventaire après
annulation conserve son message explicite.

Validation Ubuntu ciblée [référence interne] : tests du moteur,
modèles/callbacks Slint et rendus logiciels à 960×600 et 1200×760. La validation
native Windows V0.2 reste à effectuer ; les preuves V0.1 ne la remplacent pas.

## V0.2 — Treemap interactive (#9, VIEW-002 / VIEW-008)

Le panneau droit ajoute une Treemap Squarified des **enfants directs** du dossier
consulté, au-dessus du tableau existant. Les surfaces représentent les octets
logiques observés, jamais la taille allouée ou l’espace récupérable. Clic sur un
dossier : zoom avec synchronisation de l’arbre, du tableau et du chemin ;
« ↑ Parent » ou Retour arrière depuis la Treemap : remonter. Clic sur un fichier :
inspection du chemin en lecture seule, surbrillance de sa surface et de sa ligne,
avec révélation de la page et défilement du tableau. Aucun fichier n’est ouvert.

Le bouton « Treemap » place directement le focus dans le graphique. Les flèches
parcourent les surfaces dans l’ordre décroissant des tailles ; Entrée/Espace
active la surface. Le contour jaune et le libellé complet sous la Treemap
identifient le curseur, même lorsque le rectangle ne permet pas d’inscrire son
nom. Les dossiers portent un triangle, les surfaces ont des libellés accessibles
nom/taille/action ; les couleurs ne sont pas le seul repère. Le nom et la taille
figurent dans les surfaces suffisamment grandes. À 960×600, la hauteur du
graphique diminue pour conserver le tableau et son défilement. Les tris nom/taille
et les pages de 2 000 lignes restent disponibles indépendamment du graphique.

Au maximum **256 surfaces** sont retenues par dossier. Les entrées sous **0,1 %**
des octets représentables et celles dépassant le budget de surfaces sont
regroupées dans « Autres ». Ce groupe affiche sa taille et son effectif ; son
activation renvoie vers le tableau, sans lui inventer un chemin ou un sous-dossier.
Les fichiers de taille nulle et les dossiers vides n’ont pas de surface ; leurs
lignes restent consultables. Les valeurs inconnues / non mesurées et les bornes
inférieures nulles sont signalées séparément, sans aire minimale artificielle.
Un rectangle positif plus petit qu’un pixel peut être invisible ; le clavier et
le tableau restent utilisables.

Si l’inventaire est tronqué, les totaux de scan sont conservés. La différence
entre la taille observée du parent et les tailles connues de ses enfants retenus
forme une surface distincte « Omis de l’inventaire », sans navigation individuelle.
Cette différence n’est utilisée que si aucun enfant retenu n’a une taille inconnue ;
sinon la Treemap se limite au sous-ensemble mesuré et le signale. Un écart
incohérent suspend le graphique. Aucune proportion n’est une estimation des
octets non observés : erreurs, annulation et limites de parcours sont indiquées
comme couverture incomplète, les tailles concernées portent `≥`.

La préparation des groupes se fait une fois sur le worker à partir de l’inventaire
Rust. Le redimensionnement recalcule seulement la géométrie bornée ; ni le zoom,
ni le tri, ni l’inspection ne relisent le système de fichiers. Les chemins natifs
restent dans la projection ; les libellés Unicode avec perte sont réservés à
l’affichage et les identifiants numériques restent distincts. Plafonds de
50 000 nœuds / 8 Mio de chemins et budgets du parcours inchangés ; ils ne
constituent pas une limite absolue de RSS. Aucune dépendance ajoutée.

Algorithme et compromis [référence interne],
preuves Ubuntu, rendus et performances [référence interne],
tranche Windows à exécuter [référence interne]. Les interactions
Ubuntu sont automatisées sur X11 ; clavier physique, lecteur d’écran et recette
Windows native V0.2 restent à vérifier. Android est hors de cette tranche.

## V0.2 — Top 100 global (#57, VIEW-004)

Le bouton **Top 100** affiche le classement global des fichiers de la racine,
indépendant du dossier exploré et des plafonds de l’arbre (50 000 entrées,
8 Mio de chemins). Il présente rang, nom, taille logique et chemin. Les noms et
chemins abrégés dans les colonnes restent consultables dans le champ de chemin
complet en lecture seule, sélectionnable et défilable horizontalement.
Les unités non Unicode sont échappées sans perte (`\xFF` pour un octet Unix
invalide) ; l’identité conservée dans Rust reste un `PathBuf` natif.

Cliquer une ligne ou utiliser **↑ / ↓**, **Début / Fin** après avoir ouvert la
vue. La sélection est surlignée et défile jusque dans le rang 100. **Entrée** ou
**Révéler dans le tableau** consulte le parent et révèle la page/ligne si le
fichier appartient aux nœuds retenus. La Treemap suit ce dossier et l’inspection.
Sinon, le bouton est désactivé et le chemin reste affiché avec la mention
« absent de l’inventaire retenu » ; aucune entrée n’est ajoutée dans l’arbre.
**Arbre / Treemap** revient à l’exploration. Un nouveau scan réinitialise le Top.

Les égalités sont départagées par chemin natif, indépendamment de l’ordre de
réception. Hardlinks comptés par chemin ; tailles logiques uniquement, sans
estimation d’espace alloué ou récupérable. Une analyse partielle ou annulée
classe uniquement les fichiers observés. Le classement peut aussi être limité
si un chemin dépasse 64 Kio natifs ; le nombre d’omissions est alors affiché.
Les chemins de l’inventaire et ceux du Top ont des budgets indépendants.

Recette Ubuntu, 960×600 et mesures [référence interne].
Clavier/souris injectés sous X11/Xvfb ; lecteur d’écran, IME, bureau physique et
Windows MSVC non certifiés par cette tranche. Aucune nouvelle dépendance.


## V0.2 — chemin complet dans le tableau (#18, première tranche)

Exigences `VIEW-009`, `SCAN-011`, `SCAN-014` et `SCAN-015`. Chaque entrée de la
page expose une colonne **Chemin complet**, issue du `PathBuf` déjà retenu dans
l’inventaire. Le nom, la taille logique, la proportion, les deux comptages et
le type/état restent présents. Le tableau conserve ses tris nom/taille et ses
pages de 2 000 lignes ; Treemap et Top 100 partagent toujours les identifiants
de nœuds de l’inventaire.

La barre horizontale déplace ensemble les en-têtes et les lignes, y compris à
960×600. Le défilement vertical reste indépendant. Cliquer un chemin, fichier
ou dossier, l’affiche dans le champ en lecture seule sous le tableau sans
ouvrir le dossier ; son nom garde l’action d’ouverture existante. Un libellé
long peut être abrégé visuellement dans sa cellule ; sélectionner le chemin
permet de le lire/copier en entier, avec Début/Fin et défilement du champ. La
chaîne du modèle et le chemin natif ne sont jamais tronqués.

Unicode reste lisible. Les chemins non UTF-8 et les caractères de contrôle
utilisent les échappements natifs de Rust (comme le Top 100), sans confondre
les octets FE/FF ni une nouvelle ligne avec les caractères littéraux `\\n`.
Les préfixes natifs Windows restent dans ce libellé. Aucun chemin n’est
reconstruit depuis le texte : les callbacks transmettent uniquement le nœud.

`InventoryEntry.path` est déplacé vers `Tree::Node.path`, puis `Tree::details()`
formate uniquement la page visible dans `DetailRow.path`. Le worker classe
une fois chaque chemin (UTF-8 sans caractère de contrôle ou échappement requis),
avec un simple booléen par nœud. La page ordinaire copie directement le texte
natif vers Slint ; elle ne revérifie pas tous les caractères à chaque tri. Aucun second scan,
appel de métadonnées par ligne ni cache permanent de tous les libellés complets.
Les plafonds restent 50 000 nœuds et 8 Mio de chemins natifs ; les chaînes Slint
de la page et le rendu sont des coûts supplémentaires distincts de ces budgets.
Les états inconnus, incomplets et les avertissements d’inventaire sont conservés.
Le worker, la progression coalescée et le poll sans attente sont inchangés.

Preuves Ubuntu et coûts mesurés [référence interne],
recette Windows MSVC distincte à exécuter [référence interne].
Windows n’est pas revalidé dans cette tranche ; Android reste hors périmètre.
Propriétaire, dates, taille allouée, autres tris et préférences persistantes
restent à développer dans des tranches distinctes de #18/#17/#14/#16.


## V0.2 — dates de modification et de dernier accès (#17/#18)

La GUI locale demande désormais `scan_inventory_with_dates` dans son worker.
L’inventaire et la table de dates sont transmis ensemble à Tree, avec vérification
de longueur ; tri, pagination, navigation et révélation Top 100 utilisent les
identifiants existants. Les chemins restent natifs, les libellés ne servent pas
à retrouver un fichier. Aucun second scan ni requête de métadonnées à l’affichage.

Le tableau ajoute **Dernière modification** et **Dernier accès**, dans le même
défilement horizontal que les colonnes existantes et **Chemin complet**, en-têtes
compris. Format : `1960-01-01 00:00:00.123456700 UTC` ; UTC est explicite, les
instants avant 1970 sont conservés, la fraction reflète la valeur native acquise.
Une donnée absente affiche Inconnue, Non prise en charge, Accès refusé ou Non
collectée ; jamais une date de secours. Le dernier accès dépend des politiques
du système de fichiers et peut ne pas être actualisé après chaque lecture.

Seules les deux dates des 2 000 lignes maximum de la page sont formatées.
La table optionnelle de #61 est conservée sans duplication dans chaque nœud :
48 octets/enregistrement sur l’Ubuntu x86_64 mesuré, au plus 2 400 000 octets de
table avec les limites par défaut. Les chaînes de la page sont un coût distinct.
Les 50 000 nœuds / 8 Mio de chemins natifs restent inchangés. Tris par dates,
propriétaire, permissions, taille allouée et préférences de colonnes hors périmètre.

ADR-0019 [référence interne],
preuves Ubuntu, captures et mesures [référence interne],
Windows MSVC/DPI à valider [référence interne].
À 960×600 la vue avec Treemap reste dense ; agrandir la fenêtre permet de
voir davantage de lignes simultanément. L’indication sur la politique d’accès
figure dans l’en-tête pour préserver la hauteur du tableau. La branche/PR dépend de #59/#60/#61 et conserve
le merge local de #59 ; aucune intégration dans main/develop n’est faite ici.

## Allocations par chemin — #14, tranche C

Cocher **Collecter les tailles allouées**, puis lancer **Analyser**. La case est
désactivée par défaut et verrouillée pendant l'analyse. La changer ne lance
aucune acquisition et ne réinterprète pas un résultat déjà affiché. Les dates
restent collectées dans le même parcours.

Le tableau place **Alloué / Par chemin** après **Taille** : allocation observée
du fichier ou cumul des fichiers du dossier (omissions de l'inventaire incluses).
Le panneau source précise « hardlinks répétés ». Ce mode ne déduit aucun
gain et ne mesure pas de blocs uniques. Les états absents restent textuels ;
seule une borne numérique fournie par le moteur porte ≥.

Les libellés d'allocation sont préparés sur le worker ; les pages réutilisent
les références indexées par nœud. Les tris historiques restent Nom/Taille,
la Treemap et le Top 100 restent en taille logique. Le viewport commun des
en-têtes/lignes est conservé, avec 144 px pour Alloué, un contenu minimal de
1 720 px et une fenêtre minimale inchangée de **960×600**. La vue reste dense
à cette taille. Navigation, pages de 2 000, sélection et chemins natifs gardés.

Captures et mesures Ubuntu [référence interne],
recette Windows non exécutée [référence interne].

## Placement initial Windows et haute DPI

La fenêtre Windows conserve sa taille préférée lorsqu'elle tient à l'écran et
réduit son démarrage à la zone de travail disponible, cadre natif compris.
Le minimum reste 960×600 pixels logiques. Un écran dont l'espace logique est
plus petit ne bénéficie pas d'une garantie d'affichage intégral. Le placement
est appliqué après disponibilité du HWND depuis le poll initial borné.
Décision [référence interne] et
preuves Windows natives 100/125/150/200 % [référence interne].

## V0.2 — tris par modification et dernier accès

Les en-têtes **Dernière modification** et **Dernier accès (UTC)** sont des
boutons dans le viewport horizontal existant. Le premier clic sur une colonne
classe du plus récent au plus ancien (↓), le suivant du plus ancien au plus
récent (↑). Passer à l'autre colonne démarre avec les plus récents. Le libellé
accessible décrit la colonne et l'action ; celui du dernier accès rappelle que
sa valeur dépend de la politique du système de fichiers.

Dossiers avant fichiers, dates connues avant les quatre motifs d'indisponibilité
dans les deux sens. Les égalités utilisent le nom natif puis l'identifiant de
nœud, sans collation locale. Les textes UTC affichés ne déterminent jamais
l'ordre. Le clic revient à la première page de 2 000 lignes et réinitialise le
scroll vertical comme les tris historiques ; le dossier courant et le scroll
horizontal sont conservés. Le tri suit la navigation et la révélation Top 100.
Un clic temporel annule la demande de tri Owner en cours ; ses résultats tardifs
ne peuvent rétablir un ancien classement.

| Indice | Tri |
|---|---|
| 0 / 1 | Taille décroissante / croissante |
| 2 / 3 | Nom croissant / décroissant |
| 4 / 5 | Alloué décroissant / croissant |
| 6 / 7 | Propriétaire croissant / décroissant |
| 8 / 9 | Modification récente / ancienne |
| 10 / 11 | Dernier accès récent / ancien |

Aucun scan supplémentaire à la demande de tri. Les quatre combinaisons de
collecte sont couvertes ; le dernier accès conserve ses limites relatime/noatime
ou de mise à jour différée sous Windows. Validation Ubuntu, performances et
recette Windows [référence interne]. Les sections précédentes
sur les fonctionnalités hors périmètre décrivent leurs jalons historiques.

## V0.2 — sources de stockage (#12, SCAN-012 / VIEW-009)

En bas de la colonne gauche, **SOURCES DE STOCKAGE** affiche nom/point préféré,
capacité totale, libre accessible au compte, état/type et barre d'occupation
(total moins libre total, sans compter les blocs libres réservés comme utilisés).
Capacités non additionnables : les alias sont regroupés et les pools peuvent
encore partager des blocs. Aucune alerte de remplissage automatique. Les inconnus
sont textuels ; total nul ou inconnu n'a pas de pourcentage inventé.

Un clic prépare uniquement la prochaine source d'analyse via le chemin natif.
L'inventaire courant reste affiché ; cliquer **Analyser** pour lancer le scan.
Le choix est désactivé pendant scan/dialogue. **Parcourir…** et la saisie restent
disponibles pour les dossiers et autres alias. Un volume disparu depuis la mesure
peut échouer au prochain scan explicite, sans élévation de droits.

**Actualiser** relance une mesure locale prudente. **Interroger réseau / type
inconnu** autorise explicitement les probes potentiellement réseau, normalement
non interrogés au démarrage ; Windows ajoute le chemin UNC saisi à ce lot. Aucun
partage n'est recherché automatiquement. Une opération native peut durer sans
timeout garanti ; une seule est active et la fermeture reste indépendante.

La liste défile sur une hauteur fixe (112 px compacte / 164 px normale), avec
plafond d'affichage de 256 sources, états chargement/vide/erreur/indisponible et
sélection. Les douze tris, pages de 2 000, Top 100, Treemap et Owner sont conservés.
Politique Linux/Windows [référence interne],
validation et limites par plateforme [référence interne].

## V0.3 — préparation i18n #72, tranche A

L'interface actuelle reste française. Le stockage peut conserver Auto/en/fr/de/es/pt
avec les unités et colonnes, mais aucun sélecteur linguistique n'est encore exposé.
Les cinq traductions complètes, les formats localisés et le rafraîchissement des
messages Rust appartiennent aux tranches B/C. Le pilote de test isolé et la
migration [référence interne] ne valent pas livraison
i18n v0.3 ; #94 et #98 restent des réserves Windows séparées.

## V0.3 — #100, B1 anglais/français

La GUI utilise désormais des sources anglaises avec un catalogue français
embarqué complet. L'exécutable habituel reste français, indépendamment de la
préférence `language` préparée en A ; aucun sélecteur provisoire. Le binaire
`sortsize-i18n-harness` (feature `benchmarks`) exerce EN/FR avec le vrai contrôleur
dans une configuration de laboratoire isolée. Les noms/chemins/identités et les
erreurs OS restent natifs. Dates UTC et conventions de nombres inchangées.
Conception, périmètre et exceptions [référence interne] ;
validation et réserves [référence interne].

## V0.3 — langue et formats régionaux (#102, B2)

Dans **Colonnes et affichage**, choisir **Automatique (système)**, **English** ou
**Français**. La préférence se charge avant le premier affichage et se sauvegarde
avec le schéma v2 existant. La locale système détectée et la langue active sont
indiquées séparément ; Auto utilise la locale détectée au démarrage. Un catalogue
C demandé mais absent revient à l’anglais et son nom est signalé, sans perte du
choix enregistré. Ces langues supplémentaires ne sont pas proposées en B2.

La bascule actualise immédiatement les textes Slint et Rust, les fenêtres et
notices ouvertes, sans scan ni demande Owner/volumes. Sélection, pages, scroll,
tri, unités et colonnes sont conservés. Français : espace fine insécable et
virgule décimale ; anglais : virgule de groupement et point décimal. Les dates
UTC portent un mois alphabétique localisé, avec la même convention anglaise
non ambiguë pour en_US/en_GB. Les timestamps et tris restent natifs.

Conception et politique régionale [référence interne] ;
validation native et limites [référence interne].
Les paragraphes B1 ci-dessus décrivent leur jalon historique ; B2 remplace son
français fixé au démarrage. C, #94, #98 et #19 restent distinctes.

## V0.3 — cinq langues, tranche C (#114)

Dans **Colonnes et affichage**, le sélecteur propose Auto (système), English,
Français, Deutsch, Español et Português (Portugal). La préférence reste distincte
de la langue effective. Les anciens tokens v2 `de/es/pt` deviennent actifs sans
migration ; `pt_BR` et `pt-BR` utilisent le portugais européen. Locale inconnue
ou catalogue absent : anglais, préférence conservée. Auto reprend l’instantané
système du démarrage, également après un changement explicite de langue.

Les textes Slint et Rust des fenêtres existantes changent ensemble, y compris
les modèles, notices, erreurs et résultats tardifs, sans nouveau scan ni demande
volumes/Owner. La GPL intégrale et les données natives restent inchangées. Les
formats canoniques et limites des variantes sont détaillés dans le
design C [référence interne] ; UTC et précision sont conservés.
Les dialogues de fichiers natifs déjà ouverts restent sous contrôle de l’OS.

Le pipeline courant se vérifie avec `scripts/audit_i18n.py`,
`compile_rust_translations.py --check` et `extract_slint_translations.py --check`
(extracteur 1.18.1). Ces descriptions remplacent les limitations « C à faire »
des étapes historiques ; le rapport C [référence interne]
distingue tests automatisés, surfaces Winit et interactions natives réelles.

## Aperçu pendant l'analyse — V0.3 / #92

L'arbre, le tableau et la Treemap se remplissent pendant le parcours. Les tailles
et les comptes des dossiers portent `≥` : ce sont les observations disponibles,
pas une estimation de la taille finale. La sélection d'un dossier découvert ne
stoppe pas l'analyse. Le résultat final conserve la navigation en cours.
Les mises à jour sont regroupées (500 ms pour la projection, 100 ms pour les
compteurs) et l'inventaire conserve ses plafonds de 50 000 entrées / 8 Mio de
chemins. Les dates, allocations, propriétaires et Top 100 sont disponibles dans
la projection finale. Une annulation laisse des bornes inférieures ; un échec
fatal retire l'aperçu. Les détails de conception et de coût sont dans
l'ADR-0034 [référence interne] et le
journal privé de validation [référence interne].

## Dossiers omis de l'inventaire — V0.3 / #93

Après le scan initial, sélectionner un dossier insuffisamment détaillé dans
l'arbre, le tableau ou la Treemap ouvre une lecture complémentaire de ce dossier.
Le bouton « Lire le contenu du dossier » permet aussi de relire ou réessayer.
L'interface affiche les enfants progressivement, avec `≥` tant que le parcours
continue. Le pied « Analyse initiale » conserve le résultat original ; le panneau
affiche les agrégats fraîchement observés, la couverture et le total initial connu.
Un dossier entièrement omis peut avoir un total initial « Indisponible ».

Chaque fenêtre retient au plus4 000 enfants/1 Mio de chemins, présentés par pages
de2000. À sa fin, Suivante lit la fenêtre suivante ; Précédente retrouve d'abord
le cache. Quatre fenêtres inactives sont cachées pendant600 s après leur lecture.
Le tri porte sur la fenêtre, pas sur tous les enfants du dossier. Une fenêtre
absente du cache parcourt récursivement la cible pour ses agrégats ; elle ne relance
pas le scan de la source entière, sauf si la cible est elle-même cette racine.

Les totaux et le Top100 initiaux ne sont jamais additionnés aux lectures fraîches.
Une modification des fichiers peut expliquer des valeurs différentes et déplacer
les offsets entre deux fenêtres. Relire invalide le cache de la cible. Parent et
Top100 permettent de revenir au contexte initial ; changer de source vide le cache.
Erreurs et annulation restent explicites, avec réessai possible. Les chemins sont
natifs et tous les symlinks/reparse de la chaîne ciblée sont refusés ; la lecture
reste soumise aux permissions et aux courses du système de fichiers. Voir les
preuves et limitations [référence interne].

Le passage entre fenêtres complémentaires est aussi possible pendant le calcul
récursif, dès que tous les enfants retenus de la fenêtre courante sont affichés
et qu'un enfant supplémentaire a été observé. Les valeurs restent provisoires ;
changer de fenêtre annule ce calcul et lit la nouvelle cible. Une fenêtre quittée
avant sa finalisation n'est pas mise en cache. Attendre la fin fournit les agrégats
frais complets, potentiellement coûteux sur un très grand dossier Windows.
