use super::row::ALLOCATED;
use super::*;
use crate::local::Observation;
use crate::{EntryDates, ScanIssue};
use std::fs;
use std::time::{Duration, UNIX_EPOCH};

fn directory(limited: bool) -> Observation {
    Observation::Directory {
        depth_limited: limited,
        resource_limited: false,
        dates: None,
        owner: None,
    }
}
fn file(bytes: u64) -> Observation {
    Observation::File {
        bytes,
        hardlinked: None,
        dates: None,
        owner: None,
        allocation: None,
    }
}
fn fixture_rows(observations: &[(&str, Observation)], coverage: Coverage) -> Vec<Record> {
    let temp = tempfile::tempdir().unwrap();
    let mut first = Sorter::new(temp.path(), "in", 1);
    for (path, observation) in observations {
        first
            .push(
                Record::observe(
                    Path::new("root"),
                    &Path::new("root").join(path),
                    observation,
                )
                .unwrap(),
            )
            .unwrap();
    }
    let input = first.finish().unwrap();
    let mut second = Sorter::new(temp.path(), "out", 1);
    aggregate(&mut Reader::open(&input).unwrap(), &mut second, coverage).unwrap();
    let output = second.finish().unwrap();
    let mut reader = Reader::open(&output).unwrap();
    let mut rows = Vec::new();
    while let Some(row) = reader.next_record().unwrap() {
        rows.push(row);
    }
    rows
}
fn entry<'a>(rows: &'a [Record], name: &str) -> &'a Record {
    rows.iter()
        .find(|r| r.cells[6] == name && r.cells[2] == "entry")
        .unwrap()
}

#[test]
fn interleaved_subtrees_have_independent_recursive_and_direct_oracles() {
    // Intentionally neither DFS nor parent-first; the sorter is authoritative.
    let rows = fixture_rows(
        &[
            ("b/z", file(23)),
            ("a", directory(false)),
            ("b", directory(false)),
            ("a/n/x", file(7)),
            ("", directory(false)),
            ("a/n", directory(false)),
            ("zero", file(0)),
            ("a/y", file(1)),
            ("a/empty", directory(false)),
        ],
        Coverage::Complete,
    );
    let root = entry(&rows, "root");
    for (column, expected) in [
        (LOGICAL, "31"),
        (DIRECT_FILES, "1"),
        (DIRECT_DIRS, "2"),
        (RECURSIVE_FILES, "4"),
        (RECURSIVE_DIRS, "4"),
    ] {
        assert_eq!(root.cells[column], expected);
        assert_eq!(root.cells[column + 2], "exact");
    }
    let a = entry(&rows, "a");
    assert_eq!(a.cells[LOGICAL], "8");
    assert_eq!(a.cells[DIRECT_FILES], "1");
    assert_eq!(a.cells[DIRECT_DIRS], "2");
    assert_eq!(a.cells[RECURSIVE_FILES], "2");
    assert_eq!(entry(&rows, "empty").cells[LOGICAL], "0");
    assert_eq!(root.cells[ALLOCATED], "");
    assert_eq!(root.cells[ALLOCATED + 1], "not_collected");
}

#[test]
fn unknown_error_types_are_separate_and_propagate_only_to_ancestors() {
    let error = Observation::Error(ScanIssue::io(
        Path::new("root/a/gone"),
        "metadata",
        &io::Error::from(io::ErrorKind::NotFound),
    ));
    let rows = fixture_rows(
        &[
            ("", directory(false)),
            ("a", directory(false)),
            ("b", directory(false)),
            ("a/gone", error),
            ("b/f", file(4)),
        ],
        Coverage::Partial,
    );
    assert_eq!(entry(&rows, "root").cells[LOGICAL + 2], "lower_bound");
    assert_eq!(entry(&rows, "a").cells[LOGICAL + 2], "lower_bound");
    assert_eq!(entry(&rows, "b").cells[LOGICAL + 2], "exact");
    let error = rows.iter().find(|r| r.cells[2] == "error").unwrap();
    assert_eq!(error.cells[KIND], "");
    assert_eq!(error.cells[LOGICAL], "");
    let cancelled = fixture_rows(
        &[("", directory(false)), ("f", file(4))],
        Coverage::Cancelled,
    );
    assert_eq!(entry(&cancelled, "root").cells[LOGICAL + 2], "lower_bound");
    assert_eq!(entry(&cancelled, "f").cells[LOGICAL + 2], "exact");
}

