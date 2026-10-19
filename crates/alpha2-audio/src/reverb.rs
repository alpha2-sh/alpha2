#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use crate::biquad::flush_denormal;
use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Allocation-free prepared Schroeder reverb with four combs and two allpasses.
#[derive(Clone, Debug)]
pub struct SchroederReverb {
    room_size: f32,
    /// The authored damping: the combs' one-pole coefficient at the
    /// 44.1 kHz their lengths are given at.
    damping: f32,
    /// [`Self::damping`] at the prepared rate, so the filter keeps its time
    /// constant in seconds as the lines keep their lengths.
    damping_coefficient: f32,
    wet: f32,
    combs: Vec<DelayLine>,
    allpasses: Vec<DelayLine>,
    bypass: bool,
}

/// The loudest mono sample the lines take, far past any audio and far
/// enough under `f32::MAX` that no comb or allpass state can overflow.
const MAX_LINE_INPUT: f32 = 1.0e30;

/// The rate the line lengths and the damping are given at.
const REFERENCE_RATE: f64 = 44_100.0;
/// Comb lengths at [`REFERENCE_RATE`], shortest first.
const COMB_LENGTHS: [usize; 4] = [1116, 1188, 1277, 1356];
/// Allpass lengths at [`REFERENCE_RATE`].
const ALLPASS_LENGTHS: [usize; 2] = [556, 441];

/// The rate the lines are sized for: `rate` itself, bounded by the highest
/// rate any host runs the DSP at (the `AudioWorklet` binding takes every
/// `AudioContext` rate up to 768 kHz), so a direct caller cannot size them
/// from an absurd rate. Every rate in that range keeps the lines' lengths in
/// seconds, so the echoes come at the same times; at 768 kHz the lines hold
/// about 103 000 samples (413 KB), four times what they hold at 192 kHz.
fn line_rate(rate: u32) -> u32 {
    rate.clamp(1, crate::MAX_RESAMPLE_RATE)
}

/// A line `length` samples long at [`REFERENCE_RATE`], at `rate`.
fn line_length(length: usize, rate: u32) -> usize {
    (length as f32 * (rate as f32 / REFERENCE_RATE as f32)).round() as usize
}

/// The one-pole coefficient that damps at `rate` as `damping` does at
/// [`REFERENCE_RATE`]: a coefficient d decays by d each sample, so
/// d^(44100 / rate) decays as much each 1/44100 s.
fn damping_at(damping: f32, rate: u32) -> f32 {
    if damping > 0.0 && damping < 1.0 {
        libm::pow(f64::from(damping), REFERENCE_RATE / f64::from(rate)) as f32
    } else {
        damping
    }
}

#[derive(Clone, Debug)]
struct DelayLine {
    samples: Vec<f32>,
    cursor: usize,
    filter: f32,
}

impl DelayLine {
    fn new(length: usize) -> Self {
        Self {
            samples: vec![0.0; length.max(1)],
            cursor: 0,
            filter: 0.0,
        }
    }
    fn comb(&mut self, input: f32, feedback: f32, damping: f32) -> f32 {
        let delayed = self.samples[self.cursor];
        self.filter = flush_denormal(delayed * (1.0 - damping) + self.filter * damping);
        self.samples[self.cursor] = flush_denormal(input + self.filter * feedback);
        self.cursor = (self.cursor + 1) % self.samples.len();
        delayed
    }
    fn allpass(&mut self, input: f32) -> f32 {
        let delayed = self.samples[self.cursor];
        let output = delayed - input;
        self.samples[self.cursor] = flush_denormal(input + delayed * 0.5);
        self.cursor = (self.cursor + 1) % self.samples.len();
        output
    }
}

