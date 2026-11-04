//! Frame-sized pixel buffers kept between pictures, so each frame does not map and fault in fresh
//! memory.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Smaller buffers come from the allocator's own free lists cheaply.
const MIN_BYTES: usize = 1 << 20;
/// A 4K frame holds two or three frame-sized buffers at once.
const MAX_BUFFERS: usize = 8;
const DEFAULT_BUDGET_BYTES: u64 = 768 << 20;

pub(crate) type HalfPixel = [u16; 4];

/// Idle half-float pixel buffers of one CPU renderer, reused by exact length.
#[derive(Debug)]
pub(crate) struct FramePool {
    state: Mutex<State>,
    reused: AtomicU64,
}

#[derive(Debug)]
struct State {
    /// Oldest first; evicted from the front.
    idle: VecDeque<Vec<HalfPixel>>,
    bytes: u64,
    budget: u64,
}

impl Default for FramePool {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                idle: VecDeque::new(),
                bytes: 0,
                budget: DEFAULT_BUDGET_BYTES,
            }),
            reused: AtomicU64::new(0),
        }
    }
}

impl FramePool {
    /// A buffer of `len` pixels whose contents are unspecified: a reused buffer holds an earlier
    /// picture, so the caller writes every pixel.
    pub(crate) fn take(&self, len: usize) -> Vec<HalfPixel> {
        if bytes_of(len) >= MIN_BYTES as u64 {
            let mut state = self.lock();
            if let Some(index) = state.idle.iter().rposition(|buffer| buffer.len() == len) {
                let buffer = state.idle.remove(index).expect("the index is in range");
                state.bytes -= bytes_of(len);
                drop(state);
                self.reused.fetch_add(1, Ordering::Relaxed);
                return buffer;
            }
        }
        vec![[0; 4]; len]
    }

    /// Keeps `buffer` for a later [`Self::take`], evicting the oldest buffers past the limits.
    pub(crate) fn give_back(&self, buffer: Vec<HalfPixel>) {
        let bytes = bytes_of(buffer.len());
        if bytes < MIN_BYTES as u64 || buffer.capacity() != buffer.len() {
            return;
        }
        let mut state = self.lock();
        state.idle.push_back(buffer);
        state.bytes += bytes;
        let evicted = state.evict();
        drop(state);
        drop(evicted);
    }

    /// Drops every idle buffer and returns the bytes they held.
    pub(crate) fn release(&self) -> u64 {
        let mut state = self.lock();
        let idle = std::mem::take(&mut state.idle);
        let bytes = std::mem::take(&mut state.bytes);
        drop(state);
        drop(idle);
        bytes
    }

    /// Caps the idle bytes at `budget`, evicting down to it now.
    pub(crate) fn set_budget(&self, budget: u64) {
        let mut state = self.lock();
        state.budget = budget;
        let evicted = state.evict();
        drop(state);
        drop(evicted);
    }

    /// Takes served by an idle buffer.
    pub(crate) fn reused(&self) -> u64 {
        self.reused.load(Ordering::Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Removes the oldest buffers until within the limits; the caller frees them after unlocking.
    fn evict(&mut self) -> Vec<Vec<HalfPixel>> {
        let mut evicted = Vec::new();
        while self.idle.len() > MAX_BUFFERS || self.bytes > self.budget {
            let Some(buffer) = self.idle.pop_front() else {
                break;
            };
            self.bytes -= bytes_of(buffer.len());
            evicted.push(buffer);
        }
        evicted
    }
}

const fn bytes_of(len: usize) -> u64 {
    len as u64 * size_of::<HalfPixel>() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const LARGE: usize = MIN_BYTES / 8;

    #[test]
    fn a_returned_buffer_serves_the_next_take_of_its_length() {
        let pool = FramePool::default();
        let mut buffer = pool.take(LARGE);
        buffer[0] = [1, 2, 3, 4];
        let address = buffer.as_ptr();
        pool.give_back(buffer);
        assert_eq!(pool.take(LARGE + 1).len(), LARGE + 1);
        let again = pool.take(LARGE);
        assert_eq!(again.as_ptr(), address);
        assert_eq!(again[0], [1, 2, 3, 4]);
        assert_eq!(pool.reused(), 1);
    }

    #[test]
    fn small_buffers_are_not_kept() {
        let pool = FramePool::default();
        pool.give_back(vec![[0; 4]; LARGE - 1]);
        assert_eq!(pool.release(), 0);
    }

    #[test]
    fn the_oldest_buffers_go_first_past_the_count_and_byte_limits() {
        let pool = FramePool::default();
        for extra in 0..=MAX_BUFFERS {
            pool.give_back(vec![[0; 4]; LARGE + extra]);
        }
        assert_eq!(pool.take(LARGE).len(), LARGE);
        assert_eq!(pool.reused(), 0, "the first buffer was evicted");
        pool.set_budget(bytes_of(LARGE + MAX_BUFFERS));
        assert_eq!(pool.release(), bytes_of(LARGE + MAX_BUFFERS));
    }

    #[test]
    fn release_empties_the_pool() {
        let pool = FramePool::default();
        pool.give_back(vec![[0; 4]; LARGE]);
        assert_eq!(pool.release(), bytes_of(LARGE));
        assert_eq!(pool.release(), 0);
        pool.take(LARGE);
        assert_eq!(pool.reused(), 0);
    }
}
