//! Opt-in development harness. Output is local numeric JSON, never telemetry.
use crate::{Controller, connect, tree::Tree};
use slint::{ComponentHandle, Model, Timer, TimerMode};
use sortsize_core::{
    CancellationToken, Coverage, InventoryLimits, ScanOptions, ScanReport, scan,
    scan_inventory_with_limits,
};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod allocations;
mod date_bench;
#[cfg(any(target_os = "linux", windows))]
mod display_bench;
mod loop_trace;
#[cfg(any(target_os = "linux", windows))]
mod volume_bench;
#[cfg(windows)]
mod windows;
#[cfg(all(windows, feature = "allocation-profiling"))]
mod windows_memory;

fn process_resources() -> Result<String, std::io::Error> {
    #[cfg(windows)]
    {
        windows::snapshot_json()
    }
    #[cfg(not(windows))]
    {
        Ok("null".into())
    }
}

fn owner_acquisition_json() -> String {
    #[cfg(all(windows, feature = "owner-profiling"))]
    {
        format!("{:?}", sortsize_core::owner_profile::snapshot())
    }
    #[cfg(not(all(windows, feature = "owner-profiling")))]
    {
        "null".into()
    }
}

fn coverage(report: &ScanReport) -> &'static str {
    match report.coverage {
        Coverage::Complete => "complete",
        Coverage::Partial => "partial",
        Coverage::Cancelled => "cancelled",
    }
}

