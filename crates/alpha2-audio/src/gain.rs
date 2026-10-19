use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Stateless linear amplitude multiplier.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Gain {
    gain: f32,
}

impl Gain {
    /// Creates a gain processor. Non-finite values are normalized to silence.
    #[must_use]
    pub fn new(gain: f32) -> Self {
        Self {
            gain: if gain.is_finite() { gain } else { 0.0 },
        }
    }

    /// Replaces the linear amplitude multiplier.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = if gain.is_finite() { gain } else { 0.0 };
    }
}

impl Processor for Gain {
    fn prepare(&mut self, _sample_rate: u32, _max_block: usize) {}

    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for channel in 0..inputs.len().min(outputs.len()) {
            for (source, target) in inputs[channel].iter().zip(outputs[channel].iter_mut()) {
                *target = *source * self.gain;
            }
        }
    }

    fn reset(&mut self) {}

    fn save_state(&self) -> StateBlob {
        StateBlob::new(Vec::new())
    }

    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        if state.as_bytes().is_empty() {
            Ok(())
        } else {
            Err(StateError::Mismatch)
        }
    }
}
