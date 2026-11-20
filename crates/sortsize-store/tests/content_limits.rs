use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    duplicates::DuplicateOptions,
};
use sortsize_store::inventory::queries::duplicates::{
    DuplicateJob, VerificationPhase, prepare_current_folder,
};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

// The production verification permit is process-wide and refuses contention.
static SERIAL: Mutex<()> = Mutex::new(());
const DISK_BYTES: u64 = 32 * 1024 * 1024;
const FILE_BYTES: u64 = 128 * 1024 + 7;
const CONTROL: &[u8] = b"confirmed";

struct Fixture {
    _serial: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    source: PathBuf,
    scratch: PathBuf,
    guard: Arc<Guard>,
}

impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let scratch = temp.path().join("scratch");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&scratch).unwrap();
        let guard = Arc::new(
            Guard::open(
                &source,
                Policy {
                    explicit_root: true,
                    ..Policy::default()
                },
            )
            .unwrap(),
        );
        Self {
            _serial: serial,
            _temp: temp,
            source,
            scratch,
            guard,
        }
    }

    fn pair(&self, stem: &str, bytes: &[u8]) {
        for suffix in ["original", "renamed-copy"] {
            fs::write(self.source.join(format!("{stem}-{suffix}")), bytes).unwrap();
        }
    }

    fn prepare(&self) -> DuplicateJob<'static, 'static> {
        let job = prepare_current_folder(
            Arc::clone(&self.guard),
            &self.scratch,
            DISK_BYTES,
            &CancellationToken::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(job.summary().bytes_read, 0);
        assert!(!job.summary().verified);
        job
    }

    fn verify(&self, options: DuplicateOptions) -> DuplicateJob<'static, 'static> {
        let mut job = self.prepare();
        job.verify_scoped(
            std::slice::from_ref(self.guard.as_ref()),
            options,
            &CancellationToken::default(),
        )
        .unwrap();
        job
    }
}

#[test]
fn file_ceiling_accepts_exact_bytes_and_skips_plus_one_without_losing_verified_groups() {
    let fixture = Fixture::new();
    fixture.pair("at-ceiling", &vec![0x47; FILE_BYTES as usize]);
    fixture.pair("over-ceiling", &vec![0x75; FILE_BYTES as usize + 1]);
    fs::hard_link(
        fixture.source.join("at-ceiling-original"),
        fixture.source.join("alias"),
    )
    .unwrap();

    let limited = fixture.verify(DuplicateOptions {
        max_file_bytes: Some(FILE_BYTES),
        ..DuplicateOptions::without_content_limits()
    });
    let summary = limited.summary();
    assert!(summary.verified && !summary.complete && summary.budget_limited);
    assert_eq!(summary.file_size_limit, Some(FILE_BYTES));
    assert_eq!(summary.total_read_limit, None);
    assert_eq!(summary.unverified_files, Some(2));
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.files_hashed, 2);
    assert_eq!(summary.bytes_read, u128::from(4 * FILE_BYTES));
    assert_eq!(summary.aliases, 1);
    let groups = limited.groups(None).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].logical_bytes, FILE_BYTES);
    assert_eq!(groups[0].paths, 3);
    assert_eq!(groups[0].independent_members, 2);
    assert_eq!(limited.members(groups[0].id, None).unwrap().len(), 3);
    drop(limited);

    let full = fixture.verify(DuplicateOptions {
        max_file_bytes: Some(FILE_BYTES + 1),
        ..DuplicateOptions::without_content_limits()
    });
    assert!(full.summary().complete, "{:?}", full.summary());
    assert_eq!(full.summary().file_size_limit, None);
    assert_eq!(full.summary().unverified_files, Some(0));
    assert_eq!(full.summary().groups, 2);
    assert_eq!(full.summary().bytes_read, u128::from(8 * FILE_BYTES + 4));
}

#[test]
fn total_ceiling_charges_hash_and_both_comparison_reads_and_keeps_a_finished_bucket() {
    let fixture = Fixture::new();
    fixture.pair("control", CONTROL);
    fixture.pair("large", &vec![0x47; FILE_BYTES as usize]);
    let needed = 4 * (FILE_BYTES + CONTROL.len() as u64);

    for (limit, complete) in [(needed - 1, false), (needed, true), (needed + 1, true)] {
        let job = fixture.verify(DuplicateOptions {
            max_bytes_read: Some(limit),
            ..DuplicateOptions::without_content_limits()
        });
        let summary = job.summary();
        assert_eq!(summary.complete, complete, "{summary:?}");
        assert_eq!(summary.budget_limited, !complete);
        assert_eq!(summary.bytes_read, u128::from(needed.min(limit)));
        assert_eq!(summary.total_read_limit, (!complete).then_some(limit));
        assert_eq!(summary.file_size_limit, None);
        assert_eq!(summary.unverified_files, Some(if complete { 0 } else { 2 }));
        assert_eq!(summary.files_hashed, 4);
        assert_eq!(summary.errors, 0);
        let groups = job.groups(None).unwrap();
        assert_eq!(groups.len(), if complete { 2 } else { 1 });
        assert!(
            groups
                .iter()
                .any(|group| group.logical_bytes == CONTROL.len() as u64)
        );
    }
}

