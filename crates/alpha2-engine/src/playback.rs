//! How real-time playback trades pictures against sound, and the clock that enforces it.
//!
//! A [`crate::QualityProfile`] decides how a frame is drawn; a [`PlaybackPolicy`] decides which
//! frame is shown when.

use alpha2_time::{FrameRate, TICKS_PER_SECOND};
use serde::{Deserialize, Serialize};

use crate::MissingFramePolicy;

/// How early a paced frame may advance, in milliseconds. Display refresh timestamps are rounded
/// (browsers to 0.1 ms or coarser), so a due refresh can read slightly early.
const PACING_SLACK_MS: f64 = 0.5;

/// Which frame real-time playback shows, and whether sound plays.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackPolicy {
    /// Sound leads at 1x; the picture follows the audio clock, dropping late frames and holding
    /// the last one while inputs are missing. Other rates follow the wall clock silently.
    #[default]
    DropVideoKeepAudio,
    /// Every frame in order once its inputs are ready, paced no faster than the rate. Slows
    /// rather than skips. Silent.
    PreserveEveryVideoFrame,
    /// The picture follows the wall clock at any rate, holding the last frame while inputs are
    /// missing. Silent; for shuttling and scrubbing.
    SilentScrub,
    /// Every frame in order as fast as it renders, like an export. Silent.
    OfflineExact,
}

impl PlaybackPolicy {
    /// Every policy, in the order a host lists them.
    pub const ALL: [Self; 4] = [
        Self::DropVideoKeepAudio,
        Self::PreserveEveryVideoFrame,
        Self::SilentScrub,
        Self::OfflineExact,
    ];

    /// What a frame whose inputs are missing shows.
    #[must_use]
    pub const fn missing_frames(self) -> MissingFramePolicy {
        match self {
            Self::DropVideoKeepAudio | Self::SilentScrub => MissingFramePolicy::HoldPrevious,
            Self::PreserveEveryVideoFrame | Self::OfflineExact => MissingFramePolicy::Wait,
        }
    }

    /// Whether sound plays at `rate` (only when it leads, at 1x).
    #[must_use]
    pub fn audible(self, rate: f64) -> bool {
        self == Self::DropVideoKeepAudio && (rate - 1.0).abs() < f64::EPSILON
    }

    /// Whether the clock waits for each frame to be shown before the next.
    #[must_use]
    pub const fn waits_for_frames(self) -> bool {
        matches!(self, Self::PreserveEveryVideoFrame | Self::OfflineExact)
    }
}

/// What the clock asks for at one moment.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ClockStep {
    /// The start of the frame to show, in ticks.
    pub tick: i64,
    /// Whether sound should be playing.
    pub audible: bool,
    /// Whether this frame must be shown before the clock moves on.
    pub wait_for_frame: bool,
}

/// Milliseconds the audio clock may stall before the picture continues on the wall clock.
const AUDIO_STALL_MS: f64 = 250.0;

/// How far past its last update the heard position is extrapolated, in milliseconds. Audio
/// outputs report it in callback bursts of up to about 40 ms; a longer silence holds the picture.
const AUDIO_EXTRAPOLATE_MS: f64 = 80.0;

/// A heard position this far from the smoothed estimate, in milliseconds, is jumped to (a seek or
/// an underrun) instead of averaged.
const AUDIO_SNAP_MS: f64 = 100.0;

/// The weight of each heard position in the running average of its error.
const AUDIO_ERROR_WEIGHT: f64 = 0.0625;

/// The averaged error, in milliseconds, past which the smoothed estimate is moved by it. Below it
/// the estimate keeps an exact 1x rate, so frames advance as evenly as on the wall clock.
const AUDIO_DEADBAND_MS: f64 = 8.0;

// 120,000 is exact in f64.
#[allow(clippy::cast_precision_loss)]
const TICKS_PER_MS: f64 = TICKS_PER_SECOND as f64 / 1000.0;

