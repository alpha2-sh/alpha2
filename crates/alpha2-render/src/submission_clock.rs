//! How long recent renderer submissions kept the GPU busy.
//!
//! Windows resets a GPU whose submission exceeds its timeout, and a reset device's DX12 fence
//! reads complete, so the loss looks like dropped work. The submissions' run times tell the two
//! apart. Each submission's GPU time runs from the later of its submit and the previous
//! submission's end to its own completion callback, so queueing is excluded. Callback latency
//! (poll or event loop) is small beside [`alpha2_gpu::hung_threshold_ms`]. Completions one poll
//! delivers together tell only their total, so such submissions are judged as one group.

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

/// The recent submission, or group of submissions completed together, that kept the GPU busy
/// longest, and what it rendered.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionReport {
    /// GPU time in milliseconds of all `submissions` together; for one still running, the time so
    /// far.
    pub gpu_ms: f64,
    /// Whether `gpu_ms` is measured: its start and its last completion were each seen as they
    /// happened. Otherwise a completion was seen late and `gpu_ms` is an upper bound.
    pub measured: bool,
    /// Submissions whose completions were seen together, so only their total is known; 1 for a
    /// submission timed on its own.
    pub submissions: u32,
    /// The frame the first of them rendered, when the caller marked one.
    pub frame: Option<FrameMark>,
    /// The effects they ran, as `id@version`.
    pub effects: Vec<String>,
    /// Their predicted GPU time in milliseconds, when their frames were planned.
    pub predicted_ms: Option<f64>,
}

#[derive(Copy, Clone, Debug)]
struct Stamp {
    at: Instant,
    seen_as_it_happened: bool,
    recorded_when_seen: u64,
}

impl Stamp {
    fn seen_with(&self, other: &Self) -> bool {
        !self.seen_as_it_happened && self.recorded_when_seen == other.recorded_when_seen
    }
}

#[derive(Debug)]
struct Entry {
    submitted_at: Instant,
    done: Option<Stamp>,
    frame: Option<FrameMark>,
    effects: Vec<String>,
    predicted_ms: Option<f64>,
}

#[derive(Copy, Clone, Debug, Default)]
enum Before {
    #[default]
    Nothing,
    Completed(Stamp),
    Running,
}

