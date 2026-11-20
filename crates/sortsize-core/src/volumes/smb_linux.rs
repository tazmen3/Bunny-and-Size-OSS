//! Explicit desktop SMB operations through GIO/GVfs. Never a file scanner.
//! Passwords stay in the desktop's authentication UI/keyring. An SMB URI is an
//! identity only: Rust acquisitions require a separately verified native mount.
use super::{NativeVolumes, VolumeIdentity, VolumeProvider};
use std::{
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const MAX_SHARES: usize = 256;
const MAX_OUTPUT: u64 = 1024 * 1024;
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
    DesktopConnectionPending,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmbError {
    pub status: Status,
    pub os_code: u32,
}
impl fmt::Display for SmbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} (Linux {})", self.status, self.os_code)
    }
}
impl std::error::Error for SmbError {}
fn error(status: Status) -> SmbError {
    SmbError { status, os_code: 0 }
}
fn invalid() -> SmbError {
    error(Status::InvalidInput)
}
fn native(e: io::Error) -> SmbError {
    SmbError {
        status: if e.kind() == io::ErrorKind::PermissionDenied {
            Status::AccessDenied
        } else {
            Status::Failed
        },
        os_code: e.raw_os_error().unwrap_or(0) as u32,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub server: OsString,
    pub share: Option<PathBuf>,
}
impl Target {
    /// Compatibility name shared with Windows; the Linux key is an SMB URI.
    pub fn server_unc(&self) -> PathBuf {
        PathBuf::from(format!("smb://{}/", self.server.to_string_lossy()))
    }
    pub fn share_path(&self, name: &OsStr) -> Result<PathBuf, SmbError> {
        let name = name.to_str().ok_or_else(invalid)?;
        if !valid_part(name) {
            return Err(invalid());
        }
        Ok(PathBuf::from(format!(
            "{}{}",
            self.server_unc().display(),
            encode(name)
        )))
    }
}
fn valid_part(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.len() <= 4096
        && !s
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '\0'))
}
fn decode(s: &str) -> Result<String, SmbError> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let h = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            let a = bytes.next().and_then(h).ok_or_else(invalid)?;
            let b = bytes.next().and_then(h).ok_or_else(invalid)?;
            out.push(a * 16 + b);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).map_err(|_| invalid())
}
fn encode(s: &str) -> String {
    let mut out = String::new();
    const HEX: &[u8] = b"0123456789ABCDEF";
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
    out
}
/// Syntax only; userinfo, passwords, query strings, traversal and URI escapes
/// that would change the path hierarchy are rejected before any OS operation.
pub fn parse_target(input: &str) -> Result<Target, SmbError> {
    if input.is_empty() || input.len() > 32767 || input.chars().any(char::is_control) {
        return Err(invalid());
    }
    let uri = input.starts_with("smb://");
    let value = if uri {
        input[6..].to_owned()
    } else {
        input
            .strip_prefix(r"\\?\UNC\")
            .or_else(|| input.strip_prefix(r"\\"))
            .or_else(|| input.strip_prefix("//"))
            .unwrap_or(input)
            .replace('\\', "/")
    };
    if value.contains(['?', '#', '@']) {
        return Err(invalid());
    }
    let mut parts = value.split('/');
    let server = parts.next().ok_or_else(invalid)?;
    if server.is_empty()
        || !server
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || matches!(server, "." | "..")
        || server.len() > 255
        || !server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(invalid());
    }
    let mut tail: Vec<_> = parts.collect();
    while tail.last() == Some(&"") {
        tail.pop();
    }
    let mut clean = Vec::new();
    for part in tail {
        let name = if uri { decode(part)? } else { part.to_owned() };
        if !valid_part(&name) {
            return Err(invalid());
        }
        clean.push(name);
    }
    if clean
        .first()
        .is_some_and(|s| s.eq_ignore_ascii_case("IPC$"))
    {
        return Err(invalid());
    }
    let mut target = Target {
        server: server.to_ascii_lowercase().into(),
        share: None,
    };
    if !clean.is_empty() {
        target.share = Some(PathBuf::from(format!(
            "{}{}",
            target.server_unc().display(),
            clean
                .iter()
                .map(|s| encode(s))
                .collect::<Vec<_>>()
                .join("/")
        )));
    }
    Ok(target)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub name: OsString,
    pub path: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareList {
    pub shares: Vec<Share>,
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

/// No shell, credentials, stdin prompt or unbounded captured output. Stderr is
/// classified privately, never returned or logged with provider/user data.
fn gio(args: &[&OsStr], timeout: Duration) -> Result<Vec<u8>, SmbError> {
    let output = tempfile::tempfile().map_err(native)?;
    let diagnostics = tempfile::tempfile().map_err(native)?;
    let mut child = Command::new("gio")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(native)?)
        .stderr(diagnostics.try_clone().map_err(native)?)
        .spawn()
        .map_err(native)?;
    let started = Instant::now();
    let status = loop {
        if output.metadata().map_err(native)?.len() > MAX_OUTPUT
            || diagnostics.metadata().map_err(native)?.len() > MAX_OUTPUT
            || started.elapsed() > timeout
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error(Status::ServerInaccessible));
        }
        if let Some(status) = child.try_wait().map_err(native)? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(40));
    };
    if !status.success() {
        use std::io::{Seek, SeekFrom};
        let mut diagnostics = diagnostics;
        diagnostics.seek(SeekFrom::Start(0)).map_err(native)?;
        let mut bytes = Vec::new();
        diagnostics
            .take(MAX_OUTPUT)
            .read_to_end(&mut bytes)
            .map_err(native)?;
        return Err(error(gio_failure(&bytes)));
    }
    use std::io::{Seek, SeekFrom};
    let mut output = output;
    output.seek(SeekFrom::Start(0)).map_err(native)?;
    let mut bytes = Vec::new();
    output
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(native)?;
    if bytes.len() as u64 > MAX_OUTPUT {
        return Err(error(Status::Failed));
    }
    Ok(bytes)
}
// Diagnostics are used only for a coarse status, never returned or logged.
fn gio_failure(bytes: &[u8]) -> Status {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if text.contains("cancelled") || text.contains("canceled") || text.contains("annulée") {
        Status::Cancelled
    } else if [
        "no route",
        "host is down",
        "timed out",
        "could not resolve",
        "network is unreachable",
        "connection refused",
        "connexion refusée",
        "hôte est éteint",
        "délai dépassé",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        Status::ServerInaccessible
    } else if [
        "permission denied",
        "access denied",
        "authentication",
        "password",
        "accès refusé",
        "permission non accordée",
        "authentification",
        "mot de passe",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        Status::AuthenticationRequired
    } else if text.contains("not mounted")
        || text.contains("does not exist")
        || text.contains("pas monté")
        || text.contains("introuvable")
    {
        Status::ShareMissing
    } else {
        Status::Failed
    }
}
fn parse_shares(target: &Target, bytes: &[u8]) -> Result<ShareList, SmbError> {
    let text = std::str::from_utf8(bytes).map_err(|_| error(Status::Failed))?;
    let mut result = ShareList {
        shares: Vec::new(),
        limited: false,
    };
    for line in text.lines() {
        let value = line.split("standard::target-uri=").nth(1).unwrap_or(line);
        let Ok(found) = parse_target(value) else {
            continue;
        };
        if found.server != target.server {
            continue;
        }
        let Some(path) = found.share else { continue };
        let suffix = path
            .to_str()
            .and_then(|v| v.strip_prefix(target.server_unc().to_str()?));
        let Some(name) = suffix
            .filter(|s| !s.contains('/'))
            .and_then(|s| decode(s).ok())
        else {
            continue;
        };
        if name.ends_with('$') || result.shares.iter().any(|s| s.path == path) {
            continue;
        }
        if result.shares.len() == MAX_SHARES {
            result.limited = true;
            break;
        }
        result.shares.push(Share {
            name: name.into(),
            path,
        });
    }
    result.shares.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}
