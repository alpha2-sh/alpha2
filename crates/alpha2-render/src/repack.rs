//! Decoded 4:2:0 pictures copied plane for plane into NV12 targets.
//!
//! When an output frame is exactly one decoded picture (the same size and
//! color, shown with an identity presentation and nothing drawn over it), the
//! render graph and its YUV-to-RGB-to-YUV round trip change nothing a viewer
//! should see, yet cost two conversions and their rounding. A repack is the
//! transcode path's replacement for both: it reads each 8-bit code and writes
//! the same code into the encoder's planes. Whether an export may take it is
//! the host's decision; this checks only what the copy itself needs.

use std::sync::Arc;

use alpha2_media::{Rect, YuvFormat};
use alpha2_shaders::YUV_REPACK_WGSL;

use crate::graph_submit::{FrameSubmitTimings, begin_source_work, stage_source_submissions};
use crate::pipeline::CachedYuvRepackPipeline;
use crate::yuv::{
    ExternalTextureLease, GpuVideoPlanes, PendingSourceWork, SourceWork, YuvFrame, upload_plane,
    validate_plane,
};
use crate::{Nv12PlaneTargets, RenderError, RenderSize, Renderer, UploadTexture};

/// The decoded picture a repack reads.
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub enum RepackSource<'a> {
    /// Planes in CPU memory, uploaded by the repack.
    Cpu(YuvFrame<'a>),
    /// Planes already on the renderer's device, and the lease that keeps
    /// them alive until the repack's submission completes.
    Gpu {
        /// The decoded planes.
        planes: GpuVideoPlanes<'a>,
        /// Their native ownership and synchronization.
        lease: Arc<dyn ExternalTextureLease>,
    },
}

impl RepackSource<'_> {
    const fn format(&self) -> YuvFormat {
        match self {
            Self::Cpu(frame) => frame.format,
            Self::Gpu { planes, .. } => planes.format,
        }
    }

    const fn size(&self) -> RenderSize {
        match self {
            Self::Cpu(frame) => frame.size,
            Self::Gpu { planes, .. } => planes.size,
        }
    }
}

