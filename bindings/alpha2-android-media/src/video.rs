//! Video decoding through `MediaCodec` into byte buffers.
//!
//! Byte-buffer output is the CPU path on every device: some hardware decoders hand an
//! `AImageReader` vendor buffers whose CPU view is not plain YUV, while their byte buffers carry
//! the layout the output format states. 10-bit streams ask for P010 explicitly, since some
//! decoders otherwise output 8 bits.

use std::collections::HashMap;

use alpha2_container::{Packet, TrackInfo, VideoCodec};
use alpha2_host_core::video_stream::{
    BackendError, BackendFrame, BackendImage, BackendKind, Poll, Submit, VideoBackends,
    VideoDecodeBackend,
};
use alpha2_media::{Plane, Rect, SampleAspectRatio, YuvFormat};
use bytes::Bytes;

use crate::codec::{
    Codec, FLAG_CODEC_CONFIG, FLAG_END_OF_STREAM, Format, Output, api_level, from_codec_us,
    to_codec_us,
};

/// `COLOR_FormatYUV420Planar`, `COLOR_FormatYUV420SemiPlanar`, `COLOR_FormatYUVP010` and
/// `COLOR_FormatYUV420Flexible`.
const COLOR_I420: i32 = 19;
const COLOR_NV12: i32 = 21;
const COLOR_P010: i32 = 54;
const COLOR_FLEXIBLE: i32 = 0x7f42_0888;

/// How long a receive that must wait blocks for output.
const WAIT_US: i64 = 10_000;

/// The MIME type `MediaCodec` names `codec` by.
fn mime(codec: VideoCodec) -> Option<&'static str> {
    Some(match codec {
        VideoCodec::H264 => "video/avc",
        VideoCodec::Hevc => "video/hevc",
        VideoCodec::Vp8 => "video/x-vnd.on2.vp8",
        VideoCodec::Vp9 => "video/x-vnd.on2.vp9",
        VideoCodec::Av1 => "video/av01",
        VideoCodec::Mpeg4Part2 => "video/mp4v-es",
        VideoCodec::ProRes(_) | VideoCodec::Other(_) => return None,
    })
}

/// The platform's software decoders for `codec`, newest first.
fn software_decoders(codec: VideoCodec) -> &'static [&'static str] {
    match codec {
        VideoCodec::H264 => &["c2.android.avc.decoder", "OMX.google.h264.decoder"],
        VideoCodec::Hevc => &["c2.android.hevc.decoder", "OMX.google.hevc.decoder"],
        VideoCodec::Vp8 => &["c2.android.vp8.decoder", "OMX.google.vp8.decoder"],
        VideoCodec::Vp9 => &["c2.android.vp9.decoder", "OMX.google.vp9.decoder"],
        VideoCodec::Av1 => &["c2.android.av1-dav1d.decoder", "c2.android.av1.decoder"],
        VideoCodec::Mpeg4Part2 => &["c2.android.mpeg4.decoder", "OMX.google.mpeg4.decoder"],
        VideoCodec::ProRes(_) | VideoCodec::Other(_) => &[],
    }
}

/// The parameter sets of an `avcC` or `hvcC` record, each with an Annex B start code, split as
/// `MediaCodec` takes them: `csd-0` (SPS, or VPS+SPS+PPS for HEVC) and `csd-1` (PPS for H.264).
fn codec_specific_data(codec: VideoCodec, record: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut first = Vec::new();
    let mut second = Vec::new();
    let push = |into: &mut Vec<u8>, nal: &[u8]| {
        into.extend_from_slice(&[0, 0, 0, 1]);
        into.extend_from_slice(nal);
    };
    let unit = |at: usize| -> Option<(&[u8], usize)> {
        let length = usize::from(u16::from_be_bytes([*record.get(at)?, *record.get(at + 1)?]));
        Some((record.get(at + 2..at + 2 + length)?, at + 2 + length))
    };
    match codec {
        VideoCodec::H264 => {
            let Some(&sps) = record.get(5) else {
                return (first, second);
            };
            let mut at = 6;
            for _ in 0..(sps & 0x1f) {
                let Some((nal, next)) = unit(at) else { break };
                push(&mut first, nal);
                at = next;
            }
            let Some(&pps) = record.get(at) else {
                return (first, second);
            };
            at += 1;
            for _ in 0..pps {
                let Some((nal, next)) = unit(at) else { break };
                push(&mut second, nal);
                at = next;
            }
        }
        VideoCodec::Hevc => {
            let Some(&arrays) = record.get(22) else {
                return (first, second);
            };
            let mut at = 23;
            for _ in 0..arrays {
                let Some(count) = record.get(at + 1..at + 3) else {
                    break;
                };
                let count = u16::from_be_bytes([count[0], count[1]]);
                at += 3;
                for _ in 0..count {
                    let Some((nal, next)) = unit(at) else { break };
                    let kind = (nal.first().copied().unwrap_or(0) >> 1) & 0x3f;
                    if (32..=34).contains(&kind) {
                        push(&mut first, nal);
                    }
                    at = next;
                }
            }
        }
        _ => first.extend_from_slice(record),
    }
    (first, second)
}

