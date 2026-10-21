//! Thread-local, one-shot IO faults; absent from production builds.
use std::cell::RefCell;
use std::io;

thread_local! {
    static FAULT: RefCell<Option<(&'static str, usize)>> = const { RefCell::new(None) };
}

pub(super) fn check(point: &'static str) -> io::Result<()> {
    FAULT.with_borrow_mut(|fault| {
        if let Some((selected, remaining)) = fault.as_mut()
            && *selected == point
        {
            if *remaining == 0 {
                *fault = None;
                return Err(io::Error::new(io::ErrorKind::StorageFull, point));
            }
            *remaining -= 1;
        }
        Ok(())
    })
}

pub(super) fn during<T>(point: &'static str, after: usize, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAULT.with_borrow_mut(|fault| *fault = None);
        }
    }
    FAULT.with_borrow_mut(|fault| *fault = Some((point, after)));
    let _reset = Reset;
    run()
}
