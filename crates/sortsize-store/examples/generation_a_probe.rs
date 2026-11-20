//! Isolated architecture probe. Synthetic metadata, never a source acquisition.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sortsize_core::{
    CancellationToken, InventoryKind,
    analysis::{Evaluation, PreparedQuery, SearchQuery},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    codec::NativeName,
    inventory::{
        Kind, Observation, Store,
        generation_a::{CertifiedGeneration, Pipeline, PublicationOutcome, Repository},
        queries::{Cursor, Page, QueryCounts, Row},
    },
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Serialize, Deserialize)]
struct Fixture {
    files: u64,
    adverse: bool,
    root: PathBuf,
}

#[cfg(target_os = "linux")]
fn peak_memory() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024
}
#[cfg(windows)]
fn peak_memory() -> u64 {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    // SAFETY: current-process handle and correctly sized initialized output.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        assert_ne!(
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32
            ),
            0
        );
        counters.PeakWorkingSetSize as u64
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
fn peak_memory() -> u64 {
    0
}

fn raw_name(index: u64, adverse: bool) -> NativeName {
    let extension = if index.is_multiple_of(5) {
        "log"
    } else {
        "txt"
    };
    let suffix = if adverse && index.is_multiple_of(23) {
        "-İΣ"
    } else {
        ""
    };
    let text = format!("file-{index:010}{suffix}.{extension}");
    #[cfg(unix)]
    {
        let mut bytes = text.into_bytes();
        if adverse && index.is_multiple_of(19) {
            bytes.insert(15, 0xff);
        }
        NativeName::Posix(bytes)
    }
    #[cfg(windows)]
    {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        if adverse && index.is_multiple_of(19) {
            units.insert(15, 0xd800);
        }
        NativeName::Windows(units)
    }
    #[cfg(not(any(unix, windows)))]
    NativeName::Opaque(text.into_bytes())
}

fn native_component(name: NativeName) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let NativeName::Posix(bytes) = name else {
            panic!("unexpected native codec")
        };
        std::ffi::OsString::from_vec(bytes).into()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let NativeName::Windows(units) = name else {
            panic!("unexpected native codec")
        };
        std::ffi::OsString::from_wide(&units).into()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let NativeName::Opaque(bytes) = name else {
            panic!("unexpected native codec")
        };
        String::from_utf8(bytes).unwrap().into()
    }
}

fn observation(index: u64, adverse: bool) -> Observation {
    Observation {
        name: raw_name(index, adverse),
        kind: if adverse && index.is_multiple_of(17) {
            Kind::Unknown
        } else {
            Kind::File
        },
        logical: (!(adverse && index.is_multiple_of(13))).then_some(index),
        allocated: None,
        modified: (!(adverse && index.is_multiple_of(11))).then_some((index as i64 - 50_000, 5)),
        metadata_known: true,
    }
}

fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}

fn prepare(directory: &Path, files: u64, adverse: bool) -> Result<Value> {
    if files == 0 || files > i64::MAX as u64 - 50_000 {
        return Err("unsupported fixture size".into());
    }
    if directory.exists() {
        return Err("new fixture directory required".into());
    }
    sortsize_store::inventory::lifecycle::ensure_root(directory)?;
    let fixture = Fixture {
        files,
        adverse,
        root: if cfg!(windows) {
            PathBuf::from("Q:/generation-probe/offline")
        } else {
            PathBuf::from("/generation-probe/offline")
        },
    };
    let started = Instant::now();
    let template = context();
    let mut repository = Repository::open(
        &directory.join("published"),
        template.inventory,
        template.source,
    )?;
    let c = repository.next_context(template)?;
    assert_eq!(c.generation, GenerationId(1));
    let mut plan = Pipeline::plan()?;
    let mut store = repository.create_candidate(c, &mut plan)?;
    let attempt = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    store.begin_attempt(attempt)?;
    let mut sequence = 0;
    let ingest_started = Instant::now();
    for first in (0..files).step_by(512) {
        let observations: Vec<_> = (first..(first + 512).min(files))
            .map(|i| observation(i, adverse))
            .collect();
        store.ingest(
            BatchHeader {
                attempt,
                sequence,
                end: None,
            },
            &observations,
        )?;
        sequence += 1;
    }
    let ingest_ms = milliseconds(ingest_started);
    let rows_per_second = files as f64 / (ingest_ms / 1000.0);
    let listing_started = Instant::now();
    store.ingest(
        BatchHeader {
            attempt,
            sequence,
            end: Some(ListingEnd::Exhausted),
        },
        &[],
    )?;
    let listing_end_ms = milliseconds(listing_started);
    let reconcile_started = Instant::now();
    while !store.reconcile_step(attempt, ComparisonScope::Comparable, 512)? {}
    let reconcile_ms = milliseconds(reconcile_started);
    let fold_started = Instant::now();
    while !store.consolidate_step(c, 512)? {}
    let consolidate_ms = milliseconds(fold_started);
    let seal_started = Instant::now();
    let coverage = store.seal(c)?;
    let seal_ms = milliseconds(seal_started);
    assert_eq!(store.entry_count(c.generation)?, files + 1);
    store.set_retention(true, 30)?;
    store.record_measurement(c.generation, &fixture.root, 100, 200)?;
    std::fs::write(
        directory.join("fixture.json"),
        serde_json::to_vec_pretty(&fixture)?,
    )?;
    let ingestion_and_seal_ms = milliseconds(started);
    // The legacy oracle is a physical copy of these exact observations. Its
    // preparation cost is disclosed separately and never charged as ingestion.
    drop(plan);
    drop(repository);
    let copied = Instant::now();
    let baseline = store.copy_local(&directory.join("inventory"))?;
    drop(baseline);
    drop(store);
    let oracle_copy_ms = milliseconds(copied);
    Ok(
        json!({"action":"prepare","synthetic":true,"files":files,"entries":files+1,
        "profile":if adverse { "adverse" } else { "plain" },"ingestion_and_seal_ms":ingestion_and_seal_ms,
        "source_coverage":format!("{coverage:?}"),"peak_process_bytes":peak_memory(),"source_io":0,
        "oracle_copy_ms":oracle_copy_ms,"production_repository":true,
        "ingest_ms":ingest_ms,"rows_per_second":rows_per_second,"ingest_rows_per_second":rows_per_second,
        "listing_end_ms":listing_end_ms,"reconcile_ms":reconcile_ms,"consolidate_ms":consolidate_ms,"seal_ms":seal_ms}),
    )
}

