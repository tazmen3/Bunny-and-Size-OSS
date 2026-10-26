# Source correspondant — Bunny and Size v0.3.0-rc.1 Ubuntu x86_64

Code original GPL-3.0-only (LICENSE), sources Rust/Slint et ressources au commit
BUILDINFO.json, deux Cargo.lock inchangés, archives originales Cargo vérifiées
contre leurs checksums, index local limité aux versions verrouillées, notices et
textes tiers. Les sources optionnelles/résolution/tests sont distinguées des
composants de production dans SOURCE_COMPONENTS.json. Cargo exige aussi leurs
archives dans ce registre local, même lorsque leurs backends sont inactifs. Aucun cache Cargo, target,
outillage Rust ou bibliothèque système binaire n’est inclus.

Sur Ubuntu 24.04 x86_64 préparé avec Rust **1.99.0**, cible
`x86_64-unknown-linux-gnu`, compilateur/linker C, Python 3.11+, `pkg-config` et
`libfontconfig1-dev` (ses dépendances incluent FreeType), reconstruire :

```sh
sh rebuild-linux.sh /chemin/neuf/sur/DATA-Linux/reconstruction
```

La recette utilise `--frozen`, un CARGO_HOME neuf et un target neuf. Les trois
exécutables Cargo sont dans le sous-dossier
`target/x86_64-unknown-linux-gnu/release` : `bunny-and-size-cli` (CLI), `sortsize` (alias CLI) et `bunny-and-size` (GUI).
Les prérequis système de compilation doivent être installés avant de couper le
réseau. La chaîne Rust et les bibliothèques système sont des outils/prérequis
généraux externes, pas fournis ici. Les scripts de packaging et leurs entrées
sont inclus ; la reproduction des notices utilise aussi le corpus #50 c4affb3,
référence d’audit conservée dans le dépôt de développement. La reconstruction
des exécutables ne dépend pas de ce corpus externe.

Les manifestes couvrent les fichiers à l’exception conventionnelle de leurs
propres entrées ; SHA256SUMS couvre aussi FILES.json. Une même entrée produit
des archives identiques avec le même packager/epoch. L’identité bit à bit des
ELF sur un autre hôte/chemin n’est pas garantie.

Conserver toutes les mentions de droits, licences et notices des fichiers
originaux. Pour distribuer un binaire, donner également accès gratuitement au
source correspondant depuis le même lieu, conformément à GPLv3 §6(d), sans
restriction additionnelle ; ne pas se contenter du dépôt privé. Pas de signature,
verrouillage d’installation ni secret requis pour exécuter une version modifiée.
La fourniture locale et sa reconstruction constituent des preuves techniques ;
la conformité complète du mode de diffusion futur reste à vérifier.

Complément d’intégration : texte MIT officiel drm-fourcc publié en 2023 et
attributions originales conservés ; aucune mention de copyright fabriquée.
Le marqueur dirty de skia-bindings est documenté avec comparaison de son archive
exacte au commit amont ; il n’établit pas seul une incompatibilité de licence.
Le texte BSD de Skia complète les mentions des sources générées. Voir
`docs/V01_INTEGRATION.md` et SOURCE_LEGAL_SUPPLEMENTS.json. Les entrées exactes
de génération de bindings_docs.rs restent une limite de traçabilité ; aucune
fonction Skia n’est activée. Les anciennes archives #53 ne sont pas modifiées.

L'accord du propriétaire du 25 octobre 2026 couvre les seuls dérivés du lapin
listés avec leurs empreintes dans PROVENANCE.json. Voir BRANDING.md pour CC BY-SA
4.0 et la compatibilité des adaptations incorporées vers GPLv3. La disponibilité
des droits graphiques ne vaut pas autorisation de publier une nouvelle release.
Les anciens candidats `redistribution_ready=false` restent des preuves privées.

Mascotte adaptée : Louis Lejuez / TazmenWorld, CC BY-SA 4.0.
Adaptations incorporées : licence d'adaptateur GPL-3.0-only, voir BRANDING.md.
Texte CC intégral et empreintes dans licences/artwork/ ; maître et conversions
fournis avec les sources. Original complet et portrait exclus des livrables.
