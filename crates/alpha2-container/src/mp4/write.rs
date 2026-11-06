//! The MP4/MOV muxer. Timescales, edit lists and durations follow `FFmpeg`'s MOV muxer, so
//! `FFmpeg` reads the output as it reads its own stream copy; output bytes depend only on the
//! packets.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};

use alpha2_media::{
    ColorMatrix, ColorPrimaries, ColorRange, Orientation, Rotation, TransferFunction,
};
use bytes::{BufMut, Bytes, BytesMut};

use super::index::rescale;
use crate::{
    AudioCodec, AudioTrack, ContainerWriter, FourCc, MuxError, Packet, TrackId, TrackInfo,
    TrackKind, VideoCodec, VideoTrack,
};

/// Which brand of ISO base media file to write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mp4Flavor {
    /// ISO MP4 (`isom` brand).
    Mp4,
    /// `QuickTime` MOV (`qt  ` brand), which also carries `ProRes`.
    Mov,
}

/// The movie timescale `FFmpeg` writes.
const MOVIE_TIMESCALE: u32 = 1000;
/// Bytes of buffered media after which chunks are written without waiting for other tracks.
const MAX_BUFFERED: usize = 64 << 20;
/// Block size when the media data moves to make room for the index.
const MOVE_BLOCK: usize = 1 << 20;

trait Sink: Read + Write + Seek + Send {}

impl<T: Read + Write + Seek + Send> Sink for T {}

/// Writes MP4 or MOV files with the index before the media data (faststart), tracks interleaved
/// in chunks of about half a second.
pub struct Mp4Writer {
    sink: Box<dyn Sink>,
    flavor: Mp4Flavor,
    tracks: Vec<Track>,
    started: bool,
    /// Where the 16 bytes before the media data start: a `free` box and the `mdat` header.
    mdat_at: u64,
    /// Media bytes written so far.
    written: u64,
    buffered: usize,
}

