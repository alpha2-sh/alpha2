#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_range_loop
)]

use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Deterministic two-tap delay-line pitch shifter with a fixed prepared
/// window (20 ms).
///
/// It does not change duration: a host that plays a source `rate` times as
/// fast (which also transposes it by `rate`) runs it through this processor
/// to transpose it by `pitch / rate`, so the result plays at `rate` with its
/// pitch scaled by `pitch`. Two taps read the input's recent history at
/// delays that sweep one window at the transposition's speed, half a window
/// apart; each fades to nothing where its sweep wraps, so neither jumps
/// while it is heard, and their weights sum to one. Untransposed
/// (`pitch == rate`), the output is the input delayed by exactly
/// [`Processor::latency_samples`], one window, which is also the mean delay
/// of a transposed output and does not depend on the ratios. Hosts provide
/// continuous project-rate blocks and may save/restore the complete history.
#[derive(Clone, Debug)]
pub struct TimeStretchProcessor {
    rate: f32,
    pitch: f32,
    history: [Vec<f32>; 2],
    write: usize,
    /// Where the first tap is in its sweep, in `[0, window)`: its delay is
    /// half a window more than this.
    sweep: f64,
    window: usize,
    bypass: bool,
}

impl TimeStretchProcessor {
    /// Creates a processor. Both ratios are clamped to 0.25 to 4.0; a non-finite
    /// ratio becomes 1.0.
    #[must_use]
    pub fn new(rate: f32, pitch: f32) -> Self {
        Self {
            rate: ratio(rate),
            pitch: ratio(pitch),
            history: [Vec::new(), Vec::new()],
            write: 0,
            sweep: 0.0,
            window: 0,
            bypass: false,
        }
    }
    /// Updates time and pitch ratios without resetting history. The latency
    /// does not change.
    pub fn set_ratios(&mut self, rate: f32, pitch: f32) {
        self.rate = ratio(rate);
        self.pitch = ratio(pitch);
    }
    /// Enables latency-preserving bypass: the input delayed by
    /// [`Processor::latency_samples`], while the history keeps following it.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }
    /// The sweep position an untransposed output holds: the first tap at
    /// one window's delay, at full weight.
    fn centre(&self) -> f64 {
        self.window as f64 / 2.0
    }
    /// Reads `channel` `delay` samples before the sample just written,
    /// interpolating linearly.
    fn read(&self, channel: usize, delay: f64) -> f32 {
        let history = &self.history[channel];
        let capacity = history.len();
        let position = (self.write as f64 + capacity as f64 - delay).rem_euclid(capacity as f64);
        let first = position.floor() as usize % capacity;
        let second = (first + 1) % capacity;
        let fraction = (position - position.floor()) as f32;
        history[first] + (history[second] - history[first]) * fraction
    }
}

/// A tap's weight at sweep position `sweep` of `window`: nothing where the
/// sweep wraps, all at its middle. Two taps half a window apart sum to one.
fn weight(sweep: f64, window: f64) -> f32 {
    (1.0 - (2.0 * sweep / window - 1.0).abs()) as f32
}

impl Processor for TimeStretchProcessor {
    fn prepare(&mut self, sample_rate: u32, _max_block: usize) {
        self.window = (usize::try_from(sample_rate).unwrap_or(48_000) / 50).max(32);
        // The longest read is one and a half windows back, plus one sample to interpolate.
        let capacity = self.window * 2;
        self.history = [vec![0.0; capacity], vec![0.0; capacity]];
        self.write = 0;
        self.sweep = self.centre();
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
        let capacity = self.history[0].len();
        if capacity == 0 {
            return;
        }
        let window = self.window as f64;
        let half = window / 2.0;
        // Each output sample, the taps' delays grow by one sample minus the transposition's read
        // speed.
        let drift = 1.0 - f64::from(self.pitch) / f64::from(self.rate);
        for frame in 0..frames {
            for channel in 0..channels {
                let input = inputs[channel][frame];
                // A sample the history cannot hold would leak through a tap at zero weight
                // (as 0 * inf).
                self.history[channel][self.write] = if input.is_finite() { input } else { 0.0 };
            }
            let first = self.sweep;
            let second = (first + half).rem_euclid(window);
            let (first_weight, second_weight) = (weight(first, window), weight(second, window));
            for channel in 0..channels {
                outputs[channel][frame] = if self.bypass {
                    self.history[channel][(self.write + capacity - self.window) % capacity]
                } else {
                    self.read(channel, half + first) * first_weight
                        + self.read(channel, half + second) * second_weight
                };
            }
            self.sweep = (self.sweep + drift).rem_euclid(window);
            // A tiny negative sum wraps to `window` itself in rounding.
            if self.sweep >= window {
                self.sweep = 0.0;
            }
            self.write = (self.write + 1) % capacity;
        }
    }
    fn reset(&mut self) {
        for channel in &mut self.history {
            channel.fill(0.0);
        }
        self.write = 0;
        self.sweep = self.centre();
    }
    /// One window: an untransposed output's delay, and a transposed one's
    /// mean, whatever the ratios.
    fn latency_samples(&self) -> u32 {
        u32::try_from(self.window).unwrap_or(u32::MAX)
    }
    fn save_state(&self) -> StateBlob {
        let mut bytes =
            Vec::with_capacity(16 + self.history.iter().map(Vec::len).sum::<usize>() * 4);
        bytes.extend_from_slice(&(u64::try_from(self.write).unwrap_or(0)).to_le_bytes());
        bytes.extend_from_slice(&self.sweep.to_le_bytes());
        for channel in &self.history {
            for sample in channel {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }
        StateBlob::new(bytes)
    }
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        let bytes = state.as_bytes();
        let expected = 16 + self.history.iter().map(Vec::len).sum::<usize>() * 4;
        if bytes.len() != expected {
            return Err(StateError::Mismatch);
        }
        let write = usize::try_from(u64::from_le_bytes(
            bytes[0..8].try_into().expect("fixed range"),
        ))
        .ok();
        let sweep = f64::from_le_bytes(bytes[8..16].try_into().expect("fixed range"));
        let samples = bytes[16..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| f32::from_le_bytes(*word));
        if write.is_none_or(|value| value >= self.history[0].len())
            || !(0.0..self.window as f64).contains(&sweep)
            || samples.clone().any(|sample| !sample.is_finite())
        {
            return Err(StateError::Invalid);
        }
        self.write = write.unwrap_or(0);
        self.sweep = sweep;
        for (target, sample) in self.history.iter_mut().flatten().zip(samples) {
            *target = sample;
        }
        Ok(())
    }
}

