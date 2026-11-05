//! Bounded recovery shared by software and hardware-backed native decoders.
use alpha2_media::{CancelToken, DecodeError};
use std::time::Duration;

/// Damaged input handling. Initialization and resource errors are always fatal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecodePolicy {
    /// Fail on the first reported corruption.
    #[default]
    Strict,
    /// Skip damaged input, retaining diagnostics; fail when recovery is unbounded.
    Tolerant,
}

impl DecodePolicy {
    /// The policy's name as `ALPHA2_DECODE_POLICY` and `--decode-policy` spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Tolerant => "tolerant",
        }
    }

    /// The policy `value` names ([`Self::as_str`]); any other value is none.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Strict, Self::Tolerant]
            .into_iter()
            .find(|policy| policy.as_str() == value)
    }

    /// The policy `ALPHA2_DECODE_POLICY` selects; Strict when unset.
    ///
    /// # Errors
    /// Any other value, including non-Unicode, is an error rather than a silent fallback.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("ALPHA2_DECODE_POLICY") {
            Err(std::env::VarError::NotPresent) => Some(Self::Strict),
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotUnicode(_)) => None,
        }
        .ok_or_else(|| "ALPHA2_DECODE_POLICY must be strict or tolerant".to_owned())
    }
}

/// Counts of what decoders reported: damage Tolerant decoding skipped and silence it inserted.
/// Samples are at the reporting decoder's output rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeDiagnosticCounts {
    /// Corruption observations; one damaged packet can be observed at more than one stage.
    pub corrupt_packets: u64,
    /// Runs of dropped video between healthy frames, an unrecovered tail included.
    pub video_gaps: u64,
    /// Audio samples decoded as silence in place of damaged input.
    pub concealed_audio_samples: u64,
    /// Audio samples decoded as silence where a clean stream's timestamps jump.
    pub timestamp_gap_samples: u64,
    /// Audio samples dropped because their timestamps repeated samples already decoded.
    pub dropped_audio_samples: u64,
}

impl DecodeDiagnosticCounts {
    /// Counts one report.
    pub fn record(&mut self, diagnostic: &DecodeDiagnostic) {
        let samples = |start: i64, end: i64| u64::try_from(end.saturating_sub(start)).unwrap_or(0);
        match diagnostic {
            DecodeDiagnostic::Corruption(_) => {
                self.corrupt_packets = self.corrupt_packets.saturating_add(1);
            }
            DecodeDiagnostic::VideoGap(_) => self.video_gaps = self.video_gaps.saturating_add(1),
            DecodeDiagnostic::AudioGap(gap) => {
                let count = samples(gap.start_sample, gap.end_sample);
                let total = if gap.reason == "timestamp gap" {
                    &mut self.timestamp_gap_samples
                } else {
                    &mut self.concealed_audio_samples
                };
                *total = total.saturating_add(count);
            }
            DecodeDiagnostic::AudioOverlap(overlap) => {
                self.dropped_audio_samples = self
                    .dropped_audio_samples
                    .saturating_add(samples(overlap.start_sample, overlap.end_sample));
            }
        }
    }

    /// Whether nothing was reported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One observed corruption. Timestamps use the selected stream's time base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorruptPacket {
    /// Pipeline stage reporting damage (may be a frame or demux error).
    pub stage: &'static str,
    /// Best available timestamp, absent when `FFmpeg` cannot identify it.
    pub pts: Option<i64>,
    /// Numerator and denominator of the timestamp time base.
    pub time_base: (i32, i32),
}

const MAX_AUDIO_RECORDS: usize = 1024;

/// An interval decoded as silence at the decoder's output rate, where damaged input was
/// dropped or timestamps jump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcealedAudioGap {
    /// Inclusive source sample position.
    pub start_sample: i64,
    /// Exclusive source sample position.
    pub end_sample: i64,
    /// Output samples per second.
    pub sample_rate: u32,
    /// Why silence was inserted: `"corrupt input"` or `"timestamp gap"`.
    pub reason: &'static str,
}

/// Samples dropped because their timestamps overlapped samples already decoded, at
/// the decoder's output rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedAudioOverlap {
    /// Inclusive source sample position.
    pub start_sample: i64,
    /// Exclusive source sample position.
    pub end_sample: i64,
    /// Output samples per second.
    pub sample_rate: u32,
}

