//! Targeted native-window probe. No source acquisition, filesystem oracle,
//! query priming, governor override, or L17 workload is performed here.
use super::*;
use i_slint_backend_winit::presentation_probe as paint;
use serde_json::{Value, json};
use slint::{Model, Timer, TimerMode};
use sortsize_store::inventory::{Kind, queries::QueryCounts};
use std::{
    io::Write,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const CLASSES: [&str; 4] = ["all", "size_late", "size_absent", "extension"];
const REPETITIONS: usize = 3;
const PAGE_BOUND: usize = 200;
const PRODUCT_POLL_MS: u64 = 50;

#[derive(Clone)]
struct Fixture {
    files: u64,
    root: PathBuf,
}
impl Fixture {
    fn read(path: &Path) -> Result<Self> {
        if std::fs::metadata(path)?.len() > 64 * 1024 {
            return Err("fixture metadata exceeds 64 KiB".into());
        }
        let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        if value["adverse"].as_bool() != Some(false) {
            return Err("only the frozen plain fixture is supported".into());
        }
        let files = value["files"]
            .as_u64()
            .filter(|n| *n > 0 && *n < i64::MAX as u64 - 50_000)
            .ok_or("invalid fixture file count")?;
        Ok(Self {
            files,
            root: PathBuf::from(value["root"].as_str().ok_or("fixture root")?),
        })
    }
    fn matches(&self, class: &str) -> u64 {
        match class {
            "all" => self.files + 1,
            "size_late" => 1,
            "size_absent" => 0,
            "extension" => self.files - self.files.div_ceil(5),
            _ => unreachable!(),
        }
    }
    fn expected_complete(class: &str) -> bool {
        matches!(class, "all" | "extension")
    }
    fn prefix(&self, class: &str) -> Vec<(u64, PathBuf, Kind, Option<u64>)> {
        let file = |index: u64| {
            (
                index + 1,
                self.root.join(format!(
                    "file-{index:010}.{}",
                    if index.is_multiple_of(5) {
                        "log"
                    } else {
                        "txt"
                    }
                )),
                Kind::File,
                Some(index),
            )
        };
        match class {
            "all" => std::iter::once((0, self.root.clone(), Kind::Directory, None))
                .chain((0..self.files).map(file))
                .take(PAGE_BOUND)
                .collect(),
            "size_late" => vec![file(self.files - 1)],
            "size_absent" => vec![],
            "extension" => (0..self.files)
                .filter(|i| !i.is_multiple_of(5))
                .map(file)
                .take(PAGE_BOUND)
                .collect(),
            _ => unreachable!(),
        }
    }
    fn validate(
        &self,
        class: &str,
        p: &search::Prepared,
        context: AcquisitionContext,
        w: &SortSizeWindow,
    ) -> std::result::Result<(), String> {
        let expected = self.prefix(class);
        if p.source_context != Some(context) || !p.indexed || p.busy || p.token <= 0 {
            return Err("uncertified context, non-indexed query, or non-terminal state".into());
        }
        let c = p.counts;
        let unknown_sizes = u64::from(!Self::expected_complete(class));
        if c.evaluated != self.files + 1
            || c.matches != self.matches(class)
            || c.unknown_sizes != unknown_sizes
            || c.unknown_dates != 0
            || c.native_text != 0
            || c.oversized_paths != 0
            || c.unknown_kinds != 0
            || c.cancelled
        {
            return Err(format!("raw counts disagree with fixture: {c:?}"));
        }
        if p.complete != Self::expected_complete(class)
            || p.matches != c.matches
            || p.rows.len() != expected.len()
            || p.previous
            || p.next != (c.matches > expected.len() as u64)
        {
            return Err("coverage or first-page cardinality disagrees with fixture".into());
        }
        for (row, (id, path, kind, logical)) in p.rows.iter().zip(expected) {
            if row.key.inventory != context.inventory
                || row.key.generation != context.generation
                || row.key.entry.0 != id
                || row.path != path
                || row.kind != kind
                || row.logical != logical
            {
                return Err(format!("native prefix disagrees at entry {id}"));
            }
        }
        if w.get_search_token() != p.token
            || w.get_search_busy()
            || w.get_search_complete() != p.complete
            || w.get_search_matches().as_str() != c.matches.to_string()
            || w.get_search_results().row_count() != p.rows.len()
        {
            return Err("actual applied Slint model does not match the service publication".into());
        }
        for (index, expected) in p.model.iter().enumerate() {
            if w.get_search_results().row_data(index).is_none_or(|row| {
                row.id != expected.id || row.label != expected.label || row.size != expected.size
            }) {
                return Err("applied row presentation differs from the published model".into());
            }
        }
        Ok(())
    }
}
fn counts(c: QueryCounts) -> Value {
    json!({"evaluated":c.evaluated,"matches":c.matches,"unknown_sizes":c.unknown_sizes,"unknown_dates":c.unknown_dates,"native_text":c.native_text,"oversized_paths":c.oversized_paths,"unknown_kinds":c.unknown_kinds,"cancelled":c.cancelled})
}
fn resources() -> Value {
    let usage = sortsize_core::resources::Governor::global().usage();
    json!({"requested_mode":usage.limits.requested.key(),"effective_limit_bytes":usage.limits.bytes,"effective_entry_limit":usage.limits.entries,"fallback":usage.limits.fallback,"category_accounted_bytes":usage.categories,"category_limit_bytes":usage.limits.categories,"disk_used_bytes":usage.disk_used,"disk_reserved_bytes":usage.disk_reserved,"disk_limit_bytes":usage.disk_limit,"process_memory":process_memory(),"process_rss_128mib_is_not_a_gui_qualification":true})
}
#[cfg(target_os = "linux")]
fn process_memory() -> Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let value = |name: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
                .map(|n| n * 1024)
        })
    };
    json!({"current_rss_bytes":value("VmRSS:"),"peak_process_bytes":value("VmHWM:"),"method":"proc-self-status"})
}
#[cfg(windows)]
fn process_memory() -> Value {
    #[repr(C)]
    struct Counters {
        cb: u32,
        faults: u32,
        peak: usize,
        current: usize,
        pool_peak: usize,
        pool: usize,
        nonpool_peak: usize,
        nonpool: usize,
        pagefile: usize,
        pagefile_peak: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            bytes: u32,
        ) -> i32;
    }
    // SAFETY: pseudo-handle for this process, initialized C ABI output structure.
    unsafe {
        let mut c: Counters = std::mem::zeroed();
        c.cb = std::mem::size_of::<Counters>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            return json!({"method":"GetProcessMemoryInfo","unavailable":true});
        }
        json!({"current_rss_bytes":c.current,"peak_process_bytes":c.peak,"method":"GetProcessMemoryInfo"})
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
fn process_memory() -> Value {
    json!({"unavailable":true})
}
fn configure(w: &SortSizeWindow, class: &str, files: u64) {
    w.set_search_name("".into());
    w.set_search_path("".into());
    w.set_search_glob("".into());
    w.set_search_extension(if class == "extension" { ".TXT" } else { "" }.into());
    w.set_search_case(false);
    let size = match class {
        "size_late" => Some(files - 1),
        "size_absent" => Some(files + 1),
        _ => None,
    }
    .map(|n| n.to_string())
    .unwrap_or_default();
    w.set_search_min(size.clone().into());
    w.set_search_max(size.into());
}
fn write_report(out: &Path, report: &Value) -> std::io::Result<()> {
    let mut file = std::fs::File::create(out.join("native-cached-a.json"))?;
    file.write_all(&serde_json::to_vec_pretty(report)?)?;
    file.sync_all()
}
fn capture(w: &SortSizeWindow, out: &Path) -> Result<Value> {
    let begin = Instant::now();
    let pixels = w.window().take_snapshot()?;
    let snapshot_paint_ms = begin.elapsed().as_secs_f64() * 1000.;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("native-cached-a.ppm"))?;
    write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height())?;
    for pixel in pixels.as_slice() {
        file.write_all(&[pixel.r, pixel.g, pixel.b])?;
    }
    file.sync_all()?;
    Ok(
        json!({"snapshot_paint_ms":snapshot_paint_ms,"width":pixels.width(),"height":pixels.height(),"path":"native-cached-a.ppm","file_write_excluded_from_paint":true}),
    )
}

