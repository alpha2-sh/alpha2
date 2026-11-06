//! AAC in ADTS frames. Packets are the raw access units, without the ADTS header and CRC, so
//! they decode with the `AudioSpecificConfig` built from the first header as MP4 AAC does.

use bytes::Bytes;

use super::{BitWriter, Bits, Facts, Frame, Framer, Input, ScanReader, id3v2_len};
use crate::{AudioCodec, AudioTrack, ContainerReader, DemuxError};

/// The time base `FFmpeg` gives ADTS streams: the least common multiple of their sample rates.
const TIMESCALE: u32 = 28_224_000;
/// Samples per AAC frame, as `FFmpeg` times every ADTS packet.
const FRAME_SAMPLES: i64 = 1024;
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];
/// Speaker counts of the ADTS channel configurations 1 to 7.
const CHANNELS: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 8];

#[derive(Clone, Copy)]
struct Header {
    protected: bool,
    object_type: u32,
    rate_index: usize,
    channel_config: u32,
    size: u64,
    blocks: u32,
}

fn parse_header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < 7 || bytes[0] != 0xff || bytes[1] & 0xf0 != 0xf0 {
        return None;
    }
    let rate_index = usize::from((bytes[2] >> 2) & 0x0f);
    let size =
        (u64::from(bytes[3] & 0x03) << 11) | (u64::from(bytes[4]) << 3) | u64::from(bytes[5] >> 5);
    if rate_index >= SAMPLE_RATES.len() || size < 7 {
        return None;
    }
    Some(Header {
        protected: bytes[1] & 1 == 0,
        object_type: u32::from(bytes[2] >> 6) + 1,
        rate_index,
        channel_config: (u32::from(bytes[2] & 1) << 2) | u32::from(bytes[3] >> 6),
        size,
        blocks: u32::from(bytes[6] & 3) + 1,
    })
}

impl Header {
    fn header_len(self) -> u64 {
        if self.protected { 9 } else { 7 }
    }
}

/// Builds the `AudioSpecificConfig` of a stream whose first frame has `header` and `payload`.
fn audio_specific_config(header: Header, payload: &[u8]) -> Result<(Vec<u8>, u32), DemuxError> {
    let mut config = BitWriter::default();
    config.write(5, header.object_type);
    config.write(4, u32::try_from(header.rate_index).unwrap_or(0));
    config.write(4, header.channel_config);
    config.write(3, 0);
    if header.channel_config != 0 {
        return Ok((config.finish(), CHANNELS[header.channel_config as usize]));
    }
    let channels = copy_program_config(&mut Bits::new(payload), &mut config).ok_or_else(|| {
        DemuxError::Unsupported(
            "an ADTS stream without a channel configuration or a leading program config element"
                .into(),
        )
    })?;
    Ok((config.finish(), channels))
}

/// Copies the program config element that opens `payload` into `out`; returns its speaker count.
fn copy_program_config(payload: &mut Bits, out: &mut BitWriter) -> Option<u32> {
    fn copy(payload: &mut Bits, out: &mut BitWriter, bits: u32) -> Option<u32> {
        let value = payload.read(bits)?;
        out.write(bits, value);
        Some(value)
    }
    if payload.read(3)? != 5 {
        return None;
    }
    copy(payload, out, 4 + 2 + 4)?;
    let front = copy(payload, out, 4)?;
    let side = copy(payload, out, 4)?;
    let back = copy(payload, out, 4)?;
    let lfe = copy(payload, out, 2)?;
    let assoc = copy(payload, out, 3)?;
    let cc = copy(payload, out, 4)?;
    for mixdown in [4, 4, 3] {
        if copy(payload, out, 1)? == 1 {
            copy(payload, out, mixdown)?;
        }
    }
    let mut channels = lfe;
    for _ in 0..front + side + back {
        channels += if copy(payload, out, 1)? == 1 { 2 } else { 1 };
        copy(payload, out, 4)?;
    }
    for _ in 0..lfe + assoc {
        copy(payload, out, 4)?;
    }
    for _ in 0..cc {
        copy(payload, out, 5)?;
    }
    payload.align();
    out.align();
    let comment = copy(payload, out, 8)?;
    for _ in 0..comment {
        copy(payload, out, 8)?;
    }
    (channels > 0).then_some(channels)
}

