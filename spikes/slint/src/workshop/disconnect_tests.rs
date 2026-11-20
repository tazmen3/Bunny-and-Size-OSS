//! #201: deterministic local fixtures exercise the real service/SQLite path.
use super::*;
use std::time::{Duration, Instant};

fn request(service: &Service, epoch: u64, server: PathBuf, roots: Vec<PathBuf>) {
    let request = Request::confirmed(epoch, server, roots, false, Default::default()).unwrap();
    let mut lane = service.lane.lock().unwrap();
    lane.source_epoch = epoch;
    lane.view_epoch = epoch;
    lane.publication_cancel.cancel();
    lane.publication_cancel = Default::default();
    lane.smb_snapshot = Some(request.snapshot.clone());
    lane.smb_begin = Some(request);
    lane.smb_view = None;
    lane.reader_target = Some(None);
}
fn wait(service: &Service, check: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(snapshot) = service.smb_snapshot()
            && check(&snapshot)
        {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "disconnect timeout: {:?}",
            service.smb_snapshot()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn held(service: &Service, generation: GenerationId) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let lane = service.lane.lock().unwrap();
        if lane.writer_generation == Some(generation) && lane.writer_entries > 1 {
            return;
        }
        drop(lane);
        assert!(
            Instant::now() < deadline,
            "writer did not enter held generation"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn root_page(service: &Service, epoch: u64) -> Box<Prepared> {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(Output::Page(page)) = service.output.lock().unwrap().take()
            && page.epoch == epoch
            && page.measure_ready
        {
            return page;
        }
        assert!(Instant::now() < deadline, "G0 reader timeout");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn disconnect_active_share_keeps_other_share_eligible_and_reconnect_is_idle() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    for i in 0..24 {
        std::fs::write(first.join(i.to_string()), [1; 3]).unwrap();
    }
    std::fs::write(second.join("file"), [2; 7]).unwrap();
    let service = Service::start().unwrap();
    service.lane.lock().unwrap().hold_generation = Some(GenerationId(1));
    request(
        &service,
        1,
        temp.path().into(),
        vec![first.clone(), second.clone()],
    );
    held(&service, GenerationId(1));
    service.disconnect_sources(std::slice::from_ref(&first));
    // Reconnecting before the worker settles does not revive this cancelled job.
    service.reconnect_sources(std::slice::from_ref(&first));
    service.lane.lock().unwrap().hold_generation = None;
    let final_state = wait(&service, |s| !s.running);
    assert_eq!(final_state.shares[0].phase, SharePhase::Cancelled);
    assert_eq!(final_state.shares[1].phase, SharePhase::Complete);
    assert_eq!(final_state.shares[1].logical, Some(7));
    assert!(!final_state.cancelled);
    assert_ne!(final_state.totals().coverage, ScopeCoverage::Complete);
    let lane = service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none());
    drop(lane);
    assert!(!service.source_disconnected(&first));
    service.stop_for_test();
}
#[test]
fn disconnect_server_cancels_remaining_scope_and_never_rearms_it() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    for i in 0..24 {
        std::fs::write(first.join(i.to_string()), [1; 3]).unwrap();
    }
    let service = Service::start().unwrap();
    service.lane.lock().unwrap().hold_generation = Some(GenerationId(1));
    request(
        &service,
        1,
        temp.path().into(),
        vec![first.clone(), second.clone()],
    );
    held(&service, GenerationId(1));
    service.disconnect_sources(&[temp.path().into()]);
    service.lane.lock().unwrap().hold_generation = None;
    let final_state = wait(&service, |s| !s.running);
    assert!(
        final_state
            .shares
            .iter()
            .all(|s| s.phase == SharePhase::Cancelled)
    );
    assert_eq!(final_state.shares[1].files, None);
    assert!(service.source_disconnected(&first));
    assert!(service.source_disconnected(&second));
    service.reconnect_sources(&[temp.path().into()]);
    assert_eq!(service.smb_snapshot(), Some(final_state));
    assert!(service.lane.lock().unwrap().start.is_none());
    service.stop_for_test();
}
#[test]
fn disconnected_scope_is_rejected_before_any_new_preflight_or_acquisition() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("must-not-be-opened");
    let service = Service::start().unwrap();
    service.disconnect_sources(std::slice::from_ref(&root));
    // An old confirmed request cannot evade the current service admission gate.
    request(&service, 1, temp.path().into(), vec![root.clone()]);
    let final_state = wait(&service, |s| !s.running);
    assert_eq!(final_state.shares[0].phase, SharePhase::Cancelled);
    assert!(final_state.shares[0].error.is_none());
    assert_eq!(final_state.shares[0].files, None);
    assert!(service.retained_smb_shares().is_empty());
    assert!(!root.exists());
    service.stop_for_test();
}
#[test]
fn disconnect_refresh_preserves_certified_g0_and_explicit_reconnect_does_not_scan() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("share");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("original"), [1; 17]).unwrap();
    let service = Service::start().unwrap();
    request(&service, 1, temp.path().into(), vec![root.clone()]);
    assert_eq!(wait(&service, |s| !s.running).totals().logical, Some(17));
    assert!(service.open_smb_share(2, 0));
    let before = root_page(&service, 2);
    let generation = before.frame.stamp.context.generation;
    drop(before);
    for i in 0..24 {
        std::fs::write(root.join(i.to_string()), [2; 5]).unwrap();
    }
    service.lane.lock().unwrap().hold_generation = Some(GenerationId(2));
    request(&service, 3, temp.path().into(), vec![root.clone()]);
    held(&service, GenerationId(2));
    service.disconnect_sources(std::slice::from_ref(&root));
    service.lane.lock().unwrap().hold_generation = None;
    let cancelled = wait(&service, |s| s.epoch == 3 && !s.running);
    assert_eq!(cancelled.shares[0].phase, SharePhase::Cancelled);
    assert!(service.open_smb_share(4, 0));
    let preserved = root_page(&service, 4);
    assert_eq!(preserved.frame.stamp.context.generation, generation);
    assert_eq!(preserved.root_measure.unwrap().logical, Some(17));
    drop(preserved);
    service.reconnect_sources(std::slice::from_ref(&root));
    assert_eq!(service.smb_snapshot(), Some(cancelled));
    assert!(service.lane.lock().unwrap().start.is_none());
    service.stop_for_test();
}

