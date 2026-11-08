//! Supplying an extraction session from decoders: one source reader over any
//! [`alpha2_media::Decoder`], CPU presentation of its frames, and [`run`], which drives an
//! extractor and records where its data came from.

use std::sync::Arc;

use alpha2_engine::extract::{
    AudioChannels, Direction, DriveError, ExtractRequest, ExtractSession, Extractor, FrameFrom,
    Need, RequestError, SuppliedAudio, SuppliedFrame, Supplier, mono_downmix,
};
pub use alpha2_engine::extract::{displayed_size, present_for_extraction};
use alpha2_media::{
    AudioBlock, CancelToken, DecodeError, DecodeEvent, DecodeRequest, DecodeRequestKind,
    DecodedVideoFrame, Decoder, SourceId,
};
use alpha2_model::{DataSet, MediaFingerprint, Project};
use alpha2_render::CpuRenderer;
use alpha2_time::MediaTime;

/// Decoded frames kept for backward reads, at most.
pub const REVERSE_WINDOW_BYTES: u64 = 128 * 1024 * 1024;
/// How far before a frame a backward read first looks for earlier frames, in ticks (1 s).
const FIRST_WINDOW_TICKS: i64 = 120_000;

/// Frames of one source in either direction, over a [`Decoder`].
///
/// Forward reads continue the decoder after its last frame. Backward reads decode a window of
/// earlier frames forward once from the keyframe before it and hand them out in reverse, so a
/// long backward run decodes each stretch about once.
pub struct VideoReader<D> {
    decoder: D,
    source: SourceId,
    cancel: CancelToken,
    generation: u64,
    /// Frames before the last backward answer, ascending by pts.
    window: Vec<Arc<DecodedVideoFrame>>,
    /// Whether the decoder sits just after the last forward answer.
    forward_ready: bool,
}

impl<D: Decoder> VideoReader<D> {
    /// A reader of `source` from `decoder`, stopping at `cancel`.
    pub const fn new(decoder: D, source: SourceId, cancel: CancelToken) -> Self {
        Self {
            decoder,
            source,
            cancel,
            generation: 0,
            window: Vec::new(),
            forward_ready: false,
        }
    }

