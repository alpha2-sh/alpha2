//! Audio decoding over a container reader and a decoder session, placed by timestamp at the
//! project rate.
//!
//! [`AudioStream`] applies what `FFmpeg`'s demuxers and decoders do for the native host: each
//! packet's skip-samples (priming, end padding) are dropped, decoded samples sit at their packet's
//! timestamp with gaps left silent and overlaps dropped, and everything is resampled to the
//! project rate. A seek decodes from a pre-roll before its target and hands out samples from the
//! target on.

use std::collections::{HashMap, VecDeque};

use alpha2_container::{
    AudioCodec, AudioTrack, ContainerReader, Packet, PcmFormat, TrackInfo, TrackKind,
};
use alpha2_media::{
    AudioBlock, CancelToken, DecodeError, DecodeEvent, DecodeRequest, DecodeResult, Decoder,
    Orientation, SampleAspectRatio, Speaker, SpeakerLayout, StreamInfo, StreamKind,
};
use alpha2_time::{AudioFrameGrid, MediaTime, TICKS_PER_SECOND, audio_frame_grid};

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
    /// The speakers of the planes, when not the stream's: the stream takes its own speakers from
    /// them in its order, a back pair standing in for a side pair it lacks, and drops the rest
    /// (a decoder's silent LFE). `None` means the stream's speakers, in `FFmpeg`'s native channel
    /// order for compressed codecs.
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

/// Samples at 48 kHz an Opus decoder decodes before a seek target when the container declares no
/// pre-roll: the 80 ms RFC 7845 section 4.6 asks for at least.
const OPUS_SEEK_PREROLL: u32 = 3_840;

/// The least Opus pre-roll, 400 ms at 48 kHz: the decoder keeps converging past the RFC's 80 ms,
/// to under 1e-6 of a straight decode from 400 ms, the pre-roll of the desktop host.
const OPUS_CONVERGED_PREROLL: u32 = 19_200;

/// The least pre-roll for every codec, 100 ms: the desktop host's. A platform decoder resumed one
/// frame before its target still differs from a straight decode by up to about 1e-4 for the next
/// frames, so a codec's spec minimum alone would not give the samples the desktop host gives; the
/// codec's own figure decides where it is longer (low sample rates).
const SHARED_SEEK_PREROLL: MediaTime = MediaTime::from_ticks(TICKS_PER_SECOND / 10);

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
    /// Skip-samples a packet left over for the decoded audio after it.
    carried_skip: usize,
    /// The frame grid of the first packet timestamps, which places the first chunk after an open
    /// or seek where a straight decode puts it; `None` where timestamps are exact.
    grid: Option<AudioFrameGrid>,
    /// How far before its target a seek starts decoding ([`seek_preroll`]).
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

/// The planes of `to`'s speakers, in its order, from `planes` of `from`'s; `planes` unchanged
/// unless every speaker of `to` has its own plane. A back speaker stands in for the side speaker
/// on its side that `from` lacks, and the other way round, since layouts name the surround pair
/// of a 5.1 either way.
fn select(mut planes: Vec<Vec<f32>>, from: SpeakerLayout, to: SpeakerLayout) -> Vec<Vec<f32>> {
    let from = from.speakers();
    if from.len() != planes.len() {
        return planes;
    }
    let counterpart = |speaker: Speaker| match speaker {
        Speaker::SideLeft => Some(Speaker::BackLeft),
        Speaker::SideRight => Some(Speaker::BackRight),
        Speaker::BackLeft => Some(Speaker::SideLeft),
        Speaker::BackRight => Some(Speaker::SideRight),
        _ => None,
    };
    let find = |speaker: Speaker| from.iter().position(|named| *named == speaker);
    let positions = to
        .speakers()
        .iter()
        .map(|&speaker| {
            find(speaker).or_else(|| {
                counterpart(speaker)
                    .filter(|other| !to.speakers().contains(other))
                    .and_then(find)
            })
        })
        .collect::<Option<Vec<_>>>();
    let Some(positions) = positions.filter(|positions| {
        positions
            .iter()
            .enumerate()
            .all(|(index, position)| !positions[..index].contains(position))
    }) else {
        return planes;
    };
    let mut taken = planes.iter_mut().map(std::mem::take).collect::<Vec<_>>();
    positions
        .into_iter()
        .map(|position| std::mem::take(&mut taken[position]))
        .collect()
}