impl std::fmt::Debug for Mp4Writer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Mp4Writer")
            .field("flavor", &self.flavor)
            .field("tracks", &self.tracks.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct Sample {
    size: u32,
    dts: i64,
    cts: i64,
    duration: i64,
    keyframe: bool,
}

struct Chunk {
    start: i64,
    samples: Vec<(Sample, Bytes)>,
}

struct Track {
    info: TrackInfo,
    /// The sample entry box, built when the track is added.
    entry: Bytes,
    in_timescale: u32,
    timescale: u32,
    /// The duration a packet without one gets from the frame rate, in `timescale` units.
    nominal_duration: Option<i64>,
    pending: VecDeque<Chunk>,
    last_dts: Option<i64>,
    samples: Vec<Sample>,
    /// Chunk offsets from the start of the media data, and their sample counts.
    chunks: Vec<(u64, u32)>,
}

impl Mp4Writer {
    /// A writer into `file`, which must be open for reading as well as writing
    /// (`OpenOptions::new().read(true).write(true)`): `finish` moves the media data.
    #[must_use]
    pub fn create(file: std::fs::File, flavor: Mp4Flavor) -> Self {
        Self::new(file, flavor)
    }

    /// A writer into any readable, writable and seekable `sink`, starting at its offset 0.
    pub fn new<W: Read + Write + Seek + Send + 'static>(sink: W, flavor: Mp4Flavor) -> Self {
        Self {
            sink: Box::new(sink),
            flavor,
            tracks: Vec::new(),
            started: false,
            mdat_at: 0,
            written: 0,
            buffered: 0,
        }
    }

    fn start(&mut self) -> Result<(), MuxError> {
        let ftyp = self.ftyp();
        self.sink.seek(SeekFrom::Start(0))?;
        self.sink.write_all(&ftyp)?;
        self.mdat_at = ftyp.len() as u64;
        let mut header = BytesMut::with_capacity(16);
        header.put_u32(8);
        header.put_slice(if self.flavor == Mp4Flavor::Mov {
            b"wide"
        } else {
            b"free"
        });
        header.put_u32(8);
        header.put_slice(b"mdat");
        self.sink.write_all(&header)?;
        self.started = true;
        Ok(())
    }

    fn ftyp(&self) -> Bytes {
        let mut brands: Vec<&[u8; 4]> = Vec::new();
        let major: &[u8; 4] = match self.flavor {
            Mp4Flavor::Mov => {
                brands.push(b"qt  ");
                b"qt  "
            }
            Mp4Flavor::Mp4 => {
                brands.extend([b"isom", b"iso2"]);
                if self.tracks.iter().any(|track| matches!(&track.info.video, Some(video) if video.codec == VideoCodec::H264)) {
                    brands.push(b"avc1");
                }
                if self.tracks.iter().any(|track| matches!(&track.info.video, Some(video) if video.codec == VideoCodec::Av1)) {
                    brands.push(b"av01");
                }
                brands.push(b"mp41");
                b"isom"
            }
        };
        let mut payload = BytesMut::new();
        payload.put_slice(major);
        payload.put_u32(0x200);
        for brand in brands {
            payload.put_slice(brand);
        }
        boxed(*b"ftyp", &payload)
    }

    /// Writes chunks that no other track can still precede; with `all`, every chunk.
    fn flush(&mut self, all: bool) -> Result<(), MuxError> {
        loop {
            let Some((track, forced)) = self.next_chunk(all) else {
                return Ok(());
            };
            if !forced && !all && !self.ready(track) {
                return Ok(());
            }
            let chunk = self.tracks[track].pending.pop_front().expect("chunk");
            let offset = self.written;
            let mut count = 0_u32;
            for (sample, data) in &chunk.samples {
                self.sink.write_all(data)?;
                self.written += data.len() as u64;
                self.buffered -= data.len();
                self.tracks[track].samples.push(*sample);
                count += 1;
            }
            self.tracks[track].chunks.push((offset, count));
        }
    }

    /// The earliest chunk that may be written: complete chunks (every chunk but each track's
    /// last, or all of them with `all`), or any chunk once too much is buffered. The flag tells
    /// whether memory forces it.
    fn next_chunk(&self, all: bool) -> Option<(usize, bool)> {
        let forced = self.buffered > MAX_BUFFERED;
        let mut best: Option<(usize, i64, u32)> = None;
        for (position, track) in self.tracks.iter().enumerate() {
            let complete = if all || forced {
                track.pending.len()
            } else {
                track.pending.len().saturating_sub(1)
            };
            if complete == 0 {
                continue;
            }
            let start = track.pending[0].start;
            let earlier = best.is_none_or(|(_, best_start, best_scale)| {
                i128::from(start) * i128::from(best_scale)
                    < i128::from(best_start) * i128::from(track.timescale)
            });
            if earlier {
                best = Some((position, start, track.timescale));
            }
        }
        best.map(|(position, _, _)| (position, forced))
    }

    /// Whether every other track has moved past the start of `track`'s first chunk.
    fn ready(&self, track: usize) -> bool {
        let candidate = &self.tracks[track];
        let start = i128::from(candidate.pending[0].start);
        self.tracks.iter().enumerate().all(|(position, other)| {
            if position == track {
                return true;
            }
            let reached = match (other.pending.front(), other.last_dts) {
                (Some(chunk), _) => chunk.start,
                (None, Some(last)) => last,
                (None, None) => return false,
            };
            i128::from(reached) * i128::from(candidate.timescale)
                >= start * i128::from(other.timescale)
        })
    }

    fn finish_inner(&mut self) -> Result<(), MuxError> {
        if !self.started {
            self.start()?;
        }
        self.flush(true)?;
        let header_len = 16;
        let data_end = self.mdat_at + header_len + self.written;
        let mdat_size = 8 + self.written;
        let mut header = BytesMut::with_capacity(16);
        if let Ok(size) = u32::try_from(mdat_size) {
            header.put_u32(8);
            header.put_slice(if self.flavor == Mp4Flavor::Mov {
                b"wide"
            } else {
                b"free"
            });
            header.put_u32(size);
            header.put_slice(b"mdat");
        } else {
            header.put_u32(1);
            header.put_slice(b"mdat");
            header.put_u64(16 + self.written);
        }
        self.sink.seek(SeekFrom::Start(self.mdat_at))?;
        self.sink.write_all(&header)?;
        let compact_len = self.moov(0, false).len() as u64;
        let last_chunk = self
            .tracks
            .iter()
            .flat_map(|track| track.chunks.iter().map(|chunk| chunk.0))
            .max()
            .unwrap_or(0);
        let compact_base = self.mdat_at + compact_len + header_len;
        let moov = if u32::try_from(compact_base + last_chunk).is_ok() {
            self.moov(compact_base, false)
        } else {
            let wide_len = self.moov(0, true).len() as u64;
            self.moov(self.mdat_at + wide_len + header_len, true)
        };
        move_forward(
            self.sink.as_mut(),
            self.mdat_at,
            data_end,
            moov.len() as u64,
        )?;
        self.sink.seek(SeekFrom::Start(self.mdat_at))?;
        self.sink.write_all(&moov)?;
        self.sink.flush()?;
        Ok(())
    }

    fn moov(&self, base: u64, wide: bool) -> Bytes {
        let mut payload = BytesMut::new();
        let durations = self
            .tracks
            .iter()
            .filter(|track| !track.samples.is_empty())
            .map(|track| {
                rescale_up(
                    track.end_pts(),
                    i64::from(MOVIE_TIMESCALE),
                    i64::from(track.timescale),
                )
            })
            .max()
            .unwrap_or(0);
        payload.put_slice(&mvhd(
            durations,
            u32::try_from(self.tracks.len() + 1).unwrap_or(u32::MAX),
        ));
        for (position, track) in self.tracks.iter().enumerate() {
            let id = u32::try_from(position + 1).unwrap_or(u32::MAX);
            payload.put_slice(&track.trak(id, base, wide, self.flavor));
        }
        boxed(*b"moov", &payload)
    }
}

