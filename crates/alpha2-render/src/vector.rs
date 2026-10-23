//! The vector pass: a [`VectorScene`] drawn from tile coverage.
//!
//! Each fill's coverage is computed on the CPU ([`alpha2_geometry::raster`])
//! and kept between frames by its path's key. Its partial tiles go into
//! `R8Unorm` atlas pages; its full and partial tiles are drawn as instanced
//! 16 by 16 quads, each pixel colored by the paint code the generator pass
//! uses and blended over what is drawn. A group layer is drawn into a
//! texture of its own and composited at its opacity.
//!
//! Where half floats blend in hardware and the graph blends in linear light,
//! the scene is drawn in linear light into an `Rgba16Float` picture and then
//! encoded into the intermediate format, as the engine stores pictures:
//! premultiplied, sRGB-encoded. Otherwise it blends in that encoded space
//! directly, as the 8-bit fallback composites.

use std::num::NonZeroU64;
use std::sync::Arc;

use alpha2_geometry::raster::{RasterScratch, TILE, TILE_BYTES, TileCoverage, TileState};
use alpha2_geometry::{IRect, PaintKind, ScenePaint, VectorOp, VectorScene};
use wgpu::util::DeviceExt;

use crate::uniform::UniformArenaLease;
use crate::{RenderError, RenderSize, Renderer};

/// Atlas page width in pixels: 128 tile slots across.
const ATLAS_WIDTH: u32 = 2048;

/// Tile slots in one atlas row of tiles.
const SLOTS_PER_ROW: u32 = ATLAS_WIDTH / 16;

/// The slot a full tile draws with: no atlas read, coverage one.
const FULL_SLOT: u32 = u32::MAX;

/// Bytes of one op's uniform: fifteen `vec4`s (`VectorUniform` below).
pub(crate) const VECTOR_UNIFORM_BYTES: u64 = 15 * 16;

/// Bytes of one tile instance: its tile position (two `u16`) and slot.
const INSTANCE_BYTES: u64 = 8;

const _: () = assert!(TILE == 16, "the shader draws 16-pixel tiles");

/// The vector pass's shader: tile quads painted by coverage, layers
/// composited at an opacity, and linear light encoded for storage.
pub(crate) fn vector_wgsl() -> String {
    format!(
        r"{paint}
struct VectorUniform {{
    // Device pixels to paint-space pixels: rows of a 2x3 matrix.
    inverse0: vec4f,
    inverse1: vec4f,
    // Canvas width and height, opacity, and 1 to store sRGB-encoded.
    canvas: vec4f,
    paint_ab: vec4f,
    // Kind, spread, stop count.
    paint_info: vec4u,
    offsets: array<vec4f, 2>,
    colors: array<vec4f, 8>,
}};

@group(0) @binding(0) var<uniform> op: VectorUniform;
@group(0) @binding(1) var picture: texture_2d<f32>;

struct TileOut {{
    @builtin(position) position: vec4f,
    @location(0) @interpolate(flat) tile: vec2u,
    @location(1) @interpolate(flat) slot: u32,
}};

fn clip_position(pixel: vec2f) -> vec4f {{
    let size = op.canvas.xy;
    return vec4f(pixel.x / size.x * 2.0 - 1.0, 1.0 - pixel.y / size.y * 2.0, 0.0, 1.0);
}}

@vertex
fn vs_tile(@builtin(vertex_index) index: u32, @location(0) tile: vec2u, @location(1) slot: u32) -> TileOut {{
    // Two triangles: (0,0) (1,0) (0,1) and (0,1) (1,0) (1,1).
    var corners = array<vec2f, 6>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
        vec2f(0.0, 1.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0),
    );
    var out: TileOut;
    out.position = clip_position((vec2f(tile) + corners[index]) * 16.0);
    out.tile = tile;
    out.slot = slot;
    return out;
}}

fn builtin_op_paint() -> AlphaPaint {{
    let o0 = op.offsets[0];
    let o1 = op.offsets[1];
    return AlphaPaint(
        op.paint_info.x,
        op.paint_ab.xy,
        op.paint_ab.zw,
        op.paint_info.y,
        op.paint_info.z,
        array<f32, 8>(o0.x, o0.y, o0.z, o0.w, o1.x, o1.y, o1.z, o1.w),
        op.colors,
    );
}}

@fragment
fn fs_tile(in: TileOut) -> @location(0) vec4f {{
    var coverage = 1.0;
    if (in.slot != 0xffffffffu) {{
        let pixel = vec2u(in.position.xy);
        let local = pixel - in.tile * 16u;
        let origin = vec2u(in.slot % {SLOTS_PER_ROW}u, in.slot / {SLOTS_PER_ROW}u) * 16u;
        coverage = textureLoad(picture, origin + local, 0).r;
    }}
    let device = in.position.xy;
    let painted = vec2f(
        dot(op.inverse0.xy, device) + op.inverse0.z,
        dot(op.inverse1.xy, device) + op.inverse1.z,
    );
    let res = op.canvas.xy;
    var color = builtin_paint(builtin_op_paint(), painted / res, res);
    if (op.canvas.w > 0.5) {{ color = builtin_generator_store(color); }}
    return color * (coverage * op.canvas.z);
}}

struct FullOut {{
    @builtin(position) position: vec4f,
}};

@vertex
fn vs_full(@builtin(vertex_index) index: u32) -> FullOut {{
    var corners = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var out: FullOut;
    out.position = vec4f(corners[index], 0.0, 1.0);
    return out;
}}

// A layer composited at the op's opacity.
@fragment
fn fs_layer(in: FullOut) -> @location(0) vec4f {{
    return textureLoad(picture, vec2u(in.position.xy), 0) * op.canvas.z;
}}

// A linear-light picture stored as the engine stores pictures.
@fragment
fn fs_encode(in: FullOut) -> @location(0) vec4f {{
    return builtin_generator_store(textureLoad(picture, vec2u(in.position.xy), 0));
}}
",
        paint = crate::paint_wgsl::paint_wgsl(),
    )
}

