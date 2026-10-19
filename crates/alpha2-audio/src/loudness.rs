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
//! Filtering and sums run in `f64`; logarithms and the filter design use
//! `libm`, so a measurement is bit-identical natively and as wasm.
//!
//! The gated values (integrated loudness, loudness range) come from
//! histograms of the 400 ms blocks and 3 s windows, 0.01 LU a bin from the
//! absolute gate to +30 LUFS, each bin keeping its count and its summed
//! power (libebur128's histogram mode, with sums). A meter takes them when it
//! is made and never grows, however long it measures, and measuring never
//! allocates. Means over whole bins are exact; a bin the relative gate
//! falls in counts or not by the loudness of its own mean, and a percentile
//! of the range is the loudness of its bin's mean. Against keeping every
//! value, integrated loudness stays within 0.01 LU and the range within
//! 0.02 LU (two percentiles); EBU Tech 3341 and 3342 allow 0.1 and 1 LU.

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
/// Histogram bins: from the absolute gate to +30 LUFS. A louder value
/// (only far past full scale) counts in the top bin.
const BINS: usize = 10_000;
/// A K-weighting filter output under this is silence (as in the `f32`
/// processors): about -600 dBFS.
const FLUSH_BELOW: f64 = 1.0e-30;

/// What a [`LoudnessMeter`] has measured.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LoudnessReport {
    /// Gated integrated loudness in LUFS; `None` while no block passes the
    /// absolute gate (silence).
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
    /// The last `TRUE_PEAK_TAPS` input samples, newest last.
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
        // In digital silence the recursion would settle in the slow subnormal range; this is
        // hundreds of LU under the absolute gate, so no gated value moves.
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

/// The BS.1770-4 pre-filter (a high shelf of about +4 dB above 1.5 kHz) for
/// `sample_rate`.
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

/// The BS.1770-4 RLB weighting (a second-order high-pass at about 38 Hz) for
/// `sample_rate`.
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

/// An amplitude's level in decibels relative to full scale.
fn decibels(amplitude: f64) -> f64 {
    20.0 * log10(amplitude)
}

/// One 100 ms step: its weighted sum of squares and its sample count.
#[derive(Copy, Clone, Debug, Default)]
struct Step {
    energy: f64,
    samples: u64,
}

/// One histogram bin: how many values fell in it and their summed power.
#[derive(Copy, Clone, Debug, Default)]
struct Bin {
    count: u64,
    power: f64,
}

