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

/// Receives a callback's result from `receiver`, calling `poll` between tries so a callback no
/// other thread collects still runs.
///
/// # Errors
/// [`crate::RenderError::GpuTimeout`] when nothing arrives within `timeout`, and
/// [`crate::ReadbackFailure::CallbackDropped`] when the callback was dropped without sending.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn receive_polling<T>(
    receiver: &std::sync::mpsc::Receiver<T>,
    timeout: std::time::Duration,
    poll: impl Fn(),
) -> Result<T, crate::RenderError> {
    use std::sync::mpsc::RecvTimeoutError;
    let deadline = web_time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(web_time::Instant::now());
        match receiver.recv_timeout(left.min(REPOLL_INTERVAL)) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(crate::RenderError::TextureReadback(
                    crate::ReadbackFailure::CallbackDropped,
                ));
            }
            Err(RecvTimeoutError::Timeout) if left.is_zero() => {
                return Err(crate::RenderError::GpuTimeout);
            }
            Err(RecvTimeoutError::Timeout) => poll(),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::receive_polling;
    use crate::{ReadbackFailure, RenderError};

    #[test]
    fn a_result_sent_from_another_thread_later_is_received() {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = sender.send(7);
        });
        let polls = std::cell::Cell::new(0);
        let outcome = receive_polling(&receiver, Duration::from_secs(5), || {
            polls.set(polls.get() + 1);
        });
        assert_eq!(outcome, Ok(7));
        assert!(polls.get() > 0);
    }

    #[test]
    fn a_result_that_never_comes_is_a_gpu_timeout_on_time() {
        let (sender, receiver) = mpsc::channel::<u8>();
        let started = Instant::now();
        let outcome = receive_polling(&receiver, Duration::from_millis(100), || {});
        assert_eq!(outcome, Err(RenderError::GpuTimeout));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(sender);
        assert_eq!(
            receive_polling(&receiver, Duration::from_secs(5), || {}),
            Err(RenderError::TextureReadback(
                ReadbackFailure::CallbackDropped
            ))
        );
    }
}