/// The pipelines of the vector pass for one target format.
#[derive(Debug)]
pub(crate) struct VectorPipelines {
    tile: wgpu::RenderPipeline,
    layer: wgpu::RenderPipeline,
    encode: wgpu::RenderPipeline,
}

/// The vector pass's shared layout and shader, and its pipelines by
/// target format.
#[derive(Debug)]
pub(crate) struct CachedVectorPipeline {
    pub(crate) layout: wgpu::BindGroupLayout,
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    by_format: std::sync::Mutex<Vec<(wgpu::TextureFormat, crate::Shared<VectorPipelines>)>>,
}

/// Coverage kept between frames, and the buffers rasterizing reuses.
#[derive(Debug, Default)]
pub(crate) struct VectorState {
    pub(crate) cache: alpha2_geometry::raster::CoverageCache,
    pub(crate) scratch: RasterScratch,
}

/// Where a scene's partial tiles go: pages of slots, filled in op order.
struct Atlas {
    /// The bytes of each page, `ATLAS_WIDTH` wide.
    pages: Vec<Vec<u8>>,
    /// Slots in a page.
    page_slots: u32,
    /// Slots used in the last page.
    used: u32,
}

impl Atlas {
    fn new(page_slots: u32) -> Self {
        Self {
            pages: Vec::new(),
            page_slots,
            used: page_slots,
        }
    }

    /// Stores one tile's bytes, returning its page and slot.
    fn push(&mut self, bytes: &[u8; TILE_BYTES]) -> (usize, u32) {
        if self.used == self.page_slots {
            self.pages.push(Vec::new());
            self.used = 0;
        }
        let page = self.pages.len() - 1;
        let slot = self.used;
        self.used += 1;
        let rows = (slot / SLOTS_PER_ROW + 1) as usize * TILE;
        let data = &mut self.pages[page];
        let width = ATLAS_WIDTH as usize;
        if data.len() < rows * width {
            data.resize(rows * width, 0);
        }
        let (x, y) = (
            (slot % SLOTS_PER_ROW) as usize * TILE,
            (slot / SLOTS_PER_ROW) as usize * TILE,
        );
        for row in 0..TILE {
            let start = (y + row) * width + x;
            data[start..start + TILE].copy_from_slice(&bytes[row * TILE..row * TILE + TILE]);
        }
        (page, slot)
    }
}

/// One fill's tiles, drawn page by page.
struct FillDraw {
    /// Instances in the node's instance buffer, by page: `(page, range)`.
    /// A full tile joins the range before it (page 0 when it is first),
    /// whose atlas binding it does not read.
    ranges: Vec<(usize, std::ops::Range<u32>)>,
    uniform: u64,
}

/// What the vector pass draws, op by op.
enum Step {
    Fill(FillDraw),
    Push,
    Pop { bounds: IRect, uniform: u64 },
}

