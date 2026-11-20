# Bunny and Size 0.5.0-rc.2

Analyse locale du stockage, pour Ubuntu 24.04 x86_64 et Windows 11 x64.
Voir [les notes de version](RELEASE_NOTES.md) et [les avis tiers](THIRD_PARTY_NOTICES.txt).

## Compilation hors ligne

Prévoir Rust 1.99, un compilateur C et les bibliothèques de développement
X11/fontconfig sur Linux, ou les outils C++ MSVC x64 et le SDK Windows.
L’archive contient les sources Cargo vendoriées et les lockfiles.

```text
cargo build --frozen --release -p sortsize-cli
cargo build --frozen --release --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
cargo build --frozen --release --target-dir target-inventory -p sortsize-cli --features inventory-report --bin bunny-and-size-cli
```

La dernière commande produit la CLI avec les commandes d’inventaire.
Le dossier de sortie séparé évite de remplacer la CLI standard. Pour les
binaires distribués, la compilation applique aussi un remappage des chemins
de la source, de Cargo et de la toolchain. L’archive source fixe les entrées,
sans garantir l’identité binaire entre systèmes de compilation.
