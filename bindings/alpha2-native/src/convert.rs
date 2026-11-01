//! Frame conversion, colour mapping, and codec options.

use crate::capabilities::{NativeEncoderClass, encoder_class};
use alpha2_ffmpeg_bridge::{DecodedFrame, DisplayRotation};
use std::ffi::{CStr, CString};

use alpha2_media::{
    ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, Container, DecodeError,
    DecodedVideoFrame, DynamicRange, EncodeError, Orientation, PixelFormat, Plane, Rect, Rotation,
    SampleAspectRatio, TransferFunction, YuvFormat,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};
use rsmpeg::avcodec::AVCodec;
use rsmpeg::avutil::{AVDictionary, AVFrame, AVRational, av_rescale_q, ra};
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;

#[allow(clippy::too_many_lines)]
pub(crate) fn convert_video_frame(
    frame: &DecodedFrame,
    codec: ffi::AVCodecID,
    time_base: AVRational,
    color: ColorMetadata,
    scaler: &mut FrameScaler,
) -> Result<DecodedVideoFrame, DecodeError> {
    let width = u32::try_from(frame.width).map_err(|_| DecodeError::OutOfRange)?;
    let height = u32::try_from(frame.height).map_err(|_| DecodeError::OutOfRange)?;
    let pts = if frame.best_effort_timestamp == ffi::AV_NOPTS_VALUE {
        frame.pts
    } else {
        frame.best_effort_timestamp
    };
    let visible_rect = Rect {
        x: u32::try_from(frame.crop_left).unwrap_or(0),
        y: u32::try_from(frame.crop_top).unwrap_or(0),
        width: width.saturating_sub(
            u32::try_from(frame.crop_left.saturating_add(frame.crop_right)).unwrap_or(0),
        ),
        height: height.saturating_sub(
            u32::try_from(frame.crop_top.saturating_add(frame.crop_bottom)).unwrap_or(0),
        ),
    };
    // YCbCr layouts the renderer samples go as planes (shared when fully written, else copied); the
    // GPU applies subsampling, depth, crop, rotation and aspect ratio.
    if let Some(planes) = frame.yuv_planes().map_err(adapter_error)? {
        let layout = planes.layout;
        return Ok(DecodedVideoFrame {
            pts: timestamp(pts, time_base).ok_or(DecodeError::OutOfRange)?,
            dts: None,
            duration: timestamp(frame.duration, time_base),
            visible_rect,
            sample_aspect_ratio: declared_sar(frame.sample_aspect_ratio).unwrap_or_default(),
            orientation: Orientation::UPRIGHT,
            pixel_format: PixelFormat::Yuv(YuvFormat {
                chroma_shift_x: layout.chroma_shift_x,
                chroma_shift_y: layout.chroma_shift_y,
                bit_depth: layout.bit_depth,
                interleaved_chroma: layout.interleaved_chroma,
                msb_aligned: layout.msb_aligned,
                alpha: false,
            }),
            chroma_location: decoded_chroma_location(codec, frame.chroma_location),
            color: Some(color),
            planes: planes
                .planes
                .into_iter()
                .map(|plane| Plane {
                    bytes: plane.bytes,
                    stride: plane.stride,
                    width: plane.width,
                    height: plane.height,
                })
                .collect(),
        });
    }
    // Converting allocates a whole frame, so refuse unsupported sizes first.
    if !alpha2_media::frame_size_supported(width, height) {
        return Err(DecodeError::Unsupported(format!(
            "a {width}x{height} frame is larger than {} pixels or {} on a side",
            alpha2_media::MAX_FRAME_PIXELS,
            alpha2_media::MAX_FRAME_DIMENSION
        )));
    }
    let (pixel_format, planes, color) = if converts_like_rgba8(color) {
        (
            PixelFormat::Rgba8,
            vec![rgba8_plane(frame, color, scaler)?],
            color,
        )
    } else {
        full_resolution_planes(frame, color, scaler)?
    };
    Ok(DecodedVideoFrame {
        pts: timestamp(pts, time_base).ok_or(DecodeError::OutOfRange)?,
        dts: None,
        duration: timestamp(frame.duration, time_base),
        visible_rect,
        sample_aspect_ratio: declared_sar(frame.sample_aspect_ratio).unwrap_or_default(),
        orientation: Orientation::UPRIGHT,
        pixel_format,
        // Neither layout is subsampled.
        chroma_location: ChromaLocation::Unspecified,
        color: Some(color),
        planes,
    })
}

/// Whether an unsampleable frame of `color` may reach the renderer as 8-bit sRGB RGBA: SDR BT.709,
/// where conversion is matrix and range alone. Other frames keep their components so the renderer
/// can still tone-map, convert gamut, or refuse them.
fn converts_like_rgba8(color: ColorMetadata) -> bool {
    !color.is_hdr() && color.primaries == ColorPrimaries::Bt709
}

/// Converts `frame` to one plane of straight 8-bit RGBA with `color`'s matrix and range.
fn rgba8_plane(
    frame: &AVFrame,
    color: ColorMetadata,
    scaler: &mut FrameScaler,
) -> Result<Plane, DecodeError> {
    let width = u32::try_from(frame.width).map_err(|_| DecodeError::OutOfRange)?;
    let height = u32::try_from(frame.height).map_err(|_| DecodeError::OutOfRange)?;
    let active = scaler.for_frame(frame, ffi::AV_PIX_FMT_RGBA)?;
    configure_yuv_to_rgba(active, color).map_err(adapter_error)?;
    let mut bytes = vec![0; packed_len(width, height, 4)?];
    alpha2_ffmpeg_bridge::scale_frame_packed(active, frame, ffi::AV_PIX_FMT_RGBA, &mut bytes)
        .map_err(adapter_error)?;
    Ok(Plane {
        bytes: bytes.into(),
        stride: width.saturating_mul(4),
        width,
        height,
    })
}