/// Bytes of each NAL length prefix in an `avcC` or `hvcC` track's packets.
fn nal_length_size(codec: VideoCodec, record: &[u8]) -> Option<usize> {
    let at = match codec {
        VideoCodec::H264 => 4,
        VideoCodec::Hevc => 21,
        _ => return None,
    };
    record.get(at).map(|byte| usize::from(byte & 3) + 1)
}

/// `packet` with its NAL length prefixes turned into Annex B start codes.
fn annex_b(packet: &[u8], length_size: usize) -> Result<Vec<u8>, BackendError> {
    let mut out = Vec::with_capacity(packet.len() + 16);
    let mut at = 0;
    while at < packet.len() {
        let prefix = packet
            .get(at..at + length_size)
            .ok_or_else(|| BackendError::Corrupt("a NAL length runs past the packet".into()))?;
        let length = prefix
            .iter()
            .fold(0_usize, |length, byte| (length << 8) | usize::from(*byte));
        at += length_size;
        let nal = packet
            .get(at..at + length)
            .ok_or_else(|| BackendError::Corrupt("a NAL unit runs past the packet".into()))?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        at += length;
    }
    Ok(out)
}

/// `count` rows of `row_bytes` at `start`, `stride` apart, as a plane's `stride * count` bytes:
/// a buffer may end right after the last row's samples, so its padding is filled with zeros.
fn plane_bytes(
    bytes: &[u8],
    start: usize,
    stride: usize,
    row_bytes: usize,
    count: usize,
) -> Result<Bytes, BackendError> {
    let used = start + stride * count.saturating_sub(1) + row_bytes;
    if bytes.len() < used {
        return Err(BackendError::Failed(
            "the output buffer is shorter than its layout".into(),
        ));
    }
    let full = start + stride * count;
    let mut plane = bytes[start..full.min(bytes.len())].to_vec();
    plane.resize(stride * count, 0);
    Ok(Bytes::from(plane))
}

/// Whether the track's configuration record declares more than 8 bits per sample.
fn high_bit_depth(codec: VideoCodec, record: &[u8]) -> bool {
    match codec {
        // `hvcC`: bitDepthLumaMinus8 in the low three bits of byte 17.
        VideoCodec::Hevc => record.get(17).is_some_and(|byte| byte & 7 > 0),
        // `av1C`: high_bitdepth is bit 6 of byte 2.
        VideoCodec::Av1 => record.get(2).is_some_and(|byte| byte & 0x40 != 0),
        // `vpcC` body after version and flags: bitDepth in the high nibble of byte 6.
        VideoCodec::Vp9 => record.get(6).is_some_and(|byte| byte >> 4 > 8),
        _ => false,
    }
}

/// Whether an MPEG-4 Part 2 configuration's visual object sequence header declares Advanced
/// Simple Profile (`profile_and_level_indication` 0xF0 to 0xF7).
fn mpeg4_advanced_simple(codec: VideoCodec, record: &[u8]) -> bool {
    codec == VideoCodec::Mpeg4Part2
        && record
            .windows(5)
            .find(|window| window[..4] == [0, 0, 1, 0xb0])
            .is_some_and(|window| matches!(window[4], 0xf0..=0xf7))
}

