//! Video decoding through `MediaCodec` into byte buffers, or into GPU-sampled buffers the
//! renderer imports.
//!
//! Byte-buffer output is the CPU path on every device: some hardware decoders hand an
//! `AImageReader` vendor buffers whose CPU view is not plain YUV, while their byte buffers carry
//! the layout the output format states. 10-bit streams ask for P010 explicitly, since some
//! decoders otherwise output 8 bits, and a decoder that still does fails the session.

use std::collections::HashMap;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alpha2_container::{Packet, TrackInfo, VideoCodec};
use alpha2_host_core::parameter_sets::vp9_bit_depth;
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
use crate::surface::{AndroidSurface, ImageReader};

/// `COLOR_FormatYUV420Planar`, `COLOR_FormatYUV420SemiPlanar` and `COLOR_FormatYUVP010`.
const COLOR_I420: i32 = 19;
const COLOR_NV12: i32 = 21;
const COLOR_P010: i32 = 54;

/// How long a receive that must wait blocks for output.
const WAIT_US: i64 = 10_000;

/// How long a picture rendered into the image reader may take to arrive there.
const IMAGE_ARRIVAL: Duration = Duration::from_secs(2);

/// How long a started decoder is watched for a refusal of the stream's parameter sets. Decoders
/// that cannot decode a stream's profile or format report it within milliseconds of start.
const CONFIGURATION_CHECK: Duration = Duration::from_millis(20);

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

/// Where one plane's samples are in a decoded buffer, in bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlaneGeometry {
    offset: usize,
    /// From one row to the next.
    row: usize,
    /// From one sample to the next in a row.
    column: usize,
}

impl PlaneGeometry {
    const fn new(offset: usize, row: usize, column: usize) -> Self {
        Self {
            offset,
            row,
            column,
        }
    }
}

/// How decoded bytes are laid out, from the output format.
#[derive(Clone, Copy, Debug)]
struct Layout {
    width: u32,
    height: u32,
    /// Significant bits per sample. Above 8 a sample takes two bytes and sits in their high bits,
    /// as P010 and `MediaImage2` store it.
    bit_depth: u8,
    /// Luma, U and V, 4:2:0.
    planes: [PlaneGeometry; 3],
    visible: Option<Rect>,
    sample_aspect_ratio: Option<SampleAspectRatio>,
}

impl Layout {
    const fn bytes_per_sample(&self) -> usize {
        if self.bit_depth > 8 { 2 } else { 1 }
    }
}

fn positive(format: &Format, key: &str) -> Option<u32> {
    format
        .i32(key)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
}

/// The output format's `crop` rectangle, whose right and bottom are inclusive.
fn crop_rect(format: &Format) -> Option<Rect> {
    format.rect("crop").and_then(|(left, top, right, bottom)| {
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
    })
}

fn sample_aspect_ratio(format: &Format) -> Option<SampleAspectRatio> {
    match (
        positive(format, "sar-width"),
        positive(format, "sar-height"),
    ) {
        (Some(num), Some(den)) => Some(SampleAspectRatio { num, den }),
        _ => None,
    }
}

fn layout(format: &Format) -> Result<Layout, BackendError> {
    let positive = |key: &str| positive(format, key);
    let width = positive("width").ok_or_else(|| BackendError::Failed("no output width".into()))?;
    let height =
        positive("height").ok_or_else(|| BackendError::Failed("no output height".into()))?;
    let visible = crop_rect(format);
    // Some decoders report the visible size as the width and height while the crop rectangle
    // is in buffer coordinates: the buffer holds at least the rectangle.
    let (width, height) = visible.map_or((width, height), |rect| {
        (
            width.max(rect.x.saturating_add(rect.width)),
            height.max(rect.y.saturating_add(rect.height)),
        )
    });
    // The `MediaImage2` describes the buffer exactly; the color format and strides are its
    // summary, read only when a decoder gives no description.
    let described = match format.buffer("image-data") {
        Some(image) => media_image(&image)?,
        None => None,
    };
    let (planes, bit_depth) =
        described.map_or_else(|| color_format_layout(format, width, height), Ok)?;
    Ok(Layout {
        width,
        height,
        bit_depth,
        planes,
        visible,
        sample_aspect_ratio: sample_aspect_ratio(format),
    })
}