/// Repacks `frame` into 4:4:4 planes at its own depth (alpha last) without converting color, tagged
/// with the identity matrix. Staying at its own depth avoids swscale's inexact depth rescaling.
fn full_resolution_planes(
    frame: &AVFrame,
    color: ColorMetadata,
    scaler: &mut FrameScaler,
) -> Result<(PixelFormat, Vec<Plane>, ColorMetadata), DecodeError> {
    let width = u32::try_from(frame.width).map_err(|_| DecodeError::OutOfRange)?;
    let height = u32::try_from(frame.height).map_err(|_| DecodeError::OutOfRange)?;
    let profile = alpha2_ffmpeg_bridge::sample_profile(frame.format)
        .ok_or_else(|| DecodeError::Unsupported("unsupported FFmpeg pixel format".into()))?;
    let depth = match profile.bit_depth {
        0..=8 => 8,
        9..=10 => 10,
        11..=12 => 12,
        _ => 16,
    };
    let target = planar_444_format(profile.is_rgb, profile.has_alpha, depth);
    let active = scaler.for_frame(frame, target)?;
    if !profile.is_rgb {
        // Same matrix and range on both sides: swscale changes only depth, subsampling and layout.
        let coefficients = sws_coefficients(color.matrix);
        let full = color.range == ColorRange::Full;
        alpha2_ffmpeg_bridge::configure_colorspace(active, coefficients, full, coefficients, full)
            .map_err(adapter_error)?;
    }
    let plane_count = if profile.has_alpha { 4 } else { 3 };
    let sample_bytes = if depth > 8 { 2 } else { 1 };
    let plane_len = packed_len(width, height, sample_bytes)?;
    let mut bytes = vec![0; plane_len * plane_count];
    alpha2_ffmpeg_bridge::scale_frame_packed(active, frame, target, &mut bytes)
        .map_err(adapter_error)?;
    let bytes = bytes::Bytes::from(bytes);
    let stride = width.saturating_mul(u32::try_from(sample_bytes).unwrap_or(2));
    let plane = |index: usize| Plane {
        bytes: bytes.slice(index * plane_len..(index + 1) * plane_len),
        stride,
        width,
        height,
    };
    // `gbrp` stores green, blue, red; the identity matrix reads red, green, blue.
    let order: &[usize] = if profile.is_rgb {
        &[2, 0, 1]
    } else {
        &[0, 1, 2]
    };
    let mut planes = order.iter().map(|&index| plane(index)).collect::<Vec<_>>();
    if profile.has_alpha {
        planes.push(plane(3));
    }
    let format = YuvFormat::planar(0, 0, depth);
    let format = if profile.has_alpha {
        format.with_alpha()
    } else {
        format
    };
    let color = if profile.is_rgb {
        ColorMetadata {
            matrix: ColorMatrix::Rgb,
            range: ColorRange::Full,
            ..color
        }
    } else {
        color
    };
    Ok((PixelFormat::Yuv(format), planes, color))
}

/// The planar 4:4:4 format for RGB or YCbCr, with or without alpha, at
/// `depth` in native byte order.
const fn planar_444_format(rgb: bool, alpha: bool, depth: u8) -> ffi::AVPixelFormat {
    match (rgb, alpha, depth) {
        (true, false, 8) => ffi::AV_PIX_FMT_GBRP,
        (true, true, 8) => ffi::AV_PIX_FMT_GBRAP,
        (true, false, 10) => ffi::AV_PIX_FMT_GBRP10LE,
        (true, true, 10) => ffi::AV_PIX_FMT_GBRAP10LE,
        (true, false, 12) => ffi::AV_PIX_FMT_GBRP12LE,
        (true, true, 12) => ffi::AV_PIX_FMT_GBRAP12LE,
        (true, false, _) => ffi::AV_PIX_FMT_GBRP16LE,
        (true, true, _) => ffi::AV_PIX_FMT_GBRAP16LE,
        (false, false, 8) => ffi::AV_PIX_FMT_YUV444P,
        (false, true, 8) => ffi::AV_PIX_FMT_YUVA444P,
        (false, false, 10) => ffi::AV_PIX_FMT_YUV444P10LE,
        (false, true, 10) => ffi::AV_PIX_FMT_YUVA444P10LE,
        (false, false, 12) => ffi::AV_PIX_FMT_YUV444P12LE,
        (false, true, 12) => ffi::AV_PIX_FMT_YUVA444P12LE,
        (false, false, _) => ffi::AV_PIX_FMT_YUV444P16LE,
        (false, true, _) => ffi::AV_PIX_FMT_YUVA444P16LE,
    }
}

/// The scaler a stream's frames convert with, kept while size and format stay the same.
///
/// `sws_getCachedContext` would remake it every frame here, because `FFmpeg` rewrites the stored
/// flags and source format during init (full chroma interpolation, `RGB0`-style alpha twins).
#[derive(Default)]
pub(crate) struct FrameScaler {
    context: Option<(ScalerShape, SwsContext)>,
    made: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScalerShape {
    width: i32,
    height: i32,
    source: ffi::AVPixelFormat,
    target: ffi::AVPixelFormat,
}

impl FrameScaler {
    /// The scaler from `frame`'s size and format to `target`, remade only when those change.
    fn for_frame(
        &mut self,
        frame: &AVFrame,
        target: ffi::AVPixelFormat,
    ) -> Result<&mut SwsContext, DecodeError> {
        let shape = ScalerShape {
            width: frame.width,
            height: frame.height,
            source: frame.format,
            target,
        };
        if self
            .context
            .as_ref()
            .is_none_or(|(made_for, _)| *made_for != shape)
        {
            self.context = None;
            let context = SwsContext::get_context(
                frame.width,
                frame.height,
                frame.format,
                frame.width,
                frame.height,
                target,
                ffi::SWS_BILINEAR,
                None,
                None,
                None,
            )
            .ok_or_else(|| DecodeError::Unsupported("unsupported FFmpeg pixel format".into()))?;
            self.made += 1;
            self.context = Some((shape, context));
        }
        self.context
            .as_mut()
            .map(|(_, context)| context)
            .ok_or_else(|| DecodeError::Unsupported("unsupported FFmpeg pixel format".into()))
    }

    #[cfg(test)]
    pub(crate) const fn made(&self) -> u64 {
        self.made
    }

    #[cfg(test)]
    pub(crate) const fn is_empty(&self) -> bool {
        self.context.is_none()
    }
}

fn packed_len(width: u32, height: u32, sample_bytes: usize) -> Result<usize, DecodeError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(sample_bytes))
        .ok_or(DecodeError::OutOfRange)
}

/// Where the chroma of a `codec` stream or frame sits, given what `FFmpeg` reports.
///
/// H.264 infers location type 0 (left) when the SPS declares none (E.2.1), but `FFmpeg` reports
/// unspecified, which the renderer would sample centred. HEVC already reports left.
pub(crate) fn decoded_chroma_location(
    codec: ffi::AVCodecID,
    location: ffi::AVChromaLocation,
) -> ChromaLocation {
    if codec == ffi::AV_CODEC_ID_H264 && location == ffi::AVCHROMA_LOC_UNSPECIFIED {
        ChromaLocation::Left
    } else {
        chroma_location(location)
    }
}

