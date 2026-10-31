//! Releasing the GPU host after a command's outcome is out, within a deadline.
//!
//! `wgpu`'s queue drop waits for the queue to go idle (`vkQueueWaitIdle` on Vulkan) with no
//! timeout, so a device that stopped completing work without being reported lost never lets the
//! drop return. `main` therefore reports first and releases here; a release still running after
//! [`configured_gpu_wait_timeout`] is reported and the process exits with the status it would have
//! had, which frees what the device held.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::config::configured_gpu_wait_timeout;
use crate::host::Host;

/// Ends the process with `code` when the drop takes longer than [`configured_gpu_wait_timeout`].
pub(crate) fn release(host: Option<Host>, code: u8) {
    let Some(host) = host else {
        return;
    };
    release_within(host, configured_gpu_wait_timeout(), code);
}

fn release_within<T>(value: T, deadline: Duration, code: u8) {
    let (released, watched) = mpsc::channel::<()>();
    let watchdog = thread::Builder::new()
        .name("alpha2 teardown".to_owned())
        .spawn(move || {
            if watched.recv_timeout(deadline) == Err(RecvTimeoutError::Timeout) {
                alpha2_os::diagnostic!(
                    "alpha2-host: releasing the GPU device took longer than {} ms, so the device \
                     may be hung; exiting without waiting for it",
                    deadline.as_millis()
                );
                // The outcome was written and flushed before the release began.
                crate::exit_unflushed(code);
            }
        });
    drop(value);
    drop(released);
    if let Ok(watchdog) = watchdog {
        let _ = watchdog.join();
    }
}

/// A host drop that never returns, as on a device that never goes idle.
#[cfg(test)]
pub(crate) mod injected {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STALL: AtomicBool = AtomicBool::new(false);

    pub(crate) fn stall_host_drops() {
        STALL.store(true, Ordering::Release);
    }

    pub(crate) struct DropStall;

    impl Drop for DropStall {
        fn drop(&mut self) {
            if STALL.load(Ordering::Acquire) {
                loop {
                    std::thread::park();
                }
            }
        }
    }
}