impl Renderer {
    /// Whether this graph's vector scenes are drawn in linear light: it
    /// blends in linear light, and half floats can be rendered to and
    /// blended.
    pub(crate) fn vector_linear(&self, blend_space: crate::BlendSpace) -> bool {
        blend_space == crate::BlendSpace::LinearLight
            && self.intermediate_format() == wgpu::TextureFormat::Rgba16Float
            && self.gpu.capabilities().half_float_blendable
    }

    fn vector_pipeline(&self) -> &CachedVectorPipeline {
        self.pipelines.vector.get_or_init(|| {
            let device = self.gpu.device();
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("alpha2 vector layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: NonZeroU64::new(VECTOR_UNIFORM_BYTES),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            });
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("alpha2 vector shader"),
                source: wgpu::ShaderSource::Wgsl(vector_wgsl().into()),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("alpha2 vector pipeline layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            CachedVectorPipeline {
                layout,
                shader,
                pipeline_layout,
                by_format: std::sync::Mutex::default(),
            }
        })
    }

    /// The vector pipelines drawing into `format`, made the first time.
    fn vector_pipelines(&self, format: wgpu::TextureFormat) -> crate::Shared<VectorPipelines> {
        let cached = self.vector_pipeline();
        let mut by_format = cached
            .by_format
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, pipelines)) = by_format.iter().find(|(held, _)| *held == format) {
            return crate::Shared::clone(pipelines);
        }
        let device = self.gpu.device();
        let make = |label: &'static str,
                    vertex: &'static str,
                    fragment: &'static str,
                    buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
                    blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&cached.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &cached.shader,
                    entry_point: Some(vertex),
                    buffers,
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &cached.shader,
                    entry_point: Some(fragment),
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
        };
        let instances = [Some(wgpu::VertexBufferLayout {
            array_stride: INSTANCE_BYTES,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint16x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32,
                    offset: 4,
                    shader_location: 1,
                },
            ],
        })];
        let over = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let pipelines = crate::Shared::new(VectorPipelines {
            tile: make(
                "alpha2 vector tile pipeline",
                "vs_tile",
                "fs_tile",
                &instances,
                over,
            ),
            layer: make(
                "alpha2 vector layer pipeline",
                "vs_full",
                "fs_layer",
                &[],
                over,
            ),
            encode: make(
                "alpha2 vector encode pipeline",
                "vs_full",
                "fs_encode",
                &[],
                None,
            ),
        });
        by_format.push((format, crate::Shared::clone(&pipelines)));
        pipelines
    }

    /// Initializes the vector pipelines a graph drawing with
    /// `blend_space` needs.
    pub(crate) fn warm_vector(&self, blend_space: crate::BlendSpace) {
        let format = self.intermediate_format();
        self.vector_pipelines(format);
        if self.vector_linear(blend_space) {
            self.vector_pipelines(wgpu::TextureFormat::Rgba16Float);
        }
    }

    /// The coverage of each fill of `scene`, from the cache or rasterized.
    fn scene_coverage(&self, scene: &VectorScene) -> Vec<Option<Arc<TileCoverage>>> {
        let mut state = self
            .vector_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let VectorState { cache, scratch } = &mut *state;
        scene
            .ops
            .iter()
            .map(|op| {
                let VectorOp::Fill {
                    path, rule, bounds, ..
                } = op
                else {
                    return None;
                };
                let key = alpha2_geometry::raster::coverage_key(path, *rule, *bounds);
                if let Some(coverage) = cache.get(key) {
                    return Some(coverage);
                }
                let coverage = Arc::new(alpha2_geometry::raster::rasterize(
                    path, *rule, *bounds, scratch,
                ));
                cache.insert(key, Arc::clone(&coverage));
                Some(coverage)
            })
            .collect()
    }

    /// Encodes `scene` into `output`, a target of the intermediate format
    /// and the graph's size. Returns the render passes encoded.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn encode_vector(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        scene: &VectorScene,
        size: RenderSize,
        blend_space: crate::BlendSpace,
        output: &wgpu::TextureView,
        uniforms: &mut UniformArenaLease,
    ) -> Result<u64, RenderError> {
        let device = self.gpu.device();
        let linear = self.vector_linear(blend_space);
        let format = if linear {
            wgpu::TextureFormat::Rgba16Float
        } else {
            self.intermediate_format()
        };
        if scene.fill_count() == 0 {
            clear(encoder, output);
            return Ok(1);
        }
        let raster_started = self
            .pass_timing_epoch()
            .map(|epoch| (epoch, web_time::Instant::now()));
        let coverage = self.scene_coverage(scene);
        if let Some((epoch, started)) = raster_started {
            crate::diagnostics::record_pass(
                &self.diagnostics,
                epoch,
                "vector raster",
                started.elapsed(),
                crate::TimingSource::CpuEncoding,
            );
        }
        let pipelines = self.vector_pipelines(format);
        let cached = self.vector_pipeline();
        // Pages tall enough for every slot, up to the device's largest texture.
        let max_rows = (self.limits.max_texture_dimension_2d.min(ATLAS_WIDTH) / 16).max(1);
        #[cfg(test)]
        let max_rows = match self
            .vector_page_rows
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => max_rows,
            rows => rows,
        };
        let mut atlas = Atlas::new(max_rows * SLOTS_PER_ROW);
        let mut instances: Vec<u8> = Vec::new();
        let mut steps = Vec::with_capacity(scene.ops.len());
        // Canvas edges are at most 16384, exact in `f32`.
        #[allow(clippy::cast_precision_loss)]
        let canvas = [size.width as f32, size.height as f32];
        // Each open layer's bounds and opacity.
        let mut open: Vec<(IRect, f32)> = Vec::new();
        for (op, coverage) in scene.ops.iter().zip(&coverage) {
            match op {
                VectorOp::PushLayer { bounds, opacity } => {
                    open.push((*bounds, *opacity));
                    steps.push(Step::Push);
                }
                VectorOp::PopLayer => {
                    let (bounds, opacity) = open.pop().ok_or(RenderError::InvalidGraph)?;
                    let uniform = uniforms.write(&op_uniform_plain(canvas, opacity)).offset();
                    steps.push(Step::Pop { bounds, uniform });
                }
                VectorOp::Fill { paint, opacity, .. } => {
                    let coverage = coverage.as_ref().ok_or(RenderError::InvalidGraph)?;
                    let mut ranges: Vec<(usize, std::ops::Range<u32>)> = Vec::new();
                    let (tiles_x, _) = coverage.size;
                    for (index, tile) in coverage.tiles.iter().enumerate() {
                        let index = u32::try_from(index).unwrap_or(u32::MAX);
                        let x = coverage.origin.0 + i32::try_from(index % tiles_x).unwrap_or(0);
                        let y = coverage.origin.1 + i32::try_from(index / tiles_x).unwrap_or(0);
                        match tile {
                            TileState::Empty => {}
                            TileState::Full => {
                                // Full tiles read no atlas: any page's binding serves.
                                let page = ranges.last().map_or(0, |(page, _)| *page);
                                push_instance(&mut ranges, &mut instances, page, (x, y), FULL_SLOT);
                            }
                            TileState::Partial(partial) => {
                                let (page, slot) = atlas.push(&coverage.partial[*partial as usize]);
                                push_instance(&mut ranges, &mut instances, page, (x, y), slot);
                            }
                        }
                    }
                    let uniform = uniforms
                        .write(&op_uniform(paint, canvas, *opacity, !linear))
                        .offset();
                    steps.push(Step::Fill(FillDraw { ranges, uniform }));
                }
            }
        }
        let encode_uniform = uniforms.write(&op_uniform_plain(canvas, 1.0)).offset();

        // Pages to textures, copied in this encoder's order so a page reused later in the
        // frame holds its own tiles when this draws.
        let page_textures: Vec<crate::pool::TextureLease> = atlas
            .pages
            .iter()
            .map(|bytes| {
                let rows = u32::try_from(bytes.len() / ATLAS_WIDTH as usize)
                    .unwrap_or(16)
                    .max(16);
                // Heights rounded to a power of two, so pages are pooled across frames.
                let height = rows.next_power_of_two().min(max_rows * 16).max(rows);
                let texture = self.textures.acquire(
                    device,
                    RenderSize::new(ATLAS_WIDTH, height).expect("a non-empty atlas page"),
                    wgpu::TextureFormat::R8Unorm,
                    "alpha2 vector atlas page",
                );
                let staging = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("alpha2 vector atlas upload"),
                    contents: bytes,
                    usage: wgpu::BufferUsages::COPY_SRC,
                });
                encoder.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo {
                        buffer: &staging,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(ATLAS_WIDTH),
                            rows_per_image: Some(rows),
                        },
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: texture.texture(),
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: ATLAS_WIDTH,
                        height: rows,
                        depth_or_array_layers: 1,
                    },
                );
                texture
            })
            .collect();
        let instance_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("alpha2 vector tiles"),
            contents: if instances.is_empty() {
                &[0; 8]
            } else {
                &instances
            },
            usage: wgpu::BufferUsages::VERTEX,
        });
        let binding =
            uniforms.dynamic_binding(NonZeroU64::new(VECTOR_UNIFORM_BYTES).expect("non-zero"));
        let group = |view: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 vector group"),
                layout: &cached.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: binding.clone(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                ],
            })
        };
        let page_groups: Vec<wgpu::BindGroup> = if page_textures.is_empty() {
            vec![group(&self.empty_input)]
        } else {
            page_textures
                .iter()
                .map(|page| group(page.view()))
                .collect()
        };

        // The scene's picture: in linear light to encode, or the output itself.
        let linear_target = linear.then(|| {
            self.textures.acquire(
                device,
                size,
                wgpu::TextureFormat::Rgba16Float,
                "alpha2 vector linear target",
            )
        });
        let base: &wgpu::TextureView = linear_target
            .as_ref()
            .map_or(output, |target| target.view());
        let mut passes = 0_u64;
        // The stack of targets: the base, then each open layer's texture.
        let mut layers: Vec<crate::pool::TextureLease> = Vec::new();
        let mut cleared = vec![false];
        let mut index = 0;
        while index < steps.len() {
            let target = layers.last().map_or(base, crate::pool::TextureLease::view);
            match &steps[index] {
                Step::Push => {
                    layers.push(
                        self.textures
                            .acquire(device, size, format, "alpha2 vector layer"),
                    );
                    cleared.push(false);
                    index += 1;
                }
                Step::Pop { bounds, uniform } => {
                    let layer = layers.pop().ok_or(RenderError::InvalidGraph)?;
                    let layer_cleared = cleared.pop().unwrap_or(true);
                    let parent = layers.last().map_or(base, crate::pool::TextureLease::view);
                    let parent_cleared = cleared.last_mut().expect("the base is on the stack");
                    if layer_cleared {
                        let layer_group = group(layer.view());
                        let mut pass = begin(encoder, parent, !*parent_cleared);
                        *parent_cleared = true;
                        passes += 1;
                        if let Some((x, y, w, h)) = scissor(*bounds, size) {
                            pass.set_scissor_rect(x, y, w, h);
                            pass.set_pipeline(&pipelines.layer);
                            pass.set_bind_group(0, &layer_group, &[dynamic_offset(*uniform)]);
                            pass.draw(0..3, 0..1);
                        }
                    }
                    index += 1;
                }
                Step::Fill(_) => {
                    let clear_first = !*cleared.last().expect("a target");
                    *cleared.last_mut().expect("a target") = true;
                    let mut pass = begin(encoder, target, clear_first);
                    passes += 1;
                    pass.set_pipeline(&pipelines.tile);
                    pass.set_vertex_buffer(0, instance_buffer.slice(..));
                    while let Some(Step::Fill(fill)) = steps.get(index) {
                        for (page, range) in &fill.ranges {
                            pass.set_bind_group(
                                0,
                                &page_groups[(*page).min(page_groups.len() - 1)],
                                &[dynamic_offset(fill.uniform)],
                            );
                            pass.draw(0..6, range.clone());
                        }
                        index += 1;
                    }
                }
            }
        }
        if !cleared[0] {
            clear(encoder, base);
            passes += 1;
        }
        if let Some(linear_target) = &linear_target {
            let encode_group = group(linear_target.view());
            let mut pass = begin(encoder, output, true);
            passes += 1;
            if let Some((x, y, w, h)) = scissor(scene.bounds, size) {
                pass.set_scissor_rect(x, y, w, h);
                pass.set_pipeline(&pipelines.encode);
                pass.set_bind_group(0, &encode_group, &[dynamic_offset(encode_uniform)]);
                pass.draw(0..3, 0..1);
            }
        }
        Ok(passes)
    }
}

