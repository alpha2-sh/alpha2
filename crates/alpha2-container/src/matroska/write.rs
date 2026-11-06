//! The Matroska and `WebM` muxer. Layout, clusters, cues, codec delay and block durations follow
//! `FFmpeg`'s Matroska muxer writing a seekable file, so `FFmpeg` reads the output as it reads
//! its own stream copy; output bytes depend only on the tracks and packets.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};

use alpha2_media::{ColorMatrix, ColorPrimaries, ColorRange, Rotation, TransferFunction};
use bytes::{BufMut, BytesMut};

use super::describe::{reduce, rescale, xiph_headers};
use super::ebml::{
    id, put_float, put_id, put_master, put_sint, put_size, put_uint, put_void, size_len,
};
use crate::{
    AudioCodec, AudioTrack, ContainerWriter, MuxError, Packet, PcmFormat, TrackId, TrackInfo,
    TrackKind, VideoCodec, VideoTrack,
};

/// Which flavor of Matroska to write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatroskaFlavor {
    /// Matroska (`.mkv`, `.mka`), with CRC-32 elements.
    Matroska,
    /// `WebM`: VP8, VP9 or AV1 video and Opus or Vorbis audio only.
    WebM,
}

/// The space reserved for the `SeekHead`: seven entries, its header, a CRC-32 and room for a
/// Void after it.
const SEEK_HEAD_SPACE: usize = 7 * 21 + 14;
/// A new cluster starts past these many bytes or milliseconds, or at a video keyframe past
/// `KEYFRAME_CLUSTER_SIZE` bytes.
const CLUSTER_SIZE: usize = 5 << 20;
const CLUSTER_TIME: i64 = 5000;
const KEYFRAME_CLUSTER_SIZE: usize = 4 << 10;
/// Packets of one track are written ahead of a silent track once they span this many
/// microseconds (`FFmpeg`'s `max_interleave_delta`).
const MAX_INTERLEAVE_DELTA: i128 = 10_000_000;
/// The Opus `SeekPreRoll` `FFmpeg` writes, in nanoseconds.
const OPUS_SEEK_PRE_ROLL: u64 = 80_000_000;

trait Sink: Write + Seek + Send {}

impl<T: Write + Seek + Send> Sink for T {}

/// Writes Matroska or `WebM` files: the header, clusters of blocks with millisecond timestamps,
/// cues at the end, then the `SeekHead` and duration filled in by seeking back.
pub struct MatroskaWriter {
    sink: Box<dyn Sink>,
    flavor: MatroskaFlavor,
    tracks: Vec<Track>,
    started: bool,
    /// File positions: the Segment's payload (where the `SeekHead` space starts), the `Info` and
    /// `Tracks` elements, and the end of what is written.
    segment: u64,
    info_at: u64,
    tracks_at: u64,
    written: u64,
    /// Milliseconds every timestamp moves by so none is negative, once known.
    shift: Option<i64>,
    cluster: Option<Cluster>,
    /// An audio packet written after the next packet decides where clusters start.
    held_audio: Option<Ready>,
    cues: Vec<Cue>,
    /// The latest block end, in milliseconds.
    duration: i64,
}

impl std::fmt::Debug for MatroskaWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MatroskaWriter")
            .field("flavor", &self.flavor)
            .field("tracks", &self.tracks.len())
            .finish_non_exhaustive()
    }
}

struct Track {
    info: TrackInfo,
    number: u64,
    entry: BytesMut,
    /// Milliseconds the codec delay moves block timestamps by.
    ts_offset: i64,
    /// The `DefaultDuration` rounded down and up to milliseconds; 0 without one.
    default_low: i64,
    default_high: i64,
    sample_rate: u32,
    queue: VecDeque<Packet>,
    last_dts: Option<i64>,
    last_ts: i64,
    has_cue: bool,
}

/// A packet in milliseconds, ready for a block.
struct Ready {
    track: usize,
    ts: i64,
    duration: i64,
    keyframe: bool,
    packet: Packet,
}

struct Cluster {
    /// Where the Cluster element starts in the file.
    at: u64,
    timestamp: i64,
    /// The payload so far, after six bytes held for the CRC-32 in Matroska.
    payload: BytesMut,
}

