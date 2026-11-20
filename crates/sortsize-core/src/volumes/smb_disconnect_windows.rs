//! Explicit, non-forced disconnection of one verified mapped SMB drive.
//!
//! A ticket must be prepared off the UI thread before asking for confirmation,
//! then consumed only after that confirmation. UNC/session-wide operations are
//! deliberately unsupported. No saved Windows profile is ever changed.
use super::{NetworkProtocol, namespace_network_protocol};
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, Prefix},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_CONNECTION_UNAVAIL, ERROR_INVALID_DATA, ERROR_MORE_DATA, ERROR_NOT_CONNECTED,
    },
    NetworkManagement::WNet::{WNetCancelConnection2W, WNetGetConnectionW},
};

const MAX_REMOTE_UNITS: usize = 32_768;

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

impl std::fmt::Display for DisconnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never include a server path or credentials in diagnostics.
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DisconnectError {}

/// Captures one exact provider target; not permission to call `disconnect`.
#[derive(Clone, Debug)]
pub struct DisconnectTicket {
    local: [u16; 3],
    remote: OsString,
}

fn local_root(root: &Path, composite: bool) -> Result<[u16; 3], DisconnectError> {
    if composite {
        return Err(DisconnectError::UnsupportedTarget);
    }
    let units: Vec<_> = root.as_os_str().encode_wide().take(4).collect();
    if units.starts_with(&[92, 92]) {
        return Err(DisconnectError::UnsupportedTarget);
    }
    // Reject relative roots, subdirectories, device paths and embedded NULs.
    if units.len() != 3
        || !matches!(units[0], 65..=90 | 97..=122)
        || units[1] != 58
        || !matches!(units[2], 47 | 92)
    {
        return Err(DisconnectError::InvalidRoot);
    }
    Ok([units[0], 58, 0])
}

fn query(local: &[u16; 3]) -> Result<OsString, u32> {
    let mut buffer = vec![0; 512];
    // At most two bounded attempts; providers do not control unbounded memory.
    for _ in 0..2 {
        let mut length = buffer.len() as u32;
        // SAFETY: local is terminated; the output buffer and length are valid.
        let code = unsafe { WNetGetConnectionW(local.as_ptr(), buffer.as_mut_ptr(), &mut length) };
        if code == 0 {
            let Some(end) = buffer.iter().position(|unit| *unit == 0) else {
                return Err(ERROR_INVALID_DATA);
            };
            if end == 0 {
                return Err(ERROR_INVALID_DATA);
            }
            return Ok(OsString::from_wide(&buffer[..end]));
        }
        if code != ERROR_MORE_DATA || length as usize > MAX_REMOTE_UNITS || length == 0 {
            return Err(code);
        }
        buffer.resize(length as usize, 0);
    }
    Err(ERROR_MORE_DATA)
}

fn prepare_with(
    root: &Path,
    composite: bool,
    protocol: NetworkProtocol,
    query: impl FnOnce(&[u16; 3]) -> Result<OsString, u32>,
) -> Result<DisconnectTicket, DisconnectError> {
    let local = local_root(root, composite)?;
    if protocol != NetworkProtocol::Smb {
        return Err(DisconnectError::UnsupportedTarget);
    }
    let remote = query(&local).map_err(|code| DisconnectError::Native {
        stage: DisconnectStage::Prepare,
        code,
    })?;
    // Provider targets must identify an actual share, never a server/session.
    let mut components = Path::new(&remote).components();
    if !matches!((components.next(), components.next(), components.next()),
        (Some(Component::Prefix(prefix)), Some(Component::RootDir), None)
        if matches!(prefix.kind(), Prefix::UNC(server, share) if !server.is_empty() && !share.is_empty()))
    {
        return Err(DisconnectError::UnsupportedTarget);
    }
    Ok(DisconnectTicket { local, remote })
}

/// Blocking native query. Preparing a ticket does not disconnect anything.
pub fn prepare(root: &Path, composite: bool) -> Result<DisconnectTicket, DisconnectError> {
    local_root(root, composite)?;
    let protocol = namespace_network_protocol(root).map_err(|error| DisconnectError::Native {
        stage: DisconnectStage::Prepare,
        code: error.raw_os_error().unwrap_or(ERROR_INVALID_DATA as i32) as u32,
    })?;
    prepare_with(root, composite, protocol, query)
}

fn disconnect_with(
    ticket: &DisconnectTicket,
    mut query: impl FnMut(&[u16; 3]) -> Result<OsString, u32>,
    cancel: impl FnOnce(&[u16; 3], u32, i32) -> u32,
) -> Result<(), DisconnectError> {
    let current = query(&ticket.local).map_err(|code| DisconnectError::Native {
        stage: DisconnectStage::Revalidate,
        code,
    })?;
    if current != ticket.remote {
        return Err(DisconnectError::TargetChanged);
    }
    // 0 preserves remembered connections. FALSE refuses open files/jobs.
    let code = cancel(&ticket.local, 0, 0);
    if code != 0 {
        return Err(DisconnectError::Native {
            stage: DisconnectStage::Cancel,
            code,
        });
    }
    match query(&ticket.local) {
        Err(ERROR_NOT_CONNECTED | ERROR_CONNECTION_UNAVAIL) => Ok(()),
        Ok(_) => Err(DisconnectError::VerificationFailed),
        Err(code) => Err(DisconnectError::Native {
            stage: DisconnectStage::Verify,
            code,
        }),
    }
}

