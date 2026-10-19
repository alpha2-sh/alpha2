//! Count-and-byte budget for decoder surfaces.

use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Current native surface allocation.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SurfaceUsage {
    /// Number of retained decoder surfaces.
    pub surfaces: u32,
    /// Approximate native allocation bytes.
    pub bytes: u64,
}

#[derive(Debug)]
struct State {
    used: SurfaceUsage,
}

/// Bounded decoder-surface allocator shared by decode sessions.
#[derive(Clone, Debug)]
pub struct SurfaceBudget {
    max: SurfaceUsage,
    state: Arc<Mutex<State>>,
}

/// A reservation released exactly once when its final owner is dropped.
#[derive(Debug)]
pub struct SurfaceReservation {
    usage: SurfaceUsage,
    state: Arc<Mutex<State>>,
}

/// A reservation exceeded either the count or byte limit.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SurfaceBudgetError;

impl SurfaceBudget {
    /// Creates a budget with independent count and byte ceilings.
    #[must_use]
    pub fn new(max_surfaces: u32, max_bytes: u64) -> Self {
        Self {
            max: SurfaceUsage {
                surfaces: max_surfaces,
                bytes: max_bytes,
            },
            state: Arc::new(Mutex::new(State {
                used: SurfaceUsage::default(),
            })),
        }
    }

    /// Reserves one surface before the decoder is asked to acquire it.
    ///
    /// # Errors
    /// Returns [`SurfaceBudgetError`] if either configured ceiling would be exceeded.
    pub fn try_reserve(&self, bytes: u64) -> Result<SurfaceReservation, SurfaceBudgetError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = SurfaceUsage {
            surfaces: state.used.surfaces.saturating_add(1),
            bytes: state.used.bytes.saturating_add(bytes),
        };
        if next.surfaces > self.max.surfaces || next.bytes > self.max.bytes {
            return Err(SurfaceBudgetError);
        }
        state.used = next;
        Ok(SurfaceReservation {
            usage: SurfaceUsage { surfaces: 1, bytes },
            state: Arc::clone(&self.state),
        })
    }

    /// Returns the currently retained allocation.
    #[must_use]
    pub fn usage(&self) -> SurfaceUsage {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .used
    }
}

impl Drop for SurfaceReservation {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.used.surfaces = state.used.surfaces.saturating_sub(self.usage.surfaces);
        state.used.bytes = state.used.bytes.saturating_sub(self.usage.bytes);
    }
}

impl fmt::Display for SurfaceBudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("native decoder surface budget exhausted")
    }
}
impl Error for SurfaceBudgetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_enforce_both_limits_and_release_exactly_once() {
        let budget = SurfaceBudget::new(2, 30);
        let first = budget.try_reserve(10).expect("first");
        let second = budget.try_reserve(20).expect("second");
        assert_eq!(
            budget.usage(),
            SurfaceUsage {
                surfaces: 2,
                bytes: 30
            }
        );
        assert_eq!(budget.try_reserve(0).unwrap_err(), SurfaceBudgetError);
        drop(first);
        assert_eq!(
            budget.usage(),
            SurfaceUsage {
                surfaces: 1,
                bytes: 20
            }
        );
        drop(second);
        assert_eq!(budget.usage(), SurfaceUsage::default());
    }

    /// The byte ceiling refuses on its own, with surfaces to spare: a 4K
    /// P010 surface is about 25 MB, so 512 MiB fills before 32 surfaces do.
    /// The reservation that reaches the ceiling exactly fits.
    #[test]
    fn the_byte_ceiling_refuses_a_reservation_the_count_would_allow() {
        let budget = SurfaceBudget::new(10, 30);
        let held = budget.try_reserve(20).expect("within both ceilings");
        assert_eq!(budget.try_reserve(11).unwrap_err(), SurfaceBudgetError);
        assert_eq!(
            budget.usage(),
            SurfaceUsage {
                surfaces: 1,
                bytes: 20
            },
            "a refused reservation takes nothing"
        );
        let exact = budget
            .try_reserve(10)
            .expect("reaching the ceiling exactly");
        assert_eq!(budget.try_reserve(1).unwrap_err(), SurfaceBudgetError);
        drop((held, exact));

        // One surface larger than the whole ceiling, into an empty budget.
        assert_eq!(budget.try_reserve(31).unwrap_err(), SurfaceBudgetError);
        assert_eq!(budget.usage(), SurfaceUsage::default());
        drop(budget.try_reserve(30).expect("the whole ceiling at once"));
    }
}
