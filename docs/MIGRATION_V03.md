# SortSize devient Bunny and Size — v0.3

État : préparation privée de `0.3.0-dev`, sans release ni renommage GitHub effectué.
Le nom **Bunny and Size** est une marque textuelle non traduite ; slug `bunny-and-size`.

| Surface | v0.2 | v0.3 | Compatibilité / décision |
|---|---|---|---|
| Fenêtres, cinq langues | SortSize | Bunny and Size | Textes EN/FR/DE/ES/PT conservés |
| GUI Windows / Linux | SortSize.exe / sortsize-gui | bunny-and-size.exe / bunny-and-size | Nouveau nom du binaire compilé |
| CLI | sortsize, sortsize-cli | bunny-and-size-cli, sortsize | Deux points d'entrée vers une bibliothèque unique ; mêmes options et exports |
| Crates et modules | sortsize-core, sortsize-owner-resolution, sortsize-cli, sortsize-slint-spike | Identiques | Pas de bénéfice utilisateur à renommer les APIs moteur |
| Catalogue gettext | sortsize-slint-spike | Identique | Domaine interne stable, textes migrés |
| Préférences Linux | `$XDG_CONFIG_HOME/sortsize/display-v1.conf`, sinon `$HOME/.config/sortsize/display-v1.conf` | Identique | Aucun déplacement ni copie au démarrage |
| Préférences Windows | `%APPDATA%\sortsize\display-v1.conf` | Identique | Profil utilisateur, jamais Program Files |
| Android expérimental | org.sortsize.spike | Identique | Identifiant d'installation conservé ; aucune validation appareil ajoutée |
| Bancs et variables SORTSIZE_* | Noms techniques existants | Identiques | Recettes historiques reproductibles |
| Archives nouvelles | SortSize-vVERSION-* | Bunny-and-Size-vVERSION-* | Les anciennes releases restent intactes |
| Dépôts et Project | SortSize / SortSize-OSS / Bunny and Size – Development | Renommage différé | Voir le plan GitHub, après intégration autorisée |

## Préférences : migration par continuité

La migration d'identité garde volontairement le magasin historique unique.
Elle est idempotente : démarrer/redémarrer n'écrit rien et ne déplace rien.
Les configurations texte v1 (unités/colonnes) et v2 (ajout de la langue) restent
lisibles ; une modification explicite s'enregistre atomiquement en v2 avec le
worker existant. GB/GiB, dix colonnes optionnelles et langue automatique ou
EN/FR/DE/ES/PT sont préservés. Le nom de colonne reste imposé visible.
Fichier limité à 16 Kio, erreurs de lecture/version future signalées sans
réécriture au démarrage ; sauvegarde atomique et dernière modification vidée
à la fermeture. Aucun réglage global de l'utilisateur n'est modifié par les tests.

La v0.2 ne persistait ni le chemin analysé, ni les options allocation/Owner,
ni le nombre de workers : ces paramètres restent propres à la session.
On ne prétend donc pas migrer des paramètres inexistants dans l'ancien format.
Une ancienne v0.2 ne connaissant que v1 peut refuser v2 ; le partage du chemin
ne promet pas un retour arrière de schéma. Sauvegarder le fichier avant downgrade.

## Références historiques

Les ADR, preuves, captures, tags, releases v0.1/v0.2, cahier des charges initial
et le script figé `prepare_rc1_public.py` gardent leurs noms et leurs empreintes.
Les URLs existantes restent valides jusqu'au renommage. Les guides actifs CLI/GUI,
les ressources courantes et les recettes de distribution adoptent le nouveau nom.
Les chemins de laboratoire et noms des corpus ne changent pas.

## Distribution et dépendances héritées

Le graphe Slint v0.3 (catalogues embarqués et build) diffère de l'audit v0.1.
`packaging/v03-registry-review.json` fige les empreintes des deux graphes revus et
les dix archives supplémentaires MIT/Apache-2.0 ou GPL-3.0-only ; aucune autre
variation de lockfile n'est acceptée automatiquement. Le texte MIT manquant de
l'archive rspolib est fourni depuis son commit amont exact, avec provenance et
hash. Le backend local #94, ses textes GPL et sa provenance sont inclus dans
les sources correspondantes et les notices. Ce complément ne change pas les
dépendances ni le moteur ; il corrige la collecte historique pour v0.3.
