# SortSize 0.2.0-rc.1

SortSize propose une analyse locale du stockage en lecture seule, avec une interface graphique et une CLI. Cette première candidate v0.2 pour Ubuntu et Windows ajoute notamment Treemap, Top 100, métadonnées et exports détaillés. Elle est destinée aux essais et conserve les limitations décrites ci-dessous.

SortSize is an open-source storage analyzer written in Rust. This release candidate provides a desktop GUI and a command-line tool for Ubuntu 24.04 x86_64 and Windows 11 x64. Scanning is read-only. The application has no default telemetry or account requirement. The current GUI is in French; a complete English interface is not included.

This is a public prerelease for evaluation, not a stable release. Download the four verified archives from [SortSize-OSS](https://github.com/tazmen3/SortSize-OSS/releases/tag/v0.2.0-rc.1). Packaging-time statements inside the immutable archives describe their historical state on 21 October 2026; this release and its external documents provide the publication context.

## Included in v0.2

- Synchronized folder tree, file table and logical-size Treemap, with parent navigation and direct/recursive counts.
- Global Top 100 observed files, independent of the bounded GUI inventory.
- Optional native allocated-size and hardlink identity collection; allocated size and logical size remain distinct. Reclaimable space is unknown.
- Modification/access timestamps in UTC, native owner IDs and optional, bounded owner-name resolution. NSS/Active Directory lookups require explicit activation and may access the network.
- Storage-source discovery and capacity information, including space available to the current account. Selecting a source does not start a scan.
- Eleven configurable columns, binary/decimal units, twelve sort orders, saved display preferences and reset.
- CLI summary JSON v2 and CSV/TSV summary v1 with 40 columns; detailed CSV/TSV entries v1 with 52 columns, native-path preservation, external sorting, qualified aggregates and a final `end` record.
- Configurable worker pool, cooperative cancellation and explicit complete/partial/cancelled results.

## Downloads and requirements

Download the binary archive **and its matching corresponding-source archive** for your platform. Extract the entire binary archive, including notices and license texts.

| Platform | Binary archive | Corresponding source |
|---|---|---|
| Ubuntu 24.04 LTS x86_64 | [SortSize-v0.2.0-rc.1-linux-x86_64.tar.gz](https://github.com/tazmen3/SortSize-OSS/releases/download/v0.2.0-rc.1/SortSize-v0.2.0-rc.1-linux-x86_64.tar.gz) | [SortSize-v0.2.0-rc.1-source-linux-x86_64.tar.gz](https://github.com/tazmen3/SortSize-OSS/releases/download/v0.2.0-rc.1/SortSize-v0.2.0-rc.1-source-linux-x86_64.tar.gz) |
| Windows 11 x64 | [SortSize-v0.2.0-rc.1-windows-x86_64.zip](https://github.com/tazmen3/SortSize-OSS/releases/download/v0.2.0-rc.1/SortSize-v0.2.0-rc.1-windows-x86_64.zip) | [SortSize-v0.2.0-rc.1-source-windows-x86_64.zip](https://github.com/tazmen3/SortSize-OSS/releases/download/v0.2.0-rc.1/SortSize-v0.2.0-rc.1-source-windows-x86_64.zip) |

**Windows:** install the latest supported official [Microsoft Visual C++ Redistributable x64 v14](https://aka.ms/vc14/vc_redist.x64.exe) if needed. Use a version at least as recent as the MSVC Build Tools used to build the application, following [Microsoft's guidance](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist). A clean Windows 11 VM successfully ran this candidate with runtime **14.44.35211.0**; this is an observed test configuration, not a proven universal minimum. Windows supplies UCRT and Win32 libraries. The ZIP does not include the Microsoft installer or any runtime DLL. Rust, Cargo and Visual Studio are not required to run SortSize. Launch `SortSize.exe`; use `sortsize-cli.exe` for command-line work. The executables are unsigned, and the public-download SmartScreen experience has not been qualified.

**Ubuntu:** the qualified environment is Ubuntu 24.04.5 with glibc 2.39, X11/Winit software rendering. An XWayland session requires XWayland; native Wayland is not qualified. Required system components include libgcc, Fontconfig/FreeType, system fonts and X11/XKB client libraries. A conservative package list is: `libc6 libgcc-s1 libfontconfig1 libfreetype6 fonts-dejavu-core libx11-6 libx11-xcb1 libxcb1 libxcursor1 libxrandr2 libxi6 libxfixes3 libxrender1 libxext6 libxkbcommon0 libxkbcommon-x11-0`. The folder picker additionally needs a session D-Bus, `xdg-desktop-portal` and a suitable backend, such as `xdg-desktop-portal-gtk`; manual path entry remains available. Launch `./sortsize-gui`; use `./sortsize-cli` in a terminal. No compiler, Rust or Cargo is required at runtime.

The main window needs **960×600 logical pixels of client area**, plus window decorations and the taskbar. Native Windows scaling was exercised at 100%, 125%, 150% and 200% on a 2560×1600 display. A 1280×800 screen does not provide enough working height at 125%: the minimum framed window is 797 physical pixels high, against 740 available in the earlier test environment. Choose a suitable display or lower scaling. About/Licenses requires at least 580×400 logical client pixels.

## Command-line examples

Linux:

```sh
./sortsize-cli --version
./sortsize-cli scan "/path/to/folder" --format json
./sortsize-cli scan "/path/to/folder" --scope entries --format csv --output inventory.csv
```

Windows PowerShell:

```powershell
.\sortsize-cli.exe --version
.\sortsize-cli.exe scan 'C:\Data' --format json
.\sortsize-cli.exe scan 'C:\Data' --scope entries --format tsv --output inventory.tsv
```

Check the detailed export's final `end` record and coverage before treating it as complete. Existing output files, including symbolic links, are not overwritten. Exit codes: 0 complete, 2 partial, 130 cancelled, 1 execution/output failure. Argument parsing can also return 2 before scanning.

## Validation and remaining limitations

The recorded native test campaigns passed 224 distinct Rust tests on Ubuntu and 226 on Windows, including the explicit NTFS ACL guard; overlapping suites were counted once. Python checks passed 17 exercised tests per platform, with one skipped. Corresponding sources built offline with fresh Cargo homes on both platforms. Clean-system qualification covered the executable bytes reused unchanged in these public archives, including a standard Windows user account. The public containers were subsequently repackaged to remove personal paths from historical source documentation and update corresponding-source hashes. Separate integrity/privacy checks cover this container revision; no new native GUI campaign is claimed for it. The final review rechecked archive hashes, contents, source provenance and proof manifests; it did not repeat the entire native test campaign. No new remote CI run is claimed.

- One root per scan. Accessible mounts may be traversed; there is no same-volume option or filesystem snapshot.
- GUI retention is limited to 50,000 nodes / 8 MiB of paths, independently of global scan coverage. Treemap uses retained logical sizes; Top 100 describes observed files.
- Cancellation is cooperative. An OS call or export finalization can delay completion without a fixed time bound.
- Detailed exports use temporary disk space and contain sensitive paths/metadata in plaintext. Forced termination or persistent cleanup failure can leave files behind. Import CSV names as text in spreadsheets to avoid interpreting original names as formulas.
- Windows preference saves can fail transiently under external contention; the application reports the failure and retains settings for the session.
- Native Wayland, IME composition, multiple monitors and full accessibility/screen-reader support are not certified. Physical removable media, quotas, UNC scenarios and a genuinely full Windows disk are not comprehensively qualified.
- No GUI export, detailed JSON, filters/grouping, multi-root sessions, groups/permission display, SQLite history, duplicate removal, cleanup operations, network/cloud connector or Android package is included.
- Optional Skia is not enabled in the shipped desktop configuration. Its historical generated-source provenance reservation remains disclosed in the supplied source materials. Qt/Skia builds are outside this qualification.

## License, source and integrity

SortSize's original code and the selected Slint licensing path use **GPL-3.0-only**, without warranty. Third-party code and resources retain their own licenses and notices. Keep `LICENSE`, `AUTHORS.txt`, `THIRD_PARTY_NOTICES.txt` and `licences/` with the application. Corresponding-source bundles include locked Cargo dependencies and offline build recipes; external compiler/SDK/system prerequisites are documented. Bit-for-bit reproducible binaries are not promised.

All four archives use frozen source commit **`448e92e572c0e986f7706d2373531e033196ed1d`**. The public documentation renditions remove personal paths from four historical files; executable code, build scripts, resources, Cargo archives and license texts are unchanged. PUBLIC_DISTRIBUTION.json in each archive records the original container hash and the documented changes. Later audit documentation does not redefine the product source. Provide effective access to the matching source with each binary; an inaccessible private repository does not satisfy that requirement. Automatically generated Git hosting source ZIPs do not replace these dependency-complete source bundles.

See the accompanying `SHA256SUMS` for the four exact archive digests and `RELEASE_CLARIFICATIONS.md` for dated statements retained inside the frozen archives. Verify the hashes before extraction (`sha256sum -c SHA256SUMS` on Linux; `Get-FileHash -Algorithm SHA256 <archive>` on Windows).

When reporting an issue, include the version, OS, display scaling and steps to reproduce. Review logs and exports before sharing them: they may contain private paths or owner IDs.