/// Appends a tile instance, extending the last page's range of `ranges`
/// or starting one for `page`.
fn push_instance(
    ranges: &mut Vec<(usize, std::ops::Range<u32>)>,
    instances: &mut Vec<u8>,
    page: usize,
    (x, y): (i32, i32),
    slot: u32,
) {
    let index = u32::try_from(instances.len() as u64 / INSTANCE_BYTES).unwrap_or(u32::MAX);
    match ranges.last_mut() {
        Some((held, range)) if *held == page && range.end == index => range.end += 1,
        _ => ranges.push((page, index..index + 1)),
    }
    // Tile positions are within a canvas of at most 16384 pixels.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (tx, ty) = (x as u16, y as u16);
    instances.extend_from_slice(&tx.to_le_bytes());
    instances.extend_from_slice(&ty.to_le_bytes());
    instances.extend_from_slice(&slot.to_le_bytes());
}

/// A dynamic offset: the arena's offsets are within its `u32`-sized
/// capacity.
fn dynamic_offset(offset: u64) -> u32 {
    u32::try_from(offset).expect("a uniform arena is smaller than 4 GiB")
}

fn clear(encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
    let _pass = begin(encoder, target, true);
}

/// A render pass on `target`, cleared to transparent first or loading it.
fn begin<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    clear: bool,
) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("alpha2 vector pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: if clear {
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
    })
}

