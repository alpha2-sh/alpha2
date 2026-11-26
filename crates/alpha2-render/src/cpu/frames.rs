//! Frame-sized pixel buffers kept between pictures, so each frame does not map and fault in fresh
//! memory.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::recent_use::RecentPeaks;

/// Smaller buffers come from the allocator's own free lists cheaply.
const MIN_BYTES: usize = 1 << 20;
/// A 4K frame holds two or three frame-sized buffers at once.
const MAX_BUFFERS: usize = 8;
const DEFAULT_BUDGET_BYTES: u64 = 768 << 20;

pub(crate) type HalfPixel = [u16; 4];

/// Idle half-float pixel buffers of one CPU renderer, reused by exact length.
///
/// Per length it keeps as many buffers as any of the last
/// [`crate::recent_use::RECENT_FRAMES`] frames took from it at once, whatever the limits; the
/// count and byte limits bound only the idle buffers beyond that, oldest first.
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
    recent_use: HashMap<usize, RecentTakes>,
}

#[derive(Debug)]
struct RecentTakes {
    out_this_frame: usize,
    peaks: RecentPeaks,
}

impl Default for FramePool {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                idle: VecDeque::new(),
                bytes: 0,
                budget: DEFAULT_BUDGET_BYTES,
                recent_use: HashMap::new(),
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
            let taken = state.recent_use.entry(len).or_insert(RecentTakes {
                out_this_frame: 0,
                peaks: RecentPeaks::new(),
            });
            taken.out_this_frame = taken.out_this_frame.saturating_add(1);
            taken.peaks.raise(taken.out_this_frame);
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

    /// Keeps `buffer` for a later [`Self::take`], evicting the oldest spare buffers past the
    /// limits.
    pub(crate) fn give_back(&self, buffer: Vec<HalfPixel>) {
        let bytes = bytes_of(buffer.len());
        if bytes < MIN_BYTES as u64 || buffer.capacity() != buffer.len() {
            return;
        }
        let mut state = self.lock();
        if let Some(taken) = state.recent_use.get_mut(&buffer.len()) {
            taken.out_this_frame = taken.out_this_frame.saturating_sub(1);
        }
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

    pub(crate) fn next_frame(&self) {
        let mut state = self.lock();
        state.recent_use.retain(|_, taken| {
            taken.peaks.advance(1, 0);
            taken.out_this_frame = 0;
            taken.peaks.most() > 0
        });
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
    /// Removes the oldest spare buffers until within the limits.
    #[must_use]
    fn evict(&mut self) -> Vec<Vec<HalfPixel>> {
        let mut evicted = Vec::new();
        let mut index = 0;
        while self.idle.len() > MAX_BUFFERS || self.bytes > self.budget {
            let Some(len) = self.idle.get(index).map(Vec::len) else {
                break;
            };
            if self.idle_of(len) <= self.kept_idle(len) {
                index += 1;
                continue;
            }
            let buffer = self.idle.remove(index).expect("the index is in range");
            self.bytes -= bytes_of(len);
            evicted.push(buffer);
        }
        evicted
    }

    fn kept_idle(&self, len: usize) -> usize {
        self.recent_use
            .get(&len)
            .map_or(0, |taken| taken.peaks.most())
    }

    fn idle_of(&self, len: usize) -> usize {
        self.idle
            .iter()
            .filter(|buffer| buffer.len() == len)
            .count()
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

    fn frame(pool: &FramePool, len: usize, at_once: usize) {
        pool.next_frame();
        let taken = (0..at_once).map(|_| pool.take(len)).collect::<Vec<_>>();
        for buffer in taken {
            pool.give_back(buffer);
        }
    }

    #[test]
    fn a_frame_taking_more_than_the_limits_hold_reuses_its_buffers() {
        let pool = FramePool::default();
        pool.set_budget(bytes_of(LARGE));
        for _ in 0..2 {
            frame(&pool, LARGE, MAX_BUFFERS + 2);
        }
        let reused = pool.reused();
        for _ in 0..10 {
            frame(&pool, LARGE, MAX_BUFFERS + 2);
        }
        assert_eq!(
            pool.reused() - reused,
            10 * u64::try_from(MAX_BUFFERS + 2).expect("small"),
            "every take after warm-up is served by an idle buffer"
        );
    }

    #[test]
    fn a_length_no_frame_takes_any_more_leaves_after_the_recent_frames() {
        let pool = FramePool::default();
        pool.set_budget(bytes_of(LARGE + 1));
        frame(&pool, LARGE, 3);
        assert_eq!(pool.release(), 3 * bytes_of(LARGE));
        frame(&pool, LARGE, 3);
        for _ in 0..crate::recent_use::RECENT_FRAMES {
            frame(&pool, LARGE + 1, 1);
        }
        assert_eq!(
            pool.release(),
            bytes_of(LARGE + 1),
            "only the length the recent frames took stays"
        );
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
