//! One bounded owner for certified read generations. Acquisition owns a separate
//! service thread and never waits for this query/navigation loop to complete.
use super::*;
use service::{Active, Backend, Lane, Output};
use std::time::Duration;

pub(super) fn worker(
    lane: Arc<Mutex<Lane>>,
    output: Arc<Mutex<Option<Output>>>,
    search_output: Arc<Mutex<Option<search::Prepared>>>,
    resources: Arc<Mutex<Option<(crate::presentation::Context, String)>>>,
    cache_output: Arc<Mutex<Option<cache::Prepared>>>,
    preview_output: Arc<Mutex<Option<cache::PreviewOutput>>>,
) {
    let mut active: Option<Active> = None;
    // Retain the already admitted G0 until a new fast frame can actually
    // replace it. Cancelling a bounded fold restores that reader and its lease.
    let mut fast_fallback = None;
    let mut scan_metrics = None::<scan_p1::ReaderMetrics>;
    let mut meter = search::Meter::default();
    let mut writer_running = false;
    loop {
        #[cfg(test)]
        if let Some(gate) = { lane.lock().unwrap().reader_gate.clone() } {
            lane.lock().unwrap().reader_blocked = true;
            let (lock, ready) = &*gate;
            let mut release = lock.lock().unwrap();
            while !*release {
                release = ready.wait(release).unwrap();
            }
        }
        let (target, epoch, stop, presentation) = {
            let mut l = lane.lock().unwrap();
            (
                l.reader_target.take(),
                l.reader_epoch.take(),
                l.stop,
                l.presentation,
            )
        };
        if stop {
            break;
        }
        let _scope = presentation.enter();
        if let Some(target) = target {
            match target {
                Some(mut value)
                    if value.source_epoch == lane.lock().unwrap().source_epoch
                        || epoch.as_ref().is_some_and(|(_, root, inventory, source)| {
                            value.root == *root
                                && value.c.inventory == *inventory
                                && value.c.source == *source
                        }) =>
                {
                    if value.certified_generation().is_some()
                        && let Some(previous) = &active
                    {
                        value.continue_fast_navigation(previous);
                    }
                    fast_fallback = if value.is_fast_preview() {
                        active.take().filter(|old| {
                            old.source_epoch == value.source_epoch && old.root == value.root
                        })
                    } else {
                        None
                    };
                    scan_metrics = value.p1_handoff_at.take().map(scan_p1::ReaderMetrics::new);
                    active = Some(*value);
                }
                Some(_) => {} // Stale handoff cannot discard a still useful G0.
                None => {
                    active = None;
                    fast_fallback = None;
                    scan_metrics = None;
                    let mut l = lane.lock().unwrap();
                    l.interrupted_reader = false;
                    if l.reader_target.as_ref().is_some_and(Option::is_none) {
                        l.reader_target = None;
                    }
                }
            }
            if let Some(a) = &mut active {
                if let Some(workspace) = &a.workspace {
                    let previous = lane
                        .lock()
                        .unwrap()
                        .retire_private
                        .replace(workspace.clone());
                    drop(previous);
                }
                a.epoch = lane.lock().unwrap().view_epoch;
                a.force = true;
            }
            service::set_reader_workspace(
                &lane,
                active.is_some(),
                active.as_ref().and_then(|a| a.workspace.clone()),
            );
        }
        if let Some((epoch, root, inventory, source)) = epoch
            && let Some(a) = &mut active
        {
            if a.root == root && a.c.inventory == inventory && a.c.source == source {
                a.epoch = epoch;
                a.source_epoch = lane.lock().unwrap().source_epoch;
                a.force = true;
            } else {
                active = None;
                fast_fallback = None;
                lane.lock().unwrap().reader_active = false;
            }
        }
        let revoked_fast = {
            let l = lane.lock().unwrap();
            active.as_ref().is_some_and(|a| {
                a.is_fast_preview()
                    && a.source_epoch == l.source_epoch
                    && l.fast_preview_context != Some(a.c)
                    && l.publication_cancel.is_cancelled()
            })
        };
        if revoked_fast {
            active = fast_fallback.take();
            scan_metrics = None;
            if let Some(a) = &mut active {
                a.force = true;
            }
            service::set_reader_workspace(
                &lane,
                active.is_some(),
                active.as_ref().and_then(|a| a.workspace.clone()),
            );
        }
        service::retire_cancelled(&mut active, &lane);
        if active.is_none() {
            service::set_reader_workspace(&lane, false, None);
        }
        let (navigation, cache_request, units, presentation, running) = {
            let mut l = lane.lock().unwrap();
            let owns_view = l.direct_view.is_none()
                && active.as_ref().is_some_and(|a| {
                    a.source_epoch == l.source_epoch && service::smb_view_matches(&l, &a.root)
                })
                && l.reader_active;
            let opens_source = l.cache.as_ref().is_some_and(|request| {
                matches!(
                    request.action,
                    cache::Action::Open(_) | cache::Action::Verify(_) | cache::Action::Resume(_)
                )
            });
            let writer_save = l.cache.as_ref().is_some_and(|request| {
                matches!(request.action, cache::Action::SaveFast { .. })
                    || request.action.is_save()
                        && (l.writer_epoch.is_some()
                            || active.as_ref().is_some_and(Active::is_fast_preview))
            });
            let cache_request = if l.direct_view.is_none()
                && !l.cache.as_ref().is_some_and(|r| {
                    matches!(&r.action,
                    cache::Action::Open(saved) if cache::is_snapshot(saved))
                })
                && (owns_view || opens_source)
                && (!opens_source || l.writer_epoch.is_none())
                && !writer_save
            {
                let request = cache::take_request(&mut l);
                if opens_source {
                    l.reader_constructing = request.as_ref().map(|request| request.epoch);
                }
                request
            } else {
                None
            };
            (
                if owns_view { l.navigation.take() } else { None },
                cache_request,
                l.units,
                l.presentation,
                l.writer_running || l.preparing_certified_view(),
            )
        };
        let _scope = presentation.enter();
        meter.update(&lane, &resources, units);
        if let Some(request) = cache_request {
            cache::run(
                request,
                &mut active,
                &lane,
                &cache_output,
                &output,
                &preview_output,
            );
            service::notify(&lane);
            if active.as_ref().is_some_and(|a| {
                matches!(a.session, Backend::Live(_))
                    || (!matches!(a.session, Backend::Certified(_))
                        && a.sealed.is_some()
                        && !a.is_fast_preview())
            }) {
                let mut l = lane.lock().unwrap();
                let previous = std::mem::replace(&mut l.writer_target, active.take().map(Box::new));
                l.reader_active = false;
                let workspace = l.reader_workspace.take();
                l.reader_constructing = None;
                drop(l);
                drop(previous);
                drop(workspace);
                continue;
            }
            service::set_reader_workspace(
                &lane,
                active.is_some(),
                active.as_ref().and_then(|a| a.workspace.clone()),
            );
            lane.lock().unwrap().reader_constructing = None;
        }
        let mut work_remaining = false;
        let muted = lane.lock().unwrap().direct_view.is_some();
        if let Some(a) = active.as_mut().filter(|_| !muted) {
            scan_p1::progress_trace::current(|_, counters| {
                counters.context(a.source_epoch, a.c.generation.0, a.epoch, a.c.inventory.0)
            });
            let result = (|| -> sortsize_store::inventory::Result<()> {
                let sort = lane.lock().unwrap().detail_sort;
                let changed_sort = a.sync_sort(sort);
                if !changed_sort {
                    a.view_anchor(&lane);
                }
                let previous_parent = a.parent;
                let requested = navigation.as_ref().is_some_and(|(_, action)| {
                    matches!(
                        action,
                        Navigate::Open(_) | Navigate::Parent | Navigate::Reveal(_)
                    )
                });
                if let Some((epoch, action)) = navigation {
                    a.navigate(epoch, action)?;
                }
                a.sync_requested_folder(&lane)?;
                if requested || a.parent != previous_parent {
                    a.prioritize_reader_folder(&lane)?;
                }
                if writer_running != running {
                    a.force = true;
                    writer_running = running;
                }
                // Certified stores have immutable observations, but their
                // bounded folder derivative can still be under construction.
                // Never freeze a tree order against missing fold measurements.
                let folded = scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.fold), || {
                    let _trace = sortsize_store::scan_trace::phase("preview_fold");
                    a.session.store().folder_fold_step_at(
                        a.c.generation,
                        &mut a.fold,
                        a._temporary.path(),
                    )
                })?;
                if !folded {
                    work_remaining = true;
                    return Ok(());
                }
                scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.tree), || {
                    a.advance_folder_tree()
                })?;
                work_remaining |= a.folder_tree.borrow().has_work();
                if a.force
                    || a.folder_tree.borrow().dirty
                        && a.last.elapsed() >= Duration::from_millis(100)
                {
                    if !scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.order), || {
                        a.advance_order()
                    })? {
                        work_remaining = true;
                        return Ok(());
                    }
                    let scope = lane.lock().unwrap().top_scope.clone();
                    let mut page =
                        scan_p1::measure(scan_metrics.as_mut().map(|m| &mut m.prepare), || {
                            a.prepare(units, scope)
                        })?;
                    a.describe_folder_fallback(&lane, &mut page);
                    // The first root-only frame creates its lazy branch order.
                    // Finish that first bounded tree quantum before advertising
                    // the verified view as settled (the live pane stays usable).
                    if page.folders.len() <= 1 && a.folder_tree.borrow().has_work() {
                        work_remaining = true;
                        return Ok(());
                    }
                    page.busy = running;
                    if running {
                        page.status
                            .push_str(&format!(" · {}", tr!("A new inventory is being prepared.")));
                    }
                    // The previous G0 may still be alive while another source
                    // starts or fails. Fence the shared one-slot mailbox under
                    // the same lane lock used to publish the new intention.
                    let published = {
                        let _trace = sortsize_store::scan_trace::phase("publish_mailbox");
                        let mut l = lane.lock().unwrap();
                        let initial_fast_frame =
                            a.is_fast_preview() && l.fast_preview_context != Some(a.c);
                        if l.direct_view.is_none()
                            && a.source_epoch == l.source_epoch
                            && (!initial_fast_frame || !l.publication_cancel.is_cancelled())
                            && page.epoch == l.view_epoch
                            && service::smb_view_matches(&l, &a.root)
                        {
                            let mut pending = output.lock().unwrap();
                            if matches!(pending.as_ref(), Some(Output::Error(epoch, ..)) if *epoch >= page.epoch)
                            {
                                false
                            } else {
                                if initial_fast_frame
                                    && matches!(
                                        l.phase,
                                        Some((epoch, service::Phase::Finalizing, generation))
                                            if epoch == a.source_epoch && generation == a.c.generation
                                    )
                                {
                                    l.fast_preview_context = Some(a.c);
                                    l.phase = Some((
                                        a.source_epoch,
                                        service::Phase::PreviewReady,
                                        a.c.generation,
                                    ));
                                }
                                if a.certified_generation().is_some() {
                                    // The page and writer cleanup may finish in
                                    // either order. Both belong to this exact
                                    // source intention and acquisition context.
                                    l.certified_step_finished(a.source_epoch, a.c, true);
                                    page.busy = l.writer_running || l.preparing_certified_view();
                                }
                                scan_p1::progress_trace::current(|_, counters| {
                                    use scan_p1::progress_trace::Field;
                                    counters.add(Field::Pages);
                                    counters.write(&[
                                        (Field::Token, page.token as u64),
                                        (Field::HoldReason, 0),
                                        (Field::PublishedNs, scan_p1::progress_trace::now_ns()),
                                    ]);
                                });
                                *pending = Some(Output::Page(Box::new(page)));
                                true
                            }
                        } else {
                            false
                        }
                    };
                    if published {
                        if let Some(metrics) = scan_metrics.take() {
                            metrics.report(a.source_epoch, a.c.generation);
                        }
                        service::notify(&lane);
                        if a.is_fast_preview() {
                            // Release the previous generation outside the lane lock.
                            drop(fast_fallback.take());
                        }
                    }
                    a.force = false;
                    a.last = std::time::Instant::now();
                }
                Ok(())
            })();
            if let Err(error) = result {
                let l = lane.lock().unwrap();
                if l.direct_view.is_none()
                    && a.source_epoch == l.source_epoch
                    && a.epoch == l.view_epoch
                {
                    service::publish_retry_error(&output, a.epoch, error.to_string());
                }
                // A persistent preparation refusal is retried every idle tick.
                // Keep errors on the 50 ms fallback instead of waking the UI
                // repeatedly at the reader's 2 ms retry cadence.
            }
            let request = {
                let mut l = lane.lock().unwrap();
                if l.direct_view.is_none() && a.source_epoch == l.source_epoch {
                    l.search.take()
                } else {
                    None
                }
            };
            if let Some(request) = request {
                search::run(
                    &search::Frozen {
                        store: a.session.store(),
                        certified: a.certified_generation(),
                        c: a.c,
                        root: &a.root,
                        scratch: a._temporary.path(),
                        started: a.started,
                        sealed: a.sealed.unwrap_or(a.started),
                    },
                    request,
                    &lane,
                    &search_output,
                    &resources,
                );
                service::notify(&lane);
                // Search may coalesce navigation; its immutable borrow is now
                // gone, so a following tick can service that intent safely.
                a.force = true;
            }
        }
        // Each fold/order quantum is bounded. Re-enter the loop immediately
        // while it made progress, checking stop, cancellation, epochs and search
        // between quanta. The 2 ms idle poll must not tax every 256 input rows.
        if work_remaining {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    drop(active);
    service::drain_stopped_lane(&lane);
    let mut l = lane.lock().unwrap();
    l.reader_active = false;
    l.reader_workspace = None;
    l.reader_constructing = None;
}
