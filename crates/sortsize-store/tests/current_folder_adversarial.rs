//! DUP-001/003/008, SCAN-006/009/010/015. Recette NTFS sur fixtures privées.
#![cfg(windows)]

use sortsize_core::{
    CancellationToken, RawPath,
    acquisition::{Guard, Policy, ScanObservation},
    duplicates::DuplicateOptions,
    resources::{Category, Governor},
};
use sortsize_store::inventory::{
    Error,
    queries::duplicates::{
        CurrentFolderResources, DuplicateFileProfile, DuplicateJob, VerificationPhase,
        prepare_current_folder, prepare_current_folder_with_profile,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    mem::MaybeUninit,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
};

const DISK_LIMIT: u64 = 32 * 1024 * 1024;
// The production content permit is process-wide and reports busy immediately.
static SERIAL: Mutex<()> = Mutex::new(());

struct Fixture {
    base: tempfile::TempDir,
    root: PathBuf,
    scratch: PathBuf,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let base = tempfile::Builder::new()
            .prefix("current-folder-")
            .tempdir()
            .unwrap();
        let root = base.path().join("Users");
        let scratch = base.path().join("scratch");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&scratch).unwrap();
        assert!(
            open(&root).scan_directory(&root).unwrap().is_ntfs_batch(),
            "the native suite needs TMP/TEMP on local NTFS"
        );
        Self {
            base,
            root,
            scratch,
            _serial: serial,
        }
    }

    fn prepare(&self, guard: &Arc<Guard>) -> DuplicateJob<'static, 'static> {
        let job = prepare_current_folder(
            Arc::clone(guard),
            &self.scratch,
            DISK_LIMIT,
            &CancellationToken::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(job.guard_roots(), &[guard.root().to_path_buf()]);
        assert_eq!(job.summary().bytes_read, 0);
        assert!(!job.summary().verified);
        job
    }

    fn assert_clean(&self) {
        assert_eq!(fs::read_dir(&self.scratch).unwrap().count(), 0);
    }
}

fn open(root: &Path) -> Guard {
    Guard::open(
        root,
        Policy {
            explicit_root: true,
            ..Policy::default()
        },
    )
    .unwrap()
}

fn verify(job: &mut DuplicateJob<'_, '_>, guard: &Guard) {
    job.verify_scoped(
        std::slice::from_ref(guard),
        DuplicateOptions::default(),
        &CancellationToken::default(),
    )
    .unwrap();
}

fn pair(root: &Path, data: &[u8]) {
    fs::write(root.join("a"), data).unwrap();
    fs::write(root.join("b"), data).unwrap();
}

#[test]
fn current_folder_profile_skips_empty_and_small_files_before_sqlite() {
    let f = Fixture::new();
    let size = DuplicateFileProfile::default().minimum_bytes();
    for (name, length) in [
        ("zero-a", 0),
        ("zero-b", 0),
        ("tiny", 1),
        ("just-below", size - 1),
        ("first-large", size),
        ("second-large", size),
    ] {
        File::create(f.root.join(name))
            .unwrap()
            .set_len(length)
            .unwrap();
    }
    let guard = Arc::new(open(&f.root));
    let cancel = CancellationToken::default();
    let resources = CurrentFolderResources {
        temporary_limit: Some(DISK_LIMIT),
        temporary_parent: Some(f.scratch.clone()),
        ..Default::default()
    };
    let mut last = None;
    let mut job = prepare_current_folder_with_profile(
        guard,
        &resources,
        DuplicateFileProfile::default(),
        &cancel,
        |progress| last = Some(*progress),
    )
    .unwrap();
    let progress = last.unwrap();
    assert_eq!(progress.examined_files, 6);
    assert_eq!(progress.excluded_empty, 2);
    assert_eq!(progress.excluded_by_size, 2);
    assert_eq!(progress.selected_files, 2);
    assert_eq!(progress.selected_logical_bytes, 2 * u128::from(size));
    assert_eq!(job.candidate_count().unwrap(), 2);
    assert_eq!(job.summary().files_hashed, 0);
    job.verify_current_folder_with_progress(Default::default(), &cancel, |_| {})
        .unwrap();
    assert!(job.summary().complete, "{:?}", job.summary());
    assert_eq!(job.summary().files_hashed, 2);
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.summary().excluded_empty, 2);
    assert_eq!(job.summary().excluded_by_size, 2);
    let groups = job.groups(None).unwrap();
    assert_eq!(groups[0].logical_bytes, size);
    assert_eq!(groups[0].independent_members, 2);
    drop(job);
    f.assert_clean();
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn write_at_time(path: &Path, data: &[u8], time: SystemTime) {
    fs::write(path, data).unwrap();
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
    assert_eq!(modified(path), time);
}

