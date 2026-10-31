//! How long each of the renderer's recent submissions kept the GPU busy.
//!
//! A device lost while the renderer's own work ran is usually lost to that
//! work: Windows resets a GPU whose submission outruns its timeout detection,
//! and the DX12 fence of a reset device reads complete, so the loss looks
//! like any other (a missing canary, [`crate::RenderError::GpuWorkDropped`]).
//! What tells the two apart is how long the submissions before it ran. Each
//! submission records when it was submitted and, through the queue's
//! completion callback, when its work finished; its GPU time is the span
//! between the later of its submission and the previous submission's end,
//! and its own end, so time spent queued behind earlier work is not counted.
//!
//! A completion is seen when the device is polled (natively) or the browser's
//! event loop runs, so the span includes that latency: small beside the
//! threshold it is compared with ([`alpha2_gpu::hung_threshold_ms`], half the
//! watchdog).

use std::collections::VecDeque;
use std::sync::Mutex;

use web_time::Instant;

/// Submissions the clock keeps: enough to cover every submission of the last
/// frames whatever the device lost.
const KEPT: usize = 32;

/// The frame a submission belongs to, as the caller that renders it knows it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameMark {
    /// The frame's timeline time, in ticks.
    pub time_ticks: i64,
    /// The frame's index in the export, when it is one.
    pub frame_index: Option<u64>,
}

/// The longest recent submission: how long it kept the GPU busy, and what it
/// rendered.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionReport {
    /// Its GPU time, in milliseconds; for a submission still running, how
    /// long it has run so far.
    pub gpu_ms: f64,
    /// The frame it rendered, when the caller marked one.
    pub frame: Option<FrameMark>,
    /// The effects it ran, as `id@version`.
    pub effects: Vec<String>,
}

#[derive(Debug)]
struct Entry {
    submitted_at: Instant,
    done_at: Option<Instant>,
    frame: Option<FrameMark>,
    effects: Vec<String>,
}

#[derive(Debug, Default)]
struct ClockState {
    /// Sequence number of `entries[0]`.
    first: u64,
    entries: VecDeque<Entry>,
    frame: Option<FrameMark>,
    effects: Vec<String>,
}

/// The renderer's recent submissions and when each finished.
#[derive(Debug, Default)]
pub(crate) struct SubmissionClock {
    state: Mutex<ClockState>,
}

impl SubmissionClock {
    fn state(&self) -> std::sync::MutexGuard<'_, ClockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks the frame the next submissions render, until another is marked.
    pub(crate) fn mark_frame(&self, frame: Option<FrameMark>) {
        self.state().frame = frame;
    }

    /// Notes an effect the next submission runs.
    pub(crate) fn note_effect(&self, effect: &crate::EffectId) {
        let name = format!("{}@{}", effect.as_str(), effect.version());
        let mut state = self.state();
        if !state.effects.contains(&name) {
            state.effects.push(name);
        }
    }

    /// Records a submission made at `at`, and returns its sequence number for
    /// [`Self::complete`].
    pub(crate) fn record(&self, at: Instant) -> u64 {
        let mut state = self.state();
        let effects = std::mem::take(&mut state.effects);
        let frame = state.frame;
        state.entries.push_back(Entry {
            submitted_at: at,
            done_at: None,
            frame,
            effects,
        });
        if state.entries.len() > KEPT {
            state.entries.pop_front();
            state.first += 1;
        }
        state.first + u64::try_from(state.entries.len()).unwrap_or(u64::MAX) - 1
    }

    /// Records that submission `sequence`'s work finished at `at`. The queue
    /// finishes submissions in order, so every earlier one has finished too.
    pub(crate) fn complete(&self, sequence: u64, at: Instant) {
        let mut state = self.state();
        let Some(position) = sequence.checked_sub(state.first) else {
            return;
        };
        let Ok(position) = usize::try_from(position) else {
            return;
        };
        for entry in state.entries.iter_mut().take(position + 1) {
            entry.done_at.get_or_insert(at);
        }
    }

