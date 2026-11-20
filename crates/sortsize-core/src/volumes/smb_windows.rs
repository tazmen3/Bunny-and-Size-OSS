//! Explicit Windows SMB operations. No file enumeration, content read or scan.
//! Credentials belong to the native provider; this module never receives them.
use std::{
    ffi::{OsStr, OsString},
    fmt,
    mem::{align_of, size_of},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};
use windows_sys::Win32::{
    Foundation::*,
    NetworkManagement::{
        NetManagement::{NetApiBufferFree, NetApiBufferSize},
        WNet::{
            CONNECT_INTERACTIVE, NETRESOURCEW, RESOURCETYPE_ANY, RESOURCETYPE_DISK,
            WNetAddConnection3W, WNetGetConnectionW,
        },
    },
    Storage::FileSystem::{NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_MASK, STYPE_SPECIAL},
};

pub const MAX_SHARES: usize = 256;
const PAGE_BYTES: u32 = 64 * 1024;
const MAX_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_PAGES: usize = 32;
const MAX_NAME_UNITS: usize = 1024;
const MAX_INPUT_UNITS: usize = 32_767;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    SharesFound,
    ServerInaccessible,
    NoShares,
    AuthenticationRequired,
    CredentialsRejected,
    ShareMissing,
    AccessDenied,
    Connected,
    Cancelled,
    SessionConflict,
    InvalidInput,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmbError {
    pub status: Status,
    pub os_code: u32,
}
impl fmt::Display for SmbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} (Windows {})", self.status, self.os_code)
    }
}
impl std::error::Error for SmbError {}
fn invalid() -> SmbError {
    SmbError {
        status: Status::InvalidInput,
        os_code: ERROR_INVALID_PARAMETER,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub server: OsString,
    /// A complete UNC file path; absent for a server address, never a scan root.
    pub share: Option<PathBuf>,
}
impl Target {
    pub fn server_unc(&self) -> PathBuf {
        let mut unc = OsString::from(r"\\");
        unc.push(&self.server);
        PathBuf::from(unc)
    }
}

/// Pure syntax normalization. No DNS, authentication, server access or scan.
pub fn parse_target(input: &str) -> Result<Target, SmbError> {
    if input.is_empty()
        || input.chars().any(char::is_control)
        || input.encode_utf16().count() > MAX_INPUT_UNITS
    {
        return Err(invalid());
    }
    let value = input
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| input.strip_prefix(r"\\"))
        .unwrap_or(input);
    let mut parts = value.split('\\');
    let server = parts.next().ok_or_else(invalid)?;
    if server.is_empty()
        || server == "."
        || server == ".."
        || server.len() > 255
        || !server
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(invalid());
    }
    let mut tail: Vec<_> = parts.collect();
    while tail.last() == Some(&"") {
        tail.pop();
    }
    if tail.iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || part
                .chars()
                .any(|c| matches!(c, '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    }) {
        return Err(invalid());
    }
    let target = Target {
        server: server.into(),
        share: None,
    };
    if tail.is_empty() {
        return Ok(target);
    }
    if input.starts_with(r"\\") {
        // Keep explicit native UNC spelling, including the extended prefix,
        // trailing separators and native long-path semantics. The parsed server
        // is used only for server operations, never as an identity substitute.
        return Ok(Target {
            share: Some(PathBuf::from(input)),
            ..target
        });
    }
    let mut path = target.server_unc().into_os_string();
    for part in tail {
        path.push("\\");
        path.push(part);
    }
    Ok(Target {
        share: Some(path.into()),
        ..target
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub name: OsString,
    pub path: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareList {
    pub shares: Vec<Share>,
    /// API pagination/result budgets reached; the displayed list is partial.
    pub limited: bool,
}
impl ShareList {
    pub fn status(&self) -> Status {
        if self.shares.is_empty() {
            Status::NoShares
        } else {
            Status::SharesFound
        }
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Enumeration,
    Connection,
    Probe,
}
fn error(code: u32, operation: Operation) -> SmbError {
    let status = match code {
        ERROR_CANCELLED | ERROR_OPERATION_ABORTED => Status::Cancelled,
        ERROR_SESSION_CREDENTIAL_CONFLICT => Status::SessionConflict,
        ERROR_NOT_AUTHENTICATED | ERROR_NO_SUCH_LOGON_SESSION => Status::AuthenticationRequired,
        ERROR_INVALID_PASSWORD
        | ERROR_LOGON_FAILURE
        | ERROR_BAD_USERNAME
        | ERROR_ACCOUNT_LOCKED_OUT
        | ERROR_PASSWORD_EXPIRED
        | ERROR_PASSWORD_MUST_CHANGE
            if !matches!(operation, Operation::Connection) =>
        {
            Status::AuthenticationRequired
        }
        ERROR_INVALID_PASSWORD
        | ERROR_LOGON_FAILURE
        | ERROR_BAD_USERNAME
        | ERROR_ACCOUNT_LOCKED_OUT
        | ERROR_PASSWORD_EXPIRED
        | ERROR_PASSWORD_MUST_CHANGE => Status::CredentialsRejected,
        ERROR_ACCESS_DENIED | ERROR_NETWORK_ACCESS_DENIED | ERROR_LOGON_TYPE_NOT_GRANTED => {
            Status::AccessDenied
        }
        ERROR_BAD_NET_NAME if matches!(operation, Operation::Connection) => Status::ShareMissing,
        ERROR_BAD_NET_NAME
        | ERROR_BAD_NETPATH
        | ERROR_NETWORK_UNREACHABLE
        | ERROR_HOST_UNREACHABLE
        | ERROR_NETNAME_DELETED
        | ERROR_SEM_TIMEOUT
        | ERROR_TIMEOUT
        | ERROR_NO_LOGON_SERVERS
        | ERROR_NO_NETWORK
        | ERROR_NO_NET_OR_BAD_PATH
        | ERROR_NOT_CONNECTED
        | ERROR_CONNECTION_UNAVAIL => Status::ServerInaccessible,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_DIRECTORY => Status::ShareMissing,
        ERROR_INVALID_PARAMETER | ERROR_INVALID_NAME => Status::InvalidInput,
        _ => Status::Failed,
    };
    SmbError {
        status,
        os_code: code,
    }
}

fn wide(value: &OsStr) -> Result<Vec<u16>, SmbError> {
    let mut units: Vec<_> = value.encode_wide().collect();
    if units.is_empty() || units.len() > MAX_INPUT_UNITS || units.contains(&0) {
        return Err(invalid());
    }
    units.push(0);
    Ok(units)
}
struct NetBuffer(*mut u8);
impl Drop for NetBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this allocation is returned by NetShareEnum, owned once.
            unsafe { NetApiBufferFree(self.0.cast()) };
        }
    }
}
fn visible_file_share(name: &[u16], share_type: u32) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_UNITS
        && share_type & STYPE_MASK == STYPE_DISKTREE
        && share_type & STYPE_SPECIAL == 0
        && name.last() != Some(&(b'$' as u16))
        && name != [b'.' as u16]
        && name != [b'.' as u16, b'.' as u16]
        && !name
            .iter()
            .any(|c| *c < 32 || *c == 127 || b"\\/:*?\"<>|".iter().any(|b| *c == u16::from(*b)))
}
/// Read only a terminated UTF-16 name inside the owned native page allocation.
fn page_name(buffer: &NetBuffer, bytes: usize, name: *const u16) -> Option<Vec<u16>> {
    let base = buffer.0 as usize;
    let start = name as usize;
    let end = base.checked_add(bytes)?;
    if start < base || start >= end || !start.is_multiple_of(align_of::<u16>()) {
        return None;
    }
    let length = ((end - start) / size_of::<u16>()).min(MAX_NAME_UNITS + 1);
    // SAFETY: the address and length were checked inside this live allocation.
    let units = unsafe { std::slice::from_raw_parts(name, length) };
    let end = units.iter().position(|unit| *unit == 0)?;
    Some(units[..end].to_vec())
}

