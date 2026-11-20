//! Local account-file lookup only. This provider never invokes libc/NSS.
use crate::{
    AccountName, LookupError, LookupLimits, NativeOwnerId, OwnerLookup, OwnerNameResolution,
};
use std::io::Read;

/// Resolves host UIDs against `/etc/passwd` on a caller-owned worker.
/// Unknown UIDs stay numeric; LDAP, SSSD, NIS and other NSS providers are not used.
/// The fixed input/name budgets are the same as for other owner providers.
pub struct LocalLinuxOwnerLookup;
impl OwnerLookup for LocalLinuxOwnerLookup {
    fn lookup(&self, id: NativeOwnerId, limits: LookupLimits) -> OwnerNameResolution {
        let NativeOwnerId::UnixUid(uid) = id else {
            return OwnerNameResolution::Unavailable(LookupError::UnsupportedIdentity);
        };
        let file = match std::fs::File::open("/etc/passwd") {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return OwnerNameResolution::AccessDenied;
            }
            Err(error) => {
                return OwnerNameResolution::TemporaryFailure(LookupError::System(
                    error.raw_os_error().unwrap_or(0).into(),
                ));
            }
        };
        lookup_reader(file, uid, limits)
    }
}
fn lookup_reader(reader: impl Read, uid: u32, limits: LookupLimits) -> OwnerNameResolution {
    if !(1..=1024 * 1024).contains(&limits.max_buffer_bytes)
        || !(1..=64 * 1024).contains(&limits.max_name_bytes)
    {
        return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
    }
    let mut bytes = Vec::with_capacity(limits.max_buffer_bytes.min(4096));
    if let Err(error) = reader
        .take(limits.max_buffer_bytes as u64 + 1)
        .read_to_end(&mut bytes)
    {
        return OwnerNameResolution::TemporaryFailure(LookupError::System(
            error.raw_os_error().unwrap_or(0).into(),
        ));
    }
    if bytes.len() > limits.max_buffer_bytes {
        return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
    }
    for line in bytes.split(|b| *b == b'\n') {
        let mut fields = line.split(|b| *b == b':');
        let Some(name) = fields.next().filter(|name| !name.is_empty()) else {
            continue;
        };
        let _password = fields.next();
        let candidate = fields
            .next()
            .and_then(|id| std::str::from_utf8(id).ok())
            .and_then(|id| id.parse::<u32>().ok());
        if candidate == Some(uid) {
            if name.len() > limits.max_name_bytes {
                return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
            }
            return OwnerNameResolution::Resolved(AccountName::UnixBytes(name.into()));
        }
    }
    OwnerNameResolution::NotFound
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_file_resolution_preserves_bytes_unknown_ids_and_limits() {
        let limits = LookupLimits {
            max_buffer_bytes: 256,
            max_name_bytes: 32,
        };
        let accounts = b"root:x:0:0:root:/root:/bin/sh\nname\xff:x:1000:1000::/:/bin/sh\n";
        assert_eq!(
            lookup_reader(&accounts[..], 1000, limits),
            OwnerNameResolution::Resolved(AccountName::UnixBytes(b"name\xff".to_vec().into()))
        );
        assert_eq!(
            lookup_reader(&accounts[..], 1001, limits),
            OwnerNameResolution::NotFound
        );
        assert_eq!(
            lookup_reader(
                &accounts[..],
                0,
                LookupLimits {
                    max_buffer_bytes: 8,
                    ..limits
                }
            ),
            OwnerNameResolution::Unavailable(LookupError::BufferLimit)
        );
        assert_eq!(
            lookup_reader(
                &accounts[..],
                1000,
                LookupLimits {
                    max_name_bytes: 2,
                    ..limits
                }
            ),
            OwnerNameResolution::Unavailable(LookupError::BufferLimit)
        );
    }
}
