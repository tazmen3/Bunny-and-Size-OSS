//! Windows qualification with injected occurrences. Only the empty guard root is native.
use super::*;
use sha2::{Digest, Sha256};
use sortsize_core::{LocalFileId, RawPath};
use std::{
    ffi::OsString,
    fs,
    os::windows::ffi::{OsStrExt, OsStringExt},
};
use windows_sys::Win32::System::{
    ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
    Threading::GetCurrentProcess,
};

const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    Full,
    QuotaDiscovery,
    QuotaVerification,
    MemoryRefusal,
    CancelDiscovery,
    CancelHashing,
    CancelComparing,
}

impl Scenario {
    fn parse(value: &str) -> Self {
        match value {
            "full" => Self::Full,
            "quota-discovery" => Self::QuotaDiscovery,
            "quota-verification" => Self::QuotaVerification,
            "memory-refusal" => Self::MemoryRefusal,
            "cancel-discovery" => Self::CancelDiscovery,
            "cancel-hashing" => Self::CancelHashing,
            "cancel-comparing" => Self::CancelComparing,
            _ => panic!("unknown SORTSIZE_DUP_QUAL_CASE: {value}"),
        }
    }
}

fn number_setting(key: &str, default: u64, min: u64, max: u64) -> u64 {
    let value = match std::env::var(key) {
        Ok(value) => value.parse().expect("decimal qualification setting"),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{key}: {error}"),
    };
    assert!((min..=max).contains(&value), "{key}: {value}");
    value
}

#[derive(Clone, Copy)]
struct Corpus {
    count: u64,
    directory_depth: u64,
}

impl Corpus {
    fn end_of_empty(self) -> u64 {
        self.count / 2
    }

    fn end_of_pairs(self) -> u64 {
        3 * self.count / 4
    }

    fn size(self, id: u64) -> u64 {
        if id <= self.end_of_empty() {
            0
        } else if id <= self.end_of_pairs() {
            8
        } else {
            1024 + id
        }
    }

    fn representative(self, id: u64) -> u64 {
        if id <= self.end_of_empty() && id.is_multiple_of(16) {
            id - 1
        } else {
            id
        }
    }

    fn content_class(self, id: u64) -> u64 {
        if id <= self.end_of_empty() {
            0
        } else {
            (id - self.end_of_empty()).div_ceil(2)
        }
    }

    fn directory(self, root: &Path) -> PathBuf {
        let component = "é層".repeat(72);
        let mut path = root.to_owned();
        for _ in 0..self.directory_depth {
            path.push(&component);
        }
        path
    }
}

fn occurrence_name(id: u64) -> OsString {
    let mut units: Vec<u16> = format!("f{id:010}-").encode_utf16().collect();
    units.extend([0xd800, u16::from(b'x')]);
    OsString::from_wide(&units)
}

fn occurrence_id(path: &Path) -> u64 {
    path.file_name()
        .unwrap()
        .encode_wide()
        .skip(1)
        .take(10)
        .fold(0, |id, digit| id * 10 + u64::from(digit - u16::from(b'0')))
}

struct SyntheticVerifier<'a> {
    corpus: Corpus,
    scenario: Scenario,
    cancel: &'a CancellationToken,
    halted: Option<Instant>,
    hashed: u64,
    compared: u64,
    fail_next_publication: Option<Arc<workspace::Workspace>>,
}

impl SyntheticVerifier<'_> {
    fn stop(&mut self) -> io::Error {
        self.halted = Some(Instant::now());
        self.cancel.cancel();
        io::ErrorKind::Interrupted.into()
    }
}

