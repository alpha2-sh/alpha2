//! The Matroska and `WebM` demuxer.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek};

use bytes::{Bytes, BytesMut};

use super::block::{self, Block};
use super::describe::{self, Described, Encoding, Entry, Timing, rescale};
use super::ebml::{self, Children, Header, id, is_level1};
use super::timing::{Clock, Raw};
use crate::audio::Input;
use crate::{ContainerReader, DemuxError, Packet, Source, TrackId, TrackInfo, VideoCodec};

/// The largest level-1 element other than a Cluster read into memory.
const MAX_HEADER_ELEMENT: u64 = 64 << 20;
/// The largest block or block group read into memory.
const MAX_BLOCK: u64 = 256 << 20;
/// Bytes read to decode an element header.
const HEADER_PEEK: usize = 12;
/// Packets waiting for a decode time beyond the deepest reordering (16 frames).
const MAX_HELD: usize = 17;
/// How far stream probing reads (`FFmpeg`'s default `probesize`).
const PROBE_BYTES: u64 = 5_000_000;

/// Reads Matroska and `WebM` files: their tracks and each track's packets, timed as `FFmpeg`'s
/// Matroska demuxer and generic demuxing layer time them.
///
/// Timestamps are in the track's time base, `TimestampScale` nanoseconds (milliseconds by
/// default). Decode times `FFmpeg` leaves unset (the first packets of a reordered H.264, HEVC or
/// MPEG-4 Part 2 stream) are the next packet's decode time less the packet's duration.
pub struct MatroskaReader {
    input: Input,
    segment: Segment,
    tracks: Vec<TrackInfo>,
    states: Vec<TrackState>,
    /// Clusters seen so far: position and timestamp.
    clusters: BTreeMap<u64, u64>,
    /// `CuePoint`s: cluster position and cue time, both of any track.
    cues: Vec<(u64, u64)>,
}

impl std::fmt::Debug for MatroskaReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MatroskaReader")
            .field("tracks", &self.tracks)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    end: u64,
    first_cluster: Option<u64>,
    scale: u64,
}

/// Where a scan of blocks stands.
#[derive(Clone, Copy, Debug)]
struct Scan {
    pos: u64,
    cluster: Option<Cluster>,
}

#[derive(Clone, Copy, Debug)]
struct Cluster {
    /// Where its payload ends; `None` for an unknown size.
    end: Option<u64>,
    timestamp: u64,
}

/// A `SimpleBlock` or `BlockGroup` element found by a scan.
#[derive(Clone, Copy, Debug)]
struct Found {
    /// The element's first byte, which names the block.
    at: u64,
    payload: u64,
    size: u64,
    group: bool,
    cluster_timestamp: u64,
}

/// A seek's keyframe: the block (by position) and lace it is in, and its presentation time.
type Landing = ((u64, usize), i64);

/// A packet waiting to be handed out, with the block and lace it came from.
struct Pending {
    packet: Packet,
    origin: (u64, usize),
    has_dts: bool,
}

struct TrackState {
    number: u64,
    described: Described,
    track_scale: f64,
    /// The segment's `TimestampScale`, which turns a `DefaultDuration` into ticks.
    segment_scale: u64,
    default_duration: u64,
    stripped: Bytes,
    clock_start: Clock,
    clock: Clock,
    scan: Scan,
    /// The block of the AAC frame `FFmpeg` times before its decoder knows the frame length.
    first_block: Option<u64>,
    /// Packets ready to hand out, and packets waiting for a later decode time.
    ready: VecDeque<Pending>,
    held: VecDeque<Pending>,
    /// Packets before this block and lace are dropped (a seek target).
    skip_until: Option<(u64, usize)>,
    /// The block and lace of the track's first packet, once a scan from the start produced it.
    first_packet: Option<(u64, usize)>,
    /// Whether the track's scan started at the first cluster and has produced nothing yet.
    from_start: bool,
    ended: bool,
}