impl Before {
    fn start_of(self, entry: &Entry) -> (Instant, bool) {
        match self {
            Self::Completed(stamp) if stamp.at > entry.submitted_at => {
                (stamp.at, stamp.seen_as_it_happened)
            }
            Self::Nothing | Self::Completed(_) => (entry.submitted_at, true),
            Self::Running => (entry.submitted_at, false),
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct Group {
    first: usize,
    len: usize,
    gpu_ms: f64,
    measured: bool,
}

#[derive(Debug, Default)]
struct ClockState {
    /// Sequence number of `entries[0]`.
    first: u64,
    forgotten: Before,
    recorded: u64,
    completed: u64,
    watched: Vec<(u64, PreviousCompletion)>,
    entries: VecDeque<Entry>,
    frame: Option<FrameMark>,
}

impl ClockState {
    fn group_at(&self, first: usize) -> Option<(usize, Stamp)> {
        let mut last = first;
        let mut end = self.entries.get(first)?.done?;
        while let Some(next) = self.entries.get(last + 1).and_then(|entry| entry.done)
            && end.seen_with(&next)
        {
            last += 1;
            end = next;
        }
        Some((last, end))
    }

    fn groups(&self, now: Instant) -> Vec<Group> {
        let mut groups = Vec::new();
        let mut before = self.forgotten;
        let mut first = 0;
        while let Some(entry) = self.entries.get(first) {
            let (start, start_seen) = before.start_of(entry);
            let Some((last, end)) = self.group_at(first) else {
                groups.push(Group {
                    first,
                    len: 1,
                    gpu_ms: milliseconds(now.saturating_duration_since(start)),
                    measured: false,
                });
                break;
            };
            groups.push(Group {
                first,
                len: last - first + 1,
                gpu_ms: milliseconds(end.at.saturating_duration_since(start)),
                measured: start_seen && end.seen_as_it_happened,
            });
            before = Before::Completed(end);
            first = last + 1;
        }
        groups
    }

    fn members(&self, group: Group) -> impl Iterator<Item = &Entry> {
        self.entries.range(group.first..group.first + group.len)
    }

    fn predicted_ms(&self, group: Group) -> Option<f64> {
        self.members(group)
            .filter_map(|entry| entry.predicted_ms)
            .reduce(|a, b| a + b)
    }

    fn proves_a_hang(&self, group: Group, threshold_ms: f64) -> bool {
        group.gpu_ms >= threshold_ms
            && (group.len == 1
                || group.gpu_ms > alpha2_gpu::MARGIN * self.predicted_ms(group).unwrap_or(0.0))
    }

    fn report(&self, group: Group) -> SubmissionReport {
        let mut effects: Vec<String> = Vec::new();
        for effect in self.members(group).flat_map(|entry| &entry.effects) {
            if !effects.contains(effect) {
                effects.push(effect.clone());
            }
        }
        SubmissionReport {
            gpu_ms: group.gpu_ms,
            measured: group.measured,
            submissions: u32::try_from(group.len).unwrap_or(u32::MAX),
            frame: self.entries[group.first].frame,
            effects,
            predicted_ms: self.predicted_ms(group),
        }
    }

    fn forget_front(&mut self, count: usize) {
        for _ in 0..count {
            let Some(forgotten) = self.entries.pop_front() else {
                return;
            };
            self.forgotten = forgotten.done.map_or(Before::Running, Before::Completed);
            self.first += 1;
        }
    }

    fn forget(&mut self) {
        loop {
            let kept = self.entries.len();
            if kept > MOST_SUBMISSIONS_KEPT {
                self.forget_front(kept - MOST_SUBMISSIONS_KEPT);
                continue;
            }
            match self.group_at(0) {
                Some((last, _)) if kept - (last + 1) >= RECENT_SUBMISSIONS_KEPT => {
                    self.forget_front(last + 1);
                }
                _ => return,
            }
        }
    }
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

    /// Records a submission made at `at` that runs `effects` and returns its sequence number for
    /// [`Self::complete`], also watching for the completion of the submission before it when
    /// `watch_previous` and that is still running.
    pub(crate) fn record(
        &self,
        at: Instant,
        predicted_ms: Option<f64>,
        effects: Vec<String>,
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
        let frame = state.frame;
        state.entries.push_back(Entry {
            submitted_at: at,
            done: None,
            frame,
            effects,
            predicted_ms,
        });
        state.forget();
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
        let recorded_when_seen = state.recorded;
        let last = position.min(state.entries.len().saturating_sub(1));
        for (index, entry) in state.entries.iter_mut().take(position + 1).enumerate() {
            entry.done.get_or_insert(Stamp {
                at,
                seen_as_it_happened: as_it_happens && index == last,
                recorded_when_seen,
            });
        }
    }

    /// Each kept submission's GPU time, oldest first: finished ones in full, the oldest unfinished
    /// up to `now`, later unfinished ones not at all.
    fn spans(state: &ClockState, now: Instant) -> Vec<(f64, &Entry)> {
        let mut spans = Vec::new();
        let mut before = state.forgotten;
        for entry in &state.entries {
            let (start, _) = before.start_of(entry);
            let Some(done) = entry.done else {
                spans.push((milliseconds(now.saturating_duration_since(start)), entry));
                break;
            };
            spans.push((
                milliseconds(done.at.saturating_duration_since(start)),
                entry,
            ));
            before = Before::Completed(done);
        }
        spans
    }

    /// The kept submission or group with the longest GPU time, counting a running one up to
    /// `now`.
    pub(crate) fn longest_at(&self, now: Instant) -> Option<SubmissionReport> {
        let state = self.state();
        state
            .groups(now)
            .into_iter()
            .max_by(|a, b| a.gpu_ms.total_cmp(&b.gpu_ms))
            .map(|group| state.report(group))
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
                entry.done?;
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

    fn hung_at(&self, now: Instant, threshold_ms: f64) -> Option<SubmissionReport> {
        let state = self.state();
        state
            .groups(now)
            .into_iter()
            .filter(|group| state.proves_a_hang(*group, threshold_ms))
            .max_by(|a, b| a.gpu_ms.total_cmp(&b.gpu_ms))
            .map(|group| state.report(group))
    }

    #[cfg(test)]
    pub(crate) fn kept_effects(&self) -> Vec<Vec<String>> {
        self.state()
            .entries
            .iter()
            .map(|entry| entry.effects.clone())
            .collect()
    }

    /// The longest kept submission or group that implicates this renderer's own work in a loss.
    pub(crate) fn hung(&self) -> Option<SubmissionReport> {
        self.hung_at(Instant::now(), f64::from(alpha2_gpu::hung_threshold_ms()))
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
        let first = clock.record(at(0), None, Vec::new(), false).sequence;
        let second = clock.record(at(0), None, Vec::new(), false).sequence;
        let third = clock.record(at(0), None, Vec::new(), false).sequence;
        clock.complete(first, at(10), true);
        clock.complete(second, at(400), true);
        clock.complete(third, at(405), true);
        let longest = clock.longest_at(at(1_000)).expect("submissions");
        assert!((longest.gpu_ms - 390.0).abs() < 0.5, "{longest:?}");
        assert!(longest.measured, "{longest:?}");
        // Submitted after the queue went idle, so it starts at its own submit.
        let fourth = clock.record(at(2_000), None, Vec::new(), false).sequence;
        clock.complete(fourth, at(2_005), false);
        assert!((clock.longest_at(at(3_000)).expect("kept").gpu_ms - 390.0).abs() < 0.5);
        let _ = clock.record(at(3_000), None, Vec::new(), false).sequence;
        assert!((clock.longest_at(at(3_700)).expect("running").gpu_ms - 700.0).abs() < 0.5);
    }

    #[test]
    fn a_completion_finishes_every_earlier_submission() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let first = clock.record(start, None, Vec::new(), false).sequence;
        let second = clock.record(start, None, Vec::new(), false).sequence;
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
        let earlier = clock.record(at(0), None, Vec::new(), false).sequence;
        clock.complete(earlier, at(5), false);
        let first = clock
            .record(at(10), Some(100.0), Vec::new(), false)
            .sequence;
        let second = clock
            .record(at(10), Some(100.0), Vec::new(), false)
            .sequence;
        assert_eq!(clock.gpu_ms(first..second + 1), None, "not finished");
        clock.complete(second, at(210), false);
        let total = clock.gpu_ms(first..second + 1).expect("finished");
        assert!((total - 200.0).abs() < 0.5, "{total}");
        let report = clock.longest_at(at(300)).expect("kept");
        assert_eq!(report.submissions, 2, "{report:?}");
        assert_eq!(report.predicted_ms, Some(200.0));
    }

    #[test]
    fn the_clock_keeps_the_frame_and_effects_of_each_submission() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        clock.mark_frame(Some(FrameMark {
            time_ticks: 4_000,
            frame_index: Some(1),
        }));
        let heavy = clock
            .record(start, None, vec!["blur@2".to_owned()], false)
            .sequence;
        clock.complete(heavy, start + Duration::from_millis(30), false);
        let light = clock
            .record(start + Duration::from_millis(40), None, Vec::new(), false)
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
        let first = clock.record(start, None, Vec::new(), false);
        assert!(first.queue_idle);
        let second = clock.record(start, None, Vec::new(), false);
        assert!(!second.queue_idle);
        clock.complete(first.sequence, start, false);
        assert!(!clock.record(start, None, Vec::new(), false).queue_idle);
        clock.complete(second.sequence + 1, start, false);
        assert!(clock.record(start, None, Vec::new(), false).queue_idle);
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
        let first = clock.record(at(0), None, Vec::new(), false);
        assert_eq!(first.started(at(0)), Some(at(0)));
        let behind = clock.record(at(1), None, Vec::new(), true);
        let late = clock.record(at(2), None, Vec::new(), true);
        assert_eq!(behind.started(at(1)), None, "not complete yet");
        clock.complete(first.sequence, at(10), true);
        clock.complete(behind.sequence, at(30), false);
        assert_eq!(behind.started(at(1)), Some(at(10)));
        assert_eq!(late.started(at(2)), None, "seen late");
        clock.complete(late.sequence, at(35), true);
        assert!(
            clock
                .record(at(40), None, Vec::new(), true)
                .previous
                .is_none()
        );
    }

    #[test]
    fn the_oldest_kept_submission_does_not_count_its_wait_behind_a_forgotten_one() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let submitted = u64::try_from(RECENT_SUBMISSIONS_KEPT).expect("fits") + 9;
        let sequences: Vec<u64> = (0..submitted)
            .map(|_| clock.record(at(0), None, Vec::new(), false).sequence)
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
            let sequence = clock.record(at(0), None, Vec::new(), false).sequence;
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
        let long = clock
            .record(at(0), Some(100.0), vec!["blur@2".to_owned()], false)
            .sequence;
        clock.mark_frame(Some(FrameMark {
            time_ticks: 32_000,
            frame_index: Some(8),
        }));
        let mut last = long;
        for _ in 0..RECENT_SUBMISSIONS_KEPT {
            last = clock.record(at(1), Some(100.0), Vec::new(), false).sequence;
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
        let first = clock.record(at(0), None, Vec::new(), false).sequence;
        let second = clock.record(at(0), None, Vec::new(), false).sequence;
        for _ in 0..RECENT_SUBMISSIONS_KEPT {
            let _ = clock.record(at(1), Some(1.0), Vec::new(), false);
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
            let _ = clock.record(start, None, Vec::new(), false);
        }
        let state = clock.state();
        assert_eq!(state.entries.len(), MOST_SUBMISSIONS_KEPT);
        assert_eq!(state.first, 1);
    }

    #[test]
    fn submissions_completed_together_are_one_group_judged_against_their_prediction() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let sequences: Vec<u64> = (0..10)
            .map(|_| clock.record(at(0), Some(100.0), Vec::new(), false).sequence)
            .collect();
        for sequence in &sequences {
            clock.complete(*sequence, at(1_000), *sequence == sequences[9]);
        }
        let report = clock.longest_at(at(1_001)).expect("kept");
        assert_eq!(report.submissions, 10, "{report:?}");
        assert!((report.gpu_ms - 1_000.0).abs() < 0.5, "{report:?}");
        assert_eq!(report.predicted_ms, Some(1_000.0));
        assert_eq!(clock.hung_at(at(1_001), 1_000.0), None);
        let late: Vec<u64> = (0..10)
            .map(|_| {
                clock
                    .record(at(1_000), Some(100.0), Vec::new(), false)
                    .sequence
            })
            .collect();
        for sequence in &late {
            clock.complete(*sequence, at(3_001), false);
        }
        let hung = clock
            .hung_at(at(3_002), 1_000.0)
            .expect("past twice its prediction");
        assert_eq!(hung.submissions, 10, "{hung:?}");
        assert!((hung.gpu_ms - 2_001.0).abs() < 0.5, "{hung:?}");
    }

    #[test]
    fn a_submission_timed_on_its_own_is_hung_at_the_threshold_whatever_its_prediction() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let alone = clock.record(at(0), Some(900.0), Vec::new(), false).sequence;
        clock.complete(alone, at(1_000), true);
        let hung = clock.hung_at(at(1_001), 1_000.0).expect("hung");
        assert_eq!(hung.submissions, 1, "{hung:?}");
        assert_eq!(clock.hung_at(at(1_001), 1_001.0), None);
    }

