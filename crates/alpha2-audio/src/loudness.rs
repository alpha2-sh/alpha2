//! Loudness measurement: ITU-R BS.1770-4 and EBU R128.
//!
//! [`LoudnessMeter`] measures planar audio streamed in blocks of any size:
//!
//! - **Momentary** (400 ms) and **short-term** (3 s) loudness, EBU Tech 3341.
//! - **Loudness range**, EBU Tech 3342: the spread between the 10th and 95th
//!   percentiles of short-term loudness, gated at -70 LUFS and 20 LU below.
//! - **True peak** (BS.1770-4 Annex 2): the peak of the signal oversampled
//!   four times (twice at 96 kHz) through a polyphase interpolator.
//!
//! Filtering and sums run in `f64` with `libm`, so measurements are bit-identical natively and
//! in wasm.
//!
//! Gated values use fixed histograms of 0.01 LU bins from the absolute gate to +30 LUFS, each
//! keeping a count and summed power (libebur128's histogram mode with sums), so measuring never
//! allocates. A bin straddling the relative gate counts by the loudness of its mean. Compared
//! with keeping every value, integrated loudness stays within 0.01 LU and range within 0.02 LU;
//! EBU Tech 3341 and 3342 allow 0.1 and 1 LU.

use alpha2_media::{Speaker, SpeakerLayout};
use libm::{log10, pow, tan};

/// The level below which a block never counts, in LUFS.
pub const ABSOLUTE_GATE_LUFS: f64 = -70.0;
/// Integrated loudness ignores blocks this far below the gated level, in LU.
const INTEGRATED_RELATIVE_GATE: f64 = -10.0;
/// Loudness range ignores short-term values this far below theirs, in LU.
const RANGE_RELATIVE_GATE: f64 = -20.0;
/// Sub-blocks per second: every window is a whole number of 100 ms steps.
const STEPS_PER_SECOND: u64 = 10;
/// A momentary window and a gating block: 400 ms.
const MOMENTARY_STEPS: usize = 4;
/// A short-term window: 3 s.
const SHORT_TERM_STEPS: usize = 30;
/// Interpolator taps per phase in the true-peak oversampler.
const TRUE_PEAK_TAPS: usize = 12;
/// Histogram bins per LU.
const BINS_PER_LU: f64 = 100.0;
/// Histogram bins from the absolute gate to +30 LUFS; louder values count in the top bin.
const BINS: usize = 10_000;
/// K-weighting output below this (about -600 dBFS) is flushed to silence.
const FLUSH_BELOW: f64 = 1.0e-30;

/// What a [`LoudnessMeter`] has measured.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LoudnessReport {
    /// Gated integrated loudness in LUFS; `None` while no block passes the absolute gate.
    pub integrated_lufs: Option<f64>,
    /// Loudness range in LU; `None` until a 3 s window passes the gates.
    pub loudness_range_lu: Option<f64>,
    /// Maximum true peak in dBTP; `None` for digital silence.
    pub true_peak_dbtp: Option<f64>,
    /// Maximum sample peak in dBFS; `None` for digital silence.
    pub sample_peak_dbfs: Option<f64>,
    /// Loudest momentary (400 ms) value in LUFS.
    pub momentary_max_lufs: Option<f64>,
    /// Loudest short-term (3 s) value in LUFS.
    pub short_term_max_lufs: Option<f64>,
    /// Samples measured per channel.
    pub frames: u64,
}

/// Why a meter cannot be configured or fed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LoudnessError {
    /// No channels, a rate outside 8 to 96 kHz, or a non-finite weight.
    InvalidShape,
    /// A block's channel count or plane lengths do not match.
    MismatchedBlock,
}

impl std::fmt::Display for LoudnessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidShape => "a loudness meter needs channels and a rate of 8 to 96 kHz",
            Self::MismatchedBlock => "the block's channels do not match the meter's",
        })
    }
}

impl std::error::Error for LoudnessError {}