/// Moves `[from, to)` forward by `by` bytes, from the end backwards.
fn move_forward(sink: &mut dyn Sink, from: u64, to: u64, by: u64) -> Result<(), MuxError> {
    let mut buffer = vec![0; MOVE_BLOCK];
    let mut end = to;
    while end > from {
        let length = usize::try_from((end - from).min(MOVE_BLOCK as u64)).unwrap_or(MOVE_BLOCK);
        let start = end - length as u64;
        sink.seek(SeekFrom::Start(start))?;
        sink.read_exact(&mut buffer[..length])?;
        sink.seek(SeekFrom::Start(start + by))?;
        sink.write_all(&buffer[..length])?;
        end = start;
    }
    Ok(())
}

/// `a * b / c` rounded up, for positive `c`.
fn rescale_up(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 {
        return 0;
    }
    let product = i128::from(a) * i128::from(b);
    let c = i128::from(c);
    let quotient = product.div_euclid(c) + i128::from(product.rem_euclid(c) != 0);
    i64::try_from(quotient).unwrap_or(i64::MAX)
}

/// `a * b / c` rounded down, for positive `c`.
fn rescale_down(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 {
        return 0;
    }
    let quotient = (i128::from(a) * i128::from(b)).div_euclid(i128::from(c));
    i64::try_from(quotient).unwrap_or(i64::MIN)
}

fn boxed(kind: [u8; 4], payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(payload.len() + 16);
    if let Ok(size) = u32::try_from(payload.len() + 8) {
        out.put_u32(size);
        out.put_slice(&kind);
    } else {
        out.put_u32(1);
        out.put_slice(&kind);
        out.put_u64(payload.len() as u64 + 16);
    }
    out.put_slice(payload);
    out.freeze()
}

fn full_box(kind: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Bytes {
    let mut body = BytesMut::with_capacity(payload.len() + 4);
    body.put_u32((u32::from(version) << 24) | (flags & 0x00ff_ffff));
    body.put_slice(payload);
    boxed(kind, &body)
}

/// Writes a non-negative duration or offset as 64 bits in a version 1 box, else 32.
fn put_wide(out: &mut BytesMut, version: u8, value: i64) {
    let value = u64::try_from(value).unwrap_or(0);
    if version == 1 {
        out.put_u64(value);
    } else {
        out.put_u32(u32::try_from(value).unwrap_or(u32::MAX));
    }
}

fn put_matrix(out: &mut BytesMut, matrix: [i32; 9]) {
    for value in matrix {
        out.put_i32(value);
    }
}

const IDENTITY: [i32; 9] = [1 << 16, 0, 0, 0, 1 << 16, 0, 0, 0, 1 << 30];

/// The display matrix the reader turns back into `orientation`: the clockwise rotation, its first
/// column negated for a mirror (as `FFmpeg`'s `-display_hflip`).
fn display_matrix(orientation: Orientation) -> [i32; 9] {
    let (cos, sin): (i32, i32) = match orientation.rotation {
        Rotation::Deg0 => (1, 0),
        Rotation::Deg90 => (0, 1),
        Rotation::Deg180 => (-1, 0),
        Rotation::Deg270 => (0, -1),
    };
    let mut matrix = [
        cos << 16,
        sin << 16,
        0,
        -sin << 16,
        cos << 16,
        0,
        0,
        0,
        1 << 30,
    ];
    if orientation.mirror {
        matrix[0] = -cos << 16;
        matrix[1] = -sin << 16;
    }
    matrix
}

fn mvhd(duration: i64, next_track: u32) -> Bytes {
    let version = u8::from(duration >= i64::from(i32::MAX));
    let mut payload = BytesMut::new();
    put_wide(&mut payload, version, 0);
    put_wide(&mut payload, version, 0);
    payload.put_u32(MOVIE_TIMESCALE);
    put_wide(&mut payload, version, duration);
    payload.put_u32(0x0001_0000);
    payload.put_u16(0x0100);
    payload.put_slice(&[0; 10]);
    put_matrix(&mut payload, IDENTITY);
    payload.put_slice(&[0; 24]);
    payload.put_u32(next_track);
    full_box(*b"mvhd", version, 0, &payload)
}

/// An `esds` box for MPEG-4 object type `object_type` and stream type `stream_type`.
fn esds(
    track_id: u16,
    object_type: u8,
    audio: bool,
    info: &[u8],
    bit_rates: (u32, u32, u32),
) -> Bytes {
    fn descriptor(out: &mut BytesMut, tag: u8, length: usize) {
        out.put_u8(tag);
        let length = u32::try_from(length).unwrap_or(0x0fff_ffff);
        out.put_u8(0x80 | ((length >> 21) & 0x7f) as u8);
        out.put_u8(0x80 | ((length >> 14) & 0x7f) as u8);
        out.put_u8(0x80 | ((length >> 7) & 0x7f) as u8);
        out.put_u8((length & 0x7f) as u8);
    }
    let info_len = if info.is_empty() { 0 } else { 5 + info.len() };
    let mut payload = BytesMut::new();
    descriptor(&mut payload, 3, 3 + 5 + 13 + info_len + 5 + 1);
    payload.put_u16(track_id);
    payload.put_u8(0);
    descriptor(&mut payload, 4, 13 + info_len);
    payload.put_u8(object_type);
    payload.put_u8(if audio { 0x15 } else { 0x11 });
    let (buffer, max, average) = bit_rates;
    payload.put_uint(u64::from(buffer.min(0x00ff_ffff)), 3);
    payload.put_u32(max);
    payload.put_u32(average);
    if !info.is_empty() {
        descriptor(&mut payload, 5, info.len());
        payload.put_slice(info);
    }
    descriptor(&mut payload, 6, 1);
    payload.put_u8(2);
    full_box(*b"esds", 0, 0, &payload)
}

/// The `dOps` box for an Ogg `OpusHead`.
fn dops(head: &[u8]) -> Option<Bytes> {
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        return None;
    }
    let channels = head[9];
    let family = head[18];
    let mut payload = BytesMut::new();
    payload.put_u8(0);
    payload.put_u8(channels);
    payload.put_u16(u16::from_le_bytes([head[10], head[11]]));
    payload.put_u32(u32::from_le_bytes([head[12], head[13], head[14], head[15]]));
    payload.put_u16(u16::from_le_bytes([head[16], head[17]]));
    payload.put_u8(family);
    if family != 0 {
        payload.put_slice(head.get(19..21 + usize::from(channels))?);
    }
    Some(boxed(*b"dOps", &payload))
}

