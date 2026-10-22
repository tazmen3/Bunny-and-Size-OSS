# SortSize V0.1.0 — Préversion / Prerelease

**Historical v0.1 release notes.** See [v0.2.0-rc.1 release notes](docs/releases/v0.2.0-rc.1/RELEASE_NOTES.md) for the current candidate.

Première livraison expérimentale du scanner local SortSize : moteur partagé Rust, interface graphique Slint et outil en ligne de commande. Les analyses sont en lecture seule, sans suppression, compte ou télémétrie. Les tailles affichées sont les tailles logiques ; elles n’estiment pas l’espace physiquement récupérable.

Version issue du commit de développement **`fbf46e3d94451b821cfb31fb6228aaea60258045`**. Les quatre archives ont passé le contrôle croisé : 28 vérifications, 5 444 fichiers contrôlés. Le programme reste expérimental ; les limites ci-dessous font partie de cette livraison.

## Fonctions disponibles

- Analyse d’un dossier local, progression et annulation avec conservation des résultats partiels.
- Navigation par arbre et tableau, retour au parent, tris par nom ou taille.
- Sélection d’un dossier par dialogue natif, avec saisie manuelle alternative.
- CLI séparée avec sortie JSON et export JSON vers un nouveau fichier.
- Avis de licences et GPLv3 intégrale accessibles hors ligne dans **À propos / Licences**.

## Téléchargements et SHA-256

Les quatre archives ci-dessous et `SHA256SUMS` sont les cinq assets de cette Release. Choisir l’archive binaire de son système ; les archives source sont proposées au même endroit, gratuitement et sans accès au dépôt de développement privé.

| Fichier | SHA-256 |
|---|---|
| `SortSize-v0.1.0-windows-x86_64.zip` | `56faf4477b2d5589a712bdd3d7f1ee438dfa046fafdcc301c40cbd2a565cc943` |
| `SortSize-v0.1.0-source-windows-x86_64.zip` | `4b3b8d133ece3f095ef7556010c6aaf344d718ca8609c52f599f9d087ba9fcfe` |
| `SortSize-v0.1.0-linux-x86_64.tar.gz` | `b2d92ee73cf72f9d9175d5356c188603d330bff3ea2793c6feb17a8409cb7ca4` |
| `SortSize-v0.1.0-source-linux-x86_64.tar.gz` | `1018e7727db94c47a11f13efa3c2ce826cab432e0a3d6295fcfb4393b29dd5d6` |

Sous PowerShell, vérifier un fichier avec `Get-FileHash -Algorithm SHA256 -LiteralPath 'NOM_DU_FICHIER'`. Sous Ubuntu, placer les quatre fichiers et le manifeste dans un même dossier puis utiliser `sha256sum -c SHA256SUMS`.

## Windows x64