/// Invoke only after explicit user confirmation, on a dedicated bounded worker.
/// System/provider calls can remain blocking; this API promises no deadline.
pub fn disconnect(ticket: &DisconnectTicket) -> Result<(), DisconnectError> {
    disconnect_with(ticket, query, |local, flags, force| {
        // SAFETY: a validated terminated drive letter, no profile update and no
        // forced close; the exact remote target was rechecked immediately above.
        unsafe { WNetCancelConnection2W(local.as_ptr(), flags, force) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DEVICE_IN_USE, ERROR_OPEN_FILES,
    };

    fn ticket() -> DisconnectTicket {
        prepare_with(Path::new(r"Z:\"), false, NetworkProtocol::Smb, |_| {
            Ok(r"\\test.invalid\share".into())
        })
        .unwrap()
    }

    #[test]
    fn unc_composite_nfs_unknown_subpath_and_relative_never_query() {
        for (path, composite, protocol) in [
            (r"\\test.invalid\share", false, NetworkProtocol::Smb),
            (r"Z:\", true, NetworkProtocol::Smb),
            (r"Z:\", false, NetworkProtocol::Nfs),
            (r"Z:\", false, NetworkProtocol::Unknown),
            (r"Z:\folder", false, NetworkProtocol::Smb),
            (r"Z:", false, NetworkProtocol::Smb),
            (r"\\?\Z:\", false, NetworkProtocol::Smb),
        ] {
            assert!(
                prepare_with(Path::new(path), composite, protocol, |_| panic!(
                    "unsafe target queried"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn provider_subdirectory_is_refused() {
        assert_eq!(
            prepare_with(Path::new(r"Z:\"), false, NetworkProtocol::Smb, |_| Ok(
                r"\\test.invalid\share\folder".into()
            ))
            .unwrap_err(),
            DisconnectError::UnsupportedTarget
        );
    }

    #[test]
    fn no_force_no_profile_update_and_system_verification() {
        for disconnected in [ERROR_NOT_CONNECTED, ERROR_CONNECTION_UNAVAIL] {
            let ticket = ticket();
            let mut calls = 0;
            assert_eq!(
                disconnect_with(
                    &ticket,
                    |_| {
                        calls += 1;
                        if calls == 1 {
                            Ok(ticket.remote.clone())
                        } else {
                            Err(disconnected)
                        }
                    },
                    |local, flags, force| {
                        assert_eq!(local, &[90, 58, 0]);
                        assert_eq!(flags, 0);
                        assert_eq!(force, 0);
                        0
                    }
                ),
                Ok(())
            );
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn remapped_drive_and_query_error_never_cancel() {
        let ticket = ticket();
        assert_eq!(
            disconnect_with(
                &ticket,
                |_| Ok(r"\\other.invalid\share".into()),
                |_, _, _| panic!("remapped target disconnected")
            ),
            Err(DisconnectError::TargetChanged)
        );
        assert!(matches!(
            disconnect_with(
                &ticket,
                |_| Err(ERROR_ACCESS_DENIED),
                |_, _, _| panic!("unverified target disconnected")
            ),
            Err(DisconnectError::Native {
                stage: DisconnectStage::Revalidate,
                ..
            })
        ));
    }

    #[test]
    fn busy_open_files_and_native_failure_are_honest() {
        let ticket = ticket();
        for code in [ERROR_OPEN_FILES, ERROR_DEVICE_IN_USE, ERROR_ACCESS_DENIED] {
            assert_eq!(
                disconnect_with(&ticket, |_| Ok(ticket.remote.clone()), |_, _, _| code),
                Err(DisconnectError::Native {
                    stage: DisconnectStage::Cancel,
                    code
                })
            );
        }
    }

    #[test]
    fn successful_cancel_without_system_confirmation_is_not_success() {
        let ticket = ticket();
        assert_eq!(
            disconnect_with(&ticket, |_| Ok(ticket.remote.clone()), |_, _, _| 0),
            Err(DisconnectError::VerificationFailed)
        );
        let mut calls = 0;
        assert_eq!(
            disconnect_with(
                &ticket,
                |_| {
                    calls += 1;
                    if calls == 1 {
                        Ok(ticket.remote.clone())
                    } else {
                        Err(ERROR_ACCESS_DENIED)
                    }
                },
                |_, _, _| 0
            ),
            Err(DisconnectError::Native {
                stage: DisconnectStage::Verify,
                code: ERROR_ACCESS_DENIED
            })
        );
    }
}
