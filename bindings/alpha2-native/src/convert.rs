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
    // YCbCr layouts the renderer samples go as planes (shared when every byte is written, else
    // copied); the GPU applies subsampling, depth, crop, rotation, and aspect ratio.
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
    // Converting allocates a whole frame; a frame nothing in alpha2 renders is refused first.
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

/// Whether a frame of `color` the renderer cannot sample may reach it as
/// 8-bit sRGB-encoded RGBA: an SDR frame in BT.709 primaries, for which the
/// renderer's conversion is the matrix and range alone. Any other frame keeps
/// its own components so the renderer still tone-maps it, converts its
/// gamut, or refuses it, as it does a frame it samples.
fn converts_like_rgba8(color: ColorMetadata) -> bool {
    !color.is_hdr() && color.primaries == ColorPrimaries::Bt709
}

/// Converts `frame` to one plane of straight 8-bit RGBA with `color`'s
/// matrix and range.
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

/// Repacks `frame` into 4:4:4 planes of its own components at its own
/// depth (8, 10, 12, or 16 bits), its alpha in a plane after them, without
/// converting its color: luma and chroma stay in `color`'s matrix and range,
/// and red, green, and blue come out in that order, tagged with the identity
/// matrix. At its own depth swscale moves samples without rescaling them,
/// which it does not do exactly between depths.
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
        // The same matrix and range on both sides: swscale changes only depth, subsampling, and
        // layout.
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

/// The planar 4:4:4 format of RGB or YCbCr components, with alpha or
/// without, at `depth` bits in this machine's byte order.
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

/// The scaler a stream's frames convert with, kept for as long as they keep
/// the size and format it was made for.
///
/// `sws_getCachedContext` is no cache here: it reuses a context only when
/// the flags and source format it holds equal the ones asked for, and
/// `FFmpeg` rewrites both while it initialises one (full chroma
/// interpolation for RGB output from unsubsampled or odd-width input, the
/// alpha twin of an `RGB0`-style source), so it would remake the context for
/// every such frame.
#[derive(Default)]
pub(crate) struct FrameScaler {
    context: Option<(ScalerShape, SwsContext)>,
    made: u64,
}

/// What a converting scaler is made for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScalerShape {
    width: i32,
    height: i32,
    source: ffi::AVPixelFormat,
    target: ffi::AVPixelFormat,
}

impl FrameScaler {
    /// The scaler from `frame`'s size and format to `target` at that size,
    /// made only when the last one was made for another.
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

    /// How many scalers this has made.
    #[cfg(test)]
    pub(crate) const fn made(&self) -> u64 {
        self.made
    }

    /// Whether a scaler is held.
    #[cfg(test)]
    pub(crate) const fn is_empty(&self) -> bool {
        self.context.is_none()
    }
}

/// Bytes in a tightly packed `width`x`height` plane of `sample_bytes` per pixel.
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

/// Where the chroma of a `codec` stream or frame sits, given the location
/// `FFmpeg` reports for it.
///
/// H.264 infers chroma sample location type 0 (left) for a stream whose SPS
/// declares none (E.2.1), but `FFmpeg`'s decoder starts its SPS zeroed and
/// leaves a stream without VUI, or with a reserved type, unspecified, which
/// the renderer would sample centred. HEVC, the other codec with that rule,
/// already reports left.
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

/// `frame` as an encoder `AVFrame` built from its own planes, or `None` when
/// it is not I420 and has to be converted first.
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

/// Turns a decoded YUV frame of any layout into the 8-bit 4:2:0 an encoder
/// takes. A `Y'CbCr` frame keeps its codes and colour: only the sample depth
/// and the chroma resolution change. Planes of red, green, and blue (the
/// identity matrix, which a decoder gives an RGB frame outside BT.709's
/// gamut) are converted into the output's matrix and range, as an RGBA frame
/// is, their gamut left as it is. Alpha, which the output has no place for,
/// is left out. One stream's frames share one scaler.
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
    /// For a cropped frame, one whose size is not the encoder's, or a layout
    /// `FFmpeg` has no pixel format for.
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