/// REPOSITORY is a prepared G1 family, never a source directory. Fixture facts
/// describe generation_a_probe plain metadata; no path in that source is read.
fn run_inner(
    repository: PathBuf,
    fixture_path: PathBuf,
    profile: &str,
    out: PathBuf,
) -> Result<()> {
    let mode = MemoryMode::from_key(profile)
        .ok_or("profile must be auto, 50000, 250000, 1000000, 5000000 or unlimited-count")?;
    std::fs::create_dir_all(&out)?;
    if out.join("native-cached-a.json").exists() {
        return Err("output report already exists; preserve this cohort".into());
    }
    let fixture = Fixture::read(&fixture_path)?;
    let mut journal = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("native-cached-a-trials.jsonl"))?;
    let report = Rc::new(RefCell::new(
        json!({"schema":1,"status":"RUNNING","os":std::env::consts::OS,"repository":repository,"fixture":fixture_path,"files":fixture.files,"expected_entries":fixture.files+1,"expected_logical_bytes":u128::from(fixture.files)*u128::from(fixture.files-1)/2,"fixture_root":fixture.root,"profile":mode.key(),"poll_ms":PRODUCT_POLL_MS,"publication_wake":true,"apply_timestamp":"actual Host model apply, with separate timer observation","native_phase_marker":"set by actual Host model apply before oracle and heartbeat observation","oracle_and_resources_after_present":true,"repetitions":REPETITIONS,"kind":"UNAVAILABLE_UI_FILTER","ui_classes":CLASSES,"source_acquisition_invoked":false,"source_availability_tested":false,"source_io_zero_is_code_property_not_instrumented":true,"query_priming":false,"fixture_bootstrap":"read-only catalogue worker; no Refresh callback and no retention purge","physical_input":false,"native_window":true,"compositor_scanout_measured":false,"ui_complete_ready_budget_ms":100,"partial_size_queries_excluded_from_complete_ready_claim":true,"governor_override":false,"startup":{},"trials":[]}),
    ));
    write_report(&out, &report.borrow())?;
    let begin = Instant::now();
    paint::arm();
    paint::phase(1);
    let (w, c) = crate::new_session(Default::default(), Some("en".into()))?;
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c)?;
    h.borrow_mut().native_probe_phases = true;
    h.borrow().service.lane.lock().unwrap().cache_root = Some(repository.clone());
    w.invoke_memory_select(search::MODES.iter().position(|m| *m == mode).unwrap() as i32);
    w.window().set_size(slint::PhysicalSize::new(1280, 900));
    w.set_cache_visible(true);
    let weak = w.as_weak();
    let state = report.clone();
    let timer = Timer::default();
    let mut stage = 0;
    let mut at = Instant::now();
    let mut intent = Instant::now();
    let mut trial = 0;
    let mut expected_context = None;
    let mut open_id = 0;
    let mut pending = None::<(u64, i32, f64, f64, f64)>;
    let mut previous_tick = Instant::now();
    let mut heartbeat_max_ms = 0_f64;
    let output = out.clone();
    let (bootstrap_tx, bootstrap_rx) = std::sync::mpsc::sync_channel(1);
    timer.start(TimerMode::Repeated, Duration::from_millis(PRODUCT_POLL_MS), move || {
        if stage == 9 { return; }
        let result = (|| -> std::result::Result<(), String> {
            let w = weak.upgrade().ok_or("native window disappeared")?;
            heartbeat_max_ms = heartbeat_max_ms.max(previous_tick.elapsed().as_secs_f64() * 1000.); previous_tick = Instant::now();
            c.borrow_mut().poll(&w); h.borrow_mut().poll(&w, &c.borrow());
            if begin.elapsed() > Duration::from_secs(600) { return Err(format!("overall timeout at stage {stage}")); }
            if !w.get_errors().is_empty() || !w.get_cache_diagnostic().is_empty() { return Err(format!("product error: {} / {}", w.get_errors(), w.get_cache_diagnostic())); }
            { let host = h.borrow(); let lane = host.service.lane.lock().unwrap(); if lane.writer_owns_session || lane.writer_quanta != 0 { return Err("unexpected source acquisition".into()); } }
            match stage {
                0 => {
                    if let Some(sample) = paint::samples().iter().find(|sample| sample.0 == 1)
                        && sortsize_core::resources::Governor::global().usage().limits.requested == mode {
                        state.borrow_mut()["startup"]["window_to_first_native_present_ms"] = json!(sample.1);
                        state.borrow_mut()["startup"]["resources_before_catalogue"] = resources();
                        paint::disarm(); at = Instant::now();
                        let mut host = h.borrow_mut(); host.cache_id += 1; host.cache_wait = Some(host.epoch);
                        let request = cache::Request { id:host.cache_id, epoch:host.epoch, expected:host.epoch, action:cache::Action::Refresh, cancel:Default::default() };
                        let root = repository.clone(); let tx = bootstrap_tx.clone(); let presentation = host.presentation;
                        std::thread::Builder::new().name("cached-a-fixture-bootstrap".into()).spawn(move || {
                            let _scope = presentation.enter(); let begin = Instant::now();
                            let catalogue = cache::readonly_fixture_catalogue(&request,&root).map_err(|e|e.to_string());
                            let _ = tx.send((begin.elapsed().as_secs_f64()*1000.,catalogue));
                        }).map_err(|e|e.to_string())?;
                        stage = 1;
                    }
                }
                1 => {
                    if let Ok((ms,catalogue)) = bootstrap_rx.try_recv() {
                        state.borrow_mut()["startup"]["fixture_bootstrap_readonly_worker_ms"] = json!(ms);
                        *h.borrow().service.cache_output.lock().unwrap() = Some(catalogue?);
                    }
                    let host = h.borrow();
                    if let Some(catalogue) = host.cache_current.as_ref().filter(|p| p.id == host.cache_id) {
                        let (index, saved) = catalogue.catalogue.iter().enumerate().find(|(_, s)| s.context.generation == GenerationId(1)).ok_or("saved G1 missing from repository")?;
                        let total = fixture.files.checked_mul(fixture.files-1).and_then(|n| n.checked_div(2));
                        if saved.root != fixture.root || saved.logical != total || saved.measured.is_none() { return Err("saved G1 header differs from fixture facts".into()); }
                        expected_context = Some(saved.context);
                        state.borrow_mut()["startup"]["fixture_bootstrap_to_catalogue_applied_ms"] = json!(at.elapsed().as_secs_f64()*1000.);
                        drop(host); paint::arm(); intent = Instant::now(); w.invoke_cache_action(2, index as i32, w.get_cache_token());
                        state.borrow_mut()["startup"]["cache_open_callback_return_ms"] = json!(intent.elapsed().as_secs_f64()*1000.);
                        if !w.get_busy() || !w.get_cache_visible() || w.get_cache_status().is_empty() || h.borrow().cache_wait != Some(h.borrow().epoch) { return Err("open callback did not provide a useful waiting state for this intention".into()); }
                        paint::phase(4); open_id = h.borrow().cache_id; stage = 2;
                    }
                }
                2 => {
                    if let Some(sample) = paint::samples().iter().find(|sample| sample.0 == 4) {
                        state.borrow_mut()["startup"]["cache_open_callback_to_useful_waiting_native_present_ms"] = json!(sample.1);
                    }
                    let host = h.borrow();
                    if let Some(page) = host.current.as_ref().filter(|p| p.epoch == host.epoch && p.measure_ready && !p.busy && !p.frame.provisional) {
                        if page.frame.stamp.context.inventory != expected_context.unwrap().inventory || page.frame.stamp.context.generation != GenerationId(1) { return Err("cache applied a foreign generation".into()); }
                        let lane = host.service.lane.lock().unwrap(); let (id, cap_at, worker_ms) = lane.cache_open_timing.ok_or("cache worker timing missing")?;
                        if id != open_id { return Err("cache worker timing belongs to a different request".into()); }
                        state.borrow_mut()["startup"]["worker_open_and_full_certification_ms"] = json!(worker_ms);
                        state.borrow_mut()["startup"]["certification_to_worker_navigation_frame_ms"] = json!(page.worker_frame_ready_at.saturating_duration_since(cap_at).as_secs_f64()*1000.);
                        state.borrow_mut()["startup"]["cache_open_callback_to_navigation_applied_ms"] = json!(host.navigation_applied_at.ok_or("actual navigation apply timestamp missing")?.saturating_duration_since(intent).as_secs_f64()*1000.);
                        state.borrow_mut()["startup"]["cache_open_callback_to_navigation_observed_ms"] = json!(intent.elapsed().as_secs_f64()*1000.);
                        state.borrow_mut()["startup"]["navigation_apply_ms"] = json!(host.last_apply_ms);
                        state.borrow_mut()["startup"]["probe_start_to_navigation_applied_ms"] = json!(host.navigation_applied_at.unwrap().saturating_duration_since(begin).as_secs_f64()*1000.);
                        stage = 3;
                    }
                }
                3 => {
                    if let Some(sample) = paint::samples().iter().find(|sample| sample.0 == 2) {
                        state.borrow_mut()["startup"]["cache_open_callback_to_navigation_native_present_ms"] = json!(sample.1);
                        state.borrow_mut()["startup"]["resources_after_cache_ready"] = resources();
                        paint::disarm(); w.invoke_show_analysis(); stage = 4;
                    }
                }
                4 => {
                    let class = CLASSES[trial % CLASSES.len()]; configure(&w, class, fixture.files);
                    paint::arm(); intent = Instant::now(); w.invoke_search_submit(); at = Instant::now(); stage = 5;
                }
                5 => {
                    if at.elapsed() > Duration::from_secs(60) { return Err("query terminal timeout".into()); }
                    let host = h.borrow();
                    if let Some(p) = host.search_current.as_ref().filter(|p| p.id == host.search_id && !p.busy && p.token == 0) { return Err(format!("query refused: {}",p.status)); }
                    if let Some(p) = host.search_current.as_ref().filter(|p| p.id == host.search_id && !p.busy && p.token > 0) {
                        let observed_ms = intent.elapsed().as_secs_f64()*1000.;
                        let applied_ms = host.search_applied_at.ok_or("actual search apply timestamp missing")?.saturating_duration_since(intent).as_secs_f64()*1000.;
                        // Capture only the observed identity/timestamps here.
                        // Native rendering must not wait for fixture validation,
                        // raw-row serialization, resource reads or journal I/O.
                        pending = Some((p.id, p.token, applied_ms, observed_ms, host.search_apply_ms));
                        at = Instant::now(); stage = 6;
                    }
                }
                6 => {
                    if at.elapsed() > Duration::from_secs(5) { return Err("terminal model was applied but its native presentation phase was not observed".into()); }
                    if let Some(sample) = paint::samples().iter().find(|sample| sample.0 == 3) {
                        let (id, token, applied_ms, observed_ms, apply_ms) = pending.take().unwrap();
                        let host = h.borrow();
                        let p = host.search_current.as_ref().filter(|p| p.id == id && p.token == token && !p.busy)
                            .ok_or("terminal identity changed between observation and native presentation oracle")?;
                        let class = CLASSES[trial % CLASSES.len()];
                        let validation = fixture.validate(class, p, expected_context.unwrap(), &w);
                        let complete = Fixture::expected_complete(class);
                        let terminal = json!({"trial":trial,"repetition":trial/CLASSES.len()+1,"class":class,"criterion":{"kind":null,"min_bytes":if class=="size_late" {Some(fixture.files-1)} else if class=="size_absent" {Some(fixture.files+1)} else {None},"max_equals_min":matches!(class,"size_late"|"size_absent"),"extension":if class=="extension" {Some(".TXT")} else {None}},"classification":if validation.is_err() {"ERROR_UI_ORACLE"} else if complete {"COMPLETE_EXACT_UI"} else {"TERMINAL_PARTIAL_UI"},"callback_to_terminal_applied_ms":applied_ms,"callback_to_terminal_observed_ms":observed_ms,"model_apply_ms":apply_ms,"callback_to_first_complete_exact_page_applied_ms":if complete && validation.is_ok() {Some(applied_ms)} else {None},"complete_ready_100ms_pass":if complete && validation.is_ok() {Some(applied_ms<100.)} else {None},"expected_complete":complete,"actual_complete":p.complete,"counts":counts(p.counts),"indexed":p.indexed,"predicate_evaluated":p.predicate_evaluated,"raw_rows":p.rows.iter().map(|r|json!({"inventory":r.key.inventory.0.to_string(),"generation":r.key.generation.0,"entry":r.key.entry.0,"path":r.path,"kind":format!("{:?}",r.kind),"logical":r.logical})).collect::<Vec<_>>(),"page_bound":PAGE_BOUND,"more":p.next,"native_prefix_oracle":if validation.is_ok() {"PASS"} else {"FAIL"},"resources":resources(),"callback_to_native_present_ms":sample.1,"native_presented_frame_count":sample.2,"oracle_after_native_present":true});
                        writeln!(journal,"{terminal}").map_err(|e|e.to_string())?; journal.sync_all().map_err(|e|e.to_string())?;
                        state.borrow_mut()["trials"].as_array_mut().unwrap().push(terminal); write_report(&output,&state.borrow()).map_err(|e|e.to_string())?;
                        validation?;
                        paint::disarm(); trial += 1;
                        stage = if trial == CLASSES.len()*REPETITIONS {7} else {4};
                    }
                }
                7 => {
                    let snapshot = capture(&w,&output).map_err(|e|e.to_string())?;
                    let mut report = state.borrow_mut(); report["capture"] = snapshot; report["heartbeat_max_ms"] = json!(heartbeat_max_ms); report["resources_at_end"] = resources();
                    let threshold = report["trials"].as_array().unwrap().iter().filter_map(|t|t["complete_ready_100ms_pass"].as_bool()).all(|pass|pass);
                    report["complete_ui_ready_threshold"] = json!(if threshold {"PASS"} else {"FAIL"}); report["status"] = json!("COMPLETE");
                    write_report(&output,&report).map_err(|e|e.to_string())?; paint::disarm(); slint::quit_event_loop().map_err(|e|e.to_string())?; stage = 9;
                }
                _ => (),
            }
            Ok(())
        })();
        if let Err(error) = result { let mut report = state.borrow_mut(); report["status"] = json!("ERROR"); report["error"] = json!(error); report["stage"] = json!(stage); report["attempted_trial"] = json!(trial); report["attempted_class"] = json!(CLASSES[trial.min(CLASSES.len()*REPETITIONS-1)%CLASSES.len()]); report["elapsed_since_callback_ms"] = json!(intent.elapsed().as_secs_f64()*1000.); report["resources_at_error"] = resources(); let _ = write_report(&output,&report); paint::disarm(); let _ = slint::quit_event_loop(); stage = 9; }
    });
    w.run()?;
    timer.stop();
    paint::disarm();
    if report.borrow()["status"] != "COMPLETE" {
        return Err(format!("native cached A probe failed: {}", report.borrow()["error"]).into());
    }
    Ok(())
}