fn color_box(color: alpha2_media::ColorMetadata) -> Bytes {
    let primaries: u16 = match color.primaries {
        ColorPrimaries::Bt601 => 6,
        ColorPrimaries::Bt709 => 1,
        ColorPrimaries::Bt2020 => 9,
    };
    let transfer: u16 = match color.transfer {
        TransferFunction::Bt709 => 1,
        TransferFunction::Srgb => 13,
        TransferFunction::Pq => 16,
        TransferFunction::Hlg => 18,
    };
    let matrix: u16 = match color.matrix {
        ColorMatrix::Bt601 => 6,
        ColorMatrix::Bt709 => 1,
        ColorMatrix::Bt2020Ncl => 9,
        ColorMatrix::Rgb => 0,
    };
    let mut payload = BytesMut::new();
    payload.put_slice(b"nclx");
    payload.put_u16(primaries);
    payload.put_u16(transfer);
    payload.put_u16(matrix);
    payload.put_u8(if color.range == ColorRange::Full {
        0x80
    } else {
        0
    });
    boxed(*b"colr", &payload)
}

fn video_entry(video: &VideoTrack, flavor: Mp4Flavor) -> Result<Bytes, MuxError> {
    let (format, config): (FourCc, Option<Bytes>) = match video.codec {
        VideoCodec::H264 => (
            FourCc(if video.in_band_parameter_sets {
                *b"avc3"
            } else {
                *b"avc1"
            }),
            Some(boxed(*b"avcC", &video.codec_private)),
        ),
        VideoCodec::Hevc => (
            FourCc(if video.in_band_parameter_sets {
                *b"hev1"
            } else {
                *b"hvc1"
            }),
            Some(boxed(*b"hvcC", &video.codec_private)),
        ),
        VideoCodec::Av1 => (
            FourCc(*b"av01"),
            Some(boxed(*b"av1C", &video.codec_private)),
        ),
        VideoCodec::Vp9 => {
            if video.codec_private.len() < 4 {
                return Err(MuxError::Unsupported(
                    "VP9 needs its vpcC configuration to be written to MP4".into(),
                ));
            }
            (
                FourCc(*b"vp09"),
                Some(boxed(*b"vpcC", &video.codec_private)),
            )
        }
        VideoCodec::Mpeg4Part2 => (FourCc(*b"mp4v"), None),
        VideoCodec::ProRes(code) if flavor == Mp4Flavor::Mov => (code, None),
        VideoCodec::ProRes(_) => {
            return Err(MuxError::Unsupported(
                "ProRes is written to MOV, not MP4".into(),
            ));
        }
        VideoCodec::Vp8 | VideoCodec::Other(_) => {
            return Err(MuxError::Unsupported(format!(
                "{:?} video is not written to MP4 or MOV",
                video.codec
            )));
        }
    };
    if matches!(
        video.codec,
        VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1
    ) && video.codec_private.is_empty()
    {
        return Err(MuxError::Unsupported(format!(
            "{:?} needs its decoder configuration record",
            video.codec
        )));
    }
    let (Ok(width), Ok(height)) = (u16::try_from(video.width), u16::try_from(video.height)) else {
        return Err(MuxError::Unsupported(
            "the picture is too large for a sample entry".into(),
        ));
    };
    let mut payload = BytesMut::new();
    payload.put_slice(&[0; 6]);
    payload.put_u16(1);
    payload.put_slice(&[0; 16]);
    payload.put_u16(width);
    payload.put_u16(height);
    payload.put_u32(0x0048_0000);
    payload.put_u32(0x0048_0000);
    payload.put_u32(0);
    payload.put_u16(1);
    payload.put_slice(&[0; 32]);
    payload.put_u16(0x18);
    payload.put_i16(-1);
    if let Some(config) = config {
        payload.put_slice(&config);
    }
    if video.codec == VideoCodec::Mpeg4Part2 {
        payload.put_slice(&esds(0, 0x20, false, &video.codec_private, (0, 0, 0)));
    }
    if let Some(color) = video.color {
        payload.put_slice(&color_box(color));
    }
    if let Some(ratio) = video.sample_aspect_ratio {
        let mut pasp = BytesMut::new();
        pasp.put_u32(ratio.num);
        pasp.put_u32(ratio.den);
        payload.put_slice(&boxed(*b"pasp", &pasp));
    }
    Ok(boxed(format.0, &payload))
}

