//! A heartbeat from the long loops a request's own thread runs, for the host's request watch.

thread_local! {
    static REQUEST_WATCH: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Calls `beat` on this thread each time a loop of this crate that runs on a request's thread
/// takes a step (an audio decoder block, a seek), until the guard drops.
#[must_use = "the watch ends when the guard drops"]
pub fn watch_request(beat: impl Fn() + 'static) -> RequestWatchGuard {
    let previous = REQUEST_WATCH.with(|watch| watch.replace(Some(Box::new(beat))));
    RequestWatchGuard {
        previous,
        thread: std::marker::PhantomData,
    }
}

/// Ends a [`watch_request`] watch, restoring the one before it.
pub struct RequestWatchGuard {
    previous: Option<Box<dyn Fn()>>,
    thread: std::marker::PhantomData<*const ()>,
}

impl Drop for RequestWatchGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        REQUEST_WATCH.with(|watch| {
            watch.replace(previous);
        });
    }
}

/// Tells this thread's [`watch_request`] watch, if any, that a step starts.
pub fn beat_request() {
    REQUEST_WATCH.with(|watch| {
        if let Some(beat) = watch.borrow().as_ref() {
            beat();
        }
    });
}
