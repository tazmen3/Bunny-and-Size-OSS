use std::{fs, process::Command};
use tempfile::TempDir;
fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bunny-and-size-cli"))
}
#[test]
fn analysis_scope_statistics_and_no_clobber() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("été.txt"), b"hello").unwrap();
    fs::write(fixture.path().join("empty.txt"), b"").unwrap();
    fs::write(fixture.path().join("ignored.bin"), b"123").unwrap();
    let output = binary()
        .arg("analyze")
        .arg(fixture.path())
        .args(["--extension", "txt", "--kind", "file", "--max-results", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["search"]["matching_entries"], 2);
    assert_eq!(doc["search"]["statistics"]["totals"]["logical_bytes"], 5);
    assert_eq!(doc["entries"].as_array().unwrap().len(), 1);
    let report = fixture.path().join("report.tsv");
    assert!(
        binary()
            .arg("analyze")
            .arg(fixture.path())
            .args(["--format", "tsv", "--output"])
            .arg(&report)
            .status()
            .unwrap()
            .success()
    );
    let saved = fs::read(&report).unwrap();
    assert!(String::from_utf8_lossy(&saved).contains("\"end\""));
    assert!(
        !binary()
            .arg("analyze")
            .arg(fixture.path())
            .arg("--output")
            .arg(&report)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(saved, fs::read(&report).unwrap());
    assert_eq!(fs::read(fixture.path().join("été.txt")).unwrap(), b"hello");
}
#[test]
fn analysis_rejects_invalid_query_before_output_creation() {
    let f = TempDir::new().unwrap();
    let dest = f.path().join("absent");
    let o = binary()
        .arg("analyze")
        .arg(f.path())
        .args(["--min-bytes", "2", "--max-bytes", "1", "--output"])
        .arg(&dest)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(!dest.exists());
}
#[test]
fn explicit_duplicates_exports_native_groups_and_preserves_sources() {
    let f = TempDir::new().unwrap();
    for name in ["a", "b"] {
        fs::write(f.path().join(name), b"identical").unwrap();
    }
    let output = binary()
        .arg("analyze")
        .arg(f.path())
        .arg("--duplicates")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["duplicates"]["groups"].as_array().unwrap().len(), 1);
    assert_eq!(doc["duplicates"]["complete"], true);
    assert!(doc["duplicates"]["potentially_reclaimable_bytes"].is_null());
    let limited = binary()
        .arg("analyze")
        .arg(f.path())
        .args(["--duplicates", "--max-read-bytes", "1"])
        .output()
        .unwrap();
    assert_eq!(limited.status.code(), Some(2));
    let doc: serde_json::Value = serde_json::from_slice(&limited.stdout).unwrap();
    assert_eq!(doc["duplicates"]["budget_limited"], true);
    assert_eq!(fs::read(f.path().join("a")).unwrap(), b"identical");
}

#[test]
fn duplicate_read_budget_is_rejected_before_source_or_output_access() {
    let f = TempDir::new().unwrap();
    let output = binary()
        .arg("analyze")
        .arg(f.path().join("missing-source"))
        .args(["--duplicates", "--max-read-bytes", "0", "--output"])
        .arg(f.path().join("absent.json"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Content read budget"));
    assert_eq!(fs::read_dir(f.path()).unwrap().count(), 0);
}

#[test]
fn typed_duplicate_errors_do_not_change_machine_fields() {
    let f = TempDir::new().unwrap();
    for name in ["a", "b"] {
        fs::write(f.path().join(name), b"same").unwrap();
    }
    let output = binary()
        .arg("analyze")
        .arg(f.path())
        .arg("--duplicates")
        .output()
        .unwrap();
    assert!(output.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["analysis_schema_version"], 1);
    let keys = doc["duplicates"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "budget_limited",
            "bytes_read",
            "cancelled",
            "complete",
            "errors",
            "errors_count",
            "files_hashed",
            "groups",
            "hardlink_aliases",
            "potentially_reclaimable_bytes",
            "scope",
            "selected_entries",
            "selected_entries_omitted"
        ]
    );
}