impl TrackState {
    fn new(entry: &Entry, described: Described, segment: &Segment) -> Self {
        let reordered = matches!(
            described.info.video.as_ref().map(|video| video.codec),
            Some(VideoCodec::H264 | VideoCodec::Hevc)
        );
        let clock = Clock::new(
            described.timing.clone(),
            described.intra,
            described.decode_times,
            reordered,
            described.time_base,
        );
        Self {
            number: entry.number,
            described,
            track_scale: if entry.track_scale < 0.01 || entry.track_scale.is_nan() {
                1.0
            } else {
                entry.track_scale
            },
            segment_scale: segment.scale,
            default_duration: entry.default_duration,
            stripped: match &entry.encoding {
                Encoding::Stripped(bytes) => bytes.clone(),
                _ => Bytes::new(),
            },
            clock_start: clock.clone(),
            clock,
            scan: Scan {
                pos: segment.first_cluster.unwrap_or(segment.end),
                cluster: None,
            },
            first_block: None,
            ready: VecDeque::new(),
            held: VecDeque::new(),
            skip_until: None,
            first_packet: None,
            from_start: true,
            ended: false,
        }
    }

    /// The packets `FFmpeg`'s stream probing reads of this track before it knows enough: the
    /// frames its decoder takes to show a picture and settle the reorder depth, one for audio.
    fn probe_packets(&self) -> usize {
        match (&self.described.timing, &self.described.info.video) {
            (Timing::Video { depth, .. }, Some(video)) => match video.codec {
                VideoCodec::H264 if *depth == 0 => 8,
                VideoCodec::H264 => depth + 2,
                _ => depth + 1,
            },
            (_, None) if self.described.info.audio.is_some() => 1,
            _ => 0,
        }
    }
}

fn invalid(reason: impl Into<String>) -> DemuxError {
    DemuxError::Invalid(reason.into())
}

/// The element header at `pos`, `None` at the end of the file or where none can be read.
fn header_at(input: &mut Input, pos: u64) -> Result<Option<Header>, DemuxError> {
    Ok(ebml::header(input.peek(pos, HEADER_PEEK)?))
}

/// The payload of the element at `pos` with header `header`, at most `limit` bytes.
fn load(input: &mut Input, pos: u64, header: Header, limit: u64) -> Result<Bytes, DemuxError> {
    let size = header.size.unwrap_or(0);
    if size > limit {
        return Err(invalid(format!(
            "a Matroska element of {size} bytes is larger than this layer reads"
        )));
    }
    let start = pos + header.len as u64;
    let available = input.len().saturating_sub(start).min(size);
    let length = usize::try_from(available).map_err(|_| invalid("an element is too large"))?;
    Ok(Bytes::copy_from_slice(input.exact(start, length)?))
}

/// The position of the next Cluster ID at or after `pos`, before `end`.
fn resync(input: &mut Input, mut pos: u64, end: u64) -> Result<Option<u64>, DemuxError> {
    const CLUSTER: [u8; 4] = [0x1f, 0x43, 0xb6, 0x75];
    while pos < end {
        let window = input.peek(pos, 1 << 16)?;
        if window.len() < 4 {
            return Ok(None);
        }
        if let Some(found) = window.windows(4).position(|bytes| bytes == CLUSTER) {
            return Ok(Some(pos + found as u64));
        }
        pos += window.len() as u64 - 3;
    }
    Ok(None)
}

/// Reads the EBML header and returns where the Segment's payload starts and ends.
fn open_segment(input: &mut Input) -> Result<(u64, u64), DemuxError> {
    let Some(head) = header_at(input, 0)? else {
        return Err(invalid("the file has no EBML header"));
    };
    if head.id != id::EBML {
        return Err(invalid("the file has no EBML header"));
    }
    let body = load(input, 0, head, 1 << 16)?;
    let mut doc_type = String::from("(none)");
    let (mut read_version, mut max_size, mut max_id, mut doc_read) = (1, 8, 4, 1);
    for (element, value) in Children::new(&body) {
        let number = || ebml::uint(value).unwrap_or(0);
        match element {
            id::EBML_READ_VERSION => read_version = number(),
            id::EBML_MAX_SIZE_LENGTH => max_size = number(),
            id::EBML_MAX_ID_LENGTH => max_id = number(),
            id::DOC_TYPE => doc_type = ebml::string(value),
            id::DOC_TYPE_READ_VERSION => doc_read = number(),
            _ => {}
        }
    }
    if read_version > 1 || max_size > 8 || max_id > 4 || doc_read > 3 {
        return Err(DemuxError::Unsupported(format!(
            "EBML read version {read_version}, doc type {doc_type} version {doc_read} is not read"
        )));
    }
    let mut pos = head.len as u64 + head.size.unwrap_or(0);
    let file_len = input.len();
    loop {
        let Some(found) = header_at(input, pos)? else {
            return Err(invalid("the file has no Matroska segment"));
        };
        let payload = pos + found.len as u64;
        if found.id == id::SEGMENT {
            let end = found
                .size
                .map_or(file_len, |size| payload.saturating_add(size).min(file_len));
            return Ok((payload, end));
        }
        match found.size {
            Some(size) => pos = payload.saturating_add(size),
            None => return Err(invalid("the file has no Matroska segment")),
        }
    }
}