impl Verification for SyntheticVerifier<'_> {
    fn observe(&self, _: usize, _: &Path, _: u64, _: Option<SystemTime>) -> io::Result<Proof> {
        panic!("strict acquired occurrence evidence was missing")
    }

    fn observe_current(
        &self,
        _: usize,
        path: &Path,
        length: u64,
        _: Option<SystemTime>,
        evidence: &[u8],
    ) -> io::Result<Proof> {
        let id = occurrence_id(path);
        let representative = self.corpus.representative(id);
        let mut identity = vec![1];
        identity.extend(97u64.to_be_bytes());
        identity.extend(u128::from(representative).to_be_bytes());
        assert_eq!(length, self.corpus.size(id));
        assert_eq!(&evidence[1..=25], identity.as_slice());
        Proof::decode(
            &serde_json::to_vec(&serde_json::json!({
                "identity": identity, "length": length,
                "before": false, "seconds": 0, "nanos": 0, "change": [0, 0],
            }))
            .unwrap(),
        )
    }

    fn hash(
        &mut self,
        _: usize,
        path: &Path,
        _: &Proof,
        _: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<[u8; 32]> {
        if self.scenario == Scenario::CancelHashing && self.hashed == 257 {
            return Err(self.stop());
        }
        let mut digest = [0; 32];
        digest[..8].copy_from_slice(&self.corpus.content_class(occurrence_id(path)).to_be_bytes());
        self.hashed += 1;
        Ok(digest)
    }

    fn validate(&self, _: usize, _: &Path, _: &Proof) -> io::Result<()> {
        Ok(())
    }

    fn equal(
        &mut self,
        left: (usize, &Path, &Proof),
        right: (usize, &Path, &Proof),
        _: &[u8; 32],
        _: &mut dyn FnMut(DuplicateProgress),
    ) -> io::Result<bool> {
        let left_id = occurrence_id(left.1);
        let right_id = occurrence_id(right.1);
        if self.scenario == Scenario::CancelComparing && left_id > self.corpus.end_of_empty() {
            return Err(self.stop());
        }
        assert_ne!(left.2.identity(), right.2.identity());
        self.compared += 1;
        if left_id > self.corpus.end_of_empty()
            && let Some(space) = self.fail_next_publication.take()
        {
            space.inject("vfs_full");
        }
        Ok(self.corpus.content_class(left_id) == self.corpus.content_class(right_id))
    }

    fn progress(&self) -> DuplicateProgress {
        DuplicateProgress {
            bytes_read: 0,
            files_hashed: self.hashed,
        }
    }
}

fn integers(transcript: &mut Sha256, values: &[u64]) {
    for value in values {
        transcript.update(value.to_be_bytes());
    }
}

fn location(transcript: &mut Sha256, root: &Path, path: &Path) {
    let relative = path
        .strip_prefix(root)
        .expect("member outside injected root");
    let bytes: Vec<u8> = relative
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    integers(transcript, &[bytes.len() as u64 / 2]);
    transcript.update(bytes);
}

#[derive(Default)]
struct Walk {
    pages: u64,
    rows: u64,
    digest: Sha256,
}

impl Walk {
    fn receipt(self) -> serde_json::Value {
        serde_json::json!({
            "pages": self.pages, "rows": self.rows,
            "sha256": format!("{:x}", self.digest.finalize()),
        })
    }
}

