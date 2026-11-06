//! A heartbeat before each encoder and muxer call, for an outside stall watcher.

thread_local! {
    /// What this thread calls before each encoder and muxer call ([`watch_encode_calls`]).
    static ENCODE_CALL_WATCH: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Calls `beat` on this thread before every encoder and muxer call a sink
/// makes, until the guard drops.
///
/// Some calls wait forever (`h264_vulkan` waits on fences with `UINT64_MAX`; `write(2)` to a stuck
/// hard-mounted share retries forever) and cannot be interrupted, so the driving thread reports
/// where it is and its owner decides when a call is stuck. The audio encoder's thread is covered by
/// the call that waits on it.
#[must_use = "the watch ends when the guard drops"]
pub fn watch_encode_calls(beat: impl Fn() + 'static) -> EncodeCallWatch {
    let previous = ENCODE_CALL_WATCH.with(|watch| watch.replace(Some(Box::new(beat))));
    EncodeCallWatch {
        previous,
        thread: std::marker::PhantomData,
    }
}

/// Ends a [`watch_encode_calls`] watch, restoring the one before it.
pub struct EncodeCallWatch {
    previous: Option<Box<dyn Fn()>>,
    thread: std::marker::PhantomData<*const ()>,
}

impl Drop for EncodeCallWatch {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ENCODE_CALL_WATCH.with(|watch| {
            watch.replace(previous);
        });
    }
}

/// Tells this thread's [`watch_encode_calls`] watch, if any, that an encoder or muxer call
/// starts.
pub fn beat_encode_call() {
    ENCODE_CALL_WATCH.with(|watch| {
        if let Some(beat) = watch.borrow().as_ref() {
            beat();
        }
    });
}