pub(crate) fn chroma_location(location: ffi::AVChromaLocation) -> ChromaLocation {
    match location {
        ffi::AVCHROMA_LOC_LEFT => ChromaLocation::Left,
        ffi::AVCHROMA_LOC_CENTER => ChromaLocation::Center,
        ffi::AVCHROMA_LOC_TOPLEFT => ChromaLocation::TopLeft,
        _ => ChromaLocation::Unspecified,
    }
}

/// Sets `scaler` to convert full-range RGB into `color`'s YCbCr matrix and range.
pub(crate) fn configure_rgba_to_yuv(
    scaler: &mut SwsContext,
    color: ColorMetadata,
) -> Result<(), alpha2_ffmpeg_bridge::BridgeError> {
    alpha2_ffmpeg_bridge::configure_colorspace(
        scaler,
        i32::try_from(ffi::SWS_CS_ITU709).expect("color coefficient fits i32"),
        true,
        sws_coefficients(color.matrix),
        color.range == ColorRange::Full,
    )
}

/// Sets `scaler` to convert `color`'s YCbCr matrix and range into full-range RGB.
pub(crate) fn configure_yuv_to_rgba(
    scaler: &mut SwsContext,
    color: ColorMetadata,
) -> Result<(), alpha2_ffmpeg_bridge::BridgeError> {
    alpha2_ffmpeg_bridge::configure_colorspace(
        scaler,
        sws_coefficients(color.matrix),
        color.range == ColorRange::Full,
        i32::try_from(ffi::SWS_CS_ITU709).expect("color coefficient fits i32"),
        true,
    )
}

pub(crate) fn sws_coefficients(matrix: ColorMatrix) -> i32 {
    let value = match matrix {
        ColorMatrix::Bt601 => ffi::SWS_CS_ITU601,
        ColorMatrix::Bt709 | ColorMatrix::Rgb => ffi::SWS_CS_ITU709,
        ColorMatrix::Bt2020Ncl => ffi::SWS_CS_BT2020,
    };
    i32::try_from(value).expect("color coefficient fits i32")
}

/// The engine's tick clock as an `FFmpeg` time base.
pub(crate) fn tick_base() -> AVRational {
    ra(
        1,
        i32::try_from(TICKS_PER_SECOND).expect("tick rate fits i32"),
    )
}

/// `frame` as an encoder `AVFrame` built from its own planes, or `None` when it is not I420.
///
/// # Errors
/// For an I420 frame that is cropped or not `width`x`height`.
pub(crate) fn direct_yuv420p_frame(
    frame: &DecodedVideoFrame,
    width: i32,
    height: i32,
) -> Result<Option<AVFrame>, EncodeError> {
    if frame.pixel_format != PixelFormat::Yuv(YuvFormat::I420) {
        return Ok(None);
    }
    if frame.planes.len() != 3
        || frame.visible_rect.x != 0
        || frame.visible_rect.y != 0
        || frame.planes[0].width != u32::try_from(width).unwrap_or(0)
        || frame.planes[0].height != u32::try_from(height).unwrap_or(0)
    {
        return Err(EncodeError::Adapter(
            "direct YUV encoder input must contain one uncropped coded frame".into(),
        ));
    }
    alpha2_ffmpeg_bridge::yuv420p_frame(
        &frame.planes[0].bytes,
        frame.planes[0].stride,
        &frame.planes[1].bytes,
        frame.planes[1].stride,
        &frame.planes[2].bytes,
        frame.planes[2].stride,
        width,
        height,
    )
    .map(Some)
    .map_err(encode_adapter_error)
}

/// Turns a decoded YUV frame of any layout into the encoder's 8-bit 4:2:0.
///
/// `Y'CbCr` keeps its codes and colour, changing only depth and chroma resolution. RGB planes
/// (identity matrix) are converted into the output's matrix and range like RGBA, gamut untouched.
/// Alpha is dropped. One stream's frames share one scaler.
pub(crate) struct Yuv420pConverter {
    output: ColorMetadata,
    scaler: Option<(ffi::AVPixelFormat, u32, u32, SwsContext)>,
}

impl Yuv420pConverter {
    /// A converter into 4:2:0 of `output`'s matrix and range.
    pub(crate) const fn new(output: ColorMetadata) -> Self {
        Self {
            output,
            scaler: None,
        }
    }

    /// `frame` as YUV420P, `None` for a frame that is not YUV.
    ///
    /// # Errors
    /// For a cropped frame, a size other than the encoder's, or a layout `FFmpeg` cannot represent.
    pub(crate) fn convert(
        &mut self,
        frame: &DecodedVideoFrame,
        width: i32,
        height: i32,
    ) -> Result<Option<AVFrame>, EncodeError> {
        let PixelFormat::Yuv(format) = frame.pixel_format else {
            return Ok(None);
        };
        let rgb = frame
            .color
            .is_some_and(|color| color.matrix == ColorMatrix::Rgb);
        if format == YuvFormat::I420 && !rgb {
            return direct_yuv420p_frame(frame, width, height);
        }
        let (width, height) = (
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        );
        let stored = if format.interleaved_chroma { 2 } else { 3 };
        if frame.visible_rect.x != 0
            || frame.visible_rect.y != 0
            || frame.planes.len() < stored
            || frame.planes[0].width != width
            || frame.planes[0].height != height
        {
            return Err(EncodeError::Adapter(
                "YUV encoder input must contain one uncropped frame of the encoder's size".into(),
            ));
        }
        let (source_format, planes) = if rgb {
            if format.chroma_shift_x != 0
                || format.chroma_shift_y != 0
                || format.interleaved_chroma
                || format.msb_aligned
                || !matches!(format.bit_depth, 8 | 10 | 12 | 16)
            {
                return Err(EncodeError::Adapter(format!(
                    "no FFmpeg pixel format stores red, green, and blue as {format:?}"
                )));
            }
            // `gbrp` stores green, blue, red; the identity matrix's planes are red, green, blue.
            let planes = [1, 2, 0]
                .iter()
                .map(|&index: &usize| (&frame.planes[index].bytes[..], frame.planes[index].stride))
                .collect::<Vec<_>>();
            (planar_444_format(true, false, format.bit_depth), planes)
        } else {
            let layout = alpha2_ffmpeg_bridge::YuvPlaneLayout {
                chroma_shift_x: format.chroma_shift_x,
                chroma_shift_y: format.chroma_shift_y,
                bit_depth: format.bit_depth,
                interleaved_chroma: format.interleaved_chroma,
                msb_aligned: format.msb_aligned,
            };
            let source_format =
                alpha2_ffmpeg_bridge::yuv_pixel_format(layout).ok_or_else(|| {
                    EncodeError::Adapter(format!("no FFmpeg pixel format stores {format:?}"))
                })?;
            let planes = frame.planes[..stored]
                .iter()
                .map(|plane| (&plane.bytes[..], plane.stride))
                .collect::<Vec<_>>();
            (source_format, planes)
        };
        let source = alpha2_ffmpeg_bridge::planar_frame(source_format, &planes, width, height)
            .map_err(encode_adapter_error)?;
        if self
            .scaler
            .as_ref()
            .is_none_or(|(cached, w, h, _)| (*cached, *w, *h) != (source_format, width, height))
        {
            let (signed_width, signed_height) = (
                i32::try_from(width).map_err(|_| EncodeError::Adapter("width overflow".into()))?,
                i32::try_from(height)
                    .map_err(|_| EncodeError::Adapter("height overflow".into()))?,
            );
            let mut scaler = SwsContext::get_context(
                signed_width,
                signed_height,
                source_format,
                signed_width,
                signed_height,
                ffi::AV_PIX_FMT_YUV420P,
                ffi::SWS_BILINEAR,
                None,
                None,
                None,
            )
            .ok_or_else(|| EncodeError::Adapter("cannot create a YUV to YUV420P scaler".into()))?;
            if rgb {
                configure_rgba_to_yuv(&mut scaler, self.output).map_err(encode_adapter_error)?;
            }
            self.scaler = Some((source_format, width, height, scaler));
        }
        let Some((_, _, _, scaler)) = self.scaler.as_mut() else {
            unreachable!("the scaler was just made");
        };
        let mut output = AVFrame::new();
        output.set_format(ffi::AV_PIX_FMT_YUV420P);
        output.set_width(source.width);
        output.set_height(source.height);
        output.get_buffer(1).map_err(encode_adapter_error)?;
        scaler
            .scale_frame(&source, 0, source.height, &mut output)
            .map_err(encode_adapter_error)?;
        Ok(Some(output))
    }
}