impl Renderer {
    /// Copies the `visible` part of a decoded 8-bit 4:2:0 picture into NV12
    /// plane targets of the same size, sample for sample, in one submission.
    ///
    /// No color is converted, no graph runs, and nothing is sampled with a
    /// filter: the targets hold the decoder's codes. Diagnostics count the
    /// frame as a repack and not as a source conversion.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] for a picture that is not
    /// 8-bit 4:2:0, malformed CPU planes, or a visible rectangle that is
    /// outside the coded picture or does not start on a chroma sample;
    /// [`RenderError::InvalidPlaneTarget`] for targets whose size or format
    /// differ from the picture's; [`RenderError::ExternalTextureSync`] when a
    /// lease cannot stage its synchronization.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn repack_nv12_timed(
        &self,
        source: RepackSource<'_>,
        visible: Rect,
        targets: Nv12PlaneTargets<'_>,
        timings: &mut FrameSubmitTimings,
    ) -> Result<(), RenderError> {
        let started = web_time::Instant::now();
        let format = source.format();
        if format != YuvFormat::I420 && format != YuvFormat::NV12 {
            return Err(RenderError::InvalidImageData);
        }
        let coded = source.size();
        self.validate_size(coded)?;
        let output = RenderSize::new(visible.width, visible.height)?;
        let inside = visible
            .x
            .checked_add(visible.width)
            .is_some_and(|right| right <= coded.width)
            && visible
                .y
                .checked_add(visible.height)
                .is_some_and(|bottom| bottom <= coded.height);
        if !inside
            || !visible.x.is_multiple_of(2)
            || !visible.y.is_multiple_of(2)
            || !output.width.is_multiple_of(2)
            || !output.height.is_multiple_of(2)
        {
            return Err(RenderError::InvalidImageData);
        }
        check_targets(targets, output)?;
        self.check_submitted_work()?;
        let passes = self.repack_passes(source, visible, targets)?;
        self.submit_repack(&passes, started, timings)
    }

    /// The luma and chroma passes of one repack. The lease and uploads ride
    /// with the first; both are in one submission, which is what they must
    /// outlive.
    fn repack_passes(
        &self,
        source: RepackSource<'_>,
        visible: Rect,
        targets: Nv12PlaneTargets<'_>,
    ) -> Result<[PendingSourceWork; 2], RenderError> {
        let format = source.format();
        let (uploads, planes, lease) = match source {
            RepackSource::Cpu(frame) => {
                let uploads = self.upload_repack_planes(&frame)?;
                let views = uploads
                    .iter()
                    .map(|upload| upload.view().clone())
                    .collect::<Vec<_>>();
                (uploads, views, None)
            }
            RepackSource::Gpu { planes, lease } => (
                Vec::new(),
                [Some(planes.y), Some(planes.chroma), planes.v]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect(),
                Some(lease),
            ),
        };
        if format.interleaved_chroma != (planes.len() == 2) {
            return Err(RenderError::InvalidImageData);
        }
        let cached = self.yuv_repack_pipeline();
        let origin = [visible.x, visible.y, visible.x / 2, visible.y / 2]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        let pass = |label: &'static str,
                    pipeline: &wgpu::RenderPipeline,
                    target: &wgpu::Texture,
                    uploads: Vec<UploadTexture>,
                    external_leases: Vec<Arc<dyn ExternalTextureLease>>| {
            let mut uniforms = self.uniform_buffers.acquire(
                self.gpu.device(),
                u64::try_from(origin.len()).unwrap_or(u64::MAX),
            );
            let slice = uniforms.write(&origin);

            let bind_group = self
                .gpu
                .device()
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("alpha2 YUV repack bind group"),
                    layout: &cached.bind_group_layout,
                    entries: &[
                        texture_entry(0, &planes[0]),
                        texture_entry(1, &planes[1]),
                        texture_entry(2, planes.get(2).unwrap_or(&planes[1])),
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: uniforms.binding(slice),
                        },
                    ],
                });
            PendingSourceWork::new(SourceWork {
                label,
                conversion: false,
                pipeline: pipeline.clone(),
                bind_group,
                output_view: crate::pool::default_view(target),
                uniforms,
                uploads,
                external_leases,
            })
        };
        Ok([
            pass(
                "alpha2 YUV repack luma pass",
                &cached.luma,
                targets.luma,
                uploads,
                lease.into_iter().collect(),
            ),
            pass(
                "alpha2 YUV repack chroma pass",
                if format.interleaved_chroma {
                    &cached.interleaved_chroma
                } else {
                    &cached.planar_chroma
                },
                targets.chroma,
                Vec::new(),
                Vec::new(),
            ),
        ])
    }

    /// Encodes and submits a repack's passes, staging its lease's
    /// synchronization on that one submission.
    fn submit_repack(
        &self,
        passes: &[PendingSourceWork; 2],
        started: web_time::Instant,
        timings: &mut FrameSubmitTimings,
    ) -> Result<(), RenderError> {
        let frame = self.frame_lock.hold();
        let mut command_encoder = self.command_encoder("alpha2 YUV repack encoder");
        let work = begin_source_work(&frame, &[&passes[0], &passes[1]]);
        for ticket in &work {
            ticket.encode(&mut command_encoder);
        }
        // Like an NV12 pack, a repack reads nothing back; its canary rides in a buffer of
        // its own.
        let (canary_buffer, canary) = self.canaries.encode_own(&self.gpu, &mut command_encoder)?;
        let recorded = started.elapsed();
        let staged = web_time::Instant::now();
        stage_source_submissions(self.gpu.queue(), &work)?;
        let source_acquire = staged.elapsed();
        let finishing = web_time::Instant::now();
        let command_buffer = command_encoder.finish();
        let command_encode = recorded.saturating_add(finishing.elapsed());
        let submitted = self.submit_measured([command_buffer], true)?;
        self.submission_canaries
            .track(&self.gpu, canary_buffer, canary);
        let completing = web_time::Instant::now();
        // One submission carries both passes, so it is counted once.
        for (index, ticket) in work.into_iter().enumerate() {
            ticket.complete(self, index == 0);
        }
        crate::diagnostics::record_yuv_repack(&self.diagnostics);
        *timings = FrameSubmitTimings {
            graph_build: timings.graph_build,
            command_encode,
            source_acquire,
            queue_lock_wait: submitted.timings.queue_lock_wait,
            queue_submit: submitted.timings.queue_submit,
            lease_retire: submitted.lease_retire,
            complete: completing.elapsed(),
        };
        Ok(())
    }

    /// Uploads a CPU picture's planes at their own formats: `R8` luma, and
    /// `RG8` interleaved or two `R8` chroma planes.
    fn upload_repack_planes(
        &self,
        frame: &YuvFrame<'_>,
    ) -> Result<Vec<UploadTexture>, RenderError> {
        let format = frame.format;
        let (chroma_width, chroma_height) = format.chroma_size(frame.size.width, frame.size.height);
        let pairs = if format.interleaved_chroma { 2 } else { 1 };
        let specs = [
            (
                "alpha2 repack Y plane",
                frame.size.width,
                frame.size.height,
                1_u32,
            ),
            ("alpha2 repack U plane", chroma_width, chroma_height, pairs),
            ("alpha2 repack V plane", chroma_width, chroma_height, 1),
        ];
        frame
            .planes
            .iter()
            .zip(specs)
            .take(format.color_plane_count())
            .map(|(plane, (label, width, height, samples))| {
                validate_plane(plane.bytes, plane.stride, width * samples, height)?;
                upload_plane(
                    self,
                    label,
                    *plane,
                    (width, height),
                    if samples == 2 {
                        wgpu::TextureFormat::Rg8Unorm
                    } else {
                        wgpu::TextureFormat::R8Unorm
                    },
                )
            })
            .collect()
    }

    fn yuv_repack_pipeline(&self) -> &CachedYuvRepackPipeline {
        self.pipelines.yuv_repack.get_or_init(|| {
            let texture = |binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let bind_group_layout =
                self.gpu
                    .device()
                    .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("alpha2 YUV repack layout"),
                        entries: &[
                            texture(0),
                            texture(1),
                            texture(2),
                            wgpu::BindGroupLayoutEntry {
                                binding: 3,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Buffer {
                                    ty: wgpu::BufferBindingType::Uniform,
                                    has_dynamic_offset: false,
                                    min_binding_size: None,
                                },
                                count: None,
                            },
                        ],
                    });
            let shader = self
                .gpu
                .device()
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("alpha2 YUV repack shader"),
                    source: wgpu::ShaderSource::Wgsl(YUV_REPACK_WGSL.into()),
                });
            let layout =
                self.gpu
                    .device()
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("alpha2 YUV repack pipeline layout"),
                        bind_group_layouts: &[Some(&bind_group_layout)],
                        immediate_size: 0,
                    });
            let pipeline = |label, entry_point, format: wgpu::TextureFormat| {
                self.gpu
                    .device()
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some(label),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &shader,
                            entry_point: Some("vs"),
                            buffers: &[],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &shader,
                            entry_point: Some(entry_point),
                            targets: &[Some(format.into())],
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                        }),
                        primitive: wgpu::PrimitiveState::default(),
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState::default(),
                        multiview_mask: None,
                        cache: None,
                    })
            };
            CachedYuvRepackPipeline {
                luma: pipeline(
                    "alpha2 YUV repack luma pipeline",
                    "luma_fs",
                    wgpu::TextureFormat::R8Unorm,
                ),
                interleaved_chroma: pipeline(
                    "alpha2 YUV repack interleaved chroma pipeline",
                    "interleaved_chroma_fs",
                    wgpu::TextureFormat::Rg8Unorm,
                ),
                planar_chroma: pipeline(
                    "alpha2 YUV repack planar chroma pipeline",
                    "planar_chroma_fs",
                    wgpu::TextureFormat::Rg8Unorm,
                ),
                bind_group_layout,
            }
        })
    }
}

fn texture_entry(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

/// The same checks the NV12 pack makes of plane targets it may not own.
fn check_targets(targets: Nv12PlaneTargets<'_>, size: RenderSize) -> Result<(), RenderError> {
    let attachment = |texture: &wgpu::Texture| {
        texture
            .usage()
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
            && texture.mip_level_count() == 1
            && texture.sample_count() == 1
            && texture.depth_or_array_layers() == 1
    };
    if targets.luma.format() != wgpu::TextureFormat::R8Unorm
        || targets.chroma.format() != wgpu::TextureFormat::Rg8Unorm
        || targets.luma.width() != size.width
        || targets.luma.height() != size.height
        || targets.chroma.width() != size.width / 2
        || targets.chroma.height() != size.height / 2
        || !attachment(targets.luma)
        || !attachment(targets.chroma)
    {
        return Err(RenderError::InvalidPlaneTarget);
    }
    Ok(())
}