/// Explicit enumeration of the one specified server, level 1 only. This does
/// not test access to each share or inspect any directory/file within shares.
pub fn enumerate_shares(target: &Target) -> Result<ShareList, SmbError> {
    let server = wide(target.server_unc().as_os_str())?;
    let mut list = ShareList {
        shares: Vec::new(),
        limited: false,
    };
    let mut resume = 0;
    for _ in 0..MAX_PAGES {
        let before = resume;
        let mut raw = ptr::null_mut();
        let (mut count, mut total) = (0, 0);
        // SAFETY: one terminated server and writable outputs; level fixes type.
        let code = unsafe {
            NetShareEnum(
                server.as_ptr(),
                1,
                &mut raw,
                PAGE_BYTES,
                &mut count,
                &mut total,
                &mut resume,
            )
        };
        let buffer = NetBuffer(raw);
        if code != 0 && code != ERROR_MORE_DATA {
            return Err(error(code, Operation::Enumeration));
        }
        if count != 0 {
            if raw.is_null() {
                return Err(error(ERROR_INVALID_DATA, Operation::Enumeration));
            }
            let mut bytes = 0;
            // SAFETY: buffer is still the live NetShareEnum-owned allocation.
            let size_code = unsafe { NetApiBufferSize(raw.cast(), &mut bytes) };
            if size_code != 0
                || bytes as usize > MAX_BUFFER_BYTES
                || count as usize > bytes as usize / size_of::<SHARE_INFO_1>()
            {
                return Err(error(ERROR_INVALID_DATA, Operation::Enumeration));
            }
            // SAFETY: count fits the checked allocation; API supplies alignment.
            let entries =
                unsafe { std::slice::from_raw_parts(raw.cast::<SHARE_INFO_1>(), count as usize) };
            for entry in entries {
                let Some(name) = page_name(&buffer, bytes as usize, entry.shi1_netname) else {
                    return Err(error(ERROR_INVALID_DATA, Operation::Enumeration));
                };
                if !visible_file_share(&name, entry.shi1_type) {
                    continue;
                }
                let name = OsString::from_wide(&name);
                if list.shares.iter().any(|share| share.name == name) {
                    continue;
                }
                if list.shares.len() == MAX_SHARES {
                    list.limited = true;
                    return Ok(list);
                }
                let mut path = target.server_unc().into_os_string();
                path.push("\\");
                path.push(&name);
                list.shares.push(Share {
                    name,
                    path: path.into(),
                });
            }
        }
        if code == 0 {
            return Ok(list);
        }
        if resume == before || count == 0 {
            list.limited = true;
            return Ok(list);
        }
    }
    list.limited = true;
    Ok(list)
}

