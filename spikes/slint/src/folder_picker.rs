use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// One outstanding OS dialog. Dropping it never joins or blocks the UI thread.
pub(crate) struct FolderPicker<T = PathBuf> {
    receiver: Receiver<Option<T>>,
}

impl<T: Send + 'static> FolderPicker<T> {
    pub fn start(pick: impl FnOnce() -> Option<T> + Send + 'static) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("sortsize-folder-picker".into())
            .spawn(move || {
                let result = pick();
                let _ = sender.send(result);
            })?;
        Ok(Self { receiver })
    }

    pub fn poll(&self) -> Option<Result<Option<T>, TryRecvError>> {
        match self.receiver.try_recv() {
            Err(TryRecvError::Empty) => None,
            result => Some(result),
        }
    }
}