/// One channel's K-weighting filters and true-peak history.
#[derive(Clone, Debug)]
struct Channel {
    weight: f64,
    shelf: Biquad64,
    high_pass: Biquad64,
    history: [f64; TRUE_PEAK_TAPS],
}

/// A direct-form-I biquad in `f64` with the meter's own fixed coefficients.
#[derive(Copy, Clone, Debug)]
struct Biquad64 {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad64 {
    const fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self {
            b,
            a,
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn process(&mut self, input: f64) -> f64 {
        let output = self.b[0] * input + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        // Keep digital silence out of the subnormal range; this is far below the absolute gate.
        let output = if output.abs() < FLUSH_BELOW {
            0.0
        } else {
            output
        };
        self.x = [input, self.x[0]];
        self.y = [output, self.y[0]];
        output
    }
}

/// The BS.1770-4 pre-filter: a high shelf of about +4 dB above 1.5 kHz.
fn shelf(sample_rate: u32) -> Biquad64 {
    let f0 = 1_681.974_450_955_533;
    let gain_db = 3.999_843_853_973_347;
    let q = 0.707_175_236_955_419_6;
    let k = tan(core::f64::consts::PI * f0 / f64::from(sample_rate));
    let vh = pow(10.0, gain_db / 20.0);
    let vb = pow(vh, 0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    Biquad64::new(
        [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
        ],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    )
}

/// The BS.1770-4 RLB weighting: a second-order high-pass at about 38 Hz.
fn high_pass(sample_rate: u32) -> Biquad64 {
    let f0 = 38.135_470_876_024_44;
    let q = 0.500_327_037_323_877_3;
    let k = tan(core::f64::consts::PI * f0 / f64::from(sample_rate));
    let a0 = 1.0 + k / q + k * k;
    Biquad64::new(
        [1.0, -2.0, 1.0],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    )
}

/// The loudness of a mean weighted power, in LUFS (BS.1770-4 eq. 2).
fn lufs(power: f64) -> f64 {
    -0.691 + 10.0 * log10(power)
}

fn decibels(amplitude: f64) -> f64 {
    20.0 * log10(amplitude)
}

/// One 100 ms step: weighted sum of squares and sample count.
#[derive(Copy, Clone, Debug, Default)]
struct Step {
    energy: f64,
    samples: u64,
}

/// One histogram bin: value count and summed power.
#[derive(Copy, Clone, Debug, Default)]
struct Bin {
    count: u64,
    power: f64,
}

/// Powers over the absolute gate, binned by loudness, with a running count and sum.
#[derive(Clone, Debug)]
struct Histogram {
    bins: Box<[Bin]>,
    count: u64,
    power: f64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            bins: vec![Bin::default(); BINS].into_boxed_slice(),
            count: 0,
            power: 0.0,
        }
    }

    /// Counts `power` if it is finite, positive and over the absolute gate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn add(&mut self, power: f64) {
        if !(power.is_finite() && power > 0.0) {
            return;
        }
        let loudness = lufs(power);
        if loudness <= ABSOLUTE_GATE_LUFS {
            return;
        }
        let index = (((loudness - ABSOLUTE_GATE_LUFS) * BINS_PER_LU) as usize).min(BINS - 1);
        let bin = &mut self.bins[index];
        bin.count += 1;
        bin.power += power;
        self.count += 1;
        self.power += power;
    }

    /// The loudness `relative` LU below the counted mean, but not under the absolute gate
    /// (BS.1770-4 eq. 7); `None` while empty.
    #[allow(clippy::cast_precision_loss)]
    fn relative_gate(&self, relative: f64) -> Option<f64> {
        (self.count > 0)
            .then(|| (lufs(self.power / self.count as f64) + relative).max(ABSOLUTE_GATE_LUFS))
    }

