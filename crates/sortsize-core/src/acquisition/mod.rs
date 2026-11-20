//! V0.5 source boundary. Never canonicalizes, resolves owners or probes capacity.
//! Path-based checks are best effort, not an atomic filesystem snapshot.
use crate::LocalFileId;
#[doc(hidden)]
pub mod profile;
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
use linux as native;
#[cfg(windows)]
use windows as native;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceClass {
    Local,
    Remote,
    Unknown,
}
/// Optional metadata from the same authorized node observation. No name lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeProperties {
    pub allocated: crate::MetadataValue<u64>,
    pub accessed: crate::MetadataValue<SystemTime>,
    pub owner: crate::EntryOwner,
}
impl Default for NodeProperties {
    fn default() -> Self {
        Self {
            allocated: crate::MetadataValue::NotCollected,
            accessed: crate::MetadataValue::NotCollected,
            owner: crate::EntryOwner::not_collected(),
        }
    }
}
/// Metadata-only boundary evidence. Mount ID alone is never sufficient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub path: PathBuf,
    pub root: PathBuf,
    pub token: Vec<u8>,
    pub class: SourceClass,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub properties: NodeProperties,
    pub identity: Option<LocalFileId>,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub length: u64,
    pub directory: bool,
    pub regular: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attachment {
    Compatible,
    Different,
    Uncertain,
}
impl Stamp {
    /// Conservative cross-observation continuity. Without creation evidence an
    /// inode/file ID may have been reused, even if size and mtime are unchanged.
    pub fn attachment(&self, other: &Self) -> Attachment {
        if self.identity.is_some() && other.identity.is_some() && self.identity != other.identity {
            return Attachment::Different;
        }
        if self.directory != other.directory {
            return Attachment::Different;
        }
        match (self.identity, other.identity, self.created, other.created) {
            (Some(a), Some(b), Some(c), Some(d)) if a == b && c == d => Attachment::Compatible,
            (_, _, Some(c), Some(d)) if c != d => Attachment::Different,
            _ => Attachment::Uncertain,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub same_volume: bool,
    /// Explicit user action for this root only; never a persisted network grant.
    pub explicit_root: bool,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            same_volume: true,
            explicit_root: false,
        }
    }
}
#[derive(Debug)]
pub struct Guard {
    root: PathBuf,
    boundary: Boundary,
    root_stamp: Stamp,
    policy: Policy,
    internal: Option<Box<Guard>>,
    scan_observation: ScanObservation,
    #[cfg(windows)]
    ntfs_directory: bool,
}
/// Metadata acquisition method for live consultation only. Strict observations
/// remain mandatory for content access, verification and future operations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScanObservation {
    #[default]
    Strict,
    /// Use the authorized NTFS directory index when available. Values describe
    /// each listed occurrence, and can lag mutations through another hardlink.
    /// Listing rights do not imply permission to open or read a child file.
    /// Unsupported sources and requested owners retain strict observation.
    DirectoryListing,
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "source boundary not authorized or changed",
    )
}
fn validate_root_attachment(expected: &Stamp, current: &Stamp) -> io::Result<()> {
    // Within a live session absent creation evidence is acceptable only
    // with an available identical native ID. Never a durable identity proof.
    if current.identity.is_none()
        || current.identity != expected.identity
        || expected.attachment(current) == Attachment::Different
    {
        return Err(denied());
    }
    Ok(())
}
fn lexical(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(())
}
fn boundary<'a>(table: &'a [Boundary], path: &Path) -> io::Result<&'a Boundary> {
    table
        .iter()
        .filter(|m| path.starts_with(&m.path))
        .max_by_key(|m| m.path.components().count())
        .ok_or_else(denied)
}
/// Pure preflight, usable by instrumented adapters without touching a source.
fn authorize(
    table: &[Boundary],
    root: &Path,
    path: &Path,
    original: &Boundary,
    policy: Policy,
) -> io::Result<()> {
    lexical(path)?;
    if !path.starts_with(root) || boundary(table, root)? != original {
        return Err(denied());
    }
    // Check every crossed mount, including an automount hidden by a deeper mount.
    for mount in table
        .iter()
        .filter(|m| path.starts_with(&m.path) && m.path.starts_with(root))
    {
        if mount != original && (mount.class != SourceClass::Local || policy.same_volume) {
            return Err(denied());
        }
    }
    if table
        .iter()
        .any(|m| root.starts_with(&m.path) && m.class != SourceClass::Local)
        && !policy.explicit_root
    {
        return Err(denied());
    }
    let current = boundary(table, path)?;
    if current != original && (policy.same_volume || current.class != SourceClass::Local) {
        return Err(denied());
    }
    Ok(())
}
fn metadata_chain(path: &Path) -> io::Result<fs::Metadata> {
    let mut current = PathBuf::new();
    let mut last = None;
    for part in path.components() {
        current.push(part);
        // A Windows drive prefix alone is relative; wait for its root separator.
        if matches!(part, Component::Prefix(_)) || !current.is_absolute() {
            continue;
        }
        let m = fs::symlink_metadata(&current)?;
        #[cfg(windows)]
        let link = {
            use std::os::windows::fs::MetadataExt;
            m.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let link = m.file_type().is_symlink();
        if link {
            return Err(denied());
        }
        last = Some(m);
    }
    last.ok_or_else(denied)
}
/// Classify the native boundary without opening or enumerating the root.
/// Admission to traversal still requires a guarded open of the exact root.
#[cfg(any(target_os = "linux", windows))]
pub fn source_class(path: &Path) -> io::Result<SourceClass> {
    lexical(path)?;
    Ok(boundary(&native::boundaries(path)?, path)?.class)
}

/// Local application state only, including a not-yet-created suffix. Reject
/// remote/unknown mounts and existing links before creating any cache artifact.
/// This is never applied to a recorded source while consulting its cache.
pub fn local_state_location(path: &Path) -> io::Result<()> {
    lexical(path)?;
    let table = native::boundaries(path)?;
    let original = boundary(&table, path)?;
    authorize(&table, path, path, original, Policy::default())?;
    match metadata_chain(path) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(io::ErrorKind::NotADirectory.into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
/// Conservative containment for the store-exclusion guard. Windows ordinal
/// comparison is filesystem-free; it is not a persisted identity or collation.
pub fn contains_path(root: &Path, path: &Path) -> bool {
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Component, Prefix};
        let parts = |p: &Path| {
            p.components()
                .map(|c| match c {
                    Component::Prefix(prefix) => match prefix.kind() {
                        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => vec![u16::from(d), 58],
                        _ => prefix.as_os_str().encode_wide().collect(),
                    },
                    _ => c.as_os_str().encode_wide().collect(),
                })
                .collect::<Vec<Vec<u16>>>()
        };
        let a = parts(root);
        let b = parts(path);
        a.len() <= b.len()
            && a.iter().zip(&b).all(|(a, b)| {
                a.len() == b.len()
                    && unsafe {
                        windows_sys::Win32::Globalization::CompareStringOrdinal(
                            a.as_ptr(),
                            a.len() as i32,
                            b.as_ptr(),
                            b.len() as i32,
                            1,
                        )
                    } == windows_sys::Win32::Globalization::CSTR_EQUAL
            })
    }
}
impl Guard {
    pub fn open(root: &Path, policy: Policy) -> io::Result<Self> {
        lexical(root)?;
        let table = native::boundaries(root)?;
        let original = boundary(&table, root)?.clone();
        // Deny unknown/remote ancestors before stat can trigger an automount.
        if table
            .iter()
            .any(|m| root.starts_with(&m.path) && m.class != SourceClass::Local)
            && !policy.explicit_root
        {
            return Err(denied());
        }
        let m = metadata_chain(root)?;
        if !m.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        let root_stamp = native::stamp(root, &m)?;
        #[cfg(windows)]
        let ntfs_directory = original.class == SourceClass::Local
            && std::env::var_os("BUNNY_SCAN_P3_NTFS").is_none_or(|value| value == "1")
            && matches!(root_stamp.identity, Some(LocalFileId::Windows { .. }))
            && native::is_ntfs(root);
        let after = native::boundaries(root)?;
        if boundary(&after, root)? != &original {
            return Err(denied());
        }
        Ok(Self {
            root: root.into(),
            boundary: original,
            root_stamp,
            policy,
            internal: None,
            scan_observation: ScanObservation::Strict,
            #[cfg(windows)]
            ntfs_directory,
        })
    }
    /// Opt a scan into a declared observation method. This does not change
    /// `inspect`, `observe`, path authorization or content-verification behavior.
    pub fn with_scan_observation(mut self, mode: ScanObservation) -> Self {
        self.scan_observation = mode;
        self
    }

    pub fn scan_observation(&self) -> ScanObservation {
        self.scan_observation
    }
    /// Attach an application-owned directory whose ownership was established
    /// by the caller. Bind the exclusion to its native attachment and to its
    /// exact location within this source.
    pub fn with_internal_state(mut self, state: Guard) -> io::Result<Self> {
        if state.boundary.class != SourceClass::Local
            || state.root_stamp.attachment(&state.root_stamp) != Attachment::Compatible
        {
            return Err(denied());
        }
        if self.boundary.class == SourceClass::Local && state.has_native_ancestor(&self.root)? {
            return Err(denied());
        }
        if self.boundary.class == SourceClass::Local && self.has_native_ancestor(&state.root)? {
            if !self.policy.same_volume {
                return Err(denied());
            }
            self.internal = Some(Box::new(state));
        }
        Ok(self)
    }

    pub fn internal_state(&self) -> Option<&Guard> {
        self.internal.as_deref()
    }

    pub fn validate_internal_state(&self) -> io::Result<()> {
        if self.internal.is_none() {
            return Ok(());
        }
        profile::measure(profile::Stage::Workspace, || {
            self.validate_internal_state_impl()
        })
    }

    fn validate_internal_state_impl(&self) -> io::Result<()> {
        if let Some(state) = self.internal_state()
            && (boundary(&native::boundaries(&state.root)?, &state.root)? != &state.boundary
                || state.root_stamp.attachment(&state.current_root_stamp()?)
                    != Attachment::Compatible)
        {
            return Err(denied());
        }
        Ok(())
    }

    fn current_root_stamp(&self) -> io::Result<Stamp> {
        #[cfg(windows)]
        if self.ntfs_directory
            && let Some(result) = native::inspect_local(
                &self.root,
                &self.root,
                &self.root_stamp,
                false,
                false,
                false,
            )
        {
            return result.map(|(_, stamp)| stamp);
        }
        native::stamp(&self.root, &metadata_chain(&self.root)?)
    }

    /// The only exception to store_outside. An existing native ancestor must
    /// prove that this local working path belongs to the excluded directory.
    pub fn work_path_allowed(&self, path: &Path) -> io::Result<bool> {
        self.validate_internal_state()?;
        local_state_location(path)?;
        if self.store_outside(path)? {
            return Ok(true);
        }
        if let Some(state) = self.internal_state() {
            let table = native::boundaries(path)?;
            return Ok(boundary(&table, path)?.token == state.boundary.token
                && state.has_native_ancestor(path)?);
        }
        Ok(false)
    }

    /// Called before collecting a listed entry. The path comparison is only a
    /// prefilter; a same-name replacement cannot qualify as internal state.
    pub fn skip_internal_entry(&self, path: &Path) -> io::Result<bool> {
        let Some(state) = self.internal_state() else {
            return Ok(false);
        };
        if state.root.file_name() != path.file_name() {
            return Ok(false);
        }
        if !(contains_path(&state.root, path) && contains_path(path, &state.root)) {
            return Ok(false);
        }
        let stamp = self.inspect(path)?;
        if state.root_stamp.attachment(&stamp) == Attachment::Compatible {
            return Ok(true);
        }
        self.validate_internal_state()?;
        Ok(false)
    }

    pub fn is_internal_identity(&self, stamp: &Stamp) -> bool {
        self.internal_state()
            .is_some_and(|state| stamp.identity == state.root_stamp.identity)
    }

    /// Check whether this native directory is an ancestor of a guarded local path.
    pub fn has_native_ancestor(&self, path: &Path) -> io::Result<bool> {
        for part in path.ancestors().take_while(|part| part.is_absolute()) {
            match metadata_chain(part) {
                Ok(metadata) => {
                    let stamp = native::stamp(part, &metadata)?;
                    if self.root_stamp.attachment(&stamp) == Attachment::Compatible {
                        return Ok(true);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
    /// Compare only local state ancestors against the already captured source
    /// ID. No re-stat of the recorded source, canonicalization or source grant.
    pub fn store_outside(&self, path: &Path) -> io::Result<bool> {
        if contains_path(&self.root, path) {
            return Ok(false);
        }
        local_state_location(path)?;
        let cache = native::boundaries(path)?;
        if self.root_stamp.identity.is_none()
            && boundary(&cache, path)?.token == self.boundary.token
        {
            return Err(denied());
        }
        let mut next = Some(path);
        while let Some(p) = next {
            match fs::symlink_metadata(p) {
                Ok(m) => {
                    let stamp = native::stamp(p, &m)?;
                    if self.root_stamp.identity.is_some()
                        && stamp.identity == self.root_stamp.identity
                    {
                        return Ok(false);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            next = p.parent();
            #[cfg(windows)]
            if next.is_some_and(|p| !p.is_absolute()) {
                break;
            }
        }
        Ok(true)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn policy(&self) -> Policy {
        self.policy
    }
    pub fn evidence(&self) -> (&Boundary, &Stamp) {
        (&self.boundary, &self.root_stamp)
    }
    /// Revalidate only native mount/namespace evidence, without stat, following
    /// links, opening a source, or changing the caller's metadata semantics.
    pub fn authorize_path(&self, path: &Path) -> io::Result<()> {
        authorize(
            &native::boundaries(&self.root)?,
            &self.root,
            path,
            &self.boundary,
            self.policy,
        )
    }
    pub fn inspect(&self, path: &Path) -> io::Result<Stamp> {
        self.inspect_impl(path, false, false, false)
    }
    /// Fresh scope evidence for a scan's directory/EOF check, including the
    /// protected workspace. This is not a snapshot or permission to enumerate.
    /// Only local NTFS consultation may share a volume handle within this call;
    /// strict modes, aliases and other platforms retain independent checks.
    pub fn inspect_scan_directory(&self, path: &Path) -> io::Result<Stamp> {
        #[cfg(windows)]
        if self.ntfs_directory
            && self.scan_observation == ScanObservation::DirectoryListing
            && let Some(result) = native::prepare_directory_scope(self, path)
        {
            return result.map(|(stamp, _admission)| stamp);
        }
        self.validate_internal_state()?;
        self.inspect(path)
    }
    /// Explicit metadata acquisition under the same boundary checks as inspect.
    /// Owners are native IDs only; collection never resolves account names.
    pub fn observe(&self, path: &Path, owners: bool) -> io::Result<Stamp> {
        self.observe_options(path, true, owners)
    }
    pub fn observe_options(
        &self,
        path: &Path,
        allocation: bool,
        owners: bool,
    ) -> io::Result<Stamp> {
        self.inspect_impl(path, true, allocation, owners)
    }
    fn inspect_impl(
        &self,
        path: &Path,
        detailed: bool,
        allocation: bool,
        owners: bool,
    ) -> io::Result<Stamp> {
        let table = native::boundaries(&self.root)?;
        authorize(&table, &self.root, path, &self.boundary, self.policy)?;
        #[cfg(windows)]
        if self.ntfs_directory
            && let Some(result) = native::inspect_local(
                &self.root,
                path,
                &self.root_stamp,
                detailed,
                allocation,
                owners,
            )
        {
            let (_, stamp) = result?;
            authorize(
                &native::boundaries(&self.root)?,
                &self.root,
                path,
                &self.boundary,
                self.policy,
            )?;
            return Ok(stamp);
        }
        let root_meta = metadata_chain(&self.root)?;
        let root_now = native::stamp(&self.root, &root_meta)?;
        validate_root_attachment(&self.root_stamp, &root_now)?;
        let m = metadata_chain(path)?;
        let stamp = native::observe(path, &m, detailed, allocation, owners)?;
        authorize(
            &native::boundaries(&self.root)?,
            &self.root,
            path,
            &self.boundary,
            self.policy,
        )?;
        Ok(stamp)
    }
    fn check_directory(&self, path: &Path) -> io::Result<Stamp> {
        self.check_directory_with(path, |_| Ok(()))
    }
    fn check_directory_with(
        &self,
        path: &Path,
        before_open: impl FnOnce(&Stamp) -> io::Result<()>,
    ) -> io::Result<Stamp> {
        self.validate_internal_state()?;
        let stamp = self.inspect(path)?;
        self.check_directory_observation(path, stamp, before_open)
    }

    fn check_directory_observation(
        &self,
        path: &Path,
        stamp: Stamp,
        before_open: impl FnOnce(&Stamp) -> io::Result<()>,
    ) -> io::Result<Stamp> {
        if self.is_internal_identity(&stamp) {
            return Err(denied());
        }
        if let Some(state) = self.internal_state()
            && contains_path(&state.root, path)
            && state.has_native_ancestor(path)?
        {
            return Err(denied());
        }
        // Parent evidence must see a replacement by a non-directory as well:
        // the caller can then mark the original occurrence as detached before
        // any attempt to open a listing of the replacement.
        before_open(&stamp)?;
        if !stamp.directory {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(stamp)
    }

    pub fn read_dir(&self, path: &Path) -> io::Result<fs::ReadDir> {
        self.check_directory(path)?;
        fs::read_dir(path)
    }

    /// A bounded native listing for a single scheduler cursor. Validate the
    /// directory at EOF before certifying coverage, and at each quantum unless
    /// `ScanDirectory::retains_directory_handle` permits retaining the cursor.
    pub fn scan_directory(&self, path: &Path) -> io::Result<ScanDirectory> {
        self.scan_directory_with(path, |_| Ok(()))
            .map(|(_, reader)| reader)
    }

    /// Check the directory once and let the caller validate its current stamp
    /// (or cancellation) before opening a listing. The returned stamp is the
    /// pre-open snapshot, not a promise that the path cannot subsequently change.
    /// Native attachment checks before descent and at EOF remain required.
    /// A callback error prevents any listing from being opened.
    pub fn scan_directory_with(
        &self,
        path: &Path,
        before_open: impl FnOnce(&Stamp) -> io::Result<()>,
    ) -> io::Result<(Stamp, ScanDirectory)> {
        #[cfg(windows)]
        if self.ntfs_directory
            && self.scan_observation == ScanObservation::DirectoryListing
            && let Some(result) = native::prepare_directory_scope(self, path)
        {
            let (stamp, admission) = result?;
            let stamp = self.check_directory_observation(path, stamp, before_open)?;
            return self.open_native_scan_directory(path, stamp, Some(admission));
        }
        let stamp = self.check_directory_with(path, before_open)?;
        #[cfg(windows)]
        if self.ntfs_directory {
            return self.open_native_scan_directory(path, stamp, None);
        }
        #[cfg(target_os = "linux")]
        if let Some(reader) = native::Directory::open(self, path, &stamp)? {
            return Ok((
                stamp,
                ScanDirectory(DirectoryReader::Linux(Box::new(reader))),
            ));
        }
        Ok((
            stamp,
            ScanDirectory(DirectoryReader::Standard(fs::read_dir(path)?)),
        ))
    }

    #[cfg(windows)]
    fn open_native_scan_directory(
        &self,
        path: &Path,
        stamp: Stamp,
        admission: Option<native::DirectoryAdmission>,
    ) -> io::Result<(Stamp, ScanDirectory)> {
        match native::Directory::open(self, path, &stamp, admission) {
            Ok(reader) => {
                self.authorize_path(path)?;
                return Ok((
                    stamp,
                    ScanDirectory(DirectoryReader::Ntfs(Box::new(reader))),
                ));
            }
            // Only unavailable information classes may select the legacy
            // path. A denied/replaced directory must not be reopened by a
            // path-based fallback after its native attachment check failed.
            Err(error)
                if error.kind() == io::ErrorKind::Unsupported
                    || matches!(error.raw_os_error(), Some(1 | 50 | 87)) => {}
            Err(error) => return Err(error),
        }
        Ok((
            stamp,
            ScanDirectory(DirectoryReader::Standard(fs::read_dir(path)?)),
        ))
    }
}

pub struct ScanDirectory(DirectoryReader);

#[cfg_attr(
    windows,
    allow(
        clippy::large_enum_variant,
        reason = "Keep the standard ReadDir inline as before; only NTFS needs a new allocation"
    )
)]
enum DirectoryReader {
    Standard(fs::ReadDir),
    #[cfg(target_os = "linux")]
    Linux(Box<native::Directory>),
    #[cfg(windows)]
    Ntfs(Box<native::Directory>),
}

pub struct ScanEntry {
    path: PathBuf,
    name: std::ffi::OsString,
    #[cfg(windows)]
    native: Option<native::Entry>,
}

/// Path-free counters for the optional NTFS listing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NtfsListingStats {
    pub batch_calls: u64,
    pub relative_opens: u64,
    pub stale_rows: u64,
}

impl ScanEntry {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> &std::ffi::OsStr {
        &self.name
    }
}

impl ScanDirectory {
    /// A live-listing cursor pins the authorized directory itself. Callers may
    /// retain it across scheduling quanta without reopening its path. They must
    /// still check attachment before descending into any child and at EOF;
    /// errors invalidate coverage. This is never permission to read child data.
    pub fn retains_directory_handle(&self) -> bool {
        match &self.0 {
            DirectoryReader::Standard(_) => false,
            #[cfg(target_os = "linux")]
            DirectoryReader::Linux(_) => false,
            #[cfg(windows)]
            DirectoryReader::Ntfs(reader) => {
                reader.observation() == ScanObservation::DirectoryListing
            }
        }
    }

    pub fn is_ntfs_batch(&self) -> bool {
        match &self.0 {
            DirectoryReader::Standard(_) => false,
            #[cfg(target_os = "linux")]
            DirectoryReader::Linux(_) => false,
            #[cfg(windows)]
            DirectoryReader::Ntfs(_) => true,
        }
    }

    /// Whether this cursor has a native handle to validate.
    pub fn has_native_directory_handle(&self) -> bool {
        !matches!(self.0, DirectoryReader::Standard(_))
    }

    /// A handle-based cursor must still be attached to the same guarded path.
    pub fn validate(&self, stamp: &Stamp) -> io::Result<()> {
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = stamp;
        match &self.0 {
            DirectoryReader::Standard(_) => Ok(()),
            #[cfg(target_os = "linux")]
            DirectoryReader::Linux(reader) => reader.validate(stamp),
            #[cfg(windows)]
            DirectoryReader::Ntfs(reader) => reader.validate(stamp),
        }
    }

    pub fn ntfs_stats(&self) -> NtfsListingStats {
        match &self.0 {
            DirectoryReader::Standard(_) => NtfsListingStats::default(),
            #[cfg(target_os = "linux")]
            DirectoryReader::Linux(_) => NtfsListingStats::default(),
            #[cfg(windows)]
            DirectoryReader::Ntfs(reader) => reader.stats(),
        }
    }

    pub fn observe(
        &mut self,
        guard: &Guard,
        entry: &ScanEntry,
        allocation: bool,
        owners: bool,
    ) -> io::Result<Stamp> {
        #[cfg(target_os = "linux")]
        if let DirectoryReader::Linux(reader) = &self.0 {
            return reader.observe(guard, entry, allocation, owners);
        }
        #[cfg(windows)]
        if let DirectoryReader::Ntfs(reader) = &mut self.0
            && let Some(native) = &entry.native
            && let Some(stamp) = reader.observe(guard, entry.path(), native, allocation, owners)
        {
            return Ok(stamp);
        }
        if self.retains_directory_handle() {
            // Leaving a pinned cursor for a path-based fallback requires fresh
            // attachment evidence. Otherwise a substitution since the previous
            // quantum could make the fallback observe an unrelated directory
            // (including a moved application workspace).
            let parent = entry.path().parent().ok_or_else(denied)?;
            self.validate(&guard.check_directory(parent)?)?;
        }
        guard.observe_options(entry.path(), allocation, owners)
    }
}

impl Iterator for ScanDirectory {
    type Item = io::Result<ScanEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            DirectoryReader::Standard(read) => read.next().map(|entry| {
                let entry = entry?;
                Ok(ScanEntry {
                    path: entry.path(),
                    name: entry.file_name(),
                    #[cfg(windows)]
                    native: None,
                })
            }),
            #[cfg(target_os = "linux")]
            DirectoryReader::Linux(read) => read.next_entry(),
            #[cfg(windows)]
            DirectoryReader::Ntfs(read) => read.next_entry(),
        }
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
mod native {
    use super::*;
    pub fn boundaries(_: &Path) -> io::Result<Vec<Boundary>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn observe(path: &Path, m: &fs::Metadata, _: bool, _: bool, _: bool) -> io::Result<Stamp> {
        stamp(path, m)
    }
    pub fn stamp(_: &Path, _: &fs::Metadata) -> io::Result<Stamp> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
#[cfg(test)]
mod tests;
