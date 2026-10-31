//! Waiting for GPU callbacks that may run on another thread.
//!
//! `wgpu` runs a map or completion callback on whichever thread's poll or submit collected it,
//! after that call's own work. A thread's wait can therefore return while the callback is still
//! running elsewhere (on macOS the `VideoToolbox` worker polls the same device). Waits that need
//! a callback's result wait for it, polling occasionally, until a deadline.

use std::sync::{Condvar, Mutex};

/// Counts reports of one kind of GPU callback and wakes waiters, from any thread.
#[derive(Debug, Default)]
pub(crate) struct CallbackSignal {
    reports: Mutex<u64>,
    reported: Condvar,
}

impl CallbackSignal {
    /// Records a report; call after storing the reported value.
    pub(crate) fn notify(&self) {
        let mut reports = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *reports = reports.wrapping_add(1);
        drop(reports);
        self.reported.notify_all();
    }

    /// Report count; read it before checking so a report after the check ends the wait at once.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn seen(&self) -> u64 {
        *self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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

/// Sleep between polls while waiting for a callback no other thread has collected.
#[cfg(not(target_arch = "wasm32"))]
const REPOLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

#[cfg(not(target_arch = "wasm32"))]
impl crate::Renderer {
    /// Polls the device until `settled` holds or `deadline` passes, waking on `signal`. Returns
    /// whether it settled.
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
