use super::*;
use std::{ffi::OsString, os::windows::ffi::OsStringExt};

#[test]
fn local_path_preserves_native_components_and_limits_the_optimized_namespace() {
    let name = OsString::from_wide(&[0x6e, 0xd800, 0x7a]);
    let path = Path::new(r"C:\é\").join(&name);
    let parsed = LocalPath::parse(&path).unwrap();
    assert_eq!(parsed.volume, Path::new("C:\\"));
    assert_eq!(parsed.relative, [0xe9, 92, 0x6e, 0xd800, 0x7a]);
    let extended = LocalPath::parse(Path::new(r"\\?\C:\é\child")).unwrap();
    assert_eq!(extended.volume, Path::new(r"\\?\C:\"));
    assert_eq!(
        extended.relative,
        "é\\child".encode_utf16().collect::<Vec<_>>()
    );
    for path in [
        r"\\server\share\child",
        r"C:relative",
        r"C:\tail.",
        r"C:\tail ",
        r"C:\file:stream",
        r"C:\con.txt",
        r"C:\LPT1",
        r"C:\aux\child",
    ] {
        assert!(LocalPath::parse(Path::new(path)).is_none(), "{path}");
    }
}

#[test]
fn strict_inspection_matches_fresh_path_observations_for_deep_native_paths() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source-é");
    let deep = source.join("one").join("two").join("three");
    fs::create_dir_all(&deep).unwrap();
    let file = deep.join("葉.bin");
    fs::write(&file, [0u8; 8192]).unwrap();
    for source in [source.clone(), source.canonicalize().unwrap()] {
        let guard = Guard::open(&source, Policy::default()).unwrap();
        let target = source.join("one").join("two").join("three").join("葉.bin");
        let (_, actual) = inspect_local(&source, &target, &guard.root_stamp, true, true, false)
            .expect("local drive must use strict traversal")
            .unwrap();
        let expected = super::super::observe(
            &target,
            &metadata_chain(&target).unwrap(),
            true,
            true,
            false,
        )
        .unwrap();
        assert_eq!(actual.identity, expected.identity);
        assert_eq!(actual.length, 8192);
        assert_eq!(actual.properties.allocated, expected.properties.allocated);
        assert_eq!(actual.modified, expected.modified);
        let (before, current) =
            inspect_local(&source, &source, &guard.root_stamp, false, false, false)
                .unwrap()
                .unwrap();
        assert_eq!(before, current);
        assert_eq!(current.identity, guard.root_stamp.identity);
    }
}
