//! Audio encoder input: project audio at a rate the codec carries, cut into whole frames plus an
//! exact last frame, in the encoder's sample format.
//!
//! `aac` takes only MPEG-4 rates and `libopus` only 8, 12, 16, 24 and 48 kHz, while projects run at
//! 8 to 96 kHz. Unsupported rates export at the next one up (48 kHz for Opus above 24), resampled
//! by `alpha2_audio::Resampler`. The last frame is exactly what is left (`aac` accepts a short
//! frame, `libopus` pads it), so the track ends where the export's audio does.

use alpha2_media::EncodeError;
use rsmpeg::avcodec::{AVCodecContext, AVCodecRef};
use rsmpeg::avutil::{AVChannelLayout, AVRational, ra};
use rsmpeg::ffi;

use crate::capabilities::NativeAudioCodec;
use crate::convert::encode_adapter_error;
pub(crate) use alpha2_host_core::audio_frames::EncoderAudioInput;

/// The audio stream an export encodes beside its video.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioStreamConfig {
    /// The codec to encode.
    pub codec: NativeAudioCodec,
    /// Project audio sample rate.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Bitrate in bits per second; the host resolves the level.
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

/// Opens the encoder for `config` with global headers, which every export container uses.
///
/// # Errors
/// When the runtime has no encoder for the codec, it takes none of the sample formats alpha2 can
/// provide, it has no fixed frame size, or it does not open.
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

/// The sample format alpha2 hands `codec`: the first it takes of planar float, interleaved float,
/// interleaved 16-bit; planar float if it lists none; `None` if it takes none of the three.
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

/// The rate `codec` encodes a `project_rate` project at
/// ([`alpha2_host_core::audio_frames::encoder_sample_rate`]).
pub(crate) fn encoder_sample_rate(codec: &AVCodecRef<'_>, project_rate: u32) -> u32 {
    let rates = codec.supported_samplerates().map(|rates| {
        rates
            .iter()
            .filter_map(|rate| u32::try_from(*rate).ok())
            .collect::<Vec<_>>()
    });
    alpha2_host_core::audio_frames::encoder_sample_rate(rates.as_deref(), project_rate)
}