type NativeIdentity = (u32, u64);
type Partition = BTreeSet<BTreeSet<PathBuf>>;
type Groups = BTreeSet<(u64, Partition)>;

fn native_identity(file: &File) -> NativeIdentity {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: live handle, valid output and success checked before reading it.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
    assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
    let info = unsafe { info.assume_init() };
    (
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    )
}

// The stable fixture oracle reads the bytes and uses Windows file indices to
// partition hardlinks. It uses neither the candidate discovery nor its hashes.
fn oracle(root: &Path) -> Groups {
    let mut by_bytes = BTreeMap::<Vec<u8>, BTreeMap<NativeIdentity, BTreeSet<PathBuf>>>::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(path);
            } else {
                assert!(kind.is_file());
                let identity = native_identity(&File::open(&path).unwrap());
                by_bytes
                    .entry(fs::read(&path).unwrap())
                    .or_default()
                    .entry(identity)
                    .or_default()
                    .insert(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    by_bytes
        .into_iter()
        .filter(|(_, identities)| identities.len() > 1)
        .map(|(bytes, identities)| (bytes.len() as u64, identities.into_values().collect()))
        .collect()
}

fn assert_matches_oracle(job: &DuplicateJob<'_, '_>, root: &Path) {
    let summary = job.summary();
    assert!(summary.verified && summary.complete, "{summary:?}");
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.directory_listing_sources, 0);
    assert_eq!(summary.unverified_sources, 0);
    assert_eq!(summary.potentially_reclaimable_bytes, None);
    let mut actual = Groups::new();
    let mut paths_from_members = BTreeSet::new();
    let mut group_after = None;
    loop {
        let groups = job.groups(group_after).unwrap();
        assert!(groups.len() <= 100);
        if groups.is_empty() {
            break;
        }
        for group in groups {
            assert!(group.id > group_after.unwrap_or(0));
            group_after = Some(group.id);
            let mut member_after = None;
            let mut partition = BTreeMap::<u64, BTreeMap<u64, PathBuf>>::new();
            loop {
                let members = job.members(group.id, member_after).unwrap();
                assert!(members.len() <= 200);
                if members.is_empty() {
                    break;
                }
                for member in members {
                    assert!(member.cursor > member_after.unwrap_or(0));
                    member_after = Some(member.cursor);
                    let path = match member.path.raw {
                        Some(RawPath::WindowsUtf16(wide)) => {
                            PathBuf::from(OsString::from_wide(&wide))
                        }
                        None => PathBuf::from(member.path.display),
                        _ => panic!("unexpected path namespace"),
                    };
                    let relative = path.strip_prefix(root).unwrap().to_path_buf();
                    assert!(paths_from_members.insert((
                        group.id,
                        relative.clone(),
                        member.cursor != member.representative,
                    )));
                    partition
                        .entry(member.representative)
                        .or_default()
                        .insert(member.cursor, relative);
                }
            }
            assert_eq!(partition.len() as u64, group.independent_members);
            assert_eq!(
                partition.values().map(BTreeMap::len).sum::<usize>() as u64,
                group.paths
            );
            let partition = partition
                .into_iter()
                .map(|(representative, paths)| {
                    assert!(paths.contains_key(&representative));
                    paths.into_values().collect()
                })
                .collect();
            assert!(actual.insert((group.logical_bytes, partition)));
        }
    }
    assert_eq!(summary.groups, actual.len() as u64);
    assert_eq!(
        summary.independent_members,
        actual
            .iter()
            .map(|(_, partitions)| partitions.len() as u64)
            .sum::<u64>(),
    );
    assert_eq!(actual, oracle(root));

    // This is the projection used by the workshop. Check all continuation
    // pages and match the alias flag and native path to the member cursor API.
    let mut after = None;
    let mut paths_from_page = BTreeSet::new();
    loop {
        let page = job.file_page(after.as_ref(), false).unwrap();
        assert!(page.rows.len() <= 200);
        assert_eq!(page.rows.len(), page.labels.len());
        for (row, (group, _, alias)) in page.rows.iter().zip(page.labels) {
            let relative = row.path.strip_prefix(root).unwrap().to_path_buf();
            assert!(paths_from_page.insert((group, relative, alias)));
        }
        if !page.more {
            break;
        }
        after = Some(page.last.expect("missing continuation cursor"));
    }
    assert_eq!(paths_from_page, paths_from_members);
}

