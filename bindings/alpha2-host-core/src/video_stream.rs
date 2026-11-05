//! Frame-exact video decoding over a container reader and a platform decoder session.
//!
//! [`VideoStream`] owns the policy every native backend shares: a seek answers with the last frame
//! at or before the time plus the stream's presentation slack, found by decoding from the keyframe
//! before it (stepping further back when every frame from that keyframe comes later); sequential
//! reads keep one frame of lookahead; frames a session emits in decode order are put back in
//! presentation order; a failing hardware session is replaced by a software one once, and a
//! session the system takes away is reopened where the stream was.

use std::sync::Arc;

use alpha2_container::{ContainerReader, Packet, TrackInfo, VideoCodec};
use alpha2_media::{
    CancelToken, ChromaLocation, ColorMetadata, DecodeError, DecodeEvent, DecodeRequest,
    DecodeResult, DecodedVideoFrame, Decoder, Orientation, PixelFormat, Plane, Rect,
    SampleAspectRatio, StreamInfo, StreamKind, YuvFormat,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

use crate::parameter_sets::{CropWindow, ParameterSetCodec, crop_window};
use crate::recovery::{DecodeFailure, DecodePolicy, Recovery, WorkBudget};

/// Whether a decoder session runs on dedicated hardware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendKind {
    /// A hardware decoder.
    Hardware,
    /// A software decoder.
    Software,
}

/// Why a decoder session failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendError {
    /// The system took the session away (app backgrounded, codec reclaimed, device lost); a new
    /// session can continue.
    SessionLost(String),
    /// The session cannot decode this stream.
    Unsupported(String),
    /// The packet just submitted is damaged.
    Corrupt(String),
    /// A hardware session could not decode a picture.
    HardwarePicture(String),
    /// Anything else.
    Failed(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionLost(reason) => write!(formatter, "decoder session lost: {reason}"),
            Self::Unsupported(reason)
            | Self::Corrupt(reason)
            | Self::HardwarePicture(reason)
            | Self::Failed(reason) => write!(formatter, "{reason}"),
        }
    }
}

impl From<BackendError> for DecodeError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::Unsupported(reason) => Self::Unsupported(reason),
            error => Self::Adapter(error.to_string()),
        }
    }
}

/// What a session did with a packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Submit {
    /// The packet is queued.
    Accepted,
    /// The session's input is full; receive pictures, then submit the same packet again.
    Busy,
}

/// A decoded picture that stays on the platform's surface until the lease drops.
pub trait SurfaceLease: std::fmt::Debug + Send + Sync + 'static {
    /// Bytes the surface holds, charged to the frame budget.
    fn retained_bytes(&self) -> u64;

    /// Lets the platform importer recover its own lease type.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A decoded picture's pixels.
#[derive(Clone, Debug)]
pub enum BackendImage {
    /// Planes in memory.
    Cpu {
        /// Their layout.
        format: YuvFormat,
        /// Luma, then chroma (one interleaved or two planar), then alpha.
        planes: Vec<Plane>,
    },
    /// A platform surface the renderer imports.
    Surface(Arc<dyn SurfaceLease>),
}

/// One picture a session produced, with what the session knows about it.
#[derive(Clone, Debug)]
pub struct BackendFrame {
    /// The presentation time of the packet it came from, in the track's timescale.
    pub pts: i64,
    /// Its pixels.
    pub image: BackendImage,
    /// The width and height of the image's buffer.
    pub coded: (u32, u32),
    /// The visible area, when the session applied or reported the crop.
    pub visible: Option<Rect>,
    /// The pixel aspect ratio the bitstream signals, when the session reports it.
    pub sample_aspect_ratio: Option<SampleAspectRatio>,
    /// The color the bitstream signals, when the session reports it.
    pub color: Option<ColorMetadata>,
    /// The chroma siting the bitstream signals; `None` when it signals none.
    pub chroma_location: Option<ChromaLocation>,
}