#[derive(Clone, Copy)]
struct Cue {
    ts: i64,
    track: usize,
    /// The cluster's position in the Segment, and the block's in the cluster's payload.
    cluster: u64,
    relative: u64,
}

/// CRC-32 (IEEE 802.3, reflected), as Matroska's `CRC-32` element holds it.
fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0_u32; 256];
        let mut index: u32 = 0;
        while index < 256 {
            let mut value = index;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 1 != 0 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
                bit += 1;
            }
            table[index as usize] = value;
            index += 1;
        }
        table
    };
    !bytes.iter().fold(u32::MAX, |crc, byte| {
        TABLE[usize::from(crc.to_le_bytes()[0] ^ byte)] ^ (crc >> 8)
    })
}

impl MatroskaWriter {
    /// A writer into `file`.
    #[must_use]
    pub fn create(file: std::fs::File, flavor: MatroskaFlavor) -> Self {
        Self::new(file, flavor)
    }

    /// A writer into any writable and seekable `sink`, starting at its offset 0.
    pub fn new<W: Write + Seek + Send + 'static>(sink: W, flavor: MatroskaFlavor) -> Self {
        Self {
            sink: Box::new(sink),
            flavor,
            tracks: Vec::new(),
            started: false,
            segment: 0,
            info_at: 0,
            tracks_at: 0,
            written: 0,
            shift: None,
            cluster: None,
            held_audio: None,
            cues: Vec::new(),
            duration: 0,
        }
    }

    fn webm(&self) -> bool {
        self.flavor == MatroskaFlavor::WebM
    }

    /// A level-1 element, with a CRC-32 of its payload first in Matroska.
    fn level1(&self, element: u32, payload: &[u8]) -> BytesMut {
        let mut body = BytesMut::with_capacity(payload.len() + 6);
        if !self.webm() {
            put_master(&mut body, id::CRC32, &crc32(payload).to_le_bytes());
        }
        body.put_slice(payload);
        let mut out = BytesMut::with_capacity(body.len() + 12);
        put_master(&mut out, element, &body);
        out
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), MuxError> {
        self.sink.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn info(&self) -> BytesMut {
        let mut payload = BytesMut::new();
        put_uint(&mut payload, id::TIMESTAMP_SCALE, 1_000_000);
        put_master(&mut payload, id::MUXING_APP, b"alpha2");
        put_master(&mut payload, id::WRITING_APP, b"alpha2");
        #[allow(clippy::cast_precision_loss)]
        put_float(&mut payload, id::DURATION, self.duration as f64);
        self.level1(id::INFO, &payload)
    }

    fn start(&mut self) -> Result<(), MuxError> {
        let opus = self.tracks.iter().any(|track| {
            track.info.audio.as_ref().map(|audio| audio.codec) == Some(AudioCodec::Opus)
        });
        let version = if !self.webm() || opus { 4 } else { 2 };
        let mut header = BytesMut::new();
        put_uint(&mut header, id::EBML_VERSION, 1);
        put_uint(&mut header, id::EBML_READ_VERSION, 1);
        put_uint(&mut header, id::EBML_MAX_ID_LENGTH, 4);
        put_uint(&mut header, id::EBML_MAX_SIZE_LENGTH, 8);
        let doc_type: &[u8] = if self.webm() { b"webm" } else { b"matroska" };
        put_master(&mut header, id::DOC_TYPE, doc_type);
        put_uint(&mut header, id::DOC_TYPE_VERSION, version);
        put_uint(&mut header, id::DOC_TYPE_READ_VERSION, 2);
        let mut out = BytesMut::new();
        put_master(&mut out, id::EBML, &header);
        put_id(&mut out, id::SEGMENT);
        out.put_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        self.sink.seek(SeekFrom::Start(0))?;
        self.written = 0;
        self.emit(&out)?;
        self.segment = self.written;
        let mut reserved = BytesMut::new();
        put_void(&mut reserved, SEEK_HEAD_SPACE);
        self.emit(&reserved)?;
        self.info_at = self.written;
        let info = self.info();
        self.emit(&info)?;
        self.tracks_at = self.written;
        let mut entries = BytesMut::new();
        for track in &self.tracks {
            entries.put_slice(&track.entry);
        }
        let tracks = self.level1(id::TRACKS, &entries);
        self.emit(&tracks)?;
        self.started = true;
        Ok(())
    }

    /// Writes queued packets in decode-time order across tracks while every track has one
    /// queued, or one track runs too far ahead; with `all`, every queued packet.
    fn pump(&mut self, all: bool) -> Result<(), MuxError> {
        loop {
            let mut earliest: Option<usize> = None;
            for (index, track) in self.tracks.iter().enumerate() {
                let Some(head) = track.queue.front() else {
                    continue;
                };
                let earlier = earliest.is_none_or(|best| {
                    let best = &self.tracks[best];
                    let other = best.queue.front().map_or(0, |packet| packet.dts);
                    i128::from(head.dts) * i128::from(best.info.timescale)
                        < i128::from(other) * i128::from(track.info.timescale)
                });
                if earlier {
                    earliest = Some(index);
                }
            }
            let Some(index) = earliest else {
                return Ok(());
            };
            if !all && self.tracks.iter().any(|track| track.queue.is_empty()) {
                let micros = |track: &Track, dts: i64| {
                    i128::from(dts) * 1_000_000 / i128::from(track.info.timescale.max(1))
                };
                let head = &self.tracks[index];
                let top = micros(head, head.queue.front().map_or(0, |packet| packet.dts));
                let ahead = self
                    .tracks
                    .iter()
                    .filter_map(|track| track.queue.back().map(|last| micros(track, last.dts)))
                    .map(|last| last - top)
                    .max()
                    .unwrap_or(0);
                if ahead <= MAX_INTERLEAVE_DELTA {
                    return Ok(());
                }
            }
            let packet = self.tracks[index].queue.pop_front().expect("queued packet");
            self.mux(index, packet)?;
        }
    }

    /// Milliseconds every timestamp moves by: enough that the earliest of `first` and the queued
    /// packets is not negative after its codec delay (`FFmpeg`'s `avoid_negative_ts`).
    fn shift_for(&self, track: usize, first: &Packet) -> i64 {
        let ms = |track: &Track, packet: &Packet| {
            rescale(packet.pts, 1000, i64::from(track.info.timescale)) + track.ts_offset
        };
        let lowest = self
            .tracks
            .iter()
            .flat_map(|track| track.queue.iter().map(move |packet| ms(track, packet)))
            .chain(std::iter::once(ms(&self.tracks[track], first)))
            .min()
            .unwrap_or(0);
        (-lowest).max(0)
    }

    /// Hands one packet, in interleaved order, to the cluster logic.
    fn mux(&mut self, index: usize, packet: Packet) -> Result<(), MuxError> {
        let shift = if let Some(shift) = self.shift {
            shift
        } else {
            let shift = self.shift_for(index, &packet);
            self.shift = Some(shift);
            shift
        };
        let track = &self.tracks[index];
        let timescale = i64::from(track.info.timescale);
        let ts = rescale(packet.pts, 1000, timescale) + shift + track.ts_offset;
        let duration = rescale(packet.duration, 1000, timescale).max(0);
        let video = track.info.kind == TrackKind::Video;
        let audio = track.info.kind == TrackKind::Audio;
        let keyframe = packet.keyframe;
        if let Some(cluster) = &self.cluster {
            let size = cluster.payload.len();
            if size > CLUSTER_SIZE
                || ts - cluster.timestamp > CLUSTER_TIME
                || (video && keyframe && size > KEYFRAME_CLUSTER_SIZE)
            {
                self.end_cluster()?;
            }
        }
        if let Some(held) = self.held_audio.take() {
            self.write_block(held)?;
        }
        let ready = Ready {
            track: index,
            ts,
            duration,
            keyframe,
            packet,
        };
        if audio {
            self.held_audio = Some(ready);
            Ok(())
        } else {
            self.write_block(ready)
        }
    }

    fn end_cluster(&mut self) -> Result<(), MuxError> {
        let Some(cluster) = self.cluster.take() else {
            return Ok(());
        };
        if !self
            .tracks
            .iter()
            .any(|track| track.info.kind == TrackKind::Video)
        {
            for track in &mut self.tracks {
                track.has_cue = false;
            }
        }
        let skip = if self.webm() { 0 } else { 6 };
        let element = self.level1(id::CLUSTER, &cluster.payload[skip..]);
        self.emit(&element)
    }

    /// Writes a packet as a `SimpleBlock`, or a `BlockGroup` when it needs a duration or discard
    /// padding.
    fn write_block(&mut self, ready: Ready) -> Result<(), MuxError> {
        let Ready {
            track: index,
            ts,
            duration,
            keyframe,
            packet,
        } = ready;
        if let Some(cluster) = &self.cluster
            && i16::try_from(ts - cluster.timestamp).is_err()
        {
            self.end_cluster()?;
        }
        if self.cluster.is_none() {
            let mut payload = BytesMut::new();
            if !self.webm() {
                payload.put_bytes(0, 6);
            }
            let timestamp = ts.max(0);
            put_uint(
                &mut payload,
                id::CLUSTER_TIMESTAMP,
                timestamp.unsigned_abs(),
            );
            self.cluster = Some(Cluster {
                at: self.written,
                timestamp,
                payload,
            });
        }
        let segment = self.segment;
        let video = self
            .tracks
            .iter()
            .any(|track| track.info.kind == TrackKind::Video);
        let track = &mut self.tracks[index];
        let cluster = self.cluster.as_mut().expect("open cluster");
        let relative = cluster.payload.len() as u64;
        let mut block = BytesMut::with_capacity(packet.data.len() + 12);
        put_size(&mut block, track.number, size_len(track.number));
        let offset = i16::try_from(ts - cluster.timestamp).unwrap_or(0);
        block.put_i16(offset);
        let mut extra = BytesMut::new();
        if duration > 0
            && track.default_high > 0
            && duration != track.default_high
            && duration != track.default_low
        {
            put_uint(&mut extra, id::BLOCK_DURATION, duration.unsigned_abs());
        }
        if packet.discard.1 > 0 {
            let padding = rescale(
                i64::from(packet.discard.1),
                1_000_000_000,
                i64::from(track.sample_rate.max(1)),
            );
            put_sint(&mut extra, id::DISCARD_PADDING, padding);
        }
        if extra.is_empty() {
            block.put_u8(if keyframe { 0x80 } else { 0 });
            block.put_slice(&packet.data);
            put_master(&mut cluster.payload, id::SIMPLE_BLOCK, &block);
        } else {
            block.put_u8(0);
            block.put_slice(&packet.data);
            let mut group = BytesMut::with_capacity(block.len() + extra.len() + 16);
            put_master(&mut group, id::BLOCK, &block);
            group.put_slice(&extra);
            if !keyframe {
                put_sint(&mut group, id::REFERENCE_BLOCK, track.last_ts - ts);
            }
            put_master(&mut cluster.payload, id::BLOCK_GROUP, &group);
        }
        let cued = track.info.kind == TrackKind::Video || (!video && !track.has_cue);
        if keyframe && cued {
            if ts >= 0 {
                self.cues.push(Cue {
                    ts,
                    track: index,
                    cluster: cluster.at - segment,
                    relative,
                });
            }
            track.has_cue = true;
        }
        track.last_ts = ts;
        self.duration = self.duration.max(ts + duration);
        Ok(())
    }

    fn cues_element(&self) -> BytesMut {
        let mut cues = self.cues.clone();
        cues.sort_by_key(|cue| cue.ts);
        let mut payload = BytesMut::new();
        let mut at = 0;
        while at < cues.len() {
            let ts = cues[at].ts;
            let mut point = BytesMut::new();
            put_uint(&mut point, id::CUE_TIME, ts.unsigned_abs());
            let mut seen = Vec::new();
            while at < cues.len() && cues[at].ts == ts {
                let cue = cues[at];
                at += 1;
                if seen.contains(&cue.track) {
                    continue;
                }
                seen.push(cue.track);
                let mut position = BytesMut::new();
                put_uint(&mut position, id::CUE_TRACK, self.tracks[cue.track].number);
                put_uint(&mut position, id::CUE_CLUSTER_POSITION, cue.cluster);
                put_uint(&mut position, id::CUE_RELATIVE_POSITION, cue.relative);
                put_master(&mut point, id::CUE_TRACK_POSITIONS, &position);
            }
            put_master(&mut payload, id::CUE_POINT, &point);
        }
        self.level1(id::CUES, &payload)
    }

    fn finish_inner(&mut self) -> Result<(), MuxError> {
        if !self.started {
            self.start()?;
        }
        self.pump(true)?;
        if let Some(held) = self.held_audio.take() {
            self.write_block(held)?;
        }
        self.end_cluster()?;
        let mut seeks = vec![
            (id::INFO, self.info_at - self.segment),
            (id::TRACKS, self.tracks_at - self.segment),
        ];
        if !self.cues.is_empty() {
            seeks.push((id::CUES, self.written - self.segment));
            let cues = self.cues_element();
            self.emit(&cues)?;
        }
        let end = self.written;
        self.sink.seek(SeekFrom::Start(self.segment - 8))?;
        let mut size = BytesMut::new();
        put_size(&mut size, end - self.segment, 8);
        self.sink.write_all(&size)?;
        let mut entries = BytesMut::new();
        for (element, position) in seeks {
            let mut seek = BytesMut::new();
            let mut target = BytesMut::new();
            put_id(&mut target, element);
            put_master(&mut seek, id::SEEK_ID, &target);
            put_uint(&mut seek, id::SEEK_POSITION, position);
            put_master(&mut entries, id::SEEK, &seek);
        }
        let mut seek_head = self.level1(id::SEEK_HEAD, &entries);
        let filler = SEEK_HEAD_SPACE - seek_head.len();
        put_void(&mut seek_head, filler);
        self.sink.seek(SeekFrom::Start(self.segment))?;
        self.sink.write_all(&seek_head)?;
        let info = self.info();
        self.sink.seek(SeekFrom::Start(self.info_at))?;
        self.sink.write_all(&info)?;
        self.sink.seek(SeekFrom::Start(end))?;
        self.sink.flush()?;
        Ok(())
    }
}

