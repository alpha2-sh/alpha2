//! Decoded frames kept for a backward run, within a byte budget.
//!
//! A backward run decodes a stretch forward once and hands its frames out in reverse. The window
//! keys frames by time; consecutive keys may share one frame, which counts once. While it holds
//! more than its budget it drops its earliest keys, which the run reaches last and decodes again.

use std::collections::BTreeMap;
use std::sync::Arc;

use alpha2_time::MediaTime;

/// Frames by time, at most `budget` bytes of distinct frames (always at least one key).
pub struct ReverseWindow<F> {
    frames: BTreeMap<MediaTime, Arc<F>>,
    bytes: u64,
    budget: u64,
}

impl<F> ReverseWindow<F> {
    /// An empty window holding at most `budget` bytes.
    #[must_use]
    pub const fn new(budget: u64) -> Self {
        Self {
            frames: BTreeMap::new(),
            bytes: 0,
            budget,
        }
    }

    /// Adds `frame` at `time`, after every key so far, and drops the earliest keys while over
    /// budget. `bytes` is what `frame` holds; it counts once when the previous key shares it.
    pub fn push(&mut self, time: MediaTime, frame: Arc<F>, bytes: impl Fn(&F) -> u64) {
        if !self
            .frames
            .last_key_value()
            .is_some_and(|(_, last)| Arc::ptr_eq(last, &frame))
        {
            self.bytes = self.bytes.saturating_add(bytes(&frame));
        }
        self.frames.insert(time, frame);
        while self.bytes > self.budget && self.frames.len() > 1 {
            if let Some((_, dropped)) = self.frames.pop_first()
                && !self
                    .frames
                    .first_key_value()
                    .is_some_and(|(_, first)| Arc::ptr_eq(first, &dropped))
            {
                self.bytes = self.bytes.saturating_sub(bytes(&dropped));
            }
        }
    }

    /// The frame at exactly `time`.
    #[must_use]
    pub fn get(&self, time: MediaTime) -> Option<&Arc<F>> {
        self.frames.get(&time)
    }

    /// The latest key.
    #[must_use]
    pub fn last_time(&self) -> Option<MediaTime> {
        self.frames.last_key_value().map(|(time, _)| *time)
    }

    /// Removes and returns the latest frame keyed before `time`, dropping every key from it on.
    pub fn take_before(&mut self, time: MediaTime, bytes: impl Fn(&F) -> u64) -> Option<Arc<F>> {
        let (&key, _) = self.frames.range(..time).next_back()?;
        let later = self.frames.split_off(&key);
        let mut previous: Option<&Arc<F>> = self.frames.last_key_value().map(|(_, frame)| frame);
        for frame in later.values() {
            if !previous.is_some_and(|before| Arc::ptr_eq(before, frame)) {
                self.bytes = self.bytes.saturating_sub(bytes(frame));
            }
            previous = Some(frame);
        }
        later.into_values().next()
    }

    /// Bytes of the distinct frames held.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether it holds no frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// How many distinct frames it holds.
    #[must_use]
    pub fn distinct(&self) -> usize {
        let frames = self.frames.values().collect::<Vec<_>>();
        frames
            .windows(2)
            .filter(|pair| !Arc::ptr_eq(pair[0], pair[1]))
            .count()
            + usize::from(!frames.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn size(frame: &u64) -> u64 {
        *frame
    }

    #[test]
    fn a_shared_frame_counts_once_and_the_earliest_keys_go_first() {
        let mut window = ReverseWindow::new(25);
        let a = Arc::new(10);
        window.push(MediaTime(0), Arc::clone(&a), size);
        window.push(MediaTime(1), a, size);
        assert_eq!((window.bytes(), window.distinct()), (10, 1));
        window.push(MediaTime(2), Arc::new(10), size);
        window.push(MediaTime(3), Arc::new(10), size);
        // 30 bytes: both keys of the shared frame go, and its bytes once.
        assert_eq!(window.bytes(), 20);
        assert!(window.get(MediaTime(1)).is_none());
        assert_eq!(window.last_time(), Some(MediaTime(3)));
    }

    #[test]
    fn taking_before_a_time_answers_the_latest_earlier_frame_and_drops_the_rest() {
        let mut window = ReverseWindow::new(100);
        for time in 0..4 {
            window.push(MediaTime(time * 10), Arc::new(5), size);
        }
        assert_eq!(window.take_before(MediaTime(25), size).map(|f| *f), Some(5));
        assert_eq!(window.last_time(), Some(MediaTime(10)));
        assert_eq!(window.bytes(), 10);
        assert!(window.take_before(MediaTime(0), size).is_none());
        let a = Arc::new(7);
        let mut shared = ReverseWindow::new(100);
        shared.push(MediaTime(0), Arc::clone(&a), size);
        shared.push(MediaTime(1), a, size);
        assert!(shared.take_before(MediaTime(5), size).is_some());
        assert_eq!(shared.bytes(), 7);
    }
}