/// Healthy video boundaries surrounding dropped data. Unknown endpoints remain absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoRecoveryGap {
    /// Last healthy frame before damage; may be held by a containing-frame reader.
    pub previous_pts: Option<i64>,
    /// First healthy frame after damage, absent at EOF or seek reset.
    pub recovered_pts: Option<i64>,
    /// Numerator and denominator of both timestamps' time base.
    pub time_base: (i32, i32),
}

/// Cumulative diagnostics. Records are bounded; overflow fails decoding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecodeDiagnostics {
    /// Corruption observations, not necessarily distinct encoded packets.
    pub corrupt_packets: Vec<CorruptPacket>,
    /// Concealed audio intervals with original source positions.
    pub audio_gaps: Vec<ConcealedAudioGap>,
    /// Repeated audio intervals dropped, with original source positions.
    pub audio_overlaps: Vec<DroppedAudioOverlap>,
    /// Healthy boundaries around dropped video data; not a guessed CFR interval.
    pub video_gaps: Vec<VideoRecoveryGap>,
}

/// A recovery observation emitted synchronously before decoding continues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeDiagnostic {
    /// Damage reported by the demuxer or decoder.
    Corruption(CorruptPacket),
    /// Explicitly inserted silence at source sample positions.
    AudioGap(ConcealedAudioGap),
    /// Repeated samples dropped at source sample positions.
    AudioOverlap(DroppedAudioOverlap),
    /// Video recovery boundaries without rewriting frame PTS.
    VideoGap(VideoRecoveryGap),
}

/// How a backend classifies a failed decode call for [`Recovery::counts_as_damage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeFailure {
    /// The decoder reported corrupt input.
    InvalidData,
    /// A hardware session could not decode a picture. Hardware decoders report damage this way,
    /// never as corrupt input.
    HardwarePicture,
    /// Anything else: resource, configuration or internal errors.
    Other,
}

/// Corruption, timestamp and progress budgets for one decoder, with the diagnostics it collected.
#[derive(Default)]
pub struct Recovery {
    /// How damaged input is handled.
    pub policy: DecodePolicy,
    /// Receives each diagnostic as it is recorded.
    pub reporter: Option<Box<dyn Fn(DecodeDiagnostic) + Send + Sync>>,
    /// Everything recorded so far.
    pub diagnostics: DecodeDiagnostics,
    consecutive: usize,
    last_pts: Option<i64>,
    stalled: usize,
    video_damage: bool,
    time_base: (i32, i32),
    healthy_frames: usize,
    /// The decoder is a hardware session (see [`Self::error`]).
    hardware: bool,
}
impl Recovery {
    /// Recovery for a software decoder whose timestamps count `time_base` = `(num, den)` seconds.
    #[must_use]
    pub fn new(time_base: (i32, i32)) -> Self {
        Self {
            time_base,
            ..Self::default()
        }
    }
    /// Recovery for a hardware decode session.
    #[must_use]
    pub fn hardware(time_base: (i32, i32)) -> Self {
        Self {
            hardware: true,
            ..Self::new(time_base)
        }
    }
    /// Closes an open video recovery gap at `recovered_pts`.
    pub fn finish_video_gap(&mut self, recovered_pts: Option<i64>) {
        if !self.video_damage {
            return;
        }
        self.video_damage = false;
        let gap = VideoRecoveryGap {
            previous_pts: self.last_pts,
            recovered_pts,
            time_base: self.time_base,
        };
        self.diagnostics.video_gaps.push(gap.clone());
        if let Some(report) = &self.reporter {
            report(DecodeDiagnostic::VideoGap(gap));
        }
    }

