//! An offline snapshot has no source Guard and cannot grant live operations.
use super::*;
use sortsize_store::{direct_snapshot, scheduler::Control};
use std::sync::{Mutex, RwLock, atomic::AtomicU64};

pub(super) enum ViewScan {
    Live(DirectScan),
    Loaded {
        tree: Arc<RwLock<DirectTree>>,
        scratch: Arc<tempfile::TempDir>,
        control: Control,
        progress: Arc<AtomicU64>,
        preparation: CancellationToken,
    },
}

impl ViewScan {
    pub(super) fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded { .. })
    }
    pub(super) fn tree(&self) -> &Arc<RwLock<DirectTree>> {
        match self {
            Self::Live(s) => s.tree(),
            Self::Loaded { tree, .. } => tree,
        }
    }
    pub(super) fn scratch(&self) -> Arc<tempfile::TempDir> {
        match self {
            Self::Live(s) => s.scratch(),
            Self::Loaded { scratch, .. } => Arc::clone(scratch),
        }
    }
    pub(super) fn guard(&self) -> Result<Arc<Guard>> {
        match self {
            Self::Live(s) => Ok(s.guard()),
            Self::Loaded { .. } => Err(Error::Contract(
                "offline snapshot has no live source authority",
            )),
        }
    }
    pub(super) fn status(&self) -> DirectStatus {
        match self {
            Self::Live(s) => s.status(),
            Self::Loaded { .. } => DirectStatus::Finished,
        }
    }
    pub(super) fn control(&self) -> Control {
        match self {
            Self::Live(s) => s.control(),
            Self::Loaded { control, .. } => control.clone(),
        }
    }
    pub(super) fn progress_counter(&self) -> Arc<AtomicU64> {
        match self {
            Self::Live(s) => s.progress_counter(),
            Self::Loaded { progress, .. } => Arc::clone(progress),
        }
    }
    pub(super) fn cancel(&self) {
        match self {
            Self::Live(s) => s.cancel(),
            Self::Loaded { preparation, .. } => preparation.cancel(),
        }
    }
}

impl Drop for ViewScan {
    fn drop(&mut self) {
        if let Self::Loaded { preparation, .. } = self {
            preparation.cancel();
        }
    }
}

impl DirectActive {
    pub(in super::super) fn open_snapshot(
        source_epoch: u64,
        saved: &sortsize_store::inventory::catalogue::Saved,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let location = work_location()?;
        let scratch = location.temporary("bunny-snapshot-view-")?;
        let limit = native_free_disk(scratch.path())
            .map(disk_limit)
            .ok_or(Error::Contract("working volume free space unavailable"))?;
        let loaded = direct_snapshot::load(&saved.directory, scratch.path(), limit, cancel)?;
        let meta = loaded.metadata;
        if meta.context != saved.context
            || meta.root != saved.root
            || Some(meta.measured) != saved.measured
        {
            return Err(Error::Contract(
                "saved snapshot changed; refresh the catalogue",
            ));
        }
        if cancel.is_cancelled() {
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
        }
        let progress = Arc::new(AtomicU64::new(meta.summary.observations));
        let excluded = direct_snapshot::excluded_state_path(&meta)?.unwrap_or_default();
        let started = cache::system_time(meta.started)?;
        let measured = cache::system_time(meta.measured)?;
        let preparation = CancellationToken::default();
        let scratch = Arc::new(scratch);
        let prepare =
            loaded.tree.len() >= 100_000 && meta.summary.coverage == ScopeCoverage::Complete;
        let tree = Arc::new(RwLock::new(loaded.tree));
        if prepare {
            // The restored tree can already answer bounded queries. Keep
            // optional preparation off the service turn and reserve just one
            // active offline preparation even when views are replaced rapidly.
            static SLOT: Mutex<()> = Mutex::new(());
            let tree = Arc::clone(&tree);
            let scratch = Arc::clone(&scratch);
            let cancel = preparation.clone();
            let _ = std::thread::Builder::new()
                .name("bunny-offline-query-indexes".into())
                .spawn(move || {
                    let _scratch = scratch;
                    if let Ok(_slot) = SLOT.try_lock()
                        && let Ok(tree) = tree.read()
                    {
                        tree.prepare_query_indexes(&cancel);
                    }
                });
        }
        Ok(Self {
            scan: ViewScan::Loaded {
                tree,
                scratch,
                control: Control::default(),
                progress,
                preparation,
            },
            context: meta.context,
            source_epoch,
            publication_cancel: cancel.clone(),
            root: meta.root,
            workspace: excluded,
            options: meta.options,
            started,
            measured: Some(measured),
            terminal_reported: false,
            saved: true,
            saved_path: Some(saved.directory.clone()),
            saving: None,
            autosave_attempted: true,
            epoch: source_epoch,
            parent: EntryId(0),
            after: None,
            native_previous: Vec::new(),
            observation_pending: false,
            observation_published: false,
            expanded: BTreeMap::from([(EntryId(0), None)]),
            last: None,
            force: true,
            presentation: None,
            view_metrics: None,
            sort_index: 0,
            owner_labels: Vec::new(),
            detail_query: None,
            branch_queries: BTreeMap::new(),
            branch_cursor: None,
            search: None,
        })
    }

    pub(in super::super) fn saved_record(
        &self,
    ) -> Result<sortsize_store::inventory::catalogue::Saved> {
        let path = self
            .saved_path
            .as_ref()
            .filter(|_| self.saved)
            .ok_or(Error::Contract(
                "wait for the inventory to be saved before comparing",
            ))?;
        let metadata = direct_snapshot::inspect(path)?;
        if metadata.context != self.context || metadata.root != self.root {
            return Err(Error::Contract(
                "saved snapshot changed; refresh the catalogue",
            ));
        }
        Ok(sortsize_store::inventory::catalogue::Saved {
            directory: path.clone(),
            context: metadata.context,
            root: metadata.root,
            started: metadata.started,
            measured: Some(metadata.measured),
            coverage: metadata.summary.coverage,
            logical: metadata.summary.measure.logical,
            unknown: metadata.summary.measure.unknown,
            continuity: None,
        })
    }

    pub(in super::super) fn refresh_after_compatibility(&mut self) {
        self.force = true;
    }
}