#[test]
fn reconnect_parent_preserves_explicit_child_revocation() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().to_path_buf();
    let child = parent.join("explicit-share");
    let sibling = parent.join("other-share");
    let service = Service::start().unwrap();
    service.disconnect_sources(std::slice::from_ref(&child));
    service.disconnect_sources(std::slice::from_ref(&parent));
    // A stale child action while its parent is disabled must not silently
    // erase the independent choice that will matter after parent reconnect.
    service.reconnect_sources(std::slice::from_ref(&child));
    assert!(service.source_disconnected(&child));
    assert!(service.source_disconnected(&sibling));
    service.reconnect_sources(std::slice::from_ref(&parent));
    assert!(service.source_disconnected(&child));
    assert!(!service.source_disconnected(&sibling));
    service.reconnect_sources(std::slice::from_ref(&child));
    assert!(!service.source_disconnected(&child));
    let lane = service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
    drop(lane);
    service.stop_for_test();
}

#[test]
fn reconnect_during_blocked_preparation_does_not_revive_the_old_intention() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("not-opened");
    let service = Service::start().unwrap();
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let wait_release = release.clone();
    let (entered, blocked) = std::sync::mpsc::sync_channel(1);
    {
        let mut lane = service.lane.lock().unwrap();
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: root.clone(),
                explicit_root: true,
                options: Default::default(),
                observation: crate::workshop::service::size_observation(),
                preflight: Some(Box::new(move || {
                    entered.send(()).unwrap();
                    let (ready, wake) = &*wait_release;
                    let mut ready = ready.lock().unwrap();
                    while !*ready {
                        ready = wake.wait(ready).unwrap();
                    }
                    true
                })),
            },
        ));
    }
    blocked.recv_timeout(Duration::from_secs(30)).unwrap();
    service.disconnect_sources(std::slice::from_ref(&root));
    service.reconnect_sources(std::slice::from_ref(&root));
    {
        let (ready, wake) = &*release;
        *ready.lock().unwrap() = true;
        wake.notify_all();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let lane = service.lane.lock().unwrap();
        if matches!(lane.phase, Some((1, service::Phase::Cancelled, _))) {
            assert!(lane.start.is_none());
            assert!(lane.preparing_source.is_none());
            assert_eq!(lane.writer_entries, 0);
            break;
        }
        drop(lane);
        assert!(Instant::now() < deadline, "cancelled preparation restarted");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(service.output.lock().unwrap().is_none());
    service.stop_for_test();
}