1. Extraire **tout** le ZIP binaire dans un nouveau dossier accessible.
2. Installer, si nécessaire, le [Microsoft Visual C++ Redistributable x64 v14 officiel](https://aka.ms/vc14/vc_redist.x64.exe). La chaîne utilisée est MSVC 14.44 : le README du paquet indique au moins **14.44.35228**, borne minimale non testée isolément. Microsoft recommande un runtime au moins aussi récent que les outils de compilation ; utiliser la dernière version officielle compatible. [Prérequis Microsoft](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170).
3. Lancer `SortSize.exe`, choisir un dossier avec **Parcourir**, puis **Analyser**. La sélection seule ne lance pas l’analyse.
4. Pour la CLI, ouvrir un terminal dans le dossier extrait :

```powershell
.\sortsize-cli.exe --help
.\sortsize-cli.exe scan 'C:\dossier-a-analyser' --format json
```

Rust, Cargo et Visual Studio ne sont pas des prérequis d’exécution prévus. **La recette sur un Windows propre sans ces outils n’a toutefois pas été effectuée**, ni le scénario runtime absent puis installé. Le poste Windows de validation possède déjà le runtime **14.50.35719.0**. Les exécutables ne sont pas signés et le parcours de téléchargement/SmartScreen reste à vérifier. Ne pas désactiver les protections de Windows ni télécharger une DLL isolée ; utiliser le paquet Microsoft officiel si le runtime manque.

## Ubuntu x86_64

La recette native concerne **Ubuntu 24.04.5 / KDE-X11**. Extraire le paquet dans un nouveau dossier :

```sh
tar -xzf SortSize-v0.1.0-linux-x86_64.tar.gz
cd SortSize-v0.1.0-linux-x86_64
./sortsize-gui
# Ou, dans un terminal :
./sortsize-cli scan /chemin/du/dossier --format json
```

Rust et Cargo ne sont pas nécessaires pour lancer les exécutables. Les bibliothèques système restent externes : **glibc 2.39**, libgcc, Fontconfig, FreeType, polices, bibliothèques clientes X11 et xkbcommon. Le dialogue de sélection requiert un bus D-Bus de session et `xdg-desktop-portal` avec le backend adapté au bureau, notamment KDE ou GTK. Le README du paquet détaille les paquets Ubuntu nécessaires ; aucune bibliothèque système n’est incluse.

## Limites connues

- **KDE/X11** : une session a cessé de répondre pendant des injections clavier rapides dans les champs texte ; cause non établie. Des sessions indépendantes, dont une saisie temporisée, ont réussi avec IBus actif. Ces succès ne suppriment pas la réserve ; IBus n’est pas une cause démontrée.
- Composition **IME**, clavier matériel et accessibilité complète non validés. Le backend GUI livré est X11 ; ni Wayland natif ni une recette XWayland ne sont annoncés comme validés. Le contournement IBus cité dans l’ancien README n’est pas une résolution démontrée du blocage KDE final.
- Le dialogue KDE peut altérer un nom Unix non UTF-8 ; la validation du backend GTK pour ce cas est distincte. Les variantes de chemins Windows longs ou sans alias NTFS ne sont pas toutes certifiées.
- Résultats partiels possibles en cas de refus d’accès, d’annulation ou de limite de parcours. L’affichage conserve au plus 50 000 entrées et 8 Mio de chemins ; le comptage peut continuer après la limite d’affichage. Un appel système bloqué peut retarder l’annulation.
- Pas de snapshot cohérent de fichiers modifiés pendant l’analyse ; liens symboliques non suivis par défaut. Les tailles logiques comptées par chemin ne constituent pas une mesure de gain récupérable.
- Pas de Treemap, recherche avancée, doublons, suppression, historique SQLite, connecteurs cloud/NAS ou livraison Android dans cette version. Les autres distributions Linux et versions de Windows ne sont pas certifiées par cette recette.

## Licence et code source correspondant

SortSize est proposé sous **GPL-3.0-only**, avec la voie GPL de Slint 1.18.1. **Aucune garantie**, dans les limites de la loi. Lire `LICENSE`, `AUTHORS.txt`, `THIRD_PARTY_NOTICES.txt` et `licences/` dans les paquets ; les composants tiers conservent leurs propres conditions et attributions.

Le source correspondant au binaire Windows est **`SortSize-v0.1.0-source-windows-x86_64.zip`**, et celui du binaire Ubuntu est **`SortSize-v0.1.0-source-linux-x86_64.tar.gz`**. Ces archives contiennent les sources produit, lockfiles, textes légaux, dépendances Cargo verrouillées et recettes de reconstruction hors ligne. `BUILDINFO.json` lie chaque binaire à son archive source par SHA-256.

Les liens automatiques GitHub **Source code (zip/tar.gz)** ne remplacent pas ces deux archives préparées : ils ne fournissent pas leur registre Cargo local complet. Aucun compte donnant accès au dépôt privé de développement ne doit être nécessaire pour télécharger les sources jointes. Les outils de compilation restent nécessaires seulement pour reconstruire une version modifiée, selon les README source.


## English — first open-source prerelease

SortSize V0.1.0 is an **experimental open-source prerelease**, targeting Windows x64 and Ubuntu 24.04 x86_64. It combines a shared Rust engine, Slint GUI and JSON CLI. Local scans are read-only, without an account, telemetry or implicit deletion. Features include native folder selection, tree/table navigation, name/size sorting, progress, cancellation with partial results, JSON export and an offline About / Licences window.

Download the binary archive for your platform and verify its SHA-256 against the attached `SHA256SUMS`. All four exact filenames and hashes are listed above. **The two named corresponding-source bundles are available alongside the binaries without authentication or extra charge. GitHub’s automatic source ZIP/tar.gz downloads are not substitutes for these bundles and their locked local Cargo registries.**

**Windows:** extract all files and run `SortSize.exe`, or `sortsize-cli.exe scan PATH --format json`. Install the [official Microsoft Visual C++ x64 v14 Redistributable](https://aka.ms/vc14/vc_redist.x64.exe) if needed. MSVC 14.44 was used; the package documents 14.44.35228 or later, a minimum not tested separately. The validated host uses 14.50.35719.0. **A clean Windows system without development tools, including the runtime-absent-then-installed scenario, has not yet been tested.** EXEs are unsigned and SmartScreen behavior is not certified.

**Ubuntu:** extract the tar.gz and run `./sortsize-gui`, or `./sortsize-cli scan PATH --format json`. The target environment is Ubuntu 24.04 x86_64 with **glibc 2.39, Fontconfig, FreeType/fonts and X11/xkbcommon libraries**. Folder selection requires session D-Bus and xdg-desktop-portal with the appropriate KDE/GTK backend. Runtime system libraries are not bundled.

**Known limitations:** an occasional KDE/X11 hang occurred during rapid keyboard injection/text-field interaction; the cause is undetermined. Successful independent sessions do not establish a fix, and IBus is not a proven cause. **IME composition, physical keyboard, complete accessibility and Wayland/XWayland are not certified.** No general compatibility across all distributions or Windows versions is claimed. KDE may alter non-UTF-8 Unix names; not all Windows long-path cases are certified. Scans are not filesystem snapshots; access errors/cancellation/limits can yield partial results, and blocked system calls may delay cancellation. Retention is limited to 50,000 displayed entries / 8 MiB of paths. Sizes are logical per path, not recoverable-space estimates. No Treemap, advanced search, duplicate management, deletion, SQLite history, cloud/NAS connectors or Android package is included.

**GPL-3.0-only**, without warranty. Read LICENSE, AUTHORS, THIRD_PARTY_NOTICES and licences/ inside the archives. Third-party terms remain applicable; no stable-release or certified legal-compliance claim is made. The owner accepted the previously identified workstation paths that remain embedded in the unchanged assets. Product origin is recorded in BUILDINFO; the public snapshot commit and tag have a different Git identity from the origin product commit.
