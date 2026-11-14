//! Encoder sessions behind traits, and the sink that turns an export's pictures and mixed audio
//! into a container's tracks.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alpha2_container::{
    AudioCodec, AudioTrack, ContainerWriter, Packet, TrackId, TrackInfo, TrackKind, VideoCodec,
    VideoTrack,
};
use alpha2_media::{
    AudioBlock, AudioBlockSink, CancelToken, ColorMetadata, EncodeError, Orientation, PixelLayout,
    SpeakerLayout, VideoRateControl,
};
use alpha2_time::FrameRate;
use bytes::Bytes;

use crate::audio_frames::EncoderAudioInput;
use crate::rate_control::AppliedRateControl;
use crate::video_stream::Submit;

/// One plane of a rendered picture.
#[derive(Clone, Debug, Default)]
pub struct PlaneBuffer {
    /// The rows, `stride` bytes apart.
    pub bytes: Vec<u8>,
    /// Bytes between rows.
    pub stride: usize,
    /// Width in samples.
    pub width: u32,
    /// Height in rows.
    pub height: u32,
}

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
        planes: &[PlaneBuffer],
        keyframe: bool,
    ) -> Result<Submit, EncodeError>;

    /// Takes an encoded packet, in decode order. With `wait`, blocks briefly for one. Once it
    /// answers [`EncodePoll::Drained`] it keeps answering it.
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
    /// first decoded sample, priming included. Once it answers [`EncodePoll::Drained`] it keeps
    /// answering it.
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

/// How long encoders may take no frame and hand out no packet before the sink gives up on them.
const DRAIN_STALL: Duration = Duration::from_secs(10);

/// The most one turn of a wait adds to a stall, so a process frozen inside a wait adds no more.
const STALL_TURN: Duration = Duration::from_millis(100);

/// The time a wait on the encoders has made no progress, added up turn by turn.
struct Stall {
    last: Instant,
    stalled: Duration,
    limit: Duration,
}

impl Stall {
    fn new(limit: Duration) -> Self {
        Self {
            last: Instant::now(),
            stalled: Duration::ZERO,
            limit,
        }
    }

    /// Records one turn of the wait; true once the turns without progress reach the limit.
    fn turn(&mut self, progressed: bool) -> bool {
        let now = Instant::now();
        let spent = now.saturating_duration_since(self.last).min(STALL_TURN);
        self.last = now;
        self.stalled = if progressed {
            Duration::ZERO
        } else {
            self.stalled + spent
        };
        self.stalled >= self.limit
    }
}

