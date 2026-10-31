#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use core::f64::consts::PI;

use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Lowest frequency a filter renders, in hertz.
pub const MIN_FREQUENCY_HZ: f32 = 10.0;

/// Highest frequency a filter renders, as a fraction of the sample rate.
pub const MAX_FREQUENCY_FRACTION: f32 = 0.49;

/// The resonances a filter renders.
pub const Q_RANGE: core::ops::RangeInclusive<f32> = 0.05..=50.0;

/// Largest peaking gain a filter renders, in decibels either way.
pub const MAX_PEAKING_GAIN_DB: f32 = 36.0;

/// Common parametric equalizer shapes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BiquadKind {
    /// Bell-shaped parametric band.
    Peaking,
    /// Second-order low-pass.
    LowPass,
    /// Second-order high-pass.
    HighPass,
}

/// Two-channel transposed-direct-form-II biquad.
///
/// Coefficients and recursion are `f64`; samples are `f32`. Low-band poles sit within about
/// `2 * pi * f / rate` of the unit circle, which `f32` cannot resolve (a 10 Hz low-pass at 96 kHz
/// passes DC at +1.2 dB, and at 384 kHz the poles reach the circle). In `f64` every band up to
/// 768 kHz is stable and within 0.1 dB of its design.
#[derive(Clone, Debug)]
pub struct Biquad {
    kind: BiquadKind,
    frequency: f32,
    q: f32,
    gain_db: f32,
    /// `b0, b1, b2, a1, a2`, normalized by `a0`.
    coefficients: [f64; 5],
    z1: [f64; 2],
    z2: [f64; 2],
}

impl Biquad {
    /// Creates a parametric filter.
    ///
    /// [`Processor::prepare`] replaces non-finite parameters with 1 kHz, Q 0.707 and 0 dB, then
    /// clamps them to this module's ranges.
    #[must_use]
    pub fn new(kind: BiquadKind, frequency: f32, q: f32, gain_db: f32) -> Self {
        Self {
            kind,
            frequency,
            q,
            gain_db,
            coefficients: [1.0, 0.0, 0.0, 0.0, 0.0],
            z1: [0.0; 2],
            z2: [0.0; 2],
        }
    }

    fn update(&mut self, sample_rate: u32) {
        // Below about 21 Hz the Nyquist cap falls under the floor; the floor wins so `clamp`
        // cannot panic.
        let ceiling = (sample_rate as f32 * MAX_FREQUENCY_FRACTION).max(MIN_FREQUENCY_HZ);
        let frequency = finite(self.frequency, 1_000.0).clamp(MIN_FREQUENCY_HZ, ceiling);
        let q = finite(self.q, 0.707).clamp(*Q_RANGE.start(), *Q_RANGE.end());
        let omega = 2.0 * PI * f64::from(frequency) / f64::from(sample_rate.max(1));
        let (sin, cos) = libm::sincos(omega);
        // 1 - cos(omega) = 2 sin^2(omega / 2), avoiding cancellation at low frequencies.
        let half_sin = libm::sin(omega / 2.0);
        let one_minus_cos = 2.0 * half_sin * half_sin;
        let alpha = sin / (2.0 * f64::from(q));
        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            BiquadKind::LowPass => (
                one_minus_cos / 2.0,
                one_minus_cos,
                one_minus_cos / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BiquadKind::HighPass => (
                1.0_f64.midpoint(cos),
                -(1.0 + cos),
                1.0_f64.midpoint(cos),
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BiquadKind::Peaking => {
                let amplitude = libm::pow(
                    10.0,
                    f64::from(
                        finite(self.gain_db, 0.0).clamp(-MAX_PEAKING_GAIN_DB, MAX_PEAKING_GAIN_DB),
                    ) / 40.0,
                );
                (
                    1.0 + alpha * amplitude,
                    -2.0 * cos,
                    1.0 - alpha * amplitude,
                    1.0 + alpha / amplitude,
                    -2.0 * cos,
                    1.0 - alpha / amplitude,
                )
            }
        };
        self.coefficients = [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0];
    }
}

impl Biquad {
    /// Runs one channel's block. When `guarded`, non-finite inputs and outputs that overflow
    /// `f32` play as silence and clear the state.
    fn run(&mut self, channel: usize, input: &[f32], output: &mut [f32], guarded: bool) {
        let [b0, b1, b2, a1, a2] = self.coefficients;
        for (&input, output) in input.iter().zip(output.iter_mut()) {
            let input = f64::from(if guarded { finite(input, 0.0) } else { input });
            let value = flush_denormal_f64(input * b0 + self.z1[channel]);
            let z1 = flush_denormal_f64(input * b1 + self.z2[channel] - a1 * value);
            let z2 = flush_denormal_f64(input * b2 - a2 * value);
            let sample = value as f32;
            if guarded && !(sample.is_finite() && z1.is_finite() && z2.is_finite()) {
                self.z1[channel] = 0.0;
                self.z2[channel] = 0.0;
                *output = 0.0;
            } else {
                self.z1[channel] = z1;
                self.z2[channel] = z2;
                *output = sample;
            }
        }
    }
}

impl Processor for Biquad {
    fn history_samples(&self) -> u32 {
        let a1 = self.coefficients[3];
        let a2 = self.coefficients[4];
        let discriminant = a1 * a1 - 4.0 * a2;
        let radius = if discriminant < 0.0 {
            libm::sqrt(a2.abs())
        } else {
            a1.abs().midpoint(libm::sqrt(discriminant))
        };
        // Extra margin for the polynomial term of a repeated pole.
        crate::processor::settling_samples(radius).saturating_mul(2)
    }

