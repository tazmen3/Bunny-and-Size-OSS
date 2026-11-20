//! Operator-provided metadata-only qualification. Never run against an unowned fixture.
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    inventory_contract::ScopeCoverage,
};
use sortsize_store::inventory::queries::empty_entries::{EmptyKind, EmptySort, prepare};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

fn required_path(variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{variable} must identify a fixture or working directory"))
}

#[test]
#[ignore = "uses an operator-provided fixture and local scratch directory"]
fn empty_entries_metadata_qualification() {
    let root = required_path("SORTSIZE_DUP_QUAL_SOURCE");
    let scratch_parent = required_path("SORTSIZE_DUP_QUAL_SCRATCH");
    let analysis =
        std::env::var("SORTSIZE_DUP_QUAL_EMPTY_KIND").expect("empty-entry analysis kind");
    let kind = match analysis.as_str() {
        "files" => EmptyKind::Files,
        "directories" => EmptyKind::Directories,
        _ => panic!("unknown analysis kind {analysis}"),
    };
    let mib = 1 << 20;
    let disk_mib = std::env::var("SORTSIZE_DUP_QUAL_TEMP_MIB")
        .map(|value| value.parse::<u64>().expect("temporary budget in MiB"))
        .unwrap_or(2048);
    assert!((1..=24_576).contains(&disk_mib));

    let guard = Arc::new(
        Guard::open(
            &root,
            Policy {
                explicit_root: true,
                ..Policy::default()
            },
        )
        .unwrap(),
    );
    assert!(
        guard
            .work_path_allowed(&scratch_parent.canonicalize().unwrap())
            .unwrap()
    );
    let scratch = tempfile::tempdir_in(scratch_parent).unwrap();
    let cancel = CancellationToken::default();
    let started = Instant::now();
    let job = prepare(
        guard.clone(),
        scratch.path(),
        disk_mib * mib,
        kind,
        &cancel,
        |_| {},
    )
    .unwrap();
    let preparation_us = started.elapsed().as_micros();
    let summary = job.summary();
    let expect_partial = std::env::var_os("SORTSIZE_DUP_QUAL_EXPECT_PARTIAL").is_some();
    let coverage = if expect_partial {
        assert_eq!(summary.coverage, ScopeCoverage::Partial, "{summary:?}");
        assert!(summary.progress.errors > 0, "{summary:?}");
        "partial"
    } else {
        assert_eq!(summary.coverage, ScopeCoverage::Complete, "{summary:?}");
        assert_eq!(summary.progress.errors, 0, "{summary:?}");
        "complete"
    };
    assert!(!summary.cancelled);
    let querying = Instant::now();
    let first = job.page("", EmptySort::PathAscending, 0, &cancel).unwrap();
    let first_page_us = querying.elapsed().as_micros();
    assert_eq!(first.rows.is_empty(), summary.progress.matches == 0);

    let export_us = std::env::var_os("SORTSIZE_DUP_QUAL_RESULTS")
        .map(PathBuf::from)
        .map(|output| {
            let exporting = Instant::now();
            let parent = output.parent().unwrap().canonicalize().unwrap();
            assert!(
                guard.work_path_allowed(&parent).unwrap(),
                "export overlaps fixture"
            );
            let mut pending = tempfile::NamedTempFile::new_in(parent).unwrap();
            let mut stream = BufWriter::new(pending.as_file_mut());
            let mut page = first;
            let mut rows = 0;
            loop {
                for row in page.rows {
                    let display = row.path.to_str().expect("fixture paths are valid Unicode");
                    emit(&mut stream, "entry", Some(display), 1);
                    rows += 1;
                }
                let Some(offset) = page.next_offset else {
                    break;
                };
                page = job
                    .page("", EmptySort::PathAscending, offset, &cancel)
                    .unwrap();
            }
            assert_eq!(rows, summary.progress.matches);
            let mut offset = 0;
            let mut parents_total = 0;
            loop {
                let page = job.parents("", offset, &cancel).unwrap();
                for parent in page.rows {
                    let display = parent
                        .path
                        .as_deref()
                        .map(|path: &Path| path.to_str().expect("fixture paths are valid Unicode"));
                    emit(&mut stream, "parent", display, parent.count);
                    parents_total += parent.count;
                }
                let Some(next) = page.next_offset else {
                    break;
                };
                offset = next;
            }
            assert_eq!(parents_total, rows);
            stream.flush().unwrap();
            drop(stream);
            pending.persist_noclobber(output).unwrap();
            exporting.elapsed().as_micros()
        });
    let report = serde_json::json!({
        "schema": 1,
        "source": "native",
        "analysis": analysis,
        "preparation_us": preparation_us,
        "first_page_us": first_page_us,
        "export_us": export_us,
        "coverage": coverage,
        "directories_scanned": summary.progress.directories_scanned,
        "entries_seen": summary.progress.entries_seen,
        "errors": summary.progress.errors,
        "matches": summary.progress.matches,
        "temporary": job.temporary_usage(),
    });
    drop(job);
    assert_eq!(scratch.path().read_dir().unwrap().count(), 0);
    println!("DUPLICATE_RESOURCES_RESULT {report}");
}

fn emit(stream: &mut impl Write, record: &str, path: Option<&str>, count: u64) {
    let row = serde_json::json!({"record": record, "path": path, "count": count});
    writeln!(stream, "{row}").unwrap();
}
