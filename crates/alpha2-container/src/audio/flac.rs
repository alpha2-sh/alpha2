//! Native FLAC files, split into frames as `FFmpeg`'s flac parser does: at frame headers whose
//! CRC-8 holds and that continue the stream, or whose frame CRC-16 holds.

use bytes::Bytes;

use super::crc::{crc8, crc16};
use super::{Facts, Frame, Framer, Input, ScanReader, rescale_down, speakers_from_mask};
use crate::{AudioCodec, AudioTrack, ContainerReader, DemuxError};

const STREAMINFO: u8 = 0;
const PADDING: u8 = 1;
const SEEKTABLE: u8 = 3;
const VORBIS_COMMENT: u8 = 4;
const PICTURE: u8 = 6;
/// The largest `codec_private` this reader builds from metadata blocks.
const MAX_PRIVATE: usize = 1 << 24;
/// The largest frame this reader hands out; 65535 samples of 8 channels at 32 bits fit.
const MAX_FRAME: u64 = 1 << 25;
/// Bytes a frame header and the first subframe header take at most.
const HEADER_PEEK: usize = 17;
const SAMPLE_RATES: [u32; 12] = [
    0, 88_200, 176_400, 192_000, 8_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 96_000,
];
const SAMPLE_SIZES: [u32; 8] = [0, 8, 12, 0, 16, 20, 24, 32];

/// The WAVE channel masks of FLAC's channel orders for 1 to 8 channels.
pub(super) const CHANNEL_MASKS: [u64; 8] = [0x4, 0x3, 0x7, 0x33, 0x37, 0x3f, 0x70f, 0x63f];

/// A frame header, as `ff_flac_decode_frame_header` reads it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FrameHeader {
    pub(super) variable: bool,
    pub(super) blocksize: u32,
    pub(super) sample_rate: u32,
    pub(super) channels: u32,
    pub(super) bits: u32,
    /// The sample number (variable block size) or frame number (fixed).
    pub(super) number: i64,
}

/// Decodes the frame header opening `bytes` and checks its CRC-8 and first subframe header, as
/// the flac parser's `frame_header_is_valid` does.
pub(super) fn frame_header(bytes: &[u8]) -> Option<FrameHeader> {
    if bytes.len() < 6 || bytes[0] != 0xff || bytes[1] & 0xfe != 0xf8 {
        return None;
    }
    let variable = bytes[1] & 1 != 0;
    let blocksize_code = bytes[2] >> 4;
    let rate_code = bytes[2] & 0x0f;
    let channel_code = u32::from(bytes[3] >> 4);
    let channels = match channel_code {
        0..=7 => channel_code + 1,
        8..=10 => 2,
        _ => return None,
    };
    let size_code = usize::from((bytes[3] >> 1) & 7);
    if size_code == 3 || bytes[3] & 1 != 0 {
        return None;
    }
    let mut at = 4;
    let first = *bytes.get(at)?;
    at += 1;
    if first & 0xc0 == 0x80 || first >= 0xfe {
        return None;
    }
    let mut number = i64::from(first);
    let mut top = i64::from(first & 0x80) >> 1;
    while number & top != 0 {
        let next = i64::from(*bytes.get(at)?) - 128;
        at += 1;
        if next >> 6 != 0 {
            return None;
        }
        number = (number << 6) + next;
        top <<= 5;
    }
    number &= (top << 1) - 1;
    let mut read = |count: usize| -> Option<u32> {
        let value = bytes
            .get(at..at + count)?
            .iter()
            .fold(0, |value, byte| (value << 8) | u32::from(*byte));
        at += count;
        Some(value)
    };
    let blocksize = match blocksize_code {
        0 => return None,
        1 => 192,
        2..=5 => 576 << (blocksize_code - 2),
        6 => read(1)? + 1,
        7 => read(2)? + 1,
        _ => 256 << (blocksize_code - 8),
    };
    let sample_rate = match rate_code {
        0..=11 => SAMPLE_RATES[usize::from(rate_code)],
        12 => read(1)? * 1000,
        13 => read(2)?,
        14 => read(2)? * 10,
        _ => return None,
    };
    let crc = *bytes.get(at)?;
    if crc8(&bytes[..at]) != crc {
        return None;
    }
    let subframe = *bytes.get(at + 1)?;
    let kind = (subframe >> 1) & 0x3f;
    if subframe & 0x80 != 0 || !(kind <= 1 || (8..=12).contains(&kind) || kind >= 32) {
        return None;
    }
    Some(FrameHeader {
        variable,
        blocksize,
        sample_rate,
        channels,
        bits: SAMPLE_SIZES[size_code],
        number,
    })
}