fn presentation(job: &DuplicateJob<'_, '_>, root: &Path, reverse: bool) -> Result<Walk> {
    let acquisition = job.current.as_ref().unwrap();
    let mut walk = Walk::default();
    let mut cursor = reverse.then(|| DuplicateCursor {
        owner: job.owner.clone(),
        group: u64::MAX,
        entry: u64::MAX,
        reference: None,
        role: 0,
    });
    let mut previous = None;
    loop {
        let page = job.page(cursor.as_ref(), reverse)?;
        walk.pages += 1;
        assert!(page.rows.len() <= 200);
        assert_eq!(page.rows.len(), page.labels.len());
        assert_eq!(page.rows.len(), page.sides.len());
        assert_eq!(page.rows.len(), page.sources.len());
        let mut add = |index: usize| {
            let row = &page.rows[index];
            let (group, representative, alias) = page.labels[index];
            let id = row.key.entry.0;
            let address = (group, id);
            if let Some(old) = previous {
                assert!(if reverse {
                    old > address
                } else {
                    old < address
                });
            }
            previous = Some(address);
            assert_eq!(row.key.inventory, InventoryId(0));
            assert_eq!(row.key.generation, acquisition.generation);
            assert_eq!(page.sides[index], 0);
            assert_eq!(page.sources[index], acquisition.source);
            assert_eq!(occurrence_id(&row.path), id);
            integers(
                &mut walk.digest,
                &[
                    group,
                    id,
                    representative,
                    row.logical.unwrap(),
                    u64::from(alias),
                ],
            );
            location(&mut walk.digest, root, &row.path);
            walk.rows += 1;
        };
        if reverse {
            (0..page.rows.len()).rev().for_each(&mut add);
        } else {
            (0..page.rows.len()).for_each(&mut add);
        }
        let next = if reverse { page.first } else { page.last };
        if let Some(next) = next {
            // Also walk beyond the final nonempty page to check exact EOF.
            let more = page.more;
            if !more {
                let eof = job.page(Some(&next), reverse)?;
                assert!(eof.rows.is_empty() && !eof.more);
                walk.pages += 1;
                break;
            }
            cursor = Some(next);
        } else {
            assert!(page.rows.is_empty() && !page.more);
            break;
        }
    }
    Ok(walk)
}

fn groups_and_members(job: &DuplicateJob<'_, '_>, root: &Path) -> Result<(Walk, Walk)> {
    let acquisition = job.current.as_ref().unwrap();
    let mut groups = Walk::default();
    let mut members = Walk::default();
    let mut after_group = None;
    loop {
        let page = job.groups(after_group)?;
        groups.pages += 1;
        assert!(page.len() <= 100);
        if page.is_empty() {
            break;
        }
        for group in page {
            assert!(group.id > after_group.unwrap_or_default());
            after_group = Some(group.id);
            integers(
                &mut groups.digest,
                &[
                    group.id,
                    group.logical_bytes,
                    group.independent_members,
                    group.paths,
                ],
            );
            groups.rows += 1;
            let start = members.rows;
            let mut after_member = None;
            loop {
                let page = job.members(group.id, after_member)?;
                members.pages += 1;
                assert!(page.len() <= 200);
                if page.is_empty() {
                    break;
                }
                for member in page {
                    assert!(member.cursor > after_member.unwrap_or_default());
                    after_member = Some(member.cursor);
                    assert_eq!(member.entry, member.cursor);
                    assert_eq!(member.key.entry.0, member.cursor);
                    assert_eq!(member.key.inventory, InventoryId(0));
                    assert_eq!(member.key.generation, acquisition.generation);
                    assert_eq!(member.source, acquisition.source);
                    assert_eq!(member.side, 0);
                    integers(
                        &mut members.digest,
                        &[group.id, member.entry, member.representative],
                    );
                    let Some(RawPath::WindowsUtf16(units)) = member.path.raw else {
                        panic!("native UTF-16 units missing")
                    };
                    location(
                        &mut members.digest,
                        root,
                        &PathBuf::from(OsString::from_wide(&units)),
                    );
                    members.rows += 1;
                }
            }
            assert_eq!(members.rows - start, group.paths);
        }
    }
    Ok((groups, members))
}

fn process_peak() -> serde_json::Value {
    let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    memory.cb = size;
    assert_ne!(
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, size) },
        0,
        "GetProcessMemoryInfo"
    );
    serde_json::json!({
        "peak_working_set_bytes": memory.PeakWorkingSetSize,
        "peak_commit_bytes": memory.PeakPagefileUsage,
    })
}

