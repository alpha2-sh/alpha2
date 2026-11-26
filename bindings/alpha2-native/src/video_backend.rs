//! The video encoder a native sink drives: an `FFmpeg` encoder or `OpenH264`.
//!
//! `OpenH264` answers in `FFmpeg`'s terms (`EncoderDrainError` when it has no packet ready).

use std::collections::VecDeque;
use std::fmt;

use alpha2_ffmpeg_bridge::EncoderContext;
use alpha2_media::{ColorMetadata, EncodeError};
use rsmpeg::avcodec::{AVCodecParameters, AVPacket};
use rsmpeg::avutil::{AVFrame, AVRational};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use crate::bitstream::EncodedVideoFilter;
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

pub(crate) struct TaggedVideoEncoder {
    backend: VideoEncoderBackend,
    color: (
        ffi::AVColorPrimaries,
        ffi::AVColorTransferCharacteristic,
        ffi::AVColorSpace,
        ffi::AVColorRange,
    ),
    filter: Option<EncodedVideoFilter>,
    filtered: VecDeque<AVPacket>,
    filter_flushed: bool,
}

#[derive(Debug)]
pub(crate) enum VideoReceiveError {
    Codec(RsmpegError),
    Filter(EncodeError),
}

impl TaggedVideoEncoder {
    pub(crate) fn new(
        backend: VideoEncoderBackend,
        color: ColorMetadata,
    ) -> Result<Self, EncodeError> {
        let filter =
            EncodedVideoFilter::tagging(&backend.codec_parameters(), color, backend.time_base())?;
        Ok(Self {
            backend,
            color: crate::convert::color_tags(color),
            filter,
            filtered: VecDeque::new(),
            filter_flushed: false,
        })
    }

    pub(crate) fn time_base(&self) -> AVRational {
        self.backend.time_base()
    }

    pub(crate) fn codec_id(&self) -> ffi::AVCodecID {
        self.backend.codec_id()
    }

    pub(crate) fn codec_parameters(&self) -> AVCodecParameters {
        self.filter.as_ref().map_or_else(
            || self.backend.codec_parameters(),
            EncodedVideoFilter::parameters,
        )
    }

    pub(crate) fn send_frame(&mut self, frame: Option<&AVFrame>) -> Result<(), VideoSendError> {
        let Some(frame) = frame else {
            return self.backend.send_frame(None);
        };
        let mut tagged = frame.clone();
        alpha2_ffmpeg_bridge::set_frame_color(&mut tagged, self.color);
        self.backend.send_frame(Some(&tagged))
    }

    pub(crate) fn receive_packet(&mut self) -> Result<AVPacket, VideoReceiveError> {
        loop {
            if let Some(packet) = self.filtered.pop_front() {
                return Ok(packet);
            }
            let Some(filter) = self.filter.as_mut() else {
                return self
                    .backend
                    .receive_packet()
                    .map_err(VideoReceiveError::Codec);
            };
            match self.backend.receive_packet() {
                Ok(packet) => self
                    .filtered
                    .extend(filter.filter(packet).map_err(VideoReceiveError::Filter)?),
                Err(RsmpegError::EncoderFlushedError) if !self.filter_flushed => {
                    self.filter_flushed = true;
                    self.filtered
                        .extend(filter.flush().map_err(VideoReceiveError::Filter)?);
                }
                Err(error) => return Err(VideoReceiveError::Codec(error)),
            }
        }
    }
}
