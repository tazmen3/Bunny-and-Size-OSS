#![cfg(feature = "inventory-report")]
use sortsize_core::{
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::Store,
    scheduler::{Session, State},
};
use std::{
    process::Command,
    time::{Duration, Instant},
};
#[test]
fn explicit_duplicate_cli_streams_members_and_marks_transport_quota() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    std::fs::create_dir(&root).unwrap();
    for i in 0..40 {
        std::fs::write(root.join(format!("日本語-long-empty-file-{i:04}")), b"").unwrap();
    }
    let c = AcquisitionContext {
        inventory: InventoryId(920),
        source: SourceId(920),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let db = t.path().join("db");
    let mut s = Session::new(
        Store::create(&db, c.inventory, c.source, Durability::Retained).unwrap(),
        c,
        Guard::open(&root, Policy::default()).unwrap(),
    )
    .unwrap();
    let start = Instant::now();
    while s.tick().unwrap().state != State::Idle {
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    while s.finalize_step().unwrap().is_none() {}
    drop(s);
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bunny-and-size-cli"));
        cmd.arg("inventory")
            .arg(&db)
            .arg("--root")
            .arg(&root)
            .arg("--temporary-directory")
            .arg(t.path())
            .args([
                "--operation",
                "duplicates",
                "--query-disk-bytes",
                "1048576",
                "--groups-disk-bytes",
                "1048576",
            ]);
        cmd
    };
    let out = command().output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 43);
    assert_eq!(lines.first().unwrap()["content_requested"], true);
    assert_eq!(lines.last().unwrap()["complete"], true);
    let out = command().args(["--output-bytes", "4096"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.len() <= 4096);
    let trailer: serde_json::Value = serde_json::from_str(
        std::str::from_utf8(&out.stdout)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(trailer["complete"], false);
    assert_eq!(trailer["transport_complete"], false);
}
#[test]
fn frozen_report_cli_is_source_free_machine_stable_across_seven_locales_and_never_overwrites() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("日本語.txt"), [0; 8]).unwrap();
    std::fs::write(root.join("report.CSV"), [0; 13]).unwrap();
    let store_path = t.path().join("inventory");
    let c = AcquisitionContext {
        inventory: InventoryId(919),
        source: SourceId(919),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let s = Store::create(&store_path, c.inventory, c.source, Durability::Retained).unwrap();
    let mut session = Session::new(s, c, Guard::open(&root, Policy::default()).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while session.tick().unwrap().state != State::Idle {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    drop(session);
    std::fs::rename(&root, t.path().join("offline")).unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bunny-and-size-cli"));
        cmd.arg("inventory")
            .arg(&store_path)
            .arg("--root")
            .arg(&root)
            .arg("--temporary-directory")
            .arg(t.path())
            .arg("--query-disk-bytes")
            .arg("1048576")
            .arg("--groups-disk-bytes")
            .arg("1048576");
        cmd
    };
    let mut reference = None;
    for locale in [
        "en_US.UTF-8",
        "fr_FR.UTF-8",
        "de_DE.UTF-8",
        "es_ES.UTF-8",
        "pt_PT.UTF-8",
        "it_IT.UTF-8",
        "nl_NL.UTF-8",
    ] {
        let out = command()
            .env("LANG", locale)
            .env("LC_ALL", locale)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["totals"]["known_bytes"], "21");
        assert_eq!(value["totals"]["files"], "2");
        assert_eq!(value["generation"], "1");
        assert_eq!(value["complete"], true);
        if let Some(previous) = &reference {
            assert_eq!(previous, &out.stdout);
        } else {
            reference = Some(out.stdout);
        }
    }
    let dest = t.path().join("export.jsonl");
    let out = command()
        .arg("--operation")
        .arg("export")
        .arg("--output")
        .arg(&dest)
        .arg("--output-bytes")
        .arg("1048576")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let before = std::fs::read(&dest).unwrap();
    let out = command()
        .arg("--operation")
        .arg("export")
        .arg("--output")
        .arg(&dest)
        .arg("--output-bytes")
        .arg("1048576")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read(&dest).unwrap(), before);
    assert!(!root.exists());
}