#[test]
fn timestamp_and_metadata_states_are_lossless_without_numeric_sentinels() {
    let mut row = Record::observe(Path::new("root"), Path::new("root/f"), &file(0)).unwrap();
    // A fixture representable on both Unix and Windows (100 ns ticks).
    let dates = EntryDates {
        modified: MetadataValue::Known {
            value: UNIX_EPOCH - Duration::new(1, 123_456_700),
            accuracy: ValueAccuracy::Exact,
        },
        accessed: MetadataValue::AccessDenied,
    };
    row.metric(
        ALLOCATED,
        &MetadataValue::Known {
            value: 0u64,
            accuracy: ValueAccuracy::LowerBound,
        },
    );
    assert_eq!(
        &row.cells[ALLOCATED..ALLOCATED + 3],
        ["0", "known", "lower_bound"]
    );
    // Construct dates via the actual observation path rather than an export-only accessor.
    let mut observation = file(0);
    if let Observation::File { dates: slot, .. } = &mut observation {
        *slot = Some(dates);
    }
    let row = Record::observe(Path::new("root"), Path::new("root/f"), &observation).unwrap();
    assert_eq!(
        &row.cells[29..35],
        ["-1123456700", "known", "exact", "", "access_denied", ""]
    );
    for (value, expected) in [
        (MetadataValue::<u64>::Unknown, "unknown"),
        (MetadataValue::Unsupported, "unsupported"),
        (MetadataValue::AccessDenied, "access_denied"),
        (MetadataValue::NotCollected, "not_collected"),
    ] {
        let mut row = Record::empty("entry");
        row.metric(ALLOCATED, &value);
        assert_eq!(row.cells[ALLOCATED], "");
        assert_eq!(row.cells[ALLOCATED + 1], expected);
        assert_eq!(row.cells[ALLOCATED + 2], "");
    }
}

#[test]
fn external_merge_crosses_levels_without_truncation_or_path_prefix_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let mut sorter = Sorter::new(temp.path(), "runs", 1);
    for i in (0..600).rev() {
        let mut row = Record::empty("entry");
        row.key = format!("{i:06}");
        sorter.push(row).unwrap();
    }
    let run = sorter.finish().unwrap();
    let mut reader = Reader::open(&run).unwrap();
    for i in 0..600 {
        assert_eq!(
            reader.next_record().unwrap().unwrap().key,
            format!("{i:06}")
        );
    }
    assert!(reader.next_record().unwrap().is_none());
    assert!(row::native_key(Path::new("a/x")) < row::native_key(Path::new("a!")));
    assert!(!row::native_key(Path::new("ab/x")).starts_with(&row::native_key(Path::new("a"))));
}

#[test]
fn corrupt_spools_and_oversized_rows_fail_without_panicking() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad");
    fs::write(&path, b"{\"key\":\"unfinished").unwrap();
    assert!(Reader::open(&path).unwrap().next_record().is_err());
    let mut sorter = Sorter::new(temp.path(), "large", 1);
    let mut row = Record::empty("entry");
    row.cells[45] = "x".repeat(sort::MAX_RECORD_BYTES);
    sorter.push(row).unwrap();
    assert!(sorter.finish().is_err());
}

#[test]
fn fail_fast_observer_stops_and_joins_blocked_concurrent_producers() {
    let fixture = tempfile::tempdir().unwrap();
    for d in 0..10 {
        fs::create_dir(fixture.path().join(d.to_string())).unwrap();
    }
    let mut options = ScanOptions::default();
    options.concurrency.workers = 8;
    options.concurrency.max_buffered_observations = 1;
    let result = crate::scan::scan_observed_fallible(
        fixture.path(),
        &options,
        &CancellationToken::default(),
        |_| {},
        CollectFields::default(),
        |_, _| {
            Err(ScanFailure::ObserverIo(io::Error::other(
                "injected spool fault",
            )))
        },
    );
    assert!(matches!(result, Err(ScanFailure::ObserverIo(_))));
}