    /// Whether a failed decode call counts as damaged input, which [`Self::corrupt`] then records,
    /// rather than a fatal error. Hardware sessions' picture failures count only under
    /// [`DecodePolicy::Tolerant`]; Strict fails them so a host can retry in software.
    #[must_use]
    pub fn counts_as_damage(&self, failure: DecodeFailure) -> bool {
        match failure {
            DecodeFailure::InvalidData => true,
            DecodeFailure::HardwarePicture => {
                self.hardware && self.policy == DecodePolicy::Tolerant
            }
            DecodeFailure::Other => false,
        }
    }
    /// Records damaged input at `stage` and `pts`, failing under Strict or past a budget.
    ///
    /// # Errors
    /// Strict policy, the diagnostic budget, or too many consecutive corruptions.
    ///
    /// # Panics
    /// Never: the record it reports was just pushed.
    pub fn corrupt(&mut self, stage: &'static str, pts: Option<i64>) -> Result<(), DecodeError> {
        if self.diagnostics.corrupt_packets.len() >= 1024 {
            return Err(DecodeError::Adapter(
                "corruption diagnostic budget exceeded".into(),
            ));
        }
        self.diagnostics.corrupt_packets.push(CorruptPacket {
            stage,
            pts,
            time_base: self.time_base,
        });
        if let Some(report) = &self.reporter {
            report(DecodeDiagnostic::Corruption(
                self.diagnostics.corrupt_packets.last().unwrap().clone(),
            ));
        }
        self.video_damage |= stage.starts_with("video");
        self.consecutive += 1;
        if self.policy == DecodePolicy::Strict {
            return Err(DecodeError::Adapter(format!(
                "corrupt input at {stage} (strict decode policy)"
            )));
        }
        if self.consecutive > 32 {
            return Err(DecodeError::Adapter(
                "consecutive corruption budget exceeded".into(),
            ));
        }
        Ok(())
    }
    /// The video stream ended.
    ///
    /// # Errors
    /// The stream ended in damage with no healthy frame since the open or seek.
    pub fn video_eof(&mut self) -> Result<(), DecodeError> {
        self.finish_video_gap(None);
        if self.consecutive != 0 {
            self.damaged_tail("video")?;
        }
        Ok(())
    }
    /// The stream ended in damage. It ends at its last healthy frame, unless none was decoded since
    /// the open or seek, since damaged input cannot succeed with empty output.
    ///
    /// # Errors
    /// No healthy frame since the open or seek.
    pub fn damaged_tail(&self, kind: &str) -> Result<(), DecodeError> {
        if self.healthy_frames == 0 {
            return Err(DecodeError::Adapter(format!(
                "damaged {kind} has no recoverable frames"
            )));
        }
        Ok(())
    }
    /// A healthy frame decoded at `pts`, which is `None` when the frame has no timestamp.
    ///
    /// # Errors
    /// Timestamps stopped advancing for too many frames.
    pub fn progress(&mut self, pts: Option<i64>) -> Result<(), DecodeError> {
        self.healthy_frames = self.healthy_frames.saturating_add(1);
        self.finish_video_gap(pts);
        self.consecutive = 0;
        if let Some(pts) = pts
            && self.last_pts.is_none_or(|previous| pts > previous)
        {
            self.last_pts = Some(pts);
            self.stalled = 0;
        } else {
            self.stalled += 1;
        }
        if self.stalled > 256 {
            return Err(DecodeError::Adapter(
                "decoder PTS progress budget exceeded".into(),
            ));
        }
        Ok(())
    }
    /// Forgets progress after a seek or reopen.
    pub fn reset_progress(&mut self) {
        self.finish_video_gap(None);
        self.last_pts = None;
        self.healthy_frames = 0;
        self.stalled = 0;
        self.consecutive = 0;
    }
    /// Audio concealed over damage from `start_sample` to `end_sample`.
    ///
    /// # Errors
    /// The audio gap record budget is spent.
    pub fn audio_gap(
        &mut self,
        start_sample: i64,
        end_sample: i64,
        sample_rate: u32,
        reason: &'static str,
    ) -> Result<(), DecodeError> {
        if self.diagnostics.audio_gaps.len() >= MAX_AUDIO_RECORDS {
            return Err(DecodeError::Adapter(
                "audio gap diagnostic budget exceeded".into(),
            ));
        }
        self.timestamp_audio_gap(start_sample, end_sample, sample_rate, reason);
        Ok(())
    }
    /// A gap the stream's own timestamps leave: not damage, so never fatal. Past the record budget
    /// it is reported but not recorded.
    pub fn timestamp_gap(&mut self, start_sample: i64, end_sample: i64, sample_rate: u32) {
        self.timestamp_audio_gap(start_sample, end_sample, sample_rate, "timestamp gap");
    }
    fn timestamp_audio_gap(
        &mut self,
        start_sample: i64,
        end_sample: i64,
        sample_rate: u32,
        reason: &'static str,
    ) {
        let gap = ConcealedAudioGap {
            start_sample,
            end_sample,
            sample_rate,
            reason,
        };
        if let Some(report) = &self.reporter {
            report(DecodeDiagnostic::AudioGap(gap.clone()));
        }
        if self.diagnostics.audio_gaps.len() < MAX_AUDIO_RECORDS {
            self.diagnostics.audio_gaps.push(gap);
        }
    }
    /// Samples a frame repeated over ones already placed, which were dropped.
    pub fn timestamp_overlap(&mut self, start_sample: i64, end_sample: i64, sample_rate: u32) {
        let overlap = DroppedAudioOverlap {
            start_sample,
            end_sample,
            sample_rate,
        };
        if let Some(report) = &self.reporter {
            report(DecodeDiagnostic::AudioOverlap(overlap.clone()));
        }
        if self.diagnostics.audio_overlaps.len() < MAX_AUDIO_RECORDS {
            self.diagnostics.audio_overlaps.push(overlap);
        }
    }
}