fn milliseconds(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn governor_usage() -> Value {
    let usage = sortsize_core::resources::Governor::global().usage();
    json!({"requested_mode":usage.limits.requested.key(),
        "effective_limit_bytes":usage.limits.bytes,"effective_entry_limit":usage.limits.entries,
        "fallback":usage.limits.fallback,"accounted_bytes":usage.categories.iter().sum::<u64>(),
        "category_accounted_bytes":usage.categories,"category_limit_bytes":usage.limits.categories,
        "disk_used_bytes":usage.disk_used,"disk_reserved_bytes":usage.disk_reserved,"disk_limit_bytes":usage.disk_limit})
}

fn search(scenario: &str, files: u64) -> Result<SearchQuery> {
    let mut query = SearchQuery::default();
    match scenario {
        "all" => {}
        "kind" => query.kind = Some(InventoryKind::File),
        "size_late" => {
            query.kind = Some(InventoryKind::File);
            query.min_bytes = Some(files - 1);
            query.max_bytes = Some(files - 1);
        }
        "size_absent" => {
            query.kind = Some(InventoryKind::File);
            query.min_bytes = Some(files + 1);
            query.max_bytes = Some(files + 1);
        }
        "extension" => query.extension = Some(".TXT".into()),
        "substring_rare" => query.name = Some(format!("file-{:010}", files - 1)),
        "substring_absent" => query.name = Some("absent-from-this-corpus".into()),
        "glob" => query.glob = Some("file-*7*.txt".into()),
        "combined_unknowns" => {
            query.kind = Some(InventoryKind::File);
            query.min_bytes = Some(1);
            query.modified_after = Some(UNIX_EPOCH - Duration::from_secs(60_000));
            query.name = Some("file-".into());
        }
        _ => return Err("unknown scenario".into()),
    }
    Ok(query)
}

fn counts_json(counts: QueryCounts) -> Value {
    json!({"evaluated":counts.evaluated,"matches":counts.matches,
        "unknown_sizes":counts.unknown_sizes,"unknown_dates":counts.unknown_dates,
        "native_text":counts.native_text,"oversized_paths":counts.oversized_paths,
        "unknown_kinds":counts.unknown_kinds,"cancelled":counts.cancelled})
}

fn expected_complete(fixture: &Fixture, counts: QueryCounts) -> bool {
    // Every adverse fixture includes file 0 with an unknown logical size, so
    // the sealed source coverage is partial independently of query criteria.
    !fixture.adverse
        && !counts.cancelled
        && counts.unknown_sizes == 0
        && counts.unknown_dates == 0
        && counts.native_text == 0
        && counts.oversized_paths == 0
        && counts.unknown_kinds == 0
}

struct Oracle<'a> {
    fixture: &'a Fixture,
    prepared: PreparedQuery,
    kind_filter: bool,
    position: u64,
    counts: QueryCounts,
}
impl<'a> Oracle<'a> {
    fn new(fixture: &'a Fixture, query: &SearchQuery) -> Self {
        Self {
            fixture,
            prepared: PreparedQuery::new(query).unwrap(),
            kind_filter: query.kind.is_some(),
            position: 0,
            counts: QueryCounts::default(),
        }
    }
    fn next_match(&mut self) -> Option<(u64, PathBuf, Kind, Option<u64>)> {
        while self.position <= self.fixture.files {
            let id = self.position;
            self.position += 1;
            let (path, kind, logical, modified) = if id == 0 {
                (self.fixture.root.clone(), Kind::Directory, None, None)
            } else {
                let value = observation(id - 1, self.fixture.adverse);
                let modified = value.modified.map(|(seconds, nanos)| {
                    if seconds >= 0 {
                        UNIX_EPOCH + Duration::new(seconds as u64, nanos)
                    } else {
                        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
                            + Duration::from_nanos(u64::from(nanos))
                    }
                });
                (
                    self.fixture.root.join(native_component(value.name)),
                    value.kind,
                    value.logical,
                    modified,
                )
            };
            self.counts.evaluated += 1;
            if kind == Kind::Unknown && self.kind_filter {
                self.counts.unknown_kinds += 1;
                continue;
            }
            let evaluated = self.prepared.evaluate_path(
                &path,
                match kind {
                    Kind::File => InventoryKind::File,
                    Kind::Directory => InventoryKind::Directory,
                    Kind::Symlink => InventoryKind::Symlink,
                    _ => InventoryKind::Special,
                },
                logical,
                modified,
                Some(&self.fixture.root),
            );
            match evaluated {
                Evaluation::Match => {
                    self.counts.matches += 1;
                    return Some((id, path, kind, logical));
                }
                Evaluation::NoMatch => {}
                Evaluation::UnknownSize => self.counts.unknown_sizes += 1,
                Evaluation::UnknownDate => self.counts.unknown_dates += 1,
                Evaluation::NativeText => self.counts.native_text += 1,
            }
        }
        None
    }
    fn audit_rows(&mut self, rows: &[Row]) {
        self.audit_rows_generation(rows, GenerationId(1));
    }
    fn audit_rows_generation(&mut self, rows: &[Row], generation: GenerationId) {
        for row in rows {
            let (id, path, kind, logical) = self.next_match().expect("unexpected extra member");
            assert_eq!(row.key.inventory, InventoryId(1));
            assert_eq!(row.key.generation, generation);
            assert_eq!(row.key.entry, EntryId(id));
            assert_eq!(row.path, path);
            assert_eq!(row.kind, kind);
            assert_eq!(row.logical, logical);
        }
    }
}

