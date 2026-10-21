//! Directory-level parallel acquisition. Only the calling thread aggregates.
//! Never hold the scheduler lock during filesystem calls or user callbacks.
use crate::local::{CollectFields, LocalSource, Observation, PendingDirectory, open_directory};
use crate::{CancellationToken, ScanFailure, ScanIssue, ScanOptions, TraversalStats};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

type Event = (PathBuf, Observation);

#[derive(Default)]
struct State {
    pending: Vec<PendingDirectory>,
    pending_bytes: usize,
    batches: VecDeque<Vec<Event>>,
    buffered_count: usize,
    buffered_bytes: usize,
    active: usize,
    open: usize,
    stopped: bool,
    failure: Option<ScanFailure>,
    stats: TraversalStats,
}

struct Shared {
    source: LocalSource,
    options: ScanOptions,
    cancellation: CancellationToken,
    stopping: AtomicBool,
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        // CancellationToken is shared with callers and has no queue-specific CV.
        self.changed
            .wait_timeout(state, Duration::from_millis(10))
            .unwrap_or_else(|e| e.into_inner())
            .0
    }
    fn stopped(&self, state: &State) -> bool {
        state.stopped || self.cancellation.is_cancelled()
    }
    fn fail(&self, failure: ScanFailure) {
        let mut state = self.lock();
        if state.failure.is_none() {
            state.failure = Some(failure);
        }
        state.stopped = true;
        self.stopping.store(true, Ordering::Relaxed);
        self.changed.notify_all();
    }
    fn defer(&self, state: &mut State, path: &Path, depth: usize) -> bool {
        let bytes = path.as_os_str().as_encoded_bytes().len();
        let limits = self.options.traversal_limits;
        if state.pending.len() >= limits.max_pending_directories
            || bytes > limits.max_pending_path_bytes - state.pending_bytes
        {
            return false;
        }
        state.pending.push(PendingDirectory {
            path: path.to_owned().into_boxed_path(),
            depth,
        });
        state.pending_bytes += bytes;
        state.stats.peak_pending_directories = state
            .stats
            .peak_pending_directories
            .max(state.pending.len());
        state.stats.peak_pending_path_bytes =
            state.stats.peak_pending_path_bytes.max(state.pending_bytes);
        true
    }
    fn publish(&self, mut batch: Vec<Event>, depth: usize) -> bool {
        let bytes = batch
            .iter()
            .map(|(p, _)| p.as_os_str().as_encoded_bytes().len())
            .sum::<usize>();
        let limits = self.options.concurrency;
        let mut state = self.lock();
        while !self.stopped(&state)
            && (batch.len() > limits.max_buffered_observations - state.buffered_count
                || bytes > limits.max_buffered_path_bytes - state.buffered_bytes)
        {
            state = self.wait(state);
        }
        if self.stopped(&state) {
            return false;
        }
        for (path, observation) in &mut batch {
            if let Observation::Directory {
                depth_limited: false,
                resource_limited,
                ..
            } = observation
            {
                *resource_limited = !self.defer(&mut state, path, depth);
            }
        }
        state.buffered_count += batch.len();
        state.buffered_bytes += bytes;
        state.stats.peak_buffered_observations = state
            .stats
            .peak_buffered_observations
            .max(state.buffered_count);
        state.stats.peak_buffered_path_bytes = state
            .stats
            .peak_buffered_path_bytes
            .max(state.buffered_bytes);
        // Both operations are under the same lock: a child job cannot run until
        // its parent's observation is in the FIFO, ahead of all descendants.
        state.batches.push_back(batch);
        self.changed.notify_all();
        true
    }
    fn job(&self) -> Option<PendingDirectory> {
        let mut state = self.lock();
        loop {
            if self.stopped(&state) {
                return None;
            }
            if let Some(directory) = state.pending.pop() {
                state.pending_bytes -= directory.path.as_os_str().as_encoded_bytes().len();
                state.active += 1;
                return Some(directory);
            }
            if state.active == 0 {
                return None;
            }
            state = self.wait(state);
        }
    }
    fn run(&self) {
        while let Some(directory) = self.job() {
            match open_directory(&directory.path) {
                Ok(listing) => {
                    {
                        let mut state = self.lock();
                        state.open += 1;
                        state.stats.peak_open_directories =
                            state.stats.peak_open_directories.max(state.open);
                    }
                    self.list(directory, listing);
                    // list owns and closes ReadDir before this decrement.
                    self.lock().open -= 1;
                }
                Err(issue) => {
                    self.publish(
                        vec![(directory.path.into_path_buf(), Observation::Error(issue))],
                        directory.depth,
                    );
                }
            }
            let mut state = self.lock();
            state.active -= 1;
            self.changed.notify_all();
        }
    }
    fn list(&self, directory: PendingDirectory, listing: std::fs::ReadDir) {
        let limits = self.options.concurrency;
        let count_limit = limits.max_buffered_observations.min(32);
        let mut batch = Vec::new();
        let mut bytes = 0;
        for entry in listing {
            if self.cancellation.is_cancelled() || self.stopping.load(Ordering::Relaxed) {
                return;
            }
            let (path, observation) = match entry {
                Ok(entry) => {
                    let path = entry.path();
                    let observation = match entry.file_type() {
                        Ok(kind) => self.source.observe(&path, directory.depth + 1, kind),
                        Err(error) => Observation::Error(ScanIssue::io(&path, "metadata", &error)),
                    };
                    (path, observation)
                }
                Err(error) => (
                    directory.path.to_path_buf(),
                    Observation::Error(ScanIssue::io(&directory.path, "list", &error)),
                ),
            };
            let path_bytes = path.as_os_str().as_encoded_bytes().len();
            if path_bytes > limits.max_buffered_path_bytes {
                self.fail(ScanFailure::ObservationPathLimit {
                    required: path_bytes,
                    limit: limits.max_buffered_path_bytes,
                });
                return;
            }
            if !batch.is_empty()
                && (batch.len() == count_limit
                    || path_bytes > limits.max_buffered_path_bytes - bytes
                    || bytes >= 64 * 1024)
            {
                if !self.publish(std::mem::take(&mut batch), directory.depth + 1) {
                    return;
                }
                bytes = 0;
            }
            bytes += path_bytes;
            batch.push((path, observation));
        }
        if !batch.is_empty() {
            self.publish(batch, directory.depth + 1);
        }
    }
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    current: std::vec::IntoIter<Event>,
}

