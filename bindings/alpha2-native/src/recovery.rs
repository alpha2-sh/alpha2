//! Bounded recovery shared by software and hardware-backed native decoders.
use crate::convert::adapter_error;
use alpha2_media::{CancelToken, DecodeError};
use rsmpeg::{error::RsmpegError, ffi};
use std::time::{Duration, Instant};

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
    /// The policy's name, as `ALPHA2_DECODE_POLICY` and the `alpha2` CLI's
    /// `--decode-policy` spell it: `strict` or `tolerant`.
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

    /// The policy `ALPHA2_DECODE_POLICY` selects: Strict when it is unset.
    ///
    /// # Errors
    /// Any other value, including non-Unicode, is an error rather than a
    /// silent fallback.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("ALPHA2_DECODE_POLICY") {
            Err(std::env::VarError::NotPresent) => Some(Self::Strict),
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotUnicode(_)) => None,
        }
        .ok_or_else(|| "ALPHA2_DECODE_POLICY must be strict or tolerant".to_owned())
    }
}

/// What decoders reported, counted: the summary a caller gives of the damage
/// Tolerant decoding skipped and the silence it inserted. Samples are at the
/// reporting decoder's output rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeDiagnosticCounts {
    /// Corruption observations ([`DecodeDiagnostics::corrupt_packets`]): one
    /// damaged packet can be observed at more than one stage.
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

/// The most audio gaps and overlaps one decoder records.
const MAX_AUDIO_RECORDS: usize = 1024;

/// An interval decoded as silence, at the decoder's output sample rate:
/// where damaged input was dropped, or where the stream's timestamps jump.
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

/// Samples whose timestamps put them over samples already decoded, dropped,
/// at the decoder's output sample rate.
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

#[derive(Default)]
pub(crate) struct Recovery {
    pub policy: DecodePolicy,
    pub reporter: Option<Box<dyn Fn(DecodeDiagnostic) + Send + Sync>>,
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
    pub fn new(time_base: rsmpeg::avutil::AVRational) -> Self {
        Self {
            time_base: (time_base.num, time_base.den),
            ..Self::default()
        }
    }
    /// Recovery for a hardware decode session.
    // Only a hardware session, and the tests, open one.
    #[cfg_attr(not(feature = "hwdecode"), allow(dead_code))]
    pub fn hardware(time_base: rsmpeg::avutil::AVRational) -> Self {
        Self {
            hardware: true,
            ..Self::new(time_base)
        }
    }
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

