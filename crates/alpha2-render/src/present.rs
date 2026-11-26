//! Presenting rendered frames on a window's surface.

/// The pipeline that copies a rendered texture onto a presentation surface (a canvas, a
/// `CAMetalLayer`, an Android window), stretched to the surface's size.
pub struct SurfacePresenter {
    bind_groups: crate::bind_group::BindGroups,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: crate::bind_group::LinearSampler,
}

impl SurfacePresenter {
    /// A presenter drawing into surfaces of `format`.
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let bind_groups = crate::bind_group::BindGroups::default();
        let sampler = crate::bind_group::LinearSampler::new(device, &bind_groups);
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("alpha2 surface presenter layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("alpha2 surface presenter shader"),
            source: wgpu::ShaderSource::Wgsl(alpha2_shaders::CANVAS_PRESENT_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("alpha2 surface presenter pipeline layout"),
            bind_group_layouts: &[Some(&layout), Some(&sampler.layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("alpha2 surface presenter pipeline"),
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
        Self {
            bind_groups,
            layout,
            pipeline,
            sampler,
        }
    }

    /// Draws `source` over all of `target` and submits the work.
    pub fn draw(
        &self,
        device: &wgpu::Device,
        gpu: &alpha2_gpu::GpuContext,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
    ) {
        let bind_group = self.bind_groups.create(
            device,
            &wgpu::BindGroupDescriptor {
                label: Some("alpha2 surface presenter bind group"),
                layout: &self.layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                }],
            },
            0,
        );
        // Submitted before returning, reading only the caller's view: no pooled texture can be
        // let go of while it is unsubmitted.
        #[allow(clippy::disallowed_methods)]
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("alpha2 surface presenter encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("alpha2 surface presenter pass"),
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
            self.sampler.set(&mut pass);
            pass.draw(0..3, 0..1);
        }
        gpu.submit([encoder.finish()]);
    }
}
