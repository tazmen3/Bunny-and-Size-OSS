//! Native owner identifiers, without name resolution or textual conversion.
use crate::MetadataValue;
#[cfg(any(target_os = "linux", test))]
use crate::ValueAccuracy;

/// Native account identity. UID values are scoped to the source system/user
/// namespace, not globally stable identities. Future platforms may add variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeOwnerId {
    UnixUid(u32),
    /// Owned binary SID, available in the Windows local adapter.
    #[cfg(windows)]
    WindowsSid(WindowsSid),
}

/// Revision-1 SID: eight header bytes and at most fifteen subauthorities.
/// Padding is zeroed and is never part of the identity returned by `as_bytes`.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsSid([u8; 68]);

#[cfg(windows)]
impl WindowsSid {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8
            || bytes[0] != 1
            || bytes[1] > 15
            || bytes.len() != 8 + usize::from(bytes[1]) * 4
        {
            return None;
        }
        let mut sid = Self([0; 68]);
        sid.0[..bytes.len()].copy_from_slice(bytes);
        Some(sid)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0[..8 + usize::from(self.0[1]) * 4]
    }
}

// Keep the large Windows value out of every (including disabled) FIFO event.
// The builder copies it into its bounded contiguous table and drops this box.
#[cfg(windows)]
pub(crate) type ObservedOwner = Box<EntryOwner>;
#[cfg(not(windows))]
pub(crate) type ObservedOwner = EntryOwner;

pub(crate) fn observed(owner: EntryOwner) -> ObservedOwner {
    #[cfg(windows)]
    {
        Box::new(owner)
    }
    #[cfg(not(windows))]
    {
        owner
    }
}

pub(crate) fn retained(owner: &ObservedOwner) -> EntryOwner {
    #[cfg(windows)]
    {
        **owner
    }
    #[cfg(not(windows))]
    {
        *owner
    }
}

#[cfg(windows)]
pub(crate) mod windows {
    use super::*;
    use std::{fs::File, os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED, ERROR_INVALID_FUNCTION,
            ERROR_NOT_SUPPORTED, LocalFree,
        },
        Security::{
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            GetLengthSid, IsValidSid, OWNER_SECURITY_INFORMATION,
        },
    };

    struct Descriptor(*mut std::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this sole owner holds the buffer returned by GetSecurityInfo.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    pub(crate) fn missing(code: u32) -> EntryOwner {
        EntryOwner {
            id: match code {
                ERROR_ACCESS_DENIED => MetadataValue::AccessDenied,
                ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION | ERROR_CALL_NOT_IMPLEMENTED => {
                    MetadataValue::Unsupported
                }
                _ => MetadataValue::Unknown,
            },
        }
    }

    pub(crate) fn collect(file: &File) -> EntryOwner {
        #[cfg(feature = "owner-profiling")]
        crate::owner_profile::security();
        let mut sid = ptr::null_mut();
        let mut descriptor = Descriptor(ptr::null_mut());
        // SAFETY: live file handle; valid output addresses; only OWNER requested.
        // Descriptor owns the OS allocation even on an early return.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut sid,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor.0,
            )
        };
        if status != 0 {
            return missing(status);
        }
        if descriptor.0.is_null() || sid.is_null() {
            return EntryOwner {
                id: MetadataValue::Unknown,
            };
        }
        // SAFETY: non-null SID inside the live OS-provided descriptor. No user
        // byte slice is passed to an API that could read past its allocation.
        if unsafe { IsValidSid(sid) } == 0 {
            return EntryOwner {
                id: MetadataValue::Unknown,
            };
        }
        // SAFETY: SID was validated; its backing descriptor remains alive.
        let length = unsafe { GetLengthSid(sid) } as usize;
        if !(8..=68).contains(&length) {
            return EntryOwner {
                id: MetadataValue::Unknown,
            };
        }
        // SAFETY: GetSecurityInfo supplies the complete valid SID; GetLengthSid
        // bounds the copy to those bytes, before Descriptor is freed.
        let bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) };
        EntryOwner {
            id: match WindowsSid::from_bytes(bytes) {
                Some(sid) => MetadataValue::Known {
                    value: NativeOwnerId::WindowsSid(sid),
                    accuracy: crate::ValueAccuracy::Exact,
                },
                None => MetadataValue::Unknown,
            },
        }
    }
}

/// Owner of the observed node itself, never a recursive directory aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryOwner {
    pub id: MetadataValue<NativeOwnerId>,
}

impl EntryOwner {
    pub const fn not_collected() -> Self {
        Self {
            id: MetadataValue::NotCollected,
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn collect(metadata: &std::fs::Metadata) -> Self {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            Self::from_uid(metadata.uid())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = metadata;
            Self {
                id: MetadataValue::Unsupported,
            }
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn from_uid(uid: u32) -> Self {
        Self {
            id: MetadataValue::Known {
                value: NativeOwnerId::UnixUid(uid),
                accuracy: ValueAccuracy::Exact,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn windows_error_codes_preserve_proven_availability() {
        for (code, expected) in [
            (5, MetadataValue::AccessDenied),
            (50, MetadataValue::Unsupported),
            (1, MetadataValue::Unsupported),
            (120, MetadataValue::Unsupported),
            (2, MetadataValue::Unknown),
            (1337, MetadataValue::Unknown),
            (0, MetadataValue::Unknown),
        ] {
            assert_eq!(windows::missing(code).id, expected);
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn measure_in_memory_collection_without_system_calls() {
        let fixture = tempfile::TempDir::new().unwrap();
        let metadata = std::fs::symlink_metadata(fixture.path()).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..1_000_000 {
            std::hint::black_box(EntryOwner::collect(std::hint::black_box(&metadata)));
        }
        eprintln!("owner_collection_1000000_ns={}", start.elapsed().as_nanos());
    }

    #[test]
    fn zero_and_nonzero_uids_are_exact_and_never_sentinels() {
        for uid in [0, 1000, u32::MAX] {
            assert_eq!(
                EntryOwner::from_uid(uid).id,
                MetadataValue::Known {
                    value: NativeOwnerId::UnixUid(uid),
                    accuracy: ValueAccuracy::Exact,
                }
            );
        }
        assert!(!std::mem::needs_drop::<EntryOwner>());
    }
}
