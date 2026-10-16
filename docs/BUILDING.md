# Compilation / Building

Les binaires validés n’ont pas été reconstruits pour cette publication. Les commandes ci-dessous sont destinées aux personnes souhaitant compiler ou modifier les sources. The validated binaries were not rebuilt for publication; these commands are for building or modifying the source.

## Prérequis / Requirements

- Rust **1.99.0** épinglé par `rust-toolchain.toml`, Cargo et les deux lockfiles inchangés. Rust 1.99.0 is pinned; both lockfiles are preserved.
- Windows x64 : MSVC Build Tools, Windows SDK, terminal développeur x64 ; GUI native Winit. Windows requires MSVC Build Tools and a Windows SDK.
- Ubuntu 24.04 x86_64 : compilateur/linker C, `pkg-config`, développement Fontconfig (`libfontconfig-dev`) et les bibliothèques d’exécution décrites dans le guide. Ubuntu requires a C toolchain, pkg-config, Fontconfig development files and the documented runtime libraries.

## Depuis ce dépôt / From this repository

Le workspace racine contient moteur et CLI ; le prototype Slint a son propre workspace et lockfile. The root workspace contains the core and CLI; Slint has a separate workspace/lockfile.

```sh
cargo build --locked --release -p sortsize-cli --bin sortsize
cargo build --locked --release --manifest-path spikes/slint/Cargo.toml --bin sortsize-slint-spike
```

Sorties / Outputs : `target/release/sortsize` et `spikes/slint/target/release/sortsize-slint-spike`, avec suffixe `.exe` sous Windows. Les noms des binaires sources diffèrent des noms de distribution `sortsize-cli` et `sortsize-gui` / `SortSize.exe`.

Vérifications de développement possibles, non relancées par cette publication / Development checks, not rerun for this publication:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo fmt --manifest-path spikes/slint/Cargo.toml --all --check
cargo clippy --locked --manifest-path spikes/slint/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path spikes/slint/Cargo.toml
```

## Sources correspondant aux binaires / Corresponding source

Les bundles source joints à la Release sont la référence de reconstruction des binaires distribués. Ils incluent les registres Cargo verrouillés, licences et README nécessaires. The named source release assets are the reference for the shipped binaries and include locked local Cargo registries and notices.

- Windows : extraire `SortSize-v0.1.0-source-windows-x86_64.zip`, puis suivre son README et `rebuild-windows.ps1 -BuildDirectory CHEMIN_NEUF` depuis PowerShell 7 avec Rust/MSVC/SDK préparés.
- Ubuntu : extraire `SortSize-v0.1.0-source-linux-x86_64.tar.gz`, puis suivre son README et `./rebuild-linux.sh DOSSIER_BUILD_NEUF` sur un hôte préparé.

Ces recettes utilisent `--frozen` et des dossiers de build neufs ; elles ne dispensent pas d’installer préalablement la chaîne système. Elles ne sont pas exécutées pour cette publication. Offline recipes use --frozen and fresh build folders but still require a preinstalled toolchain; they were not executed for this publication.

Aucune identité bit à bit entre des builds sur des chemins/hôtes différents n’est promise. Le snapshot public conserve les sources produit exactes, sans exporter l’ancien historique ou les scripts de packaging dépendant des preuves internes. No bit-for-bit reproducibility across hosts/paths is promised; history-dependent internal packaging tools are not imported.