/// How decoded bytes are laid out, from the output format.
#[derive(Clone, Copy, Debug)]
struct Layout {
    width: u32,
    height: u32,
    stride: usize,
    format: YuvFormat,
    /// Byte offsets of the chroma planes (one for interleaved chroma) from the buffer start.
    chroma: [usize; 2],
    visible: Option<Rect>,
    sample_aspect_ratio: Option<SampleAspectRatio>,
}

fn layout(format: &Format) -> Result<Layout, BackendError> {
    let positive = |key: &str| {
        format
            .i32(key)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
    };
    let width = positive("width").ok_or_else(|| BackendError::Failed("no output width".into()))?;
    let height =
        positive("height").ok_or_else(|| BackendError::Failed("no output height".into()))?;
    let visible = format.rect("crop").and_then(|(left, top, right, bottom)| {
        Some(Rect {
            x: u32::try_from(left).ok()?,
            y: u32::try_from(top).ok()?,
            width: u32::try_from(right - left + 1)
                .ok()
                .filter(|width| *width > 0)?,
            height: u32::try_from(bottom - top + 1)
                .ok()
                .filter(|height| *height > 0)?,
        })
    });
    // Some decoders report the visible size as the width and height while the crop rectangle
    // is in buffer coordinates: the buffer holds at least the rectangle.
    let (width, height) = visible.map_or((width, height), |rect| {
        (
            width.max(rect.x.saturating_add(rect.width)),
            height.max(rect.y.saturating_add(rect.height)),
        )
    });
    let color = format.i32("color-format").unwrap_or(COLOR_NV12);
    let bytes_per_sample = if color == COLOR_P010 { 2 } else { 1 };
    let stride = positive("stride")
        .map_or(width as usize * bytes_per_sample, |stride| stride as usize)
        .max(width as usize * bytes_per_sample);
    let slice_height = positive("slice-height")
        .map_or(height as usize, |rows| rows as usize)
        .max(height as usize);
    let luma = stride * slice_height;
    let (yuv, chroma) = match color {
        COLOR_NV12 => (YuvFormat::NV12, [luma, 0]),
        COLOR_P010 => (YuvFormat::P010, [luma, 0]),
        COLOR_I420 => {
            let chroma_plane = (stride / 2) * slice_height.div_ceil(2);
            (YuvFormat::I420, [luma, luma + chroma_plane])
        }
        COLOR_FLEXIBLE => flexible(format)?,
        other => {
            return Err(BackendError::Unsupported(format!(
                "decoder output color format {other:#x}"
            )));
        }
    };
    let sample_aspect_ratio = match (positive("sar-width"), positive("sar-height")) {
        (Some(num), Some(den)) => Some(SampleAspectRatio { num, den }),
        _ => None,
    };
    Ok(Layout {
        width,
        height,
        stride,
        format: yuv,
        chroma,
        visible,
        sample_aspect_ratio,
    })
}

/// The layout a `COLOR_FormatYUV420Flexible` buffer has, from its `MediaImage2` description.
fn flexible(format: &Format) -> Result<(YuvFormat, [usize; 2]), BackendError> {
    let image = format
        .buffer("image-data")
        .ok_or_else(|| BackendError::Unsupported("a flexible output without image-data".into()))?;
    let word = |index: usize| {
        image
            .get(index * 4..index * 4 + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    // MediaImage2: type, planes, width, height, bit depth, allocated bit depth, then per plane
    // offset, column increment, row increment and subsampling.
    let plane = |index: usize| -> Option<(usize, u32)> {
        let base = 6 + index * 5;
        Some((word(base)? as usize, word(base + 1)?))
    };
    let (Some((u, u_step)), Some((v, _))) = (plane(1), plane(2)) else {
        return Err(BackendError::Unsupported("an unreadable image-data".into()));
    };
    if u_step == 2 && v == u + 1 {
        Ok((YuvFormat::NV12, [u, 0]))
    } else if u_step == 1 {
        Ok((YuvFormat::I420, [u, v]))
    } else {
        Err(BackendError::Unsupported(format!(
            "decoder chroma interleaved by {u_step} with V at {v} and U at {u}"
        )))
    }
}

/// A `MediaCodec` video decoder session.
pub struct MediaCodecVideo {
    codec: Codec,
    kind: BackendKind,
    timescale: i64,
    length_size: Option<usize>,
    layout: Option<Layout>,
    /// Presentation times of queued packets, by the microseconds the codec carries.
    times: HashMap<i64, i64>,
    eos_pending: bool,
    drained: bool,
}

impl std::fmt::Debug for MediaCodecVideo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaCodecVideo")
            .field("codec", &self.codec.name())
            .finish_non_exhaustive()
    }
}

