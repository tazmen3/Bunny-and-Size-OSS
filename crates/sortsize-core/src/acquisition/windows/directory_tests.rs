use super::*;

#[test]
fn group_rows_preserve_native_names_and_reject_invalid_bounds() {
    let mut bytes = [0u8; NAME_OFFSET + 8];
    let size_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength);
    let file_id_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
    bytes[size_at..size_at + 4].copy_from_slice(&2u32.to_le_bytes());
    bytes[file_id_at] = 42;
    bytes[NAME_OFFSET..NAME_OFFSET + 2].copy_from_slice(&0xd800u16.to_le_bytes());
    let (name, native, next) = row(&bytes).unwrap();
    assert_eq!(name.encode_wide().collect::<Vec<_>>(), [0xd800]);
    assert_eq!(native.id[0], 42);
    assert_eq!(next, None);

    bytes[..4].copy_from_slice(&8u32.to_le_bytes());
    assert!(row(&bytes).is_none());
    bytes[..4].copy_from_slice(&0u32.to_le_bytes());
    assert!(row(&bytes[..NAME_OFFSET + 1]).is_none());
    bytes[NAME_OFFSET..NAME_OFFSET + 2].copy_from_slice(&92u16.to_le_bytes());
    assert!(row(&bytes).is_none());
}

#[test]
fn native_rows_keep_hardlinks_and_remeasure_changed_files() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let a = root.join("file");
    let b = root.join("alias");
    fs::write(&a, [0; 128]).unwrap();
    fs::hard_link(&a, &b).unwrap();
    let mut guard = Guard::open(root, Policy::default()).unwrap();
    guard.ntfs_directory = true;
    let mut list = guard.scan_directory(root).unwrap();
    assert!(list.ntfs_stats().batch_calls > 0);
    fs::write(&a, [0; 16384]).unwrap();

    let mut observations = Vec::new();
    while let Some(row) = list.next().transpose().unwrap() {
        let expected = guard.observe_options(row.path(), true, false).unwrap();
        let actual = list.observe(&guard, &row, true, false).unwrap();
        assert_eq!(expected.identity, actual.identity);
        assert_eq!(expected.length, actual.length);
        assert_eq!(expected.properties.allocated, actual.properties.allocated);
        assert_eq!(actual.length, 16384);
        observations.push(actual.identity);
    }
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0], observations[1]);
    assert_eq!(list.ntfs_stats().relative_opens, 2);
    assert_eq!(list.ntfs_stats().stale_rows, 2);
}

#[test]
fn pinned_directory_rejects_replacement_at_the_old_path() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let child = root.join("child");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("entry"), b"old").unwrap();
    let mut guard = Guard::open(root, Policy::default()).unwrap();
    guard.ntfs_directory = true;
    let list = guard.scan_directory(&child).unwrap();
    assert!(list.ntfs_stats().batch_calls > 0);

    fs::rename(&child, root.join("old")).unwrap();
    fs::create_dir(&child).unwrap();
    assert!(list.validate(&guard.inspect(&child).unwrap()).is_err());
}

#[test]
fn listing_rows_preserve_identity_dates_and_unknown_allocation() {
    use crate::MetadataValue as M;
    let mut entry = Entry {
        id: [231; 16],
        attrs: FILE_ATTRIBUTE_NORMAL,
        length: 17,
        allocation: 4096,
        created: 132_000_000_000_000_000,
        modified: 133_000_000_000_000_000,
        accessed: 134_000_000_000_000_000,
    };
    let stamp = entry.listed_stamp(u64::MAX, true).unwrap();
    assert_eq!(
        stamp.identity,
        Some(LocalFileId::Windows {
            volume_serial: u64::MAX,
            file_id: [231; 16]
        })
    );
    assert_eq!(stamp.created, file_time(entry.created));
    assert_eq!(stamp.modified, file_time(entry.modified));
    assert!(
        matches!(stamp.properties.accessed, M::Known { value, .. } if Some(value) == file_time(entry.accessed))
    );
    assert!(matches!(
        stamp.properties.allocated,
        M::Known { value: 4096, .. }
    ));
    assert_eq!(
        entry.listed_stamp(1, false).unwrap().properties.allocated,
        M::NotCollected
    );
    for attrs in [
        FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_ATTRIBUTE_COMPRESSED,
        FILE_ATTRIBUTE_OFFLINE,
    ] {
        entry.attrs = attrs;
        assert_eq!(
            entry.listed_stamp(1, true).unwrap().properties.allocated,
            M::Unknown
        );
    }
    entry.attrs = FILE_ATTRIBUTE_NORMAL;
    entry.allocation = -1;
    assert_eq!(
        entry.listed_stamp(1, true).unwrap().properties.allocated,
        M::Unknown
    );
    entry.length = -1;
    assert!(entry.listed_stamp(1, true).is_none());
    entry.length = 0;
    entry.attrs = FILE_ATTRIBUTE_REPARSE_POINT;
    assert!(entry.listed_stamp(1, true).is_none());
    entry.attrs = FILE_ATTRIBUTE_NORMAL;
    entry.id = [0; 16];
    assert!(entry.listed_stamp(1, true).is_none());
}

