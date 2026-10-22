# Testing SortSize v0.2.0-rc.1 on real Windows and Ubuntu systems

**Volunteer campaign / Campagne de test : [GitHub issue #1](https://github.com/tazmen3/SortSize-OSS/issues/1)**  
**Release / Téléchargements : [v0.2.0-rc.1](https://github.com/tazmen3/SortSize-OSS/releases/tag/v0.2.0-rc.1)**

We are seeking **3 Windows 11 x64 and 3 Ubuntu 24.04 x86_64 volunteers** using diverse **physical** machines. This **15–20-minute smoke test** checks behaviour outside the project's qualification VMs. You do not need programming experience. The GUI is currently **in French**. This is a GPL-3.0-only **evaluation prerelease**, not a stable version.

Nous cherchons **trois retours Windows 11 et trois retours Ubuntu 24.04**, idéalement sur de vrais PC. L'interface est actuellement en français. Les réponses « réussi / échec / non testé » sont bienvenues dans [l'issue #1](https://github.com/tazmen3/SortSize-OSS/issues/1).

## Download & safety

1. Download the **platform-specific binary archive** from the official release. Matching GPL corresponding-source archives are also provided but **are not required just to run the program**.
2. Verify the archive against the release's [SHA256SUMS](SHA256SUMS). On Ubuntu: `sha256sum SortSize-v0.2.0-rc.1-linux-x86_64.tar.gz`; on Windows PowerShell: `Get-FileHash .\SortSize-v0.2.0-rc.1-windows-x86_64.zip -Algorithm SHA256`. Compare the full SHA-256 with the matching line.
3. Extract the **complete archive** to a user-writable folder; keep the license and notices with the executables. No Rust, Cargo, Visual Studio or administrator rights should be necessary to run SortSize.
4. On Windows, start `SortSize.exe` (or `sortsize-cli.exe` for command-line testing). If a Microsoft VC++ runtime is missing, see the [official Microsoft x64 v14 redistributable](https://aka.ms/vc14/vc_redist.x64.exe) and the release notes. The Windows executables are **unsigned**: **if SmartScreen blocks the application, please report the block rather than weakening security protections**.
5. On Ubuntu, start `./sortsize-gui` (or `./sortsize-cli`). X11/Winit is the qualified configuration; other sessions are exploratory. See [release notes](RELEASE_NOTES.md) for X11, font and desktop-portal dependencies.
6. Create a **non-sensitive test folder**, for example `sortsize-rc-smoke`, with two subfolders, an empty folder, a few files of different sizes and a name containing accents and spaces, such as `été 2026.txt`. Avoid personal documents, whole home directories and privileged locations.

> SortSize scans in **read-only** mode. For testing, do **not** disable SmartScreen, antivirus, UAC, file permissions or operating-system protections. Never share unredacted logs or inventory exports. See the [known limitations](KNOWN_ISSUES.md).

## Test scenarios — mark PASS / FAIL / SKIP

| ID | Test | What to observe |
|---|---|---|
| T01 | Start GUI and CLI | Window opens; `--version` reports `0.2.0-rc.1`. |
| T02 | Choose and scan | Select test folder, click **Analyser**; scan completes, or clearly explains a partial result/error. |
| T03 | Tree navigation | Navigate into child directories, then **Parent** without triggering another scan. |
| T04 | Visualizations | Treemap responds to folder selection; Top 100 lists the largest observed files. |
| T05 | File details | Visible sizes and file/folder counts seem consistent with the small known fixture. Unknown allocated values are not misrepresented as zero. |
| T06 | Preferences | Sort columns; change column visibility and binary/decimal units; restart GUI and check saved **display** preferences. |
| T07 | Recovery | On a moderately sized **non-sensitive** test folder, try cancelling a scan and starting another. If no suitable folder, mark SKIP. |
| T08 | Summary CLI | Scan test path to JSON (examples below). Check exit status and presence of summary output. |
| T09 | Detailed CLI | Optionally export entries CSV/TSV to a **new** file; check the terminal `end` record and whether the result is complete/partial/cancelled. |
| T10 | Real display | Report resolution, scale, multi-monitor setup and whether window and dialogs remain accessible. |

**Ubuntu CLI example** (from the extracted binary folder):

```sh
./sortsize-cli --version
./sortsize-cli scan "/absolute/path/to/sortsize-rc-smoke" --format json
./sortsize-cli scan "/absolute/path/to/sortsize-rc-smoke" --scope entries --format csv --output "./sortsize-rc-entries.csv"
```

**Windows PowerShell example** (from the extracted binary folder):

```powershell
.\sortsize-cli.exe --version
.\sortsize-cli.exe scan 'C:\path\to\sortsize-rc-smoke' --format json
.\sortsize-cli.exe scan 'C:\path\to\sortsize-rc-smoke' --scope entries --format csv --output '.\sortsize-rc-entries.csv'
```

For T09, use an output path that does **not** already exist, and do not place the export inside the folder you are scanning. Detailed exports can contain sensitive paths and metadata; do not attach them unless completely anonymized.

### Useful differences from the qualified VMs

Physical laptop, high-DPI display, 125% or 150% Windows scaling, NTFS/ext4/Btrfs, non-admin Windows account, XWayland, multi-monitor, removable storage, non-English OS locale. State whether the scenario is within the qualified scope or **exploratory**.

**Known Windows layout limitation:** the main GUI needs **960×600 logical *client* pixels plus decorations and taskbar**. A previously tested 1280×800 / 125% setup left only 740 physical pixels of usable height versus a minimum framed height of 797, so the 57-pixel overrun is **known**, not a new regression. Still report exact resolution, scale and behaviour.

## Reporting

**Short report:** reply to [issue #1](https://github.com/tazmen3/SortSize-OSS/issues/1) with platform and version, physical/VM, filesystem, display resolution/scaling (and X11/Wayland session on Ubuntu), T01–T10 statuses, and any notable behaviour.

**Reproducible bug:** [create a separate GitHub issue](https://github.com/tazmen3/SortSize-OSS/issues/new) with steps, expected and actual result, precise version and the environment; refer to #1 so findings can be triaged. Mention if the test was outside the known qualified scope.

**Privacy:** command lines, screenshots, logs and exports can expose file paths, usernames, Windows SIDs/Linux UIDs, filenames and metadata. **Remove or replace personal information before posting.** Do not provide secrets or private directories as sample fixtures.

We will distinguish real failures from known limits, and release-blocking issues from improvements for a later version. Thanks for testing! / Merci pour votre aide !