fn recorded_source(
    root: PathBuf,
    directory: PathBuf,
) -> sortsize_store::inventory::catalogue::Saved {
    sortsize_store::inventory::catalogue::Saved {
        directory,
        context: AcquisitionContext {
            inventory: InventoryId(1),
            source: SourceId(1),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        },
        root,
        started: 0,
        measured: None,
        coverage: ScopeCoverage::Partial,
        logical: None,
        unknown: 0,
        continuity: None,
    }
}
#[test]
fn cache_verify_resume_cannot_bypass_revocation_even_when_dequeued_before_reconnect() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("network-source");
    let service = Service::start().unwrap();
    for resume in [false, true] {
        let saved = recorded_source(root.clone(), temp.path().join("missing-cache"));
        let request = {
            let mut lane = service.lane.lock().unwrap();
            lane.cache = Some(super::super::cache::Request {
                id: 1,
                epoch: 1,
                expected: 1,
                action: if resume {
                    super::super::cache::Action::Resume(saved)
                } else {
                    super::super::cache::Action::Verify(saved)
                },
                cancel: Default::default(),
            });
            super::super::cache::take_request(&mut lane).unwrap()
        };
        service.disconnect_sources(std::slice::from_ref(&root));
        service.reconnect_sources(std::slice::from_ref(&root));
        assert!(request.cancel.is_cancelled());
        assert!(service.lane.lock().unwrap().source_operation_revoked);
        let mut active = None;
        super::super::cache::run(
            request,
            &mut active,
            &service.lane,
            &service.cache_output,
            &service.output,
            &service.preview_output,
        );
        assert!(active.is_none());
        assert!(service.output.lock().unwrap().is_none());
        assert!(
            service
                .lane
                .lock()
                .unwrap()
                .source_operation_owner
                .is_none()
        );
        assert_eq!(
            service.cache_output.lock().unwrap().take().unwrap().status,
            tr!("Cancelled")
        );
    }
    service.stop_for_test();
}

fn publication_blocked(service: &Service) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if service.lane.lock().unwrap().publication_blocked {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "publication did not reach controlled gate"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn release_publication(service: &Service, gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
    service.lane.lock().unwrap().publication_gate = None;
    let (ready, wake) = &**gate;
    *ready.lock().unwrap() = true;
    wake.notify_all();
}
#[test]
fn disconnect_during_certification_before_first_g0_never_publishes_ready() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("share");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), [1; 19]).unwrap();
    let service = Service::start().unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    service.lane.lock().unwrap().publication_gate = Some(gate.clone());
    request(&service, 1, temp.path().into(), vec![root.clone()]);
    publication_blocked(&service);
    service.disconnect_sources(std::slice::from_ref(&root));
    // Restoring eligibility cannot restore the revoked certification token.
    service.reconnect_sources(std::slice::from_ref(&root));
    release_publication(&service, &gate);
    let stopped = wait(&service, |s| !s.running);
    assert_eq!(stopped.shares[0].phase, SharePhase::Cancelled);
    assert!(stopped.shares[0].error.is_none());
    assert!(service.retained_smb_shares().is_empty());
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((1, service::Phase::Cancelled, _))
    ));
    service.stop_for_test();
}
#[test]
fn disconnect_during_g1_certification_keeps_the_certified_g0_selected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("share");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), [1; 17]).unwrap();
    let service = Service::start().unwrap();
    request(&service, 1, temp.path().into(), vec![root.clone()]);
    assert_eq!(wait(&service, |s| !s.running).totals().logical, Some(17));
    assert!(service.open_smb_share(2, 0));
    let previous = root_page(&service, 2);
    let generation = previous.frame.stamp.context.generation;
    drop(previous);
    std::fs::write(root.join("file"), [2; 31]).unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    service.lane.lock().unwrap().publication_gate = Some(gate.clone());
    request(&service, 3, temp.path().into(), vec![root.clone()]);
    publication_blocked(&service);
    service.disconnect_sources(std::slice::from_ref(&root));
    service.reconnect_sources(std::slice::from_ref(&root));
    release_publication(&service, &gate);
    let stopped = wait(&service, |s| s.epoch == 3 && !s.running);
    assert_eq!(stopped.shares[0].phase, SharePhase::Cancelled);
    assert!(stopped.shares[0].error.is_none());
    assert!(matches!(
        service.lane.lock().unwrap().phase,
        Some((3, service::Phase::Cancelled, _))
    ));
    assert!(service.open_smb_share(4, 0));
    let preserved = root_page(&service, 4);
    assert_eq!(preserved.frame.stamp.context.generation, generation);
    assert_eq!(preserved.root_measure.unwrap().logical, Some(17));
    drop(preserved);
    let retained = service.retained_smb_shares();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].logical, Some(17));
    assert!(service.lane.lock().unwrap().start.is_none());
    service.stop_for_test();
}

