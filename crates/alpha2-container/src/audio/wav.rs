//! RIFF WAVE files of integer or float PCM, packetized as `FFmpeg`'s wav demuxer does.

use bytes::Bytes;

use super::{Facts, Frame, Framer, Input, ScanReader, speakers_from_mask};
use crate::{AudioCodec, AudioTrack, ContainerReader, DemuxError, PcmFormat};

/// The last 12 bytes of `KSDATAFORMAT_SUBTYPE_PCM` and its siblings, whose first 4 hold a tag.
const SUBTYPE_BASE: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
/// The same for the ambisonic B-format subtypes.
const AMBISONIC_BASE: [u8; 12] = [
    0x21, 0x07, 0xd3, 0x11, 0x86, 0x44, 0xc8, 0xc1, 0xca, 0x00, 0x00, 0x00,
];
/// Packets per second of audio `FFmpeg` aims at (`PCM_DEMUX_TARGET_FPS`).
const PACKETS_PER_SECOND: i64 = 10;
/// The largest packet this reader hands out.
const MAX_PACKET: i64 = 1 << 26;

struct Format {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits: u16,
    mask: Option<u32>,
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Parses a `fmt ` chunk as `ff_get_wav_header` does.
fn parse_format(body: &[u8]) -> Result<Format, DemuxError> {
    if body.len() < 14 {
        return Err(DemuxError::Invalid(
            "the WAVE fmt chunk is too short".into(),
        ));
    }
    let mut format = Format {
        tag: le16(body, 0),
        channels: le16(body, 2),
        sample_rate: le32(body, 4),
        byte_rate: le32(body, 8),
        block_align: le16(body, 12),
        bits: if body.len() >= 16 { le16(body, 14) } else { 8 },
        mask: None,
    };
    if body.len() >= 18 {
        let extension = usize::from(le16(body, 16)).min(body.len() - 18);
        if format.tag == 0xfffe && extension >= 22 {
            let valid = le16(body, 18);
            if valid != 0 {
                format.bits = valid;
            }
            format.mask = Some(le32(body, 20));
            let guid = &body[24..40];
            format.tag = if guid[4..] == SUBTYPE_BASE || guid[4..] == AMBISONIC_BASE {
                u16::try_from(le32(guid, 0)).unwrap_or(0)
            } else {
                0
            };
        }
    }
    if format.tag == 0xfffe {
        format.tag = 0;
    }
    if format.sample_rate == 0 || format.sample_rate > i32::MAX as u32 {
        return Err(DemuxError::Invalid(format!(
            "invalid WAVE sample rate {}",
            format.sample_rate
        )));
    }
    Ok(format)
}

#[derive(Clone, Copy)]
struct Data {
    start: u64,
    /// The declared size; 0 when unknown.
    size: u64,
    end: u64,
}

/// Walks the chunks as `wav_read_header` does: the first `fmt ` chunk and the last `data` chunk
/// before the end of the file or a `data` chunk of unknown size.
fn chunks(input: &mut Input) -> Result<(Format, Data), DemuxError> {
    let riff = input.exact(0, 12)?;
    if &riff[..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(DemuxError::Invalid("not a RIFF WAVE file".into()));
    }
    let mut format = None;
    let mut data = None;
    let mut pos = 12_u64;
    loop {
        let header = input.peek(pos, 8)?;
        if header.len() < 8 {
            break;
        }
        let tag: [u8; 4] = [header[0], header[1], header[2], header[3]];
        let size = u64::from(le32(header, 4));
        let body = pos + 8;
        let mut next = body + size;
        if &tag == b"fmt " && format.is_none() {
            let bytes = input.peek(body, usize::try_from(size).unwrap_or(usize::MAX))?;
            format = Some(parse_format(bytes)?);
        } else if &tag == b"data" {
            if size == 0 || size == 0xffff_ffff {
                data = Some(Data {
                    start: body,
                    size: 0,
                    end: u64::MAX,
                });
                break;
            }
            data = Some(Data {
                start: body,
                size,
                end: next,
            });
        }
        if next >= input.len() {
            break;
        }
        next += next & 1;
        pos = next;
    }
    let format =
        format.ok_or_else(|| DemuxError::Invalid("the WAVE file has no fmt chunk".into()))?;
    let data = data.ok_or_else(|| DemuxError::Invalid("the WAVE file has no data chunk".into()))?;
    Ok((format, data))
}

pub(super) fn open(mut input: Input) -> Result<Box<dyn ContainerReader>, DemuxError> {
    let (format, data) = chunks(&mut input)?;
    let float = match format.tag {
        1 => false,
        3 => true,
        tag => {
            return Err(DemuxError::Unsupported(format!(
                "WAVE format tag {tag:#06x} is not PCM"
            )));
        }
    };
    let channels = u32::from(format.channels);
    let block_align = u32::from(format.block_align);
    if channels == 0 || block_align == 0 || block_align % channels != 0 {
        return Err(DemuxError::Invalid(format!(
            "WAVE block size {block_align} does not fit {channels} channels"
        )));
    }
    let container = block_align / channels;
    let codec_bytes = u32::from(format.bits).div_ceil(8);
    let sizes_ok = if float {
        matches!(container, 4 | 8) && codec_bytes == container
    } else {
        matches!(container, 1..=4 | 8) && (1..=container).contains(&codec_bytes)
    };
    if !sizes_ok || format.bits == 0 {
        return Err(DemuxError::Unsupported(format!(
            "{}-bit {} PCM in {container}-byte samples",
            format.bits,
            if float { "float" } else { "integer" }
        )));
    }
    let codec_bits = i64::from(codec_bytes * 8);
    let rate = i64::from(format.sample_rate);
    let align = i64::from(block_align);
    let bit_rate = codec_bits * rate * i64::from(channels);
    let samples = (bit_rate / 8 / PACKETS_PER_SECOND / align).clamp(1, i64::from(i32::MAX) / align);
    let packet = (align << samples.ilog2()).max(align) / align * align;
    if packet > MAX_PACKET {
        return Err(DemuxError::Unsupported(format!(
            "{rate} Hz {channels}-channel PCM needs packets over {MAX_PACKET} bytes"
        )));
    }
    let len = input.len();
    let duration = if data.size > 0 && data.end <= len {
        Some(i64::try_from(data.size).unwrap_or(i64::MAX) * 8 / (i64::from(channels) * codec_bits))
    } else if format.byte_rate > 0 {
        let size = i64::try_from(len.saturating_sub(data.start)).unwrap_or(i64::MAX);
        Some(super::rescale(
            size,
            8 * rate,
            i64::from(format.byte_rate) * 8,
        ))
    } else {
        None
    };
    let audio = AudioTrack {
        codec: AudioCodec::Pcm(PcmFormat {
            bits: u16::try_from(container * 8).unwrap_or(u16::MAX),
            float,
            big_endian: false,
            signed: float || container > 1,
        }),
        codec_private: Bytes::new(),
        sample_rate: format.sample_rate,
        channels,
        speakers: format
            .mask
            .and_then(|mask| speakers_from_mask(u64::from(mask), channels)),
        bits_per_sample: u16::try_from(codec_bits).ok(),
        priming: 0,
    };
    let framer = WavFramer {
        first: data,
        packet: u64::try_from(packet).unwrap_or(u64::MAX),
        frame_bits: align * 8,
        state: WavState {
            pos: data.start,
            end: data.end,
            pts: 0,
        },
    };
    let facts = Facts {
        timescale: format.sample_rate,
        duration,
        language: None,
        title: None,
        audio,
    };
    Ok(Box::new(ScanReader::new(input, facts, framer)))
}

#[derive(Clone, Copy)]
struct WavState {
    pos: u64,
    end: u64,
    pts: i64,
}

struct WavFramer {
    first: Data,
    packet: u64,
    /// Bits of one sample frame.
    frame_bits: i64,
    state: WavState,
}

impl WavFramer {
    /// Finds the next `data` chunk from `pos`, as `FFmpeg` does when one ends.
    fn next_data(&mut self, input: &mut Input) -> Result<bool, DemuxError> {
        let mut pos = self.state.pos;
        loop {
            let header = input.peek(pos, 8)?;
            if header.len() < 8 {
                return Ok(false);
            }
            let size = u64::from(le32(header, 4));
            if &header[..4] == b"data" {
                self.state.pos = pos + 8;
                self.state.end = (pos + 8).saturating_add(size);
                return Ok(true);
            }
            pos = pos.saturating_add(8 + size + (size & 1));
        }
    }
}

impl Framer for WavFramer {
    type State = WavState;

