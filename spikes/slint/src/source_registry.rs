//! Explicit source bookmarks, separate from inventories and automatic intentions.
//! Loading/adding a bookmark never opens its target or authorizes an acquisition.
use sortsize_core::{MetadataValue, ValueAccuracy, volumes::*};
use sortsize_store::codec::NativeName;
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, RwLock},
};

const MAGIC_V1: &[u8] = b"BUNNY-ADDED-SOURCES\0\x01";
const MAGIC: &[u8] = b"BUNNY-ADDED-SOURCES\0\x02";
const MAX_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SOURCES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Smb,
    Nfs,
    Local,
    /// Logical group of SMB shares. Its path is a server key, not a file root.
    SmbServer,
}
impl Kind {
    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Smb),
            1 => Some(Self::Nfs),
            2 => Some(Self::Local),
            3 => Some(Self::SmbServer),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    pub kind: Kind,
    pub path: PathBuf,
    pub label: String,
    /// Durable user exclusion; never granted or cleared by discovery.
    pub disconnected: bool,
}
impl Source {
    pub fn is_server(&self) -> bool {
        self.kind == Kind::SmbServer
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn server_target(&self) -> Option<smb::Target> {
        if !self.is_server() {
            return None;
        }
        smb::parse_target(self.path.to_str()?)
            .ok()
            .filter(|target| target.share.is_none())
    }

    pub fn new(
        kind: i32,
        path: PathBuf,
        label: &str,
        discovery: Option<&VolumeDiscovery>,
    ) -> io::Result<Self> {
        let kind = Kind::from_index(kind).ok_or(io::ErrorKind::InvalidInput)?;
        let path = if kind == Kind::SmbServer {
            #[cfg(any(windows, target_os = "linux"))]
            {
                let target = smb::parse_target(path.to_str().ok_or(io::ErrorKind::InvalidInput)?)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                if target.share.is_some() {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                target.server_unc()
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                return Err(io::ErrorKind::Unsupported.into());
            }
        } else {
            #[cfg(target_os = "linux")]
            let path = if kind == Kind::Smb && !path.is_absolute() {
                smb::parse_target(path.to_str().ok_or(io::ErrorKind::InvalidInput)?)
                    .map_err(|_| io::ErrorKind::InvalidInput)?
                    .share
                    .ok_or(io::ErrorKind::InvalidInput)?
            } else if kind == Kind::Smb {
                smb::mounted_uri(&path).unwrap_or(path)
            } else {
                path
            };
            path
        };
        if !valid_registered_path(kind, &path)
            || label.len() > 1024
            || label.chars().any(char::is_control)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        #[cfg(windows)]
        if !matches!(kind, Kind::Local | Kind::SmbServer) {
            let expected = if kind == Kind::Smb {
                NetworkProtocol::Smb
            } else {
                NetworkProtocol::Nfs
            };
            if namespace_network_protocol(&path)? != expected {
                return Err(io::ErrorKind::Unsupported.into());
            }
        }
        if kind == Kind::Local {
            #[cfg(windows)]
            if unc_source(&path).is_ok() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            if discovery
                .and_then(|d| {
                    d.volumes
                        .iter()
                        .flat_map(|v| v.mounts.iter().map(move |m| (v, m)))
                        .filter(|(_, m)| path.starts_with(&m.path))
                        .max_by_key(|(_, m)| m.path.components().count())
                })
                .is_some_and(|(v, _)| v.kind == SourceKind::Remote)
            {
                return Err(io::ErrorKind::InvalidInput.into());
            }
        }
        #[cfg(target_os = "linux")]
        if !matches!(kind, Kind::Local | Kind::SmbServer)
            && !(kind == Kind::Smb && is_smb_uri(&path))
            && !discovery.is_some_and(|d| {
                d.volumes
                    .iter()
                    .any(|v| matches_protocol(v, kind) && v.mounts.iter().any(|m| m.path == path))
            })
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(Self {
            kind,
            path,
            label: label.trim().to_owned(),
            disconnected: false,
        })
    }
}

/// Lexical authorization boundary, not a claim of identical physical storage.
/// Windows spelling variants cannot bypass a user exclusion. No filesystem or
/// network call is made; opaque native UTF-16 names remain native throughout.
pub(crate) fn covers(scope: &Path, path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Server/share URI keys and native CIFS/GVfs aliases are the same
        // authorization scope, without probing a server or opening its files.
        let key = |path: &Path| {
            let text = path.to_str();
            if text.is_some_and(|s| s.starts_with("smb://") || s.starts_with(r"\\")) {
                let target = smb::parse_target(text?).ok()?;
                Some(target.share.clone().unwrap_or_else(|| target.server_unc()))
            } else {
                smb::logical_path(path)
            }
        };
        if let (Some(scope), Some(path)) = (key(scope), key(path)) {
            return smb::covers(&scope, &path);
        }
        path.starts_with(scope)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        path.starts_with(scope)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        fn units(path: &Path) -> Vec<u16> {
            let mut value: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .map(|unit| {
                    if unit == b'/' as u16 {
                        b'\\' as u16
                    } else {
                        unit
                    }
                })
                .collect();
            let unc: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
            if value.len() >= unc.len()
                && value[..unc.len()].iter().zip(&unc).all(|(a, b)| {
                    a == b
                        || (*a <= 127 && *b <= 127 && (*a as u8).eq_ignore_ascii_case(&(*b as u8)))
                })
            {
                value.splice(..unc.len(), [b'\\' as u16, b'\\' as u16]);
            } else if value.starts_with(&[92, 92, 63, 92]) {
                value.drain(..4);
            }
            while value.last() == Some(&(b'\\' as u16)) {
                value.pop();
            }
            value
        }
        let scope = units(scope);
        let path = units(path);
        if scope.is_empty()
            || path.len() < scope.len()
            || (path.len() != scope.len() && path[scope.len()] != b'\\' as u16)
        {
            return false;
        }
        if scope == path[..scope.len()] {
            return true;
        }
        let Ok(length) = i32::try_from(scope.len()) else {
            return false;
        };
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CompareStringOrdinal(
                a: *const u16,
                a_len: i32,
                b: *const u16,
                b_len: i32,
                ignore_case: i32,
            ) -> i32;
        }
        // Both pointers refer to length valid UTF-16 buffers; this API accepts
        // counted native units and performs no allocation or network access.
        unsafe { CompareStringOrdinal(scope.as_ptr(), length, path.as_ptr(), length, 1) == 2 }
    }
}