#[test]
fn fresh_strict_discovery_finds_new_paths_and_changed_sizes_in_the_selected_subtree() {
    let f = Fixture::new();
    let other = f.base.path().join("Users-old");
    fs::create_dir(&other).unwrap();
    fs::write(f.root.join("original"), b"new").unwrap();
    fs::write(f.root.join("old-name"), b"new").unwrap();
    let singleton = f.root.join("singleton");
    let external_alias = other.join("external-alias");
    fs::write(&singleton, b"old singleton length").unwrap();
    fs::hard_link(&singleton, &external_alias).unwrap();
    fs::write(other.join("external-copy"), b"new").unwrap();

    let listing = open(&f.root).with_scan_observation(ScanObservation::DirectoryListing);
    let mut before = listing.scan_directory(&f.root).unwrap();
    let rows = before
        .by_ref()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    let row = rows
        .iter()
        .find(|row| row.file_name() == "singleton")
        .unwrap();
    let old_size = before.observe(&listing, row, false, false).unwrap().length;
    assert_eq!(old_size, b"old singleton length".len() as u64);
    assert_eq!(rows.len(), 3);

    fs::write(&external_alias, b"new").unwrap();
    fs::rename(f.root.join("old-name"), f.root.join("renamed")).unwrap();
    let branch = f.root.join("new-branch");
    fs::create_dir(&branch).unwrap();
    fs::write(branch.join("new-copy"), b"new").unwrap();
    assert_eq!(fs::metadata(&singleton).unwrap().len(), 3);
    assert_eq!(
        before.observe(&listing, row, false, false).unwrap().length,
        old_size
    );
    // Index staleness of a *new* listing varies across NTFS cache states.
    let mut current_listing = listing.scan_directory(&f.root).unwrap();
    while let Some(row) = current_listing.next().transpose().unwrap() {
        if row.file_name() == "singleton" {
            let size = current_listing
                .observe(&listing, &row, false, false)
                .unwrap()
                .length;
            eprintln!("NTFS current index size={size}, strict size=3, old size={old_size}");
        }
    }

    let guard = Arc::new(open(&f.root));
    let mut job = f.prepare(&guard);
    assert_eq!(job.candidate_count().unwrap(), 4);
    verify(&mut job, &guard);
    assert_matches_oracle(&job, &f.root);
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.summary().independent_members, 4);
    assert_eq!(job.summary().aliases, 0);
    drop(job);
    f.assert_clean();
}