impl SchroederReverb {
    /// Creates a reverb with room, damping, and wet controls clamped to the unit interval.
    /// Non-finite values fall back to 0.7, 0.3, and 0.25.
    #[must_use]
    pub fn new(room_size: f32, damping: f32, wet: f32) -> Self {
        Self {
            room_size: unit(room_size, 0.7),
            damping: unit(damping, 0.3),
            damping_coefficient: unit(damping, 0.3),
            wet: unit(wet, 0.25),
            combs: Vec::new(),
            allpasses: Vec::new(),
            bypass: false,
        }
    }
    /// Enables transparent bypass while the tail continues evolving.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }
    /// How long the reverb rings after its input stops, in samples at
    /// `sample_rate`, prepared or not: until its slowest frequencies (the
    /// lowest, which the damping filter passes) have fallen 60 dB through
    /// the longest comb, plus the allpasses' own 60 dB. A bound for these
    /// room and damping values, not a fixed length: a room of 1 rings about
    /// four seconds.
    #[must_use]
    pub fn tail_samples(&self, sample_rate: u32) -> u64 {
        let safe_rate = line_rate(sample_rate);
        let longest = line_length(COMB_LENGTHS[COMB_LENGTHS.len() - 1], safe_rate) as f64;
        let damping = f64::from(damping_at(self.damping, safe_rate));
        let comb = if damping >= 1.0 {
            // The filter stays at zero, so a comb only repeats its input once.
            longest
        } else {
            let feedback = f64::from(self.feedback());
            // Each cycle loses ln(feedback) at low frequencies and takes the line plus the filter's
            // group delay there.
            let cycles = libm::log(1_000.0) / -libm::log(feedback);
            longest + cycles * (longest + damping / (1.0 - damping))
        };
        // An allpass's feedback of 0.5 loses 60 dB in ten trips.
        let allpasses = ALLPASS_LENGTHS
            .into_iter()
            .map(|length| line_length(length, safe_rate) as f64 * 10.0)
            .sum::<f64>();
        libm::ceil(comb + allpasses) as u64
    }
    /// The combs' feedback gain.
    fn feedback(&self) -> f32 {
        0.65 + self.room_size * 0.3
    }
}

impl Processor for SchroederReverb {
    fn history_samples(&self) -> u32 {
        let delay = self
            .combs
            .iter()
            .map(|line| line.samples.len())
            .max()
            .unwrap_or(0);
        let delay = u32::try_from(delay).unwrap_or(u32::MAX);
        let comb = if self.damping_coefficient >= 1.0 {
            // The zero-initialized feedback filter never changes in this case.
            f64::from(delay)
        } else {
            let damping = f64::from(self.damping_coefficient);
            let feedback = f64::from(self.feedback());
            (f64::from(delay) + 1.0 / (1.0 - damping)) * 32.0 / (1.0 - feedback)
        };
        let comb = libm::ceil(comb) as u32;
        self.allpasses.iter().fold(comb, |total, line| {
            total.saturating_add(
                u32::try_from(line.samples.len())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(crate::processor::settling_samples(0.5)),
            )
        })
    }

