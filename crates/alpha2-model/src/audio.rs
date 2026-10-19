use crate::{Animated, TrackId};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};
use serde::{Deserialize, Serialize};

/// Ticks in a millisecond.
pub(crate) const TICKS_PER_MS: u32 = 120;
const _: () = assert!(TICKS_PER_SECOND == 1_000 * TICKS_PER_MS as i64);

/// Largest linear amplitude accepted by any authored gain.
pub const MAX_GAIN: f32 = 16.0;

/// Most audio effects a project holds state for at once: its master's, its
/// tracks', and those of the clips holding state then (see
/// [`AUDIO_EFFECT_STATE_MARGIN_MS`]). An equalizer is one effect, whatever its
/// bands.
pub const MAX_AUDIO_EFFECTS: usize = 8_192;

/// Most audio a project's effects hold in delay lines at once, in
/// milliseconds, summed over the effects [`MAX_AUDIO_EFFECTS`] counts then: a
/// compressor holds its `lookahead_ms`, a reverb [`REVERB_DELAY_MS`]. At the
/// highest project rate this is under 100 MB of state.
pub const MAX_AUDIO_DELAY_MS: u32 = 120_000;

/// How long before its start, beyond the lookahead of its track's and its own
/// compressors, a clip's effects are counted as holding state, in
/// milliseconds. A mixer builds a clip's effects for the blocks that mix the
/// clip and drops them for the first that does not; it reads a clip early by
/// the lookahead after it, and one block of the largest size at the lowest
/// project rate, with the rounding of each lookahead to samples, is within
/// this margin. A clip's effects count until its end.
pub const AUDIO_EFFECT_STATE_MARGIN_MS: u32 = 1_100;

/// What one reverb counts toward [`MAX_AUDIO_DELAY_MS`]: its comb and allpass
/// lines hold about 157 ms of audio at any rate.
pub const REVERB_DELAY_MS: u32 = 160;

/// Largest peaking gain, in decibels either way, an EQ band renders.
pub const MAX_EQ_GAIN_DB: f32 = 36.0;

/// The resonances an EQ band renders.
pub const EQ_Q_RANGE: std::ops::RangeInclusive<f32> = 0.05..=50.0;

/// The frequencies an EQ band may name, in hertz: from 10 Hz to just under
/// the Nyquist frequency of the highest project rate. A band renders at most
/// at 0.49 times the project's own rate, so a band above that limit of the
/// project it is in sounds at the limit.
pub const EQ_FREQUENCY_RANGE_HZ: std::ops::RangeInclusive<f32> = 10.0..=47_040.0;

/// Largest compressor ratio.
pub const MAX_COMPRESSOR_RATIO: f32 = 100.0;

/// Most effects one chain holds: a clip's, a track's, or the master's.
pub const MAX_CHAIN_EFFECTS: usize = 16;

/// Most bands one equalizer holds.
pub const MAX_EQ_BANDS: usize = 16;

/// The quietest level a compressor threshold or a ducking depth names, in
/// decibels; both are at most 0 dB.
pub const MIN_LEVEL_DB: f32 = -96.0;

/// Longest compressor lookahead, in milliseconds.
pub const MAX_LOOKAHEAD_MS: f32 = 100.0;

/// Longest attack or release time, in milliseconds.
pub const MAX_ENVELOPE_MS: f32 = 10_000.0;

/// The filter shape of one equalizer band.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EqBandKind {
    /// Bell-shaped parametric band.
    Peaking,
    /// Second-order low-pass.
    LowPass,
    /// Second-order high-pass.
    HighPass,
}

/// A single equalizer section stored independently from its DSP implementation.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EqBand {
    /// Filter shape.
    pub kind: EqBandKind,
    /// Center or corner frequency in hertz, within [`EQ_FREQUENCY_RANGE_HZ`].
    pub frequency_hz: f32,
    /// Resonance, within [`EQ_Q_RANGE`]. Larger values produce narrower bands.
    pub q: f32,
    /// Peaking gain in decibels, at most [`MAX_EQ_GAIN_DB`] either way.
    /// Ignored by the pass filters.
    #[serde(default)]
    pub gain_db: f32,
}