fn audio_entry(audio: &AudioTrack, flavor: Mp4Flavor) -> Result<Bytes, MuxError> {
    let mut channels = audio.channels;
    let mut rate = audio.sample_rate;
    let (format, config): (&[u8; 4], Bytes) = match audio.codec {
        AudioCodec::Aac => (b"mp4a", Bytes::new()),
        AudioCodec::Opus if flavor == Mp4Flavor::Mp4 => {
            let config = dops(&audio.codec_private).ok_or_else(|| {
                MuxError::Unsupported("Opus needs its OpusHead to be written to MP4".into())
            })?;
            channels = u32::from(audio.codec_private[9]);
            rate = 48_000;
            (b"Opus", config)
        }
        codec => {
            return Err(MuxError::Unsupported(format!(
                "{codec:?} audio is not written to {}",
                if flavor == Mp4Flavor::Mov {
                    "MOV"
                } else {
                    "MP4"
                }
            )));
        }
    };
    let mut payload = BytesMut::new();
    payload.put_slice(&[0; 6]);
    payload.put_u16(1);
    payload.put_slice(&[0; 8]);
    payload.put_u16(u16::try_from(channels).unwrap_or(2));
    payload.put_u16(16);
    payload.put_u16(0);
    payload.put_u16(0);
    payload.put_u32(if rate <= 0xffff { rate << 16 } else { 0 });
    payload.put_slice(&config);
    Ok(boxed(*format, &payload))
}

fn language_code(language: Option<&str>) -> u16 {
    let code = language.unwrap_or("und");
    let letters = code.as_bytes();
    if letters.len() == 3 && letters.iter().all(u8::is_ascii_lowercase) {
        letters
            .iter()
            .fold(0, |packed, letter| (packed << 5) | u16::from(letter - 0x60))
    } else {
        0x55c4
    }
}

impl Track {
    /// The presentation end: the latest `pts + duration`.
    fn end_pts(&self) -> i64 {
        self.samples
            .iter()
            .map(|sample| {
                sample
                    .dts
                    .saturating_add(sample.cts)
                    .saturating_add(sample.duration)
            })
            .max()
            .unwrap_or(0)
    }

    /// The first sample's presentation time.
    fn start_pts(&self) -> i64 {
        self.samples
            .first()
            .map_or(0, |sample| sample.dts.saturating_add(sample.cts))
    }

    fn trak(&self, id: u32, base: u64, wide: bool, flavor: Mp4Flavor) -> Bytes {
        let mut payload = BytesMut::new();
        payload.put_slice(&self.tkhd(id));
        if self.samples.first().is_some_and(|first| first.dts != 0)
            || self.samples.iter().any(|sample| sample.cts != 0)
        {
            payload.put_slice(&self.edts());
        }
        payload.put_slice(&self.mdia(base, wide, flavor));
        boxed(*b"trak", &payload)
    }