#[test]
fn strict_reconnect_does_not_allow_retry_of_a_candidate_revoked_during_certification() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), [1; 7]).unwrap();
    let service = Service::start().unwrap();
    let publication = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    {
        let mut lane = service.lane.lock().unwrap();
        lane.force_strict = true;
        lane.publication_gate = Some(publication.clone());
        lane.source_epoch = 1;
        lane.view_epoch = 1;
        lane.start = Some((
            1,
            Start {
                root: root.clone(),
                explicit_root: true,
                observation: sortsize_core::acquisition::ScanObservation::Strict,
                preflight: None,
                options: Default::default(),
            },
        ));
    }
    publication_blocked(&service);
    service.disconnect_sources(std::slice::from_ref(&root));
    service.reconnect_sources(std::slice::from_ref(&root));
    release_publication(&service, &publication);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(
            service.lane.lock().unwrap().phase,
            Some((1, service::Phase::Cancelled, _))
        ) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let (quanta, generation, revoked) = {
        let lane = service.lane.lock().unwrap();
        assert!(!lane.writer_running);
        assert!(lane.writer_owns_session);
        assert!(!lane.reader_active);
        let generation = lane.phase.as_ref().unwrap().2;
        let revoked = lane.acquisition_publication.as_ref().unwrap().1.clone();
        assert!(revoked.is_cancelled());
        (lane.writer_quanta, generation, revoked)
    };
    {
        let mut lane = service.lane.lock().unwrap();
        // A fresh UI token must not restore the revoked candidate's token.
        lane.publication_cancel = Default::default();
        assert!(!lane.publication_cancel.is_cancelled());
        lane.retry = true;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if !service.lane.lock().unwrap().retry {
            break;
        }
        assert!(Instant::now() < deadline, "Retry was not consumed");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Taking Retry precedes its guard in the writer turn. Joining every worker
    // completes that turn before inspection, without waiting for an acquisition
    // tick: a drained cancelled candidate must never reach that tick again.
    let lane = service.lane.clone();
    service.stop_for_test();
    let lane = lane.lock().unwrap();
    assert!(!lane.retry);
    assert!(!lane.writer_running);
    assert!(!lane.writer_owns_session);
    assert!(!lane.reader_active);
    assert!(lane.reader_target.is_none() && lane.reader_epoch.is_none());
    assert_eq!(lane.writer_quanta, quanta);
    assert_eq!(lane.writer_generation, Some(generation));
    assert!(matches!(
        lane.phase,
        Some((1, service::Phase::Cancelled, actual)) if actual == generation
    ));
    assert!(revoked.is_cancelled());
    assert!(
        lane.acquisition_publication
            .as_ref()
            .unwrap()
            .1
            .is_cancelled()
    );
}