/// The frame grid `track`'s first packet timestamps show ([`audio_frame_grid`]), read from
/// `reader`, which the caller seeks back.
fn frame_grid(
    reader: &mut dyn ContainerReader,
    track: &TrackInfo,
    sample_rate: u32,
) -> Result<Option<AudioFrameGrid>, DecodeError> {
    let timestamps = crate::video_stream::first_timestamps(reader, track)?;
    Ok(audio_frame_grid(
        &timestamps,
        1,
        i64::from(track.timescale.max(1)),
        sample_rate,
    ))
}

/// How far before a seek target decoding starts, so samples from the target on come out as a
/// straight decode gives them.
///
/// The codec's own need, at least [`SHARED_SEEK_PREROLL`], comes first, then one timestamp unit
/// (the reader seeks by rounded timestamps) and the resampler's reach before its first output when
/// the project rate differs.
fn seek_preroll(audio: &AudioTrack, timescale: u32, project_rate: u32) -> MediaTime {
    let rate = audio.sample_rate.max(1);
    // Samples and the rate they are counted at.
    let codec = match audio.codec {
        // Each frame's IMDCT output is overlap-added with the previous frame's (ISO/IEC 14496-3
        // 4.6.11), so the frame before the target's decodes first.
        AudioCodec::Aac => {
            Some(crate::aac::core_frame(&audio.codec_private).unwrap_or((1_024, rate)))
        }
        AudioCodec::Opus => {
            let declared = audio.seek_preroll.unwrap_or(OPUS_SEEK_PREROLL);
            Some((declared.max(OPUS_CONVERGED_PREROLL), 48_000))
        }
        // A packet's window overlaps the previous packet's, and blocks reach 8192 samples
        // (Vorbis I specification 4.2.2).
        AudioCodec::Vorbis => Some((8_192, rate)),
        // Layer III `main_data_begin` reaches up to 511 bytes back (ISO/IEC 11172-3), six 96-byte
        // frames at its lowest rate, plus one frame of filterbank overlap: 7 x 1152.
        AudioCodec::Mp3 => Some((8_064, rate)),
        // Each block's MDCT overlaps the previous block's; a frame is 1536 samples (ATSC A/52).
        AudioCodec::Ac3 | AudioCodec::Eac3 => Some((1_536, rate)),
        // The core's QMF history spans less than one frame of at most 4096 samples
        // (ETSI TS 102 114).
        AudioCodec::Dts => Some((4_096, rate)),
        // Frames decode alone.
        AudioCodec::Pcm(_) | AudioCodec::Flac | AudioCodec::Alac => Some((0, rate)),
        AudioCodec::Other(_) => None,
    };
    let ticks = |samples: u64, rate: u32| {
        let ticks = (u128::from(samples) * u128::from(TICKS_PER_SECOND.unsigned_abs()))
            .div_ceil(u128::from(rate.max(1)));
        i64::try_from(ticks).unwrap_or(i64::MAX)
    };
    let codec = codec
        .map_or(0, |(samples, rate)| ticks(u64::from(samples), rate))
        .max(SHARED_SEEK_PREROLL.ticks());
    let unit = ticks(1, timescale);
    let resampler = alpha2_audio::Resampler::new(rate, project_rate)
        .ok()
        .filter(|resampler| !resampler.is_identity())
        .map_or(0, |resampler| {
            ticks(resampler.half_taps().unsigned_abs(), rate)
        });
    MediaTime::from_ticks(codec.saturating_add(unit).saturating_add(resampler))
}