/// `value` in `base` as engine ticks; `None` for `AV_NOPTS_VALUE` or an invalid base.
pub(crate) fn timestamp(value: i64, base: AVRational) -> Option<MediaTime> {
    (value != ffi::AV_NOPTS_VALUE && base.den > 0)
        .then(|| MediaTime::from_ticks(av_rescale_q(value, base, tick_base())))
}

/// A declared pixel aspect ratio in lowest terms, or `None` when undeclared (`FFmpeg` uses a
/// numerator of zero or less, and 0/1 by default).
pub(crate) fn declared_sar(value: AVRational) -> Option<SampleAspectRatio> {
    SampleAspectRatio::new(
        u64::try_from(value.num).ok()?,
        u64::try_from(value.den).ok()?,
    )
}

/// A stream's pixel aspect ratio by `av_guess_sample_aspect_ratio`'s rule: the container's, else
/// the pictures' (codec parameters, or a decoded picture's own), else square.
pub(crate) fn guess_sar(container: AVRational, pictures: AVRational) -> SampleAspectRatio {
    declared_sar(container)
        .or_else(|| declared_sar(pictures))
        .unwrap_or_default()
}

/// The rotation a stream's legacy `rotate` metadata tag declares, `Deg0` without one.
pub(crate) fn stream_rotation(metadata: Option<rsmpeg::avutil::AVDictionaryRef<'_>>) -> Rotation {
    let Some(metadata) = metadata else {
        return Rotation::Deg0;
    };
    let Some(entry) = metadata.get(c"rotate", None, 0) else {
        return Rotation::Deg0;
    };
    match entry
        .value()
        .to_string_lossy()
        .parse::<i32>()
        .unwrap_or(0)
        .rem_euclid(360)
    {
        90 => Rotation::Deg90,
        180 => Rotation::Deg180,
        270 => Rotation::Deg270,
        _ => Rotation::Deg0,
    }
}

/// The clockwise [`Rotation`] for `FFmpeg`'s counter-clockwise display angle.
///
/// `FFmpeg`'s angle turns the other way, so `-display_rotation 90` shows turned 270 degrees.
pub(crate) fn rotation_from_display(counter_clockwise: f64) -> Option<Rotation> {
    let normalized = (-counter_clockwise).rem_euclid(360.0);
    if !(0.5..359.5).contains(&normalized) {
        Some(Rotation::Deg0)
    } else if (normalized - 90.0).abs() < 0.5 {
        Some(Rotation::Deg90)
    } else if (normalized - 180.0).abs() < 0.5 {
        Some(Rotation::Deg180)
    } else if (normalized - 270.0).abs() < 0.5 {
        Some(Rotation::Deg270)
    } else {
        None
    }
}

/// The [`Orientation`] a display matrix declares; `None` for an angle that is not a multiple of 90.
///
/// `FFmpeg` reads a mirroring matrix as rotation then horizontal flip, which equals flipping before
/// the opposite turn: Exif `Orientation` 5 is a mirror turned 270 degrees.
pub(crate) fn orientation_from_display(display: DisplayRotation) -> Option<Orientation> {
    if display.flipped {
        rotation_from_display(-display.counter_clockwise_degrees).map(|rotation| Orientation {
            rotation,
            mirror: true,
        })
    } else {
        rotation_from_display(display.counter_clockwise_degrees).map(Orientation::rotated)
    }
}

/// The display matrix for `orientation`, the inverse of [`orientation_from_display`];
/// `None` when shown as stored.
pub(crate) fn display_of(orientation: Orientation) -> Option<DisplayRotation> {
    if orientation == Orientation::default() {
        return None;
    }
    let clockwise = match orientation.rotation {
        Rotation::Deg0 => 0.0,
        Rotation::Deg90 => 90.0,
        Rotation::Deg180 => 180.0,
        Rotation::Deg270 => 270.0,
    };
    Some(DisplayRotation {
        counter_clockwise_degrees: if orientation.mirror {
            clockwise
        } else {
            -clockwise
        },
        flipped: orientation.mirror,
    })
}

/// `hvc1`: an HEVC sample entry with all parameter sets in its `hvcC`.
const HVC1: u32 = u32::from_le_bytes(*b"hvc1");
/// `hev1`: parameter sets may also be in the stream.
const HEV1: u32 = u32::from_le_bytes(*b"hev1");

