#![cfg(feature = "inventory-report")]
use std::{path::Path, process::Command};
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bunny-and-size-cli"))
        .args(args)
        .output()
        .unwrap()
}
fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}
fn good(args: &[&str]) -> Vec<serde_json::Value> {
    let r = run(args);
    assert!(
        r.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&r.stderr)
    );
    String::from_utf8(r.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
#[test]
fn cache_and_diff_cli_are_stable_offline_and_keep_separate_measures() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("f"), [0; 240]).unwrap();
    let state = t.path().join("state");
    let scratch = t.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let old = good(&[
        "cache",
        "measure",
        path(&source),
        "--directory",
        path(&state),
    ]);
    assert_eq!(old[0]["logical_bytes"], "240");
    assert_eq!(old[0]["source_acquired"], true);
    let store = std::fs::read_dir(&state)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("inv-"))
        })
        .unwrap()
        .path();
    let parent_mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
    std::fs::write(source.join("f"), [0; 160]).unwrap();
    assert_eq!(
        parent_mtime,
        std::fs::metadata(&source).unwrap().modified().unwrap()
    );
    let current = good(&["cache", "verify", path(&store)]);
    assert_eq!(current[0]["logical_bytes"], "160");
    assert_eq!(current[0]["generation"], "2");
    assert_eq!(current[0]["source_acquired"], true);
    let args = [
        "diff",
        path(&store),
        path(&store),
        "--old-generation",
        "1",
        "--new-generation",
        "2",
        "--temporary-directory",
        path(&scratch),
    ];
    let first = run(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run(&args);
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
    let data: Vec<serde_json::Value> = String::from_utf8(first.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let summary = &data.last().unwrap()["summary"];
    assert_eq!(summary["old_logical"], 240);
    assert_eq!(summary["new_logical"], 160);
    if summary["compatible"] == true {
        assert_eq!(summary["comparable_delta"], "-80");
    } else {
        assert!(summary["comparable_delta"].is_null());
    }
    let offline = t.path().join("offline");
    std::fs::rename(&source, &offline).unwrap();
    assert_eq!(
        good(&["cache", "show", path(&store)])[0]["logical_bytes"],
        "240"
    );
    assert_eq!(
        good(&["cache", "show", path(&store), "--generation", "2"])[0]["logical_bytes"],
        "160"
    );
    assert_eq!(
        good(&["cache", "show", path(&store)])[0]["source_acquired"],
        false
    );
    let refusal = run(&["cache", "forget", path(&store)]);
    assert!(!refusal.status.success());
    assert!(store.join("CURRENT.json").is_file());
    for g in [1, 2] {
        assert!(
            store
                .join(format!("generation-{g:016x}/inventory.db"))
                .is_file()
        );
    }
    good(&["cache", "forget", path(&store), "--confirm"]);
    assert!(!store.join("CURRENT.json").exists());
    for g in [1, 2] {
        assert!(
            !store
                .join(format!("generation-{g:016x}/inventory.db"))
                .exists()
        );
    }
    assert_eq!(std::fs::metadata(offline.join("f")).unwrap().len(), 160);
}
#[test]
fn new_diff_export_never_overwrites_existing_output() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let state = t.path().join("state");
    good(&[
        "cache",
        "measure",
        path(&source),
        "--directory",
        path(&state),
    ]);
    let store = std::fs::read_dir(&state)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("inv-"))
        })
        .unwrap()
        .path();
    let output = t.path().join("keep.jsonl");
    std::fs::write(&output, b"keep").unwrap();
    let scratch = t.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let r = run(&[
        "diff",
        path(&store),
        path(&store),
        "--temporary-directory",
        path(&scratch),
        "--output",
        path(&output),
    ]);
    assert!(!r.status.success());
    assert_eq!(std::fs::read(output).unwrap(), b"keep");
}

#[test]
fn cache_inside_source_is_rejected_before_creating_any_inventory() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let destination = source.join("state");
    let result = run(&[
        "cache",
        "measure",
        path(&source),
        "--directory",
        path(&destination),
    ]);
    assert!(!result.status.success());
    assert!(!destination.exists());
}
