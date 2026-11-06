//! Demuxers for audio-only files: WAV, MP3, ADTS AAC, native FLAC and Ogg.
//!
//! Each file holds one track. Packets, timestamps and skip-samples follow `FFmpeg`'s demuxer and
//! parser for the format, so a host on this layer times audio the way the `FFmpeg` hosts do.

use std::io::{Read, Seek, SeekFrom};

use alpha2_media::{Speaker, SpeakerLayout};
use bytes::Bytes;

use crate::{
    AudioTrack, ContainerFormat, ContainerReader, DemuxError, Packet, Source, TrackId, TrackInfo,
    TrackKind,
};

mod adts;
mod crc;
mod flac;
mod mpeg;
mod ogg;
mod wav;

#[cfg(test)]
mod tests;

/// The id of the one track of an audio-only file.
const TRACK: TrackId = TrackId(1);

/// Opens an audio-only file of `format`, which [`crate::sniff`] found in its first bytes.
pub(crate) fn open(
    format: ContainerFormat,
    source: Box<dyn Source>,
) -> Result<Box<dyn ContainerReader>, DemuxError> {
    let mut input = Input::new(source)?;
    match format {
        ContainerFormat::Wav => wav::open(input),
        ContainerFormat::Adts => adts::open(input, 0),
        ContainerFormat::Flac => flac::open(input, 0),
        ContainerFormat::Ogg => ogg::open(input),
        ContainerFormat::Mp3 => {
            // An ID3v2 tag may precede any raw stream, as FFmpeg reads it for every format.
            let start = skip_id3v2(&mut input, 0)?;
            let head = input.peek(start, 4096)?.to_vec();
            match crate::sniff(&head) {
                Ok(ContainerFormat::Adts) => adts::open(input, start),
                Ok(ContainerFormat::Flac) => flac::open(input, start),
                _ => mpeg::open(input, start),
            }
        }
        ContainerFormat::Mp4 | ContainerFormat::Matroska => Err(DemuxError::Unsupported(format!(
            "{format:?} is not an audio-only format"
        ))),
    }
}

/// Random access to a source through a read-ahead window.
pub(crate) struct Input {
    source: Box<dyn Source>,
    len: u64,
    window: Vec<u8>,
    start: u64,
}

/// Bytes read around a request.
const WINDOW: usize = 1 << 18;
/// Bytes kept before a request.
const BEHIND: u64 = 1 << 14;