/// Fixed, small metadata-only native smoke input. Never used for scale claims.
pub fn prepare_smoke(directory: PathBuf) -> Result<()> {
    use sortsize_store::{
        Durability,
        codec::NativeName,
        inventory::{
            Observation, Store,
            generation_a::{Pipeline, Repository},
        },
    };
    std::fs::create_dir(&directory)?;
    let root = if cfg!(windows) {
        PathBuf::from("Q:/generation-probe/offline")
    } else {
        PathBuf::from("/generation-probe/offline")
    };
    let c = AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut store = Store::create(
        &directory.join("canonical"),
        c.inventory,
        c.source,
        Durability::Retained,
    )?;
    let now = super::cache::seconds(std::time::SystemTime::now());
    store.create_generation(c)?;
    store.record_acquisition(c, &root, now)?;
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
    let rows = (0..257)
        .map(|index: u64| {
            let name = format!(
                "file-{index:010}.{}",
                if index.is_multiple_of(5) {
                    "log"
                } else {
                    "txt"
                }
            );
            #[cfg(unix)]
            let name = NativeName::Posix(name.into_bytes());
            #[cfg(windows)]
            let name = NativeName::Windows(name.encode_utf16().collect());
            #[cfg(not(any(unix, windows)))]
            let name = NativeName::Opaque(name.into_bytes());
            Observation {
                name,
                kind: Kind::File,
                logical: Some(index),
                allocated: None,
                modified: Some((index as i64 - 50_000, 5)),
                metadata_known: true,
            }
        })
        .collect::<Vec<_>>();
    store.ingest(
        BatchHeader {
            attempt,
            sequence: 0,
            end: None,
        },
        &rows,
    )?;
    store.ingest(
        BatchHeader {
            attempt,
            sequence: 1,
            end: Some(ListingEnd::Exhausted),
        },
        &[],
    )?;
    while !store.reconcile_step(attempt, ComparisonScope::Comparable, 512)? {}
    while !store.consolidate_step(c, 512)? {}
    store.seal(c)?;
    store.record_measurement(c.generation, &root, now, now)?;
    drop(store);
    let store = Store::certify_readonly(&directory.join("canonical"), &Default::default())?;
    let mut repository = Repository::open(&directory.join("repository"), c.inventory, c.source)?;
    let mut plan = Pipeline::plan()?;
    let outcome = repository.adopt_legacy_with_plan(
        &store,
        c.generation,
        &root,
        Repository::publication_budget(&store)?,
        &Default::default(),
        &mut plan,
    )?;
    if !outcome.committed() {
        return Err("smoke publication acknowledgement incomplete".into());
    }
    repository.set_retention(true, 30)?;
    std::fs::write(
        directory.join("fixture.json"),
        serde_json::to_vec_pretty(&json!({"files":257,"adverse":false,"root":root}))?,
    )?;
    Ok(())
}