#[test]
fn complete_membership_and_aliases_across_group_and_member_pages() {
    let f = Fixture::new();
    for value in 0_u32..101 {
        let bytes = value.to_le_bytes();
        fs::write(f.root.join(format!("pair-{value}-a")), bytes).unwrap();
        fs::write(f.root.join(format!("pair-{value}-b")), bytes).unwrap();
    }
    for value in 0..513 {
        fs::write(f.root.join(format!("empty-{value:04}")), []).unwrap();
    }
    let ill_formed = OsString::from_wide(&[b'n' as u16, 0xd800]);
    assert_eq!(ill_formed.encode_wide().last(), Some(0xd800));
    let original = f.root.join("copy-original");
    fs::write(&original, b"a copy").unwrap();
    fs::write(f.root.join("copy-independent"), b"a copy").unwrap();
    fs::hard_link(&original, f.root.join(&ill_formed)).unwrap();
    fs::write(f.root.join("alias-original"), b"aliases only").unwrap();
    fs::hard_link(f.root.join("alias-original"), f.root.join("alias-only")).unwrap();
    write_at_time(
        &f.root.join("different"),
        b"else",
        modified(&f.root.join("pair-0-a")),
    );

    let guard = Arc::new(open(&f.root));
    let too_small = prepare_current_folder(
        Arc::clone(&guard),
        &f.scratch,
        128 * 1024,
        &CancellationToken::default(),
        |_| {},
    );
    assert!(matches!(
        too_small,
        Err(Error::DuplicateTemporaryLimit { limit }) if limit == 128 * 1024
    ));
    f.assert_clean();
    let mut job = f.prepare(&guard);
    verify(&mut job, &guard);
    assert_matches_oracle(&job, &f.root);
    assert_eq!(job.summary().groups, 103);
    assert_eq!(job.summary().aliases, 2);
    let usage = job.temporary_usage();
    assert!(usage.charged <= usage.peak && usage.peak <= DISK_LIMIT);
    drop(job);
    f.assert_clean();
}

#[test]
fn cancellation_retains_a_fully_published_multibatch_group() {
    let f = Fixture::new();
    for value in 0..513 {
        fs::write(f.root.join(format!("empty-{value:04}")), []).unwrap();
    }
    pair(&f.root, b"still unverified");
    let guard = Arc::new(open(&f.root));
    let mut job = f.prepare(&guard);
    let cancel = CancellationToken::default();
    job.verify_scoped_with_progress(
        std::slice::from_ref(guard.as_ref()),
        DuplicateOptions::default(),
        &cancel,
        |progress| {
            if progress.verification_phase == Some(VerificationPhase::Comparing)
                && progress.groups == 1
            {
                cancel.cancel();
            }
        },
    )
    .unwrap();

    let summary = job.summary();
    assert!(summary.cancelled && !summary.complete, "{summary:?}");
    assert_eq!(summary.groups, 1);
    let groups = job.groups(None).unwrap();
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(group.logical_bytes, 0);
    assert_eq!(group.independent_members, 513);
    let mut after = None;
    let mut occurrences = 0;
    loop {
        let page = job.members(group.id, after).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page[0].cursor > after.unwrap_or_default());
        assert!(
            page.iter()
                .all(|member| member.cursor == member.representative)
        );
        after = page.last().map(|member| member.cursor);
        occurrences += page.len();
    }
    assert_eq!(occurrences, 513);
    drop(job);
    f.assert_clean();
}

#[test]
fn same_size_and_mtime_do_not_cache_content_or_approve_a_mutation_after_hashing() {
    let f = Fixture::new();
    let a = f.root.join("a");
    let b = f.root.join("b");
    pair(&f.root, b"same");
    let time = modified(&a);
    write_at_time(&b, b"else", time);
    let guard = Arc::new(open(&f.root));
    let mut old = f.prepare(&guard);
    verify(&mut old, &guard);
    assert!(old.summary().complete);
    assert_eq!(old.summary().groups, 0);
    drop(old);

    let mut current = f.prepare(&guard);
    write_at_time(&b, b"same", time);
    verify(&mut current, &guard);
    assert_matches_oracle(&current, &f.root);
    assert_eq!(current.summary().groups, 1);
    drop(current);

    let mut in_flight = f.prepare(&guard);
    let mut mutated = false;
    in_flight
        .verify_scoped_with_progress(
            std::slice::from_ref(guard.as_ref()),
            DuplicateOptions::default(),
            &CancellationToken::default(),
            |progress| {
                if !mutated && progress.files_hashed == 2 {
                    assert_eq!(progress.groups, 0);
                    write_at_time(&b, b"else", time);
                    mutated = true;
                }
            },
        )
        .unwrap();
    assert!(mutated);
    assert_eq!(fs::read(&b).unwrap(), b"else");
    let summary = in_flight.summary();
    assert!(!summary.complete && summary.errors > 0, "{summary:?}");
    assert_eq!(summary.groups, 0);
    drop(in_flight);
    f.assert_clean();
}

