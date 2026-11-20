# Bunny and Size 0.5.0-rc.3

Evaluation prerelease for Windows 11 x64 and Ubuntu 24.04 x86_64.

## What's new

- The NAS view keeps discovered SMB shares visible and lists shares disconnected in Bunny with a Reconnect action for each one.
- Click a share to start its first scan. If a measurement is already available, the click opens it. Use **Scan** to refresh that measurement. Scanning the whole server still asks you to confirm the selected shares.
- The footer updates immediately after a language change. Screen readers distinguish the two unit controls, and shorter windows leave more room for the folder table.
- Global search avoids repeated parent-path lookups. The minimum-size filter in duplicate-file searches now applies to files and reports complete coverage when verification succeeds.

## Download and run

Extract the full archive and verify its checksum with [SHA256SUMS](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/SHA256SUMS).

| Platform | Download |
| --- | --- |
| Windows 11 x64 | [Windows (.zip)](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/Bunny-and-Size-v0.5.0-rc.3-windows-x86_64.zip) |
| Ubuntu 24.04 x86_64 | [Ubuntu (.tar.gz)](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/Bunny-and-Size-v0.5.0-rc.3-ubuntu-x86_64.tar.gz) |
| Source code | [Source archive (.tar.gz)](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/download/v0.5.0-rc.3/Bunny-and-Size-v0.5.0-rc.3-source.tar.gz) |

- **Windows**. Open `bunny-and-size.exe` in the extracted folder. The app requires the [x64 Microsoft Visual C++ runtime](https://aka.ms/vc14/vc_redist.x64.exe).
- **Ubuntu**. Run `./bunny-and-size` from the extracted folder in an X11 or XWayland session.

## Validation

The SMB view and single-click workflow were checked on a physical NAS from Windows. Targeted SMB and interface tests also passed on Ubuntu in a virtual machine. Earlier RC.3 load runs verified local inventories up to 10 million files in Auto mode. The full qualification matrix is still in progress. NAS scan performance work is deferred.

The original code uses Apache-2.0. The archives contain third-party licenses, artwork attribution, and build provenance.
