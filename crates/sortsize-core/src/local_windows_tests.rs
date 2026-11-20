//! SCAN-006/007/009/010, META-006: deterministic changes after enumeration.
use super::*;
use crate::{MetadataValue, ValueAccuracy};
use std::fs;

fn listed(source: &LocalSource, name: &str) -> fs::DirEntry {
    source
        .open_directory(source.root())
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == name)
        .unwrap()
}

#[test]
fn deleted_file_keeps_enumerated_logical_bytes_and_missing_advanced_values() {
    for (allocation_requested, owner_requested) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("gone"), b"enumerated").unwrap();
        let source = LocalSource::new(
            fixture.path(),
            &ScanOptions::default(),
            CollectFields {
                dates: true,
                allocation: allocation_requested,
                owner: owner_requested,
                ..Default::default()
            },
        )
        .unwrap();
        let entry = listed(&source, "gone");
        let before = entry.metadata().unwrap();
        fs::remove_file(entry.path()).unwrap();

        let Observation::File {
            bytes,
            dates,
            allocation,
            owner,
            ..
        } = source.observe_entry(&entry.path(), 1, &entry)
        else {
            panic!("an enumerated file remains a logical observation after deletion");
        };
        assert_eq!(bytes, 10);
        assert_eq!(dates, Some(EntryDates::collect(&before)));
        assert_eq!(allocation.is_some(), allocation_requested);
        if let Some(allocation) = allocation {
            assert_eq!(allocation.allocated_bytes, MetadataValue::Unknown);
            assert_eq!(allocation.identity, MetadataValue::Unknown);
            assert_eq!(allocation.link_count, MetadataValue::Unknown);
        }
        assert_eq!(owner.is_some(), owner_requested);
        if let Some(owner) = owner {
            assert_eq!(owner.id, MetadataValue::Unknown);
        }
    }
}

#[test]
fn advanced_collection_refreshes_length_and_dates_from_the_opened_file() {
    use std::time::{Duration, UNIX_EPOCH};

    for (allocation_requested, owner_requested) in [(true, false), (false, true), (true, true)] {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("changed"), b"old").unwrap();
        let source = LocalSource::new(
            fixture.path(),
            &ScanOptions::default(),
            CollectFields {
                dates: true,
                allocation: allocation_requested,
                owner: owner_requested,
                ..Default::default()
            },
        )
        .unwrap();
        let entry = listed(&source, "changed");
        assert_eq!(entry.metadata().unwrap().len(), 3);
        let modified = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(entry.path())
            .unwrap();
        file.set_len(8192).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        drop(file);
        // The stale row remains available; the advanced path must replace it.
        assert_eq!(entry.metadata().unwrap().len(), 3);

        let Observation::File {
            bytes,
            dates,
            allocation,
            owner,
            ..
        } = source.observe_entry(&entry.path(), 1, &entry)
        else {
            panic!("the changed regular file must remain observable");
        };
        assert_eq!(bytes, 8192);
        assert_eq!(
            dates.unwrap().modified,
            MetadataValue::Known {
                value: modified,
                accuracy: ValueAccuracy::Exact,
            }
        );
        if let Some(allocation) = allocation {
            assert!(matches!(
                allocation.allocated_bytes,
                MetadataValue::Known { .. }
            ));
            assert!(matches!(allocation.identity, MetadataValue::Known { .. }));
        }
        if let Some(owner) = owner {
            assert!(matches!(owner.id, MetadataValue::Known { .. }));
        }
    }
}

#[test]
fn changed_directory_is_rechecked_before_descent() {
    let fixture = tempfile::tempdir().unwrap();
    fs::create_dir(fixture.path().join("changed")).unwrap();
    let source = LocalSource::new(
        fixture.path(),
        &ScanOptions::default(),
        CollectFields::default(),
    )
    .unwrap();
    let entry = listed(&source, "changed");
    fs::remove_dir(entry.path()).unwrap();
    fs::write(entry.path(), b"now a file").unwrap();
    assert!(matches!(
        source.observe_entry(&entry.path(), 1, &entry),
        Observation::Directory { .. }
    ));
    let error = source.open_directory(&entry.path()).err().unwrap();
    assert_eq!(error.kind, "ChangedDuringScan");
}

#[test]
fn excluded_deleted_entry_does_not_trigger_advanced_collection() {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(fixture.path().join("excluded"), b"old").unwrap();
    let source = LocalSource::new(
        fixture.path(),
        &ScanOptions {
            exclusions: vec!["excluded".into()],
            ..Default::default()
        },
        CollectFields {
            dates: true,
            allocation: true,
            owner: true,
            ..Default::default()
        },
    )
    .unwrap();
    let entry = listed(&source, "excluded");
    fs::remove_file(entry.path()).unwrap();
    assert!(matches!(
        source.observe_entry(&entry.path(), 1, &entry),
        Observation::Excluded
    ));
}

#[test]
fn directory_replaced_by_junction_is_not_opened_or_followed() {
    let fixture = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("sentinel"), b"outside").unwrap();
    let link = fixture.path().join("changed");
    fs::create_dir(&link).unwrap();
    let source = LocalSource::new(
        fixture.path(),
        &ScanOptions::default(),
        CollectFields::default(),
    )
    .unwrap();
    let entry = listed(&source, "changed");
    fs::remove_dir(&link).unwrap();
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&link)
        .arg(target.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "native junction fixture: {output:?}"
    );
    struct RemoveJunction(PathBuf);
    impl Drop for RemoveJunction {
        fn drop(&mut self) {
            // Remove the single fixture link, never recursively its target.
            fs::remove_dir(&self.0).unwrap();
        }
    }
    let cleanup = RemoveJunction(link);
    assert!(matches!(
        source.observe_entry(&entry.path(), 1, &entry),
        Observation::Directory { .. }
    ));
    assert!(source.open_directory(&entry.path()).is_err());
    let fresh = listed(&source, "changed");
    assert!(matches!(
        source.observe_entry(&fresh.path(), 1, &fresh),
        Observation::Symlink
    ));
    drop(cleanup);
    assert_eq!(
        fs::read(target.path().join("sentinel")).unwrap(),
        b"outside"
    );
}