/// What [`Decoder::streams`] tells of audio track `index`.
fn audio_stream_info(index: usize, track: &TrackInfo) -> Option<StreamInfo> {
    let audio = track.audio.as_ref()?;
    Some(StreamInfo {
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
    })
}

/// The speakers of a track's decoded planes.
///
/// PCM planes keep the container's order. Decoders of compressed audio deliver `FFmpeg`'s native
/// channel order (the WAVE mask order), so a declared layout is put in that order, and a track
/// that declares none takes its codec's layout for the channel count.
fn decoded_speakers(audio: &AudioTrack, channels: usize) -> Option<SpeakerLayout> {
    if matches!(audio.codec, AudioCodec::Pcm(_)) {
        return audio
            .speakers
            .or_else(|| SpeakerLayout::default_for(channels));
    }
    if let Some(declared) = audio.speakers {
        let mut speakers = declared.speakers().to_vec();
        speakers.sort_by_key(|speaker| speaker.ffmpeg_channel());
        return SpeakerLayout::new(&speakers);
    }
    let codec_layout = match audio.codec {
        AudioCodec::Aac => crate::aac::layout(&audio.codec_private, channels),
        AudioCodec::Opus | AudioCodec::Vorbis | AudioCodec::Flac => vorbis_family_layout(channels),
        _ => None,
    };
    codec_layout.or_else(|| SpeakerLayout::default_for(channels))
}

/// The layouts Vorbis defines by channel count (Opus mapping family 1 and FLAC share them), in
/// native order.
fn vorbis_family_layout(channels: usize) -> Option<SpeakerLayout> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency,
        SideLeft, SideRight,
    };
    let speakers: &[Speaker] = match channels {
        1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontRight, FrontCenter],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
        6 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
        ],
        7 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackCenter,
            SideLeft,
            SideRight,
        ],
        8 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ],
        _ => return None,
    };
    SpeakerLayout::new(speakers)
}