/// The H.273 codes of `color`, in the order `FFmpeg` writes them.
fn colour(color: alpha2_media::ColorMetadata) -> BytesMut {
    let transfer = match color.transfer {
        TransferFunction::Bt709 => 1,
        TransferFunction::Srgb => 13,
        TransferFunction::Pq => 16,
        TransferFunction::Hlg => 18,
    };
    let matrix = match color.matrix {
        ColorMatrix::Rgb => 0,
        ColorMatrix::Bt709 => 1,
        ColorMatrix::Bt601 => 6,
        ColorMatrix::Bt2020Ncl => 9,
    };
    let primaries = match color.primaries {
        ColorPrimaries::Bt709 => 1,
        ColorPrimaries::Bt601 => 6,
        ColorPrimaries::Bt2020 => 9,
    };
    let range = if color.range == ColorRange::Full {
        2
    } else {
        1
    };
    let mut payload = BytesMut::new();
    put_uint(&mut payload, id::TRANSFER_CHARACTERISTICS, transfer);
    put_uint(&mut payload, id::MATRIX_COEFFICIENTS, matrix);
    put_uint(&mut payload, id::PRIMARIES, primaries);
    put_uint(&mut payload, id::RANGE, range);
    payload
}

/// The projection pose `FFmpeg` derives from the display matrix of `orientation`: a yaw of 180
/// for a mirror and the counter-clockwise roll.
fn pose(orientation: alpha2_media::Orientation) -> BytesMut {
    let matrix = crate::mp4::write::display_matrix(orientation);
    let mut payload = BytesMut::new();
    if orientation.rotation == Rotation::Deg0 && !orientation.mirror {
        return payload;
    }
    let yaw = if orientation.mirror { 180.0 } else { 0.0 };
    let roll = f64::from(matrix[3])
        .atan2(f64::from(matrix[4]))
        .to_degrees();
    if orientation.mirror {
        put_float(&mut payload, id::PROJECTION_POSE_YAW, yaw);
    }
    if roll != 0.0 {
        put_float(&mut payload, id::PROJECTION_POSE_ROLL, roll);
    }
    payload
}

