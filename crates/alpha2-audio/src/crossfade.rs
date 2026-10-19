use core::f32::consts::FRAC_PI_2;

use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Equal-power crossfade between two planar input buses.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EqualPowerCrossfade {
    progress: f32,
}

impl EqualPowerCrossfade {
    /// Creates a crossfade with progress clamped to the unit interval.
    #[must_use]
    pub fn new(progress: f32) -> Self {
        Self {
            progress: if progress.is_finite() {
                progress.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }

    /// Updates crossfade progress, clamped and sanitized as in [`Self::new`].
    pub fn set_progress(&mut self, progress: f32) {
        *self = Self::new(progress);
    }
}

impl Processor for EqualPowerCrossfade {
    /// The bus faded from, then the bus faded to.
    fn inputs_per_output(&self) -> usize {
        2
    }

    fn prepare(&mut self, _sample_rate: u32, _max_block: usize) {}

    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let channels = outputs.len();
        if inputs.len() < channels.saturating_mul(2) {
            return;
        }
        let (from_gain, to_gain) = match self.progress {
            0.0 => (1.0, 0.0),
            1.0 => (0.0, 1.0),
            progress => (
                libm::cosf(progress * FRAC_PI_2),
                libm::sinf(progress * FRAC_PI_2),
            ),
        };
        for channel in 0..channels {
            let from = inputs[channel];
            let to = inputs[channel + channels];
            for ((from, to), output) in from.iter().zip(to).zip(outputs[channel].iter_mut()) {
                *output = from * from_gain + *to * to_gain;
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
