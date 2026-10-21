use crate::local::Observation;
use crate::{
    EntryAllocation, EntryDates, EntryOwner, MetadataValue, NativeOwnerId, NativePath, RawPath,
    ValueAccuracy,
};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const COLUMNS: &[&str] = &[
    "export_schema_version",
    "export_scope",
    "record_type",
    "path_display",
    "path_raw_encoding",
    "path_raw_units_json",
    "name_display",
    "name_raw_encoding",
    "name_raw_units_json",
    "kind",
    "depth",
    "logical_bytes",
    "logical_availability",
    "logical_accuracy",
    "allocated_bytes",
    "allocated_availability",
    "allocated_accuracy",
    "direct_files",
    "direct_files_availability",
    "direct_files_accuracy",
    "direct_directories",
    "direct_directories_availability",
    "direct_directories_accuracy",
    "recursive_files",
    "recursive_files_availability",
    "recursive_files_accuracy",
    "recursive_directories",
    "recursive_directories_availability",
    "recursive_directories_accuracy",
    "modified_unix_ns",
    "modified_availability",
    "modified_accuracy",
    "accessed_unix_ns",
    "accessed_availability",
    "accessed_accuracy",
    "time_resolution",
    "owner_kind",
    "owner_id",
    "owner_availability",
    "owner_accuracy",
    "depth_limited",
    "resource_limited",
    "coverage",
    "operation",
    "error_kind",
    "error_message",
    "records",
    "entry_records",
    "error_records",
    "observations_omitted",
    "report_json",
    "internal_spool_excluded",
];
pub(super) const KIND: usize = 9;
pub(super) const LOGICAL: usize = 11;
pub(super) const ALLOCATED: usize = 14;
pub(super) const DIRECT_FILES: usize = 17;
pub(super) const DIRECT_DIRS: usize = 20;
pub(super) const RECURSIVE_FILES: usize = 23;
pub(super) const RECURSIVE_DIRS: usize = 26;
pub(super) const COVERAGE: usize = 42;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct Record {
    // Relative native components, each terminated with '!'. Hex units preserve
    // byte/u16 ordering; '!' sorts before every hex digit. Subtrees are contiguous.
    pub key: String,
    pub rank: u8,
    pub cells: Vec<String>,
    pub incomplete: bool,
}
impl Record {
    pub fn empty(record_type: &str) -> Self {
        let mut cells = vec![String::new(); COLUMNS.len()];
        cells[0] = "1".into();
        cells[1] = "entries".into();
        cells[2] = record_type.into();
        Self {
            key: String::new(),
            rank: 0,
            cells,
            incomplete: false,
        }
    }
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.key.capacity()
            + self.cells.capacity() * std::mem::size_of::<String>()
            + self.cells.iter().map(String::capacity).sum::<usize>()
    }
    pub fn observe(root: &Path, path: &Path, observation: &Observation) -> io::Result<Self> {
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        let mut row = Self::empty("entry");
        row.key = native_key(relative);
        row.cells[10] = relative.components().count().to_string();
        path_cells(&mut row.cells[3..6], path)?;
        path_cells(
            &mut row.cells[6..9],
            Path::new(path.file_name().unwrap_or(path.as_os_str())),
        )?;
        for start in [
            LOGICAL,
            ALLOCATED,
            DIRECT_FILES,
            DIRECT_DIRS,
            RECURSIVE_FILES,
            RECURSIVE_DIRS,
            29,
            32,
        ] {
            row.cells[start + 1] = "not_collected".into();
        }
        row.cells[38] = "not_collected".into();
        match observation {
            Observation::File {
                bytes,
                dates,
                owner,
                allocation,
                ..
            } => {
                row.rank = 1;
                row.cells[KIND] = "file".into();
                row.metric(
                    LOGICAL,
                    &MetadataValue::Known {
                        value: *bytes,
                        accuracy: ValueAccuracy::Exact,
                    },
                );
                row.metric(
                    ALLOCATED,
                    &allocation
                        .unwrap_or_else(EntryAllocation::not_collected)
                        .allocated_bytes,
                );
                row.metadata(*dates, owner.as_ref().map(crate::owner::retained));
            }
            Observation::Directory {
                depth_limited,
                resource_limited,
                dates,
                owner,
            } => {
                row.cells[KIND] = "directory".into();
                row.incomplete = *depth_limited || *resource_limited;
                row.cells[40] = depth_limited.to_string();
                row.cells[41] = resource_limited.to_string();
                row.metadata(*dates, owner.as_ref().map(crate::owner::retained));
            }
            Observation::Error(issue) => {
                row.rank = 2;
                row.cells[2] = "error".into();
                row.incomplete = true;
                row.cells[43] = issue.operation.into();
                row.cells[44] = issue.kind.clone();
                row.cells[45] = issue.message.clone();
            }
            Observation::Symlink => {
                row.rank = 3;
                row.cells[2] = "symlink_skipped".into();
            }
            Observation::Special => {
                row.rank = 4;
                row.cells[2] = "special_skipped".into();
            }
            Observation::Excluded => {
                row.rank = 5;
                row.cells[2] = "excluded".into();
            }
        }
        Ok(row)
    }
    pub fn metric<T: ToString>(&mut self, start: usize, value: &MetadataValue<T>) {
        let (availability, accuracy) = state(value);
        self.cells[start + 1] = availability.into();
        self.cells[start + 2] = accuracy.into();
        if let MetadataValue::Known { value, .. } = value {
            self.cells[start] = value.to_string();
        }
    }
    fn metadata(&mut self, dates: Option<EntryDates>, owner: Option<EntryOwner>) {
        let dates = dates.unwrap_or_else(EntryDates::not_collected);
        self.date(29, &dates.modified);
        self.date(32, &dates.accessed);
        self.cells[35] = "native_resolution_unknown".into();
        let owner = owner.unwrap_or_else(EntryOwner::not_collected);
        let (availability, accuracy) = state(&owner.id);
        self.cells[38] = availability.into();
        self.cells[39] = accuracy.into();
        if let MetadataValue::Known { value, .. } = owner.id {
            match value {
                NativeOwnerId::UnixUid(uid) => {
                    self.cells[36] = "unix_uid".into();
                    self.cells[37] = uid.to_string();
                }
                #[cfg(windows)]
                NativeOwnerId::WindowsSid(sid) => {
                    self.cells[36] = "windows_sid_hex".into();
                    self.cells[37] = sid.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
                }
            }
        }
    }
    fn date(&mut self, start: usize, value: &MetadataValue<SystemTime>) {
        let (availability, accuracy) = state(value);
        self.cells[start + 1] = availability.into();
        self.cells[start + 2] = accuracy.into();
        if let MetadataValue::Known { value, .. } = value {
            self.cells[start] = match value.duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_nanos().to_string(),
                Err(error) => format!("-{}", error.duration().as_nanos()),
            };
        }
    }
}