/// The `FFmpeg` muxer that writes `container`.
pub(crate) const fn muxer_name(container: Container) -> &'static CStr {
    match container {
        Container::Mp4 => c"mp4",
        Container::Mov => c"mov",
        Container::Mkv => c"matroska",
        Container::Webm => c"webm",
    }
}

/// The options `container`'s muxer writes its header with.
///
/// Matroska and `WebM` get `fflags=+bitexact`, since otherwise `matroskaenc` writes random track
/// and segment UIDs and a versioned writing application. MP4 and MOV are deterministic already.
pub(crate) fn muxer_header_options(container: Container) -> Option<AVDictionary> {
    matches!(container, Container::Mkv | Container::Webm)
        .then(|| AVDictionary::new(c"fflags", c"+bitexact", 0))
}

/// The codec tag a stream of `codec_id` carrying `tag` is written to `container` with.
///
/// MP4 and MOV: encoded HEVC is `hvc1` (what browsers and Apple players expect), a copied HEVC
/// stream keeps its entry, other copied streams are cleared for the muxer to pick, and encoded
/// streams keep the encoder's tag. Matroska and `WebM` always clear it, since a foreign fourcc can
/// make the muxer refuse the stream.
pub(crate) fn container_codec_tag(
    container: Container,
    codec_id: ffi::AVCodecID,
    tag: u32,
    copied: bool,
) -> u32 {
    if matches!(container, Container::Mkv | Container::Webm) {
        0
    } else if codec_id == ffi::AV_CODEC_ID_HEVC {
        if tag == HVC1 || tag == HEV1 {
            tag
        } else {
            HVC1
        }
    } else if copied {
        0
    } else {
        tag
    }
}

pub(crate) fn color_metadata(
    parameters: &rsmpeg::avcodec::AVCodecParameters,
) -> Option<ColorMetadata> {
    map_color(
        parameters.color_primaries,
        parameters.color_trc,
        parameters.color_space,
        parameters.color_range,
    )
}

pub(crate) fn frame_color_metadata(frame: &AVFrame) -> Option<ColorMetadata> {
    map_color(
        frame.color_primaries,
        frame.color_trc,
        frame.colorspace,
        frame.color_range,
    )
}

/// The color `FFmpeg`'s tags describe, `None` when all four are unspecified.
///
/// An unrecognized tag, or an unspecified one beside others, reads as its BT.709 limited-range
/// default; PQ and HLG make the color HDR.
pub(crate) fn map_color(
    primaries: ffi::AVColorPrimaries,
    transfer: ffi::AVColorTransferCharacteristic,
    matrix: ffi::AVColorSpace,
    range: ffi::AVColorRange,
) -> Option<ColorMetadata> {
    if primaries == ffi::AVCOL_PRI_UNSPECIFIED
        && transfer == ffi::AVCOL_TRC_UNSPECIFIED
        && matrix == ffi::AVCOL_SPC_UNSPECIFIED
        && range == ffi::AVCOL_RANGE_UNSPECIFIED
    {
        return None;
    }
    let primaries = match primaries {
        ffi::AVCOL_PRI_BT2020 => ColorPrimaries::Bt2020,
        ffi::AVCOL_PRI_SMPTE170M | ffi::AVCOL_PRI_BT470BG => ColorPrimaries::Bt601,
        _ => ColorPrimaries::Bt709,
    };
    let transfer = match transfer {
        ffi::AVCOL_TRC_SMPTE2084 => TransferFunction::Pq,
        ffi::AVCOL_TRC_ARIB_STD_B67 => TransferFunction::Hlg,
        ffi::AVCOL_TRC_IEC61966_2_1 => TransferFunction::Srgb,
        _ => TransferFunction::Bt709,
    };
    let matrix = match matrix {
        ffi::AVCOL_SPC_BT2020_NCL => ColorMatrix::Bt2020Ncl,
        ffi::AVCOL_SPC_SMPTE170M | ffi::AVCOL_SPC_BT470BG => ColorMatrix::Bt601,
        ffi::AVCOL_SPC_RGB => ColorMatrix::Rgb,
        _ => ColorMatrix::Bt709,
    };
    Some(ColorMetadata {
        primaries,
        transfer,
        matrix,
        range: if range == ffi::AVCOL_RANGE_JPEG {
            ColorRange::Full
        } else {
            ColorRange::Limited
        },
        dynamic_range: if matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg) {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        },
    })
}

/// Options every video decoder opens with.
///
/// `flags=+unaligned` lets `FFmpeg` crop software frames exactly; without it a small left crop
/// (under 64 columns of 8-bit 4:2:0) is dropped to keep planes aligned. Hardware frames keep their
/// crop as the visible rectangle either way.
pub(crate) fn video_decoder_options(threads: i64) -> AVDictionary {
    AVDictionary::new_int(c"threads", threads, 0).set(c"flags", c"+unaligned", 0)
}

/// The `FFmpeg` pixel format of `layout`.
pub(crate) fn pixel_format(layout: alpha2_media::PixelLayout) -> ffi::AVPixelFormat {
    match layout.ffmpeg_name() {
        "yuva420p" => ffi::AV_PIX_FMT_YUVA420P,
        "yuv420p10le" => ffi::AV_PIX_FMT_YUV420P10LE,
        "yuva420p10le" => ffi::AV_PIX_FMT_YUVA420P10LE,
        "yuv422p" => ffi::AV_PIX_FMT_YUV422P,
        "yuva422p" => ffi::AV_PIX_FMT_YUVA422P,
        "yuv422p10le" => ffi::AV_PIX_FMT_YUV422P10LE,
        "yuva422p10le" => ffi::AV_PIX_FMT_YUVA422P10LE,
        "yuv444p" => ffi::AV_PIX_FMT_YUV444P,
        "yuva444p" => ffi::AV_PIX_FMT_YUVA444P,
        "yuv444p10le" => ffi::AV_PIX_FMT_YUV444P10LE,
        "yuva444p10le" => ffi::AV_PIX_FMT_YUVA444P10LE,
        "p010le" => ffi::AV_PIX_FMT_P010LE,
        _ => ffi::AV_PIX_FMT_YUV420P,
    }
}

/// Visible bytes per row and rows of each plane of `layout`, in `FFmpeg`'s plane order.
#[must_use]
pub(crate) fn frame_planes(
    layout: alpha2_media::PixelLayout,
    width: u32,
    height: u32,
) -> Vec<(usize, usize)> {
    layout
        .planes(width, height)
        .into_iter()
        .map(|(plane_width, rows, bytes)| ((plane_width * bytes) as usize, rows as usize))
        .collect()
}