/// A pixel aspect ratio as declared, in lowest terms, or `None` where it is
/// undeclared: `FFmpeg` writes an unknown ratio with a numerator of zero or
/// less, and starts every stream's at 0/1.
pub(crate) fn declared_sar(value: AVRational) -> Option<SampleAspectRatio> {
    SampleAspectRatio::new(
        u64::try_from(value.num).ok()?,
        u64::try_from(value.den).ok()?,
    )
}

/// The pixel aspect ratio a stream's pictures are shown with, by `FFmpeg`'s
/// rule (`av_guess_sample_aspect_ratio`): what the container declares, else
/// what `pictures` declare, else square pixels. For a stream, `pictures` are
/// its codec parameters (the SPS VUI, a sequence header); for a decoded
/// picture, its own ratio.
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
/// `Rotation` is what to apply to show the frame, turning clockwise, as
/// `VideoFrame.rotation` is. `FFmpeg`'s display matrix angle turns the other
/// way, so a stream written with `-display_rotation 90` shows turned 270
/// degrees.
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

/// The [`Orientation`] a display matrix declares; `None` for an angle that
/// is not a multiple of 90 degrees.
///
/// A mirroring matrix is, in `FFmpeg`'s reading, its rotation followed by a
/// horizontal flip. Flipping after a turn is flipping before the opposite
/// turn, so the mirror comes first and the angle changes sign: Exif
/// `Orientation` 5 (a rotation of 90 degrees clockwise, then a flip) is a
/// mirror turned 270 degrees.
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

/// The display matrix that shows a picture as `orientation` says, the
/// inverse of [`orientation_from_display`]; `None` for a picture shown as
/// stored, which needs none.
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

/// `hvc1`: an HEVC sample entry whose parameter sets are all in its `hvcC`.
const HVC1: u32 = u32::from_le_bytes(*b"hvc1");
/// `hev1`: one whose parameter sets may also be in the stream.
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
/// Matroska and `WebM` get `fflags=+bitexact`: without it, `matroskaenc` gives
/// every track, and a Matroska file its segment, a random UID, so two exports
/// of the same frames would differ. With it the track UIDs are their numbers,
/// the segment has no UID (it links to no other), and the writing
/// application is `Lavf` without a version. MP4 and MOV are deterministic
/// without it and keep their header as it is.
pub(crate) fn muxer_header_options(container: Container) -> Option<AVDictionary> {
    matches!(container, Container::Mkv | Container::Webm)
        .then(|| AVDictionary::new(c"fflags", c"+bitexact", 0))
}

/// The codec tag a stream of `codec_id` carrying `tag` is written to
/// `container` with.
///
/// MP4 and MOV name a stream's codec by its sample entry. HEVC is `hvc1`, as
/// browsers and Apple's players expect and as the browser exporter writes it;
/// an encoder's parameter sets are always in the sample description (global
/// header). A copied HEVC stream keeps the entry it came with, which says
/// where its parameter sets are. Any other copied stream's tag is cleared, so
/// the muxer picks its container's; an encoded one keeps what the encoder set.
///
/// Matroska and `WebM` name it by a `CodecID` string the muxer derives from the
/// codec, and a fourcc left over from another container can make it refuse
/// the stream, so the tag is always cleared there.
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
/// A tag that is set to anything unrecognized, or left unspecified beside
/// others, reads as its BT.709 (limited range) default; PQ and HLG make the
/// color HDR.
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
/// `flags=+unaligned` lets `FFmpeg` crop a software frame exactly. Without
/// it, a left crop is rounded down to keep plane pointers aligned, and one
/// below that alignment (64 columns of 8-bit 4:2:0) is dropped whole: the
/// frame then shows columns its stream crops away. The planes are shared
/// with the decoder, wherever they start. A hardware frame keeps its left
/// and top crop either way, as its visible rectangle.
pub(crate) fn video_decoder_options(threads: i64) -> AVDictionary {
    AVDictionary::new_int(c"threads", threads, 0).set(c"flags", c"+unaligned", 0)
}

/// Options every export sends `codec`, run on the GPU when `on_hardware`, for
/// frames `width` wide.
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

