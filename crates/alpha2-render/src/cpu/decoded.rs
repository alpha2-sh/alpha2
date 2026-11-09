//! Lends decoded frames to the CPU renderer, which draws the decoder's own planes at a size.

use std::sync::Arc;

use alpha2_media::{ColorMetadata, DecodedVideoFrame, PixelFormat};

use crate::{
    ColorDiagnostic, ColorPolicy, CpuRenderer, CpuSource, RenderSize, RgbaPixels, SourceAlpha,
    SourcePlane, SourcePresentation, YuvFrame, YuvPlanes,
};

/// Refuses an HDR frame a project whose color pipeline rejects HDR would show.
///
/// # Errors
/// The refusal, for an HDR frame under [`ColorPolicy::RejectHdr`].
pub fn refuse_hdr(color: ColorPolicy, frame_color: Option<ColorMetadata>) -> Result<(), String> {
    if color == ColorPolicy::RejectHdr && frame_color.is_some_and(crate::is_hdr) {
        return Err(
            "the project refuses HDR sources (its color pipeline's hdr is \"reject\")".into(),
        );
    }
    Ok(())
}

/// How `frame` is shown at `size`, from its visible rectangle, orientation, aspect and siting.
#[must_use]
pub fn presentation(frame: &DecodedVideoFrame, size: RenderSize) -> SourcePresentation {
    SourcePresentation {
        output_size: size,
        visible_rect: frame.visible_rect,
        orientation: frame.orientation,
        sample_aspect_ratio: (frame.sample_aspect_ratio.num, frame.sample_aspect_ratio.den),
        chroma_siting: frame.chroma_location,
    }
}

/// The size of `frame`'s first plane.
///
/// # Errors
/// A frame without planes, or of a size the renderer refuses.
pub fn coded_size(frame: &DecodedVideoFrame) -> Result<RenderSize, String> {
    let coded = frame
        .planes
        .first()
        .ok_or_else(|| "the decoded frame has no planes".to_owned())?;
    RenderSize::new(coded.width, coded.height).map_err(|error| error.to_string())
}

/// Plane `index` of `frame`, or an empty one.
#[must_use]
pub fn plane(frame: &DecodedVideoFrame, index: usize) -> SourcePlane<'_> {
    frame.planes.get(index).map_or(
        SourcePlane {
            bytes: &[],
            stride: 0,
        },
        |plane| SourcePlane {
            bytes: &plane.bytes,
            stride: plane.stride,
        },
    )
}

/// Luma and chroma planes, then the alpha plane when the format has one.
#[must_use]
pub fn yuv_source(
    frame: &DecodedVideoFrame,
    format: alpha2_media::YuvFormat,
    size: RenderSize,
) -> YuvFrame<'_> {
    let colors = format.color_plane_count();
    // With interleaved chroma the third plane is not read: an empty one, never the alpha plane.
    let third = if colors == 3 {
        plane(frame, 2)
    } else {
        SourcePlane {
            bytes: &[],
            stride: 0,
        }
    };
    YuvFrame {
        format,
        planes: [plane(frame, 0), plane(frame, 1), third],
        alpha_plane: format.alpha.then(|| plane(frame, colors)),
        size,
        alpha: 1.0,
        color: frame.color,
    }
}

/// Whether the renderer skipped an HDR source the policy does not draw.
#[must_use]
pub fn skipped_hdr(diagnostics: &[ColorDiagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic, ColorDiagnostic::UnsupportedHdr))
}

#[derive(Debug)]
struct LentPlanes {
    frame: Arc<DecodedVideoFrame>,
    format: alpha2_media::YuvFormat,
    size: RenderSize,
}

impl YuvPlanes for LentPlanes {
    fn frame(&self) -> YuvFrame<'_> {
        yuv_source(&self.frame, self.format, self.size)
    }
}

#[derive(Debug)]
struct LentRgba(Arc<DecodedVideoFrame>);

impl RgbaPixels for LentRgba {
    fn plane(&self) -> SourcePlane<'_> {
        plane(&self.0, 0)
    }
}

/// Lends a decoded frame's planes to the CPU renderer, drawn at `size`.
///
/// # Errors
/// An HDR frame the color policy refuses, a plane count that does not match the pixel format, or
/// a renderer failure.
pub fn present_decoded(
    renderer: &CpuRenderer,
    size: RenderSize,
    color: ColorPolicy,
    frame: Arc<DecodedVideoFrame>,
) -> Result<CpuSource, String> {
    refuse_hdr(color, frame.color)?;
    let coded = coded_size(&frame)?;
    let presentation = presentation(&frame, size);
    match frame.pixel_format {
        PixelFormat::Yuv(format) => {
            if frame.planes.len() != format.plane_count() {
                return Err(format!(
                    "a YUV frame must contain {} planes",
                    format.plane_count()
                ));
            }
            let planes = Arc::new(LentPlanes {
                frame,
                format,
                size: coded,
            });
            let (source, diagnostics) = renderer
                .present_yuv(planes, presentation, color)
                .map_err(|error| error.to_string())?;
            if skipped_hdr(&diagnostics) {
                return Err("the project skips HDR sources".into());
            }
            Ok(source)
        }
        PixelFormat::Rgba8 | PixelFormat::LinearRgbaF16 => {
            if frame.planes.len() != 1 {
                return Err("an RGBA frame must contain one plane".into());
            }
            let stride = frame.planes[0].stride;
            let linear = frame.pixel_format == PixelFormat::LinearRgbaF16;
            let pixels = Arc::new(LentRgba(frame));
            if linear {
                renderer.present_linear_rgba16f(pixels, coded, stride, presentation)
            } else {
                renderer.present_rgba8(pixels, coded, stride, presentation, SourceAlpha::Straight)
            }
            .map_err(|error| error.to_string())
        }
        other => Err(format!("the decoder returned {other:?}")),
    }
}