/// `bounds` within a target of `size` as a scissor rectangle, or `None`
/// when nothing of it is inside.
fn scissor(bounds: IRect, size: RenderSize) -> Option<(u32, u32, u32, u32)> {
    let canvas = IRect::new(
        0,
        0,
        i32::try_from(size.width).unwrap_or(i32::MAX),
        i32::try_from(size.height).unwrap_or(i32::MAX),
    );
    let inside = bounds.intersect(canvas);
    if inside.is_empty() {
        return None;
    }
    Some((
        u32::try_from(inside.x0).unwrap_or(0),
        u32::try_from(inside.y0).unwrap_or(0),
        inside.width(),
        inside.height(),
    ))
}

/// A fill op's uniform: its paint through the inverse transform, the
/// canvas, its opacity, and whether it stores sRGB-encoded.
fn op_uniform(paint: &ScenePaint, canvas: [f32; 2], opacity: f32, encoded: bool) -> Vec<u8> {
    let [xx, yx, xy, yy, x0, y0] = paint.inverse.as_coeffs();
    #[allow(clippy::cast_possible_truncation)]
    let narrow = |value: f64| value as f32;
    let mut floats = [0.0_f32; 60];
    // Paint-space x = xx * x + xy * y + x0; y = yx * x + yy * y + y0.
    floats[0..4].copy_from_slice(&[narrow(xx), narrow(xy), narrow(x0), 0.0]);
    floats[4..8].copy_from_slice(&[narrow(yx), narrow(yy), narrow(y0), 0.0]);
    floats[8..12].copy_from_slice(&[
        canvas[0],
        canvas[1],
        opacity,
        if encoded { 1.0 } else { 0.0 },
    ]);
    floats[12..16].copy_from_slice(&[paint.a[0], paint.a[1], paint.b[0], paint.b[1]]);
    let kind: u32 = match paint.kind {
        PaintKind::Solid => 1,
        PaintKind::Linear => 2,
        PaintKind::Radial => 3,
        PaintKind::Conic => 4,
    };
    let count = u32::try_from(paint.stops.len().min(crate::MAX_GENERATOR_STOPS)).unwrap_or(0);
    for (index, stop) in paint
        .stops
        .iter()
        .take(crate::MAX_GENERATOR_STOPS)
        .enumerate()
    {
        floats[20 + index] = stop.offset;
        floats[28 + index * 4..32 + index * 4].copy_from_slice(&stop.color);
    }
    let mut bytes = Vec::with_capacity(floats.len() * 4);
    for (index, value) in floats.iter().enumerate() {
        // The fifth `vec4` holds integers.
        let word = match index {
            16 => kind,
            17 => paint.spread,
            18 => count,
            19 => 0,
            _ => value.to_bits(),
        };
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    bytes
}

/// A layer or encode pass's uniform: the canvas and an opacity.
fn op_uniform_plain(canvas: [f32; 2], opacity: f32) -> Vec<u8> {
    let mut floats = [0.0_f32; 60];
    floats[8..12].copy_from_slice(&[canvas[0], canvas[1], opacity, 0.0]);
    floats
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

impl CachedVectorPipeline {
    /// Pipelines made so far, for the cache's counts.
    pub(crate) fn pipeline_count(&self) -> usize {
        self.by_format
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}
