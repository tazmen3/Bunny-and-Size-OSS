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
