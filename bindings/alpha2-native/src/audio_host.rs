use std::collections::HashMap;

use alpha2_audio::{AudioGraph, ChannelLayout, ProcessCtx, TransportState};
#[cfg(feature = "timeline")]
use alpha2_engine::{AudioRenderer, EngineProject};
use alpha2_media::{
    AudioBlock, AudioBlockProvider, AudioBlockSink, AudioSpan, CancelToken, EncodeError, SourceId,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

/// Native offline-render failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OfflineAudioError {
    /// Input channel count or length is invalid.
    InvalidInput,
    /// Sink rejected a block.
    Sink(EncodeError),
    /// The project could not be mixed at the requested configuration.
    #[cfg(feature = "timeline")]
    Engine(alpha2_engine::EngineError),
    /// An audible clip needed media the caller had not decoded.
    MissingSources(Vec<SourceId>),
}

impl std::fmt::Display for OfflineAudioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput => formatter.write_str("offline audio input is invalid"),
            Self::Sink(error) => write!(formatter, "offline audio sink failed: {error}"),
            #[cfg(feature = "timeline")]
            Self::Engine(error) => write!(formatter, "offline audio rendering failed: {error}"),
            Self::MissingSources(sources) => {
                write!(
                    formatter,
                    "offline audio is missing {} sources",
                    sources.len()
                )
            }
        }
    }
}

impl std::error::Error for OfflineAudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sink(error) => Some(error),
            #[cfg(feature = "timeline")]
            Self::Engine(error) => Some(error),
            _ => None,
        }
    }
}

/// Decoded, project-rate planar audio held per source for offline mixing.
///
/// The native decoder produces whole-stream planar audio, so one retained block
/// per source is enough; the renderer indexes it by absolute source position.
#[derive(Default)]
pub struct DecodedAudioSources {
    sources: HashMap<SourceId, AudioBlock>,
}

impl DecodedAudioSources {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Retains one source's decoded audio, replacing any earlier block.
    pub fn insert(&mut self, source: SourceId, block: AudioBlock) {
        self.sources.insert(source, block);
    }

    /// Drops one source's decoded audio.
    pub fn remove(&mut self, source: &SourceId) -> bool {
        self.sources.remove(source).is_some()
    }

    /// Returns how many sources are retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Returns whether no source is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

/// Each source is decoded once from end to end, so a block is its source's
/// entire audio: before and after it the source is silent.
impl AudioBlockProvider for DecodedAudioSources {
    fn block(
        &self,
        source: &SourceId,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        self.sources.get(source).map(AudioSpan::whole)
    }
}

/// One offline project-audio render request.
#[cfg(feature = "timeline")]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProjectAudioRequest<'a> {
    /// Output channel count. One or two.
    pub channels: usize,
    /// Absolute project-rate sample index the range starts at. The blocks
    /// written are positioned from the range's start, which is sample zero.
    pub start_sample: i64,
    /// Samples per channel to produce.
    pub total_frames: usize,
    /// Repeating block schedule, which may be irregular.
    pub block_sizes: &'a [usize],
}

/// Mixes a whole project audio range and writes exact project-rate blocks.
///
/// This is the offline counterpart of the browser pump: the caller decodes each
/// source once, and the engine owns gain, fades, panning, effects, bus summing,
/// and limiting. Irregular `block_sizes` are repeated so callers can prove the
/// mix does not depend on the host's block size.
///
/// # Errors
/// Returns [`OfflineAudioError::InvalidInput`] for an empty schedule or range,
/// [`OfflineAudioError::Engine`] when the renderer rejects the configuration,
/// [`OfflineAudioError::MissingSources`] when an audible clip had no decoded
/// media, and [`OfflineAudioError::Sink`] when the destination rejects output.
#[cfg(feature = "timeline")]
pub fn render_project_audio(
    snapshot: &EngineProject,
    request: &ProjectAudioRequest<'_>,
    sources: &dyn AudioBlockProvider,
    sink: &mut dyn AudioBlockSink,
    cancel: &CancelToken,
) -> Result<(), OfflineAudioError> {
    let ProjectAudioRequest {
        channels,
        start_sample,
        total_frames,
        block_sizes,
    } = *request;
    if total_frames == 0 || block_sizes.is_empty() || block_sizes.contains(&0) || start_sample < 0 {
        return Err(OfflineAudioError::InvalidInput);
    }
    let max_block = block_sizes.iter().copied().max().unwrap_or(1);
    let mut renderer =
        AudioRenderer::new(snapshot, channels, max_block).map_err(OfflineAudioError::Engine)?;

    let mut produced = 0;
    let mut sequence = 0;
    let mut missing: Vec<SourceId> = Vec::new();
    while produced < total_frames {
        if cancel.is_cancelled() {
            sink.abort_audio().map_err(OfflineAudioError::Sink)?;
            return Err(OfflineAudioError::Sink(EncodeError::Cancelled));
        }
        let count = block_sizes[sequence % block_sizes.len()].min(total_frames - produced);
        let offset = i64::try_from(produced).unwrap_or(i64::MAX);
        let status = renderer
            .render(
                snapshot,
                start_sample.saturating_add(offset),
                count,
                sources,
                TransportState::Playing,
            )
            .map_err(OfflineAudioError::Engine)?;
        for source in &status.missing_sources {
            if !missing.contains(source) {
                missing.push(source.clone());
            }
        }
        let mut block = renderer.block(&status);
        // The range is its own file: its first sample is the sink's sample zero.
        block.start_sample = offset;
        sink.write_audio(&block, cancel)
            .map_err(OfflineAudioError::Sink)?;
        produced += count;
        sequence += 1;
    }
    if !missing.is_empty() {
        // Report an incomplete mix rather than committing partial audio.
        sink.abort_audio().map_err(OfflineAudioError::Sink)?;
        return Err(OfflineAudioError::MissingSources(missing));
    }
    sink.finish_audio(cancel).map_err(OfflineAudioError::Sink)
}

