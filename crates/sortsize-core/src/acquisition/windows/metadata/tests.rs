use super::*;

#[test]
fn combined_ntfs_metadata_matches_historical_same_handle_observation() {
    let fixture = tempfile::tempdir().unwrap();
    if !is_ntfs(fixture.path()) {
        return;
    }
    let path = fixture.path().join("file");
    let alias = fixture.path().join("alias");
    fs::write(&path, [0; 8193]).unwrap();
    fs::hard_link(&path, &alias).unwrap();
    for path in [fixture.path(), path.as_path(), alias.as_path()] {
        let file = metadata_open(path, FILE_READ_ATTRIBUTES | READ_CONTROL).unwrap();
        assert!(
            query(&file).unwrap().is_some(),
            "native class must be exercised"
        );
        for detailed in [false, true] {
            for allocation in [false, true] {
                for owners in [false, true] {
                    let expected =
                        super::super::observe_file(&file, detailed, allocation, owners, None)
                            .unwrap();
                    let actual = observe_file(&file, detailed, allocation, owners, None).unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
        // Losing owner collection must not turn it into a known/zero owner or
        // discard otherwise accessible metadata.
        let expected = super::super::observe_file(&file, true, true, true, Some(5)).unwrap();
        let actual = observe_file(&file, true, true, true, Some(5)).unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn combined_ntfs_metadata_remeasures_a_changed_hardlink() {
    let fixture = tempfile::tempdir().unwrap();
    if !is_ntfs(fixture.path()) {
        return;
    }
    let path = fixture.path().join("file");
    let alias = fixture.path().join("alias");
    fs::write(&path, b"first").unwrap();
    fs::hard_link(&path, &alias).unwrap();
    let file = metadata_open(&alias, FILE_READ_ATTRIBUTES).unwrap();
    let before = observe_file(&file, true, true, false, None).unwrap();
    fs::write(&path, [0; 131_073]).unwrap();
    let after = observe_file(&file, true, true, false, None).unwrap();
    let expected = super::super::observe_file(&file, true, true, false, None).unwrap();
    assert_eq!(after, expected);
    assert_eq!(before.identity, after.identity);
    assert_eq!(before.length, 5);
    assert_eq!(after.length, 131_073);
    assert!(matches!(after.identity, Some(LocalFileId::Windows { .. })));
}

#[test]
fn combined_metadata_preserves_time_domain_and_rejects_invalid_sizes_and_links() {
    assert_eq!(file_time(116_444_736_000_000_000), Some(UNIX_EPOCH));
    assert_eq!(
        file_time(116_444_735_999_999_999),
        UNIX_EPOCH.checked_sub(Duration::from_nanos(100))
    );
    assert_eq!(
        file_time(0),
        UNIX_EPOCH.checked_sub(Duration::from_secs(11_644_473_600))
    );
    let mut info = FILE_STAT_INFORMATION {
        EndOfFile: 17,
        AllocationSize: 4096,
        NumberOfLinks: 2,
        ..Default::default()
    };
    assert_eq!(shape(&info).unwrap(), (17, false));
    info.EndOfFile = -1;
    assert_eq!(shape(&info).unwrap_err().kind(), io::ErrorKind::InvalidData);
    info.EndOfFile = 17;
    info.FileAttributes = FILE_ATTRIBUTE_REPARSE_POINT;
    assert!(shape(&info).is_err());
    info.FileAttributes = FILE_ATTRIBUTE_DIRECTORY;
    assert_eq!(shape(&info).unwrap(), (17, true));
    // The optimization reuses the existing allocation consistency gate.
    let mut standard = FILE_STANDARD_INFO {
        AllocationSize: 4096,
        EndOfFile: 17,
        NumberOfLinks: 0,
        DeletePending: false,
        Directory: false,
    };
    assert_eq!(allocation_value(standard, 17), MetadataValue::Unknown);
    standard.NumberOfLinks = 2;
    standard.AllocationSize = -1;
    assert_eq!(allocation_value(standard, 17), MetadataValue::Unknown);
}

#[test]
fn unsupported_class_fallback_does_not_include_access_or_io_errors() {
    use windows_sys::Win32::Foundation::{STATUS_ACCESS_DENIED, STATUS_IO_DEVICE_ERROR};
    assert!(unsupported_class(STATUS_INVALID_INFO_CLASS));
    assert!(unsupported_class(STATUS_NOT_SUPPORTED));
    assert!(!unsupported_class(STATUS_ACCESS_DENIED));
    assert!(!unsupported_class(STATUS_IO_DEVICE_ERROR));
    assert!(!unsupported_class(0));
}