/// Each plane of a `width` x `height` frame of `layout` as its visible bytes
/// per row and rows, in `FFmpeg`'s plane order.
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

/// The options `prores_ks` writes `layout` with: 422 HQ, or 4444 with a
/// 16-bit alpha channel, tagged with Apple's vendor code so players treat
/// the stream as Apple `ProRes`.
/// A reusable encoder frame for `width` x `height` pictures of `layout`:
/// the planes [`alpha2_ffmpeg_bridge::Yuv420pFrameBuffer::begin_write_planes`]
/// hands out are the layout's, in its storage order.
///
/// # Errors
/// Returns an error for a size that does not fit a frame or a failed
/// allocation.
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
    // A hybrid encoder is a software transform until it is told otherwise, and `hw_encoding` exists
    // only on the encoders that are hybrid, so setting it anywhere else would trip
    // `reject_unused_encode_options`.
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

/// Speed and structure options a software encoder runs with, fixed per
/// encoder so an export is reproducible: the levels choose only the rate.
///
/// - `libvpx-vp9` runs libvpx's `good` deadline at `cpu-used` 4 with
///   row-based multithreading, one tile column per 256 pixels of width (the
///   most libvpx allows, and what its threads split across), and its
///   alt-ref frames over a 25-frame look-ahead.
/// - `libsvtav1` runs preset 8. SVT-AV1 sizes its threads from a level of
///   parallelism (1 to 6), not from `threads`; the level is the one SVT-AV1
///   would pick for a machine with `threads` cores, so the export's codec
///   thread budget still bounds it. Above level 1 its pictures can be coded
///   in whatever order its threads finish them (two encodes of the same
///   frames at levels 3 and 5 differed); [`deterministic_encode`] runs it at
///   level 1, which codes them in decode order.
///
/// `libvpx-vp9` writes the same bytes at any thread count.
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

/// Whether `ALPHA2_DETERMINISTIC_ENCODE=1` asks every software encoder for
/// output that is the same bytes on every run, at the cost of speed. Only
/// `libsvtav1` needs it: it then runs on one thread of pictures.
pub(crate) fn deterministic_encode() -> bool {
    std::env::var_os("ALPHA2_DETERMINISTIC_ENCODE").is_some_and(|value| value == "1")
}

/// `tile-columns` for libvpx: log2 of the tile columns a `width`-pixel frame
/// splits into, at least 256 pixels each.
pub(crate) fn vp9_tile_columns_log2(width: u32) -> u32 {
    (width / 256).max(1).ilog2().min(6)
}

/// SVT-AV1's level of parallelism for a budget of `threads` workers: the
/// level its own default gives a machine with that many cores (levels 1 to 6
/// for 1, 2, 3-5, 6-11, 12-23, and 24 or more cores).
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

/// Sets `key` to `value` in an encoder's options. A list-valued option
/// (`svtav1-params`, `:`-separated `key=value` pairs) already set gains
/// `value` instead of losing what it held.
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
/// A standalone context is capped at 16 workers. Without an override, two
/// cores are reserved for non-codec work. The export host uses
/// [`codec_thread_allocation`] to divide a budget of up to 64 workers across
/// all active contexts.
#[must_use]
pub fn codec_thread_budget() -> i64 {
    i64::try_from(configured_codec_thread_budget().min(16)).unwrap_or(1)
}

/// The software-codec worker budget `ALPHA2_FFMPEG_THREADS` sets
/// ([`codec_thread_override`]).
///
/// # Errors
/// Returns the setting when it is not a positive worker count, which the
/// budget ignores for its default: a caller that reports the budget's origin
/// says so rather than calling it explicit.
pub fn configured_codec_thread_override() -> Result<Option<usize>, String> {
    codec_thread_override(std::env::var_os("ALPHA2_FFMPEG_THREADS").as_deref())
}

/// The worker budget `setting` names: `None` unset, a positive count up to
/// 64 (clamped), or the setting itself as the error when it is anything else
/// (`0`, a word), which leaves the default in force.
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

