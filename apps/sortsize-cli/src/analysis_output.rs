//! Separate additive analysis contract; legacy scan exports remain unchanged.
use clap::{Args, ValueEnum};
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, InventoryKind, InventoryLimits, NativePath, ScanOptions,
    analysis::{self, AnalysisSnapshot, SearchQuery},
    scan_inventory_with_dates,
};
use std::{
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};
#[derive(Args)]
pub(super) struct Arguments {
    pub path: PathBuf,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub path_contains: Option<String>,
    #[arg(long)]
    pub extension: Option<String>,
    #[arg(long)]
    pub glob: Option<String>,
    #[arg(long, value_enum)]
    pub kind: Option<Kind>,
    #[arg(long)]
    pub min_bytes: Option<u64>,
    #[arg(long)]
    pub max_bytes: Option<u64>,
    /// Inclusive UTC seconds since 1970; unavailable dates do not match.
    #[arg(long)]
    pub modified_after: Option<u64>,
    #[arg(long)]
    pub modified_before: Option<u64>,
    #[arg(long)]
    pub case_sensitive: bool,
    /// Explicitly read candidate content for exact duplicate groups; no deletion.
    #[arg(long)]
    pub duplicates: bool,
    #[arg(long, default_value_t = 1_073_741_824)]
    pub max_read_bytes: u64,
    #[arg(long, default_value_t = 536_870_912)]
    pub max_file_bytes: u64,
    #[arg(long, default_value_t = 50_000)]
    pub max_results: usize,
    #[arg(long, default_value_t = 50_000)]
    pub max_entries: usize,
    #[arg(long, default_value_t = 1)]
    pub workers: usize,
    #[arg(long)]
    pub exclude: Vec<String>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: Format,
    /// New local destination; never overwrite an existing file.
    #[arg(long)]
    pub output: Option<PathBuf>,
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Kind {
    File,
    Directory,
    Symlink,
    Special,
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Format {
    Json,
    Csv,
    Tsv,
}

pub(super) fn run(a: Arguments) -> Result<u8, Box<dyn std::error::Error>> {
    if !(1..=analysis::MAX_ANALYSIS_ENTRIES).contains(&a.max_entries)
        || !(1..=analysis::MAX_ANALYSIS_ENTRIES).contains(&a.max_results)
    {
        return Err(io::Error::other("Limits must be between 1 and 50000").into());
    }
    if a.duplicates && a.max_read_bytes == 0 {
        return Err(io::Error::other("Content read budget must be greater than zero").into());
    }
    let time = |v: Option<u64>| {
        v.map(|s| {
            UNIX_EPOCH
                .checked_add(Duration::from_secs(s))
                .ok_or_else(|| io::Error::other("Timestamp out of range"))
        })
        .transpose()
    };
    let query = SearchQuery {
        name: a.name,
        path: a.path_contains,
        extension: a.extension,
        glob: a.glob,
        kind: a.kind.map(|k| match k {
            Kind::File => InventoryKind::File,
            Kind::Directory => InventoryKind::Directory,
            Kind::Symlink => InventoryKind::Symlink,
            Kind::Special => InventoryKind::Special,
        }),
        min_bytes: a.min_bytes,
        max_bytes: a.max_bytes,
        modified_after: time(a.modified_after)?,
        modified_before: time(a.modified_before)?,
        case_sensitive: a.case_sensitive,
    };
    // Validate before opening/scanning a supplied location.
    analysis::search(
        &AnalysisSnapshot {
            entries: vec![],
            scan_coverage: sortsize_core::Coverage::Complete,
            entries_omitted: 0,
        },
        &query,
        a.max_results,
        &CancellationToken::default(),
    )
    .map_err(io::Error::other)?;
    let staged = a
        .output
        .as_deref()
        .map(super::entries_output::StagedOutput::new)
        .transpose()?;
    let cancel = CancellationToken::default();
    let signal = cancel.clone();
    let signals = Arc::new(AtomicU64::new(0));
    let received = signals.clone();
    ctrlc::set_handler(move || {
        signal.cancel();
        received.fetch_add(1, Ordering::Relaxed);
    })?;
    let scan = scan_inventory_with_dates(
        &a.path,
        &ScanOptions {
            exclusions: a.exclude,
            concurrency: ConcurrencyOptions {
                workers: a.workers,
                ..Default::default()
            },
            ..Default::default()
        },
        &cancel,
        InventoryLimits {
            max_entries: a.max_entries,
            ..Default::default()
        },
        |_| {},
    )?;
    let snapshot =
        AnalysisSnapshot::from_inventory(&scan.inventory, &scan.dates).map_err(io::Error::other)?;
    let result =
        analysis::search(&snapshot, &query, a.max_results, &cancel).map_err(io::Error::other)?;
    let duplicates = if a.duplicates {
        Some(
            sortsize_core::duplicates::find(
                &snapshot,
                &result,
                sortsize_core::duplicates::DuplicateOptions {
                    max_bytes_read: a.max_read_bytes,
                    max_file_bytes: a.max_file_bytes,
                    ..Default::default()
                },
                &cancel,
                |_| {},
            )
            .map_err(io::Error::other)?,
        )
    } else {
        None
    };
    // A cancelled acquisition may still export its explicitly marked partial
    // report. A later signal cancels serialization/publication instead.
    let export_epoch = signals.load(Ordering::Relaxed);
    if cancel.is_cancelled()
        && !result.cancelled
        && !duplicates.as_ref().is_some_and(|d| d.cancelled)
    {
        return Ok(130);
    }
    let entries:Vec<_>=result.indices.iter().map(|&i|{let e=&snapshot.entries[i];serde_json::json!({"path":NativePath::from(e.path.as_path()),"kind":match e.kind {InventoryKind::File=>"file",InventoryKind::Directory=>"directory",InventoryKind::Symlink=>"symlink",InventoryKind::Special=>"special"},"logical_bytes":e.logical_bytes})}).collect();
    let document = serde_json::json!({"analysis_schema_version":1,"search":result,"entries":entries,"duplicates":duplicates});
    let write = |w: &mut dyn Write| -> io::Result<()> {
        match a.format {
            Format::Json => {
                serde_json::to_writer_pretty(&mut *w, &document)?;
                w.write_all(b"\n")?;
            }
            Format::Csv | Format::Tsv => {
                let separator = if matches!(a.format, Format::Csv) {
                    ','
                } else {
                    '\t'
                };
                row(
                    w,
                    separator,
                    &["analysis_schema_version", "record_type", "data_json"],
                )?;
                for entry in &entries {
                    row(w, separator, &["1", "entry", &entry.to_string()])?;
                }
                if let Some(duplicates) = &duplicates {
                    row(
                        w,
                        separator,
                        &["1", "duplicates", &serde_json::to_string(duplicates)?],
                    )?;
                }
                row(
                    w,
                    separator,
                    &["1", "end", &serde_json::to_string(&result)?],
                )?;
            }
        };
        w.flush()
    };
    let write_checked = |w: &mut dyn Write| {
        let mut guarded = ExportWriter {
            inner: BufWriter::with_capacity(64 * 1024, w),
            signals: &signals,
            epoch: export_epoch,
        };
        let result = write(&mut guarded);
        // Avoid BufWriter's implicit drop flush after cancellation/write failure.
        let _ = guarded.inner.into_parts();
        result
    };
    let publication = if let Some(mut output) = staged {
        write_checked(&mut output)
            .and_then(|()| output.publish_checked(|| check_export(&signals, export_epoch)))
    } else {
        write_checked(&mut io::stdout().lock())
    };
    if signals.load(Ordering::Relaxed) != export_epoch {
        return Ok(130);
    }
    publication?;
    Ok(
        if result.cancelled || duplicates.as_ref().is_some_and(|d| d.cancelled) {
            130
        } else if !result.complete
            || result.omitted_results > 0
            || duplicates.as_ref().is_some_and(|d| !d.complete)
        {
            2
        } else {
            0
        },
    )
}
fn row(w: &mut dyn Write, separator: char, fields: &[&str]) -> io::Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            write!(w, "{separator}")?;
        }
        // All dynamic text is a JSON object, never a formula-leading raw path.
        write!(w, "\"{}\"", field.replace('"', "\"\""))?;
    }
    w.write_all(b"\r\n")
}