    /// The GPU time of each kept submission, oldest first: finished ones in
    /// full, the oldest unfinished one up to `now`, later unfinished ones not
    /// at all (they wait behind it).
    fn spans(state: &ClockState, now: Instant) -> Vec<(f64, &Entry)> {
        let mut spans = Vec::new();
        let mut previous_done: Option<Instant> = None;
        for entry in &state.entries {
            let start =
                previous_done.map_or(entry.submitted_at, |done| done.max(entry.submitted_at));
            let Some(done) = entry.done_at else {
                spans.push((milliseconds(now.saturating_duration_since(start)), entry));
                break;
            };
            spans.push((milliseconds(done.saturating_duration_since(start)), entry));
            previous_done = Some(done);
        }
        spans
    }

    /// The kept submission that kept the GPU busy longest, counting a
    /// submission still running up to `now`.
    pub(crate) fn longest_at(&self, now: Instant) -> Option<SubmissionReport> {
        let state = self.state();
        Self::spans(&state, now)
            .into_iter()
            .max_by(|(a, _), (b, _)| a.total_cmp(b))
            .map(|(gpu_ms, entry)| SubmissionReport {
                gpu_ms,
                frame: entry.frame,
                effects: entry.effects.clone(),
            })
    }

    /// [`Self::longest_at`] now.
    pub(crate) fn longest(&self) -> Option<SubmissionReport> {
        self.longest_at(Instant::now())
    }

    /// The longest recent submission when it ran for at least the hung
    /// threshold ([`alpha2_gpu::hung_threshold_ms`]): the evidence that a
    /// loss now was caused by this renderer's own work.
    pub(crate) fn hung(&self) -> Option<SubmissionReport> {
        self.longest()
            .filter(|report| report.gpu_ms >= f64::from(alpha2_gpu::hung_threshold_ms()))
    }
}

fn milliseconds(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Three submissions made at once run one after another: each one's GPU
    /// time starts where the one before it ended, not when it was submitted.
    #[test]
    fn gpu_time_excludes_queueing_behind_earlier_submissions() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let first = clock.record(at(0));
        let second = clock.record(at(0));
        let third = clock.record(at(0));
        clock.complete(first, at(10));
        clock.complete(second, at(400));
        clock.complete(third, at(405));
        let longest = clock.longest_at(at(1_000)).expect("submissions");
        assert!((longest.gpu_ms - 390.0).abs() < 0.5, "{longest:?}");
        // A later submission made after the queue went idle starts at its own submission.
        let fourth = clock.record(at(2_000));
        clock.complete(fourth, at(2_005));
        assert!((clock.longest_at(at(3_000)).expect("kept").gpu_ms - 390.0).abs() < 0.5);
        // A submission that never finished counts up to now.
        let _ = clock.record(at(3_000));
        assert!((clock.longest_at(at(3_700)).expect("running").gpu_ms - 700.0).abs() < 0.5);
    }

    #[test]
    fn a_completion_finishes_every_earlier_submission() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let first = clock.record(start);
        let second = clock.record(start);
        clock.complete(second, start + Duration::from_millis(8));
        let _ = first;
        assert_eq!(
            clock
                .longest_at(start + Duration::from_secs(5))
                .map(|r| r.gpu_ms.round()),
            Some(8.0)
        );
    }

    #[test]
    fn the_clock_keeps_the_frame_and_effects_of_each_submission() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        clock.mark_frame(Some(FrameMark {
            time_ticks: 4_000,
            frame_index: Some(1),
        }));
        clock.note_effect(&crate::EffectId::from_static("blur", 2));
        let heavy = clock.record(start);
        clock.complete(heavy, start + Duration::from_millis(30));
        let light = clock.record(start + Duration::from_millis(40));
        clock.complete(light, start + Duration::from_millis(41));
        let report = clock
            .longest_at(start + Duration::from_millis(50))
            .expect("kept");
        assert_eq!(report.effects, ["blur@2"]);
        assert_eq!(report.frame.and_then(|frame| frame.frame_index), Some(1));
    }
}