impl MediaCodecVideo {
    /// A session of `kind` for `track`.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when no decoder of that kind takes the track.
    pub fn open(track: &TrackInfo, kind: BackendKind) -> Result<Self, BackendError> {
        let video = track
            .video
            .as_ref()
            .ok_or_else(|| BackendError::Unsupported("not a video track".into()))?;
        let mime = mime(video.codec).ok_or_else(|| {
            BackendError::Unsupported(format!("no Android decoder for {:?}", video.codec))
        })?;
        // Android's H.264 decoders stop at High profile: 4:2:0, 8 bits.
        if video.codec == VideoCodec::H264
            && video
                .codec_private
                .get(1)
                .is_some_and(|profile| matches!(profile, 44 | 110 | 122 | 244))
        {
            return Err(BackendError::Unsupported(
                "Android decodes H.264 up to High profile, not High 10, 4:2:2 or 4:4:4".into(),
            ));
        }
        let codec = match kind {
            BackendKind::Hardware => {
                let codec = Codec::decoder_for(mime)?;
                if codec.is_software() {
                    return Err(BackendError::Unsupported(format!(
                        "no hardware decoder for {mime}"
                    )));
                }
                // The emulator's host-backed decoders crop the wrong columns and stall seeks, so
                // an emulator decodes with the platform's software decoders.
                if codec.name().starts_with("c2.goldfish.")
                    || codec.name().starts_with("OMX.android.goldfish.")
                {
                    return Err(BackendError::Unsupported(format!(
                        "{} is the emulator's decoder; the software decoder is used instead",
                        codec.name()
                    )));
                }
                codec
            }
            BackendKind::Software if mpeg4_advanced_simple(video.codec, &video.codec_private) => {
                return Err(BackendError::Unsupported(
                    "Android's software MPEG-4 decoder reads Simple Profile, not Advanced Simple"
                        .into(),
                ));
            }
            BackendKind::Software => software_decoders(video.codec)
                .iter()
                .find_map(|name| Codec::by_name(name).ok())
                .ok_or_else(|| {
                    BackendError::Unsupported(format!("no software decoder for {mime}"))
                })?,
        };
        let mut format = Format::new();
        format.set_str("mime", mime);
        format.set_i32("width", i32::try_from(video.width).unwrap_or(i32::MAX));
        format.set_i32("height", i32::try_from(video.height).unwrap_or(i32::MAX));
        let pixels = u64::from(video.width) * u64::from(video.height);
        format.set_i32(
            "max-input-size",
            i32::try_from((pixels * 3).max(1 << 20)).unwrap_or(i32::MAX),
        );
        let (first, second) = codec_specific_data(video.codec, &video.codec_private);
        if !first.is_empty() {
            format.set_buffer("csd-0", &first);
        }
        if !second.is_empty() {
            format.set_buffer("csd-1", &second);
        }
        if high_bit_depth(video.codec, &video.codec_private) {
            if api_level() < 33 {
                return Err(BackendError::Unsupported(
                    "10-bit decoding needs Android 13 for P010 output".into(),
                ));
            }
            format.set_i32("color-format", COLOR_P010);
        }
        let mut session = Self {
            codec,
            kind,
            timescale: i64::from(track.timescale.max(1)),
            length_size: nal_length_size(video.codec, &video.codec_private),
            layout: None,
            times: HashMap::new(),
            eos_pending: false,
            drained: false,
        };
        session.codec.start(&format, false)?;
        Ok(session)
    }

    /// The component decoding.
    #[must_use]
    pub fn component(&self) -> &str {
        self.codec.name()
    }

    fn to_us(&self, pts: i64) -> i64 {
        to_codec_us(pts, self.timescale)
    }

    fn try_queue_eos(&mut self) -> Result<(), BackendError> {
        if let Some(index) = self.codec.dequeue_input(0)? {
            self.codec.queue_input(index, &[], 0, FLAG_END_OF_STREAM)?;
            self.eos_pending = false;
        }
        Ok(())
    }

