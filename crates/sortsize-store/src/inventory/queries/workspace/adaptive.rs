//! Growing disposable workspace. Native free space already excludes bytes
//! successfully allocated by previous writes, including residue.
use super::*;
use crate::inventory::{differential::NativeEvidence, queries::duplicates::DuplicateResourceCause};
use sortsize_core::acquisition::{Guard, Policy, Stamp, local_state_location};
use sortsize_core::resources::{DiskVolume, native_disk_volume, native_free_disk};
use std::{
    fs::{self, File},
    sync::{
        MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
mod owned;
pub(super) use owned::ArtifactIdentity;

const GIB: u64 = 1024 * 1024 * 1024;
pub(in crate::inventory::queries) const MINIMUM: u64 = 128 * 1024;
const MAXIMUM: u64 = 64 * GIB;
const MIN_RESERVE: u64 = GIB;
const MAX_ACTIVE: usize = 16;
static ACTIVE_PRESENT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
pub(in crate::inventory::queries) struct SpacePolicy {
    pub limit: u64,
    pub reserve: u64,
    pub limited_by_space: bool,
}

impl SpacePolicy {
    pub(in crate::inventory::queries) fn new(free: u64, requested: Option<u64>) -> Self {
        let reserve = MIN_RESERVE.max(free / 16);
        let available = free.saturating_sub(reserve);
        let limit = requested.map_or_else(
            || (available / 2).min(MAXIMUM),
            |bytes| bytes.min(available),
        );
        Self {
            limit,
            reserve,
            limited_by_space: requested.is_some_and(|bytes| bytes > available),
        }
    }

    fn permits(self, free: u64, prospective: u64, growth: u64) -> bool {
        growth
            <= free
                .saturating_sub(self.reserve)
                .saturating_sub(prospective)
    }
}

fn active() -> &'static Mutex<Vec<NativeEvidence>> {
    static ACTIVE: OnceLock<Mutex<Vec<NativeEvidence>>> = OnceLock::new();
    ACTIVE.get_or_init(Mutex::default)
}

pub(crate) fn is_active_stamp(stamp: &Stamp) -> bool {
    ACTIVE_PRESENT.load(Ordering::Acquire)
        && stamp.directory
        && stamp.identity.is_some()
        && stamp.created.is_some()
        && active()
            .lock()
            .unwrap()
            .contains(&NativeEvidence::from_stamp(stamp))
}

pub(super) struct AdaptiveSpace {
    pub path: PathBuf,
    pub policy: SpacePolicy,
    observation: Mutex<(Option<u64>, Option<DuplicateResourceCause>)>,
    #[cfg(all(test, windows))]
    mock_free: std::sync::atomic::AtomicU64,
    volume: Option<DiskVolume>,
    identity: NativeEvidence,
    directory_pin: Option<File>,
    _parents: Vec<File>,
}

impl AdaptiveSpace {
    pub(super) fn create(
        parent: &Path,
        free: u64,
        policy: SpacePolicy,
        make: impl FnOnce(&Path) -> Result<PathBuf>,
    ) -> Result<Self> {
        let parents = pin_parents(parent)?;
        local_state_location(parent)?;
        let mut registered = active().lock().unwrap();
        if registered.len() >= MAX_ACTIVE {
            return Err(Error::Contract("active duplicate workspace budget"));
        }
        // A reader that observes a just-created directory waits for its
        // identity to be published before it can schedule enumeration.
        ACTIVE_PRESENT.store(true, Ordering::Release);
        let result = (|| {
            let path = make(parent)?;
            // A failed identity or access check leaves the empty directory intact.
            let pin = pin_directory(&path, true)?;
            crate::inventory::lifecycle::check_directory(&path)?;
            let guard = Guard::open(&path, Policy::default())?;
            let identity = NativeEvidence::from_stamp(guard.evidence().1);
            if identity.identity.is_none()
                || identity.created.is_none()
                || registered.contains(&identity)
            {
                return Err(Error::Contract("duplicate workspace identity unavailable"));
            }
            let volume = native_disk_volume(&path);
            registered.push(identity.clone());
            Ok(Self {
                path,
                identity,
                policy,
                observation: Mutex::new((Some(free), None)),
                #[cfg(all(test, windows))]
                mock_free: std::sync::atomic::AtomicU64::new(u64::MAX),
                volume,
                directory_pin: pin,
                _parents: parents,
            })
        })();
        ACTIVE_PRESENT.store(!registered.is_empty(), Ordering::Release);
        result
    }

    pub(super) fn observation(&self) -> (Option<u64>, Option<DuplicateResourceCause>) {
        *self.observation.lock().unwrap()
    }

    pub(super) fn reject(&self, cause: DuplicateResourceCause) {
        self.observation.lock().unwrap().1.get_or_insert(cause);
    }

    pub(super) fn admit(&self, growth: u64) -> Option<MutexGuard<'static, ()>> {
        // All adaptive writes share the admission lock through native I/O.
        // The next write observes that allocation in the OS quota, regardless
        // of path / volume aliases. Existing legacy leases remain prospective.
        static ADMISSION: Mutex<()> = Mutex::new(());
        let admission = ADMISSION.lock().unwrap();
        let free = self.read_free();
        let mut observation = self.observation.lock().unwrap();
        observation.0 = free;
        let Some(free) = free else {
            observation.1 = Some(DuplicateResourceCause::TemporaryAccess);
            return None;
        };
        let prospective = Governor::global().prospective_disk_bytes(self.volume.as_ref());
        if !self.policy.permits(free, prospective, growth) {
            observation.1 = Some(DuplicateResourceCause::TemporarySpace);
            return None;
        }
        Some(admission)
    }

    fn read_free(&self) -> Option<u64> {
        #[cfg(all(test, windows))]
        {
            let value = self.mock_free.load(std::sync::atomic::Ordering::Relaxed);
            if value != u64::MAX {
                return Some(value);
            }
        }
        native_free_disk(&self.path)
    }
}

