//! What the shared test device computes less precisely than the CPU renderer, measured once per
//! process: the CPU-parity classes widen by these numbers, not by adapter or backend names.
//!
//! Measured (release, DXC on DX12) on discrete and integrated GPUs from NVIDIA, AMD and Intel
//! over Vulkan and DX12, an Apple-silicon GPU over Metal, WARP, lavapipe and llvmpipe: no
//! device's `pow` is more than 0.0001 of a code from `f64`. What sets devices apart is how they
//! filter 8-bit pictures: discrete and integrated desktop GPUs with 7 to 9 bits of weight (NVIDIA
//! 8, the AMD and Intel integrated GPUs 9, WARP 7), Apple-silicon GPUs at half precision (about 5
//! bits, 1/32 of a code off), and the software rasterizers rounding the filtered value to 8 bits
//! (1 bit of weight).
//!
//! Each probe draws a full-screen triangle into a one-row target whose texel `x` takes input
//! `x` from a uniform, so no value is a constant the shader compiler could fold, and reads the
//! target back as written. Measured values leave the shader as the bits of their `f32`s in an
//! `Rgba32Uint` target, which every backend renders to (GL does not render to `Rgba32Float`).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::sync::OnceLock;

use alpha2_gpu::GpuContext;

use super::adapter_gpu;

/// Texels in each one-row probe target; every texel carries four values.
pub(super) const TEXELS: u32 = 256;
/// Values per draw: one per channel of every texel.
pub(super) const VALUES: usize = TEXELS as usize * 4;

const SHADER: &str = "
@group(0) @binding(0) var<uniform> values: array<vec4u, 256>;
@group(0) @binding(1) var ramp: texture_2d<f32>;
@group(0) @binding(2) var ramp_sampler: sampler;

@vertex fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    return vec4f(x, y, 0.0, 1.0);
}

fn input(position: vec4f) -> vec4f {
    return bitcast<vec4f>(values[u32(position.x)]);
}

// The bits of the input, untouched: what differs is the device's conversion into the target.
@fragment fn store(@builtin(position) position: vec4f) -> @location(0) vec4f {
    return input(position);
}

// The effects' sRGB curves (runtime/effects/helpers.wgsl), on their `pow` branches.
@fragment fn curves(@builtin(position) position: vec4f) -> @location(0) vec4u {
    let value = input(position);
    let decoded = pow((value.x + 0.055) / 1.055, 2.4);
    let encoded = 1.055 * pow(value.y, 1.0 / 2.4) - 0.055;
    return bitcast<vec4u>(vec4f(decoded, encoded, 0.0, 1.0));
}

// An 8-bit ramp one code high (100, then 101) sampled with the linear filter at position
// `value.x` between the texel centres: an exact filter returns 100 codes and the position.
@fragment fn filtered(@builtin(position) position: vec4f) -> @location(0) vec4u {
    let between = input(position).x;
    let uv = vec2f((0.5 + between) / 2.0, 0.5);
    return bitcast<vec4u>(textureSampleLevel(ramp, ramp_sampler, uv, 0.0));
}
";

