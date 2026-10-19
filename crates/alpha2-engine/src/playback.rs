//! How real-time playback trades pictures against sound, and the clock that
//! enforces it.
//!
//! Quality (how a frame is drawn) and the clock (which frame is shown when)
//! are separate choices: a [`crate::QualityProfile`] decides the first, a
//! [`PlaybackPolicy`] the second.

use alpha2_time::{FrameRate, TICKS_PER_SECOND};
use serde::{Deserialize, Serialize};

use crate::MissingFramePolicy;

/// How early a paced frame may advance, in milliseconds: a display's
/// refresh timestamps are rounded (browsers to 0.1 ms or coarser), so a
/// refresh that is due can read a fraction early.
const PACING_SLACK_MS: f64 = 0.5;

/// Which frame real-time playback shows, and whether sound plays.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackPolicy {
    /// Sound leads at 1x: the picture shows the frame at the audio clock's
    /// time, dropping frames that are late and holding the last one while
    /// inputs are missing. At any other rate the picture follows the wall clock and
    /// sound is silent.
    #[default]
    DropVideoKeepAudio,
    /// Every frame is shown, in order, each once its inputs are ready, paced
    /// no faster than the rate asks: playback slows rather than skips, and is
    /// silent.
    PreserveEveryVideoFrame,
    /// The picture follows the wall clock at any rate, holding the last frame
    /// while inputs are missing, and is silent: for shuttling and scrubbing.
    SilentScrub,
    /// Every frame, in order, each after its inputs are ready, as fast as it
    /// renders: what an export does. Silent.
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

    /// Whether sound plays at `rate`: only when it leads, at 1x.
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
    /// The timeline time to show, in ticks.
    pub tick: i64,
    /// Whether sound should be playing.
    pub audible: bool,
    /// Whether this frame must be shown before the clock moves on.
    pub wait_for_frame: bool,
}

/// How long the audio clock may stand still before the picture continues on
/// the wall clock from where it stood, in milliseconds.
const AUDIO_STALL_MS: f64 = 250.0;

/// The playback clock of one run of playback: asked each display refresh
/// which timeline time to show.
#[derive(Clone, Debug)]
pub struct PlaybackClock {
    policy: PlaybackPolicy,
    rate: f64,
    frame_rate: FrameRate,
    /// Wall time and tick the wall clock counts from.
    origin_ms: f64,
    origin_tick: i64,
    /// The tick playback started at: neither clock runs before it.
    start_tick: i64,
    /// Whether the audio clock leads.
    following_audio: bool,
    last_audio: Option<i64>,
    last_audio_ms: f64,
    /// For a frame-by-frame policy: the frame shown next, and when the
    /// current one began showing.
    frame: i64,
    frame_ms: f64,
}

impl PlaybackClock {
    /// A clock starting at `tick` at wall time `now_ms`, playing at `rate`
    /// (negative plays backwards) under `policy`.
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

    /// The time to show at wall time `now_ms`. `heard` is the audio output's
    /// position in ticks while it runs (a position before the clock's start
    /// is its start); `presented` says whether the frame the clock last asked
    /// for has been shown.
    pub fn step(&mut self, now_ms: f64, heard: Option<i64>, presented: bool) -> ClockStep {
        let audible = self.policy.audible(self.rate);
        match self.policy {
            PlaybackPolicy::DropVideoKeepAudio | PlaybackPolicy::SilentScrub => {
                let tick = if audible {
                    self.follow_audio(now_ms, heard)
                } else {
                    self.wall_tick(now_ms)
                };
                ClockStep {
                    tick,
                    audible,
                    wait_for_frame: false,
                }
            }
            PlaybackPolicy::PreserveEveryVideoFrame | PlaybackPolicy::OfflineExact => {
                let paced = self.policy == PlaybackPolicy::PreserveEveryVideoFrame;
                let period = self.frame_ms_at_rate();
                // Carry each refresh's overshoot to the next deadline so frames average the rate on
                // any display; a late frame is followed by at most one more. The slack absorbs
                // rounded timestamps.
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

    /// The audio clock while it advances; the wall clock, from where the
    /// audio stood, while it does not.
    fn follow_audio(&mut self, now_ms: f64, heard: Option<i64>) -> i64 {
        // Sound leads only at 1x, forwards. Output that starts a tick before the start (a host
        // rounding to its nearest sample) still shows the start frame.
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
        // A refresh can be stamped before the clock started (a browser's `requestAnimationFrame`
        // time is the frame's start).
        let elapsed = (now_ms - self.origin_ms).max(0.0) * ticks_per_ms;
        // Within any timeline length; the host stops at the end.
        #[allow(clippy::cast_possible_truncation)]
        let advanced = (elapsed * self.rate).floor() as i64;
        self.origin_tick.saturating_add(advanced)
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
        let step = clock.step(10.0, Some(1_200), false);
        assert_eq!(
            (step.tick, step.audible, step.wait_for_frame),
            (1_200, true, false)
        );
        // The audio stands still past the stall: the picture continues on the wall clock from
        // there.
        clock.step(100.0, Some(1_200), true);
        let stalled = clock.step(400.0, Some(1_200), true);
        assert_eq!(stalled.tick, 1_200);
        let later = clock.step(500.0, Some(1_200), true);
        assert_eq!(later.tick, 1_200 + 12_000);
        assert_eq!(clock.step(510.0, Some(50_000), true).tick, 50_000);
        // At any rate other than 1x it is silent and on the wall clock.
        let mut fast = PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, rate(), 2.0, 0, 0.0);
        let step = fast.step(1_000.0, Some(7), true);
        assert_eq!((step.tick, step.audible), (240_000, false));
    }

    #[test]
    fn audio_heard_before_the_start_shows_the_start() {
        // 29.97 fps, playback from frame 2 (tick 8008). A host that starts its output on the
        // nearest sample at 48 kHz (3203, tick 8007.5) hears tick 8007 first: frame 1.
        let ntsc = FrameRate::new(30_000, 1_001).expect("rate");
        let mut clock =
            PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, ntsc, 1.0, 8_008, 0.0);
        assert_eq!(clock.step(5.0, Some(8_007), false).tick, 8_008);
        assert_eq!(clock.step(10.0, Some(8_007), true).tick, 8_008);
        assert_eq!(clock.step(20.0, Some(9_000), true).tick, 9_000);
        // A stall continues on the wall clock from where it stood, never before the start.
        let mut stalled =
            PlaybackClock::new(PlaybackPolicy::DropVideoKeepAudio, ntsc, 1.0, 8_008, 0.0);
        stalled.step(1.0, Some(8_007), false);
        assert_eq!(stalled.step(300.0, Some(8_007), true).tick, 8_008);
        assert_eq!(stalled.step(400.0, Some(8_007), true).tick, 8_008 + 12_000);
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
        // Restarting each frame's wait at the refresh it advanced on would hold every frame a
        // whole number of refreshes, so 29.97 fps would play at 20 fps on a 60 Hz display.
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
        assert_eq!(clock.step(1_500.0, None, true).tick, 60_000);
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
        assert_eq!((step.tick, step.audible), (60_000, false));
    }
}