fn ratio(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.25, 4.0)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::TimeStretchProcessor;
    use crate::{ProcessCtx, Processor, TransportState};
    use alpha2_time::MediaTime;

    fn context(sample_rate: u32) -> ProcessCtx {
        ProcessCtx {
            position: MediaTime::ZERO,
            sample_position: 0,
            sample_rate,
            transport: TransportState::Playing,
        }
    }

    /// Runs `input` through `processor` in 512-sample blocks.
    fn run(processor: &mut TimeStretchProcessor, sample_rate: u32, input: &[f32]) -> Vec<f32> {
        processor.prepare(sample_rate, 512);
        let mut output = vec![0.0; input.len()];
        for (input, output) in input.chunks(512).zip(output.chunks_mut(512)) {
            processor.process(&context(sample_rate), &[input], &mut [output]);
        }
        output
    }

    fn largest_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn a_transposed_tone_moves_as_smoothly_as_the_tone_it_becomes() {
        // Tones whose period does not divide the 20 ms window, so a splice between grains would
        // show.
        for (rate, pitch, frequency) in [
            (1.0, 1.2, 37.0),
            (1.0, 0.8, 37.0),
            (1.0, 1.2, 440.0),
            (0.9, 1.2, 37.0),
            (1.0, 1.05, 220.0),
            (2.0, 0.5, 97.0),
        ] {
            let input = (0..96_000)
                .map(|index| {
                    0.5 * libm::sinf(core::f32::consts::TAU * frequency * index as f32 / 48_000.0)
                })
                .collect::<Vec<_>>();
            let mut processor = TimeStretchProcessor::new(rate, pitch);
            let output = run(&mut processor, 48_000, &input);
            // The tone it becomes steps at most 0.5 * 2 pi f r / fs a sample; the crossfade between
            // two grains adds a little.
            let transposed =
                0.5 * core::f32::consts::TAU * frequency * (pitch / rate).max(1.0) / 48_000.0;
            let step = largest_step(&output[4 * 960..]);
            assert!(
                step < 2.0 * transposed + 2.0e-3,
                "rate {rate} pitch {pitch} {frequency} Hz: a step of {step}, the tone's {transposed}"
            );
        }
    }

    #[test]
    fn an_impulse_comes_out_as_late_as_the_reported_latency_bypassed_or_not() {
        for sample_rate in [44_100, 48_000, 96_000] {
            for ratio in [0.25, 0.5, 1.0, 1.5, 2.0, 4.0] {
                for bypass in [false, true] {
                    let mut input = vec![0.0; 16_384];
                    input[1_000] = 1.0;
                    let mut processor = TimeStretchProcessor::new(ratio, ratio);
                    processor.set_bypass(bypass);
                    let output = run(&mut processor, sample_rate, &input);
                    let peak = output
                        .iter()
                        .enumerate()
                        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                        .map(|(index, _)| index)
                        .expect("samples");
                    assert_eq!(
                        peak - 1_000,
                        processor.latency_samples() as usize,
                        "{sample_rate} Hz, ratio {ratio}, bypass {bypass}"
                    );
                    assert_eq!(output[peak].to_bits(), 1.0_f32.to_bits());
                }
            }
        }
    }

    #[test]
    fn changing_the_ratios_keeps_the_latency() {
        let mut processor = TimeStretchProcessor::new(1.0, 1.0);
        processor.prepare(48_000, 512);
        let latency = processor.latency_samples();
        processor.set_ratios(0.5, 2.0);
        assert_eq!(processor.latency_samples(), latency);
        processor.set_ratios(4.0, 0.25);
        assert_eq!(processor.latency_samples(), latency);
    }
}
