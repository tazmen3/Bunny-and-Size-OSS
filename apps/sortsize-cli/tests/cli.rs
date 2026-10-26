use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sortsize"))
}

#[test]
fn json_stdout_is_machine_readable_and_input_is_unchanged() {
    let fixture = TempDir::new().unwrap();
    let file = fixture.path().join("original");
    fs::write(&file, b"12345").unwrap();
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["files"], 1);
    assert_eq!(json["directories"], 1);
    assert_eq!(json["logical_bytes"], 5);
    assert_eq!(json["coverage"], "complete");
    assert_eq!(fs::read(file).unwrap(), b"12345");
}

#[test]
fn output_is_created_after_scan_and_existing_files_are_preserved() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("input"), b"123").unwrap();
    let report_path = fixture.path().join("report.json");
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "json", "--output"])
        .arg(&report_path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let original = fs::read(&report_path).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(json["files"], 1); // Does not scan its own freshly created output.
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .arg("--output")
        .arg(&report_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read(&report_path).unwrap(), original);
}

#[test]
fn invalid_roots_are_fatal_and_depth_limited_scans_exit_two() {
    let fixture = TempDir::new().unwrap();
    let missing = binary()
        .arg("scan")
        .arg(fixture.path().join("absent"))
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    fs::create_dir(fixture.path().join("child")).unwrap();
    let partial = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--max-depth", "0", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(partial.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&partial.stdout).unwrap();
    assert_eq!(json["coverage"], "partial");
}

#[test]
fn explicit_no_follow_and_exclusions_work_in_the_cli() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join(".git/objects")).unwrap();
    fs::write(fixture.path().join(".git/objects/object"), b"123").unwrap();
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args([
            "--format",
            "json",
            "--no-follow-symlinks",
            "--exclude",
            "**/.git/**",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["files"], 0);
    assert_eq!(json["excluded_entries"], 1);
}

#[cfg(unix)]
#[test]
fn text_output_escapes_control_characters_in_native_names() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("line\nbreak\u{1b}[31m");
    fs::create_dir(&root).unwrap();
    let output = binary().arg("scan").arg(&root).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("line\\nbreak\\u{1b}[31m"));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn traversal_limits_export_a_partial_report_and_exit_two() {
    let fixture = TempDir::new().unwrap();
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--max-pending-path-bytes", "1", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["coverage"], "partial");
    assert_eq!(json["resource_limited_directories"], 1);
    assert_eq!(json["errors_count"], 0);
    let text = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--max-pending-path-bytes", "1"])
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(2));
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .contains("au moins 0 octets")
    );
    let invalid = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--max-pending-directories", "0"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
}

#[test]
fn concurrent_cli_exports_effective_limits_and_rejects_invalid_workers() {
    let fixture = TempDir::new().unwrap();
    for n in 0..12 {
        let child = fixture.path().join(format!("{n}"));
        fs::create_dir(&child).unwrap();
        fs::write(child.join("data"), b"xyz").unwrap();
    }
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args([
            "--workers",
            "4",
            "--max-buffered-observations",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["logical_bytes"], 36);
    assert_eq!(json["files"], 12);
    assert_eq!(json["concurrency"]["workers"], 4);
    assert_eq!(json["traversal_stats"]["worker_threads"], 4);
    assert_eq!(json["traversal_stats"]["peak_buffered_observations"], 1);
    let invalid = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--workers", "0"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
}

#[test]
fn json_v2_preserves_the_entire_legacy_shape_and_zero_vs_null_values() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("empty"), []).unwrap();
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(actual["started_at_unix_ms"].is_u64());
    assert!(actual["duration_ms"].is_u64());
    actual["started_at_unix_ms"] = 0.into();
    actual["duration_ms"] = 0.into();
    let root = fs::canonicalize(fixture.path()).unwrap();
    assert_eq!(
        actual,
        serde_json::json!({
            "schema_version":2, "provider":"local", "root":{"display":root.to_str().unwrap()},
            "started_at_unix_ms":0, "duration_ms":0, "coverage":"complete",
            "files":1, "directories":1, "logical_bytes":0, "logical_size_basis":"per_file_path",
            "allocated_bytes":null, "potentially_reclaimable_bytes":null,
            "symlinks_skipped":0, "special_entries_skipped":0, "excluded_entries":0,
            "depth_limited_directories":0, "resource_limited_directories":0,
            "traversal_limits":{"max_pending_directories":65536,"max_pending_path_bytes":8388608},
            "traversal_stats":{
                "peak_pending_directories":1, "peak_pending_path_bytes":root.as_os_str().as_encoded_bytes().len(),
                "peak_open_directories":1, "worker_threads":1,
                "peak_buffered_observations":0, "peak_buffered_path_bytes":0
            },
            "concurrency":{"workers":1,"max_buffered_observations":1024,"max_buffered_path_bytes":1048576},
            "hardlinked_file_paths":if cfg!(unix) {Some(0u64)} else {None},
            "errors_count":0, "errors_omitted":0, "errors":[],
            "follow_symlinks":false, "exclusions":[], "max_depth":null,
            "mount_policy":"cross_accessible_mounts"
        })
    );
}

