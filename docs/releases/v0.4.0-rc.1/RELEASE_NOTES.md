# Bunny and Size v0.4.0-rc.1 — prerelease d’évaluation

Cette prerelease contient les fonctions V0.4 décrites ci-dessous. Windows est
non signé et l’éditeur non vérifié. Les réserves D1 et les limites de qualification
restent applicables.

## Fonctionnalités V0.4

- Recherche et filtres sur l'inventaire retenu, pagination, tri et navigation
  vers le fichier/dossier ; le filtre ne relance pas une analyse du disque.
- Statistiques : nombres de fichiers, tailles logiques connues, extensions et
  classes de taille, tables et barres. Les métriques inconnues restent explicites.
- Doublons exacts à la demande : présélection par taille, SHA-256 et comparaison
  octet par octet. Lecture seule, sans suppression ni déduplication automatique.
- Accès aux partages SMB déjà autorisés via UNC/lecteurs connectés Windows et
  montages CIFS Linux ; aucun connecteur constructeur ou stockage de secrets.
- Sept langues : français, anglais, allemand, espagnol, portugais, italien,
  néerlandais, avec changement à chaud et préférences conservées.
- Exports d'analyse JSON/CSV/TSV et couverture, erreurs, limites et annulation
  explicites. Les anciens exports de scan restent disponibles.

L'inventaire retenu est borné : ces vues ne garantissent pas un index exhaustif
du disque. Les tailles logiques, allouées et potentiellement récupérables ne
sont pas assimilées. Les fonctions locales restent hors ligne, sans compte ni
télémétrie par défaut. Aucune nouvelle fonction n'est ajoutée par cette clôture.

## Qualification et limites

La RC.1 a été exercée le 26 octobre sur Windows 11 physique, console locale,
écran 2560 × 1440 à 125 %. Scan/annulation/reprise, recherche/pagination,
statistiques/doublons, Treemap synchronisée, exports, sept langues, Alt+Tab,
réduction/restauration et agrandissement/restauration sont étayés dans les
scénarios testés. Les 1 703 fichiers de preuve ont été revérifiés le 27 octobre.
Un défaut de calcul des hashes de l'outil d'observation a été corrigé séparément,
sans modification des exécutables ; l'échec historique reste consigné.

**D1 reste UNRESOLVED** : le pied d'interface peut garder les pixels de la langue
précédente après une bascule. Il n'a pas été reproduit dans les scénarios RC.1
exécutés ; aucune correction ni absence générale de défaut n'est démontrée.

Restent non qualifiés sur ce PC : redimensionnement libre par bord, annonces
d'un lecteur d'écran et suivi du focus UIA, autres DPI/multi-écran, SMB sans
partage de test autorisé. Les preuves Linux/VM précédentes conservent leur
portée ; elles ne remplacent pas ces essais physiques. Android, Wayland natif,
politiques d'entreprise et reproductibilité intermachine gardent leurs réserves.

## Windows non signé — éditeur non vérifié

SignPath Foundation a refusé la candidature ; aucune signature ou approbation
SignPath n'est revendiquée. Le paquet Windows est explicitement
non signé (`Unsigned public release — publisher unverified`), avec GUI
`bunny-and-size.exe`, CLI `bunny-and-size-cli.exe` et alias `sortsize.exe`.

SmartScreen peut avertir ou bloquer ; certains postes administrés peuvent
interdire le lancement. Le lancement local sans avertissement observé lors
de la qualification ne valide pas la réputation d'un téléchargement public :
les EXE locaux n'avaient pas de Mark of the Web. Le vrai parcours de téléchargement
public RC.1 reste NOT_RUN et doit être observé séparément après autorisation.
Conserver Defender, SmartScreen et les politiques de sécurité actifs.

Les SHA-256 vérifient l'intégrité ; ils ne remplacent pas une signature numérique.
Les fichiers SHA256SUMS et PROVENANCE.json accompagnent les archives exécutables
et les sources correspondantes. Le SHA du ZIP privé de qualification
ne doit pas être présenté comme le SHA du ZIP public.

## Sources et licences

Code GPL-3.0-only, textes et avis des tiers fournis ; adaptations graphiques
avec attributions CC BY-SA 4.0 et conditions précisées dans BRANDING.md/CREDITS.md.
Les sources de compilation, lockfiles et composants vendoriés correspondants
sont fournis dans l’archive source correspondante. Aucun historique privé, preuve utilisateur
ou original graphique réservé ne fait partie de l'export public approuvé.

Les archives portent le mode `public-unsigned` et la destination prerelease publique.
La validation technique du packaging est distincte de la décision de diffusion :
une autorisation propriétaire externe lie les sept noms, tailles et SHA-256 finaux.
Aucune autorisation de publication n’est contenue dans les archives.
