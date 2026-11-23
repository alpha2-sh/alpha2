//! How long each recent renderer submission kept the GPU busy.
//!
//! Windows resets a GPU whose submission exceeds its timeout, and a reset device's DX12 fence
//! reads complete, so the loss looks like dropped work. The submissions' run times tell the two
//! apart. Each submission's GPU time runs from the later of its submit and the previous
//! submission's end to its own completion callback, so queueing is excluded. Callback latency
//! (poll or event loop) is small beside [`alpha2_gpu::hung_threshold_ms`].

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use web_time::Instant;

const RECENT_SUBMISSIONS_KEPT: usize = 256;
const MOST_SUBMISSIONS_KEPT: usize = 65_536;

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
    last_forgotten_done_at: Option<Instant>,
    recorded: u64,
    completed: u64,
    watched: Vec<(u64, PreviousCompletion)>,
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

    /// Records a submission made at `at` and returns its sequence number for [`Self::complete`],
    /// also watching for the completion of the submission before it when `watch_previous` and
    /// that is still running.
    pub(crate) fn record(
        &self,
        at: Instant,
        predicted_ms: Option<f64>,
        watch_previous: bool,
    ) -> Recorded {
        let mut state = self.state();
        let queue_idle = state.completed == state.recorded;
        let previous = (watch_previous && !queue_idle).then(|| {
            let watch = PreviousCompletion::default();
            let sequence = state.recorded - 1;
            state.watched.push((sequence, watch.clone()));
            watch
        });
        state.recorded += 1;
        let effects = std::mem::take(&mut state.effects);
        let frame = state.frame;
        state.entries.push_back(Entry {
            submitted_at: at,
            done_at: None,
            frame,
            effects,
            predicted_ms,
        });
        while state.entries.len() > MOST_SUBMISSIONS_KEPT
            || (state.entries.len() > RECENT_SUBMISSIONS_KEPT
                && state
                    .entries
                    .front()
                    .is_some_and(|entry| entry.done_at.is_some()))
        {
            let forgotten = state.entries.pop_front();
            state.last_forgotten_done_at = forgotten.and_then(|entry| entry.done_at);
            state.first += 1;
        }
        Recorded {
            sequence: state.recorded - 1,
            queue_idle,
            previous,
        }
    }

    /// Records that submission `sequence` finished at `at`, seen `as_it_happens` or not; the
    /// queue completes in order, so all earlier ones have too.
    pub(crate) fn complete(&self, sequence: u64, at: Instant, as_it_happens: bool) {
        let mut state = self.state();
        state.completed = state.completed.max(sequence + 1);
        state.watched.retain(|(watched, completion)| {
            if *watched > sequence {
                return true;
            }
            completion.set(at, as_it_happens && *watched == sequence);
            false
        });
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
        let mut previous_done = state.last_forgotten_done_at;
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

#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    pub(crate) sequence: u64,
    pub(crate) queue_idle: bool,
    pub(crate) previous: Option<PreviousCompletion>,
}