#[test]
fn ordinary_cache_cancel_is_not_a_revocation_and_stale_cleanup_keeps_the_new_owner() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("network-source");
    let service = Service::start().unwrap();
    let mut requests = Vec::new();
    for id in [1, 2] {
        let saved = recorded_source(root.clone(), temp.path().join("missing-cache"));
        let mut lane = service.lane.lock().unwrap();
        lane.cache = Some(super::super::cache::Request {
            id,
            epoch: 1,
            expected: 1,
            action: super::super::cache::Action::Resume(saved),
            cancel: Default::default(),
        });
        requests.push(super::super::cache::take_request(&mut lane).unwrap());
    }
    let current = requests.pop().unwrap();
    let old = requests.pop().unwrap();
    old.cancel.cancel();
    assert!(!service.lane.lock().unwrap().source_operation_revoked);
    let mut active = None;
    super::super::cache::run(
        old,
        &mut active,
        &service.lane,
        &service.cache_output,
        &service.output,
        &service.preview_output,
    );
    {
        let lane = service.lane.lock().unwrap();
        assert_eq!(lane.source_operation_owner, Some((2, 1)));
        assert!(!lane.source_operation_revoked);
        assert!(!current.cancel.is_cancelled());
    }
    service.disconnect_sources(std::slice::from_ref(&root));
    service.reconnect_sources(std::slice::from_ref(&root));
    assert!(service.lane.lock().unwrap().source_operation_revoked);
    assert!(current.cancel.is_cancelled());
    super::super::cache::run(
        current,
        &mut active,
        &service.lane,
        &service.cache_output,
        &service.output,
        &service.preview_output,
    );
    assert!(active.is_none());
    assert!(
        service
            .lane
            .lock()
            .unwrap()
            .source_operation_owner
            .is_none()
    );
    service.stop_for_test();
}

#[test]
fn frozen_logical_native_alias_stays_revoked_after_mount_disappears() {
    let service = Service::start().unwrap();
    let logical = PathBuf::from("smb://offline.invalid/TV");
    let native = PathBuf::from("/nonexistent/bunny-gvfs-old-root");
    service.bind_source(logical.clone(), native.clone());
    let pending = sortsize_core::CancellationToken::default();
    service.lane.lock().unwrap().source_operation = Some((native.clone(), pending.clone()));
    service.disconnect_sources(std::slice::from_ref(&logical));
    assert!(service.source_disconnected(&native));
    assert!(service.source_disconnected(&logical));
    assert!(pending.is_cancelled());
    service.reconnect_sources(std::slice::from_ref(&logical));
    assert!(!service.source_disconnected(&native));
    assert!(service.lane.lock().unwrap().start.is_none());
    // The old operation's cancellation is irreversible even if the worker
    // has already drained its transient scope-revocation mailbox.
    assert!(pending.is_cancelled());
    service.stop_for_test();
}