impl Pool {
    pub(crate) fn new(
        root: &Path,
        options: &ScanOptions,
        cancellation: &CancellationToken,
        fields: CollectFields,
    ) -> Result<Self, ScanFailure> {
        let source = LocalSource::new(root, options, fields)?;
        if source.root().as_os_str().as_encoded_bytes().len()
            > options.concurrency.max_buffered_path_bytes
        {
            return Err(ScanFailure::ObservationPathLimit {
                required: source.root().as_os_str().as_encoded_bytes().len(),
                limit: options.concurrency.max_buffered_path_bytes,
            });
        }
        let shared = Arc::new(Shared {
            source,
            options: options.clone(),
            cancellation: cancellation.clone(),
            stopping: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let root = shared.source.root().to_owned();
        let observation = shared.source.root_observation();
        shared.publish(vec![(root, observation)], 0);
        let mut pool = Self {
            shared,
            threads: Vec::new(),
            current: Vec::new().into_iter(),
        };
        let workers = options
            .concurrency
            .workers
            .min(options.max_open_directories);
        for index in 0..workers {
            let shared = pool.shared.clone();
            let thread = std::thread::Builder::new()
                .name(format!("sortsize-acquire-{index}"))
                .spawn(move || {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared.run()))
                        .is_err()
                    {
                        shared.fail(ScanFailure::WorkerPanic);
                    }
                })
                .map_err(ScanFailure::WorkerIo)?;
            pool.threads.push(thread);
        }
        pool.shared.lock().stats.worker_threads = workers;
        Ok(pool)
    }
    pub(crate) fn root(&self) -> &Path {
        self.shared.source.root()
    }
    pub(crate) fn effective_exclusions(&self) -> &[String] {
        self.shared.source.effective_exclusions()
    }
    pub(crate) fn finish(&mut self) -> Result<TraversalStats, ScanFailure> {
        self.stop_and_join();
        let mut state = self.shared.lock();
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        Ok(state.stats)
    }
    fn stop_and_join(&mut self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        self.shared.lock().stopped = true;
        self.shared.changed.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
impl Iterator for Pool {
    type Item = Event;
    fn next(&mut self) -> Option<Event> {
        if self.shared.cancellation.is_cancelled() {
            return None;
        }
        if let Some(event) = self.current.next() {
            return Some(event);
        }
        let mut state = self.shared.lock();
        loop {
            if self.shared.stopped(&state) {
                return None;
            }
            if let Some(batch) = state.batches.pop_front() {
                state.buffered_count -= batch.len();
                state.buffered_bytes -= batch
                    .iter()
                    .map(|(p, _)| p.as_os_str().as_encoded_bytes().len())
                    .sum::<usize>();
                self.current = batch.into_iter();
                self.shared.changed.notify_all();
                return self.current.next();
            }
            if state.active == 0 && state.pending.is_empty() {
                return None;
            }
            state = self.shared.wait(state);
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}