#[test]
fn consultation_uses_captured_occurrences_and_keeps_strict_observation_available() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let a = root.join("file");
    fs::write(&a, [0; 128]).unwrap();
    fs::hard_link(&a, root.join("alias")).unwrap();
    let mut guard = Guard::open(root, Policy::default()).unwrap();
    assert_eq!(guard.scan_observation(), ScanObservation::Strict);
    guard.ntfs_directory = true;
    let guard = guard.with_scan_observation(ScanObservation::DirectoryListing);
    let mut list = guard.scan_directory(root).unwrap();
    assert!(list.retains_directory_handle());
    // Force a mutation after the directory batch was captured. Listing mode
    // deliberately retains the historical row, including hardlink occurrences.
    let rows = list.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
    fs::write(&a, [0; 16384]).unwrap();
    let mut identities = Vec::new();
    for row in &rows {
        let listed = list.observe(&guard, row, true, false).unwrap();
        assert_eq!(listed.length, 128);
        assert_eq!(listed.length as i64, row.native.as_ref().unwrap().length);
        assert_eq!(
            guard
                .observe_options(row.path(), false, false)
                .unwrap()
                .length,
            16384
        );
        identities.push(listed.identity);
    }
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0], identities[1]);
    assert_eq!(list.ntfs_stats().relative_opens, 0);
    // Owner collection cannot be satisfied from an index record and retains
    // strict handle semantics, even on a consultation cursor.
    assert_eq!(
        list.observe(&guard, &rows[0], true, true).unwrap().length,
        16384
    );
    assert!(list.ntfs_stats().relative_opens > 0);
}

#[test]
fn retained_consultation_cursor_never_reads_a_workspace_substitution() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let child = root.join("child");
    let state = root.join("state");
    fs::create_dir(&child).unwrap();
    fs::create_dir(&state).unwrap();
    fs::write(child.join("public"), b"old").unwrap();
    fs::write(state.join("secret"), b"private").unwrap();
    let state_guard = Guard::open(&state, Policy::default()).unwrap();
    let mut guard = Guard::open(root, Policy::default())
        .unwrap()
        .with_internal_state(state_guard)
        .unwrap()
        .with_scan_observation(ScanObservation::DirectoryListing);
    guard.ntfs_directory = true;
    assert!(guard.skip_internal_entry(&state).unwrap());
    let mut list = guard.scan_directory(&child).unwrap();
    assert!(list.retains_directory_handle());
    fs::rename(&child, root.join("old")).unwrap();
    fs::rename(&state, &child).unwrap();
    let rows = list.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].file_name(), "public");
    // An invalid record forces the strict path fallback. It must check the
    // pinned parent again rather than looking through its substituted path.
    let mut fallback = ScanEntry {
        path: child.join("secret"),
        name: "secret".into(),
        native: None,
    };
    assert!(list.observe(&guard, &fallback, true, false).is_err());
    fallback.path = child.join("public");
    assert!(list.observe(&guard, &fallback, true, false).is_err());
    // EOF evidence and any new descent reject the substituted workspace.
    assert!(list.validate(&guard.inspect(&child).unwrap()).is_err());
    assert!(guard.validate_internal_state().is_err());
    assert!(guard.inspect_scan_directory(&child).is_err());
    assert!(guard.scan_directory(&child).is_err());
    fs::rename(&child, &state).unwrap();
    fs::rename(root.join("old"), &child).unwrap();
    guard.validate_internal_state().unwrap();
}

