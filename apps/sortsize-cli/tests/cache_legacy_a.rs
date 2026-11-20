#![cfg(feature = "inventory-report")]
//! Legacy format consultation and explicit family adoption through the real CLI.
use sortsize_core::{
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{Store, catalogue, generation_a::Repository, lifecycle},
    scheduler::{Session, State},
};
use std::{fs, path::Path, process::Command, time::Duration};
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
    let output = run(args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn legacy(path: &Path, root: &Path) {
    let c = AcquisitionContext {
        inventory: InventoryId(5511),
        source: catalogue::source_hint(root),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let store = Store::create(path, c.inventory, c.source, Durability::Retained).unwrap();
    let guard = Guard::open(
        root,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut session = Session::new(store, c, guard).unwrap();
    session
        .store_mut()
        .record_acquisition(c, root, 100)
        .unwrap();
    session.store_mut().set_retention(true, 30).unwrap();
    let mut finished = false;
    for _ in 0..10000 {
        let status = session.tick().unwrap();
        assert_ne!(status.state, State::StoreError);
        if status.state == State::Idle
            && status.in_flight == 0
            && session.finalize_step().unwrap().is_some()
        {
            finished = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(finished, "legacy fixture acquisition did not finish");
    session
        .store_mut()
        .record_measurement(c.generation, root, 100, 200)
        .unwrap();
}
#[test]
fn legacy_reads_stay_local_and_explicit_verify_adopts_a_qualified_independent_family() {
    let t = tempfile::tempdir().unwrap();
    let state = t.path().join("state");
    lifecycle::ensure_root(&state).unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("f"), [0; 240]).unwrap();
    let legacy_path = state.join("inv-legacy");
    legacy(&legacy_path, &source);
    let companion = Repository::companion_path(&legacy_path).unwrap();
    let original_db = fs::read(legacy_path.join("inventory.db")).unwrap();
    let offline = t.path().join("offline");
    fs::rename(&source, &offline).unwrap();
    let old = good(&["cache", "show", path(&legacy_path)]);
    assert_eq!(old[0]["logical_bytes"], "240");
    assert_eq!(old[0]["measured_unix"], 200);
    assert_eq!(old[0]["source_acquired"], false);
    assert!(!companion.exists());
    assert_eq!(
        fs::read(legacy_path.join("inventory.db")).unwrap(),
        original_db
    );
    fs::rename(&offline, &source).unwrap();
    fs::write(source.join("f"), [0; 160]).unwrap();
    let current = good(&["cache", "verify", path(&legacy_path)]);
    assert_eq!(current[0]["generation"], "2");
    assert_eq!(current[0]["logical_bytes"], "160");
    assert_eq!(current[0]["source_acquired"], true);
    assert!(companion.join("ORIGIN.json").is_file());
    assert_eq!(
        fs::read(legacy_path.join("inventory.db")).unwrap(),
        original_db
    );
    fs::rename(&source, &offline).unwrap();
    assert_eq!(
        good(&["cache", "show", path(&legacy_path)])[0]["measured_unix"],
        200
    );
    assert_eq!(
        good(&["cache", "show", path(&legacy_path), "--generation", "2"])[0]["logical_bytes"],
        "160"
    );
    let rows = good(&["cache", "list", "--directory", path(&state)]);
    assert_eq!(
        rows.iter().filter(|r| r["record"] == "measurement").count(),
        2
    );
    assert_eq!(rows.last().unwrap()["unavailable"], 0);
    let destination = state.join("inv-independent");
    good(&["cache", "retain", path(&legacy_path), path(&destination)]);
    assert!(!destination.join("ORIGIN.json").exists());
    assert!(
        !run(&["cache", "forget", path(&legacy_path)])
            .status
            .success()
    );
    assert!(legacy_path.join("inventory.db").is_file());
    good(&["cache", "forget", path(&legacy_path), "--confirm"]);
    assert!(!legacy_path.join("inventory.db").exists());
    assert!(!companion.join("CURRENT.json").exists());
    assert_eq!(
        good(&["cache", "show", path(&destination)])[0]["logical_bytes"],
        "240"
    );
    assert_eq!(
        good(&["cache", "show", path(&destination), "--generation", "2"])[0]["logical_bytes"],
        "160"
    );
    assert_eq!(fs::metadata(offline.join("f")).unwrap().len(), 160);
}

fn partial_legacy(path: &Path, root: &Path) -> AcquisitionContext {
    let c = AcquisitionContext {
        inventory: InventoryId(5522),
        source: catalogue::source_hint(root),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let store = Store::create(path, c.inventory, c.source, Durability::Retained).unwrap();
    let guard = Guard::open(
        root,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut session = Session::new(store, c, guard).unwrap();
    session
        .store_mut()
        .record_acquisition(c, root, 100)
        .unwrap();
    session.store_mut().set_retention(true, 30).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while session.store().entry_count(c.generation).unwrap() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "partial fixture did not commit its member"
        );
        assert_ne!(session.tick().unwrap().state, State::StoreError);
        std::thread::sleep(Duration::from_millis(1));
    }
    // No finalize_step: a real committed member, acquisition metadata and
    // continuity guard exist, but the historical candidate stays unmeasured.
    assert_eq!(session.store().saved(c.generation).unwrap().measured, None);
    c
}
#[test]
fn explicit_resume_retries_bound_interrupted_adoption_without_changing_the_original() {
    for use_companion_path in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        lifecycle::ensure_root(&state).unwrap();
        let source = t.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("kept.txt"), [3; 240]).unwrap();
        let original = state.join("inv-legacy-partial");
        let c = partial_legacy(&original, &source);
        let original_db = fs::read(original.join("inventory.db")).unwrap();
        let companion = Repository::companion_path(&original).unwrap();
        let legacy = Store::certify_readonly(&original, &Default::default()).unwrap();
        let mut repository = Repository::open(&companion, c.inventory, c.source).unwrap();
        repository.bind_legacy_origin(&legacy).unwrap();
        let origin_before = fs::read(companion.join("ORIGIN.json")).unwrap();
        let marker = companion.join(format!(".pending-adopt-{:016x}", c.generation.0));
        fs::write(
            &marker,
            serde_json::to_vec(&serde_json::json!({"version":1,"generation":c.generation.0}))
                .unwrap(),
        )
        .unwrap();
        // FlushFileBuffers requires a writable handle on Windows; this is
        // the fixture-owned marker, not the read-only legacy inventory.
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&marker)
            .unwrap()
            .sync_all()
            .unwrap();
        let interrupted_path = repository.candidate_path(c.generation);
        let mut interrupted = Store::create(
            &interrupted_path,
            c.inventory,
            c.source,
            Durability::Retained,
        )
        .unwrap();
        interrupted.create_generation(c).unwrap();
        drop(interrupted);
        let interrupted_db = fs::read(interrupted_path.join("inventory.db")).unwrap();
        drop(repository);
        drop(legacy);
        let argument = if use_companion_path {
            &companion
        } else {
            &original
        };
        let result = good(&["cache", "resume", path(argument)]);
        assert_eq!(result[0]["generation"], "1");
        assert_eq!(result[0]["logical_bytes"], "240");
        assert_eq!(result[0]["started_unix"], 100);
        assert_eq!(result[0]["source_acquired"], true);
        assert!(result[0]["measured_unix"].as_i64().unwrap() >= 100);
        assert!(!marker.exists());
        assert_eq!(
            fs::read(original.join("inventory.db")).unwrap(),
            original_db
        );
        assert_eq!(
            fs::read(companion.join("ORIGIN.json")).unwrap(),
            origin_before
        );
        let quarantines = fs::read_dir(&companion)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarantine-a-")
            })
            .collect::<Vec<_>>();
        assert_eq!(quarantines.len(), 1);
        assert_eq!(
            fs::read(quarantines[0].join("inventory.db")).unwrap(),
            interrupted_db
        );
        let original_view = Store::certify_readonly(&original, &Default::default()).unwrap();
        assert_eq!(original_view.acquisition_context(c.generation).unwrap(), c);
        assert_eq!(original_view.saved(c.generation).unwrap().measured, None);
        assert_eq!(original_view.entry_count(c.generation).unwrap(), 2);
        drop(original_view);
        let repository = Repository::open_readonly(&companion).unwrap();
        let cap = repository
            .open_latest(&Default::default())
            .unwrap()
            .unwrap();
        assert_eq!(cap.generation(), c.generation);
        assert_eq!(cap.as_store().entry_count(c.generation).unwrap(), 2);
        assert_eq!(
            cap.as_store().measurement(c.generation).unwrap().logical,
            Some(240)
        );
        assert_eq!(fs::read(source.join("kept.txt")).unwrap(), [3; 240]);
    }
}