/// What a session had on a receive.
#[derive(Debug)]
pub enum Poll {
    /// A picture.
    Frame(BackendFrame),
    /// Nothing yet; submit more input.
    Pending,
    /// Input ended and every picture was given.
    Drained,
}

/// One decoder session for one video track, used from one thread at a time.
pub trait VideoDecodeBackend: Send {
    /// Whether it runs on hardware.
    fn kind(&self) -> BackendKind;

    /// Queues a packet for decoding.
    ///
    /// # Errors
    /// A lost session, a damaged packet, or a failed decoder.
    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError>;

    /// Takes a decoded picture, in any order. With `wait`, blocks briefly for one: the session
    /// refused input, or input ended.
    ///
    /// # Errors
    /// A lost session, an undecodable picture, or a failed decoder.
    fn receive(&mut self, wait: bool) -> Result<Poll, BackendError>;

    /// No more packets follow until a [`Self::flush`]; receives then drain the session.
    ///
    /// # Errors
    /// A lost session or a failed decoder.
    fn finish_input(&mut self) -> Result<(), BackendError>;

    /// Drops every queued packet and picture, ready for packets from a keyframe.
    ///
    /// # Errors
    /// A lost session or a failed decoder.
    fn flush(&mut self) -> Result<(), BackendError>;
}

/// Opens decoder sessions for tracks.
pub trait VideoBackends: Send + Sync {
    /// A session of `kind` for `track`.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when no decoder of that kind reads the track.
    fn open(
        &self,
        track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError>;
}

/// Which sessions a stream may use.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DecodePreference {
    /// Hardware, then software once when hardware fails.
    #[default]
    Auto,
    /// Hardware only.
    Hardware,
    /// Software only.
    Software,
}

/// How a [`VideoStream`] is opened.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamOptions {
    /// Which sessions it may use.
    pub preference: DecodePreference,
    /// How damaged input is handled.
    pub policy: DecodePolicy,
    /// The color the project declares for the source, which every frame then carries.
    pub declared_color: Option<ColorMetadata>,
}

/// The facts every frame carries besides its pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameFacts {
    /// Presentation time.
    pub pts: MediaTime,
    /// The visible area in buffer pixels.
    pub visible_rect: Rect,
    /// How the picture is shown.
    pub orientation: Orientation,
    /// Pixel aspect ratio.
    pub sample_aspect_ratio: SampleAspectRatio,
    /// Chroma siting.
    pub chroma_location: ChromaLocation,
    /// Color, when declared or tagged.
    pub color: Option<ColorMetadata>,
}

/// A decoded frame of a [`VideoStream`].
#[derive(Clone, Debug)]
pub struct StreamFrame {
    /// Its facts.
    pub facts: FrameFacts,
    /// Its pixels.
    pub image: BackendImage,
}

impl StreamFrame {
    /// The frame as a CPU frame; `None` for a surface.
    #[must_use]
    pub fn to_cpu(&self) -> Option<DecodedVideoFrame> {
        let BackendImage::Cpu { format, planes } = &self.image else {
            return None;
        };
        Some(DecodedVideoFrame {
            pts: self.facts.pts,
            dts: None,
            duration: None,
            visible_rect: self.facts.visible_rect,
            sample_aspect_ratio: self.facts.sample_aspect_ratio,
            orientation: self.facts.orientation,
            pixel_format: PixelFormat::Yuv(*format),
            chroma_location: self.facts.chroma_location,
            color: self.facts.color,
            planes: planes.clone(),
        })
    }
}

/// Puts pictures a session emits in decode order back in presentation order.
///
/// Pictures are held, sorted, until one arrives that is no earlier than every picture held; the
/// held ones are then released. A session that already emits presentation order costs one picture
/// of latency.
#[derive(Debug, Default)]
pub struct ReorderBuffer {
    arriving: Vec<BackendFrame>,
    ready: std::collections::VecDeque<BackendFrame>,
}

