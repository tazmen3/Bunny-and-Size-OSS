use super::*;
use sortsize_core::acquisition::Policy;
use std::{fs, time::Duration};
use tempfile::{TempDir, tempdir};

const DISK: u64 = 8 * 1024 * 1024;

fn guard(path: &Path) -> Arc<Guard> {
    Arc::new(Guard::open(path, Policy::default()).unwrap())
}

fn scan(source: &TempDir, scratch: &TempDir, kind: EmptyKind) -> EmptySearch {
    prepare(
        guard(source.path()),
        scratch.path(),
        DISK,
        kind,
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap()
}

fn page(job: &EmptySearch) -> EmptyPage {
    job.page("", EmptySort::default(), 0, &CancellationToken::default())
        .unwrap()
}

#[test]
fn distinct_modes_keep_file_occurrences_and_only_directly_empty_directories() {
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    let branch = source.path().join("arbre-é");
    let empty = branch.join("enfant-vide");
    fs::create_dir_all(&empty).unwrap();
    let zero = source.path().join("Été-中文");
    let alias = branch.join("alias");
    let one = source.path().join("un-octet");
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_730_000_000);
    fs::File::create(&zero)
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    fs::hard_link(&zero, &alias).unwrap();
    fs::write(&one, [8]).unwrap();

    let files = scan(&source, &scratch, EmptyKind::Files);
    assert_eq!(files.summary().progress.matches, 2);
    assert_eq!(files.summary().progress.directories_scanned, 3);
    assert_eq!(files.summary().progress.entries_seen, 5);
    assert_eq!(files.summary().progress.errors, 0);
    let rows = page(&files).rows;
    assert!(rows.iter().all(|row| row.modified == Some(stamp)));
    assert!(rows.iter().any(|row| row.path.ends_with("Été-中文")));
    assert!(rows.iter().any(|row| row.path.ends_with("arbre-é/alias")
        && row.parent.as_ref().unwrap().ends_with("arbre-é")));
    let parents = files.parents("", 0, &CancellationToken::default()).unwrap();
    assert_eq!(parents.rows.len(), 2);
    assert!(parents.rows.iter().all(|parent| parent.count == 1));

    let directories = scan(&source, &scratch, EmptyKind::Directories);
    assert_eq!(directories.summary().progress.matches, 1);
    let rows = page(&directories).rows;
    assert!(rows[0].path.ends_with("arbre-é/enfant-vide"));
    assert!(rows[0].parent.as_ref().unwrap().ends_with("arbre-é"));
}

#[test]
fn an_empty_root_is_a_result_but_a_protected_entry_still_prevents_emptiness() {
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    let job = scan(&source, &scratch, EmptyKind::Directories);
    assert_eq!(job.summary().progress.matches, 1);
    assert_eq!(page(&job).rows[0].path, guard(source.path()).root());
    drop(job);

    let internal = source.path().join("internal");
    fs::create_dir(&internal).unwrap();
    let root = Guard::open(source.path(), Policy::default())
        .unwrap()
        .with_internal_state(Guard::open(&internal, Policy::default()).unwrap())
        .unwrap();
    let dir_job = prepare(
        Arc::new(root),
        &internal,
        DISK,
        EmptyKind::Directories,
        &CancellationToken::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(dir_job.summary().progress.entries_seen, 1);
    assert_eq!(dir_job.summary().progress.matches, 0);
    assert!(page(&dir_job).rows.is_empty());
}

#[test]
fn page_filter_sort_aggregation_and_cancellation_apply_to_the_whole_result() {
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    let deep = source.path().join("Répertoire");
    fs::create_dir(&deep).unwrap();
    for index in 0..105 {
        fs::File::create(deep.join(format!("Été-{index:03}")))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(index))
            .unwrap();
    }
    fs::File::create(source.path().join("other")).unwrap();
    let job = scan(&source, &scratch, EmptyKind::Files);
    assert_eq!(job.summary().progress.matches, 106);
    let cancel = CancellationToken::default();
    let first = job
        .page("été-", EmptySort::ModifiedNewest, 0, &cancel)
        .unwrap();
    assert_eq!(first.rows.len(), MAX_ROWS);
    assert_eq!(first.previous_offset, None);
    assert!(first.rows[0].path.ends_with("Été-104"));
    let last = job
        .page(
            "été-",
            EmptySort::ModifiedNewest,
            first.next_offset.unwrap(),
            &cancel,
        )
        .unwrap();
    assert_eq!(last.rows.len(), 5);
    assert_eq!(last.previous_offset, Some(0));
    assert!(last.next_offset.is_none());
    assert!(last.rows[4].path.ends_with("Été-000"));
    let parent = job.parents("ÉTÉ-", 0, &cancel).unwrap();
    assert_eq!(parent.rows.len(), 1);
    assert_eq!(parent.rows[0].count, 105);
    assert!(
        parent.rows[0]
            .path
            .as_ref()
            .unwrap()
            .ends_with("Répertoire")
    );
    cancel.cancel();
    assert!(matches!(job.page("", EmptySort::default(), 0, &cancel),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::Interrupted));
}