impl EqBand {
    fn is_valid(self) -> bool {
        EQ_FREQUENCY_RANGE_HZ.contains(&self.frequency_hz)
            && EQ_Q_RANGE.contains(&self.q)
            && self.gain_db.is_finite()
            && self.gain_db.abs() <= MAX_EQ_GAIN_DB
    }
}

/// A serializable single-input audio effect applied to a clip or track bus.
///
/// Sidechain ducking is not an entry here because it consumes a second input.
/// It is authored per track through [`Ducking`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioEffect {
    /// Cascaded biquad sections.
    Eq {
        /// Ordered filter sections, 1 to [`MAX_EQ_BANDS`] of them.
        bands: Vec<EqBand>,
    },
    /// Lookahead dynamic range compression.
    Compressor {
        /// Level above which gain reduction begins, in decibels from
        /// [`MIN_LEVEL_DB`] to 0.
        threshold_db: f32,
        /// Downward ratio, from one (unity) to [`MAX_COMPRESSOR_RATIO`].
        ratio: f32,
        /// Attack time in milliseconds, positive and at most [`MAX_ENVELOPE_MS`].
        attack_ms: f32,
        /// Release time in milliseconds, positive and at most [`MAX_ENVELOPE_MS`].
        release_ms: f32,
        /// Lookahead in milliseconds, at most [`MAX_LOOKAHEAD_MS`]. Reported as
        /// graph latency.
        #[serde(default)]
        lookahead_ms: f32,
    },
    /// Schroeder reverberation.
    Reverb {
        /// Room size, in `0..=1`.
        room_size: f32,
        /// High-frequency damping, in `0..=1`.
        damping: f32,
        /// Wet mix, in `0..=1`.
        mix: f32,
    },
}

impl AudioEffect {
    /// What this effect counts toward [`MAX_AUDIO_DELAY_MS`], in ticks (a
    /// millisecond is 120), rounded up: a compressor its `lookahead_ms`, a
    /// reverb [`REVERB_DELAY_MS`]. A tick count is exact, so the effects of a
    /// project add up to the same number in any order.
    #[must_use]
    pub fn delay_ticks(&self) -> u64 {
        match self {
            Self::Eq { .. } => 0,
            Self::Compressor { .. } => self.lookahead_ticks(),
            Self::Reverb { .. } => u64::from(REVERB_DELAY_MS) * u64::from(TICKS_PER_MS),
        }
    }

    /// How early a mixer reads this effect's input, in ticks, rounded up: a
    /// compressor's `lookahead_ms`.
    #[must_use]
    pub fn lookahead_ticks(&self) -> u64 {
        match self {
            // A 32-bit value times 120 is exact in 64 bits, so rounding it up is too.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Self::Compressor { lookahead_ms, .. } if *lookahead_ms > 0.0 => {
                (f64::from(*lookahead_ms) * f64::from(TICKS_PER_MS)).ceil() as u64
            }
            _ => 0,
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Eq { bands } => {
                !bands.is_empty()
                    && bands.len() <= MAX_EQ_BANDS
                    && bands.iter().copied().all(EqBand::is_valid)
            }
            Self::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                lookahead_ms,
            } => {
                threshold_db.is_finite()
                    && (MIN_LEVEL_DB..=0.0).contains(threshold_db)
                    && (1.0..=MAX_COMPRESSOR_RATIO).contains(ratio)
                    && positive_ms(*attack_ms)
                    && positive_ms(*release_ms)
                    && lookahead_ms.is_finite()
                    && (0.0..=MAX_LOOKAHEAD_MS).contains(lookahead_ms)
            }
            Self::Reverb {
                room_size,
                damping,
                mix,
            } => unit(*room_size) && unit(*damping) && unit(*mix),
        }
    }
}

/// Sidechain ducking authored on the track that is attenuated.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Ducking {
    /// Track whose mixed output drives the envelope follower.
    pub sidechain_track: TrackId,
    /// Linear level above which attenuation begins. Positive.
    pub threshold: f32,
    /// Maximum attenuation in decibels, from [`MIN_LEVEL_DB`] to 0.
    pub depth_db: f32,
    /// Attack time in milliseconds, positive and at most [`MAX_ENVELOPE_MS`].
    pub attack_ms: f32,
    /// Release time in milliseconds, positive and at most [`MAX_ENVELOPE_MS`].
    pub release_ms: f32,
}