struct Junction(PathBuf);

impl Junction {
    fn create(link: &Path, target: &Path) -> Self {
        let output = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "New-Item -ItemType Junction -Path $env:BUNNY_TEST_LINK -Target $env:BUNNY_TEST_TARGET -ErrorAction Stop | Out-Null",
            ])
            .env("BUNNY_TEST_LINK", link)
            .env("BUNNY_TEST_TARGET", target)
            .env_remove("PSModulePath")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        Self(link.to_path_buf())
    }
}

impl Drop for Junction {
    fn drop(&mut self) {
        fs::remove_dir(&self.0).expect("remove fixture junction");
    }
}

#[test]
fn reparse_entries_are_not_descended_and_replacing_a_parent_by_a_junction_is_rejected() {
    let f = Fixture::new();
    let outside = f.base.path().join("outside");
    fs::create_dir(&outside).unwrap();
    pair(&outside, b"same");
    let child = f.root.join("child");
    let guard = Arc::new(open(&f.root));
    let link = Junction::create(&child, &outside);
    let mut existing = f.prepare(&guard);
    verify(&mut existing, &guard);
    assert_eq!(existing.candidate_count().unwrap(), 0);
    assert_eq!(existing.summary().bytes_read, 0);
    assert_eq!(existing.summary().groups, 0);
    drop(existing);
    drop(link);

    fs::create_dir(&child).unwrap();
    pair(&child, b"same");
    let mut replaced = f.prepare(&guard);
    assert_eq!(replaced.candidate_count().unwrap(), 2);
    let moved = f.base.path().join("moved-child");
    fs::rename(&child, &moved).unwrap();
    let link = Junction::create(&child, &moved);
    // Windows resolves the same files at the old spelling through the junction.
    assert_eq!(fs::read(child.join("a")).unwrap(), b"same");
    verify(&mut replaced, &guard);
    let summary = replaced.summary();
    assert!(!summary.complete && summary.errors > 0, "{summary:?}");
    assert_eq!(summary.bytes_read, 0);
    assert_eq!(summary.groups, 0);
    drop(replaced);
    drop(link);
    f.assert_clean();
}

#[test]
fn missing_and_replaced_roots_cannot_be_adopted_by_a_prepared_job() {
    let f = Fixture::new();
    pair(&f.root, b"same");
    let guard = Arc::new(open(&f.root));
    let mut missing = f.prepare(&guard);
    let mut replaced = f.prepare(&guard);
    let mut reopened = f.prepare(&guard);
    fs::rename(&f.root, f.base.path().join("previous-root")).unwrap();
    missing
        .verify_available_scoped_with_progress(
            &[None],
            DuplicateOptions::default(),
            &CancellationToken::default(),
            |_| {},
        )
        .unwrap();
    assert!(!missing.summary().complete);
    assert_eq!(missing.summary().bytes_read, 0);
    assert_eq!(missing.summary().groups, 0);
    assert!(missing.scope_errors().unwrap()[0] > 0);
    drop(missing);

    fs::create_dir(&f.root).unwrap();
    pair(&f.root, b"same");
    verify(&mut replaced, &guard);
    assert!(!replaced.summary().complete);
    assert_eq!(replaced.summary().bytes_read, 0);
    assert_eq!(replaced.summary().groups, 0);
    assert!(replaced.scope_errors().unwrap()[0] > 0);
    drop(replaced);

    let current_guard = open(&f.root);
    // A new guard has the same path but authorizes a different root object.
    assert_ne!(
        current_guard.evidence().1.identity,
        guard.evidence().1.identity
    );
    let verification = reopened.verify_scoped(
        &[current_guard],
        DuplicateOptions::default(),
        &CancellationToken::default(),
    );
    assert!(verification.is_err() || reopened.scope_errors().unwrap()[0] > 0);
    assert!(!reopened.summary().complete);
    assert_eq!(reopened.summary().bytes_read, 0);
    assert_eq!(reopened.summary().groups, 0);
    drop(reopened);
    f.assert_clean();
}

