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
    fn follow_audio(&mut self, now_ms: f64, heard: Option<i64>) -> i64 {
        // Audio a tick before the start (host sample rounding) still shows the start frame.
        let heard = heard.map(|position| position.max(self.start_tick));
        match heard {
            Some(position) if Some(position) != self.last_audio => {
                self.last_audio = Some(position);
                self.last_audio_ms = now_ms;
                self.following_audio = true;
            }
            Some(position)
                if self.following_audio && now_ms - self.last_audio_ms > AUDIO_STALL_MS =>
            {
                self.following_audio = false;
                self.origin_ms = now_ms;
                self.origin_tick = position;
            }
            _ => {}
        }
        match heard {
            Some(position) if self.following_audio => position,
            _ => self.wall_tick(now_ms),
        }
    }

    fn wall_tick(&self, now_ms: f64) -> i64 {
        // 120,000 is exact in f64.
        #[allow(clippy::cast_precision_loss)]
        let ticks_per_ms = TICKS_PER_SECOND as f64 / 1000.0;
        // `requestAnimationFrame` times can precede the clock's start.
        let elapsed = (now_ms - self.origin_ms).max(0.0) * ticks_per_ms;
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
        clock.step(100.0, Some(1_200), true);
        let stalled = clock.step(400.0, Some(1_200), true);
        assert_eq!(stalled.tick, 0);
        // 1,200 + 12,000 ticks is inside frame 2.
        let later = clock.step(500.0, Some(1_200), true);
        assert_eq!(later.tick, 2 * 4_800);
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
        // 8,008 + 12,000 ticks is inside frame 4, which starts at 16,016.
        assert_eq!(stalled.step(400.0, Some(8_007), true).tick, 16_016);
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

    #[test]
    fn audio_positions_inside_one_frame_ask_for_that_frame_once() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 1.0, 0, 0.0);
        // Three display refreshes inside frame 1 (4,800..9,600) ask for the same tick.
        let ticks: Vec<i64> = [(16.0, 4_800), (32.0, 6_400), (48.0, 9_599)]
            .into_iter()
            .map(|(now, heard)| clock.step(now, Some(heard), true).tick)
            .collect();
        assert_eq!(ticks, [4_800; 3]);
        // Crossing the boundary asks for the next frame's start.
        assert_eq!(clock.step(64.0, Some(9_600), true).tick, 9_600);
    }

    #[test]
    fn the_wall_clock_after_an_audio_stall_is_quantized_too() {
        let mut clock = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 1.0, 0, 0.0);
        clock.step(0.0, Some(100), false);
        // Stalled past `AUDIO_STALL_MS`: the wall clock continues from 100 ticks.
        assert_eq!(clock.step(300.0, Some(100), true).tick, 0);
        // 30 ms later is 3,700 ticks, still frame 0; 40 ms later is 4,900, frame 1.
        assert_eq!(clock.step(330.0, Some(100), true).tick, 0);
        assert_eq!(clock.step(340.0, Some(100), true).tick, 4_800);
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
