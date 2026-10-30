//! Copy-initialized textures read back exactly what was copied, even where the copy is all zero and
//! the memory under the new texture held other pixels just before.
//!
//! A driver that skips all-zero blocks of the copy into memory created without zeroing (Iris Xe
//! over DX12 with committed resources) shows stale texels in exactly these blocks.

use super::*;

const ROUNDS: u32 = 48;
const BLOCK: u32 = 8;

fn round_size(round: u32) -> RenderSize {
    RenderSize::new(64 + 4 * (round % 12), 32 + 4 * (round % 5)).expect("non-zero size")
}

/// Leaves non-zero bytes in freed memory: a render target of `size` cleared to opaque white,
/// submitted and dropped before the next allocation.
fn dirty_memory(renderer: &Renderer, size: RenderSize) {
    let device = renderer.gpu.device();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("alpha2 test dirty memory"),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("alpha2 test dirty clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    }));
    renderer.gpu.submit([encoder.finish()]);
    drop(view);
    drop(texture);
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("poll");
}

/// Opaque, non-zero premultiplied RGBA from a seed.
fn seeded(seed: u32) -> [u8; 4] {
    let mixed = seed.wrapping_mul(0x9e37_79b9) ^ (seed >> 7);
    let channel = |shift: u32| 32 + u8::try_from((mixed >> shift) & 0xbf).expect("masked");
    [channel(0), channel(8), channel(16), 255]
}

fn is_zero_block(x: u32, y: u32) -> bool {
    (x / BLOCK + y / BLOCK).is_multiple_of(2)
}

/// 8x8 blocks alternating all-zero and a seeded pattern.
fn checker_pixels(size: RenderSize, round: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size.width * size.height * 4) as usize);
    for y in 0..size.height {
        for x in 0..size.width {
            if is_zero_block(x, y) {
                pixels.extend_from_slice(&[0; 4]);
            } else {
                pixels.extend_from_slice(&seeded(round * 65_537 + y * size.width + x));
            }
        }
    }
    pixels
}

/// Copies an RGBA8 texture that has no `COPY_SRC` usage into one that has, texel for texel, by
/// `textureLoad` in a render pass, then reads it back without row padding.
fn read_texture_exactly(
    renderer: &Renderer,
    view: &wgpu::TextureView,
    size: RenderSize,
) -> Vec<u8> {
    let device = renderer.gpu.device();
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("alpha2 test texel copy"),
        source: wgpu::ShaderSource::Wgsl(
            "@group(0) @binding(0) var source: texture_2d<f32>;
             @vertex fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
                 let uv = vec2f(f32((index << 1u) & 2u), f32(index & 2u));
                 return vec4f(uv * 2.0 - 1.0, 0.0, 1.0);
             }
             @fragment fn fs(@builtin(position) position: vec4f) -> @location(0) vec4f {
                 return textureLoad(source, vec2u(position.xy), 0);
             }"
            .into(),
        ),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("alpha2 test texel copy"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("alpha2 test texel copy"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(view),
        }],
    });
    let copy = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("alpha2 test texel copy target"),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let copy_view = copy.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("alpha2 test texel copy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &copy_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    renderer.gpu.submit([encoder.finish()]);
    read_copy_source(renderer, &copy, size)
}

/// Reads an RGBA8 texture with `COPY_SRC` usage as tightly packed rows.
fn read_copy_source(renderer: &Renderer, texture: &wgpu::Texture, size: RenderSize) -> Vec<u8> {
    let device = renderer.gpu.device();
    let row = size.width * 4;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("alpha2 test exact readback"),
        size: u64::from(padded * size.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(size.height),
            },
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    renderer.gpu.submit([encoder.finish()]);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, |result| result.expect("map"));
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("poll");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    mapped
        .chunks(padded as usize)
        .flat_map(|line| line[..row as usize].iter().copied())
        .collect()
}