/// Usage: sortsize-measure summary|inventory|tree|gui|gui-navigation PATH [cancel_after_ms].
/// The harness does not write to the analysed folder.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args.get(1).and_then(|s| s.to_str()).unwrap_or("");
    let root = args.get(2).ok_or("Missing corpus path")?;
    #[cfg(all(feature = "allocation-profiling", any(target_os = "linux", windows)))]
    if mode == "volume-memory" {
        return volume_bench::memory();
    }
    #[cfg(any(target_os = "linux", windows))]
    if mode == "volume-ui" {
        return volume_bench::native_panel(Path::new(root));
    }
    #[cfg(any(target_os = "linux", windows))]
    if mode == "volume-bench" {
        return volume_bench::run();
    }
    if mode == "date-bench" {
        return date_bench::run(Path::new(root));
    }
    let mut limits = InventoryLimits::default();
    let mut options = ScanOptions::default();
    if let Ok(value) = std::env::var("SORTSIZE_MEASURE_WORKERS") {
        options.concurrency.workers = value.parse()?;
    }
    if let Ok(value) = std::env::var("SORTSIZE_MEASURE_MAX_OPEN_DIRECTORIES") {
        if mode.starts_with("gui") {
            return Err(
                "Native GUI uses product defaults; descriptor sweeps are for engine modes".into(),
            );
        }
        options.max_open_directories = value.parse()?;
    }
    if let Ok(value) = std::env::var("SORTSIZE_MEASURE_PATH_BYTES") {
        if mode.starts_with("gui") {
            return Err(
                "Native GUI uses product defaults; path-budget sweeps are for inventory/tree"
                    .into(),
            );
        }
        limits.max_path_bytes = value.parse()?;
    }
    let cancel_ms = args
        .get(3)
        .map(|s| s.to_string_lossy().parse::<u64>())
        .transpose()?;
    if matches!(mode, "gui" | "gui-navigation") {
        return native(
            Path::new(root),
            cancel_ms,
            mode == "gui-navigation",
            options.concurrency.workers,
        );
    }
    if !matches!(mode, "summary" | "inventory" | "tree") {
        return Err("Expected summary, inventory, tree or gui".into());
    }
    let start = Instant::now();
    let token = CancellationToken::default();
    let requested = Arc::new(Mutex::new(None));
    if let Some(ms) = cancel_ms {
        let token = token.clone();
        let requested = requested.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            *requested.lock().unwrap() = Some(Instant::now());
            token.cancel();
        });
    }
    let mut top_metrics = "null".to_owned();
    let (report, retained, omitted, path_bytes, projection_ms, mut tree) = if mode == "summary" {
        (
            scan(Path::new(root), &options, &token, |_| {})?,
            0,
            0,
            0,
            0.0,
            None,
        )
    } else {
        let acquired = if mode == "tree" {
            Some(crate::worker::acquire_with_owner(
                Path::new(root),
                &options,
                &token,
                limits,
                std::env::var("SORTSIZE_MEASURE_ALLOCATION").as_deref() == Ok("1"),
                std::env::var("SORTSIZE_MEASURE_OWNER").as_deref() == Ok("1"),
                |_| {},
            )?)
        } else {
            None
        };
        let plain = if acquired.is_none() {
            Some(scan_inventory_with_limits(
                Path::new(root),
                &options,
                &token,
                limits,
                |_| {},
            )?)
        } else {
            None
        };
        let inventory = acquired
            .as_ref()
            .map_or_else(|| plain.as_ref().unwrap(), |result| result.inventory());
        top_metrics = format!(
            "{{\"count\":{},\"retained_path_bytes\":{},\"oversized_paths\":{},\"largest\":{},\"smallest\":{}}}",
            inventory.top_files.entries.len(),
            inventory.top_files.retained_path_bytes,
            inventory.top_files.oversized_paths,
            inventory
                .top_files
                .entries
                .first()
                .map_or(0, |f| f.logical_bytes),
            inventory
                .top_files
                .entries
                .last()
                .map_or(0, |f| f.logical_bytes)
        );
        let retained = inventory.entries.len();
        let paths: usize = inventory
            .entries
            .iter()
            .map(|n| n.path.as_os_str().as_encoded_bytes().len())
            .sum();
        let stamp = Instant::now();
        let omitted = inventory.entries_omitted;
        let (tree, report) = if let Some(acquired) = acquired {
            let (tree, report) = acquired.into_tree()?;
            (Some(tree), report)
        } else {
            (None, plain.unwrap().report)
        };
        let projection_ms = stamp.elapsed().as_secs_f64() * 1000.0;
        (report, retained, omitted, paths, projection_ms, tree)
    };
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let cancel_latency = requested
        .lock()
        .unwrap()
        .map(|t| t.elapsed().as_secs_f64() * 1000.0);
    let stats = report.traversal_stats;
    let resources = process_resources()?;
    let map_metrics = if let Some(tree) = &tree {
        let mut samples = Vec::with_capacity(1000);
        let mut tiles = 0;
        for i in 0..1000 {
            let stamp = Instant::now();
            let map = tree.treemap(if i % 2 == 0 { 840.0 } else { 640.0 }, 190.0);
            tiles = map.len();
            std::hint::black_box(map);
            samples.push(stamp.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        format!(
            "{{\"tiles\":{tiles},\"samples\":1000,\"p95_ms\":{:.6},\"max_ms\":{:.6}}}",
            samples[949], samples[999]
        )
    } else {
        "null".into()
    };
    let detail_metrics = tree.as_mut().map_or_else(|| "null".into(), measure_details);
    let allocation_stats = allocations::json();
    let owner_preparation_ms = tree.as_ref().map_or(0.0, |tree| tree.owner_prepare_ms);
    let acquisition = owner_acquisition_json();
    println!(
        "{{\"mode\":\"{mode}\",\"owner_preparation_ms\":{owner_preparation_ms:.6},\"owner_acquisition\":{acquisition},\"treemap_layout\":{map_metrics},\"detail_projection\":{detail_metrics},\"top_files\":{top_metrics},\"files\":{},\"directories\":{},\"logical_bytes\":{},\"coverage\":\"{}\",\"errors\":{},\"resource_limited_directories\":{},\"traversal_stats\":{{\"peak_pending_directories\":{},\"peak_pending_path_bytes\":{},\"peak_open_directories\":{},\"worker_threads\":{},\"peak_buffered_observations\":{},\"peak_buffered_path_bytes\":{}}},\"workers\":{},\"retained\":{retained},\"omitted\":{omitted},\"retained_path_bytes\":{path_bytes},\"scan_ms\":{},\"projection_ms\":{projection_ms:.3},\"wall_ms\":{wall_ms:.3},\"cancel_latency_ms\":{},\"process_resources\":{resources},\"allocation_stats\":{allocation_stats}}}",
        report.files,
        report.directories,
        report.logical_bytes,
        coverage(&report),
        report.errors_count,
        report.resource_limited_directories,
        stats.peak_pending_directories,
        stats.peak_pending_path_bytes,
        stats.peak_open_directories,
        stats.worker_threads,
        stats.peak_buffered_observations,
        stats.peak_buffered_path_bytes,
        report.concurrency.workers,
        report.duration_ms,
        cancel_latency.map_or("null".into(), |ms| format!("{ms:.3}"))
    );
    std::hint::black_box(tree);
    Ok(())
}

// Same harness can be applied to the base to isolate page projection cost.
// No scan or allocation counters are enabled by the ordinary application.
fn measure_details(tree: &mut Tree) -> String {
    // Exercise a retained file's parent, including the long-path corpus rather
    // than measuring only its single root directory row.
    if let Some(index) = (0..100).find_map(|i| tree.top_selection(i).and_then(|(_, node)| node)) {
        tree.select_entry_parent(index);
    }
    let before = allocations::json();
    let (page, _) = tree.details();
    let with_page = allocations::json();
    let rows = page.len();
    let path_label_bytes: usize = page
        .iter()
        .map(|row| {
            tree.entry_path(row.node_id as usize)
                .map_or(0, |path| path.len())
        })
        .sum();
    drop(page);
    let after = allocations::json();
    let mut samples = Vec::with_capacity(101);
    for _ in 0..101 {
        let start = Instant::now();
        let page = tree.details();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(page);
    }
    samples.sort_by(f64::total_cmp);
    format!(
        "{{\"rows\":{rows},\"row_struct_bytes\":{},\"native_path_label_bytes\":{path_label_bytes},\"samples\":101,\"p50_ms\":{:.6},\"p95_ms\":{:.6},\"max_ms\":{:.6},\"before\":{before},\"with_page\":{with_page},\"after_drop\":{after}}}",
        std::mem::size_of::<crate::DetailRow>(),
        samples[50],
        samples[95],
        samples[100]
    )
}

#[derive(Default)]
struct NativeMetrics {
    gaps: Vec<f64>,
    last_tick: Option<Instant>,
    cancel_requested: Option<Instant>,
    cancel_latency_ms: Option<f64>,
    finished: Option<Instant>,
    callback_max_ms: f64,
    poll_max_ms: f64,
    error: bool,
    cancelled: bool,
    navigation_steps: usize,
    treemap_steps: usize,
    top_steps: usize,
    navigation_done: bool,
    tail_actions: usize,
    interactions_finished: Option<Instant>,
    owner_steps: usize,
}

fn native(
    root: &Path,
    cancel_ms: Option<u64>,
    navigate: bool,
    workers: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = Rc::new(RefCell::new(loop_trace::Trace::default()));
    let backend = i_slint_backend_winit::Backend::builder()
        .with_renderer_name("software")
        .with_custom_application_handler(Box::new(loop_trace::Handler(trace.clone())))
        .build()?;
    slint::platform::set_platform(Box::new(backend))?;
    let window = crate::new_window()?;
    if std::env::var_os("SORTSIZE_MEASURE_COMPACT").is_some() {
        window
            .window()
            .set_size(slint::LogicalSize::new(960.0, 600.0));
    }
    let diagnostic_heartbeats = std::env::var_os("SORTSIZE_MEASURE_HEARTBEATS").is_some();
    #[cfg(windows)]
    if diagnostic_heartbeats {
        eprintln!("sortsize-ui-thread:{}", windows::thread_id());
    }
    // Use the same visible choice and controller path as the product.
    window.set_worker_choice(match workers {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        _ => return Err("GUI accepts 1, 2, 4 or 8 workers".into()),
    });
    window.set_scan_path(
        root.to_str()
            .ok_or("GUI benchmark corpus must be Unicode")?
            .into(),
    );
    window
        .set_collect_allocation(std::env::var("SORTSIZE_MEASURE_ALLOCATION").as_deref() == Ok("1"));
    window.set_collect_owner(std::env::var("SORTSIZE_MEASURE_OWNER").as_deref() == Ok("1"));
    window.set_resolve_owner(std::env::var("SORTSIZE_MEASURE_RESOLVE_OWNER").as_deref() == Ok("1"));
    let state: Rc<RefCell<Controller>> = connect(&window);
    #[cfg(any(target_os = "linux", windows))]
    {
        volume_bench::configure(&mut state.borrow_mut(), root);
        state.borrow_mut().refresh_sources(&window, false);
    }
    window.show()?;
    window.invoke_start_scan();
    trace.borrow_mut().phase = "scan";
    let start = Instant::now();
    let metrics = Rc::new(RefCell::new(NativeMetrics::default()));
    let poll = Timer::default();
    let weak = window.as_weak();
    let controller = state.clone();
    let poll_metrics = metrics.clone();
    poll.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        if let Some(window) = weak.upgrade() {
            let stamp = Instant::now();
            controller.borrow_mut().poll(&window);
            let mut metrics = poll_metrics.borrow_mut();
            metrics.poll_max_ms = metrics
                .poll_max_ms
                .max(stamp.elapsed().as_secs_f64() * 1000.0);
        }
    });
    let heartbeat = Timer::default();
    let weak = window.as_weak();
    let shared = metrics.clone();
    let heartbeat_controller = state.clone();
    let heartbeat_trace = trace.clone();
    heartbeat.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let mut metrics = shared.borrow_mut();
        let now = Instant::now();
        if diagnostic_heartbeats {
            eprintln!("sortsize-heartbeat");
        }
        if let Some(last) = metrics.last_tick.replace(now) {
            // Bounded even if an unexpectedly slow filesystem exceeds the deadline.
            if metrics.gaps.len() < 10_000 {
                metrics
                    .gaps
                    .push(now.duration_since(last).as_secs_f64() * 1000.0);
            }
        }
        let Some(window) = weak.upgrade() else {
            return;
        };
        if let Some(ms) = cancel_ms
            && metrics.cancel_requested.is_none()
            && start.elapsed() >= Duration::from_millis(ms)
            && window.get_busy()
        {
            metrics.cancel_requested = Some(Instant::now());
            window.invoke_cancel_scan();
            heartbeat_trace.borrow_mut().phase = "cancelling";
        }
        if !window.get_busy() && metrics.finished.is_none() {
            metrics.cancel_latency_ms = metrics
                .cancel_requested
                .map(|t| t.elapsed().as_secs_f64() * 1000.0);
            let summary = heartbeat_controller.borrow().benchmark_summary;
            metrics.error = summary.is_none();
            metrics.cancelled = summary.is_some_and(|s| s.coverage == Coverage::Cancelled);
            // Exercise the real controller/model on the native event loop. These
            // are callback timings, not end-to-end physical mouse input latency.
            for name in [false, false, true, true] {
                let stamp = Instant::now();
                window.invoke_sort_details(name);
                metrics.callback_max_ms = metrics
                    .callback_max_ms
                    .max(stamp.elapsed().as_secs_f64() * 1000.0);
            }
            if window.get_allocation_sort_available() {
                for _ in 0..2 {
                    let stamp = Instant::now();
                    window.invoke_sort_allocated();
                    metrics.callback_max_ms = metrics
                        .callback_max_ms
                        .max(stamp.elapsed().as_secs_f64() * 1000.0);
                }
            }
            for (accessed, order) in [(false, 8), (false, 9), (true, 10), (true, 11)] {
                let stamp = Instant::now();
                window.invoke_sort_dates(accessed);
                let callback_ms = stamp.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(window.get_sort_order(), order);
                let expected = heartbeat_controller
                    .borrow()
                    .tree
                    .as_ref()
                    .unwrap()
                    .details()
                    .0;
                assert_eq!(
                    window
                        .get_details()
                        .iter()
                        .map(|r| r.node_id)
                        .collect::<Vec<_>>(),
                    expected.iter().map(|r| r.node_id).collect::<Vec<_>>()
                );
                metrics.callback_max_ms = metrics.callback_max_ms.max(callback_ms);
            }
            metrics.finished = Some(Instant::now());
            heartbeat_trace.borrow_mut().phase = if navigate { "navigation" } else { "settle" };
        }
        if navigate && metrics.finished.is_some() && metrics.interactions_finished.is_none() {
            let stamp = Instant::now();
            if !metrics.navigation_done && metrics.navigation_steps < 64 {
                if let Some(index) = window.get_tiles().iter().position(|tile| tile.directory) {
                    window.invoke_activate_tile(index as i32);
                    metrics.navigation_steps += 1;
                    metrics.treemap_steps += 1;
                } else if let Some(folder) = window.get_details().iter().find(|row| row.directory) {
                    window.invoke_select_node(folder.node_id);
                    metrics.navigation_steps += 1;
                } else {
                    metrics.navigation_done = true;
                }
            } else if metrics.tail_actions == 0 {
                if let Some(index) = window
                    .get_tiles()
                    .iter()
                    .position(|tile| tile.node_id >= 0 && !tile.directory)
                {
                    window.invoke_activate_tile(index as i32);
                    metrics.treemap_steps += 1;
                }
                metrics.tail_actions += 1;
            } else if metrics.tail_actions < 4 {
                window.invoke_sort_details(metrics.tail_actions >= 2);
                metrics.tail_actions += 1;
            } else if metrics.tail_actions < 8 {
                let index = metrics.tail_actions - 4;
                window.invoke_change_page(index < 2, index % 2 == 0);
                metrics.tail_actions += 1;
            } else if metrics.tail_actions < 108 && window.get_top_rows().row_count() > 0 {
                let rank = (metrics.tail_actions - 8) % window.get_top_rows().row_count();
                window.set_top_visible(true);
                window.invoke_select_top(rank as i32);
                metrics.top_steps += 1;
                if metrics.tail_actions % 10 == 0 {
                    window.invoke_reveal_top();
                }
                metrics.tail_actions += 1;
            } else {
                window.invoke_go_parent();
                metrics.interactions_finished = Some(Instant::now());
                heartbeat_trace.borrow_mut().phase = "settle";
            }
            metrics.callback_max_ms = metrics
                .callback_max_ms
                .max(stamp.elapsed().as_secs_f64() * 1000.0);
        }
        let ready = if navigate {
            metrics.interactions_finished
        } else {
            metrics.finished
        };
        if metrics.finished.is_some()
            && window.get_owner_sort_available()
            && !window.get_owner_sort_preparing()
            && metrics.owner_steps < 2
        {
            let stamp = Instant::now();
            window.invoke_sort_owner();
            metrics.callback_max_ms = metrics
                .callback_max_ms
                .max(stamp.elapsed().as_secs_f64() * 1000.0);
            metrics.owner_steps += 1;
        }
        if ready.is_some_and(|t| t.elapsed() >= Duration::from_millis(600))
            || start.elapsed() >= Duration::from_secs(120)
        {
            let _ = slint::quit_event_loop();
        }
    });
    slint::run_event_loop()?;
    heartbeat.stop();
    if diagnostic_heartbeats {
        eprintln!("sortsize-heartbeat-done");
    }
    poll.stop();
    state.borrow_mut().task = None;
    let mut m = metrics.borrow_mut();
    m.gaps.sort_by(f64::total_cmp);
    let max = m.gaps.last().copied().unwrap_or(0.0);
    let p95 = m
        .gaps
        .get(m.gaps.len().saturating_sub(1) * 95 / 100)
        .copied()
        .unwrap_or(0.0);
    let mode = if navigate { "gui-navigation" } else { "gui" };
    let scan_summary = state.borrow().benchmark_summary.map_or("null".into(), |s| {
        let coverage = match s.coverage {
            Coverage::Complete => "complete", Coverage::Partial => "partial",
            Coverage::Cancelled => "cancelled",
        };
        format!("{{\"files\":{},\"directories\":{},\"logical_bytes\":{},\"coverage\":\"{coverage}\",\"errors\":{},\"resource_limited_directories\":{},\"scan_ms\":{},\"projection_ms\":{:.3},\"traversal_stats\":{{\"peak_pending_directories\":{},\"peak_pending_path_bytes\":{},\"peak_open_directories\":{},\"worker_threads\":{},\"peak_buffered_observations\":{},\"peak_buffered_path_bytes\":{}}},\"workers\":{}}}", s.files, s.directories, s.logical_bytes, s.errors, s.resource_limited_directories, s.scan_ms, s.projection_ms, s.traversal_stats.peak_pending_directories, s.traversal_stats.peak_pending_path_bytes, s.traversal_stats.peak_open_directories, s.traversal_stats.worker_threads, s.traversal_stats.peak_buffered_observations, s.traversal_stats.peak_buffered_path_bytes, s.concurrency.workers)
    });
    let resources = process_resources()?;
    let loop_trace = trace.borrow().json();
    let allocation_stats = allocations::json();
    let owner_metrics = {
        let acquisition = owner_acquisition_json();
        let controller = state.borrow();
        let stats = controller.owner_stats;
        let (pools, native_calls) = controller
            .owner_task
            .as_ref()
            .map_or((0, 0), |task| task.diagnostics());
        let prepared = controller
            .tree
            .as_ref()
            .map_or(0.0, |tree| tree.owner_prepare_ms);
        format!(
            "{{\"acquisition\":{acquisition},\"pools\":{pools},\"native_api_calls\":{native_calls},\"lookup_calls\":{},\"cache_hits\":{},\"cache_entries\":{},\"cache_bytes\":{},\"peak_pending\":{},\"peak_running\":{},\"initial_preparation_ms\":{prepared:.6},\"sort_preparation_ms\":{:.6},\"patch_max_ms\":{:.6},\"patched_rows\":{}}}",
            stats.lookup_calls,
            stats.cache_hits,
            stats.cache_entries,
            stats.cache_bytes,
            stats.peak_pending,
            stats.peak_running,
            controller.owner_sort_prepare_ms,
            controller.owner_patch_max_ms,
            controller.owner_patch_rows
        )
    };
    println!(
        "{{\"mode\":\"{mode}\",\"owner_metrics\":{owner_metrics},\"heartbeat_samples\":{},\"heartbeat_p95_ms\":{p95:.3},\"heartbeat_max_ms\":{max:.3},\"callback_max_ms\":{:.3},\"poll_max_ms\":{:.3},\"cancel_latency_ms\":{},\"completed\":{},\"error\":{},\"cancelled\":{},\"navigation_steps\":{},\"treemap_steps\":{},\"top_steps\":{},\"scan_summary\":{scan_summary},\"process_resources\":{resources},\"loop_trace\":{loop_trace},\"allocation_stats\":{allocation_stats}}}",
        m.gaps.len(),
        m.callback_max_ms,
        m.poll_max_ms,
        m.cancel_latency_ms
            .map_or("null".into(), |ms| format!("{ms:.3}")),
        m.finished.is_some(),
        m.error,
        m.cancelled,
        m.navigation_steps,
        m.treemap_steps,
        m.top_steps
    );
    if m.finished.is_none() || (navigate && m.interactions_finished.is_none()) || m.error {
        return Err("Native benchmark did not complete successfully".into());
    }
    Ok(())
}