/// The container codec of `codec` encoded in `layout`: `ProRes` is 4444 (`ap4h`) with alpha, else
/// 422 HQ (`apch`), as [`crate::codecs::NativeVideoCodec::ProRes`] encodes it.
fn container_video_codec(codec: alpha2_media::VideoCodec, layout: PixelLayout) -> VideoCodec {
    match codec {
        alpha2_media::VideoCodec::H264 => VideoCodec::H264,
        alpha2_media::VideoCodec::Hevc => VideoCodec::Hevc,
        alpha2_media::VideoCodec::Vp9 => VideoCodec::Vp9,
        alpha2_media::VideoCodec::Av1 => VideoCodec::Av1,
        alpha2_media::VideoCodec::Mpeg4Part2 => VideoCodec::Mpeg4Part2,
        alpha2_media::VideoCodec::ProRes => {
            VideoCodec::ProRes(alpha2_container::FourCc(if layout.alpha {
                *b"ap4h"
            } else {
                *b"apch"
            }))
        }
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

/// How far audio written to the container may run ahead of the video written, in seconds.
const AUDIO_LEAD: i64 = 1;

/// Whether audio starting at `start` (in units of `rate` per second) is at most [`AUDIO_LEAD`]
/// past the video packet decoded at `video` (in units of `video_rate`); never before any video.
fn within_lead(start: i64, rate: u32, video: Option<i64>, video_rate: u32) -> bool {
    video.is_some_and(|video| {
        let limit = (i128::from(video) + i128::from(AUDIO_LEAD) * i128::from(video_rate))
            * i128::from(rate);
        i128::from(start) * i128::from(video_rate) <= limit
    })
}

/// Turns an export's pictures and mixed audio into a container's tracks.
///
/// Tracks are added once every encoder has published its configuration record; packets before
/// that wait. Audio goes to the writer at most one second ahead of the video written, so a mix
/// encoded before the pictures still reaches the writer interleaved with them.
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
    /// Samples handed to the audio encoder, at its rate.
    audio_pts: i64,
    /// The decode time of the last video packet written, in the video encoder's units.
    video_written: Option<i64>,
    /// Packets taken from both encoders.
    received: u64,
    /// Every input is in: held audio packets go out.
    finishing: bool,
    /// Stops the waits on a busy encoder.
    cancel: CancelToken,
    /// How long the encoders may make no progress before a wait on them fails.
    stall_limit: Duration,
    /// Frames the video encoder accepted.
    video_frames: u64,
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
    /// A sink writing `video` (and `audio`, when the export has sound) into `writer`; `cancel`
    /// stops its waits on a busy encoder.
    #[must_use]
    pub fn new(
        writer: Box<dyn ContainerWriter>,
        video: Box<dyn VideoEncodeBackend>,
        video_config: VideoEncodeConfig,
        audio: Option<(Box<dyn AudioEncodeBackend>, AudioEncodeConfig)>,
        cancel: CancelToken,
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
            video_written: None,
            received: 0,
            finishing: false,
            cancel,
            stall_limit: DRAIN_STALL,
            video_frames: 0,
        }
    }

    #[cfg(test)]
    fn with_stall_limit(mut self, limit: Duration) -> Self {
        self.stall_limit = limit;
        self
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
                codec: container_video_codec(config.codec, config.layout),
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
                reorder_depth: None,
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
                channels: u32::try_from(config.speakers.channels()).unwrap_or(u32::MAX),
                speakers: Some(config.speakers),
                bits_per_sample: None,
                priming: encoder.priming(),
                seek_preroll: None,
            }),
        })
    }

    /// Moves packets the encoders have into their tracks.
    fn drain(&mut self, wait_video: bool, wait_audio: bool) -> Result<(bool, bool), EncodeError> {
        let mut video_done = false;
        loop {
            let received = self.video.receive(wait_video);
            match received.map_err(|error| self.rejected(error))? {
                EncodePoll::Packet(packet) => {
                    self.received += 1;
                    self.video_track.queued.push_back(packet);
                }
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
                    EncodePoll::Packet(packet) => {
                        self.received += 1;
                        self.audio_track.queued.push_back(packet);
                    }
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
            self.video_written = Some(packet.dts);
            self.writer()?.write(packet)?;
        }
        if let (Some(audio), Some((encoder, _, _))) = (self.audio_track.id, self.audio.as_ref()) {
            let frame = i64::try_from(encoder.frame_size()).unwrap_or(i64::MAX);
            // Audio ends where the input does: a packet reaching past the samples written so far
            // waits, and at the end its duration stops at the last input sample. Encoders pad
            // their last frames; with the priming taken off, a packet starting at or after the
            // end decodes to padding alone.
            let end = self.audio_pts;
            let rate = encoder.sample_rate();
            let video_rate = self.video.timescale();
            while let Some(packet) = self.audio_track.queued.front() {
                let start = packet.pts - i64::from(priming);
                if !self.finishing
                    && (start + frame > end
                        || !within_lead(start, rate, self.video_written, video_rate))
                {
                    break;
                }
                let Some(packet) = self.audio_track.queued.pop_front() else {
                    break;
                };
                if start >= end {
                    continue;
                }
                let packet = Packet {
                    track: audio,
                    pts: start,
                    dts: packet.dts - i64::from(priming),
                    duration: (end - start).clamp(1, frame),
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
            let mut stall = Stall::new(self.stall_limit);
            loop {
                let pts = self.audio_pts;
                let Some((encoder, _, _)) = self.audio.as_mut() else {
                    return Ok(());
                };
                match encoder.submit(pts, &frame)? {
                    Submit::Accepted => break,
                    Submit::Busy => self.wait_busy(&mut stall, false, "audio")?,
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
    /// As [`Self::write_frame`]; encoders that give no packet for 10 s while draining, or an
    /// encoder that never published its configuration.
    pub fn finish(mut self) -> Result<(), EncodeError> {
        let tail = self
            .audio
            .as_mut()
            .map(|(_, input, _)| input.finish())
            .unwrap_or_default();
        self.submit_audio(tail)?;
        let finished = self.video.finish_input();
        finished.map_err(|error| self.rejected(error))?;
        if let Some((encoder, _, _)) = self.audio.as_mut() {
            encoder.finish_input()?;
        }
        let mut done = (false, self.audio.is_none());
        let mut stall = Stall::new(self.stall_limit);
        while !(done.0 && done.1) {
            let received = self.received;
            let (video, audio) = self.drain(!done.0, !done.1)?;
            done = (done.0 || video, done.1 || audio);
            if stall.turn(self.received != received) {
                let which = match done {
                    (false, false) => "video and audio encoders",
                    (false, true) => "video encoder",
                    _ => "audio encoder",
                };
                return Err(EncodeError::Adapter(format!(
                    "the {which} gave no packet for {} s while draining",
                    self.stall_limit.as_secs_f64()
                )));
            }
        }
        self.finishing = true;
        self.flush_tracks()?;
        if self.video_track.id.is_none() {
            let which = if self.video_info().is_none() {
                "video"
            } else {
                "audio"
            };
            return Err(EncodeError::Adapter(format!(
                "the {which} encoder published no configuration"
            )));
        }
        let writer = self
            .writer
            .take()
            .ok_or_else(|| EncodeError::Adapter("the output is already finished".into()))?;
        writer.finish().map_err(EncodeError::from)
    }
}

impl EncodeSink {
    /// Encodes frame `index` of the export, its planes in the configured layout.
    ///
    /// # Errors
    /// [`EncodeError::EncoderRejected`] for any failure of the video encoder; a writer or audio
    /// encoder failure as it is; [`EncodeError::Cancelled`] when the token is set while an encoder
    /// is busy; an encoder that takes no frame and gives no packet for 10 s.
    pub fn write_frame(&mut self, index: u64, planes: &[PlaneBuffer]) -> Result<(), EncodeError> {
        let rate = self.video_config.frame_rate;
        let pts = i64::try_from(index).unwrap_or(i64::MAX)
            * i64::from(rate.denominator())
            * i64::from(self.video.timescale())
            / i64::from(rate.numerator().max(1));
        let interval = u64::from(self.video_config.keyframe_interval.max(1));
        let keyframe = index.is_multiple_of(interval);
        let mut stall = Stall::new(self.stall_limit);
        loop {
            let submitted = self.video.submit(pts, planes, keyframe);
            match submitted.map_err(|error| self.rejected(error))? {
                Submit::Accepted => break,
                Submit::Busy => self.wait_busy(&mut stall, true, "video")?,
            }
        }
        self.video_frames += 1;
        self.drain(false, false).map(|_| ())
    }

    /// One turn of a wait for the `which` encoder to take a frame: drains the encoders, waiting on
    /// the video one when `video`.
    fn wait_busy(
        &mut self,
        stall: &mut Stall,
        video: bool,
        which: &str,
    ) -> Result<(), EncodeError> {
        if self.cancel.is_cancelled() {
            return Err(EncodeError::Cancelled);
        }
        let received = self.received;
        self.drain(video, !video)?;
        if stall.turn(self.received != received) {
            return Err(EncodeError::Adapter(format!(
                "the {which} encoder accepted no frame for {} s",
                self.stall_limit.as_secs_f64()
            )));
        }
        Ok(())
    }

    /// A video encoder failure as one another encoder may not have: anything but a cancel.
    fn rejected(&self, error: EncodeError) -> EncodeError {
        match error {
            EncodeError::Cancelled | EncodeError::EncoderRejected { .. } => error,
            error => EncodeError::EncoderRejected {
                encoder: self.video.name().to_owned(),
                frame: self.video_frames,
                message: error.to_string(),
            },
        }
    }
}

#[cfg(feature = "engine")]
impl crate::export::VideoSink for EncodeSink {
    fn write_video(
        &mut self,
        index: u64,
        _time: alpha2_time::MediaTime,
        _layout: PixelLayout,
        planes: &[PlaneBuffer],
    ) -> Result<(), EncodeError> {
        self.write_frame(index, planes)
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
