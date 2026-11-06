//! The byte budget of decoded source frames, which the host may lower while work runs, as an app
//! does when the system warns it of memory pressure.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alpha2_os::MemoryCapacity;

/// Fewest bytes a budget halves to. A cache following the budget keeps at least its last frame's
/// working set on top of this ([`super::SourceFrameCache::follow`]), so the floor only bounds
/// how much a run of memory warnings can take, not what a frame of any size needs.
pub const MIN_FRAME_BYTES: u64 = 32 << 20;

/// A shared byte budget for one export, still run or preview. Clones see the same budget; the
/// work reads it before every frame and evicts to fit.
#[derive(Clone, Debug)]
pub struct FrameBudget(Arc<AtomicU64>);

impl FrameBudget {
    /// A budget of `bytes`, never below [`MIN_FRAME_BYTES`].
    #[must_use]
    pub fn new(bytes: u64) -> Self {
        Self(Arc::new(AtomicU64::new(bytes.max(MIN_FRAME_BYTES))))
    }

    /// A quarter of the memory `capacity` leaves this process, at most `ceiling`. Without a
    /// figure for it, `ceiling`.
    #[must_use]
    pub fn of_memory(capacity: &MemoryCapacity, ceiling: u64) -> Self {
        Self::new(
            capacity
                .headroom()
                .map_or(ceiling, |free| (free / 4).min(ceiling)),
        )
    }

    /// The budget now.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Halves the budget, down to [`MIN_FRAME_BYTES`].
    pub fn halve(&self) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
                Some((bytes / 2).max(MIN_FRAME_BYTES))
            });
    }

    /// Whether work other than this handle still holds the budget.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameBudget, MIN_FRAME_BYTES};
    use alpha2_os::MemoryCapacity;

    #[test]
    fn a_budget_takes_a_quarter_of_the_headroom_up_to_its_ceiling_and_halves_to_the_floor() {
        let capacity = MemoryCapacity {
            available_bytes: Some(1 << 30),
            ..MemoryCapacity::default()
        };
        assert_eq!(
            FrameBudget::of_memory(&capacity, 1 << 30).bytes(),
            256 << 20
        );
        assert_eq!(
            FrameBudget::of_memory(&capacity, 64 << 20).bytes(),
            64 << 20
        );
        let unknown = FrameBudget::of_memory(&MemoryCapacity::default(), 512 << 20);
        assert_eq!(unknown.bytes(), 512 << 20);
        let shared = unknown.clone();
        assert!(unknown.is_shared());
        for _ in 0..8 {
            shared.halve();
        }
        assert_eq!(unknown.bytes(), MIN_FRAME_BYTES);
        drop(shared);
        assert!(!unknown.is_shared());
        assert_eq!(FrameBudget::new(0).bytes(), MIN_FRAME_BYTES);
    }
}
