use sortsize_core::{CancellationToken, Coverage, ScanFailure, ScanOptions, scan};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn run(root: &Path) -> sortsize_core::ScanReport {
    scan(
        root,
        &ScanOptions::default(),
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap()
}

#[test]
fn nested_unicode_empty_files_and_directories_have_exact_totals() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join("photos/été/空")).unwrap();
    fs::write(fixture.path().join("vide"), []).unwrap();
    fs::write(fixture.path().join("photos/été/échantillon.txt"), b"12345").unwrap();
    fs::write(fixture.path().join("照片.bin"), b"abc").unwrap();
    let report = run(fixture.path());
    assert_eq!(report.coverage, Coverage::Complete);
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (3, 4, 8)
    );
    assert_eq!(report.errors_count, 0);
    assert!(report.allocated_bytes.is_none());
    assert!(report.potentially_reclaimable_bytes.is_none());
    assert_eq!(report.logical_size_basis, "per_file_path");
    assert!(!report.follow_symlinks);
}

#[test]
fn exclusions_prune_entire_directories_and_match_root_children() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir_all(fixture.path().join(".git/objects")).unwrap();
    fs::create_dir_all(fixture.path().join("nested/.git/objects")).unwrap();
    fs::write(fixture.path().join(".git/objects/hidden"), b"never count").unwrap();
    fs::write(
        fixture.path().join("nested/.git/objects/hidden"),
        b"never count",
    )
    .unwrap();
    fs::write(fixture.path().join("keep"), b"yes").unwrap();
    fs::write(fixture.path().join("ignore.tmp"), b"no").unwrap();
    let options = ScanOptions {
        exclusions: vec!["**/.git/**".into(), "*.tmp".into()],
        ..Default::default()
    };
    let report = scan(
        fixture.path(),
        &options,
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (1, 2, 3)
    );
    assert_eq!(report.excluded_entries, 3); // Pruned roots, not their unseen descendants.
    assert_eq!(report.coverage, Coverage::Complete); // Complete within declared exclusions.
}

#[test]
fn hardlinks_count_logical_bytes_per_path_without_claiming_physical_size() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("original"), b"123456").unwrap();
    fs::hard_link(
        fixture.path().join("original"),
        fixture.path().join("alias"),
    )
    .unwrap();
    let report = run(fixture.path());
    assert_eq!((report.files, report.logical_bytes), (2, 12));
    assert!(report.allocated_bytes.is_none());
    #[cfg(unix)]
    assert_eq!(report.hardlinked_file_paths, Some(2));
}

#[test]
fn deep_native_paths_work_with_one_open_directory() {
    let fixture = TempDir::new().unwrap();
    let mut deepest = fixture.path().to_owned();
    for _ in 0..30 {
        deepest.push("directory-long-name");
    }
    fs::create_dir_all(&deepest).unwrap();
    fs::write(deepest.join("file"), b"12345").unwrap();
    let options = ScanOptions {
        max_open_directories: 1,
        ..Default::default()
    };
    let report = scan(
        fixture.path(),
        &options,
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.coverage, Coverage::Complete);
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (1, 31, 5)
    );
}

#[test]
fn depth_limit_is_explicitly_partial() {
    let fixture = TempDir::new().unwrap();
    fs::create_dir(fixture.path().join("child")).unwrap();
    fs::write(fixture.path().join("child/file"), b"123").unwrap();
    let options = ScanOptions {
        max_depth: Some(1),
        ..Default::default()
    };
    let report = scan(
        fixture.path(),
        &options,
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.coverage, Coverage::Partial);
    assert_eq!((report.files, report.directories), (0, 2));
    assert_eq!(report.depth_limited_directories, 1);
}

#[test]
fn cancellation_from_progress_returns_partial_results_and_stops_traversal() {
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("file"), b"123").unwrap();
    let token = CancellationToken::default();
    let callback_token = token.clone();
    let mut updates = 0;
    let report = scan(fixture.path(), &ScanOptions::default(), &token, |_| {
        updates += 1;
        callback_token.cancel();
    })
    .unwrap();
    assert_eq!(report.coverage, Coverage::Cancelled);
    assert_eq!(updates, 1);
    assert_eq!((report.files, report.directories), (0, 1));
}

