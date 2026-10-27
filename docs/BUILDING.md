# Compilation — v0.4.0-rc.1

Pour reconstruire les exécutables distribués, télécharger l’asset source correspondant de la release,
vérifier SHA256SUMS et extraire l’intégralité de `Bunny-and-Size-v0.4.0-rc.1-source.tar.gz`.
Suivre son README : Python >= 3.11, Rust 1.99.0 x64 natif, outils système Ubuntu 24.04 ou MSVC/SDK Windows.
Depuis l’archive extraite intacte :

```text
python scripts/v04_distribution.py build --source . --output ../build-neuf
```

Cette recette contrôle les manifestes originaux et utilise deux jobs, les dépendances vendoriées,
`--frozen` et Cargo hors ligne. Les outils système ne sont pas redistribués.
La reproductibilité binaire entre machines n’est pas revendiquée.

L’instantané Git conserve les mêmes 26 998 entrées de compilation et les dépendances hors ligne.
Une compilation manuelle de développement peut être lancée avec :

```text
cargo build --frozen --release -p sortsize-cli
cargo build --frozen --release --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
```

Les documents publics courants diffèrent de ceux de l’archive ; leurs empreintes et celles des fichiers originaux
sont documentées dans SOURCE_FILES.json. Les scripts de préparation exigeant les manifestes d’origine
doivent être exécutés depuis l’archive source intacte. Aucun rebuild n’a été effectué pour cette publication.