/// Preserve partial samples and a failure diagnosis when native setup fails.
pub fn run(repository: PathBuf, fixture: PathBuf, mode: &str, out: PathBuf) -> Result<()> {
    let existed = out.join("native-cached-a.json").exists();
    let result = run_inner(repository, fixture, mode, out.clone());
    if let Err(error) = &result
        && !existed
        && out.is_dir()
    {
        let mut report = std::fs::read(out.join("native-cached-a.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or_else(|| json!({"schema":1,"trials":[]}));
        report["status"] = json!("ERROR");
        report["error"] = json!(error.to_string());
        let _ = write_report(&out, &report);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_prefix_and_partial_ui_contract_are_bounded_and_match_the_plain_generator() {
        let fixture = Fixture {
            files: 257,
            root: PathBuf::from("/generation-probe/offline"),
        };
        assert_eq!(fixture.matches("all"), 258);
        assert_eq!(fixture.matches("extension"), 205);
        let all = fixture.prefix("all");
        assert_eq!(all.len(), 200);
        assert_eq!(all[0].0, 0);
        assert_eq!(all[199].0, 199);
        assert_eq!(all[1].1.file_name().unwrap(), "file-0000000000.log");
        let extension = fixture.prefix("extension");
        assert_eq!(extension.len(), 200);
        assert_eq!(extension[0].0, 2);
        assert_eq!(fixture.prefix("size_late")[0].0, 257);
        assert!(fixture.prefix("size_absent").is_empty());
        assert!(!Fixture::expected_complete("size_absent"));
        assert!(!Fixture::expected_complete("size_late"));
        assert!(Fixture::expected_complete("all"));
    }
}
