//! Ogg files of Opus, Vorbis or FLAC. Pages, packets and granule positions become timestamps
//! the way `FFmpeg`'s ogg demuxer and its `oggparseopus`/`oggparsevorbis` hooks make them.
//!
//! The first logical stream of a supported codec is read; other multiplexed streams are skipped
//! and a chained stream ends the track.

use bytes::Bytes;

use super::crc::crc32;
use super::flac::{
    CHANNEL_MASKS, codec_private as flac_private, comment_channel_mask, frame_header, stream_info,
    vorbis_comments,
};
use super::{Bits, Facts, Frame, Framer, Input, ScanReader, speakers_from_mask, vorbis_order};
use crate::{AudioCodec, AudioTrack, ContainerReader, DemuxError};

const FLAG_CONTINUED: u8 = 1;
const FLAG_BOS: u8 = 2;
const FLAG_EOS: u8 = 4;
/// The largest page: header, 255 lacing values and 255 full segments.
const MAX_PAGE: u64 = 65_307;
/// The largest packet this reader assembles.
const MAX_PACKET: usize = 1 << 25;
/// The largest `codec_private` this reader assembles from header packets.
const MAX_HEADERS: usize = 1 << 24;

/// One Ogg page whose CRC holds.
#[derive(Clone)]
struct Page {
    pos: u64,
    flags: u8,
    granule: i64,
    serial: u32,
    segments: Vec<u8>,
    body: u64,
    next: u64,
}

impl Page {
    fn body_len(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| usize::from(*segment))
            .sum()
    }
}

/// Reads the page at or after `pos`, searching for the capture pattern and skipping pages whose
/// CRC or version is wrong, as `ogg_read_page` does.
fn read_page(input: &mut Input, mut pos: u64) -> Result<Option<Page>, DemuxError> {
    loop {
        if input.peek(pos, 4)? != b"OggS" {
            let window = input.peek(pos, usize::try_from(MAX_PAGE).unwrap_or(0) + 4)?;
            let Some(offset) = window.windows(4).position(|bytes| bytes == b"OggS") else {
                return Ok(None);
            };
            pos += offset as u64;
        }
        let header = input.peek(pos, 27)?;
        if header.len() < 27 {
            return Ok(None);
        }
        let header = header.to_vec();
        let count = usize::from(header[26]);
        let segments = input.peek(pos + 27, count)?.to_vec();
        if segments.len() < count {
            return Ok(None);
        }
        let body_len: usize = segments.iter().map(|segment| usize::from(*segment)).sum();
        let body = pos + 27 + count as u64;
        let bytes = input.peek(body, body_len)?;
        if bytes.len() < body_len {
            return Ok(None);
        }
        let mut crc = crc32(0, &header[..22]);
        crc = crc32(crc, &[0; 4]);
        crc = crc32(crc, &header[26..27]);
        crc = crc32(crc, &segments);
        crc = crc32(crc, bytes);
        let stored = u32::from_le_bytes([header[22], header[23], header[24], header[25]]);
        if crc != stored || header[4] != 0 {
            pos += 4;
            continue;
        }
        let granule = i64::from_le_bytes(header[6..14].try_into().unwrap_or([0; 8]));
        return Ok(Some(Page {
            pos,
            flags: header[5],
            granule,
            serial: u32::from_le_bytes([header[14], header[15], header[16], header[17]]),
            segments,
            body,
            next: body + body_len as u64,
        }));
    }
}

/// What a Vorbis setup header tells about packet sizes, as `FFmpeg`'s vorbis parser reads it.
#[derive(Clone, Debug)]
pub(crate) struct VorbisModes {
    pub(crate) blocksizes: [i64; 2],
    pub(crate) long: Vec<bool>,
    mode_mask: u8,
    prev_mask: u8,
}