    pub fn error(
        &mut self,
        error: RsmpegError,
        stage: &'static str,
        pts: Option<i64>,
    ) -> Result<(), DecodeError> {
        let damaged = match error.raw_error() {
            Some(ffi::AVERROR_INVALIDDATA) => true,
            // A hardware session reports an undecodable picture with its hwaccel's error, never
            // INVALIDDATA (VideoToolbox: AVERROR_UNKNOWN; Vulkan, D3D12: AVERROR_EXTERNAL).
            // Tolerant skips it up to the corruption limit; Strict fails, which lets a host retry
            // in software.
            Some(ffi::AVERROR_UNKNOWN | ffi::AVERROR_EXTERNAL) => {
                self.hardware && self.policy == DecodePolicy::Tolerant
            }
            _ => false,
        };
        if !damaged {
            return Err(adapter_error(error));
        }
        self.corrupt(stage, pts)
    }
    pub fn corrupt(&mut self, stage: &'static str, pts: Option<i64>) -> Result<(), DecodeError> {
        if self.diagnostics.corrupt_packets.len() >= 1024 {
            return Err(DecodeError::Adapter(
                "corruption diagnostic budget exceeded".into(),
            ));
        }
        self.diagnostics.corrupt_packets.push(CorruptPacket {
            stage,
            pts: pts.filter(|pts| *pts != ffi::AV_NOPTS_VALUE),
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
    pub fn video_eof(&mut self) -> Result<(), DecodeError> {
        self.finish_video_gap(None);
        if self.consecutive != 0 {
            self.damaged_tail("video")?;
        }
        Ok(())
    }
    /// The stream ended in damage. It ends at its last healthy frame, unless
    /// none was decoded since the open or seek: damaged input cannot succeed
    /// with empty output.
    pub fn damaged_tail(&self, kind: &str) -> Result<(), DecodeError> {
        if self.healthy_frames == 0 {
            return Err(DecodeError::Adapter(format!(
                "damaged {kind} has no recoverable frames"
            )));
        }
        Ok(())
    }
    pub fn progress(&mut self, pts: i64) -> Result<(), DecodeError> {
        self.healthy_frames = self.healthy_frames.saturating_add(1);
        self.finish_video_gap((pts != ffi::AV_NOPTS_VALUE).then_some(pts));
        self.consecutive = 0;
        if pts != ffi::AV_NOPTS_VALUE && self.last_pts.is_none_or(|previous| pts > previous) {
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
    pub fn reset_progress(&mut self) {
        self.finish_video_gap(None);
        self.last_pts = None;
        self.healthy_frames = 0;
        self.stalled = 0;
        self.consecutive = 0;
    }
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
    /// A gap the stream's own timestamps leave: not damage, so never fatal.
    /// Past the record budget it is still reported, not recorded.
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

/// The longest a decoder operation (a seek, the next frame, an open's first
/// frame) may go without producing a frame before it fails with "decode
/// progress budget exceeded": the time since it started or since its last
/// decoded frame, between the `FFmpeg` calls it makes. A seek that decodes
/// from a distant keyframe at a steady pace takes as long as it needs; a
/// loop that decodes nothing does not.
///
/// Time spent inside one call is not counted: `FFmpeg` gives no way to stop a
/// call that is blocked, which is why a caller that watches decode for stalls
/// allows this plus its own GPU wait limit, from the last
/// [`watch_decode_progress`] report.
pub const DECODE_WORK_LIMIT: Duration = Duration::from_secs(10);

/// The most loop steps a decoder operation takes without producing a frame.
const DECODE_WORK_STEPS: usize = 65_536;

thread_local! {
    static DECODE_PROGRESS_WATCH: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// [`DECODE_WORK_LIMIT`] as a test on this thread sets it, else as
    /// `ALPHA2_TEST_DECODE_WORK_LIMIT_MS` sets it for a whole run: a run
    /// under valgrind decodes a frame many times slower and sets a longer
    /// one, which no production build reads.
    pub(crate) static WORK_LIMIT: std::cell::Cell<Duration> =
        std::cell::Cell::new(test_work_limit(std::env::var("ALPHA2_TEST_DECODE_WORK_LIMIT_MS").ok().as_deref()));
    /// [`DECODE_WORK_STEPS`] as a test on this thread sets it.
    pub(crate) static WORK_STEPS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(DECODE_WORK_STEPS) };
    /// How long a seek on this thread takes after each frame it decodes, as
    /// a slow decoder takes: slept, or, with [`VIRTUAL_CLOCK`] set, added
    /// to it.
    pub(crate) static FRAME_DELAY: std::cell::Cell<Duration> =
        const { std::cell::Cell::new(Duration::ZERO) };
    /// When set, the clock a work budget on this thread measures, which only
    /// [`FRAME_DELAY`] advances: a test of how long an operation may take
    /// between frames then depends on no real decoding speed, a release
    /// build's or one under valgrind.
    pub(crate) static VIRTUAL_CLOCK: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// The test builds' default [`WORK_LIMIT`], `setting` milliseconds when it
/// is a positive count.
#[cfg(test)]
fn test_work_limit(setting: Option<&str>) -> Duration {
    setting
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map_or(DECODE_WORK_LIMIT, Duration::from_millis)
}

/// What a decoder operation on this thread takes after a frame in a test
/// ([`FRAME_DELAY`]).
#[cfg(test)]
pub(crate) fn take_frame_delay() {
    let delay = FRAME_DELAY.with(std::cell::Cell::get);
    match VIRTUAL_CLOCK.with(std::cell::Cell::get) {
        Some(now) => VIRTUAL_CLOCK.with(|clock| clock.set(Some(now.saturating_add(delay)))),
        None => std::thread::sleep(delay),
    }
}

/// Calls `progressed` on this thread each time a decoder operation running
/// on it produces a frame, or a packet copy's plan reads a source packet,
/// until the guard drops.
///
/// An operation such as a seek can decode many frames before it returns, and
/// a caller that watches decode for stalls from outside (a host's watchdog)
/// takes this as the operation being alive rather than blocked.
#[must_use = "the watch ends when the guard drops"]
pub fn watch_decode_progress(progressed: impl Fn() + 'static) -> DecodeProgressWatch {
    let previous = DECODE_PROGRESS_WATCH.with(|watch| watch.replace(Some(Box::new(progressed))));
    DecodeProgressWatch {
        previous,
        thread: std::marker::PhantomData,
    }
}

/// Tells this thread's [`watch_decode_progress`] watch, if any, that the
/// operation in progress produced something: a frame, or a packet of a read
/// that goes through a whole file.
pub(crate) fn report_decode_progress() {
    DECODE_PROGRESS_WATCH.with(|watch| {
        if let Some(progressed) = watch.borrow().as_ref() {
            progressed();
        }
    });
}

/// Ends a [`watch_decode_progress`] watch, restoring the one before it.
pub struct DecodeProgressWatch {
    previous: Option<Box<dyn Fn()>>,
    /// The watch belongs to the thread that set it.
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

/// The time and loop steps one decoder operation has spent since it started or
/// last produced a frame, checked against [`DECODE_WORK_LIMIT`] and
/// [`DECODE_WORK_STEPS`].
pub(crate) struct WorkBudget {
    started: Instant,
    /// Where [`VIRTUAL_CLOCK`] stood at `started`, when it is set.
    #[cfg(test)]
    started_virtual: Option<Duration>,
    steps: usize,
}
impl WorkBudget {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            #[cfg(test)]
            started_virtual: VIRTUAL_CLOCK.with(std::cell::Cell::get),
            steps: 0,
        }
    }

    /// How long since the operation started or last produced a frame.
    fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let (Some(now), Some(started)) = (
            VIRTUAL_CLOCK.with(std::cell::Cell::get),
            self.started_virtual,
        ) {
            return now.saturating_sub(started);
        }
        self.started.elapsed()
    }
    pub fn step(&mut self, cancel: &CancelToken) -> Result<(), DecodeError> {
        if cancel.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        self.steps += 1;
        #[cfg(test)]
        let (limit, steps) = (
            WORK_LIMIT.with(std::cell::Cell::get),
            WORK_STEPS.with(std::cell::Cell::get),
        );
        #[cfg(not(test))]
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
        self.started = Instant::now();
        #[cfg(test)]
        {
            self.started_virtual = VIRTUAL_CLOCK.with(std::cell::Cell::get);
        }
        self.steps = 0;
        report_decode_progress();
    }
}

/// The closure always refers to the same packet until send accepts it. `true`
/// requests receive/drain, `false` requests send; its result means progress.
pub(crate) fn submit_packet(
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

    /// The policy names are the host's, both ways, and nothing else reads as one.
    #[test]
    fn a_policy_is_named_as_the_host_names_it() {
        for policy in [DecodePolicy::Strict, DecodePolicy::Tolerant] {
            assert_eq!(DecodePolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(DecodePolicy::Tolerant.as_str(), "tolerant");
        assert_eq!(DecodePolicy::parse("Tolerant"), None);
        assert_eq!(DecodePolicy::parse(""), None);
    }

    /// Counts sort audio silence by why it was inserted.
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
    /// Under Tolerant a hardware session's failure to decode one picture
    /// is damage, as `INVALIDDATA` is for software: a hardware session never
    /// reports `INVALIDDATA`, so Tolerant could not otherwise skip its damage.
    #[test]
    fn a_hardware_sessions_picture_failure_is_damage_only_when_tolerant() {
        let base = rsmpeg::avutil::ra(1, 30);
        for code in [ffi::AVERROR_UNKNOWN, ffi::AVERROR_EXTERNAL] {
            let mut tolerant = Recovery::hardware(base);
            tolerant.policy = DecodePolicy::Tolerant;
            assert!(
                tolerant
                    .error(RsmpegError::AVError(code), "video send", Some(3))
                    .is_ok()
            );
            assert_eq!(tolerant.diagnostics.corrupt_packets.len(), 1);
            let mut strict = Recovery::hardware(base);
            assert!(
                strict
                    .error(RsmpegError::AVError(code), "video send", None)
                    .is_err()
            );
            let mut software = Recovery::new(base);
            software.policy = DecodePolicy::Tolerant;
            assert!(
                software
                    .error(RsmpegError::AVError(code), "video send", None)
                    .is_err()
            );
        }
    }

    #[test]
    fn strict_tolerant_and_fatal_errors() {
        let mut state = Recovery::default();
        assert!(state.corrupt("send", Some(12)).is_err());
        state.policy = DecodePolicy::Tolerant;
        state.reset_progress();
        assert!(
            state
                .error(
                    RsmpegError::AVError(ffi::AVERROR_INVALIDDATA),
                    "send",
                    Some(13)
                )
                .is_ok()
        );
        assert!(
            state
                .error(RsmpegError::AVError(-12), "send", None)
                .is_err()
        );
        assert_eq!(state.diagnostics.corrupt_packets.len(), 2);
        for _ in 0..31 {
            state.corrupt("receive", None).unwrap();
        }
        assert!(state.corrupt("receive", None).is_err());
    }
    #[test]
    fn progress_and_cancellation_are_bounded() {
        let mut state = Recovery::default();
        state.progress(10).unwrap();
        for _ in 0..256 {
            state.progress(10).unwrap();
        }
        assert!(state.progress(10).is_err());
        state.reset_progress();
        state.progress(0).unwrap();
        let cancel = CancelToken::default();
        let mut budget = WorkBudget::new();
        for _ in 0..65_536 {
            budget.step(&cancel).unwrap();
        }
        assert!(budget.step(&cancel).is_err());
        // The production limit, whatever ALPHA2_TEST_DECODE_WORK_LIMIT_MS gives this run.
        WORK_LIMIT.with(|limit| limit.set(DECODE_WORK_LIMIT));
        let mut budget = WorkBudget::new();
        budget.started -= Duration::from_secs(11);
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
