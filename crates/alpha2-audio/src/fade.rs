use alpha2_time::{MediaTime, TICKS_PER_SECOND};

use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Direction of a linear amplitude fade.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FadeDirection {
    /// Silence to unity.
    In,
    /// Unity to silence.
    Out,
}

/// Sample-addressed linear fade independent of host block size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LinearFade {
    start: MediaTime,
    end: MediaTime,
    direction: FadeDirection,
}

impl LinearFade {
    /// Creates a fade over a non-empty half-open timeline range.
    #[must_use]
    pub fn new(start: MediaTime, end: MediaTime, direction: FadeDirection) -> Option<Self> {
        (start < end).then_some(Self {
            start,
            end,
            direction,
        })
    }

    fn gain_at(self, time: MediaTime) -> f32 {
        let numerator = time.ticks().saturating_sub(self.start.ticks());
        let denominator = self.end.ticks().saturating_sub(self.start.ticks());
        #[allow(clippy::cast_precision_loss)]
        let progress = (numerator as f32 / denominator as f32).clamp(0.0, 1.0);
        match self.direction {
            FadeDirection::In => progress,
            FadeDirection::Out => 1.0 - progress,
        }
    }
}

impl Processor for LinearFade {
    fn prepare(&mut self, _sample_rate: u32, _max_block: usize) {}

    fn process(&mut self, ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if ctx.sample_rate == 0 {
            return;
        }
        for channel in 0..inputs.len().min(outputs.len()) {
            for (index, (source, target)) in inputs[channel]
                .iter()
                .zip(outputs[channel].iter_mut())
                .enumerate()
            {
                let Ok(index) = i64::try_from(index) else {
                    *target = 0.0;
                    continue;
                };
                let sample = ctx.sample_position.saturating_add(index);
                let ticks = sample
                    .saturating_mul(TICKS_PER_SECOND)
                    .div_euclid(i64::from(ctx.sample_rate));
                *target = *source * self.gain_at(MediaTime(ticks));
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
