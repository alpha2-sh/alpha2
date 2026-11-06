//! Encoder sessions behind traits, and the sink that turns an export's pictures and mixed audio
//! into a container's tracks.

use std::collections::VecDeque;

use alpha2_container::{
    AudioCodec, AudioTrack, ContainerWriter, Packet, TrackId, TrackInfo, TrackKind, VideoCodec,
    VideoTrack,
};
use alpha2_media::{
    AudioBlock, AudioBlockSink, CancelToken, ColorMetadata, EncodeError, Orientation, PixelLayout,
    SpeakerLayout, VideoRateControl,
};
use alpha2_time::{FrameRate, MediaTime};
use bytes::Bytes;

use crate::audio_frames::EncoderAudioInput;
use crate::rate_control::AppliedRateControl;
use crate::video_stream::Submit;

/// What a video encoder is asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoEncodeConfig {
    /// The codec.
    pub codec: alpha2_media::VideoCodec,
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// Frames per second.
    pub frame_rate: FrameRate,
    /// The planes each frame arrives in.
    pub layout: PixelLayout,
    /// The rate control the export resolved.
    pub rate: VideoRateControl,
    /// The color the bitstream is tagged with.
    pub color: ColorMetadata,
    /// Frames between forced keyframes.
    pub keyframe_interval: u32,
    /// Ask for a software encoder (a retry after a hardware encoder failed).
    pub software: bool,
}

/// One encoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPacket {
    /// Presentation time, in the session's time units.
    pub pts: i64,
    /// Decode time, in the same units.
    pub dts: i64,
    /// A sync sample.
    pub keyframe: bool,
    /// The frame, as the container stores it (length-prefixed NAL units for H.264 and HEVC).
    pub data: Bytes,
}

/// What an encoder session had on a receive.
#[derive(Debug)]
pub enum EncodePoll {
    /// A packet.
    Packet(EncodedPacket),
    /// Nothing yet.
    Pending,
    /// Input ended and every packet was given.
    Drained,
}

/// One video encoder session, used from one thread at a time.
pub trait VideoEncodeBackend: Send {
    /// The encoder's name, for reports.
    fn name(&self) -> &str;

    /// Whether it runs on hardware.
    fn hardware(&self) -> bool;

    /// Units per second of the timestamps it takes and gives.
    fn timescale(&self) -> u32;

    /// Queues a frame at `pts` whose planes are in the configured layout.
    ///
    /// # Errors
    /// A failed encoder.
    fn submit(
        &mut self,
        pts: i64,
        planes: &[crate::export::PlaneBuffer],
        keyframe: bool,
    ) -> Result<Submit, EncodeError>;

    /// Takes an encoded packet, in decode order. With `wait`, blocks briefly for one.
    ///
    /// # Errors
    /// A failed encoder.
    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError>;

    /// No more frames follow.
    ///
    /// # Errors
    /// A failed encoder.
    fn finish_input(&mut self) -> Result<(), EncodeError>;

    /// The decoder configuration record (`avcC`, `hvcC`, `av1C`), once the first packet is out.
    fn codec_private(&self) -> Option<Bytes>;

    /// Which form of the rate control the encoder runs.
    fn applied_rate_control(&self) -> AppliedRateControl;
}

/// Opens video encoder sessions.
pub trait VideoEncoders: Send + Sync {
    /// A session for `config`.
    ///
    /// # Errors
    /// [`EncodeError::EncodersRefused`] naming every encoder tried.
    fn open(&self, config: &VideoEncodeConfig) -> Result<Box<dyn VideoEncodeBackend>, EncodeError>;
}

/// What an audio encoder is asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioEncodeConfig {
    /// The codec.
    pub codec: alpha2_media::AudioCodec,
    /// The project's sample rate, which the audio arrives at.
    pub project_rate: u32,
    /// The speakers, one plane each.
    pub speakers: SpeakerLayout,
    /// Bits per second.
    pub bitrate: u32,
}

/// One audio encoder session, used from one thread at a time.
pub trait AudioEncodeBackend: Send {
    /// The encoder's name, for reports.
    fn name(&self) -> &str;

    /// The rate it encodes at.
    fn sample_rate(&self) -> u32;

    /// Samples per channel in each frame it takes.
    fn frame_size(&self) -> usize;

