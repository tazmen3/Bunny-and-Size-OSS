#!/bin/sh
# Reconstruction sur un hôte Ubuntu x86_64 préparé ; réseau Cargo interdit.
set -eu
[ "$#" -eq 1 ] || { echo "Usage: $0 DOSSIER_BUILD_NEUF" >&2; exit 2; }
[ ! -e "$1" ] || { echo 'Le dossier doit être neuf ; aucun écrasement.' >&2; exit 2; }
mkdir -p -- "$1"
build_root=$(CDPATH= cd -- "$1" && pwd)
export CARGO_HOME="$build_root/cargo-home"
export CARGO_TARGET_DIR="$build_root/target"
export CARGO_NET_OFFLINE=true
export RUSTUP_TOOLCHAIN=1.99.0-x86_64-unknown-linux-gnu
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET SLINT_STYLE SLINT_DEFAULT_FONT SLINT_FONT_PATH SLINT_EMBED_TEXTURES SLINT_EMBED_RESOURCES RUST_FONTCONFIG_DLOPEN
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"
export RUSTFLAGS="--remap-path-prefix=$CARGO_TARGET_DIR=/build --remap-path-prefix=$root=/sortsize --remap-path-prefix=$CARGO_HOME=/cargo"
export CARGO_BUILD_JOBS=2
cargo build --frozen --release --target x86_64-unknown-linux-gnu -p sortsize-cli --bins
cargo build --frozen --release --target x86_64-unknown-linux-gnu --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
printf 'Reconstruit hors ligne : %s/x86_64-unknown-linux-gnu/release\n' "$CARGO_TARGET_DIR"