#[test]
fn every_machine_format_preserves_exit_and_output_contracts() {
    for format in ["json", "csv", "tsv"] {
        let fixture = TempDir::new().unwrap();
        fs::write(fixture.path().join("empty"), []).unwrap();
        fs::create_dir(fixture.path().join("child")).unwrap();
        let complete = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format])
            .output()
            .unwrap();
        assert_eq!(complete.status.code(), Some(0));
        assert!(complete.stderr.is_empty());
        assert!(!complete.stdout.is_empty());
        let partial = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format, "--max-depth", "0"])
            .output()
            .unwrap();
        assert_eq!(partial.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&partial.stdout).contains("partial"));
        let missing = binary()
            .arg("scan")
            .arg(fixture.path().join("missing"))
            .args(["--format", format])
            .output()
            .unwrap();
        assert_eq!(missing.status.code(), Some(1));
        assert!(missing.stdout.is_empty());
        assert!(!missing.stderr.is_empty());
        let target = fixture.path().join(format!("report.{format}"));
        let written = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format, "--output"])
            .arg(&target)
            .output()
            .unwrap();
        assert!(written.status.success());
        assert!(written.stdout.is_empty());
        let original = fs::read(&target).unwrap();
        let refused = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format, "--output"])
            .arg(&target)
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(1));
        assert!(refused.stdout.is_empty());
        assert_eq!(fs::read(&target).unwrap(), original);
    }
}

#[test]
fn progress_is_configurable_and_quiet_keeps_fatal_diagnostics() {
    let fixture = TempDir::new().unwrap();
    for format in ["json", "csv", "tsv"] {
        for args in [
            vec!["--progress", "never"],
            vec!["--progress", "always", "--quiet"],
        ] {
            let output = binary()
                .arg("scan")
                .arg(fixture.path())
                .args(["--format", format])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
        }
        let output = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format, "--progress", "always"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Analyse démarrée"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Analyse démarrée"));
        let output = binary()
            .arg("scan")
            .arg(fixture.path().join("missing"))
            .args(["--format", format, "--quiet"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(!output.stderr.is_empty());
        assert!(output.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn symlink_destinations_including_dangling_links_are_never_replaced() {
    use std::os::unix::fs::symlink;
    let fixture = TempDir::new().unwrap();
    let target = fixture.path().join("target");
    fs::write(&target, b"preserved").unwrap();
    for format in ["json", "csv", "tsv"] {
        for destination in [&target, &fixture.path().join("missing")] {
            let link = fixture.path().join(format!("link-{format}"));
            symlink(destination, &link).unwrap();
            let output = binary()
                .arg("scan")
                .arg(fixture.path())
                .args(["--format", format, "--output"])
                .arg(&link)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert_eq!(fs::read_link(&link).unwrap(), *destination);
            assert_eq!(fs::read(&target).unwrap(), b"preserved");
            assert!(!fixture.path().join("missing").exists());
            fs::remove_file(link).unwrap();
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn native_non_unicode_roots_and_files_work_in_every_machine_format() {
    #[cfg(unix)]
    let (name, encoding) = {
        use std::os::unix::ffi::OsStringExt;
        (
            std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec()),
            "unix_bytes",
        )
    };
    #[cfg(windows)]
    let (name, encoding) = {
        use std::os::windows::ffi::OsStringExt;
        (
            std::ffi::OsString::from_wide(&[110, 111, 110, 45, 0xd800]),
            "windows_utf16",
        )
    };
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join(&name);
    fs::create_dir(&root).unwrap();
    fs::write(root.join(name), b"abc").unwrap();
    for format in ["json", "csv", "tsv"] {
        let output = binary()
            .arg("scan")
            .arg(&root)
            .args(["--format", format])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8(output.stdout).unwrap().contains(encoding));
    }
}

#[test]
fn entries_is_explicit_and_preserves_summary_default() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("f"), b"abc").unwrap();
    for format in ["csv", "tsv"] {
        let default = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format])
            .output()
            .unwrap();
        let summary = String::from_utf8(default.stdout).unwrap();
        assert!(summary.contains("summary"));
        assert!(!summary.contains("record_type"));
        let detailed = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format, "--scope", "entries"])
            .output()
            .unwrap();
        assert!(
            detailed.status.success(),
            "{}",
            String::from_utf8_lossy(&detailed.stderr)
        );
        let data = String::from_utf8(detailed.stdout).unwrap();
        assert!(data.contains("record_type"));
        assert!(data.contains("entry"));
        assert!(data.contains(if format == "csv" { ",end," } else { "\tend\t" }));
    }
    for format in ["text", "json"] {
        let invalid = binary()
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", format, "--scope", "entries"])
            .output()
            .unwrap();
        assert_eq!(invalid.status.code(), Some(1));
        assert!(invalid.stdout.is_empty());
    }
    let invalid = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--sort-memory-bytes", "1"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
}