    fn seek(&mut self, time: MediaTime) -> Result<Option<DecodedVideoFrame>, DecodeError> {
        self.generation += 1;
        let request = DecodeRequest {
            source: self.source.clone(),
            time,
            kind: DecodeRequestKind::Seek,
            generation: self.generation,
        };
        match self.decoder.seek(request, &self.cancel) {
            Ok(result) => match result.event {
                DecodeEvent::Video(frame) => Ok(Some(frame)),
                DecodeEvent::EndOfStream | DecodeEvent::Retry => Ok(None),
                DecodeEvent::Audio(_) => Err(DecodeError::Unsupported(
                    "an audio stream where video was asked".to_owned(),
                )),
            },
            Err(DecodeError::OutOfRange) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn next_decoded(&mut self) -> Result<Option<DecodedVideoFrame>, DecodeError> {
        loop {
            match self.decoder.decode_next(&self.cancel)? {
                DecodeEvent::Video(frame) => return Ok(Some(frame)),
                DecodeEvent::Retry => {}
                DecodeEvent::EndOfStream => return Ok(None),
                DecodeEvent::Audio(_) => {
                    return Err(DecodeError::Unsupported(
                        "an audio stream where video was asked".to_owned(),
                    ));
                }
            }
        }
    }

    /// The frame a session's [`Need::SourceFrame`] asks for, or `None` when there is none.
    ///
    /// # Errors
    /// The decoder's failure or cancellation.
    pub fn frame(
        &mut self,
        from: FrameFrom,
        direction: Direction,
    ) -> Result<Option<Arc<DecodedVideoFrame>>, DecodeError> {
        match (from, direction) {
            (FrameFrom::Start(time), _) => {
                self.window.clear();
                let frame = self.seek(time)?;
                self.forward_ready = frame.is_some();
                Ok(frame.map(Arc::new))
            }
            (FrameFrom::After(pts), Direction::Forward) => {
                if !self.forward_ready {
                    // Position after `pts` again: the frame shown at it, then the next.
                    if self.seek(pts)?.is_none() {
                        return Ok(None);
                    }
                    self.forward_ready = true;
                }
                while let Some(frame) = self.next_decoded()? {
                    if frame.pts > pts {
                        return Ok(Some(Arc::new(frame)));
                    }
                }
                self.forward_ready = false;
                Ok(None)
            }
            (FrameFrom::After(pts), Direction::Backward) => {
                self.forward_ready = false;
                if let Some(index) = self.window.iter().rposition(|frame| frame.pts < pts) {
                    self.window.truncate(index + 1);
                    return Ok(self.window.pop());
                }
                self.fill_window(pts)?;
                Ok(self.window.pop())
            }
        }
    }

    /// Decodes the frames before `pts`, as many as [`REVERSE_WINDOW_BYTES`] holds, widening the
    /// look-back until one is found or the stream's start is reached.
    fn fill_window(&mut self, pts: MediaTime) -> Result<(), DecodeError> {
        let mut back = FIRST_WINDOW_TICKS;
        loop {
            let start = MediaTime(pts.ticks().saturating_sub(back));
            let mut frames: Vec<Arc<DecodedVideoFrame>> = Vec::new();
            let mut bytes = 0_u64;
            let mut current = self.seek(start)?;
            while let Some(frame) = current {
                if frame.pts >= pts {
                    break;
                }
                bytes += frame
                    .planes
                    .iter()
                    .map(|plane| plane.bytes.len() as u64)
                    .sum::<u64>();
                frames.push(Arc::new(frame));
                // Keep the latest frames that fit; earlier ones are decoded again later.
                while bytes > REVERSE_WINDOW_BYTES && frames.len() > 1 {
                    let dropped = frames.remove(0);
                    bytes -= dropped
                        .planes
                        .iter()
                        .map(|plane| plane.bytes.len() as u64)
                        .sum::<u64>();
                }
                current = self.next_decoded()?;
            }
            if !frames.is_empty() || start.ticks() <= 0 {
                self.window = frames;
                return Ok(());
            }
            back = back.saturating_mul(2);
        }
    }
}

/// Audio of one source at one rate, over a [`Decoder`] delivering that rate.
pub struct AudioReader<D> {
    decoder: D,
    source: SourceId,
    cancel: CancelToken,
    generation: u64,
    rate: u32,
    /// Decoded samples from `buffer_start`, one vector a channel, with their speakers.
    buffer: Option<AudioBlock>,
    ended: bool,
}

impl<D: Decoder> AudioReader<D> {
    /// A reader of `source` from `decoder`, which delivers `rate` samples a second.
    pub const fn new(decoder: D, source: SourceId, rate: u32, cancel: CancelToken) -> Self {
        Self {
            decoder,
            source,
            cancel,
            generation: 0,
            rate,
            buffer: None,
            ended: false,
        }
    }

    fn reset_at(&mut self, sample: i64) -> Result<(), DecodeError> {
        self.generation += 1;
        let time = MediaTime(alpha2_media::sample_ticks_floor(sample, self.rate));
        let request = DecodeRequest {
            source: self.source.clone(),
            time,
            kind: DecodeRequestKind::Seek,
            generation: self.generation,
        };
        self.ended = false;
        self.buffer = match self.decoder.seek(request, &self.cancel) {
            Ok(result) => {
                if let DecodeEvent::Audio(block) = result.event {
                    Some(block)
                } else {
                    self.ended = true;
                    None
                }
            }
            Err(DecodeError::OutOfRange) => {
                self.ended = true;
                None
            }
            Err(error) => return Err(error),
        };
        Ok(())
    }

    fn extend(&mut self) -> Result<bool, DecodeError> {
        if self.ended {
            return Ok(false);
        }
        loop {
            match self.decoder.decode_next(&self.cancel)? {
                DecodeEvent::Audio(block) => {
                    match &mut self.buffer {
                        Some(buffer)
                            if buffer.end_sample() == block.start_sample
                                && buffer.channels.len() == block.channels.len() =>
                        {
                            for (held, more) in buffer.channels.iter_mut().zip(block.channels) {
                                held.extend(more);
                            }
                        }
                        _ => self.buffer = Some(block),
                    }
                    return Ok(true);
                }
                DecodeEvent::Retry => {}
                DecodeEvent::EndOfStream => {
                    self.ended = true;
                    return Ok(false);
                }
                DecodeEvent::Video(_) => {
                    return Err(DecodeError::Unsupported(
                        "a video stream where audio was asked".to_owned(),
                    ));
                }
            }
        }
    }

    /// Exactly `frames` frames from `start_sample`, in `channels`, silence outside the stream.
    ///
    /// # Errors
    /// The decoder's failure or cancellation.
    pub fn block(
        &mut self,
        start_sample: i64,
        frames: u32,
        channels: AudioChannels,
    ) -> Result<SuppliedAudio, DecodeError> {
        let end = start_sample + i64::from(frames);
        let covers_start = self.buffer.as_ref().is_some_and(|buffer| {
            buffer.start_sample <= start_sample && start_sample <= buffer.end_sample()
        });
        if !covers_start {
            self.reset_at(start_sample)?;
        }
        while self
            .buffer
            .as_ref()
            .is_some_and(|buffer| buffer.end_sample() < end)
        {
            if !self.extend()? {
                break;
            }
        }
        let length = usize::try_from(frames).unwrap_or(0);
        let Some(buffer) = &self.buffer else {
            return Ok(SuppliedAudio {
                channels: 1,
                samples: vec![0.0; length],
            });
        };
        let planes = buffer
            .channels
            .iter()
            .map(|plane| {
                (0..length)
                    .map(|offset| {
                        let index =
                            start_sample + i64::try_from(offset).unwrap_or(0) - buffer.start_sample;
                        usize::try_from(index)
                            .ok()
                            .and_then(|index| plane.get(index))
                            .copied()
                            .unwrap_or(0.0)
                    })
                    .collect::<Vec<f32>>()
            })
            .collect::<Vec<_>>();
        // Drop what no later forward block can need, keeping memory bounded.
        if let Some(buffer) = &mut self.buffer {
            let keep_from = usize::try_from(start_sample - buffer.start_sample)
                .unwrap_or(0)
                .min(buffer.frames());
            if keep_from > 0 {
                for plane in &mut buffer.channels {
                    plane.drain(..keep_from);
                }
                buffer.start_sample += i64::try_from(keep_from).unwrap_or(0);
            }
        }
        Ok(match channels {
            AudioChannels::Mono => {
                let speakers = self
                    .buffer
                    .as_ref()
                    .map(|buffer| buffer.speakers.speakers().to_vec())
                    .unwrap_or_default();
                let mono = mono_downmix(&speakers, &planes).unwrap_or_else(|| vec![0.0; length]);
                SuppliedAudio {
                    channels: 1,
                    samples: mono,
                }
            }
            AudioChannels::Source => SuppliedAudio {
                channels: u32::try_from(planes.len().max(1)).unwrap_or(1),
                samples: if planes.is_empty() {
                    vec![0.0; length]
                } else {
                    planes.concat()
                },
            },
        })
    }
}

/// Supplies a session's source needs from a video and an audio reader; timeline needs come from
/// the host's own renderer and are refused here.
pub struct ReaderSupplier<'a, V, A> {
    renderer: &'a CpuRenderer,
    video: Option<VideoReader<V>>,
    audio: Option<AudioReader<A>>,
}

impl<'a, V: Decoder, A: Decoder> ReaderSupplier<'a, V, A> {
    /// A supplier presenting frames on `renderer`.
    pub const fn new(
        renderer: &'a CpuRenderer,
        video: Option<VideoReader<V>>,
        audio: Option<AudioReader<A>>,
    ) -> Self {
        Self {
            renderer,
            video,
            audio,
        }
    }
}

impl<V: Decoder, A: Decoder> Supplier for ReaderSupplier<'_, V, A> {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        let Need::SourceFrame {
            from,
            direction,
            size,
            ..
        } = need
        else {
            return Err("timeline frames come from the host's renderer".to_owned());
        };
        let reader = self
            .video
            .as_mut()
            .ok_or_else(|| "no video reader".to_owned())?;
        reader
            .frame(*from, *direction)
            .map_err(|error| error.to_string())?
            .map(|frame| present_for_extraction(self.renderer, frame, *size))
            .transpose()
    }

    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String> {
        let Need::SourceAudio {
            start_sample,
            frames,
            channels,
            ..
        } = need
        else {
            return Err("timeline audio comes from the host's mixer".to_owned());
        };
        let reader = self
            .audio
            .as_mut()
            .ok_or_else(|| "no audio reader".to_owned())?;
        reader
            .block(*start_sample, *frames, *channels)
            .map_err(|error| error.to_string())
    }
}

/// Why [`run`] failed.
#[derive(Debug)]
pub enum RunError {
    /// The request did not fit the descriptor or project.
    Request(RequestError),
    /// The run failed.
    Drive(DriveError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) => error.fmt(formatter),
            Self::Drive(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs `extractor` over `request` with inputs from `supplier`, and sets each source-anchored
/// result's `made_from` to `original`, the fingerprint of the original source (also when the
/// frames came from a proxy).
///
/// # Errors
/// [`RunError`] for a refused request or a failed run.
pub fn run(
    project: &Project,
    request: ExtractRequest,
    extractor: &mut dyn Extractor,
    supplier: &mut dyn Supplier,
    original: Option<&MediaFingerprint>,
) -> Result<Vec<DataSet>, RunError> {
    let descriptor = extractor.describe().clone();
    let session = ExtractSession::new(project, &descriptor, request).map_err(RunError::Request)?;
    let mut sets =
        alpha2_engine::extract::drive(session, extractor, supplier).map_err(RunError::Drive)?;
    for set in &mut sets {
        if set.anchor.source().is_some() {
            set.made_from = original.cloned();
        }
    }
    Ok(sets)
}