/// The clock for one playback run, asked each display refresh which time to show.
#[derive(Clone, Debug)]
pub struct PlaybackClock {
    policy: PlaybackPolicy,
    rate: f64,
    frame_rate: FrameRate,
    /// Wall time and tick the wall clock counts from.
    origin_ms: f64,
    origin_tick: i64,
    /// The tick playback started at; neither clock runs before it.
    start_tick: i64,
    /// Whether the audio clock leads.
    following_audio: bool,
    last_audio: Option<i64>,
    last_audio_ms: f64,
    /// The smoothed heard time runs on the wall clock from this tick at `heard_anchor_ms`.
    heard_anchor_tick: f64,
    heard_anchor_ms: f64,
    /// Running average of heard positions minus the smoothed time, in ticks.
    heard_error: f64,
    /// Added to the smoothed time when a frame is picked, in ticks. It is at most half a frame,
    /// set when the line moves so that refresh falls mid-frame; at a display rate equal to the
    /// frame rate every refresh then stays clear of frame boundaries.
    heard_bias: f64,
    /// The latest time shown while following audio; the smoothed time never goes back past it.
    heard_shown: i64,
    /// For frame-by-frame policies: the next frame and when the current one began showing.
    frame: i64,
    frame_ms: f64,
}

impl PlaybackClock {
    /// A clock starting at `tick` at wall time `now_ms`, playing at `rate` (negative is backwards).
    #[must_use]
    pub fn new(
        policy: PlaybackPolicy,
        frame_rate: FrameRate,
        rate: f64,
        tick: i64,
        now_ms: f64,
    ) -> Self {
        let frame = frame_rate.frame_at_or_before(alpha2_time::MediaTime(tick.max(0)));
        Self {
            policy,
            rate,
            frame_rate,
            origin_ms: now_ms,
            origin_tick: tick,
            start_tick: tick,
            following_audio: true,
            last_audio: None,
            last_audio_ms: now_ms,
            heard_anchor_tick: 0.0,
            heard_anchor_ms: now_ms,
            heard_error: 0.0,
            heard_bias: 0.0,
            heard_shown: tick,
            frame: frame.unwrap_or(0),
            frame_ms: now_ms,
        }
    }

    /// The policy it enforces.
    #[must_use]
    pub const fn policy(&self) -> PlaybackPolicy {
        self.policy
    }

    /// The time to show at wall time `now_ms`. `heard` is the audio output position in ticks
    /// while it runs; `presented` says whether the last requested frame has been shown.
    pub fn step(&mut self, now_ms: f64, heard: Option<i64>, presented: bool) -> ClockStep {
        let audible = self.policy.audible(self.rate);
        match self.policy {
            PlaybackPolicy::DropVideoKeepAudio | PlaybackPolicy::SilentScrub => {
                let time = if audible {
                    self.follow_audio(now_ms, heard)
                } else {
                    self.wall_tick(now_ms)
                };
                ClockStep {
                    tick: self.frame_start_at(time),
                    audible,
                    wait_for_frame: false,
                }
            }
            PlaybackPolicy::PreserveEveryVideoFrame | PlaybackPolicy::OfflineExact => {
                let paced = self.policy == PlaybackPolicy::PreserveEveryVideoFrame;
                let period = self.frame_ms_at_rate();
                // Carry overshoot to the next deadline so frames average the rate on any display.
                let due = !paced || now_ms - self.frame_ms >= period - PACING_SLACK_MS;
                if presented && due {
                    self.frame = (self.frame + if self.rate < 0.0 { -1 } else { 1 }).max(0);
                    self.frame_ms = if paced {
                        (self.frame_ms + period).max(now_ms - period)
                    } else {
                        now_ms
                    };
                }
                ClockStep {
                    tick: self.frame_tick(),
                    audible: false,
                    wait_for_frame: true,
                }
            }
        }
    }

