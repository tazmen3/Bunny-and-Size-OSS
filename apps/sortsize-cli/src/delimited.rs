//! Versioned summary projection of the existing report; no inventory or I/O scan.
use sortsize_core::{Coverage, NativePath, RawPath, ScanReport};
use std::cmp::Ordering;
use std::io::{self, Write};

const HEADER: &[&str] = &[
    "export_schema_version",
    "export_scope",
    "schema_version",
    "provider",
    "root_display",
    "root_raw_encoding",
    "root_raw_units_json",
    "started_at_unix_ms",
    "duration_ms",
    "coverage",
    "files",
    "directories",
    "logical_bytes",
    "logical_size_basis",
    "allocated_bytes",
    "potentially_reclaimable_bytes",
    "symlinks_skipped",
    "special_entries_skipped",
    "excluded_entries",
    "depth_limited_directories",
    "resource_limited_directories",
    "max_pending_directories",
    "max_pending_path_bytes",
    "peak_pending_directories",
    "peak_pending_path_bytes",
    "peak_open_directories",
    "worker_threads",
    "peak_buffered_observations",
    "peak_buffered_path_bytes",
    "workers",
    "max_buffered_observations",
    "max_buffered_path_bytes",
    "hardlinked_file_paths",
    "errors_count",
    "errors_omitted",
    "errors_json",
    "follow_symlinks",
    "exclusions_json",
    "max_depth",
    "mount_policy",
];

// Sort retained values only. This cannot make a limited scan's retained subset,
// OS messages or traversal high-water marks reproducible across executions.
pub(crate) fn canonicalize(report: &mut ScanReport) {
    report.exclusions.sort_unstable();
    report.errors.sort_unstable_by(|a, b| {
        compare_paths(&a.path, &b.path)
            .then_with(|| a.operation.cmp(b.operation))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.message.cmp(&b.message))
    });
}

fn compare_paths(a: &NativePath, b: &NativePath) -> Ordering {
    a.display.cmp(&b.display).then_with(|| {
        // Display is not an identity: distinct invalid native names can have
        // identical lossy labels. Compare the full raw units to break ties.
        let tag = |p: &NativePath| match p.raw {
            None => 0,
            Some(RawPath::UnixBytes(_)) => 1,
            Some(RawPath::WindowsUtf16(_)) => 2,
        };
        tag(a).cmp(&tag(b)).then_with(|| match (&a.raw, &b.raw) {
            (Some(RawPath::UnixBytes(a)), Some(RawPath::UnixBytes(b))) => a.cmp(b),
            (Some(RawPath::WindowsUtf16(a)), Some(RawPath::WindowsUtf16(b))) => a.cmp(b),
            _ => Ordering::Equal,
        })
    })
}