impl Recorded {
    pub(crate) fn started(&self, at: Instant) -> Option<Instant> {
        if self.queue_idle {
            return Some(at);
        }
        let (done, as_it_happens) = self.previous.as_ref()?.get()?;
        as_it_happens.then(|| done.max(at))
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PreviousCompletion(Arc<Mutex<Option<(Instant, bool)>>>);

impl PreviousCompletion {
    fn set(&self, at: Instant, as_it_happens: bool) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((at, as_it_happens));
    }

    fn get(&self) -> Option<(Instant, bool)> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

thread_local! {
    static WAITING: Cell<Option<(usize, u64)>> = const { Cell::new(None) };
}

impl SubmissionClock {
    fn address(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }

    pub(crate) fn sees_as_it_happens(self: &Arc<Self>, sequence: u64) -> bool {
        WAITING
            .get()
            .is_some_and(|(clock, waited)| clock == self.address() && sequence >= waited)
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct Waiting(Option<(usize, u64)>);

#[cfg(not(target_arch = "wasm32"))]
impl Waiting {
    fn on(clock: usize, sequence: u64) -> Self {
        Self(WAITING.replace(Some((clock, sequence))))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Waiting {
    fn drop(&mut self) {
        WAITING.set(self.0);
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(
    target_arch = "wasm32",
    expect(dead_code, reason = "a browser cannot wait for a submission")
)]
pub(crate) struct ClockedSubmission {
    pub(crate) index: wgpu::SubmissionIndex,
    clock: Arc<SubmissionClock>,
    sequence: u64,
}

impl ClockedSubmission {
    pub(crate) const fn new(
        index: wgpu::SubmissionIndex,
        clock: Arc<SubmissionClock>,
        sequence: u64,
    ) -> Self {
        Self {
            index,
            clock,
            sequence,
        }
    }

    /// # Errors
    /// [`crate::RenderError::GpuTimeout`] past `timeout`, or
    /// [`crate::RenderError::TextureReadback`] when polling fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn wait(
        &self,
        gpu: &alpha2_gpu::GpuContext,
        timeout: std::time::Duration,
    ) -> Result<(), crate::RenderError> {
        gpu.device()
            .poll(wgpu::PollType::Poll)
            .map_err(crate::error::poll_error)?;
        let _waiting = Waiting::on(self.clock.address(), self.sequence);
        gpu.device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(self.index.clone()),
                timeout: Some(timeout),
            })
            .map_err(crate::error::poll_error)?;
        Ok(())
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
        let first = clock.record(at(0), None, false).sequence;
        let second = clock.record(at(0), None, false).sequence;
        let third = clock.record(at(0), None, false).sequence;
        clock.complete(first, at(10), false);
        clock.complete(second, at(400), false);
        clock.complete(third, at(405), false);
        let longest = clock.longest_at(at(1_000)).expect("submissions");
        assert!((longest.gpu_ms - 390.0).abs() < 0.5, "{longest:?}");
        // Submitted after the queue went idle, so it starts at its own submit.
        let fourth = clock.record(at(2_000), None, false).sequence;
        clock.complete(fourth, at(2_005), false);
        assert!((clock.longest_at(at(3_000)).expect("kept").gpu_ms - 390.0).abs() < 0.5);
        let _ = clock.record(at(3_000), None, false).sequence;
        assert!((clock.longest_at(at(3_700)).expect("running").gpu_ms - 700.0).abs() < 0.5);
    }

    #[test]
    fn a_completion_finishes_every_earlier_submission() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let first = clock.record(start, None, false).sequence;
        let second = clock.record(start, None, false).sequence;
        clock.complete(second, start + Duration::from_millis(8), false);
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
        let earlier = clock.record(at(0), None, false).sequence;
        clock.complete(earlier, at(5), false);
        let first = clock.record(at(10), Some(100.0), false).sequence;
        let second = clock.record(at(10), Some(100.0), false).sequence;
        assert_eq!(clock.gpu_ms(first..second + 1), None, "not finished");
        // Both seen at 210 ms: the first takes the whole span, and the sum is unchanged.
        clock.complete(second, at(210), false);
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
        let heavy = clock.record(start, None, false).sequence;
        clock.complete(heavy, start + Duration::from_millis(30), false);
        let light = clock
            .record(start + Duration::from_millis(40), None, false)
            .sequence;
        clock.complete(light, start + Duration::from_millis(41), false);
        let report = clock
            .longest_at(start + Duration::from_millis(50))
            .expect("kept");
        assert_eq!(report.effects, ["blur@2"]);
        assert_eq!(report.frame.and_then(|frame| frame.frame_index), Some(1));
    }

    #[test]
    fn a_submission_starts_on_an_idle_queue_only_once_every_earlier_one_completed() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let first = clock.record(start, None, false);
        assert!(first.queue_idle);
        let second = clock.record(start, None, false);
        assert!(!second.queue_idle);
        clock.complete(first.sequence, start, false);
        assert!(!clock.record(start, None, false).queue_idle);
        clock.complete(second.sequence + 1, start, false);
        assert!(clock.record(start, None, false).queue_idle);
    }

    #[test]
    fn only_completions_inside_a_wait_on_them_or_earlier_ones_are_seen_as_they_happen() {
        let clock = Arc::new(SubmissionClock::default());
        let other = Arc::new(SubmissionClock::default());
        assert!(!clock.sees_as_it_happens(0));
        WAITING.set(Some((clock.address(), 5)));
        assert!(!clock.sees_as_it_happens(4));
        assert!(clock.sees_as_it_happens(5));
        assert!(clock.sees_as_it_happens(6));
        assert!(!other.sees_as_it_happens(6));
        WAITING.set(None);
    }

    #[test]
    fn a_frame_behind_a_completion_seen_as_it_happened_starts_at_that_completion() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let first = clock.record(at(0), None, false);
        assert_eq!(first.started(at(0)), Some(at(0)));
        let behind = clock.record(at(1), None, true);
        let late = clock.record(at(2), None, true);
        assert_eq!(behind.started(at(1)), None, "not complete yet");
        clock.complete(first.sequence, at(10), true);
        clock.complete(behind.sequence, at(30), false);
        assert_eq!(behind.started(at(1)), Some(at(10)));
        assert_eq!(late.started(at(2)), None, "seen late");
        clock.complete(late.sequence, at(35), true);
        assert!(clock.record(at(40), None, true).previous.is_none());
    }

    #[test]
    fn the_oldest_kept_submission_does_not_count_its_wait_behind_a_forgotten_one() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let submitted = u64::try_from(RECENT_SUBMISSIONS_KEPT).expect("fits") + 9;
        let sequences: Vec<u64> = (0..submitted)
            .map(|_| clock.record(at(0), None, false).sequence)
            .collect();
        for (finished, sequence) in (1..).zip(&sequences) {
            clock.complete(*sequence, at(10 * finished), true);
        }
        let longest = clock.longest_at(at(10 * submitted + 1)).expect("kept");
        assert!(
            (longest.gpu_ms - 10.0).abs() < 0.5,
            "every submission ran 10 ms: {longest:?}"
        );
    }