fn check_export(signals: &AtomicU64, epoch: u64) -> io::Result<()> {
    if signals.load(Ordering::Relaxed) != epoch {
        // Interrupted would be retried indefinitely by Write::write_all.
        Err(io::Error::other("Export cancelled"))
    } else {
        Ok(())
    }
}
struct ExportWriter<'a, W> {
    inner: W,
    signals: &'a AtomicU64,
    epoch: u64,
}
impl<W: Write> Write for ExportWriter<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        check_export(self.signals, self.epoch)?;
        self.inner.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        check_export(self.signals, self.epoch)?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_during_serialization_does_not_publish_or_leave_staging() {
        let f = tempfile::tempdir().unwrap();
        let destination = f.path().join("report.json");
        let mut stage = super::super::entries_output::StagedOutput::new(&destination).unwrap();
        let signals = AtomicU64::new(0);
        let mut writer = ExportWriter {
            inner: &mut stage,
            signals: &signals,
            epoch: 0,
        };
        writer.write_all(b"partial").unwrap();
        signals.store(1, Ordering::Relaxed);
        assert!(writer.write_all(b"more").is_err());
        assert!(writer.flush().is_err());
        assert!(stage.publish_checked(|| check_export(&signals, 0)).is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(f.path()).unwrap().count(), 0);
    }
    #[test]
    fn partial_acquisition_export_allows_only_the_observed_signal_epoch() {
        let signals = AtomicU64::new(1);
        let mut writer = ExportWriter {
            inner: Vec::new(),
            signals: &signals,
            epoch: 1,
        };
        writer.write_all(b"marked partial report").unwrap();
        signals.store(2, Ordering::Relaxed);
        assert!(writer.write_all(b"later").is_err());
    }
}