impl FrameHeader {
    /// Whether `next` continues this frame's stream with nothing changed (no parser penalty).
    fn continued_by(&self, next: &Self) -> bool {
        next.sample_rate == self.sample_rate
            && next.bits == self.bits
            && next.variable == self.variable
            && next.channels == self.channels
            && (next.number - self.number == i64::from(self.blocksize)
                || next.number == self.number + 1)
    }

    /// Whether this frame directly follows `last`, the frame output before it.
    fn follows(&self, last: &Self) -> bool {
        last.variable == self.variable
            && if self.variable {
                last.number + i64::from(last.blocksize) == self.number
            } else {
                last.number + 1 == self.number
            }
    }
}

/// The `STREAMINFO` facts a reader needs.
pub(super) struct StreamInfo {
    pub(super) sample_rate: u32,
    pub(super) channels: u32,
    pub(super) bits: u32,
    pub(super) samples: u64,
    pub(super) max_blocksize: u32,
    pub(super) max_frame: u32,
}

pub(super) fn stream_info(block: &[u8]) -> Result<StreamInfo, DemuxError> {
    if block.len() != 34 {
        return Err(DemuxError::Invalid(
            "FLAC STREAMINFO is not 34 bytes".into(),
        ));
    }
    let packed = u64::from_be_bytes([
        block[10], block[11], block[12], block[13], block[14], block[15], block[16], block[17],
    ]);
    let info = StreamInfo {
        sample_rate: u32::try_from(packed >> 44).unwrap_or(0),
        channels: u32::try_from((packed >> 41) & 7).unwrap_or(0) + 1,
        bits: u32::try_from((packed >> 36) & 0x1f).unwrap_or(0) + 1,
        samples: packed & ((1 << 36) - 1),
        max_blocksize: u32::from(u16::from_be_bytes([block[2], block[3]])),
        max_frame: u32::from_be_bytes([0, block[7], block[8], block[9]]),
    };
    if info.sample_rate == 0 {
        return Err(DemuxError::Invalid("FLAC sample rate 0".into()));
    }
    Ok(info)
}

/// The channel mask a `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` Vorbis comment sets, as `FFmpeg` reads
/// it (`strtol` with base 0, at most 18 speakers).
pub(super) fn comment_channel_mask(block: &[u8]) -> Option<u64> {
    let mut mask = None;
    for comment in vorbis_comments(block) {
        let Some((key, value)) = comment.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("WAVEFORMATEXTENSIBLE_CHANNEL_MASK") {
            continue;
        }
        let value = value.trim_start();
        let parsed = if let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            u64::from_str_radix(&leading(hex, 16), 16).ok()
        } else if value.starts_with('0') {
            u64::from_str_radix(&leading(value, 8), 8).ok().or(Some(0))
        } else {
            leading(value, 10).parse().ok()
        };
        mask = parsed.filter(|mask| *mask != 0 && mask & !0x3_ffff == 0);
    }
    mask
}

fn leading(text: &str, radix: u32) -> String {
    text.chars().take_while(|c| c.is_digit(radix)).collect()
}