#[test]
fn mid_quantum_disconnect_then_reconnect_keeps_dated_g0_and_cancelled_status() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    std::fs::write(first.join("a"), [1; 12]).unwrap();
    std::fs::write(second.join("b"), [2; 5]).unwrap();
    let service = Service::start().unwrap();
    request(
        &service,
        1,
        temp.path().into(),
        vec![first.clone(), second.clone()],
    );
    let g0 = wait(&service, |snapshot| !snapshot.running);
    assert_eq!(g0.shares[0].logical, Some(12));
    assert!(service.open_smb_share(2, 0));
    assert_eq!(
        root_page(&service, 2).root_measure.unwrap().logical,
        Some(12)
    );
    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let held = Release(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    {
        let mut lane = service.lane.lock().unwrap();
        lane.tick_gate = Some(held.0.clone());
        lane.tick_blocked = false;
    }
    request(&service, 3, temp.path().into(), vec![first.clone(), second]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let lane = service.lane.lock().unwrap();
        if lane.tick_blocked {
            assert_eq!(lane.writer_epoch, Some(3));
            assert_eq!(lane.acquisition_root.as_ref(), Some(&first));
            break;
        }
        drop(lane);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    service.disconnect_sources(std::slice::from_ref(&first));
    service.reconnect_sources(std::slice::from_ref(&first));
    service.lane.lock().unwrap().tick_gate = None;
    drop(held);
    let refresh = wait(&service, |snapshot| {
        snapshot.epoch == 3 && !snapshot.running
    });
    assert_eq!(refresh.shares[0].phase, SharePhase::Cancelled);
    assert!(!refresh.shares[0].current_observation);
    assert_eq!(refresh.shares[1].phase, SharePhase::Complete);
    assert_eq!(refresh.totals().logical, Some(5));
    assert!(service.open_smb_path(4, &first));
    assert_eq!(
        root_page(&service, 4).root_measure.unwrap().logical,
        Some(12)
    );
    assert!(service.lane.lock().unwrap().start.is_none());
    // Reconnect itself was idle. A later confirmed scope must nevertheless be
    // able to recover the frozen candidate in the SAME repository/family.
    std::fs::write(first.join("a"), [3; 19]).unwrap();
    request(
        &service,
        5,
        temp.path().into(),
        vec![first.clone(), temp.path().join("second")],
    );
    let next = wait(&service, |snapshot| {
        snapshot.epoch == 5 && !snapshot.running
    });
    assert_eq!(
        next.shares[0].phase,
        SharePhase::Complete,
        "a later explicit batch must recover its cancelled physical candidate"
    );
    assert_eq!(next.shares[0].logical, Some(19));
    assert_eq!(next.shares[1].phase, SharePhase::Complete);
    assert_eq!(next.totals().logical, Some(24));
    assert!(service.open_smb_path(6, &first));
    assert_eq!(
        root_page(&service, 6).root_measure.unwrap().logical,
        Some(19)
    );
    service.stop_for_test();
}

#[test]
fn new_global_scope_skips_disabled_share_and_keeps_its_certified_g0() {
    let temp = tempfile::tempdir().unwrap();
    let disabled = temp.path().join("disabled");
    let active = temp.path().join("active");
    std::fs::create_dir(&disabled).unwrap();
    std::fs::create_dir(&active).unwrap();
    std::fs::write(disabled.join("file"), [1; 17]).unwrap();
    std::fs::write(active.join("file"), [2; 7]).unwrap();
    let service = Service::start().unwrap();
    request(
        &service,
        1,
        temp.path().into(),
        vec![disabled.clone(), active.clone()],
    );
    wait(&service, |snapshot| !snapshot.running);
    assert!(service.open_smb_path(2, &disabled));
    let original = root_page(&service, 2);
    let generation = original.frame.stamp.context.generation;
    assert_eq!(original.root_measure.unwrap().logical, Some(17));
    drop(original);

    service.disconnect_sources(std::slice::from_ref(&disabled));
    std::fs::write(disabled.join("file"), [3; 29]).unwrap();
    std::fs::write(active.join("file"), [4; 31]).unwrap();
    // A stale discovery result may still contain the disabled share. Admission
    // must reject it independently of the filtered source rows in the UI.
    request(
        &service,
        3,
        temp.path().into(),
        vec![disabled.clone(), active],
    );
    let refreshed = wait(&service, |snapshot| {
        snapshot.epoch == 3 && !snapshot.running
    });
    assert_eq!(refreshed.shares[0].phase, SharePhase::Cancelled);
    assert!(!refreshed.shares[0].current_observation);
    assert_eq!(refreshed.shares[1].phase, SharePhase::Complete);
    assert_eq!(refreshed.totals().logical, Some(31));
    assert!(service.open_smb_path(4, &disabled));
    let preserved = root_page(&service, 4);
    assert_eq!(preserved.frame.stamp.context.generation, generation);
    assert_eq!(preserved.root_measure.unwrap().logical, Some(17));
    drop(preserved);

    service.reconnect_sources(std::slice::from_ref(&disabled));
    assert_eq!(service.smb_snapshot(), Some(refreshed));
    let lane = service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
    drop(lane);
    service.stop_for_test();
}

#[test]
fn logical_server_disconnect_revokes_native_aliases_without_touching_other_server() {
    let service = Service::start().unwrap();
    let server = PathBuf::from("smb://disabled.invalid");
    let share = server.join("TV");
    let other_share = PathBuf::from("smb://active.invalid/TV");
    let native = PathBuf::from("/nonexistent/bunny-gvfs-disabled");
    let other_native = PathBuf::from("/nonexistent/bunny-gvfs-active");
    service.bind_source(share.clone(), native.clone());
    service.bind_source(other_share.clone(), other_native.clone());
    let pending = sortsize_core::CancellationToken::default();
    service.lane.lock().unwrap().source_operation = Some((native.clone(), pending.clone()));

    service.disconnect_sources(std::slice::from_ref(&server));
    assert!(pending.is_cancelled());
    assert!(service.source_disconnected(&share));
    assert!(service.source_disconnected(&native.join("folder")));
    assert!(!service.source_disconnected(&other_share));
    assert!(!service.source_disconnected(&other_native));

    service.reconnect_sources(std::slice::from_ref(&server));
    assert!(!service.source_disconnected(&share));
    assert!(!service.source_disconnected(&native));
    assert!(pending.is_cancelled());
    let lane = service.lane.lock().unwrap();
    assert!(lane.start.is_none() && lane.smb_begin.is_none() && !lane.retry);
    drop(lane);
    service.stop_for_test();
}
