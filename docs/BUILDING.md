# Build Bunny and Size 0.5.0

Use Rust 1.99.0. The source archive includes Cargo lockfiles and a local dependency mirror for offline builds. On Windows, install Visual Studio Build Tools with C++ and the Windows SDK, then use an x64 Developer Shell. On Ubuntu, install a C/C++ toolchain, pkg-config, and development libraries for fontconfig, X11, and Wayland.

Run these commands from the extracted source directory.

```sh
cargo build --frozen --release -p sortsize-cli --no-default-features --bins
cargo build --frozen --release --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
```

For the inventory CLI, build `bunny-and-size-cli` with `--features inventory-report` and save it separately as `bunny-and-size-inventory-cli`. See the [release notes](../RELEASE_NOTES.md) for runtime requirements.
