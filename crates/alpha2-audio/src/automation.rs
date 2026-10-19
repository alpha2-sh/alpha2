/// One sample-addressed automation point.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AutomationPoint {
    /// Absolute project-rate sample index.
    pub sample: i64,
    /// Finite parameter value.
    pub value: f32,
}

/// Sample-accurate gain driven by an immutable automation curve.
#[derive(Clone, Debug)]
pub struct AutomatedGain {
    curve: AutomationCurve,
    fallback: f32,
}

impl AutomatedGain {
    /// Creates an automated gain processor.
    ///
    /// `fallback` is the gain used when the curve has no points; a non-finite fallback becomes
    /// silence.
    #[must_use]
    pub fn new(curve: AutomationCurve, fallback: f32) -> Self {
        Self {
            curve,
            fallback: if fallback.is_finite() { fallback } else { 0.0 },
        }
    }
}

impl Processor for AutomatedGain {
    fn prepare(&mut self, _sample_rate: u32, _max_block: usize) {}
    fn process(&mut self, ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for channel in 0..inputs.len().min(outputs.len()) {
            for (index, (&input, output)) in inputs[channel]
                .iter()
                .zip(outputs[channel].iter_mut())
                .enumerate()
            {
                let sample = ctx
                    .sample_position
                    .saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
                *output = input * self.curve.value_at(sample, self.fallback);
            }
        }
    }
    fn reset(&mut self) {}
    fn save_state(&self) -> StateBlob {
        StateBlob::default()
    }
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        if state.as_bytes().is_empty() {
            Ok(())
        } else {
            Err(StateError::Mismatch)
        }
    }
}

/// Immutable piecewise-linear automation prepared off the realtime thread.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomationCurve {
    points: Vec<AutomationPoint>,
}

impl AutomationCurve {
    /// Creates a sorted curve, rejecting duplicate positions and non-finite values.
    ///
    /// # Errors
    /// Returns a stable message for non-finite values or duplicate positions.
    pub fn new(mut points: Vec<AutomationPoint>) -> Result<Self, &'static str> {
        if points.iter().any(|point| !point.value.is_finite()) {
            return Err("automation values must be finite");
        }
        points.sort_by_key(|point| point.sample);
        if points
            .windows(2)
            .any(|pair| pair[0].sample == pair[1].sample)
        {
            return Err("automation positions must be unique");
        }
        Ok(Self { points })
    }

    /// Evaluates the curve at `sample` without allocating.
    ///
    /// Interpolates linearly between points and holds the first and last values outside them.
    /// Returns `fallback` only when the curve is empty.
    #[must_use]
    pub fn value_at(&self, sample: i64, fallback: f32) -> f32 {
        let index = self.points.partition_point(|point| point.sample <= sample);
        match index {
            0 => self.points.first().map_or(fallback, |point| point.value),
            index if index == self.points.len() => self.points[index - 1].value,
            index => {
                let left = self.points[index - 1];
                let right = self.points[index];
                #[allow(clippy::cast_precision_loss)]
                let t = (i128::from(sample) - i128::from(left.sample)) as f64
                    / (i128::from(right.sample) - i128::from(left.sample)) as f64;
                #[allow(clippy::cast_possible_truncation)]
                let value = (f64::from(left.value)
                    + (f64::from(right.value) - f64::from(left.value)) * t)
                    as f32;
                value
            }
        }
    }
}
use crate::{ProcessCtx, Processor, StateBlob, StateError};