#[test]
fn a_root_modified_after_its_listing_cannot_be_published_as_empty() {
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    let mut written = false;
    let result = prepare(
        guard(source.path()),
        scratch.path(),
        DISK,
        EmptyKind::Directories,
        &CancellationToken::default(),
        |progress| {
            if !written && progress.matches == 1 {
                fs::write(source.path().join("appeared"), b"new entry").unwrap();
                written = true;
            }
        },
    )
    .unwrap();
    assert!(written);
    assert_eq!(result.summary().coverage, ScopeCoverage::Partial);
    assert!(result.summary().progress.errors > 0);
    assert!(page(&result).rows.is_empty());
}

#[test]
fn invalidation_discards_early_results_below_the_changed_directory() {
    // A deterministic receiver case: a descendant could finish while the
    // parent's cursor is parked, before the final attachment check fails.
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    let root = guard(source.path());
    let current = root.root().join("changed");
    let sibling = root.root().join("changed-sibling");
    let space = Workspace::current_folder(scratch.path(), DISK).unwrap();
    let (db, _vfs) = open_results(&space).unwrap();
    let mut sink = ScanSink::new(&db, EmptyKind::Files).unwrap();
    sink.insert(&current.join("zero"), None).unwrap();
    sink.insert(&current.join("child").join("zero"), None)
        .unwrap();
    sink.insert(&sibling.join("wanted"), None).unwrap();
    let cancel = CancellationToken::default();
    sink.accept(
        direct::DirectBatch {
            batch: direct::ObservationBatch {
                parent: EntryId(1),
                sequence: 1,
                end: Some(ListingEnd::Incomplete),
                observations: &[],
                evidence: &[],
                properties: &[],
                directory: None,
                detached: true,
                errors: 1,
                denied: 0,
            },
            entries_seen: 0,
            parent_path: &current,
            last_error: None,
            ntfs: Default::default(),
            frontier_bytes: 0,
        },
        &cancel,
    )
    .unwrap();
    let summary = sink.finish(Attachment::Compatible, &cancel).unwrap();
    assert_eq!(summary.progress.matches, 1);
    assert_eq!(summary.coverage, ScopeCoverage::Partial);
    let raw: Vec<u8> = db
        .query_row("SELECT path FROM results", [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_recorded_path(&raw).unwrap(), sibling.join("wanted"));
}

#[test]
fn workspace_is_released_after_cancellation_and_quota_refusal() {
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    fs::File::create(source.path().join("zero")).unwrap();
    let cancel = CancellationToken::default();
    let cancelled = prepare(
        guard(source.path()),
        scratch.path(),
        DISK,
        EmptyKind::Files,
        &cancel,
        |_| cancel.cancel(),
    );
    assert!(
        matches!(cancelled, Err(Error::Io(error)) if error.kind() == io::ErrorKind::Interrupted)
    );
    let refused = prepare(
        guard(source.path()),
        scratch.path(),
        4096,
        EmptyKind::Directories,
        &CancellationToken::default(),
        |_| {},
    );
    assert!(
        matches!(refused, Err(Error::Io(error)) if error.kind() == io::ErrorKind::QuotaExceeded)
    );
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn native_non_unicode_paths_survive_and_links_do_not_make_a_directory_empty() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let source = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    let name = std::ffi::OsStr::from_bytes(b"zero\xff");
    fs::File::create(source.path().join(name)).unwrap();
    let cycle = source.path().join("cycle");
    fs::create_dir(&cycle).unwrap();
    symlink(source.path(), cycle.join("parent-link")).unwrap();
    symlink(source.path().join(name), cycle.join("file-link")).unwrap();
    let files = scan(&source, &scratch, EmptyKind::Files);
    let results = page(&files);
    assert_eq!(results.rows.len(), 1);
    assert_eq!(results.rows[0].path.file_name(), Some(name));
    let dirs = scan(&source, &scratch, EmptyKind::Directories);
    assert!(page(&dirs).rows.is_empty());
}