/// The layout the color format, stride and slice height name.
fn color_format_layout(
    format: &Format,
    width: u32,
    height: u32,
) -> Result<([PlaneGeometry; 3], u8), BackendError> {
    let positive = |key: &str| positive(format, key);
    let color = format.i32("color-format").ok_or_else(|| {
        BackendError::Unsupported("the decoder output names no color format".into())
    })?;
    let sample = if color == COLOR_P010 { 2 } else { 1 };
    let stride = positive("stride")
        .map_or(width as usize * sample, |stride| stride as usize)
        .max(width as usize * sample);
    let slice_height = positive("slice-height")
        .map_or(height as usize, |rows| rows as usize)
        .max(height as usize);
    let luma = stride * slice_height;
    let y = PlaneGeometry::new(0, stride, sample);
    Ok(match color {
        COLOR_NV12 => (
            [
                y,
                PlaneGeometry::new(luma, stride, 2),
                PlaneGeometry::new(luma + 1, stride, 2),
            ],
            8,
        ),
        COLOR_P010 => (
            [
                y,
                PlaneGeometry::new(luma, stride, 4),
                PlaneGeometry::new(luma + 2, stride, 4),
            ],
            10,
        ),
        COLOR_I420 => {
            // The platform lays planar chroma at half the luma stride.
            let chroma = stride / 2;
            let v = luma + chroma * slice_height.div_ceil(2);
            (
                [
                    y,
                    PlaneGeometry::new(luma, chroma, 1),
                    PlaneGeometry::new(v, chroma, 1),
                ],
                8,
            )
        }
        other => {
            return Err(BackendError::Unsupported(format!(
                "decoder output color format {other:#x} without image-data"
            )));
        }
    })
}

