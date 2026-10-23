//! What an audio encoder is given: the project's audio, at a rate the codec
//! carries, in whole frames and one last frame of exactly what is left, in
//! the sample format the encoder takes.
//!
//! The native AAC encoder takes only the MPEG-4 sample rates and `libopus`
//! only 8, 12, 16, 24 and 48 kHz, while a project may run at any rate from 8
//! to 96 kHz, every one of which exports. A project at a rate the encoder
//! does not carry is exported at the next one up (48 kHz for Opus from
//! anything above 24), resampled by `alpha2_audio::Resampler`, the converter
//! that brings every source to the project rate. The encoder is told where the
//! audio ends: its last frame is as long as what is left (`FFmpeg`'s `aac`
//! takes a short last frame, and `libopus` pads one itself), so the track ends
//! where the export's audio does instead of up to a frame later.

use std::collections::VecDeque;

use alpha2_media::{AudioBlock, EncodeError};
use rsmpeg::avcodec::{AVCodecContext, AVCodecRef};
use rsmpeg::avutil::{AVChannelLayout, AVRational, ra};
use rsmpeg::ffi;

use crate::audio_timeline::SourceTimeline;
use crate::capabilities::NativeAudioCodec;
use crate::convert::encode_adapter_error;

/// The audio stream an export encodes beside its video.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioStreamConfig {
    /// The codec to encode.
    pub codec: NativeAudioCodec,
    /// Project audio sample rate.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Resolved bitrate in bits per second; the host resolves the level.
    pub bit_rate: u32,
}

/// An opened audio encoder, before it has a stream.
pub(crate) struct OpenedAudioEncoder {
    pub(crate) encoder: AVCodecContext,
    pub(crate) encoder_time_base: AVRational,
    /// The input the encoder takes its frames from.
    pub(crate) input: EncoderAudioInput,
    /// The rate it is encoded at ([`encoder_sample_rate`]).
    pub(crate) encoder_rate: u32,
    /// The sample format its frames carry ([`encoder_sample_format`]).
    pub(crate) sample_format: ffi::AVSampleFormat,
}

/// Opens the encoder for `config`, with global headers for a container that
/// keeps them in its header, which every export container does.
///
/// # Errors
/// When the runtime has no encoder for the codec, it takes none of the
/// sample formats alpha2 can hand it, it has no fixed frame size, or it does
/// not open.
pub(crate) fn open_audio_encoder(
    config: AudioStreamConfig,
    global_header: bool,
) -> Result<OpenedAudioEncoder, EncodeError> {
    let AudioStreamConfig {
        codec,
        sample_rate,
        channels,
        bit_rate,
    } = config;
    let encoder_codec = crate::capabilities::audio_encoder(codec).ok_or_else(|| {
        EncodeError::Adapter(format!(
            "this FFmpeg runtime has no {} encoder",
            codec.as_str()
        ))
    })?;
    let sample_format = encoder_sample_format(&encoder_codec).ok_or_else(|| {
        EncodeError::Adapter(format!(
            "the {} encoder takes neither float nor 16-bit samples",
            codec.as_str()
        ))
    })?;
    let encoder_rate = encoder_sample_rate(&encoder_codec, sample_rate);
    let rate = i32::try_from(encoder_rate)
        .map_err(|_| EncodeError::Adapter("audio sample rate is out of range".into()))?;
    let encoder_time_base = ra(1, rate);
    let mut encoder = AVCodecContext::new(&encoder_codec);
    encoder.set_sample_rate(rate);
    encoder.set_sample_fmt(sample_format);
    encoder.set_ch_layout(AVChannelLayout::from_nb_channels(i32::from(channels)).into_inner());
    encoder.set_time_base(encoder_time_base);
    encoder.set_bit_rate(i64::from(bit_rate));
    if global_header {
        encoder.set_flags(
            encoder.flags | i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0),
        );
    }
    encoder.open(None).map_err(encode_adapter_error)?;
    let frame_size = usize::try_from(encoder.frame_size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| {
            EncodeError::Adapter(format!(
                "the {} encoder has no fixed frame size",
                codec.as_str()
            ))
        })?;
    Ok(OpenedAudioEncoder {
        encoder,
        encoder_time_base,
        input: EncoderAudioInput::new(sample_rate, encoder_rate, usize::from(channels), frame_size),
        encoder_rate,
        sample_format,
    })
}

/// The sample format alpha2 hands `codec`: planar float, else interleaved
/// float, else interleaved 16-bit, whichever it takes first in that order.
/// An encoder that lists no formats takes planar float. `None` when it takes
/// none of the three.
pub(crate) fn encoder_sample_format(codec: &AVCodecRef<'_>) -> Option<ffi::AVSampleFormat> {
    let Some(formats) = codec.sample_fmts() else {
        return Some(ffi::AV_SAMPLE_FMT_FLTP);
    };
    [
        ffi::AV_SAMPLE_FMT_FLTP,
        ffi::AV_SAMPLE_FMT_FLT,
        ffi::AV_SAMPLE_FMT_S16,
    ]
    .into_iter()
    .find(|format| formats.contains(format))
}