fn connection_root(path: &Path) -> Result<PathBuf, SmbError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid());
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(invalid());
    };
    let (server, share) = match prefix.kind() {
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => (server, share),
        _ => return Err(invalid()),
    };
    let mut root = OsString::from(r"\\");
    root.push(server);
    root.push("\\");
    root.push(share);
    wide(&root)?;
    Ok(root.into())
}

fn connect_with(
    path: &Path,
    owner: usize,
    resource_type: u32,
    flags: u32,
    provider: impl FnOnce(HWND, &NETRESOURCEW, *const u16, *const u16, u32) -> u32,
) -> Result<(), SmbError> {
    let mut remote = wide(path.as_os_str())?;
    let resource = NETRESOURCEW {
        dwType: resource_type,
        lpRemoteName: remote.as_mut_ptr(),
        ..Default::default()
    };
    let code = provider(owner as HWND, &resource, ptr::null(), ptr::null(), flags);
    if code == 0 {
        Ok(())
    } else {
        Err(error(code, Operation::Connection))
    }
}

fn connect_resource(path: &Path, owner: usize, resource_type: u32) -> Result<(), SmbError> {
    connect_resource_flags(path, owner, resource_type, CONNECT_INTERACTIVE)
}

fn connect_resource_flags(
    path: &Path,
    owner: usize,
    resource_type: u32,
    flags: u32,
) -> Result<(), SmbError> {
    connect_with(
        path,
        owner,
        resource_type,
        flags,
        |owner, resource, password, username, flags| {
            // SAFETY: resource/remote remain live for the call; owner is a borrowed
            // HWND captured on the UI thread. Null credentials use Windows identity.
            unsafe { WNetAddConnection3W(owner, resource, password, username, flags) }
        },
    )
}

