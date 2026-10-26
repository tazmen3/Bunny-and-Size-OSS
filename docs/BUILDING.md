# Compilation / Building — v0.3.0-rc.1

Pour reconstruire les binaires distribués hors ligne, télécharger l'archive source correspondante Linux ou Windows de la release, vérifier SHA256SUMS, extraire tout et suivre son README et `rebuild-linux.sh` / `rebuild-windows.ps1`. Rust/Cargo 1.99.0 est requis ; les outils natifs C/C++ et dépendances système restent des prérequis externes. Les dépendances Cargo verrouillées et le backend Slint modifié sont fournis dans ces archives. Aucune garantie de reproductibilité binaire octet pour octet n'est revendiquée.

For offline rebuilding, download the matching platform's corresponding-source release asset, verify SHA256SUMS, fully extract and follow its README and `rebuild-linux.sh` / `rebuild-windows.ps1`. Rust/Cargo 1.99.0 is required; native C/C++ tools and system dependencies are external prerequisites. The bundles include locked Cargo dependencies and the modified Slint backend. Byte-for-byte binary reproducibility is not claimed.

L'instantané Git public peut utiliser le registre Cargo en ligne / The public Git snapshot can use the online Cargo registry:

```sh
cargo build --locked --release -p sortsize-cli
cargo build --locked --release --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
```

Ubuntu : environnement de compilation C/C++, pkg-config et fichiers de développement Fontconfig, plus bibliothèques du guide utilisateur. Windows : outils MSVC C++ x64 et SDK Windows. Les scripts historiques de packaging conservent leurs références d'audit privé ; ils ne sont pas la recette publique de reconstruction. / Ubuntu needs C/C++ build tools, pkg-config and Fontconfig development files, plus the user guide's libraries. Windows needs MSVC C++ x64 build tools and Windows SDK. Historical packaging scripts retain private audit references; use the source bundle recipes for public rebuilding.
