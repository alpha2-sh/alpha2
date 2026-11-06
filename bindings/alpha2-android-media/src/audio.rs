//! Audio decoding through `MediaCodec`.
//!
//! Decoders are given no codec delay: the container's skip-samples are applied by the host-core
//! stream, as `FFmpeg` applies them, so the decoder must not drop priming itself.

use std::collections::VecDeque;

use alpha2_container::{AudioCodec, AudioTrack, Packet, TrackInfo, vorbis_order};
use alpha2_host_core::audio_stream::{AudioBackends, AudioDecodeBackend, AudioPoll, PcmChunk};
use alpha2_host_core::video_stream::{BackendError, Submit};
use alpha2_media::{Speaker, SpeakerLayout};

use crate::codec::{
    Codec, FLAG_CODEC_CONFIG, FLAG_END_OF_STREAM, Format, Output, from_codec_us, to_codec_us,
};

/// `AudioFormat.ENCODING_PCM_16BIT` and `ENCODING_PCM_FLOAT`.
const PCM_16: i32 = 2;
const PCM_FLOAT: i32 = 4;

const WAIT_US: i64 = 10_000;

/// `AudioFormat.CHANNEL_OUT_*` bits in order, and the speaker each one is.
const CHANNEL_MASK: [(i32, Speaker); 18] = [
    (0x4, Speaker::FrontLeft),
    (0x8, Speaker::FrontRight),
    (0x10, Speaker::FrontCenter),
    (0x20, Speaker::LowFrequency),
    (0x40, Speaker::BackLeft),
    (0x80, Speaker::BackRight),
    (0x100, Speaker::FrontLeftOfCenter),
    (0x200, Speaker::FrontRightOfCenter),
    (0x400, Speaker::BackCenter),
    (0x800, Speaker::SideLeft),
    (0x1000, Speaker::SideRight),
    (0x2000, Speaker::TopCenter),
    (0x4000, Speaker::TopFrontLeft),
    (0x8000, Speaker::TopFrontCenter),
    (0x1_0000, Speaker::TopFrontRight),
    (0x2_0000, Speaker::TopBackLeft),
    (0x4_0000, Speaker::TopBackCenter),
    (0x8_0000, Speaker::TopBackRight),
];

/// The speakers a channel mask names, when it names exactly `channels` of them.
fn speakers(mask: i32, channels: usize) -> Option<SpeakerLayout> {
    let named = CHANNEL_MASK
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, speaker)| *speaker)
        .collect::<Vec<_>>();
    (named.len() == channels)
        .then(|| SpeakerLayout::new(&named))
        .flatten()
}

fn mime(codec: AudioCodec) -> Option<&'static str> {
    Some(match codec {
        AudioCodec::Aac => "audio/mp4a-latm",
        AudioCodec::Mp3 => "audio/mpeg",
        AudioCodec::Opus => "audio/opus",
        AudioCodec::Vorbis => "audio/vorbis",
        AudioCodec::Flac => "audio/flac",
        AudioCodec::Alac => "audio/alac",
        AudioCodec::Ac3 => "audio/ac3",
        AudioCodec::Eac3 => "audio/eac3",
        AudioCodec::Pcm(_) | AudioCodec::Dts | AudioCodec::Other(_) => return None,
    })
}

/// The platform's own software decoder for `codec`, in every Android release supported: its
/// behavior is the same on every device, where vendor decoders differ (one fails on FLAC with
/// more than two channels).
const fn aosp_decoder(codec: AudioCodec) -> Option<&'static str> {
    Some(match codec {
        AudioCodec::Aac => "c2.android.aac.decoder",
        AudioCodec::Mp3 => "c2.android.mp3.decoder",
        AudioCodec::Opus => "c2.android.opus.decoder",
        AudioCodec::Vorbis => "c2.android.vorbis.decoder",
        AudioCodec::Flac => "c2.android.flac.decoder",
        _ => return None,
    })
}

/// The three Vorbis headers from Xiph lacing.
fn vorbis_headers(private: &[u8]) -> Option<[&[u8]; 3]> {
    let (&count, mut rest) = private.split_first()?;
    if count != 2 {
        return None;
    }
    let mut sizes = [0_usize; 2];
    for size in &mut sizes {
        loop {
            let (&byte, tail) = rest.split_first()?;
            rest = tail;
            *size += usize::from(byte);
            if byte != 255 {
                break;
            }
        }
    }
    let (first, rest) = rest.split_at_checked(sizes[0])?;
    let (second, third) = rest.split_at_checked(sizes[1])?;
    Some([first, second, third])
}

