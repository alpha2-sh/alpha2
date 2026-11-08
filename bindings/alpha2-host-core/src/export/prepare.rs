//! Decoded frames made ready for the renderer: GPU textures, or CPU sources lending the planes.

use std::sync::Arc;

use alpha2_engine::frame_store::StoredFrame;
use alpha2_media::{DecodedVideoFrame, PixelFormat};
pub use alpha2_render::decoded::present_decoded as prepare_cpu;
use alpha2_render::decoded::{
    coded_size, plane, presentation, refuse_hdr, skipped_hdr, yuv_source,
};
use alpha2_render::{
    ColorPolicy, ExternalTextureLease, GpuVideoPlanes, PendingSourceWork, PreparedSourceTexture,
    RenderSize, RenderedTexture, Renderer, SourcePresentation, SourceView,
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