/// The codec workers a machine of `hardware_threads` gives an export: all
/// but [`codec_thread_reserve`], at least one.
pub(crate) fn codec_thread_budget_for(hardware_threads: usize) -> usize {
    hardware_threads
        .saturating_sub(codec_thread_reserve(hardware_threads))
        .max(1)
}

/// Hardware threads the codec budget leaves to the renderer, the coordinator
/// and the rest of the machine: two on a machine of 16 or more, one on 6 to
/// 15, none below, where every thread is needed for the codecs.
///
/// Measured on a 4-core, 8-thread laptop CPU with two 4K H.264 sources
/// decoded in software side by side, three alternating rounds: with
/// `h264_mf`, a reserve of two and an encoder floor of four (budget 6, one
/// decoder thread each, encoder 4) left the export waiting on decode and
/// about half again slower per frame than this reserve and
/// [`encoder_thread_floor`] (budget 7, two decoder threads each, encoder 3).
/// `OpenH264`, whose eight 4K slice threads leave each decoder one thread
/// either way, ran about the same under both.
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
    /// The workers the software video encoder runs whatever it is offered,
    /// when its count is fixed by the picture (`OpenH264`'s one thread per
    /// slice): the split reserves exactly that for it and gives decode the
    /// rest, so the encoder threads it reports are the ones that run.
    pub encoder_fixed_threads: Option<usize>,
}

/// Luma samples one software decoder worker is sized to carry.
const DECODE_PIXELS_PER_THREAD: u64 = 2_000_000;
/// Smallest worker count a software decode session is given.
const MIN_SOFTWARE_DECODER_THREADS: usize = 2;
/// Largest worker count a software decode session is given. Frame threading
/// stops paying above this on a single stream, and every worker above it is
/// contending the encoder for cores instead.
const MAX_SOFTWARE_DECODER_THREADS: usize = 6;
/// Workers the software video encoder keeps whatever decode asks for, on a
/// budget of 12 or more; a third of a smaller budget ([`encoder_thread_floor`]).
const ENCODER_THREAD_FLOOR: usize = 4;

/// The encoder's floor out of a budget of `total_threads`: four, or a third
/// of a budget under 12, so a small machine's decoders are not starved for
/// an encoder that a two-thread share keeps busy.
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
/// Such a session decodes on the device; its workers only parse and submit.
/// Each one is a frame thread with its own codec context, and frame threading
/// holds back one packet per extra worker, which a session opened for a short
/// cut fills and then drops. Measured on a discrete NVIDIA GPU (`h264_vulkan`
/// decode and encode, 24-core budget) with 120 1080p sources in 295 cuts and
/// with one 60-second 4K source: the even share of the budget (22 workers)
/// took nearly twice as long on the many-cut project as two to four
/// workers, and the long source's decode was fastest at two to four.
///
/// Two, not one: the many-cut run is the same, and one worker doubles a long
/// source's decode time, which an encode-bound export hides but a decode-bound
/// one would not. The worker count changes no output byte. That GPU is the
/// only measurement: `VideoToolbox` and D3D12VA sessions take the same count
/// unmeasured.
pub const HARDWARE_DECODER_THREADS: usize = 2;

