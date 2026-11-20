//! Root-aware placement of temporary acquisition state (SCAN-001, HIST-001).
//! The normal location is selected once per operation. Existing retained
//! authorities are checked separately before a writer is reopened.
use super::{DiskAdmission, Error, lifecycle};
use sortsize_core::acquisition::{Guard, local_state_location};
use sortsize_core::resources::{Governor, disk_limit, native_free_disk};
use std::{
    fmt,
    path::{Path, PathBuf},
};

const CONTAINER: &str = ".bunny-and-size-temporary-v1";
const WORKSPACE: &str = ".bunny-and-size-work-v2";
const RECEIPT: &str = "workspace-owner-v1";

/// The only working location considered for normal GUI use. Existing
/// directories are recognized by their private permissions and native receipt.
pub struct Workspace {
    root: PathBuf,
    parent: PathBuf,
    explicit: bool,
    proof: Vec<u8>,
}

impl Workspace {
    pub fn selected() -> super::Result<Self> {
        if let Some(parent) = std::env::var_os("BUNNY_AND_SIZE_STORE_DIR") {
            return Self::open(Path::new(&parent), true);
        }
        let history = lifecycle::default_root()?;
        let parent = history
            .parent()
            .and_then(Path::parent)
            .ok_or(Error::Contract("local working folder unavailable"))?;
        #[cfg(not(windows))]
        {
            local_state_location(parent)?;
            std::fs::create_dir_all(parent)?;
        }
        Self::open(parent, false)
    }

    fn open(parent: &Path, explicit: bool) -> super::Result<Self> {
        use std::io::{Read, Write};
        static INITIALIZING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _initializing = INITIALIZING.lock().unwrap();
        local_state_location(parent)?;
        let parent = std::fs::canonicalize(parent)?;
        let root = parent.join(WORKSPACE);
        local_state_location(&root)?;
        let created = match lifecycle::private_directory(&root) {
            Ok(()) => true,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        lifecycle::check_directory(&root)?;
        let guard = Guard::open(&root, Default::default())?;
        let proof = crate::scheduler::workspace_identity(&guard)
            .ok_or(Error::Contract("workspace native identity unavailable"))?;
        let receipt = root.join(RECEIPT);
        if created {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&receipt)?;
            file.write_all(&proof)?;
            file.sync_all()?;
            lifecycle::sync_directory(&root)?;
        }
        let metadata = std::fs::symlink_metadata(&receipt)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(Error::Contract("workspace receipt is a reparse point"));
            }
        }
        if !metadata.is_file() || metadata.len() != proof.len() as u64 {
            return Err(Error::Contract("workspace receipt is invalid"));
        }
        let mut recorded = Vec::new();
        std::fs::File::open(receipt)?
            .take(proof.len() as u64 + 1)
            .read_to_end(&mut recorded)?;
        if recorded != proof {
            return Err(Error::Contract("workspace ownership or location changed"));
        }
        Ok(Self {
            root,
            parent,
            explicit,
            proof,
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn catalogue(&self) -> PathBuf {
        self.root.join("inventories-v1")
    }

    pub fn contains(&self, path: &Path) -> super::Result<bool> {
        local_state_location(path)?;
        Ok(self.guard()?.has_native_ancestor(path)?)
    }

    fn guard(&self) -> super::Result<Guard> {
        lifecycle::check_directory(&self.root)?;
        let state = Guard::open(&self.root, Default::default())?;
        if crate::scheduler::workspace_identity(&state).as_deref() != Some(&self.proof) {
            return Err(Error::Contract("workspace ownership or location changed"));
        }
        Ok(state)
    }

    pub fn scope(&self, guard: Guard) -> super::Result<Guard> {
        Ok(guard.with_internal_state(self.guard()?)?)
    }

    pub fn temporary(&self, prefix: &str) -> super::Result<tempfile::TempDir> {
        let state = self.guard()?;
        let directory = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&self.root)?;
        if !state.has_native_ancestor(directory.path())?
            || state.evidence().1.attachment(&state.inspect(&self.root)?)
                != sortsize_core::acquisition::Attachment::Compatible
        {
            return Err(Error::Contract("workspace changed during creation"));
        }
        Ok(directory)
    }

    /// A retained authority can be used automatically only on the chosen
    /// volume. An explicit parent further limits reuse to that parent.
    pub fn permits_writer(&self, path: &Path) -> super::Result<bool> {
        local_state_location(path)?;
        let state = self.guard()?;
        for ancestor in path.ancestors().take_while(|part| part.is_absolute()) {
            match Guard::open(ancestor, Default::default()) {
                Ok(other) => {
                    return Ok(state.evidence().0.token == other.evidence().0.token
                        && (!self.explicit
                            || Guard::open(&self.parent, Default::default())?
                                .has_native_ancestor(path)?));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }
}

/// Local diagnostics for candidates refused before a session could start.
#[derive(Debug)]
pub struct PlacementAttempt {
    pub directory: PathBuf,
    pub reason: Error,
}

#[derive(Debug)]
pub struct PlacementError {
    pub root: PathBuf,
    pub attempts: Vec<PlacementAttempt>,
}

impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "No accessible local folder outside the acquired root ({}) is available for temporary inventory state. Select a narrower source, or set BUNNY_AND_SIZE_STORE_DIR to an existing local folder outside this source (on another volume for a whole-drive scan), then retry.",
            self.root.display()
        )?;
        for attempt in &self.attempts {
            write!(f, "\n{} — {}", attempt.directory.display(), attempt.reason)?;
        }
        Ok(())
    }
}
impl std::error::Error for PlacementError {}

