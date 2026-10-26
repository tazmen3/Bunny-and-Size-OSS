# Source correspondant candidat — Windows x64

`BUILDINFO.json` indique le commit exact des sources produit et les hashes des
trois exécutables candidats (GUI, CLI et alias). Les sources du projet sont exportées des blobs Git
de ce commit, avec les **deux Cargo.lock originaux**, sans modification des crates.
Le registre `vendor-registry/` contient les archives Cargo originales et un
index local. `SOURCE_COMPONENTS.json` donne leurs checksums, rôles, licences et
textes associés. Aucun cache, identifiant, binaire de build ou outil MSVC fourni.

## Reconstruction

Si les sources viennent d'un clone Git Windows, désactiver la conversion CRLF
pour ce seul clone avant checkout (`git -c core.autocrlf=false clone ...`).
Le packager refuse les entrées différentes des blobs Git, même si Git les
considère propres. L'archive source fournie contient directement les blobs LF.

Préparer Windows x64 avec Rust **1.99.0 MSVC**, Visual Studio Build Tools C++
x64 et SDK Windows. Ouvrir un Developer Windows PowerShell 5.1 ou PowerShell 7 x64, puis se placer à la
racine de ce dossier et exécuter :

```powershell
.\rebuild-windows.ps1 -BuildDirectory 'C:\chemin\neuf-pour-reconstruction'
```

Le script refuse un dossier existant, utilise un **CARGO_HOME neuf**, un target
neuf et `--frozen` (verrouillé + hors ligne). Le toolchain doit être installé
avant l'essai ; sa fourniture et les outils Microsoft sont des prérequis de
développement externes. Le runtime Rust lié est accompagné de son disclosure
et de ses licences. Les sorties sont bunny-and-size.exe, bunny-and-size-cli.exe et sortsize.exe.

## Choix de vendoration

`cargo vendor --locked --versioned-dirs --sync spikes/slint/Cargo.toml` a été
étudié. Il vendorerait aussi les sources des autres plateformes. Une tentative
limitée aux dépendances compilées ne suffit pas à Cargo : la résolution demande
aussi des sources optionnelles (par exemple ash), sans les compiler.

Le registre local utilise le remplacement de source Cargo documenté :
<https://doc.rust-lang.org/cargo/reference/source-replacement.html#local-registry-sources>.
Seules les archives renvoyées par `cargo metadata --locked --filter-platform
x86_64-pc-windows-msvc` pour les deux workspaces sont incluses (tests et sources
nécessaires à la résolution Windows inclus). Les résumés d'index des lockfiles
complets sont préservés, sans ajouter les sources des autres plateformes.
Les fonctionnalités optionnelles Skia/Qt/Android ne sont pas livrées comme
fonctionnalités actives ; ce bundle ne promet pas de les reconstruire hors ligne.

Les textes de licence #50/#51 sont réutilisés en lecture seule. Les seules
sources supplémentaires non couvertes par ce corpus reçoivent un inventaire
séparé et leurs textes légaux extraits des archives hachées. Elles conservent
leurs licences d'origine ; aucune requalification générale sous GPL n'est faite.
Six compléments sont repris aux commits amont déclarés par leurs archives,
avec URL/hash dans `SOURCE_LEGAL_SUPPLEMENTS.json`. `skia-bindings` déclare
`dirty: true` dans sa provenance de publication : l'archive Cargo hachée reste
la source exacte fournie, sans prétendre qu'elle égale le commit Git amont.
Skia n'est pas compilé dans ces PE. Le complément d’intégration compare les
54 fichiers source correspondants et distingue trois fichiers ajoutés/générés
et le manifeste Cargo normalisé. Le marqueur dirty n’est pas une interdiction
juridique en soi ; l’archive exacte reste fournie. Le texte BSD de Skia est ajouté
pour les sources générées, dont les entrées exactes de génération ne sont pas
reproduites. Voir docs/V01_INTEGRATION.md ; revue finale du bundle requise.

L'essai de reconstruction garantit uniquement la configuration Windows x64
logicielle par défaut réellement testée, pas l'identité bit à bit des PE ni un
essai Windows propre. Avant distribution, valider les notices, le mode d'accès
aux sources, les mentions produit et la disponibilité effective pour les
destinataires avec les deux archives finales. Conformité globale non démontrée.

L'accord du propriétaire du 25 octobre 2026 couvre les seuls dérivés du lapin
listés avec leurs empreintes dans PROVENANCE.json. Voir BRANDING.md pour CC BY-SA
4.0 et la compatibilité des adaptations incorporées vers GPLv3. La disponibilité
des droits graphiques ne vaut pas autorisation de publier une nouvelle release.
Les anciens candidats `redistribution_ready=false` restent des preuves privées.

Mascotte adaptée : Louis Lejuez / TazmenWorld, CC BY-SA 4.0.
Adaptations incorporées : licence d'adaptateur GPL-3.0-only, voir BRANDING.md.
Texte CC intégral et empreintes dans licences/artwork/ ; maître et conversions
fournis avec les sources. Original complet et portrait exclus des livrables.