/// The longest a decoder operation (seek, next frame, open) may go without producing a frame before
/// failing with "decode progress budget exceeded".
///
/// Time blocked inside one `FFmpeg` call is not counted, since such a call cannot
/// be stopped; a stall watcher allows this plus its own GPU wait limit from the
/// last [`watch_decode_progress`] report.
pub const DECODE_WORK_LIMIT: Duration = Duration::from_secs(10);

/// The most loop steps a decoder operation takes without producing a frame.
const DECODE_WORK_STEPS: usize = 65_536;

thread_local! {
    static DECODE_PROGRESS_WATCH: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    /// [`DECODE_WORK_LIMIT`] as a test on this thread sets it, else
    /// `ALPHA2_TEST_DECODE_WORK_LIMIT_MS` (longer under valgrind). Test builds only.
    pub static WORK_LIMIT: std::cell::Cell<Duration> =
        std::cell::Cell::new(test_work_limit(std::env::var("ALPHA2_TEST_DECODE_WORK_LIMIT_MS").ok().as_deref()));
    /// [`DECODE_WORK_STEPS`] as a test on this thread sets it.
    pub static WORK_STEPS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(DECODE_WORK_STEPS) };
    /// Delay a seek on this thread takes after each decoded frame, slept or added to
    /// [`VIRTUAL_CLOCK`] when set.
    pub static FRAME_DELAY: std::cell::Cell<Duration> =
        const { std::cell::Cell::new(Duration::ZERO) };
    /// When set, the clock work budgets on this thread measure, advanced only by [`FRAME_DELAY`],
    /// so tests do not depend on real decode speed.
    pub static VIRTUAL_CLOCK: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// The test builds' default [`WORK_LIMIT`]: `setting` milliseconds when positive.
#[cfg(any(test, feature = "test-hooks"))]
fn test_work_limit(setting: Option<&str>) -> Duration {
    setting
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map_or(DECODE_WORK_LIMIT, Duration::from_millis)
}

/// Takes [`FRAME_DELAY`] after a decoded frame. Test builds only.
#[cfg(any(test, feature = "test-hooks"))]
pub fn take_frame_delay() {
    let delay = FRAME_DELAY.with(std::cell::Cell::get);
    match VIRTUAL_CLOCK.with(std::cell::Cell::get) {
        Some(now) => VIRTUAL_CLOCK.with(|clock| clock.set(Some(now.saturating_add(delay)))),
        None => std::thread::sleep(delay),
    }
}

/// Calls `progressed` on this thread each time a decoder operation produces a frame or a packet
/// copy plan reads a source packet, until the guard drops.
///
/// Lets an outside stall watcher see a long seek as alive rather than blocked.
#[must_use = "the watch ends when the guard drops"]
pub fn watch_decode_progress(progressed: impl Fn() + 'static) -> DecodeProgressWatch {
    let previous = DECODE_PROGRESS_WATCH.with(|watch| watch.replace(Some(Box::new(progressed))));
    DecodeProgressWatch {
        previous,
        thread: std::marker::PhantomData,
    }
}

/// Tells this thread's [`watch_decode_progress`] watch, if any, that the current operation
/// produced a frame or packet.
pub fn report_decode_progress() {
    DECODE_PROGRESS_WATCH.with(|watch| {
        if let Some(progressed) = watch.borrow().as_ref() {
            progressed();
        }
    });
}

/// Ends a [`watch_decode_progress`] watch, restoring the one before it.
pub struct DecodeProgressWatch {
    previous: Option<Box<dyn Fn()>>,
    thread: std::marker::PhantomData<*const ()>,
}