#[test]
fn entries_publish_after_scan_and_partial_has_a_final_marker() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("input"), b"abc").unwrap();
    let destination = fixture.path().join("inventory.tsv");
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "tsv", "--scope", "entries", "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let data = fs::read(&destination).unwrap();
    let text = String::from_utf8(data.clone()).unwrap();
    assert!(!text.contains("\tinventory.tsv\t"));
    assert!(text.contains("\tend\t"));
    let existing = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "tsv", "--scope", "entries", "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert_eq!(existing.status.code(), Some(1));
    assert_eq!(fs::read(&destination).unwrap(), data);
    let partial = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "csv", "--scope", "entries", "--max-depth", "0"])
        .output()
        .unwrap();
    assert_eq!(partial.status.code(), Some(2));
    let data = String::from_utf8(partial.stdout).unwrap();
    assert!(data.contains("lower_bound"));
    assert!(data.contains(",end,"));
    assert!(data.contains(",partial,"));
    let missing = binary()
        .arg("scan")
        .arg(fixture.path().join("missing"))
        .args(["--scope", "entries", "--format", "csv", "--output"])
        .arg(fixture.path().join("missing.csv"))
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(!fixture.path().join("missing.csv").exists());
}

#[cfg(unix)]
#[test]
fn entries_refuse_dangling_symlinks_and_missing_output_parents() {
    use std::os::unix::fs::symlink;
    let fixture = TempDir::new().unwrap();
    let link = fixture.path().join("link");
    symlink("absent", &link).unwrap();
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "csv", "--scope", "entries", "--output"])
        .arg(&link)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fs::read_link(&link).unwrap(),
        std::path::Path::new("absent")
    );
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "csv", "--scope", "entries", "--output"])
        .arg(fixture.path().join("absent/out.csv"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!fixture.path().join("absent").exists());
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn entries_native_windows_utf16_sid_and_private_spool_work_together() {
    use std::os::windows::ffi::OsStringExt;
    let fixture = TempDir::new().unwrap();
    let name = std::ffi::OsString::from_wide(&[0xd800]);
    fs::write(fixture.path().join(name), b"xyz").unwrap();
    let output = binary()
        .arg("scan")
        .arg(fixture.path())
        .args(["--format", "tsv", "--scope", "entries", "--workers", "8"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = String::from_utf8(output.stdout).unwrap();
    assert!(data.contains("windows_utf16"));
    assert!(data.contains("[55296]"));
    assert!(data.contains("windows_sid_hex"));
    assert!(data.contains("\tend\t"));
}

#[test]
fn new_command_and_legacy_alias_share_identity_errors_and_scan_contract() {
    let programs = [
        env!("CARGO_BIN_EXE_bunny-and-size-cli"),
        env!("CARGO_BIN_EXE_sortsize"),
    ];
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("données.txt"), b"preserve").unwrap();
    let mut reports = Vec::new();
    for program in programs {
        let version = Command::new(program).arg("--version").output().unwrap();
        assert!(version.status.success());
        assert!(
            String::from_utf8_lossy(&version.stdout)
                .contains(&format!("Bunny and Size {}", env!("CARGO_PKG_VERSION")))
        );
        let help = Command::new(program).arg("--help").output().unwrap();
        assert!(String::from_utf8_lossy(&help.stdout).contains("Bunny and Size"));
        let invalid = Command::new(program)
            .args(["scan", "--workers", "0"])
            .output()
            .unwrap();
        assert!(!invalid.status.success());
        let output = Command::new(program)
            .arg("scan")
            .arg(fixture.path())
            .args(["--format", "json", "--quiet"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        reports.push(report);
    }
    for key in [
        "schema_version",
        "files",
        "directories",
        "logical_bytes",
        "coverage",
        "errors",
    ] {
        assert_eq!(reports[0][key], reports[1][key], "{key}");
    }
    assert_eq!(reports[0]["logical_bytes"], 8);
}
