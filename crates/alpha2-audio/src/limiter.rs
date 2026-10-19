use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Linked-channel peak limiter with instantaneous attack and exponential release.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PeakLimiter {
    threshold: f32,
    release_ms: f32,
    envelope: f32,
    release_coefficient: f32,
}

impl PeakLimiter {
    /// Creates a limiter with a positive linear threshold and release time.
    ///
    /// A non-finite or non-positive threshold becomes 1.0 and such a release becomes 50 ms.
    #[must_use]
    pub fn new(threshold: f32, release_ms: f32) -> Self {
        Self {
            threshold: valid_positive(threshold, 1.0),
            release_ms: valid_positive(release_ms, 50.0),
            envelope: 0.0,
            release_coefficient: 0.0,
        }
    }
}

impl Processor for PeakLimiter {
    fn prepare(&mut self, sample_rate: u32, _max_block: usize) {
        #[allow(clippy::cast_precision_loss)]
        let release_samples = self.release_ms * 0.001 * sample_rate.max(1) as f32;
        self.release_coefficient = libm::expf(-1.0 / release_samples);
    }

    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let frames = inputs
            .iter()
            .map(|channel| channel.len())
            .chain(outputs.iter().map(|channel| channel.len()))
            .min()
            .unwrap_or(0);
        let channels = inputs.len().min(outputs.len());
        for frame in 0..frames {
            let peak = inputs[..channels]
                .iter()
                .map(|channel| finite_or_zero(channel[frame]).abs())
                .fold(0.0_f32, f32::max);
            if !self.envelope.is_finite() {
                self.envelope = 0.0;
            }
            self.envelope = peak.max(self.envelope * self.release_coefficient);
            // Released to silence, the envelope would sink into the slow subnormal range (k * c
            // rounds back to k). No usable threshold is this small.
            if self.envelope < 1.0e-30 {
                self.envelope = 0.0;
            }
            let gain = if self.envelope > self.threshold {
                self.threshold / self.envelope
            } else {
                1.0
            };
            for channel in 0..channels {
                outputs[channel][frame] = finite_or_zero(inputs[channel][frame]) * gain;
            }
        }
    }

    fn reset(&mut self) {
        self.envelope = 0.0;
    }

    fn history_samples(&self) -> u32 {
        crate::processor::settling_samples(f64::from(self.release_coefficient))
    }

    fn save_state(&self) -> StateBlob {
        StateBlob::new(self.envelope.to_le_bytes().to_vec())
    }

    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        let bytes = <[u8; 4]>::try_from(state.as_bytes()).map_err(|_| StateError::Mismatch)?;
        let envelope = f32::from_le_bytes(bytes);
        if !envelope.is_finite() || envelope < 0.0 {
            return Err(StateError::Invalid);
        }
        self.envelope = envelope;
        Ok(())
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

fn valid_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}