fn run_baseline(
    directory: &Path,
    fixture: &Fixture,
    scenario: &str,
    scratch: &Path,
    quota: u64,
) -> Result<Value> {
    let search = search(scenario, fixture.files)?;
    // Streaming oracle outside every measured region, without source/database I/O.
    let mut expected = Oracle::new(fixture, &search);
    while expected.next_match().is_some() {}
    let expected_complete = expected_complete(fixture, expected.counts);
    let expected_counts = counts_json(expected.counts);
    std::fs::create_dir(scratch)?;
    let since_open = Instant::now();
    let store = Store::reopen(&directory.join("inventory"), Durability::Retained)?;
    let open_certification_ms = milliseconds(since_open);
    let intention = Instant::now();
    let query = store.query(
        GenerationId(1),
        &fixture.root,
        &search,
        scratch,
        quota,
        &CancellationToken::default(),
    )?;
    let exhaustive_and_result_sync_ms = milliseconds(intention);
    let temporary = query.temporary_usage();
    assert_eq!(counts_json(query.counts), expected_counts);
    assert_eq!(query.complete(), expected_complete);
    let mut result = audit_pages(
        fixture,
        &search,
        expected_counts,
        since_open,
        intention,
        |cursor| Ok(query.page(cursor)?),
    )?;
    result["action"] = json!("run");
    result["architecture"] = json!("baseline");
    result["scenario"] = json!(scenario);
    result["open_and_certification_ms"] = json!(open_certification_ms);
    result["exhaustive_and_result_sync_ms"] = json!(exhaustive_and_result_sync_ms);
    result["complete"] = json!(query.complete());
    result["expected_complete"] = json!(expected_complete);
    result["quota_bytes"] = json!(quota);
    result["temporary_charged_bytes"] = json!(temporary.charged);
    result["temporary_peak_bytes"] = json!(temporary.peak);
    result["peak_process_bytes"] = json!(peak_memory());
    result["indexed"] = json!(false);
    result["result_storage"] = json!("new_synced_sequence");
    result["references_generation"] = json!(false);
    result["new_result_sync_required"] = json!(true);
    result["governor_usage"] = governor_usage();
    Ok(result)
}