    fn prepare(&mut self, sample_rate: u32, _max_block: usize) {
        let safe_rate = line_rate(sample_rate);
        self.combs = COMB_LENGTHS
            .into_iter()
            .map(|length| DelayLine::new(line_length(length, safe_rate)))
            .collect();
        self.allpasses = ALLPASS_LENGTHS
            .into_iter()
            .map(|length| DelayLine::new(line_length(length, safe_rate)))
            .collect();
        self.damping_coefficient = damping_at(self.damping, safe_rate);
    }
    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let channels = inputs.len().min(outputs.len()).min(2);
        let frames = inputs
            .iter()
            .take(channels)
            .map(|v| v.len())
            .chain(outputs.iter().take(channels).map(|v| v.len()))
            .min()
            .unwrap_or(0);
        if self.combs.is_empty() {
            return;
        }
        for frame in 0..frames {
            let mono = inputs[..channels]
                .iter()
                .map(|channel| channel[frame])
                .sum::<f32>()
                / channels.max(1) as f32;
            // The lines feed back on themselves, so a value they cannot hold would stay (as NaN)
            // until a reset. A non-finite sum is silence, and a finite one is held where no line
            // can overflow: a comb's gain is under 1 / (1 - 0.95) = 20 and an allpass's under 2.
            let mono = if mono.is_finite() {
                mono.clamp(-MAX_LINE_INPUT, MAX_LINE_INPUT)
            } else {
                0.0
            };
            let (feedback, damping) = (self.feedback(), self.damping_coefficient);
            let mut reverberant = self
                .combs
                .iter_mut()
                .map(|comb| comb.comb(mono, feedback, damping))
                .sum::<f32>()
                / self.combs.len() as f32;
            for allpass in &mut self.allpasses {
                reverberant = allpass.allpass(reverberant);
            }
            for channel in 0..channels {
                let input = finite_or_zero(inputs[channel][frame]);
                outputs[channel][frame] = if self.bypass {
                    input
                } else {
                    input * (1.0 - self.wet) + reverberant * self.wet
                };
            }
        }
    }
    fn reset(&mut self) {
        for delay in self.combs.iter_mut().chain(&mut self.allpasses) {
            delay.samples.fill(0.0);
            delay.cursor = 0;
            delay.filter = 0.0;
        }
    }
    fn save_state(&self) -> StateBlob {
        let mut bytes = Vec::new();
        for delay in self.combs.iter().chain(&self.allpasses) {
            bytes.extend_from_slice(&(u64::try_from(delay.cursor).unwrap_or(0)).to_le_bytes());
            bytes.extend_from_slice(&delay.filter.to_le_bytes());
            for sample in &delay.samples {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }
        StateBlob::new(bytes)
    }
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        let bytes = state.as_bytes();
        // Every line's part is its cursor, its filter, then its samples: check them all before
        // taking any.
        let expected = self
            .combs
            .iter()
            .chain(&self.allpasses)
            .map(|delay| 12 + delay.samples.len() * 4)
            .sum::<usize>();
        if bytes.len() != expected {
            return Err(StateError::Mismatch);
        }
        let mut offset = 0;
        for delay in self.combs.iter().chain(&self.allpasses) {
            let cursor = usize::try_from(u64::from_le_bytes(
                bytes[offset..offset + 8].try_into().expect("fixed range"),
            ))
            .ok();
            let values = bytes[offset + 8..offset + 12 + delay.samples.len() * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| f32::from_le_bytes(*word));
            if cursor.is_none_or(|value| value >= delay.samples.len())
                || values.clone().any(|value| !value.is_finite())
            {
                return Err(StateError::Invalid);
            }
            offset += 12 + delay.samples.len() * 4;
        }
        let mut offset = 0;
        for delay in self.combs.iter_mut().chain(&mut self.allpasses) {
            let mut values = bytes[offset + 8..offset + 12 + delay.samples.len() * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| f32::from_le_bytes(*word));
            delay.cursor = usize::try_from(u64::from_le_bytes(
                bytes[offset..offset + 8].try_into().expect("fixed range"),
            ))
            .unwrap_or(0);
            delay.filter = values.next().unwrap_or(0.0);
            for (target, value) in delay.samples.iter_mut().zip(values) {
                *target = value;
            }
            offset += 12 + delay.samples.len() * 4;
        }
        Ok(())
    }
}