impl AudioStream {
    /// Opens audio track `stream` (an ordinal over the file's audio tracks in container order;
    /// `None` is the default track) of the file at `path`, delivered at `project_rate`.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a container this layer does not read, a missing track,
    /// or a track no session reads; read failures.
    pub fn open_file(
        path: &std::path::Path,
        backends: &dyn AudioBackends,
        project_rate: u32,
        stream: Option<u32>,
        policy: DecodePolicy,
    ) -> Result<Self, DecodeError> {
        let reader = alpha2_container::open(path)?;
        let tracks = reader.tracks();
        let index = match stream {
            None => alpha2_container::default_track(tracks, TrackKind::Audio),
            Some(ordinal) => tracks
                .iter()
                .enumerate()
                .filter(|(_, track)| track.kind == TrackKind::Audio)
                .nth(usize::try_from(ordinal).unwrap_or(usize::MAX))
                .map(|(index, _)| index),
        }
        .ok_or_else(|| {
            DecodeError::Unsupported(match stream {
                Some(ordinal) => format!("the file has no audio stream {ordinal}"),
                None => "the file has no audio stream".into(),
            })
        })?;
        Self::open(reader, index, backends, project_rate, policy)
    }

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
        let speakers = decoded_speakers(&audio, channels).ok_or_else(|| {
            DecodeError::Unsupported(format!("no layout for {channels} channels"))
        })?;
        let grid = frame_grid(reader.as_mut(), &track, audio.sample_rate)?;
        reader.seek(track.id, i64::MIN)?;
        let mut recovery = Recovery::new((1, i32::try_from(track.timescale).unwrap_or(i32::MAX)));
        recovery.policy = policy;
        // Every audio stream of the file, as the desktop decoder lists them.
        let info = reader
            .tracks()
            .iter()
            .enumerate()
            .filter_map(|(index, track)| audio_stream_info(index, track))
            .collect();
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
            carried_skip: 0,
            grid,
            preroll: seek_preroll(&audio, track.timescale, project_rate),
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
        self.carried_skip = 0;
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
    ///
    /// A chunk whose packet carries no leading skip-samples has its samples before time 0
    /// dropped instead: they are priming the container does not mark on that packet, as on a
    /// packet a seek's pre-roll starts within the priming.
    fn place(&mut self, mut chunk: PcmChunk) -> Result<(), DecodeError> {
        if let Some(order) = chunk.speakers.filter(|order| *order != self.speakers) {
            chunk.planes = select(chunk.planes, order, self.speakers);
        }
        if chunk.planes.len() != self.channels {
            return Err(DecodeError::Adapter(format!(
                "the decoder gave {} channels of a {}-channel track",
                chunk.planes.len(),
                self.channels
            )));
        }
        let (front, back) = self.discards.remove(&chunk.pts).unwrap_or((0, 0));
        let length = chunk.planes[0].len();
        let rate = chunk.sample_rate;
        let scaled =
            i128::from(chunk.pts) * i128::from(rate) / i128::from(self.track.timescale.max(1));
        let scaled = i64::try_from(scaled).unwrap_or(0);
        let before_zero = if front == 0 {
            let start = self.source_start(scaled, length, rate);
            usize::try_from(start.saturating_neg().max(0)).unwrap_or(usize::MAX)
        } else {
            0
        };
        // Skip-samples longer than their packet go on into the next ones, as `FFmpeg` skips them.
        let front = usize::try_from(front)
            .unwrap_or(usize::MAX)
            .saturating_add(std::mem::take(&mut self.carried_skip))
            .max(before_zero);
        self.carried_skip = front.saturating_sub(length);
        let front = front.min(length);
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
        let mut at = scaled + i64::try_from(front).unwrap_or(0);
        let frames = i64::try_from(chunk.planes[0].len()).unwrap_or(i64::MAX);
        match self.timeline.expected(rate) {
            None => {
                if let Some(grid) = self.grid_at(rate) {
                    at = grid.place(at, frames);
                }
            }
            Some(expected) if self.snaps(at, expected, rate) => at = expected,
            Some(_) => {}
        }
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

    /// The frame grid, for chunks at the track's declared rate.
    fn grid_at(&self, rate: u32) -> Option<AudioFrameGrid> {
        let declared = self.track.audio.as_ref().map(|audio| audio.sample_rate);
        self.grid.filter(|_| declared == Some(rate))
    }

    /// Whether a continuing chunk whose timestamp says `at` starts at `expected`, the end of the
    /// samples before it.
    ///
    /// Both positions come from timestamps rounded to the container's unit (Matroska's
    /// milliseconds): each is within half a unit of its exact position, and truncating to samples
    /// adds under one sample to their difference. One unit rounded up to samples bounds it.
    fn snaps(&self, at: i64, expected: i64, rate: u32) -> bool {
        let unit = u64::from(rate).div_ceil(u64::from(self.track.timescale.max(1)));
        let unit = i64::try_from(unit).unwrap_or(i64::MAX);
        unit > 1 && (at - expected).abs() <= unit
    }

    /// Where a `length`-sample chunk whose timestamp says `scaled` starts before any skip, as
    /// [`Self::place`] would put it.
    fn source_start(&self, scaled: i64, length: usize, rate: u32) -> i64 {
        let frames = i64::try_from(length).unwrap_or(i64::MAX);
        match self.timeline.expected(rate) {
            None => self
                .grid_at(rate)
                .map_or(scaled, |grid| grid.place(scaled, frames)),
            Some(expected) if self.snaps(scaled, expected, rate) => expected,
            Some(_) => scaled,
        }
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

/// A stream behind a mix's audio windows; its blocks hold at most `BLOCK_FRAMES` samples.
#[cfg(feature = "engine")]
impl crate::audio_windows::WindowedAudioDecoder for AudioStream {
    fn channels(&self) -> usize {
        self.channels
    }

    fn speakers(&self) -> SpeakerLayout {
        self.speakers
    }

    fn audio_duration(&self) -> Option<MediaTime> {
        self.info.first().and_then(|info| info.duration)
    }
}
