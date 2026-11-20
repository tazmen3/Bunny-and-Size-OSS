//! Automatic acquisition follows source intentions, never rendering or navigation.
//! Native selections have their own bounded store; display preferences stay intact.
use sortsize_core::volumes::{VolumeDiscovery, VolumeIdentity, VolumeMount};
use sortsize_store::codec::NativeName;
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

#[derive(Default)]
pub(crate) struct AutoAnalysis {
    selected: Option<PathBuf>,
}
impl AutoAnalysis {
    /// A source transition consumes its automatic attempt before preparation.
    /// Completion, errors and cancellation never reset that intention.
    pub fn select(&mut self, root: &Path) -> bool {
        if self.selected.as_deref() == Some(root) {
            return false;
        }
        self.selected = Some(root.to_owned());
        true
    }
    pub fn restored(&mut self, root: &Path) -> bool {
        self.select(root)
    }
    pub fn manual(&mut self, root: &Path) {
        self.selected = Some(root.to_owned());
    }
    /// A replaced native source at the same location is a new intention.
    pub fn different_source(&mut self) {
        self.selected = None;
    }
    pub fn cancel(&mut self) {
        // Keep the consumed intention. Only another selection or a manual
        // command may acquire again, including when cancellation is asynchronous.
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastSelection {
    pub root: PathBuf,
    pub identity: VolumeIdentity,
    pub mount: VolumeMount,
}
impl LastSelection {
    pub fn local(root: PathBuf, discovery: &VolumeDiscovery) -> Option<Self> {
        if !root.is_absolute() {
            return None;
        }
        // Resolve the actual boundary before testing kind. Filtering local
        // volumes first would admit an embedded NAS through its local ancestor.
        let (volume, mount) = discovery
            .volumes
            .iter()
            .flat_map(|volume| volume.mounts.iter().map(move |mount| (volume, mount)))
            .filter(|(_, mount)| root.starts_with(&mount.path))
            .max_by_key(|(_, mount)| mount.path.components().count())?;
        if !crate::volume_view::can_auto_scan(volume) {
            return None;
        }
        Some(Self {
            root,
            identity: volume.identity.clone(),
            mount: mount.clone(),
        })
    }
    pub fn matches_local(&self, discovery: &VolumeDiscovery) -> bool {
        Self::local(self.root.clone(), discovery).is_some_and(|current| {
            current.identity == self.identity
                && current.mount.path == self.mount.path
                && current.mount.root == self.mount.root
            // Linux mount IDs are session evidence, not durable identity. The
            // caller captures the fresh mount for its worker-side preflight.
        })
    }
}

pub(crate) fn config_path() -> Option<PathBuf> {
    crate::preferences_store::config_path().map(|path| path.with_file_name("last-source-v1.conf"))
}
const MAGIC: &[u8] = b"BUNNY-LAST-SOURCE\0\x01";
const MAX_BYTES: usize = 256 * 1024;

fn native(value: &OsStr) -> NativeName {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        NativeName::Posix(value.as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        NativeName::Windows(value.encode_wide().collect())
    }
}
fn decode_native(value: &[u8]) -> Option<OsString> {
    match NativeName::decode(value).ok()? {
        #[cfg(unix)]
        NativeName::Posix(bytes) => {
            use std::os::unix::ffi::OsStringExt;
            Some(OsString::from_vec(bytes))
        }
        #[cfg(windows)]
        NativeName::Windows(units) => {
            use std::os::windows::ffi::OsStringExt;
            Some(OsString::from_wide(&units))
        }
        _ => None,
    }
}
fn field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend((value.len() as u64).to_be_bytes());
    output.extend(value);
}
fn name(output: &mut Vec<u8>, value: &OsStr) {
    field(output, &native(value).encode());
}
fn encode(selection: &LastSelection) -> Vec<u8> {
    let mut output = MAGIC.to_vec();
    name(&mut output, selection.root.as_os_str());
    match &selection.identity {
        VolumeIdentity::Linux {
            major,
            minor,
            filesystem,
            source,
        } => {
            output.push(0);
            output.extend(major.to_be_bytes());
            output.extend(minor.to_be_bytes());
            name(&mut output, filesystem);
            name(&mut output, source);
        }
        VolumeIdentity::WindowsGuid(guid) => {
            output.push(1);
            name(&mut output, guid);
        }
        VolumeIdentity::WindowsPath(path) => {
            output.push(2);
            name(&mut output, path.as_os_str());
        }
    }
    name(&mut output, selection.mount.path.as_os_str());
    match selection.mount.mount_id {
        Some(id) => {
            output.push(1);
            output.extend(id.to_be_bytes());
        }
        None => output.push(0),
    }
    match &selection.mount.root {
        Some(root) => {
            output.push(1);
            name(&mut output, root.as_os_str());
        }
        None => output.push(0),
    }
    output
}
struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (value, rest) = self.0.split_at_checked(length)?;
        self.0 = rest;
        Some(value)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn name(&mut self) -> Option<OsString> {
        let length = usize::try_from(self.u64()?).ok()?;
        decode_native(self.take(length)?)
    }
}
fn decode(bytes: &[u8]) -> Option<LastSelection> {
    if bytes.len() > MAX_BYTES || !bytes.starts_with(MAGIC) {
        return None;
    }
    let mut d = Decoder(&bytes[MAGIC.len()..]);
    let root = PathBuf::from(d.name()?);
    let identity = match d.byte()? {
        0 => VolumeIdentity::Linux {
            major: u32::from_be_bytes(d.take(4)?.try_into().ok()?),
            minor: u32::from_be_bytes(d.take(4)?.try_into().ok()?),
            filesystem: d.name()?,
            source: d.name()?,
        },
        1 => VolumeIdentity::WindowsGuid(d.name()?),
        2 => VolumeIdentity::WindowsPath(PathBuf::from(d.name()?)),
        _ => return None,
    };
    let path = PathBuf::from(d.name()?);
    let mount_id = match d.byte()? {
        0 => None,
        1 => Some(d.u64()?),
        _ => return None,
    };
    let mount_root = match d.byte()? {
        0 => None,
        1 => Some(PathBuf::from(d.name()?)),
        _ => return None,
    };
    if !d.0.is_empty() || !root.is_absolute() || !path.is_absolute() || !root.starts_with(&path) {
        return None;
    }
    Some(LastSelection {
        root,
        identity,
        mount: VolumeMount {
            path,
            mount_id,
            root: mount_root,
        },
    })
}
pub(crate) fn load(path: &Path) -> Option<LastSelection> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    decode(&bytes)
}
fn save(path: &Path, selection: Option<&LastSelection>) -> io::Result<()> {
    // A versioned empty record clears the remembered intention atomically.
    let bytes = selection.map_or_else(|| MAGIC.to_vec(), encode);
    if bytes.len() > MAX_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
#[derive(Default)]
struct Pending {
    latest: Option<Option<LastSelection>>,
    closing: bool,
}
pub(crate) struct SelectionStore {
    state: Arc<(Mutex<Pending>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl SelectionStore {
    pub fn start(path: PathBuf) -> io::Result<Self> {
        let state = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let shared = state.clone();
        let worker = std::thread::Builder::new()
            .name("bunny-last-source".into())
            .spawn(move || {
                loop {
                    let selection = {
                        let (lock, changed) = &*shared;
                        let state = lock.lock().unwrap_or_else(|error| error.into_inner());
                        let mut state = changed
                            .wait_while(state, |s| s.latest.is_none() && !s.closing)
                            .unwrap_or_else(|error| error.into_inner());
                        match state.latest.take() {
                            Some(selection) => selection,
                            None => break,
                        }
                    };
                    // Best effort, like an absent preference file: a failed
                    // remember never blocks an explicit, read-only acquisition.
                    let _ = save(&path, selection.as_ref());
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    pub fn submit(&mut self, selection: LastSelection) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest = Some(Some(selection));
        self.state.1.notify_one();
    }
    pub fn clear(&mut self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest = Some(None);
        self.state.1.notify_one();
    }
}
impl Drop for SelectionStore {
    fn drop(&mut self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closing = true;
        self.state.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::{MetadataValue, volumes::*};

    fn selection() -> LastSelection {
        LastSelection {
            root: std::env::temp_dir().join("bunny explicit source"),
            identity: VolumeIdentity::Linux {
                major: 8,
                minor: 1,
                filesystem: "ext4".into(),
                source: "/dev/test-source".into(),
            },
            mount: VolumeMount {
                path: std::env::temp_dir(),
                mount_id: Some(42),
                root: Some(PathBuf::from("/")),
            },
        }
    }
    fn volume(selection: &LastSelection, kind: SourceKind) -> Volume {
        Volume {
            identity: selection.identity.clone(),
            label: MetadataValue::Unknown,
            filesystem: MetadataValue::Unknown,
            kind,
            mounts: vec![selection.mount.clone()],
            capacity: VolumeCapacity::absent(MetadataValue::Unknown),
            availability: VolumeAvailability::Available,
        }
    }
    #[test]
    fn automatic_attempts_follow_source_transitions_and_survive_cancel() {
        let mut policy = AutoAnalysis::default();
        let a = std::env::temp_dir().join("a");
        let b = std::env::temp_dir().join("b");
        assert!(policy.restored(&a));
        assert!(!policy.select(&a));
        policy.cancel();
        assert!(!policy.restored(&a));
        assert!(!policy.select(&a));
        policy.manual(&a);
        assert!(!policy.select(&a));
        assert!(policy.select(&b));
        assert!(policy.select(&a));
        policy.different_source();
        assert!(policy.select(&a));
    }
    #[test]
    fn native_roundtrip_is_bounded_and_rejects_invalid_or_truncated_state() {
        let selected = selection();
        let bytes = encode(&selected);
        assert_eq!(decode(&bytes), Some(selected));
        for length in 0..bytes.len() {
            assert!(decode(&bytes[..length]).is_none());
        }
        assert!(decode(&[0; MAX_BYTES + 1]).is_none());
        let mut extra = bytes;
        extra.push(0);
        assert!(decode(&extra).is_none());
    }
    #[cfg(unix)]
    #[test]
    fn remembered_paths_keep_non_utf8_native_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let mut selected = selection();
        selected.root = selected
            .mount
            .path
            .join(OsString::from_vec(b"native-\xff".to_vec()));
        assert_eq!(decode(&encode(&selected)), Some(selected));
    }
    #[cfg(windows)]
    #[test]
    fn remembered_paths_keep_unpaired_utf16_native_units() {
        use std::os::windows::ffi::OsStringExt;
        let mut selected = selection();
        selected.root = selected
            .mount
            .path
            .join(OsString::from_wide(&[0x61, 0xd800]));
        assert_eq!(decode(&encode(&selected)), Some(selected));
    }
    #[test]
    fn async_remember_does_not_touch_display_preferences_and_load_never_writes() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("last-source-v1.conf");
        let display = temporary.path().join("display-v1.conf");
        fs::write(&display, b"existing display settings").unwrap();
        assert!(load(&path).is_none());
        let mut store = SelectionStore::start(path.clone()).unwrap();
        store.submit(selection());
        let mut latest = selection();
        latest.root.push("latest");
        store.submit(latest.clone());
        drop(store);
        let before = fs::read(&path).unwrap();
        assert_eq!(load(&path), Some(latest));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(display).unwrap(), b"existing display settings");
        let mut store = SelectionStore::start(path.clone()).unwrap();
        store.clear();
        drop(store);
        assert!(load(&path).is_none());
    }
    #[test]
    fn restoration_requires_exact_available_local_boundary_and_rejects_nested_nas() {
        let selected = selection();
        let local = volume(&selected, SourceKind::Local);
        let mut discovery = VolumeDiscovery {
            volumes: vec![local.clone()],
            enumeration_time: Default::default(),
            capacity_time: Default::default(),
            capacity_calls: 0,
        };
        assert!(selected.matches_local(&discovery));
        discovery.volumes[0].mounts[0].mount_id = Some(99);
        assert!(selected.matches_local(&discovery));
        discovery.volumes[0].identity = VolumeIdentity::WindowsGuid("changed".into());
        assert!(!selected.matches_local(&discovery));
        discovery.volumes[0] = local.clone();
        discovery.volumes[0].availability = VolumeAvailability::Unavailable;
        assert!(!selected.matches_local(&discovery));
        discovery.volumes[0] = local;
        let mut nas = volume(&selected, SourceKind::Remote);
        nas.mounts[0].path = selected.root.clone();
        discovery.volumes.push(nas);
        assert!(!selected.matches_local(&discovery));
        assert!(LastSelection::local(selected.root, &discovery).is_none());
    }

    #[test]
    fn restored_network_or_uncertain_source_never_inherits_local_consent() {
        let selected = selection();
        for kind in [SourceKind::Remote, SourceKind::Unknown] {
            for availability in [
                VolumeAvailability::Available,
                VolumeAvailability::NotProbed,
                VolumeAvailability::AccessDenied,
                VolumeAvailability::Unavailable,
                VolumeAvailability::Unknown,
            ] {
                // Keeping the exact old identity and pathname is insufficient:
                // a successful network probe is not acquisition authorization.
                let mut current = volume(&selected, kind);
                current.availability = availability;
                let discovery = VolumeDiscovery {
                    volumes: vec![current],
                    enumeration_time: Default::default(),
                    capacity_time: Default::default(),
                    capacity_calls: 0,
                };
                assert!(!selected.matches_local(&discovery));
                assert!(LastSelection::local(selected.root.clone(), &discovery).is_none());
            }
        }
    }

    #[test]
    fn nested_nas_boundary_blocks_local_ancestor_in_either_discovery_order() {
        let selected = selection();
        let local = volume(&selected, SourceKind::Local);
        for filesystem in ["cifs", "nfs", "nfs4"] {
            let mut network = volume(&selected, SourceKind::Remote);
            network.identity = VolumeIdentity::Linux {
                major: 0,
                minor: 99,
                filesystem: filesystem.into(),
                source: "dedicated-test-server".into(),
            };
            network.mounts[0].path = selected.root.clone();
            network.availability = VolumeAvailability::NotProbed;
            for volumes in [
                vec![local.clone(), network.clone()],
                vec![network.clone(), local.clone()],
            ] {
                let discovery = VolumeDiscovery {
                    volumes,
                    enumeration_time: Default::default(),
                    capacity_time: Default::default(),
                    capacity_calls: 0,
                };
                assert!(LastSelection::local(selected.root.clone(), &discovery).is_none());
                assert!(
                    LastSelection::local(selected.root.join("nested-folder"), &discovery).is_none()
                );
            }
        }
    }
}