    #[test]
    fn a_submission_kept_after_forgotten_ones_starts_where_the_last_forgotten_one_completed() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let submitted = u64::try_from(RECENT_SUBMISSIONS_KEPT).expect("fits") + 9;
        let mut previous = None;
        for finished in 1..=submitted {
            let sequence = clock.record(at(0), None, false).sequence;
            if let Some(previous) = previous.replace(sequence) {
                clock.complete(previous, at(10 * (finished - 1)), true);
            }
        }
        clock.complete(previous.expect("recorded"), at(10 * submitted), true);
        let longest = clock.longest_at(at(10 * submitted + 1)).expect("kept");
        assert!(
            (longest.gpu_ms - 10.0).abs() < 0.5,
            "every submission ran 10 ms: {longest:?}"
        );
        assert_eq!(clock.state().entries.len(), RECENT_SUBMISSIONS_KEPT);
    }

    #[test]
    fn a_running_submission_is_still_named_however_many_were_submitted_behind_it() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        clock.mark_frame(Some(FrameMark {
            time_ticks: 28_000,
            frame_index: Some(7),
        }));
        clock.note_effect(&crate::EffectId::from_static("blur", 2));
        let long = clock.record(at(0), Some(100.0), false).sequence;
        clock.mark_frame(Some(FrameMark {
            time_ticks: 32_000,
            frame_index: Some(8),
        }));
        let mut last = long;
        for _ in 0..RECENT_SUBMISSIONS_KEPT {
            last = clock.record(at(1), Some(100.0), false).sequence;
        }
        let running = clock.longest_at(at(3_000)).expect("kept");
        assert_eq!(
            running.frame.and_then(|frame| frame.frame_index),
            Some(7),
            "{running:?}"
        );
        for sequence in long..=last {
            clock.complete(sequence, at(3_000), false);
        }
        let report = clock.longest_at(at(3_001)).expect("kept");
        assert_eq!(
            report.frame.and_then(|frame| frame.frame_index),
            Some(7),
            "{report:?}"
        );
        assert_eq!(report.effects, ["blur@2"]);
        assert!((report.gpu_ms - 3_000.0).abs() < 0.5, "{report:?}");
    }

    #[test]
    fn draws_read_after_the_frame_behind_them_are_kept_however_many_submissions_it_makes() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let first = clock.record(at(0), None, false).sequence;
        let second = clock.record(at(0), None, false).sequence;
        for _ in 0..RECENT_SUBMISSIONS_KEPT {
            let _ = clock.record(at(1), Some(1.0), false);
        }
        clock.complete(first, at(5), false);
        clock.complete(second, at(10), false);
        let total = clock.gpu_ms(first..second + 1);
        assert!(
            total.is_some_and(|ms| (ms - 10.0).abs() < 0.5),
            "the draws ran 10 ms: {total:?}"
        );
    }

    #[test]
    fn past_the_memory_cap_the_oldest_submission_is_forgotten_even_while_it_runs() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        for _ in 0..=MOST_SUBMISSIONS_KEPT {
            let _ = clock.record(start, None, false);
        }
        let state = clock.state();
        assert_eq!(state.entries.len(), MOST_SUBMISSIONS_KEPT);
        assert_eq!(state.first, 1);
    }
}