fn audit_pages(
    fixture: &Fixture,
    search: &SearchQuery,
    expected_counts: Value,
    since_open: Instant,
    intention: Instant,
    mut page_for: impl FnMut(Option<&Cursor>) -> Result<Page>,
) -> Result<Value> {
    let mut oracle = Oracle::new(fixture, search);
    let mut cursor = None;
    let mut pages = Vec::new();
    let mut logical_pages = 0;
    let mut members = 0_u64;
    let mut first_rows = None;
    let mut first_page_ms = None;
    let mut ready_since_intention_ms = None;
    let mut total_since_open_ms = None;
    loop {
        let started = Instant::now();
        let page = page_for(cursor.as_ref())?;
        pages.push(milliseconds(started));
        logical_pages += 1;
        if first_page_ms.is_none() {
            first_page_ms = pages.last().copied();
            ready_since_intention_ms = Some(milliseconds(intention));
            total_since_open_ms = Some(milliseconds(since_open));
            first_rows = Some(page.rows.clone());
        }
        assert!(page.rows.len() <= 200 && page.accounted_bytes <= 512 * 1024);
        oracle.audit_rows(&page.rows);
        members += page.rows.len() as u64;
        if !page.more {
            break;
        }
        cursor = page.next;
    }
    assert!(oracle.next_match().is_none());
    assert_eq!(counts_json(oracle.counts), expected_counts);
    assert_eq!(members, oracle.counts.matches);
    // Small/empty results still provide 30 observations. Repeats follow the
    // first exact page and complete oracle; they never prime READY timing.
    while pages.len() < 30 {
        let started = Instant::now();
        let repeated = page_for(None)?;
        pages.push(milliseconds(started));
        let first = first_rows.as_ref().unwrap();
        assert_eq!(repeated.rows.len(), first.len());
        for (actual, expected) in repeated.rows.iter().zip(first) {
            assert_eq!(actual.key, expected.key);
            assert_eq!(actual.path, expected.path);
            assert_eq!(actual.kind, expected.kind);
            assert_eq!(actual.logical, expected.logical);
        }
    }
    pages.sort_by(f64::total_cmp);
    Ok(json!({"synthetic":true,
        "entries":fixture.files+1,"profile":if fixture.adverse {"adverse"} else {"plain"},
        "first_page_ms":first_page_ms,"ready_since_intention_ms":ready_since_intention_ms,
        "total_since_open_ms":total_since_open_ms,"counts":expected_counts,"members_checked":members,
        "pages":logical_pages,"page_samples":pages.len(),"page_p95_ms":pages[pages.len()*95/100],"page_max_ms":pages.last(),
        "page_repeats_after_full_audit":pages.len()-logical_pages,
        "exactness":"PASS","source_io":0,"first_query_after_open":true}))
}

fn publish(directory: &Path, requested_growth: &str) -> Result<Value> {
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(directory.join("fixture.json"))?)?;
    let total = Instant::now();
    let mut repository = Repository::open_existing(&directory.join("published"))?;
    let mut plan = Pipeline::plan()?;
    let store = repository.open_candidate(GenerationId(1), &mut plan)?;
    let source_open_and_certification_ms = milliseconds(total);
    let started = Instant::now();
    let quota = if requested_growth == "auto" {
        Repository::publication_budget(&store)?
    } else {
        requested_growth.parse()?
    };
    let outcome = repository.publish_with_plan(
        &store,
        GenerationId(1),
        &fixture.root,
        quota,
        &CancellationToken::default(),
        &mut plan,
    )?;
    let index_build_and_publication_ms = milliseconds(started);
    let report = match outcome {
        PublicationOutcome::Committed { report, .. } => report,
        PublicationOutcome::PublishedUncommitted { .. } => {
            return Err(
                "generation published but durable catalogue commit failed; artifacts preserved"
                    .into(),
            );
        }
    };
    drop(store);
    let started = Instant::now();
    let certified = repository
        .open_latest_with_plan(&mut plan, &CancellationToken::default())?
        .ok_or("repository has no published generation")?;
    let published_open_and_certification_ms = milliseconds(started);
    let governor = governor_usage();
    drop(certified);
    Ok(
        json!({"action":"publish","synthetic":true,"entries":report.entries,"oversized_paths":report.oversized_paths,
        "artifact_bytes":report.artifact_bytes,"retained_bytes_per_entry":report.artifact_bytes as f64/(fixture.files+1) as f64,"retained_entries_denominator":fixture.files+1,"quota_bytes":quota,"publication_growth_quota_bytes":quota,"publication_growth_quota_mode":if requested_growth=="auto" {"auto"} else {"explicit"},"source_open_and_certification_ms":source_open_and_certification_ms,
        "index_build_and_publication_ms":index_build_and_publication_ms,"published_open_and_certification_ms":published_open_and_certification_ms,
        "publish_and_certify_ms":milliseconds(total),"peak_process_bytes":peak_memory(),"source_io":0,
        "governor_usage_at_certified":governor,"production_repository":true,"pointer_committed":true}),
    )
}

