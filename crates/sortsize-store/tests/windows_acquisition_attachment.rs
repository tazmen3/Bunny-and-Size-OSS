//! SCAN-006/009/010/015 : rattachement Windows pendant l'acquisition directe.
//! Les mutations restent dans des fixtures privées, sans parcourir le disque.
#![cfg(windows)]

use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    inventory_contract::ScopeCoverage,
};
use sortsize_store::{
    direct_tree::DirectTree,
    inventory::queries::empty_entries::{self, EmptyKind, EmptySort},
    scheduler::{CollectionOptions, Control, direct},
};
use std::{fs, path::Path, process::Command, sync::Arc};

fn guard(path: &Path) -> Guard {
    Guard::open(path, Policy::default()).unwrap()
}

fn junction(link: &Path, target: &Path) {
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "New-Item -ItemType Junction -Path $env:BUNNY_GUARD_LINK -Target $env:BUNNY_GUARD_TARGET -ErrorAction Stop | Out-Null",
        ])
        .env("BUNNY_GUARD_LINK", link)
        .env("BUNNY_GUARD_TARGET", target)
        .env_remove("PSModulePath")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn empty_directories_skip_junctions_and_inaccessible_listings() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let with_junction = source.join("with-junction");
    let inaccessible = source.join("inaccessible");
    let empty = source.join("empty");
    let target = fixture.path().join("outside");
    let scratch = fixture.path().join("scratch");
    for path in [&with_junction, &inaccessible, &empty, &target, &scratch] {
        fs::create_dir_all(path).unwrap();
    }
    fs::create_dir(target.join("outside-empty")).unwrap();
    let link = with_junction.join("link");
    junction(&link, &target);
    let acl = |operation, access| {
        Command::new("icacls.exe")
            .arg(&inaccessible)
            .args([operation, access])
            .output()
            .unwrap()
    };
    assert!(acl("/deny", "*S-1-1-0:(RD)").status.success());
    let listing = fs::read_dir(&inaccessible);
    let cancel = CancellationToken::default();
    let result = empty_entries::prepare(
        Arc::new(guard(&source)),
        &scratch,
        8 * 1024 * 1024,
        EmptyKind::Directories,
        &cancel,
        |_| {},
    );
    assert!(acl("/remove:d", "*S-1-1-0").status.success());
    fs::remove_dir(&link).unwrap();

    assert_eq!(
        listing.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let job = result.unwrap();
    let rows = job
        .page("", EmptySort::PathAscending, 0, &cancel)
        .unwrap()
        .rows;
    assert_eq!(job.summary().coverage, ScopeCoverage::Partial);
    assert!(job.summary().progress.errors > 0);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].path.ends_with("empty"));
}

#[test]
fn replaced_root_invalidates_existing_cursor_and_file_observation() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("entry"), b"original").unwrap();
    let acquired = guard(&source);
    let cursor = acquired.scan_directory(&source).unwrap();
    cursor
        .validate(&acquired.inspect(&source).unwrap())
        .unwrap();

    fs::rename(&source, fixture.path().join("previous")).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("entry"), b"replacement").unwrap();

    assert!(acquired.inspect(&source).is_err());
    assert!(
        acquired
            .observe_options(&source.join("entry"), true, false)
            .is_err()
    );
    assert!(acquired.scan_directory(&source).is_err());
}

#[test]
fn replaced_file_after_listing_uses_the_current_identity_size_and_allocation() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    fs::create_dir(&source).unwrap();
    let file = source.join("entry");
    fs::write(&file, b"old").unwrap();
    let acquired = guard(&source);
    let original = acquired.inspect(&file).unwrap();
    let mut cursor = acquired.scan_directory(&source).unwrap();
    let entry = cursor.next().unwrap().unwrap();
    fs::rename(&file, fixture.path().join("previous-file")).unwrap();
    fs::write(&file, [0; 8192]).unwrap();

    let current = cursor.observe(&acquired, &entry, true, false).unwrap();
    let independently_opened = acquired.observe_options(&file, true, false).unwrap();
    assert_ne!(current.identity, original.identity);
    assert_eq!(current.identity, independently_opened.identity);
    assert_eq!(current.length, 8192);
    assert_eq!(
        current.properties.allocated,
        independently_opened.properties.allocated
    );
    if cursor.is_ntfs_batch() {
        assert!(cursor.ntfs_stats().stale_rows > 0);
    }
}

#[test]
fn native_inspection_preserves_windows_case_resolution_for_descendant_names() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("MiXeD-Entry"), b"same file").unwrap();
    let acquired = guard(&source);
    let original = acquired.inspect(&source.join("MiXeD-Entry")).unwrap();
    let alias = acquired.inspect(&source.join("mixed-entry")).unwrap();
    assert_eq!(original.identity, alias.identity);
    assert_eq!(original.length, alias.length);
}