/// Host sample representation considered during deterministic negotiation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HostSampleFormat {
    /// Native 32-bit floating point.
    F32,
    /// Signed 16-bit integer.
    I16,
    /// Any format not supported by the realtime host.
    Unsupported,
}

/// Platform-neutral device range used by negotiation tests and cpal mapping.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HostAudioConfig {
    /// Channel count.
    pub channels: u16,
    /// Lowest supported sample rate.
    pub min_sample_rate: u32,
    /// Highest supported sample rate.
    pub max_sample_rate: u32,
    /// Device sample representation.
    pub format: HostSampleFormat,
}

/// Selects an output that plays stereo, requiring exact project-rate `f32`
/// support.
///
/// Exactly two channels come first, then the fewest above two, then mono.
/// Backends do not list the same counts: ALSA lists every count a device
/// takes, WASAPI only its mix format's, and `CoreAudio` the sum of the
/// device's stream channels, so a 5.1 endpoint or a multichannel interface
/// may offer no stereo entry. Stereo is written to such a device's first two
/// channels, which are front left and front right on all three, and the rest
/// are silent.
#[must_use]
pub fn negotiate_output_config(
    configs: &[HostAudioConfig],
    project_rate: u32,
) -> Option<HostAudioConfig> {
    configs
        .iter()
        .copied()
        .filter(|config| {
            config.format == HostSampleFormat::F32
                && config.channels > 0
                && (config.min_sample_rate..=config.max_sample_rate).contains(&project_rate)
        })
        .min_by_key(|config| match config.channels {
            2 => (0, 0),
            1 => (2, 0),
            more => (1, more),
        })
}

/// Runs the same graph used by realtime hosts and writes exact project-rate blocks.
///
/// `inputs` are the graph's buses, flattened: each bus's planes in the graph's
/// layout, one bus after another. All channels must have the same length.
/// The output is in the graph's layout. Irregular `block_sizes` are repeated
/// until the source is exhausted.
///
/// # Errors
/// Returns [`OfflineAudioError::InvalidInput`] when `inputs` are not exactly
/// the graph's buses, or for a malformed schedule, and
/// [`OfflineAudioError::Sink`] when the destination rejects output.
pub fn render_offline(
    graph: &mut AudioGraph,
    sample_rate: u32,
    inputs: &[Vec<f32>],
    block_sizes: &[usize],
    sink: &mut dyn AudioBlockSink,
    cancel: &CancelToken,
) -> Result<(), OfflineAudioError> {
    let layout = graph.layout();
    let frames = inputs.first().map_or(0, Vec::len);
    if sample_rate == 0
        || inputs.is_empty()
        || graph.buses().checked_mul(layout.channels()) != Some(inputs.len())
        || inputs.iter().any(|channel| channel.len() != frames)
        || block_sizes.is_empty()
        || block_sizes.contains(&0)
    {
        return Err(OfflineAudioError::InvalidInput);
    }
    let max_block = block_sizes.iter().copied().max().unwrap_or(1);
    graph.prepare(sample_rate, max_block);
    let mut output: Vec<Vec<f32>> = (0..layout.channels())
        .map(|_| vec![0.0; max_block])
        .collect();
    let mut offset = 0;
    let mut sequence = 0;
    while offset < frames {
        if cancel.is_cancelled() {
            sink.abort_audio().map_err(OfflineAudioError::Sink)?;
            return Err(OfflineAudioError::Sink(EncodeError::Cancelled));
        }
        let count = block_sizes[sequence % block_sizes.len()].min(frames - offset);
        let refs: Vec<&[f32]> = inputs
            .iter()
            .map(|channel| &channel[offset..offset + count])
            .collect();
        let mut outputs: Vec<&mut [f32]> = output
            .iter_mut()
            .map(|channel| &mut channel[..count])
            .collect();
        let sample_position = i64::try_from(offset).unwrap_or(i64::MAX);
        graph.process(
            &ProcessCtx {
                position: MediaTime::from_ticks(
                    sample_position
                        .saturating_mul(TICKS_PER_SECOND)
                        .div_euclid(i64::from(sample_rate)),
                ),
                sample_position,
                sample_rate,
                transport: TransportState::Playing,
            },
            &refs,
            &mut outputs,
        );
        drop(outputs);
        sink.write_audio(
            &AudioBlock {
                start_sample: sample_position,
                sample_rate,
                speakers: match layout {
                    ChannelLayout::Mono => alpha2_media::SpeakerLayout::MONO,
                    ChannelLayout::Stereo => alpha2_media::SpeakerLayout::STEREO,
                },
                channels: output
                    .iter()
                    .map(|channel| channel[..count].to_vec())
                    .collect(),
            },
            cancel,
        )
        .map_err(OfflineAudioError::Sink)?;
        offset += count;
        sequence += 1;
    }
    sink.finish_audio(cancel).map_err(OfflineAudioError::Sink)
}