/// A reusable encoder frame for `width` x `height` pictures of `layout`, its planes in
/// the layout's storage order.
///
/// # Errors
/// Returns an error for a size that does not fit a frame or a failed allocation.
pub fn packed_frame(
    width: u32,
    height: u32,
    layout: alpha2_media::PixelLayout,
) -> Result<alpha2_ffmpeg_bridge::Yuv420pFrameBuffer, alpha2_ffmpeg_bridge::BridgeError> {
    let (coded_width, coded_height) = (
        i32::try_from(width)
            .map_err(|_| alpha2_ffmpeg_bridge::BridgeError::InvalidInput("width"))?,
        i32::try_from(height)
            .map_err(|_| alpha2_ffmpeg_bridge::BridgeError::InvalidInput("height"))?,
    );
    if layout.is_yuv420p() {
        return alpha2_ffmpeg_bridge::Yuv420pFrameBuffer::new(coded_width, coded_height);
    }
    alpha2_ffmpeg_bridge::Yuv420pFrameBuffer::with_format(
        coded_width,
        coded_height,
        pixel_format(layout),
        &frame_planes(layout, width, height),
    )
}

pub(crate) fn prores_options(layout: alpha2_media::PixelLayout) -> Vec<(&'static CStr, String)> {
    let mut options = vec![
        (
            c"profile",
            if layout.alpha { "4444" } else { "hq" }.to_owned(),
        ),
        (c"vendor", "apl0".to_owned()),
    ];
    if layout.alpha {
        options.push((c"alpha_bits", "16".to_owned()));
    }
    options
}

pub(crate) fn video_encoder_options(
    color: ColorMetadata,
    codec: &AVCodec,
    on_hardware: bool,
    threads: i64,
    width: u32,
) -> AVDictionary {
    let mut options = color_options(color);
    // A hybrid encoder is a software transform unless told otherwise, and only hybrid encoders have
    // `hw_encoding`, so setting it elsewhere would trip `reject_unused_encode_options`.
    if encoder_class(codec) == NativeEncoderClass::Hybrid {
        options = options.set_int(c"hw_encoding", i64::from(on_hardware), 0);
    }
    if on_hardware {
        return options;
    }
    let name = codec.name().to_string_lossy();
    for (key, value) in software_encoder_profile(&name, width, threads) {
        options = set_encoder_option(options, key, &value);
    }
    options.set_int(c"threads", threads, 0)
}

/// Speed and structure options per software encoder, fixed so exports are reproducible.
///
/// - `libvpx-vp9`: `good` deadline, `cpu-used` 4, row multithreading, one tile column per 256
///   pixels of width, alt-ref frames over a 25-frame look-ahead. Same bytes at any thread count.
/// - `libsvtav1`: preset 8. SVT-AV1 sizes threads from a parallelism level, set to what it would
///   pick for `threads` cores. Above level 1 picture order depends on thread timing, so
///   [`deterministic_encode`] forces level 1.
fn software_encoder_profile(name: &str, width: u32, threads: i64) -> Vec<(&'static CStr, String)> {
    match name {
        "libvpx-vp9" => vec![
            (c"deadline", "good".into()),
            (c"cpu-used", "4".into()),
            (c"row-mt", "1".into()),
            (c"tile-columns", vp9_tile_columns_log2(width).to_string()),
            (c"lag-in-frames", "25".into()),
            (c"auto-alt-ref", "1".into()),
        ],
        "libsvtav1" => vec![
            (c"preset", "8".into()),
            (
                c"svtav1-params",
                format!(
                    "lp={}",
                    if deterministic_encode() {
                        1
                    } else {
                        svt_av1_parallelism(threads)
                    }
                ),
            ),
        ],
        _ => Vec::new(),
    }
}

/// Whether `ALPHA2_DETERMINISTIC_ENCODE=1` asks for byte-identical software encodes at the cost of
/// speed. Only `libsvtav1` needs it.
pub(crate) fn deterministic_encode() -> bool {
    std::env::var_os("ALPHA2_DETERMINISTIC_ENCODE").is_some_and(|value| value == "1")
}

/// libvpx `tile-columns`: log2 of the tile columns for `width`, each at least 256 pixels.
pub(crate) fn vp9_tile_columns_log2(width: u32) -> u32 {
    (width / 256).max(1).ilog2().min(6)
}

/// SVT-AV1's parallelism level for `threads` workers, matching its own default for that many cores
/// (1 to 6 for 1, 2, 3-5, 6-11, 12-23, and 24+ cores).
pub(crate) fn svt_av1_parallelism(threads: i64) -> u32 {
    match threads {
        ..=1 => 1,
        2 => 2,
        3..=5 => 3,
        6..=11 => 4,
        12..=23 => 5,
        _ => 6,
    }
}

/// Sets `key` to `value` in an encoder's options. A list-valued option already set
/// (`svtav1-params`, `:`-separated pairs) is appended to.
pub(crate) fn set_encoder_option(options: AVDictionary, key: &CStr, value: &str) -> AVDictionary {
    let value = CString::new(value).unwrap_or_default();
    if key == c"svtav1-params" && options.get(key, None, 0).is_some() {
        let appended = CString::new(format!(":{}", value.to_string_lossy())).unwrap_or_default();
        options.set(key, &appended, ffi::AV_DICT_APPEND)
    } else {
        options.set(key, &value, 0)
    }
}

/// Returns the worker budget for one standalone software video codec context.
///
/// Capped at 16 workers; by default two cores are reserved for non-codec work. Exports split up to
/// 64 workers across contexts with [`codec_thread_allocation`].
#[must_use]
pub fn codec_thread_budget() -> i64 {
    i64::try_from(configured_codec_thread_budget().min(16)).unwrap_or(1)
}

/// The software-codec worker budget `ALPHA2_FFMPEG_THREADS` sets ([`codec_thread_override`]).
///
/// # Errors
/// Returns the setting when it is not a positive count; the default then applies, and callers
/// reporting the budget's origin should not call it explicit.
pub fn configured_codec_thread_override() -> Result<Option<usize>, String> {
    codec_thread_override(std::env::var_os("ALPHA2_FFMPEG_THREADS").as_deref())
}

/// The worker budget `setting` names: `None` unset, or a positive count clamped to 64.
///
/// # Errors
/// Returns the setting, lossily as text, when it is not a positive count.
pub fn codec_thread_override(setting: Option<&std::ffi::OsStr>) -> Result<Option<usize>, String> {
    let Some(setting) = setting else {
        return Ok(None);
    };
    setting
        .to_str()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| Some(value.min(64)))
        .ok_or_else(|| setting.to_string_lossy().into_owned())
}