    /// The bins whose mean loudness is over `threshold`, quietest first.
    #[allow(clippy::cast_precision_loss)]
    fn over(&self, threshold: f64) -> impl Iterator<Item = &Bin> {
        self.bins
            .iter()
            .filter(move |bin| bin.count > 0 && lufs(bin.power / bin.count as f64) > threshold)
    }

    /// BS.1770-4 eq. 7: mean power over the absolute gate and the relative gate `relative` LU
    /// below.
    #[allow(clippy::cast_precision_loss)]
    fn gated_mean(&self, relative: f64) -> Option<f64> {
        let threshold = self.relative_gate(relative)?;
        let (power, count) = self
            .over(threshold)
            .fold((0.0, 0_u64), |(power, count), bin| {
                (power + bin.power, count + bin.count)
            });
        (count > 0).then(|| power / count as f64)
    }

    /// EBU Tech 3342: the 95th minus the 10th percentile (nearest rank) of gated loudness, each
    /// taken as its bin's mean.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn range(&self, relative: f64) -> Option<f64> {
        let threshold = self.relative_gate(relative)?;
        let count = self.over(threshold).map(|bin| bin.count).sum::<u64>();
        let last = count.checked_sub(1)? as f64;
        let at = |percentile: f64| {
            let rank = (last * percentile + 0.5) as u64;
            let mut below = 0;
            self.over(threshold)
                .find(|bin| {
                    below += bin.count;
                    below > rank
                })
                .map(|bin| lufs(bin.power / bin.count as f64))
        };
        Some(at(0.95)? - at(0.10)?)
    }
}

/// A streaming BS.1770-4 / EBU R128 loudness meter.
#[derive(Clone, Debug)]
pub struct LoudnessMeter {
    sample_rate: u32,
    channels: Vec<Channel>,
    /// Polyphase true-peak interpolator: `phases[p][tap]`.
    phases: Vec<[f64; TRUE_PEAK_TAPS]>,
    open: Step,
    step_index: u64,
    /// The most recent steps, newest last, at most a short-term window.
    recent: Vec<Step>,
    /// The mean powers of the complete 400 ms gating blocks.
    blocks: Histogram,
    /// The mean powers of the complete 3 s short-term windows.
    short_terms: Histogram,
    momentary_max: Option<f64>,
    short_term_max: Option<f64>,
    true_peak: f64,
    sample_peak: f64,
    frames: u64,
}

impl LoudnessMeter {
    /// A meter for `channels` channels in the [`SpeakerLayout::default_for`] layout, weighted as
    /// [`Self::for_speakers`] does.
    ///
    /// | Channels | Order | Weights |
    /// |---|---|---|
    /// | 1, 2 | C / L R | 1 |
    /// | 3 | L R LFE | 1 1 0 |
    /// | 4 | L R C Cs | 1 1 1 1 |
    /// | 5 | L R C Ls Rs | 1 1 1 1.41 1.41 |
    /// | 6 | L R C LFE Ls Rs | 1 1 1 0 1.41 1.41 |
    /// | 7 | L R C LFE Cs Ls Rs | 1 1 1 0 1 1.41 1.41 |
    /// | 8 | L R C LFE Lrs Rrs Lss Rss | 1 1 1 0 1 1 1.41 1.41 |
    ///
    /// Above eight, named layouts (5.1.4, 7.1.4, 9.1.4, 9.1.6, 22.2) weigh by speaker; other
    /// counts are 7.1 plus channels of weight 1.0.
    ///
    /// # Errors
    /// Returns [`LoudnessError::InvalidShape`] for zero channels, more than
    /// [`alpha2_media::MAX_AUDIO_CHANNELS`], or a rate outside 8 to 96 kHz.
    pub fn new(sample_rate: u32, channels: usize) -> Result<Self, LoudnessError> {
        let layout = SpeakerLayout::default_for(channels).ok_or(LoudnessError::InvalidShape)?;
        Self::for_speakers(sample_rate, layout.speakers())
    }

