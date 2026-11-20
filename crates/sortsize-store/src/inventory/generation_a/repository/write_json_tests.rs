use super::*;

fn directory() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("documents");
    lifecycle::private_directory(&path).unwrap();
    (temp, path)
}
fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut result = std::fs::read_dir(path)
        .unwrap()
        .map(|item| item.unwrap().file_name())
        .collect::<Vec<_>>();
    result.sort();
    result
}
#[test]
fn immutable_json_preserves_existing_file_and_explicit_replace_still_works() {
    let (_temp, path) = directory();
    write_json(&path, "identity.json", &"original", false).unwrap();
    let original = std::fs::read(path.join("identity.json")).unwrap();
    assert!(write_json(&path, "identity.json", &"replacement", false).is_err());
    assert_eq!(std::fs::read(path.join("identity.json")).unwrap(), original);
    assert_eq!(
        names(&path),
        vec![std::ffi::OsString::from("identity.json")]
    );
    write_json(&path, "identity.json", &"replacement", true).unwrap();
    assert_eq!(
        read_json::<String>(&path.join("identity.json")).unwrap(),
        "replacement"
    );
}
#[test]
fn document_budget_and_filesystem_errors_do_not_publish_or_replace() {
    let (_temp, path) = directory();
    write_json(&path, "identity.json", &"original", false).unwrap();
    let original = std::fs::read(path.join("identity.json")).unwrap();
    let before = names(&path);
    let oversized = "x".repeat(DOCUMENT_LIMIT as usize);
    for (name, replace) in [("identity.json", true), ("new.json", false)] {
        assert!(matches!(
            write_json(&path, name, &oversized, replace),
            Err(Error::Contract("repository document exceeds byte budget"))
        ));
    }
    assert!(write_json(&path.join("missing"), "document.json", &"data", false).is_err());
    assert_eq!(std::fs::read(path.join("identity.json")).unwrap(), original);
    assert_eq!(names(&path), before);
}
#[test]
fn destination_directory_and_symlink_are_preserved() {
    let (temp, path) = directory();
    std::fs::create_dir(path.join("directory.json")).unwrap();
    for replace in [false, true] {
        assert!(write_json(&path, "directory.json", &"data", replace).is_err());
    }
    let target = temp.path().join("untouched.json");
    std::fs::write(&target, b"untouched").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, path.join("link.json")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, path.join("link.json")).unwrap();
    #[cfg(any(unix, windows))]
    {
        for replace in [false, true] {
            assert!(write_json(&path, "link.json", &"data", replace).is_err());
        }
        assert!(
            std::fs::symlink_metadata(path.join("link.json"))
                .unwrap()
                .is_symlink()
        );
    }
    assert_eq!(std::fs::read(target).unwrap(), b"untouched");
    assert!(path.join("directory.json").is_dir());
}
#[cfg(windows)]
#[test]
fn windows_create_new_race_keeps_exactly_one_complete_normal_document() {
    use std::os::windows::fs::MetadataExt;
    use std::sync::Barrier;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;
    let (_temp, path) = directory();
    for iteration in 0..8 {
        let name = format!("race-{iteration}.json");
        let barrier = Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                barrier.wait();
                ("left", write_json(&path, &name, &"left", false).is_ok())
            });
            let right = scope.spawn(|| {
                barrier.wait();
                ("right", write_json(&path, &name, &"right", false).is_ok())
            });
            [left.join().unwrap(), right.join().unwrap()]
        });
        assert_eq!(outcomes.iter().filter(|(_, success)| *success).count(), 1);
        let winner = outcomes.iter().find(|(_, success)| *success).unwrap().0;
        assert_eq!(read_json::<String>(&path.join(&name)).unwrap(), winner);
        let metadata = std::fs::metadata(path.join(name)).unwrap();
        assert_eq!(metadata.file_attributes() & FILE_ATTRIBUTE_TEMPORARY, 0);
    }
    assert!(
        names(&path)
            .iter()
            .all(|name| !name.to_string_lossy().starts_with(".pending-"))
    );
}

#[cfg(windows)]
#[test]
fn windows_failed_replace_preserves_destination_and_removes_only_its_stage() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let (_temp, path) = directory();
    write_json(&path, "current.json", &"original", false).unwrap();
    let original = std::fs::read(path.join("current.json")).unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path.join("current.json"))
        .unwrap();
    assert!(write_json(&path, "current.json", &"replacement", true).is_err());
    assert_eq!(std::fs::read(path.join("current.json")).unwrap(), original);
    assert_eq!(names(&path), vec![std::ffi::OsString::from("current.json")]);
    drop(held);
    write_json(&path, "current.json", &"replacement", true).unwrap();
    assert_eq!(
        read_json::<String>(&path.join("current.json")).unwrap(),
        "replacement"
    );
}