fn configured_codec_thread_budget() -> usize {
    configured_codec_thread_override()
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            codec_thread_budget_for(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )
        })
        .clamp(1, 64)
}

/// Codec workers for an export on `hardware_threads`: all but
/// [`codec_thread_reserve`], at least one.
pub(crate) fn codec_thread_budget_for(hardware_threads: usize) -> usize {
    hardware_threads
        .saturating_sub(codec_thread_reserve(hardware_threads))
        .max(1)
}

/// Hardware threads left to the renderer, coordinator and system: two on 16+
/// threads, one on 6-15, none below.
///
/// On a 4-core/8-thread laptop with two 4K software H.264 decodes, reserving two (and an encoder
/// floor of four) left `h264_mf` exports waiting on decode and about 50 % slower per frame.
pub(crate) const fn codec_thread_reserve(hardware_threads: usize) -> usize {
    match hardware_threads {
        16.. => 2,
        6..=15 => 1,
        _ => 0,
    }
}

/// What one export needs from the shared software-codec worker budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecThreadRequest {
    /// Decoder contexts the pool may run at once.
    pub decode_concurrency: usize,
    /// Whether decode sessions produce device-resident frames.
    pub hardware_decode: bool,
    /// Luma samples in the largest video source, or zero when unknown.
    pub source_frame_pixels: u64,
    /// Whether the video encoder draws on this budget.
    pub software_video_encoder: bool,
    /// Workers a software encoder runs regardless of the offer, when the picture
    /// fixes them (`OpenH264`'s one thread per slice). The split reserves exactly
    /// that and gives decode the rest.
    pub encoder_fixed_threads: Option<usize>,
}

/// Luma samples one software decoder worker is sized to carry.
const DECODE_PIXELS_PER_THREAD: u64 = 2_000_000;
/// Smallest worker count a software decode session is given.
const MIN_SOFTWARE_DECODER_THREADS: usize = 2;
/// Largest worker count a software decode session is given. Frame threading stops paying above this
/// on one stream, and extra workers only contend with the encoder.
const MAX_SOFTWARE_DECODER_THREADS: usize = 6;
/// Workers the software video encoder keeps on budgets of 12 or more; a third of
/// smaller budgets ([`encoder_thread_floor`]).
const ENCODER_THREAD_FLOOR: usize = 4;

/// The encoder's floor: four, or a third of a budget under 12 so small machines'
/// decoders are not starved.
const fn encoder_thread_floor(total_threads: usize) -> usize {
    let third = total_threads / 3;
    if third < ENCODER_THREAD_FLOOR {
        if third == 0 { 1 } else { third }
    } else {
        ENCODER_THREAD_FLOOR
    }
}
/// `FFmpeg` workers a hardware decode session opens with, whatever the budget.
///
/// Hardware sessions decode on the device; workers only parse and submit, and each extra frame
/// thread holds back a packet that short cuts fill and drop. On a discrete GPU, an even share (22
/// workers) took nearly twice as long on a many-cut project as two to four. One worker doubles a
/// long source's decode time. The count changes no output byte. `VideoToolbox` and D3D12VA use
/// the same count unmeasured.
pub const HARDWARE_DECODER_THREADS: usize = 2;

/// One export-wide software-codec allocation: `decoder_threads` per software decoder,
/// `hardware_decoder_threads` per hardware one, `encoder_threads` for the software encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecThreadAllocation {
    /// Total software codec workers available to this export.
    pub total_threads: usize,
    /// Number of decoder contexts allowed to execute concurrently.
    pub decode_concurrency: usize,
    /// `FFmpeg` workers per software decoder context, including a hardware session's fallback.
    pub decoder_threads: i64,
    /// `FFmpeg` workers a hardware decode session opens with.
    pub hardware_decoder_threads: i64,
    /// `FFmpeg` worker count assigned to the software video encoder, or zero for hardware.
    pub encoder_threads: i64,
    /// Workers this allocation actually hands out, decoders plus encoder.
    pub allocated_threads: usize,
}

/// Divides the export CPU budget across concurrently active codec contexts.
///
/// The budget is an export-wide cap and each context needs a worker, so decoder concurrency is
/// reduced first. Hardware video encoding does not consume this budget.
///
/// With a software encoder, which paces the run, software decoders are sized to their frame area
/// and the encoder takes the remainder above a floor. With a hardware encoder the even split stays,
/// since cutting decode workers only made decode fall behind the renderer.
///
/// Hardware decode sessions open with [`HARDWARE_DECODER_THREADS`], but their share is still
/// reserved for a software fallback, which also keeps the encoder's worker count (and output)
/// independent of how sources decode.
#[must_use]
pub fn codec_thread_allocation(request: CodecThreadRequest) -> CodecThreadAllocation {
    codec_thread_allocation_within(request, configured_codec_thread_budget())
}

/// [`codec_thread_allocation`] out of a budget of `configured` workers.
pub(crate) fn codec_thread_allocation_within(
    request: CodecThreadRequest,
    configured: usize,
) -> CodecThreadAllocation {
    let encoder_slots = usize::from(request.software_video_encoder);
    let decode_concurrency = request
        .decode_concurrency
        .max(1)
        .min(configured.saturating_sub(encoder_slots).max(1));
    let total_threads = configured.max(decode_concurrency.saturating_add(encoder_slots));
    // Reserve the encoder floor before sizing decode, so a small budget
    // starves the stage with slack.
    let fixed_encoder = request
        .encoder_fixed_threads
        .filter(|_| request.software_video_encoder)
        .map(|threads| threads.max(1));
    let encoder_floor = match fixed_encoder {
        Some(threads) => threads.min(total_threads.saturating_sub(decode_concurrency).max(1)),
        None if request.software_video_encoder => encoder_thread_floor(total_threads)
            .min(total_threads.saturating_sub(decode_concurrency).max(1)),
        None => 0,
    };
    let decode_budget = total_threads
        .saturating_sub(encoder_floor)
        .max(decode_concurrency);
    let even_share = if fixed_encoder.is_some() {
        // The encoder takes what it runs; decode shares the rest.
        (decode_budget / decode_concurrency).max(1)
    } else {
        let contexts = decode_concurrency.saturating_add(encoder_slots).max(1);
        (total_threads / contexts).max(1)
    };
    let decoder_threads = if request.hardware_decode || !request.software_video_encoder {
        // A hardware-decode fallback keeps the even share, and with a hardware encoder nothing
        // could use what decode gives up.
        even_share
    } else {
        // Never above the even share: on a small budget one large source would otherwise leave the
        // encoder fewer workers than a decoder.
        software_decoder_threads(request.source_frame_pixels)
            .clamp(1, (decode_budget / decode_concurrency).max(1))
            .min(even_share)
    };
    let decode_assigned = decoder_threads.saturating_mul(decode_concurrency);
    // Taking the remainder rather than the floor keeps the total inside the budget
    // when decode could not yield.
    let encoder_threads = match fixed_encoder {
        Some(threads) => threads,
        None if request.software_video_encoder => {
            total_threads.saturating_sub(decode_assigned).max(1)
        }
        None => 0,
    };
    CodecThreadAllocation {
        total_threads,
        decode_concurrency,
        decoder_threads: i64::try_from(decoder_threads).unwrap_or(1).clamp(1, 64),
        hardware_decoder_threads: i64::try_from(HARDWARE_DECODER_THREADS.min(total_threads))
            .unwrap_or(1)
            .max(1),
        encoder_threads: i64::try_from(encoder_threads).unwrap_or(0).clamp(0, 64),
        allocated_threads: decode_assigned.saturating_add(encoder_threads),
    }
}

