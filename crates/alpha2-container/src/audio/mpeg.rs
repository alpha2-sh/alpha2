//! MPEG audio Layer III files, split into frames as `FFmpeg`'s mp3 demuxer and parser do.

use bytes::Bytes;

use super::{Facts, Frame, Framer, Input, ScanReader, rescale, rescale_down};
use crate::{AudioCodec, AudioTrack, ContainerReader, DemuxError};

/// The time base `FFmpeg` gives MP3 streams: the least common multiple of their sample rates.
const TIMESCALE: i64 = 14_112_000;
/// The decoder delay `FFmpeg` adds to the LAME encoder delay (528 plus one).
const DECODER_DELAY: i64 = 529;
/// Header bits two frames of one stream share (`MP3_MASK`).
const STREAM_MASK: u32 = 0xfffe_0ccf;
/// Header bits whose change resets the parser's trust in the stream (`SAME_HEADER_MASK`).
const SAME_HEADER_MASK: u32 = 0xffe0_0000 | (3 << 17) | (3 << 10) | (3 << 19);
/// The most bytes between two frames before the stream is taken to end.
const MAX_JUNK: u64 = 1 << 20;
/// How far past the tags `FFmpeg` looks for two consecutive frames.
const JUNK_WINDOW: u64 = 64 * 1024;
const BITRATES: [[[u32; 15]; 3]; 2] = [
    [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ],
    [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];

/// One decoded MPEG audio frame header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    /// MPEG-2 or 2.5 (low sampling frequency).
    lsf: bool,
    layer: u32,
    pub(crate) sample_rate: u32,
    channels: u32,
    /// Frame size in bytes.
    size: u64,
    /// Samples per frame.
    pub(crate) samples: i64,
    bit_rate: u32,
}