impl Ducking {
    fn is_valid(&self) -> bool {
        self.threshold.is_finite()
            && self.threshold > 0.0
            && self.depth_db.is_finite()
            && (MIN_LEVEL_DB..=0.0).contains(&self.depth_db)
            && positive_ms(self.attack_ms)
            && positive_ms(self.release_ms)
    }
}

/// Curve applied by a clip's fade in and fade out.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FadeShape {
    /// Amplitude moves linearly. Two linear fades over one overlap dip by 6 dB.
    #[default]
    Linear,
    /// Amplitude follows a quarter sine. Two of these crossfade at constant
    /// power, which is what overlapping clips normally want.
    EqualPower,
}

impl FadeShape {
    /// Maps linear fade progress in `0..=1` to an amplitude multiplier.
    #[must_use]
    pub fn amplitude(self, progress: f32) -> f32 {
        let progress = progress.clamp(0.0, 1.0);
        match self {
            Self::Linear => progress,
            Self::EqualPower => libm::sinf(progress * core::f32::consts::FRAC_PI_2),
        }
    }
}

/// Per-clip audio presentation controls.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AudioProperties {
    /// Animated linear amplitude constrained to `0..=MAX_GAIN`.
    #[serde(default = "unit_gain")]
    pub gain: Animated<f32>,
    /// Animated stereo position from `-1` (left) to `1` (right).
    #[serde(default = "center_pan")]
    pub pan: Animated<f32>,
    /// Fade-in duration measured from the clip's timeline start. Non-negative.
    #[serde(default)]
    pub fade_in: MediaTime,
    /// Fade-out duration measured back from the clip's timeline end.
    /// Non-negative.
    #[serde(default)]
    pub fade_out: MediaTime,
    /// Curve used by both fades.
    #[serde(default)]
    pub fade_shape: FadeShape,
    /// Silences this clip without removing it.
    #[serde(default)]
    pub muted: bool,
    /// Ordered single-input effects applied before the track bus, at most
    /// [`MAX_CHAIN_EFFECTS`].
    #[serde(default)]
    pub effects: Vec<AudioEffect>,
}

impl Default for AudioProperties {
    fn default() -> Self {
        Self {
            gain: unit_gain(),
            pan: center_pan(),
            fade_in: MediaTime::ZERO,
            fade_out: MediaTime::ZERO,
            fade_shape: FadeShape::Linear,
            muted: false,
            effects: Vec::new(),
        }
    }
}

impl AudioProperties {
    /// Returns whether this clip can contribute signal at any timestamp.
    #[must_use]
    pub fn is_audible(&self) -> bool {
        !self.muted
            && match &self.gain {
                Animated::Constant { value } => *value > 0.0,
                Animated::Keyframes { keyframes } => {
                    keyframes.iter().any(|keyframe| keyframe.value > 0.0)
                }
            }
    }

    pub(crate) fn validate(&self) -> bool {
        valid_gain(&self.gain)
            && valid_pan(&self.pan)
            && self.fade_in.ticks() >= 0
            && self.fade_out.ticks() >= 0
            && self.effects.len() <= MAX_CHAIN_EFFECTS
            && self.effects.iter().all(AudioEffect::is_valid)
    }
}

/// Per-track audio bus controls.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TrackAudio {
    /// Animated linear bus amplitude constrained to `0..=MAX_GAIN`.
    #[serde(default = "unit_gain")]
    pub gain: Animated<f32>,
    /// Animated stereo position applied after the clip mix, from `-1` (left)
    /// to `1` (right).
    #[serde(default = "center_pan")]
    pub pan: Animated<f32>,
    /// Silences this bus.
    #[serde(default)]
    pub muted: bool,
    /// Restricts the mix to soloed buses when any track sets this.
    #[serde(default)]
    pub solo: bool,
    /// Ordered single-input effects applied to the summed clip signal, at
    /// most [`MAX_CHAIN_EFFECTS`].
    #[serde(default)]
    pub effects: Vec<AudioEffect>,
    /// Optional sidechain attenuation driven by another track.
    #[serde(default)]
    pub ducking: Option<Ducking>,
}