/// The planes and bit depth a `MediaImage2` describes; `None` when it does not describe
/// three-plane YUV.
fn media_image(image: &[u8]) -> Result<Option<([PlaneGeometry; 3], u8)>, BackendError> {
    /// `MediaImage2::MEDIA_IMAGE_TYPE_YUV`.
    const YUV: u32 = 1;
    let word = |index: usize| {
        image
            .get(index * 4..index * 4 + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    // Type, planes, width, height, bit depth, allocated bit depth, then per plane offset, column
    // increment, row increment, horizontal and vertical subsampling.
    let (Some(YUV), Some(3), Some(bit_depth), Some(allocated)) =
        (word(0), word(1), word(4), word(5))
    else {
        return Ok(None);
    };
    let sample = match allocated {
        8 => 1,
        16 => 2,
        _ => return Ok(None),
    };
    let unsupported = || BackendError::Unsupported("an unreadable decoder image-data".into());
    let bit_depth = u8::try_from(bit_depth)
        .ok()
        .filter(|depth| (8..=allocated).contains(&u32::from(*depth)))
        .ok_or_else(unsupported)?;
    let mut planes = [PlaneGeometry::new(0, 0, 0); 3];
    for (index, plane) in planes.iter_mut().enumerate() {
        let base = 6 + index * 5;
        let field = |at: usize| word(base + at).ok_or_else(unsupported);
        let subsampling = if index == 0 { 1 } else { 2 };
        if field(3)? != subsampling || field(4)? != subsampling {
            return Err(BackendError::Unsupported(
                "decoder output that is not 4:2:0".into(),
            ));
        }
        // The increments are signed; negative ones (mirrored planes) are not read.
        let increment = |at: usize| {
            field(at).and_then(|value| {
                usize::try_from(value.cast_signed())
                    .ok()
                    .filter(|value| *value >= sample)
                    .ok_or_else(unsupported)
            })
        };
        *plane = PlaneGeometry::new(field(0)? as usize, increment(2)?, increment(1)?);
    }
    Ok(Some((planes, bit_depth)))
}

/// `width` x `height` samples of `plane`: its rows as they are when its samples are adjacent,
/// else gathered into packed rows.
fn read_plane(
    bytes: &[u8],
    plane: PlaneGeometry,
    width: usize,
    height: usize,
    sample: usize,
) -> Result<Plane, BackendError> {
    let dimension = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    if plane.column == sample {
        return Ok(Plane {
            bytes: plane_bytes(bytes, plane.offset, plane.row, width * sample, height)?,
            stride: dimension(plane.row),
            width: dimension(width),
            height: dimension(height),
        });
    }
    let mut packed = vec![0; width * height * sample];
    gather(bytes, plane, width, height, sample, |x, y, from| {
        let at = (y * width + x) * sample;
        packed[at..at + sample].copy_from_slice(from);
    })?;
    Ok(Plane {
        bytes: Bytes::from(packed),
        stride: dimension(width * sample),
        width: dimension(width),
        height: dimension(height),
    })
}

/// `u` and `v` interleaved into one plane, U first, as NV12 and P010 store chroma.
fn interleave(
    bytes: &[u8],
    [u, v]: [PlaneGeometry; 2],
    width: usize,
    height: usize,
    sample: usize,
) -> Result<Plane, BackendError> {
    let stride = width * 2 * sample;
    let mut packed = vec![0; stride * height];
    for (plane, first) in [(u, 0), (v, sample)] {
        gather(bytes, plane, width, height, sample, |x, y, from| {
            let at = y * stride + x * 2 * sample + first;
            packed[at..at + sample].copy_from_slice(from);
        })?;
    }
    let dimension = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    Ok(Plane {
        bytes: Bytes::from(packed),
        stride: dimension(stride),
        width: dimension(width),
        height: dimension(height),
    })
}

/// Calls `put` with each sample of `plane`'s `width` x `height` and its column and row.
fn gather(
    bytes: &[u8],
    plane: PlaneGeometry,
    width: usize,
    height: usize,
    sample: usize,
    mut put: impl FnMut(usize, usize, &[u8]),
) -> Result<(), BackendError> {
    if width == 0 || height == 0 {
        return Ok(());
    }
    let last = plane.offset + plane.row * (height - 1) + plane.column * (width - 1) + sample;
    if bytes.len() < last {
        return Err(BackendError::Failed(
            "the output buffer is shorter than its layout".into(),
        ));
    }
    for y in 0..height {
        for x in 0..width {
            let at = plane.offset + plane.row * y + plane.column * x;
            put(x, y, &bytes[at..at + sample]);
        }
    }
    Ok(())
}

/// The planes of a decoded picture laid out as `layout` says: NV12 or P010 when the decoder's
/// chroma interleaves U first, else I420 for 8 bits and P010 for more.
fn read_picture(layout: &Layout, bytes: &[u8]) -> Result<(YuvFormat, Vec<Plane>), BackendError> {
    let sample = layout.bytes_per_sample();
    let (width, height) = (layout.width as usize, layout.height as usize);
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    let [luma, u, v] = layout.planes;
    let semi_planar = YuvFormat {
        bit_depth: layout.bit_depth,
        ..if sample == 2 {
            YuvFormat::P010
        } else {
            YuvFormat::NV12
        }
    };
    let luma = read_plane(bytes, luma, width, height, sample)?;
    Ok(
        if u.column == 2 * sample
            && v.column == u.column
            && v.row == u.row
            && v.offset == u.offset + sample
        {
            let chroma = read_plane(
                bytes,
                PlaneGeometry::new(u.offset, u.row, 2 * sample),
                chroma_width,
                chroma_height,
                2 * sample,
            )?;
            (semi_planar, vec![luma, chroma])
        } else if sample == 1 {
            (
                YuvFormat::I420,
                vec![
                    luma,
                    read_plane(bytes, u, chroma_width, chroma_height, 1)?,
                    read_plane(bytes, v, chroma_width, chroma_height, 1)?,
                ],
            )
        } else {
            // 16-bit chroma in any other arrangement is repacked as P010's.
            let chroma = interleave(bytes, [u, v], chroma_width, chroma_height, 2)?;
            (semi_planar, vec![luma, chroma])
        },
    )
}

/// A decoder of `kind` for `video`.
fn decoder(
    video: &alpha2_container::VideoTrack,
    mime: &str,
    kind: BackendKind,
) -> Result<Codec, BackendError> {
    Ok(match kind {
        BackendKind::Hardware => {
            let codec = Codec::decoder_for(mime)?;
            if codec.is_software() {
                return Err(BackendError::Unsupported(format!(
                    "no hardware decoder for {mime}"
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
            .ok_or_else(|| BackendError::Unsupported(format!("no software decoder for {mime}")))?,
    })
}

/// Where hardware sessions put decoded pictures.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoOutput {
    /// Byte buffers: CPU planes.
    #[default]
    ByteBuffers,
    /// GPU-sampled buffers the renderer imports, as [`AndroidSurface`] frames; `ten_bit` says
    /// whether 10-bit tracks may use them too.
    ///
    /// Software sessions still use byte buffers. Their pictures are made in memory, so a buffer
    /// the GPU samples would add a copy rather than save one, and some of them write the cropped
    /// picture into a buffer of the coded size, which then disagrees with the stream's crop.
    Surfaces {
        /// Whether 10-bit tracks render into surfaces.
        ten_bit: bool,
    },
}

/// A session's image reader and the picture it waits for there.
struct SurfaceOutput {
    reader: Arc<ImageReader>,
    ten_bit: bool,
    /// The picture rendered last and not yet acquired: its codec time, whether it ends the
    /// stream, and when it was rendered. One picture at a time is in flight, since a reader
    /// may drop a queued picture that a newer one overtakes.
    awaiting: Option<(i64, bool, Instant)>,
    /// Pictures rendered before a flush that may still arrive, dropped when they do.
    stale: usize,
    crop: Option<Rect>,
    sample_aspect_ratio: Option<SampleAspectRatio>,
}

/// The bit depth a session asks its decoder for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Depth {
    /// 8 bits, the decoders' default.
    Eight,
    /// P010, which 8-bit output then fails.
    Ten,
    /// A VP9 track without `vpcC`, whose first frame's header declares its depth.
    FirstFrame,
}

/// How far a session's codec has come since it started.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Progress {
    Started,
    /// An output format or buffer arrived.
    Output,
    /// A picture went out.
    Picture,
}

/// A `MediaCodec` video decoder session.
pub struct MediaCodecVideo {
    // Before `surface`: the codec renders into the reader's window, which must outlive it.
    codec: Codec,
    /// The format the codec was configured with, for a restart.
    format: Format,
    kind: BackendKind,
    timescale: i64,
    length_size: Option<usize>,
    layout: Option<Layout>,
    surface: Option<SurfaceOutput>,
    /// Whether 10-bit pictures may render into the surface.
    surface_ten_bit: bool,
    depth: Depth,
    /// In-band parameter sets of the format's `csd-*`, which a flush before any output drops.
    config: Vec<Vec<u8>>,
    /// Parameter sets to queue before the next packet.
    config_pending: Vec<Vec<u8>>,
    progress: Progress,
    /// The latest codec time queued since the last flush.
    last_us: Option<i64>,
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
            .field("surface", &self.surface.is_some())
            .finish_non_exhaustive()
    }
}

impl MediaCodecVideo {
    /// A session of `kind` for `track`, putting pictures where `output` says. A hardware decoder
    /// that refuses surface output decodes into byte buffers instead.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when no decoder of that kind takes the track.
    pub fn open(
        track: &TrackInfo,
        kind: BackendKind,
        output: VideoOutput,
    ) -> Result<Self, BackendError> {
        match Self::open_as(track, kind, output) {
            Err(BackendError::Unsupported(reason)) if output != VideoOutput::ByteBuffers => {
                Self::open_as(track, kind, VideoOutput::ByteBuffers).map_err(|bytes| {
                    if bytes.to_string() == reason {
                        bytes
                    } else {
                        BackendError::Unsupported(format!(
                            "surface output: {reason}; byte buffers: {bytes}"
                        ))
                    }
                })
            }
            opened => opened,
        }
    }

    fn open_as(
        track: &TrackInfo,
        kind: BackendKind,
        output: VideoOutput,
    ) -> Result<Self, BackendError> {
        let video = track
            .video
            .as_ref()
            .ok_or_else(|| BackendError::Unsupported("not a video track".into()))?;
        let mime = mime(video.codec).ok_or_else(|| {
            BackendError::Unsupported(format!("no Android decoder for {:?}", video.codec))
        })?;
        let codec = decoder(video, mime, kind)?;
        let ten_bit = high_bit_depth(video.codec, &video.codec_private);
        let mut format = Format::new();
        format.set_str("mime", mime);
        format.set_i32("width", i32::try_from(video.width).unwrap_or(i32::MAX));
        format.set_i32("height", i32::try_from(video.height).unwrap_or(i32::MAX));
        // Level limits keep a conforming frame under half its raw picture (a minimum compression
        // ratio of 2), so the raw 4:2:0 size bounds every packet; the floor is for small pictures,
        // whose headers weigh more.
        let raw =
            u64::from(video.width) * u64::from(video.height) * 3 / 2 * (1 + u64::from(ten_bit));
        format.set_i32(
            "max-input-size",
            i32::try_from(raw.max(1 << 20)).unwrap_or(i32::MAX),
        );
        let (first, second) = codec_specific_data(video.codec, &video.codec_private);
        let config = [&first, &second]
            .into_iter()
            .filter(|data| !data.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        for (key, data) in ["csd-0", "csd-1"].into_iter().zip(&config) {
            format.set_buffer(key, data);
        }
        if ten_bit {
            if api_level() < 33 {
                return Err(BackendError::Unsupported(
                    "10-bit decoding needs Android 13 for P010 output".into(),
                ));
            }
            format.set_i32("color-format", COLOR_P010);
        }
        let surface_ten_bit = matches!(output, VideoOutput::Surfaces { ten_bit: true });
        let surface = match output {
            VideoOutput::Surfaces { .. }
                if kind == BackendKind::Hardware && (surface_ten_bit || !ten_bit) =>
            {
                Some(SurfaceOutput {
                    reader: ImageReader::new(
                        video.width,
                        video.height,
                        video.reorder_depth.unwrap_or(1),
                    )?,
                    ten_bit,
                    awaiting: None,
                    stale: 0,
                    crop: None,
                    sample_aspect_ratio: None,
                })
            }
            _ => None,
        };
        let mut session = Self {
            codec,
            format,
            kind,
            timescale: i64::from(track.timescale.max(1)),
            length_size: nal_length_size(video.codec, &video.codec_private),
            layout: None,
            surface,
            surface_ten_bit,
            depth: if ten_bit {
                Depth::Ten
            } else if video.codec == VideoCodec::Vp9 && video.codec_private.is_empty() {
                Depth::FirstFrame
            } else {
                Depth::Eight
            },
            // VP8, VP9 and AV1 frames carry what decoding needs; their records are only hints.
            config: if matches!(
                video.codec,
                VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Mpeg4Part2
            ) {
                config
            } else {
                Vec::new()
            },
            config_pending: Vec::new(),
            progress: Progress::Started,
            last_us: None,
            times: HashMap::new(),
            eos_pending: false,
            drained: false,
        };
        match &session.surface {
            Some(surface) => session
                .codec
                .start_on(&session.format, surface.reader.window())?,
            None => session.codec.start(&session.format, false)?,
        }
        session.check_configuration()?;
        Ok(session)
    }

    /// Whether pictures arrive as [`AndroidSurface`]s rather than CPU planes.
    #[must_use]
    pub const fn renders_to_surfaces(&self) -> bool {
        self.surface.is_some()
    }

    /// The component decoding.
    #[must_use]
    pub fn component(&self) -> &str {
        self.codec.name()
    }

    fn to_us(&self, pts: i64) -> i64 {
        to_codec_us(pts, self.timescale)
    }

    /// Restarts the codec for P010 output, into byte buffers when surfaces take no 10-bit.
    fn restart_ten_bit(&mut self) -> Result<(), BackendError> {
        if api_level() < 33 {
            return Err(BackendError::Unsupported(
                "10-bit decoding needs Android 13 for P010 output".into(),
            ));
        }
        self.depth = Depth::Ten;
        self.format.set_i32("color-format", COLOR_P010);
        let window = match &self.surface {
            Some(surface) if self.surface_ten_bit => surface.reader.window(),
            _ => null_mut(),
        };
        self.codec.restart(&self.format, window)?;
        match self.surface.as_mut() {
            Some(surface) if self.surface_ten_bit => surface.ten_bit = true,
            _ => self.surface = None,
        }
        self.config_pending.clear();
        Ok(())
    }

    fn try_queue_eos(&mut self) -> Result<(), BackendError> {
        if let Some(index) = self.codec.dequeue_input(0)? {
            let time = self.last_us.unwrap_or_else(|| to_codec_us(0, 1));
            self.codec
                .queue_input(index, &[], time, FLAG_END_OF_STREAM)?;
            self.eos_pending = false;
        }
        Ok(())
    }

    /// Watches the started codec for a refusal of the parameter sets it was configured with. No
    /// release before API 36 declares which profiles a decoder takes, and that release's codec
    /// store is not read (see [`Codec::is_software`]), so the decoder's own reaction decides; one
    /// that parses them only with the first packet fails before its first picture instead.
    fn check_configuration(&mut self) -> Result<(), BackendError> {
        if self.config.is_empty() {
            return Ok(());
        }
        let until = Instant::now() + CONFIGURATION_CHECK;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            match self
                .codec
                .dequeue_output(i64::try_from(left.as_micros()).unwrap_or(0))
            {
                Ok(Output::TryAgain) => {}
                // An output format means the decoder took the parameter sets.
                Ok(Output::FormatChanged) => return self.format_changed(),
                Ok(Output::Buffer { index, .. }) => self.codec.release_output(index)?,
                Err(BackendError::Failed(reason)) => {
                    return Err(BackendError::Unsupported(format!(
                        "{} refuses this stream's parameter sets: {reason}",
                        self.codec.name()
                    )));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Takes in a new output format.
    fn format_changed(&mut self) -> Result<(), BackendError> {
        self.progress = self.progress.max(Progress::Output);
        if let Some(surface) = self.surface.as_mut() {
            let format = self.codec.output_format();
            surface.crop = crop_rect(&format);
            surface.sample_aspect_ratio = sample_aspect_ratio(&format);
            Ok(())
        } else {
            self.read_layout()
        }
    }

    /// Reads the output format's layout, which must keep the 10 bits asked for.
    fn read_layout(&mut self) -> Result<(), BackendError> {
        let layout = layout(&self.codec.output_format())?;
        if self.depth == Depth::Ten && layout.bit_depth <= 8 {
            return Err(BackendError::Unsupported(format!(
                "{} decodes this 10-bit stream to 8 bits",
                self.codec.name()
            )));
        }
        self.layout = Some(layout);
        Ok(())
    }

    fn picture(&self, bytes: &[u8], pts_us: i64) -> Result<BackendFrame, BackendError> {
        let layout = self
            .layout
            .ok_or_else(|| BackendError::Failed("a picture before any output format".into()))?;
        let (format, planes) = read_picture(&layout, bytes)?;
        let pts = self
            .times
            .get(&pts_us)
            .copied()
            .unwrap_or_else(|| from_codec_us(pts_us, self.timescale));
        Ok(BackendFrame {
            pts,
            image: BackendImage::Cpu { format, planes },
            coded: (layout.width, layout.height),
            visible: layout.visible,
            sample_aspect_ratio: layout.sample_aspect_ratio,
            color: None,
            chroma_location: None,
        })
    }
}

/// A picture from the image reader, with the facts the session knows.
fn surface_frame(surface: &SurfaceOutput, image: AndroidSurface, pts: i64) -> BackendFrame {
    let coded = image.extent();
    let inside = |rect: &Rect| {
        rect.x.saturating_add(rect.width) <= coded.0
            && rect.y.saturating_add(rect.height) <= coded.1
    };
    let whole = Rect {
        x: 0,
        y: 0,
        width: coded.0,
        height: coded.1,
    };
    // `AImage_getCropRect` gives the whole buffer when the decoder set no crop on it, so a
    // whole-buffer rectangle defers to the output format's crop.
    let visible = match image.crop().filter(inside) {
        Some(rect) if rect != whole => Some(rect),
        image_crop => surface.crop.filter(inside).or(image_crop),
    };
    BackendFrame {
        pts,
        image: BackendImage::Surface(Arc::new(image)),
        coded,
        visible,
        sample_aspect_ratio: surface.sample_aspect_ratio,
        color: None,
        chroma_location: None,
    }
}

impl MediaCodecVideo {
    /// The awaited picture if it reached the reader, dropping pictures a flush left behind.
    fn take_image(&mut self) -> Result<Option<Poll>, BackendError> {
        let Some(surface) = self.surface.as_mut() else {
            return Ok(None);
        };
        let Some((pts_us, end, _)) = surface.awaiting else {
            return Ok(None);
        };
        while let Some(image) = surface.reader.acquire(surface.ten_bit)? {
            if surface.stale > 0 && image.timestamp_ns() != pts_us.saturating_mul(1000) {
                surface.stale -= 1;
                continue;
            }
            surface.awaiting = None;
            let pts = self
                .times
                .remove(&pts_us)
                .unwrap_or_else(|| from_codec_us(pts_us, self.timescale));
            self.drained = end;
            return Ok(Some(Poll::Frame(surface_frame(surface, image, pts))));
        }
        Ok(None)
    }

    fn receive_surface(&mut self, wait: bool) -> Result<Poll, BackendError> {
        let until = Instant::now() + Duration::from_micros(WAIT_US.unsigned_abs());
        loop {
            if let Some(poll) = self.take_image()? {
                return Ok(poll);
            }
            if let Some((_, _, rendered)) =
                self.surface.as_ref().and_then(|surface| surface.awaiting)
            {
                if rendered.elapsed() > IMAGE_ARRIVAL {
                    return Err(BackendError::Failed(
                        "a rendered picture never reached the decoder's image reader".into(),
                    ));
                }
                if !wait || Instant::now() >= until {
                    return Ok(Poll::Pending);
                }
                std::thread::sleep(Duration::from_micros(250));
                continue;
            }
            match self.codec.dequeue_output(if wait { WAIT_US } else { 0 })? {
                Output::TryAgain => return Ok(Poll::Pending),
                Output::FormatChanged => self.format_changed()?,
                Output::Buffer {
                    index,
                    size,
                    pts_us,
                    flags,
                    ..
                } => {
                    self.progress = self.progress.max(Progress::Output);
                    let end = flags & FLAG_END_OF_STREAM != 0;
                    if size > 0 && flags & FLAG_CODEC_CONFIG == 0 {
                        self.codec
                            .render_output(index, pts_us.saturating_mul(1000))?;
                        if let Some(surface) = self.surface.as_mut() {
                            surface.awaiting = Some((pts_us, end, Instant::now()));
                        }
                    } else {
                        self.codec.release_output(index)?;
                        if end {
                            self.drained = true;
                            return Ok(Poll::Drained);
                        }
                    }
                }
            }
        }
    }

    fn receive_bytes(&mut self, wait: bool) -> Result<Poll, BackendError> {
        loop {
            match self.codec.dequeue_output(if wait { WAIT_US } else { 0 })? {
                Output::TryAgain => return Ok(Poll::Pending),
                Output::FormatChanged => self.format_changed()?,
                Output::Buffer {
                    index,
                    offset,
                    size,
                    pts_us,
                    flags,
                } => {
                    self.progress = self.progress.max(Progress::Output);
                    let end = flags & FLAG_END_OF_STREAM != 0;
                    let picture = if size > 0 && flags & FLAG_CODEC_CONFIG == 0 {
                        if self.layout.is_none() {
                            self.read_layout()?;
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
}

impl VideoDecodeBackend for MediaCodecVideo {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        if self.depth == Depth::FirstFrame {
            self.depth = Depth::Eight;
            if vp9_bit_depth(&packet.data).is_some_and(|depth| depth > 8) {
                self.restart_ten_bit()?;
            }
        }
        let pts_us = self.to_us(packet.pts);
        while let Some(config) = self.config_pending.first() {
            let Some(index) = self.codec.dequeue_input(0)? else {
                return Ok(Submit::Busy);
            };
            self.codec
                .queue_input(index, config, pts_us, FLAG_CODEC_CONFIG)?;
            self.config_pending.remove(0);
        }
        let Some(index) = self.codec.dequeue_input(0)? else {
            return Ok(Submit::Busy);
        };
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
        self.last_us = Some(self.last_us.map_or(pts_us, |last| last.max(pts_us)));
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<Poll, BackendError> {
        if self.drained {
            return Ok(Poll::Drained);
        }
        if self.eos_pending {
            self.try_queue_eos()?;
        }
        let polled = if self.surface.is_some() {
            self.receive_surface(wait)
        } else {
            self.receive_bytes(wait)
        };
        match polled {
            Ok(Poll::Frame(frame)) => {
                self.progress = Progress::Picture;
                Ok(Poll::Frame(frame))
            }
            // A decoder that fails before its first picture does not decode this stream.
            Err(BackendError::Failed(reason)) if self.progress < Progress::Picture => {
                Err(BackendError::Unsupported(format!(
                    "{} failed before its first picture: {reason}",
                    self.codec.name()
                )))
            }
            polled => polled,
        }
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.eos_pending = true;
        self.try_queue_eos()
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.codec.flush()?;
        // A flush before the first output drops the parameter sets the codec was configured
        // with; they go in again ahead of the next packet.
        if self.progress == Progress::Started {
            self.config_pending.clone_from(&self.config);
        }
        if let Some(surface) = self.surface.as_mut()
            && surface.awaiting.take().is_some()
        {
            surface.stale += 1;
        }
        self.times.clear();
        self.last_us = None;
        self.eos_pending = false;
        self.drained = false;
        Ok(())
    }
}

/// Opens `MediaCodec` video sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AndroidVideo {
    /// Where hardware sessions put pictures.
    pub output: VideoOutput,
}

impl VideoBackends for AndroidVideo {
    fn open(
        &self,
        track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        Ok(Box::new(MediaCodecVideo::open(track, kind, self.output)?))
    }
}

#[cfg(test)]
mod tests {
    use alpha2_host_core::video_stream::BackendError;
    use alpha2_media::YuvFormat;

    use super::{Layout, media_image, read_picture};

    /// A `MediaImage2` of a 4x2 picture with YUV planes at `(offset, column, row)`.
    fn image(bit_depth: u32, allocated: u32, planes: [(u32, u32, u32); 3]) -> Vec<u8> {
        let mut words = vec![1, 3, 4, 2, bit_depth, allocated];
        for (index, (offset, column, row)) in planes.into_iter().enumerate() {
            let subsampling = if index == 0 { 1 } else { 2 };
            words.extend([offset, column, row, subsampling, subsampling]);
        }
        // Room for the fourth plane the struct holds.
        words.resize(26, 0);
        words.into_iter().flat_map(u32::to_le_bytes).collect()
    }

    fn layout(image: &[u8]) -> Layout {
        let (planes, bit_depth) = media_image(image).expect("readable").expect("YUV");
        Layout {
            width: 4,
            height: 2,
            bit_depth,
            planes,
            visible: None,
            sample_aspect_ratio: None,
        }
    }

    /// Luma `1..=8` in rows 6 bytes apart, then chroma from byte 12.
    fn buffer(chroma: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        bytes.extend_from_slice(chroma);
        bytes
    }

    #[test]
    fn interleaved_chroma_with_u_first_is_read_as_nv12() {
        let layout = layout(&image(8, 8, [(0, 1, 6), (12, 2, 6), (13, 2, 6)]));
        let (format, planes) = read_picture(&layout, &buffer(&[10, 20, 11, 21])).expect("planes");
        assert_eq!(format, YuvFormat::NV12);
        assert_eq!(planes[0].stride, 6);
        assert_eq!(planes[0].bytes[..10], [1, 2, 3, 4, 0, 0, 5, 6, 7, 8]);
        assert_eq!(planes[1].bytes[..4], [10, 20, 11, 21]);
    }

    #[test]
    fn interleaved_chroma_with_v_first_is_split_into_planes() {
        let layout = layout(&image(8, 8, [(0, 1, 6), (13, 2, 6), (12, 2, 6)]));
        let (format, planes) = read_picture(&layout, &buffer(&[20, 10, 21, 11])).expect("planes");
        assert_eq!(format, YuvFormat::I420);
        assert_eq!(planes[1].bytes[..], [10, 11]);
        assert_eq!(planes[2].bytes[..], [20, 21]);
    }

    #[test]
    fn planar_chroma_keeps_its_own_row_increment() {
        let layout = layout(&image(8, 8, [(0, 1, 6), (12, 1, 3), (15, 1, 3)]));
        let (format, planes) =
            read_picture(&layout, &buffer(&[10, 11, 0, 20, 21, 0])).expect("planes");
        assert_eq!(format, YuvFormat::I420);
        assert_eq!((planes[1].stride, planes[2].stride), (3, 3));
        assert_eq!(planes[1].bytes[..2], [10, 11]);
        assert_eq!(planes[2].bytes[..2], [20, 21]);
    }

    #[test]
    fn sixteen_bit_planar_chroma_is_interleaved_as_p010() {
        let layout = layout(&image(10, 16, [(0, 2, 8), (16, 2, 4), (20, 2, 4)]));
        let mut bytes = (1..=8_u16)
            .flat_map(|luma| (luma << 6).to_le_bytes())
            .collect::<Vec<_>>();
        for chroma in [100_u16, 101, 200, 201] {
            bytes.extend((chroma << 6).to_le_bytes());
        }
        let (format, planes) = read_picture(&layout, &bytes).expect("planes");
        assert_eq!(format, YuvFormat::P010);
        let samples = |bytes: &[u8]| {
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair) >> 6)
                .collect::<Vec<_>>()
        };
        assert_eq!(samples(&planes[0].bytes[..16]), (1..=8).collect::<Vec<_>>());
        assert_eq!(samples(&planes[1].bytes), [100, 200, 101, 201]);
    }

    #[test]
    fn a_short_buffer_or_other_subsampling_is_refused() {
        let layout = layout(&image(8, 8, [(0, 1, 6), (12, 2, 6), (13, 2, 6)]));
        assert!(read_picture(&layout, &buffer(&[10, 20])).is_err());
        // The U plane's vertical subsampling, word 15.
        let mut four_two_two = image(8, 8, [(0, 1, 6), (12, 2, 6), (13, 2, 6)]);
        four_two_two[60..64].copy_from_slice(&1_u32.to_le_bytes());
        assert!(matches!(
            media_image(&four_two_two),
            Err(BackendError::Unsupported(_))
        ));
        let mut rgb = image(8, 8, [(0, 1, 6), (12, 2, 6), (13, 2, 6)]);
        rgb[..4].copy_from_slice(&3_u32.to_le_bytes());
        assert!(matches!(media_image(&rgb), Ok(None)));
    }
}