/// The plane order of a decoder that gives no channel mask or a misleading one.
///
/// The Opus and Vorbis decoders keep the coded (Vorbis) order. The AAC decoder labels channel
/// configuration 7 as 7.1 but holds in its front pair what `FFmpeg` decodes as the side pair,
/// and the other way round.
fn plane_order(audio: &AudioTrack) -> Option<SpeakerLayout> {
    let channels = audio.channels;
    match audio.codec {
        AudioCodec::Opus | AudioCodec::Vorbis if channels > 2 => vorbis_order(channels),
        AudioCodec::Aac if channels == 8 && aac_configuration(&audio.codec_private) == Some(7) => {
            SpeakerLayout::new(&[
                Speaker::SideLeft,
                Speaker::SideRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
                Speaker::BackLeft,
                Speaker::BackRight,
                Speaker::FrontLeft,
                Speaker::FrontRight,
            ])
        }
        _ => None,
    }
}

/// The channel configuration of an `AudioSpecificConfig`.
fn aac_configuration(config: &[u8]) -> Option<u32> {
    let mut bits = config
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |bit| u32::from(byte >> bit & 1)));
    let mut read = |count: u32| -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some(value << 1 | bits.next()?))
    };
    if read(5)? == 31 {
        read(6)?; // escaped object type
    }
    if read(4)? == 15 {
        read(24)?; // explicit sampling frequency
    }
    read(4)
}

/// A `MediaCodec` audio decoder session.
pub struct MediaCodecAudio {
    codec: Codec,
    timescale: i64,
    channels: usize,
    rate: u32,
    encoding: i32,
    speakers: Option<SpeakerLayout>,
    /// The plane order the decoder uses where its channel mask is missing or misleading.
    order: Option<SpeakerLayout>,
    /// Presentation times of queued packets in order; outputs answer them in order.
    times: VecDeque<(i64, i64)>,
    eos_pending: bool,
    drained: bool,
}

impl std::fmt::Debug for MediaCodecAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaCodecAudio")
            .field("codec", &self.codec.name())
            .finish_non_exhaustive()
    }
}

impl MediaCodecAudio {
    /// A session for `track`.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when no decoder takes the track.
    pub fn open(track: &TrackInfo) -> Result<Self, BackendError> {
        let audio = track
            .audio
            .as_ref()
            .ok_or_else(|| BackendError::Unsupported("not an audio track".into()))?;
        let mime = mime(audio.codec).ok_or_else(|| {
            BackendError::Unsupported(format!("no Android decoder for {:?} audio", audio.codec))
        })?;
        let mut codec = match aosp_decoder(audio.codec).and_then(|name| Codec::by_name(name).ok()) {
            Some(codec) => codec,
            None => Codec::decoder_for(mime)?,
        };
        let mut format = Format::new();
        format.set_str("mime", mime);
        format.set_i32(
            "sample-rate",
            i32::try_from(audio.sample_rate).unwrap_or(48_000),
        );
        format.set_i32("channel-count", i32::try_from(audio.channels).unwrap_or(2));
        format.set_i32("pcm-encoding", PCM_FLOAT);
        // Decoders downmix surround to stereo unless told the output may keep every channel, and
        // the Dolby decoders take only speaker counts of whole layouts (2, 6 or 8): a 5.0 stream
        // comes out as 5.1, whose silent LFE the stream drops.
        let outputs = match audio.channels {
            0..=2 => 2,
            3..=6 => 6,
            _ => 8,
        };
        format.set_i32("max-output-channel-count", outputs);
        let private = &audio.codec_private[..];
        match audio.codec {
            AudioCodec::Opus => {
                format.set_buffer("csd-0", private);
                // Codec delay and seek pre-roll in nanoseconds: none, the stream applies them.
                format.set_buffer("csd-1", &0_i64.to_le_bytes());
                format.set_buffer("csd-2", &0_i64.to_le_bytes());
            }
            AudioCodec::Vorbis => {
                let [identification, _, setup] = vorbis_headers(private)
                    .ok_or_else(|| BackendError::Unsupported("unreadable Vorbis headers".into()))?;
                format.set_buffer("csd-0", identification);
                format.set_buffer("csd-1", setup);
            }
            _ if !private.is_empty() => format.set_buffer("csd-0", private),
            _ => {}
        }
        codec.start(&format, false)?;
        Ok(Self {
            codec,
            timescale: i64::from(track.timescale.max(1)),
            channels: usize::try_from(audio.channels).unwrap_or(0),
            rate: audio.sample_rate,
            encoding: PCM_FLOAT,
            speakers: None,
            order: plane_order(audio),
            times: VecDeque::new(),
            eos_pending: false,
            drained: false,
        })
    }