    fn state(&self) -> WavState {
        self.state
    }

    fn restore(&mut self, state: WavState) {
        self.state = state;
    }

    fn next(&mut self, input: &mut Input, data: bool) -> Result<Option<Frame>, DemuxError> {
        if self.state.pos >= self.state.end && !self.next_data(input)? {
            return Ok(None);
        }
        let want = self.packet.min(self.state.end - self.state.pos);
        let available = input.len().saturating_sub(self.state.pos).min(want);
        if available == 0 {
            return Ok(None);
        }
        let size = usize::try_from(available).unwrap_or(usize::MAX);
        let bytes = if data {
            Bytes::copy_from_slice(input.exact(self.state.pos, size)?)
        } else {
            Bytes::new()
        };
        let duration = i64::try_from(available).unwrap_or(i64::MAX) * 8 / self.frame_bits;
        let frame = Frame {
            pts: self.state.pts,
            duration,
            data: bytes,
            discard: (0, 0),
        };
        self.state.pos += available;
        self.state.pts += duration;
        Ok(Some(frame))
    }

    fn hint(&mut self, input: &mut Input, pts: i64) -> Result<Option<(i64, WavState)>, DemuxError> {
        let full = i64::try_from(self.packet).unwrap_or(i64::MAX) * 8 / self.frame_bits;
        let end = self.first.end.min(input.len());
        if full <= 0 || end <= self.first.start {
            return Ok(None);
        }
        let last = (end - self.first.start - 1) / self.packet;
        let index = u64::try_from(pts.max(0) / full).unwrap_or(0).min(last);
        let pts = i64::try_from(index).unwrap_or(i64::MAX) * full;
        Ok(Some((
            pts,
            WavState {
                pos: self.first.start + index * self.packet,
                end: self.first.end,
                pts,
            },
        )))
    }
}
