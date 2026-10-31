//! Per-frame progress from long provider calls, so a host watchdog can tell slow from stuck.
//!
//! The watch is thread-local, so a provider needs no handle to report it.

use std::cell::RefCell;

thread_local! {
    static WATCH: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Calls `progressed` each time a provider call on this thread gets further, until the watch drops.
pub fn watch_progress(progressed: impl Fn() + 'static) -> ProgressWatch {
    let previous = WATCH.with(|watch| watch.replace(Some(Box::new(progressed))));
    ProgressWatch {
        previous,
        thread: std::marker::PhantomData,
    }
}

/// Ends a [`watch_progress`] watch, restoring the one before it.
pub struct ProgressWatch {
    previous: Option<Box<dyn Fn()>>,
    /// Makes the watch `!Send`, as it must drop on the thread that set it.
    thread: std::marker::PhantomData<*const ()>,
}

impl Drop for ProgressWatch {
    fn drop(&mut self) {
        let previous = self.previous.take();
        WATCH.with(|watch| {
            watch.replace(previous);
        });
    }
}

#[cfg(feature = "animated-image")]
pub(crate) fn progressed() {
    WATCH.with(|watch| {
        if let Some(progressed) = watch.borrow().as_ref() {
            progressed();
        }
    });
}