impl Drop for AdaptiveSpace {
    fn drop(&mut self) {
        let mut registered = active().lock().unwrap();
        // Only known artifacts were unlinked by Workspace. Foreign contents
        // or a sharing violation keep the directory intact.
        if let Some(pin) = self.directory_pin.take() {
            #[cfg(windows)]
            let _ = owned::mark_for_removal(&pin);
            #[cfg(not(windows))]
            drop(pin);
        } else {
            let _ = fs::remove_dir(&self.path);
        }
        registered.retain(|identity| *identity != self.identity);
        ACTIVE_PRESENT.store(!registered.is_empty(), Ordering::Release);
    }
}

#[cfg(windows)]
fn pin_directory(path: &Path, owned: bool) -> Result<Option<File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let handle = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | if owned { DELETE } else { 0 })
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = handle.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::Contract("unsafe duplicate temporary path"));
    }
    Ok(Some(handle))
}

#[cfg(not(windows))]
fn pin_directory(_: &Path, _: bool) -> Result<Option<File>> {
    Ok(None)
}

fn pin_parents(path: &Path) -> Result<Vec<File>> {
    let ancestors = path
        .ancestors()
        .take_while(|ancestor| ancestor.is_absolute())
        .collect::<Vec<_>>();
    if ancestors.len() > 128 {
        return Err(Error::Contract("duplicate temporary path depth"));
    }
    let mut pins = Vec::new();
    // The volume root cannot be moved. Anchor the following components from
    // shallowest to deepest before creating any job files.
    for ancestor in ancestors.into_iter().rev().skip(1) {
        pins.extend(pin_directory(ancestor, false)?);
    }
    Ok(pins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_headroom_with_a_finite_ceiling() {
        let policy = SpacePolicy::new(5 * GIB, None);
        assert_eq!(policy.limit, 2 * GIB);
        assert_eq!(policy.reserve, GIB);
        assert_eq!(SpacePolicy::new(u64::MAX, None).limit, MAXIMUM);
        assert_eq!(SpacePolicy::new(GIB, None).limit, 0);
        assert_eq!(SpacePolicy::new(5 * GIB, Some(GIB)).limit, GIB);
        assert_eq!(SpacePolicy::new(5 * GIB, Some(9 * GIB)).limit, 4 * GIB);
    }

    #[test]
    fn real_free_space_and_prospective_peers_gate_each_growth() {
        let policy = SpacePolicy::new(8 * GIB, None);
        assert!(policy.permits(GIB + 8192, 4096, 4096));
        assert!(!policy.permits(GIB + 8191, 4096, 4096));
        assert!(!policy.permits(GIB, 0, 1));
    }

    #[cfg(windows)]
    #[test]
    fn a_workspace_rechecks_free_space_after_success_and_releases_its_own_files() {
        use std::{io::Write, sync::atomic::Ordering};

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let free = 8 * GIB;
        let policy = SpacePolicy::new(free, Some(1024 * 1024));
        let workspace = Workspace::current_folder_adaptive(&parent, free, policy).unwrap();
        let adaptive = workspace.adaptive().unwrap();
        adaptive.mock_free.store(free, Ordering::Relaxed);
        let directory = workspace.path().to_owned();
        let mut artifact = workspace.file().unwrap();
        {
            let _permit = artifact.admit_growth(8192).unwrap();
            artifact.file_mut().write_all(&[1; 8192]).unwrap();
        }
        let usage = workspace.usage();
        adaptive.mock_free.store(policy.reserve, Ordering::Relaxed);
        assert!(artifact.admit_growth(32 * 1024).is_err());
        assert_eq!(workspace.usage().charged, usage.charged);
        assert_eq!(
            adaptive.observation(),
            (
                Some(policy.reserve),
                Some(DuplicateResourceCause::TemporarySpace)
            )
        );
        // The failed operation retains its reason, but never reserves growth
        // on the volume. A later admission samples free space independently.
        adaptive.mock_free.store(free, Ordering::Relaxed);
        assert!(artifact.admit_growth(32 * 1024).is_ok());
        drop(artifact);
        drop(workspace);
        assert!(!directory.exists());
        assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
    }
}
