//! How long each recent renderer submission kept the GPU busy.
//!
//! Windows resets a GPU whose submission exceeds its timeout, and a reset device's DX12 fence
//! reads complete, so the loss looks like dropped work. The submissions' run times tell the two
//! apart. Each submission's GPU time runs from the later of its submit and the previous
//! submission's end to its own completion callback, so queueing is excluded. Callback latency
//! (poll or event loop) is small beside [`alpha2_gpu::hung_threshold_ms`].

use std::collections::VecDeque;
use std::sync::Mutex;

use web_time::Instant;

/// Submissions kept; enough to cover the last frames' submissions.
const KEPT: usize = 32;

/// The frame a submission belongs to, as its caller knows it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameMark {
    /// The frame's timeline time, in ticks.
    pub time_ticks: i64,
    /// The frame's index in the export, when it is one.
    pub frame_index: Option<u64>,
}

/// The longest recent submission and what it rendered.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionReport {
    /// GPU time in milliseconds; for one still running, the time so far.
    pub gpu_ms: f64,
    /// The frame it rendered, when the caller marked one.
    pub frame: Option<FrameMark>,
    /// The effects it ran, as `id@version`.
    pub effects: Vec<String>,
    /// Predicted GPU time in milliseconds, when the frame was planned.
    pub predicted_ms: Option<f64>,
}

#[derive(Debug)]
struct Entry {
    submitted_at: Instant,
    done_at: Option<Instant>,
    frame: Option<FrameMark>,
    effects: Vec<String>,
    predicted_ms: Option<f64>,
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

    /// Records a submission made at `at` and returns its sequence number for [`Self::complete`].
    pub(crate) fn record(&self, at: Instant, predicted_ms: Option<f64>) -> u64 {
        let mut state = self.state();
        let effects = std::mem::take(&mut state.effects);
        let frame = state.frame;
        state.entries.push_back(Entry {
            submitted_at: at,
            done_at: None,
            frame,
            effects,
            predicted_ms,
        });
        if state.entries.len() > KEPT {
            state.entries.pop_front();
            state.first += 1;
        }
        state.first + u64::try_from(state.entries.len()).unwrap_or(u64::MAX) - 1
    }

    /// Records that submission `sequence` finished at `at`; the queue completes in order, so all
    /// earlier ones have too.
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

    /// Each kept submission's GPU time, oldest first: finished ones in full, the oldest unfinished
    /// up to `now`, later unfinished ones not at all.
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

    /// The kept submission with the longest GPU time, counting a running one up to `now`.
    pub(crate) fn longest_at(&self, now: Instant) -> Option<SubmissionReport> {
        let state = self.state();
        Self::spans(&state, now)
            .into_iter()
            .max_by(|(a, _), (b, _)| a.total_cmp(b))
            .map(|(gpu_ms, entry)| SubmissionReport {
                gpu_ms,
                frame: entry.frame,
                effects: entry.effects.clone(),
                predicted_ms: entry.predicted_ms,
            })
    }

    /// Summed GPU time of `sequences` once all finished and are still kept. Late completions shift
    /// time between consecutive submissions but not out of the sum, so a frame's total holds.
    pub(crate) fn gpu_ms(&self, sequences: std::ops::Range<u64>) -> Option<f64> {
        let state = self.state();
        let first = state.first;
        let spans = Self::spans(&state, Instant::now());
        let mut total = 0.0;
        let mut counted = 0;
        for (position, (ms, entry)) in spans.into_iter().enumerate() {
            let sequence = first + u64::try_from(position).unwrap_or(u64::MAX);
            if sequences.contains(&sequence) {
                entry.done_at?;
                total += ms;
                counted += 1;
            }
        }
        (counted == sequences.end.saturating_sub(sequences.start)).then_some(total)
    }

    /// [`Self::longest_at`] now.
    pub(crate) fn longest(&self) -> Option<SubmissionReport> {
        self.longest_at(Instant::now())
    }

    /// The longest recent submission if it ran at least [`alpha2_gpu::hung_threshold_ms`], which
    /// implicates this renderer's own work in a loss.
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

    /// Submissions made at once run back to back; each starts where the previous ended.
    #[test]
    fn gpu_time_excludes_queueing_behind_earlier_submissions() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let first = clock.record(at(0), None);
        let second = clock.record(at(0), None);
        let third = clock.record(at(0), None);
        clock.complete(first, at(10));
        clock.complete(second, at(400));
        clock.complete(third, at(405));
        let longest = clock.longest_at(at(1_000)).expect("submissions");
        assert!((longest.gpu_ms - 390.0).abs() < 0.5, "{longest:?}");
        // Submitted after the queue went idle, so it starts at its own submit.
        let fourth = clock.record(at(2_000), None);
        clock.complete(fourth, at(2_005));
        assert!((clock.longest_at(at(3_000)).expect("kept").gpu_ms - 390.0).abs() < 0.5);
        let _ = clock.record(at(3_000), None);
        assert!((clock.longest_at(at(3_700)).expect("running").gpu_ms - 700.0).abs() < 0.5);
    }

    #[test]
    fn a_completion_finishes_every_earlier_submission() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let first = clock.record(start, None);
        let second = clock.record(start, None);
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
    fn a_frames_gpu_time_survives_completions_seen_together() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let earlier = clock.record(at(0), None);
        clock.complete(earlier, at(5));
        let first = clock.record(at(10), Some(100.0));
        let second = clock.record(at(10), Some(100.0));
        assert_eq!(clock.gpu_ms(first..second + 1), None, "not finished");
        // Both seen at 210 ms: the first takes the whole span, and the sum is unchanged.
        clock.complete(second, at(210));
        let total = clock.gpu_ms(first..second + 1).expect("finished");
        assert!((total - 200.0).abs() < 0.5, "{total}");
        assert_eq!(
            clock
                .longest_at(at(300))
                .and_then(|report| report.predicted_ms),
            Some(100.0)
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
        let heavy = clock.record(start, None);
        clock.complete(heavy, start + Duration::from_millis(30));
        let light = clock.record(start + Duration::from_millis(40), None);
        clock.complete(light, start + Duration::from_millis(41));
        let report = clock
            .longest_at(start + Duration::from_millis(50))
            .expect("kept");
        assert_eq!(report.effects, ["blur@2"]);
        assert_eq!(report.frame.and_then(|frame| frame.frame_index), Some(1));
    }
}
