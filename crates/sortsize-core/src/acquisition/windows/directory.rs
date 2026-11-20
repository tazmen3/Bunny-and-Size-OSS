use super::{
    super::*,
    metadata::{file_time, observe_file, observe_with},
    metadata_open,
};
use std::{
    ffi::OsString,
    mem::{offset_of, size_of, size_of_val},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle},
    },
    ptr::{null, null_mut},
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
            NtOpenFile,
        },
    },
    Win32::{
        Foundation::{
            ERROR_NO_MORE_FILES, OBJ_DONT_REPARSE, RtlNtStatusToDosError, UNICODE_STRING,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
};

const BUFFER_SIZE: usize = 16 * 1024;
const NAME_OFFSET: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);

#[repr(align(8))]
struct Buffer([u8; BUFFER_SIZE]);

pub(in crate::acquisition) struct Directory {
    path: PathBuf,
    before: Stamp,
    root_boundary: Boundary,
    root_stamp: Stamp,
    handle: fs::File,
    buffer: Box<Buffer>,
    offset: Option<usize>,
    exhausted: bool,
    serial: u64,
    stats: NtfsListingStats,
    observation: ScanObservation,
}

pub(in crate::acquisition) struct Entry {
    id: [u8; 16],
    attrs: u32,
    length: i64,
    allocation: i64,
    created: i64,
    modified: i64,
    accessed: i64,
}

pub(in crate::acquisition) fn is_ntfs(root: &Path) -> bool {
    let Ok(handle) = metadata_open(root, FILE_READ_ATTRIBUTES) else {
        return false;
    };
    let mut name = [0u16; 16];
    // SAFETY: live handle and writable filesystem-name buffer.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    ok != 0
        && name[..5]
            .iter()
            .copied()
            .eq("NTFS".encode_utf16().chain([0]))
}

impl Directory {
    pub(in crate::acquisition) fn open(
        guard: &Guard,
        path: &Path,
        before: &Stamp,
        admission: Option<super::DirectoryAdmission>,
    ) -> io::Result<Self> {
        let Some(LocalFileId::Windows { volume_serial, .. }) = before.identity else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        let (handle, on_handle) = profile::measure(profile::Stage::Cursor, || {
            let handle = if let Some(admission) = &admission {
                admission.open_listing()?
            } else {
                metadata_open(path, FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY)?
            };
            let on_handle = observe_file(&handle, false, false, false, None)?;
            if !on_handle.directory || before.attachment(&on_handle) != Attachment::Compatible {
                return Err(denied());
            }
            Ok((handle, on_handle))
        })?;
        let mut reader = Self {
            path: path.into(),
            before: on_handle,
            root_boundary: guard.boundary.clone(),
            root_stamp: guard.root_stamp.clone(),
            handle,
            buffer: Box::new(Buffer([0; BUFFER_SIZE])),
            offset: None,
            exhausted: false,
            serial: volume_serial,
            stats: NtfsListingStats::default(),
            observation: guard.scan_observation(),
        };
        // Probe the information class before any result escapes, so unsupported
        // drivers, permissions or filters can fall back to the ordinary cursor.
        reader.load()?;
        Ok(reader)
    }

    pub(in crate::acquisition) fn validate(&self, stamp: &Stamp) -> io::Result<()> {
        if self.before.attachment(stamp) != Attachment::Compatible
            || self.before.modified != stamp.modified
        {
            return Err(io::Error::other("directory changed during listing"));
        }
        Ok(())
    }

    pub(in crate::acquisition) fn stats(&self) -> NtfsListingStats {
        self.stats
    }

    pub(in crate::acquisition) fn observation(&self) -> ScanObservation {
        self.observation
    }

    fn load(&mut self) -> io::Result<()> {
        self.buffer.0.fill(0);
        self.stats.batch_calls += 1;
        // SAFETY: correct class, a live directory handle, and a writable
        // 8-byte aligned buffer of the given size.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                self.handle.as_raw_handle(),
                FileIdExtdDirectoryInfo,
                self.buffer.0.as_mut_ptr().cast(),
                BUFFER_SIZE as u32,
            )
        };
        if ok != 0 {
            self.offset = Some(0);
        } else {
            self.exhausted = true;
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                return Err(err);
            }
        }
        Ok(())
    }

    pub(in crate::acquisition) fn next_entry(&mut self) -> Option<io::Result<ScanEntry>> {
        while !self.exhausted {
            if self.offset.is_none()
                && let Err(error) = self.load()
            {
                return Some(Err(error));
            }
            let offset = self.offset?;
            let Some((name, native, next)) = row(&self.buffer.0[offset..]) else {
                self.exhausted = true;
                return Some(Err(io::ErrorKind::InvalidData.into()));
            };
            self.offset = next.map(|delta| offset + delta);
            if name == "." || name == ".." {
                continue;
            }
            return Some(Ok(ScanEntry {
                path: self.path.join(&name),
                name,
                native: Some(native),
            }));
        }
        None
    }

    pub(in crate::acquisition) fn observe(
        &mut self,
        guard: &Guard,
        path: &Path,
        entry: &Entry,
        allocation: bool,
        owners: bool,
    ) -> Option<Stamp> {
        // Reparse records and ambiguous pathname syntax always get the full
        // historical guard, which declines links and keeps error reporting.
        if !guard.ntfs_directory
            || self.root_boundary != guard.boundary
            || self.root_stamp.attachment(&guard.root_stamp) != Attachment::Compatible
            || !self.path.starts_with(guard.root())
            || path.parent() != Some(self.path.as_path())
            || entry.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return None;
        }
        let name: Vec<u16> = path.file_name()?.encode_wide().collect();
        if matches!(name.last(), Some(32 | 46)) {
            return None;
        }
        if self.observation == ScanObservation::DirectoryListing
            && guard.scan_observation() == ScanObservation::DirectoryListing
            && !owners
        {
            return entry.listed_stamp(self.serial, allocation);
        }
        let stamp = observe_with(true, allocation, owners, |rights| {
            self.stats.relative_opens += 1;
            open_relative(&self.handle, &name, rights)
        })
        .ok()?;
        // A listing is not proof of the rights or current size of a child.
        // A file can change in place without updating the parent mtime, and
        // a second hardlink can lag in the directory index.
        if stamp.identity
            != Some(LocalFileId::Windows {
                volume_serial: self.serial,
                file_id: entry.id,
            })
            || stamp.directory != (entry.attrs & FILE_ATTRIBUTE_DIRECTORY != 0)
        {
            self.stats.stale_rows += 1;
            return None;
        }
        if stamp.regular
            && (u64::try_from(entry.length) != Ok(stamp.length)
                || matches!(stamp.properties.allocated, crate::MetadataValue::Known { value, .. }
                    if u64::try_from(entry.allocation) != Ok(value)))
        {
            self.stats.stale_rows += 1;
        }
        Some(stamp)
    }
}