impl Default for TrackAudio {
    fn default() -> Self {
        Self {
            gain: unit_gain(),
            pan: center_pan(),
            muted: false,
            solo: false,
            effects: Vec::new(),
            ducking: None,
        }
    }
}

impl TrackAudio {
    /// Whether this bus adds nothing to the mix: it is muted, or another
    /// track solos (`any_solo`, [`crate::Timeline::any_solo`]) and this one
    /// does not.
    #[must_use]
    pub const fn is_silenced(&self, any_solo: bool) -> bool {
        self.muted || (any_solo && !self.solo)
    }

    pub(crate) fn validate(&self) -> bool {
        valid_gain(&self.gain)
            && valid_pan(&self.pan)
            && self.effects.len() <= MAX_CHAIN_EFFECTS
            && self.effects.iter().all(AudioEffect::is_valid)
            && self.ducking.as_ref().is_none_or(Ducking::is_valid)
    }
}

/// Project-wide master bus settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MasterAudio {
    /// Animated linear master amplitude constrained to `0..=MAX_GAIN`.
    #[serde(default = "unit_gain")]
    pub gain: Animated<f32>,
    /// Ordered single-input effects applied to the summed track buses, at
    /// most [`MAX_CHAIN_EFFECTS`].
    #[serde(default)]
    pub effects: Vec<AudioEffect>,
    /// Final peak limiter. An omitted field is [`Limiter::default`]; an
    /// explicit `null` leaves the mix unlimited.
    #[serde(default = "default_limiter")]
    pub limiter: Option<Limiter>,
}

impl Default for MasterAudio {
    fn default() -> Self {
        Self {
            gain: unit_gain(),
            effects: Vec::new(),
            limiter: default_limiter(),
        }
    }
}

impl MasterAudio {
    pub(crate) fn validate(&self) -> bool {
        valid_gain(&self.gain)
            && self.effects.len() <= MAX_CHAIN_EFFECTS
            && self.effects.iter().all(AudioEffect::is_valid)
            && self.limiter.is_none_or(Limiter::is_valid)
    }
}

/// Final peak limiter placed at the end of the master bus.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Limiter {
    /// Linear ceiling, positive and at most [`MAX_GAIN`]. One is full scale.
    pub threshold: f32,
    /// Release time in milliseconds, positive and at most [`MAX_ENVELOPE_MS`].
    pub release_ms: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self {
            threshold: 1.0,
            release_ms: 50.0,
        }
    }
}

impl Limiter {
    fn is_valid(self) -> bool {
        self.threshold.is_finite()
            && self.threshold > 0.0
            && self.threshold <= MAX_GAIN
            && positive_ms(self.release_ms)
    }
}

/// Serde default: a project without a `limiter` field keeps the safety ceiling.
#[allow(clippy::unnecessary_wraps)]
fn default_limiter() -> Option<Limiter> {
    Some(Limiter::default())
}

fn unit_gain() -> Animated<f32> {
    Animated::Constant { value: 1.0 }
}

fn center_pan() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
}

fn unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn positive_ms(value: f32) -> bool {
    value.is_finite() && value > 0.0 && value <= MAX_ENVELOPE_MS
}

fn valid_gain(gain: &Animated<f32>) -> bool {
    valid_range(gain, 0.0, MAX_GAIN)
}

fn valid_pan(pan: &Animated<f32>) -> bool {
    valid_range(pan, -1.0, 1.0)
}

fn valid_range(animation: &Animated<f32>, low: f32, high: f32) -> bool {
    let valid = |value: f32| value.is_finite() && (low..=high).contains(&value);
    match animation {
        Animated::Constant { value } => valid(*value),
        Animated::Keyframes { keyframes } => {
            crate::animation::valid_keyframes(keyframes)
                && keyframes.iter().all(|keyframe| valid(keyframe.value))
        }
    }
}
