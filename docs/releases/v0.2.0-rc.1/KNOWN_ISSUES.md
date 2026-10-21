# SortSize 0.2.0-rc.1 — qualification and known limitations


The recorded native test campaigns passed 224 distinct Rust tests on Ubuntu and 226 on Windows, including the explicit NTFS ACL guard; overlapping suites were counted once. Python checks passed 17 exercised tests per platform, with one skipped. Corresponding sources built offline with fresh Cargo homes on both platforms. Clean-system qualification covered the executable bytes reused unchanged in these public archives, including a standard Windows user account. The public containers were subsequently repackaged to remove personal paths from historical source documentation and update corresponding-source hashes. Separate integrity/privacy checks cover this container revision; no new native GUI campaign is claimed for it. The final review rechecked archive hashes, contents, source provenance and proof manifests; it did not repeat the entire native test campaign. No new remote CI run is claimed.

- One root per scan. Accessible mounts may be traversed; there is no same-volume option or filesystem snapshot.
- GUI retention is limited to 50,000 nodes / 8 MiB of paths, independently of global scan coverage. Treemap uses retained logical sizes; Top 100 describes observed files.
- Cancellation is cooperative. An OS call or export finalization can delay completion without a fixed time bound.
- Detailed exports use temporary disk space and contain sensitive paths/metadata in plaintext. Forced termination or persistent cleanup failure can leave files behind. Import CSV names as text in spreadsheets to avoid interpreting original names as formulas.
- Windows preference saves can fail transiently under external contention; the application reports the failure and retains settings for the session.
- Native Wayland, IME composition, multiple monitors and full accessibility/screen-reader support are not certified. Physical removable media, quotas, UNC scenarios and a genuinely full Windows disk are not comprehensively qualified.
- No GUI export, detailed JSON, filters/grouping, multi-root sessions, groups/permission display, SQLite history, duplicate removal, cleanup operations, network/cloud connector or Android package is included.
- Optional Skia is not enabled in the shipped desktop configuration. Its historical generated-source provenance reservation remains disclosed in the supplied source materials. Qt/Skia builds are outside this qualification.
