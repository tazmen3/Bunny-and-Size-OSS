//! Same-handle metadata for the admitted local NTFS acquisition path.
//!
//! FileStatInformation combines dates, attributes and both sizes. Keep the
//! separate 128-bit identity query: its 64-bit FileId is not interchangeable.
use super::*;
use crate::{MetadataValue, ValueAccuracy};
use std::time::{Duration, UNIX_EPOCH};
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_STAT_INFORMATION, FileStatInformation, NtQueryInformationFile,
    },
    Win32::{
        Foundation::{
            RtlNtStatusToDosError, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_INFO_CLASS,
            STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

pub(super) fn observe_with(
    detailed: bool,
    allocation: bool,
    owners: bool,
    open: impl FnMut(u32) -> io::Result<fs::File>,
) -> io::Result<Stamp> {
    let (file, owner_error) = open_for_observation(detailed, owners, open)?;
    observe_file(&file, detailed, allocation, owners, owner_error)
}

pub(super) fn observe_file(
    file: &fs::File,
    detailed: bool,
    allocation: bool,
    owners: bool,
    owner_error: Option<u32>,
) -> io::Result<Stamp> {
    let Some(info) = query(file)? else {
        return super::observe_file(file, detailed, allocation, owners, owner_error);
    };
    let (length, directory) = shape(&info)?;
    let identity = handle_identity(file, length, directory)?;
    let mut properties = NodeProperties::default();
    if detailed {
        if allocation && !directory {
            properties.allocated = allocation_value(
                FILE_STANDARD_INFO {
                    AllocationSize: info.AllocationSize,
                    EndOfFile: info.EndOfFile,
                    NumberOfLinks: info.NumberOfLinks,
                    DeletePending: false,
                    Directory: false,
                },
                length,
            );
        }
        properties.accessed =
            file_time(info.LastAccessTime).map_or(MetadataValue::Unknown, |value| {
                MetadataValue::Known {
                    value,
                    accuracy: ValueAccuracy::Exact,
                }
            });
        if owners {
            properties.owner = if let Some(code) = owner_error {
                crate::owner::windows::missing(code)
            } else {
                profile::measure(profile::Stage::Owner, || {
                    crate::owner::windows::collect(file)
                })
            };
        }
    }
    Ok(Stamp {
        properties,
        identity,
        created: file_time(info.CreationTime),
        modified: file_time(info.LastWriteTime),
        length,
        directory,
        regular: !directory,
    })
}

fn query(file: &fs::File) -> io::Result<Option<FILE_STAT_INFORMATION>> {
    let mut info = FILE_STAT_INFORMATION::default();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: a live synchronous handle, matching class and fully writable
    // output structures. No pathname is reopened or permission widened.
    let result = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut status,
            (&mut info as *mut FILE_STAT_INFORMATION).cast(),
            size_of::<FILE_STAT_INFORMATION>() as u32,
            FileStatInformation,
        )
    };
    if unsupported_class(result) {
        return Ok(None);
    }
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            // SAFETY: pure status-code translation.
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    if result != 0 || status.Information < size_of::<FILE_STAT_INFORMATION>() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(info))
}

fn unsupported_class(status: i32) -> bool {
    matches!(
        status,
        STATUS_INVALID_INFO_CLASS
            | STATUS_INVALID_DEVICE_REQUEST
            | STATUS_INVALID_PARAMETER
            | STATUS_NOT_IMPLEMENTED
            | STATUS_NOT_SUPPORTED
    )
}

fn shape(info: &FILE_STAT_INFORMATION) -> io::Result<(u64, bool)> {
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(denied());
    }
    let length = u64::try_from(info.EndOfFile).map_err(|_| io::ErrorKind::InvalidData)?;
    Ok((length, info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0))
}

pub(super) fn file_time(raw: i64) -> Option<SystemTime> {
    // Match FILETIME's unsigned 100 ns ticks, including dates before Unix epoch.
    const UNIX_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = raw as u64;
    let delta = ticks.abs_diff(UNIX_TICKS);
    let duration = Duration::new(delta / 10_000_000, ((delta % 10_000_000) * 100) as u32);
    if ticks >= UNIX_TICKS {
        UNIX_EPOCH.checked_add(duration)
    } else {
        UNIX_EPOCH.checked_sub(duration)
    }
}

#[cfg(test)]
mod tests;