#[test]
fn consultation_callback_cannot_insert_an_ancestor_junction_to_the_same_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let branch = root.join("branch");
    let old = root.join("old");
    let leaf = branch.join("leaf");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("entry"), b"same directory identity").unwrap();
    let mut guard = Guard::open(root, Policy::default())
        .unwrap()
        .with_scan_observation(ScanObservation::DirectoryListing);
    guard.ntfs_directory = true;
    let before = guard.inspect(&leaf).unwrap();
    let result = guard.scan_directory_with(&leaf, |observed| {
        assert_eq!(observed.identity, before.identity);
        fs::rename(&branch, &old)?;
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&branch)
            .arg(&old)
            .output()?;
        assert!(output.status.success());
        Ok(())
    });
    // Remove only the fixture's junction before either assertion can panic.
    fs::remove_dir(&branch).unwrap();
    assert_eq!(
        guard.inspect(&old.join("leaf")).unwrap().identity,
        before.identity
    );
    assert!(
        result.is_err(),
        "an unchanged child ID cannot authorize a new ancestor reparse"
    );
}

#[test]
fn consultation_callback_can_cancel_before_listing_and_sees_non_directory_replacements() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let child = root.join("child");
    fs::create_dir(&child).unwrap();
    let mut guard = Guard::open(root, Policy::default())
        .unwrap()
        .with_scan_observation(ScanObservation::DirectoryListing);
    guard.ntfs_directory = true;
    let profile = profile::Profile::start(true);
    let cancelled = guard.scan_directory_with(&child, |_| Err(io::ErrorKind::Interrupted.into()));
    let metrics = profile.finish();
    let mut output = Vec::new();
    metrics.write(&mut output).unwrap();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("acq_cursor_calls=0 ")
    );
    assert!(matches!(cancelled, Err(error) if error.kind() == io::ErrorKind::Interrupted));
    fs::remove_dir(&child).unwrap();
    fs::write(&child, b"replacement file").unwrap();
    let mut called = false;
    let replaced = guard.scan_directory_with(&child, |stamp| {
        called = true;
        assert!(!stamp.directory);
        Ok(())
    });
    assert!(called);
    assert!(matches!(replaced, Err(error) if error.kind() == io::ErrorKind::NotADirectory));
}

#[test]
fn consultation_scope_checks_root_and_workspace_freshly_at_eof() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("source");
    fs::create_dir(&root).unwrap();
    if !is_ntfs(&root) {
        return;
    }
    let child = root.join("child");
    let state = root.join("state");
    fs::create_dir(&child).unwrap();
    fs::create_dir(&state).unwrap();
    let mut guard = Guard::open(&root, Policy::default())
        .unwrap()
        .with_internal_state(Guard::open(&state, Policy::default()).unwrap())
        .unwrap()
        .with_scan_observation(ScanObservation::DirectoryListing);
    guard.ntfs_directory = true;
    let mut list = guard.scan_directory(&child).unwrap();
    assert!(list.next().is_none());
    list.validate(&guard.inspect_scan_directory(&child).unwrap())
        .unwrap();
    fs::rename(&state, root.join("old-state")).unwrap();
    fs::create_dir(&state).unwrap();
    assert!(guard.inspect_scan_directory(&child).is_err());
    fs::remove_dir(&state).unwrap();
    fs::rename(root.join("old-state"), &state).unwrap();
    // Windows can prevent renaming a parent while a descendant directory
    // handle remains open. The cached-EOF workspace check above keeps its
    // cursor alive; close it before separately checking fresh root attachment.
    drop(list);
    fs::rename(&root, fixture.path().join("old-source")).unwrap();
    fs::create_dir_all(&child).unwrap();
    assert!(guard.inspect_scan_directory(&child).is_err());
}

#[test]
fn consultation_callback_cannot_replace_the_target_with_the_workspace() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    if !is_ntfs(root) {
        return;
    }
    let child = root.join("child");
    let state = root.join("state");
    fs::create_dir(&child).unwrap();
    fs::create_dir(&state).unwrap();
    fs::write(state.join("secret"), b"private").unwrap();
    let mut guard = Guard::open(root, Policy::default())
        .unwrap()
        .with_internal_state(Guard::open(&state, Policy::default()).unwrap())
        .unwrap()
        .with_scan_observation(ScanObservation::DirectoryListing);
    guard.ntfs_directory = true;
    let result = guard.scan_directory_with(&child, |_| {
        fs::rename(&child, root.join("old"))?;
        fs::rename(&state, &child)?;
        Ok(())
    });
    fs::rename(&child, &state).unwrap();
    fs::rename(root.join("old"), &child).unwrap();
    assert!(result.is_err());
    guard.validate_internal_state().unwrap();
}