fn unit(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::SchroederReverb;
    use crate::{ProcessCtx, Processor, TransportState};
    use alpha2_time::MediaTime;

    /// The impulse response of a fully wet reverb, `seconds` long.
    fn impulse_response(room_size: f32, damping: f32, sample_rate: u32, seconds: f64) -> Vec<f32> {
        let mut reverb = SchroederReverb::new(room_size, damping, 1.0);
        reverb.prepare(sample_rate, 512);
        let ctx = ProcessCtx {
            position: MediaTime::ZERO,
            sample_position: 0,
            sample_rate,
            transport: TransportState::Playing,
        };
        let frames = (f64::from(sample_rate) * seconds) as usize;
        let mut input = vec![0.0; frames];
        input[0] = 1.0;
        let mut output = vec![0.0; frames];
        for (input, output) in input.chunks(512).zip(output.chunks_mut(512)) {
            reverb.process(&ctx, &[input], &mut [output]);
        }
        output
    }

    /// How fast `response` decays around `frequency`, in dB a second:
    /// the energy through a fourth-order band-pass (two RBJ sections,
    /// Q 1.414) over 0.2 to 0.4 s against 0.6 to 0.8 s.
    fn band_decay_db_per_second(response: &[f32], sample_rate: u32, frequency: f64) -> f64 {
        let omega = core::f64::consts::TAU * frequency / f64::from(sample_rate);
        let alpha = libm::sin(omega) / (2.0 * 1.414);
        let a0 = 1.0 + alpha;
        let (b0, b2, a1, a2) = (
            alpha / a0,
            -alpha / a0,
            -2.0 * libm::cos(omega) / a0,
            (1.0 - alpha) / a0,
        );
        let mut signal = response
            .iter()
            .map(|&sample| f64::from(sample))
            .collect::<Vec<_>>();
        for _ in 0..2 {
            let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
            for sample in &mut signal {
                let y = b0 * *sample + b2 * x2 - a1 * y1 - a2 * y2;
                (x2, x1, y2, y1) = (x1, *sample, y1, y);
                *sample = y;
            }
        }
        let energy = |from: f64, to: f64| {
            let rate = f64::from(sample_rate);
            signal[(from * rate) as usize..(to * rate) as usize]
                .iter()
                .map(|sample| sample * sample)
                .sum::<f64>()
        };
        10.0 * libm::log10(energy(0.2, 0.4) / energy(0.6, 0.8)) / 0.4
    }

    #[test]
    fn a_room_decays_alike_in_every_band_at_any_project_rate() {
        for (room_size, damping) in [(0.7, 0.3), (0.7, 0.5), (0.9, 0.2)] {
            for frequency in [500.0, 5_000.0, 8_000.0] {
                let reference = band_decay_db_per_second(
                    &impulse_response(room_size, damping, 48_000, 1.0),
                    48_000,
                    frequency,
                );
                for sample_rate in [44_100, 96_000] {
                    let decay = band_decay_db_per_second(
                        &impulse_response(room_size, damping, sample_rate, 1.0),
                        sample_rate,
                        frequency,
                    );
                    let difference = (decay / reference - 1.0).abs();
                    assert!(
                        difference < 0.08,
                        "room {room_size} damping {damping} at {frequency} Hz: {decay:.1} dB/s at {sample_rate} Hz, {reference:.1} at 48 kHz"
                    );
                }
            }
        }
    }

    #[test]
    fn the_tail_bound_outlasts_every_room_without_overstating_it() {
        for sample_rate in [44_100, 48_000, 96_000] {
            for room_size in [1.0, 0.95, 0.9, 0.5, 0.0] {
                for damping in [0.0, 0.3, 0.7, 1.0] {
                    let response = impulse_response(room_size, damping, sample_rate, 5.5);
                    let peak = response
                        .iter()
                        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                    // The last sample within 60 dB of the peak.
                    let rings = response
                        .iter()
                        .rposition(|sample| sample.abs() >= peak * 1.0e-3)
                        .expect("a response") as u64;
                    assert!(
                        rings < response.len() as u64 - u64::from(sample_rate) / 4,
                        "the response is long enough to measure"
                    );
                    let mut reverb = SchroederReverb::new(room_size, damping, 1.0);
                    let tail = reverb.tail_samples(sample_rate);
                    reverb.prepare(sample_rate, 512);
                    assert_eq!(reverb.tail_samples(sample_rate), tail, "prepared or not");
                    assert!(
                        rings <= tail,
                        "room {room_size} damping {damping} at {sample_rate} Hz rings {rings} samples, past {tail}"
                    );
                    assert!(
                        tail <= rings * 3,
                        "room {room_size} damping {damping} at {sample_rate} Hz rings {rings} samples, far under {tail}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_decaying_tail_never_leaves_subnormal_state() {
        // A small, bright room empties its combs within seconds, after which the allpasses only
        // halve what they hold.
        let mut reverb = SchroederReverb::new(0.0, 0.0, 1.0);
        reverb.prepare(48_000, 128);
        let ctx = ProcessCtx {
            position: MediaTime::ZERO,
            sample_position: 0,
            sample_rate: 48_000,
            transport: TransportState::Playing,
        };
        let burst = [0.5_f32; 128];
        let silence = [0.0_f32; 128];
        let mut output = [0.0_f32; 128];
        reverb.process(&ctx, &[&burst], &mut [&mut output]);
        for block in 0..3_750 {
            reverb.process(&ctx, &[&silence], &mut [&mut output]);
            let subnormal = reverb
                .combs
                .iter()
                .chain(&reverb.allpasses)
                .flat_map(|line| line.samples.iter().chain([&line.filter]))
                .find(|value| value.is_subnormal());
            assert_eq!(subnormal, None, "block {block}");
        }
    }
}
