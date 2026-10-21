//! Local deterministic date-sort probe. Same harness is applied to the exact base;
//! only the adapters for the four missing baseline sorts return no samples.
use crate::{SortSizeWindow, connect, worker};
use slint::Model;
use sortsize_core::{
    CancellationToken, EntryDates, InventoryLimits, MetadataValue, ScanOptions, ValueAccuracy,
};
use std::{
    path::Path,
    time::{Duration, Instant, UNIX_EPOCH},
};

fn known(nanos: u64) -> MetadataValue<std::time::SystemTime> {
    MetadataValue::Known {
        value: UNIX_EPOCH + Duration::from_nanos(nanos),
        accuracy: ValueAccuracy::Exact,
    }
}
fn summary(mut samples: Vec<f64>) -> String {
    if samples.is_empty() {
        return "null".into();
    }
    samples.sort_by(f64::total_cmp);
    format!(
        "{{\"samples\":{},\"min_ms\":{:.6},\"median_ms\":{:.6},\"p95_ms\":{:.6},\"max_ms\":{:.6}}}",
        samples.len(),
        samples[0],
        samples[samples.len() / 2],
        samples[(samples.len() - 1) * 95 / 100],
        samples[samples.len() - 1]
    )
}
fn measure(mut action: impl FnMut(), count: usize) -> String {
    let samples = (0..count)
        .map(|_| {
            let start = Instant::now();
            action();
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    summary(samples)
}
fn temporal_callbacks(window: &SortSizeWindow) -> String {
    let mut samples = Vec::new();
    for _ in 0..25 {
        for accessed in [false, false, true, true] {
            let start = Instant::now();
            window.invoke_sort_dates(accessed);
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    summary(samples)
}

pub(super) fn run(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let allocation = std::env::var("SORTSIZE_MEASURE_ALLOCATION").as_deref() == Ok("1");
    let owner = std::env::var("SORTSIZE_MEASURE_OWNER").as_deref() == Ok("1");
    let pattern = std::env::var("SORTSIZE_MEASURE_DATE_PATTERN").unwrap_or("varied".into());
    let start = Instant::now();
    let mut raw = worker::acquire_with_owner(
        root,
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        allocation,
        owner,
        |_| {},
    )?;
    let scan_ms = start.elapsed().as_secs_f64() * 1000.0;
    let inventory = raw.inventory();
    let count = inventory.entries.len();
    let files = inventory.report.files;
    let omitted = inventory.entries_omitted;
    let dates = match &mut raw {
        worker::AcquiredInventory::Dates(r) => &mut r.dates,
        worker::AcquiredInventory::Allocated(r) => &mut r.metadata.dates,
        worker::AcquiredInventory::Owners(r) => &mut r.metadata.dates,
        worker::AcquiredInventory::OwnerAllocated(r) => &mut r.allocated.metadata.dates,
    };
    for (i, date) in dates.iter_mut().enumerate() {
        // Deterministic unordered instants and all absence states; no real atime oracle.
        let value = match (pattern.as_str(), i % 6) {
            ("identical", _) => known(42),
            ("mixed", 1) => MetadataValue::Unknown,
            ("mixed", 2) => MetadataValue::Unsupported,
            ("mixed", 3) => MetadataValue::AccessDenied,
            ("mixed", 4) => MetadataValue::NotCollected,
            _ => known((i as u64 * 7919) % 50_003),
        };
        *date = EntryDates {
            modified: value,
            accessed: value,
        };
    }
    let start = Instant::now();
    let (mut tree, _) = raw.into_tree()?;
    let projection_ms = start.elapsed().as_secs_f64() * 1000.0;
    let date_prepare_ms = tree.date_prepare_ms;
    let date_order_bytes = tree.date_order_bytes();
    let expected_rows = tree.details().0.len();
    let details = measure(
        || {
            std::hint::black_box(tree.details());
        },
        31,
    );
    let pages = measure(
        || {
            tree.change_page(true, true);
            std::hint::black_box(tree.details());
            tree.change_page(true, false);
        },
        31,
    );
    let tree_sort = measure(
        || {
            tree.sort_by(true);
            std::hint::black_box(tree.sort());
        },
        1000,
    );
    // Public Slint software backend: callbacks include the real model/page projection.
    use slint::platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };
    use std::rc::Rc;
    struct TestPlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    slint::platform::set_platform(Box::new(TestPlatform(MinimalSoftwareWindow::new(
        RepaintBufferType::NewBuffer,
    ))))?;
    let window = SortSizeWindow::new()?;
    let controller = connect(&window);
    controller.borrow_mut().tree = Some(tree);
    controller.borrow_mut().show_rows(&window);
    let historical_callbacks = measure(
        || {
            window.invoke_sort_details(true);
        },
        100,
    );
    let date_callbacks = temporal_callbacks(&window);
    // Compare pagination on the same name-ascending order in both builds.
    window.invoke_sort_details(true);
    let page_callbacks = measure(
        || {
            window.invoke_change_page(true, true);
            window.invoke_change_page(true, false);
        },
        31,
    );
    assert_eq!(window.get_details().row_count(), expected_rows);
    #[cfg(any(target_os = "linux", windows))]
    let display_metrics = super::display_bench::run(&window, &controller);
    #[cfg(not(any(target_os = "linux", windows)))]
    let display_metrics = "null";
    let resources = super::process_resources()?;
    println!(
        "{{\"resources\":{resources},\"display_metrics\":{display_metrics},\"nodes\":{count},\"files\":{files},\"omitted\":{omitted},\"scan_ms\":{scan_ms:.6},\"projection_ms\":{projection_ms:.6},\"date_prepare_ms\":{date_prepare_ms:.6},\"date_order_bytes\":{date_order_bytes},\"details\":{details},\"pages\":{pages},\"tree_sort\":{tree_sort},\"historical_callbacks\":{historical_callbacks},\"date_callbacks\":{date_callbacks},\"page_callbacks\":{page_callbacks}}}"
    );
    Ok(())
}