fn state<T>(value: &MetadataValue<T>) -> (&'static str, &'static str) {
    match value {
        MetadataValue::Known { accuracy, .. } => (
            "known",
            match accuracy {
                ValueAccuracy::Exact => "exact",
                ValueAccuracy::LowerBound => "lower_bound",
            },
        ),
        MetadataValue::Unknown => ("unknown", ""),
        MetadataValue::Unsupported => ("unsupported", ""),
        MetadataValue::AccessDenied => ("access_denied", ""),
        MetadataValue::NotCollected => ("not_collected", ""),
    }
}
fn path_cells(cells: &mut [String], path: &Path) -> io::Result<()> {
    let native = NativePath::from(path);
    cells[0] = native.display;
    match native.raw {
        Some(RawPath::UnixBytes(units)) => {
            cells[1] = "unix_bytes".into();
            cells[2] = serde_json::to_string(&units).map_err(io::Error::other)?;
        }
        Some(RawPath::WindowsUtf16(units)) => {
            cells[1] = "windows_utf16".into();
            cells[2] = serde_json::to_string(&units).map_err(io::Error::other)?;
        }
        None => {}
    }
    Ok(())
}
pub(super) fn native_key(path: &Path) -> String {
    use std::fmt::Write;
    let mut key = String::new();
    for part in path.components() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            for unit in part.as_os_str().as_bytes() {
                let _ = write!(key, "{unit:02x}");
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            for unit in part.as_os_str().encode_wide() {
                let _ = write!(key, "{unit:04x}");
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            for unit in part.as_os_str().as_encoded_bytes() {
                let _ = write!(key, "{unit:02x}");
            }
        }
        key.push('!');
    }
    key
}

pub(super) fn write_cells(
    writer: &mut impl Write,
    cells: &[impl AsRef<str>],
    separator: u8,
) -> io::Result<()> {
    for (index, cell) in cells.iter().enumerate() {
        if index != 0 {
            writer.write_all(&[separator])?;
        }
        let value = cell.as_ref().as_bytes();
        if value
            .iter()
            .any(|b| [separator, b'"', b'\r', b'\n'].contains(b))
        {
            writer.write_all(b"\"")?;
            for part in value.split_inclusive(|b| *b == b'"') {
                writer.write_all(part)?;
                if part.last() == Some(&b'"') {
                    writer.write_all(b"\"")?;
                }
            }
            writer.write_all(b"\"")?;
        } else {
            writer.write_all(value)?;
        }
    }
    writer.write_all(b"\n")
}