impl VorbisModes {
    pub(crate) fn parse(identification: &[u8], setup: &[u8]) -> Option<Self> {
        if identification.len() < 30 || setup.len() < 7 || setup[0] != 5 {
            return None;
        }
        let blocksizes = [
            1_i64 << (identification[28] & 0x0f),
            1_i64 << (identification[28] >> 4),
        ];
        let reversed = setup.iter().rev().copied().collect::<Vec<_>>();
        let total = reversed.len() * 8;
        let mut bits = Bits::new(&reversed);
        let mut framing = None;
        while total - bits.position() > 97 {
            if bits.read(1)? == 1 {
                framing = Some(bits.position());
                break;
            }
        }
        let framing = framing?;
        let mut count = 0_u32;
        let mut last = 0_u32;
        while total - bits.position() >= 97 {
            if bits.read(8)? > 63 || bits.read(16)? != 0 || bits.read(16)? != 0 {
                break;
            }
            bits.read(1)?;
            count += 1;
            if count > 64 {
                break;
            }
            let mut ahead = Bits::new(&reversed);
            ahead.skip(bits.position());
            if ahead.read(6)? + 1 == count {
                last = count;
            }
        }
        if last == 0 || last > 63 {
            return None;
        }
        let mut bits = Bits::new(&reversed);
        bits.skip(framing);
        let mut long = vec![false; last as usize];
        for mode in (0..last as usize).rev() {
            bits.skip(40);
            long[mode] = bits.read(1)? == 1;
        }
        let mode_mask = u8::try_from(((1_u32 << ((last - 1).max(1).ilog2() + 1)) - 1) << 1).ok()?;
        Some(Self {
            blocksizes,
            long,
            mode_mask,
            prev_mask: (mode_mask | 1) + 1,
        })
    }

    /// The samples a packet whose first byte is `first` adds after a block of `previous` samples;
    /// header packets add none. Updates `previous`.
    pub(crate) fn duration(&self, first: u8, previous: &mut i64) -> Option<i64> {
        if first & 1 != 0 {
            return Some(0);
        }
        let mode = if self.long.len() == 1 {
            0
        } else {
            usize::from((first & self.mode_mask) >> 1)
        };
        let long = *self.long.get(mode)?;
        let mut before = *previous;
        if long {
            before = self.blocksizes[usize::from(first & self.prev_mask != 0)];
        }
        let current = self.blocksizes[usize::from(long)];
        *previous = current;
        Some((before + current) >> 2)
    }
}

/// The samples an Opus packet holds, from its TOC byte (`opus_duration`).
pub(crate) fn opus_duration(packet: &[u8]) -> Option<i64> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => (960 * i64::from(config & 3)).max(480),
        12..=15 => 480 << (config & 1),
        _ => 120 << (config & 3),
    };
    let frames = match toc & 3 {
        0 => 1,
        3 => i64::from(*packet.get(1)? & 0x3f),
        _ => 2,
    };
    Some(frame * frames)
}

#[derive(Clone)]
enum Codec {
    Vorbis(VorbisModes),
    Opus { pre_skip: i64 },
    Flac,
}

/// The demuxer state at a packet boundary. It holds the page of the next unread segment and the
/// fields of `FFmpeg`'s `ogg_stream` and codec hooks that time packets.
#[derive(Clone)]
struct OggState {
    page: u64,
    segment: usize,
    flags: u8,
    granule: i64,
    last_pts: Option<i64>,
    last_dts: Option<i64>,
    start_trimming: u32,
    next_pts: i64,
    ended: bool,
    vorbis_previous: i64,
    final_pts: Option<i64>,
    final_duration: i64,
    opus_dts: i64,
    flac_duration: i64,
}

/// One packet as the page layer hands it to a codec hook.
struct RawPacket {
    data: Vec<u8>,
}

struct OggFramer {
    serial: u32,
    codec: Codec,
    /// Where the first data packet's page starts.
    data_page: u64,
    loaded: Option<Page>,
    state: OggState,
}

/// Bytes below which a bisection stops and a scan takes over.
const BISECT_SPAN: u64 = 1 << 12;

impl OggFramer {
    fn page(&mut self, input: &mut Input) -> Result<Option<Page>, DemuxError> {
        if self
            .loaded
            .as_ref()
            .is_none_or(|page| page.pos != self.state.page)
        {
            self.loaded = read_page(input, self.state.page)?;
        }
        Ok(self.loaded.clone())
    }

    /// The next page of this stream after `page`, or `None` at its end.
    fn next_page(&mut self, input: &mut Input, after: u64) -> Result<Option<Page>, DemuxError> {
        let mut pos = after;
        loop {
            let Some(page) = read_page(input, pos)? else {
                return Ok(None);
            };
            if page.serial == self.serial {
                // A second BOS page of the serial starts a chained stream.
                return Ok((page.flags & FLAG_BOS == 0).then_some(page));
            }
            pos = page.next;
        }
    }