/// One export-wide software-codec allocation. Every software decoder receives
/// `decoder_threads`, every hardware one `hardware_decoder_threads`; the
/// software video encoder receives `encoder_threads`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecThreadAllocation {
    /// Total software codec workers available to this export.
    pub total_threads: usize,
    /// Number of decoder contexts allowed to execute concurrently.
    pub decode_concurrency: usize,
    /// `FFmpeg` worker count assigned to each software decoder context,
    /// including one a hardware session falls back to.
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
/// The configured value is an export-wide cap and a context always needs one
/// worker, so requested decoder concurrency is reduced before threads are
/// assigned. Hardware video encoding does not consume this software budget.
///
/// The split is weighted toward the output, but only when there is an output
/// to weight it toward. With a software video encoder a software decode
/// session is sized to its own frame area rather than to an even share, and
/// the encoder takes the remainder above a floor, because it is the stage that
/// paces the run: an even split gave two 27 %-utilized decoders 14 of 22
/// workers and the 98 %-utilized encoder 8. With a hardware video encoder
/// there is nobody to hand the difference to, and taking it from decode is
/// pure loss: measured on the 4K fixture with nvenc, cutting each software
/// decoder from 11 workers to 5 moved `decodeWaitMs` from 250 ms to 1,340 ms,
/// so decode stops being ahead of the renderer. That configuration keeps the
/// even split.
///
/// A hardware session opens with [`HARDWARE_DECODER_THREADS`] instead. The
/// share is still reserved for it, because any session can fall back to
/// software; and with a software encoder that keeps the encoder's worker
/// count, and so its output, the same whichever way the sources decode.
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
    // The encoder's floor is reserved before decode is sized, so a small budget starves the stage
    // with slack rather than the one on the path.
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
        // A fallback from hardware decode keeps the even share it was measured with (see above),
        // and with a hardware encoder nothing can use what decode would give up.
        even_share
    } else {
        // Never above the even share either: on a small budget, whose encoder floor is a third,
        // one large source would otherwise leave the encoder fewer workers than a decoder.
        software_decoder_threads(request.source_frame_pixels)
            .clamp(1, (decode_budget / decode_concurrency).max(1))
            .min(even_share)
    };
    let decode_assigned = decoder_threads.saturating_mul(decode_concurrency);
    // The floor is already reserved out of `decode_budget`; taking the remainder rather than the
    // floor keeps the total inside the budget when decode could not yield (the hardware case).
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

/// Workers one software decode session needs for a frame of this area.
///
/// Zero pixels means the source could not be probed, which takes the minimum
/// rather than the maximum: the decoders are not the stage with the deadline.
fn software_decoder_threads(source_frame_pixels: u64) -> usize {
    usize::try_from(source_frame_pixels.div_ceil(DECODE_PIXELS_PER_THREAD))
        .unwrap_or(MAX_SOFTWARE_DECODER_THREADS)
        .clamp(MIN_SOFTWARE_DECODER_THREADS, MAX_SOFTWARE_DECODER_THREADS)
}

/// The options an `FFmpeg` open left in its dictionary, which it did not consume.
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

/// The tags an encoder writes for `color`, as `FFmpeg` numbers them.
///
/// `FFmpeg`'s numbers are ITU-T H.273's code points, which an H.264 or HEVC
/// VUI and an AV1 sequence header carry as they are, so this is the one
/// table every export's colour signalling reads: [`color_options`], a copy's
/// source check, and the export bitstream filter.
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

/// The colour options an encoder opens with for `color`: [`color_tags`]'
/// values, so the encoder's fields, the container's `colr` box and the VUI
/// the export bitstream filter writes ([`crate::bitstream`]) all come from
/// one table.
pub(crate) fn color_options(color: ColorMetadata) -> AVDictionary {
    let (primaries, transfer, matrix, range) = color_tags(color);
    AVDictionary::new_int(c"color_primaries", i64::from(primaries), 0)
        .set_int(c"color_trc", i64::from(transfer), 0)
        .set_int(c"colorspace", i64::from(matrix), 0)
        .set_int(c"color_range", i64::from(range), 0)
}

/// What a failed native GPU call's status said about the device, when
/// `error` is one: the evidence the adapter errors below keep typed.
fn gpu_fault(error: &dyn std::any::Any) -> Option<alpha2_media::GpuFault> {
    error
        .downcast_ref::<alpha2_ffmpeg_bridge::BridgeError>()
        .and_then(alpha2_ffmpeg_bridge::BridgeError::gpu_fault)
}

/// `error` as a decoder adapter failure; a native GPU call's fault stays
/// typed ([`DecodeError::Gpu`]) instead of becoming only words.
pub(crate) fn adapter_error(error: impl std::fmt::Display + 'static) -> DecodeError {
    match gpu_fault(&error) {
        Some(fault) => DecodeError::Gpu {
            fault,
            message: error.to_string(),
        },
        None => DecodeError::Adapter(error.to_string()),
    }
}

/// `error` as an encoder adapter failure; a native GPU call's fault stays
/// typed ([`EncodeError::Gpu`]) instead of becoming only words.
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