/// Explicit native login for a precise share, using its native provider only.
pub fn connect_share(path: &Path, owner: usize) -> Result<(), SmbError> {
    connect_resource(&connection_root(path)?, owner, RESOURCETYPE_DISK)
}

/// Server-only login uses the standard IPC session resource internally. IPC$
/// is never returned as a selectable share, stored source or acquisition root.
/// Listing again remains a separate explicit user command.
pub fn connect_target(target: &Target, owner: usize) -> Result<(), SmbError> {
    match &target.share {
        Some(path) => connect_share(path, owner),
        None => {
            let mut ipc = target.server_unc().into_os_string();
            ipc.push(r"\IPC$");
            connect_resource(Path::new(&ipc), owner, RESOURCETYPE_ANY)
        }
    }
}

fn authentication_may_help(error: SmbError) -> bool {
    matches!(
        error.status,
        Status::AuthenticationRequired | Status::CredentialsRejected | Status::AccessDenied
    ) || matches!(
        error.os_code,
        ERROR_CONNECTION_UNAVAIL | ERROR_NOT_CONNECTED
    )
}

/// Query one existing drive mapping, with a fixed allocation and retry budget.
/// An unavailable/unknown mapping is never guessed, changed or disconnected.
fn mapped_remote_with(
    drive: u8,
    mut query: impl FnMut(&[u16; 3], &mut [u16], &mut u32) -> u32,
) -> Result<PathBuf, SmbError> {
    let local = [u16::from(drive), b':' as u16, 0];
    let mut buffer = vec![0; 512];
    for _ in 0..2 {
        let mut length = buffer.len() as u32;
        let code = query(&local, &mut buffer, &mut length);
        if code == 0 {
            let end = buffer
                .iter()
                .position(|unit| *unit == 0)
                .filter(|end| *end > 0)
                .ok_or_else(|| error(ERROR_INVALID_DATA, Operation::Probe))?;
            let path = PathBuf::from(OsString::from_wide(&buffer[..end]));
            let root = connection_root(&path)?;
            // A drive must resolve to precisely a share, not a nested path.
            if path.components().ne(root.components()) {
                return Err(invalid());
            }
            return Ok(root);
        }
        if code != ERROR_MORE_DATA || length == 0 || length as usize > MAX_INPUT_UNITS + 1 {
            return Err(error(code, Operation::Probe));
        }
        buffer.resize(length as usize, 0);
    }
    Err(error(ERROR_MORE_DATA, Operation::Probe))
}

fn mapped_remote(drive: u8) -> Result<PathBuf, SmbError> {
    mapped_remote_with(drive, |local, buffer, length| {
        // SAFETY: a terminated drive name and bounded writable output buffer.
        unsafe { WNetGetConnectionW(local.as_ptr(), buffer.as_mut_ptr(), length) }
    })
}

fn probe_directory(path: &Path) -> Result<(), SmbError> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    // Reuse the native read-attributes guard: no listing or file contents, and
    // no following reparse points. The explicit root is not a scan permission.
    let io_error = |failure: std::io::Error| {
        error(
            // A lexical/reparse policy refusal is not missing credentials.
            failure
                .raw_os_error()
                .map_or(ERROR_INVALID_DATA, |code| code as u32),
            Operation::Probe,
        )
    };
    let _guard = crate::acquisition::Guard::open(
        path,
        crate::acquisition::Policy {
            explicit_root: true,
            ..Default::default()
        },
    )
    .map_err(io_error)?;
    // Request the right to list this exact directory without issuing any
    // enumeration/read. Read-attributes alone would hide listing ACL denials.
    let directory = std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(io_error)?;
    let metadata = directory.metadata().map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(error(ERROR_INVALID_DATA, Operation::Probe));
    }
    Ok(())
}