impl Input {
    pub(crate) fn new(mut source: Box<dyn Source>) -> Result<Self, DemuxError> {
        let len = source.seek(SeekFrom::End(0))?;
        Ok(Self {
            source,
            len,
            window: Vec::new(),
            start: 0,
        })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Up to `count` bytes at `pos`; fewer at the end of the file.
    pub(crate) fn peek(&mut self, pos: u64, count: usize) -> Result<&[u8], DemuxError> {
        if pos >= self.len {
            return Ok(&[]);
        }
        let count = count.min(usize::try_from(self.len - pos).unwrap_or(usize::MAX));
        let end = pos + count as u64;
        if pos < self.start || end > self.start + self.window.len() as u64 {
            // Keep a little before `pos`, as scans step back to the frame they started from.
            let start = pos.saturating_sub(BEHIND);
            let behind = usize::try_from(pos - start).unwrap_or(0);
            let size = (behind + count)
                .max(WINDOW)
                .min(usize::try_from(self.len - start).unwrap_or(usize::MAX));
            self.window.clear();
            self.window.resize(size, 0);
            self.source.seek(SeekFrom::Start(start))?;
            let mut filled = 0;
            while filled < size {
                match self.source.read(&mut self.window[filled..]) {
                    Ok(0) => break,
                    Ok(read) => filled += read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.window.truncate(filled);
            self.start = start;
        }
        let offset = usize::try_from(pos - self.start)
            .unwrap_or(usize::MAX)
            .min(self.window.len());
        let available = self.window.len().saturating_sub(offset).min(count);
        Ok(&self.window[offset..offset + available])
    }

    /// Exactly `count` bytes at `pos`.
    pub(crate) fn exact(&mut self, pos: u64, count: usize) -> Result<&[u8], DemuxError> {
        let bytes = self.peek(pos, count)?;
        if bytes.len() < count {
            return Err(DemuxError::Invalid(format!(
                "the file ends inside a {count}-byte structure at byte {pos}"
            )));
        }
        Ok(bytes)
    }
}

/// The size of an `ID3v2` tag starting with `head`, header and footer included.
pub(crate) fn id3v2_len(head: &[u8]) -> Option<u64> {
    if head.len() < 10
        || &head[..3] != b"ID3"
        || head[3] == 0xff
        || head[4] == 0xff
        || head[6..10].iter().any(|byte| byte & 0x80 != 0)
    {
        return None;
    }
    let size = head[6..10]
        .iter()
        .fold(0_u64, |size, byte| (size << 7) | u64::from(*byte));
    let footer = if head[5] & 0x10 == 0 { 0 } else { 10 };
    Some(10 + size + footer)
}

/// The position after the `ID3v2` tags that start at `pos`.
pub(crate) fn skip_id3v2(input: &mut Input, mut pos: u64) -> Result<u64, DemuxError> {
    while let Some(len) = id3v2_len(input.peek(pos, 10)?) {
        pos += len;
    }
    Ok(pos)
}

/// `a * b / c` rounded to nearest, halves away from zero (`av_rescale`).
pub(crate) fn rescale(a: i64, b: i64, c: i64) -> i64 {
    let product = i128::from(a) * i128::from(b);
    let c = i128::from(c.max(1));
    let rounded = if product < 0 {
        -((-product + c / 2) / c)
    } else {
        (product + c / 2) / c
    };
    i64::try_from(rounded).unwrap_or(if rounded < 0 { i64::MIN } else { i64::MAX })
}

/// `a * b / c` rounded down.
pub(crate) fn rescale_down(a: i64, b: i64, c: i64) -> i64 {
    let rounded = (i128::from(a) * i128::from(b)).div_euclid(i128::from(c.max(1)));
    i64::try_from(rounded).unwrap_or(if rounded < 0 { i64::MIN } else { i64::MAX })
}

/// The speakers a WAVE channel mask names, in bit order, when it names `channels` of them.
pub(crate) fn speakers_from_mask(mask: u64, channels: u32) -> Option<SpeakerLayout> {
    if mask.count_ones() != channels {
        return None;
    }
    let speakers = (0..64)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(Speaker::from_ffmpeg_channel)
        .collect::<Vec<_>>();
    SpeakerLayout::new(&speakers)
}

/// The speakers of Vorbis's channel order (also Opus mapping family 1) for 1 to 8 channels.
pub(crate) fn vorbis_order(channels: u32) -> Option<SpeakerLayout> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency,
        SideLeft, SideRight,
    };
    let speakers: &[Speaker] = match channels {
        1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontCenter, FrontRight],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontCenter, FrontRight, BackLeft, BackRight],
        6 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            BackLeft,
            BackRight,
            LowFrequency,
        ],
        7 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            SideLeft,
            SideRight,
            BackCenter,
            LowFrequency,
        ],
        8 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            SideLeft,
            SideRight,
            BackLeft,
            BackRight,
            LowFrequency,
        ],
        _ => return None,
    };
    SpeakerLayout::new(speakers)
}

/// Facts of the one track of an audio-only file.
pub(crate) struct Facts {
    pub(crate) timescale: u32,
    pub(crate) duration: Option<i64>,
    pub(crate) language: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) audio: AudioTrack,
}

impl Facts {
    fn track(self) -> TrackInfo {
        TrackInfo {
            id: TRACK,
            kind: TrackKind::Audio,
            timescale: self.timescale,
            duration: self.duration,
            language: self.language,
            title: self.title,
            default: false,
            enabled: true,
            video: None,
            audio: Some(self.audio),
        }
    }
}

/// A packet before it is tied to the track.
pub(crate) struct Frame {
    pub(crate) pts: i64,
    pub(crate) duration: i64,
    pub(crate) data: Bytes,
    pub(crate) discard: (u32, u32),
}

/// Splits a file into packets from a restorable state.
pub(crate) trait Framer: Send + 'static {
    /// Everything [`Framer::next`] depends on, so a seek can resume anywhere a scan has been.
    type State: Clone + Send;

    fn state(&self) -> Self::State;

    fn restore(&mut self, state: Self::State);

    /// The next packet in decode order. Its data may be left empty when `data` is false.
    fn next(&mut self, input: &mut Input, data: bool) -> Result<Option<Frame>, DemuxError>;

    /// A state at a packet starting at or before `pts` and its `pts`, when the file has an index.
    fn hint(
        &mut self,
        _input: &mut Input,
        _pts: i64,
    ) -> Result<Option<(i64, Self::State)>, DemuxError> {
        Ok(None)
    }
}

