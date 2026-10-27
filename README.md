# Bunny and Size — v0.4.0-rc.1

Analyse locale de l’espace disque, en lecture seule, sous licence **GPL-3.0-only**.
Prerelease publique d’évaluation publiée avec l’autorisation du propriétaire du **27 octobre 2026**.

**[Téléchargements et notes de release](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.4.0-rc.1)** · [Guide V0.4](docs/v0.4/USAGE.md) · [Licences](docs/LICENSING.md) · [Sécurité](SECURITY.md)

## Fonctions

Recherche et filtres sur l’inventaire retenu, pagination, statistiques par extension et classe de taille,
doublons exacts à la demande (SHA-256 puis comparaison octet par octet), Treemap, exports JSON/CSV/TSV,
et sept langues : français, anglais, allemand, espagnol, portugais, italien et néerlandais.
Les partages SMB doivent déjà être autorisés et accessibles par le système (UNC/lecteurs Windows, montages CIFS Linux).
Inventaire borné, métriques inconnues explicites, annulation et résultats partiels ; aucune suppression automatique.
Les fonctions locales sont utilisables hors ligne, sans compte ni télémétrie par défaut.

## Installation

Télécharger l’archive Linux x86_64 ou Windows x64 et l’extraire intégralement.
Lancer `bunny-and-size` sous Linux, `bunny-and-size.exe` sous Windows ; CLI `bunny-and-size-cli` et alias `sortsize`.
Consulter les prérequis dans [le guide](docs/v0.4/USAGE.md) et les documents du paquet.
Windows requiert le runtime officiel Microsoft Visual C++ Redistributable x64 v14.
Rust n’est pas nécessaire pour exécuter les binaires. Les SHA-256 sont dans [SHA256SUMS](SHA256SUMS).

## Réserves acceptées

- **D1 #139 reste UNRESOLVED** : le pied d’interface peut garder les pixels de la langue précédente.
- **Windows sans signature Authenticode, éditeur non vérifié.** SmartScreen peut avertir ou bloquer ; certains postes administrés peuvent interdire l’exécution.
- Accessibilité humaine, certaines configurations DPI/multi-écrans et redimensionnement libre non entièrement qualifiés.
- SMB physique non testé sur le PC de qualification ; les validations Linux et Windows VM conservent leur portée.
- Parcours physique de téléchargement Windows avec Mark of the Web / SmartScreen : **NOT_RUN**. Un contrôle en VM ne vaut pas qualification physique.
- Android et Wayland natif non qualifiés pour cette livraison. Cette RC n’est pas une version stable.

Conserver Defender, SmartScreen et les politiques de sécurité actifs. Les empreintes SHA-256 ne constituent pas une signature numérique.
Lire les [notes complètes](docs/releases/v0.4.0-rc.1/RELEASE_NOTES.md) et [WINDOWS_SECURITY.md](docs/release/v0.4/WINDOWS_SECURITY.md).

## Sources, provenance et licences

L’archive `Bunny-and-Size-v0.4.0-rc.1-source.tar.gz` de la release est la source correspondante de référence,
avec dépendances Cargo verrouillées et vendoriées, recettes hors ligne et textes légaux par plateforme.
Le présent dépôt reprend ces fichiers publics approuvés, sans historique privé ni preuves de qualification.
Les **26 998 entrées de compilation sont identiques** ; les adaptations de documentation publique sont décrites dans
[PUBLICATION.json](docs/releases/v0.4.0-rc.1/PUBLICATION.json) et [SOURCE_FILES.json](SOURCE_FILES.json).
Les métadonnées originales de l’archive sont conservées dans `docs/releases/v0.4.0-rc.1/source-metadata/`.
Pour reconstruire les binaires distribués, utiliser l’archive correspondante et [la recette](docs/BUILDING.md).
Les archives GitHub automatiques ne remplacent pas l’asset source correspondant.

Les indicateurs embarqués `public_release=false` / `distribution_approved=false` décrivent la préparation.
La publication est autorisée séparément par une décision propriétaire externe schema 3 portant sur les sept assets finaux ;
ils restent immuables. Le gate natif Windows a retourné **AUTHORIZED FOR PUBLICATION**.

Code : [GPL-3.0-only](LICENSE). Visuels adaptés : **CC BY-SA 4.0**, attribution **Louis Lejuez / TazmenWorld**,
selon [BRANDING.md](BRANDING.md), [CREDITS.md](CREDITS.md) et [le texte CC](assets/branding/CC-BY-SA-4.0.txt).
Les composants tiers conservent leurs licences et avis ; voir `distribution-legal/`. Aucune garantie.
Les versions et tags antérieurs conservent leur portée historique.