    /// A meter for planes feeding `speakers`, weighted per BS.1770-4 Annex 1 Table 4: LFE 0,
    /// ear-height surrounds 60 to 120 degrees off centre 1.41, everything else 1.0.
    ///
    /// Side speakers get 1.41, as do back speakers in layouts without sides (5.0 and 5.1
    /// surrounds, which `FFmpeg` calls back left and right). With sides present, the back pair is
    /// a 7.1 rear and weighs 1.0. Height, bottom and unnamed speakers weigh 1.0.
    ///
    /// # Errors
    /// Returns [`LoudnessError::InvalidShape`] for no speakers or a rate
    /// outside 8 to 96 kHz.
    pub fn for_speakers(sample_rate: u32, speakers: &[Speaker]) -> Result<Self, LoudnessError> {
        const SURROUND: f64 = 1.41;
        let has_sides = speakers
            .iter()
            .any(|speaker| matches!(speaker, Speaker::SideLeft | Speaker::SideRight));
        let weights = speakers
            .iter()
            .map(|speaker| match speaker {
                Speaker::LowFrequency | Speaker::LowFrequency2 => 0.0,
                Speaker::SideLeft
                | Speaker::SideRight
                | Speaker::SideSurroundLeft
                | Speaker::SideSurroundRight
                | Speaker::SurroundDirectLeft
                | Speaker::SurroundDirectRight
                | Speaker::WideLeft
                | Speaker::WideRight => SURROUND,
                Speaker::BackLeft | Speaker::BackRight if !has_sides => SURROUND,
                _ => 1.0,
            })
            .collect::<Vec<_>>();
        Self::with_weights(sample_rate, &weights)
    }

    /// A meter with one explicit weight per channel.
    ///
    /// # Errors
    /// Returns [`LoudnessError::InvalidShape`] for no weights, a negative or
    /// non-finite weight, or a rate outside 8 to 96 kHz.
    pub fn with_weights(sample_rate: u32, weights: &[f64]) -> Result<Self, LoudnessError> {
        if weights.is_empty()
            || weights
                .iter()
                .any(|weight| !weight.is_finite() || *weight < 0.0)
            || !(8_000..=96_000).contains(&sample_rate)
        {
            return Err(LoudnessError::InvalidShape);
        }
        let factor = if sample_rate < 96_000 { 4 } else { 2 };
        Ok(Self {
            sample_rate,
            channels: weights
                .iter()
                .map(|&weight| Channel {
                    weight,
                    shelf: shelf(sample_rate),
                    high_pass: high_pass(sample_rate),
                    history: [0.0; TRUE_PEAK_TAPS],
                })
                .collect(),
            phases: interpolator(factor),
            open: Step::default(),
            step_index: 0,
            recent: Vec::with_capacity(SHORT_TERM_STEPS),
            blocks: Histogram::new(),
            short_terms: Histogram::new(),
            momentary_max: None,
            short_term_max: None,
            true_peak: 0.0,
            sample_peak: 0.0,
            frames: 0,
        })
    }

    /// Measures one block of equal-length planes. Non-finite samples measure as silence.
    ///
    /// # Errors
    /// Returns [`LoudnessError::MismatchedBlock`] for another channel count or ragged planes.
    pub fn push(&mut self, planes: &[&[f32]]) -> Result<(), LoudnessError> {
        let Some(frames) = planes.first().map(|plane| plane.len()) else {
            return Err(LoudnessError::MismatchedBlock);
        };
        if planes.len() != self.channels.len() || planes.iter().any(|plane| plane.len() != frames) {
            return Err(LoudnessError::MismatchedBlock);
        }
        for index in 0..frames {
            let mut energy = 0.0;
            for (channel, plane) in self.channels.iter_mut().zip(planes) {
                // Non-finite samples would poison the recursive filters; treat them as silence,
                // as the limiter does.
                let sample = plane[index];
                let sample = if sample.is_finite() {
                    f64::from(sample)
                } else {
                    0.0
                };
                let weighted = channel.high_pass.process(channel.shelf.process(sample));
                energy += channel.weight * weighted * weighted;
                self.sample_peak = self.sample_peak.max(sample.abs());
                channel.history.copy_within(1.., 0);
                channel.history[TRUE_PEAK_TAPS - 1] = sample;
                for phase in &self.phases {
                    let interpolated = phase
                        .iter()
                        .zip(channel.history.iter().rev())
                        .fold(0.0, |sum, (tap, value)| sum + tap * value);
                    self.true_peak = self.true_peak.max(interpolated.abs());
                }
            }
            self.open.energy += energy;
            self.open.samples += 1;
            self.frames += 1;
            if self.frames == self.step_end() {
                self.close_step();
            }
        }
        Ok(())
    }