    /// The component decoding.
    #[must_use]
    pub fn component(&self) -> &str {
        self.codec.name()
    }

    fn read_format(&mut self) {
        let format = self.codec.output_format();
        if let Some(rate) = format
            .i32("sample-rate")
            .and_then(|rate| u32::try_from(rate).ok())
        {
            self.rate = rate;
        }
        if let Some(channels) = format
            .i32("channel-count")
            .and_then(|count| usize::try_from(count).ok())
        {
            self.channels = channels;
        }
        self.encoding = format.i32("pcm-encoding").unwrap_or(PCM_16);
        self.speakers = format
            .i32("channel-mask")
            .and_then(|mask| speakers(mask, self.channels));
    }

    fn try_queue_eos(&mut self) -> Result<(), BackendError> {
        if let Some(index) = self.codec.dequeue_input(0)? {
            self.codec.queue_input(index, &[], 0, FLAG_END_OF_STREAM)?;
            self.eos_pending = false;
        }
        Ok(())
    }

    fn planes(&self, bytes: &[u8]) -> Vec<Vec<f32>> {
        let channels = self.channels.max(1);
        let width = if self.encoding == PCM_FLOAT { 4 } else { 2 };
        let frames = bytes.len() / (width * channels);
        let mut planes = vec![Vec::with_capacity(frames); channels];
        for frame in 0..frames {
            for (channel, plane) in planes.iter_mut().enumerate() {
                let at = (frame * channels + channel) * width;
                let sample = if width == 4 {
                    f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                } else {
                    f32::from(i16::from_le_bytes([bytes[at], bytes[at + 1]])) / 32_768.0
                };
                plane.push(sample);
            }
        }
        planes
    }
}

impl AudioDecodeBackend for MediaCodecAudio {
    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        let Some(index) = self.codec.dequeue_input(0)? else {
            return Ok(Submit::Busy);
        };
        let pts_us = to_codec_us(packet.pts, self.timescale);
        self.codec.queue_input(index, &packet.data, pts_us, 0)?;
        self.times.push_back((pts_us, packet.pts));
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<AudioPoll, BackendError> {
        if self.drained {
            return Ok(AudioPoll::Drained);
        }
        if self.eos_pending {
            self.try_queue_eos()?;
        }
        loop {
            match self.codec.dequeue_output(if wait { WAIT_US } else { 0 })? {
                Output::TryAgain => return Ok(AudioPoll::Pending),
                Output::FormatChanged => self.read_format(),
                Output::Buffer {
                    index,
                    offset,
                    size,
                    pts_us,
                    flags,
                } => {
                    let end = flags & FLAG_END_OF_STREAM != 0;
                    let planes = (size > 0 && flags & FLAG_CODEC_CONFIG == 0)
                        .then(|| {
                            self.codec
                                .output_bytes(index, offset, size)
                                .map(|bytes| self.planes(bytes))
                        })
                        .flatten();
                    self.codec.release_output(index)?;
                    self.drained = end;
                    if let Some(planes) = planes {
                        // An output answers the queued packet with its time, and every one before.
                        let pts =
                            if let Some(at) = self.times.iter().position(|(us, _)| *us == pts_us) {
                                let pts = self.times[at].1;
                                self.times.drain(..=at);
                                pts
                            } else {
                                from_codec_us(pts_us, self.timescale)
                            };
                        return Ok(AudioPoll::Chunk(PcmChunk {
                            pts,
                            sample_rate: self.rate,
                            planes,
                            speakers: self.order.or(self.speakers),
                        }));
                    }
                    if end {
                        return Ok(AudioPoll::Drained);
                    }
                }
            }
        }
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.eos_pending = true;
        self.try_queue_eos()
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.codec.flush()?;
        self.times.clear();
        self.eos_pending = false;
        self.drained = false;
        Ok(())
    }
}

/// Opens `MediaCodec` audio sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AndroidAudio;

impl AudioBackends for AndroidAudio {
    fn open(&self, track: &TrackInfo) -> Result<Box<dyn AudioDecodeBackend>, BackendError> {
        Ok(Box::new(MediaCodecAudio::open(track)?))
    }
}