#[test]
fn export_cleans_spool_after_success_failure_and_cancellation() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    fs::write(fixture.path().join("f"), b"abc").unwrap();
    let export = ExportOptions {
        temporary_directory: Some(parent.path().into()),
        sort_memory_bytes: 1,
    };
    struct Fails;
    impl Write for Fails {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::StorageFull.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(
        export_entries(
            fixture.path(),
            &ScanOptions::default(),
            &export,
            &CancellationToken::default(),
            |_| {},
            &mut Fails,
            b','
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    let token = CancellationToken::default();
    let signal = token.clone();
    let mut out = Vec::new();
    let report = export_entries(
        fixture.path(),
        &ScanOptions::default(),
        &export,
        &token,
        |_| signal.cancel(),
        &mut out,
        b',',
    )
    .unwrap();
    assert_eq!(report.coverage, Coverage::Cancelled);
    assert!(String::from_utf8(out).unwrap().contains(",end,"));
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn native_non_unicode_keys_and_private_spools_do_not_alias_lossy_paths() {
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join(std::ffi::OsString::from_vec(vec![0xff]));
    let second = root.path().join(std::ffi::OsString::from_vec(vec![0xfe]));
    fs::write(&first, []).unwrap();
    fs::write(&second, []).unwrap();
    assert_eq!(first.to_string_lossy(), second.to_string_lossy());
    let a = Record::observe(root.path(), &first, &file(0)).unwrap();
    let b = Record::observe(root.path(), &second, &file(0)).unwrap();
    assert_ne!(a.key, b.key);
    assert_ne!(a.cells[5], b.cells[5]);
    assert_ne!(a.cells[8], b.cells[8]);
    let export = ExportOptions {
        temporary_directory: Some(root.path().into()),
        ..Default::default()
    };
    for workers in [1, 4, 8] {
        let mut options = ScanOptions::default();
        options.concurrency.workers = workers;
        let mut checked = false;
        let report = export_entries(
            root.path(),
            &options,
            &export,
            &CancellationToken::default(),
            |_| {
                if !checked {
                    for entry in fs::read_dir(root.path()).unwrap().flatten() {
                        if entry.file_type().unwrap().is_dir() {
                            assert_eq!(
                                entry.metadata().unwrap().permissions().mode() & 0o777,
                                0o700
                            );
                        }
                    }
                    checked = true;
                }
            },
            &mut Vec::new(),
            b'\t',
        )
        .unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.directories, 1);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
}

#[cfg(windows)]
#[test]
fn native_utf16_order_keeps_lone_surrogates_and_sid_binary_identity() {
    use std::os::windows::ffi::OsStringExt;
    let root = Path::new("root");
    let a = root.join(std::ffi::OsString::from_wide(&[0xd800]));
    let b = root.join(std::ffi::OsString::from_wide(&[0xd801]));
    let row_a = Record::observe(root, &a, &file(0)).unwrap();
    let row_b = Record::observe(root, &b, &file(0)).unwrap();
    assert!(row_a.key < row_b.key);
    assert_ne!(row_a.cells[5], row_b.cells[5]);
    assert_eq!(row_a.cells[7], "windows_utf16");
}

#[test]
fn spool_io_faults_are_fatal_without_end_and_release_all_handles() {
    let root = tempfile::tempdir().unwrap();
    for i in 0..80 {
        fs::write(root.path().join(format!("f{i:03}")), b"abc").unwrap();
    }
    for (point, after) in [
        ("create", 0),
        ("create", 2),
        ("write", 3),
        ("flush", 0),
        ("flush", 8),
        ("merge", 0),
        ("merge", 2),
        ("read", 2),
        ("remove", 0),
        ("remove", 8),
        ("cleanup", 0),
    ] {
        let parent = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let result = faults::during(point, after, || {
            export_entries(
                root.path(),
                &ScanOptions::default(),
                &ExportOptions {
                    temporary_directory: Some(parent.path().into()),
                    sort_memory_bytes: 1,
                },
                &CancellationToken::default(),
                |_| {},
                &mut out,
                b',',
            )
        });
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains(point),
            "{point}/{after}: {error}"
        );
        assert!(
            !String::from_utf8(out).unwrap().contains(",end,"),
            "{point}"
        );
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0, "{point}");
        // On Windows this also proves that no reader/writer still holds a file.
        parent.close().unwrap();
    }
}

fn synthetic_input(temp: &Path, depth: usize, error: bool) -> PathBuf {
    let input = temp.join("synthetic");
    let mut writer = io::BufWriter::new(fs::File::create_new(&input).unwrap());
    for level in 0..depth {
        let mut row = Record::empty("entry");
        row.key = "!".repeat(level);
        row.cells[KIND] = "directory".into();
        row.cells[6] = level.to_string();
        serde_json::to_writer(&mut writer, &row).unwrap();
        writer.write_all(b"\n").unwrap();
    }
    let mut row = Record::empty(if error { "error" } else { "entry" });
    row.key = "!".repeat(depth);
    if !error {
        row.cells[KIND] = "file".into();
        row.cells[LOGICAL] = "17".into();
    }
    serde_json::to_writer(&mut writer, &row).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    input
}

#[test]
fn deep_synthetic_aggregation_keeps_counts_and_lower_bounds() {
    for error in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let depth = 2048;
        let input = synthetic_input(temp.path(), depth, error);
        let mut output = Sorter::new(temp.path(), "deep", 256 * 1024);
        aggregate(
            &mut Reader::open(&input).unwrap(),
            &mut output,
            Coverage::Complete,
        )
        .unwrap();
        let mut reader = Reader::open(&output.finish().unwrap()).unwrap();
        let mut directories = 0;
        while let Some(row) = reader.next_record().unwrap() {
            if row.cells[KIND] != "directory" {
                continue;
            }
            let level: usize = row.cells[6].parse().unwrap();
            assert_eq!(row.cells[RECURSIVE_DIRS], (depth - level - 1).to_string());
            assert_eq!(
                row.cells[DIRECT_DIRS],
                usize::from(level + 1 < depth).to_string()
            );
            assert_eq!(row.cells[RECURSIVE_FILES], usize::from(!error).to_string());
            assert_eq!(
                row.cells[DIRECT_FILES],
                usize::from(!error && level + 1 == depth).to_string()
            );
            assert_eq!(row.cells[LOGICAL], if error { "0" } else { "17" });
            assert_eq!(
                row.cells[LOGICAL + 2],
                if error { "lower_bound" } else { "exact" }
            );
            directories += 1;
        }
        assert_eq!(directories, depth);
    }
}