    /// Assembles the next packet of the stream across pages (`ogg_packet`).
    fn read_raw(&mut self, input: &mut Input) -> Result<Option<(RawPacket, Page)>, DemuxError> {
        if self.state.ended {
            return Ok(None);
        }
        let mut data = Vec::new();

        let Some(mut page) = self.page(input)? else {
            self.state.ended = true;
            return Ok(None);
        };
        let mut offset: usize = page.segments[..self.state.segment.min(page.segments.len())]
            .iter()
            .map(|segment| usize::from(*segment))
            .sum();
        loop {
            while self.state.segment < page.segments.len() {
                let size = usize::from(page.segments[self.state.segment]);
                self.state.segment += 1;
                if data.len() + size > MAX_PACKET {
                    return Err(DemuxError::Invalid("an Ogg packet is too large".into()));
                }
                data.extend_from_slice(input.exact(page.body + offset as u64, size)?);
                offset += size;

                if size < 255 {
                    return Ok(Some((RawPacket { data }, page)));
                }
            }
            let Some(next) = self.next_page(input, page.next)? else {
                self.state.ended = true;
                return Ok(None);
            };
            page = next;
            self.loaded = Some(page.clone());
            self.state.page = page.pos;
            self.state.segment = 0;
            self.state.flags = page.flags;
            self.state.granule = page.granule;
            offset = 0;
            if page.flags & FLAG_CONTINUED != 0 && data.is_empty() {
                // A packet continued from a page not read: drop its tail.
                while self.state.segment < page.segments.len() {
                    let size = usize::from(page.segments[self.state.segment]);
                    self.state.segment += 1;
                    offset += size;
                    if size < 255 {
                        break;
                    }
                }
            }
        }
    }

    /// Whether no complete packet follows the current one on its page (`page_end`).
    fn page_end(&self, page: &Page) -> bool {
        page.segments[self.state.segment.min(page.segments.len())..]
            .iter()
            .all(|segment| *segment == 255)
    }

    /// The complete packets after the current one on `page`.
    fn following(&self, input: &mut Input, page: &Page) -> Result<Vec<Vec<u8>>, DemuxError> {
        let body = input.peek(page.body, page.body_len())?.to_vec();
        let mut start: usize = page.segments[..self.state.segment.min(page.segments.len())]
            .iter()
            .map(|segment| usize::from(*segment))
            .sum();
        let mut end = start;
        let mut packets = Vec::new();
        for segment in &page.segments[self.state.segment.min(page.segments.len())..] {
            end += usize::from(*segment);
            if *segment < 255 {
                packets.push(body.get(start..end.min(body.len())).unwrap_or(&[]).to_vec());
                start = end;
            }
        }
        Ok(packets)
    }

    /// Times a Vorbis packet as `vorbis_packet` does. Returns its duration and end trimming.
    fn vorbis(
        &mut self,
        modes: &VorbisModes,
        input: &mut Input,
        page: &Page,
        data: &[u8],
    ) -> Result<(i64, u32), DemuxError> {
        let state = &self.state;
        let first_byte = |packet: &[u8], fallback: u8| packet.first().copied().unwrap_or(fallback);
        if matches!(state.last_pts, None | Some(0))
            && state.flags & FLAG_EOS == 0
            && state.granule >= 0
        {
            let following = self.following(input, page)?;
            let fallback = following.first().and_then(|packet| packet.first()).copied();
            let mut previous = modes.blocksizes[0];
            let Some(mut duration) =
                modes.duration(first_byte(data, fallback.unwrap_or(0)), &mut previous)
            else {
                return Ok((0, 0));
            };
            for (index, packet) in following.iter().enumerate() {
                let next = following
                    .get(index + 1)
                    .and_then(|packet| packet.first())
                    .copied()
                    .unwrap_or(0);
                let Some(samples) = modes.duration(first_byte(packet, next), &mut previous) else {
                    duration = self.state.granule;
                    break;
                };
                duration += samples;
            }
            let state = &mut self.state;
            let start = state.granule - duration;
            let start = (state.granule != 0 || duration == 0).then_some(start);
            state.last_pts = start;
            state.last_dts = start;
            state.final_pts = None;
            state.vorbis_previous = modes.blocksizes[0];
        }
        let state = &mut self.state;
        let mut duration = 0;
        if !data.is_empty() {
            match modes.duration(data[0], &mut state.vorbis_previous) {
                Some(samples) => duration = samples,
                None => return Ok((0, 0)),
            }
        }
        let mut trimming = 0;
        if state.flags & FLAG_EOS != 0 {
            if let Some(pts) = state.last_pts {
                state.final_pts = Some(pts);
                state.final_duration = 0;
            }
            if self.state.segment >= page.segments.len()
                && let Some(final_pts) = self.state.final_pts
            {
                let state = &mut self.state;
                let skip = final_pts + state.final_duration + duration - state.granule;
                if skip > 0 {
                    trimming = u32::try_from(skip).unwrap_or(u32::MAX);
                }
                duration = (state.granule - final_pts - state.final_duration).max(0);
            }
            self.state.final_duration += duration;
        }
        Ok((duration, trimming))
    }