fn run_generation(
    directory: &Path,
    fixture: &Fixture,
    scenario: &str,
    scratch: &Path,
    quota: u64,
    canonical_control: bool,
) -> Result<Value> {
    let case = QueryCase::new(fixture, scenario)?;
    std::fs::create_dir(scratch)?;
    let since_open = Instant::now();
    let repository = Repository::open_readonly(&directory.join("published"))?;
    let mut plan = Pipeline::plan()?;
    let certified = repository
        .open_latest_with_plan(&mut plan, &CancellationToken::default())?
        .ok_or("repository has no published generation")?;
    assert_eq!(certified.generation(), GenerationId(1));
    assert_eq!(certified.root(), fixture.root);
    query_on_ready(
        &certified,
        fixture,
        &case,
        scratch,
        quota,
        since_open,
        canonical_control,
    )
}

struct QueryCase {
    search: SearchQuery,
    scenario: String,
    counts: Value,
    complete: bool,
}
impl QueryCase {
    fn new(fixture: &Fixture, scenario: &str) -> Result<Self> {
        let search = search(scenario, fixture.files)?;
        let mut oracle = Oracle::new(fixture, &search);
        while oracle.next_match().is_some() {}
        Ok(Self {
            complete: expected_complete(fixture, oracle.counts),
            counts: counts_json(oracle.counts),
            search,
            scenario: scenario.into(),
        })
    }
}
fn query_on_ready(
    certified: &CertifiedGeneration,
    fixture: &Fixture,
    case: &QueryCase,
    scratch: &Path,
    quota: u64,
    since_open: Instant,
    canonical_control: bool,
) -> Result<Value> {
    let search = &case.search;
    let scenario = &case.scenario;
    let expected_counts = case.counts.clone();
    let expected_complete = case.complete;
    let open_and_certification_ms = milliseconds(since_open);
    let intention = Instant::now();
    let allocation = Instant::now();
    let mut query = if canonical_control {
        certified.as_store().start_query(
            certified.generation(),
            certified.root(),
            search,
            scratch,
            quota,
        )?
    } else {
        certified.start_query(search, scratch, quota)?
    };
    let query_allocation_ms = milliseconds(allocation);
    let mut evaluation_ms = 0.0;
    let mut conversion_ms = 0.0;
    loop {
        let phase = query.preparation_phase();
        let started = Instant::now();
        let done = query.advance(&CancellationToken::default())?;
        let elapsed = milliseconds(started);
        if phase == "evaluation" {
            evaluation_ms += elapsed;
        } else {
            conversion_ms += elapsed;
        }
        if done {
            break;
        }
    }
    let exhaustive_and_result_sync_ms = milliseconds(intention);
    let temporary = query.temporary_usage();
    assert_eq!(counts_json(query.counts), expected_counts);
    assert_eq!(query.complete(), expected_complete);
    let mut result = audit_pages(
        fixture,
        search,
        expected_counts,
        since_open,
        intention,
        |cursor| Ok(query.page(cursor)?),
    )?;
    result["action"] = json!("run");
    result["architecture"] = json!(if canonical_control {
        "generation-canonical"
    } else {
        "generation"
    });
    result["query_source"] = json!(if canonical_control {
        "canonical_entry_id_control"
    } else {
        "persistent_native_rank"
    });
    result["production_repository"] = json!(true);
    result["query_allocation_ms"] = json!(query_allocation_ms);
    result["evaluation_ms"] = json!(evaluation_ms);
    result["conversion_and_sync_ms"] = json!(conversion_ms);
    result["conversion_or_reference_promotion_ms"] = json!(conversion_ms);
    result["scenario"] = json!(scenario);
    result["open_and_certification_ms"] = json!(open_and_certification_ms);
    result["exhaustive_and_result_sync_ms"] = json!(exhaustive_and_result_sync_ms);
    result["complete"] = json!(query.complete());
    result["expected_complete"] = json!(expected_complete);
    result["quota_bytes"] = json!(quota);
    result["temporary_charged_bytes"] = json!(temporary.charged);
    result["temporary_peak_bytes"] = json!(temporary.peak);
    result["peak_process_bytes"] = json!(peak_memory());
    result["indexed"] = json!(query.indexed());
    result["predicate_evaluated"] = json!(query.predicate_evaluated());
    result["references_generation"] = json!(query.references_generation());
    result["query_working_memory_bytes"] = json!(query.working_memory_bytes());
    result["result_storage"] = json!(if query.indexed() {
        "durable_persistent_view"
    } else if query.references_generation() {
        "certified_reference_view"
    } else {
        "new_synced_sequence"
    });
    result["new_result_sync_required"] = json!(query.new_result_sync_required());
    result["governor_usage"] = governor_usage();
    Ok(result)
}