    fn tkhd(&self, id: u32) -> Bytes {
        let duration = if self.samples.is_empty() {
            0
        } else {
            rescale_up(
                self.end_pts(),
                i64::from(MOVIE_TIMESCALE),
                i64::from(self.timescale),
            )
        };
        let version = u8::from(duration >= i64::from(i32::MAX));
        let mut payload = BytesMut::new();
        put_wide(&mut payload, version, 0);
        put_wide(&mut payload, version, 0);
        payload.put_u32(id);
        payload.put_u32(0);
        put_wide(&mut payload, version, duration);
        payload.put_u64(0);
        payload.put_u16(0);
        payload.put_u16(u16::from(self.info.kind == TrackKind::Audio));
        payload.put_u16(if self.info.kind == TrackKind::Audio {
            0x0100
        } else {
            0
        });
        payload.put_u16(0);
        let matrix = self
            .info
            .video
            .as_ref()
            .map_or(IDENTITY, |video| display_matrix(video.orientation));
        put_matrix(&mut payload, matrix);
        match &self.info.video {
            Some(video) => {
                payload.put_u32(video.width << 16);
                payload.put_u32(video.height << 16);
            }
            None => payload.put_u64(0),
        }
        let flags = 2 | u32::from(self.info.enabled);
        full_box(*b"tkhd", version, flags, &payload)
    }

    /// The edit list `FFmpeg` writes: an empty edit for a positive start, then the media from
    /// its first presented sample.
    fn edts(&self) -> Bytes {
        let timescale = i64::from(self.timescale);
        let first = self.samples[0];
        let samples_duration = self.end_pts().saturating_sub(self.start_pts());
        let mut duration = rescale_up(samples_duration, i64::from(MOVIE_TIMESCALE), timescale);
        let delay = rescale_down(
            first.dts.saturating_add(first.cts),
            i64::from(MOVIE_TIMESCALE),
            timescale,
        );
        let mut start = first.cts;
        if delay <= 0 {
            start = -first.dts.min(0);
            duration = duration.saturating_add(delay);
        }
        let version = u8::from(duration >= i64::from(i32::MAX) || delay >= i64::from(i32::MAX));
        let mut payload = BytesMut::new();
        payload.put_u32(1 + u32::from(delay > 0));
        let mut entry = |segment: i64, media: i64| {
            put_wide(&mut payload, version, segment);
            if version == 1 {
                payload.put_i64(media);
            } else {
                payload.put_i32(i32::try_from(media).unwrap_or(-1));
            }
            payload.put_u32(0x0001_0000);
        };
        if delay > 0 {
            entry(delay, -1);
        }
        entry(duration, start);
        boxed(*b"edts", &full_box(*b"elst", version, 0, &payload))
    }

    fn mdia(&self, base: u64, wide: bool, flavor: Mp4Flavor) -> Bytes {
        let duration = if self.samples.is_empty() {
            0
        } else {
            self.end_pts().saturating_sub(self.start_pts())
        };
        let version = u8::from(duration >= i64::from(i32::MAX));
        let mut mdhd = BytesMut::new();
        put_wide(&mut mdhd, version, 0);
        put_wide(&mut mdhd, version, 0);
        mdhd.put_u32(self.timescale);
        put_wide(&mut mdhd, version, duration);
        mdhd.put_u16(language_code(self.info.language.as_deref()));
        mdhd.put_u16(0);
        let (handler, name): (&[u8; 4], &str) = match self.info.kind {
            TrackKind::Video => (b"vide", "VideoHandler"),
            _ => (b"soun", "SoundHandler"),
        };
        let mut hdlr = BytesMut::new();
        hdlr.put_slice(if flavor == Mp4Flavor::Mov {
            b"mhlr"
        } else {
            &[0; 4]
        });
        hdlr.put_slice(handler);
        hdlr.put_slice(&[0; 12]);
        if flavor == Mp4Flavor::Mov {
            hdlr.put_u8(u8::try_from(name.len()).unwrap_or(0));
            hdlr.put_slice(name.as_bytes());
        } else {
            hdlr.put_slice(name.as_bytes());
            hdlr.put_u8(0);
        }
        let mut payload = BytesMut::new();
        payload.put_slice(&full_box(*b"mdhd", version, 0, &mdhd));
        payload.put_slice(&full_box(*b"hdlr", 0, 0, &hdlr));
        payload.put_slice(&self.minf(base, wide));
        boxed(*b"mdia", &payload)
    }

    fn minf(&self, base: u64, wide: bool) -> Bytes {
        let mut payload = BytesMut::new();
        if self.info.kind == TrackKind::Video {
            payload.put_slice(&full_box(*b"vmhd", 0, 1, &[0; 8]));
        } else {
            payload.put_slice(&full_box(*b"smhd", 0, 0, &[0; 4]));
        }
        let url = full_box(*b"url ", 0, 1, &[]);
        let mut dref = BytesMut::new();
        dref.put_u32(1);
        dref.put_slice(&url);
        payload.put_slice(&boxed(*b"dinf", &full_box(*b"dref", 0, 0, &dref)));
        payload.put_slice(&self.stbl(base, wide));
        boxed(*b"minf", &payload)
    }

