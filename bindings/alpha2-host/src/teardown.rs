//! Releasing the GPU host once a command's outcome is out, within a
//! deadline.
//!
//! Dropping the host waits for its device to finish its work: `wgpu`'s queue
//! drop waits for the queue to go idle (`vkQueueWaitIdle` on Vulkan), and
//! that wait has no timeout. A device that stopped completing work without
//! being reported lost never goes idle, and the drop never returns. A host
//! dropped before the outcome is reported would leave the caller with
//! neither the error nor an exit.
//!
//! So `main` reports first and releases afterwards, here, and a release that
//! has not finished within [`configured_gpu_wait_timeout`] (the limit every
//! other wait on GPU work has) is reported and the process ends without
//! waiting for it, with the status it would have had. Ending the process is
//! what frees what the device still held.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::config::configured_gpu_wait_timeout;
use crate::host::Host;

/// Drops `host`, if a command made one, and returns; or, when that takes
/// longer than [`configured_gpu_wait_timeout`], ends the process with `code`.
pub(crate) fn release(host: Option<Host>, code: u8) {
    let Some(host) = host else {
        return;
    };
    release_within(host, configured_gpu_wait_timeout(), code);
}

/// Drops `value` on this thread while another waits `deadline` for it, and
/// ends the process with `code` if the drop is still running then.
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
    // Without a watchdog thread the drop was waited for unbounded. The watchdog returns at
    // once when the drop is done.
    if let Ok(watchdog) = watchdog {
        let _ = watchdog.join();
    }
}

/// A host drop that never returns, as one on a device that never goes idle.
#[cfg(test)]
pub(crate) mod injected {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STALL: AtomicBool = AtomicBool::new(false);

    /// Every later drop of a host in this process blocks forever.
    pub(crate) fn stall_host_drops() {
        STALL.store(true, Ordering::Release);
    }

    /// Held by every host ([`crate::host::Host::_drop_stall`]).
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