fn governor_usage() -> serde_json::Value {
    let usage = Governor::global().usage();
    serde_json::json!({
        "memory_limit_bytes": usage.limits.bytes,
        "memory_categories_bytes": usage.categories,
        "disk_reserved_bytes": usage.disk_reserved,
        "disk_used_bytes": usage.disk_used,
        "disk_limit_bytes": usage.disk_limit,
    })
}

struct ByteLimitReset;

impl Drop for ByteLimitReset {
    fn drop(&mut self) {
        Governor::global().set_byte_limit(None);
    }
}

struct Settings {
    case: String,
    scenario: Scenario,
    memory_limit: Option<u64>,
    global_memory_limit: Option<u64>,
    temporary_limit: Option<u64>,
    scratch: tempfile::TempDir,
}

impl Settings {
    fn read(source: Option<&Guard>) -> Self {
        let case = std::env::var("SORTSIZE_DUP_QUAL_CASE").unwrap_or_else(|_| "full".into());
        let scenario = Scenario::parse(&case);
        let default_memory = if scenario == Scenario::MemoryRefusal {
            7
        } else {
            128
        };
        let memory = number_setting("SORTSIZE_DUP_QUAL_MEMORY_MIB", default_memory, 0, 4096);
        let global = number_setting("SORTSIZE_DUP_QUAL_GLOBAL_MIB", 128, 0, 4096);
        let default_temporary = if scenario == Scenario::QuotaDiscovery {
            2
        } else {
            0
        };
        let temporary = number_setting("SORTSIZE_DUP_QUAL_TEMP_MIB", default_temporary, 0, 24_576);
        let parent = std::env::var_os("SORTSIZE_DUP_QUAL_SCRATCH")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        local_state_location(&parent).unwrap();
        if let Some(source) = source {
            assert!(
                source.work_path_allowed(&parent).unwrap(),
                "scratch parent inside fixture"
            );
        }
        let scratch = tempfile::Builder::new()
            .prefix("bunny-duplicate-qualification-")
            .tempdir_in(parent)
            .unwrap();
        let memory_limit = (memory != 0).then_some(memory * MIB);
        let temporary_limit = (temporary != 0).then_some(temporary * MIB);
        let global_memory_limit = (global != 0).then_some(global * MIB);
        Governor::global().set_byte_limit(global_memory_limit);
        Self {
            case,
            scenario,
            memory_limit,
            global_memory_limit,
            temporary_limit,
            scratch,
        }
    }

    fn resources(&self) -> CurrentFolderResources {
        CurrentFolderResources {
            memory_limit: self.memory_limit,
            temporary_limit: self.temporary_limit,
            temporary_parent: Some(self.scratch.path().to_owned()),
        }
    }

    fn remaining(&self) -> usize {
        fs::read_dir(self.scratch.path())
            .unwrap()
            .try_fold(0, |count, entry| entry.map(|_| count + 1))
            .unwrap()
    }
}

fn clear(settings: &Settings, initial: &serde_json::Value) -> serde_json::Value {
    assert_eq!(settings.remaining(), 0, "temporary workspace residue");
    let final_usage = governor_usage();
    for key in [
        "memory_categories_bytes",
        "disk_reserved_bytes",
        "disk_used_bytes",
    ] {
        assert_eq!(final_usage[key], initial[key], "resource leak: {key}");
    }
    final_usage
}

fn preparation_stop(
    scenario: Scenario,
    error: &Error,
    observed: u64,
    cancellation: Option<Instant>,
) -> serde_json::Value {
    let resources = match (scenario, error) {
        (Scenario::QuotaDiscovery, Error::DuplicateResources(usage))
            if usage.cause == Some(DuplicateResourceCause::TemporaryLimit) && observed > 0 =>
        {
            Some(usage)
        }
        (Scenario::MemoryRefusal, Error::DuplicateResources(usage))
            if usage.cause == Some(DuplicateResourceCause::Memory) && observed == 0 =>
        {
            Some(usage)
        }
        (Scenario::CancelDiscovery, Error::Io(error))
            if error.kind() == io::ErrorKind::Interrupted
                && observed > 0
                && cancellation.is_some() =>
        {
            None
        }
        _ => panic!("unexpected preparation failure ({scenario:?}, {observed}): {error:?}"),
    };
    serde_json::json!({
        "message": error.to_string(), "resources": resources,
    })
}