fn reconnect_share_with(
    path: &Path,
    mut probe: impl FnMut(&Path) -> Result<(), SmbError>,
    connect: impl FnOnce() -> Result<(), SmbError>,
) -> Result<(), SmbError> {
    match probe(path) {
        Ok(()) => return Ok(()),
        Err(failure) if authentication_may_help(failure) => {}
        Err(failure) => return Err(failure),
    }
    connect()?;
    // A successful provider login does not prove that this folder is readable.
    probe(path)
}

fn reconnect_server_with(
    mut connect: impl FnMut(u32) -> Result<(), SmbError>,
) -> Result<(), SmbError> {
    match connect(0) {
        Ok(()) => Ok(()),
        Err(failure) if authentication_may_help(failure) => connect(CONNECT_INTERACTIVE),
        Err(failure) => Err(failure),
    }
}

/// Explicit reconnection only; call from a worker after the user's command.
/// No discovery, directory listing, acquisition, credential storage, drive
/// redirection, profile mutation or disconnection is performed. Unknown mapped
/// targets fail closed and can be restored manually in Windows Explorer.
/// Server keys use IPC$ only and are never opened as filesystem directories.
pub fn reconnect(path: &Path, is_server: bool, owner: usize) -> Result<(), SmbError> {
    if is_server {
        let target = parse_target(path.to_str().ok_or_else(invalid)?)?;
        if target.share.is_some() {
            return Err(invalid());
        }
        let mut ipc = target.server_unc().into_os_string();
        ipc.push(r"\IPC$");
        return reconnect_server_with(|flags| {
            connect_resource_flags(Path::new(&ipc), owner, RESOURCETYPE_ANY, flags)
        });
    }
    // Validate before the first access, even for an already accessible path.
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid());
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(invalid());
    };
    let (remote, mapped) = match prefix.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => (connection_root(path)?, None),
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            if super::namespace_network_protocol(path).ok() != Some(super::NetworkProtocol::Smb) {
                return Err(invalid());
            }
            (mapped_remote(drive)?, Some(drive))
        }
        _ => return Err(invalid()),
    };
    let revalidate_mapping = || {
        if let Some(drive) = mapped
            && mapped_remote(drive)? != remote
        {
            return Err(error(ERROR_INVALID_DATA, Operation::Probe));
        }
        Ok(())
    };
    reconnect_share_with(
        path,
        |path| {
            revalidate_mapping()?;
            probe_directory(path)
        },
        || {
            // Revalidate before native authentication and again before the
            // final probe, without changing or guessing the mapped identity.
            revalidate_mapping()?;
            connect_resource(&remote, owner, RESOURCETYPE_DISK)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_reuses_accessible_session_without_native_login() {
        let mut probes = 0;
        reconnect_share_with(
            Path::new(r"\\nas\share"),
            |_| {
                probes += 1;
                Ok(())
            },
            || panic!("valid session must not prompt"),
        )
        .unwrap();
        assert_eq!(probes, 1);
    }
    #[test]
    fn reconnect_authenticates_once_then_revalidates_the_requested_directory() {
        let mut probes = 0;
        let mut connections = 0;
        reconnect_share_with(
            Path::new(r"\\nas\share\folder"),
            |path| {
                assert_eq!(path, Path::new(r"\\nas\share\folder"));
                probes += 1;
                if probes == 1 {
                    Err(error(ERROR_LOGON_FAILURE, Operation::Probe))
                } else {
                    Ok(())
                }
            },
            || {
                connections += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((probes, connections), (2, 1));
    }
    #[test]
    fn reconnect_cancellation_and_session_conflict_never_retry_or_probe_again() {
        for code in [
            ERROR_CANCELLED,
            ERROR_SESSION_CREDENTIAL_CONFLICT,
            ERROR_LOGON_FAILURE,
        ] {
            let mut probes = 0;
            let failure = reconnect_share_with(
                Path::new(r"\\nas\share"),
                |_| {
                    probes += 1;
                    Err(error(ERROR_NOT_AUTHENTICATED, Operation::Probe))
                },
                || Err(error(code, Operation::Connection)),
            )
            .unwrap_err();
            assert_eq!(failure.os_code, code);
            assert_eq!(probes, 1);
        }
    }
    #[test]
    fn reconnect_does_not_prompt_for_an_unreachable_or_missing_share() {
        for code in [ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME, ERROR_PATH_NOT_FOUND] {
            let failure = reconnect_share_with(
                Path::new(r"\\nas\share"),
                |_| Err(error(code, Operation::Probe)),
                || panic!("unavailable resource must not prompt"),
            )
            .unwrap_err();
            assert_eq!(failure.os_code, code);
        }
    }
    #[test]
    fn successful_login_does_not_hide_remaining_access_denial() {
        let mut probes = 0;
        let failure = reconnect_share_with(
            Path::new(r"\\nas\share"),
            |_| {
                probes += 1;
                Err(error(ERROR_ACCESS_DENIED, Operation::Probe))
            },
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(failure.status, Status::AccessDenied);
        assert_eq!(probes, 2);
    }
    #[test]
    fn server_reconnect_uses_noninteractive_session_then_optional_native_ui_only() {
        let mut flags_seen = Vec::new();
        reconnect_server_with(|flags| {
            flags_seen.push(flags);
            if flags == 0 {
                Err(error(ERROR_LOGON_FAILURE, Operation::Connection))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(flags_seen, [0, CONNECT_INTERACTIVE]);
        flags_seen.clear();
        reconnect_server_with(|flags| {
            flags_seen.push(flags);
            Ok(())
        })
        .unwrap();
        assert_eq!(flags_seen, [0]);
    }
    #[test]
    fn mapping_resolution_is_bounded_and_rejects_unavailable_or_ambiguous_targets() {
        let expected: Vec<u16> = r"\\nas\share".encode_utf16().chain([0]).collect();
        let result = mapped_remote_with(b'Z', |local, buffer, _| {
            assert_eq!(*local, [b'Z' as u16, b':' as u16, 0]);
            buffer[..expected.len()].copy_from_slice(&expected);
            0
        })
        .unwrap();
        assert_eq!(result, Path::new(r"\\nas\share"));
        for code in [ERROR_NOT_CONNECTED, ERROR_CONNECTION_UNAVAIL] {
            assert_eq!(
                mapped_remote_with(b'Z', |_, _, _| code)
                    .unwrap_err()
                    .os_code,
                code
            );
        }
        let mut calls = 0;
        assert!(
            mapped_remote_with(b'Z', |_, _, length| {
                calls += 1;
                *length = MAX_INPUT_UNITS as u32 + 2;
                ERROR_MORE_DATA
            })
            .is_err()
        );
        assert_eq!(calls, 1);
        assert!(
            mapped_remote_with(b'Z', |_, buffer, _| {
                let bad: Vec<_> = r"\\nas\share\nested".encode_utf16().chain([0]).collect();
                buffer[..bad.len()].copy_from_slice(&bad);
                0
            })
            .is_err()
        );
    }
    #[test]
    fn server_addresses_are_valid_but_never_share_or_scan_roots() {
        for address in [
            "192.168.1.144",
            "nas.local",
            "MY_NAS",
            r"\\192.168.1.144",
            r"\\nas.local\",
        ] {
            let target = parse_target(address).unwrap();
            assert!(target.share.is_none());
            assert!(connection_root(&target.server_unc()).is_err());
            // Assert lexical rejection before exercising Guard, so this test
            // cannot issue network I/O even if the guard regresses later.
            assert!(!target.server_unc().is_absolute());
            assert!(
                crate::acquisition::Guard::open(
                    &target.server_unc(),
                    crate::acquisition::Policy {
                        explicit_root: true,
                        ..Default::default()
                    },
                )
                .is_err()
            );
        }
        for invalid in [
            "",
            ".",
            "..",
            "http://nas",
            r"C:\local",
            r"\\nas\share\..\other",
            r"\\nas\\share",
            "user:password@nas",
            "nas\n",
            r"\\.\C$",
        ] {
            assert!(parse_target(invalid).is_err(), "accepted invalid address");
        }
    }
    #[test]
    fn complete_share_and_nested_folder_keep_native_target() {
        let target = parse_target(r"\\nas\été 日本語\nested").unwrap();
        assert_eq!(target.server, OsStr::new("nas"));
        assert_eq!(
            target.share,
            Some(PathBuf::from(r"\\nas\été 日本語\nested"))
        );
        assert_eq!(
            connection_root(target.share.as_ref().unwrap()).unwrap(),
            Path::new(r"\\nas\été 日本語")
        );
    }
    #[test]
    fn explicit_extended_unc_preserves_native_spelling_and_long_path_semantics() {
        let input = format!(r"\\?\UNC\nas\share\{}\", "long".repeat(100));
        let target = parse_target(&input).unwrap();
        assert_eq!(target.share, Some(PathBuf::from(&input)));
        assert_eq!(target.server_unc(), Path::new(r"\\nas"));
        assert_eq!(
            connection_root(target.share.as_ref().unwrap()).unwrap(),
            Path::new(r"\\nas\share")
        );
    }
    #[test]
    fn native_connection_request_uses_current_identity_and_no_drive_or_saved_profile() {
        let result = connect_with(
            Path::new(r"\\nas\share"),
            42,
            RESOURCETYPE_DISK,
            CONNECT_INTERACTIVE,
            |owner, resource, password, username, flags| {
                assert_eq!(owner as usize, 42);
                assert_eq!(resource.dwType, RESOURCETYPE_DISK);
                assert!(resource.lpLocalName.is_null());
                assert!(resource.lpProvider.is_null());
                assert!(password.is_null());
                assert!(username.is_null());
                assert_eq!(flags, CONNECT_INTERACTIVE);
                let expected: Vec<_> = r"\\nas\share".encode_utf16().chain([0]).collect();
                // SAFETY: the provider receives the live terminated request buffer.
                assert_eq!(
                    unsafe { std::slice::from_raw_parts(resource.lpRemoteName, expected.len()) },
                    expected
                );
                ERROR_SESSION_CREDENTIAL_CONFLICT
            },
        );
        assert_eq!(result.unwrap_err().status, Status::SessionConflict);
    }
    #[test]
    fn disk_share_filter_excludes_hidden_admin_ipc_and_other_types() {
        let name = |name: &str| name.encode_utf16().collect::<Vec<_>>();
        assert!(visible_file_share(&name("été 日本語"), STYPE_DISKTREE));
        assert!(!visible_file_share(&name("C$"), STYPE_DISKTREE));
        assert!(!visible_file_share(&name("IPC$"), 3));
        assert!(!visible_file_share(&name("ADMIN"), STYPE_SPECIAL));
        assert!(!visible_file_share(&name("printer"), 1));
        assert!(!visible_file_share(&name(".."), 0));
        assert!(!visible_file_share(&name("bad/name"), 0));
    }
    #[test]
    fn native_errors_keep_auth_denial_cancellation_and_conflict_distinct() {
        for (code, status) in [
            (ERROR_ACCESS_DENIED, Status::AccessDenied),
            (ERROR_NOT_AUTHENTICATED, Status::AuthenticationRequired),
            (ERROR_LOGON_FAILURE, Status::CredentialsRejected),
            (ERROR_BAD_NETPATH, Status::ServerInaccessible),
            (ERROR_BAD_NET_NAME, Status::ShareMissing),
            (ERROR_CANCELLED, Status::Cancelled),
            (ERROR_SESSION_CREDENTIAL_CONFLICT, Status::SessionConflict),
        ] {
            assert_eq!(
                error(code, Operation::Connection),
                SmbError {
                    status,
                    os_code: code
                }
            );
        }
        assert_eq!(
            error(ERROR_BAD_NET_NAME, Operation::Enumeration).status,
            Status::ServerInaccessible
        );
        assert_eq!(
            error(ERROR_LOGON_FAILURE, Operation::Enumeration).status,
            Status::AuthenticationRequired
        );
        assert_eq!(
            ShareList {
                shares: vec![],
                limited: false
            }
            .status(),
            Status::NoShares
        );
    }
}