#[cfg(feature = "cpal-host")]
mod realtime {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, Stream, StreamConfig};

    use super::{
        HostAudioConfig, HostSampleFormat, MediaTime, ProcessCtx, TICKS_PER_SECOND, TransportState,
        negotiate_output_config,
    };

    /// Pull source invoked exclusively by the audio callback.
    ///
    /// `alpha2_engine::AudioRenderer` is an offline/block renderer and does
    /// not implement this contract because timeline evaluation may allocate.
    /// Realtime hosts must supply a pre-indexed, allocation-free source or feed
    /// this callback from a bounded producer ring.
    pub trait RealtimeAudioSource: Send + 'static {
        /// Fills project-rate planar output without allocation or blocking.
        fn render(&mut self, ctx: &ProcessCtx, output: &mut [&mut [f32]]);
        /// Algorithmic latency already present in the source graph.
        fn latency_samples(&self) -> u32 {
            0
        }
    }

    /// Native realtime host failure.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum CpalHostError {
        /// No output device exists.
        NoOutputDevice,
        /// No compatible floating-point configuration exists.
        NoCompatibleFormat,
        /// The callback limit is zero frames.
        InvalidCallbackSize,
        /// Device query or stream construction failed.
        Device(String),
    }

    impl std::fmt::Display for CpalHostError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::NoOutputDevice => formatter.write_str("no audio output device is available"),
                Self::NoCompatibleFormat => {
                    formatter.write_str("no compatible audio output format is available")
                }
                Self::InvalidCallbackSize => {
                    formatter.write_str("the audio callback limit must be at least one frame")
                }
                Self::Device(message) => write!(formatter, "audio device failed: {message}"),
            }
        }
    }

    impl std::error::Error for CpalHostError {}

    /// Running cpal output stream and observable underrun count.
    pub struct CpalOutputHost {
        stream: Stream,
        underruns: Arc<AtomicU64>,
        sample_rate: u32,
        channels: u16,
        latency_samples: u32,
    }

    impl CpalOutputHost {
        /// Opens the default output at the exact project rate in `f32`, with
        /// the channels [`negotiate_output_config`] picks.
        ///
        /// `source` renders at most `max_callback_frames` frames at a time,
        /// into that many frames of scratch allocated here. The device's own
        /// buffer may be larger, and is rendered in as many calls as it takes.
        ///
        /// # Errors
        /// Returns [`CpalHostError::InvalidCallbackSize`] when
        /// `max_callback_frames` is zero, and a device or format error when no
        /// exact realtime configuration can be opened.
        pub fn open(
            mut source: impl RealtimeAudioSource,
            project_rate: u32,
            max_callback_frames: usize,
        ) -> Result<Self, CpalHostError> {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or(CpalHostError::NoOutputDevice)?;
            if max_callback_frames == 0 {
                return Err(CpalHostError::InvalidCallbackSize);
            }
            let supported = device
                .supported_output_configs()
                .map_err(|error| CpalHostError::Device(error.to_string()))?
                .collect::<Vec<_>>();
            let offered = supported
                .iter()
                .map(|config| HostAudioConfig {
                    channels: config.channels(),
                    min_sample_rate: config.min_sample_rate(),
                    max_sample_rate: config.max_sample_rate(),
                    format: match config.sample_format() {
                        SampleFormat::F32 => HostSampleFormat::F32,
                        SampleFormat::I16 => HostSampleFormat::I16,
                        _ => HostSampleFormat::Unsupported,
                    },
                })
                .collect::<Vec<_>>();
            let chosen = negotiate_output_config(&offered, project_rate)
                .ok_or(CpalHostError::NoCompatibleFormat)?;
            let selected = offered
                .iter()
                .position(|config| *config == chosen)
                .map(|index| supported[index])
                .ok_or(CpalHostError::NoCompatibleFormat)?;
            let device_rate = project_rate;
            let config: StreamConfig = selected.with_sample_rate(device_rate).config();
            let channels = usize::from(config.channels);
            let mut planar = [
                vec![0.0_f32; max_callback_frames],
                vec![0.0_f32; max_callback_frames],
            ];
            let underruns = Arc::new(AtomicU64::new(0));
            let callback_underruns = Arc::clone(&underruns);
            let error_underruns = Arc::clone(&underruns);
            let mut sample_position = 0_i64;
            let latency_samples = source.latency_samples();
            let stream = device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| {
                        if !fill_device_buffer(
                            &mut source,
                            &mut planar,
                            data,
                            channels,
                            device_rate,
                            &mut sample_position,
                        ) {
                            callback_underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    move |_| {
                        error_underruns.fetch_add(1, Ordering::Relaxed);
                    },
                    None,
                )
                .map_err(|error| CpalHostError::Device(error.to_string()))?;
            Ok(Self {
                stream,
                underruns,
                sample_rate: device_rate,
                channels: config.channels,
                latency_samples,
            })
        }
        /// Starts callback delivery.
        ///
        /// # Errors
        /// Returns the device error reported by cpal.
        pub fn play(&self) -> Result<(), CpalHostError> {
            self.stream
                .play()
                .map_err(|error| CpalHostError::Device(error.to_string()))
        }
        /// Number of errors the device's stream reported, and of device
        /// buffers that were not whole frames and were played as silence.
        #[must_use]
        pub fn underruns(&self) -> u64 {
            self.underruns.load(Ordering::Relaxed)
        }
        /// Negotiated device sample rate.
        #[must_use]
        pub const fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
        /// Negotiated output channels.
        #[must_use]
        pub const fn channels(&self) -> u16 {
            self.channels
        }
        /// Source graph latency for transport compensation.
        #[must_use]
        pub const fn latency_samples(&self) -> u32 {
            self.latency_samples
        }
    }

    /// Renders `source` into one interleaved device buffer of `channels`,
    /// advancing `sample_position` by the frames written.
    ///
    /// The buffer is rendered in pieces no longer than the scratch planes,
    /// each at its own position. The source renders mono for a mono device
    /// and stereo otherwise; a device with more channels gets stereo in its
    /// first two and silence in the rest. Returns false, after writing
    /// silence, only for a buffer that is not whole frames.
    pub(super) fn fill_device_buffer(
        source: &mut impl RealtimeAudioSource,
        planar: &mut [Vec<f32>; 2],
        data: &mut [f32],
        channels: usize,
        device_rate: u32,
        sample_position: &mut i64,
    ) -> bool {
        let piece = planar[0].len();
        if channels == 0 || piece == 0 || !data.len().is_multiple_of(channels) {
            data.fill(0.0);
            return false;
        }
        let rendered = channels.min(2);
        for chunk in data.chunks_mut(piece * channels) {
            let frames = chunk.len() / channels;
            let ctx = ProcessCtx {
                position: MediaTime::from_ticks(
                    sample_position
                        .saturating_mul(TICKS_PER_SECOND)
                        .div_euclid(i64::from(device_rate)),
                ),
                sample_position: *sample_position,
                sample_rate: device_rate,
                transport: TransportState::Playing,
            };
            let (left, right) = planar.split_at_mut(1);
            if rendered == 1 {
                source.render(&ctx, &mut [&mut left[0][..frames]]);
            } else {
                source.render(&ctx, &mut [&mut left[0][..frames], &mut right[0][..frames]]);
            }
            for (frame, destination) in chunk.chunks_exact_mut(channels).enumerate() {
                for (channel, sample) in destination.iter_mut().enumerate() {
                    *sample = if channel < rendered {
                        planar[channel][frame]
                    } else {
                        0.0
                    };
                }
            }
            *sample_position =
                sample_position.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX));
        }
        true
    }
}

#[cfg(feature = "cpal-host")]
pub use realtime::*;

#[cfg(test)]
#[path = "audio_host_tests.rs"]
mod tests;