impl ReorderBuffer {
    /// Takes a picture from the session.
    pub fn arrive(&mut self, frame: BackendFrame) {
        if self
            .arriving
            .last()
            .is_some_and(|latest| frame.pts >= latest.pts)
        {
            self.ready.extend(self.arriving.drain(..));
        }
        let at = self
            .arriving
            .iter()
            .position(|held| held.pts > frame.pts)
            .unwrap_or(self.arriving.len());
        self.arriving.insert(at, frame);
    }

    /// The session drained: every held picture is released.
    pub fn drain(&mut self) {
        self.ready.extend(self.arriving.drain(..));
    }

    /// The next picture in presentation order, once it is certain.
    pub fn take(&mut self) -> Option<BackendFrame> {
        self.ready.pop_front()
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.arriving.clear();
        self.ready.clear();
    }

    /// Pictures held now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.arriving.len() + self.ready.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Seconds a seek steps back, beyond the keyframe before its time, when every frame decoded from
/// that keyframe comes later (open GOPs whose leading frames precede their keyframe).
const SEEK_BACK_OFF_SECONDS: [i64; 4] = [0, 1, 2, 4];

/// Sessions the system may take from one stream before its decode fails.
const MAX_SESSION_REOPENS: u32 = 3;

/// Where a session's input stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Input {
    /// Packets go in.
    Open,
    /// The session refused the pending packet until it outputs.
    Full,
    /// Every packet is in; the session drains.
    Ended,
    /// The session gave its last picture.
    Drained,
}

/// Why decoding stopped: a session to replace, or a failure for the caller.
enum Interrupt {
    Backend(BackendError),
    Decode(DecodeError),
}

impl From<DecodeError> for Interrupt {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<alpha2_container::DemuxError> for Interrupt {
    fn from(error: alpha2_container::DemuxError) -> Self {
        Self::Decode(error.into())
    }
}

/// A video track decoded frame-exactly.
pub struct VideoStream {
    reader: Box<dyn ContainerReader>,
    track: TrackInfo,
    codec: VideoCodec,
    backends: Arc<dyn VideoBackends>,
    backend: Box<dyn VideoDecodeBackend>,
    options: StreamOptions,
    software_fallback_used: bool,
    reopens: u32,
    recovery: Recovery,
    reorder: ReorderBuffer,
    lookahead: Option<StreamFrame>,
    pending: Option<Packet>,
    input: Input,
    /// The last frame handed out, which a replacement session decodes past.
    last_returned: Option<i64>,
    /// Frames at or before this time are dropped: they were handed out by a lost session.
    resume_after: Option<i64>,
    /// The stream's first keyframe, where a seek cannot step back from.
    first_keyframe: Option<i64>,
    slack: MediaTime,
    crop: Option<CropWindow>,
    info: Vec<StreamInfo>,
}

impl std::fmt::Debug for VideoStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoStream")
            .field("track", &self.track.id)
            .field("backend", &self.backend.kind())
            .finish_non_exhaustive()
    }
}