/// Workers one software decode session needs for a frame of this area. Zero (unprobed) takes the
/// minimum, since decoders are not the stage with the deadline.
fn software_decoder_threads(source_frame_pixels: u64) -> usize {
    usize::try_from(source_frame_pixels.div_ceil(DECODE_PIXELS_PER_THREAD))
        .unwrap_or(MAX_SOFTWARE_DECODER_THREADS)
        .clamp(MIN_SOFTWARE_DECODER_THREADS, MAX_SOFTWARE_DECODER_THREADS)
}

/// The option names an `FFmpeg` open left unconsumed in its dictionary.
pub(crate) fn unused_option_names(options: Option<AVDictionary>) -> Option<String> {
    let options = options?;
    let names = options
        .iter()
        .map(|entry| entry.key().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| names.join(", "))
}

pub(crate) fn reject_unused_decode_options(
    options: Option<AVDictionary>,
) -> Result<(), DecodeError> {
    if let Some(names) = unused_option_names(options) {
        Err(DecodeError::Adapter(format!(
            "FFmpeg decoder did not consume options: {names}"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn reject_unused_encode_options(
    options: Option<AVDictionary>,
) -> Result<(), EncodeError> {
    if let Some(names) = unused_option_names(options) {
        Err(EncodeError::Adapter(format!(
            "FFmpeg encoder did not consume options: {names}"
        )))
    } else {
        Ok(())
    }
}

/// The tags an encoder writes for `color`, as `FFmpeg` (ITU-T H.273) numbers them.
///
/// The single table behind [`color_options`], a copy's source check, and
/// the export bitstream filter.
pub(crate) fn color_tags(
    color: ColorMetadata,
) -> (
    ffi::AVColorPrimaries,
    ffi::AVColorTransferCharacteristic,
    ffi::AVColorSpace,
    ffi::AVColorRange,
) {
    (
        match color.primaries {
            ColorPrimaries::Bt601 => ffi::AVCOL_PRI_SMPTE170M,
            ColorPrimaries::Bt709 => ffi::AVCOL_PRI_BT709,
            ColorPrimaries::Bt2020 => ffi::AVCOL_PRI_BT2020,
        },
        match color.transfer {
            TransferFunction::Bt709 => ffi::AVCOL_TRC_BT709,
            TransferFunction::Srgb => ffi::AVCOL_TRC_IEC61966_2_1,
            TransferFunction::Pq => ffi::AVCOL_TRC_SMPTE2084,
            TransferFunction::Hlg => ffi::AVCOL_TRC_ARIB_STD_B67,
        },
        match color.matrix {
            ColorMatrix::Bt601 => ffi::AVCOL_SPC_SMPTE170M,
            ColorMatrix::Bt709 => ffi::AVCOL_SPC_BT709,
            ColorMatrix::Bt2020Ncl => ffi::AVCOL_SPC_BT2020_NCL,
            ColorMatrix::Rgb => ffi::AVCOL_SPC_RGB,
        },
        match color.range {
            ColorRange::Limited => ffi::AVCOL_RANGE_MPEG,
            ColorRange::Full => ffi::AVCOL_RANGE_JPEG,
        },
    )
}

/// The colour options an encoder opens with, from [`color_tags`], so encoder fields, the `colr` box
/// and the bitstream filter's VUI ([`crate::bitstream`]) agree.
pub(crate) fn color_options(color: ColorMetadata) -> AVDictionary {
    let (primaries, transfer, matrix, range) = color_tags(color);
    AVDictionary::new_int(c"color_primaries", i64::from(primaries), 0)
        .set_int(c"color_trc", i64::from(transfer), 0)
        .set_int(c"colorspace", i64::from(matrix), 0)
        .set_int(c"color_range", i64::from(range), 0)
}

/// The device fault in a failed native GPU call's status, if `error` is one.
fn gpu_fault(error: &dyn std::any::Any) -> Option<alpha2_media::GpuFault> {
    error
        .downcast_ref::<alpha2_ffmpeg_bridge::BridgeError>()
        .and_then(alpha2_ffmpeg_bridge::BridgeError::gpu_fault)
}

/// `error` as a decoder adapter failure, keeping a GPU fault typed ([`DecodeError::Gpu`]).
pub(crate) fn adapter_error(error: impl std::fmt::Display + 'static) -> DecodeError {
    match gpu_fault(&error) {
        Some(fault) => DecodeError::Gpu {
            fault,
            message: error.to_string(),
        },
        None => DecodeError::Adapter(error.to_string()),
    }
}

/// `error` as an encoder adapter failure, keeping a GPU fault typed ([`EncodeError::Gpu`]).
pub(crate) fn encode_adapter_error(error: impl std::fmt::Display + 'static) -> EncodeError {
    match gpu_fault(&error) {
        Some(fault) => EncodeError::Gpu {
            fault,
            message: error.to_string(),
        },
        None => EncodeError::Adapter(error.to_string()),
    }
}

pub(crate) fn encode_adapter_error_at(
    context: &str,
    error: impl std::fmt::Display + 'static,
) -> EncodeError {
    match gpu_fault(&error) {
        Some(fault) => EncodeError::Gpu {
            fault,
            message: format!("{context}: {error}"),
        },
        None => EncodeError::Adapter(format!("{context}: {error}")),
    }
}