impl Drop for DecodeProgressWatch {
    fn drop(&mut self) {
        let previous = self.previous.take();
        DECODE_PROGRESS_WATCH.with(|watch| {
            watch.replace(previous);
        });
    }
}

/// Time and loop steps one decoder operation has spent since it started or last produced a frame,
/// checked against [`DECODE_WORK_LIMIT`] and [`DECODE_WORK_STEPS`].
pub struct WorkBudget {
    /// On the awake clock, so system sleep does not count.
    started: alpha2_os::AwakeInstant,
    /// Where [`VIRTUAL_CLOCK`] stood at `started`, when it is set.
    #[cfg(any(test, feature = "test-hooks"))]
    started_virtual: Option<Duration>,
    steps: usize,
}
impl Default for WorkBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkBudget {
    /// A budget starting now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: alpha2_os::AwakeInstant::now(),
            #[cfg(any(test, feature = "test-hooks"))]
            started_virtual: VIRTUAL_CLOCK.with(std::cell::Cell::get),
            steps: 0,
        }
    }

    fn elapsed(&self) -> Duration {
        #[cfg(any(test, feature = "test-hooks"))]
        if let (Some(now), Some(started)) = (
            VIRTUAL_CLOCK.with(std::cell::Cell::get),
            self.started_virtual,
        ) {
            return now.saturating_sub(started);
        }
        self.started.elapsed()
    }
    /// One loop step of the operation.
    ///
    /// # Errors
    /// Cancellation, or the operation's time or step limit is spent.
    pub fn step(&mut self, cancel: &CancelToken) -> Result<(), DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        self.steps += 1;
        #[cfg(any(test, feature = "test-hooks"))]
        let (limit, steps) = (
            WORK_LIMIT.with(std::cell::Cell::get),
            WORK_STEPS.with(std::cell::Cell::get),
        );
        #[cfg(not(any(test, feature = "test-hooks")))]
        let (limit, steps) = (DECODE_WORK_LIMIT, DECODE_WORK_STEPS);
        if self.steps > steps || self.elapsed() > limit {
            return Err(DecodeError::Adapter(
                "decode progress budget exceeded".into(),
            ));
        }
        Ok(())
    }
    /// The operation produced a frame: its limits start again from now.
    pub fn progressed(&mut self) {
        self.started = alpha2_os::AwakeInstant::now();
        #[cfg(any(test, feature = "test-hooks"))]
        {
            self.started_virtual = VIRTUAL_CLOCK.with(std::cell::Cell::get);
        }
        self.steps = 0;
        report_decode_progress();
    }
}