impl VideoStream {
    /// Opens track `index` of `reader` with a session from `backends`.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a track that is not video or that no allowed session
    /// reads.
    pub fn open(
        mut reader: Box<dyn ContainerReader>,
        index: usize,
        backends: Arc<dyn VideoBackends>,
        options: StreamOptions,
    ) -> Result<Self, DecodeError> {
        let track =
            reader.tracks().get(index).cloned().ok_or_else(|| {
                DecodeError::Unsupported(format!("the file has no track {index}"))
            })?;
        let Some(video) = track.video.clone() else {
            return Err(DecodeError::Unsupported(format!(
                "track {} is not video",
                track.id.0
            )));
        };
        let crop = match video.codec {
            VideoCodec::H264 => crop_window(ParameterSetCodec::H264, &video.codec_private),
            VideoCodec::Hevc => crop_window(ParameterSetCodec::Hevc, &video.codec_private),
            _ => None,
        };
        let first = match options.preference {
            DecodePreference::Software => BackendKind::Software,
            DecodePreference::Auto | DecodePreference::Hardware => BackendKind::Hardware,
        };
        let (backend, software_fallback_used) = match backends.open(&track, first) {
            Ok(backend) => (backend, false),
            Err(hardware) if options.preference == DecodePreference::Auto => {
                let software = backends.open(&track, BackendKind::Software).map_err(|software| {
                    DecodeError::Unsupported(format!(
                        "no decoder here reads this stream (hardware: {hardware}; software: {software})"
                    ))
                })?;
                (software, true)
            }
            Err(error) => return Err(error.into()),
        };
        let first_keyframe = reader.seek(track.id, i64::MIN)?;
        let timescale = i64::from(track.timescale.max(1));
        let mut recovery = Recovery::new((1, i32::try_from(timescale).unwrap_or(i32::MAX)));
        recovery.policy = options.policy;
        let dimensions = crop
            .and_then(|crop| crop.visible())
            .map_or((video.width, video.height), |rect| {
                (rect.width, rect.height)
            });
        let info = vec![StreamInfo {
            index,
            kind: StreamKind::Video,
            codec: codec_name(video.codec),
            duration: track
                .duration
                .map(|duration| to_media_time(duration, track.timescale)),
            dimensions: Some(dimensions),
            frame_rate: video.frame_rate,
            sample_rate: None,
            channels: None,
            orientation: video.orientation,
            sample_aspect_ratio: video
                .sample_aspect_ratio
                .unwrap_or(SampleAspectRatio { num: 1, den: 1 }),
            color: options.declared_color.or(video.color),
            frame_bytes: None,
            alpha: None,
            language: track.language.clone(),
            title: track.title.clone(),
        }];
        Ok(Self {
            reader,
            codec: video.codec,
            backends,
            backend,
            options,
            software_fallback_used,
            reopens: 0,
            recovery,
            reorder: ReorderBuffer::default(),
            lookahead: None,
            pending: None,
            input: Input::Open,
            last_returned: None,
            resume_after: None,
            first_keyframe,
            slack: alpha2_time::presentation_slack(1, timescale, -1),
            crop,
            info,
            track,
        })
    }

    /// The track decoded.
    #[must_use]
    pub const fn track(&self) -> &TrackInfo {
        &self.track
    }

    /// How far after a time a frame may be stored and still be the one shown at it.
    #[must_use]
    pub const fn presentation_slack(&self) -> MediaTime {
        self.slack
    }

    /// Whether the session open now runs on hardware.
    #[must_use]
    pub fn on_hardware(&self) -> bool {
        self.backend.kind() == BackendKind::Hardware
    }

    /// Damage recorded so far.
    #[must_use]
    pub const fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// The next frame in presentation order; `None` at the end.
    ///
    /// # Errors
    /// Cancellation, a spent work budget, damage beyond the policy, or a decoder failure no
    /// replacement session survives.
    pub fn next(&mut self, cancel: &CancelToken) -> Result<Option<StreamFrame>, DecodeError> {
        let frame = match self.lookahead.take() {
            Some(frame) => Some(frame),
            None => self.decode_next(cancel)?,
        };
        if let Some(frame) = &frame {
            self.last_returned = Some(self.track_units(frame.facts.pts));
        }
        Ok(frame)
    }

    /// When the frame [`Self::next`] returns is presented; `None` at the end. The frame is decoded
    /// now and kept.
    ///
    /// # Errors
    /// As [`Self::next`].
    pub fn peek_pts(&mut self, cancel: &CancelToken) -> Result<Option<MediaTime>, DecodeError> {
        if self.lookahead.is_none() {
            self.lookahead = self.decode_next(cancel)?;
        }
        Ok(self.lookahead.as_ref().map(|frame| frame.facts.pts))
    }