    /// Times an Opus packet as `opus_packet` does. Returns its duration and end trimming.
    fn opus(
        &mut self,
        pre_skip: i64,
        input: &mut Input,
        page: &Page,
        data: &[u8],
    ) -> Result<(i64, u32), DemuxError> {
        if data.is_empty() {
            return Err(DemuxError::Invalid("an empty Opus packet".into()));
        }
        if self.state.granule < 0 || self.state.granule > 1 << 62 {
            return Err(DemuxError::Invalid(
                "an Ogg Opus granule position out of range".into(),
            ));
        }
        if matches!(self.state.last_pts, None | Some(0)) && self.state.flags & FLAG_EOS == 0 {
            let Some(mut duration) = opus_duration(data) else {
                return Ok((0, 0));
            };
            for packet in self.following(input, page)? {
                if let Some(samples) = opus_duration(&packet).filter(|samples| *samples > 0) {
                    duration += samples;
                }
            }
            self.state.last_pts = Some(self.state.granule - duration);
            self.state.last_dts = self.state.last_pts;
        }
        let mut duration = opus_duration(data)
            .ok_or_else(|| DemuxError::Invalid("a truncated Opus packet".into()))?;
        let state = &mut self.state;
        if let Some(pts) = state.last_pts {
            let shifted = pts - pre_skip;
            state.last_pts = Some(shifted);
            state.last_dts = Some(shifted);
            state.opus_dts = shifted;
        }
        state.opus_dts += duration;
        let mut trimming = 0;
        if state.flags & FLAG_EOS != 0 {
            let skip = (state.opus_dts - state.granule + pre_skip).min(duration);
            if skip > 0 {
                duration = if skip < duration { duration - skip } else { 1 };
                trimming = u32::try_from(skip).unwrap_or(u32::MAX);
            }
        }
        Ok((duration, trimming))
    }
}

impl OggFramer {
    /// The time `granule` ends at: the timestamp of the packet after the one it closes.
    fn granule_time(&self, granule: i64) -> i64 {
        match self.codec {
            Codec::Opus { pre_skip } => granule - pre_skip,
            _ => granule,
        }
    }

    /// The first page of the stream in `from..limit` that ends a packet.
    fn granule_page(
        &mut self,
        input: &mut Input,
        from: u64,
        limit: u64,
    ) -> Result<Option<Page>, DemuxError> {
        let mut pos = from;
        while let Some(page) = read_page(input, pos)? {
            if page.pos >= limit {
                break;
            }
            if page.serial == self.serial && page.granule != -1 {
                return Ok(Some(page));
            }
            pos = page.next;
        }
        Ok(None)
    }