/// The rate `codec` encodes a project at `project_rate` with: that rate when
/// the codec takes it, else the lowest it takes above it, else its highest.
pub(crate) fn encoder_sample_rate(codec: &AVCodecRef<'_>, project_rate: u32) -> u32 {
    let Some(rates) = codec.supported_samplerates() else {
        return project_rate;
    };
    let rates = rates
        .iter()
        .filter_map(|rate| u32::try_from(*rate).ok())
        .filter(|rate| *rate > 0)
        .collect::<Vec<_>>();
    if rates.contains(&project_rate) {
        return project_rate;
    }
    rates
        .iter()
        .copied()
        .filter(|rate| *rate > project_rate)
        .min()
        .or_else(|| rates.iter().copied().max())
        .unwrap_or(project_rate)
}

/// Project-rate audio blocks, cut into the frames an encoder takes at its own
/// rate.
pub(crate) struct EncoderAudioInput {
    project_rate: u32,
    channels: usize,
    frame_size: usize,
    /// Brings the project's audio to the encoder's rate, when they differ.
    resampler: Option<SourceTimeline>,
    /// Samples at the encoder's rate not yet handed over.
    pending: Vec<VecDeque<f32>>,
    /// Project-rate samples accepted.
    received: u64,
}

impl EncoderAudioInput {
    /// Input for an encoder of `frame_size` samples at `encoder_rate` from
    /// `channels` channels of audio at `project_rate`.
    pub(crate) fn new(
        project_rate: u32,
        encoder_rate: u32,
        channels: usize,
        frame_size: usize,
    ) -> Self {
        Self {
            project_rate,
            channels,
            frame_size,
            resampler: (encoder_rate != project_rate)
                .then(|| SourceTimeline::new(encoder_rate, channels, Some(0))),
            pending: (0..channels).map(|_| VecDeque::new()).collect(),
            received: 0,
        }
    }

    /// Takes the next block, which must start where the last one ended, and
    /// returns every whole frame it completes.
    ///
    /// # Errors
    /// A block of another rate or channel count, of unequal channels, or out
    /// of order.
    pub(crate) fn push(&mut self, block: &AudioBlock) -> Result<Vec<Vec<Vec<f32>>>, EncodeError> {
        if block.sample_rate != self.project_rate
            || block.channels.len() != self.channels
            || block.channels.is_empty()
        {
            return Err(EncodeError::Adapter("audio block format changed".into()));
        }
        let frames = block.channels[0].len();
        if block.channels.iter().any(|channel| channel.len() != frames) {
            return Err(EncodeError::Adapter("audio channel lengths differ".into()));
        }
        if u64::try_from(block.start_sample).ok() != Some(self.received) {
            return Err(EncodeError::InvalidTimestamp);
        }
        match self.resampler.as_mut() {
            Some(resampler) => {
                resampler
                    .place(
                        self.project_rate,
                        block.start_sample,
                        block.channels.clone(),
                    )
                    .map_err(|error| EncodeError::Adapter(error.to_string()))?;
                resampler.produce(&mut self.pending, usize::MAX);
            }
            None => {
                for (pending, channel) in self.pending.iter_mut().zip(&block.channels) {
                    pending.extend(channel.iter().copied());
                }
            }
        }
        self.received = self
            .received
            .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
        let mut ready = Vec::new();
        while self.pending[0].len() >= self.frame_size {
            ready.push(self.take(self.frame_size));
        }
        Ok(ready)
    }

    /// Ends the input: the frames still to encode, the last one as long as
    /// what is left, none when nothing is.
    pub(crate) fn finish(&mut self) -> Vec<Vec<Vec<f32>>> {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.finish();
            resampler.produce(&mut self.pending, usize::MAX);
        }
        let mut frames = Vec::new();
        while !self.pending[0].is_empty() {
            frames.push(self.take(self.frame_size.min(self.pending[0].len())));
        }
        frames
    }

    /// The input time accepted so far, in ticks: what the encoder reports
    /// back as consumed once it has encoded the frames handed over with it.
    pub(crate) fn received_ticks(&self) -> i64 {
        i64::try_from(self.received)
            .unwrap_or(i64::MAX)
            .saturating_mul(alpha2_time::TICKS_PER_SECOND)
            .div_euclid(i64::from(self.project_rate))
    }

    fn take(&mut self, samples: usize) -> Vec<Vec<f32>> {
        self.pending
            .iter_mut()
            .map(|pending| pending.drain(..samples).collect())
            .collect()
    }
}
