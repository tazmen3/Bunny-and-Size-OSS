# SortSize

Analyse locale du stockage en lecture seule, avec interface graphique Slint et
CLI Rust. La candidate **0.2.0-rc.1** est préparée pour Ubuntu 24.04 x86_64 et
Windows 11 x64 ; elle n'est pas publiée par cette préparation.

Read-only local storage analysis with a Rust CLI and Slint GUI. Release candidate
**0.2.0-rc.1** is prepared for Ubuntu 24.04 x86_64 and Windows 11 x64.
This branch prepares publication; it does not create a tag or release.

- [Release notes / Présentation française](docs/releases/v0.2.0-rc.1/RELEASE_NOTES.md)
- [User instructions](docs/releases/v0.2.0-rc.1/USER_GUIDE.md)
- [Prepared downloads and matching sources](docs/releases/v0.2.0-rc.1/DOWNLOADS.md)
- [Known limitations](docs/releases/v0.2.0-rc.1/KNOWN_ISSUES.md)
- [Qualification clarifications](docs/releases/v0.2.0-rc.1/RELEASE_CLARIFICATIONS.md)
- [Archive checksums](docs/releases/v0.2.0-rc.1/SHA256SUMS)
- [Public distribution provenance](docs/releases/v0.2.0-rc.1/PROVENANCE.json)

The GUI is currently in French. Treemap, Top 100, configurable columns, UTC dates,
optional allocations/owners and CLI summary/detailed exports are included.
No cleanup operation, GUI export, SQLite history, cloud connector or Android
package is delivered. See the release notes for scope and remaining reservations.

Windows: install the latest supported official
[Microsoft Visual C++ Redistributable x64 v14](https://aka.ms/vc14/vc_redist.x64.exe)
if needed. Version 14.44.35211.0 was tested; it is not a universal minimum.
Use a runtime at least as recent as the MSVC Build Tools, following
[Microsoft guidance](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist).
No Microsoft DLL or installer is bundled. Launch SortSize.exe or sortsize-cli.exe.
Ubuntu: launch ./sortsize-gui or ./sortsize-cli; system X11/Fontconfig/portal
prerequisites are described in the release notes. No Rust/compiler is needed
to run the binaries.

**GPL-3.0-only**, without warranty. Keep LICENSE, AUTHORS.txt, third-party notices
and license texts. Both dependency-complete corresponding-source bundles must be
served with the binaries; GitHub's automatic source downloads do not replace them.

This public repository has its own history. The v0.2 product snapshot matches
frozen source 448e92e572c0e986f7706d2373531e033196ed1d, but that private-origin
commit and its ancestors are not imported. The future public tag will point to
a commit in this public history. Public source containers remove personal paths
from four historical documentation files, with original/public hashes recorded.
Executable bytes, build inputs, Cargo sources and license texts are unchanged.

The previous v0.1 release remains historical; its documentation is not the current
v0.2 qualification. No private validation corpus or development workflow is added.