/// The codec-specific parts of a `TrackEntry`.
struct Parts {
    codec_id: &'static str,
    /// `CodecDelay` and `SeekPreRoll`, which `FFmpeg` writes before the track type.
    delay: BytesMut,
    kind: u64,
    /// `DefaultDuration` in nanoseconds, from a frame or sample rate.
    default_duration: Option<(u64, u64)>,
    fields: BytesMut,
    private: BytesMut,
}

fn video_parts(video: &VideoTrack, webm: bool) -> Result<Parts, MuxError> {
    let mut private = BytesMut::new();
    let codec_id = match video.codec {
        VideoCodec::Vp8 => "V_VP8",
        // Matroska's VP9 private data is a feature list, not the `vpcC` body; none is needed.
        VideoCodec::Vp9 => "V_VP9",
        VideoCodec::Av1 => {
            private.put_slice(&video.codec_private);
            "V_AV1"
        }
        _ if webm => {
            return Err(MuxError::Unsupported(format!(
                "{:?} video is not written to WebM",
                video.codec
            )));
        }
        VideoCodec::H264 => {
            private.put_slice(&video.codec_private);
            "V_MPEG4/ISO/AVC"
        }
        VideoCodec::Hevc => {
            private.put_slice(&video.codec_private);
            "V_MPEGH/ISO/HEVC"
        }
        VideoCodec::Mpeg4Part2 => {
            private.put_slice(&video.codec_private);
            "V_MPEG4/ISO/ASP"
        }
        VideoCodec::ProRes(code) => {
            private.put_slice(&code.0);
            "V_PRORES"
        }
        VideoCodec::Other(code) => {
            return Err(MuxError::Unsupported(format!(
                "{code} video is not written to Matroska"
            )));
        }
    };
    let mut fields = BytesMut::new();
    put_uint(&mut fields, id::PIXEL_WIDTH, u64::from(video.width));
    put_uint(&mut fields, id::PIXEL_HEIGHT, u64::from(video.height));
    if video.alpha {
        put_uint(&mut fields, id::ALPHA_MODE, 1);
    }
    match video.sample_aspect_ratio {
        Some(ratio) if ratio.num != ratio.den => {
            let (width, height) = (u64::from(video.width), u64::from(video.height));
            if webm {
                let display = rescale(
                    i64::from(video.width),
                    i64::from(ratio.num),
                    i64::from(ratio.den),
                );
                put_uint(&mut fields, id::DISPLAY_WIDTH, display.unsigned_abs());
                put_uint(&mut fields, id::DISPLAY_HEIGHT, height);
            } else {
                let (num, den) = reduce(
                    width * u64::from(ratio.num),
                    height * u64::from(ratio.den),
                    1 << 20,
                );
                put_uint(&mut fields, id::DISPLAY_WIDTH, num);
                put_uint(&mut fields, id::DISPLAY_HEIGHT, den);
                put_uint(&mut fields, id::DISPLAY_UNIT, 3);
            }
        }
        None if !webm => put_uint(&mut fields, id::DISPLAY_UNIT, 4),
        _ => {}
    }
    if let Some(color) = video.color {
        put_master(&mut fields, id::COLOUR, &colour(color));
    }
    let pose = pose(video.orientation);
    if !pose.is_empty() {
        put_master(&mut fields, id::PROJECTION, &pose);
    }
    let mut out = BytesMut::new();
    put_master(&mut out, id::VIDEO, &fields);
    Ok(Parts {
        codec_id,
        delay: BytesMut::new(),
        kind: 1,
        default_duration: video
            .frame_rate
            .map(|rate| (u64::from(rate.denominator()), u64::from(rate.numerator()))),
        fields: out,
        private,
    })
}

