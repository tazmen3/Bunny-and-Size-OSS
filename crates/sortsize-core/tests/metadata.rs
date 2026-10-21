use serde_json::json;
use sortsize_core::{Coverage, Metadata, MetadataValue, NativePath, ValueAccuracy};
use std::path::Path;

#[test]
fn known_zero_and_each_absence_reason_remain_distinct() {
    let states = [
        MetadataValue::Known {
            value: 0u64,
            accuracy: ValueAccuracy::Exact,
        },
        MetadataValue::Unknown,
        MetadataValue::Unsupported,
        MetadataValue::AccessDenied,
        MetadataValue::NotCollected,
    ];
    let expected = [
        json!({"availability":"known", "value":0, "accuracy":"exact"}),
        json!({"availability":"unknown"}),
        json!({"availability":"unsupported"}),
        json!({"availability":"access_denied"}),
        json!({"availability":"not_collected"}),
    ];
    for (index, state) in states.iter().enumerate() {
        assert_eq!(serde_json::to_value(state).unwrap(), expected[index]);
        for other in &states[index + 1..] {
            assert_ne!(state, other);
        }
    }
}

#[test]
fn accuracy_and_coverage_are_independent() {
    for (coverage, coverage_label) in [
        (Coverage::Complete, "complete"),
        (Coverage::Partial, "partial"),
        (Coverage::Cancelled, "cancelled"),
    ] {
        for (accuracy, accuracy_label) in [
            (ValueAccuracy::Exact, "exact"),
            (ValueAccuracy::LowerBound, "lower_bound"),
        ] {
            let observation = Metadata {
                value: MetadataValue::Known {
                    value: 0u64,
                    accuracy,
                },
                coverage,
            };
            assert_eq!(observation.coverage, coverage);
            assert_eq!(
                serde_json::to_value(observation).unwrap(),
                json!({
                    "value": {"availability":"known", "value":0, "accuracy":accuracy_label},
                    "coverage":coverage_label,
                })
            );
        }
        // Traversal can finish without collecting this particular property.
        let not_collected = Metadata::<u64> {
            value: MetadataValue::NotCollected,
            coverage,
        };
        assert_eq!(
            serde_json::to_value(not_collected).unwrap(),
            json!({
                "value":{"availability":"not_collected"}, "coverage":coverage_label,
            })
        );
    }
    // A partial scan may still contain an exact observation of a visited file.
    let file = Metadata {
        value: MetadataValue::Known {
            value: 7u64,
            accuracy: ValueAccuracy::Exact,
        },
        coverage: Coverage::Partial,
    };
    // A directory total with unseen descendants is only a proven lower bound.
    let directory = Metadata {
        value: MetadataValue::Known {
            value: 7u64,
            accuracy: ValueAccuracy::LowerBound,
        },
        coverage: Coverage::Partial,
    };
    assert_ne!(file, directory);
}

#[test]
fn borrowed_native_paths_and_opaque_provider_keys_keep_their_identity() {
    #[derive(Debug, PartialEq, Eq)]
    struct ProviderObject<'a> {
        uri: &'a str,
        key: &'a [u8],
    }
    let provider = ProviderObject {
        uri: "content://provider/tree/A%2FB/document/id:42",
        key: &[0xff, 0, b'/', b':'],
    };
    let location = Metadata {
        value: MetadataValue::Known {
            value: &provider,
            accuracy: ValueAccuracy::Exact,
        },
        coverage: Coverage::Complete,
    };
    let copy = location;
    assert_eq!(location, copy); // Copy borrows the payload; it does not clone it.
    match copy.value {
        MetadataValue::Known { value, .. } => assert!(std::ptr::eq(value, &provider)),
        _ => panic!("lost provider identity"),
    }
    let uri = MetadataValue::Known {
        value: provider.uri,
        accuracy: ValueAccuracy::Exact,
    };
    assert_eq!(serde_json::to_value(uri).unwrap()["value"], provider.uri);
    let path = Path::new("native source");
    let native = Metadata {
        value: MetadataValue::Known {
            value: path,
            accuracy: ValueAccuracy::Exact,
        },
        coverage: Coverage::Complete,
    };
    match native.value {
        MetadataValue::Known { value, .. } => assert!(std::ptr::eq(value, path)),
        _ => panic!("lost native identity"),
    }
}

#[test]
fn non_unicode_path_export_uses_the_existing_lossless_native_contract() {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'n', 0xff]))
    };
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&[0x006e, 0xd800]))
    };
    #[cfg(not(any(unix, windows)))]
    let path = std::path::PathBuf::from("native");
    let native = NativePath::from(path.as_path());
    let before = serde_json::to_value(&native).unwrap();
    let value = Metadata {
        value: MetadataValue::Known {
            value: &native,
            accuracy: ValueAccuracy::Exact,
        },
        coverage: Coverage::Complete,
    };
    assert_eq!(
        serde_json::to_value(value).unwrap()["value"]["value"],
        before
    );
    #[cfg(unix)]
    assert_eq!(
        before["raw"],
        json!({"encoding":"unix_bytes", "units":[110,255]})
    );
    #[cfg(windows)]
    assert_eq!(
        before["raw"],
        json!({"encoding":"windows_utf16", "units":[110,55296]})
    );
}

#[test]
fn legacy_inventory_truncation_and_partial_totals_keep_their_original_meaning() {
    use sortsize_core::{CancellationToken, ScanOptions, scan_inventory};
    let fixture = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(fixture.path().join("nested")).unwrap();
    std::fs::write(fixture.path().join("nested/file"), b"1234").unwrap();
    for (max_depth, retained_limit, bytes, coverage, accuracy) in [
        (None, 1, 4, Coverage::Complete, ValueAccuracy::Exact),
        (Some(0), 10, 0, Coverage::Partial, ValueAccuracy::LowerBound),
    ] {
        let inventory = scan_inventory(
            fixture.path(),
            &ScanOptions {
                max_depth,
                ..Default::default()
            },
            &CancellationToken::default(),
            retained_limit,
            |_| {},
        )
        .unwrap();
        let before = serde_json::to_value(&inventory.report).unwrap();
        assert_eq!(inventory.report.logical_bytes, bytes);
        assert_eq!(inventory.report.coverage, coverage);
        assert_eq!(inventory.entries[0].logical_bytes, Some(bytes));
        assert_eq!(inventory.entries[0].incomplete, max_depth.is_some());
        if max_depth.is_none() {
            assert_eq!(inventory.entries_omitted, 2);
        }
        // Example of a future caller's explicit annotation, not a new scan API.
        let annotation = Metadata {
            value: MetadataValue::Known {
                value: bytes,
                accuracy,
            },
            coverage,
        };
        assert_eq!(annotation.coverage, inventory.report.coverage);
        assert_eq!(serde_json::to_value(&inventory.report).unwrap(), before);
        assert_eq!(before["schema_version"], 2);
        assert_eq!(before["allocated_bytes"], serde_json::Value::Null);
        assert_eq!(
            before["potentially_reclaimable_bytes"],
            serde_json::Value::Null
        );
    }
}