/// Explicit candidate injection supports native regression tests without
/// process-wide environment mutation or scanning personal drives.
pub fn temporary_directory_in(
    guard: &Guard,
    prefix: &str,
    candidates: &[PathBuf],
) -> Result<tempfile::TempDir, PlacementError> {
    temporary_directory_checked_in(guard, prefix, candidates, |_| Ok(()))
}

fn temporary_directory_checked_in(
    guard: &Guard,
    prefix: &str,
    candidates: &[PathBuf],
    mut admit: impl FnMut(&Path) -> super::Result<()>,
) -> Result<tempfile::TempDir, PlacementError> {
    let mut attempts = Vec::new();
    for candidate in candidates {
        match try_candidate(guard, prefix, candidate).and_then(|temporary| {
            admit(temporary.path())?;
            Ok(temporary)
        }) {
            Ok(temporary) => return Ok(temporary),
            Err(reason) => attempts.push(PlacementAttempt {
                directory: candidate.clone(),
                reason,
            }),
        }
    }
    Err(PlacementError {
        root: guard.root().to_owned(),
        attempts,
    })
}

fn outside(guard: &Guard, path: &Path) -> super::Result<()> {
    if guard.store_outside(path)? {
        Ok(())
    } else {
        Err(Error::Contract("location is inside the acquired root"))
    }
}

fn try_candidate(guard: &Guard, prefix: &str, parent: &Path) -> super::Result<tempfile::TempDir> {
    outside(guard, parent)?;
    if !std::fs::metadata(parent)?.is_dir() {
        return Err(Error::Contract(
            "candidate parent must be an existing directory",
        ));
    }
    let container = parent.join(CONTAINER);
    // A permitted ancestor of the source may contain the source itself at our
    // container name. Validate this path before creating anything.
    outside(guard, &container)?;
    lifecycle::ensure_root(&container)?;
    // The existing private-state ACL policy is unchanged. Never repair or
    // replace an existing container with unsuitable permissions.
    let temporary = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&container)?;
    outside(guard, temporary.path())?;
    outside(guard, &temporary.path().join("repository"))?;
    Ok(temporary)
}

#[derive(Clone, Copy)]
pub struct DiskBudget {
    /// Additional capacity required while current owners are still live.
    pub now: u64,
    /// Floor for the native quota after other views have closed.
    pub minimum_limit: u64,
}

