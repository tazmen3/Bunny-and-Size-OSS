//! Bounded virtual projection over independent share inventories. No server
//! directory enumeration, entry copy, or cross-share identity merge occurs here.
use super::*;
use service::RepositoryWorkspace;

pub(super) const MAX_SHARES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SharePhase {
    Queued,
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Share {
    pub path: PathBuf,
    /// Frozen native mount for this operation; URI identities remain in `path`.
    /// An absent mount fails this share without invalidating the other shares.
    pub native_root: Option<PathBuf>,
    pub logical: Option<u64>,
    pub files: Option<u64>,
    pub directories: Option<u64>,
    pub coverage: ScopeCoverage,
    pub phase: SharePhase,
    pub error: Option<String>,
    pub measured: Option<std::time::SystemTime>,
    /// False for dated G0 seeds; only this operation's observations contribute
    /// to its lower bounds. A previous inventory may be larger than its G1.
    pub current_observation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub epoch: u64,
    pub server: PathBuf,
    pub shares: Vec<Share>,
    pub running: bool,
    pub cancelled: bool,
    pub discovery_uncertain: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Totals {
    pub logical: Option<u64>,
    pub files: Option<u64>,
    pub directories: Option<u64>,
    pub coverage: ScopeCoverage,
}

impl Snapshot {
    pub fn completed(&self) -> usize {
        self.shares
            .iter()
            .filter(|share| share.phase == SharePhase::Complete)
            .count()
    }

    /// Observed logical sums, not physical usage or a deduplicated NAS total.
    /// A missing component makes coverage partial and never becomes an exact zero.
    pub fn totals(&self) -> Totals {
        let sum = |value: fn(&Share) -> Option<u64>| {
            self.shares
                .iter()
                .filter(|share| share.current_observation)
                .filter_map(value)
                .try_fold(None, |sum, n| sum.unwrap_or(0u64).checked_add(n).map(Some))
                .flatten()
        };
        let logical = sum(|share| share.logical);
        let files = sum(|share| share.files);
        let directories = sum(|share| share.directories);
        let complete = !self.discovery_uncertain
            && !self.running
            && !self.cancelled
            && !self.shares.is_empty()
            && self.shares.iter().all(|share| {
                share.phase == SharePhase::Complete
                    && share.current_observation
                    && share.coverage == ScopeCoverage::Complete
                    && share.logical.is_some()
                    && share.files.is_some()
                    && share.directories.is_some()
            })
            && logical.is_some()
            && files.is_some()
            && directories.is_some();
        Totals {
            logical,
            files,
            directories,
            coverage: if complete {
                ScopeCoverage::Complete
            } else if logical.is_some() || files.is_some() || directories.is_some() {
                ScopeCoverage::Partial
            } else {
                ScopeCoverage::Unknown
            },
        }
    }
}

pub(super) struct Request {
    pub snapshot: Snapshot,
    pub options: sortsize_store::scheduler::CollectionOptions,
    _memory: Arc<sortsize_core::resources::Reservation>,
}
impl Request {
    pub fn disable_sources(&mut self, scopes: &[PathBuf]) {
        disable_sources(&mut self.snapshot, scopes);
    }
    pub fn memory(&self) -> Arc<sortsize_core::resources::Reservation> {
        self._memory.clone()
    }
    pub fn new(
        epoch: u64,
        server: PathBuf,
        shares: Vec<PathBuf>,
        discovery_uncertain: bool,
        options: sortsize_store::scheduler::CollectionOptions,
    ) -> Result<Self, String> {
        if shares.is_empty() || shares.len() > MAX_SHARES {
            return Err(tr!("Could not start scan.").into());
        }
        #[cfg(windows)]
        {
            use sortsize_core::volumes::smb::parse_target;
            let target = server
                .to_str()
                .and_then(|path| parse_target(path).ok())
                .filter(|target| target.share.is_none())
                .ok_or(tr!("Could not start scan."))?;
            for path in &shares {
                use std::path::{Component, Prefix};
                // Discovery returns native Windows paths. Only the server
                // address is text; a share name must retain its original UTF-16.
                let Some(Component::Prefix(prefix)) = path.components().next() else {
                    return Err(tr!("Could not start scan.").into());
                };
                let (server, share) = match prefix.kind() {
                    Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                        (server, share)
                    }
                    _ => return Err(tr!("Could not start scan.").into()),
                };
                if share.is_empty()
                    || !path
                        .components()
                        .all(|part| matches!(part, Component::Prefix(_) | Component::RootDir))
                    || sortsize_core::volumes::unc_source(path).is_err()
                    || !server
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&target.server.to_string_lossy())
                {
                    return Err(tr!("Could not start scan.").into());
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            use sortsize_core::volumes::smb::parse_target;
            let target = server
                .to_str()
                .and_then(|path| parse_target(path).ok())
                .filter(|target| target.share.is_none())
                .ok_or(tr!("Could not start scan."))?;
            for path in &shares {
                let share = path
                    .to_str()
                    .and_then(|path| parse_target(path).ok())
                    .filter(|share| share.share.as_deref() == Some(path))
                    .ok_or(tr!("Could not start scan."))?;
                let prefix = target.server_unc();
                let suffix = path
                    .to_str()
                    .and_then(|path| path.strip_prefix(prefix.to_str()?));
                if share.server != target.server
                    || !suffix.is_some_and(|suffix| !suffix.is_empty() && !suffix.contains('/'))
                {
                    return Err(tr!("Could not start scan.").into());
                }
            }
            let mut request = Self::confirmed(epoch, server, shares, discovery_uncertain, options)?;
            for share in &mut request.snapshot.shares {
                share.native_root = sortsize_core::volumes::smb::resolve_path(&share.path).ok();
            }
            Ok(request)
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (epoch, server, shares, discovery_uncertain, options);
            Err(tr!("Could not start scan.").into())
        }
        #[cfg(windows)]
        Self::confirmed(epoch, server, shares, discovery_uncertain, options)
    }

    #[cfg(any(windows, target_os = "linux", test))]
    fn confirmed(
        epoch: u64,
        server: PathBuf,
        shares: Vec<PathBuf>,
        discovery_uncertain: bool,
        options: sortsize_store::scheduler::CollectionOptions,
    ) -> Result<Self, String> {
        let mut seen = std::collections::HashSet::new();
        if shares.is_empty()
            || shares.len() > MAX_SHARES
            || shares.iter().any(|path| !seen.insert(path.clone()))
        {
            return Err(tr!("Could not start scan.").into());
        }
        let bytes = shares.iter().map(|path| path.capacity()).sum::<usize>()
            + server.capacity()
            + shares.capacity() * std::mem::size_of::<Share>();
        if bytes > 1024 * 1024 {
            return Err(tr!("Could not start scan.").into());
        }
        let memory = sortsize_core::resources::Governor::global()
            .reserve(
                sortsize_core::resources::Category::Publications,
                (bytes * 10 + 32 * 1024) as u64,
                None,
            )
            .ok_or(tr!("Could not start scan."))?;
        Ok(Self {
            snapshot: Snapshot {
                epoch,
                server,
                shares: shares
                    .into_iter()
                    .map(|path| Share {
                        native_root: Some(path.clone()),
                        path,
                        logical: None,
                        files: None,
                        directories: None,
                        coverage: ScopeCoverage::Unknown,
                        phase: SharePhase::Queued,
                        error: None,
                        measured: None,
                        current_observation: false,
                    })
                    .collect(),
                running: true,
                cancelled: false,
                discovery_uncertain,
            },
            options,
            _memory: Arc::new(memory),
        })
    }
}

#[derive(Clone)]
pub(super) struct Kept {
    pub workspace: Arc<RepositoryWorkspace>,
    pub generation: GenerationId,
    pub share: Share,
    pub _memory: Arc<sortsize_core::resources::Reservation>,
}

pub(super) struct Batch {
    pub snapshot: Snapshot,
    pub options: sortsize_store::scheduler::CollectionOptions,
    pub current: Option<usize>,
    _memory: Arc<sortsize_core::resources::Reservation>,
}
fn disable_sources(snapshot: &mut Snapshot, scopes: &[PathBuf]) {
    for share in &mut snapshot.shares {
        if matches!(share.phase, SharePhase::Queued | SharePhase::Running)
            && scopes.iter().any(|scope| {
                crate::source_registry::covers(scope, &share.path)
                    || share
                        .native_root
                        .as_ref()
                        .is_some_and(|root| crate::source_registry::covers(scope, root))
            })
        {
            share.phase = SharePhase::Cancelled;
            if share.current_observation {
                share.coverage = ScopeCoverage::Partial;
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn native_mount_unavailable() -> &'static str {
    tr!(
        "No accessible Ubuntu mount was found for this SMB share. Open it in Files, then click Reconnect. A successful connection does not start scanning."
    )
}

impl Batch {
    pub fn disable_sources(&mut self, scopes: &[PathBuf]) {
        disable_sources(&mut self.snapshot, scopes);
        self.snapshot.running = self.current.is_some()
            || self
                .snapshot
                .shares
                .iter()
                .any(|share| share.phase == SharePhase::Queued);
    }
    pub fn memory(&self) -> Arc<sortsize_core::resources::Reservation> {
        self._memory.clone()
    }
    pub fn new(request: Request) -> Self {
        Self {
            snapshot: request.snapshot,
            options: request.options,
            current: None,
            _memory: request._memory,
        }
    }
    pub fn next(&mut self) -> Option<Start> {
        if self.current.is_some() || self.snapshot.cancelled {
            return None;
        }
        for (index, share) in self.snapshot.shares.iter_mut().enumerate() {
            if share.phase != SharePhase::Queued {
                continue;
            }
            let Some(root) = share.native_root.clone() else {
                share.phase = SharePhase::Failed;
                #[cfg(target_os = "linux")]
                let message = native_mount_unavailable();
                #[cfg(not(target_os = "linux"))]
                let message = tr!("Source is no longer available.");
                share.error = Some(message.into());
                share.coverage = ScopeCoverage::Unknown;
                continue;
            };
            share.phase = SharePhase::Running;
            self.current = Some(index);
            return Some(Start {
                root,
                explicit_root: true,
                observation: sortsize_core::acquisition::ScanObservation::Strict,
                preflight: None,
                options: self.options,
            });
        }
        self.snapshot.running = false;
        None
    }
    pub fn observe(
        &mut self,
        measure: Option<sortsize_store::inventory::folder_totals::FolderMeasure>,
        coverage: ScopeCoverage,
        provisional: bool,
    ) {
        let Some(index) = self.current else { return };
        let share = &mut self.snapshot.shares[index];
        if let Some(measure) = measure {
            share.current_observation = true;
            share.measured = None;
            share.logical = measure.logical;
            share.files = Some(measure.files);
            share.directories = (!provisional).then_some(measure.directories);
            share.coverage = if provisional || measure.unknown > 0 {
                ScopeCoverage::Partial
            } else {
                coverage
            };
        }
    }
    pub fn finish(&mut self, error: Option<String>) {
        if let Some(index) = self.current.take() {
            let share = &mut self.snapshot.shares[index];
            share.phase = if self.snapshot.cancelled || share.phase == SharePhase::Cancelled {
                SharePhase::Cancelled
            } else if error.is_some() {
                SharePhase::Failed
            } else {
                SharePhase::Complete
            };
            share.error = error;
            if share.phase != SharePhase::Complete {
                share.coverage = if share.current_observation && share.files.is_some() {
                    ScopeCoverage::Partial
                } else {
                    ScopeCoverage::Unknown
                };
            }
        }
        self.snapshot.running = self.current.is_some()
            || self
                .snapshot
                .shares
                .iter()
                .any(|share| share.phase == SharePhase::Queued);
    }
    pub fn cancel(&mut self) {
        self.snapshot.cancelled = true;
        for share in &mut self.snapshot.shares {
            if share.phase == SharePhase::Queued {
                share.phase = SharePhase::Cancelled;
            }
        }
        self.snapshot.running = self.current.is_some();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    #[test]
    fn confirmed_share_roots_preserve_native_utf16_without_lossy_reconstruction() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut units = r"\\native-host\".encode_utf16().collect::<Vec<_>>();
        units.extend([0xd800, b'x' as u16]);
        let path = PathBuf::from(std::ffi::OsString::from_wide(&units));
        let request = Request::new(
            1,
            r"\\native-host".into(),
            vec![path.clone()],
            false,
            Default::default(),
        )
        .unwrap();
        assert_eq!(
            request.snapshot.shares[0]
                .path
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            units
        );
        assert!(
            Request::new(
                1,
                r"\\other-host".into(),
                vec![path],
                false,
                Default::default()
            )
            .is_err()
        );
        assert!(
            Request::new(
                1,
                r"\\native-host".into(),
                vec![r"\\native-host\share\child".into()],
                false,
                Default::default()
            )
            .is_err()
        );
    }

    fn injected(service: &Service, epoch: u64, roots: Vec<PathBuf>) {
        let request = Request::confirmed(
            epoch,
            PathBuf::from("test-server"),
            roots,
            false,
            Default::default(),
        )
        .unwrap();
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

    fn wait(service: &Service, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(snapshot) = service.smb_snapshot()
                && predicate(&snapshot)
            {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "batch timeout: {:?}",
                service.smb_snapshot()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn page(service: &Service, epoch: u64) -> Box<Prepared> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let output = service.output.lock().unwrap().take();
            match output {
                Some(Output::Page(page)) if page.epoch == epoch && page.measure_ready => {
                    return page;
                }
                Some(Output::Error(_, error, _)) => panic!("share read: {error}"),
                _ => {}
            }
            assert!(Instant::now() < deadline, "share reader timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn batch() -> Batch {
        Batch::new(
            Request::confirmed(
                1,
                PathBuf::from("server"),
                vec![PathBuf::from("share-a"), PathBuf::from("share-b")],
                false,
                Default::default(),
            )
            .unwrap(),
        )
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn unmounted_share_failure_uses_the_initiating_ui_language_in_all_seven_catalogs() {
        for language in crate::locale::Language::ALL {
            let presentation = crate::presentation::Context {
                language,
                generation: 1,
            };
            let expected = {
                let _scope = presentation.enter();
                tr!("No accessible Ubuntu mount was found for this SMB share. Open it in Files, then click Reconnect. A successful connection does not start scanning.").to_owned()
            };
            let service = Service::start().unwrap();
            service.lane.lock().unwrap().presentation = presentation;
            service
                .begin_smb_batch(
                    1,
                    "smb://offline.invalid/".into(),
                    vec!["smb://offline.invalid/unmounted-qualification".into()],
                    false,
                    Default::default(),
                )
                .unwrap();
            let snapshot = wait(&service, |snapshot| !snapshot.running);
            service.stop_for_test();
            assert_eq!(snapshot.shares[0].phase, SharePhase::Failed);
            assert_eq!(
                snapshot.shares[0].error.as_deref(),
                Some(expected.as_str()),
                "{language:?}"
            );
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_confirmation_keeps_uri_identity_and_never_scans_an_unmounted_share() {
        let request = Request::new(
            1,
            "smb://offline.invalid/".into(),
            vec![
                "smb://offline.invalid/TV".into(),
                "smb://offline.invalid/Docs".into(),
            ],
            false,
            Default::default(),
        )
        .unwrap();
        assert!(
            request
                .snapshot
                .shares
                .iter()
                .all(|share| share.native_root.is_none())
        );
        let mut batch = Batch::new(request);
        assert!(batch.next().is_none());
        assert!(!batch.snapshot.running);
        assert!(
            batch
                .snapshot
                .shares
                .iter()
                .all(|share| share.phase == SharePhase::Failed)
        );
        assert_eq!(batch.snapshot.totals().logical, None);
        assert!(
            Request::new(
                1,
                "smb://nas.invalid/".into(),
                vec!["smb://other.invalid/TV".into()],
                false,
                Default::default()
            )
            .is_err()
        );
        assert!(
            Request::new(
                1,
                "smb://nas.invalid/".into(),
                vec!["/tmp/local-directory".into()],
                false,
                Default::default()
            )
            .is_err()
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires two real, readable mounted SMB shares in BUNNY_SMB_TEST_SERVER/SHARE_A/SHARE_B"]
    fn linux_native_smb_batch_preserves_per_share_g0_revocation_and_offline_views() {
        let server = PathBuf::from(std::env::var_os("BUNNY_SMB_TEST_SERVER").expect("server URI"));
        let a = PathBuf::from(std::env::var_os("BUNNY_SMB_TEST_SHARE_A").expect("share A URI"));
        let b = PathBuf::from(std::env::var_os("BUNNY_SMB_TEST_SHARE_B").expect("share B URI"));
        let service = Service::start().unwrap();
        service
            .begin_smb_batch(
                1,
                server.clone(),
                vec![a.clone(), b.clone()],
                false,
                Default::default(),
            )
            .unwrap();
        let g0 = wait(&service, |snapshot| !snapshot.running);
        assert!(
            g0.shares
                .iter()
                .all(|share| share.phase == SharePhase::Complete && share.native_root.is_some())
        );
        assert_eq!(g0.totals().coverage, ScopeCoverage::Complete);
        assert!(
            g0.shares
                .iter()
                .all(|share| share.measured.is_some() && share.logical.is_some())
        );
        let a_measure = g0.shares[0].logical.unwrap();
        let b_measure = g0.shares[1].logical.unwrap();
        for (index, key) in [
            (0, "BUNNY_SMB_TEST_EXPECT_A"),
            (1, "BUNNY_SMB_TEST_EXPECT_B"),
        ] {
            if let Ok(expected) = std::env::var(key) {
                let values = expected
                    .split(',')
                    .map(|value| value.parse::<u64>().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(g0.shares[index].logical, Some(values[0]));
                assert_eq!(g0.shares[index].files, Some(values[1]));
            }
        }
        assert!(service.open_smb_share(2, 0));
        assert_eq!(
            page(&service, 2).root_measure.unwrap().logical,
            Some(a_measure)
        );
        struct ReleaseTick(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for ReleaseTick {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let held = ReleaseTick(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
        {
            let mut lane = service.lane.lock().unwrap();
            lane.tick_gate = Some(held.0.clone());
            lane.tick_blocked = false;
        }
        service
            .begin_smb_batch(
                3,
                server.clone(),
                vec![a.clone(), b.clone()],
                false,
                Default::default(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let lane = service.lane.lock().unwrap();
            if lane.tick_blocked {
                assert_eq!(
                    lane.writer_epoch,
                    Some(3),
                    "gate belongs to a previous acquisition"
                );
                assert_eq!(
                    lane.acquisition_root.as_ref(),
                    g0.shares[0].native_root.as_ref(),
                    "gate must hold alpha, not another share"
                );
                break;
            }
            drop(lane);
            assert!(
                Instant::now() < deadline,
                "native refresh did not reach its acquisition quantum"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        service.disconnect_sources(std::slice::from_ref(&a));
        service.reconnect_sources(std::slice::from_ref(&a));
        service.lane.lock().unwrap().tick_gate = None;
        drop(held);
        let refresh = wait(&service, |snapshot| !snapshot.running);
        assert_eq!(
            refresh.shares[0].phase,
            SharePhase::Cancelled,
            "terminal revoked scope: {refresh:?}"
        );
        assert_eq!(refresh.shares[1].phase, SharePhase::Complete);
        assert_eq!(refresh.shares[1].logical, Some(b_measure));
        let kept = service.retained_smb_shares();
        assert_eq!(
            kept.iter().find(|share| share.path == a).unwrap().logical,
            Some(a_measure)
        );
        assert!(service.open_smb_path(4, &a));
        assert_eq!(
            page(&service, 4).root_measure.unwrap().logical,
            Some(a_measure)
        );
        assert!(service.lane.lock().unwrap().start.is_none());
        // An unmounted logical share is an isolated failure, not a filesystem
        // interpretation of its URI or a second inventory of the whole server.
        let target = sortsize_core::volumes::smb::parse_target(server.to_str().unwrap()).unwrap();
        let missing = target
            .share_path(std::ffi::OsStr::new(
                "bunny-unmounted-qualification-missing",
            ))
            .unwrap();
        service
            .begin_smb_batch(5, server, vec![missing, b], false, Default::default())
            .unwrap();
        let mixed = wait(&service, |snapshot| !snapshot.running);
        assert_eq!(mixed.shares[0].phase, SharePhase::Failed);
        assert_eq!(mixed.shares[1].phase, SharePhase::Complete);
        assert_eq!(mixed.shares[1].logical, Some(b_measure));
        assert_eq!(mixed.totals().coverage, ScopeCoverage::Partial);
        service.stop_for_test();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_server_scope_rejects_share_subdirectories() {
        assert!(
            Request::new(
                1,
                "smb://nas.invalid/".into(),
                vec!["smb://nas.invalid/TV/child".into()],
                false,
                Default::default()
            )
            .is_err()
        );
    }
    #[test]
    fn native_mount_is_frozen_and_logical_revocation_cancels_queued_native_acquisition() {
        let mut batch = Batch::new(
            Request::confirmed(
                1,
                "smb://nas/".into(),
                vec!["smb://nas/TV".into(), "smb://nas/Docs".into()],
                false,
                Default::default(),
            )
            .unwrap(),
        );
        batch.snapshot.shares[0].native_root = Some("/run/user/1000/gvfs/mount-a".into());
        batch.snapshot.shares[1].native_root = None;
        assert_eq!(
            batch.next().unwrap().root,
            Path::new("/run/user/1000/gvfs/mount-a")
        );
        batch.finish(None);
        batch.disable_sources(&["smb://nas/".into()]);
        assert!(batch.next().is_none());
        assert_eq!(batch.snapshot.shares[0].path, Path::new("smb://nas/TV"));
        assert_eq!(batch.snapshot.shares[1].phase, SharePhase::Cancelled);
    }
    #[test]
    fn aggregate_unknown_and_overlap_semantics() {
        let mut batch = batch();
        assert_eq!(batch.snapshot.totals().coverage, ScopeCoverage::Unknown);
        assert_eq!(batch.snapshot.totals().logical, None);
        for _ in 0..2 {
            batch.next().unwrap();
            batch.observe(
                Some(sortsize_store::inventory::folder_totals::FolderMeasure {
                    logical: Some(42),
                    files: 2,
                    directories: 1,
                    ..Default::default()
                }),
                ScopeCoverage::Complete,
                false,
            );
            batch.finish(None);
        }
        assert!(batch.next().is_none());
        let totals = batch.snapshot.totals();
        assert_eq!(totals.logical, Some(84));
        assert_eq!(totals.files, Some(4));
        assert_eq!(totals.directories, Some(2));
        assert_eq!(totals.coverage, ScopeCoverage::Complete);
        batch.snapshot.discovery_uncertain = true;
        assert_eq!(batch.snapshot.totals().coverage, ScopeCoverage::Partial);
        batch.snapshot.shares[0].logical = Some(u64::MAX);
        assert_eq!(batch.snapshot.totals().logical, None);
        assert_eq!(batch.snapshot.totals().coverage, ScopeCoverage::Partial);
    }
    #[test]
    fn fixed_scope_failure_and_cancellation() {
        let mut batch = batch();
        let first = batch.next().unwrap();
        assert_eq!(first.root, PathBuf::from("share-a"));
        assert!(batch.next().is_none());
        batch.finish(Some("unavailable".into()));
        assert_eq!(batch.snapshot.shares[0].phase, SharePhase::Failed);
        assert_eq!(batch.next().unwrap().root, PathBuf::from("share-b"));
        batch.cancel();
        batch.finish(None);
        assert!(batch.next().is_none());
        assert!(!batch.snapshot.running);
        assert_eq!(batch.snapshot.completed(), 0);
        assert_eq!(batch.snapshot.shares.len(), 2);
        assert_eq!(batch.snapshot.shares[1].phase, SharePhase::Cancelled);
    }

    #[test]
    fn current_totals_exclude_larger_dated_g0_until_new_observations() {
        let mut batch = batch();
        for share in &mut batch.snapshot.shares {
            share.logical = Some(100);
            share.files = Some(4);
            share.directories = Some(2);
            share.coverage = ScopeCoverage::Complete;
            share.measured = Some(std::time::UNIX_EPOCH);
        }
        batch.next().unwrap();
        assert_eq!(batch.snapshot.totals().logical, None);
        assert_eq!(batch.snapshot.totals().coverage, ScopeCoverage::Unknown);
        batch.observe(
            Some(sortsize_store::inventory::folder_totals::FolderMeasure {
                logical: Some(10),
                files: 1,
                directories: 0,
                ..Default::default()
            }),
            ScopeCoverage::Complete,
            false,
        );
        assert_eq!(batch.snapshot.totals().logical, Some(10));
        assert_eq!(batch.snapshot.totals().files, Some(1));
        assert!(batch.snapshot.shares[0].current_observation);
        assert!(batch.snapshot.shares[0].measured.is_none());
        batch.finish(None);
        batch.next().unwrap();
        batch.finish(Some("offline before first observation".into()));
        assert!(!batch.snapshot.shares[1].current_observation);
        assert_eq!(batch.snapshot.shares[1].logical, Some(100));
        assert_eq!(batch.snapshot.shares[1].coverage, ScopeCoverage::Unknown);
        assert_eq!(batch.snapshot.totals().logical, Some(10));
        assert_eq!(batch.snapshot.totals().coverage, ScopeCoverage::Partial);
    }

    #[test]
    fn composite_service_isolates_failures_and_reopens_each_inventory_without_scan() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(first.join("nested")).unwrap();
        std::fs::create_dir_all(second.join("empty")).unwrap();
        std::fs::write(first.join("root-file"), [1; 3]).unwrap();
        std::fs::write(first.join("nested/child"), [2; 5]).unwrap();
        std::fs::write(second.join("other"), [3; 11]).unwrap();
        let service = Service::start().unwrap();
        injected(
            &service,
            1,
            vec![first.clone(), temp.path().join("missing"), second.clone()],
        );
        let snapshot = wait(&service, |snapshot| !snapshot.running);
        assert_eq!(snapshot.completed(), 2);
        assert_eq!(snapshot.shares[1].phase, SharePhase::Failed);
        assert!(snapshot.shares[1].error.is_some());
        assert_eq!(snapshot.shares[0].logical, Some(8));
        assert_eq!(snapshot.shares[2].logical, Some(11));
        assert_eq!(
            snapshot.totals(),
            Totals {
                logical: Some(19),
                files: Some(3),
                directories: Some(2),
                coverage: ScopeCoverage::Partial,
            }
        );
        assert!(!service.open_smb_share(2, 1));
        assert!(service.open_smb_share(2, 0));
        let first_page = page(&service, 2);
        assert_eq!(first_page.root_measure.unwrap().logical, Some(8));
        assert_eq!(first_page.details.len(), 2);
        drop(first_page);
        assert!(service.open_smb_share(3, 2));
        let second_page = page(&service, 3);
        assert_eq!(second_page.root_measure.unwrap().logical, Some(11));
        assert_eq!(second_page.details.len(), 2);
        assert_eq!(service.smb_snapshot(), Some(snapshot));
        assert!(!service.lane.lock().unwrap().writer_running);
        service.stop_for_test();
    }

    #[test]
    fn composite_failed_refresh_preserves_certified_g0_without_network_retry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("share");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), [1; 17]).unwrap();
        let service = Service::start().unwrap();
        injected(&service, 1, vec![root.clone()]);
        assert_eq!(
            wait(&service, |s| !s.running).totals().coverage,
            ScopeCoverage::Complete
        );
        assert!(service.open_smb_share(2, 0));
        let old = page(&service, 2);
        let generation = old.frame.stamp;
        drop(old);
        std::fs::rename(&root, temp.path().join("offline")).unwrap();
        injected(&service, 3, vec![root]);
        let failed = wait(&service, |s| s.epoch == 3 && !s.running);
        assert_eq!(failed.shares[0].phase, SharePhase::Failed);
        assert_eq!(failed.shares[0].logical, Some(17));
        assert!(!failed.shares[0].current_observation);
        assert_eq!(failed.totals().logical, None);
        assert_eq!(failed.totals().coverage, ScopeCoverage::Unknown);
        assert!(service.open_smb_share(4, 0));
        let preserved = page(&service, 4);
        assert_eq!(preserved.root_measure.unwrap().logical, Some(17));
        assert_eq!(
            preserved.frame.stamp.context.generation,
            generation.context.generation
        );
        assert_eq!(service.smb_snapshot(), Some(failed));
        service.stop_for_test();
    }

    #[test]
    fn composite_shrinking_share_and_initial_failure_do_not_reuse_old_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("share");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("file");
        std::fs::write(&file, [1; 100]).unwrap();
        let service = Service::start().unwrap();
        injected(&service, 1, vec![root.clone()]);
        assert_eq!(wait(&service, |s| !s.running).totals().logical, Some(100));
        std::fs::write(file, [2; 10]).unwrap();
        injected(&service, 2, vec![root.clone()]);
        let reduced = wait(&service, |s| s.epoch == 2 && !s.running);
        assert_eq!(reduced.totals().logical, Some(10));
        assert_eq!(reduced.totals().coverage, ScopeCoverage::Complete);
        assert!(reduced.shares[0].current_observation);
        std::fs::rename(&root, temp.path().join("offline")).unwrap();
        injected(&service, 3, vec![root.clone()]);
        let unavailable = wait(&service, |s| s.epoch == 3 && !s.running);
        assert_eq!(unavailable.shares[0].logical, Some(10));
        assert!(!unavailable.shares[0].current_observation);
        assert_eq!(unavailable.totals().logical, None);
        assert_eq!(unavailable.totals().coverage, ScopeCoverage::Unknown);
        assert!(service.open_smb_path(4, &root));
        assert_eq!(page(&service, 4).root_measure.unwrap().logical, Some(10));
        service.stop_for_test();
    }

    #[test]
    fn narrower_failed_scope_keeps_other_shares_dated_g0_without_adding_them_to_totals() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("a"), [1; 31]).unwrap();
        std::fs::write(second.join("b"), [2; 23]).unwrap();
        let service = Service::start().unwrap();
        injected(&service, 1, vec![first.clone(), second.clone()]);
        let full = wait(&service, |s| !s.running);
        assert_eq!(full.totals().logical, Some(54));
        assert_eq!(full.totals().coverage, ScopeCoverage::Complete);
        std::fs::rename(&first, temp.path().join("offline")).unwrap();
        injected(&service, 2, vec![first]);
        let narrowed = wait(&service, |s| s.epoch == 2 && !s.running);
        assert_eq!(narrowed.shares.len(), 1);
        assert_eq!(narrowed.shares[0].phase, SharePhase::Failed);
        assert_eq!(narrowed.totals().logical, None);
        assert_eq!(narrowed.totals().coverage, ScopeCoverage::Unknown);
        let retained = service.retained_smb_shares();
        assert_eq!(retained.len(), 2);
        let old = retained.iter().find(|share| share.path == second).unwrap();
        assert_eq!(old.logical, Some(23));
        assert!(old.measured.is_some());
        assert!(service.open_smb_path(3, &second));
        let page = page(&service, 3);
        assert_eq!(page.root_measure.unwrap().logical, Some(23));
        assert_eq!(service.smb_snapshot(), Some(narrowed));
        service.stop_for_test();
    }

    #[test]
    fn composite_cancel_freezes_scope_and_never_starts_remaining_roots() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for i in 0..20 {
            std::fs::write(first.join(i.to_string()), [1; 3]).unwrap();
        }
        let service = Service::start().unwrap();
        service.lane.lock().unwrap().hold_generation = Some(GenerationId(1));
        injected(&service, 1, vec![first, second]);
        let deadline = Instant::now() + Duration::from_secs(30);
        while service.lane.lock().unwrap().writer_entries <= 1 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        service.cancel_smb_batch();
        service.lane.lock().unwrap().hold_generation = None;
        let snapshot = wait(&service, |s| !s.running);
        assert!(snapshot.cancelled);
        assert_eq!(snapshot.shares.len(), 2);
        assert_eq!(snapshot.shares[1].phase, SharePhase::Cancelled);
        assert_eq!(snapshot.shares[1].logical, None);
        assert_eq!(snapshot.shares[1].files, None);
        assert_ne!(snapshot.totals().coverage, ScopeCoverage::Complete);
        service.stop_for_test();
    }
}

#[cfg(test)]
#[path = "disconnect_tests.rs"]
mod disconnect_tests;