    /// The state that resumes reading on the page after `last`, a page ending a packet. The next
    /// packet completed is timed by `last`'s granule position.
    fn state_after(
        &mut self,
        input: &mut Input,
        last: &Page,
    ) -> Result<Option<OggState>, DemuxError> {
        let Some(next) = self.next_page(input, last.next)? else {
            return Ok(None);
        };
        if next.flags & FLAG_CONTINUED != 0 {
            return Ok(None);
        }
        // The last packet of `last` must start there to give its block size or frame header.
        let mut start = 0;
        let mut end = 0;
        let mut began = last.flags & FLAG_CONTINUED == 0;
        let mut last_start = None;
        for segment in &last.segments {
            end += usize::from(*segment);
            if *segment < 255 {
                if began {
                    last_start = Some(start);
                }
                began = true;
                start = end;
            }
        }
        let Some(last_start) = last_start else {
            return Ok(None);
        };
        let packet = input
            .peek(last.body + last_start as u64, (start - last_start).min(32))?
            .to_vec();
        let mut vorbis_previous = 0;
        let mut flac_duration = 0;
        match &self.codec {
            Codec::Vorbis(modes) => {
                let Some(first) = packet.first() else {
                    return Ok(None);
                };
                if modes.duration(*first, &mut vorbis_previous).is_none() {
                    return Ok(None);
                }
            }
            Codec::Flac => {
                flac_duration =
                    frame_header(&packet).map_or(0, |header| i64::from(header.blocksize));
            }
            Codec::Opus { .. } => {}
        }
        Ok(Some(OggState {
            page: next.pos,
            segment: 0,
            flags: next.flags,
            granule: next.granule,
            last_pts: Some(last.granule),
            last_dts: Some(last.granule),
            start_trimming: 0,
            next_pts: last.granule,
            ended: false,
            vorbis_previous,
            final_pts: None,
            final_duration: 0,
            opus_dts: 0,
            flac_duration,
        }))
    }
}

impl Framer for OggFramer {
    type State = OggState;

    fn state(&self) -> OggState {
        self.state.clone()
    }

    fn restore(&mut self, state: OggState) {
        self.state = state;
    }

    fn hint(&mut self, input: &mut Input, pts: i64) -> Result<Option<(i64, OggState)>, DemuxError> {
        let mut low = self.data_page;
        let mut high = input.len();
        let mut best = None;
        while high.saturating_sub(low) > BISECT_SPAN {
            let middle = low + (high - low) / 2;
            let Some(page) = self.granule_page(input, middle, high)? else {
                high = middle;
                continue;
            };
            if self.granule_time(page.granule) <= pts {
                let closed = page.segments.last().is_some_and(|segment| *segment < 255);
                low = page.next;
                if closed && page.flags & FLAG_EOS == 0 {
                    best = Some(page);
                }
            } else {
                high = middle;
            }
        }
        let Some(last) = best else {
            return Ok(None);
        };
        let time = self.granule_time(last.granule);
        Ok(self.state_after(input, &last)?.map(|state| (time, state)))
    }

    fn next(&mut self, input: &mut Input, _data: bool) -> Result<Option<Frame>, DemuxError> {
        let Some((raw, page)) = self.read_raw(input)? else {
            return Ok(None);
        };
        let (duration, end_trimming) = match self.codec.clone() {
            Codec::Vorbis(modes) => self.vorbis(&modes, input, &page, &raw.data)?,
            Codec::Opus { pre_skip } => self.opus(pre_skip, input, &page, &raw.data)?,
            Codec::Flac => {
                if let Some(header) = frame_header(&raw.data) {
                    self.state.flac_duration = i64::from(header.blocksize);
                }
                (self.state.flac_duration, 0)
            }
        };
        let state = &mut self.state;
        let mut pts = state.last_pts.take();
        let dts = state.last_dts.take();
        if self.page_end(&page) && self.state.granule != -1 {
            let state = &mut self.state;
            state.last_pts = Some(state.granule);
            state.last_dts = Some(state.granule);
            state.granule = -1;
        }
        let state = &mut self.state;
        pts = pts.or(dts);
        let pts = pts.unwrap_or(state.next_pts);
        state.next_pts = pts + duration;
        let discard = (std::mem::take(&mut state.start_trimming), end_trimming);
        Ok(Some(Frame {
            pts,
            duration,
            data: Bytes::from(raw.data),
            discard,
        }))
    }
}

/// The stream tags `FFmpeg` reads from a Vorbis comment block, as `(title, language)`.
fn tags(block: &[u8]) -> (Option<String>, Option<String>) {
    let mut title: Option<String> = None;
    let mut language: Option<String> = None;
    for comment in vorbis_comments(block) {
        let Some((key, value)) = comment.split_once('=') else {
            continue;
        };
        let value = value.split('\0').next().unwrap_or_default();
        if key.is_empty() || value.is_empty() {
            continue;
        }
        let slot = if key.eq_ignore_ascii_case("title") {
            &mut title
        } else if key.eq_ignore_ascii_case("language") {
            &mut language
        } else {
            continue;
        };
        match slot {
            Some(joined) => {
                joined.push(';');
                joined.push_str(value);
            }
            None => *slot = Some(value.to_owned()),
        }
    }
    (title, language.filter(|language| language != "und"))
}