/// Explicit server-share discovery only; never recursively lists file contents.
pub fn enumerate_shares(target: &Target) -> Result<ShareList, SmbError> {
    let checked = parse_target(target.server_unc().to_str().ok_or_else(invalid)?)?;
    if checked.share.is_some() {
        return Err(invalid());
    }
    let uri = checked.server_unc();
    let args = [
        OsStr::new("list"),
        OsStr::new("-l"),
        OsStr::new("-a"),
        OsStr::new("standard::target-uri"),
        uri.as_os_str(),
    ];
    let bytes = match gio(&args, Duration::from_secs(12)) {
        Ok(bytes) => bytes,
        Err(_) => {
            // Anonymous mount cannot request or receive a password. Authenticated
            // discovery is retried by the user after connecting in the desktop.
            gio(
                &[OsStr::new("mount"), OsStr::new("-a"), uri.as_os_str()],
                Duration::from_secs(12),
            )?;
            gio(&args, Duration::from_secs(12))?
        }
    };
    parse_shares(&checked, &bytes)
}
fn gvfs_root() -> PathBuf {
    // SAFETY: geteuid has no preconditions or side effects.
    PathBuf::from(format!("/run/user/{}/gvfs", unsafe { libc::geteuid() }))
}
pub(crate) fn gvfs_target(name: &OsStr) -> Option<Target> {
    let suffix = name.to_str()?.strip_prefix("smb-share:")?;
    let mut server = None;
    let mut share = None;
    for field in suffix.split(',') {
        let (key, value) = field.split_once('=')?;
        match key {
            "server" => server = Some(decode(value).ok()?),
            "share" => share = Some(decode(value).ok()?),
            "user" | "domain" => {}
            _ => return None,
        }
    }
    let target = parse_target(&server?).ok()?;
    let uri = target.share_path(OsStr::new(&share?)).ok()?;
    parse_target(uri.to_str()?).ok()
}
fn locations() -> Result<Vec<(PathBuf, PathBuf)>, SmbError> {
    let volumes = NativeVolumes.enumerate().map_err(native)?;
    let mut out = Vec::new();
    let gvfs = gvfs_root();
    for volume in volumes {
        let VolumeIdentity::Linux {
            filesystem, source, ..
        } = volume.identity
        else {
            continue;
        };
        if ["cifs", "smb3", "smbfs"]
            .iter()
            .any(|fs| filesystem == OsStr::new(fs))
        {
            if let Some(target) = source.to_str().and_then(|s| parse_target(s).ok())
                && let Some(uri) = target.share
            {
                for mount in volume.mounts {
                    // Bind subtrees need a separately proven relative mapping.
                    if mount.root.as_deref() == Some(Path::new("/")) {
                        out.push((uri.clone(), mount.path));
                    }
                }
            }
        } else if filesystem == OsStr::new("fuse.gvfsd-fuse")
            && volume.mounts.iter().any(|m| m.path == gvfs)
        {
            // Only the local FUSE mount-name directory, never a remote share.
            if let Ok(entries) = fs::read_dir(&gvfs) {
                for entry in entries.take(MAX_SHARES).flatten() {
                    if let Some(target) = gvfs_target(&entry.file_name())
                        && let Some(uri) = target.share
                    {
                        out.push((uri, entry.path()));
                    }
                }
            }
        }
    }
    Ok(out)
}
/// Compare logical SMB scopes without changing their persisted spelling.
/// SMB share names are ASCII-case-insensitive; paths below a share retain their
/// native case. Server aliases are never inferred or contacted here.
pub fn covers(scope: &Path, path: &Path) -> bool {
    logical_relative(scope, path).is_some()
}
fn logical_parts(path: &Path) -> Option<(OsString, Vec<String>)> {
    let target = parse_target(path.to_str()?).ok()?;
    let server_root = target.server_unc();
    let parts = match target.share {
        Some(uri) => uri
            .to_str()?
            .strip_prefix(server_root.to_str()?)?
            .split('/')
            .map(decode)
            .collect::<Result<Vec<_>, _>>()
            .ok()?,
        None => Vec::new(),
    };
    Some((target.server, parts))
}
fn logical_relative(scope: &Path, path: &Path) -> Option<PathBuf> {
    let (scope_server, scope_parts) = logical_parts(scope)?;
    let (path_server, path_parts) = logical_parts(path)?;
    if scope_server != path_server || scope_parts.len() > path_parts.len() {
        return None;
    }
    for (index, (scope, path)) in scope_parts.iter().zip(&path_parts).enumerate() {
        if if index == 0 {
            !scope.eq_ignore_ascii_case(path)
        } else {
            scope != path
        } {
            return None;
        }
    }
    Some(path_parts[scope_parts.len()..].iter().collect())
}
/// Local mount evidence only. No server probe, canonicalize or file enumeration.
/// A missing mount never resolves to the empty directory underneath it.
pub fn resolve_path(path: &Path) -> Result<PathBuf, SmbError> {
    resolve_from_locations(path, locations()?)
}
fn resolve_from_locations(
    path: &Path,
    entries: Vec<(PathBuf, PathBuf)>,
) -> Result<PathBuf, SmbError> {
    if path.is_absolute() {
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(invalid());
        }
        return entries
            .into_iter()
            .filter(|(_, native)| path.starts_with(native))
            .max_by_key(|(_, native)| native.components().count())
            .map(|_| path.to_path_buf())
            .ok_or_else(|| error(Status::ShareMissing));
    }
    let target = parse_target(path.to_str().ok_or_else(invalid)?)?;
    let uri = target.share.ok_or_else(invalid)?;
    let mut resolved: Option<(usize, PathBuf)> = None;
    let mut ambiguous = false;
    for (logical, native) in entries {
        let Some(tail) = logical_relative(&logical, &uri) else {
            continue;
        };
        let specificity = logical.components().count();
        let candidate = if tail.as_os_str().is_empty() {
            native
        } else {
            native.join(tail)
        };
        match &resolved {
            Some((depth, _)) if *depth > specificity => {}
            Some((depth, previous)) if *depth == specificity && *previous != candidate => {
                // Different user/domain mounts must not select an arbitrary
                // authenticated session merely because share casing matches.
                ambiguous = true;
            }
            Some((depth, _)) if *depth == specificity => {}
            _ => {
                resolved = Some((specificity, candidate));
                ambiguous = false;
            }
        }
    }
    if ambiguous {
        return Err(error(Status::SessionConflict));
    }
    resolved
        .map(|(_, native)| native)
        .ok_or_else(|| error(Status::ShareMissing))
}
pub fn resolve_mount(path: &Path) -> Result<PathBuf, SmbError> {
    resolve_path(path)
}
/// Reverse local mount mapping, including a relative path inside one share.
/// This never invents equivalence between DNS aliases or physical NAS storage.
pub fn logical_path(path: &Path) -> Option<PathBuf> {
    let entries = locations().ok()?;
    let (uri, root) = entries
        .into_iter()
        .filter(|(_, root)| path.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())?;
    let mut text = uri.to_str()?.to_owned();
    for component in path.strip_prefix(root).ok()?.components() {
        let std::path::Component::Normal(name) = component else {
            return None;
        };
        text.push('/');
        text.push_str(&encode(name.to_str()?));
    }
    Some(text.into())
}
pub fn mounted_uri(path: &Path) -> Option<PathBuf> {
    locations()
        .ok()?
        .into_iter()
        .find(|(_, native)| native == path)
        .map(|(uri, _)| uri)
}
fn usable(path: &Path) -> Result<(), SmbError> {
    let native_path = resolve_path(path)?;
    let metadata = fs::metadata(&native_path).map_err(native)?;
    if metadata.is_dir() {
        // Open permission only: never consume the directory iterator here.
        drop(fs::read_dir(native_path).map_err(native)?);
        Ok(())
    } else {
        Err(error(Status::ShareMissing))
    }
}
fn open_desktop(uri: &Path) -> Result<(), SmbError> {
    if gio(
        &[OsStr::new("open"), uri.as_os_str()],
        Duration::from_secs(5),
    )
    .is_ok()
    {
        return Ok(());
    }
    // GIO open pre-probes unmounted SMB URIs on some desktops. Launch Files
    // directly through its system desktop entry so its native mount operation
    // owns interactive authentication. Do not change MIME associations.
    let files = Path::new("/usr/share/applications/org.gnome.Nautilus.desktop");
    if files.is_file() {
        return gio(
            &[OsStr::new("launch"), files.as_os_str(), uri.as_os_str()],
            Duration::from_secs(5),
        )
        .map(|_| ());
    }
    Err(error(Status::DesktopConnectionPending))
}
pub fn connect_share(path: &Path, _owner: usize) -> Result<(), SmbError> {
    if usable(path).is_ok() {
        return Ok(());
    }
    let uri = if path.is_absolute() {
        mounted_uri(path).ok_or_else(|| error(Status::ShareMissing))?
    } else {
        parse_target(path.to_str().ok_or_else(invalid)?)?
            .share
            .ok_or_else(invalid)?
    };
    // Explicit anonymous native connection is useful for guest shares and
    // cannot consume or request a password. Authentication remains in Files.
    if gio(
        &[OsStr::new("mount"), OsStr::new("-a"), uri.as_os_str()],
        Duration::from_secs(10),
    )
    .is_ok()
        && usable(&uri).is_ok()
    {
        return Ok(());
    }
    open_desktop(&uri)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if usable(&uri).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(error(Status::DesktopConnectionPending))
}
pub fn connect_target(target: &Target, owner: usize) -> Result<(), SmbError> {
    if let Some(path) = &target.share {
        return connect_share(path, owner);
    }
    let checked = parse_target(target.server_unc().to_str().ok_or_else(invalid)?)?;
    if checked.share.is_some() {
        return Err(invalid());
    }
    // A logical server has no acquisition path. Require at least one exact
    // share already exposed to Rust before declaring the server usable.
    for (uri, _) in locations()? {
        if parse_target(uri.to_str().ok_or_else(invalid)?)?.server == checked.server
            && usable(&uri).is_ok()
        {
            return Ok(());
        }
    }
    open_desktop(&checked.server_unc())?;
    // Opening a server in Files says nothing about share-level Rust access.
    Err(error(Status::DesktopConnectionPending))
}
pub fn reconnect(path: &Path, is_server: bool, owner: usize) -> Result<(), SmbError> {
    if is_server {
        let target = parse_target(path.to_str().ok_or_else(invalid)?)?;
        if target.share.is_some() {
            return Err(invalid());
        }
        connect_target(&target, owner)
    } else {
        connect_share(path, owner)
    }
}

