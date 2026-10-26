//! One persistent scan/listing worker and one replaceable pending request. Rapid
//! navigation never creates an unbounded number of blocked filesystem threads.
use crate::worker::{Job, Mailbox, Task, run_job};
use sortsize_core::CancellationToken;
use std::sync::{Arc, Condvar, Mutex};

pub(crate) type Preflight = Box<dyn FnOnce() -> bool + Send>;
type Request = (
    Job,
    CancellationToken,
    Arc<Mutex<Mailbox>>,
    Option<Preflight>,
);
#[derive(Default)]
struct Queue {
    pending: Option<Request>,
    active: Option<CancellationToken>,
    closed: bool,
}
pub(crate) struct DirectoryWorker(Arc<(Mutex<Queue>, Condvar)>);
impl DirectoryWorker {
    pub(crate) fn start() -> std::io::Result<Self> {
        Self::named("sortsize-directory")
    }
    pub(crate) fn named(name: &str) -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker = shared.clone();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                loop {
                    let (job, token, mailbox, preflight) = {
                        let (lock, wake) = &*worker;
                        let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while queue.pending.is_none() && !queue.closed {
                            queue = wake.wait(queue).unwrap_or_else(|e| e.into_inner());
                        }
                        if queue.closed {
                            break;
                        }
                        let request = queue.pending.take().unwrap();
                        queue.active = Some(request.1.clone());
                        request
                    };
                    // Even a request cancelled before acquisition must publish
                    // its terminal state; otherwise the UI would stay busy.
                    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // Do not begin an additional native discovery for a job
                        // already cancelled while waiting for this worker.
                        token.is_cancelled() || preflight.is_none_or(|check| check())
                    }));
                    match checked {
                        Ok(true) => run_job(job, token, mailbox),
                        result => {
                            mailbox.lock().unwrap_or_else(|e| e.into_inner()).completion =
                                Some(Err(if result.is_err() {
                                    crate::worker::Failure::Panicked
                                } else {
                                    crate::worker::Failure::SourceChanged
                                }))
                        }
                    }
                    worker.0.lock().unwrap_or_else(|e| e.into_inner()).active = None;
                }
            })?;
        Ok(Self(shared))
    }
    pub(crate) fn submit(&self, job: Job) -> Task {
        self.submit_checked(job, None)
    }
    pub(crate) fn submit_checked(&self, job: Job, preflight: Option<Preflight>) -> Task {
        let cancellation = CancellationToken::default();
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let mut queue = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &queue.active {
            active.cancel();
        }
        if let Some((_, pending, _, _)) = queue.pending.take() {
            pending.cancel();
        }
        queue.pending = Some((job, cancellation.clone(), mailbox.clone(), preflight));
        self.0.1.notify_one();
        Task {
            cancellation,
            mailbox,
        }
    }
}
impl Drop for DirectoryWorker {
    fn drop(&mut self) {
        let mut queue = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        queue.closed = true;
        if let Some(active) = &queue.active {
            active.cancel();
        }
        if let Some((_, pending, _, _)) = queue.pending.take() {
            pending.cancel();
        }
        self.0.1.notify_one();
        // No join: a native filesystem call may be uninterruptible.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_blocked_preflight_coalesces_replacements_and_worker_recovers_after_panic() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let fixture = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        std::fs::write(root.join("latest-only"), [0; 19]).unwrap();
        let job = || Job {
            root: root.clone(),
            scope: None,
            workers: 1,
            collect_owner: false,
            collect_allocation: false,
            limits: sortsize_core::InventoryLimits::default(),
            child_offset: 0,
        };
        let worker = DirectoryWorker::start().unwrap();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let first = worker.submit_checked(
            job(),
            Some(Box::new(move || {
                entered.send(std::thread::current().id()).unwrap();
                gate.recv_timeout(Duration::from_secs(10)).unwrap();
                panic!("injected preflight panic");
            })),
        );
        let worker_id = started.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut previous: Option<Task> = None;
        for _ in 0..100 {
            let task = worker.submit_checked(
                job(),
                Some(Box::new(|| {
                    panic!("superseded pending preflight must never execute")
                })),
            );
            if let Some(old) = previous.replace(task) {
                assert!(old.cancellation.is_cancelled());
            }
        }
        let (entered, started) = mpsc::channel();
        let latest = worker.submit_checked(
            job(),
            Some(Box::new(move || {
                entered.send(std::thread::current().id()).unwrap();
                true
            })),
        );
        assert!(first.cancellation.is_cancelled());
        assert!(previous.unwrap().cancellation.is_cancelled());
        release.send(()).unwrap();
        assert_eq!(
            started.recv_timeout(Duration::from_secs(10)).unwrap(),
            worker_id
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(completion) = latest.mailbox.lock().unwrap().completion.take() {
                let result = completion.unwrap();
                assert_eq!(result.presentation.report.files, 1);
                assert_eq!(result.presentation.report.logical_bytes, 19);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(matches!(
            first.mailbox.lock().unwrap().completion,
            Some(Err(crate::worker::Failure::Panicked))
        ));
    }

    #[test]
    fn blocked_acquisition_keeps_only_one_latest_pending_generation() {
        // Model an uninterruptible OS call: keep active set and never wake a worker.
        let active = CancellationToken::default();
        let worker = DirectoryWorker(Arc::new((
            Mutex::new(Queue {
                active: Some(active.clone()),
                ..Default::default()
            }),
            Condvar::new(),
        )));
        let mut previous: Option<Task> = None;
        for i in 0..10_000 {
            let next = worker.submit(Job {
                root: format!("volume-{i}").into(),
                scope: None,
                workers: 8,
                collect_owner: false,
                collect_allocation: false,
                limits: sortsize_core::InventoryLimits::default(),
                child_offset: 0,
            });
            if let Some(old) = previous.take() {
                assert!(old.cancellation.is_cancelled());
            }
            previous = Some(next);
        }
        assert!(active.is_cancelled());
        assert_eq!(
            worker.0.0.lock().unwrap().pending.as_ref().unwrap().0.root,
            std::path::PathBuf::from("volume-9999")
        );
        let last = previous.unwrap();
        drop(worker);
        assert!(last.cancellation.is_cancelled());
    }

    #[test]
    fn dropping_pending_task_cancels_without_submitting_a_replacement() {
        // No thread is started: acquisition cannot race the assertion.
        let worker = DirectoryWorker(Arc::new((Mutex::new(Queue::default()), Condvar::new())));
        let task = worker.submit(Job {
            root: "unused".into(),
            scope: Some("unused".into()),
            workers: 1,
            collect_owner: false,
            collect_allocation: false,
            limits: sortsize_core::InventoryLimits::default(),
            child_offset: 0,
        });
        let token = task.cancellation.clone();
        assert!(!token.is_cancelled());
        drop(task);
        assert!(token.is_cancelled());
        let queue = worker.0.0.lock().unwrap();
        assert!(queue.pending.as_ref().unwrap().1.is_cancelled());
    }

    #[test]
    fn pending_navigation_is_coalesced_and_cancelled_requests_complete() {
        let fixture = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        std::fs::write(root.join("one"), [0; 7]).unwrap();
        let worker = DirectoryWorker::start().unwrap();
        let mut last = None;
        for offset in 0..100 {
            last = Some(worker.submit(Job {
                root: root.clone(),
                scope: Some(root.clone()),
                workers: 1,
                collect_owner: false,
                collect_allocation: false,
                limits: sortsize_core::InventoryLimits::default(),
                child_offset: offset,
            }));
        }
        let task = last.unwrap();
        task.cancellation.cancel();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(result) = task.mailbox.lock().unwrap().completion.take() {
                let result = result.unwrap();
                assert!(matches!(
                    result.presentation.report.coverage,
                    sortsize_core::Coverage::Cancelled | sortsize_core::Coverage::Complete
                ));
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod cancelled_pending_tests {
    use super::*;
    #[test]
    fn cancellation_before_wakeup_skips_preflight_and_has_a_terminal_mailbox_result() {
        let fixture = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let worker = DirectoryWorker::start().unwrap();
        let token = CancellationToken::default();
        token.cancel();
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        {
            let mut queue = worker.0.0.lock().unwrap();
            queue.pending = Some((
                Job {
                    root: root.clone(),
                    scope: Some(root),
                    workers: 1,
                    collect_owner: false,
                    collect_allocation: false,
                    limits: sortsize_core::InventoryLimits::default(),
                    child_offset: 0,
                },
                token,
                mailbox.clone(),
                Some(Box::new(|| {
                    panic!("cancelled request must not start native discovery")
                })),
            ));
            worker.0.1.notify_one();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(result) = mailbox.lock().unwrap().completion.take() {
                assert_eq!(
                    result.unwrap().presentation.report.coverage,
                    sortsize_core::Coverage::Cancelled
                );
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod previous_window_tests {
    use super::*;
    #[test]
    fn four_thousand_child_boundaries_keep_exact_totals_and_disjoint_windows() {
        let fixture = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        for count in [4_000, 4_001, 8_000] {
            let start = if count == 4_000 {
                0
            } else if count == 4_001 {
                4_000
            } else {
                4_001
            };
            for i in start..count {
                std::fs::write(root.join(format!("f{i:05}")), [0; 3]).unwrap();
            }
            let mut paths = std::collections::HashSet::new();
            let mut offset = 0;
            while offset < count {
                let mailbox = Arc::new(Mutex::new(Mailbox::default()));
                run_job(
                    Job {
                        root: root.clone(),
                        scope: Some(root.clone()),
                        workers: 1,
                        collect_owner: false,
                        collect_allocation: false,
                        limits: sortsize_core::InventoryLimits {
                            max_entries: 4_001,
                            max_path_bytes: 1024 * 1024,
                        },
                        child_offset: offset,
                    },
                    CancellationToken::default(),
                    mailbox.clone(),
                );
                let result = mailbox.lock().unwrap().completion.take().unwrap().unwrap();
                assert_eq!(result.direct_children_total, count);
                assert_eq!(result.presentation.report.files, count);
                assert_eq!(result.presentation.report.logical_bytes, count * 3);
                assert_eq!(
                    result.previous_child_offset,
                    (offset > 0).then_some(offset.saturating_sub(4_000))
                );
                let retained = result.tree.node_count() - 1;
                assert_eq!(retained as u64, (count - offset).min(4_000));
                let mut tree = result.tree;
                // All direct children fit in two UI pages per inventory window.
                for _ in 0..retained.div_ceil(2_000) {
                    for row in tree.details().0 {
                        assert!(paths.insert(row.path));
                    }
                    tree.change_page(true, true);
                }
                offset += retained as u64;
            }
            assert_eq!(paths.len() as u64, count);
        }
    }

    #[test]
    fn byte_limited_windows_reconstruct_previous_offsets_without_history() {
        let fixture = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        for i in 0..20 {
            std::fs::write(
                root.join(format!("{i:02}-{}", "x".repeat((i * 17) % 75))),
                [0; 3],
            )
            .unwrap();
        }
        let root_bytes = root.as_os_str().as_encoded_bytes().len();
        let mut offset = 0;
        let mut previous = None;
        let mut seen = 0;
        loop {
            let mailbox = Arc::new(Mutex::new(Mailbox::default()));
            run_job(
                Job {
                    root: root.clone(),
                    scope: Some(root.clone()),
                    workers: 1,
                    collect_owner: false,
                    collect_allocation: false,
                    limits: sortsize_core::InventoryLimits {
                        max_entries: 6,
                        max_path_bytes: root_bytes * 3 + 100,
                    },
                    child_offset: offset,
                },
                CancellationToken::default(),
                mailbox.clone(),
            );
            let result = mailbox.lock().unwrap().completion.take().unwrap().unwrap();
            assert_eq!(result.previous_child_offset, previous);
            assert_eq!(result.direct_children_total, 20);
            assert_eq!(result.presentation.report.logical_bytes, 60);
            let retained = result.tree.node_count() - 1;
            assert!(retained > 0 && retained <= 5);
            seen += retained;
            if seen == 20 {
                break;
            }
            previous = Some(offset);
            offset += retained as u64;
        }
        assert_eq!(seen, 20);
    }
}
