//! Native lock aliases for moving a certified group without releasing its lease.
//! Anchors are only installed by explicit, validated repository retry. Opening
//! a store never creates an anchor and all aliases lock the same physical file.
use super::{Error, File, OpenOptions, Path, Result};
use std::path::PathBuf;

const PREFIX: &str = ".owner-anchor-v1-";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    volume: u64,
    file: [u8; 16],
}
impl Identity {
    fn name(self) -> String {
        let mut name = format!("{PREFIX}{:016x}-", self.volume);
        for byte in self.file {
            use std::fmt::Write;
            write!(&mut name, "{byte:02x}").expect("writing a String");
        }
        name.push_str(".lock");
        name
    }
}
#[cfg(unix)]
fn native_identity(file: &File) -> Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(Identity {
        volume: m.dev(),
        file: u128::from(m.ino()).to_be_bytes(),
    })
}
#[cfg(windows)]
fn native_identity(file: &File) -> Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: live handle, exact FileIdInfo output layout, initialized on success.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    Ok(Identity {
        volume: info.VolumeSerialNumber,
        file: info.FileId.Identifier,
    })
}
#[cfg(not(any(unix, windows)))]
fn native_identity(_: &File) -> Result<Identity> {
    Err(Error::Contract("native owner identity unavailable"))
}
#[cfg(test)]
std::thread_local! {
    static NATIVE_ID_UNAVAILABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn identity(file: &File) -> Result<Identity> {
    #[cfg(test)]
    if NATIVE_ID_UNAVAILABLE.with(std::cell::Cell::get) {
        return Err(Error::Contract("native owner identity unavailable (test)"));
    }
    native_identity(file)
}
#[cfg(unix)]
fn link_count(file: &File) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}
#[cfg(windows)]
fn link_count(file: &File) -> Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: live handle and exact initialized-on-success output structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(u64::from(unsafe { info.assume_init() }.nNumberOfLinks))
}
#[cfg(not(any(unix, windows)))]
fn link_count(_: &File) -> Result<u64> {
    Err(Error::Contract("native owner link count unavailable"))
}
// Before anchors are explicitly enabled, historical opens must not require
// FileIdInfo (128-bit IDs). Ordinary Windows locking retains its old filesystem
// availability and uses the legacy native identity only to fence name races.
#[cfg(windows)]
fn legacy_identity(file: &File) -> Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: live handle and exact initialized-on-success output structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok(Identity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: u128::from(index).to_be_bytes(),
    })
}
#[cfg(not(windows))]
fn legacy_identity(file: &File) -> Result<Identity> {
    native_identity(file)
}
// Anchors live only in managed repository parents. Their bounded metadata
// namespace is already limited to its existing directory budget; never scan arbitrary legacy
// state parents merely to open a historical inventory.
fn anchors_enabled(group: &Path) -> Result<bool> {
    let parent = group
        .parent()
        .ok_or(Error::Contract("owner group parent unavailable"))?;
    match std::fs::symlink_metadata(parent.join("IDENTITY.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
        Ok(m) if m.is_file() && !m.is_symlink() => (),
        Ok(_) => return Err(Error::Contract("owner anchor parent metadata preserved")),
    }
    for (index, entry) in std::fs::read_dir(parent)?.enumerate() {
        if index >= super::generation_a::repository::DIRECTORY_LIMIT {
            return Err(Error::Contract("owner anchor parent metadata budget"));
        }
        if is_anchor_name(&entry?.file_name()) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn acquire_legacy(owner: &Path, shared: bool) -> Result<File> {
    let before = legacy_identity(&open_existing(owner)?)?;
    let file = open_existing(owner)?;
    if legacy_identity(&file)? != before {
        return Err(Error::Contract("opened owner identity changed"));
    }
    native_lock(&file, shared)?;
    if legacy_identity(&open_existing(owner)?)? != before {
        return Err(Error::Contract("owner identity changed during locking"));
    }
    Ok(file)
}
fn native_lock(file: &File, shared: bool) -> Result<()> {
    if shared {
        file.try_lock_shared()
            .map_err(|_| Error::Contract("inventory exclusive writer or retirement is active"))?;
    } else {
        file.try_lock()
            .map_err(|_| Error::Contract("inventory already owned or native lock unavailable"))?;
    }
    Ok(())
}
fn regular_zero(path: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_file() || m.is_symlink() || m.len() != 0 {
        return Err(Error::Contract("owner or anchor metadata preserved"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(Error::Contract("owner reparse point preserved"));
        }
    }
    Ok(())
}
fn open_existing(path: &Path) -> Result<File> {
    regular_zero(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() != 0 {
        return Err(Error::Contract("opened owner metadata preserved"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(Error::Contract("opened owner reparse point preserved"));
        }
    }
    regular_zero(path)?;
    Ok(file)
}
fn identity_at(path: &Path) -> Result<Identity> {
    identity(&open_existing(path)?)
}
fn anchor_path(group: &Path, id: Identity) -> Result<PathBuf> {
    Ok(group
        .parent()
        .ok_or(Error::Contract("owner group parent unavailable"))?
        .join(id.name()))
}
fn require_same(path: &Path, id: Identity) -> Result<()> {
    if identity_at(path)? != id {
        return Err(Error::Contract("owner or anchor identity changed"));
    }
    Ok(())
}
/// Check a root-level anchor as metadata, without granting authority over any
/// group. Orphan anchors remain preserved until explicit repository cleanup.
pub(super) fn validate_anchor(path: &Path) -> Result<()> {
    let id = identity_at(path)?;
    if path.file_name() != Some(std::ffi::OsStr::new(&id.name())) {
        return Err(Error::Contract("foreign owner anchor preserved"));
    }
    Ok(())
}
fn anchor_has_group(path: &Path) -> Result<bool> {
    let id = identity_at(path)?;
    let parent = path
        .parent()
        .ok_or(Error::Contract("owner anchor parent unavailable"))?;
    for (index, entry) in std::fs::read_dir(parent)?.enumerate() {
        if index >= super::generation_a::repository::DIRECTORY_LIMIT {
            return Err(Error::Contract("owner anchor parent metadata budget"));
        }
        let entry = entry?;
        let m = std::fs::symlink_metadata(entry.path())?;
        if m.is_dir() && !m.is_symlink() {
            let owner = entry.path().join("owner.lock");
            match std::fs::symlink_metadata(&owner) {
                Ok(_) if identity_at(&owner)? == id => return Ok(true),
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(false)
}
/// Orphans are readable/accounted metadata, never evidence authorizing unlink.
/// Explicit Forget preflights them before deleting any certified group.
pub(super) fn preflight_removal(parent: &Path) -> Result<()> {
    for (index, entry) in std::fs::read_dir(parent)?.enumerate() {
        if index >= super::generation_a::repository::DIRECTORY_LIMIT {
            return Err(Error::Contract("owner anchor parent metadata budget"));
        }
        let entry = entry?;
        if is_anchor_name(&entry.file_name()) {
            validate_anchor(&entry.path())?;
            if !anchor_has_group(&entry.path())? {
                return Err(Error::Contract(
                    "unbound owner anchor preserved; explicit recovery required",
                ));
            }
        }
    }
    Ok(())
}
pub(super) fn is_anchor_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(key) = name
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(".lock"))
    else {
        return false;
    };
    key.len() == 49
        && key.as_bytes()[16] == b'-'
        && key
            .bytes()
            .enumerate()
            .all(|(i, c)| i == 16 || c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
/// Existing aliases are verified both before and after native locking. An old
/// opener using owner.lock still conflicts because the anchor is a hard link.
pub(super) fn acquire(group: &Path, shared: bool, create: bool) -> Result<File> {
    let owner = group.join("owner.lock");
    if create {
        match std::fs::symlink_metadata(&owner) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&owner)?;
                drop(file);
            }
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
    }
    let owner_probe = open_existing(&owner)?;
    let links = link_count(&owner_probe)?;
    // This probe is optional on historical filesystems. A one-link owner has
    // no genuine anchor; if native IDs are available, still reject a forged
    // anchor at its exact name rather than silently selecting that file.
    let expected = match identity(&owner_probe) {
        Ok(id) => id,
        Err(_) if links <= 1 => return acquire_legacy(&owner, shared),
        Err(_) if !anchors_enabled(group)? => return acquire_legacy(&owner, shared),
        Err(e) => return Err(e),
    };
    drop(owner_probe);
    let anchor = anchor_path(group, expected)?;
    let selected = match std::fs::symlink_metadata(&anchor) {
        Ok(_) => {
            require_same(&anchor, expected)?;
            &anchor
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return acquire_legacy(&owner, shared);
        }
        Err(e) => return Err(e.into()),
    };
    let file = open_existing(selected)?;
    if identity(&file)? != expected {
        return Err(Error::Contract("opened owner alias changed"));
    }
    native_lock(&file, shared)?;
    // A substituted name cannot confer a lease on the replacement inode.
    require_same(&owner, expected)?;
    require_same(selected, expected)?;
    Ok(file)
}
/// Caller holds an exclusively locked, FULL-certified group. The temporary
/// handoff below must never reuse that certificate after releasing this lease.
#[cfg(any(windows, test))]
pub(super) fn install(group: &Path, lease: &File) -> Result<()> {
    let id = identity(lease)?;
    let owner = group.join("owner.lock");
    require_same(&owner, id)?;
    let anchor = anchor_path(group, id)?;
    match std::fs::hard_link(&owner, &anchor) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    require_same(&owner, id)?;
    require_same(&anchor, id)?;
    open_existing(&anchor)?.sync_all()?;
    super::lifecycle::sync_directory(anchor.parent().unwrap())
}
#[cfg(any(windows, test))]
pub(super) fn verify_installed(group: &Path, lease: &File) -> Result<()> {
    let id = identity(lease)?;
    require_same(&group.join("owner.lock"), id)?;
    require_same(&anchor_path(group, id)?, id)
}
/// Only after the explicitly retired group is gone, remove its exact physical
/// anchor under the still-held exclusive lease. No directory scan/implicit GC.
pub(super) fn remove_after_group(group: &Path, lease: &File) -> Result<()> {
    match std::fs::symlink_metadata(group) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
        Ok(_) => {
            return Err(Error::Contract(
                "owner anchor still belongs to an existing group",
            ));
        }
    }
    if link_count(lease)? == 0 || !anchors_enabled(group)? {
        return Ok(());
    }
    let id = identity(lease)?;
    let anchor = anchor_path(group, id)?;
    match std::fs::symlink_metadata(&anchor) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(_) => (),
    }
    require_same(&anchor, id)?;
    std::fs::remove_file(&anchor)?;
    super::lifecycle::sync_directory(anchor.parent().unwrap())
}

#[cfg(test)]
mod tests;