#[test]
fn late_root_detachment_retracts_every_group_after_content_verification() {
    let f = Fixture::new();
    pair(&f.root, b"same");
    fs::write(f.root.join("c"), b"other").unwrap();
    fs::write(f.root.join("d"), b"other").unwrap();
    let guard = Arc::new(open(&f.root));
    let mut job = f.prepare(&guard);
    let moved = f.base.path().join("detached-root");
    let mut detached = false;
    // Four file hashes and a byte comparison of both copies for each pair.
    let verified_bytes = 4 * (b"same".len() + b"other".len()) as u128;
    job.verify_scoped_with_progress(
        std::slice::from_ref(guard.as_ref()),
        DuplicateOptions::default(),
        &CancellationToken::default(),
        |progress| {
            if !detached && progress.files_hashed == 4 && progress.bytes_read == verified_bytes {
                // The first group is already recorded. The final comparison
                // has succeeded and its group is about to be recorded.
                assert_eq!(progress.groups, 1);
                fs::rename(&f.root, &moved).unwrap();
                fs::create_dir(&f.root).unwrap();
                detached = true;
            }
        },
    )
    .unwrap();
    assert!(detached);
    assert_eq!(fs::read(moved.join("d")).unwrap(), b"other");
    assert_ne!(
        open(&f.root).evidence().1.identity,
        guard.evidence().1.identity
    );
    let summary = job.summary();
    assert!(summary.verified && !summary.complete, "{summary:?}");
    assert!(!summary.source_independence_verified);
    assert_eq!(summary.files_hashed, 4);
    assert_eq!(summary.bytes_read, verified_bytes);
    assert!(job.scope_errors().unwrap()[0] > 0);
    assert_eq!(summary.groups, 0);
    assert_eq!(summary.independent_members, 0);
    assert!(job.groups(None).unwrap().is_empty());
    assert!(job.file_page(None, false).unwrap().rows.is_empty());
    drop(job);
    f.assert_clean();
}

#[test]
fn a_fresh_regular_subtree_is_allowed_but_a_prepared_candidate_cannot_switch_identity() {
    let f = Fixture::new();
    let child = f.root.join("child");
    fs::create_dir(&child).unwrap();
    pair(&child, b"same");
    let times = [modified(&child.join("a")), modified(&child.join("b"))];
    let guard = Arc::new(open(&f.root));
    let mut prepared = f.prepare(&guard);
    fs::rename(&child, f.base.path().join("previous-child")).unwrap();
    fs::create_dir(&child).unwrap();
    for (name, time) in ["a", "b"].into_iter().zip(times) {
        write_at_time(&child.join(name), b"same", time);
    }
    verify(&mut prepared, &guard);
    let summary = prepared.summary();
    assert!(!summary.complete && summary.errors > 0, "{summary:?}");
    assert_eq!(summary.groups, 0);
    assert_eq!(summary.bytes_read, 0);
    drop(prepared);

    let mut fresh = f.prepare(&guard);
    verify(&mut fresh, &guard);
    assert_matches_oracle(&fresh, &f.root);
    assert_eq!(fresh.summary().groups, 1);
    drop(fresh);
    f.assert_clean();
}

struct DenyRead(PathBuf);

fn set_acl(path: &Path, args: &[&str]) {
    let output = Command::new("icacls.exe")
        .arg(path)
        .args(args)
        .arg("/Q")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
}

