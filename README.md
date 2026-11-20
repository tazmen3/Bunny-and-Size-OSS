# Bunny and Size

Bunny and Size analyzes local storage on Ubuntu and Windows. Explore folder sizes, find large or duplicate files, and reopen saved scans offline.

## Download v0.5.0-rc.3

This is an evaluation prerelease.

| Platform | Download |
| --- | --- |
| Windows 11 (x64) | [Windows archive (.zip)](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/Bunny-and-Size-v0.5.0-rc.3-windows-x86_64.zip) |
| Ubuntu 24.04 (x86_64) | [Ubuntu archive (.tar.gz)](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/Bunny-and-Size-v0.5.0-rc.3-ubuntu-x86_64.tar.gz) |
| Source code | [Source archive (.tar.gz)](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/Bunny-and-Size-v0.5.0-rc.3-source.tar.gz) |

[Release notes](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.5.0-rc.3) · [SHA-256 checksums](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/SHA256SUMS) · [Build provenance](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/PROVENANCE.json)

## Run the app

Extract the entire archive before launching the app.

- **Ubuntu**. Run `./bunny-and-size` from the extracted folder in an X11 or XWayland session.
- **Windows**. Open `bunny-and-size.exe` from the extracted folder. The app requires the x64 Visual C++ runtime. [Microsoft Visual C++ Redistributable](https://aka.ms/vc14/vc_redist.x64.exe)

The standard CLI is `bunny-and-size-cli`, also available as `sortsize`. Use `bunny-and-size-inventory-cli` for inventory commands.

[Windows distribution details](CODE_SIGNING.md) · [Build from source](docs/BUILDING.md)

## Licenses

The original code is licensed under Apache-2.0. The adapted artwork is licensed under CC BY-SA 4.0. The desktop binaries use Slint under its Royalty-free 2.0 license. Third-party components retain their own licenses. [Apache-2.0 license](LICENSE) · [Third-party licenses and attribution](docs/LICENSING.md)
