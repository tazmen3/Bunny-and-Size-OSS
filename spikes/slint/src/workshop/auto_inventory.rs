//! Source selection reuses the existing catalogue, certified reader and resume
//! contracts. It never retains, prunes or repairs an inventory implicitly.
use super::*;
#[cfg(test)]
use sortsize_store::inventory::catalogue;

pub(super) fn cancelled_start(epoch: u64, lane: &Mutex<Lane>) {
    let mut l = lane.lock().unwrap();
    if l.source_epoch == epoch {
        let generation = l.phase.map_or(GenerationId(0), |(_, _, g)| g);
        l.phase = Some((epoch, Phase::Cancelled, generation));
        l.writer_running = false;
        l.writer_epoch = None;
        l.writer_owns_session = false;
    }
    drop(l);
    notify(lane);
}

pub(super) fn preserve_reader(
    epoch: u64,
    root: &Path,
    policy: Policy,
    previous: Option<&(RepositoryState, Arc<RepositoryWorkspace>)>,
    lane: &Mutex<Lane>,
) -> bool {
    if let Some((_, workspace)) = previous
        && workspace.root == root
    {
        // If the location is accessible, distinguish a replaced native source
        // before any old page is published under the new source intention.
        // An unavailable location may still consult the already selected G0.
        if let Ok(guard) = Guard::open(root, policy)
            && !workspace
                .repository()
                .lock()
                .unwrap()
                .source_matches(&guard, &Default::default())
                .unwrap_or(false)
        {
            return false;
        }
        let mut l = lane.lock().unwrap();
        if l.source_epoch != epoch {
            return false;
        }
        if !smb_view_matches(&l, root) {
            return true;
        }
        l.reader_epoch = Some((
            l.view_epoch,
            root.to_owned(),
            workspace.inventory,
            workspace.source,
        ));
        true
    } else {
        false
    }
}