/// Decodes `header` as `avpriv_mpegaudio_decode_header` does; `None` if invalid or free-format.
pub(crate) fn decode(header: u32) -> Option<Header> {
    if header & 0xffe0_0000 != 0xffe0_0000
        || header & (3 << 19) == 1 << 19
        || header & (3 << 17) == 0
        || header & (0xf << 12) == 0xf << 12
        || header & (3 << 10) == 3 << 10
    {
        return None;
    }
    let (lsf, mpeg25) = if header & (1 << 20) != 0 {
        (header & (1 << 19) == 0, false)
    } else {
        (true, true)
    };
    let layer = 4 - ((header >> 17) & 3);
    let base = [44_100, 48_000, 32_000][((header >> 10) & 3) as usize];
    let sample_rate = base >> (u32::from(lsf) + u32::from(mpeg25));
    let bitrate_index = ((header >> 12) & 0xf) as usize;
    if bitrate_index == 0 {
        return None;
    }
    let padding = u64::from((header >> 9) & 1);
    let kbps = BITRATES[usize::from(lsf)][(layer - 1) as usize][bitrate_index];
    let size = match layer {
        1 => (u64::from(kbps) * 12_000 / u64::from(sample_rate) + padding) * 4,
        2 => u64::from(kbps) * 144_000 / u64::from(sample_rate) + padding,
        _ => u64::from(kbps) * 144_000 / (u64::from(sample_rate) << u32::from(lsf)) + padding,
    };
    let samples = match layer {
        1 => 384,
        2 => 1152,
        _ if lsf => 576,
        _ => 1152,
    };
    Some(Header {
        lsf,
        layer,
        sample_rate,
        channels: if (header >> 6) & 3 == 3 { 1 } else { 2 },
        size,
        samples,
        bit_rate: kbps * 1000,
    })
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The header at `pos` and its frame size, as the demuxer's `check` reads it.
fn check(input: &mut Input, pos: u64) -> Result<Option<(u32, Header)>, DemuxError> {
    let bytes = input.peek(pos, 4)?;
    if bytes.len() < 4 {
        return Err(DemuxError::Invalid(
            "no two consecutive MPEG audio frames".into(),
        ));
    }
    let header = be32(bytes);
    Ok(decode(header).map(|decoded| (header, decoded)))
}

/// What a Xing, Info or VBRI frame says about the stream.
#[derive(Default)]
struct VbrInfo {
    frames: u32,
    stream_size: u32,
    start_pad: i64,
    end_pad: i64,
    gapless: bool,
}

/// Reads the VBR tag frame at `base`, as `mp3_parse_vbr_tags` does; returns its size when the
/// frame is a tag rather than audio.
fn parse_vbr_tags(input: &mut Input, base: u64, info: &mut VbrInfo) -> Result<u64, DemuxError> {
    let head = input.peek(base, 4)?;
    if head.len() < 4 {
        return Ok(0);
    }
    let Some(header) = decode(be32(head)) else {
        return Ok(0);
    };
    if header.layer != 3 {
        return Ok(0);
    }
    let offset = match (header.lsf, header.channels == 1) {
        (false, false) => 32,
        (false, true) | (true, false) => 17,
        (true, true) => 9,
    };
    let mut tag = input.peek(base + 4 + offset, 192)?.to_vec();
    tag.resize(192, 0);
    let file_left = input.len().saturating_sub(base + 4);
    if &tag[..4] == b"Xing" || &tag[..4] == b"Info" {
        let flags = be32(&tag[4..]);
        let mut at = 8;
        let mut take = |count: usize| {
            let field = &tag[at..at + count];
            at += count;
            field.to_vec()
        };
        if flags & 1 != 0 {
            info.frames = be32(&take(4));
        }
        if flags & 2 != 0 {
            info.stream_size = be32(&take(4));
        }
        if info.stream_size != 0 && file_left != 0 {
            let declared = u64::from(info.stream_size);
            let min = file_left.min(declared);
            let delta = file_left.max(declared) - min;
            if file_left > declared && delta > min >> 4 {
                info.frames = 0;
            }
        }
        if flags & 4 != 0 {
            take(100);
        }
        if flags & 8 != 0 {
            take(4);
        }
        let version = take(9);
        take(1 + 1 + 4 + 2 + 2 + 1 + 1);
        let delays = take(3);
        if [&b"LAME"[..], b"Lavf", b"Lavc"].contains(&&version[..4]) {
            let delays =
                (u32::from(delays[0]) << 16) | (u32::from(delays[1]) << 8) | u32::from(delays[2]);
            info.start_pad = i64::from(delays >> 12);
            info.end_pad = i64::from(delays & 0xfff);
            info.gapless = true;
        }
    }
    let mut vbri = input.peek(base + 4 + 32, 18)?.to_vec();
    vbri.resize(18, 0);
    if &vbri[..4] == b"VBRI" && vbri[4..6] == [0, 1] {
        info.stream_size = be32(&vbri[10..]);
        info.frames = be32(&vbri[14..]);
    }
    if info.frames == 0 && info.stream_size == 0 {
        return Ok(0);
    }
    Ok(header.size)
}

pub(super) fn open(mut input: Input, start: u64) -> Result<Box<dyn ContainerReader>, DemuxError> {
    let mut info = VbrInfo::default();
    let tag_size = parse_vbr_tags(&mut input, start, &mut info)?;
    let off = start + tag_size;
    let mut data = off;
    for skip in 0..JUNK_WINDOW {
        if let Some((header, decoded)) = check(&mut input, off + skip)?
            && let Ok(Some((next, _))) = check(&mut input, off + skip + decoded.size)
            && header & STREAM_MASK == next & STREAM_MASK
        {
            data = off + skip;
            break;
        }
    }
    let mut first = data;
    let header = loop {
        let bytes = input.peek(first, 4)?;
        if bytes.len() < 4 {
            return Err(DemuxError::Invalid("no MPEG audio frame".into()));
        }
        if let Some(header) = decode(be32(bytes)) {
            break header;
        }
        first += 1;
    };
    if header.layer != 3 {
        return Err(DemuxError::Unsupported(format!(
            "MPEG audio layer {} is not read; only Layer III (MP3)",
            header.layer
        )));
    }
    let rate = i64::from(header.sample_rate);
    let start_skip = if info.gapless {
        info.start_pad + DECODER_DELAY
    } else {
        0
    };
    let frame_count = i64::from(info.frames);
    let (first_discard, last_discard) = if info.gapless && frame_count > 0 {
        let total = frame_count * header.samples;
        (total - info.end_pad + DECODER_DELAY, total)
    } else {
        (0, 0)
    };
    let duration = if frame_count > 0 {
        Some(rescale(
            frame_count * header.samples - info.start_pad - info.end_pad,
            TIMESCALE,
            rate,
        ))
    } else {
        let size = i64::try_from(input.len().saturating_sub(data)).unwrap_or(i64::MAX);
        (size > 0).then(|| rescale(size, 8 * TIMESCALE, i64::from(header.bit_rate)))
    };
    let audio = AudioTrack {
        codec: AudioCodec::Mp3,
        codec_private: Bytes::new(),
        sample_rate: header.sample_rate,
        channels: header.channels,
        speakers: None,
        bits_per_sample: None,
        priming: u32::try_from(start_skip).unwrap_or(0),
        seek_preroll: None,
    };
    let framer = MpegFramer {
        sample_rate: rate,
        start_skip,
        first_discard,
        last_discard,
        state: MpegState {
            pos: data,
            pts: 0,
            header: 0,
            header_count: 0,
            samples: header.samples,
            rate,
        },
    };
    let facts = Facts {
        timescale: u32::try_from(TIMESCALE).unwrap_or(0),
        duration,
        language: None,
        title: None,
        audio,
    };
    Ok(Box::new(ScanReader::new(input, facts, framer)))
}

#[derive(Clone, Copy)]
struct MpegState {
    pos: u64,
    pts: i64,
    /// The last header the parser accepted, and its trust in the stream (`header_count`).
    header: u32,
    header_count: i32,
    /// Samples per frame and sample rate the parser times packets with.
    samples: i64,
    rate: i64,
}

struct MpegFramer {
    /// The stream's sample rate, as `FFmpeg` converts timestamps to samples.
    sample_rate: i64,
    start_skip: i64,
    first_discard: i64,
    last_discard: i64,
    state: MpegState,
}

impl MpegFramer {
    /// The skip-samples `FFmpeg`'s demuxer attaches to a packet at `pts` lasting `duration`.
    fn discard(&self, pts: i64, duration: i64) -> (u32, u32) {
        let skip = if self.start_skip > 0 && pts == 0 {
            self.start_skip
        } else {
            0
        };
        let mut padding = 0;
        if self.first_discard > 0 {
            let sample = rescale(pts, self.sample_rate, TIMESCALE);
            let samples = rescale(duration, self.sample_rate, TIMESCALE);
            let end = sample + samples;
            if samples > 0 && end >= self.first_discard && sample < self.last_discard {
                padding = (end - self.first_discard).min(samples);
            }
        }
        (
            u32::try_from(skip).unwrap_or(0),
            u32::try_from(padding).unwrap_or(0),
        )
    }
}

impl Framer for MpegFramer {
    type State = MpegState;

    fn state(&self) -> MpegState {
        self.state
    }

    fn restore(&mut self, state: MpegState) {
        self.state = state;
    }

    fn next(&mut self, input: &mut Input, data: bool) -> Result<Option<Frame>, DemuxError> {
        let start = self.state.pos;
        let len = input.len();
        if start >= len {
            return Ok(None);
        }
        let mut pos = start;
        let mut found = None;
        loop {
            let bytes = input.peek(pos, 4)?;
            if bytes.len() < 4 {
                break;
            }
            if pos - start > MAX_JUNK {
                self.state.pos = len;
                return Ok(None);
            }
            let header = be32(bytes);
            if let Some(decoded) = decode(header) {
                found = Some((header, decoded));
                break;
            }
            pos += 1;
        }
        let end = if let Some((header, decoded)) = found {
            let state = &mut self.state;
            if pos > start {
                state.header_count = -2;
            }
            if state.header != 0 && header & SAME_HEADER_MASK != state.header & SAME_HEADER_MASK {
                state.header_count = -3;
            }
            state.header = header;
            state.header_count += 1;
            let threshold = i32::from(decoded.layer != 3);
            if state.header_count > threshold {
                state.rate = i64::from(decoded.sample_rate);
                state.samples = decoded.samples;
            }
            (pos + decoded.size).min(len)
        } else {
            let rest = input.peek(start, 128)?;
            let tag = rest.len() >= 128 && rest.starts_with(b"TAG");
            let ape = rest.len() >= 32 && rest.starts_with(b"APETAGEX");
            if tag || ape || len - start > MAX_JUNK {
                self.state.pos = len;
                return Ok(None);
            }
            len
        };
        let size = usize::try_from(end - start).unwrap_or(usize::MAX);
        let bytes = if data {
            Bytes::copy_from_slice(input.exact(start, size)?)
        } else {
            Bytes::new()
        };
        let duration = rescale_down(self.state.samples, TIMESCALE, self.state.rate);
        let pts = self.state.pts;
        self.state.pos = end;
        self.state.pts += duration;
        Ok(Some(Frame {
            pts,
            duration,
            data: bytes,
            discard: self.discard(pts, duration),
        }))
    }
}