    fn prepare(&mut self, sample_rate: u32, _max_block: usize) {
        self.update(sample_rate);
    }

    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for channel in 0..inputs.len().min(outputs.len()).min(2) {
            let state = (self.z1[channel], self.z2[channel]);
            self.run(channel, inputs[channel], outputs[channel], false);
            // A non-finite or overflowing sample would poison the recursion, so rerun the block
            // with such samples silenced.
            let outputs_finite = outputs[channel]
                [..inputs[channel].len().min(outputs[channel].len())]
                .iter()
                .all(|sample| sample.is_finite());
            if !(outputs_finite && self.z1[channel].is_finite() && self.z2[channel].is_finite()) {
                (self.z1[channel], self.z2[channel]) = state;
                self.run(channel, inputs[channel], outputs[channel], true);
            }
        }
    }

    fn reset(&mut self) {
        self.z1 = [0.0; 2];
        self.z2 = [0.0; 2];
    }
    fn save_state(&self) -> StateBlob {
        let mut bytes = Vec::with_capacity(32);
        for value in self.z1.into_iter().chain(self.z2) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        StateBlob::new(bytes)
    }
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        let bytes: &[u8; 32] = state
            .as_bytes()
            .try_into()
            .map_err(|_| StateError::Mismatch)?;
        let values: [f64; 4] = core::array::from_fn(|index| {
            f64::from_le_bytes(
                bytes[index * 8..index * 8 + 8]
                    .try_into()
                    .expect("fixed range"),
            )
        });
        if values.iter().any(|value| !value.is_finite()) {
            return Err(StateError::Invalid);
        }
        self.z1 = [values[0], values[1]];
        self.z2 = [values[2], values[3]];
        Ok(())
    }
}