    /// The end of the current step at `floor(k * rate / 10)`, exact even at 11 025 Hz.
    fn step_end(&self) -> u64 {
        (self.step_index + 1) * u64::from(self.sample_rate) / STEPS_PER_SECOND
    }

    fn close_step(&mut self) {
        if self.recent.len() == SHORT_TERM_STEPS {
            self.recent.remove(0);
        }
        self.recent.push(self.open);
        self.open = Step::default();
        self.step_index += 1;
        if let Some(power) = self.window_power(MOMENTARY_STEPS) {
            self.blocks.add(power);
            let loudness = lufs(power);
            self.momentary_max = Some(self.momentary_max.map_or(loudness, |max| max.max(loudness)));
        }
        if let Some(power) = self.window_power(SHORT_TERM_STEPS) {
            self.short_terms.add(power);
            let loudness = lufs(power);
            self.short_term_max = Some(
                self.short_term_max
                    .map_or(loudness, |max| max.max(loudness)),
            );
        }
    }

    /// The mean power of the last `steps` complete steps, once available.
    #[allow(clippy::cast_precision_loss)]
    fn window_power(&self, steps: usize) -> Option<f64> {
        let window = self.recent.get(self.recent.len().checked_sub(steps)?..)?;
        let (energy, samples) = window.iter().fold((0.0, 0_u64), |(energy, samples), step| {
            (energy + step.energy, samples + step.samples)
        });
        (samples > 0).then(|| energy / samples as f64)
    }

    /// Momentary loudness of the last 400 ms, in LUFS.
    #[must_use]
    pub fn momentary_lufs(&self) -> Option<f64> {
        self.window_power(MOMENTARY_STEPS).map(lufs)
    }

    /// Short-term loudness of the last 3 s, in LUFS.
    #[must_use]
    pub fn short_term_lufs(&self) -> Option<f64> {
        self.window_power(SHORT_TERM_STEPS).map(lufs)
    }

    /// Everything measured so far.
    #[must_use]
    pub fn report(&self) -> LoudnessReport {
        let peak = |amplitude: f64| (amplitude > 0.0).then(|| decibels(amplitude));
        LoudnessReport {
            integrated_lufs: self.blocks.gated_mean(INTEGRATED_RELATIVE_GATE).map(lufs),
            loudness_range_lu: self.short_terms.range(RANGE_RELATIVE_GATE),
            true_peak_dbtp: peak(self.true_peak.max(self.sample_peak)),
            sample_peak_dbfs: peak(self.sample_peak),
            momentary_max_lufs: self.momentary_max,
            short_term_max_lufs: self.short_term_max,
            frames: self.frames,
        }
    }
}