/// The level-1 elements the reader keeps.
#[derive(Default)]
struct Headers {
    scale: Option<u64>,
    entries: Option<Vec<Entry>>,
    cues: Option<Vec<(u64, u64)>>,
    seeks: Vec<(u32, u64)>,
}

impl Headers {
    fn take(&mut self, element: u32, body: &[u8], start: u64) {
        match element {
            id::INFO if self.scale.is_none() => {
                let mut scale = 1_000_000;
                for (child, value) in Children::new(body) {
                    if child == id::TIMESTAMP_SCALE {
                        scale = ebml::uint(value).unwrap_or(0);
                    }
                }
                self.scale = Some(if scale == 0 { 1_000_000 } else { scale });
            }
            id::TRACKS if self.entries.is_none() => {
                self.entries = Some(
                    Children::new(body)
                        .filter(|(child, _)| *child == id::TRACK_ENTRY)
                        .map(|(_, entry)| describe::entry(entry))
                        .collect(),
                );
            }
            id::CUES if self.cues.is_none() => self.cues = Some(cues(body, start)),
            id::SEEK_HEAD => self.seeks.extend(seeks(body)),
            _ => {}
        }
    }
}

/// The level-1 elements before the first Cluster, then those the `SeekHead`s point to that are
/// still missing, and where the first Cluster is.
fn read_headers(
    input: &mut Input,
    start: u64,
    end: u64,
) -> Result<(Headers, Option<u64>), DemuxError> {
    let mut headers = Headers::default();
    let mut first_cluster = None;
    let mut pos = start;
    while pos < end {
        let Some(found) = header_at(input, pos)? else {
            first_cluster = resync(input, pos + 1, end)?;
            break;
        };
        if found.id == id::CLUSTER {
            first_cluster = Some(pos);
            break;
        }
        let Some(size) = found.size else {
            break;
        };
        if matches!(found.id, id::INFO | id::TRACKS | id::CUES | id::SEEK_HEAD) {
            let body = load(input, pos, found, MAX_HEADER_ELEMENT)?;
            headers.take(found.id, &body, start);
        }
        pos = pos + found.len as u64 + size;
    }
    for (element, position) in std::mem::take(&mut headers.seeks) {
        let missing = match element {
            id::INFO => headers.scale.is_none(),
            id::TRACKS => headers.entries.is_none(),
            id::CUES => headers.cues.is_none(),
            _ => false,
        };
        let at = start.saturating_add(position);
        if !missing || at >= end {
            continue;
        }
        if let Some(found) = header_at(input, at)?
            && found.id == element
            && found.size.is_some()
        {
            let body = load(input, at, found, MAX_HEADER_ELEMENT)?;
            headers.take(element, &body, start);
        }
    }
    Ok((headers, first_cluster))
}

fn seeks(body: &[u8]) -> Vec<(u32, u64)> {
    Children::new(body)
        .filter(|(element, _)| *element == id::SEEK)
        .filter_map(|(_, seek)| {
            let (mut target, mut position) = (None, None);
            for (element, value) in Children::new(seek) {
                match element {
                    id::SEEK_ID => target = ebml::uint(value),
                    id::SEEK_POSITION => position = ebml::uint(value),
                    _ => {}
                }
            }
            Some((u32::try_from(target?).ok()?, position?))
        })
        .collect()
}