    /// The frame shown at `time`: the last at or before `time` plus the slack, the first for an
    /// earlier time and the last for a later one. The frame after it becomes the lookahead.
    ///
    /// # Errors
    /// [`DecodeError::OutOfRange`] for a stream without frames; otherwise as [`Self::next`].
    pub fn seek(
        &mut self,
        time: MediaTime,
        cancel: &CancelToken,
    ) -> Result<StreamFrame, DecodeError> {
        loop {
            match self.seek_once(time, cancel) {
                Ok(frame) => {
                    self.last_returned = Some(self.track_units(frame.facts.pts));
                    return Ok(frame);
                }
                Err(Interrupt::Decode(error)) => return Err(error),
                Err(Interrupt::Backend(error)) => self.replace_session(error)?,
            }
        }
    }

    fn seek_once(
        &mut self,
        time: MediaTime,
        cancel: &CancelToken,
    ) -> Result<StreamFrame, Interrupt> {
        let deadline = time.presentation_deadline(self.slack);
        let target = self.track_units_floor(deadline);
        let timescale = i64::from(self.track.timescale.max(1));
        for (attempt, seconds) in SEEK_BACK_OFF_SECONDS.into_iter().enumerate() {
            let keyframe = self
                .reader
                .seek(self.track.id, target.saturating_sub(seconds * timescale))?;
            self.reset(None)?;
            let Some(first) = self.decode_raw(cancel)? else {
                return Err(DecodeError::OutOfRange.into());
            };
            let at_start = keyframe.is_none() || keyframe <= self.first_keyframe;
            let more = attempt + 1 < SEEK_BACK_OFF_SECONDS.len();
            if first.facts.pts > deadline && !at_start && more {
                continue;
            }
            let mut shown = first;
            loop {
                match self.decode_raw(cancel)? {
                    Some(frame) if frame.facts.pts <= deadline => shown = frame,
                    Some(frame) => {
                        self.lookahead = Some(frame);
                        break;
                    }
                    None => break,
                }
            }
            return Ok(shown);
        }
        Err(DecodeError::OutOfRange.into())
    }

    fn decode_next(&mut self, cancel: &CancelToken) -> Result<Option<StreamFrame>, DecodeError> {
        loop {
            match self.decode_raw(cancel) {
                Ok(frame) => return Ok(frame),
                Err(Interrupt::Decode(error)) => return Err(error),
                Err(Interrupt::Backend(error)) => {
                    self.replace_session(error)?;
                    self.resume().map_err(|interrupt| match interrupt {
                        Interrupt::Decode(error) => error,
                        Interrupt::Backend(error) => error.into(),
                    })?;
                }
            }
        }
    }

    /// Decodes until the next frame in presentation order is certain.
    fn decode_raw(&mut self, cancel: &CancelToken) -> Result<Option<StreamFrame>, Interrupt> {
        let mut budget = WorkBudget::new();
        loop {
            if let Some(picture) = self.reorder.take() {
                let frame = self.facts(picture);
                let pts = self.track_units(frame.facts.pts);
                if self.resume_after.is_some_and(|after| pts <= after) {
                    continue;
                }
                self.resume_after = None;
                budget.progressed();
                self.recovery.progress(Some(pts))?;
                return Ok(Some(frame));
            }
            if self.input == Input::Drained {
                if self.recovery.diagnostics.corrupt_packets.is_empty() {
                    return Ok(None);
                }
                self.recovery.video_eof()?;
                return Ok(None);
            }
            budget.step(cancel)?;
            self.step()?;
        }
    }