fn prepare_synthetic(
    settings: &Settings,
    guard: Arc<Guard>,
    cancel: &CancellationToken,
    mut discover: impl FnMut(&mut CandidateSink<'_>, &Arc<workspace::Workspace>) -> Result<()>,
) -> Result<DuplicateJob<'static, 'static>> {
    check_cancel(cancel)?;
    check_guard(&guard)?;
    let (space, memory, mut usage) = admit_resources(&settings.resources(), cancel)?;
    let result = prepare_in_space(
        guard,
        &space,
        memory,
        Some(usage),
        cancel,
        &mut |_| {},
        CandidateDiscovery {
            profile: None,
            synthetic: Some(&mut discover),
        },
    );
    result.map_err(|error| {
        usage.refresh(&space);
        if let Some(cause) = usage.cause {
            resource_error(usage, cause)
        } else if is_temporary_limit(&error) {
            resource_error(usage, native_resource_cause(&error))
        } else {
            error
        }
    })
}

#[test]
#[ignore = "Windows release qualification; optional multi-million occurrence corpus"]
fn synthetic_windows_duplicate_resources() {
    let _reset = ByteLimitReset;
    let settings = Settings::read(None);
    let governor_initial = governor_usage();
    let corpus = Corpus {
        count: number_setting("SORTSIZE_DUP_QUAL_FILES", 8192, 1024, 4_000_000),
        directory_depth: number_setting("SORTSIZE_DUP_QUAL_DEPTH", 3, 1, 12),
    };
    assert!(corpus.count.is_multiple_of(32));
    let source = tempfile::tempdir().unwrap();
    let guard = Arc::new(Guard::open(source.path(), Default::default()).unwrap());
    let parent = corpus.directory(guard.root());
    let cancel = CancellationToken::default();
    let mut inserted = 0;
    let mut stopped = None;
    let mut temporary_after_ingest = None;
    let started = Instant::now();
    let prepared = prepare_synthetic(&settings, guard.clone(), &cancel, |sink, space| {
        for id in 1..=corpus.count {
            if settings.scenario == Scenario::CancelDiscovery && inserted == 513 {
                stopped = Some(Instant::now());
                cancel.cancel();
            }
            check_cancel(&cancel)?;
            let path = parent.join(occurrence_name(id));
            let evidence = NativeEvidence {
                identity: Some(LocalFileId::Windows {
                    volume_serial: 97,
                    file_id: u128::from(corpus.representative(id)).to_be_bytes(),
                }),
                created: Some((0, 0)),
            };
            {
                let result = sink.insert(Candidate {
                    path: &path,
                    size: corpus.size(id),
                    modified: Some((0, 0)),
                    evidence: Some(&evidence),
                });
                if result.is_err() {
                    temporary_after_ingest = Some(space.usage());
                }
                result?;
            }
            inserted += 1;
        }
        sink.progress.observations = corpus.count;
        sink.pending = 0;
        Ok(())
    });
    let preparation_us = started.elapsed().as_micros();
    let mut report = serde_json::json!({
        "schema": 1, "source": "synthetic", "case": settings.case,
        "files_requested": corpus.count, "directory_depth": corpus.directory_depth,
        "requested_memory_bytes": settings.memory_limit,
        "requested_global_memory_bytes": settings.global_memory_limit,
        "requested_temporary_bytes": settings.temporary_limit,
        "governor_initial": governor_initial, "inserted": inserted, "preparation_us": preparation_us,
        "summary": null, "walks": null, "temporary_before_drop": null,
    });
    match prepared {
        Err(error) => {
            report["error"] = preparation_stop(settings.scenario, &error, inserted, stopped);
            assert!(inserted < corpus.count);
            report["cancellation_us"] =
                serde_json::json!(stopped.map(|time| time.elapsed().as_micros()));
        }
        Ok(mut job) => {
            assert!(!matches!(
                settings.scenario,
                Scenario::QuotaDiscovery | Scenario::CancelDiscovery | Scenario::MemoryRefusal
            ));
            assert_eq!(job.candidate_count().unwrap(), corpus.count);
            assert_eq!(job.summary().bytes_read, 0);
            assert!(!job.summary().verified);
            temporary_after_ingest = Some(job.temporary_usage());
            let mut verifier = SyntheticVerifier {
                corpus,
                scenario: settings.scenario,
                cancel: &cancel,
                halted: None,
                hashed: 0,
                compared: 0,
                fail_next_publication: (settings.scenario == Scenario::QuotaVerification)
                    .then(|| job.space.clone()),
            };
            let verifying = Instant::now();
            job.verify_current_with(
                &mut verifier,
                DuplicateOptions::without_content_limits(),
                &cancel,
                &mut |_| {},
            )
            .unwrap();
            report["verification_us"] = serde_json::json!(verifying.elapsed().as_micros());
            report["cancellation_us"] =
                serde_json::json!(verifier.halted.map(|time| time.elapsed().as_micros()));
            let summary = job.summary();
            assert_eq!(
                summary.complete,
                settings.scenario == Scenario::Full,
                "{summary:?}"
            );
            assert_eq!(
                summary.cancelled,
                matches!(
                    settings.scenario,
                    Scenario::CancelHashing | Scenario::CancelComparing
                ),
                "{summary:?}"
            );
            assert_eq!(
                summary.budget_limited,
                settings.scenario == Scenario::QuotaVerification
            );
            if settings.scenario == Scenario::QuotaVerification {
                let integrity: String = job
                    .db
                    .pragma_query_value(None, "integrity_check", |row| row.get(0))
                    .unwrap();
                assert_eq!(integrity, "ok");
                assert_eq!(
                    summary.resources.unwrap().cause,
                    Some(DuplicateResourceCause::TemporaryLimit)
                );
                report["sqlite_integrity"] = serde_json::json!(integrity);
            }
            assert_eq!(summary.errors, 0, "{summary:?}");
            assert_eq!(summary.bytes_read, 0, "{summary:?}");
            report["summary"] = summary_receipt(summary);
            report["selected_logical_bytes"] = serde_json::json!(summary.selected_logical_bytes);
            report["hash_calls"] = serde_json::json!(verifier.hashed);
            report["comparison_calls"] = serde_json::json!(verifier.compared);
            report["temporary_before_drop"] = serde_json::json!(job.temporary_usage());
            let presenting = Instant::now();
            let (groups, members) = groups_and_members(&job, guard.root()).unwrap();
            let forward = presentation(&job, guard.root(), false).unwrap();
            let reverse = presentation(&job, guard.root(), true).unwrap();
            report["pagination_us"] = serde_json::json!(presenting.elapsed().as_micros());
            report["walks"] = serde_json::json!({
                "groups": groups.receipt(), "members": members.receipt(),
                "forward": forward.receipt(), "reverse": reverse.receipt(),
            });
            drop(job);
        }
    }
    assert!(fs::read_dir(source.path()).unwrap().next().is_none());
    report["temporary_after_ingest"] = serde_json::json!(temporary_after_ingest);
    report["workspace_remaining"] = serde_json::json!(settings.remaining());
    report["governor_final"] = clear(&settings, &governor_initial);
    report["outcome"] = serde_json::json!(settings.case);
    report["process"] = process_peak();
    println!("DUPLICATE_RESOURCES_RESULT {report}");
}

fn summary_receipt(summary: &Summary) -> serde_json::Value {
    let mut data = serde_json::to_value(summary).unwrap();
    data["selected_logical_bytes"] = serde_json::json!(summary.selected_logical_bytes);
    data["unverified_files"] = serde_json::json!(summary.unverified_files);
    data["resources"] = serde_json::json!(summary.resources);
    data["temporary_limit"] = serde_json::json!(summary.temporary_limit);
    data
}

fn export_native(job: &DuplicateJob<'_, '_>, output: &Path) {
    use std::io::{BufWriter, Write};
    let mut pending = tempfile::Builder::new()
        .prefix(".members-")
        .suffix(".pending")
        .tempfile_in(output.parent().unwrap())
        .unwrap();
    let mut stream = BufWriter::new(pending.as_file_mut());
    let mut emit = |row| writeln!(stream, "{row}").unwrap();
    emit(serde_json::json!({
        "record": "summary", "summary": summary_receipt(job.summary()),
        "temporary_usage": job.temporary_usage(),
    }));
    let mut after_group = None;
    loop {
        let groups = job.groups(after_group).unwrap();
        emit(serde_json::json!({"record": "group_page", "after": after_group, "groups": groups}));
        if groups.is_empty() {
            break;
        }
        for group in groups {
            after_group = Some(group.id);
            let mut after_member = None;
            loop {
                let members = job.members(group.id, after_member).unwrap();
                emit(serde_json::json!({
                    "record": "member_page", "group": group.id, "after": after_member, "members": members
                }));
                let Some(member) = members.last() else {
                    break;
                };
                after_member = Some(member.cursor);
            }
        }
    }
    stream.flush().unwrap();
    drop(stream);
    pending.persist_noclobber(output).unwrap();
}

#[test]
#[ignore = "uses an operator-provided local NTFS fixture and scratch directory"]
fn native_windows_duplicate_resources() {
    let _reset = ByteLimitReset;
    let root = PathBuf::from(
        std::env::var_os("SORTSIZE_DUP_QUAL_SOURCE").expect("native fixture required"),
    );
    let guard = Arc::new(Guard::open(&root, Default::default()).unwrap());
    let settings = Settings::read(Some(&guard));
    assert_ne!(
        settings.scenario,
        Scenario::QuotaVerification,
        "fault injection is synthetic"
    );
    let output = std::env::var_os("SORTSIZE_DUP_QUAL_RESULTS")
        .map(PathBuf::from)
        .map(|output| {
            let file_name = output.file_name().expect("native export file name");
            let parent = output
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent = parent.canonicalize().unwrap();
            assert!(
                guard.work_path_allowed(&parent).unwrap(),
                "export path inside fixture"
            );
            parent.join(file_name)
        });
    let governor_initial = governor_usage();
    let cancel = CancellationToken::default();
    let mut acquisition = AcquisitionProgress::default();
    let mut stopped = None;
    let profile_name =
        std::env::var("SORTSIZE_DUP_QUAL_PROFILE").unwrap_or_else(|_| "legacy".into());
    let profile = match profile_name.as_str() {
        "legacy" => None,
        "general" => Some(DuplicateFileProfile::General),
        "optimized" => Some(DuplicateFileProfile::Optimized),
        "large" => Some(DuplicateFileProfile::LargeFiles),
        "exhaustive" => Some(DuplicateFileProfile::Exhaustive),
        _ => panic!("unknown qualification profile: {profile_name}"),
    };
    let started = Instant::now();
    let update = |progress: &AcquisitionProgress| {
        acquisition = *progress;
        if stopped.is_none()
            && settings.scenario == Scenario::CancelDiscovery
            && progress.observations >= 512
        {
            stopped = Some(Instant::now());
            cancel.cancel();
        }
    };
    let resources = settings.resources();
    let prepared = match profile {
        Some(profile) => {
            prepare_current_folder_with_profile(guard, &resources, profile, &cancel, update)
        }
        None => prepare_current_folder_with_resources(guard, &resources, &cancel, update),
    };
    let preparation_us = started.elapsed().as_micros();
    let mut report = serde_json::json!({
        "schema": 1, "source": "native", "case": settings.case,
        "profile": profile_name, "minimum_bytes": profile.map(DuplicateFileProfile::minimum_bytes),
        "examined_files": acquisition.examined_files,
        "excluded_by_size": acquisition.excluded_by_size,
        "excluded_empty": acquisition.excluded_empty,
        "requested_memory_bytes": settings.memory_limit,
        "requested_global_memory_bytes": settings.global_memory_limit,
        "requested_temporary_bytes": settings.temporary_limit,
        "governor_initial": governor_initial, "preparation_us": preparation_us,
        "observed": acquisition.observations, "selected_during_discovery": acquisition.selected_files,
        "summary": null, "temporary_after_ingest": null, "temporary_before_drop": null,
    });
    match prepared {
        Err(error) => {
            report["error"] =
                preparation_stop(settings.scenario, &error, acquisition.observations, stopped);
            report["cancellation_us"] =
                serde_json::json!(stopped.map(|time| time.elapsed().as_micros()));
        }
        Ok(mut job) => {
            assert!(!matches!(
                settings.scenario,
                Scenario::QuotaDiscovery | Scenario::CancelDiscovery | Scenario::MemoryRefusal
            ));
            let after_ingest = job.temporary_usage();
            let verify_started = Instant::now();
            let mut phase_bytes = None;
            let start_cancellation_after = u128::from(number_setting(
                "SORTSIZE_DUP_QUAL_CANCEL_AFTER_BYTES",
                0,
                0,
                1 << 36,
            ));
            job.verify_current_folder_with_progress(
                DuplicateOptions::without_content_limits(),
                &cancel,
                |progress| {
                    let phase_matches = match settings.scenario {
                        Scenario::CancelHashing => {
                            progress.verification_phase == Some(VerificationPhase::Hashing)
                        }
                        Scenario::CancelComparing => {
                            progress.verification_phase == Some(VerificationPhase::Comparing)
                        }
                        _ => false,
                    };
                    if stopped.is_none()
                        && phase_matches
                        && progress.files_hashed > 0
                        && progress.bytes_read >= start_cancellation_after
                    {
                        stopped = Some(Instant::now());
                        phase_bytes = Some(progress.bytes_read);
                        cancel.cancel();
                    }
                },
            )
            .unwrap();
            let summary = job.summary();
            assert_eq!(
                summary.complete,
                settings.scenario == Scenario::Full,
                "{summary:?}"
            );
            assert_eq!(
                summary.cancelled,
                settings.scenario != Scenario::Full,
                "{summary:?}"
            );
            report["verification_us"] = serde_json::json!(verify_started.elapsed().as_micros());
            report["cancellation_us"] =
                serde_json::json!(stopped.map(|time| time.elapsed().as_micros()));
            report["cancellation_content_bytes"] = serde_json::json!(phase_bytes);
            report["summary"] = summary_receipt(summary);
            report["temporary_after_ingest"] = serde_json::json!(after_ingest);
            report["temporary_before_drop"] = serde_json::json!(job.temporary_usage());
            report["candidate_rows"] = serde_json::json!(job.candidate_count().unwrap());
            report["process_before_export"] = process_peak();
            if let Some(output) = output {
                let exporting = Instant::now();
                export_native(&job, &output);
                report["export_us"] = serde_json::json!(exporting.elapsed().as_micros());
            }
            drop(job);
        }
    }
    report["workspace_remaining"] = serde_json::json!(settings.remaining());
    report["governor_final"] = clear(&settings, &governor_initial);
    report["outcome"] = serde_json::json!(settings.case);
    report["process"] = process_peak();
    println!("DUPLICATE_RESOURCES_RESULT {report}");
}