fn cues(body: &[u8], start: u64) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    for (element, point) in Children::new(body) {
        if element != id::CUE_POINT {
            continue;
        }
        let mut time = 0;
        let mut positions = Vec::new();
        for (child, value) in Children::new(point) {
            match child {
                id::CUE_TIME => time = ebml::uint(value).unwrap_or(0),
                id::CUE_TRACK_POSITIONS => {
                    for (inner, number) in Children::new(value) {
                        if inner == id::CUE_CLUSTER_POSITION
                            && let Some(position) = ebml::uint(number)
                        {
                            positions.push(start.saturating_add(position));
                        }
                    }
                }
                _ => {}
            }
        }
        found.extend(positions.into_iter().map(|position| (position, time)));
    }
    found
}

impl MatroskaReader {
    /// Opens the Matroska or `WebM` file at `path`.
    ///
    /// # Errors
    /// A file that cannot be read, is not Matroska, or breaks this layer's limits.
    pub fn open(path: &std::path::Path) -> Result<Self, DemuxError> {
        Self::new(std::fs::File::open(path)?)
    }

    /// Reads a Matroska or `WebM` file from `source`.
    ///
    /// # Errors
    /// A source that cannot be read, is not Matroska, or breaks this layer's limits.
    pub fn new<R: Read + Seek + Send + 'static>(source: R) -> Result<Self, DemuxError> {
        let source: Box<dyn Source> = Box::new(source);
        let mut input = Input::new(source)?;
        let (start, end) = open_segment(&mut input)?;
        let (headers, first_cluster) = read_headers(&mut input, start, end)?;
        let segment = Segment {
            end,
            first_cluster,
            scale: headers.scale.unwrap_or(1_000_000),
        };
        let mut tracks = Vec::new();
        let mut states = Vec::new();
        for entry in headers.entries.unwrap_or_default() {
            if !describe::listed(&entry) {
                continue;
            }
            let described = describe::describe(&entry, segment.scale)?;
            tracks.push(described.info.clone());
            states.push(TrackState::new(&entry, described, &segment));
        }
        let mut cues = headers.cues.unwrap_or_default();
        cues.retain(|(position, _)| *position >= start && *position < end);
        cues.sort_unstable();
        cues.dedup();
        let mut reader = Self {
            input,
            segment,
            tracks,
            states,
            clusters: BTreeMap::new(),
            cues,
        };
        reader.probe()?;
        Ok(reader)
    }

    fn state(&self, track: TrackId) -> Result<usize, DemuxError> {
        self.tracks
            .iter()
            .position(|info| info.id == track)
            .ok_or(DemuxError::NoSuchTrack(track.0))
    }

    /// The next block element `scan` reaches, of any track.
    fn next_block(&mut self, scan: &mut Scan) -> Result<Option<Found>, DemuxError> {
        let segment_end = self.segment.end;
        loop {
            if let Some(cluster) = scan.cluster.as_mut() {
                let end = cluster.end.unwrap_or(segment_end);
                if scan.pos >= end {
                    scan.cluster = None;
                    continue;
                }
                let Some(found) = header_at(&mut self.input, scan.pos)? else {
                    scan.cluster = None;
                    match resync(&mut self.input, scan.pos + 1, segment_end)? {
                        Some(next) => scan.pos = next,
                        None => return Ok(None),
                    }
                    continue;
                };
                if cluster.end.is_none() && is_level1(found.id) {
                    scan.cluster = None;
                    continue;
                }
                let payload = scan.pos + found.len as u64;
                let Some(size) = found.size else {
                    scan.cluster = None;
                    match resync(&mut self.input, scan.pos + 1, segment_end)? {
                        Some(next) => scan.pos = next,
                        None => return Ok(None),
                    }
                    continue;
                };
                let next = payload.saturating_add(size);
                if next > self.input.len() {
                    scan.pos = segment_end;
                    scan.cluster = None;
                    return Ok(None);
                }
                let at = scan.pos;
                scan.pos = next.min(end);
                match found.id {
                    id::CLUSTER_TIMESTAMP => {
                        let bytes = load(&mut self.input, at, found, 8)?;
                        cluster.timestamp = ebml::uint(&bytes).unwrap_or(0);
                    }
                    id::SIMPLE_BLOCK | id::BLOCK_GROUP => {
                        return Ok(Some(Found {
                            at,
                            payload,
                            size: scan.pos - payload,
                            group: found.id == id::BLOCK_GROUP,
                            cluster_timestamp: cluster.timestamp,
                        }));
                    }
                    _ => {}
                }
            } else {
                if scan.pos >= segment_end {
                    return Ok(None);
                }
                let Some(found) = header_at(&mut self.input, scan.pos)? else {
                    match resync(&mut self.input, scan.pos + 1, segment_end)? {
                        Some(next) => scan.pos = next,
                        None => return Ok(None),
                    }
                    continue;
                };
                let payload = scan.pos + found.len as u64;
                if found.id == id::CLUSTER {
                    let end = found
                        .size
                        .map(|size| payload.saturating_add(size).min(segment_end));
                    let timestamp = self.cluster_timestamp(payload, end)?;
                    self.clusters.insert(scan.pos, timestamp);
                    scan.cluster = Some(Cluster { end, timestamp });
                    scan.pos = payload;
                    continue;
                }
                match found.size {
                    Some(size) => scan.pos = payload.saturating_add(size),
                    None => return Ok(None),
                }
            }
        }
    }

    /// The timestamp of the cluster whose payload starts at `payload`, from its elements before
    /// the first block (0 without one, as `FFmpeg` defaults it).
    fn cluster_timestamp(&mut self, payload: u64, end: Option<u64>) -> Result<u64, DemuxError> {
        let end = end.unwrap_or(self.segment.end);
        let mut pos = payload;
        while pos < end {
            let Some(found) = header_at(&mut self.input, pos)? else {
                break;
            };
            match (found.id, found.size) {
                (id::CLUSTER_TIMESTAMP, Some(_)) => {
                    let bytes = load(&mut self.input, pos, found, 8)?;
                    return Ok(ebml::uint(&bytes).unwrap_or(0));
                }
                (id::SIMPLE_BLOCK | id::BLOCK_GROUP, _) | (_, None) => break,
                (element, Some(size)) => {
                    if is_level1(element) {
                        break;
                    }
                    pos = pos + found.len as u64 + size;
                }
            }
        }
        Ok(0)
    }

    /// The track number and frame count of the block element `found`.
    fn block_head(&mut self, found: &Found) -> Result<Option<(u64, usize)>, DemuxError> {
        if !found.group {
            return Ok(block::head(self.input.peek(found.payload, 16)?));
        }
        let end = found.payload + found.size;
        let mut pos = found.payload;
        while pos < end {
            let Some(child) = header_at(&mut self.input, pos)? else {
                return Ok(None);
            };
            let body = pos + child.len as u64;
            if child.id == id::BLOCK {
                return Ok(block::head(self.input.peek(body, 16)?));
            }
            pos = body.saturating_add(child.size.unwrap_or(u64::MAX));
        }
        Ok(None)
    }

    /// The next block of the track at `index` after `scan`, read whole.
    fn next_track_block(
        &mut self,
        index: usize,
        scan: &mut Scan,
    ) -> Result<Option<(Found, Block)>, DemuxError> {
        let number = self.states[index].number;
        while let Some(found) = self.next_block(scan)? {
            if self.block_head(&found)?.map(|(track, _)| track) != Some(number) {
                continue;
            }
            if found.size > MAX_BLOCK {
                return Err(invalid(format!(
                    "a Matroska block of {} bytes is larger than this layer reads",
                    found.size
                )));
            }
            let length =
                usize::try_from(found.size).map_err(|_| invalid("a block is too large"))?;
            let data = Bytes::copy_from_slice(self.input.exact(found.payload, length)?);
            if let Some(parsed) = block::parse(&data, found.group)?
                && parsed.track == number
            {
                return Ok(Some((found, parsed)));
            }
        }
        Ok(None)
    }

    /// The packets of `block`, timed by `clock`.
    fn packets_of(
        state: &TrackState,
        clock: &mut Clock,
        found: &Found,
        block: &Block,
        first: bool,
    ) -> Vec<Pending> {
        let described = &state.described;
        let (num, _) = described.time_base;
        let scale = i64::try_from(num).unwrap_or(1);
        let relative = i64::from(block.relative);
        let mut timecode = (relative >= 0 || found.cluster_timestamp >= relative.unsigned_abs())
            .then(|| {
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let cluster = (found.cluster_timestamp as f64 / state.track_scale) as u64;
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
                let time = cluster
                    .wrapping_add(relative as u64)
                    .wrapping_sub(described.delay as u64) as i64;
                time
            });
        let laces = block.frames.len() as u64;
        let mut duration = block.duration;
        if duration == 0 && state.default_duration != 0 {
            duration = state
                .default_duration
                .saturating_mul(laces)
                .checked_div(state.segment_scale)
                .unwrap_or(0);
        }
        let rate = state
            .described
            .info
            .audio
            .as_ref()
            .map_or(0, |audio| audio.sample_rate);
        let discard = if block.discard_padding == 0 {
            (0, 0)
        } else {
            let samples = rescale(block.discard_padding, i64::from(rate), 1_000_000_000);
            let clamp = |value: i64| u32::try_from(value.max(0)).unwrap_or(u32::MAX);
            if samples > 0 {
                (0, clamp(samples))
            } else {
                (clamp(-samples), 0)
            }
        };
        let mut out = Vec::with_capacity(block.frames.len());
        for (lace, frame) in block.frames.iter().enumerate() {
            let lace_u = lace as u64;
            let lace_duration = (u128::from(duration) * u128::from(lace_u + 1) / u128::from(laces))
                .saturating_sub(u128::from(duration) * u128::from(lace_u) / u128::from(laces));
            let lace_duration = i64::try_from(lace_duration).unwrap_or(i64::MAX);
            let data = if state.stripped.is_empty() {
                frame.clone()
            } else {
                let mut joined = BytesMut::with_capacity(state.stripped.len() + frame.len());
                joined.extend_from_slice(&state.stripped);
                joined.extend_from_slice(frame);
                joined.freeze()
            };
            let this_timecode = timecode;
            timecode = timecode
                .and_then(|time| (lace_duration != 0).then(|| time.wrapping_add(lace_duration)));
            if data.is_empty() {
                continue;
            }
            let times = clock.times(&Raw {
                timecode: this_timecode,
                duration: lace_duration,
                keyframe: block.keyframe && lace == 0,
                data: &data,
                first: first && lace == 0,
            });
            let has_dts = times.dts.is_some();
            let pts = times.pts.or(times.dts).unwrap_or(0);
            out.push(Pending {
                packet: Packet {
                    track: state.described.info.id,
                    pts: pts.saturating_mul(scale),
                    dts: times.dts.unwrap_or(pts).saturating_mul(scale),
                    duration: times.duration.saturating_mul(scale),
                    keyframe: times.keyframe,
                    data,
                    discard,
                },
                origin: (found.at, lace),
                has_dts,
            });
        }
        out
    }

    /// Reads the next block of the track at `index` into its queues; false at its end.
    fn advance(&mut self, index: usize) -> Result<bool, DemuxError> {
        let mut scan = self.states[index].scan;
        let next = self.next_track_block(index, &mut scan)?;
        self.states[index].scan = scan;
        let Some((found, block)) = next else {
            let state = &mut self.states[index];
            state.ended = true;
            while let Some(mut pending) = state.held.pop_front() {
                pending.packet.dts = pending.packet.pts;
                state.ready.push_back(pending);
            }
            return Ok(false);
        };
        let first = self.is_first_block(index, found.at);
        let state = &mut self.states[index];
        let mut clock = state.clock.clone();
        let produced = Self::packets_of(state, &mut clock, &found, &block, first);
        state.clock = clock;
        if state.from_start
            && let Some(pending) = produced.first()
        {
            state.first_packet = state.first_packet.or(Some(pending.origin));
            state.from_start = false;
        }
        for pending in produced {
            if pending.has_dts {
                let mut next_dts = pending.packet.dts;
                for held in state.held.iter_mut().rev() {
                    next_dts = next_dts.saturating_sub(held.packet.duration.max(1));
                    held.packet.dts = next_dts;
                }
                state.ready.extend(state.held.drain(..));
                state.ready.push_back(pending);
            } else {
                state.held.push_back(pending);
                // Reordering never holds more back; a stream without timestamps does not wait.
                if state.held.len() > MAX_HELD {
                    for mut held in state.held.drain(..) {
                        held.packet.dts = held.packet.pts;
                        state.ready.push_back(held);
                    }
                }
            }
        }
        Ok(true)
    }

    /// Rewinds the track at `index` to `pos` (a cluster or the first one) with a fresh clock.
    fn restart(&mut self, index: usize, pos: u64, skip_until: Option<(u64, usize)>) {
        let from_start = Some(pos) == self.segment.first_cluster;
        let state = &mut self.states[index];
        state.scan = Scan { pos, cluster: None };
        state.clock = state.clock_start.clone();
        state.ready.clear();
        state.held.clear();
        state.skip_until = skip_until;
        state.from_start = from_start;
        state.ended = false;
    }

    /// The next packet of the track at `index`, past any seek target's predecessors.
    fn next_pending(&mut self, index: usize) -> Result<Option<Pending>, DemuxError> {
        loop {
            let state = &mut self.states[index];
            if let Some(pending) = state.ready.pop_front() {
                if let Some(target) = state.skip_until {
                    if pending.origin != target {
                        continue;
                    }
                    state.skip_until = None;
                }
                return Ok(Some(Self::with_codec_delay(state, pending)));
            }
            if state.ended || !self.advance(index)? {
                let state = &mut self.states[index];
                if state.ready.is_empty() {
                    return Ok(None);
                }
            }
        }
    }

    /// `pending` with the track's `CodecDelay` as its skip-samples when it is the track's first
    /// packet, as `FFmpeg` marks the first packet it reads of a track with an initial padding.
    fn with_codec_delay(state: &TrackState, mut pending: Pending) -> Pending {
        let priming = state
            .described
            .info
            .audio
            .as_ref()
            .map_or(0, |audio| audio.priming);
        if priming > 0 && state.first_packet == Some(pending.origin) {
            pending.packet.discard.0 = priming;
        }
        pending
    }

    /// Cluster positions a seek may start from, with their time in the track's units.
    fn seek_starts(&mut self, index: usize, target: i64) -> Result<Vec<(u64, i64)>, DemuxError> {
        let Some(first) = self.segment.first_cluster else {
            return Ok(Vec::new());
        };
        self.discover_clusters(index, target)?;
        let state = &self.states[index];
        let scale = i64::try_from(state.described.time_base.0).unwrap_or(1);
        let time = |ticks: u64| -> i64 {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let scaled = (ticks as f64 / state.track_scale) as i64;
            scaled
                .saturating_sub(state.described.delay)
                .saturating_mul(scale)
        };
        let mut starts = vec![(first, i64::MIN)];
        starts.extend(self.cues.iter().map(|(pos, ticks)| (*pos, time(*ticks))));
        starts.extend(
            self.clusters
                .iter()
                .map(|(pos, ticks)| (*pos, time(*ticks))),
        );
        starts.sort_unstable();
        starts.dedup_by_key(|start| start.0);
        Ok(starts)
    }

    /// Finds clusters past the last one seen, by their headers alone, until one starts after
    /// `target` or one has no size.
    fn discover_clusters(&mut self, index: usize, target: i64) -> Result<(), DemuxError> {
        let state = &self.states[index];
        let (track_scale, delay) = (state.track_scale, state.described.delay);
        let scale = i64::try_from(state.described.time_base.0).unwrap_or(1);
        let Some(mut pos) = self
            .clusters
            .last_key_value()
            .map(|(pos, _)| *pos)
            .or(self.segment.first_cluster)
        else {
            return Ok(());
        };
        while pos < self.segment.end {
            let Some(found) = header_at(&mut self.input, pos)? else {
                return Ok(());
            };
            let payload = pos + found.len as u64;
            let Some(size) = found.size else {
                return Ok(());
            };
            if found.id == id::CLUSTER {
                let end = payload.saturating_add(size).min(self.segment.end);
                let timestamp = self.cluster_timestamp(payload, Some(end))?;
                self.clusters.insert(pos, timestamp);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let time = ((timestamp as f64 / track_scale) as i64)
                    .saturating_sub(delay)
                    .saturating_mul(scale);
                if time > target {
                    return Ok(());
                }
            }
            pos = payload.saturating_add(size);
        }
        Ok(())
    }

    /// The latest keyframe at or before `target` reached by reading from `pos`: its block, lace
    /// and presentation time.
    fn keyframe_from(
        &mut self,
        index: usize,
        pos: u64,
        target: i64,
    ) -> Result<Option<Landing>, DemuxError> {
        let mut scan = Scan { pos, cluster: None };
        let mut clock = self.states[index].clock_start.clone();
        let mut best = None;
        let mut from_start = Some(pos) == self.segment.first_cluster;
        while let Some((found, block)) = self.next_track_block(index, &mut scan)? {
            let first = self.is_first_block(index, found.at);
            let produced = Self::packets_of(&self.states[index], &mut clock, &found, &block, first);
            let state = &mut self.states[index];
            if from_start && let Some(pending) = produced.first() {
                state.first_packet = state.first_packet.or(Some(pending.origin));
                from_start = false;
            }
            for pending in produced {
                if !pending.packet.keyframe {
                    continue;
                }
                if pending.packet.pts > target {
                    return Ok(best);
                }
                best = Some((pending.origin, pending.packet.pts));
            }
        }
        Ok(best)
    }

    /// Whether the block at `at` holds the frame `FFmpeg` times before its decoder knows the
    /// frame length (see [`Self::probe`]).
    fn is_first_block(&self, index: usize, at: u64) -> bool {
        self.states[index].first_block == Some(at)
    }

    /// Reads blocks in file order, as `FFmpeg`'s stream probing does, until every track has had
    /// the packets probing waits for. An AAC track whose second packet comes later gets its first
    /// packet handed out before the decoder told its frame length, so without a block duration
    /// that packet has none.
    fn probe(&mut self) -> Result<(), DemuxError> {
        let aac = |state: &TrackState| matches!(state.described.timing, Timing::Aac { .. });
        if !self.states.iter().any(aac) {
            return Ok(());
        }
        let needs = self
            .states
            .iter()
            .map(TrackState::probe_packets)
            .collect::<Vec<_>>();
        let mut counts = vec![0_usize; self.states.len()];
        let mut first = vec![None; self.states.len()];
        let start = self.segment.first_cluster.unwrap_or(self.segment.end);
        let mut scan = Scan {
            pos: start,
            cluster: None,
        };
        while counts.iter().zip(&needs).any(|(count, need)| count < need) {
            let Some(found) = self.next_block(&mut scan)? else {
                break;
            };
            if found.at - start > PROBE_BYTES {
                break;
            }
            let Some((number, laces)) = self.block_head(&found)? else {
                continue;
            };
            if let Some(index) = self.states.iter().position(|state| state.number == number) {
                first[index] = first[index].or(Some(found.at));
                counts[index] += laces;
            }
        }
        for (index, state) in self.states.iter_mut().enumerate() {
            if aac(state) && counts[index] < 2 {
                state.first_block = first[index];
            }
        }
        Ok(())
    }
}

impl ContainerReader for MatroskaReader {
    fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn seek(&mut self, track: TrackId, pts: i64) -> Result<Option<i64>, DemuxError> {
        let index = self.state(track)?;
        let starts = self.seek_starts(index, pts)?;
        let begin = self.segment.first_cluster.unwrap_or(self.segment.end);
        let mut at = starts
            .iter()
            .rposition(|(_, time)| *time <= pts)
            .unwrap_or(0);
        while let Some(&(pos, _)) = starts.get(at) {
            if let Some((origin, found)) = self.keyframe_from(index, pos, pts)? {
                self.restart(index, pos, Some(origin));
                return Ok(Some(found));
            }
            if at == 0 {
                break;
            }
            at -= 1;
        }
        self.restart(index, begin, None);
        let first = self.next_pending(index)?;
        let state = &mut self.states[index];
        Ok(first.map(|pending| {
            let pts = pending.packet.pts;
            state.ready.push_front(pending);
            pts
        }))
    }

    fn next_packet(&mut self, track: TrackId) -> Result<Option<Packet>, DemuxError> {
        let index = self.state(track)?;
        Ok(self.next_pending(index)?.map(|pending| pending.packet))
    }
}