/// Packets between two entries of a reader's seek index.
const INDEX_STRIDE: u32 = 32;

/// A reader over a [`Framer`] that indexes the packets it passes so later seeks resume nearby.
pub(crate) struct ScanReader<F: Framer> {
    input: Input,
    tracks: [TrackInfo; 1],
    framer: F,
    start: F::State,
    index: Vec<(i64, F::State)>,
    since_index: u32,
}

impl<F: Framer> ScanReader<F> {
    pub(crate) fn new(input: Input, facts: Facts, framer: F) -> Self {
        let start = framer.state();
        Self {
            input,
            tracks: [facts.track()],
            framer,
            start,
            index: Vec::new(),
            since_index: 0,
        }
    }

    /// Seeds the seek index with `(pts, state)` pairs in rising `pts` order.
    pub(crate) fn with_index(mut self, index: Vec<(i64, F::State)>) -> Self {
        self.index = index;
        self
    }

    fn next_frame(&mut self, data: bool) -> Result<Option<Frame>, DemuxError> {
        let before = self.framer.state();
        let frame = self.framer.next(&mut self.input, data)?;
        if let Some(frame) = &frame {
            self.since_index += 1;
            let later = self.index.last().is_none_or(|(pts, _)| frame.pts > *pts);
            if self.since_index >= INDEX_STRIDE && later {
                self.index.push((frame.pts, before));
                self.since_index = 0;
            }
        }
        Ok(frame)
    }
}

impl<F: Framer> ContainerReader for ScanReader<F> {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(&mut self, track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError> {
        if track != TRACK {
            return Err(DemuxError::NoSuchTrack(track.0));
        }
        let entry = self.index.partition_point(|(entry, _)| *entry <= pts);
        let mut from = match entry.checked_sub(1) {
            Some(entry) => Some(self.index[entry].clone()),
            None => None,
        };
        // Within the indexed stretch an entry lies at most a stride before the target.
        let explored = entry < self.index.len();
        if !explored
            && let Some((hinted, state)) = self.framer.hint(&mut self.input, pts)?
            && hinted <= pts
            && from.as_ref().is_none_or(|(indexed, _)| hinted > *indexed)
        {
            from = Some((hinted, state));
        }
        self.framer
            .restore(from.map_or_else(|| self.start.clone(), |(_, state)| state));
        self.since_index = 0;
        let mut best: Option<(i64, F::State)> = None;
        loop {
            let before = self.framer.state();
            let Some(frame) = self.next_frame(false)? else {
                break;
            };
            if frame.pts > pts && best.is_some() {
                break;
            }
            best = Some((frame.pts, before));
            if frame.pts > pts {
                break;
            }
        }
        Ok(best.map(|(found, state)| {
            self.framer.restore(state);
            found
        }))
    }

    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError> {
        if track != TRACK {
            return Err(DemuxError::NoSuchTrack(track.0));
        }
        Ok(self.next_frame(true)?.map(|frame| Packet {
            track: TRACK,
            pts: frame.pts,
            dts: frame.pts,
            duration: frame.duration,
            keyframe: true,
            data: frame.data,
            discard: frame.discard,
        }))
    }
}

/// Reads big-endian bit fields, MSB first.
pub(crate) struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The next `count` bits (at most 32), or `None` past the end.
    pub(crate) fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..count {
            let byte = *self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(value)
    }

    pub(crate) fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    pub(crate) fn skip(&mut self, count: usize) {
        self.pos += count;
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }
}

/// Writes big-endian bit fields, MSB first.
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub(crate) fn write(&mut self, count: u32, value: u32) {
        for bit in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> bit) & 1 != 0 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    pub(crate) fn align(&mut self) {
        self.bits = self.bytes.len() * 8;
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Opens a file held in memory.
#[cfg(test)]
pub(crate) fn open_bytes(data: Vec<u8>) -> Result<Box<dyn ContainerReader>, DemuxError> {
    crate::open_source(Box::new(std::io::Cursor::new(data)))
}
