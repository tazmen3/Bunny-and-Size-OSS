#!/bin/sh
# Arguments : corpus #50 propre, sortie neuve, cache target DATA-Linux explicite.
set -eu
[ "$#" -eq 3 ] || { echo "Usage: $0 AUDIT_ROOT OUTPUT TARGET_DIR" >&2; exit 2; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
[ -z "$(git status --porcelain --untracked-files=all)" ] || { echo "Le checkout doit être propre avant compilation." >&2; exit 2; }
[ ! -e "$2" ] || { echo 'La sortie doit être neuve.' >&2; exit 2; }
rustc +1.99.0 -Vv | grep -qx 'host: x86_64-unknown-linux-gnu'
for name in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET SLINT_STYLE SLINT_DEFAULT_FONT SLINT_FONT_PATH SLINT_EMBED_TEXTURES SLINT_EMBED_RESOURCES RUST_FONTCONFIG_DLOPEN; do
    value=$(printenv "$name" || true)
    [ -z "$value" ] || { echo "Variable de build non prévue : $name" >&2; exit 2; }
done
mkdir -p -- "$3"
target_root=$(CDPATH= cd -- "$3" && pwd)
export RUSTFLAGS="--remap-path-prefix=$target_root=/build --remap-path-prefix=$root=/sortsize --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
export CARGO_BUILD_JOBS=2
before=$(sha256sum Cargo.lock spikes/slint/Cargo.lock)
cargo +1.99.0 build --locked --release --target x86_64-unknown-linux-gnu --target-dir "$3" -p sortsize-cli --bins
cargo +1.99.0 build --locked --release --target x86_64-unknown-linux-gnu --target-dir "$3" --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
[ "$before" = "$(sha256sum Cargo.lock spikes/slint/Cargo.lock)" ]
# Variables locales à ce processus ; aucun réglage global.
RUSTUP_TOOLCHAIN=1.99.0 python3 "$root/scripts/package_linux.py" --audit-root "$1" --output "$2" --target-dir "$3"