fn valid_path(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        && native(path.as_os_str()).encode().len() <= 65536
        && !path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
}
fn valid_registered_path(kind: Kind, path: &Path) -> bool {
    if kind == Kind::SmbServer {
        // Server keys deliberately do not pass the filesystem absolute-path
        // validator. Only the dedicated syntax parser accepts this kind.
        #[cfg(any(windows, target_os = "linux"))]
        return path.to_str().is_some_and(|value| {
            smb::parse_target(value)
                .is_ok_and(|target| target.share.is_none() && target.server_unc() == path)
        });
        #[cfg(not(any(windows, target_os = "linux")))]
        return false;
    }
    #[cfg(target_os = "linux")]
    if kind == Kind::Smb && is_smb_uri(path) {
        return path.to_str().is_some_and(|value| {
            smb::parse_target(value).is_ok_and(|target| target.share.as_deref() == Some(path))
        });
    }
    if !valid_path(path) {
        return false;
    }
    #[cfg(windows)]
    if kind == Kind::Smb
        && let Some(std::path::Component::Prefix(prefix)) = path.components().next()
        && let std::path::Prefix::UNC(_, share) | std::path::Prefix::VerbatimUNC(_, share) =
            prefix.kind()
        && share
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("IPC$"))
    {
        return false;
    }
    #[cfg(not(windows))]
    let _ = kind;
    true
}

#[cfg(target_os = "linux")]
fn is_smb_uri(path: &Path) -> bool {
    path.to_str().is_some_and(|path| path.starts_with("smb://"))
}

/// Resolve a logical SMB bookmark only at an explicit acquisition boundary.
/// The native adapter only inspects existing mounts here; it never connects.
pub(crate) fn scan_root(path: &Path) -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if is_smb_uri(path) || path.to_str().is_some_and(|p| p.starts_with(r"\\")) {
        return smb::resolve_path(path).map_err(|_| io::ErrorKind::NotFound.into());
    }
    Ok(path.to_owned())
}

#[cfg(target_os = "linux")]
fn matches_protocol(volume: &Volume, kind: Kind) -> bool {
    let VolumeIdentity::Linux { filesystem, .. } = &volume.identity else {
        return false;
    };
    match kind {
        Kind::Smb => ["cifs", "smb3", "smbfs"]
            .iter()
            .any(|fs| filesystem == OsStr::new(fs)),
        Kind::Nfs => ["nfs", "nfs4"]
            .iter()
            .any(|fs| filesystem == OsStr::new(fs)),
        Kind::Local => true,
        Kind::SmbServer => false,
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    pub entries: Vec<Source>,
    store: Option<Store>,
    admission: Arc<RwLock<Admission>>,
}

/// Adding a bookmark is never an implicit request to clear a user exclusion.
/// Only `Added` guarantees that the source can appear in the active projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AddOutcome {
    Added,
    ReconnectRequired(Source),
    ParentDisconnected(Source),
}