impl DenyRead {
    fn new(path: &Path) -> Self {
        set_acl(path, &["/deny", "*S-1-1-0:(RD)"]);
        Self(path.to_path_buf())
    }
}

impl Drop for DenyRead {
    fn drop(&mut self) {
        set_acl(&self.0, &["/remove:d", "*S-1-1-0"]);
    }
}

#[test]
fn file_size_limit_skips_large_candidates_before_opening_content() {
    let f = Fixture::new();
    pair(&f.root, b"same");
    let denied_file = f.root.join("large-denied");
    fs::write(&denied_file, b"large candidate").unwrap();
    fs::write(f.root.join("large-copy"), b"large candidate").unwrap();
    let deny_read = DenyRead::new(&denied_file);
    assert_eq!(
        fs::read(&denied_file).unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
    let guard = Arc::new(open(&f.root));
    let mut job = f.prepare(&guard);
    assert_eq!(job.summary().errors, 0);
    assert_eq!(job.summary().selected_files, 4);
    job.verify_scoped(
        std::slice::from_ref(guard.as_ref()),
        DuplicateOptions {
            max_file_bytes: Some(4),
            ..DuplicateOptions::without_content_limits()
        },
        &CancellationToken::default(),
    )
    .unwrap();
    let summary = job.summary();
    assert_eq!(summary.file_size_limit, Some(4));
    assert_eq!(summary.total_read_limit, None);
    assert_eq!(summary.error_limit, None);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.unverified_files, Some(2));
    assert_eq!(summary.groups, 1);
    assert_eq!(summary.bytes_read, 16);
    assert!(!summary.complete);
    drop(job);
    drop(deny_read);
    f.assert_clean();
}

#[test]
fn verification_stops_at_exactly_one_hundred_real_errors() {
    let f = Fixture::new();
    let paths = (0..101)
        .map(|i| f.root.join(format!("stale-{i:03}")))
        .collect::<Vec<_>>();
    for path in &paths {
        fs::write(path, b"same").unwrap();
    }
    let guard = Arc::new(open(&f.root));
    let mut job = f.prepare(&guard);
    assert_eq!(job.summary().errors, 0);
    assert_eq!(job.summary().selected_files, 101);
    for path in paths {
        fs::remove_file(path).unwrap();
    }
    job.verify_scoped(
        std::slice::from_ref(guard.as_ref()),
        DuplicateOptions::without_content_limits(),
        &CancellationToken::default(),
    )
    .unwrap();
    let summary = job.summary();
    assert_eq!(summary.error_limit, Some(100));
    assert_eq!(summary.errors, 100);
    assert_eq!(summary.unverified_files, Some(101));
    assert_eq!(summary.groups, 0);
    assert_eq!(summary.bytes_read, 0);
    assert!(!summary.budget_limited && !summary.cancelled && !summary.complete);
    drop(job);
    f.assert_clean();
}

#[test]
fn denied_discovery_and_content_failures_keep_scope_partial_with_positive_groups() {
    let f = Fixture::new();
    let dir = f.root.join("private");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("hidden"), b"same").unwrap();
    pair(&f.root, b"same");
    let denied_file = f.root.join("no-content");
    fs::write(&denied_file, b"zzzz").unwrap();
    let deny_directory = DenyRead::new(&dir);
    let deny_file = DenyRead::new(&denied_file);
    assert_eq!(
        fs::read_dir(&dir).err().unwrap().kind(),
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        fs::read(&denied_file).unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );

    let guard = Arc::new(open(&f.root));
    let mut job = f.prepare(&guard);
    let acquisition_errors = job.summary().errors;
    assert!(acquisition_errors >= 1);
    verify(&mut job, &guard);
    assert!(!job.summary().complete);
    assert!(
        job.summary().errors > acquisition_errors,
        "{:?}",
        job.summary()
    );
    assert_eq!(job.summary().groups, 1);
    assert_eq!(job.scope_errors().unwrap(), vec![job.summary().errors]);
    assert!(
        job.diagnostics()
            .unwrap()
            .iter()
            .all(|(scope, _)| *scope == 0)
    );
    drop(job);
    drop(deny_file);
    drop(deny_directory);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    assert_eq!(fs::read(&denied_file).unwrap(), b"zzzz");
    f.assert_clean();
}