/// The core and extension sample rates of an `AudioSpecificConfig`; the second is 0 without SBR.
fn aac_rates(config: &[u8]) -> Option<(u32, u32)> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let mut bits = crate::mp4::nal::Bits::new(config);
    let mut object_type = bits.read(5)?;
    if object_type == 31 {
        object_type = 32 + bits.read(6)?;
    }
    let rate_of = |bits: &mut crate::mp4::nal::Bits<'_>| -> Option<u32> {
        let index = bits.read(4)?;
        if index == 15 {
            bits.read(24)
        } else {
            RATES.get(index as usize).copied()
        }
    };
    let core = rate_of(&mut bits)?;
    bits.read(4)?;
    let extension = if object_type == 5 || object_type == 29 {
        rate_of(&mut bits)?
    } else {
        0
    };
    Some((core, extension))
}

fn audio_parts(audio: &AudioTrack, webm: bool) -> Result<Parts, MuxError> {
    let mut private = BytesMut::new();
    let mut sample_rate = audio.sample_rate;
    let mut output_rate = 0;
    let mut bit_depth = audio.bits_per_sample.map(u64::from);
    let mut frame_samples = None;
    let codec_id = match audio.codec {
        AudioCodec::Opus => {
            if audio.codec_private.len() < 19 || !audio.codec_private.starts_with(b"OpusHead") {
                return Err(MuxError::Unsupported("Opus without an OpusHead".into()));
            }
            private.put_slice(&audio.codec_private);
            "A_OPUS"
        }
        AudioCodec::Vorbis => {
            if xiph_headers(&audio.codec_private).is_none_or(|headers| headers.len() != 3) {
                return Err(MuxError::Unsupported(
                    "Vorbis without its three headers".into(),
                ));
            }
            private.put_slice(&audio.codec_private);
            "A_VORBIS"
        }
        _ if webm => {
            return Err(MuxError::Unsupported(format!(
                "{:?} audio is not written to WebM",
                audio.codec
            )));
        }
        AudioCodec::Aac => {
            if let Some((core, extension)) = aac_rates(&audio.codec_private) {
                sample_rate = core;
                output_rate = extension;
            }
            private.put_slice(&audio.codec_private);
            "A_AAC"
        }
        AudioCodec::Mp3 => "A_MPEG/L3",
        AudioCodec::Flac => {
            private.put_slice(&audio.codec_private);
            "A_FLAC"
        }
        AudioCodec::Alac => {
            private.put_slice(&audio.codec_private);
            "A_ALAC"
        }
        AudioCodec::Ac3 => {
            frame_samples = Some(1536);
            "A_AC3"
        }
        AudioCodec::Eac3 => "A_EAC3",
        AudioCodec::Dts => "A_DTS",
        AudioCodec::Pcm(format) => {
            bit_depth = Some(u64::from(format.bits));
            pcm_codec(format)?
        }
        AudioCodec::Other(code) => {
            return Err(MuxError::Unsupported(format!(
                "{code} audio is not written to Matroska"
            )));
        }
    };
    if sample_rate == 0 {
        return Err(MuxError::Invalid(
            "an audio track needs a sample rate".into(),
        ));
    }
    let mut fields = BytesMut::new();
    put_uint(&mut fields, id::CHANNELS, u64::from(audio.channels));
    put_float(&mut fields, id::SAMPLING_FREQUENCY, f64::from(sample_rate));
    if output_rate > 0 {
        put_float(
            &mut fields,
            id::OUTPUT_SAMPLING_FREQUENCY,
            f64::from(output_rate),
        );
    }
    if let Some(bits) = bit_depth {
        put_uint(&mut fields, id::BIT_DEPTH, bits);
    }
    let mut out = BytesMut::new();
    put_master(&mut out, id::AUDIO, &fields);
    Ok(Parts {
        codec_id,
        delay: delay_elements(audio),
        kind: 2,
        default_duration: frame_samples.map(|samples| (samples, u64::from(audio.sample_rate))),
        fields: out,
        private,
    })
}