#[test]
fn zero_byte_budget_still_verifies_empty_files() {
    let fixture = Fixture::new();
    fixture.pair("empty", b"");
    let job = fixture.verify(DuplicateOptions {
        max_bytes_read: Some(0),
        max_file_bytes: Some(0),
        ..DuplicateOptions::without_content_limits()
    });
    let summary = job.summary();
    assert!(summary.complete, "{summary:?}");
    assert_eq!(summary.bytes_read, 0);
    assert_eq!(summary.files_hashed, 2);
    assert_eq!(summary.groups, 1);
}

#[test]
fn cancellation_interrupts_native_hash_and_binary_comparison_between_reads() {
    let fixture = Fixture::new();
    let size = 2 * 1024 * 1024 + 17;
    fixture.pair("content", &vec![0x47; size]);
    let bytes = size as u128;
    for (phase, lower, upper) in [
        (VerificationPhase::Hashing, 0, bytes),
        (VerificationPhase::Comparing, 2 * bytes, 4 * bytes),
    ] {
        let mut job = fixture.prepare();
        let cancel = CancellationToken::default();
        let mut observed = None;
        job.verify_scoped_with_progress(
            std::slice::from_ref(fixture.guard.as_ref()),
            DuplicateOptions::without_content_limits(),
            &cancel,
            |progress| {
                if observed.is_none()
                    && progress.verification_phase == Some(phase)
                    && progress.bytes_read > lower
                {
                    observed = Some((progress.bytes_read, Instant::now()));
                    cancel.cancel();
                }
            },
        )
        .unwrap();
        let (cancelled_at, time) = observed.expect("no notification during an active content read");
        let summary = job.summary();
        assert!(summary.cancelled && !summary.complete, "{summary:?}");
        assert!(cancelled_at < upper, "{phase:?}: {cancelled_at}");
        assert_eq!(summary.bytes_read, cancelled_at);
        assert!(job.groups(None).unwrap().is_empty());
        eprintln!(
            "content_cancel phase={phase:?} bytes_read={} acknowledgement_us={}",
            summary.bytes_read,
            time.elapsed().as_micros()
        );
    }
}

#[test]
fn cancellation_keeps_the_finished_bucket_attached_to_its_original_root() {
    let fixture = Fixture::new();
    fixture.pair("control", CONTROL);
    fixture.pair("pending", &vec![0x85; FILE_BYTES as usize]);
    let mut job = fixture.prepare();
    let cancel = CancellationToken::default();
    let mut before_cancel = None;
    job.verify_scoped_with_progress(
        std::slice::from_ref(fixture.guard.as_ref()),
        DuplicateOptions::without_content_limits(),
        &cancel,
        |progress| {
            if before_cancel.is_none()
                && progress.verification_phase == Some(VerificationPhase::Comparing)
                && progress.groups == 1
            {
                before_cancel = Some(progress.bytes_read);
                cancel.cancel();
            }
        },
    )
    .unwrap();

    let summary = job.summary();
    assert!(summary.cancelled && !summary.complete, "{summary:?}");
    let control_bytes = CONTROL.len() as u64;
    assert_eq!(
        before_cancel,
        Some(u128::from(2 * FILE_BYTES + 4 * control_bytes))
    );
    assert_eq!(summary.bytes_read, before_cancel.unwrap());
    assert_eq!(summary.groups, 1);
    assert_eq!(summary.unverified_files, Some(2));
    let groups = job.groups(None).unwrap();
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(group.logical_bytes, control_bytes);
    assert_eq!(group.paths, 2);
    assert_eq!(group.independent_members, 2);
    let members = job.members(group.id, None).unwrap();
    assert_eq!(members.len(), 2);
    let paths = members
        .iter()
        .map(|member| {
            PathBuf::from(&member.path.display)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        paths,
        ["control-original".into(), "control-renamed-copy".into()].into()
    );
}
