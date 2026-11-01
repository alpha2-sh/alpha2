//! Platform-independent, real-time-safe audio DSP primitives and graph.

mod automation;
mod biquad;
mod compressor;
mod crossfade;
mod ducking;
mod fade;
mod gain;
mod graph;
mod limiter;
mod loudness;
mod processor;
mod resample;
mod reverb;
mod stretch;
mod waveform;

pub use automation::{AutomatedGain, AutomationCurve, AutomationPoint};
pub use biquad::{
    Biquad, BiquadKind, MAX_FREQUENCY_FRACTION, MAX_PEAKING_GAIN_DB, MIN_FREQUENCY_HZ, Q_RANGE,
};
pub use compressor::{Compressor, MAX_RATIO};
pub use crossfade::EqualPowerCrossfade;
pub use ducking::{Ducker, EnvelopeFollower};
pub use fade::{FadeDirection, LinearFade};
pub use gain::Gain;
pub use graph::{
    AudioGraph, AudioGraphState, ChainError, ChainStateError, GraphStateError, ProcessorChain,
};
pub use limiter::PeakLimiter;
pub use loudness::{
    ABSOLUTE_GATE_LUFS, LIMITER_MARGIN_DB, LoudnessError, LoudnessMeter, LoudnessNormalization,
    LoudnessReport,
};
pub use processor::{ChannelLayout, ProcessCtx, Processor, StateBlob, StateError, TransportState};
pub use resample::{
    Interpolator, MAX_DOWNSAMPLE_FACTOR, MAX_FILTERED_SPEED, MAX_RESAMPLE_RATE, ResampleError,
    Resampler,
};
pub use reverb::SchroederReverb;
pub use stretch::{
    STRETCH_SEGMENT_GRAINS, STRETCH_WARMUP_GRAINS, Stretch, StretchReader, StretchSignal,
};
pub use waveform::{
    MAX_WAVEFORM_LEVELS, Waveform, WaveformBucket, WaveformError, WaveformLevel, WaveformSummary,
};

#[cfg(test)]
mod tests;