    fn picture(&self, bytes: &[u8], pts_us: i64) -> Result<BackendFrame, BackendError> {
        let layout = self
            .layout
            .ok_or_else(|| BackendError::Failed("a picture before any output format".into()))?;
        let rows = |start: usize, row_bytes: usize, count: usize| {
            plane_bytes(bytes, start, layout.stride, row_bytes, count)
        };
        let sample = layout.format.bytes_per_sample() as usize;
        let (width, height) = (layout.width, layout.height);
        let chroma_rows = height.div_ceil(2) as usize;
        let luma = Plane {
            bytes: rows(0, width as usize * sample, height as usize)?,
            stride: u32::try_from(layout.stride).unwrap_or(u32::MAX),
            width,
            height,
        };
        let planes = if layout.format.interleaved_chroma {
            vec![
                luma,
                Plane {
                    bytes: rows(
                        layout.chroma[0],
                        width.div_ceil(2) as usize * 2 * sample,
                        chroma_rows,
                    )?,
                    stride: u32::try_from(layout.stride).unwrap_or(u32::MAX),
                    width: width.div_ceil(2),
                    height: height.div_ceil(2),
                },
            ]
        } else {
            let half = layout.stride / 2;
            let chroma = |offset: usize| -> Result<Plane, BackendError> {
                Ok(Plane {
                    bytes: plane_bytes(
                        bytes,
                        offset,
                        half,
                        width.div_ceil(2) as usize,
                        chroma_rows,
                    )?,
                    stride: u32::try_from(half).unwrap_or(u32::MAX),
                    width: width.div_ceil(2),
                    height: height.div_ceil(2),
                })
            };
            vec![luma, chroma(layout.chroma[0])?, chroma(layout.chroma[1])?]
        };
        let pts = self
            .times
            .get(&pts_us)
            .copied()
            .unwrap_or_else(|| from_codec_us(pts_us, self.timescale));
        Ok(BackendFrame {
            pts,
            image: BackendImage::Cpu {
                format: layout.format,
                planes,
            },
            coded: (width, height),
            visible: layout.visible,
            sample_aspect_ratio: layout.sample_aspect_ratio,
            color: None,
            chroma_location: None,
        })
    }
}

impl VideoDecodeBackend for MediaCodecVideo {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        let Some(index) = self.codec.dequeue_input(0)? else {
            return Ok(Submit::Busy);
        };
        let pts_us = self.to_us(packet.pts);
        let converted;
        let bytes = match self.length_size {
            Some(size) => {
                converted = annex_b(&packet.data, size)?;
                &converted[..]
            }
            None => &packet.data[..],
        };
        self.times.insert(pts_us, packet.pts);
        self.codec.queue_input(index, bytes, pts_us, 0)?;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<Poll, BackendError> {
        if self.drained {
            return Ok(Poll::Drained);
        }
        if self.eos_pending {
            self.try_queue_eos()?;
        }
        loop {
            match self.codec.dequeue_output(if wait { WAIT_US } else { 0 })? {
                Output::TryAgain => return Ok(Poll::Pending),
                Output::FormatChanged => {
                    self.layout = Some(layout(&self.codec.output_format())?);
                }
                Output::Buffer {
                    index,
                    offset,
                    size,
                    pts_us,
                    flags,
                } => {
                    let end = flags & FLAG_END_OF_STREAM != 0;
                    let picture = if size > 0 && flags & FLAG_CODEC_CONFIG == 0 {
                        if self.layout.is_none() {
                            self.layout = Some(layout(&self.codec.output_format())?);
                        }
                        let bytes =
                            self.codec
                                .output_bytes(index, offset, size)
                                .ok_or_else(|| {
                                    BackendError::Failed("an unreadable output buffer".into())
                                })?;
                        Some(self.picture(bytes, pts_us))
                    } else {
                        None
                    };
                    self.codec.release_output(index)?;
                    if let Some(picture) = picture {
                        let picture = picture?;
                        self.times.remove(&pts_us);
                        self.drained = end;
                        return Ok(Poll::Frame(picture));
                    }
                    if end {
                        self.drained = true;
                        return Ok(Poll::Drained);
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

/// Opens `MediaCodec` video sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AndroidVideo;

impl VideoBackends for AndroidVideo {
    fn open(
        &self,
        track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        Ok(Box::new(MediaCodecVideo::open(track, kind)?))
    }
}
