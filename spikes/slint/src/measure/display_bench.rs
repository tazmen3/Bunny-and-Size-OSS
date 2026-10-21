//! Identical callback probe is also applied to the exact base; new actions are
//! explicitly null there. Headless callbacks measure CPU work, not input latency.
use crate::{Controller, SortSizeWindow};
use slint::Model;
use std::{cell::RefCell, rc::Rc, time::Instant};
fn stats(mut samples: Vec<f64>) -> String {
    if samples.is_empty() {
        return "null".into();
    }
    samples.sort_by(f64::total_cmp);
    format!(
        "{{\"samples\":{},\"min_ms\":{:.6},\"median_ms\":{:.6},\"max_ms\":{:.6}}}",
        samples.len(),
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() - 1]
    )
}
fn time(mut action: impl FnMut(), n: usize) -> String {
    stats(
        (0..n)
            .map(|_| {
                let start = Instant::now();
                action();
                start.elapsed().as_secs_f64() * 1000.0
            })
            .collect(),
    )
}
pub(super) fn run(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) -> String {
    let mut sorts: [Vec<f64>; 12] = Default::default();
    for group in 0..6 {
        if group == 2 && !window.get_allocation_sort_available()
            || group == 3 && !window.get_owner_sort_available()
        {
            continue;
        }
        for _ in 0..62 {
            let start = Instant::now();
            match group {
                0 => window.invoke_sort_details(false),
                1 => window.invoke_sort_details(true),
                2 => window.invoke_sort_allocated(),
                3 => window.invoke_sort_owner(),
                4 => window.invoke_sort_dates(false),
                _ => window.invoke_sort_dates(true),
            }
            sorts[window.get_sort_order() as usize].push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let sorts = sorts.into_iter().map(stats).collect::<Vec<_>>().join(",");
    let extra = extra(window, controller);
    format!("{{\"twelve_callbacks\":[{sorts}],\"display\":{extra}}}")
}
// Candidate-only body. The baseline adapter replaces this function with null.
fn extra(window: &SortSizeWindow, controller: &Rc<RefCell<Controller>>) -> String {
    // Injected retained volumes: presentation only, zero provider/capacity calls.
    use sortsize_core::{MetadataValue, ValueAccuracy, volumes::*};
    let known = |value| MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    };
    let discovery = VolumeDiscovery {
        volumes: (0..256)
            .map(|i| Volume {
                identity: VolumeIdentity::WindowsPath(format!("fixture-{i}").into()),
                label: MetadataValue::Unknown,
                filesystem: MetadataValue::Unknown,
                kind: SourceKind::Local,
                mounts: vec![VolumeMount {
                    path: format!("fixture-{i}").into(),
                    mount_id: None,
                    root: None,
                }],
                capacity: VolumeCapacity::from_bytes(
                    known(1 << 40),
                    known(1 << 39),
                    MetadataValue::Unknown,
                ),
                availability: VolumeAvailability::Available,
            })
            .collect(),
        enumeration_time: std::time::Duration::ZERO,
        capacity_time: std::time::Duration::ZERO,
        capacity_calls: 0,
    };
    controller.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(discovery));
    let volume_rows = time(
        || {
            std::hint::black_box(
                controller
                    .borrow()
                    .sources
                    .current
                    .as_ref()
                    .unwrap()
                    .rows_for(crate::units::Units::Decimal),
            );
        },
        101,
    );
    let columns = time(
        || {
            window.invoke_set_column(10, false);
            window.invoke_set_column(10, true);
        },
        101,
    );
    let units = time(
        || {
            window.invoke_set_units(!window.get_decimal_units());
        },
        62,
    );
    let reset = time(|| window.invoke_reset_display(), 31);
    let panel = time(
        || {
            window.set_preferences_open(true);
            window.set_preferences_open(false);
        },
        101,
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.conf");
    let preferences = controller.borrow().preferences.clone();
    let save = time(
        || crate::preferences_store::save(&path, &preferences).unwrap(),
        31,
    );
    let load = time(
        || {
            std::hint::black_box(crate::preferences_store::load(&path));
        },
        101,
    );
    let mut store = crate::preferences_store::Store::start(path).unwrap();
    let dispatch = time(|| store.submit(preferences.clone()), 101);
    drop(store);
    assert!(window.get_details().row_count() <= 2000);
    format!(
        "{{\"injected_volume_rows\":{volume_rows},\"column_pair\":{columns},\"units\":{units},\"reset\":{reset},\"panel_property_pair\":{panel},\"load\":{load},\"atomic_save_io\":{save},\"save_dispatch\":{dispatch},\"allocation_value_bytes_per_node\":{}}}",
        std::mem::size_of::<sortsize_core::MetadataValue<u64>>()
    )
}