    /// Queues one frame of planar samples starting at sample `pts` of the encoded stream; only
    /// the last frame may be short.
    ///
    /// # Errors
    /// A failed encoder.
    fn submit(&mut self, pts: i64, planes: &[Vec<f32>]) -> Result<Submit, EncodeError>;

    /// Takes an encoded packet, timestamps in samples at [`Self::sample_rate`] counted from the
    /// first decoded sample, priming included.
    ///
    /// # Errors
    /// A failed encoder.
    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError>;

    /// No more frames follow.
    ///
    /// # Errors
    /// A failed encoder.
    fn finish_input(&mut self) -> Result<(), EncodeError>;

    /// The codec's setup data (`AudioSpecificConfig`, `OpusHead`), once known.
    fn codec_private(&self) -> Option<Bytes>;

    /// Samples of priming at the start of the decoded stream, which the container's edit list
    /// skips.
    fn priming(&self) -> u32;
}

/// Opens audio encoder sessions.
pub trait AudioEncoders: Send + Sync {
    /// A session for `config`.
    ///
    /// # Errors
    /// [`EncodeError::EncodersRefused`] naming every encoder tried.
    fn open(&self, config: &AudioEncodeConfig) -> Result<Box<dyn AudioEncodeBackend>, EncodeError>;
}

fn container_video_codec(codec: alpha2_media::VideoCodec) -> VideoCodec {
    match codec {
        alpha2_media::VideoCodec::H264 => VideoCodec::H264,
        alpha2_media::VideoCodec::Hevc => VideoCodec::Hevc,
        alpha2_media::VideoCodec::Vp9 => VideoCodec::Vp9,
        alpha2_media::VideoCodec::Av1 => VideoCodec::Av1,
        alpha2_media::VideoCodec::Mpeg4Part2 => VideoCodec::Mpeg4Part2,
        alpha2_media::VideoCodec::ProRes => VideoCodec::ProRes(alpha2_container::FourCc(*b"apch")),
    }
}

fn container_audio_codec(codec: alpha2_media::AudioCodec) -> AudioCodec {
    match codec {
        alpha2_media::AudioCodec::Aac => AudioCodec::Aac,
        alpha2_media::AudioCodec::Opus => AudioCodec::Opus,
    }
}

/// A track waiting for its encoder's configuration record before it is added.
struct PendingTrack {
    id: Option<TrackId>,
    queued: VecDeque<EncodedPacket>,
}

impl PendingTrack {
    const fn new() -> Self {
        Self {
            id: None,
            queued: VecDeque::new(),
        }
    }
}

/// Turns an export's pictures and mixed audio into a container's tracks.
///
/// Tracks are added once every encoder has published its configuration record; packets before
/// that wait. The writer interleaves the tracks.
pub struct EncodeSink {
    writer: Option<Box<dyn ContainerWriter>>,
    video: Box<dyn VideoEncodeBackend>,
    video_config: VideoEncodeConfig,
    video_track: PendingTrack,
    audio: Option<(
        Box<dyn AudioEncodeBackend>,
        EncoderAudioInput,
        AudioEncodeConfig,
    )>,
    audio_track: PendingTrack,
    audio_pts: i64,
}

impl std::fmt::Debug for EncodeSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncodeSink")
            .field("video", &self.video.name())
            .finish_non_exhaustive()
    }
}

impl EncodeSink {
    /// A sink writing `video` (and `audio`, when the export has sound) into `writer`.
    #[must_use]
    pub fn new(
        writer: Box<dyn ContainerWriter>,
        video: Box<dyn VideoEncodeBackend>,
        video_config: VideoEncodeConfig,
        audio: Option<(Box<dyn AudioEncodeBackend>, AudioEncodeConfig)>,
    ) -> Self {
        let audio = audio.map(|(encoder, config)| {
            let input = EncoderAudioInput::new(
                config.project_rate,
                encoder.sample_rate(),
                config.speakers.speakers().len(),
                encoder.frame_size(),
            );
            (encoder, input, config)
        });
        Self {
            writer: Some(writer),
            video,
            video_config,
            video_track: PendingTrack::new(),
            audio,
            audio_track: PendingTrack::new(),
            audio_pts: 0,
        }
    }

