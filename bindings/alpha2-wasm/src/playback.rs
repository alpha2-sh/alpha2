//! The engine's playback clock, published to JavaScript.
//!
//! Which frame real-time playback shows, and whether sound plays, is the
//! engine's decision ([`alpha2_engine::PlaybackPolicy`]); the page only asks
//! the clock each display refresh and does what it says.

use alpha2_engine::{PlaybackClock, PlaybackPolicy};
use alpha2_time::FrameRate;
use wasm_bindgen::prelude::*;

use crate::web::{invalid_input, typed_error};

/// One run of playback's clock.
#[wasm_bindgen(js_name = PlaybackClock)]
pub struct WebPlaybackClock {
    clock: PlaybackClock,
}

/// What the clock asks for at one moment.
#[wasm_bindgen(js_name = ClockStep)]
pub struct WebClockStep {
    tick: f64,
    audible: bool,
    wait_for_frame: bool,
}

#[wasm_bindgen(js_class = ClockStep)]
impl WebClockStep {
    /// The start of the frame to show, in ticks.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn tick(&self) -> f64 {
        self.tick
    }

    /// Whether sound should be playing.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn audible(&self) -> bool {
        self.audible
    }

    /// Whether this frame must be shown before the clock moves on.
    #[wasm_bindgen(getter, js_name = waitForFrame)]
    #[must_use]
    pub fn wait_for_frame(&self) -> bool {
        self.wait_for_frame
    }
}

#[wasm_bindgen(js_class = PlaybackClock)]
impl WebPlaybackClock {
    /// A clock starting at `tick` at wall time `now_ms`, playing at `rate`
    /// under `policy` (`drop_video_keep_audio`, `preserve_every_video_frame`,
    /// `silent_scrub`, or `offline_exact`).
    ///
    /// # Errors
    /// Rejects an unknown policy, an invalid frame rate, or a rate or tick
    /// that is not finite.
    #[wasm_bindgen(constructor)]
    pub fn new(
        policy: &str,
        frame_rate_num: u32,
        frame_rate_den: u32,
        rate: f64,
        tick: f64,
        now_ms: f64,
    ) -> Result<Self, JsValue> {
        let policy = parse_policy(policy)?;
        let frame_rate =
            FrameRate::new(frame_rate_num, frame_rate_den).map_err(|error| typed_error(&error))?;
        if !rate.is_finite() || rate == 0.0 {
            return Err(invalid_input("playback rate must be finite and non-zero"));
        }
        Ok(Self {
            clock: PlaybackClock::new(policy, frame_rate, rate, ticks(tick)?, now_ms),
        })
    }

    /// Whether sound plays under `policy` at `rate`.
    ///
    /// # Errors
    /// Rejects an unknown policy.
    pub fn audible(policy: &str, rate: f64) -> Result<bool, JsValue> {
        Ok(parse_policy(policy)?.audible(rate))
    }

    /// The time to show at wall time `now_ms`. `heard` is the audio output's
    /// position in ticks while it runs, else `undefined` (a position before
    /// the clock's start is its start); `presented` says whether the frame
    /// the clock last asked for has been shown.
    ///
    /// # Errors
    /// Rejects a `heard` that is not an integer tick.
    pub fn step(
        &mut self,
        now_ms: f64,
        heard: Option<f64>,
        presented: bool,
    ) -> Result<WebClockStep, JsValue> {
        let heard = heard.map(ticks).transpose()?;
        let step = self.clock.step(now_ms, heard, presented);
        // Ticks stay within a timeline's length, far inside f64's integers.
        #[allow(clippy::cast_precision_loss)]
        let tick = step.tick as f64;
        Ok(WebClockStep {
            tick,
            audible: step.audible,
            wait_for_frame: step.wait_for_frame,
        })
    }
}

fn parse_policy(policy: &str) -> Result<PlaybackPolicy, JsValue> {
    serde_json::from_value(serde_json::Value::String(policy.to_owned()))
        .map_err(|_| invalid_input(format!("unknown playback policy {policy:?}")))
}

fn ticks(value: f64) -> Result<i64, JsValue> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if !value.is_finite() || value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER {
        return Err(invalid_input("ticks must be a safe integer"));
    }
    // Checked above: an integer within i64.
    #[allow(clippy::cast_possible_truncation)]
    Ok(value as i64)
}
