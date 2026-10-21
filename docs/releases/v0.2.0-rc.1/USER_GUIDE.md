# SortSize 0.2.0-rc.1 — user instructions

Download and extract the complete binary archive for your platform. Keep LICENSE,
AUTHORS.txt, THIRD_PARTY_NOTICES.txt and licences/ beside the executables.
Keep access to the matching corresponding-source bundle from the same release.

On Windows 11 x64, install the latest supported official
[Microsoft Visual C++ Redistributable x64 v14](https://aka.ms/vc14/vc_redist.x64.exe)
if needed. 14.44.35211.0 was the clean-VM test configuration; it is not a
universal minimum. See [Microsoft guidance](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist).
Rust, Cargo and Visual Studio are not needed to run the binaries.
Launch SortSize.exe; use sortsize-cli.exe for command-line work.

On Ubuntu 24.04 x86_64, launch ./sortsize-gui. Fontconfig/FreeType, system fonts,
X11/XKB libraries and a session D-Bus/desktop portal are required; see the release
notes for the conservative package list. Native Wayland is not qualified.

## Graphical interface

The GUI currently uses French labels. Choose **Parcourir…** or enter a folder,
then click **Analyser**. Choosing a source alone does not start a scan.
Only one root is scanned at a time. **Annuler** requests cooperative cancellation.
Accessible mounts may be traversed, and a scan is not a filesystem snapshot.

The folder tree, table and logical-size Treemap share navigation. **Parent**
moves up without starting another scan. **Top 100** lists observed files globally;
an item outside the retained GUI inventory may not be revealable in the tree.
GUI retention is limited to 50,000 nodes / 8 MiB of paths, separately from scan
coverage. Pages contain at most 2,000 table rows.

**Colonnes et affichage** controls columns and binary/decimal units. Name remains
visible. **Réinitialiser** resets these preferences; units and column visibility
are saved locally. Owner collection and owner-name resolution are separate,
optional choices. NSS/AD name resolution may use the network after explicit
activation. Timestamps are UTC; unknown values are not zero.

**À propos / Licences** provides notices and the complete GPLv3 offline.
The main window requires 960×600 logical client pixels plus frame/taskbar room.
Native 100/125/150/200% Windows DPI was exercised on 2560×1600. At 125%, the
minimum framed height is 797 physical pixels: it exceeds the earlier 740-pixel
work area by 57 pixels. A smaller screen is not certified by those tests.

## Command line

```sh
./sortsize-cli --version
./sortsize-cli scan "/path/to/folder" --format json
./sortsize-cli scan "/path/to/folder" --scope entries --format csv --output inventory.csv
```

In Windows PowerShell, replace ./sortsize-cli with .\sortsize-cli.exe and use
a native folder path, for example 'C:\Data'.
JSON v2 and 40-column CSV/TSV describe summaries. Detailed entries use 52-column
CSV/TSV with a final end record; detailed JSON and GUI export are not included.
Check the end record and coverage before treating an export as complete.

Existing output files, including symbolic links, are not overwritten. Codes:
0 complete, 2 partial, 130 cancelled, 1 execution/output failure. Argument parsing
can also return 2 before scanning. Temporary/export files contain sensitive
paths and metadata in plaintext; forced termination can leave residues. Import
CSV names as text in spreadsheets rather than interpreting them as formulas.

See KNOWN_ISSUES.md and RELEASE_CLARIFICATIONS.md for the remaining limitations.
The software is GPL-3.0-only, without warranty; third-party licenses are retained.