#[derive(Default)]
struct Admission {
    exclusions: Vec<PathBuf>,
    invalid_store: bool,
    revision: u64,
    revocations: Vec<(PathBuf, u64)>,
}
impl Admission {
    fn denies(&self, path: &Path) -> bool {
        self.invalid_store || self.exclusions.iter().any(|scope| covers(scope, path))
    }
    fn revoked_at(&self, path: &Path) -> u64 {
        self.revocations
            .iter()
            .filter(|(scope, _)| covers(scope, path))
            .map(|(_, revision)| *revision)
            .max()
            .unwrap_or(0)
    }
    fn revoke(&mut self, path: PathBuf) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("source revocation exhausted");
        if let Some((_, revision)) = self
            .revocations
            .iter_mut()
            .find(|(scope, _)| scope == &path)
        {
            *revision = self.revision;
        } else {
            self.revocations.push((path, self.revision));
        }
    }
}
impl Registry {
    pub fn open(path: PathBuf) -> Self {
        let (entries, invalid_store) = match read_entries(&path) {
            Ok(entries) => (entries, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
            Err(_) => (Vec::new(), true),
        };
        let mut registry = Self {
            entries,
            // Never overwrite an unreadable/corrupt policy with an empty list.
            store: (!invalid_store).then(|| Store::start(path).ok()).flatten(),
            admission: Arc::new(RwLock::new(Admission {
                invalid_store,
                ..Default::default()
            })),
        };
        registry.publish_admission();
        registry
    }
    fn publish_admission(&mut self) {
        self.admission
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .exclusions = self
            .entries
            .iter()
            .filter(|source| source.disconnected)
            .map(|source| source.path.clone())
            .collect();
    }
    pub fn is_disconnected(&self, path: &Path) -> bool {
        self.admission
            .read()
            .map_or(true, |policy| policy.denies(path))
    }
    /// Explicit exclusions for the management dialog, independent of the active
    /// discovery. Reconnecting a server restores its non-excluded children;
    /// explicitly disconnected shares retain their own separate choice.
    pub fn disconnected_sources(&self) -> Vec<Source> {
        self.entries
            .iter()
            .filter(|source| source.disconnected)
            .cloned()
            .collect()
    }
    /// The broad exclusion must be cleared first, even when the child also has
    /// its own exclusion. Equivalent SMB spellings are the same policy scope,
    /// without asserting identity or accessing the server.
    pub fn disconnected_parent(&self, path: &Path) -> Option<&Source> {
        self.entries
            .iter()
            .filter(|source| {
                source.disconnected && covers(&source.path, path) && !covers(path, &source.path)
            })
            .min_by_key(|source| !source.is_server())
    }
    /// Called only after the UI has obtained explicit confirmation. The policy
    /// is revoked immediately; persistence success is reported by poll_saved.
    pub fn disconnect(&mut self, mut source: Source) -> io::Result<()> {
        if source.kind == Kind::Local || !valid_registered_path(source.kind, &source.path) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        source.disconnected = true;
        let path = source.path.clone();
        self.store_source(source)?;
        self.admission
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .revoke(path);
        Ok(())
    }
    /// Clears only the exact user exclusion. A disabled parent continues to
    /// exclude its descendants, and explicit child exclusions remain intact.
    pub fn reconnect(&mut self, path: &Path) -> io::Result<()> {
        if self
            .admission
            .read()
            .map_or(true, |policy| policy.invalid_store)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if self.disconnected_parent(path).is_some() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let source = self
            .entries
            .iter_mut()
            .find(|source| source.path == path)
            .ok_or(io::ErrorKind::NotFound)?;
        source.disconnected = false;
        self.publish_admission();
        if let Some(store) = &mut self.store {
            store.submit(self.entries.clone());
        }
        Ok(())
    }
    pub fn find(&self, path: &Path) -> Option<&Source> {
        let exact = self.entries.iter().find(|source| source.path == path);
        #[cfg(target_os = "linux")]
        {
            exact.or_else(|| {
                let logical =
                    if is_smb_uri(path) || path.to_str().is_some_and(|p| p.starts_with(r"\\")) {
                        path.to_owned()
                    } else {
                        smb::logical_path(path)?
                    };
                self.entries.iter().find(|source| {
                    smb::covers(&source.path, &logical) && smb::covers(&logical, &source.path)
                })
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            exact
        }
    }
    pub fn add(&mut self, source: Source) -> io::Result<AddOutcome> {
        self.validate_source(&source)?;
        if source.disconnected {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if let Some(parent) = self.disconnected_parent(&source.path) {
            return Ok(AddOutcome::ParentDisconnected(parent.clone()));
        }
        if let Some(existing) = self.entries.iter().find(|existing| {
            existing.disconnected
                && covers(&existing.path, &source.path)
                && covers(&source.path, &existing.path)
        }) {
            return Ok(AddOutcome::ReconnectRequired(existing.clone()));
        }
        self.store_source(source)?;
        Ok(AddOutcome::Added)
    }
    fn validate_source(&self, source: &Source) -> io::Result<()> {
        if self
            .admission
            .read()
            .map_or(true, |policy| policy.invalid_store)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if !valid_registered_path(source.kind, &source.path)
            || (source.kind == Kind::Local && source.disconnected)
            || source.label.len() > 1024
            || source.label.chars().any(char::is_control)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    }
    /// Used by explicit disconnect as well as accepted additions. This is the
    /// only writer; callers must decide whether changing the policy is allowed.
    fn store_source(&mut self, mut source: Source) -> io::Result<()> {
        self.validate_source(&source)?;
        if let Some(existing) = self.entries.iter_mut().find(|s| s.path == source.path) {
            // Exact native location only. Names, case folds and server aliases
            // are never evidence of identical storage.
            if existing.disconnected && source.kind == Kind::Local {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            source.disconnected |= existing.disconnected;
            *existing = source;
        } else {
            if self.entries.len() >= MAX_SOURCES {
                return Err(io::ErrorKind::StorageFull.into());
            }
            self.entries.push(source);
        }
        self.publish_admission();
        if let Some(store) = &mut self.store {
            store.submit(self.entries.clone());
        }
        Ok(())
    }
    pub fn saving_available(&self) -> bool {
        self.store.is_some()
    }
    pub fn poll_saved(&self) -> Option<bool> {
        self.store.as_ref()?.poll()
    }
    pub fn scan_preflight(&self, path: &Path) -> Option<crate::directory_worker::Preflight> {
        let source = self.find(path)?.clone();
        let native = scan_root(&source.path).ok();
        let admission = self.admission.clone();
        let revision = admission
            .read()
            .map_or(u64::MAX, |policy| policy.revoked_at(&source.path));
        Some(Box::new(move || {
            let allowed = || {
                admission.read().is_ok_and(|policy| {
                    !policy.denies(&source.path) && policy.revoked_at(&source.path) == revision
                })
            };
            // Native access may block. Recheck after it returns so a revocation
            // during that call cannot authorize a delayed acquisition.
            allowed()
                && native.is_some()
                && scan_root(&source.path).ok() == native
                && preflight(&source).is_ok()
                && allowed()
                && scan_root(&source.path).ok() == native
        }))
    }
}
pub(crate) fn config_path() -> Option<PathBuf> {
    crate::preferences_store::config_path().map(|p| p.with_file_name("added-sources-v1.conf"))
}

/// Explicit check / manual-analysis preflight only. No listing or file content.
pub(crate) fn preflight(source: &Source) -> io::Result<()> {
    if source.disconnected {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // A virtual server is never opened as a directory, including on an explicit
    // access check. Its bounded native share discovery has a separate action.
    if source.is_server() {
        return Err(io::ErrorKind::Unsupported.into());
    }
    #[cfg(windows)]
    if source.kind != Kind::Local {
        let expected = if source.kind == Kind::Smb {
            NetworkProtocol::Smb
        } else {
            NetworkProtocol::Nfs
        };
        if namespace_network_protocol(&source.path)? != expected {
            return Err(io::ErrorKind::NotFound.into());
        }
    }
    let root = scan_root(&source.path)?;
    #[cfg(target_os = "linux")]
    if source.kind != Kind::Local && !is_smb_uri(&source.path) {
        let volumes = NativeVolumes.enumerate()?;
        if !volumes.iter().any(|v| {
            matches_protocol(v, source.kind) && v.mounts.iter().any(|m| m.path == source.path)
        }) {
            return Err(io::ErrorKind::NotFound.into());
        }
    }
    let guard = sortsize_core::acquisition::Guard::open(
        &root,
        sortsize_core::acquisition::Policy {
            explicit_root: source.kind != Kind::Local,
            ..Default::default()
        },
    )?;
    if !guard.evidence().1.directory {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(())
}

pub(crate) fn merge(discovery: &mut VolumeDiscovery, entries: &[Source]) {
    let excluded = |path: &Path| {
        entries
            .iter()
            .any(|source| source.disconnected && covers(&source.path, path))
    };
    // Filter both already-published rows and fresh native discovery before
    // merging bookmarks. A still-mounted source is not a reconnection intent.
    discovery.volumes.retain_mut(|volume| {
        if is_technical_mount(volume) || native_smb_key(volume).as_deref().is_some_and(excluded) {
            return false;
        }
        volume.mounts.retain(|mount| !excluded(&mount.path));
        !volume.mounts.is_empty()
    });
    for source in entries.iter().filter(|source| !excluded(&source.path)) {
        #[cfg(target_os = "linux")]
        let native_alias = is_smb_uri(&source.path)
            .then(|| scan_root(&source.path).ok())
            .flatten();
        #[cfg(not(target_os = "linux"))]
        let native_alias: Option<PathBuf> = None;
        if let Some(volume) = discovery.volumes.iter_mut().find(|v| {
            v.mounts
                .iter()
                .any(|m| m.path == source.path || native_alias.as_ref() == Some(&m.path))
        }) {
            if let Some(label) = source_label(source) {
                volume.label = MetadataValue::Known {
                    value: label.into(),
                    accuracy: ValueAccuracy::Exact,
                };
            }
            // Explicit bookmarks are selected to prepare a separate Analyze
            // command, even if they refer to an otherwise local volume.
            if source.kind != Kind::Local {
                volume.kind = SourceKind::Remote;
            }
            if let Some(index) = volume.mounts.iter().position(|m| m.path == source.path) {
                volume.mounts.swap(0, index);
            } else if native_alias.is_some() {
                // The primary row keeps its durable logical key; the proven
                // native mount remains an alias for the existing engine.
                volume.mounts.insert(
                    0,
                    VolumeMount {
                        path: source.path.clone(),
                        mount_id: None,
                        root: None,
                    },
                );
            }
            continue;
        }
        discovery.volumes.push(Volume {
            // Location-only fallback, including on Unix: no physical identity
            // is invented while its native mount is offline.
            identity: VolumeIdentity::WindowsPath(source.path.clone()),
            label: source_label(source).map_or(MetadataValue::NotCollected, |label| {
                MetadataValue::Known {
                    value: label.into(),
                    accuracy: ValueAccuracy::Exact,
                }
            }),
            filesystem: MetadataValue::NotCollected,
            kind: if source.kind == Kind::Local {
                SourceKind::Unknown
            } else {
                SourceKind::Remote
            },
            mounts: vec![VolumeMount {
                path: source.path.clone(),
                mount_id: None,
                root: None,
            }],
            capacity: VolumeCapacity::absent(MetadataValue::NotCollected),
            availability: VolumeAvailability::NotProbed,
        });
    }
}

fn is_technical_mount(volume: &Volume) -> bool {
    // GVfs exposes a technical FUSE namespace, not one user storage source.
    // Keep NativeVolumes unchanged: SMB resolution still needs this mount to
    // find its directly analysable share directories.
    matches!(&volume.identity, VolumeIdentity::Linux { filesystem, .. }
        if filesystem == OsStr::new("fuse.gvfsd-fuse"))
}

fn native_smb_key(volume: &Volume) -> Option<PathBuf> {
    // Linux's mount identity includes the logical remote source. This also
    // filters CIFS aliases and bind mounts without relying on a live OS probe.
    #[cfg(target_os = "linux")]
    if let VolumeIdentity::Linux {
        filesystem, source, ..
    } = &volume.identity
        && ["cifs", "smb3", "smbfs"]
            .iter()
            .any(|fs| filesystem == OsStr::new(fs))
    {
        return smb::parse_target(source.to_str()?).ok()?.share;
    }
    let _ = volume;
    None
}
fn source_label(source: &Source) -> Option<String> {
    match source.kind {
        Kind::Smb => Some(format!("{} (SMB)", source.label)),
        Kind::Nfs => Some(format!("{} (NFS)", source.label)),
        Kind::Local => (!source.label.is_empty()).then(|| source.label.clone()),
        Kind::SmbServer => Some(if source.label.is_empty() {
            format!(
                "NAS - {}",
                source.path.to_string_lossy().trim_start_matches('\\')
            )
        } else {
            format!("{} (NAS SMB)", source.label)
        }),
    }
}

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
fn decode_native(bytes: &[u8]) -> Option<OsString> {
    match NativeName::decode(bytes).ok()? {
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
fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u64).to_be_bytes());
    out.extend(bytes);
}
fn encode(entries: &[Source]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend((entries.len() as u64).to_be_bytes());
    for source in entries {
        out.push(match source.kind {
            Kind::Smb => 0,
            Kind::Nfs => 1,
            Kind::Local => 2,
            Kind::SmbServer => 3,
        });
        out.push(u8::from(source.disconnected));
        field(&mut out, &native(source.path.as_os_str()).encode());
        field(&mut out, source.label.as_bytes());
    }
    out
}
struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (value, rest) = self.0.split_at_checked(length)?;
        self.0 = rest;
        Some(value)
    }
    fn number(&mut self) -> Option<usize> {
        usize::try_from(u64::from_be_bytes(self.take(8)?.try_into().ok()?)).ok()
    }
    fn field(&mut self) -> Option<&'a [u8]> {
        let len = self.number()?;
        self.take(len)
    }
}
fn decode(bytes: &[u8]) -> Option<Vec<Source>> {
    if bytes.len() > MAX_BYTES || !(bytes.starts_with(MAGIC) || bytes.starts_with(MAGIC_V1)) {
        return None;
    }
    let version_two = bytes.starts_with(MAGIC);
    let mut d = Decoder(&bytes[MAGIC.len()..]);
    let count = d.number()?;
    if count > MAX_SOURCES {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = Kind::from_index(i32::from(d.take(1)?[0]))?;
        let disconnected = if version_two {
            match d.take(1)?[0] {
                0 => false,
                1 if kind != Kind::Local => true,
                _ => return None,
            }
        } else {
            false
        };
        let path = PathBuf::from(decode_native(d.field()?)?);
        let label = String::from_utf8(d.field()?.to_vec()).ok()?;
        if !valid_registered_path(kind, &path)
            || label.len() > 1024
            || label.chars().any(char::is_control)
            || entries.iter().any(|s: &Source| s.path == path)
        {
            return None;
        }
        entries.push(Source {
            kind,
            path,
            label,
            disconnected,
        });
    }
    d.0.is_empty().then_some(entries)
}
#[cfg(test)]
fn load(path: &Path) -> Option<Vec<Source>> {
    read_entries(path).ok()
}
fn read_entries(path: &Path) -> io::Result<Vec<Source>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    decode(&bytes).ok_or_else(|| io::ErrorKind::InvalidData.into())
}
fn save(path: &Path, entries: &[Source]) -> io::Result<()> {
    let bytes = encode(entries);
    if bytes.len() > MAX_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
#[derive(Default)]
struct Pending {
    latest: Option<(u64, Vec<Source>)>,
    result: Option<(u64, bool)>,
    closing: bool,
}
struct Store {
    state: Arc<(Mutex<Pending>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
    serial: u64,
}
impl Store {
    fn start(path: PathBuf) -> io::Result<Self> {
        let state = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let shared = state.clone();
        let worker = std::thread::Builder::new()
            .name("bunny-added-sources".into())
            .spawn(move || {
                loop {
                    let (serial, entries) = {
                        let (lock, changed) = &*shared;
                        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        let mut state = changed
                            .wait_while(state, |s| s.latest.is_none() && !s.closing)
                            .unwrap_or_else(|e| e.into_inner());
                        match state.latest.take() {
                            Some(next) => next,
                            None => break,
                        }
                    };
                    let result = save(&path, &entries).is_ok();
                    shared.0.lock().unwrap_or_else(|e| e.into_inner()).result =
                        Some((serial, result));
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
            serial: 0,
        })
    }
    fn submit(&mut self, entries: Vec<Source>) {
        self.serial = self.serial.wrapping_add(1);
        self.state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest = Some((self.serial, entries));
        self.state.1.notify_one();
    }
    fn poll(&self) -> Option<bool> {
        self.state
            .0
            .try_lock()
            .ok()?
            .result
            .take()
            .filter(|(serial, _)| *serial == self.serial)
            .map(|(_, success)| success)
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
    fn source(path: PathBuf) -> Source {
        Source {
            kind: Kind::Smb,
            path,
            label: "NAS \u{e9}t\u{e9} \u{65e5}\u{672c}\u{8a9e}".into(),
            disconnected: false,
        }
    }
    #[test]
    fn reconnect_never_rearms_an_old_preflight_or_revokes_an_unrelated_root() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let other = temp.path().join("other");
        let mut policy = Admission::default();
        let old_first = policy.revoked_at(&first);
        let old_other = policy.revoked_at(&other);
        policy.revoke(first.clone());
        assert_ne!(policy.revoked_at(&first.join("child")), old_first);
        assert_eq!(policy.revoked_at(&other), old_other);
        // Reconnection clears the persistent exclusion but never erases the
        // revocation epoch captured by work from before the confirmation.
        policy.exclusions.clear();
        assert!(!policy.denies(&first));
        assert_ne!(policy.revoked_at(&first), old_first);
    }
    #[test]
    fn exclusion_survives_restart_readding_and_explicit_reconnect_only() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("policy");
        let item = source(temp.path().join("unavailable-share"));
        let descendant = item.path.join("folder");
        {
            let mut registry = Registry::open(config.clone());
            assert_eq!(registry.add(item.clone()).unwrap(), AddOutcome::Added);
            let pending = registry.scan_preflight(&item.path).unwrap();
            registry.disconnect(item.clone()).unwrap();
            assert!(registry.is_disconnected(&descendant));
            assert!(!pending());
            let disconnected = registry.entries[0].clone();
            assert_eq!(
                registry.add(item.clone()).unwrap(),
                AddOutcome::ReconnectRequired(disconnected)
            );
            assert!(registry.is_disconnected(&item.path));
            // No source path exists and no acquisition was performed.
            assert!(!item.path.exists());
        }
        {
            let mut registry = Registry::open(config.clone());
            assert!(registry.is_disconnected(&descendant));
            assert!(!registry.is_disconnected(&temp.path().join("another-share")));
            assert_eq!(registry.entries.len(), 1);
            assert!(registry.entries[0].disconnected);
            registry.reconnect(&item.path).unwrap();
            assert!(!registry.is_disconnected(&descendant));
            assert!(!item.path.exists());
        }
        assert!(!Registry::open(config).is_disconnected(&descendant));
    }
    #[test]
    fn readding_disconnected_source_preserves_bookmark_and_persisted_policy() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("policy");
        let original = source(temp.path().join("offline-share"));
        {
            let mut registry = Registry::open(config.clone());
            assert_eq!(registry.add(original.clone()).unwrap(), AddOutcome::Added);
            registry.disconnect(original.clone()).unwrap();
        }
        let bytes = fs::read(&config).unwrap();
        let mut requested = original.clone();
        requested.label = "Replacement must wait for explicit reconnection".into();
        {
            let mut registry = Registry::open(config.clone());
            let saved = registry.entries[0].clone();
            assert_eq!(
                registry.add(requested).unwrap(),
                AddOutcome::ReconnectRequired(saved.clone())
            );
            assert_eq!(registry.entries, [saved]);
            assert!(registry.is_disconnected(&original.path));
            assert!(registry.poll_saved().is_none());
        }
        assert_eq!(fs::read(config).unwrap(), bytes);
        assert!(!original.path.exists());
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn readding_share_requires_parent_first_and_preserves_separate_child_exclusions() {
        let server = Source::new(3, r"\\offline.invalid".into(), "NAS", None).unwrap();
        let child = Source::new(0, r"\\offline.invalid\Bouquins".into(), "Books", None).unwrap();
        let sibling = Source::new(0, r"\\offline.invalid\Music".into(), "Music", None).unwrap();
        let future = Source::new(0, r"\\offline.invalid\Photos".into(), "Photos", None).unwrap();
        let mut registry = Registry::default();
        for entry in [&server, &child, &sibling] {
            assert_eq!(registry.add(entry.clone()).unwrap(), AddOutcome::Added);
        }
        registry.disconnect(child.clone()).unwrap();
        registry.disconnect(server.clone()).unwrap();
        let before = registry.entries.clone();
        let excluded_server = registry.find(&server.path).unwrap().clone();
        for requested in [&child, &future] {
            assert_eq!(
                registry.add(requested.clone()).unwrap(),
                AddOutcome::ParentDisconnected(excluded_server.clone())
            );
        }
        assert_eq!(registry.entries, before);
        assert_eq!(
            registry.reconnect(&child.path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(registry.entries, before);
        registry.reconnect(&server.path).unwrap();
        assert!(!registry.is_disconnected(&sibling.path));
        assert!(registry.is_disconnected(&child.path));
        let excluded_child = registry.find(&child.path).unwrap().clone();
        assert_eq!(
            registry.add(child.clone()).unwrap(),
            AddOutcome::ReconnectRequired(excluded_child)
        );
        assert_eq!(registry.add(future.clone()).unwrap(), AddOutcome::Added);
        assert!(!registry.is_disconnected(&future.path));
        registry.reconnect(&child.path).unwrap();
        assert_eq!(registry.add(child.clone()).unwrap(), AddOutcome::Added);
        assert!(!registry.is_disconnected(&child.path));
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn readding_excluded_smb_alias_does_not_register_a_hidden_duplicate() {
        let original = Source::new(0, r"\\offline.invalid\Books".into(), "Books", None).unwrap();
        let alias = Source::new(0, r"\\OFFLINE.INVALID\books".into(), "Renamed", None).unwrap();
        let mut registry = Registry::default();
        registry.disconnect(original).unwrap();
        let before = registry.entries.clone();
        assert_eq!(
            registry.add(alias).unwrap(),
            AddOutcome::ReconnectRequired(before[0].clone())
        );
        assert_eq!(registry.entries, before);
    }
    #[test]
    fn addition_cannot_claim_active_success_for_an_excluded_input() {
        let temp = tempfile::tempdir().unwrap();
        let mut excluded = source(temp.path().join("offline-share"));
        excluded.disconnected = true;
        let mut registry = Registry::default();
        assert_eq!(
            registry.add(excluded).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(registry.entries.is_empty());
    }
    #[test]
    fn legacy_bookmarks_load_as_connected_without_rewriting_on_open() {
        let temp = tempfile::tempdir().unwrap();
        let item = source(temp.path().join("legacy"));
        let mut bytes = MAGIC_V1.to_vec();
        bytes.extend(1u64.to_be_bytes());
        bytes.push(0);
        field(&mut bytes, &native(item.path.as_os_str()).encode());
        field(&mut bytes, item.label.as_bytes());
        let config = temp.path().join("legacy-policy");
        fs::write(&config, &bytes).unwrap();
        {
            let registry = Registry::open(config.clone());
            assert_eq!(registry.entries.as_slice(), std::slice::from_ref(&item));
            assert!(!registry.is_disconnected(&item.path));
        }
        assert_eq!(fs::read(config).unwrap(), bytes);
    }
    #[test]
    fn unreadable_policy_fails_closed_and_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("corrupt");
        let bytes = b"corrupt policy must not become authorization";
        fs::write(&config, bytes).unwrap();
        let mut registry = Registry::open(config.clone());
        let item = source(temp.path().join("source"));
        assert!(registry.is_disconnected(&item.path));
        assert!(!registry.saving_available());
        assert!(registry.add(item.clone()).is_err());
        assert!(registry.reconnect(&item.path).is_err());
        drop(registry);
        assert_eq!(fs::read(config).unwrap(), bytes);
    }
    #[cfg(windows)]
    #[test]
    fn server_exclusion_covers_future_shares_without_clearing_explicit_children() {
        let server = Source::new(3, r"\\nas.invalid\".into(), "NAS", None).unwrap();
        let tv = Source::new(0, r"\\nas.invalid\TV".into(), "TV", None).unwrap();
        let mut registry = Registry::default();
        registry.disconnect(tv.clone()).unwrap();
        registry.disconnect(server.clone()).unwrap();
        let future = Path::new(r"\\NAS.INVALID\new-share\folder");
        assert!(registry.is_disconnected(future));
        assert_eq!(
            registry.reconnect(&tv.path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(registry.is_disconnected(&tv.path)); // Parent still excludes it.
        registry.reconnect(&server.path).unwrap();
        assert!(!registry.is_disconnected(future));
        assert!(registry.is_disconnected(&tv.path)); // Explicit child is retained.
        registry.reconnect(&tv.path).unwrap();
        assert!(!registry.is_disconnected(&tv.path));
    }
    #[cfg(windows)]
    #[test]
    fn windows_scope_matching_is_native_bounded_and_spelling_safe() {
        let root = Path::new(r"\\nas.invalid\été");
        assert!(covers(root, Path::new(r"\\NAS.INVALID\ÉTÉ\folder")));
        assert!(covers(root, Path::new(r"\\?\UNC\nas.invalid\été\folder")));
        assert!(!covers(root, Path::new(r"\\nas.invalid\été-other")));
        assert!(!covers(root, Path::new(r"\\another.invalid\été")));
        use std::os::windows::ffi::OsStringExt;
        let native = PathBuf::from(OsString::from_wide(&[92, 92, 110, 97, 115, 92, 0xd800]));
        assert!(covers(&native, &native.join("child")));
    }
    #[test]
    fn bookmarks_restore_offline_without_touching_target_or_claiming_identity() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("offline");
        let source = source(missing.clone());
        save(&temp.path().join("sources"), std::slice::from_ref(&source)).unwrap();
        let entries = load(&temp.path().join("sources")).unwrap();
        assert_eq!(entries, [source]);
        assert!(!missing.exists());
        let mut d = VolumeDiscovery {
            volumes: vec![],
            enumeration_time: Default::default(),
            capacity_time: Default::default(),
            capacity_calls: 0,
        };
        merge(&mut d, &entries);
        assert_eq!(d.capacity_calls, 0);
        assert_eq!(d.volumes[0].availability, VolumeAvailability::NotProbed);
        assert!(!crate::volume_view::can_auto_scan(&d.volumes[0]));
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn active_projection_respects_persisted_server_and_share_choices() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("policy");
        let server = Source::new(3, r"\\offline.invalid".into(), "NAS", None).unwrap();
        let share = Source::new(0, r"\\offline.invalid\TV".into(), "TV", None).unwrap();
        let sibling = Source::new(0, r"\\offline.invalid\Music".into(), "Music", None).unwrap();
        let other = Source::new(0, r"\\other.invalid\TV".into(), "Other", None).unwrap();
        let mut registry = Registry::open(config.clone());
        for source in [&server, &share, &sibling, &other] {
            registry.add(source.clone()).unwrap();
        }
        let mut visible = VolumeDiscovery {
            volumes: vec![],
            enumeration_time: Default::default(),
            capacity_time: Default::default(),
            capacity_calls: 0,
        };
        merge(&mut visible, &registry.entries);
        let fresh_volumes = visible.volumes.clone();
        assert_eq!(visible.volumes.len(), 4);
        registry.disconnect(share.clone()).unwrap();
        merge(&mut visible, &registry.entries);
        assert_eq!(visible.volumes.len(), 3);
        assert!(
            visible
                .volumes
                .iter()
                .all(|v| v.mounts[0].path != share.path)
        );
        assert_eq!(registry.disconnected_sources().len(), 1);
        registry.disconnect(server.clone()).unwrap();
        merge(&mut visible, &registry.entries);
        assert_eq!(visible.volumes.len(), 1);
        assert_eq!(visible.volumes[0].mounts[0].path, other.path);
        drop(registry);

        let mut registry = Registry::open(config);
        let mut rediscovered = VolumeDiscovery {
            volumes: fresh_volumes,
            enumeration_time: Default::default(),
            capacity_time: Default::default(),
            capacity_calls: 0,
        };
        merge(&mut rediscovered, &registry.entries);
        assert_eq!(rediscovered.volumes.len(), 1);
        assert_eq!(registry.disconnected_sources().len(), 2);
        registry.reconnect(&server.path).unwrap();
        merge(&mut rediscovered, &registry.entries);
        assert_eq!(rediscovered.volumes.len(), 3);
        assert!(
            rediscovered
                .volumes
                .iter()
                .all(|v| v.mounts[0].path != share.path)
        );
        registry.reconnect(&share.path).unwrap();
        merge(&mut rediscovered, &registry.entries);
        assert_eq!(rediscovered.volumes.len(), 4);
        assert_eq!(rediscovered.capacity_calls, 0);
        assert!(rediscovered.volumes.iter().all(|v| {
            v.availability == VolumeAvailability::NotProbed && !crate::volume_view::can_auto_scan(v)
        }));
        assert!(registry.disconnected_sources().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn projection_hides_gvfs_namespace_and_revoked_cifs_aliases_but_keeps_nfs() {
        let native = |filesystem: &str, source: &str, path: &str| Volume {
            identity: VolumeIdentity::Linux {
                major: 0,
                minor: 1,
                filesystem: filesystem.into(),
                source: source.into(),
            },
            label: MetadataValue::NotCollected,
            filesystem: MetadataValue::NotCollected,
            kind: SourceKind::Remote,
            mounts: vec![VolumeMount {
                path: path.into(),
                mount_id: Some(1),
                root: Some("/".into()),
            }],
            capacity: VolumeCapacity::absent(MetadataValue::NotCollected),
            availability: VolumeAvailability::NotProbed,
        };
        let mut discovery = VolumeDiscovery {
            volumes: vec![
                native("fuse.gvfsd-fuse", "gvfsd-fuse", "/run/user/1000/gvfs"),
                native("cifs", "//offline.invalid/TV", "/synthetic-cifs-tv"),
                native("cifs", "//offline.invalid/Music", "/synthetic-cifs-music"),
                native("nfs4", "offline.invalid:/TV", "/synthetic-nfs"),
            ],
            enumeration_time: Default::default(),
            capacity_time: Default::default(),
            capacity_calls: 0,
        };
        merge(&mut discovery, &[]);
        assert_eq!(discovery.volumes.len(), 3);
        let mut registry = Registry::default();
        let share = Source::new(0, "smb://offline.invalid/tv".into(), "TV", None).unwrap();
        registry.disconnect(share).unwrap();
        merge(&mut discovery, &registry.entries);
        assert_eq!(discovery.volumes.len(), 2);
        assert_eq!(
            discovery.volumes[0].mounts[0].path,
            Path::new("/synthetic-cifs-music")
        );
        let server = Source::new(3, "offline.invalid".into(), "NAS", None).unwrap();
        registry.disconnect(server).unwrap();
        merge(&mut discovery, &registry.entries);
        assert_eq!(discovery.volumes.len(), 1);
        assert_eq!(
            discovery.volumes[0].mounts[0].path,
            Path::new("/synthetic-nfs")
        );
    }
    #[test]
    fn corrupted_and_oversized_registry_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let s = source(temp.path().join("NAS"));
        let bytes = encode(std::slice::from_ref(&s));
        assert_eq!(decode(&bytes), Some(vec![s]));
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_none());
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(decode(&extra).is_none());
        assert!(decode(&vec![0; MAX_BYTES + 1]).is_none());
    }
    #[test]
    fn adding_only_deduplicates_exact_native_locations() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        registry.add(source(temp.path().join("server"))).unwrap();
        registry.add(source(temp.path().join("server"))).unwrap();
        registry
            .add(source(temp.path().join("other-alias")))
            .unwrap();
        assert_eq!(registry.entries.len(), 2);
    }
    #[cfg(windows)]
    #[test]
    fn virtual_server_addition_is_pure_normalized_and_distinct_from_share_roots() {
        let mut registry = Registry::default();
        for input in [
            "offline.invalid",
            r"\\offline.invalid",
            r"\\offline.invalid\",
        ] {
            let server = Source::new(3, input.into(), "", None).unwrap();
            assert!(server.is_server());
            assert_eq!(server.path, Path::new(r"\\offline.invalid"));
            let target = server.server_target().unwrap();
            assert_eq!(target.server, OsStr::new("offline.invalid"));
            assert!(target.share.is_none());
            registry.add(server).unwrap();
        }
        assert_eq!(registry.entries.len(), 1);
        let share = Source::new(0, r"\\offline.invalid\TV".into(), "TV", None).unwrap();
        assert!(!share.is_server());
        assert!(share.server_target().is_none());
        registry.add(share).unwrap();
        assert_eq!(registry.entries.len(), 2);
        for invalid in [
            r"\\offline.invalid\TV",
            r"\\offline.invalid\TV\child",
            r"C:\",
            "smb://offline.invalid",
            "offline.invalid\\..",
        ] {
            assert!(Source::new(3, invalid.into(), "", None).is_err());
        }
        assert!(Source::new(0, r"\\offline.invalid\".into(), "", None).is_err());
        assert!(Source::new(3, "offline.invalid".into(), "secret\nlabel", None).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn virtual_server_restores_offline_without_acquisition_preflight_or_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("server-and-legacy-sources");
        let local = Source::new(2, temp.path().into(), "Local", None).unwrap();
        let share = Source::new(0, r"\\offline.invalid\TV".into(), "TV", None).unwrap();
        let server = Source::new(3, r"\\offline.invalid\".into(), "", None).unwrap();
        let legacy = vec![local, share];
        // Kind discriminants stay unchanged; the versioned codec preserves all
        // bookmarks and adds the durable user exclusion without touching SQLite.
        let legacy_bytes = encode(&legacy);
        assert!(legacy_bytes.starts_with(MAGIC));
        assert_eq!(decode(&legacy_bytes), Some(legacy.clone()));
        let mut entries = legacy;
        entries.push(server.clone());
        {
            let mut registry = Registry::open(path.clone());
            for entry in &entries {
                registry.add(entry.clone()).unwrap();
            }
        }
        let registry = Registry::open(path.clone());
        assert_eq!(registry.entries, entries);
        assert_eq!(load(&path), Some(entries));
        assert_eq!(
            preflight(&server).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        // Even if the generic scan preparation is called accidentally, it is
        // denied synchronously without opening the virtual server as a file.
        assert!(!(registry.scan_preflight(&server.path).unwrap())());
        let mut discovery = VolumeDiscovery {
            volumes: vec![],
            enumeration_time: Default::default(),
            capacity_time: Default::default(),
            capacity_calls: 0,
        };
        merge(&mut discovery, std::slice::from_ref(&server));
        assert_eq!(discovery.capacity_calls, 0);
        assert_eq!(discovery.volumes.len(), 1);
        let volume = &discovery.volumes[0];
        assert_eq!(volume.kind, SourceKind::Remote);
        assert_eq!(volume.identity, VolumeIdentity::WindowsPath(server.path));
        assert_eq!(volume.availability, VolumeAvailability::NotProbed);
        assert_eq!(volume.capacity.total, MetadataValue::NotCollected);
        assert!(!crate::volume_view::can_auto_scan(volume));
        assert!(
            matches!(&volume.label, MetadataValue::Known { value, .. } if value == OsStr::new("NAS - offline.invalid"))
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn logical_server_revocation_covers_canonical_unc_and_only_its_own_shares() {
        let mut registry = Registry::default();
        let server = Source::new(3, "smb://NAS.invalid/".into(), "NAS", None).unwrap();
        let share = Source::new(0, r"\\nas.invalid\TV".into(), "TV", None).unwrap();
        registry.add(share.clone()).unwrap();
        registry.disconnect(server.clone()).unwrap();
        assert!(registry.is_disconnected(&share.path));
        assert!(registry.is_disconnected(Path::new(r"\\NAS.INVALID\TV")));
        assert!(!registry.is_disconnected(Path::new("smb://other.invalid/TV")));
        registry.disconnect(share.clone()).unwrap();
        registry.reconnect(&server.path).unwrap();
        assert!(registry.is_disconnected(&share.path));
        assert!(!registry.is_disconnected(Path::new("smb://nas.invalid/Other")));
        registry.reconnect(&share.path).unwrap();
        assert!(!registry.is_disconnected(&share.path));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn logical_server_and_share_restore_without_a_mount_or_network_probe() {
        let server = Source::new(3, r"\\offline.invalid".into(), "NAS", None).unwrap();
        let share = Source::new(0, "smb://offline.invalid/TV".into(), "TV", None).unwrap();
        assert_eq!(server.path, Path::new("smb://offline.invalid/"));
        assert!(server.server_target().unwrap().share.is_none());
        assert_eq!(
            decode(&encode(&[server.clone(), share.clone()])),
            Some(vec![server, share])
        );
        assert!(Source::new(3, "smb://offline.invalid/TV".into(), "", None).is_err());
        assert!(Source::new(0, "smb://offline.invalid/".into(), "", None).is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn smb_share_spelling_preserves_bookmark_identity_and_user_exclusions() {
        let share = Source::new(0, "smb://nas.invalid/TV".into(), "TV", None).unwrap();
        let original = share.path.clone();
        let mut registry = Registry::default();
        registry.add(share.clone()).unwrap();
        assert_eq!(
            registry
                .find(Path::new("smb://NAS.invalid/tv"))
                .unwrap()
                .path,
            original
        );
        registry.disconnect(share).unwrap();
        assert!(registry.is_disconnected(Path::new("smb://nas.invalid/tv/Folder")));
        assert!(registry.is_disconnected(Path::new(r"\\NAS.INVALID\tv\Folder")));
        assert!(!registry.is_disconnected(Path::new("smb://nas.invalid/TV-other")));
        assert!(!registry.is_disconnected(Path::new("smb://other.invalid/tv")));
        // The SMB share name is case-insensitive; paths inside the share keep
        // native case semantics and exact component boundaries.
        assert!(covers(
            Path::new("smb://nas.invalid/TV/Folder"),
            Path::new("smb://nas.invalid/tv/Folder/child")
        ));
        assert!(!covers(
            Path::new("smb://nas.invalid/TV/Folder"),
            Path::new("smb://nas.invalid/tv/folder/child")
        ));
        registry.reconnect(&original).unwrap();
        assert!(!registry.is_disconnected(Path::new("smb://nas.invalid/tv")));
        assert_eq!(registry.entries[0].path, original);
        assert_eq!(decode(&encode(&registry.entries)), Some(registry.entries));
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an explicitly selected, already mounted native SMB share"]
    fn native_smb_preflight_preserves_logical_identity_and_revocation() {
        let uri = std::env::var_os("BUNNY_SMB_P1_URI")
            .expect("BUNNY_SMB_P1_URI must name the explicitly authorized share");
        let source = Source::new(0, uri.into(), "qualification", None).unwrap();
        let original = source.path.clone();
        let native = scan_root(&original).unwrap();
        assert!(native.is_absolute());
        assert_ne!(native, original);
        preflight(&source).unwrap();
        let mut registry = Registry::default();
        registry.add(source.clone()).unwrap();
        assert_eq!(registry.find(&native).unwrap().path, original);
        assert!(registry.scan_preflight(&native).unwrap()());
        let pending_disconnect = registry.scan_preflight(&native).unwrap();
        let pending_reconnect = registry.scan_preflight(&native).unwrap();
        registry.disconnect(source).unwrap();
        assert!(registry.is_disconnected(&native));
        assert!(!pending_disconnect());
        registry.reconnect(&original).unwrap();
        assert!(!pending_reconnect()); // Reconnection cannot rearm an old admission.
        assert!(registry.scan_preflight(&native).unwrap()());
        assert_eq!(registry.entries[0].path, original);
    }
    #[cfg(windows)]
    #[test]
    fn unc_can_be_added_without_server_and_native_utf16_survives_store() {
        use std::os::windows::ffi::OsStringExt;
        assert!(Source::new(0, PathBuf::from(r"\\offline.invalid\partage"), "NAS", None).is_ok());
        assert!(Source::new(0, PathBuf::from(r"C:\local"), "", None).is_err());
        assert!(Source::new(2, PathBuf::from(r"\\offline.invalid\partage"), "", None).is_err());
        assert!(Source::new(1, PathBuf::from(r"\\offline.invalid\partage"), "", None).is_err());
        let s = source(PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xd800])));
        assert_eq!(decode(&encode(std::slice::from_ref(&s))), Some(vec![s]));
    }
    #[cfg(windows)]
    #[test]
    fn ipc_session_is_not_a_registered_source_but_manual_admin_file_shares_are_allowed() {
        for path in [
            r"\\nas.invalid\IPC$",
            r"\\nas.invalid\iPc$\child",
            r"\\?\UNC\nas.invalid\ipc$",
        ] {
            assert!(Source::new(0, path.into(), "", None).is_err());
            let source = source(PathBuf::from(path));
            assert!(decode(&encode(std::slice::from_ref(&source))).is_none());
            assert!(Registry::default().add(source).is_err());
        }
        for path in [r"\\nas.invalid\C$", r"\\nas.invalid\Documents\IPC$"] {
            assert!(Source::new(0, path.into(), "", None).is_ok());
        }
    }
    #[cfg(unix)]
    #[test]
    fn native_non_utf8_paths_survive_store() {
        use std::os::unix::ffi::OsStringExt;
        let s = source(PathBuf::from(OsString::from_vec(
            b"/mnt/non-utf8-\xff".to_vec(),
        )));
        assert_eq!(decode(&encode(std::slice::from_ref(&s))), Some(vec![s]));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn mounted_network_sources_require_matching_native_protocol() {
        for (kind, filesystem) in [(0, "cifs"), (1, "nfs"), (1, "nfs4")] {
            let source = source(PathBuf::from("/mnt/explicit-native-share"));
            let mut d = VolumeDiscovery {
                volumes: vec![],
                enumeration_time: Default::default(),
                capacity_time: Default::default(),
                capacity_calls: 0,
            };
            merge(&mut d, &[source]);
            d.volumes[0].identity = VolumeIdentity::Linux {
                major: 0,
                minor: 100,
                filesystem: filesystem.into(),
                source: "native-test-service".into(),
            };
            let path = PathBuf::from("/mnt/explicit-native-share");
            assert!(Source::new(kind, path.clone(), "", Some(&d)).is_ok());
            assert!(Source::new(2, path.clone(), "", Some(&d)).is_err());
            assert!(
                Source::new(if kind == 0 { 1 } else { 0 }, path.clone(), "", Some(&d)).is_err()
            );
            assert!(Source::new(kind, path, "", None).is_err());
        }
    }
    #[test]
    fn writer_flushes_last_bookmark_atomically_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("configuration").join("sources");
        let last = source(temp.path().join("offline-last"));
        {
            let mut registry = Registry::open(path.clone());
            registry
                .add(source(temp.path().join("offline-first")))
                .unwrap();
            registry.add(last.clone()).unwrap();
        }
        assert_eq!(load(&path).unwrap().last(), Some(&last));
    }
}