/// Asserts each texel of `actual` against `expected`: exactly zero in zero blocks, within
/// `tolerance` codes elsewhere. The message names the round, block and first wrong value.
fn assert_blocks(
    label: &str,
    round: u32,
    size: RenderSize,
    actual: &[u8],
    expected: &[u8],
    tolerance: u8,
) {
    for (index, (got, want)) in actual.chunks(4).zip(expected.chunks(4)).enumerate() {
        let index = u32::try_from(index).expect("small image");
        let (x, y) = (index % size.width, index / size.width);
        let zero = is_zero_block(x, y);
        let wrong = if zero {
            got != [0; 4]
        } else {
            got.iter()
                .zip(want)
                .any(|(g, w)| g.abs_diff(*w) > tolerance)
        };
        assert!(
            !wrong,
            "{label}: round {round} ({}x{}), {} block at ({}, {}), texel ({x}, {y}) read {got:?}, \
             expected {want:?}",
            size.width,
            size.height,
            if zero { "zero" } else { "seeded" },
            x / BLOCK * BLOCK,
            y / BLOCK * BLOCK,
        );
    }
}

#[test]
fn uploads_with_zero_blocks_over_recycled_memory_read_back_exactly() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    for round in 0..ROUNDS {
        let size = round_size(round);
        dirty_memory(&renderer, size);
        let pixels = checker_pixels(size, round);
        let upload = renderer.upload_rgba8(size, &pixels).expect("upload");

        let exact = read_texture_exactly(&renderer, upload.view(), size);
        assert_blocks("uploaded texels", round, size, &exact, &pixels, 0);

        let view = SourceView::from_wgpu(upload.view());
        let mut graph = RenderGraph::new(size);
        let source = graph.add_source(view);
        let output = graph
            .add_composite(vec![GraphLayer {
                source,
                visual: visual([1.0, 1.0], 1.0),
                blend_mode: BlendMode::Normal,
            }])
            .expect("dependency exists");
        graph.set_output(output).expect("output exists");
        let composed = renderer.execute_graph(&graph).expect("graph executes");
        let composed = read_rgba8(&renderer, &composed);
        assert_blocks("1:1 composite", round, size, &composed, &pixels, 1);
    }
}

#[test]
fn copy_buffer_initialized_textures_with_empty_rows_read_back_empty() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let device = renderer.gpu.device();
    for round in 0..ROUNDS {
        let size = round_size(round);
        dirty_memory(&renderer, size);
        let row = size.width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        // Rows of zeros alternate with seeded rows, as on a vector atlas page whose empty
        // tiles are uploaded together with the drawn ones.
        let mut expected = Vec::with_capacity((row * size.height) as usize);
        let mut staging = vec![0_u8; (padded * size.height) as usize];
        for y in 0..size.height {
            for x in 0..size.width {
                let texel = if y.is_multiple_of(2) {
                    [0; 4]
                } else {
                    seeded(round * 65_537 + y * size.width + x)
                };
                let at = (y * padded + x * 4) as usize;
                staging[at..at + 4].copy_from_slice(&texel);
                expected.extend_from_slice(&texel);
            }
        }
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("alpha2 test row staging"),
            size: staging.len() as u64,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        renderer.gpu.queue().write_buffer(&buffer, 0, &staging);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("alpha2 test copy-initialized page"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(size.height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );
        renderer.gpu.submit([encoder.finish()]);
        let actual = read_copy_source(&renderer, &texture, size);
        for (y, (got, want)) in actual
            .chunks(row as usize)
            .zip(expected.chunks(row as usize))
            .enumerate()
        {
            assert!(
                got == want,
                "round {round} ({}x{}): row {y} ({}) differs first at byte {}",
                size.width,
                size.height,
                if y.is_multiple_of(2) {
                    "zero"
                } else {
                    "seeded"
                },
                got.iter().zip(want).position(|(g, w)| g != w).unwrap_or(0),
            );
        }
    }
}