/// `CodecDelay` for the priming, and the Opus `SeekPreRoll`.
fn delay_elements(audio: &AudioTrack) -> BytesMut {
    let mut delay = BytesMut::new();
    if audio.priming > 0 {
        let nanoseconds = rescale(
            i64::from(audio.priming),
            1_000_000_000,
            i64::from(audio.sample_rate.max(1)),
        );
        put_uint(&mut delay, id::CODEC_DELAY, nanoseconds.unsigned_abs());
    }
    if audio.codec == AudioCodec::Opus {
        put_uint(&mut delay, id::SEEK_PRE_ROLL, OPUS_SEEK_PRE_ROLL);
    }
    delay
}

/// The Matroska codec ID of a PCM layout `FFmpeg`'s muxer has one for.
fn pcm_codec(format: PcmFormat) -> Result<&'static str, MuxError> {
    match (format.bits, format.float, format.big_endian, format.signed) {
        (32 | 64, true, false, _) => Ok("A_PCM/FLOAT/IEEE"),
        (8, false, _, false) | (16 | 24 | 32, false, false, true) => Ok("A_PCM/INT/LIT"),
        (16 | 24 | 32, false, true, true) => Ok("A_PCM/INT/BIG"),
        _ => Err(MuxError::Unsupported(format!(
            "{format:?} PCM is not written to Matroska"
        ))),
    }
}