/// Check the location's quota and the simultaneous startup footprint without
/// dropping charges. Actual operations still reserve and account their writes.
pub fn check_disk_admission_at(
    store: &Path,
    scratch: &Path,
    budget: DiskBudget,
) -> super::Result<()> {
    let measure = |path: &Path| {
        native_free_disk(path).ok_or(Error::Contract("working volume free space unavailable"))
    };
    let store_free = measure(store)?;
    let scratch_free = if store == scratch {
        store_free
    } else {
        measure(scratch)?
    };
    check_disk_admission_in(
        &Governor::global(),
        store,
        scratch,
        budget,
        store_free,
        scratch_free,
    )
}

fn check_disk_admission_in(
    governor: &Governor,
    store: &Path,
    scratch: &Path,
    budget: DiskBudget,
    store_free: u64,
    scratch_free: u64,
) -> super::Result<()> {
    let limit = disk_limit(store_free.min(scratch_free));
    let usage = governor.usage();
    if budget.minimum_limit <= limit
        && usage
            .disk_used
            .checked_add(usage.disk_reserved)
            .and_then(|committed| committed.checked_add(budget.now))
            .is_some_and(|total| total <= limit)
    {
        return Ok(());
    }
    Err(Error::DiskAdmission(Box::new(DiskAdmission {
        store: store.to_owned(),
        scratch: scratch.to_owned(),
        requested: budget.now,
        minimum_limit: budget.minimum_limit,
        store_free,
        scratch_free,
        used: usage.disk_used,
        reserved: usage.disk_reserved,
        limit,
    })))
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    use sortsize_core::inventory_contract::MemoryMode;

    #[test]
    fn internal_workspace_is_excluded_and_resume_requires_the_recorded_scope() {
        use crate::{
            Durability,
            inventory::Store,
            scheduler::{Session, State},
        };
        use sortsize_core::inventory_contract::*;
        use std::{
            fs,
            time::{Duration, Instant},
        };
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("Source-Mixed");
        let parent = source.join("user-data");
        let other_parent = source.join("other");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir(&other_parent).unwrap();
        let lookalike = other_parent.join(WORKSPACE);
        fs::create_dir(&lookalike).unwrap();
        fs::write(lookalike.join("keep"), [0; 11]).unwrap();
        fs::write(parent.join("neighbor"), [0; 7]).unwrap();
        let work = Workspace::open(&parent, true).unwrap();
        let scratch = work.temporary("test-").unwrap();
        fs::write(scratch.path().join("internal"), [0; 100]).unwrap();
        let store_path = scratch.path().join("candidate");
        #[cfg(windows)]
        let acquired_root = PathBuf::from(source.to_string_lossy().to_lowercase());
        #[cfg(not(windows))]
        let acquired_root = source.clone();
        let source_guard = || Guard::open(&acquired_root, Default::default()).unwrap();
        let reopened_work = Workspace::open(&parent, true).unwrap();
        assert!(
            reopened_work
                .scope(Guard::open(work.path(), Default::default()).unwrap())
                .is_err()
        );
        let scope = || reopened_work.scope(source_guard()).unwrap();
        assert!(!source_guard().work_path_allowed(&store_path).unwrap());
        assert!(scope().work_path_allowed(&store_path).unwrap());
        assert!(scope().read_dir(work.path()).is_err());
        assert!(
            scope()
                .read_dir(&acquired_root.join("other").join(WORKSPACE))
                .is_ok()
        );
        // A native alias can take the identity check instead of the lexical
        // fast path. Assert the traversal denial and the actual scan totals.
        assert!(
            scope()
                .read_dir(&acquired_root.join("user-data").join(WORKSPACE))
                .is_err()
        );
        let context = AcquisitionContext {
            inventory: InventoryId(1),
            source: SourceId(2),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let create = Store::create(
            &store_path,
            context.inventory,
            context.source,
            Durability::Retained,
        )
        .unwrap();
        let mut session = Session::new(create, context, scope()).unwrap();
        session.control().cancel();
        assert_eq!(session.tick().unwrap().state, State::Cancelled);
        assert_eq!(
            Session::excluded_state_path(session.store(), context.generation)
                .unwrap()
                .as_deref(),
            Some(work.path())
        );
        assert!(
            !Session::resume_scope_matches(session.store(), context.generation, &source_guard())
                .unwrap()
        );
        assert!(
            Session::source_matches(session.store(), context.generation, &source_guard()).unwrap()
        );
        drop(session);

        let store = Store::reopen(&store_path, Durability::Retained).unwrap();
        let mut resumed = Session::resume(store, context.generation, scope()).unwrap();
        assert!(resumed.context().run > context.run);
        let deadline = Instant::now() + Duration::from_secs(15);
        while resumed.tick().unwrap().state != State::Idle {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let coverage = loop {
            if let Some(coverage) = resumed.finalize_step().unwrap() {
                break coverage;
            }
            assert!(Instant::now() < deadline);
        };
        assert_eq!(coverage, ScopeCoverage::Complete);
        assert_eq!(
            resumed.store().totals(context.generation).unwrap().logical,
            Some(18)
        );
        assert_eq!(resumed.store().entry_count(context.generation).unwrap(), 6);
    }

    #[cfg(windows)]
    #[test]
    fn internal_workspace_cannot_be_selected_through_a_junction() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let owned = Workspace::open(&target, true).unwrap();
        let junction = fixture.path().join("junction");
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "New-Item -ItemType Junction -Path $env:BUNNY_TEST_JUNCTION -Target $env:BUNNY_TEST_TARGET -ErrorAction Stop | Out-Null"])
            .env("BUNNY_TEST_JUNCTION", &junction)
            .env("BUNNY_TEST_TARGET", &target)
            .env_remove("PSModulePath")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(Workspace::open(&junction, true).is_err());
        let guard = owned
            .scope(Guard::open(fixture.path(), Default::default()).unwrap())
            .unwrap();
        assert!(guard.inspect(&junction).is_err());
        assert!(guard.read_dir(&junction.join(WORKSPACE)).is_err());
    }

    #[test]
    fn internal_workspace_refuses_an_unowned_name_and_detects_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join(WORKSPACE);
        lifecycle::private_directory(&root).unwrap();
        assert!(Workspace::open(fixture.path(), true).is_err());
        std::fs::rename(&root, fixture.path().join("unowned")).unwrap();
        let owned = Workspace::open(fixture.path(), true).unwrap();
        let scope = owned
            .scope(Guard::open(fixture.path(), Default::default()).unwrap())
            .unwrap();
        let moved = fixture.path().join("moved");
        std::fs::rename(owned.path(), &moved).unwrap();
        lifecycle::private_directory(owned.path()).unwrap();
        std::fs::copy(moved.join(RECEIPT), owned.path().join(RECEIPT)).unwrap();
        assert!(scope.validate_internal_state().is_err());
        assert!(Workspace::open(fixture.path(), true).is_err());
    }

    #[test]
    fn quota_placement_skips_accessible_but_inadmissible_location() {
        const LOW_FREE: u64 = 4_195_905_536;
        const HIGH_FREE: u64 = 290_388_242_432;
        const REQUEST: u64 = 1024 * 1024 * 1024 + 8 * 1024 * 1024;
        let budget = DiskBudget {
            now: REQUEST,
            minimum_limit: REQUEST,
        };
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let low = fixture.path().join("low");
        let high = fixture.path().join("high");
        for dir in [&source, &low, &high] {
            std::fs::create_dir(dir).unwrap();
        }
        let guard = Guard::open(&source, Default::default()).unwrap();
        let governor = Governor::new(MemoryMode::Auto, None, HIGH_FREE);
        assert!(governor.observe_artifact_group(&high, HIGH_FREE, 4096));
        let lease = governor.reserve_disk(4096).unwrap();
        let initial = governor.usage();
        let mut visited = Vec::new();
        let admitted = temporary_directory_checked_in(
            &guard,
            "inventory-",
            &[low.clone(), high.clone()],
            |dir| {
                visited.push(dir.to_owned());
                let free = if dir.starts_with(&low) {
                    LOW_FREE
                } else {
                    HIGH_FREE
                };
                check_disk_admission_in(&governor, dir, dir, budget, free, free)
            },
        )
        .unwrap();
        assert_eq!(visited.len(), 2);
        assert!(visited[0].starts_with(&low));
        assert!(!visited[0].exists());
        assert!(admitted.path().starts_with(&high));

        let error = temporary_directory_checked_in(
            &guard,
            "inventory-",
            &[low.clone(), high.clone()],
            |dir| check_disk_admission_in(&governor, dir, dir, budget, LOW_FREE, LOW_FREE),
        )
        .unwrap_err();
        assert_eq!(error.attempts.len(), 2);
        for (attempt, parent) in error.attempts.iter().zip([&low, &high]) {
            assert_eq!(&attempt.directory, parent);
            let Error::DiskAdmission(detail) = &attempt.reason else {
                panic!("accessible location must return its quota refusal");
            };
            assert!(detail.scratch.starts_with(parent));
            assert!(!detail.scratch.exists());
            assert_eq!(detail.requested, REQUEST);
            assert_eq!(detail.used, 4096);
            assert_eq!(detail.reserved, 4096);
            assert_eq!(detail.limit, LOW_FREE / 4);
        }
        let final_usage = governor.usage();
        assert_eq!(final_usage.disk_used, initial.disk_used);
        assert_eq!(final_usage.disk_reserved, initial.disk_reserved);
        assert_eq!(final_usage.disk_limit, initial.disk_limit);
        drop(lease);
        assert_eq!(governor.usage().disk_reserved, 0);
    }

    #[test]
    fn writer_with_existing_reader_admits_only_immediate_work_but_keeps_location_floor() {
        const MIB: u64 = 1024 * 1024;
        const FREE: u64 = 8_766_062_592;
        let fixture = tempfile::tempdir().unwrap();
        let g0 = fixture.path().join("generation-0");
        std::fs::write(&g0, b"certified bytes").unwrap();
        let governor = Governor::new(MemoryMode::Auto, None, FREE);
        assert!(governor.observe_artifact_group(&g0, FREE, 238_887));
        let reader_fold = governor.reserve_disk(512 * MIB).unwrap();
        let reader_order = governor.reserve_disk(512 * MIB).unwrap();
        let reader_tree = governor.reserve_disk(4 * MIB).unwrap();
        let comparison = governor.reserve_disk(512 * MIB).unwrap();
        let full = DiskBudget {
            now: 1032 * MIB,
            minimum_limit: 1032 * MIB,
        };
        let writer_only = DiskBudget {
            now: 8 * MIB,
            ..full
        };
        let path = fixture.path();
        let refused = check_disk_admission_in(&governor, path, path, full, FREE, FREE);
        let Err(Error::DiskAdmission(refused)) = refused else {
            panic!("opening a second view at once must be refused");
        };
        assert_eq!(refused.reserved, 1_614_807_040);
        assert!(check_disk_admission_in(&governor, path, path, writer_only, FREE, FREE).is_ok());
        assert_eq!(governor.usage().disk_used, 238_887);
        assert_eq!(governor.usage().disk_reserved, refused.reserved);
        assert_eq!(std::fs::read(&g0).unwrap(), b"certified bytes");
        drop((reader_fold, reader_order, reader_tree, comparison));
        assert_eq!(governor.usage().disk_reserved, 0);
        assert!(
            check_disk_admission_in(&governor, path, path, writer_only, FREE, 4_195_905_536)
                .is_err(),
            "reject a work volume whose own limit cannot host the future view"
        );
    }
}
