#![allow(clippy::cast_precision_loss)]

use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Largest ratio a compressor renders.
pub const MAX_RATIO: f32 = 100.0;

/// Linked-channel compressor with deterministic lookahead and soft attack/release.
#[derive(Clone, Debug)]
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    lookahead_ms: f32,
    envelope: f32,
    attack_coefficient: f32,
    release_coefficient: f32,
    /// The threshold as a linear amplitude: an envelope at or under it is
    /// not compressed, which spares the logarithm.
    threshold_amplitude: f32,
    delay: [Vec<f32>; 2],
    cursor: usize,
    latency: u32,
    bypass: bool,
}

impl Compressor {
    /// Creates a compressor; invalid parameters are replaced by conservative defaults.
    #[must_use]
    pub fn new(
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        lookahead_ms: f32,
    ) -> Self {
        Self {
            threshold_db: finite(threshold_db, -12.0).clamp(-96.0, 0.0),
            ratio: finite(ratio, 4.0).clamp(1.0, MAX_RATIO),
            attack_ms: positive(attack_ms, 10.0),
            release_ms: positive(release_ms, 100.0),
            lookahead_ms: finite(lookahead_ms, 0.0).clamp(0.0, 100.0),
            envelope: 0.0,
            attack_coefficient: 0.0,
            release_coefficient: 0.0,
            threshold_amplitude: 0.0,
            delay: [Vec::new(), Vec::new()],
            cursor: 0,
            latency: 0,
            bypass: false,
        }
    }

    /// Enables latency-preserving bypass.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }
}

impl Processor for Compressor {
    fn prepare(&mut self, sample_rate: u32, _max_block: usize) {
        let rate = sample_rate.max(1) as f32;
        self.attack_coefficient = libm::expf(-1.0 / (self.attack_ms * 0.001 * rate));
        self.release_coefficient = libm::expf(-1.0 / (self.release_ms * 0.001 * rate));
        // Rounded down a little, so an envelope this passes is never one whose level in decibels
        // lies above the threshold: the decibel comparison below still decides.
        self.threshold_amplitude = libm::exp10f(self.threshold_db / 20.0) * (1.0 - 1.0e-4);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.latency = (self.lookahead_ms * 0.001 * rate).round() as u32;
        }
        let size = usize::try_from(self.latency).unwrap_or(0).max(1);
        self.delay = [vec![0.0; size], vec![0.0; size]];
        self.cursor = 0;
    }

    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let channels = inputs.len().min(outputs.len()).min(2);
        let frames = inputs
            .iter()
            .take(channels)
            .map(|input| input.len())
            .chain(outputs.iter().take(channels).map(|output| output.len()))
            .min()
            .unwrap_or(0);
        if self.delay[0].is_empty() {
            return;
        }
        for frame in 0..frames {
            let peak = inputs[..channels]
                .iter()
                .map(|channel| finite(channel[frame], 0.0).abs())
                .fold(0.0_f32, f32::max);
            if !self.envelope.is_finite() {
                self.envelope = 0.0;
            }
            let coefficient = if peak > self.envelope {
                self.attack_coefficient
            } else {
                self.release_coefficient
            };
            self.envelope = coefficient * self.envelope + (1.0 - coefficient) * peak;
            // Released to silence, the envelope would sink into the slow subnormal range; this
            // floor is far below the 1e-12 one below, so the gain never sees it.
            if self.envelope < 1.0e-30 {
                self.envelope = 0.0;
            }
            // Under the threshold (silence included) the gain is exactly one without a logarithm.
            // Over it, `exp10f` turns the reduction into a gain: as bit-identical across platforms
            // as `powf(10, x)`, which costs about nine times as much.
            let reduction_db = if self.envelope <= self.threshold_amplitude {
                0.0
            } else {
                let level_db = 20.0 * libm::log10f(self.envelope.max(1.0e-12));
                if level_db > self.threshold_db {
                    (self.threshold_db + (level_db - self.threshold_db) / self.ratio) - level_db
                } else {
                    0.0
                }
            };
            let gain = if self.bypass || reduction_db == 0.0 {
                1.0
            } else {
                libm::exp10f(reduction_db / 20.0)
            };
            for channel in 0..channels {
                let input = finite(inputs[channel][frame], 0.0);
                let delayed = if self.latency == 0 {
                    input
                } else {
                    let delayed = self.delay[channel][self.cursor];
                    self.delay[channel][self.cursor] = input;
                    finite(delayed, 0.0)
                };
                outputs[channel][frame] = delayed * gain;
            }
            if self.latency != 0 {
                self.cursor = (self.cursor + 1) % self.delay[0].len();
            }
        }
    }

    fn reset(&mut self) {
        self.envelope = 0.0;
        self.cursor = 0;
        for channel in &mut self.delay {
            channel.fill(0.0);
        }
    }
    fn latency_samples(&self) -> u32 {
        self.latency
    }
    fn history_samples(&self) -> u32 {
        if self.ratio <= 1.0 {
            return 0;
        }
        crate::processor::settling_samples(f64::from(
            self.attack_coefficient.max(self.release_coefficient),
        ))
    }
    fn save_state(&self) -> StateBlob {
        let mut bytes = Vec::with_capacity(16 + self.delay.iter().map(Vec::len).sum::<usize>() * 4);
        bytes.extend_from_slice(&self.envelope.to_le_bytes());
        bytes.extend_from_slice(&(u64::try_from(self.cursor).unwrap_or(0)).to_le_bytes());
        bytes.extend_from_slice(&self.latency.to_le_bytes());
        for channel in &self.delay {
            for value in channel {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        StateBlob::new(bytes)
    }
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        let bytes = state.as_bytes();
        let expected = 16 + self.delay.iter().map(Vec::len).sum::<usize>() * 4;
        if bytes.len() != expected {
            return Err(StateError::Mismatch);
        }
        let envelope = f32::from_le_bytes(bytes[0..4].try_into().expect("fixed range"));
        let cursor = usize::try_from(u64::from_le_bytes(
            bytes[4..12].try_into().expect("fixed range"),
        ))
        .ok();
        let latency = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed range"));
        if latency != self.latency {
            return Err(StateError::Mismatch);
        }
        let values = bytes[16..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| f32::from_le_bytes(*word));
        if !envelope.is_finite()
            || envelope < 0.0
            || cursor.is_none_or(|value| value >= self.delay[0].len())
            || values.clone().any(|value| !value.is_finite())
        {
            return Err(StateError::Invalid);
        }
        self.envelope = envelope;
        self.cursor = cursor.unwrap_or(0);
        for (target, value) in self.delay.iter_mut().flatten().zip(values) {
            *target = value;
        }
        Ok(())
    }
}

fn finite(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}
fn positive(value: f32, fallback: f32) -> f32 {
    let value = finite(value, fallback);
    if value > 0.0 { value } else { fallback }
}