    /// One receive, or one packet in.
    fn step(&mut self) -> Result<(), Interrupt> {
        match self.backend.receive(self.input != Input::Open) {
            Ok(Poll::Frame(picture)) => {
                if self.input == Input::Full {
                    self.input = Input::Open;
                }
                self.reorder.arrive(picture);
                return Ok(());
            }
            Ok(Poll::Drained) => {
                self.reorder.drain();
                self.input = Input::Drained;
                return Ok(());
            }
            Ok(Poll::Pending) => {}
            Err(BackendError::HardwarePicture(reason))
                if self
                    .recovery
                    .counts_as_damage(DecodeFailure::HardwarePicture) =>
            {
                let _ = reason;
                self.recovery.corrupt("video receive", None)?;
                return Ok(());
            }
            Err(error) => return Err(Interrupt::Backend(error)),
        }
        if matches!(self.input, Input::Ended | Input::Drained) {
            return Ok(());
        }
        if self.pending.is_none() {
            if let Some(packet) = self.reader.next_packet(self.track.id)? {
                self.pending = Some(packet);
            } else {
                self.backend.finish_input().map_err(Interrupt::Backend)?;
                self.input = Input::Ended;
                return Ok(());
            }
        }
        let Some(packet) = self.pending.as_ref() else {
            return Ok(());
        };
        match self.backend.submit(packet) {
            Ok(Submit::Accepted) => {
                self.pending = None;
                self.input = Input::Open;
            }
            Ok(Submit::Busy) => self.input = Input::Full,
            Err(BackendError::Corrupt(_))
                if self.recovery.counts_as_damage(DecodeFailure::InvalidData) =>
            {
                let pts = packet.pts;
                self.pending = None;
                self.recovery.corrupt("video send", Some(pts))?;
            }
            Err(error) => return Err(Interrupt::Backend(error)),
        }
        Ok(())
    }

    /// Empties the session and the queues, keeping `lookahead`.
    fn reset(&mut self, lookahead: Option<StreamFrame>) -> Result<(), Interrupt> {
        self.reorder.clear();
        self.lookahead = lookahead;
        self.pending = None;
        self.input = Input::Open;
        self.recovery.reset_progress();
        self.backend.flush().map_err(Interrupt::Backend)
    }

    /// Opens the session that continues after `error`, or fails with it.
    fn replace_session(&mut self, error: BackendError) -> Result<(), DecodeError> {
        let kind = match &error {
            BackendError::SessionLost(_) if self.reopens < MAX_SESSION_REOPENS => {
                self.reopens += 1;
                self.backend.kind()
            }
            BackendError::Unsupported(_)
            | BackendError::Failed(_)
            | BackendError::Corrupt(_)
            | BackendError::HardwarePicture(_)
                if self.backend.kind() == BackendKind::Hardware
                    && self.options.preference == DecodePreference::Auto
                    && !self.software_fallback_used =>
            {
                self.software_fallback_used = true;
                BackendKind::Software
            }
            _ => return Err(error.into()),
        };
        self.backend = self.backends.open(&self.track, kind).map_err(|reopen| {
            DecodeError::Adapter(format!("{error}; no session replaces it: {reopen}"))
        })?;
        Ok(())
    }

    /// Positions a new session after the last frame handed out.
    fn resume(&mut self) -> Result<(), Interrupt> {
        let lookahead = self.lookahead.take();
        let after = lookahead
            .as_ref()
            .map(|frame| self.track_units(frame.facts.pts))
            .or(self.last_returned);
        self.reader.seek(self.track.id, after.unwrap_or(i64::MIN))?;
        self.reset(lookahead)?;
        self.resume_after = after;
        Ok(())
    }

    fn facts(&self, picture: BackendFrame) -> StreamFrame {
        let video = self.track.video.as_ref();
        let visible = picture
            .visible
            .or_else(|| self.crop.and_then(|crop| crop.visible()))
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                width: picture.coded.0,
                height: picture.coded.1,
            });
        // H.264 without VUI infers chroma location type 0 (left).
        let chroma_location = picture.chroma_location.unwrap_or(match self.codec {
            VideoCodec::H264 => ChromaLocation::Left,
            _ => ChromaLocation::Unspecified,
        });
        StreamFrame {
            facts: FrameFacts {
                pts: to_media_time(picture.pts, self.track.timescale),
                visible_rect: visible,
                orientation: video.map(|video| video.orientation).unwrap_or_default(),
                sample_aspect_ratio: video
                    .and_then(|video| video.sample_aspect_ratio)
                    .or(picture.sample_aspect_ratio)
                    .unwrap_or(SampleAspectRatio { num: 1, den: 1 }),
                chroma_location,
                color: self
                    .options
                    .declared_color
                    .or_else(|| video.and_then(|video| video.color))
                    .or(picture.color),
            },
            image: picture.image,
        }
    }

    /// `time` in the track's units, rounded to nearest.
    fn track_units(&self, time: MediaTime) -> i64 {
        rescale(
            time.ticks(),
            i64::from(self.track.timescale.max(1)),
            TICKS_PER_SECOND,
            false,
        )
    }

    /// `time` in the track's units, rounded down.
    fn track_units_floor(&self, time: MediaTime) -> i64 {
        rescale(
            time.ticks(),
            i64::from(self.track.timescale.max(1)),
            TICKS_PER_SECOND,
            true,
        )
    }
}

