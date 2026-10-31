//! Host-independent media vocabulary and I/O contracts.
//!
//! These types describe media itself, not any application built on it: source
//! identity and color, decoded frames and audio blocks, the decoder and encoder
//! traits that platform bindings implement, and output rate control. The
//! renderer, the timeline model, and the platform bindings all speak them, and
//! none of those owns them.

mod color;
mod container;
mod ids;
mod io;
mod pixels;
mod rate_control;

pub use color::{
    ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, TransferFunction,
    YcbcrLevels, YuvEncoding,
};
pub use container::{Container, ContainerError, resolve_export_format};
pub use ids::{IdError, SourceId};
pub use io::{
    AudioBlock, AudioBlockProvider, AudioBlockSink, AudioSpan, CancelToken, ChromaLocation,
    DecodeError, DecodeEvent, DecodeRequest, DecodeRequestKind, DecodeResult, DecodedVideoFrame,
    Decoder, EmojiPolicy, EncodeError, EncodeEvent, EncodeSink, EncoderRefusal,
    GPU_TIMEOUT_MESSAGE, GpuFault, HostTexture, HostTexturePixels, MAX_AUDIO_CHANNELS,
    MAX_FRAME_DIMENSION, MAX_FRAME_PIXELS, OpenFailureKind, Orientation, PixelFormat, Plane,
    Premultiplication, RasterColorSpace, Rect, Rotation, SampleAspectRatio, Speaker, SpeakerLayout,
    StreamInfo, StreamKind, TextAlign, TextTexture, YuvFormat, frame_size_supported,
    sample_ticks_floor,
};
pub use pixels::{
    BitDepth, Chroma, OutputPixels, OutputPixelsError, PixelLayout, PlaneLayout, resolve_pixels,
};
pub use rate_control::{
    AudioCodec, BitrateMode, FRAME_RATE_EXPONENT, MIN_VIDEO_BITRATE, QualityLevel, REFERENCE_FPS,
    REFERENCE_PIXELS, RESOLUTION_EXPONENT, RateControlError, VideoCodec, VideoRateControl,
    codec_factor, level_quantizer, resolve_audio_bitrate, resolve_video_rate_control,
};
