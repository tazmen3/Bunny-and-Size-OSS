//! Detailed CSV/TSV inventory, independent of the bounded GUI inventory.
//! See docs/ENTRIES_EXPORT.md and ADR-0031 for the versioned wire contract.
#[cfg(test)]
mod faults;
mod row;
mod sort;
#[cfg(test)]
mod tests;

use crate::local::CollectFields;
use crate::{
    CancellationToken, Coverage, MetadataValue, Progress, ScanFailure, ScanOptions, ScanReport,
    ValueAccuracy,
};
pub use row::COLUMNS;
use row::{
    COVERAGE, DIRECT_DIRS, DIRECT_FILES, KIND, LOGICAL, RECURSIVE_DIRS, RECURSIVE_FILES, Record,
};
use sort::{Reader, Sorter};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Limits concern export structures, independently of scanner queues and RSS.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Per-sorter retained row payload; one additional row may exceed this.
    pub sort_memory_bytes: usize,
    /// Existing temporary parent. The spool itself is private and ephemeral.
    pub temporary_directory: Option<PathBuf>,
}
impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            sort_memory_bytes: 8 * 1024 * 1024,
            temporary_directory: None,
        }
    }
}

#[derive(Debug)]
pub enum ExportFailure {
    Scan(ScanFailure),
    Io(io::Error),
}
impl std::fmt::Display for ExportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scan(e) => e.fmt(f),
            Self::Io(e) => write!(f, "Échec de l'export détaillé : {e}"),
        }
    }
}
impl std::error::Error for ExportFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Scan(e) => e,
            Self::Io(e) => e,
        })
    }
}
impl From<io::Error> for ExportFailure {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<ScanFailure> for ExportFailure {
    fn from(e: ScanFailure) -> Self {
        Self::Scan(e)
    }
}

/// One native traversal with synchronous fallible spooling and pool backpressure.
/// Writes a typed end record only after all rows; absence of it means incomplete
/// transport. Caller must flush/publish its destination and inspect the coverage.
/// Cancellation stops acquisition, then finalizes the observations already consumed.
pub fn export_entries(
    root: &Path,
    options: &ScanOptions,
    export: &ExportOptions,
    cancellation: &CancellationToken,
    progress: impl FnMut(&Progress),
    writer: &mut impl Write,
    separator: u8,
) -> Result<ScanReport, ExportFailure> {
    if !b",\t".contains(&separator) || export.sort_memory_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Séparateur CSV/TSV et budget de tri positif requis",
        )
        .into());
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("sortsize-entries-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let spool = match &export.temporary_directory {
        Some(parent) => builder.tempdir_in(parent)?,
        None => builder.tempdir()?,
    };
    #[cfg(windows)]
    private_windows_directory(spool.path())?;
    let private_path = std::fs::canonicalize(spool.path())?;
    let mut sorted = Sorter::new(&private_path, "observations", export.sort_memory_bytes);
    let mut observed_root = None;
    let mut internal_spool_excluded = false;
    let report = crate::scan::scan_observed_fallible(
        root,
        options,
        cancellation,
        progress,
        CollectFields {
            dates: true,
            allocation: true,
            owner: true,
            private_exclusion: Some(private_path.clone().into()),
        },
        |path, observation| {
            let root = observed_root.get_or_insert_with(|| path.to_owned());
            if path == private_path {
                internal_spool_excluded = true;
                return Ok(());
            }
            sorted
                .push(Record::observe(root, path, observation).map_err(ScanFailure::ObserverIo)?)
                .map_err(ScanFailure::ObserverIo)
        },
    )?;
    let input = sorted.finish()?;
    let mut reader = Reader::open(&input)?;
    let mut output = Sorter::new(&private_path, "rows", export.sort_memory_bytes);
    aggregate(&mut reader, &mut output, report.coverage)?;
    drop(reader);
    #[cfg(test)]
    faults::check("remove")?;
    std::fs::remove_file(input)?;
    let rows = output.finish()?;
    let mut reader = Reader::open(&rows)?;
    row::write_cells(writer, COLUMNS, separator)?;
    let mut count = 0u64;
    let mut entries = 0u64;
    let mut errors = 0u64;
    while let Some(mut record) = reader.next_record()? {
        record.cells[COVERAGE] = coverage_name(if cancellation.is_cancelled() {
            Coverage::Cancelled
        } else {
            report.coverage
        })
        .into();
        row::write_cells(writer, &record.cells, separator)?;
        count = add(count, 1)?;
        entries = add(entries, u64::from(record.cells[2] == "entry"))?;
        errors = add(errors, u64::from(record.cells[2] == "error"))?;
    }
    // A signal during external sorting/serialization also produces exit 130.
    let mut report = report;
    if cancellation.is_cancelled() {
        report.coverage = Coverage::Cancelled;
    }
    let mut end = Record::empty("end");
    end.cells[COVERAGE] = coverage_name(report.coverage).into();
    end.cells[46] = count.to_string();
    end.cells[47] = entries.to_string();
    end.cells[48] = errors.to_string();
    end.cells[49] = "0".into();
    report.exclusions.sort();
    report.errors.sort_unstable_by(|a, b| {
        a.path
            .display
            .cmp(&b.path.display)
            .then_with(|| compare_raw(&a.path.raw, &b.path.raw))
            .then_with(|| a.operation.cmp(b.operation))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.message.cmp(&b.message))
    });
    // Full error details are separate records; the bounded summary is diagnostic.
    end.cells[50] = serde_json::to_string(&report).map_err(io::Error::other)?;
    end.cells[51] = internal_spool_excluded.to_string();
    // Drop readers before removing files (required on Windows).
    drop(reader);
    #[cfg(test)]
    faults::check("cleanup")?;
    spool.close()?;
    row::write_cells(writer, &end.cells, separator)?;
    Ok(report)
}