/// The closure refers to the same packet until send accepts it: `true` requests receive/drain,
/// `false` send; its result means progress.
///
/// # Errors
/// The step's error, cancellation, a spent budget, or no progress.
pub fn submit_packet(
    mut step: impl FnMut(bool) -> Result<bool, DecodeError>,
    budget: &mut WorkBudget,
    cancel: &CancelToken,
) -> Result<(), DecodeError> {
    for _ in 0..64 {
        budget.step(cancel)?;
        if step(false)? {
            return Ok(());
        }
        budget.step(cancel)?;
        if !step(true)? {
            return Err(DecodeError::Adapter(
                "decoder send/receive made no progress".into(),
            ));
        }
    }
    Err(DecodeError::Adapter("packet retry budget exceeded".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_is_named_as_the_host_names_it() {
        for policy in [DecodePolicy::Strict, DecodePolicy::Tolerant] {
            assert_eq!(DecodePolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(DecodePolicy::Tolerant.as_str(), "tolerant");
        assert_eq!(DecodePolicy::parse("Tolerant"), None);
        assert_eq!(DecodePolicy::parse(""), None);
    }

    #[test]
    fn counts_tell_concealed_damage_from_a_timestamp_gap() {
        let mut counts = DecodeDiagnosticCounts::default();
        assert!(counts.is_empty());
        let gap = |start_sample, end_sample, reason| {
            DecodeDiagnostic::AudioGap(ConcealedAudioGap {
                start_sample,
                end_sample,
                sample_rate: 48_000,
                reason,
            })
        };
        for diagnostic in [
            DecodeDiagnostic::Corruption(CorruptPacket {
                stage: "audio send",
                pts: None,
                time_base: (1, 48_000),
            }),
            gap(1024, 2048, "corrupt input"),
            gap(4096, 4196, "timestamp gap"),
            DecodeDiagnostic::AudioOverlap(DroppedAudioOverlap {
                start_sample: 10,
                end_sample: 15,
                sample_rate: 48_000,
            }),
            DecodeDiagnostic::VideoGap(VideoRecoveryGap {
                previous_pts: Some(1),
                recovered_pts: None,
                time_base: (1, 10),
            }),
        ] {
            counts.record(&diagnostic);
        }
        assert_eq!(
            counts,
            DecodeDiagnosticCounts {
                corrupt_packets: 1,
                video_gaps: 1,
                concealed_audio_samples: 1024,
                timestamp_gap_samples: 100,
                dropped_audio_samples: 5,
            }
        );
        assert!(!counts.is_empty());
    }
    /// Under Tolerant a hardware session's undecodable picture counts as damage, since hardware
    /// sessions never report corrupt input.
    #[test]
    fn a_hardware_sessions_picture_failure_is_damage_only_when_tolerant() {
        let mut tolerant = Recovery::hardware((1, 30));
        tolerant.policy = DecodePolicy::Tolerant;
        assert!(tolerant.counts_as_damage(DecodeFailure::HardwarePicture));
        assert!(!Recovery::hardware((1, 30)).counts_as_damage(DecodeFailure::HardwarePicture));
        let mut software = Recovery::new((1, 30));
        software.policy = DecodePolicy::Tolerant;
        assert!(!software.counts_as_damage(DecodeFailure::HardwarePicture));
        assert!(software.counts_as_damage(DecodeFailure::InvalidData));
        assert!(!software.counts_as_damage(DecodeFailure::Other));
    }

    #[test]
    fn strict_tolerant_and_fatal_errors() {
        let mut state = Recovery::default();
        assert!(state.corrupt("send", Some(12)).is_err());
        state.policy = DecodePolicy::Tolerant;
        state.reset_progress();
        assert!(state.counts_as_damage(DecodeFailure::InvalidData));
        assert!(state.corrupt("send", Some(13)).is_ok());
        assert_eq!(state.diagnostics.corrupt_packets.len(), 2);
        for _ in 0..31 {
            state.corrupt("receive", None).unwrap();
        }
        assert!(state.corrupt("receive", None).is_err());
    }
    #[test]
    fn progress_and_cancellation_are_bounded() {
        let mut state = Recovery::default();
        state.progress(Some(10)).unwrap();
        for _ in 0..256 {
            state.progress(Some(10)).unwrap();
        }
        assert!(state.progress(Some(10)).is_err());
        state.reset_progress();
        state.progress(Some(0)).unwrap();
        let cancel = CancelToken::default();
        let mut budget = WorkBudget::new();
        for _ in 0..65_536 {
            budget.step(&cancel).unwrap();
        }
        assert!(budget.step(&cancel).is_err());
        // The production limit, whatever `ALPHA2_TEST_DECODE_WORK_LIMIT_MS` gives this run.
        WORK_LIMIT.with(|limit| limit.set(DECODE_WORK_LIMIT));
        let mut budget = WorkBudget::new();
        budget.started = budget
            .started
            .checked_sub(Duration::from_secs(11))
            .expect("the clock reaches back 11 s");
        assert!(budget.step(&cancel).is_err());
        cancel.cancel();
        assert_eq!(WorkBudget::new().step(&cancel), Err(DecodeError::Cancelled));
    }
    #[test]
    fn eagain_drains_and_retries_the_same_packet_until_accepted() {
        let mut actions = Vec::new();
        let mut sends = 0;
        submit_packet(
            |drain| {
                actions.push(drain);
                if drain {
                    return Ok(true);
                }
                sends += 1;
                Ok(sends == 3)
            },
            &mut WorkBudget::new(),
            &CancelToken::default(),
        )
        .unwrap();
        assert_eq!(actions, [false, true, false, true, false]);
        assert!(
            submit_packet(
                |_| Ok(false),
                &mut WorkBudget::new(),
                &CancelToken::default()
            )
            .is_err()
        );
        assert!(submit_packet(Ok, &mut WorkBudget::new(), &CancelToken::default()).is_err());
        let cancel = CancelToken::default();
        let result = submit_packet(
            |drain| {
                if drain {
                    cancel.cancel();
                }
                Ok(drain)
            },
            &mut WorkBudget::new(),
            &cancel,
        );
        assert_eq!(result, Err(DecodeError::Cancelled));
    }
}