pub(super) fn restore(
    epoch: u64,
    start: &Start,
    previous: &mut Option<(RepositoryState, Arc<RepositoryWorkspace>)>,
    lane: &Arc<Mutex<Lane>>,
    cancel: &sortsize_core::CancellationToken,
    may_use_reader_preview: bool,
    direct_requested: bool,
) -> sortsize_store::inventory::Result<Option<(Active, DiskBudget)>> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let policy = Policy {
        explicit_root: start.explicit_root,
        ..Default::default()
    };
    let quick_guard = direct_requested
        .then(|| Guard::open(&start.root, policy))
        .transpose()?;
    let direct_local = quick_guard.as_ref().is_some_and(|guard| {
        guard.evidence().0.class == sortsize_core::acquisition::SourceClass::Local
    });
    if direct_local {
        // An explicit fast traversal neither resumes a persistent candidate
        // nor drops the workspace owning a cancelled candidate or certified G0.
        preserve_reader(epoch, &start.root, policy, previous.as_ref(), lane);
        return Ok(None);
    }
    if preserve_reader(epoch, &start.root, policy, previous.as_ref(), lane) {
        // Keep an already open G0 usable even if the new preparation fails.
        return Ok(None);
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    // An unrelated source cannot supply the next generation's repository.
    drop(previous.take());
    let location = work_location()?;
    let guard = location.scope(match quick_guard {
        Some(guard) => guard,
        None => Guard::open(&start.root, policy)?,
    })?;
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let configured = lane.lock().unwrap().cache_root.clone();
    let legacy = configured
        .is_none()
        .then(super::cache::legacy_catalogue)
        .flatten()
        .filter(|path| location.permits_writer(path).unwrap_or(false));
    let root = configured.unwrap_or_else(|| location.catalogue());
    // A damaged/unavailable catalogue does not prevent a fresh acquisition.
    let catalogue = match super::cache::list_catalogues(&root, legacy.as_deref(), &[]) {
        Ok(catalogue) => catalogue,
        Err(_) => return Ok(None),
    };
    let matching: Vec<_> = catalogue
        .rows
        .iter()
        .filter(|saved| saved.root == start.root)
        .filter(|saved| location.permits_writer(&saved.directory).unwrap_or(false))
        .collect();
    if let Some(saved) = matching.first() {
        lane.lock().unwrap().phase = Some((epoch, Phase::Reopening, saved.context.generation));
    }
    // Cancelling acquisition must not discard a valid dated inventory. Reading
    // it has no source I/O and remains independent from the writer's token.
    let read_cancel = sortsize_core::CancellationToken::default();
    let mut workspace = None;
    for saved in matching.iter().filter(|saved| saved.measured.is_some()) {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let Ok(mut reader) = Active::open_saved(epoch, saved, None, &read_cancel) else {
            continue;
        };
        // Paths and catalogue hints do not authorize reuse after replacement.
        if !Session::source_matches(reader.session.store(), reader.c.generation, &guard)? {
            continue;
        }
        if !matches!(reader.session, Backend::Certified(_))
            && guard.work_path_allowed(reader.session.store().directory())?
        {
            let Ok((certified, parked)) = reader.into_reader() else {
                continue;
            };
            drop(parked);
            reader = certified;
        }
        workspace = reader.workspace.clone();
        let mut l = lane.lock().unwrap();
        if l.source_epoch != epoch {
            return Ok(None);
        }
        reader.epoch = l.view_epoch;
        if !smb_view_matches(&l, &start.root) {
            drop(l);
            break;
        }
        l.reader_epoch = Some((
            l.view_epoch,
            start.root.clone(),
            reader.c.inventory,
            reader.c.source,
        ));
        l.reader_active = true;
        let old_workspace = std::mem::replace(&mut l.reader_workspace, workspace.clone());
        let old_target = l.reader_target.replace(Some(Box::new(reader)));
        drop(l);
        drop(old_workspace);
        drop(old_target);
        notify(lane);
        break;
    }
    if let Some(workspace) = &workspace {
        *previous = Some((
            RepositoryState {
                owner: workspace.repository(),
                plan: None,
                published: None,
            },
            workspace.clone(),
        ));
    }
    for saved in matching.iter().filter(|saved| saved.measured.is_none()) {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        if !start.accepts_resume(saved.context.policy) {
            continue;
        }
        // Resume only the selected inventory family when G0 is already open.
        if workspace.as_ref().is_some_and(|owner| {
            owner.inventory != saved.context.inventory || owner.source != saved.context.source
        }) {
            continue;
        }
        let Ok(view) = Active::open_saved(epoch, saved, workspace.clone(), cancel) else {
            continue;
        };
        let options = view
            .session
            .store()
            .collection_options(saved.context.generation)?;
        let compatible =
            Session::resume_scope_matches(view.session.store(), saved.context.generation, &guard)?
                && guard.work_path_allowed(view.session.store().directory())?
                && options == Some((start.options.allocation, start.options.owners));
        drop(view);
        if !compatible {
            continue;
        }
        // The existing recovery path fences attempts and checks the fresh Guard.
        // A failure preserves the independently owned G0 and all candidate bytes.
        let disk = acquisition_budget(may_use_reader_preview && lane.lock().unwrap().reader_active);
        return Active::resume_saved_with_options(
            epoch,
            guard.policy(),
            saved,
            workspace.clone(),
            cancel,
            cancel.clone(),
            ResumeOptions {
                expected_collection: None,
                disk,
            },
        )
        .map(|active| Some((active, disk)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(root: &Path) -> Start {
        Start {
            root: root.to_owned(),
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: None,
            options: Default::default(),
        }
    }

    fn retained_measure(source: &Path, state: &Path) -> catalogue::Saved {
        let mut active = Active::open(1, request(source), None, Default::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while active.session.status().state != State::Sealed {
            let status = active.session.tick().unwrap();
            if matches!(status.state, State::Idle | State::Cancelled) && status.in_flight == 0 {
                active.session.finalize_step().unwrap();
            }
            assert!(Instant::now() < deadline);
        }
        let measured = std::time::SystemTime::now();
        active
            .session
            .store_mut()
            .unwrap()
            .record_measurement(
                active.c.generation,
                source,
                super::super::super::cache::seconds(active.started),
                super::super::super::cache::seconds(measured),
            )
            .unwrap();
        active.sealed = Some(measured);
        let mut reader = active
            .publish(&Mutex::new(Lane::default()))
            .unwrap()
            .unwrap();
        drop(active.take_repository());
        drop(active);
        reader.retain(state, &Default::default()).unwrap();
        drop(reader);
        catalogue::list(state, None).unwrap().rows.remove(0)
    }

    fn lane(state: &Path) -> Arc<Mutex<Lane>> {
        Arc::new(Mutex::new(Lane {
            source_epoch: 2,
            view_epoch: 2,
            cache_root: Some(state.to_owned()),
            ..Default::default()
        }))
    }

    #[test]
    fn automatic_reopen_keeps_certified_g0_through_refresh_failure_and_cancel() {
        sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("before"), [1; 7]).unwrap();
        let saved = retained_measure(&source, &state);
        std::fs::write(source.join("after"), [2; 11]).unwrap();
        let lane = lane(&state);
        let mut previous = None;
        assert!(
            restore(
                2,
                &request(&source),
                &mut previous,
                &lane,
                &Default::default(),
                true,
                false,
            )
            .unwrap()
            .is_none()
        );
        let g0 = lane.lock().unwrap().reader_target.take().unwrap().unwrap();
        assert!(matches!(g0.session, Backend::Certified(_)));
        assert_eq!(g0.c, saved.context);
        assert_eq!(g0.session.store().entry_count(g0.c.generation).unwrap(), 2);
        assert!(g0.session.store().totals(g0.c.generation).unwrap().logical == Some(7));
        #[cfg(windows)]
        {
            let workspace = previous.as_ref().unwrap().1.clone();
            assert!(
                restore(
                    2,
                    &request(&source),
                    &mut previous,
                    &lane,
                    &Default::default(),
                    true,
                    true,
                )
                .unwrap()
                .is_none()
            );
            assert!(Arc::ptr_eq(&workspace, &previous.as_ref().unwrap().1));
            assert_eq!(g0.c, saved.context);
            assert_eq!(g0.session.store().entry_count(g0.c.generation).unwrap(), 2);
            assert_eq!(
                g0.session.store().totals(g0.c.generation).unwrap().logical,
                Some(7)
            );
        }
        let failed = Start {
            explicit_root: false,
            observation: crate::workshop::service::size_observation(),
            preflight: Some(Box::new(|| false)),
            ..request(&source)
        };
        // Preparation failure consumes only its writer lease, never the reader.
        let workspace = previous.as_ref().unwrap().1.clone();
        assert!(Active::open(2, failed, previous.take(), Default::default()).is_err());
        assert_eq!(g0.session.store().entry_count(g0.c.generation).unwrap(), 2);
        previous = Some((
            RepositoryState {
                owner: workspace.repository(),
                plan: None,
                published: None,
            },
            workspace,
        ));
        let cancel = sortsize_core::CancellationToken::default();
        let writer = Active::open_with_placement_log(
            2,
            request(&source),
            previous.take(),
            cancel.clone(),
            acquisition_budget(lane.lock().unwrap().reader_active),
            None,
            false,
        )
        .unwrap();
        assert_eq!(writer.c.inventory, saved.context.inventory);
        assert!(writer.c.generation.0 > saved.context.generation.0);
        cancel.cancel();
        writer.session.control().cancel();
        drop(writer);
        assert_eq!(
            g0.session.store().totals(g0.c.generation).unwrap().logical,
            Some(7)
        );
        drop(g0);
        drain_stopped_lane(&lane);
        assert!(saved.directory.exists());
    }

    #[test]
    fn automatic_reopen_rejects_a_replacement_at_the_same_native_path() {
        sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("old"), [1; 3]).unwrap();
        let saved = retained_measure(&source, &state);
        let reader = Active::open_saved(1, &saved, None, &Default::default()).unwrap();
        let workspace = reader.workspace.clone().unwrap();
        let mut previous = Some((
            RepositoryState {
                owner: workspace.repository(),
                plan: None,
                published: None,
            },
            workspace,
        ));
        std::fs::rename(&source, temp.path().join("old-source")).unwrap();
        std::fs::create_dir(&source).unwrap();
        let lane = lane(&state);
        assert!(!preserve_reader(
            2,
            &source,
            Policy::default(),
            previous.as_ref(),
            &lane
        ));
        assert!(lane.lock().unwrap().reader_epoch.is_none());
        drop(reader);
        assert!(
            restore(
                2,
                &request(&source),
                &mut previous,
                &lane,
                &Default::default(),
                true,
                false,
            )
            .unwrap()
            .is_none()
        );
        assert!(previous.is_none());
        assert!(lane.lock().unwrap().reader_target.is_none());
        assert!(saved.directory.exists());
    }

    #[test]
    fn automatic_reopen_resumes_retained_candidate_with_its_existing_generation() {
        sortsize_core::resources::Governor::global().configure(MemoryMode::Entries50k, None);
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        std::fs::create_dir(&source).unwrap();
        for i in 0..32 {
            std::fs::write(source.join(format!("file-{i}")), [1; 3]).unwrap();
        }
        let mut active = Active::open(1, request(&source), None, Default::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while active
            .session
            .store()
            .entry_count(active.c.generation)
            .unwrap()
            <= 1
        {
            active.session.tick().unwrap();
            assert!(Instant::now() < deadline);
        }
        active.retain(&state, &Default::default()).unwrap();
        let before = active.c;
        drop(active);
        let saved = catalogue::list(&state, None).unwrap().rows.remove(0);
        let mut reader = Active::open_saved(2, &saved, None, &Default::default()).unwrap();
        assert!(!reader.sync_sort(3));
        assert!(reader.advance_order().unwrap());
        let page = reader
            .prepare(crate::units::Units::default(), None)
            .unwrap();
        assert!(page.frame.provisional);
        assert!(!page.frame.rows.is_empty());
        assert_eq!(page.sort_index, -1);
        drop(reader);
        let lane = lane(&state);
        #[cfg(windows)]
        {
            // Explicit fast mode must not recover this persisted partial
            // candidate, even though normal recovery below must still do so.
            let mut previous = None;
            assert!(
                restore(
                    2,
                    &request(&source),
                    &mut previous,
                    &lane,
                    &Default::default(),
                    true,
                    true,
                )
                .unwrap()
                .is_none()
            );
            assert!(previous.is_none());
            let unchanged = catalogue::list(&state, None).unwrap();
            assert_eq!(unchanged.rows.len(), 1);
            assert_eq!(unchanged.rows[0].context, saved.context);
            assert_eq!(unchanged.rows[0].measured, None);
            assert!(saved.directory.exists());
        }
        let (resumed, _) = restore(
            2,
            &request(&source),
            &mut None,
            &lane,
            &Default::default(),
            true,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resumed.c.inventory, before.inventory);
        assert_eq!(resumed.c.generation, before.generation);
        assert!(resumed.c.run.0 > before.run.0);
        assert!(matches!(resumed.session, Backend::Live(_)));
        assert!(resumed.retained_copy.as_ref().unwrap().exists());
    }
}
