//! How long one submission may keep the GPU busy before the operating system
//! may reset the device.
//!
//! Windows resets a GPU whose work runs past its timeout detection (two
//! seconds by default), macOS restarts one after a similar stretch, and a
//! browser treats either as a lost device. A device lost right after a
//! submission that ran for half of that was most likely lost to that
//! submission: the export's own work stops the GPU, and running it again on
//! the GPU does the same. [`hung_threshold_ms`] is that line, and
//! [`submission_budget_ms`] the share of the watchdog one submission is meant
//! to stay under.

use std::sync::atomic::{AtomicU32, Ordering};

/// The watchdog this crate assumes, in milliseconds: Windows' default timeout
/// detection, the shortest of the platforms'.
pub const WATCHDOG_MS: u32 = 2_000;

/// The watchdog set through [`set_watchdog_ms`]; zero when unset.
static OVERRIDE_MS: AtomicU32 = AtomicU32::new(0);

/// The watchdog, in milliseconds: a value set through [`set_watchdog_ms`],
/// else natively `ALPHA2_GPU_WATCHDOG_MS` when it is a positive integer, else
/// [`WATCHDOG_MS`].
#[must_use]
pub fn watchdog_ms() -> u32 {
    match OVERRIDE_MS.load(Ordering::Relaxed) {
        0 => environment_watchdog_ms().unwrap_or(WATCHDOG_MS),
        set => set,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn environment_watchdog_ms() -> Option<u32> {
    std::env::var("ALPHA2_GPU_WATCHDOG_MS")
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
}

#[cfg(target_arch = "wasm32")]
const fn environment_watchdog_ms() -> Option<u32> {
    None
}

/// Replaces the watchdog for this process (zero restores the default): how a
/// browser page, which has no environment, makes a short one for a test.
pub fn set_watchdog_ms(value: u32) {
    OVERRIDE_MS.store(value, Ordering::Relaxed);
}

/// A submission that ran at least this long before the device was lost is
/// taken as the loss's cause: half the watchdog.
#[must_use]
pub fn hung_threshold_ms() -> u32 {
    watchdog_ms() / 2
}

/// The GPU time one submission is meant to stay under: an eighth of the
/// watchdog, which leaves room for other work queued on the same GPU.
#[must_use]
pub fn submission_budget_ms() -> u32 {
    watchdog_ms() / 8
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_threshold_and_budget_are_fractions_of_the_watchdog() {
        super::set_watchdog_ms(4_000);
        assert_eq!(super::watchdog_ms(), 4_000);
        assert_eq!(super::hung_threshold_ms(), 2_000);
        assert_eq!(super::submission_budget_ms(), 500);
        super::set_watchdog_ms(0);
        assert!(super::watchdog_ms() > 0);
    }
}
