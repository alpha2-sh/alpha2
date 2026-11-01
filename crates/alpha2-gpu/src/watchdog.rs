//! How long one submission may keep the GPU busy before the OS may reset the device.
//!
//! Windows TDR resets a GPU after two seconds by default, macOS after a similar time, and
//! browsers report either as device loss. A loss right after a submission that ran past
//! [`hung_threshold_ms`] is attributed to that submission. [`submission_budget_ms`] is the target
//! per submission.

use std::sync::atomic::{AtomicU32, Ordering};

/// The assumed watchdog in milliseconds: Windows' default TDR, the shortest of the platforms.
pub const WATCHDOG_MS: u32 = 2_000;

/// Zero when unset.
static OVERRIDE_MS: AtomicU32 = AtomicU32::new(0);

/// The watchdog in milliseconds: [`set_watchdog_ms`], else a positive `ALPHA2_GPU_WATCHDOG_MS`
/// (native only), else [`WATCHDOG_MS`].
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

/// Overrides the watchdog for this process; zero restores the default. Lets browser tests,
/// which have no environment variables, shorten it.
pub fn set_watchdog_ms(value: u32) {
    OVERRIDE_MS.store(value, Ordering::Relaxed);
}

/// Half the watchdog. A submission this long before a device loss is taken as its cause.
#[must_use]
pub fn hung_threshold_ms() -> u32 {
    watchdog_ms() / 2
}

/// An eighth of the watchdog per submission, leaving room for other work on the GPU.
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