#[test]
fn aggregation_stack_releases_budget_and_enforces_both_limits() {
    let temp = tempfile::tempdir().unwrap();
    let mut output = Sorter::new(temp.path(), "closed", 1024);
    let row = Record::empty("entry");
    let cost = row.memory_bytes() + std::mem::size_of::<Frame>();
    let mut stack = vec![Frame {
        row,
        bytes: 0,
        direct_files: 0,
        direct_dirs: 0,
        recursive_files: 0,
        recursive_dirs: 0,
    }];
    let mut bytes = cost;
    close(&mut stack, &mut bytes, &mut output).unwrap();
    assert_eq!(bytes, 0);
    for large in [false, true] {
        let input = temp.path().join(if large { "bytes" } else { "count" });
        let mut writer = io::BufWriter::new(fs::File::create_new(&input).unwrap());
        for _ in 0..if large { 40 } else { 4097 } {
            let mut row = Record::empty("entry");
            row.cells[KIND] = "directory".into();
            if large {
                row.cells[45] = "x".repeat(256 * 1024);
            }
            serde_json::to_writer(&mut writer, &row).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        writer.flush().unwrap();
        let error = aggregate(
            &mut Reader::open(&input).unwrap(),
            &mut output,
            Coverage::Complete,
        )
        .unwrap_err();
        assert!(error.to_string().contains("4096 dossiers / 8 Mio"));
    }
}

#[test]
#[ignore = "comparative release microbenchmark; run explicitly with --nocapture"]
fn deep_stack_budget_benchmark() {
    use std::hint::black_box;
    use std::time::Instant;
    for depth in [512, 1024, 2048] {
        let rows: Vec<_> = (0..depth)
            .map(|i| {
                let mut row = Record::empty("entry");
                row.key = "!".repeat(i);
                row
            })
            .collect();
        for repetition in 0..5 {
            for legacy in if repetition % 2 == 0 {
                [true, false]
            } else {
                [false, true]
            } {
                let start = Instant::now();
                let mut checksum = 0usize;
                for _ in 0..20 {
                    let mut total = 0usize;
                    for i in 0..rows.len() {
                        if legacy {
                            total = black_box(&rows[..=i])
                                .iter()
                                .map(|r| r.memory_bytes() + std::mem::size_of::<Frame>())
                                .sum();
                        } else {
                            total +=
                                black_box(&rows[i]).memory_bytes() + std::mem::size_of::<Frame>();
                        }
                        checksum ^= black_box(total);
                    }
                }
                println!(
                    "budget,depth={depth},repeat={repetition},legacy={legacy},ns={},checksum={}",
                    start.elapsed().as_nanos(),
                    black_box(checksum)
                );
            }
        }
    }
}

#[test]
fn spool_inside_scan_is_pruned_for_every_worker_and_cleanup_path() {
    let root = tempfile::tempdir().unwrap();
    for i in 0..16 {
        let dir = root.path().join(format!("d{i}"));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("f"), b"abc").unwrap();
    }
    #[cfg(windows)]
    let scan_root = PathBuf::from(root.path().to_str().unwrap().to_uppercase());
    #[cfg(not(windows))]
    let scan_root = root.path().join(".");
    for workers in [1, 4, 8] {
        for outcome in ["success", "failure", "cancelled"] {
            let mut options = ScanOptions::default();
            options.concurrency.workers = workers;
            let token = CancellationToken::default();
            let mut out = Vec::new();
            let mut inspected = false;
            let mut run = || {
                export_entries(
                    &scan_root,
                    &options,
                    &ExportOptions {
                        temporary_directory: Some(root.path().into()),
                        sort_memory_bytes: 1,
                    },
                    &token,
                    |_| {
                        inspected = true;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            for entry in fs::read_dir(root.path()).unwrap().flatten() {
                                if entry
                                    .file_name()
                                    .to_string_lossy()
                                    .starts_with("sortsize-entries-")
                                {
                                    assert_eq!(
                                        entry.metadata().unwrap().permissions().mode() & 0o777,
                                        0o700
                                    );
                                }
                            }
                        }
                        if outcome == "cancelled" {
                            token.cancel();
                        }
                    },
                    &mut out,
                    b',',
                )
            };
            let result = if outcome == "failure" {
                faults::during("flush", 2, run)
            } else {
                run()
            };
            match outcome {
                "failure" => {
                    assert!(result.is_err());
                    assert!(!String::from_utf8(out).unwrap().contains(",end,"));
                }
                "cancelled" => {
                    assert_eq!(result.unwrap().coverage, Coverage::Cancelled);
                }
                _ => {
                    let report = result.unwrap();
                    assert_eq!(report.files, 16);
                    assert_eq!(report.directories, 17);
                    assert_eq!(report.excluded_entries, 1);
                    let text = String::from_utf8(out).unwrap();
                    assert!(!text.contains("sortsize-entries-"));
                    assert!(text.trim_end().ends_with(",true"));
                }
            }
            assert!(inspected || outcome == "failure");
            assert_eq!(
                fs::read_dir(root.path()).unwrap().count(),
                16,
                "{workers}/{outcome}"
            );
        }
    }
}