    /// The audio clock while it advances, else the wall clock from where the audio stopped.
    ///
    /// Outputs report the heard position in callback bursts, so it is smoothed. It runs on the
    /// wall clock and moves only when the reports' averaged error leaves a dead band, so it cannot
    /// drift from the device either.
    fn follow_audio(&mut self, now_ms: f64, heard: Option<i64>) -> i64 {
        // Audio a tick before the start (host sample rounding) still shows the start frame.
        let heard = heard.map(|position| position.max(self.start_tick));
        match heard {
            Some(position) if Some(position) != self.last_audio => {
                self.heard_report(now_ms, position);
                self.last_audio = Some(position);
                self.last_audio_ms = now_ms;
                self.following_audio = true;
            }
            Some(position)
                if self.following_audio && now_ms - self.last_audio_ms > AUDIO_STALL_MS =>
            {
                self.following_audio = false;
                self.origin_ms = now_ms;
                self.origin_tick = position.max(self.heard_shown);
            }
            _ => {}
        }
        match heard {
            Some(_) if self.following_audio => {
                // Within any timeline length.
                #[allow(clippy::cast_possible_truncation)]
                let estimate = (self.heard_estimate(now_ms) + self.heard_bias).floor() as i64;
                self.heard_shown = self.heard_shown.max(estimate);
                self.heard_shown
            }
            _ => self.wall_tick(now_ms),
        }
    }

    /// The smoothed heard time at `now_ms`, in ticks, extrapolated at most
    /// [`AUDIO_EXTRAPOLATE_MS`] past the last report.
    fn heard_estimate(&self, now_ms: f64) -> f64 {
        let until = now_ms.min(self.last_audio_ms + AUDIO_EXTRAPOLATE_MS);
        self.heard_anchor_tick + (until - self.heard_anchor_ms).max(0.0) * TICKS_PER_MS
    }

    /// Takes in a new heard position. Call before `last_audio` and `last_audio_ms` move.
    fn heard_report(&mut self, now_ms: f64, position: i64) {
        // Within any timeline length.
        #[allow(clippy::cast_precision_loss)]
        let position = position as f64;
        let error = position - self.heard_estimate(now_ms);
        if self.last_audio.is_none()
            || !self.following_audio
            || error.abs() > AUDIO_SNAP_MS * TICKS_PER_MS
        {
            self.heard_anchor_tick = position;
            self.heard_anchor_ms = now_ms;
            self.heard_error = 0.0;
            self.center_heard_bias(now_ms);
            return;
        }
        if now_ms > self.last_audio_ms + AUDIO_EXTRAPOLATE_MS {
            // The estimate held through a gap, so the line restarts where it stopped.
            self.heard_anchor_tick = self.heard_estimate(now_ms);
            self.heard_anchor_ms = now_ms;
        }
        self.heard_error += (error - self.heard_error) * AUDIO_ERROR_WEIGHT;
        if self.heard_error.abs() > AUDIO_DEADBAND_MS * TICKS_PER_MS {
            self.heard_anchor_tick += self.heard_error;
            self.heard_error = 0.0;
            self.center_heard_bias(now_ms);
        }
    }

    fn center_heard_bias(&mut self, now_ms: f64) {
        let time = self.heard_estimate(now_ms);
        // Within any timeline length.
        #[allow(clippy::cast_possible_truncation)]
        let start = self.frame_start_at(time.floor() as i64);
        #[allow(clippy::cast_precision_loss)]
        let (start, end) = (start as f64, self.frame_end_after(start) as f64);
        self.heard_bias = f64::midpoint(start, end) - time;
    }

    fn wall_tick(&self, now_ms: f64) -> i64 {
        // `requestAnimationFrame` times can precede the clock's start.
        let elapsed = (now_ms - self.origin_ms).max(0.0) * TICKS_PER_MS;
        // Within any timeline length; the host stops at the end.
        #[allow(clippy::cast_possible_truncation)]
        let advanced = (elapsed * self.rate).floor() as i64;
        self.origin_tick.saturating_add(advanced)
    }

    /// The start of the frame containing `tick`, in either direction.
    fn frame_start_at(&self, tick: i64) -> i64 {
        self.frame_rate
            .frame_at_or_before(alpha2_time::MediaTime(tick.max(0)))
            .and_then(|frame| self.frame_rate.frame_start(frame))
            .map_or(0, alpha2_time::MediaTime::ticks)
    }