/// The comments of a Vorbis comment block (vendor string first, then counted comments).
pub(super) fn vorbis_comments(block: &[u8]) -> Vec<String> {
    let le32 = |at: usize| -> Option<usize> {
        let bytes = block.get(at..at + 4)?;
        usize::try_from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).ok()
    };
    let mut comments = Vec::new();
    let Some(vendor) = le32(0) else {
        return comments;
    };
    let mut at = 4 + vendor;
    let Some(count) = le32(at) else {
        return comments;
    };
    at += 4;
    for _ in 0..count {
        let Some(len) = le32(at) else {
            break;
        };
        at += 4;
        let Some(text) = block.get(at..at.saturating_add(len)) else {
            break;
        };
        at += len;
        comments.push(String::from_utf8_lossy(text).into_owned());
    }
    comments
}

/// Builds `codec_private` from metadata blocks: `fLaC` and every block but padding and pictures,
/// the last flagged as last.
pub(super) fn codec_private(blocks: &[(u8, Vec<u8>)]) -> Bytes {
    let kept = blocks
        .iter()
        .filter(|(kind, _)| *kind != PADDING && *kind != PICTURE)
        .collect::<Vec<_>>();
    let mut private = b"fLaC".to_vec();
    for (index, (kind, body)) in kept.iter().enumerate() {
        let last = if index + 1 == kept.len() { 0x80 } else { 0 };
        let size = u32::try_from(body.len()).unwrap_or(0).to_be_bytes();
        private.extend_from_slice(&[kind | last, size[1], size[2], size[3]]);
        private.extend_from_slice(body);
    }
    Bytes::from(private)
}