/// One-row draws of [`SHADER`]'s entry points on a device.
pub(super) struct Probe {
    gpu: GpuContext,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    ramp: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

/// A 2x1 `Rgba8Unorm` texture holding `texels`.
fn ramp_texture(gpu: &GpuContext, texels: [u8; 8]) -> wgpu::Texture {
    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("precision probe ramp"),
        size: wgpu::Extent3d {
            width: 2,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue().write_texture(
        texture.as_image_copy(),
        &texels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(1),
        },
        texture.size(),
    );
    texture
}

impl Probe {
    pub(super) fn new(gpu: GpuContext) -> Self {
        let device = gpu.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("precision probe"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("precision probe"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("precision probe"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let ramp = ramp_texture(&gpu, [100, 100, 100, 255, 101, 101, 101, 255]);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("precision probe"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        Self {
            ramp: ramp.create_view(&wgpu::TextureViewDescriptor::default()),
            sampler,
            gpu,
            layout,
            pipeline_layout,
            module,
        }
    }

    fn pipeline(
        &self,
        entry: &str,
        format: wgpu::TextureFormat,
        blend: Option<wgpu::BlendState>,
    ) -> wgpu::RenderPipeline {
        self.gpu
            .device()
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("precision probe"),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &self.module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
    }

    /// The inputs of one draw as a bind group (with the buffer, which must outlive the draw).
    fn inputs(&self, bits: &[u32]) -> (wgpu::Buffer, wgpu::BindGroup) {
        assert_eq!(bits.len(), VALUES);
        let device = self.gpu.device();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("precision probe inputs"),
            size: (VALUES * 4) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bytes = bits
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<u8>>();
        self.gpu.queue().write_buffer(&buffer, 0, &bytes);
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("precision probe inputs"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.ramp),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        (buffer, group)
    }

    /// Draws entry point `entry` with `inputs` into a fresh one-row `format` target and returns
    /// the target's bytes. With `under`, the target first takes `under` through the `store`
    /// entry point, and `entry` is then drawn onto it with `blend`.
    pub(super) fn draw(
        &self,
        entry: &str,
        format: wgpu::TextureFormat,
        inputs: &[u32],
        under: Option<(&[u32], wgpu::BlendState)>,
    ) -> Vec<u8> {
        let device = self.gpu.device();
        let texel_bytes = format.block_copy_size(None).expect("a color format");
        let row = TEXELS * texel_bytes;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("precision probe target"),
            size: wgpu::Extent3d {
                width: TEXELS,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut draws = Vec::new();
        if let Some((under, _)) = under {
            draws.push((self.pipeline("store", format, None), self.inputs(under)));
        }
        let blend = under.map(|(_, blend)| blend);
        draws.push((self.pipeline(entry, format, blend), self.inputs(inputs)));
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        for (index, (pipeline, (_, group))) in draws.iter().enumerate() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("precision probe"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if index == 0 {
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, group, &[]);
            pass.draw(0..3, 0..1);
        }
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("precision probe readback"),
            size: u64::from(row),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(1),
                },
            },
            texture.size(),
        );
        self.gpu.submit([encoder.finish()]);
        self.gpu
            .map_async(&buffer.slice(..), wgpu::MapMode::Read, |result| {
                result.expect("probe readback maps");
            });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("probe readback");
        buffer
            .slice(..)
            .get_mapped_range()
            .expect("probe readback mapped")
            .to_vec()
    }
}

/// What the parity classes widen by.
#[derive(Copy, Clone, Debug)]
pub(crate) struct DevicePrecision {
    /// Bits of the linear filter's weight on an 8-bit picture: `floor(-log2(e))` for the
    /// largest error `e`, in codes, of a ramp one code high sampled at 256 positions between
    /// its two texels. 24 when every sample is exact in `f32`.
    pub(crate) filter_weight_bits: u32,
    /// The largest error, in 8-bit codes, of the effects' sRGB decode and encode `pow` calls
    /// over 256 inputs each, against the same curves in `f64`.
    pub(crate) pow_error_codes: f64,
}

/// The shared test device's [`DevicePrecision`], measured on first use.
pub(crate) fn device_precision() -> DevicePrecision {
    static PRECISION: OnceLock<DevicePrecision> = OnceLock::new();
    *PRECISION.get_or_init(|| measure(&Probe::new(adapter_gpu())))
}

fn measure(probe: &Probe) -> DevicePrecision {
    let format = wgpu::TextureFormat::Rgba32Uint;
    // Positions off the 1/256 grid a device's fixed-point weight may hold exactly.
    let positions = (0..TEXELS)
        .map(|index| (index as f32 + 0.37) / TEXELS as f32)
        .collect::<Vec<_>>();
    let inputs = positions
        .iter()
        .flat_map(|position| [position.to_bits(), 0, 0, 0])
        .collect::<Vec<_>>();
    let sampled = floats(&probe.draw("filtered", format, &inputs, None));
    let weight_error = positions
        .iter()
        .zip(sampled.as_chunks::<4>().0)
        .map(|(position, texel)| {
            (f64::from(texel[0]) * 255.0 - (100.0 + f64::from(*position))).abs()
        })
        .fold(0.0_f64, f64::max);
    let filter_weight_bits = if weight_error == 0.0 {
        24
    } else {
        (-libm::log2(weight_error)).floor().clamp(0.0, 24.0) as u32
    };

    // Encoded values above the decode's linear segment, and linear values above the encode's,
    // spaced evenly in their logarithm so the near-black end, where `pow` is least precise in
    // codes, is well sampled.
    let curve_inputs = (0..TEXELS)
        .map(|index| {
            let step = (f64::from(index) + 0.5) / f64::from(TEXELS);
            let encoded = 0.040_45 + (1.0 - 0.040_45) * step;
            let linear = 0.003_130_8 * libm::pow(1.0 / 0.003_130_8, step);
            (encoded as f32, linear as f32)
        })
        .collect::<Vec<_>>();
    let inputs = curve_inputs
        .iter()
        .flat_map(|(encoded, linear)| [encoded.to_bits(), linear.to_bits(), 0, 0])
        .collect::<Vec<_>>();
    let curves = floats(&probe.draw("curves", format, &inputs, None));
    let pow_error = curve_inputs
        .iter()
        .zip(curves.as_chunks::<4>().0)
        .map(|((encoded, linear), texel)| {
            let decoded = libm::pow((f64::from(*encoded) + 0.055) / 1.055, 2.4);
            let reencoded = 1.055 * libm::pow(f64::from(*linear), 1.0 / 2.4) - 0.055;
            (f64::from(texel[0]) - decoded)
                .abs()
                .max((f64::from(texel[1]) - reencoded).abs())
        })
        .fold(0.0_f64, f64::max);
    DevicePrecision {
        filter_weight_bits,
        pow_error_codes: pow_error * 255.0,
    }
}

fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| f32::from_le_bytes(*word))
        .collect()
}