/// Xiph lacing of `len`, as `av_xiphlacing` writes it.
fn xiph_lacing(out: &mut Vec<u8>, mut len: usize) {
    while len >= 255 {
        out.push(255);
        len -= 255;
    }
    out.push(u8::try_from(len).unwrap_or(0));
}

/// The comment header as `FFmpeg` keeps it in `extradata`: the vendor string and no comments.
fn strip_comments(comment: &[u8]) -> Vec<u8> {
    if comment.len() > 10 && comment.len() > 8 {
        let body = &comment[7..comment.len() - 1];
        let vendor = u32::from_le_bytes([comment[7], comment[8], comment[9], comment[10]]);
        let parses = body.len() >= 8 && u64::from(vendor) <= (body.len() - 4) as u64;
        let kept = 7 + 4 + u64::from(vendor) + 4 + 1;
        if parses && kept >= 16 && kept < comment.len() as u64 {
            let kept = usize::try_from(kept).unwrap_or(0);
            let mut stripped = comment[..kept].to_vec();
            stripped[kept - 5..kept - 1].fill(0);
            stripped[kept - 1] = 1;
            return stripped;
        }
    }
    comment.to_vec()
}

/// What the header packets of the chosen stream say.
struct Headers {
    codec: Codec,
    audio: AudioTrack,
    timescale: u32,
    title: Option<String>,
    language: Option<String>,
}

pub(super) fn open(mut input: Input) -> Result<Box<dyn ContainerReader>, DemuxError> {
    let first = first_stream(&mut input)?;
    let mut framer = OggFramer {
        serial: first.serial,
        codec: Codec::Flac,
        data_page: 0,
        loaded: None,
        state: OggState {
            page: first.pos,
            segment: 0,
            flags: first.flags,
            granule: first.granule,
            last_pts: Some(0),
            last_dts: Some(0),
            start_trimming: 0,
            next_pts: 0,
            ended: false,
            vorbis_previous: 0,
            final_pts: None,
            final_duration: 0,
            opus_dts: 0,
            flac_duration: 0,
        },
    };
    let packets = header_packets(&mut framer, &mut input)?;
    let data_start = framer.state.clone();
    framer.data_page = data_start.page;
    let headers = parse_headers(&packets)?;
    framer.codec = headers.codec.clone();
    if let Codec::Opus { pre_skip } = headers.codec {
        framer.state.start_trimming = u32::try_from(pre_skip).unwrap_or(0);
    }
    if let Codec::Vorbis(modes) = &headers.codec {
        framer.state.vorbis_previous = modes.blocksizes[usize::from(modes.long[0])];
    }
    // FFmpeg reads the first data packet once more while measuring the stream; codec state that
    // pass leaves behind shapes the first real packet.
    let real = framer.state.clone();
    let start = rehearse(&mut framer, &mut input, &data_start)?;
    let OggState {
        vorbis_previous,
        final_pts,
        final_duration,
        opus_dts,
        ..
    } = framer.state.clone();
    framer.state = OggState {
        vorbis_previous,
        final_pts,
        final_duration,
        opus_dts,
        ..real
    };
    framer.loaded = None;
    let duration = last_granule(&mut input, framer.serial)?.map(|granule| match headers.codec {
        Codec::Vorbis(_) => granule - start,
        _ => granule,
    });
    let facts = Facts {
        timescale: headers.timescale,
        duration,
        language: headers.language,
        title: headers.title,
        audio: headers.audio,
    };
    Ok(Box::new(ScanReader::new(input, facts, framer)))
}

/// The BOS page of the first stream whose first packet names a supported codec.
fn first_stream(input: &mut Input) -> Result<Page, DemuxError> {
    let mut pos = 0;
    loop {
        let Some(page) = read_page(input, pos)? else {
            return Err(DemuxError::Invalid(
                "no Ogg stream of a supported codec".into(),
            ));
        };
        if page.flags & FLAG_BOS == 0 {
            return Err(DemuxError::Unsupported(
                "no Ogg Opus, Vorbis or FLAC stream".into(),
            ));
        }
        let head = input.peek(page.body, 8)?;
        if head.starts_with(b"\x01vorbis")
            || head.starts_with(b"OpusHead")
            || head.starts_with(b"\x7fFLAC")
        {
            return Ok(page);
        }
        pos = page.next;
    }
}