/// The powers over the absolute gate, binned by loudness (see the module
/// documentation), with their count and sum in arrival order.
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

    /// Counts `power` if it is over the absolute gate. A power that is not a
    /// finite positive number (no meter fed finite samples makes one) is left
    /// out.
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

    /// The loudness `relative` LU below the mean of everything counted, but
    /// never under the absolute gate (BS.1770-4 eq. 7 counts only the values
    /// over both gates); `None` while nothing is counted.
    #[allow(clippy::cast_precision_loss)]
    fn relative_gate(&self, relative: f64) -> Option<f64> {
        (self.count > 0)
            .then(|| (lufs(self.power / self.count as f64) + relative).max(ABSOLUTE_GATE_LUFS))
    }

    /// The bins over `threshold`, quietest first: a bin counts whole when
    /// the loudness of its mean is over it.
    #[allow(clippy::cast_precision_loss)]
    fn over(&self, threshold: f64) -> impl Iterator<Item = &Bin> {
        self.bins
            .iter()
            .filter(move |bin| bin.count > 0 && lufs(bin.power / bin.count as f64) > threshold)
    }

    /// BS.1770-4 eq. 7: the mean power of the values over the absolute gate
    /// and over the relative gate `relative` LU below their own mean.
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

    /// EBU Tech 3342: the 95th minus the 10th percentile (nearest rank) of
    /// the loudness over the absolute gate and the relative gate `relative`
    /// LU below, each the loudness of its bin's mean.
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
    /// Index of the step being filled.
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
    /// A meter for `channels` channels at `sample_rate`, in the layout a
    /// source of that many channels that declares none has
    /// ([`SpeakerLayout::default_for`]: `FFmpeg`'s default order, which the
    /// mix folds such a source by), weighted as [`Self::for_speakers`]
    /// weighs it.
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
    /// Above eight, the named layouts (5.1.4, 7.1.4, 9.1.4, 9.1.6, 22.2)
    /// weigh by their speakers too, and a count with none is a 7.1 whose
    /// further channels weigh 1.0.
    ///
    /// # Errors
    /// Returns [`LoudnessError::InvalidShape`] for zero channels, more than
    /// [`alpha2_media::MAX_AUDIO_CHANNELS`], or a rate outside 8 to 96 kHz.
    pub fn new(sample_rate: u32, channels: usize) -> Result<Self, LoudnessError> {
        let layout = SpeakerLayout::default_for(channels).ok_or(LoudnessError::InvalidShape)?;
        Self::for_speakers(sample_rate, layout.speakers())
    }

    /// A meter for planes feeding `speakers`, weighted as BS.1770-4 (Annex 1,
    /// Table 4) weighs them: the LFE does not count, a surround 60 to 120
    /// degrees off centre at ear height weighs 1.41, and every other
    /// speaker 1.0.
    ///
    /// The side speakers sit there (90 to 110 degrees), and so do the back
    /// pair of a layout without sides: the surrounds of 5.0 and 5.1, which
    /// `FFmpeg` names back left and right. Beside sides, the back pair is a
    /// 7.1's rear (135 to 150 degrees) and weighs 1.0. A height or bottom
    /// speaker is 30 degrees off ear height and weighs 1.0, as does a
    /// speaker no position names.
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

    /// Measures one block of planar samples, one slice per channel, all of
    /// one length. An infinite or NaN sample measures as silence.
    ///
    /// # Errors
    /// Returns [`LoudnessError::MismatchedBlock`] for another channel count
    /// or ragged planes.
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
                // A NaN measures as silence, as the limiter after the meter plays it: the recursive
                // filters would carry it to the end, and an infinite power has no loudness.
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

    /// The sample after the step being filled: steps split each second
    /// into ten, `floor(k * rate / 10)`, so they stay exact at 11 025 Hz.
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

    /// The mean power of the last `steps` complete steps, once there are
    /// that many.
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

/// The polyphase interpolator of the true-peak oversampler: a low-pass at the
/// original Nyquist frequency, Hann-windowed sinc, `factor` phases of
/// `TRUE_PEAK_TAPS` taps, each phase normalized to unity gain.
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

/// How far below a master limiter's threshold a normalized mix keeps its
/// sample peak, in dB, so the limiter (which acts only on a sample above its
/// threshold) leaves it alone. It covers the rounding of the 32-bit gain and
/// of each sample multiplied by it (a few parts in 10^7, about 2e-6 dB) a
/// thousand times over, and is far below an audible level change.
pub const LIMITER_MARGIN_DB: f64 = 0.01;

/// A static gain that brings a measured mix to a loudness target without
/// letting its true peak exceed a ceiling, or its sample peak reach a master
/// limiter after it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LoudnessNormalization {
    /// Gain to apply, in dB.
    pub gain_db: f64,
    /// Integrated loudness after the gain, in LUFS.
    pub integrated_lufs: f64,
    /// True peak after the gain, in dBTP.
    pub true_peak_dbtp: f64,
    /// Whether a peak bound, not the target, set the gain: the true-peak
    /// ceiling or the limiter ([`Self::limiter_limited`]). The result is
    /// quieter than the target.
    pub peak_limited: bool,
    /// Whether the master limiter's threshold was the bound that set the
    /// gain, below what the ceiling allows.
    pub limiter_limited: bool,
}

impl LoudnessNormalization {
    /// The gain that moves `report` to `target_lufs`, lowered if needed so
    /// the true peak stays at or below `ceiling_dbtp` and, when the mix goes
    /// on through a master limiter whose linear threshold is
    /// `limiter_threshold`, so the sample peak stays [`LIMITER_MARGIN_DB`]
    /// below that threshold. The limiter then never acts on the normalized
    /// mix, which comes out at the planned loudness. The limiter bound is on
    /// the sample peak because that is what the limiter detects.
    ///
    /// `None` when the measured audio has no integrated loudness (it is
    /// silent below the absolute gate), or when the report holds a loudness
    /// or peak, or the threshold is, not a finite (and for the threshold,
    /// positive) number, which no gain can be planned from.
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