/// `units` in a track of `timescale` units per second, as ticks rounded to nearest (half away
/// from zero, as `FFmpeg`'s `av_rescale_q` rounds).
#[must_use]
pub fn to_media_time(units: i64, timescale: u32) -> MediaTime {
    MediaTime::from_ticks(rescale(
        units,
        TICKS_PER_SECOND,
        i64::from(timescale.max(1)),
        false,
    ))
}

fn rescale(value: i64, numerator: i64, denominator: i64, floor: bool) -> i64 {
    let product = i128::from(value) * i128::from(numerator);
    let denominator = i128::from(denominator);
    let scaled = if floor {
        product.div_euclid(denominator)
    } else {
        let half = denominator / 2;
        if product >= 0 {
            (product + half) / denominator
        } else {
            (product - half) / denominator
        }
    };
    i64::try_from(scaled).unwrap_or(if scaled < 0 { i64::MIN } else { i64::MAX })
}

fn codec_name(codec: VideoCodec) -> String {
    match codec {
        VideoCodec::H264 => "h264".into(),
        VideoCodec::Hevc => "hevc".into(),
        VideoCodec::Vp8 => "vp8".into(),
        VideoCodec::Vp9 => "vp9".into(),
        VideoCodec::Av1 => "av1".into(),
        VideoCodec::Mpeg4Part2 => "mpeg4".into(),
        VideoCodec::ProRes(_) => "prores".into(),
        VideoCodec::Other(fourcc) => fourcc.to_string(),
    }
}

/// The CPU-frame decoder contract, for consumers and conformance tests that take
/// [`alpha2_media::Decoder`]. A surface frame is refused as unsupported here.
impl Decoder for VideoStream {
    fn streams(&self) -> &[StreamInfo] {
        &self.info
    }

    fn flush(&mut self) -> Result<(), DecodeError> {
        let after = self.last_returned;
        self.reader.seek(self.track.id, after.unwrap_or(i64::MIN))?;
        match self.reset(None) {
            Ok(()) => {}
            Err(Interrupt::Decode(error)) => return Err(error),
            Err(Interrupt::Backend(error)) => self.replace_session(error)?,
        }
        self.resume_after = after;
        Ok(())
    }

    fn seek(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<DecodeResult, DecodeError> {
        let frame = VideoStream::seek(self, request.time, cancel)?;
        Ok(DecodeResult {
            request,
            event: DecodeEvent::Video(cpu_frame(&frame)?),
        })
    }

    fn decode_next(&mut self, cancel: &CancelToken) -> Result<DecodeEvent, DecodeError> {
        Ok(match self.next(cancel)? {
            Some(frame) => DecodeEvent::Video(cpu_frame(&frame)?),
            None => DecodeEvent::EndOfStream,
        })
    }
}

fn cpu_frame(frame: &StreamFrame) -> Result<DecodedVideoFrame, DecodeError> {
    frame.to_cpu().ok_or_else(|| {
        DecodeError::Unsupported(
            "the decoder produced a surface where CPU planes were asked".into(),
        )
    })
}

#[cfg(test)]
#[path = "video_stream_tests.rs"]
mod tests;