/// Reads the stream's header packets and leaves `framer` at its first data packet.
fn header_packets(framer: &mut OggFramer, input: &mut Input) -> Result<Vec<Vec<u8>>, DemuxError> {
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut total = 0_usize;
    loop {
        let before = framer.state.clone();
        let Some((raw, _)) = framer.read_raw(input)? else {
            return Err(DemuxError::Invalid(
                "the Ogg stream ends in its headers".into(),
            ));
        };
        let first_byte = raw.data.first().copied().unwrap_or(0);
        let is_header = match packets.first().map(|packet| packet.first().copied()) {
            None => true,
            Some(Some(1)) => first_byte & 1 == 1 && packets.len() < 3,
            Some(Some(b'O')) => packets.len() < 2,
            _ => first_byte != 0xff,
        };
        if !is_header {
            framer.state = before;
            return Ok(packets);
        }
        total += raw.data.len();
        if total > MAX_HEADERS {
            return Err(DemuxError::Invalid(
                "Ogg header packets are too large".into(),
            ));
        }
        packets.push(raw.data);
    }
}

/// Runs the first data packet through the demuxer once from the start of the page it begins on,
/// with the state `ogg_reset` leaves there, as `FFmpeg` does while measuring the stream. Returns
/// the start time `FFmpeg` subtracts from a Vorbis stream's duration.
fn rehearse(framer: &mut OggFramer, input: &mut Input, at: &OggState) -> Result<i64, DemuxError> {
    let Some(mut page) = read_page(input, at.page)? else {
        return Ok(0);
    };
    if at.segment >= page.segments.len() {
        match framer.next_page(input, page.next)? {
            Some(next) => page = next,
            None => return Ok(0),
        }
    }
    let mut segment = 0;
    if page.flags & FLAG_CONTINUED != 0 {
        while segment < page.segments.len() {
            let size = page.segments[segment];
            segment += 1;
            if size < 255 {
                break;
            }
        }
    }
    framer.state = OggState {
        page: page.pos,
        segment,
        flags: page.flags,
        granule: page.granule,
        last_pts: Some(0),
        last_dts: None,
        start_trimming: 0,
        ended: false,
        ..framer.state.clone()
    };
    framer.loaded = None;
    let frame = framer.next(input, false)?;
    Ok(match &framer.codec {
        Codec::Vorbis(_) if page.flags & FLAG_EOS == 0 => frame.map_or(0, |frame| frame.pts.max(0)),
        _ => 0,
    })
}

/// The granule position of the stream's last page that has one, read from the file's end as
/// `ogg_get_length` does.
fn last_granule(input: &mut Input, serial: u32) -> Result<Option<i64>, DemuxError> {
    let mut pos = input.len().saturating_sub(MAX_PAGE);
    let mut last = None;
    while let Some(page) = read_page(input, pos)? {
        if page.serial == serial && page.granule != -1 && page.granule != 0 {
            last = Some(page.granule);
        }
        pos = page.next;
    }
    Ok(last)
}

fn parse_headers(packets: &[Vec<u8>]) -> Result<Headers, DemuxError> {
    let first = &packets[0];
    if first.starts_with(b"\x01vorbis") {
        return parse_vorbis(packets);
    }
    if first.starts_with(b"OpusHead") {
        return parse_opus(packets);
    }
    parse_flac(packets)
}

