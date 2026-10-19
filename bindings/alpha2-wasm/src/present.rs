//! Surface presentation pipeline and playback frame plumbing.

use alpha2_engine::FrameProvider;
use alpha2_media::SourceId;
use alpha2_render::{
    PendingSourceWork, RenderCacheKey, RenderedTexture, SourceView, UploadTexture,
};
use alpha2_time::MediaTime;
use std::sync::atomic::{AtomicBool, Ordering};
use wasm_bindgen::prelude::*;

use crate::error::Code;
use crate::frame_store::FrameStore;
use crate::web::{coded_error, invalid_state};

/// Draws `source` into the canvas surface's current texture and presents it.
///
/// An outdated surface is reconfigured once, and so is a lost one while the
/// device is not known lost. A surface lost after that, or an invalid one,
/// marks the device lost (`GPU_DEVICE_LOST`); a timed-out or occluded one, or
/// an outdated one still unavailable after reconfiguring, answers
/// `GPU_TIMEOUT` and leaves the binding usable.
pub(crate) fn present_texture(
    surface: Option<&wgpu::Surface<'_>>,
    surface_config: Option<&wgpu::SurfaceConfiguration>,
    presenter: Option<&Presenter>,
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
        // A browser can drop a context's configuration while the device lives on (Firefox, after
        // `transferToImageBitmap`); wgpu reports a lost surface, so configure once more first.
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
    // wgpu on the web wraps browser GPU objects in dynamic borrow checks: drop the view so its
    // borrow does not overlap `present`.
    drop(target_view);
    gpu.present(current);
    Ok(())
}

/// Marks the binding's device lost after its canvas surface was lost.
fn surface_lost(device_lost: &AtomicBool) -> JsValue {
    device_lost.store(true, Ordering::Release);
    coded_error(Code::GPU_DEVICE_LOST, "canvas GPU surface was lost")
}

pub(crate) enum UploadedTextureBacking {
    Raw { _texture: UploadTexture },
    Rendered { _texture: RenderedTexture },
}

/// A source frame held for the engine: the texture that owns the memory, a
/// view of it, and the work that still has to run before it is read.
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

impl FrameProvider for FrameStore<UploadedTexture> {
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        self.get(source.as_str(), time.ticks())
            .map(|texture| SourceView::from_wgpu(&texture.view))
    }

    fn pending_source_work(
        &self,
        source: &SourceId,
        time: MediaTime,
    ) -> Option<&PendingSourceWork> {
        self.get(source.as_str(), time.ticks())
            .and_then(|texture| texture.pending.as_ref())
    }

    /// The source and the upload generation: an upload is presented once,
    /// so its texture is the same whenever and wherever it is drawn.
    fn source_key(&self, source: &SourceId, time: MediaTime) -> Option<RenderCacheKey> {
        let generation = self.generation(source.as_str(), time.ticks())?;
        let mut hash = std::hash::DefaultHasher::new();
        std::hash::Hash::hash(&(source.as_str(), generation), &mut hash);
        Some(RenderCacheKey::source(std::hash::Hasher::finish(&hash)))
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebPlaybackResult {
    pub(crate) requested_time_ticks: i64,
    /// JSON `null` until a frame has been presented, and after every
    /// `resetPlayer`: the TS consumers read `number | null`.
    pub(crate) presented_time_ticks: Option<i64>,
    pub(crate) missing_sources: Vec<String>,
    /// The exact frame each missing source was needed at, from the same
    /// snapshot. A trimmed, offset, or retimed clip needs its source at a
    /// time other than the timeline's.
    pub(crate) missing_frames: Vec<WebMissingFrame>,
    pub(crate) repeated: bool,
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

/// The pipeline that copies a rendered texture onto the canvas surface.
pub(crate) struct Presenter {
    pub(crate) layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) sampler: wgpu::Sampler,
}

impl Presenter {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("alpha2 canvas presenter layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("alpha2 canvas presenter shader"),
            source: wgpu::ShaderSource::Wgsl(alpha2_shaders::CANVAS_PRESENT_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("alpha2 canvas presenter pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("alpha2 canvas presenter pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("alpha2 canvas presenter sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            layout,
            pipeline,
            sampler,
        }
    }

    pub(crate) fn draw(
        &self,
        device: &wgpu::Device,
        gpu: &alpha2_gpu::GpuContext,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
    ) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("alpha2 canvas presenter bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("alpha2 canvas presenter encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("alpha2 canvas presenter pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        gpu.submit([encoder.finish()]);
    }
}
