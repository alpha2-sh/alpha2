#![allow(clippy::cast_precision_loss)]

use crate::{ProcessCtx, Processor, StateBlob, StateError};

/// Reusable peak envelope follower.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EnvelopeFollower {
    attack_ms: f32,
    release_ms: f32,
    attack: f32,
    release: f32,
    value: f32,
}

impl EnvelopeFollower {
    /// Creates an attack/release envelope follower. Invalid times fall back to 5 ms attack and
    /// 100 ms release.
    #[must_use]
    pub fn new(attack_ms: f32, release_ms: f32) -> Self {
        Self {
            attack_ms: positive(attack_ms, 5.0),
            release_ms: positive(release_ms, 100.0),
            attack: 0.0,
            release: 0.0,
            value: 0.0,
        }
    }
    /// Computes the attack and release coefficients for `sample_rate`. Call before [`Self::next`].
    pub fn prepare(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1) as f32;
        self.attack = libm::expf(-1.0 / (self.attack_ms * 0.001 * rate));
        self.release = libm::expf(-1.0 / (self.release_ms * 0.001 * rate));
    }
    /// Feeds one sample and returns the updated envelope. Non-finite input counts as silence.
    pub fn next(&mut self, sample: f32) -> f32 {
        if !self.value.is_finite() {
            self.value = 0.0;
        }
        let peak = finite_or_zero(sample).abs();
        let coefficient = if peak > self.value {
            self.attack
        } else {
            self.release
        };
        self.value = coefficient * self.value + (1.0 - coefficient) * peak;
        if self.value.abs() < 1.0e-30 {
            self.value = 0.0;
        }
        self.value
    }
    /// Clears the envelope to silence.
    pub fn reset(&mut self) {
        self.value = 0.0;
    }
}

/// Sidechain ducker. Inputs are program channels followed by matching sidechain channels.
///
/// At most two channels are processed; the envelope follows the loudest sidechain channel.
#[derive(Clone, Debug)]
pub struct Ducker {
    follower: EnvelopeFollower,
    threshold: f32,
    depth: f32,
    bypass: bool,
}

impl Ducker {
    /// Creates a ducker with linear threshold and maximum attenuation in decibels.
    ///
    /// The sign of `depth_db` is ignored and the attenuation is capped at 96 dB. A non-finite or
    /// non-positive threshold becomes 0.1.
    #[must_use]
    pub fn new(threshold: f32, depth_db: f32, attack_ms: f32, release_ms: f32) -> Self {
        Self {
            follower: EnvelopeFollower::new(attack_ms, release_ms),
            threshold: positive(threshold, 0.1),
            depth: libm::powf(10.0, -depth_db.abs().min(96.0) / 20.0),
            bypass: false,
        }
    }
    /// Enables transparent bypass while retaining envelope continuity.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }
}

impl Processor for Ducker {
    /// The program, then the sidechain.
    fn inputs_per_output(&self) -> usize {
        2
    }

    fn history_samples(&self) -> u32 {
        crate::processor::settling_samples(f64::from(
            self.follower.attack.max(self.follower.release),
        ))
    }

    fn prepare(&mut self, sample_rate: u32, _max_block: usize) {
        self.follower.prepare(sample_rate);
    }
    fn process(&mut self, _ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let channels = outputs.len().min(inputs.len() / 2).min(2);
        let frames = outputs
            .iter()
            .take(channels)
            .map(|v| v.len())
            .chain(inputs.iter().take(channels * 2).map(|v| v.len()))
            .min()
            .unwrap_or(0);
        for frame in 0..frames {
            let sidechain = inputs[channels..channels * 2]
                .iter()
                .map(|channel| channel[frame].abs())
                .fold(0.0_f32, f32::max);
            let envelope = self.follower.next(sidechain);
            let mix = ((envelope - self.threshold) / self.threshold.max(1.0e-9)).clamp(0.0, 1.0);
            let gain = if self.bypass {
                1.0
            } else {
                1.0 + (self.depth - 1.0) * mix
            };
            for channel in 0..channels {
                outputs[channel][frame] = finite_or_zero(inputs[channel][frame]) * gain;
            }
        }
    }
    fn reset(&mut self) {
        self.follower.reset();
    }
    fn save_state(&self) -> StateBlob {
        StateBlob::new(self.follower.value.to_le_bytes().to_vec())
    }
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError> {
        let bytes = <[u8; 4]>::try_from(state.as_bytes()).map_err(|_| StateError::Mismatch)?;
        let value = f32::from_le_bytes(bytes);
        if !value.is_finite() || value < 0.0 {
            return Err(StateError::Invalid);
        }
        self.follower.value = value;
        Ok(())
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

fn positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}