#[test]
fn real_disappearance_and_modification_are_observed_without_a_second_walk() {
    let fixture = tempfile::tempdir().unwrap();
    let gone = fixture.path().join("gone");
    fs::write(&gone, b"before").unwrap();
    let mut removed = false;
    let mut out = Vec::new();
    let report = export_entries(
        fixture.path(),
        &ScanOptions::default(),
        &ExportOptions::default(),
        &CancellationToken::default(),
        |_| {
            if !removed {
                fs::remove_file(&gone).unwrap();
                removed = true;
            }
        },
        &mut out,
        b',',
    )
    .unwrap();
    // A deletion before readdir can be invisible; if readdir retained it, it is
    // an explicit error. Neither outcome invents a file or an empty file value.
    assert_eq!(report.files, 0);
    assert!(report.errors_count <= 1);
    let changed = fixture.path().join("changed");
    fs::write(&changed, b"a").unwrap();
    let mut modified = false;
    let report = export_entries(
        fixture.path(),
        &ScanOptions::default(),
        &ExportOptions::default(),
        &CancellationToken::default(),
        |_| {
            if !modified {
                fs::write(&changed, b"changed").unwrap();
                modified = true;
            }
        },
        &mut Vec::new(),
        b',',
    )
    .unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(report.logical_bytes, 7);
}

#[test]
fn signal_during_serialization_makes_the_final_marker_cancelled() {
    struct SignalWriter {
        bytes: Vec<u8>,
        token: CancellationToken,
    }
    impl Write for SignalWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(data);
            self.token.cancel();
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let fixture = tempfile::tempdir().unwrap();
    fs::write(fixture.path().join("f"), b"x").unwrap();
    let token = CancellationToken::default();
    let mut writer = SignalWriter {
        bytes: Vec::new(),
        token: token.clone(),
    };
    let report = export_entries(
        fixture.path(),
        &ScanOptions::default(),
        &ExportOptions::default(),
        &token,
        |_| {},
        &mut writer,
        b',',
    )
    .unwrap();
    assert_eq!(report.coverage, Coverage::Cancelled);
    assert!(
        String::from_utf8(writer.bytes)
            .unwrap()
            .contains(",cancelled,")
    );
}
