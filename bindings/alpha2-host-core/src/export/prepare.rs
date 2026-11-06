//! Decoded frames made ready for the renderer: GPU textures, or CPU sources lending the planes.

use std::sync::Arc;

use alpha2_engine::frame_store::StoredFrame;
use alpha2_media::{ColorMetadata, DecodedVideoFrame, PixelFormat};
use alpha2_render::{
    ColorDiagnostic, ColorPolicy, CpuRenderer, CpuSource, ExternalTextureLease, GpuVideoPlanes,
    PendingSourceWork, PreparedSourceTexture, RenderSize, RenderedTexture, Renderer, RgbaPixels,
    SourceAlpha, SourcePlane, SourcePresentation, SourceView, YuvFrame, YuvPlanes,
};

use crate::video_stream::{FrameFacts, SurfaceLease};

/// A source picture on the GPU, with the work it still waits on before it may be sampled.
pub struct GpuSource {
    _texture: RenderedTexture,
    view: wgpu::TextureView,
    pending: Option<PendingSourceWork>,
    bytes: u64,
}

impl std::fmt::Debug for GpuSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GpuSource")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl GpuSource {
    /// Bytes the picture and its pending work keep alive.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.bytes.saturating_add(
            self.pending
                .as_ref()
                .map_or(0, PendingSourceWork::retained_bytes),
        )
    }
}

impl StoredFrame for GpuSource {
    fn view(&self) -> SourceView<'_> {
        SourceView::from_wgpu(&self.view)
    }

    fn pending_work(&self) -> Option<&PendingSourceWork> {
        self.pending.as_ref()
    }
}

/// Refuses an HDR frame a project whose color pipeline rejects HDR would show.
fn refuse_hdr(color: ColorPolicy, frame_color: Option<ColorMetadata>) -> Result<(), String> {
    if color == ColorPolicy::RejectHdr && frame_color.is_some_and(alpha2_render::is_hdr) {
        return Err(
            "the project refuses HDR sources (its color pipeline's hdr is \"reject\")".into(),
        );
    }
    Ok(())
}

fn presentation(frame: &DecodedVideoFrame, size: RenderSize) -> SourcePresentation {
    SourcePresentation {
        output_size: size,
        visible_rect: frame.visible_rect,
        orientation: frame.orientation,
        sample_aspect_ratio: (frame.sample_aspect_ratio.num, frame.sample_aspect_ratio.den),
        chroma_siting: frame.chroma_location,
    }
}

fn coded_size(frame: &DecodedVideoFrame) -> Result<RenderSize, String> {
    let coded = frame
        .planes
        .first()
        .ok_or_else(|| "the decoded frame has no planes".to_owned())?;
    RenderSize::new(coded.width, coded.height).map_err(|error| error.to_string())
}

fn plane(frame: &DecodedVideoFrame, index: usize) -> SourcePlane<'_> {
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
fn yuv_source(
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

fn skipped_hdr(diagnostics: &[ColorDiagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic, ColorDiagnostic::UnsupportedHdr))
}

/// Uploads a decoded frame for the GPU renderer, drawn at `size`.
///
/// # Errors
/// An HDR frame the color policy refuses, a plane count that does not match the pixel format,
/// an unsupported format, or a renderer failure.
pub fn prepare_gpu(
    renderer: &Renderer,
    size: RenderSize,
    color: ColorPolicy,
    frame: &DecodedVideoFrame,
) -> Result<GpuSource, String> {
    refuse_hdr(color, frame.color)?;
    let coded = coded_size(frame)?;
    let presentation = presentation(frame, size);
    let prepared = match frame.pixel_format {
        PixelFormat::Yuv(format) => {
            if frame.planes.len() != format.plane_count() {
                return Err(format!(
                    "a YUV frame must contain {} planes",
                    format.plane_count()
                ));
            }
            let (prepared, diagnostics) = renderer
                .prepare_yuv_presented(yuv_source(frame, format, coded), presentation, color)
                .map_err(|error| error.to_string())?;
            if skipped_hdr(&diagnostics) {
                return Err("the project skips HDR sources".into());
            }
            prepared
        }
        PixelFormat::Rgba8 => {
            if frame.planes.len() != 1 {
                return Err("an RGBA frame must contain one plane".into());
            }
            renderer
                .prepare_rgba8_presented(plane(frame, 0), coded, presentation)
                .map_err(|error| error.to_string())?
        }
        PixelFormat::LinearRgbaF16 => {
            if frame.planes.len() != 1 {
                return Err("a linear RGBA frame must contain one plane".into());
            }
            let upload = renderer
                .write_linear_rgba16f(coded, plane(frame, 0))
                .map_err(|error| error.to_string())?;
            renderer
                .prepare_linear_presented(upload, coded, presentation)
                .map_err(|error| error.to_string())?
        }
        other => return Err(format!("the decoder returned {other:?}")),
    };
    Ok(gpu_source(prepared, size))
}

fn gpu_source(prepared: PreparedSourceTexture, size: RenderSize) -> GpuSource {
    let (presented, pending) = prepared.into_parts();
    let view = presented.view().clone();
    let bytes = u64::from(size.width) * u64::from(size.height) * 8;
    GpuSource {
        _texture: presented,
        view,
        pending,
        bytes,
    }
}

/// Prepares a frame whose planes are already GPU textures, drawn at `size`; `lease` keeps what
/// the planes need until the renderer's work on them completes.
///
/// # Errors
/// An HDR frame the color policy refuses, planes that do not match their format, or a renderer
/// failure.
pub fn prepare_gpu_planes(
    renderer: &Renderer,
    size: RenderSize,
    color: ColorPolicy,
    facts: &FrameFacts,
    planes: GpuVideoPlanes<'_>,
    lease: Arc<dyn ExternalTextureLease>,
) -> Result<GpuSource, String> {
    refuse_hdr(color, planes.color)?;
    let presentation = SourcePresentation {
        output_size: size,
        visible_rect: facts.visible_rect,
        orientation: facts.orientation,
        sample_aspect_ratio: (facts.sample_aspect_ratio.num, facts.sample_aspect_ratio.den),
        chroma_siting: facts.chroma_location,
    };
    let (prepared, diagnostics) = renderer
        .prepare_gpu_yuv_presented(planes, presentation, color, lease)
        .map_err(|error| error.to_string())?;
    if skipped_hdr(&diagnostics) {
        return Err("the project skips HDR sources".into());
    }
    Ok(gpu_source(prepared, size))
}

/// Turns a platform decoder's surfaces into GPU sources: the GPU renderer's path for frames that
/// stay on the GPU, made by the platform that decodes them.
pub trait SurfaceImport: Send + Sync {
    /// The surface `lease` of a frame with `facts`, prepared for `renderer` at `size`.
    ///
    /// # Errors
    /// A surface of another platform, a failed import, or an error of [`prepare_gpu_planes`].
    fn prepare(
        &self,
        renderer: &Renderer,
        size: RenderSize,
        color: ColorPolicy,
        facts: &FrameFacts,
        lease: &Arc<dyn SurfaceLease>,
    ) -> Result<GpuSource, String>;
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
pub fn prepare_cpu(
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