pub(crate) fn write_report(
    writer: &mut impl Write,
    report: &ScanReport,
    delimiter: u8,
) -> io::Result<()> {
    let (encoding, units) = match &report.root.raw {
        None => ("", String::new()),
        Some(RawPath::UnixBytes(units)) => (
            "unix_bytes",
            serde_json::to_string(units).map_err(io::Error::other)?,
        ),
        Some(RawPath::WindowsUtf16(units)) => (
            "windows_utf16",
            serde_json::to_string(units).map_err(io::Error::other)?,
        ),
    };
    let optional = |value: Option<u64>| value.map(|n| n.to_string()).unwrap_or_default();
    let fields = [
        "1".into(),
        "summary".into(),
        report.schema_version.to_string(),
        report.provider.into(),
        report.root.display.clone(),
        encoding.into(),
        units,
        report.started_at_unix_ms.to_string(),
        report.duration_ms.to_string(),
        match report.coverage {
            Coverage::Complete => "complete",
            Coverage::Partial => "partial",
            Coverage::Cancelled => "cancelled",
        }
        .into(),
        report.files.to_string(),
        report.directories.to_string(),
        report.logical_bytes.to_string(),
        report.logical_size_basis.into(),
        optional(report.allocated_bytes),
        optional(report.potentially_reclaimable_bytes),
        report.symlinks_skipped.to_string(),
        report.special_entries_skipped.to_string(),
        report.excluded_entries.to_string(),
        report.depth_limited_directories.to_string(),
        report.resource_limited_directories.to_string(),
        report.traversal_limits.max_pending_directories.to_string(),
        report.traversal_limits.max_pending_path_bytes.to_string(),
        report.traversal_stats.peak_pending_directories.to_string(),
        report.traversal_stats.peak_pending_path_bytes.to_string(),
        report.traversal_stats.peak_open_directories.to_string(),
        report.traversal_stats.worker_threads.to_string(),
        report
            .traversal_stats
            .peak_buffered_observations
            .to_string(),
        report.traversal_stats.peak_buffered_path_bytes.to_string(),
        report.concurrency.workers.to_string(),
        report.concurrency.max_buffered_observations.to_string(),
        report.concurrency.max_buffered_path_bytes.to_string(),
        optional(report.hardlinked_file_paths),
        report.errors_count.to_string(),
        report.errors_omitted.to_string(),
        serde_json::to_string(&report.errors).map_err(io::Error::other)?,
        report.follow_symlinks.to_string(),
        serde_json::to_string(&report.exclusions).map_err(io::Error::other)?,
        report.max_depth.map(|n| n.to_string()).unwrap_or_default(),
        report.mount_policy.into(),
    ];
    debug_assert_eq!(HEADER.len(), fields.len());
    write_record(writer, HEADER.iter().copied(), delimiter)?;
    write_record(writer, fields.iter().map(String::as_str), delimiter)
}