#[test]
fn missing_root_file_root_and_invalid_options_are_errors() {
    let fixture = TempDir::new().unwrap();
    let token = CancellationToken::default();
    assert!(matches!(
        scan(
            &fixture.path().join("absent"),
            &ScanOptions::default(),
            &token,
            |_| {}
        ),
        Err(ScanFailure::RootIo { .. })
    ));
    let file = fixture.path().join("file");
    fs::write(&file, []).unwrap();
    assert!(matches!(
        scan(&file, &ScanOptions::default(), &token, |_| {}),
        Err(ScanFailure::InvalidRoot(_))
    ));
    let invalid = ScanOptions {
        exclusions: vec!["[".into()],
        ..Default::default()
    };
    assert!(matches!(
        scan(fixture.path(), &invalid, &token, |_| {}),
        Err(ScanFailure::InvalidOptions(_))
    ));
    let invalid = ScanOptions {
        max_open_directories: 0,
        ..Default::default()
    };
    assert!(matches!(
        scan(fixture.path(), &invalid, &token, |_| {}),
        Err(ScanFailure::InvalidOptions(_))
    ));
}

#[cfg(unix)]
#[test]
fn cyclic_broken_file_and_directory_symlinks_are_never_followed() {
    use std::os::unix::fs::symlink;
    let fixture = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    fs::write(external.path().join("outside"), b"must not count").unwrap();
    fs::write(fixture.path().join("file"), b"123").unwrap();
    symlink(fixture.path(), fixture.path().join("cycle")).unwrap();
    symlink("missing", fixture.path().join("broken")).unwrap();
    symlink("file", fixture.path().join("file-link")).unwrap();
    symlink(external.path(), fixture.path().join("outside-link")).unwrap();
    let report = run(fixture.path());
    assert_eq!(report.coverage, Coverage::Complete);
    assert_eq!(
        (report.files, report.directories, report.logical_bytes),
        (1, 1, 3)
    );
    assert_eq!(report.symlinks_skipped, 4);
    assert!(matches!(
        scan(
            &fixture.path().join("outside-link"),
            &ScanOptions::default(),
            &CancellationToken::default(),
            |_| {}
        ),
        Err(ScanFailure::InvalidRoot(_))
    ));
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_scan_and_serialize_losslessly() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join(OsString::from_vec(vec![b'd', 0xff]));
    fs::create_dir(&root).unwrap();
    fs::write(root.join(OsString::from_vec(vec![b'f', 0xfe])), b"123").unwrap();
    let report = run(&root);
    assert_eq!((report.files, report.logical_bytes), (1, 3));
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["root"]["raw"]["encoding"], "unix_bytes");
    let units: Vec<u8> = serde_json::from_value(json["root"]["raw"]["units"].clone()).unwrap();
    assert_eq!(OsString::from_vec(units), root.as_os_str());
}

#[cfg(unix)]
#[test]
fn real_permission_denial_is_reported_when_not_privileged() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = TempDir::new().unwrap();
    let blocked = fixture.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("secret"), b"123").unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
    let privileged = fs::read_dir(&blocked).is_ok();
    let report = run(fixture.path());
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    if privileged {
        eprintln!(
            "Accès privilégié : le refus réel n'est pas applicable ; test simulé dans scan.rs."
        );
        return;
    }
    assert_eq!(report.coverage, Coverage::Partial);
    assert!(report.errors.iter().any(|e| e.kind == "PermissionDenied"));
    assert_eq!(report.files, 0);
}

#[cfg(windows)]
#[test]
fn windows_symlinks_are_not_followed_when_the_account_can_create_them() {
    use std::os::windows::fs::symlink_dir;
    let fixture = TempDir::new().unwrap();
    let link = fixture.path().join("cycle");
    if let Err(error) = symlink_dir(fixture.path(), &link) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("Privilège symlink absent ; scénario Windows non exécuté.");
            return;
        }
        panic!("Création du lien impossible : {error}");
    }
    let report = run(fixture.path());
    assert_eq!(report.symlinks_skipped, 1);
    assert_eq!(report.directories, 1);
}