    fn stbl(&self, base: u64, wide: bool) -> Bytes {
        let mut payload = BytesMut::new();
        let mut descriptions = BytesMut::new();
        descriptions.put_u32(1);
        descriptions.put_slice(&self.entry_with_rates());
        payload.put_slice(&full_box(*b"stsd", 0, 0, &descriptions));
        payload.put_slice(&self.stts());
        if self.samples.iter().any(|sample| sample.cts != 0) {
            payload.put_slice(&self.ctts());
        }
        if self.samples.iter().any(|sample| !sample.keyframe) {
            let keys = self
                .samples
                .iter()
                .enumerate()
                .filter(|(_, sample)| sample.keyframe)
                .map(|(at, _)| u32::try_from(at + 1).unwrap_or(u32::MAX))
                .collect::<Vec<_>>();
            let mut sync = BytesMut::new();
            sync.put_u32(u32::try_from(keys.len()).unwrap_or(u32::MAX));
            for key in keys {
                sync.put_u32(key);
            }
            payload.put_slice(&full_box(*b"stss", 0, 0, &sync));
        }
        payload.put_slice(&self.stsc());
        payload.put_slice(&self.stsz());
        let mut offsets = BytesMut::new();
        offsets.put_u32(u32::try_from(self.chunks.len()).unwrap_or(u32::MAX));
        for (offset, _) in &self.chunks {
            if wide {
                offsets.put_u64(base + offset);
            } else {
                offsets.put_u32(u32::try_from(base + offset).unwrap_or(u32::MAX));
            }
        }
        payload.put_slice(&full_box(
            if wide { *b"co64" } else { *b"stco" },
            0,
            0,
            &offsets,
        ));
        boxed(*b"stbl", &payload)
    }

    /// The sample entry; an `mp4a` entry gets its `esds` here, once bit rates are known.
    fn entry_with_rates(&self) -> Bytes {
        let Some(audio) = &self.info.audio else {
            return self.entry.clone();
        };
        if audio.codec != AudioCodec::Aac {
            return self.entry.clone();
        }
        let total: u64 = self
            .samples
            .iter()
            .map(|sample| u64::from(sample.size))
            .sum();
        let largest = self
            .samples
            .iter()
            .map(|sample| sample.size)
            .max()
            .unwrap_or(0);
        let span = self
            .end_pts()
            .saturating_sub(self.samples.first().map_or(0, |sample| sample.dts));
        let average = if span > 0 {
            u32::try_from(rescale_down(
                i64::try_from(total * 8).unwrap_or(i64::MAX),
                i64::from(self.timescale),
                span,
            ))
            .unwrap_or(u32::MAX)
        } else {
            0
        };
        let esds = esds(
            0,
            0x40,
            true,
            &audio.codec_private,
            (largest, average, average),
        );
        let mut entry = BytesMut::from(&self.entry[..]);
        entry.put_slice(&esds);
        let size = u32::try_from(entry.len()).unwrap_or(u32::MAX);
        entry[..4].copy_from_slice(&size.to_be_bytes());
        entry.freeze()
    }

    fn stts(&self) -> Bytes {
        let mut entries: Vec<(u32, u32)> = Vec::new();
        for (at, sample) in self.samples.iter().enumerate() {
            let delta = match self.samples.get(at + 1) {
                Some(next) => next.dts.saturating_sub(sample.dts),
                None => sample.duration,
            };
            let delta = u32::try_from(delta.max(0)).unwrap_or(u32::MAX);
            match entries.last_mut() {
                Some((count, last)) if *last == delta => *count += 1,
                _ => entries.push((1, delta)),
            }
        }
        let mut payload = BytesMut::new();
        payload.put_u32(u32::try_from(entries.len()).unwrap_or(u32::MAX));
        for (count, delta) in entries {
            payload.put_u32(count);
            payload.put_u32(delta);
        }
        full_box(*b"stts", 0, 0, &payload)
    }

    fn ctts(&self) -> Bytes {
        let mut entries: Vec<(u32, i64)> = Vec::new();
        for sample in &self.samples {
            match entries.last_mut() {
                Some((count, offset)) if *offset == sample.cts => *count += 1,
                _ => entries.push((1, sample.cts)),
            }
        }
        let negative = entries.iter().any(|(_, offset)| *offset < 0);
        let mut payload = BytesMut::new();
        payload.put_u32(u32::try_from(entries.len()).unwrap_or(u32::MAX));
        for (count, offset) in entries {
            payload.put_u32(count);
            payload.put_i32(i32::try_from(offset).unwrap_or(i32::MAX));
        }
        full_box(*b"ctts", u8::from(negative), 0, &payload)
    }

    fn stsc(&self) -> Bytes {
        let mut entries: Vec<(u32, u32)> = Vec::new();
        for (at, (_, count)) in self.chunks.iter().enumerate() {
            if entries.last().is_none_or(|(_, last)| last != count) {
                entries.push((u32::try_from(at + 1).unwrap_or(u32::MAX), *count));
            }
        }
        let mut payload = BytesMut::new();
        payload.put_u32(u32::try_from(entries.len()).unwrap_or(u32::MAX));
        for (first, count) in entries {
            payload.put_u32(first);
            payload.put_u32(count);
            payload.put_u32(1);
        }
        full_box(*b"stsc", 0, 0, &payload)
    }