pub(super) fn open(mut input: Input, start: u64) -> Result<Box<dyn ContainerReader>, DemuxError> {
    let mut framer = AdtsFramer {
        duration: 0,
        state: AdtsState {
            pos: start,
            frames: 0,
        },
    };
    let first = framer.locate(&mut input)?.ok_or_else(|| {
        DemuxError::Invalid("the ADTS stream has no complete frame header".into())
    })?;
    let header = parse_header(input.exact(first, 7)?)
        .ok_or_else(|| DemuxError::Invalid("invalid ADTS frame header".into()))?;
    let payload = input
        .peek(
            first + header.header_len(),
            usize::try_from(header.size).unwrap_or(0),
        )?
        .to_vec();
    let (config, channels) = audio_specific_config(header, &payload)?;
    let sample_rate = SAMPLE_RATES[header.rate_index];
    framer.duration = FRAME_SAMPLES * i64::from(TIMESCALE / sample_rate);
    framer.state.pos = first;
    let begin = framer.state;
    // Counting frames gives the duration and indexes every seek target in one pass.
    let mut index = Vec::new();
    while let Some(frame) = {
        let before = framer.state;
        framer.next(&mut input, false)?.map(|frame| (frame, before))
    } {
        let (frame, before) = frame;
        if before.frames % 64 == 0 {
            index.push((frame.pts, before));
        }
    }
    let count = framer.state.frames;
    framer.state = begin;
    let audio = AudioTrack {
        codec: AudioCodec::Aac,
        codec_private: Bytes::from(config),
        sample_rate,
        channels,
        speakers: None,
        bits_per_sample: None,
        priming: 0,
        seek_preroll: None,
    };
    let facts = Facts {
        timescale: TIMESCALE,
        duration: Some(count * framer.duration),
        language: None,
        title: None,
        audio,
    };
    Ok(Box::new(
        ScanReader::new(input, facts, framer).with_index(index),
    ))
}

#[derive(Clone, Copy)]
struct AdtsState {
    pos: u64,
    frames: i64,
}

struct AdtsFramer {
    duration: i64,
    state: AdtsState,
}

impl AdtsFramer {
    /// Moves to the next frame header, past `ID3v2` tags and junk as `FFmpeg`'s demuxer does.
    fn locate(&mut self, input: &mut Input) -> Result<Option<u64>, DemuxError> {
        loop {
            let pos = self.state.pos;
            let head = input.peek(pos, 10)?;
            if head.len() < 7 {
                return Ok(None);
            }
            if head[0] == 0xff && head[1] & 0xf0 == 0xf0 {
                if parse_header(head).is_some() {
                    return Ok(Some(pos));
                }
                self.state.pos += 1;
                continue;
            }
            if let Some(len) = id3v2_len(head) {
                self.state.pos += len;
                continue;
            }
            if head.len() < 10 {
                return Ok(None);
            }
            // FFmpeg has read 10 bytes when it gives up on a header and searches from there.
            match find_sync(input, pos + 10)? {
                Some(found) => self.state.pos = found,
                None => return Ok(None),
            }
        }
    }
}

/// The position of the next `0xFFF` sync word at or after `pos`.
fn find_sync(input: &mut Input, mut pos: u64) -> Result<Option<u64>, DemuxError> {
    loop {
        let bytes = input.peek(pos, 1 << 16)?;
        if bytes.len() < 2 {
            return Ok(None);
        }
        if let Some(offset) = bytes
            .windows(2)
            .position(|pair| pair[0] == 0xff && pair[1] & 0xf0 == 0xf0)
        {
            return Ok(Some(pos + offset as u64));
        }
        pos += bytes.len() as u64 - 1;
    }
}

impl Framer for AdtsFramer {
    type State = AdtsState;

    fn state(&self) -> AdtsState {
        self.state
    }

    fn restore(&mut self, state: AdtsState) {
        self.state = state;
    }

    fn next(&mut self, input: &mut Input, data: bool) -> Result<Option<Frame>, DemuxError> {
        let Some(pos) = self.locate(input)? else {
            return Ok(None);
        };
        let Some(header) = parse_header(input.exact(pos, 7)?) else {
            return Ok(None);
        };
        if header.protected && header.blocks > 1 {
            return Err(DemuxError::Unsupported(
                "ADTS frames with several CRC-protected raw data blocks".into(),
            ));
        }
        let end = (pos + header.size).min(input.len());
        let payload = (pos + header.header_len()).min(end);
        let bytes = if data {
            let count = usize::try_from(end - payload).unwrap_or(0);
            Bytes::copy_from_slice(input.exact(payload, count)?)
        } else {
            Bytes::new()
        };
        let frame = Frame {
            pts: self.state.frames * self.duration,
            duration: self.duration,
            data: bytes,
            discard: (0, 0),
        };
        self.state.pos = end;
        self.state.frames += 1;
        Ok(Some(frame))
    }
}