impl Entry {
    fn listed_stamp(&self, serial: u64, allocation: bool) -> Option<Stamp> {
        use crate::{MetadataValue as M, ValueAccuracy};
        if self.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 || self.id == [0; 16] {
            return None;
        }
        let directory = self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let length = u64::try_from(self.length).ok()?;
        // These values describe the index row, not a fresh child-handle stat.
        // Sparse/compressed/offline streams need a separate measurement; never
        // mistake their reservation or unavailable allocation for disk usage.
        let allocated = if !allocation || directory {
            M::NotCollected
        } else if self.attrs
            & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_OFFLINE)
            != 0
        {
            M::Unknown
        } else {
            u64::try_from(self.allocation).map_or(M::Unknown, |value| M::Known {
                value,
                accuracy: ValueAccuracy::Exact,
            })
        };
        Some(Stamp {
            properties: NodeProperties {
                allocated,
                accessed: file_time(self.accessed).map_or(M::Unknown, |value| M::Known {
                    value,
                    accuracy: ValueAccuracy::Exact,
                }),
                owner: crate::EntryOwner::not_collected(),
            },
            identity: Some(LocalFileId::Windows {
                volume_serial: serial,
                file_id: self.id,
            }),
            created: file_time(self.created),
            modified: file_time(self.modified),
            length,
            directory,
            regular: !directory,
        })
    }
}

fn row(bytes: &[u8]) -> Option<(OsString, Entry, Option<usize>)> {
    // Read fields by offset instead of treating variable-sized records as
    // Rust structs. In particular the last row may end before struct padding.
    if bytes.len() < NAME_OFFSET {
        return None;
    }
    fn read<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
        bytes[offset..offset + N].try_into().unwrap()
    }
    let name_bytes = u32::from_le_bytes(read(
        bytes,
        offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength),
    )) as usize;
    let end = NAME_OFFSET.checked_add(name_bytes)?;
    let next = u32::from_le_bytes(read(bytes, 0)) as usize;
    if name_bytes == 0
        || name_bytes > 510
        || !name_bytes.is_multiple_of(2)
        || (next != 0
            && (!next.is_multiple_of(8) || next < end || next > bytes.len() - NAME_OFFSET))
    {
        return None;
    }
    let (name, _) = bytes.get(NAME_OFFSET..end)?.as_chunks::<2>();
    let name: Vec<u16> = name.iter().copied().map(u16::from_le_bytes).collect();
    if name.iter().any(|&c| matches!(c, 0 | 47 | 58 | 92)) {
        return None;
    }
    let entry = Entry {
        id: read(bytes, offset_of!(FILE_ID_EXTD_DIR_INFO, FileId)),
        attrs: u32::from_le_bytes(read(
            bytes,
            offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes),
        )),
        length: i64::from_le_bytes(read(bytes, offset_of!(FILE_ID_EXTD_DIR_INFO, EndOfFile))),
        allocation: i64::from_le_bytes(read(
            bytes,
            offset_of!(FILE_ID_EXTD_DIR_INFO, AllocationSize),
        )),
        created: i64::from_le_bytes(read(bytes, offset_of!(FILE_ID_EXTD_DIR_INFO, CreationTime))),
        modified: i64::from_le_bytes(read(
            bytes,
            offset_of!(FILE_ID_EXTD_DIR_INFO, LastWriteTime),
        )),
        accessed: i64::from_le_bytes(read(
            bytes,
            offset_of!(FILE_ID_EXTD_DIR_INFO, LastAccessTime),
        )),
    };
    Some((
        OsString::from_wide(&name),
        entry,
        (next != 0).then_some(next),
    ))
}

pub(super) fn open_relative(parent: &fs::File, name: &[u16], rights: u32) -> io::Result<fs::File> {
    let bytes = u16::try_from(size_of_val(name))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    };
    let object = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_DONT_REPARSE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut output = null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the relative name has validated components, both input
    // structures live across the call and a successful synchronous open
    // returns an owned handle. Same attribute and backup intent as metadata_open.
    let result = unsafe {
        NtOpenFile(
            &mut output,
            rights | SYNCHRONIZE,
            &object,
            &mut status,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT | FILE_SYNCHRONOUS_IO_NONALERT,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    if output.is_null() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(unsafe { fs::File::from_raw_handle(output) })
}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod tests;