#[test]
fn transport_admission_reports_the_combined_minimum_after_releasing_the_batch() {
    const AVAILABLE: u64 = 6 * 1024 * 1024;
    const MINIMUM: u64 = 8 * 1024 * 1024;
    let f = Fixture::new();
    pair(&f.root, b"same");
    let guard = Arc::new(open(&f.root));
    let governor = Governor::global();
    let mut reservation = governor
        .reserve_up_to(Category::Transport, AVAILABLE, u64::MAX)
        .unwrap();
    assert!(reservation.shrink_to(reservation.bytes() - AVAILABLE));

    let result = prepare_current_folder(
        guard,
        &f.scratch,
        DISK_LIMIT,
        &CancellationToken::default(),
        |_| {},
    );
    assert!(matches!(
        result,
        Err(Error::MemoryAdmission {
            minimum: MINIMUM,
            available: AVAILABLE,
            category_limit,
        }) if category_limit == governor.usage().limits.categories[Category::Transport as usize]
    ));
    drop(reservation);
    f.assert_clean();
}

#[test]
fn memory_and_content_budgets_and_cancellation_leave_no_unverified_group_or_scratch() {
    let f = Fixture::new();
    pair(&f.root, b"same");
    let guard = Arc::new(open(&f.root));
    let cancel = CancellationToken::default();
    let governor = Governor::global();
    let reservation = governor
        .reserve_up_to(Category::Queries, 1, u64::MAX)
        .unwrap();
    let budgeted =
        prepare_current_folder(Arc::clone(&guard), &f.scratch, DISK_LIMIT, &cancel, |_| {});
    assert!(matches!(
        budgeted,
        Err(Error::MemoryAdmission {
            minimum,
            available: 0,
            category_limit,
        }) if minimum == 1024 * 1024
            && category_limit == governor.usage().limits.categories[Category::Queries as usize]
    ));
    drop(reservation);
    f.assert_clean();

    let mut acquisition_stopped = false;
    let stopped = prepare_current_folder(
        Arc::clone(&guard),
        &f.scratch,
        DISK_LIMIT,
        &cancel,
        |progress| {
            if progress.selected_files > 0 {
                acquisition_stopped = true;
                cancel.cancel();
            }
        },
    );
    assert!(acquisition_stopped);
    assert!(matches!(stopped, Err(Error::Io(error)) if error.kind() == ErrorKind::Interrupted));
    f.assert_clean();

    let mut limited = f.prepare(&guard);
    limited
        .verify_scoped(
            std::slice::from_ref(guard.as_ref()),
            DuplicateOptions {
                max_bytes_read: Some(1),
                ..DuplicateOptions::default()
            },
            &CancellationToken::default(),
        )
        .unwrap();
    let summary = limited.summary();
    assert!(summary.budget_limited && !summary.complete, "{summary:?}");
    assert_eq!(summary.groups, 0);
    assert!(summary.bytes_read <= 1);
    drop(limited);

    let mut cancelled = f.prepare(&guard);
    let cancel = CancellationToken::default();
    cancelled
        .verify_scoped_with_progress(
            std::slice::from_ref(guard.as_ref()),
            DuplicateOptions::default(),
            &cancel,
            |progress| {
                if progress.files_hashed == 1 {
                    cancel.cancel();
                }
            },
        )
        .unwrap();
    let summary = cancelled.summary();
    assert!(summary.cancelled && !summary.complete, "{summary:?}");
    assert_eq!(summary.files_hashed, 1);
    assert_eq!(summary.groups, 0);
    assert!(cancelled.groups(None).unwrap().is_empty());
    drop(cancelled);
    f.assert_clean();
}
