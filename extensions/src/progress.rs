//! Progress a provider reports while one call works through many frames.
//!
//! A host that watches its threads for calls that never return (a decode
//! shard's heartbeat) cannot tell a long call that keeps going from one that
//! is stuck. An animated source's open decodes every frame once, and a
//! request for an earlier frame decodes forward from the file's start: each
//! frame done is progress. The watch is the calling thread's own, as the
//! native decoders' is, so a provider needs no handle to report it.

use std::cell::RefCell;

thread_local! {
    static WATCH: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Calls `progressed` on this thread each time a provider call it makes
/// gets further, until the returned watch drops.
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
    /// Makes the watch `!Send`: it must drop on the thread that set it.
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

/// The call in progress got further: a frame was decoded.
#[cfg(feature = "animated-image")]
pub(crate) fn progressed() {
    WATCH.with(|watch| {
        if let Some(progressed) = watch.borrow().as_ref() {
            progressed();
        }
    });
}