pub(super) fn open(mut input: Input, start: u64) -> Result<Box<dyn ContainerReader>, DemuxError> {
    if input.exact(start, 4)? != b"fLaC" {
        return Err(DemuxError::Invalid("not a FLAC file".into()));
    }
    let mut pos = start + 4;
    let mut blocks = Vec::new();
    let mut private_size = 0_usize;
    loop {
        let header = input.exact(pos, 4)?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let size =
            (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
        if blocks.is_empty() && kind != STREAMINFO {
            return Err(DemuxError::Invalid(
                "FLAC STREAMINFO is not the first block".into(),
            ));
        }
        if kind == STREAMINFO && !blocks.is_empty() {
            return Err(DemuxError::Invalid("FLAC has two STREAMINFO blocks".into()));
        }
        if kind != PADDING && kind != PICTURE {
            private_size += size + 4;
            if private_size > MAX_PRIVATE {
                return Err(DemuxError::Invalid("FLAC metadata is too large".into()));
            }
            blocks.push((kind, input.exact(pos + 4, size)?.to_vec()));
        } else {
            blocks.push((kind, Vec::new()));
        }
        pos += 4 + size as u64;
        if last {
            break;
        }
    }
    let info = stream_info(&blocks[0].1)?;
    let mask = blocks
        .iter()
        .filter(|(kind, _)| *kind == VORBIS_COMMENT)
        .find_map(|(_, body)| comment_channel_mask(body));
    let speakers = mask
        .and_then(|mask| speakers_from_mask(mask, info.channels))
        .or_else(|| speakers_from_mask(CHANNEL_MASKS[info.channels as usize - 1], info.channels));
    let seek_points = blocks
        .iter()
        .filter(|(kind, _)| *kind == SEEKTABLE)
        .flat_map(|(_, body)| body.as_chunks::<18>().0)
        .filter_map(|point| {
            let sample = u64::from_be_bytes(point[..8].try_into().ok()?);
            let offset = u64::from_be_bytes(point[8..16].try_into().ok()?);
            (sample != u64::MAX).then_some((i64::try_from(sample).ok()?, offset))
        })
        .collect::<Vec<_>>();
    let audio = AudioTrack {
        codec: AudioCodec::Flac,
        codec_private: codec_private(&blocks),
        sample_rate: info.sample_rate,
        channels: info.channels,
        speakers,
        bits_per_sample: u16::try_from(info.bits).ok(),
        priming: 0,
    };
    let framer = FlacFramer {
        first: pos,
        sample_rate: info.sample_rate,
        max_blocksize: i64::from(info.max_blocksize),
        max_frame: if info.max_frame > 0 {
            u64::from(info.max_frame) + 16
        } else {
            1 << 20
        },
        seek_points,
        state: FlacState {
            pos,
            next_pts: 0,
            last: None,
        },
    };
    let facts = Facts {
        timescale: info.sample_rate,
        duration: (info.samples > 0).then(|| i64::try_from(info.samples).unwrap_or(i64::MAX)),
        language: None,
        title: None,
        audio,
    };
    Ok(Box::new(ScanReader::new(input, facts, framer)))
}

#[derive(Clone, Copy)]
struct FlacState {
    pos: u64,
    /// Where the previous packet ended, as `FFmpeg` times packets the parser leaves untimed.
    next_pts: i64,
    last: Option<FrameHeader>,
}

struct FlacFramer {
    first: u64,
    sample_rate: u32,
    /// Samples in every frame of a fixed-block-size stream but its last.
    max_blocksize: i64,
    /// How far a bisection probe looks for a frame's end.
    max_frame: u64,
    seek_points: Vec<(i64, u64)>,
    state: FlacState,
}

/// Bytes below which a bisection stops and a scan takes over.
const BISECT_SPAN: u64 = 1 << 12;
/// Later headers a bisection probe tries as the end of a candidate frame.
const PROBE_CANDIDATES: usize = 8;

/// Bytes scanned per read while looking for frame headers.
const SCAN: usize = 1 << 16;

impl FlacFramer {
    /// The next position after `from` holding a valid frame header, and that header.
    fn next_header(
        input: &mut Input,
        mut from: u64,
    ) -> Result<Option<(u64, FrameHeader)>, DemuxError> {
        loop {
            let bytes = input.peek(from, SCAN + HEADER_PEEK)?;
            if bytes.len() < 2 {
                return Ok(None);
            }
            let scan = bytes.len().saturating_sub(1).min(SCAN);
            for offset in 0..scan {
                if bytes[offset] == 0xff
                    && bytes[offset + 1] & 0xfe == 0xf8
                    && let Some(header) = frame_header(&bytes[offset..])
                {
                    return Ok(Some((from + offset as u64, header)));
                }
            }
            if bytes.len() <= SCAN {
                return Ok(None);
            }
            from += SCAN as u64;
        }
    }

    /// Where the frame at `pos` with `header` ends: at the first later header that continues it
    /// or closes a valid CRC-16, else at the end of the file.
    fn frame_end(
        &self,
        input: &mut Input,
        pos: u64,
        header: &FrameHeader,
    ) -> Result<Option<u64>, DemuxError> {
        let trusted = self.state.last.is_some_and(|last| header.follows(&last));
        let mut from = pos + 1;
        while let Some((next, child)) = Self::next_header(input, from)? {
            if next - pos > MAX_FRAME {
                break;
            }
            let size = usize::try_from(next - pos).unwrap_or(usize::MAX);
            if (trusted && header.continued_by(&child)) || crc16(input.exact(pos, size)?) == 0 {
                return Ok(Some(next));
            }
            from = next + 1;
        }
        Ok(None)
    }

    /// The first frame header in `from..limit` whose frame CRC-16 holds up to a later header.
    fn verified_frame(
        &self,
        input: &mut Input,
        from: u64,
        limit: u64,
    ) -> Result<Option<(u64, FrameHeader)>, DemuxError> {
        let mut at = from;
        while let Some((pos, header)) = Self::next_header(input, at)? {
            if pos >= limit {
                break;
            }
            let mut probe = pos + 1;
            for _ in 0..PROBE_CANDIDATES {
                let Some((next, _)) = Self::next_header(input, probe)? else {
                    break;
                };
                if next - pos > self.max_frame {
                    break;
                }
                let size = usize::try_from(next - pos).unwrap_or(usize::MAX);
                if crc16(input.exact(pos, size)?) == 0 {
                    return Ok(Some((pos, header)));
                }
                probe = next + 1;
            }
            at = pos + 1;
        }
        Ok(None)
    }

    /// Bisects the file for the latest verified frame starting at or before `pts`.
    fn bisect(&self, input: &mut Input, pts: i64) -> Result<Option<(i64, u64)>, DemuxError> {
        let mut low = self.first;
        let mut high = input.len();
        let mut best = None;
        while high.saturating_sub(low) > BISECT_SPAN {
            let middle = low + (high - low) / 2;
            let Some((pos, header)) = self.verified_frame(input, middle, high)? else {
                high = middle;
                continue;
            };
            let start = if header.variable {
                header.number
            } else {
                header.number * self.max_blocksize
            };
            if start <= pts {
                best = Some((start, pos));
                low = pos + 1;
            } else {
                high = middle;
            }
        }
        Ok(best)
    }
}

impl Framer for FlacFramer {
    type State = FlacState;

    fn state(&self) -> FlacState {
        self.state
    }

    fn restore(&mut self, state: FlacState) {
        self.state = state;
    }

    fn next(&mut self, input: &mut Input, data: bool) -> Result<Option<Frame>, DemuxError> {
        let pos = self.state.pos;
        if pos >= input.len() {
            return Ok(None);
        }
        let header = frame_header(input.peek(pos, HEADER_PEEK)?);
        let (end, pts, duration) = if let Some(header) = header {
            let end = self.frame_end(input, pos, &header)?;
            let pts = if header.variable {
                header.number
            } else if end.is_some() {
                header.number * i64::from(header.blocksize)
            } else {
                self.state.next_pts
            };
            let rate = if header.sample_rate == 0 {
                self.sample_rate
            } else {
                header.sample_rate
            };
            let duration = rescale_down(
                i64::from(header.blocksize),
                i64::from(self.sample_rate),
                i64::from(rate),
            );
            self.state.last = Some(header);
            (end.unwrap_or(input.len()), pts, duration)
        } else {
            // Bytes before the next frame header go out as a junk packet without a duration.
            let Some((next, _)) = Self::next_header(input, pos + 1)? else {
                self.state.pos = input.len();
                return Ok(None);
            };
            (next, self.state.next_pts, 0)
        };
        if end - pos > MAX_FRAME {
            return Err(DemuxError::Invalid(format!(
                "no FLAC frame boundary within {MAX_FRAME} bytes of byte {pos}"
            )));
        }
        let size = usize::try_from(end - pos).unwrap_or(usize::MAX);
        let bytes = if data {
            Bytes::copy_from_slice(input.exact(pos, size)?)
        } else {
            Bytes::new()
        };
        self.state.pos = end;
        self.state.next_pts = pts + duration;
        Ok(Some(Frame {
            pts,
            duration,
            data: bytes,
            discard: (0, 0),
        }))
    }

    fn hint(
        &mut self,
        input: &mut Input,
        pts: i64,
    ) -> Result<Option<(i64, FlacState)>, DemuxError> {
        let point = self
            .seek_points
            .iter()
            .filter(|(sample, _)| *sample <= pts)
            .max_by_key(|(sample, _)| *sample)
            .map(|(sample, offset)| (*sample, self.first.saturating_add(*offset)));
        let found = self.bisect(input, pts)?;
        let best = match (point, found) {
            (Some(point), Some(found)) => Some(if found.0 > point.0 { found } else { point }),
            (point, found) => point.or(found),
        };
        Ok(best.map(|(sample, pos)| {
            (
                sample,
                FlacState {
                    pos,
                    next_pts: sample,
                    last: None,
                },
            )
        }))
    }
}