pub mod disconnect {
    use super::*;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DisconnectStage {
        Prepare,
        Revalidate,
        Cancel,
        Verify,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DisconnectError {
        UnsupportedTarget,
        InvalidRoot,
        Native { stage: DisconnectStage, code: u32 },
        TargetChanged,
        VerificationFailed,
    }
    impl fmt::Display for DisconnectError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{self:?}")
        }
    }
    impl std::error::Error for DisconnectError {}
    #[derive(Clone, Debug)]
    pub struct DisconnectTicket {
        local: PathBuf,
        uri: PathBuf,
    }
    pub fn prepare(root: &Path, composite: bool) -> Result<DisconnectTicket, DisconnectError> {
        if composite {
            return Err(DisconnectError::UnsupportedTarget);
        }
        let local = resolve_path(root).map_err(|_| DisconnectError::UnsupportedTarget)?;
        if local.parent() != Some(gvfs_root().as_path()) {
            return Err(DisconnectError::UnsupportedTarget);
        }
        let uri = mounted_uri(&local).ok_or(DisconnectError::UnsupportedTarget)?;
        Ok(DisconnectTicket { local, uri })
    }
    pub fn disconnect(ticket: &DisconnectTicket) -> Result<(), DisconnectError> {
        let current = prepare(&ticket.local, false)?;
        if current.uri != ticket.uri {
            return Err(DisconnectError::TargetChanged);
        }
        // No --force, sudo, system unmount, scheme-wide or server-wide action.
        gio(
            &[
                OsStr::new("mount"),
                OsStr::new("-u"),
                ticket.uri.as_os_str(),
            ],
            Duration::from_secs(10),
        )
        .map_err(|e| DisconnectError::Native {
            stage: DisconnectStage::Cancel,
            code: e.os_code,
        })?;
        if mounted_uri(&ticket.local).is_some() {
            Err(DisconnectError::VerificationFailed)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_diagnostics_produce_only_coarse_credential_free_states() {
        assert_eq!(
            gio_failure(b"gio: location not mounted"),
            Status::ShareMissing
        );
        assert_eq!(gio_failure(b"No route to host"), Status::ServerInaccessible);
        assert_eq!(
            gio_failure(b"Permission denied"),
            Status::AuthenticationRequired
        );
        assert_eq!(gio_failure(b"Operation cancelled"), Status::Cancelled);
        assert_eq!(gio_failure(b"Unknown provider failure"), Status::Failed);
    }
    #[test]
    fn uri_unc_and_server_inputs_are_pure_canonical_keys() {
        for input in ["NAS", "smb://nas/", r"\\NAS\", "//NAS/"] {
            assert_eq!(
                parse_target(input).unwrap().server_unc(),
                Path::new("smb://nas/")
            );
        }
        for input in [r"\\NAS\Mes données", "smb://NAS/Mes%20donn%C3%A9es"] {
            assert_eq!(
                parse_target(input).unwrap().share.unwrap(),
                Path::new("smb://nas/Mes%20donn%C3%A9es")
            );
        }
    }
    #[test]
    fn credentials_traversal_and_hierarchy_escapes_are_rejected() {
        for input in [
            "smb://user:password@nas/share",
            "smb://nas/a/../b",
            "smb://nas/%2E%2E",
            "smb://nas/a%2Fb",
            "smb://nas/a%5Cb",
            "smb://nas/a%00b",
            "smb://nas/%FF",
            "smb://nas/share?secret=x",
            "smb://nas/IPC$",
            "file:///tmp",
            "-option",
            "smb://nas//share",
        ] {
            assert!(parse_target(input).is_err(), "{input}");
        }
    }
    #[test]
    fn listing_accepts_only_immediate_file_shares_of_the_requested_server() {
        let target = parse_target("nas").unwrap();
        let list=parse_shares(&target,b"data\t0\t(mountable)\tstandard::target-uri=smb://nas/data\nsmb://nas/IPC$\nsmb://other/data\nsmb://nas/data/file\nsmb://nas/data\nsmb://nas/Mes%20donn%C3%A9es\n").unwrap();
        assert_eq!(list.shares.len(), 2);
        assert_eq!(list.shares[0].name, OsStr::new("Mes données"));
    }
    #[test]
    fn gvfs_names_keep_identity_and_reject_unknown_options() {
        assert_eq!(
            gvfs_target(OsStr::new(
                "smb-share:server=NAS,share=Mes%20donn%C3%A9es,user=test"
            ))
            .unwrap()
            .share
            .unwrap(),
            Path::new("smb://nas/Mes%20donn%C3%A9es")
        );
        assert_eq!(
            gvfs_target(OsStr::new(
                "smb-share:share=Mes%20donn%C3%A9es,domain=WORKGROUP,user=test,server=NAS"
            ))
            .unwrap()
            .share
            .unwrap(),
            Path::new("smb://nas/Mes%20donn%C3%A9es")
        );
        for name in [
            "smb-share:server=nas,share=data,malformed",
            "smb-share:server=nas,share=data,",
            "smb-share:server=nas",
            "smb-share:share=data",
            "smb-share:server=nas,share=%FF",
            "smb-share:server=nas,share=data%2Fchild",
        ] {
            assert!(gvfs_target(OsStr::new(name)).is_none(), "{name}");
        }
        assert!(
            gvfs_target(OsStr::new(
                "smb-share:server=nas,share=data,password=secret"
            ))
            .is_none()
        );
    }
    #[test]
    fn mounted_share_case_is_matched_without_rewriting_logical_identity() {
        let requested = Path::new("smb://192.168.1.144/TV");
        let native = PathBuf::from("/native/smb-share:server=192.168.1.144,share=tv");
        let mounted = vec![(PathBuf::from("smb://192.168.1.144/tv"), native.clone())];
        assert_eq!(resolve_from_locations(requested, mounted).unwrap(), native);
        assert_eq!(
            parse_target(requested.to_str().unwrap())
                .unwrap()
                .share
                .unwrap(),
            requested
        );
    }
    #[test]
    fn logical_scopes_fold_only_share_case_and_preserve_decoded_descendants() {
        for (scope, path) in [
            ("smb://NAS/TV", "smb://nas/tv"),
            ("smb://nas/TV", "smb://nas/tv/Season%201"),
            ("smb://nas/TV/Season%201", "smb://nas/tv/Season%201/episode"),
            ("smb://nas/", "smb://nas/TV"),
            ("smb://nas/My%20Files", "smb://nas/my%20files/%C3%A9pisode"),
        ] {
            assert!(
                covers(Path::new(scope), Path::new(path)),
                "{scope} : {path}"
            );
        }
        for (scope, path) in [
            ("smb://nas/TV", "smb://other/tv"),
            ("smb://nas/TV", "smb://nas-alias/tv"),
            ("smb://nas/TV", "smb://nas/tv-archive"),
            ("smb://nas/TV/Season", "smb://nas/tv/season"),
            ("smb://nas/TV", "smb://nas/tv/%2E%2E"),
            ("smb://nas/TV", "smb://nas/tv/a%2Fb"),
            ("smb://nas/TV", "smb://user@nas/tv"),
            ("smb://nas/TV", "/native/tv"),
        ] {
            assert!(
                !covers(Path::new(scope), Path::new(path)),
                "{scope} : {path}"
            );
        }
        let mounted = vec![(
            PathBuf::from("smb://nas/my%20files"),
            PathBuf::from("/native"),
        )];
        assert_eq!(
            resolve_from_locations(
                Path::new("smb://nas/My%20Files/Season%201/%C3%A9pisode"),
                mounted
            )
            .unwrap(),
            Path::new("/native/Season 1/épisode")
        );
    }
    #[test]
    fn distinct_native_sessions_for_one_share_are_not_chosen_arbitrarily() {
        let mounted = vec![
            (
                PathBuf::from("smb://nas/TV"),
                PathBuf::from("/native/user-a"),
            ),
            (
                PathBuf::from("smb://nas/tv"),
                PathBuf::from("/native/user-b"),
            ),
        ];
        assert_eq!(
            resolve_from_locations(Path::new("smb://nas/TV"), mounted.clone())
                .unwrap_err()
                .status,
            Status::SessionConflict
        );
        assert_eq!(
            resolve_from_locations(Path::new("/native/user-a"), mounted).unwrap(),
            Path::new("/native/user-a")
        );
        assert_eq!(
            resolve_from_locations(Path::new("smb://nas/TV"), Vec::new())
                .unwrap_err()
                .status,
            Status::ShareMissing
        );
    }
    #[test]
    fn system_and_composite_disconnects_are_refused_without_a_native_call() {
        assert_eq!(
            disconnect::prepare(Path::new("/"), true).unwrap_err(),
            disconnect::DisconnectError::UnsupportedTarget
        );
        assert_eq!(
            disconnect::prepare(Path::new("/not-a-user-gvfs-mount"), false).unwrap_err(),
            disconnect::DisconnectError::UnsupportedTarget
        );
    }
}