    /// The video encoder's name and the rate control it ran.
    #[must_use]
    pub fn video_encoder(&self) -> (&str, bool, AppliedRateControl) {
        (
            self.video.name(),
            self.video.hardware(),
            self.video.applied_rate_control(),
        )
    }

    fn writer(&mut self) -> Result<&mut Box<dyn ContainerWriter>, EncodeError> {
        self.writer
            .as_mut()
            .ok_or_else(|| EncodeError::Adapter("the output is already finished".into()))
    }

    fn video_info(&self) -> Option<TrackInfo> {
        let private = self.video.codec_private()?;
        let config = &self.video_config;
        Some(TrackInfo {
            id: TrackId(0),
            kind: TrackKind::Video,
            timescale: self.video.timescale(),
            duration: None,
            language: None,
            title: None,
            default: true,
            enabled: true,
            video: Some(VideoTrack {
                codec: container_video_codec(config.codec),
                codec_private: private,
                in_band_parameter_sets: false,
                width: config.width,
                height: config.height,
                orientation: Orientation::default(),
                sample_aspect_ratio: None,
                color: Some(config.color),
                clean_aperture: None,
                frame_rate: Some(config.frame_rate),
                alpha: config.layout.alpha,
            }),
            audio: None,
        })
    }

    fn audio_info(&self) -> Option<TrackInfo> {
        let (encoder, _, config) = self.audio.as_ref()?;
        let private = encoder.codec_private()?;
        Some(TrackInfo {
            id: TrackId(0),
            kind: TrackKind::Audio,
            timescale: encoder.sample_rate(),
            duration: None,
            language: None,
            title: None,
            default: true,
            enabled: true,
            video: None,
            audio: Some(AudioTrack {
                codec: container_audio_codec(config.codec),
                codec_private: private,
                sample_rate: encoder.sample_rate(),
                channels: u32::try_from(config.speakers.speakers().len()).unwrap_or(2),
                speakers: Some(config.speakers),
                bits_per_sample: None,
                priming: encoder.priming(),
            }),
        })
    }

    /// Moves packets the encoders have into their tracks.
    fn drain(&mut self, wait_video: bool, wait_audio: bool) -> Result<(bool, bool), EncodeError> {
        let mut video_done = false;
        loop {
            match self.video.receive(wait_video)? {
                EncodePoll::Packet(packet) => self.video_track.queued.push_back(packet),
                EncodePoll::Pending => break,
                EncodePoll::Drained => {
                    video_done = true;
                    break;
                }
            }
        }
        let mut audio_done = self.audio.is_none();
        if let Some((encoder, _, _)) = self.audio.as_mut() {
            loop {
                match encoder.receive(wait_audio)? {
                    EncodePoll::Packet(packet) => self.audio_track.queued.push_back(packet),
                    EncodePoll::Pending => break,
                    EncodePoll::Drained => {
                        audio_done = true;
                        break;
                    }
                }
            }
        }
        self.flush_tracks()?;
        Ok((video_done, audio_done))
    }

    /// Adds tracks whose configuration is known and writes their queued packets.
    fn flush_tracks(&mut self) -> Result<(), EncodeError> {
        // Containers add every track before the first packet; video is always the first track.
        if self.video_track.id.is_none() {
            let Some(video) = self.video_info() else {
                return Ok(());
            };
            let audio = match self.audio {
                Some(_) => match self.audio_info() {
                    Some(info) => Some(info),
                    None => return Ok(()),
                },
                None => None,
            };
            self.video_track.id = Some(self.writer()?.add_track(&video)?);
            if let Some(info) = audio {
                self.audio_track.id = Some(self.writer()?.add_track(&info)?);
            }
        }
        let Some(video) = self.video_track.id else {
            return Ok(());
        };
        let priming = self
            .audio
            .as_ref()
            .map_or(0, |(encoder, _, _)| encoder.priming());
        let frame_duration = i64::from(self.video_config.frame_rate.denominator())
            * i64::from(self.video.timescale())
            / i64::from(self.video_config.frame_rate.numerator().max(1));
        while let Some(packet) = self.video_track.queued.pop_front() {
            let packet = Packet {
                track: video,
                pts: packet.pts,
                dts: packet.dts,
                duration: frame_duration,
                keyframe: packet.keyframe,
                data: packet.data,
                discard: (0, 0),
            };
            self.writer()?.write(packet)?;
        }
        if let Some(audio) = self.audio_track.id {
            while let Some(packet) = self.audio_track.queued.pop_front() {
                let frame = self
                    .audio
                    .as_ref()
                    .map_or(1_024, |(encoder, _, _)| encoder.frame_size());
                let packet = Packet {
                    track: audio,
                    pts: packet.pts - i64::from(priming),
                    dts: packet.dts - i64::from(priming),
                    duration: i64::try_from(frame).unwrap_or(1_024),
                    keyframe: true,
                    data: packet.data,
                    discard: (0, 0),
                };
                self.writer()?.write(packet)?;
            }
        }
        Ok(())
    }

