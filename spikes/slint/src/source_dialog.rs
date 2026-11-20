//! One explicitly requested native source operation at a time. Closing/editing
//! invalidates publication; a blocked OS call never creates a second worker.
use std::{
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
};

pub(crate) struct Outcome {
    pub state: i32,
    pub shares: Option<Vec<PathBuf>>,
    pub limited: bool,
}

/// The first displayed row is a localized prompt, never a source. Keeping it
/// outside the native share list avoids Slint's model update silently showing
/// the first share without invoking the user-selection callback.
pub(crate) fn share_index(display_index: i32) -> Option<usize> {
    usize::try_from(display_index).ok()?.checked_sub(1)
}

impl Outcome {
    pub fn state(state: i32) -> Self {
        Self {
            state,
            shares: None,
            limited: false,
        }
    }
}
struct Job {
    revision: u64,
    receiver: Receiver<Outcome>,
}
#[derive(Default)]
pub(crate) struct Operations {
    revision: u64,
    active: Option<Job>,
    reserved: bool,
}
impl Operations {
    pub fn busy(&self) -> bool {
        self.active.is_some() || self.reserved
    }
    /// Share the native source lane with the reconnect worker without tying it
    /// to the independent acquisition busy state.
    pub fn reserve(&mut self) -> bool {
        if self.busy() {
            return false;
        }
        self.reserved = true;
        true
    }
    pub fn release(&mut self) {
        self.reserved = false;
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn start(&mut self, work: impl FnOnce() -> Outcome + Send + 'static) -> io::Result<bool> {
        if self.busy() {
            return Ok(false);
        }
        self.invalidate();
        let revision = self.revision;
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("bunny-source-operation".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                    .unwrap_or_else(|_| Outcome::state(16));
                let _ = sender.send(outcome);
            })?;
        self.active = Some(Job { revision, receiver });
        Ok(true)
    }
    /// Outer Some means the worker finished; inner None is an obsolete result.
    pub fn poll(&mut self) -> Option<Option<Outcome>> {
        let active = self.active.as_ref()?;
        let outcome = match active.receiver.try_recv() {
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Outcome::state(16),
            Ok(outcome) => outcome,
        };
        let current = active.revision == self.revision;
        self.active = None;
        Some(current.then_some(outcome))
    }
}
pub(crate) fn access_state(error: &io::Error) -> i32 {
    #[cfg(windows)]
    match error.raw_os_error() {
        Some(53 | 64 | 1231 | 1232) => return 8,
        Some(67) => return 12,
        // A check did not ask for credentials. Offer native sign-in instead
        // of claiming that credentials entered in this application failed.
        Some(86 | 1326 | 1909 | 1244 | 1312) => return 10,
        Some(1219) => return 15,
        Some(1223) => return 14,
        _ => {}
    }
    if error.kind() == io::ErrorKind::PermissionDenied {
        3
    } else {
        2
    }
}

#[cfg(windows)]
pub(crate) fn owner_handle(window: &slint::Window) -> usize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let owner = window.window_handle();
    match owner.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(native)) => native.hwnd.get() as usize,
        _ => 0,
    }
}

#[cfg(any(target_os = "linux", windows))]
pub(crate) fn enumeration_state(status: sortsize_core::volumes::smb::Status) -> i32 {
    if status == sortsize_core::volumes::smb::Status::AccessDenied {
        19
    } else {
        native_state(status)
    }
}

#[cfg(any(target_os = "linux", windows))]
pub(crate) fn native_state(status: sortsize_core::volumes::smb::Status) -> i32 {
    use sortsize_core::volumes::smb::Status;
    match status {
        Status::SharesFound => 0,
        Status::ServerInaccessible => 8,
        Status::NoShares => 9,
        Status::AuthenticationRequired => 10,
        Status::CredentialsRejected => 11,
        Status::ShareMissing => 12,
        Status::AccessDenied => 3,
        Status::Connected => 13,
        Status::Cancelled => 14,
        Status::SessionConflict => 15,
        Status::InvalidInput => 4,
        Status::Failed => 16,
        #[cfg(target_os = "linux")]
        Status::DesktopConnectionPending => 20,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    #[test]
    fn displayed_share_prompt_never_resolves_to_a_native_share() {
        assert_eq!(share_index(-1), None);
        assert_eq!(share_index(0), None);
        assert_eq!(share_index(1), Some(0));
        assert_eq!(share_index(3), Some(2));
    }
    #[cfg(windows)]
    #[test]
    fn native_errors_keep_listing_denial_distinct_from_failed_credentials() {
        use sortsize_core::volumes::smb::Status;
        assert_eq!(enumeration_state(Status::AccessDenied), 19);
        assert_eq!(native_state(Status::AccessDenied), 3);
        assert_eq!(native_state(Status::CredentialsRejected), 11);
        assert_eq!(access_state(&io::Error::from_raw_os_error(1326)), 10);
        assert_eq!(native_state(Status::SessionConflict), 15);
    }
    #[test]
    fn reconnect_reservation_blocks_source_work_in_both_directions() {
        let mut operations = Operations::default();
        assert!(operations.reserve());
        assert!(!operations.reserve());
        assert!(
            !operations
                .start(|| panic!("reserved native work must not start"))
                .unwrap()
        );
        operations.invalidate();
        assert!(operations.busy());
        operations.release();
        assert!(!operations.busy());
        let (sender, receiver) = mpsc::channel();
        assert!(
            operations
                .start(move || {
                    receiver.recv().unwrap();
                    Outcome::state(13)
                })
                .unwrap()
        );
        assert!(!operations.reserve());
        sender.send(()).unwrap();
    }
    #[test]
    fn native_work_is_single_flight_and_late_results_are_discarded_without_joining() {
        let (sender, receiver) = mpsc::channel();
        let mut operations = Operations::default();
        assert!(
            operations
                .start(move || {
                    receiver.recv().unwrap();
                    Outcome::state(13)
                })
                .unwrap()
        );
        let entered = Arc::new(AtomicBool::new(false));
        let flag = entered.clone();
        assert!(
            !operations
                .start(move || {
                    flag.store(true, Ordering::SeqCst);
                    Outcome::state(1)
                })
                .unwrap()
        );
        operations.invalidate();
        assert!(!entered.load(Ordering::SeqCst));
        sender.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = operations.poll() {
                assert!(result.is_none());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!operations.busy());
        let (sender, receiver) = mpsc::channel();
        operations
            .start(move || {
                let _ = receiver.recv();
                Outcome::state(13)
            })
            .unwrap();
        let start = Instant::now();
        drop(operations);
        assert!(start.elapsed() < Duration::from_millis(100));
        sender.send(()).unwrap();
    }
}
