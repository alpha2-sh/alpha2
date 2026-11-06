//! Surface presentation pipeline and playback frame plumbing.

use alpha2_engine::frame_store::StoredFrame;
use alpha2_render::{PendingSourceWork, RenderedTexture, SourceView, UploadTexture};
use std::sync::atomic::{AtomicBool, Ordering};
use wasm_bindgen::prelude::*;

use crate::error::Code;
use crate::web::{coded_error, invalid_state};

/// Draws `source` into the canvas surface's current texture and presents it.
///
/// An outdated surface, or a lost one while the device is not known lost, is reconfigured
/// once. A surface lost again, or invalid, marks the device lost (`GPU_DEVICE_LOST`). A
/// timed-out, occluded, or still-outdated surface answers `GPU_TIMEOUT` and leaves the binding
/// usable.
pub(crate) fn present_texture(
    surface: Option<&wgpu::Surface<'_>>,
    surface_config: Option<&wgpu::SurfaceConfiguration>,
    presenter: Option<&alpha2_render::SurfacePresenter>,
    device: &wgpu::Device,
    gpu: &alpha2_gpu::GpuContext,
    device_lost: &AtomicBool,
    source: &wgpu::TextureView,
) -> Result<(), JsValue> {
    let surface = surface.ok_or_else(|| invalid_state("renderer was created without a canvas"))?;
    let presenter = presenter.ok_or_else(|| invalid_state("canvas presenter is unavailable"))?;
    let current = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(texture)
        | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
        wgpu::CurrentSurfaceTexture::Outdated => {
            let config = surface_config
                .ok_or_else(|| invalid_state("surface configuration is unavailable"))?;
            surface.configure(device, config);
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(texture)
                | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
                _ => {
                    return Err(coded_error(
                        Code::GPU_TIMEOUT,
                        "canvas surface remains unavailable",
                    ));
                }
            }
        }
        // A browser can drop a context's configuration while the device lives on (Firefox after
        // `transferToImageBitmap`), which wgpu reports as lost, so configure once more.
        wgpu::CurrentSurfaceTexture::Lost if !device_lost.load(Ordering::Acquire) => {
            let config = surface_config
                .ok_or_else(|| invalid_state("surface configuration is unavailable"))?;
            surface.configure(device, config);
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(texture)
                | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
                _ => return Err(surface_lost(device_lost)),
            }
        }
        wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
            return Err(surface_lost(device_lost));
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
            // The surface may return on the next frame; the binding stays usable.
            return Err(coded_error(
                Code::GPU_TIMEOUT,
                "canvas surface is temporarily unavailable",
            ));
        }
    };
    let target_view = current
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    presenter.draw(device, gpu, source, &target_view);
    // wgpu on the web wraps browser GPU objects in dynamic borrow checks, so the view's borrow
    // must end before `present`.
    drop(target_view);
    gpu.present(current);
    Ok(())
}

fn surface_lost(device_lost: &AtomicBool) -> JsValue {
    device_lost.store(true, Ordering::Release);
    coded_error(Code::GPU_DEVICE_LOST, "canvas GPU surface was lost")
}

pub(crate) enum UploadedTextureBacking {
    Raw { _texture: UploadTexture },
    Rendered { _texture: RenderedTexture },
}

/// A source frame held for the engine: its texture, a view of it, and the work that must run
/// before it is read.
pub(crate) struct UploadedTexture {
    pub(crate) backing: UploadedTextureBacking,
    pub(crate) view: wgpu::TextureView,
    pub(crate) output_bytes: u64,
    pub(crate) pending: Option<PendingSourceWork>,
}

impl UploadedTexture {
    /// GPU bytes this frame keeps alive, counting its pending work.
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.output_bytes.saturating_add(
            self.pending
                .as_ref()
                .map_or(0, PendingSourceWork::retained_bytes),
        )
    }
}

impl StoredFrame for UploadedTexture {
    fn view(&self) -> SourceView<'_> {
        SourceView::from_wgpu(&self.view)
    }

    fn pending_work(&self) -> Option<&PendingSourceWork> {
        self.pending.as_ref()
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebPlaybackResult {
    pub(crate) requested_time_ticks: i64,
    /// `null` until a frame has been presented and after every `resetPlayer`.
    pub(crate) presented_time_ticks: Option<i64>,
    pub(crate) missing_sources: Vec<String>,
    /// The exact frame each missing source was needed at, which differs from the timeline time
    /// for a trimmed, offset, or retimed clip.
    pub(crate) missing_frames: Vec<WebMissingFrame>,
    /// Sources shown through the browser's own conversion (`uploadDegradedVideoFrame`), so the
    /// preview differs from an export.
    pub(crate) degraded_sources: Vec<String>,
    pub(crate) repeated: bool,
    /// The frame equals the last presented one, so nothing was rendered or presented.
    pub(crate) unchanged: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebMissingFrame {
    pub(crate) clip_id: String,
    /// The composition step that reads the source, when one does.
    pub(crate) reader_id: Option<String>,
    pub(crate) source_id: String,
    /// Decimal string, so JavaScript keeps every i64 bit.
    pub(crate) media_time_ticks: String,
}