fn parse_vorbis(packets: &[Vec<u8>]) -> Result<Headers, DemuxError> {
    let invalid = |reason: &str| DemuxError::Invalid(format!("Vorbis {reason}"));
    let [identification, comment, setup] = packets else {
        return Err(invalid("needs three header packets"));
    };
    if identification.len() != 30
        || identification[7..11] != [0; 4]
        || identification[29] != 1
        || comment.first() != Some(&3)
        || setup.first() != Some(&5)
    {
        return Err(invalid("headers are malformed"));
    }
    let channels = u32::from(identification[11]);
    let sample_rate = u32::from_le_bytes([
        identification[12],
        identification[13],
        identification[14],
        identification[15],
    ]);
    let small = identification[28] & 0x0f;
    let large = identification[28] >> 4;
    if channels == 0
        || sample_rate == 0
        || sample_rate > i32::MAX as u32
        || small > large
        || small < 6
        || large > 13
    {
        return Err(invalid("identification header is out of range"));
    }
    let modes = VorbisModes::parse(identification, setup)
        .ok_or_else(|| invalid("setup header is malformed"))?;
    let (title, language) = tags(
        comment
            .get(7..comment.len().saturating_sub(1))
            .unwrap_or(&[]),
    );
    let stripped = strip_comments(comment);
    let mut private = vec![2];
    xiph_lacing(&mut private, identification.len());
    xiph_lacing(&mut private, stripped.len());
    private.extend_from_slice(identification);
    private.extend_from_slice(&stripped);
    private.extend_from_slice(setup);
    Ok(Headers {
        codec: Codec::Vorbis(modes),
        audio: AudioTrack {
            codec: AudioCodec::Vorbis,
            codec_private: Bytes::from(private),
            sample_rate,
            channels,
            speakers: vorbis_order(channels),
            bits_per_sample: None,
            priming: 0,
            seek_preroll: None,
        },
        timescale: sample_rate,
        title,
        language,
    })
}

fn parse_opus(packets: &[Vec<u8>]) -> Result<Headers, DemuxError> {
    let head = &packets[0];
    if head.len() < 19 || head[8] & 0xf0 != 0 {
        return Err(DemuxError::Invalid("malformed OpusHead".into()));
    }
    let tags_packet = packets
        .get(1)
        .filter(|packet| packet.starts_with(b"OpusTags"));
    let Some(tags_packet) = tags_packet else {
        return Err(DemuxError::Invalid("Ogg Opus without OpusTags".into()));
    };
    let channels = u32::from(head[9]);
    if channels == 0 {
        return Err(DemuxError::Invalid("Opus with no channels".into()));
    }
    let pre_skip = u16::from_le_bytes([head[10], head[11]]);
    let speakers = match head[18] {
        0 if channels <= 2 => vorbis_order(channels),
        1 => vorbis_order(channels),
        _ => None,
    };
    let (title, language) = tags(&tags_packet[8..]);
    Ok(Headers {
        codec: Codec::Opus {
            pre_skip: i64::from(pre_skip),
        },
        audio: AudioTrack {
            codec: AudioCodec::Opus,
            codec_private: Bytes::copy_from_slice(head),
            sample_rate: 48_000,
            channels,
            speakers,
            bits_per_sample: None,
            priming: u32::from(pre_skip),
            seek_preroll: None,
        },
        timescale: 48_000,
        title,
        language,
    })
}

fn parse_flac(packets: &[Vec<u8>]) -> Result<Headers, DemuxError> {
    let first = &packets[0];
    // 0x7f "FLAC", version 1.x, a header count, "fLaC", then the STREAMINFO block.
    if first.len() < 51 || first[5] != 1 || first[13..17] != [0, 0, 0, 34] {
        return Err(DemuxError::Invalid("malformed Ogg FLAC header".into()));
    }
    let streaminfo = first[17..51].to_vec();
    let info = stream_info(&streaminfo)?;
    let mut blocks = vec![(0_u8, streaminfo)];
    let mut title = None;
    let mut language = None;
    let mut mask = None;
    for packet in &packets[1..] {
        if packet.len() < 4 {
            continue;
        }
        let kind = packet[0] & 0x7f;
        let body = packet[4..].to_vec();
        if kind == 4 {
            (title, language) = tags(&body);
            mask = mask.or_else(|| comment_channel_mask(&body));
        }
        blocks.push((kind, body));
    }
    let speakers = mask
        .and_then(|mask| speakers_from_mask(mask, info.channels))
        .or_else(|| speakers_from_mask(CHANNEL_MASKS[info.channels as usize - 1], info.channels));
    Ok(Headers {
        codec: Codec::Flac,
        audio: AudioTrack {
            codec: AudioCodec::Flac,
            codec_private: flac_private(&blocks),
            sample_rate: info.sample_rate,
            channels: info.channels,
            speakers,
            bits_per_sample: u16::try_from(info.bits).ok(),
            priming: 0,
            seek_preroll: None,
        },
        timescale: info.sample_rate,
        title,
        language,
    })
}