fn finite(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}
pub(crate) fn flush_denormal(value: f32) -> f32 {
    if value.abs() < 1.0e-30 { 0.0 } else { value }
}
/// [`flush_denormal`] for the `f64` state, so a decayed tail never reaches subnormal values.
fn flush_denormal_f64(value: f64) -> f64 {
    if value.abs() < 1.0e-30 { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

    use super::{Biquad, BiquadKind};
    use crate::{ProcessCtx, Processor, TransportState};

    /// Project rates (8 to 96 kHz) and `AudioWorklet` rates (3 to 768 kHz).
    const RATES: [u32; 11] = [
        3_000, 8_000, 11_025, 22_050, 44_100, 48_000, 88_200, 96_000, 192_000, 384_000, 768_000,
    ];

    /// The radius of the filter's larger pole.
    fn pole_radius(filter: &Biquad) -> f64 {
        let a1 = filter.coefficients[3];
        let a2 = filter.coefficients[4];
        let discriminant = a1 * a1 - 4.0 * a2;
        if discriminant < 0.0 {
            libm::sqrt(a2.abs())
        } else {
            a1.abs().midpoint(libm::sqrt(discriminant))
        }
    }

    fn run(filter: &mut Biquad, rate: u32, input: &[f32]) -> Vec<f32> {
        let mut output = vec![0.0; input.len()];
        for (block, (input, output)) in input.chunks(128).zip(output.chunks_mut(128)).enumerate() {
            let sample_position = i64::try_from(block * 128).expect("position");
            let ctx = ProcessCtx {
                position: alpha2_time::MediaTime(0),
                sample_position,
                sample_rate: rate,
                transport: TransportState::Playing,
            };
            filter.process(&ctx, &[input], &mut [output]);
        }
        output
    }

    /// Measured gain in dB where the ideal is exact: low-pass at DC, high-pass at Nyquist, and a
    /// peaking band at its centre.
    fn measured_gain_db(kind: BiquadKind, frequency: f32, q: f32, gain_db: f32, rate: u32) -> f64 {
        let mut filter = Biquad::new(kind, frequency, q, gain_db);
        filter.prepare(rate, 128);
        // Long enough for the slower pole to fall by e^-20, then a second of signal to measure.
        let settle = libm::ceil(20.0 / -libm::log(pole_radius(&filter))) as usize;
        let measure = rate as usize;
        let input: Vec<f32> = (0..settle + measure)
            .map(|index| match kind {
                BiquadKind::LowPass => 0.25,
                BiquadKind::HighPass => {
                    if index % 2 == 0 {
                        0.25
                    } else {
                        -0.25
                    }
                }
                BiquadKind::Peaking => {
                    let phase = core::f64::consts::TAU * f64::from(frequency) * index as f64
                        / f64::from(rate);
                    (0.25 * libm::sin(phase)) as f32
                }
            })
            .collect();
        let output = run(&mut filter, rate, &input);
        let energy = |samples: &[f32]| {
            samples
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
        };
        // No phase shift at a peaking band's centre, so energies compare exactly.
        10.0 * libm::log10(energy(&output[settle..]) / energy(&input[settle..]))
    }

    #[test]
    fn every_filter_is_stable_and_accurate_at_every_rate() {
        let mut failures = Vec::new();
        for rate in RATES {
            for kind in [
                BiquadKind::LowPass,
                BiquadKind::HighPass,
                BiquadKind::Peaking,
            ] {
                for frequency in [10.0, 20.0, 40.0] {
                    for q in [0.05, 0.5, 0.707, 1.0, 4.0, 50.0] {
                        let mut filter = Biquad::new(kind, frequency, q, 12.0);
                        filter.prepare(rate, 128);
                        let radius = pole_radius(&filter);
                        if radius >= 1.0 {
                            failures.push(format!(
                                "{rate} Hz {kind:?} {frequency} Hz Q {q}: pole radius {radius}"
                            ));
                            continue;
                        }
                        // Extreme resonances settle too slowly; their poles are checked above.
                        if !(0.5..=4.0).contains(&q) {
                            continue;
                        }
                        let expected = if kind == BiquadKind::Peaking {
                            12.0
                        } else {
                            0.0
                        };
                        let gain = measured_gain_db(kind, frequency, q, 12.0, rate);
                        if (gain - expected).abs() > 0.1 {
                            failures.push(format!(
                                "{rate} Hz {kind:?} {frequency} Hz Q {q}: {gain:.3} dB, not {expected} dB"
                            ));
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