    fn submit_audio(&mut self, frames: Vec<Vec<Vec<f32>>>) -> Result<(), EncodeError> {
        for frame in frames {
            let length = i64::try_from(frame.first().map_or(0, Vec::len)).unwrap_or(0);
            loop {
                let pts = self.audio_pts;
                let Some((encoder, _, _)) = self.audio.as_mut() else {
                    return Ok(());
                };
                match encoder.submit(pts, &frame)? {
                    Submit::Accepted => break,
                    Submit::Busy => {
                        self.drain(false, true)?;
                    }
                }
            }
            self.audio_pts += length;
            self.drain(false, false)?;
        }
        Ok(())
    }

    /// Ends both inputs, drains the encoders and closes the output.
    ///
    /// # Errors
    /// An encoder or writer failure, or an encoder that never published its configuration.
    pub fn finish(mut self) -> Result<(), EncodeError> {
        let tail = self
            .audio
            .as_mut()
            .map(|(_, input, _)| input.finish())
            .unwrap_or_default();
        self.submit_audio(tail)?;
        self.video.finish_input()?;
        if let Some((encoder, _, _)) = self.audio.as_mut() {
            encoder.finish_input()?;
        }
        let mut done = (false, self.audio.is_none());
        let mut idle = 0;
        while !(done.0 && done.1) {
            let (video, audio) = self.drain(!done.0, !done.1)?;
            done = (done.0 || video, done.1 || audio);
            idle += 1;
            if idle > 100_000 {
                return Err(EncodeError::Adapter("the encoders never drained".into()));
            }
        }
        self.flush_tracks()?;
        if self.video_track.id.is_none() {
            return Err(EncodeError::Adapter(
                "the video encoder published no configuration".into(),
            ));
        }
        let writer = self
            .writer
            .take()
            .ok_or_else(|| EncodeError::Adapter("the output is already finished".into()))?;
        writer.finish().map_err(EncodeError::from)
    }
}

impl crate::export::VideoSink for EncodeSink {
    fn write_video(
        &mut self,
        index: u64,
        _time: MediaTime,
        _layout: PixelLayout,
        planes: &[crate::export::PlaneBuffer],
    ) -> Result<(), EncodeError> {
        let rate = self.video_config.frame_rate;
        let pts = i64::try_from(index).unwrap_or(i64::MAX)
            * i64::from(rate.denominator())
            * i64::from(self.video.timescale())
            / i64::from(rate.numerator().max(1));
        let interval = u64::from(self.video_config.keyframe_interval.max(1));
        let keyframe = index.is_multiple_of(interval);
        loop {
            match self.video.submit(pts, planes, keyframe)? {
                Submit::Accepted => break,
                Submit::Busy => {
                    self.drain(true, false)?;
                }
            }
        }
        self.drain(false, false)?;
        Ok(())
    }
}

impl AudioBlockSink for EncodeSink {
    fn write_audio(&mut self, block: &AudioBlock, cancel: &CancelToken) -> Result<(), EncodeError> {
        if cancel.is_cancelled() {
            return Err(EncodeError::Cancelled);
        }
        let Some((_, input, _)) = self.audio.as_mut() else {
            return Ok(());
        };
        let frames = input.push(block)?;
        self.submit_audio(frames)
    }

    fn finish_audio(&mut self, _cancel: &CancelToken) -> Result<(), EncodeError> {
        Ok(())
    }

    fn abort_audio(&mut self) -> Result<(), EncodeError> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "encode_tests.rs"]
mod tests;