#[test]
fn ancestor_junction_is_rejected_even_when_it_reaches_the_same_native_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let ancestor = fixture.path().join("ancestor");
    let source = ancestor.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("entry"), b"original").unwrap();
    let acquired = guard(&source);
    let before = acquired.inspect(&source).unwrap();
    let moved = fixture.path().join("moved");
    fs::rename(&ancestor, &moved).unwrap();
    // The root itself is unchanged: only an ancestor now redirects to it.
    assert_eq!(
        before.identity,
        guard(&moved.join("source")).evidence().1.identity
    );
    junction(&ancestor, &moved);

    let inspection = acquired.inspect(&source);
    let observation = acquired.observe_options(&source.join("entry"), true, false);
    let listing = acquired.scan_directory(&source);
    fs::remove_dir(&ancestor).unwrap();
    assert!(inspection.is_err());
    assert!(observation.is_err());
    assert!(listing.is_err());
    assert_eq!(fs::read(moved.join("source/entry")).unwrap(), b"original");
}

#[test]
fn internal_exclusion_rejects_same_name_replacement_and_preserves_neighbours() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let internal = source.join("private-workspace");
    let neighbour = source.join("private-workspace-other");
    fs::create_dir_all(&internal).unwrap();
    fs::create_dir(&neighbour).unwrap();
    let acquired = guard(&source)
        .with_internal_state(guard(&internal))
        .unwrap();
    assert!(acquired.skip_internal_entry(&internal).unwrap());
    assert!(!acquired.skip_internal_entry(&neighbour).unwrap());
    assert!(acquired.scan_directory(&internal).is_err());
    assert!(acquired.scan_directory(&neighbour).is_ok());

    fs::rename(&internal, source.join("previous-workspace")).unwrap();
    fs::create_dir(&internal).unwrap();
    fs::write(internal.join("unrelated-file"), b"must not be excluded").unwrap();
    assert!(acquired.validate_internal_state().is_err());
    assert!(acquired.skip_internal_entry(&internal).is_err());
    assert!(acquired.scan_directory(&source).is_err());
    assert!(acquired.work_path_allowed(&internal).is_err());
}

#[test]
fn root_replacement_between_direct_batches_cannot_produce_complete_coverage() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let scratch = fixture.path().join("scratch");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&scratch).unwrap();
    for index in 0..100 {
        fs::write(source.join(format!("original-{index}")), b"abc").unwrap();
    }
    let acquired = Arc::new(guard(&source));
    let mut tree = DirectTree::new(source.clone(), &scratch, 1024 * 1024).unwrap();
    let mut replaced = false;
    direct::run(
        acquired,
        CollectionOptions::default(),
        &Control::default(),
        |batch| {
            let nonempty = !batch.batch.observations.is_empty();
            let children = tree.accept(batch.batch)?;
            if nonempty && !replaced {
                fs::rename(&source, fixture.path().join("previous"))?;
                fs::create_dir(&source)?;
                fs::write(source.join("replacement"), [0; 4096])?;
                replaced = true;
            }
            Ok(children)
        },
    )
    .unwrap();
    tree.finish().unwrap();
    let summary = tree.summary();
    assert!(replaced);
    assert_eq!(summary.coverage, ScopeCoverage::Partial);
    assert!(summary.errors > 0);
    assert!(summary.measure.files < 100);
    assert_eq!(summary.measure.logical, Some(summary.measure.files * 3));
}

#[test]
fn cancellation_between_direct_batches_prevents_a_complete_view() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let scratch = fixture.path().join("scratch");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&scratch).unwrap();
    for index in 0..100 {
        fs::write(source.join(format!("entry-{index}")), b"abc").unwrap();
    }
    let control = Control::default();
    let mut tree = DirectTree::new(source.clone(), &scratch, 1024 * 1024).unwrap();
    let mut batches = 0;
    let result = direct::run(
        Arc::new(guard(&source)),
        CollectionOptions::default(),
        &control,
        |batch| {
            batches += 1;
            let children = tree.accept(batch.batch)?;
            control.cancel();
            Ok(children)
        },
    );
    assert!(result.is_err());
    assert_eq!(batches, 1);
    assert!(tree.summary().observations < 100);
    assert_ne!(tree.summary().coverage, ScopeCoverage::Complete);
    assert!(tree.finish().is_err());
}

#[test]
fn observed_directory_replaced_by_a_file_keeps_detached_coverage() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let child = source.join("child");
    let scratch = fixture.path().join("scratch");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir(&scratch).unwrap();
    fs::write(child.join("unread-entry"), b"original").unwrap();
    let acquired = Arc::new(guard(&source));
    let mut tree = DirectTree::new(source.clone(), &scratch, 1024 * 1024).unwrap();
    let mut replaced = false;
    direct::run(
        acquired,
        CollectionOptions::default(),
        &Control::default(),
        |batch| {
            let children = tree.accept(batch.batch)?;
            if !children.is_empty() && !replaced {
                // The parent has observed this directory, but the scheduler
                // cannot dispatch it until this callback returns its ID.
                fs::rename(&child, fixture.path().join("previous-child"))?;
                fs::write(&child, b"replacement file")?;
                replaced = true;
            }
            Ok(children)
        },
    )
    .unwrap();
    tree.finish().unwrap();
    let summary = tree.summary();
    assert!(replaced);
    assert_eq!(summary.coverage, ScopeCoverage::Partial);
    assert_eq!(summary.detached_directories, 1);
    assert_eq!(summary.measure.files, 0);
}