/// The true-peak interpolator: a Hann-windowed sinc low-pass at the original Nyquist, `factor`
/// phases of `TRUE_PEAK_TAPS` taps, each normalized to unity gain.
#[allow(clippy::cast_precision_loss)]
fn interpolator(factor: usize) -> Vec<[f64; TRUE_PEAK_TAPS]> {
    let length = factor * TRUE_PEAK_TAPS;
    let centre = (length as f64 - 1.0) / 2.0;
    let prototype = (0..length)
        .map(|index| {
            let offset = (index as f64 - centre) / factor as f64;
            let sinc = if offset.abs() < 1e-12 {
                1.0
            } else {
                let angle = core::f64::consts::PI * offset;
                libm::sin(angle) / angle
            };
            let window = 0.5
                - 0.5
                    * libm::cos(2.0 * core::f64::consts::PI * (index as f64 + 0.5) / length as f64);
            sinc * window
        })
        .collect::<Vec<_>>();
    (0..factor)
        .map(|phase| {
            let mut taps = [0.0; TRUE_PEAK_TAPS];
            for (tap, value) in taps.iter_mut().enumerate() {
                *value = prototype[phase + tap * factor];
            }
            let sum = taps.iter().sum::<f64>();
            taps.map(|value| value / sum)
        })
        .collect()
}

/// Headroom in dB below a master limiter's threshold for a normalized mix's sample peak, so the
/// limiter never acts. It covers `f32` gain rounding (about 2e-6 dB) a thousand times over.
pub const LIMITER_MARGIN_DB: f64 = 0.01;

/// A static gain to a loudness target, bounded by a true-peak ceiling and any master limiter.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LoudnessNormalization {
    /// Gain to apply, in dB.
    pub gain_db: f64,
    /// Integrated loudness after the gain, in LUFS.
    pub integrated_lufs: f64,
    /// True peak after the gain, in dBTP.
    pub true_peak_dbtp: f64,
    /// Whether a peak bound (ceiling or limiter) set the gain, leaving the result under target.
    pub peak_limited: bool,
    /// Whether the master limiter threshold was the binding bound.
    pub limiter_limited: bool,
}

impl LoudnessNormalization {
    /// The gain that moves `report` to `target_lufs`, lowered so the true peak stays at or below
    /// `ceiling_dbtp` and, with a master limiter at linear `limiter_threshold`, the sample peak
    /// stays [`LIMITER_MARGIN_DB`] under it (the limiter detects sample peaks), so it never acts.
    ///
    /// `None` without integrated loudness, or for non-finite inputs or a non-positive threshold.
    #[must_use]
    pub fn plan(
        report: &LoudnessReport,
        target_lufs: f64,
        ceiling_dbtp: f64,
        limiter_threshold: Option<f64>,
    ) -> Option<Self> {
        let integrated = report.integrated_lufs.filter(|value| value.is_finite())?;
        if report.true_peak_dbtp.is_some_and(|peak| !peak.is_finite())
            || report
                .sample_peak_dbfs
                .is_some_and(|peak| !peak.is_finite())
            || limiter_threshold
                .is_some_and(|threshold| !(threshold.is_finite() && threshold > 0.0))
        {
            return None;
        }
        let to_target = target_lufs - integrated;
        let to_ceiling = report
            .true_peak_dbtp
            .map_or(f64::INFINITY, |peak| ceiling_dbtp - peak);
        let to_limiter = match (limiter_threshold, report.sample_peak_dbfs) {
            (Some(threshold), Some(peak)) => {
                20.0 * libm::log10(threshold) - LIMITER_MARGIN_DB - peak
            }
            _ => f64::INFINITY,
        };
        let gain_db = to_target.min(to_ceiling).min(to_limiter);
        Some(Self {
            gain_db,
            integrated_lufs: integrated + gain_db,
            true_peak_dbtp: report
                .true_peak_dbtp
                .map_or(f64::NEG_INFINITY, |peak| peak + gain_db),
            peak_limited: gain_db < to_target,
            limiter_limited: to_limiter < to_target.min(to_ceiling),
        })
    }

    /// The gain as a linear factor.
    #[must_use]
    pub fn linear_gain(&self) -> f64 {
        pow(10.0, self.gain_db / 20.0)
    }
}

#[cfg(test)]
#[path = "loudness_tests.rs"]
mod tests;
