//! Waiting for GPU callbacks that may run on another thread.
//!
//! `wgpu` runs a mapping or queue-completion callback on the thread whose
//! poll (or submit) collected it, after that call has done its own work. A
//! thread that waited on the queue can therefore return while a callback
//! for the work it waited on is still on its way on another thread: on
//! macOS the `VideoToolbox` encode worker polls the renderer's device for
//! every frame. So a wait that needs a callback's result does not read
//! "not reported" as final once its own poll returns; it waits for the
//! callback, polling now and then in case nobody else is, up to a deadline.

use std::sync::{Condvar, Mutex};

/// Counts the reports of one kind of GPU callback and wakes the threads
/// waiting for one, on whichever thread the callback runs.
#[derive(Debug, Default)]
pub(crate) struct CallbackSignal {
    reports: Mutex<u64>,
    reported: Condvar,
}

impl CallbackSignal {
    /// Records that a callback reported, after it stored what it reports.
    pub(crate) fn notify(&self) {
        let mut reports = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *reports = reports.wrapping_add(1);
        drop(reports);
        self.reported.notify_all();
    }

    /// How many reports there have been: read before checking, so that a
    /// report landing after the check ends the wait at once.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn seen(&self) -> u64 {
        *self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits until there has been a report since `seen`, or `timeout`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn wait_past(&self, seen: u64, timeout: std::time::Duration) {
        let reports = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.reported
                .wait_timeout_while(reports, timeout, |reports| *reports == seen)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

/// How long a settling wait sleeps before polling the device again, for a
/// callback that no other thread's poll has collected.
#[cfg(not(target_arch = "wasm32"))]
const REPOLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

#[cfg(not(target_arch = "wasm32"))]
impl crate::Renderer {
    /// Polls this renderer's device until `settled` holds or `deadline`
    /// passes, waking whenever `signal` reports. Returns whether it settled.
    ///
    /// # Errors
    /// Returns what `settled` returns.
    pub(crate) fn settle(
        &self,
        signal: &CallbackSignal,
        deadline: web_time::Instant,
        mut settled: impl FnMut() -> Result<bool, crate::RenderError>,
    ) -> Result<bool, crate::RenderError> {
        loop {
            let seen = signal.seen();
            if settled()? {
                return Ok(true);
            }
            let now = web_time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            signal.wait_past(seen, (deadline - now).min(REPOLL_INTERVAL));
            let _ = self.gpu.device().poll(wgpu::PollType::Poll);
        }
    }
}
