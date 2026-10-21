use super::*;

/// Explicit permission to invoke host identity providers. NSS and Windows may
/// contact remote servers. This is NOT a promise of offline/local-only lookup.
#[derive(Debug, Clone, Copy)]
pub enum SystemProviderConsent {
    AllowPotentialNetworkLookup,
}

/// Local-host namespace only: do not resolve IDs from a different machine,
/// container/user namespace or saved inventory without an appropriate provider.
pub struct SystemOwnerLookup {
    native_calls: AtomicUsize,
}
impl SystemOwnerLookup {
    pub fn new(_consent: SystemProviderConsent) -> Self {
        Self {
            native_calls: AtomicUsize::new(0),
        }
    }
    /// Number of native API invocations (Windows sizing and fill are separate).
    /// These are library calls, NOT a count of kernel/network operations.
    pub fn native_calls(&self) -> usize {
        self.native_calls.load(Ordering::Relaxed)
    }
}
impl OwnerLookup for SystemOwnerLookup {
    fn lookup(&self, id: NativeOwnerId, limits: LookupLimits) -> OwnerNameResolution {
        if !(1..=1024 * 1024).contains(&limits.max_buffer_bytes)
            || !(1..=64 * 1024).contains(&limits.max_name_bytes)
        {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        }
        match id {
            #[cfg(target_os = "linux")]
            NativeOwnerId::UnixUid(uid) => linux::lookup(self, uid, limits),
            #[cfg(windows)]
            NativeOwnerId::WindowsSid(sid) => windows::lookup(self, sid, limits),
            #[allow(unreachable_patterns)]
            _ => {
                let _ = limits;
                OwnerNameResolution::Unavailable(LookupError::UnsupportedIdentity)
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{mem::MaybeUninit, ptr};
    pub(super) fn error(code: i32) -> OwnerNameResolution {
        use OwnerNameResolution::*;
        match code {
            libc::ENOENT | libc::ESRCH => NotFound,
            libc::EACCES | libc::EPERM => AccessDenied,
            libc::ENOSYS | libc::ENOTSUP => Unavailable(LookupError::System(code.into())),
            libc::ERANGE => Unavailable(LookupError::BufferLimit),
            _ => TemporaryFailure(LookupError::System(code.into())),
        }
    }
    pub(super) fn lookup(
        provider: &SystemOwnerLookup,
        uid: u32,
        limits: LookupLimits,
    ) -> OwnerNameResolution {
        // One fixed bounded allocation/call, no unbounded ERANGE growth. All
        // struct/string pointers are used only while this backing buffer lives.
        if limits.max_buffer_bytes == 0 || limits.max_buffer_bytes > 1024 * 1024 {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        }
        let mut buffer = vec![0u8; limits.max_buffer_bytes];
        let mut record = MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        provider.native_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: valid record/output pointers and writable buffer of the exact
        // supplied size; libc owns no returned allocation. No errno guessing.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status != 0 {
            return error(status);
        }
        if result.is_null() {
            return OwnerNameResolution::NotFound;
        }
        // SAFETY: successful non-null result initializes the supplied record.
        let record = unsafe { record.assume_init() };
        // Validate the name range before reading, even for a faulty NSS module.
        let offset = (record.pw_name as usize).wrapping_sub(buffer.as_ptr() as usize);
        let Some(name) = buffer.get(offset..) else {
            return OwnerNameResolution::Unavailable(LookupError::System(libc::EIO.into()));
        };
        let Some(length) = name.iter().position(|b| *b == 0) else {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        };
        if length > limits.max_name_bytes {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        }
        OwnerNameResolution::Resolved(AccountName::UnixBytes(name[..length].into()))
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn error_mapping_does_not_invent_an_unknown_account() {
            assert_eq!(error(libc::EACCES), OwnerNameResolution::AccessDenied);
            assert_eq!(
                error(libc::ENOSYS),
                OwnerNameResolution::Unavailable(LookupError::System(libc::ENOSYS.into()))
            );
            assert_eq!(
                error(libc::EAGAIN),
                OwnerNameResolution::TemporaryFailure(LookupError::System(libc::EAGAIN.into()))
            );
            assert_eq!(
                error(libc::ERANGE),
                OwnerNameResolution::Unavailable(LookupError::BufferLimit)
            );
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::{Foundation::*, Security::LookupAccountSidW};
    pub(super) fn error(code: u32) -> OwnerNameResolution {
        use OwnerNameResolution::*;
        match code {
            ERROR_NONE_MAPPED => NotFound,
            ERROR_ACCESS_DENIED => AccessDenied,
            ERROR_NOT_SUPPORTED | ERROR_CALL_NOT_IMPLEMENTED | ERROR_INVALID_FUNCTION => {
                Unavailable(LookupError::System(code.into()))
            }
            ERROR_INSUFFICIENT_BUFFER => Unavailable(LookupError::BufferLimit),
            _ => TemporaryFailure(LookupError::System(code.into())),
        }
    }
    pub(super) fn lookup(
        provider: &SystemOwnerLookup,
        sid: sortsize_core::WindowsSid,
        limits: LookupLimits,
    ) -> OwnerNameResolution {
        // DWORD-aligned private copy; never pass an unaligned u8-array pointer.
        let mut aligned = [0u32; 17];
        // SAFETY: the validated SID is at most 68 bytes; destination is 68 bytes.
        unsafe {
            ptr::copy_nonoverlapping(
                sid.as_bytes().as_ptr(),
                aligned.as_mut_ptr().cast::<u8>(),
                sid.as_bytes().len(),
            );
        }
        let sid_ptr = aligned.as_mut_ptr().cast();
        let (mut names, mut domains, mut use_kind) = (0u32, 0u32, 0i32);
        provider.native_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: aligned complete SID, valid length/use outputs. Null buffers
        // and zero capacities are the documented sizing query; local system.
        let ok = unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid_ptr,
                ptr::null_mut(),
                &mut names,
                ptr::null_mut(),
                &mut domains,
                &mut use_kind,
            )
        };
        // SAFETY: immediately after the failed Win32 call on this thread.
        let code = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        if code != ERROR_INSUFFICIENT_BUFFER {
            return error(code);
        }
        let Some(units) = (names as usize).checked_add(domains as usize) else {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        };
        if names == 0 || units > limits.max_buffer_bytes / 2 {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        }
        let mut buffer = vec![0u16; units];
        let (name, domain) = buffer.split_at_mut(names as usize);
        provider.native_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: buffer lengths match sizing capacities; valid SID and outputs.
        // All buffers are Rust-owned and freed on every return; no LocalFree.
        let ok = unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid_ptr,
                name.as_mut_ptr(),
                &mut names,
                domain.as_mut_ptr(),
                &mut domains,
                &mut use_kind,
            )
        };
        if ok == 0 {
            return error(unsafe { GetLastError() });
        }
        let Some(name) = name.get(..names as usize) else {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        };
        let Some(domain) = domain.get(..domains as usize) else {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        };
        // Successful output counts exclude their terminating NULs. Preserve
        // UTF-16 code units rather than silently replacing unpaired surrogates.
        let length = name.len() + domain.len() + usize::from(!domain.is_empty());
        if length > limits.max_name_bytes / 2 {
            return OwnerNameResolution::Unavailable(LookupError::BufferLimit);
        }
        let mut qualified = Vec::with_capacity(length);
        if !domain.is_empty() {
            qualified.extend_from_slice(domain);
            qualified.push(b'\\'.into());
        }
        qualified.extend_from_slice(name);
        OwnerNameResolution::Resolved(AccountName::WindowsUtf16(qualified.into_boxed_slice()))
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn error_mapping_distinguishes_domain_outages_from_deleted_accounts() {
            assert_eq!(error(ERROR_NONE_MAPPED), OwnerNameResolution::NotFound);
            assert_eq!(
                error(ERROR_ACCESS_DENIED),
                OwnerNameResolution::AccessDenied
            );
            assert_eq!(
                error(ERROR_NO_SUCH_DOMAIN),
                OwnerNameResolution::TemporaryFailure(LookupError::System(
                    ERROR_NO_SUCH_DOMAIN.into()
                ))
            );
            assert_eq!(
                error(ERROR_INSUFFICIENT_BUFFER),
                OwnerNameResolution::Unavailable(LookupError::BufferLimit)
            );
            for code in [
                ERROR_NOT_SUPPORTED,
                ERROR_CALL_NOT_IMPLEMENTED,
                ERROR_INVALID_FUNCTION,
            ] {
                assert_eq!(
                    error(code),
                    OwnerNameResolution::Unavailable(LookupError::System(code.into()))
                );
            }
            assert_eq!(
                error(ERROR_INVALID_SID),
                OwnerNameResolution::TemporaryFailure(LookupError::System(
                    ERROR_INVALID_SID.into()
                ))
            );
        }
    }
}
