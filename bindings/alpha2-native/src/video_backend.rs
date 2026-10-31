//! The video encoder a native sink drives: an `FFmpeg` encoder or `OpenH264`.
//!
//! `OpenH264` answers in `FFmpeg`'s terms (`EncoderDrainError` when it has no packet ready).

use std::fmt;

use alpha2_ffmpeg_bridge::EncoderContext;
use rsmpeg::avcodec::{AVCodecParameters, AVPacket};
use rsmpeg::avutil::{AVFrame, AVRational};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use crate::openh264::OpenH264Encoder;

pub(crate) enum VideoEncoderBackend {
    /// Closed under the `MediaFoundation` lock when it is an `_mf` encoder.
    Ffmpeg(EncoderContext),
    OpenH264(Box<OpenH264Encoder>),
}

#[derive(Debug)]
pub(crate) enum VideoSendError {
    Ffmpeg(RsmpegError),
    OpenH264(String),
}

impl fmt::Display for VideoSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffmpeg(error) => error.fmt(formatter),
            Self::OpenH264(message) => formatter.write_str(message),
        }
    }
}

impl VideoEncoderBackend {
    pub(crate) fn time_base(&self) -> AVRational {
        match self {
            Self::Ffmpeg(encoder) => encoder.time_base,
            Self::OpenH264(encoder) => encoder.time_base(),
        }
    }

    pub(crate) fn codec_id(&self) -> ffi::AVCodecID {
        match self {
            Self::Ffmpeg(encoder) => encoder.codec_id,
            Self::OpenH264(_) => ffi::AV_CODEC_ID_H264,
        }
    }

    pub(crate) fn codec_parameters(&self) -> AVCodecParameters {
        match self {
            Self::Ffmpeg(encoder) => encoder.extract_codecpar(),
            Self::OpenH264(encoder) => encoder.codec_parameters(),
        }
    }

    /// Hands one frame over, or with `None` ends the stream.
    pub(crate) fn send_frame(&mut self, frame: Option<&AVFrame>) -> Result<(), VideoSendError> {
        match self {
            Self::Ffmpeg(encoder) => encoder.send_frame(frame).map_err(VideoSendError::Ffmpeg),
            Self::OpenH264(encoder) => encoder.send(frame).map_err(VideoSendError::OpenH264),
        }
    }

    pub(crate) fn receive_packet(&mut self) -> Result<AVPacket, RsmpegError> {
        match self {
            Self::Ffmpeg(encoder) => encoder.receive_packet(),
            Self::OpenH264(encoder) => encoder.receive(),
        }
    }
}