fn coverage_name(coverage: Coverage) -> &'static str {
    match coverage {
        Coverage::Complete => "complete",
        Coverage::Partial => "partial",
        Coverage::Cancelled => "cancelled",
    }
}

fn compare_raw(a: &Option<crate::RawPath>, b: &Option<crate::RawPath>) -> std::cmp::Ordering {
    use crate::RawPath;
    let tag = |raw: &Option<RawPath>| match raw {
        None => 0,
        Some(RawPath::UnixBytes(_)) => 1,
        Some(RawPath::WindowsUtf16(_)) => 2,
    };
    tag(a).cmp(&tag(b)).then_with(|| match (a, b) {
        (Some(RawPath::UnixBytes(a)), Some(RawPath::UnixBytes(b))) => a.cmp(b),
        (Some(RawPath::WindowsUtf16(a)), Some(RawPath::WindowsUtf16(b))) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    })
}
fn add(a: u64, b: u64) -> io::Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| io::Error::other("Débordement d'un cumul d'export"))
}

struct Frame {
    row: Record,
    bytes: u64,
    direct_files: u64,
    direct_dirs: u64,
    recursive_files: u64,
    recursive_dirs: u64,
}
impl Frame {
    fn finish(mut self) -> Record {
        let accuracy = if self.row.incomplete {
            ValueAccuracy::LowerBound
        } else {
            ValueAccuracy::Exact
        };
        for (column, value) in [
            (LOGICAL, self.bytes),
            (DIRECT_FILES, self.direct_files),
            (DIRECT_DIRS, self.direct_dirs),
            (RECURSIVE_FILES, self.recursive_files),
            (RECURSIVE_DIRS, self.recursive_dirs),
        ] {
            self.row
                .metric(column, &MetadataValue::Known { value, accuracy });
        }
        self.row
    }
}
fn close(stack: &mut Vec<Frame>, stack_bytes: &mut usize, output: &mut Sorter) -> io::Result<()> {
    let Some(frame) = stack.pop() else {
        return Ok(());
    };
    *stack_bytes -= frame.row.memory_bytes() + std::mem::size_of::<Frame>();
    if let Some(parent) = stack.last_mut() {
        parent.bytes = add(parent.bytes, frame.bytes)?;
        parent.recursive_files = add(parent.recursive_files, frame.recursive_files)?;
        parent.recursive_dirs = add(parent.recursive_dirs, add(frame.recursive_dirs, 1)?)?;
        parent.direct_dirs = add(parent.direct_dirs, 1)?;
        parent.row.incomplete |= frame.row.incomplete;
    }
    output.push(frame.finish())
}
fn aggregate(reader: &mut Reader, output: &mut Sorter, coverage: Coverage) -> io::Result<()> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut stack_bytes = 0usize;
    while let Some(mut row) = reader.next_record()? {
        while stack
            .last()
            .is_some_and(|f| !row.key.starts_with(&f.row.key))
        {
            close(&mut stack, &mut stack_bytes, output)?;
        }
        if row.cells[KIND] == "directory" {
            row.incomplete |= coverage == Coverage::Cancelled;
            // Hard bounds independent of N, including adversarial deep paths.
            // Each retained row is charged once on push and released on pop.
            // Include the new frame too; never walk all ancestors for admission.
            let cost = row.memory_bytes() + std::mem::size_of::<Frame>();
            if stack.len() >= 4096 || stack_bytes.saturating_add(cost) > 8 * 1024 * 1024 {
                return Err(io::Error::other(
                    "Pile d'agrégation de l'export saturée (4096 dossiers / 8 Mio)",
                ));
            }
            stack_bytes += cost;
            stack.push(Frame {
                row,
                bytes: 0,
                direct_files: 0,
                direct_dirs: 0,
                recursive_files: 0,
                recursive_dirs: 0,
            });
        } else {
            if let Some(parent) = stack.last_mut() {
                if row.cells[KIND] == "file" {
                    let bytes: u64 = row.cells[LOGICAL].parse().map_err(io::Error::other)?;
                    parent.bytes = add(parent.bytes, bytes)?;
                    parent.direct_files = add(parent.direct_files, 1)?;
                    parent.recursive_files = add(parent.recursive_files, 1)?;
                }
                if row.cells[2] == "error" {
                    parent.row.incomplete = true;
                }
            }
            output.push(row)?;
        }
    }
    while !stack.is_empty() {
        close(&mut stack, &mut stack_bytes, output)?;
    }
    Ok(())
}

#[cfg(windows)]
fn private_windows_directory(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION,
        },
    };
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: terminated UTF-16 strings, valid outputs. The empty directory has
    // no sensitive data until this protected owner-only DACL has been installed.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = ptr::null_mut();
        let ok = GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted);
        let code = if ok == 0 || present == 0 {
            Err(io::Error::last_os_error())
        } else {
            let code = SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null_mut(),
            );
            if code == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(code as i32))
            }
        };
        LocalFree(descriptor);
        code
    }
}