fn write_record<'a>(
    writer: &mut impl Write,
    fields: impl IntoIterator<Item = &'a str>,
    delimiter: u8,
) -> io::Result<()> {
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(&[delimiter])?;
        }
        if field
            .bytes()
            .any(|b| matches!(b, b'"' | b'\r' | b'\n') || b == delimiter)
        {
            writer.write_all(b"\"")?;
            // Split on quotes, doubling each one without copying the field.
            for (index, part) in field.split('"').enumerate() {
                if index > 0 {
                    writer.write_all(b"\"\"")?;
                }
                writer.write_all(part.as_bytes())?;
            }
            writer.write_all(b"\"")?;
        } else {
            writer.write_all(field.as_bytes())?;
        }
    }
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sortsize_core::{CancellationToken, ScanIssue, ScanOptions, scan};
    use std::path::Path;

    fn report() -> ScanReport {
        let fixture = tempfile::tempdir().unwrap();
        let mut report = scan(
            fixture.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            |_| {},
        )
        .unwrap();
        report.root = NativePath::from(Path::new("root"));
        report
    }

    fn encoded(report: &ScanReport, delimiter: u8) -> Vec<u8> {
        let mut data = Vec::new();
        write_report(&mut data, report, delimiter).unwrap();
        data
    }

    #[test]
    fn csv_and_tsv_quote_delimiters_quotes_and_both_newlines() {
        let mut data = Vec::new();
        write_record(&mut data, ["é", "a,b", "a\tb", "a\"b", "a\rb\nc", ""], b',').unwrap();
        assert_eq!(data, "é,\"a,b\",a\tb,\"a\"\"b\",\"a\rb\nc\",\n".as_bytes());
        data.clear();
        write_record(
            &mut data,
            ["é", "a,b", "a\tb", "a\"b", "a\rb\nc", ""],
            b'\t',
        )
        .unwrap();
        assert_eq!(
            data,
            "é\ta,b\t\"a\tb\"\t\"a\"\"b\"\t\"a\rb\nc\"\t\n".as_bytes()
        );
    }

    #[test]
    fn summary_schema_distinguishes_unknown_zero_and_coverage() {
        let mut report = report();
        report.allocated_bytes = Some(0);
        report.hardlinked_file_paths = None;
        report.max_depth = Some(0);
        for coverage in [Coverage::Complete, Coverage::Partial, Coverage::Cancelled] {
            report.coverage = coverage;
            for delimiter in *b",\t" {
                let data = String::from_utf8(encoded(&report, delimiter)).unwrap();
                let mut lines = data.lines();
                assert_eq!(
                    lines
                        .next()
                        .unwrap()
                        .split(delimiter as char)
                        .collect::<Vec<_>>(),
                    HEADER
                );
                let cells = lines
                    .next()
                    .unwrap()
                    .split(delimiter as char)
                    .collect::<Vec<_>>();
                assert_eq!(cells.len(), HEADER.len());
                let cell = |name| cells[HEADER.iter().position(|h| *h == name).unwrap()];
                assert_eq!(cell("export_scope"), "summary");
                assert_eq!(cell("export_schema_version"), "1");
                assert_eq!(cell("schema_version"), "2");
                assert_eq!(cell("logical_bytes"), "0");
                assert_eq!(cell("allocated_bytes"), "0");
                assert_eq!(cell("potentially_reclaimable_bytes"), "");
                assert_eq!(cell("hardlinked_file_paths"), "");
                assert_eq!(cell("max_depth"), "0");
                assert_eq!(
                    cell("coverage"),
                    serde_json::to_value(coverage).unwrap().as_str().unwrap()
                );
                assert!(lines.next().is_none());
            }
        }
    }

    #[test]
    fn both_raw_path_encodings_are_lossless_even_with_unpaired_utf16() {
        let mut report = report();
        for (raw, encoding, units) in [
            (
                RawPath::UnixBytes(vec![47, 255]),
                "unix_bytes",
                vec![47, 255],
            ),
            (
                RawPath::WindowsUtf16(vec![67, 58, 92, 0xd800]),
                "windows_utf16",
                vec![67, 58, 92, 0xd800],
            ),
        ] {
            report.root.raw = Some(raw);
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(json["root"]["raw"]["encoding"], encoding);
            assert_eq!(json["root"]["raw"]["units"], serde_json::json!(units));
            let csv = String::from_utf8(encoded(&report, b',')).unwrap();
            assert!(csv.contains(&format!(
                ",{encoding},\"{}\",",
                serde_json::to_string(&units).unwrap()
            )));
            let tsv = String::from_utf8(encoded(&report, b'\t')).unwrap();
            assert!(tsv.contains(&format!(
                "\t{encoding}\t{}\t",
                serde_json::to_string(&units).unwrap()
            )));
        }
    }

    #[test]
    fn equivalent_retained_data_have_identical_bytes_in_every_format() {
        let mut report = report();
        for byte in [255, 254] {
            for operation in ["metadata", "list"] {
                for message in ["b\n\"\t,", "a"] {
                    report.errors.push(ScanIssue {
                        path: NativePath {
                            display: "same lossy �".into(),
                            raw: Some(RawPath::UnixBytes(vec![47, byte])),
                        },
                        operation,
                        kind: "PermissionDenied".into(),
                        message: message.into(),
                    });
                }
            }
        }
        report.exclusions = vec!["z".into(), "é".into(), "a".into()];
        canonicalize(&mut report);
        let before = [
            encoded(&report, b','),
            encoded(&report, b'\t'),
            serde_json::to_vec(&report).unwrap(),
        ];
        report.errors.reverse();
        report.exclusions.reverse();
        canonicalize(&mut report);
        let after = [
            encoded(&report, b','),
            encoded(&report, b'\t'),
            serde_json::to_vec(&report).unwrap(),
        ];
        assert_eq!(before, after);
        assert!(
            matches!(&report.errors[0].path.raw, Some(RawPath::UnixBytes(units)) if units == &[47, 254])
        );
    }

    #[test]
    fn export_propagates_write_failures() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture write failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_report(&mut Failing, &report(), b',').is_err());
    }
}
