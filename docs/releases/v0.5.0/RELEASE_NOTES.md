# Bunny and Size 0.5.0

Stable release for Ubuntu 24.04 x86_64 and Windows 11 x64.

## What's new in 0.5

- Save and reopen storage inventories, including when the original drive or share is offline. Compare saved inventories and search for duplicates across sources.
- Open an existing SMB measurement from the NAS view with one click. Use **Scan** to refresh it, or **Reconnect** to recover a disconnected share.
- Closing **Saved measures** with Escape returns keyboard focus to the button that opened it. The Windows inventory CLI accepts equivalent path separators on local drives when checking duplicate searches.
- Large saved inventories use on-disk indexes for exact name searches and sorted results under constrained memory.

## Download and run

Extract the full archive and verify it against [SHA256SUMS](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0/SHA256SUMS).

| Platform | Download |
| --- | --- |
| Ubuntu 24.04 x86_64 | [Ubuntu archive](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0/Bunny-and-Size-v0.5.0-ubuntu-x86_64.tar.gz) |
| Windows 11 x64 | [Windows archive](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0/Bunny-and-Size-v0.5.0-windows-x86_64.zip) |
| Source code | [Offline build archive](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0/Bunny-and-Size-v0.5.0-source.tar.gz) |

- **Ubuntu**. Run `./bunny-and-size` from the extracted directory in an X11 or XWayland session.
- **Windows**. Launch `bunny-and-size.exe` in the extracted directory. The app requires the [x64 Microsoft Visual C++ runtime](https://aka.ms/vc14/vc_redist.x64.exe). The executables are unsigned. SmartScreen and managed-device policies may affect launch.

## Known limits

- The first exact page waits for integrity verification when opening a saved inventory. In qualification on Ubuntu, opening and producing the first page at 10 million files with a 128 MiB memory limit took 110.88 seconds, including 107.95 seconds for integrity verification. The 100 ms opening target and the full load qualification matrix remain open.
- Physical multi-monitor qualification is planned for 0.6. NAS scan performance qualification remains open.

The original code uses Apache-2.0. The downloads include third-party notices, artwork attribution, and build provenance. The source archive includes the Rust dependencies for offline builds.