fn digest_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut block = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        digest.update(&block[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
struct ConcurrentReader {
    fixture: Fixture,
    repository: PathBuf,
    scratch: PathBuf,
    quota: u64,
    general: String,
    plan: std::sync::Arc<std::sync::Mutex<Pipeline>>,
    ready: std::sync::mpsc::SyncSender<()>,
    start: std::sync::Arc<std::sync::Barrier>,
    done: std::sync::mpsc::Receiver<()>,
    batches: std::sync::Arc<std::sync::atomic::AtomicU64>,
    active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    epoch: Instant,
}
impl ConcurrentReader {
    fn run(self) -> Result<Value> {
        use std::sync::atomic::Ordering;
        // Oracles finish before the writer starts. No hidden model priming or
        // oracle work is charged to an individual READY interval.
        let cases = [
            QueryCase::new(&self.fixture, "all")?,
            QueryCase::new(&self.fixture, &self.general)?,
        ];
        std::fs::create_dir_all(&self.scratch)?;
        for (index, _) in cases.iter().enumerate() {
            std::fs::create_dir(self.scratch.join(index.to_string()))?;
        }
        let since_open = Instant::now();
        let repository = Repository::open_readonly(&self.repository)?;
        let certified = {
            let mut plan = self.plan.lock().map_err(|_| "pipeline lock poisoned")?;
            repository
                .open_latest_with_plan(&mut plan, &CancellationToken::default())?
                .ok_or("missing G0")?
        };
        assert_eq!(certified.generation(), GenerationId(1));
        let open_and_certification_ms = milliseconds(since_open);
        drop(repository);
        self.ready.send(())?;
        self.start.wait();
        let mut queries = Vec::new();
        for (index, case) in cases.iter().enumerate() {
            let before = self.batches.load(Ordering::Acquire);
            let active_before = self.active.load(Ordering::Acquire);
            let started_ms = milliseconds(self.epoch);
            // The cap was already READY before the overlap experiment. Preserve
            // its real certification measurement separately.
            let mut report = query_on_ready(
                &certified,
                &self.fixture,
                case,
                &self.scratch.join(index.to_string()),
                self.quota,
                since_open,
                false,
            )?;
            report["read_started_ms"] = json!(started_ms);
            report["read_full_audit_finished_ms"] = json!(milliseconds(self.epoch));
            report["writer_batches_before"] = json!(before);
            report["writer_batches_after"] = json!(self.batches.load(Ordering::Acquire));
            report["writer_active_at_start"] = json!(active_before);
            report["writer_active_at_end"] = json!(self.active.load(Ordering::Acquire));
            report["first_query_after_open"] = json!(index == 0);
            report["pinned_generation"] = json!(certified.generation().0);
            report["open_and_certification_ms"] = json!(open_and_certification_ms);
            queries.push(report);
        }
        self.done.recv()?;
        // Publication of G1 does not relabel or invalidate this G0 capability.
        let retained = certified.query(
            &Default::default(),
            &self.scratch,
            self.quota,
            &CancellationToken::default(),
        )?;
        assert_eq!(retained.counts.matches, self.fixture.files + 1);
        assert_eq!(retained.page(None)?.rows[0].key.generation, GenerationId(1));
        Ok(
            json!({"queries":queries,"g0_retained_after_g1_publication":true,"reader_open_and_certification_ms":open_and_certification_ms}),
        )
    }
}
fn concurrent(directory: &Path, files: u64, general: &str, quota: u64) -> Result<Value> {
    use std::sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    };
    if !matches!(
        general,
        "substring_rare" | "substring_absent" | "glob" | "combined_unknowns"
    ) {
        return Err("concurrent requires a general scenario".into());
    }
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(directory.join("fixture.json"))?)?;
    if fixture.files != files || files == 0 {
        return Err("concurrent fixture size mismatch".into());
    }
    let repository = Repository::open_readonly(&directory.join("published"))?;
    let descriptor = repository.latest_context()?.ok_or("missing published G0")?;
    if descriptor.generation != GenerationId(1) {
        return Err("concurrent requires fresh G0-only repository".into());
    }
    let g0_file = descriptor.directory.join("inventory.db");
    let before = digest_file(&g0_file)?;
    drop(repository);
    // Reconcile/prune repository candidates before reserving every pipeline
    // slot. Context allocation needs its own bounded verification workspace.
    let mut repository = Repository::open_existing(&directory.join("published"))?;
    let c = repository.next_context(context())?;
    assert_eq!(c.generation, GenerationId(2));
    let plan = Arc::new(Mutex::new(Pipeline::plan()?));
    let start = Arc::new(Barrier::new(2));
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (done_tx, done_rx) = mpsc::sync_channel(0);
    let batches = Arc::new(AtomicU64::new(0));
    let active = Arc::new(AtomicBool::new(false));
    let epoch = Instant::now();
    let job = ConcurrentReader {
        fixture: fixture.clone(),
        repository: directory.join("published"),
        scratch: directory.join("concurrent-scratch"),
        quota,
        general: general.into(),
        plan: plan.clone(),
        ready: ready_tx,
        start: start.clone(),
        done: done_rx,
        batches: batches.clone(),
        active: active.clone(),
        epoch,
    };
    let reader = std::thread::spawn(move || job.run().map_err(|error| error.to_string()));
    ready_rx.recv()?;
    let mut store = {
        let mut plan = plan.lock().map_err(|_| "pipeline lock poisoned")?;
        repository.create_candidate(c, &mut plan)?
    };
    let attempt = AttemptKey {
        task: TaskKey {
            context: c,
            directory: EntryId(0),
            kind: TaskKind::EnumerateDirect,
        },
        attempt: AttemptId(1),
        owner: OwnerEpoch(1),
        branch: BranchRevision(1),
    };
    store.begin_attempt(attempt)?;
    let writer_started_ms = milliseconds(epoch);
    let writer_started = Instant::now();
    active.store(true, Ordering::Release);
    let mut sequence = 0;
    for first in (0..files).step_by(256) {
        let rows: Vec<_> = (first..(first + 256).min(files))
            .map(|index| observation(index, fixture.adverse))
            .collect();
        store.ingest(
            BatchHeader {
                attempt,
                sequence,
                end: None,
            },
            &rows,
        )?;
        sequence += 1;
        batches.fetch_add(1, Ordering::AcqRel);
        if first == 0 {
            start.wait();
        }
    }
    let writer_ingest_ms = milliseconds(writer_started);
    let writer_rows_per_second = files as f64 / (writer_ingest_ms / 1000.0);
    store.ingest(
        BatchHeader {
            attempt,
            sequence,
            end: Some(ListingEnd::Exhausted),
        },
        &[],
    )?;
    while !store.reconcile_step(attempt, ComparisonScope::Comparable, 256)? {}
    while !store.consolidate_step(c, 256)? {}
    store.seal(c)?;
    store.record_measurement(c.generation, &fixture.root, 300, 400)?;
    let ingestion_and_seal_ms = milliseconds(writer_started);
    let publication_started = Instant::now();
    let publication_growth_quota_bytes = Repository::publication_budget(&store)?;
    let outcome = {
        let mut plan = plan.lock().map_err(|_| "pipeline lock poisoned")?;
        repository.publish_with_plan(
            &store,
            c.generation,
            &fixture.root,
            publication_growth_quota_bytes,
            &CancellationToken::default(),
            &mut plan,
        )?
    };
    let report = match outcome {
        PublicationOutcome::Committed { report, .. } => report,
        PublicationOutcome::PublishedUncommitted { .. } => {
            return Err("G1 published without durable catalogue commit".into());
        }
    };
    let publication_ms = milliseconds(publication_started);
    let writer_finished_ms = milliseconds(epoch);
    active.store(false, Ordering::Release);
    done_tx.send(())?;
    let read = reader
        .join()
        .map_err(|_| "concurrent reader panicked")?
        .map_err(|error| format!("concurrent reader failed: {error}"))?;
    drop(store);
    drop(plan);
    let latest = repository
        .open_latest(&CancellationToken::default())?
        .ok_or("G1 not selected")?;
    assert_eq!(latest.generation(), GenerationId(2));
    assert_eq!(latest.as_store().entry_count(GenerationId(2))?, files + 1);
    let scratch = directory.join("g1-oracle-scratch");
    std::fs::create_dir(&scratch)?;
    let all = SearchQuery::default();
    let query = latest.query(&all, &scratch, quota, &CancellationToken::default())?;
    let mut oracle = Oracle::new(&fixture, &all);
    let mut cursor = None;
    loop {
        let page = query.page(cursor.as_ref())?;
        oracle.audit_rows_generation(&page.rows, GenerationId(2));
        if !page.more {
            break;
        }
        cursor = page.next;
    }
    assert!(oracle.next_match().is_none());
    assert_eq!(counts_json(query.counts), counts_json(oracle.counts));
    assert_eq!(query.complete(), expected_complete(&fixture, oracle.counts));
    drop(query);
    drop(latest);
    assert_eq!(before, digest_file(&g0_file)?);
    let queries = read["queries"].as_array().ok_or("missing reader metrics")?;
    let full_reads_overlap = queries.iter().all(|query| {
        let started = query["read_started_ms"].as_f64().unwrap();
        let ended = query["read_full_audit_finished_ms"].as_f64().unwrap();
        started < writer_finished_ms && ended > writer_started_ms
    });
    let writer_progress_during_reads = queries.iter().any(|query| {
        query["writer_batches_after"].as_u64().unwrap()
            > query["writer_batches_before"].as_u64().unwrap()
    });
    let ready_intervals_overlap = queries.iter().all(|query| {
        let started = query["read_started_ms"].as_f64().unwrap();
        let ready = started + query["ready_since_intention_ms"].as_f64().unwrap();
        started < writer_finished_ms && ready > writer_started_ms
    });
    Ok(
        json!({"action":"concurrent","synthetic":true,"production_repository":true,"files":files,"general_scenario":general,"writer_batches":batches.load(Ordering::Acquire),"writer_started_ms":writer_started_ms,"writer_finished_ms":writer_finished_ms,"writer_ingestion_and_seal_ms":ingestion_and_seal_ms,"writer_ingest_ms":writer_ingest_ms,"writer_rows_per_second":writer_rows_per_second,"writer_publication_ms":publication_ms,"publication_growth_quota_bytes":publication_growth_quota_bytes,"publication_growth_quota_mode":"auto","g1_artifact_bytes":report.artifact_bytes,"g1_selected":true,"g0_sha256_unchanged":before,"reader":read,"both_full_reads_overlap_writer":full_reads_overlap,"both_ready_intervals_overlap_writer":ready_intervals_overlap,"writer_batches_progress_during_full_audits":writer_progress_during_reads,"peak_process_bytes":peak_memory(),"governor_usage":governor_usage(),"query_quota_bytes":quota,"exactness":"PASS","source_io":0}),
    )
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let action = args.next().ok_or("prepare|publish|run")?;
    if action == "prepare" {
        let directory = PathBuf::from(args.next().ok_or("directory")?);
        let files = args.next().ok_or("files")?.parse()?;
        let profile = args.next().ok_or("plain|adverse")?;
        let mode = args.next().ok_or("mode")?;
        configure(&mode)?;
        let adverse = match profile.as_str() {
            "plain" => false,
            "adverse" => true,
            _ => return Err("plain|adverse".into()),
        };
        println!("{}", prepare(&directory, files, adverse)?);
    } else if action == "run" {
        let architecture = args.next().ok_or("baseline|generation")?;
        let directory = PathBuf::from(args.next().ok_or("directory")?);
        let scenario = args.next().ok_or("scenario")?;
        let scratch = PathBuf::from(args.next().ok_or("scratch")?);
        let quota = args.next().ok_or("quota bytes")?.parse()?;
        let mode = args.next().ok_or("mode")?;
        configure(&mode)?;
        let fixture: Fixture =
            serde_json::from_slice(&std::fs::read(directory.join("fixture.json"))?)?;
        let result = match architecture.as_str() {
            "baseline" => run_baseline(&directory, &fixture, &scenario, &scratch, quota)?,
            "generation" => {
                run_generation(&directory, &fixture, &scenario, &scratch, quota, false)?
            }
            "generation-canonical" => {
                if !matches!(
                    scenario.as_str(),
                    "substring_rare" | "substring_absent" | "glob" | "combined_unknowns"
                ) {
                    return Err(
                        "canonical control is limited to general profiling scenarios".into(),
                    );
                }
                run_generation(&directory, &fixture, &scenario, &scratch, quota, true)?
            }
            _ => return Err("baseline|generation".into()),
        };
        println!("{result}");
    } else if action == "concurrent" {
        let directory = PathBuf::from(args.next().ok_or("directory")?);
        let files = args.next().ok_or("files")?.parse()?;
        let scenario = args.next().ok_or("general scenario")?;
        let quota = args.next().ok_or("quota bytes")?.parse()?;
        let mode = args.next().ok_or("mode")?;
        configure(&mode)?;
        println!("{}", concurrent(&directory, files, &scenario, quota)?);
    } else if action == "publish" {
        let directory = PathBuf::from(args.next().ok_or("directory")?);
        let quota = args.next().ok_or("growth quota bytes|auto")?;
        let mode = args.next().ok_or("mode")?;
        configure(&mode)?;
        println!("{}", publish(&directory, &quota)?);
    } else {
        return Err("prepare|publish|run".into());
    }
    Ok(())
}

fn configure(mode: &str) -> Result<()> {
    let selected = MemoryMode::from_key(mode).ok_or("invalid memory mode")?;
    sortsize_core::resources::Governor::global()
        .configure(selected, sortsize_core::resources::native_memory());
    Ok(())
}
