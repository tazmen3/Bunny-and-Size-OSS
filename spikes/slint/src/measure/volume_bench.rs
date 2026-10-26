use crate::volume_view::{Prepared, Sources};
use sortsize_core::volumes::*;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut enums = Vec::new();
    let mut capacity = Vec::new();
    let mut projection = Vec::new();
    let mut count = 0;
    let mut calls = 0;
    let mut model_bytes = 0;
    for _ in 0..101 {
        let d = discover_volumes(DiscoveryOptions::default())?;
        enums.push(d.enumeration_time.as_secs_f64() * 1000.0);
        capacity.push(d.capacity_time.as_secs_f64() * 1000.0);
        calls = d.capacity_calls;
        count = d.volumes.len();
        model_bytes = d.volumes.capacity() * std::mem::size_of::<Volume>()
            + d.volumes
                .iter()
                .map(|v| {
                    v.mounts.capacity() * std::mem::size_of::<VolumeMount>()
                        + v.mounts
                            .iter()
                            .map(|m| {
                                m.path.as_os_str().as_encoded_bytes().len()
                                    + m.root
                                        .as_ref()
                                        .map(|p| p.as_os_str().as_encoded_bytes().len())
                                        .unwrap_or(0)
                            })
                            .sum::<usize>()
                })
                .sum::<usize>();
        let start = Instant::now();
        let p = Prepared::new(d);
        projection.push(start.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(p);
    }
    let summary = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        format!(
            "{{\"min_ms\":{},\"median_ms\":{},\"max_ms\":{}}}",
            v[0],
            v[v.len() / 2],
            v[v.len() - 1]
        )
    };
    // Slow provider: waiting time and dispatch CPU are different costs.
    let mut s = Sources::with_provider(Arc::new(|_| {
        std::thread::sleep(Duration::from_millis(250));
        Ok(VolumeDiscovery {
            volumes: vec![],
            enumeration_time: Duration::ZERO,
            capacity_time: Duration::from_millis(250),
            capacity_calls: 0,
        })
    }));
    let start = Instant::now();
    s.refresh(DiscoveryOptions::default());
    let dispatch = start.elapsed().as_secs_f64() * 1000.0;
    let mut poll_max: f64 = 0.0;
    while s.loading() {
        let t = Instant::now();
        s.poll();
        poll_max = poll_max.max(t.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    println!(
        "{{\"samples\":101,\"volumes\":{count},\"capacity_calls_per_discovery\":{calls},\"enumeration\":{},\"capacity\":{},\"projection\":{},\"model_inline_and_path_payload_bytes\":{model_bytes},\"source_service_inline_bytes\":{},\"slow_provider_wall_ms\":{},\"dispatch_ms\":{dispatch},\"poll_max_ms\":{poll_max}}}",
        summary(enums),
        summary(capacity),
        summary(projection),
        std::mem::size_of::<Sources>(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

/// Native harness only: deterministic errors/delay, no production environment hooks.
pub(super) fn configure(controller: &mut crate::Controller, root: &std::path::Path) {
    use sortsize_core::MetadataValue;
    if let Ok(value) = std::env::var("SORTSIZE_MEASURE_VOLUME_DELAY_MS") {
        if let Ok(ms) = value.parse::<u64>() {
            controller.sources = Sources::with_provider(Arc::new(move |options| {
                std::thread::sleep(Duration::from_millis(ms));
                discover_volumes(options)
            }));
        }
    } else if std::env::var("SORTSIZE_MEASURE_VOLUME_STATES").as_deref() == Ok("1") {
        let root = root.to_owned();
        controller.sources = Sources::with_provider(Arc::new(move |_| {
            let states = [
                VolumeAvailability::Available,
                VolumeAvailability::AccessDenied,
                VolumeAvailability::Unavailable,
                VolumeAvailability::NotProbed,
            ];
            let volumes = states
                .into_iter()
                .enumerate()
                .map(|(i, availability)| {
                    let path = if i == 0 {
                        root.clone()
                    } else {
                        root.join(format!("simulated-source-{i}"))
                    };
                    let state = if i == 1 {
                        MetadataValue::AccessDenied
                    } else {
                        MetadataValue::Unknown
                    };
                    Volume {
                        identity: VolumeIdentity::WindowsPath(path.clone()),
                        label: MetadataValue::Unknown,
                        filesystem: MetadataValue::Unknown,
                        kind: if i == 3 {
                            SourceKind::Remote
                        } else {
                            SourceKind::Local
                        },
                        mounts: vec![VolumeMount {
                            path,
                            mount_id: None,
                            root: None,
                        }],
                        capacity: VolumeCapacity::absent(state),
                        availability,
                    }
                })
                .collect();
            Ok(VolumeDiscovery {
                volumes,
                enumeration_time: Duration::ZERO,
                capacity_time: Duration::ZERO,
                capacity_calls: 0,
            })
        }));
    }
}

pub(super) fn native_panel(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use slint::{ComponentHandle, Timer, TimerMode};
    let window = crate::SortSizeWindow::new()?;
    let controller = crate::connect(&window);
    window.set_scan_path(
        controller
            .borrow_mut()
            .path_input
            .select(root.to_owned())
            .into(),
    );
    configure(&mut controller.borrow_mut(), root);
    controller.borrow_mut().refresh_sources(&window, false);
    let weak = window.as_weak();
    let state = controller.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        if let Some(w) = weak.upgrade() {
            state.borrow_mut().poll(&w);
        }
    });
    let result = window.run();
    timer.stop();
    result?;
    Ok(())
}

#[cfg(feature = "allocation-profiling")]
pub(super) fn memory() -> Result<(), Box<dyn std::error::Error>> {
    // Warm native/runtime lazy state before the isolated allocation snapshots.
    drop(Prepared::new(
        discover_volumes(DiscoveryOptions::default())?,
    ));
    let before = super::allocations::live_requested_bytes();
    let d = discover_volumes(DiscoveryOptions::default())?;
    let discovery_bytes = super::allocations::live_requested_bytes().saturating_sub(before);
    let p = Prepared::new(d);
    let rows = p.rows_for(crate::units::Units::Binary);
    let projected_bytes = super::allocations::live_requested_bytes().saturating_sub(before);
    let count = rows.len();
    drop(rows);
    drop(p);
    let retained_after_drop = super::allocations::live_requested_bytes().saturating_sub(before);
    println!(
        "{{\"volumes\":{count},\"discovery_live_requested_bytes\":{discovery_bytes},\"discovery_and_rows_live_requested_bytes\":{projected_bytes},\"retained_after_drop_bytes\":{retained_after_drop},\"includes_allocator_metadata\":false,\"includes_thread_stacks\":false}}"
    );
    Ok(())
}