    fn stsz(&self) -> Bytes {
        let mut payload = BytesMut::new();
        let first = self.samples.first().map_or(0, |sample| sample.size);
        let constant =
            !self.samples.is_empty() && self.samples.iter().all(|sample| sample.size == first);
        payload.put_u32(if constant { first } else { 0 });
        payload.put_u32(u32::try_from(self.samples.len()).unwrap_or(u32::MAX));
        if !constant {
            for sample in &self.samples {
                payload.put_u32(sample.size);
            }
        }
        full_box(*b"stsz", 0, 0, &payload)
    }
}

impl ContainerWriter for Mp4Writer {
    fn add_track(&mut self, track: &TrackInfo) -> Result<TrackId, MuxError> {
        if self.started {
            return Err(MuxError::Invalid(
                "tracks are added before the first packet".into(),
            ));
        }
        if track.timescale == 0 {
            return Err(MuxError::Invalid("a track needs a timescale".into()));
        }
        let (entry, timescale, nominal) = match (track.kind, &track.video, &track.audio) {
            (TrackKind::Video, Some(video), _) => {
                let mut timescale = track.timescale;
                while timescale < 10_000 {
                    timescale *= 2;
                }
                let nominal = video.frame_rate.and_then(|rate| {
                    let ticks = u64::from(timescale) * u64::from(rate.denominator());
                    (ticks % u64::from(rate.numerator()) == 0)
                        .then(|| i64::try_from(ticks / u64::from(rate.numerator())).ok())
                        .flatten()
                });
                (video_entry(video, self.flavor)?, timescale, nominal)
            }
            (TrackKind::Audio, _, Some(audio)) => {
                let timescale = if audio.sample_rate > 0 {
                    audio.sample_rate
                } else {
                    track.timescale
                };
                (audio_entry(audio, self.flavor)?, timescale, None)
            }
            _ => {
                return Err(MuxError::Unsupported(
                    "only video and audio tracks are written to MP4".into(),
                ));
            }
        };
        if self.tracks.len() >= u16::MAX as usize {
            return Err(MuxError::Unsupported("too many tracks".into()));
        }
        self.tracks.push(Track {
            info: track.clone(),
            entry,
            in_timescale: track.timescale,
            timescale,
            nominal_duration: nominal,
            pending: VecDeque::new(),
            last_dts: None,
            samples: Vec::new(),
            chunks: Vec::new(),
        });
        Ok(TrackId(
            u32::try_from(self.tracks.len()).unwrap_or(u32::MAX),
        ))
    }

    fn write(&mut self, packet: Packet) -> Result<(), MuxError> {
        let position = usize::try_from(packet.track.0)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .filter(|position| *position < self.tracks.len())
            .ok_or_else(|| MuxError::Invalid(format!("no track {}", packet.track.0)))?;
        if !self.started {
            self.start()?;
        }
        let track = &mut self.tracks[position];
        let (input, output) = (i64::from(track.in_timescale), i64::from(track.timescale));
        let dts = rescale(packet.dts, output, input);
        let pts = rescale(packet.pts, output, input);
        if track.last_dts.is_some_and(|last| dts <= last) {
            return Err(MuxError::Invalid(format!(
                "track {} decode times do not increase at {}",
                packet.track.0, packet.dts
            )));
        }
        let mut duration = rescale(packet.duration, output, input);
        if duration <= 0 {
            duration = track.nominal_duration.unwrap_or_else(|| {
                track
                    .samples
                    .last()
                    .or_else(|| {
                        track
                            .pending
                            .back()
                            .and_then(|chunk| chunk.samples.last().map(|(sample, _)| sample))
                    })
                    .map_or(0, |previous| dts.saturating_sub(previous.dts).max(0))
            });
        }
        let size = u32::try_from(packet.data.len())
            .map_err(|_| MuxError::Unsupported("a packet is larger than 4 GiB".into()))?;
        let sample = Sample {
            size,
            dts,
            cts: pts.saturating_sub(dts),
            duration,
            keyframe: packet.keyframe,
        };
        let half_second = output / 2;
        match track.pending.back_mut() {
            Some(chunk) if dts.saturating_sub(chunk.start) < half_second.max(1) => {
                chunk.samples.push((sample, packet.data.clone()));
            }
            _ => track.pending.push_back(Chunk {
                start: dts,
                samples: vec![(sample, packet.data.clone())],
            }),
        }
        track.last_dts = Some(dts);
        self.buffered += packet.data.len();
        self.flush(false)
    }

    fn finish(mut self: Box<Self>) -> Result<(), MuxError> {
        self.finish_inner()
    }
}