    #[test]
    fn submissions_completed_together_without_a_prediction_are_hung_at_the_threshold() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let first = clock.record(at(0), None, Vec::new(), false).sequence;
        let second = clock.record(at(0), None, Vec::new(), false).sequence;
        clock.complete(first, at(1_000), false);
        clock.complete(second, at(1_000), false);
        let hung = clock.hung_at(at(1_001), 1_000.0).expect("hung");
        assert_eq!(hung.submissions, 2, "{hung:?}");
    }

    #[test]
    fn completions_seen_after_a_later_submit_or_as_they_happened_are_not_grouped() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let first = clock.record(at(0), Some(1.0), Vec::new(), false).sequence;
        let second = clock.record(at(0), Some(1.0), Vec::new(), false).sequence;
        clock.complete(first, at(400), true);
        clock.complete(second, at(400), false);
        let third = clock.record(at(0), Some(1.0), Vec::new(), false).sequence;
        let fourth = clock.record(at(401), Some(1.0), Vec::new(), false).sequence;
        clock.complete(third, at(402), false);
        let _ = clock.record(at(403), Some(1.0), Vec::new(), false);
        clock.complete(fourth, at(1_402), false);
        let report = clock.longest_at(at(1_403)).expect("kept");
        assert_eq!(report.submissions, 1, "{report:?}");
        assert!((report.gpu_ms - 1_000.0).abs() < 0.5, "{report:?}");
        assert!(clock.hung_at(at(1_403), 1_000.0).is_some());
    }

    #[test]
    fn a_group_of_completions_is_forgotten_whole_once_enough_later_submissions_are_kept() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let together = RECENT_SUBMISSIONS_KEPT + 44;
        let sequences: Vec<u64> = (0..together)
            .map(|_| clock.record(at(0), Some(1.0), Vec::new(), false).sequence)
            .collect();
        for sequence in &sequences {
            clock.complete(*sequence, at(5_000), false);
        }
        for later in 1..RECENT_SUBMISSIONS_KEPT {
            let sequence = clock.record(at(5_000), None, Vec::new(), false).sequence;
            clock.complete(sequence, at(5_000), true);
            assert_eq!(clock.state().entries.len(), together + later);
        }
        let report = clock.longest_at(at(5_001)).expect("kept");
        assert_eq!(
            report.submissions,
            u32::try_from(together).expect("fits"),
            "{report:?}"
        );
        let _ = clock.record(at(5_000), None, Vec::new(), false);
        assert_eq!(clock.state().entries.len(), RECENT_SUBMISSIONS_KEPT);
    }

    #[test]
    fn a_time_is_measured_only_between_completions_seen_as_they_happened() {
        let clock = SubmissionClock::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let seen = clock.record(at(0), None, Vec::new(), false).sequence;
        let late = clock.record(at(0), None, Vec::new(), false).sequence;
        clock.complete(seen, at(100), true);
        clock.complete(late, at(300), false);
        let late_report = clock.longest_at(at(301)).expect("kept");
        assert!(!late_report.measured, "{late_report:?}");
        assert!((late_report.gpu_ms - 200.0).abs() < 0.5, "{late_report:?}");
        let after_late = clock.record(at(250), None, Vec::new(), false).sequence;
        clock.complete(after_late, at(700), true);
        let report = clock.longest_at(at(701)).expect("kept");
        assert!((report.gpu_ms - 400.0).abs() < 0.5, "{report:?}");
        assert!(
            !report.measured,
            "it starts at a completion seen late: {report:?}"
        );
        let idle = clock.record(at(800), None, Vec::new(), false).sequence;
        clock.complete(idle, at(1_400), true);
        let report = clock.longest_at(at(1_401)).expect("kept");
        assert!((report.gpu_ms - 600.0).abs() < 0.5, "{report:?}");
        assert!(report.measured, "{report:?}");
    }
}
