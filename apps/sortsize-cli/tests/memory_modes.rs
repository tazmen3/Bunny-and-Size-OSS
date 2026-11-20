#![cfg(feature = "inventory-report")]
use std::process::Command;

#[test]
fn native_acquisition_uses_each_requested_mode_with_bounded_effective_limits() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("payload"), [17; 37]).unwrap();
    for (mode, entries) in [
        ("auto", None),
        ("50000", Some(50_000)),
        ("250000", Some(250_000)),
        ("1000000", Some(1_000_000)),
        ("5000000", Some(5_000_000)),
        ("unlimited-count", None),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bunny-and-size-cli"))
            .args([
                "--memory-mode",
                mode,
                "--resource-receipt",
                "cache",
                "measure",
            ])
            .arg(&source)
            .arg("--directory")
            .arg(temporary.path().join(mode))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        let receipt: serde_json::Value =
            serde_json::from_str(stderr.lines().next().unwrap()).unwrap();
        assert_eq!(receipt["requested_mode"], mode);
        assert_eq!(receipt["effective_mode"], mode);
        let bytes = receipt["byte_limit"].as_u64().unwrap();
        assert!(bytes > 0 && bytes <= 4 * 1024 * 1024 * 1024);
        let categories = receipt["category_byte_limits"].as_array().unwrap();
        assert!(categories.iter().map(|v| v.as_u64().unwrap()).sum::<u64>() <= bytes);
        if let Some(entries) = entries {
            assert_eq!(receipt["entry_limit"], entries);
        } else if mode == "auto" {
            assert_eq!(
                receipt["entry_limit"].as_u64().unwrap(),
                categories[0].as_u64().unwrap() / 512
            );
        } else {
            assert!(receipt["entry_limit"].is_null());
        }
        let stdout = String::from_utf8(output.stdout).unwrap();
        let measured: serde_json::Value =
            serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
        assert_eq!(measured["logical_bytes"], "37");
        assert_eq!(measured["source_acquired"], true);
    }
}

#[test]
fn unknown_modes_and_legacy_commands_cannot_silently_fall_back() {
    for args in [
        vec!["--memory-mode", "invalid", "cache", "list"],
        vec!["--resource-receipt", "cache", "list"],
        vec!["--memory-mode", "50000", "scan", "."],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bunny-and-size-cli"))
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}
