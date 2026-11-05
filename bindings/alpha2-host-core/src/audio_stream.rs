//! Audio decoding over a container reader and a decoder session, placed by timestamp at the
//! project rate.
//!
//! [`AudioStream`] applies what `FFmpeg`'s demuxers and decoders do for the native host: each
//! packet's skip-samples (priming, end padding) are dropped, decoded samples sit at their packet's
//! timestamp with gaps left silent and overlaps dropped, and everything is resampled to the
//! project rate. A seek decodes from a pre-roll before its target and hands out samples from the
//! target on.

use std::collections::{HashMap, VecDeque};

use alpha2_container::{AudioCodec, ContainerReader, Packet, PcmFormat, TrackInfo};
use alpha2_media::{
    AudioBlock, CancelToken, DecodeError, DecodeEvent, DecodeRequest, DecodeResult, Decoder,
    Orientation, SampleAspectRatio, SpeakerLayout, StreamInfo, StreamKind,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

use crate::audio_timeline::SourceTimeline;
use crate::recovery::{DecodePolicy, Recovery, WorkBudget};
use crate::video_stream::{BackendError, Submit};

/// Decoded samples from one packet.
#[derive(Clone, Debug, PartialEq)]
pub struct PcmChunk {
    /// The presentation time of the packet it came from, in the track's timescale.
    pub pts: i64,
    /// Samples per second of the decoded audio.
    pub sample_rate: u32,
    /// One plane per channel.
    pub planes: Vec<Vec<f32>>,
    /// The speaker order of the planes, when the decoder reports it.
    pub speakers: Option<SpeakerLayout>,
}

/// What an audio session had on a receive.
#[derive(Debug)]
pub enum AudioPoll {
    /// Decoded samples.
    Chunk(PcmChunk),
    /// Nothing yet; submit more input.
    Pending,
    /// Input ended and every sample was given.
    Drained,
}

/// One decoder session for one audio track, used from one thread at a time.
pub trait AudioDecodeBackend: Send {
    /// Queues a packet for decoding.
    ///
    /// # Errors
    /// A lost session, a damaged packet, or a failed decoder.
    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError>;

    /// Takes decoded samples, in packet order. With `wait`, blocks briefly for them.
    ///
    /// # Errors
    /// A lost session or a failed decoder.
    fn receive(&mut self, wait: bool) -> Result<AudioPoll, BackendError>;

    /// No more packets follow until a [`Self::flush`].
    ///
    /// # Errors
    /// A lost session or a failed decoder.
    fn finish_input(&mut self) -> Result<(), BackendError>;

    /// Drops every queued packet and sample.
    ///
    /// # Errors
    /// A lost session or a failed decoder.
    fn flush(&mut self) -> Result<(), BackendError>;
}

/// Opens audio decoder sessions for tracks.
pub trait AudioBackends: Send + Sync {
    /// A session for `track`.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when no decoder reads the track.
    fn open(&self, track: &TrackInfo) -> Result<Box<dyn AudioDecodeBackend>, BackendError>;
}

/// Decodes PCM tracks itself; other codecs go to `platform`.
pub struct WithPcm<B>(pub B);

impl<B: AudioBackends> AudioBackends for WithPcm<B> {
    fn open(&self, track: &TrackInfo) -> Result<Box<dyn AudioDecodeBackend>, BackendError> {
        match track.audio.as_ref().map(|audio| (audio.codec, audio)) {
            Some((AudioCodec::Pcm(format), audio)) => Ok(Box::new(PcmSession::new(
                format,
                audio.sample_rate,
                audio.channels,
            )?)),
            _ => self.0.open(track),
        }
    }
}

/// No platform decoder: only PCM decodes.
#[derive(Clone, Copy, Debug, Default)]
pub struct PcmOnly;

impl AudioBackends for PcmOnly {
    fn open(&self, track: &TrackInfo) -> Result<Box<dyn AudioDecodeBackend>, BackendError> {
        let codec = track.audio.as_ref().map(|audio| audio.codec);
        Err(BackendError::Unsupported(format!(
            "no decoder here reads {codec:?} audio"
        )))
    }
}

/// Interleaved PCM to planar float.
struct PcmSession {
    format: PcmFormat,
    sample_rate: u32,
    channels: usize,
    out: VecDeque<PcmChunk>,
    ended: bool,
}

impl PcmSession {
    fn new(format: PcmFormat, sample_rate: u32, channels: u32) -> Result<Self, BackendError> {
        let supported = matches!(
            (format.float, format.bits),
            (false, 8 | 16 | 24 | 32) | (true, 32 | 64)
        );
        if !supported || channels == 0 || sample_rate == 0 {
            return Err(BackendError::Unsupported(format!(
                "PCM of {} bits ({}) in {channels} channels at {sample_rate} Hz",
                format.bits,
                if format.float { "float" } else { "integer" }
            )));
        }
        Ok(Self {
            format,
            sample_rate,
            channels: usize::try_from(channels).unwrap_or(usize::MAX),
            out: VecDeque::new(),
            ended: false,
        })
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn sample(&self, bytes: &[u8]) -> f32 {
        let PcmFormat {
            bits,
            float,
            big_endian,
            signed,
        } = self.format;
        let mut word = [0_u8; 8];
        let width = usize::from(bits / 8);
        for (at, byte) in bytes.iter().take(width).enumerate() {
            let index = if big_endian { width - 1 - at } else { at };
            word[index] = *byte;
        }
        let raw = u64::from_le_bytes(word);
        match (float, bits) {
            (true, 32) => f32::from_bits(u32::try_from(raw & 0xffff_ffff).unwrap_or(0)),
            (true, _) => f64::from_bits(raw) as f32,
            (false, 8) if !signed => (raw as f32 - 128.0) / 128.0,
            (false, _) => {
                let shift = 64 - u32::from(bits);
                let value = (raw << shift).cast_signed() >> shift;
                value as f32 / (1_u64 << (bits - 1)) as f32
            }
        }
    }
}

impl AudioDecodeBackend for PcmSession {
    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        let width = usize::from(self.format.bits / 8);
        let frame = width * self.channels;
        let frames = packet.data.len() / frame;
        let mut planes = vec![Vec::with_capacity(frames); self.channels];
        for index in 0..frames {
            for (channel, plane) in planes.iter_mut().enumerate() {
                let at = index * frame + channel * width;
                plane.push(self.sample(&packet.data[at..at + width]));
            }
        }
        self.out.push_back(PcmChunk {
            pts: packet.pts,
            sample_rate: self.sample_rate,
            planes,
            speakers: None,
        });
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<AudioPoll, BackendError> {
        Ok(match self.out.pop_front() {
            Some(chunk) => AudioPoll::Chunk(chunk),
            None if self.ended => AudioPoll::Drained,
            None => AudioPoll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.ended = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.out.clear();
        self.ended = false;
        Ok(())
    }
}

/// A decoder resuming mid-stream lacks history (AAC's first frame misses the previous overlap), so
/// decoding from 100 ms early and discarding up to the target matches a straight decode.
const SEEK_PREROLL: MediaTime = MediaTime::from_ticks(TICKS_PER_SECOND / 10);

/// Opus converges slowly: the target's samples are under 1e-6 off a straight decode from 400 ms.
const OPUS_SEEK_PREROLL: MediaTime = MediaTime::from_ticks(TICKS_PER_SECOND * 2 / 5);

/// Samples per block handed out.
const BLOCK_FRAMES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Input {
    Open,
    Full,
    Ended,
    Drained,
}

/// An audio track decoded at the project rate.
pub struct AudioStream {
    reader: Box<dyn ContainerReader>,
    track: TrackInfo,
    backend: Box<dyn AudioDecodeBackend>,
    project_rate: u32,
    channels: usize,
    speakers: SpeakerLayout,
    timeline: SourceTimeline,
    pending: Vec<VecDeque<f32>>,
    next_sample: Option<i64>,
    input: Input,
    pending_packet: Option<Packet>,
    /// Skip-samples of submitted packets, by presentation time.
    discards: HashMap<i64, (u32, u32)>,
    preroll: MediaTime,
    recovery: Recovery,
    info: Vec<StreamInfo>,
}

impl std::fmt::Debug for AudioStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioStream")
            .field("track", &self.track.id)
            .finish_non_exhaustive()
    }
}

impl AudioStream {
    /// Opens track `index` of `reader`, delivered at `project_rate`.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a track that is not audio or that no session reads;
    /// [`DecodeError::OutOfRange`] for a zero rate.
    pub fn open(
        mut reader: Box<dyn ContainerReader>,
        index: usize,
        backends: &dyn AudioBackends,
        project_rate: u32,
        policy: DecodePolicy,
    ) -> Result<Self, DecodeError> {
        if project_rate == 0 {
            return Err(DecodeError::OutOfRange);
        }
        let track =
            reader.tracks().get(index).cloned().ok_or_else(|| {
                DecodeError::Unsupported(format!("the file has no track {index}"))
            })?;
        let Some(audio) = track.audio.clone() else {
            return Err(DecodeError::Unsupported(format!(
                "track {} is not audio",
                track.id.0
            )));
        };
        let backend = backends.open(&track)?;
        let channels = usize::try_from(audio.channels).unwrap_or(0);
        if channels == 0 || channels > alpha2_media::MAX_AUDIO_CHANNELS {
            return Err(DecodeError::Unsupported(format!(
                "audio with {channels} channels"
            )));
        }
        let speakers = audio
            .speakers
            .or_else(|| SpeakerLayout::default_for(channels))
            .ok_or_else(|| {
                DecodeError::Unsupported(format!("no layout for {channels} channels"))
            })?;
        reader.seek(track.id, i64::MIN)?;
        let mut recovery = Recovery::new((1, i32::try_from(track.timescale).unwrap_or(i32::MAX)));
        recovery.policy = policy;
        let info = vec![StreamInfo {
            index,
            kind: StreamKind::Audio,
            codec: format!("{:?}", audio.codec).to_lowercase(),
            duration: track
                .duration
                .map(|duration| crate::video_stream::to_media_time(duration, track.timescale)),
            dimensions: None,
            frame_rate: None,
            sample_rate: Some(audio.sample_rate),
            channels: Some(audio.channels),
            orientation: Orientation::default(),
            sample_aspect_ratio: SampleAspectRatio { num: 1, den: 1 },
            color: None,
            frame_bytes: None,
            alpha: None,
            language: track.language.clone(),
            title: track.title.clone(),
        }];
        Ok(Self {
            reader,
            backend,
            project_rate,
            channels,
            speakers,
            timeline: SourceTimeline::new(project_rate, channels, None),
            pending: vec![VecDeque::new(); channels],
            next_sample: None,
            input: Input::Open,
            pending_packet: None,
            discards: HashMap::new(),
            preroll: if audio.codec == AudioCodec::Opus {
                OPUS_SEEK_PREROLL
            } else {
                SEEK_PREROLL
            },
            recovery,
            info,
            track,
        })
    }

    /// The track decoded.
    #[must_use]
    pub const fn track(&self) -> &TrackInfo {
        &self.track
    }

    /// The speaker order of every block.
    #[must_use]
    pub const fn speakers(&self) -> SpeakerLayout {
        self.speakers
    }

    /// Gaps and overlaps recorded so far.
    #[must_use]
    pub const fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// The next block of up to 1024 samples at the project rate; `None` at the end.
    ///
    /// # Errors
    /// Cancellation, a spent work budget, or a decoder failure.
    pub fn next_block(&mut self, cancel: &CancelToken) -> Result<Option<AudioBlock>, DecodeError> {
        self.fill(BLOCK_FRAMES, cancel)?;
        let frames = self.pending[0].len().min(BLOCK_FRAMES);
        if frames == 0 {
            return Ok(None);
        }
        let start_sample = self.next_sample.unwrap_or(0);
        let channels = self
            .pending
            .iter_mut()
            .map(|plane| plane.drain(..frames).collect())
            .collect();
        self.next_sample = Some(start_sample + i64::try_from(frames).unwrap_or(0));
        Ok(Some(AudioBlock {
            start_sample,
            sample_rate: self.project_rate,
            speakers: self.speakers,
            channels,
        }))
    }

    /// The block holding `time`'s sample, starting at that sample, or at the stream's first
    /// sample for an earlier time.
    ///
    /// # Errors
    /// [`DecodeError::OutOfRange`] past the end; otherwise as [`Self::next_block`].
    pub fn seek(
        &mut self,
        time: MediaTime,
        cancel: &CancelToken,
    ) -> Result<AudioBlock, DecodeError> {
        let target =
            i128::from(time.ticks()) * i128::from(self.project_rate) / i128::from(TICKS_PER_SECOND);
        let target = i64::try_from(target).unwrap_or(i64::MAX);
        let from = MediaTime::from_ticks(time.ticks().saturating_sub(self.preroll.ticks()));
        let units = i128::from(from.ticks()) * i128::from(self.track.timescale.max(1))
            / i128::from(TICKS_PER_SECOND);
        self.reader
            .seek(self.track.id, i64::try_from(units).unwrap_or(i64::MIN))?;
        self.restart(Some(target))?;
        self.next_block(cancel)?.ok_or(DecodeError::OutOfRange)
    }

    fn restart(&mut self, first: Option<i64>) -> Result<(), DecodeError> {
        self.backend.flush()?;
        self.timeline = SourceTimeline::new(self.project_rate, self.channels, first);
        self.pending.iter_mut().for_each(VecDeque::clear);
        self.next_sample = None;
        self.input = Input::Open;
        self.pending_packet = None;
        self.discards.clear();
        self.recovery.reset_progress();
        Ok(())
    }

    /// Decodes until `frames` samples are pending or the stream ends.
    fn fill(&mut self, frames: usize, cancel: &CancelToken) -> Result<(), DecodeError> {
        let mut budget = WorkBudget::new();
        loop {
            let held = self.pending[0].len();
            if held >= frames {
                return Ok(());
            }
            budget.step(cancel)?;
            if let Some((start, _)) = self.timeline.produce(&mut self.pending, frames - held) {
                if held == 0 {
                    self.next_sample = Some(start);
                }
                budget.progressed();
                continue;
            }
            if self.input == Input::Drained {
                if self.timeline.finished() {
                    return Ok(());
                }
                self.timeline.finish();
                continue;
            }
            self.step()?;
        }
    }

    fn step(&mut self) -> Result<(), DecodeError> {
        match self.backend.receive(self.input != Input::Open)? {
            AudioPoll::Chunk(chunk) => {
                if self.input == Input::Full {
                    self.input = Input::Open;
                }
                return self.place(chunk);
            }
            AudioPoll::Drained => {
                self.input = Input::Drained;
                return Ok(());
            }
            AudioPoll::Pending => {}
        }
        if matches!(self.input, Input::Ended | Input::Drained) {
            return Ok(());
        }
        if self.pending_packet.is_none() {
            if let Some(packet) = self.reader.next_packet(self.track.id)? {
                self.pending_packet = Some(packet);
            } else {
                self.backend.finish_input()?;
                self.input = Input::Ended;
                return Ok(());
            }
        }
        let Some(packet) = self.pending_packet.as_ref() else {
            return Ok(());
        };
        match self.backend.submit(packet)? {
            Submit::Accepted => {
                if packet.discard != (0, 0) {
                    self.discards.insert(packet.pts, packet.discard);
                }
                self.pending_packet = None;
                self.input = Input::Open;
            }
            Submit::Busy => self.input = Input::Full,
        }
        Ok(())
    }

    /// Drops a chunk's skip-samples and places it by its timestamp.
    fn place(&mut self, mut chunk: PcmChunk) -> Result<(), DecodeError> {
        if chunk.planes.len() != self.channels {
            return Err(DecodeError::Adapter(format!(
                "the decoder gave {} channels of a {}-channel track",
                chunk.planes.len(),
                self.channels
            )));
        }
        let (front, back) = self.discards.remove(&chunk.pts).unwrap_or((0, 0));
        let length = chunk.planes[0].len();
        let front = usize::try_from(front).unwrap_or(usize::MAX).min(length);
        let back = usize::try_from(back)
            .unwrap_or(usize::MAX)
            .min(length - front);
        for plane in &mut chunk.planes {
            plane.truncate(length - back);
            plane.drain(..front);
        }
        if chunk.planes[0].is_empty() {
            return Ok(());
        }
        let rate = chunk.sample_rate;
        let scaled =
            i128::from(chunk.pts) * i128::from(rate) / i128::from(self.track.timescale.max(1));
        let mut at = i64::try_from(scaled).unwrap_or(0) + i64::try_from(front).unwrap_or(0);
        if let Some(expected) = self.timeline.expected(rate) {
            if at < expected {
                let overlap = usize::try_from(expected - at).unwrap_or(usize::MAX);
                let dropped = overlap.min(chunk.planes[0].len());
                self.recovery
                    .timestamp_overlap(at, at + i64::try_from(dropped).unwrap_or(0), rate);
                for plane in &mut chunk.planes {
                    plane.drain(..dropped);
                }
                at += i64::try_from(dropped).unwrap_or(0);
                if chunk.planes[0].is_empty() {
                    return Ok(());
                }
            } else if at > expected {
                self.recovery.timestamp_gap(expected, at, rate);
            }
        }
        self.timeline.place(rate, at, chunk.planes)
    }
}

impl Decoder for AudioStream {
    fn streams(&self) -> &[StreamInfo] {
        &self.info
    }

    fn flush(&mut self) -> Result<(), DecodeError> {
        self.reader.seek(self.track.id, i64::MIN)?;
        self.restart(None)
    }

    fn seek(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<DecodeResult, DecodeError> {
        let block = AudioStream::seek(self, request.time, cancel)?;
        Ok(DecodeResult {
            request,
            event: DecodeEvent::Audio(block),
        })
    }

    fn decode_next(&mut self, cancel: &CancelToken) -> Result<DecodeEvent, DecodeError> {
        Ok(match self.next_block(cancel)? {
            Some(block) => DecodeEvent::Audio(block),
            None => DecodeEvent::EndOfStream,
        })
    }
}

#[cfg(test)]
#[path = "audio_stream_tests.rs"]
mod tests;