    /// The end of the frame starting at `start`.
    fn frame_end_after(&self, start: i64) -> i64 {
        self.frame_rate
            .frame_at_or_before(alpha2_time::MediaTime(start.max(0)))
            .and_then(|frame| self.frame_rate.frame_start(frame + 1))
            .map_or(start, alpha2_time::MediaTime::ticks)
    }

    fn frame_tick(&self) -> i64 {
        self.frame_rate
            .frame_start(self.frame)
            .map_or(0, alpha2_time::MediaTime::ticks)
    }

    /// How long one frame shows at the clock's rate, in milliseconds.
    fn frame_ms_at_rate(&self) -> f64 {
        let frame_ms = 1000.0 * f64::from(self.frame_rate.denominator())
            / f64::from(self.frame_rate.numerator());
        frame_ms / self.rate.abs().max(f64::MIN_POSITIVE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate() -> FrameRate {
        FrameRate::new(25, 1).expect("rate")
    }

    #[test]
    fn keeping_audio_follows_it_and_falls_back_to_the_wall_clock_when_it_stalls() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 1.0, 0, 0.0);
        // One frame at 25 fps is 4,800 ticks: the clock asks for the frame the audio is in.
        let step = clock.step(10.0, Some(1_200), false);
        assert_eq!(
            (step.tick, step.audible, step.wait_for_frame),
            (0, true, false)
        );
        // Between reports the heard time runs on for at most 80 ms (9,600 ticks), then holds.
        assert_eq!(clock.step(100.0, Some(1_200), true).tick, 2 * 4_800);
        let stalled = clock.step(400.0, Some(1_200), true);
        assert_eq!(stalled.tick, 2 * 4_800);
        // Picked mid-frame (1,200 ticks later), the held time is 12,000; the wall clock continues
        // from it, and 100 ms later is frame 5.
        let later = clock.step(500.0, Some(1_200), true);
        assert_eq!(later.tick, 5 * 4_800);
        assert_eq!(clock.step(510.0, Some(50_000), true).tick, 10 * 4_800);
        let mut fast = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 2.0, 0, 0.0);
        let step = fast.step(1_000.0, Some(7), true);
        assert_eq!((step.tick, step.audible), (240_000, false));
    }

    #[test]
    fn audio_heard_before_the_start_shows_the_start() {
        // 29.97 fps from frame 2 (tick 8008). 48 kHz output heard at tick 8007 (frame 1) still
        // shows frame 2.
        let ntsc = FrameRate::new(30_000, 1_001).expect("rate");
        let mut clock =
            PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, ntsc, 1.0, 8_008, 0.0);
        assert_eq!(clock.step(5.0, Some(8_007), false).tick, 8_008);
        assert_eq!(clock.step(10.0, Some(8_007), true).tick, 8_008);
        assert_eq!(clock.step(20.0, Some(9_000), true).tick, 8_008);
        let mut stalled =
            PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, ntsc, 1.0, 8_008, 0.0);
        stalled.step(1.0, Some(8_007), false);
        assert_eq!(stalled.step(300.0, Some(8_007), true).tick, 8_008);
        // Picked mid-frame, the held time is 8,508; 12,000 ticks later is inside frame 5.
        assert_eq!(stalled.step(400.0, Some(8_007), true).tick, 20_020);
    }

    #[test]
    fn preserving_every_frame_advances_one_frame_at_a_time_and_waits() {
        let mut clock =
            PlaybackClock::new(PlaybackPolicy::PreserveEveryVideoFrame, rate(), 1.0, 0, 0.0);
        let frame = 4_800; // one frame at 25 fps
        assert_eq!(clock.step(0.0, None, false).tick, 0);
        // Shown, but its 40 ms are not up.
        assert_eq!(clock.step(20.0, None, true).tick, 0);
        let next = clock.step(41.0, None, true);
        assert_eq!(
            (next.tick, next.audible, next.wait_for_frame),
            (frame, false, true)
        );
        assert_eq!(clock.step(500.0, None, false).tick, frame);
        assert_eq!(clock.step(501.0, None, true).tick, 2 * frame);
    }

    #[test]
    fn preserving_every_frame_keeps_the_rate_on_any_display() {
        // Without carrying overshoot, 29.97 fps would play at 20 fps on a 60 Hz display.
        for (num, den) in [
            (24_000, 1_001),
            (25, 1),
            (30_000, 1_001),
            (30, 1),
            (60_000, 1_001),
            (60, 1),
        ] {
            let rate = FrameRate::new(num, den).expect("rate");
            let fps = f64::from(num) / f64::from(den);
            for hz in [59.94, 60.0, 120.0, 144.0] {
                for grain in [0.0, 0.1, 1.0] {
                    if fps > hz {
                        continue;
                    }
                    let mut clock = PlaybackClock::new(
                        PlaybackPolicy::PreserveEveryVideoFrame,
                        rate,
                        1.0,
                        0,
                        0.0,
                    );
                    let seconds = 60.0;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let refreshes = (seconds * hz) as u64;
                    let mut tick = 0;
                    for refresh in 0..=refreshes {
                        #[allow(clippy::cast_precision_loss)]
                        let exact = refresh as f64 * 1_000.0 / hz;
                        let now = if grain > 0.0 {
                            (exact / grain).round() * grain
                        } else {
                            exact
                        };
                        tick = clock.step(now, None, true).tick;
                    }
                    let shown = clock.frame;
                    let expected = seconds * fps;
                    #[allow(clippy::cast_precision_loss)]
                    let short = expected - shown as f64;
                    assert!(
                        short.abs() <= 1.0,
                        "{fps} fps at {hz} Hz, {grain} ms: {shown} frames in {seconds} s, not {expected} (tick {tick})"
                    );
                }
            }
        }
    }

    #[test]
    fn offline_exact_moves_as_soon_as_each_frame_is_shown() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::OfflineExact, rate(), 1.0, 9_600, 0.0);
        assert_eq!(clock.step(0.0, None, false).tick, 9_600);
        assert_eq!(clock.step(0.0, None, true).tick, 14_400);
        assert_eq!(
            PlaybackPolicy::OfflineExact.missing_frames(),
            MissingFramePolicy::Wait
        );
        assert_eq!(
            PlaybackPolicy::SilentScrub.missing_frames(),
            MissingFramePolicy::HoldPrevious
        );
    }

    #[test]
    fn the_wall_clock_holds_at_its_start_until_its_start_time() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::SilentScrub, rate(), 1.0, 0, 1_000.0);
        assert_eq!(clock.step(992.0, None, false).tick, 0);
        // 60,000 ticks is inside frame 12.
        assert_eq!(clock.step(1_500.0, None, true).tick, 12 * 4_800);
        let mut silent = PlaybackClock::new(
            PlaybackPolicy::DropVideoKeepAudio,
            rate(),
            1.0,
            9_600,
            1_000.0,
        );
        assert_eq!(silent.step(990.0, None, false).tick, 9_600);
    }

    #[test]
    fn scrubbing_is_silent_and_follows_the_wall_clock_backwards_too() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::SilentScrub, rate(), -1.0, 120_000, 0.0);
        let step = clock.step(500.0, Some(0), true);
        assert_eq!((step.tick, step.audible), (12 * 4_800, false));
    }

    /// Frames advanced per display refresh over 10 s, skipping the first second, while the heard
    /// position moves in bursts of `burst_ms` from a device clock running at `drift` times the
    /// wall clock. Refresh times are rounded to 0.1 ms, as browsers report them.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn steps_per_refresh(fps: u32, hz: f64, burst_ms: f64, phase_ms: f64, drift: f64) -> Vec<i64> {
        let frame_rate = FrameRate::new(fps, 1).expect("rate");
        let mut clock =
            PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, frame_rate, 1.0, 0, 0.0);
        let frame_ticks = TICKS_PER_SECOND / i64::from(fps);
        let mut steps = Vec::new();
        let mut previous = None;
        for refresh in 0..(10.0 * hz) as u64 {
            let now = ((phase_ms + refresh as f64 * 1_000.0 / hz) * 10.0).round() / 10.0;
            // The output's callbacks are not aligned with the display or the frame grid.
            let device = now * drift;
            let callback = ((device - 5.3) / burst_ms).floor() * burst_ms + 5.3;
            let heard = (callback.max(0.0) * TICKS_PER_MS) as i64;
            let frame = clock.step(now, Some(heard), true).tick / frame_ticks;
            if now >= 1_000.0
                && let Some(previous) = previous
            {
                steps.push(frame - previous);
            }
            previous = Some(frame);
        }
        steps
    }

    fn count(steps: &[i64], step: i64) -> usize {
        steps.iter().filter(|&&each| each == step).count()
    }

    #[test]
    fn audio_reported_in_bursts_advances_one_frame_per_refresh_at_the_clip_rate() {
        // 1024 and 1152 samples at 48 kHz, and a 40 ms output.
        for burst_ms in [21.333, 24.0, 40.0] {
            for phase in 0..17 {
                let steps = steps_per_refresh(60, 60.0, burst_ms, f64::from(phase), 1.0);
                // At most one repeat while the averaged error settles, and no skip.
                assert!(
                    count(&steps, 1) + 1 >= steps.len() && count(&steps, 2) == 0,
                    "{burst_ms} ms bursts, phase {phase} ms: {steps:?}"
                );
            }
        }
        // A device 100 ppm fast moves the estimate once, repeating or skipping one frame.
        let drifting = steps_per_refresh(60, 60.0, 21.333, 3.0, 1.000_1);
        assert!(drifting.len() - count(&drifting, 1) <= 2, "{drifting:?}");
        let fast_display = steps_per_refresh(60, 120.0, 21.333, 3.0, 1.0);
        assert_eq!(count(&fast_display, 1), 540);
        assert_eq!(count(&fast_display, 0), 540);
        // 50 fps on 60 Hz repeats one frame in six and never skips.
        let pulldown = steps_per_refresh(50, 60.0, 21.333, 3.0, 1.0);
        assert_eq!(count(&pulldown, 0), 90);
        assert_eq!(count(&pulldown, 1), 450);
    }

    #[test]
    fn a_heard_position_far_from_the_estimate_is_jumped_to_but_never_backwards() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 1.0, 0, 0.0);
        clock.step(0.0, Some(0), false);
        // 2 s ahead: snapped, not slewed.
        assert_eq!(clock.step(20.0, Some(240_000), true).tick, 240_000);
        // 150 ms behind what was shown: the picture holds instead of going back.
        assert_eq!(clock.step(40.0, Some(240_000 - 18_000), true).tick, 240_000);
    }

    #[test]
    fn the_wall_clock_after_an_audio_stall_is_quantized_too() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 1.0, 0, 0.0);
        clock.step(0.0, Some(100), false);
        // Stalled past `AUDIO_STALL_MS`: the wall clock continues from the 2,400 ticks shown
        // (100 picked mid-frame).
        assert_eq!(clock.step(300.0, Some(100), true).tick, 0);
        // 10 ms later is 3,600 ticks, still frame 0; 30 ms later is 6,000, frame 1.
        assert_eq!(clock.step(310.0, Some(100), true).tick, 0);
        assert_eq!(clock.step(330.0, Some(100), true).tick, 4_800);
    }

    #[test]
    fn reverse_play_asks_for_the_frame_containing_the_time() {
        // From inside frame 25 (120,000..124,800) backwards: 10 ms back is 121,000, then 30 ms
        // more is frame 24.
        let mut clock = PlaybackClock::new(PlaybackPolicy::SilentScrub, rate(), -1.0, 122_200, 0.0);
        assert_eq!(clock.step(10.0, None, true).tick, 120_000);
        assert_eq!(clock.step(40.0, None, true).tick, 115_200);
        // Past the start it holds frame 0.
        assert_eq!(clock.step(10_000.0, None, true).tick, 0);
    }
}