impl ContainerWriter for MatroskaWriter {
    fn add_track(&mut self, track: &TrackInfo) -> Result<TrackId, MuxError> {
        if self.started {
            return Err(MuxError::Invalid(
                "tracks are added before the first packet".into(),
            ));
        }
        if track.timescale == 0 {
            return Err(MuxError::Invalid("a track needs a timescale".into()));
        }
        let webm = self.webm();
        let parts = match (track.kind, &track.video, &track.audio) {
            (TrackKind::Video, Some(video), _) => video_parts(video, webm)?,
            (TrackKind::Audio, _, Some(audio)) => audio_parts(audio, webm)?,
            _ => {
                return Err(MuxError::Unsupported(
                    "only video and audio tracks are written to Matroska".into(),
                ));
            }
        };
        let number = self.tracks.len() as u64 + 1;
        if number > 126 {
            return Err(MuxError::Unsupported("too many tracks".into()));
        }
        let mut payload = BytesMut::new();
        put_uint(&mut payload, id::TRACK_NUMBER, number);
        put_uint(&mut payload, id::TRACK_UID, number);
        put_uint(&mut payload, id::FLAG_LACING, 0);
        if let Some(title) = &track.title {
            put_master(&mut payload, id::NAME, title.as_bytes());
        }
        let language = track.language.as_deref().filter(|code| !code.is_empty());
        put_master(
            &mut payload,
            id::LANGUAGE,
            language.unwrap_or("und").as_bytes(),
        );
        if !track.default {
            put_uint(&mut payload, id::FLAG_DEFAULT, 0);
        }
        if !track.enabled {
            put_uint(&mut payload, id::FLAG_ENABLED, 0);
        }
        put_master(&mut payload, id::CODEC_ID, parts.codec_id.as_bytes());
        payload.put_slice(&parts.delay);
        put_uint(&mut payload, id::TRACK_TYPE, parts.kind);
        let (mut default_low, mut default_high) = (0, 0);
        if let Some((num, den)) = parts
            .default_duration
            .filter(|(num, den)| *num > 0 && *den > 0)
        {
            put_uint(
                &mut payload,
                id::DEFAULT_DURATION,
                1_000_000_000 * num / den,
            );
            default_low = i64::try_from(1000 * num / den).unwrap_or(0);
            default_high = default_low + i64::from((1000 * num) % den != 0);
        }
        payload.put_slice(&parts.fields);
        if !parts.private.is_empty() {
            put_master(&mut payload, id::CODEC_PRIVATE, &parts.private);
        }
        let mut entry = BytesMut::new();
        put_master(&mut entry, id::TRACK_ENTRY, &payload);
        let sample_rate = track.audio.as_ref().map_or(0, |audio| audio.sample_rate);
        let ts_offset = track.audio.as_ref().map_or(0, |audio| {
            rescale(
                i64::from(audio.priming),
                1000,
                i64::from(audio.sample_rate.max(1)),
            )
        });
        self.tracks.push(Track {
            info: track.clone(),
            number,
            entry,
            ts_offset,
            default_low,
            default_high,
            sample_rate,
            queue: VecDeque::new(),
            last_dts: None,
            last_ts: 0,
            has_cue: false,
        });
        Ok(TrackId(u32::try_from(number).unwrap_or(u32::MAX)))
    }

    fn write(&mut self, packet: Packet) -> Result<(), MuxError> {
        let index = usize::try_from(packet.track.0)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .filter(|index| *index < self.tracks.len())
            .ok_or_else(|| MuxError::Invalid(format!("no track {}", packet.track.0)))?;
        if !self.started {
            self.start()?;
        }
        let track = &mut self.tracks[index];
        if track.last_dts.is_some_and(|last| packet.dts < last) {
            return Err(MuxError::Invalid(format!(
                "track {} decode times go back at {}",
                packet.track.0, packet.dts
            )));
        }
        track.last_dts = Some(packet.dts);
        track.queue.push_back(packet);
        self.pump(false)
    }

    fn finish(mut self: Box<Self>) -> Result<(), MuxError> {
        self.finish_inner()
    }
}
