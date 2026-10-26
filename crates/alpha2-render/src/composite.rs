use crate::{
    BlendMode, Color, GraphError, GraphLayer, RenderError, RenderGraph, RenderSize,
    RenderedTexture, Renderer, SourceView,
    pipeline::CachedCompositePipeline,
    uniform::{BATCH_UNIFORM_FLOATS, UniformArenaLease, f32_bytes, float_uniform_bytes},
};
use alpha2_shaders::COMPOSITE_WGSL;

/// The most layers one composite pass takes (`BATCH_LAYERS` in
/// composite.wesl). Each has a texture binding of its own, and with the
/// backdrop's that stays inside the 16 sampled textures a WebGL2-class
/// device offers a shader stage.
pub(crate) const COMPOSITE_BATCH_LAYERS: usize = 8;

/// Placement and opacity of one composited layer.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LayerTransform {
    /// Normalized output translation, as `[x, y]`.
    pub position: [f32; 2],
    /// Normalized source anchor, as `[x, y]`.
    pub anchor: [f32; 2],
    /// Per-axis scale, as `[x, y]`.
    pub scale: [f32; 2],
    /// Clockwise rotation in degrees.
    pub rotation_degrees: f32,
    /// Opacity from zero to one.
    pub opacity: f32,
}

/// One decoded, premultiplied sRGB layer and its presentation controls.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct CompositeLayer<'a> {
    /// Source texture sampled by this layer.
    pub texture: &'a wgpu::TextureView,
    /// Layer transform and opacity.
    pub visual: LayerTransform,
    /// Artistic blend operation evaluated before source-over coverage.
    pub blend_mode: BlendMode,
}

/// Where a composite pass draws and how it blends.
#[derive(Clone, Copy)]
pub(crate) struct CompositeTarget<'a> {
    /// What the first layer composites over; transparent when `None`.
    pub(crate) background: Option<Color>,
    /// The space blend modes are evaluated in.
    pub(crate) blend_space: crate::BlendSpace,
    /// The texture the pass writes.
    pub(crate) output: &'a wgpu::Texture,
    /// The default view of `output`.
    pub(crate) output_view: &'a wgpu::TextureView,
}

impl Renderer {
    /// Composites layers in slice order onto a transparent RGBA target.
    ///
    /// Inputs and output use premultiplied, sRGB-encoded RGBA storage. Blend
    /// arithmetic uses the renderer's configured [`crate::BlendSpace`].
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidVisual`] for non-finite or out-of-range
    /// layer presentation values. A zero-scale layer is valid and draws
    /// nothing.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn render_layers(
        &self,
        size: RenderSize,
        layers: &[CompositeLayer<'_>],
    ) -> Result<RenderedTexture, RenderError> {
        let mut graph = RenderGraph::new(size);
        let mut graph_layers = Vec::with_capacity(layers.len());
        for layer in layers {
            graph_layers.push(GraphLayer {
                source: graph.add_source(SourceView::from_wgpu(layer.texture)),
                visual: layer.visual,
                blend_mode: layer.blend_mode,
            });
        }
        let output = graph
            .add_composite(graph_layers)
            .map_err(GraphError::into_render_error)?;
        graph
            .set_output(output)
            .map_err(GraphError::into_render_error)?;
        self.execute_graph(&graph)
            .map_err(GraphError::into_render_error)
    }

    /// Composites `layers` over `background` (transparent when `None`) into
    /// `output`, returning the render passes encoded.
    ///
    /// Layers whose pixels cannot reach the target, or that have no opacity,
    /// are skipped. Consecutive small layers composite in batches of up to
    /// [`Self::composite_batch_layers`], one pass per batch (see
    /// [`composite_batches`]). Which pipeline draws a pass follows from its
    /// layers' [`LayerTier`] alone (see [`Self::composite_program`]): every
    /// small layer, alone or in a batch, draws with the loop pipeline, the
    /// batch's layer count a uniform, so a small layer drawn alone runs the
    /// same compiled code as a batch: each pixel runs the batch's layers in
    /// order, each result as the intermediate texture holds it, and the result
    /// is what a pass per layer stores. A large layer, which never shares a
    /// pass, draws with the one-layer pipeline. The first batch composites
    /// over the background in a pass that clears the target to it as stored
    /// (what the shader writes where no layer reaches), drawing only inside
    /// its layers' bounds when they cover less than half the target. A
    /// later batch whose layers cover half the target or more draws the whole
    /// frame into the other of two textures, over the one holding the
    /// composite so far; a smaller one draws in place, only inside its
    /// layers' bounds, over a copy of the region it covers.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn encode_layers_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        size: RenderSize,
        layers: &[CompositeLayer<'_>],
        target: CompositeTarget<'_>,
        uniforms: &mut UniformArenaLease,
    ) -> Result<u64, RenderError> {
        let CompositeTarget {
            background,
            blend_space,
            output,
            output_view,
        } = target;
        for layer in layers {
            validate_visual(layer.visual)?;
        }
        let format = self.intermediate_format();
        let drawn = self.drawn_layers(layers.iter().map(|layer| layer.visual), size);
        let drawn = drawn
            .iter()
            .map(|(index, bounds, tier)| (&layers[*index], *bounds, *tier))
            .collect::<Vec<_>>();
        let background = stored_background(background, format);
        if drawn.is_empty() {
            // The value a first layer writes where it does not reach, so a canvas with
            // nothing on it matches the canvas beside a layer.
            clear_target(encoder, output_view, background);
            return Ok(1);
        }

        let cached = self.composite_pipeline();
        let preparation = BatchPreparationContext {
            device: self.gpu.device(),
            layout: &cached.bind_group_layout,
            sampler: &cached.sampler,
            no_backdrop: &self.empty_input,
            blend_space,
            max_filter_samples: self.quality_controls().max_filter_samples,
            output_size: size,
            background,
        };
        let batches = composite_batches(&drawn, self.composite_batch_layers())
            .into_iter()
            .map(|(range, bounds)| (&drawn[range], bounds))
            .collect::<Vec<_>>();
        let bounded = |bounds: &PixelRect| under_half(*bounds, size);
        let ((first, first_bounds), rest) = batches.split_first().expect("a layer is drawn");
        let first_program = self.batch_program(first);
        let first = prepare_batch(&preparation, None, first, uniforms);
        // The first pass clears to the stored background, which the shader writes wherever layers
        // do not reach, so a first batch under half the target draws only inside its bounds.
        let first_pass = PassTarget {
            load: wgpu::LoadOp::Clear(stored_color(background)),
            scissor: bounded(first_bounds).then_some(*first_bounds),
        };
        if rest.is_empty() {
            self.draw_batch(
                encoder,
                cached,
                first_program,
                &first,
                output_view,
                first_pass,
            );
            return Ok(1);
        }

        let scratch = self.textures.acquire(
            self.gpu.device(),
            size,
            format,
            "alpha2 layer composition scratch",
        );
        let textures = [output, scratch.texture()];
        let views = [output_view, scratch.view()];
        // Each full-frame batch flips the target, so start where the last lands in `output`.
        let mut current = rest.iter().filter(|(_, bounds)| !bounded(bounds)).count() % 2;
        self.draw_batch(
            encoder,
            cached,
            first_program,
            &first,
            views[current],
            first_pass,
        );
        for (batch, bounds) in rest {
            let other = 1 - current;
            let program = self.batch_program(batch);
            if bounded(bounds) {
                // What the batch composites over, set aside where it draws: a pass
                // cannot read the texture it writes.
                copy_region(encoder, textures[current], textures[other], *bounds);
                let prepared = prepare_batch(&preparation, Some(views[other]), batch, uniforms);
                self.draw_batch(
                    encoder,
                    cached,
                    program,
                    &prepared,
                    views[current],
                    PassTarget {
                        load: wgpu::LoadOp::Load,
                        scissor: Some(*bounds),
                    },
                );
            } else {
                let prepared = prepare_batch(&preparation, Some(views[current]), batch, uniforms);
                self.draw_batch(
                    encoder,
                    cached,
                    program,
                    &prepared,
                    views[other],
                    PassTarget::WHOLE,
                );
                current = other;
            }
        }
        debug_assert_eq!(current, 0);
        Ok(u64::try_from(batches.len()).unwrap_or(u64::MAX))
    }

    /// The program a batch's pass draws with: its layers share a tier, and a
    /// batch of several is drawn only with the loop.
    fn batch_program(
        &self,
        batch: &[(&CompositeLayer<'_>, PixelRect, LayerTier)],
    ) -> (CompositeProgram, LayerTier, usize) {
        let tier = batch[0].2;
        debug_assert!(batch.iter().all(|(_, _, other)| *other == tier));
        let program = self.composite_program(tier);
        debug_assert!(batch.len() == 1 || program == CompositeProgram::Loop);
        (program, tier, batch.len())
    }

    /// Draws one composite pass with the pipeline of `program`.
    fn draw_batch(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        cached: &CachedCompositePipeline,
        (program, tier, layers): (CompositeProgram, LayerTier, usize),
        bind_group: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        pass: PassTarget,
    ) {
        let pipeline = self.composite_program_pipeline(cached, program);
        #[cfg(test)]
        {
            let mut log = self
                .composite_pipelines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            log.passes += 1;
            log.drawn
                .insert((program, pipeline.clone(), tier, layers > 1));
        }
        #[cfg(not(test))]
        let _ = (tier, layers);
        encode_batch(encoder, pipeline, bind_group, target, pass);
    }

    /// Whether this renderer composites small layers in batches, drawing every
    /// small layer with the loop pipeline: on a GPU, and in tests that set
    /// `composite_batch_limit`, which composite as a GPU does on any device
    /// (a limit of one draws a pass per layer with the loop pipeline).
    pub(crate) fn batches_small_layers(&self) -> bool {
        #[cfg(test)]
        {
            if self
                .composite_batch_limit
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
            {
                return true;
            }
        }
        !self.gpu.capabilities().software_rasterizer
    }

    /// The program that composites a layer of `tier`: the loop for a small
    /// layer on a renderer that batches small layers, whether its pass holds
    /// one layer or several, and the one-layer program otherwise.
    ///
    /// A pass of one small layer and a batch therefore run one compiled
    /// program and store the same bytes on every device; two programs, one
    /// for each, were optimized apart (contraction) by Metal, D3D12 and GL
    /// compilers, a last bit apart. A large layer never shares a pass, so no
    /// batch is compared with it, and the one-layer program spares it the
    /// loop. A software rasterizer, which runs every step of a shader whether
    /// it is taken or not, never batches and composites every layer with the
    /// one-layer program.
    pub(crate) fn composite_program(&self, tier: LayerTier) -> CompositeProgram {
        if tier == LayerTier::Small && self.batches_small_layers() {
            CompositeProgram::Loop
        } else {
            CompositeProgram::OneLayer
        }
    }

    /// The most layers one composite pass takes: [`COMPOSITE_BATCH_LAYERS`]
    /// on a GPU, one on a software rasterizer, or in tests what
    /// `composite_batch_limit` sets.
    ///
    /// A GPU runs a batch's step for a layer only on the 2 x 2 quads that
    /// overlap the layer, and saves the pass, the barrier and the backdrop
    /// copy each layer would cost. A software rasterizer runs every step of a
    /// shader on every pixel of the pass whatever it skips, so a small layer
    /// in a batch costs it the batch's whole area; a pass per layer, drawn
    /// only inside the layer with the one-layer program, costs it least.
    pub(crate) fn composite_batch_layers(&self) -> usize {
        #[cfg(test)]
        {
            let limit = self
                .composite_batch_limit
                .load(std::sync::atomic::Ordering::Relaxed);
            if limit > 0 {
                return limit.min(COMPOSITE_BATCH_LAYERS);
            }
        }
        if self.batches_small_layers() {
            COMPOSITE_BATCH_LAYERS
        } else {
            1
        }
    }

    /// The composite shader, layout and sampler both composite pipelines
    /// share, created on first use.
    pub(crate) fn composite_pipeline(&self) -> &CachedCompositePipeline {
        self.pipelines.composite.get_or_init(|| {
            let device = self.gpu.device();
            let bind_group_layout = batch_layout(device);
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("alpha2 layer composite shader"),
                source: wgpu::ShaderSource::Wgsl(COMPOSITE_WGSL.into()),
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("alpha2 layer sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            CachedCompositePipeline {
                bind_group_layout,
                sampler,
                shader,
                programs: Default::default(),
            }
        })
    }

    /// The pipeline of `program`, created the first time a pass draws with it.
    pub(crate) fn composite_program_pipeline<'a>(
        &self,
        cached: &'a CachedCompositePipeline,
        program: CompositeProgram,
    ) -> &'a wgpu::RenderPipeline {
        let (slot, entry, label) = match program {
            CompositeProgram::Loop => (0, "fs", "alpha2 small layer composite pipeline"),
            CompositeProgram::OneLayer => (1, "fs_layer", "alpha2 large layer composite pipeline"),
        };
        cached.programs[slot].get_or_init(|| {
            #[cfg(test)]
            {
                self.composite_pipelines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .created
                    .push(program);
            }
            composite_pipeline(
                self.gpu.device(),
                &cached.bind_group_layout,
                &cached.shader,
                self.intermediate_format(),
                entry,
                label,
            )
        })
    }

    /// The layers a composite draws, by index, with the target pixels each
    /// can change and its tier: those that reach the target and have opacity.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    pub(crate) fn drawn_layers(
        &self,
        visuals: impl Iterator<Item = LayerTransform>,
        size: RenderSize,
    ) -> Vec<(usize, PixelRect, LayerTier)> {
        #[cfg(test)]
        let full_frame = self
            .full_frame_layers
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let full_frame = false;
        visuals
            .enumerate()
            .filter_map(|(index, visual)| {
                if is_degenerate(visual) || is_invisible(visual) {
                    return None;
                }
                let bounds = layer_bounds(visual, size);
                // Drawn over the whole target in tests, a layer keeps the tier of
                // its bounds, so it draws with the same pipeline as drawn inside them.
                let tier = LayerTier::of(bounds.unwrap_or(PixelRect::EMPTY), size);
                if full_frame {
                    Some((index, PixelRect::whole(size), tier))
                } else {
                    bounds.map(|bounds| (index, bounds, tier))
                }
            })
            .collect()
    }

    /// The passes, and so the batch uniforms, a composite of these layers
    /// encodes, a clear counted as none.
    pub(crate) fn composite_batch_count(
        &self,
        visuals: impl Iterator<Item = LayerTransform>,
        size: RenderSize,
    ) -> usize {
        let drawn = self.drawn_layers(visuals, size);
        composite_batches(&drawn, self.composite_batch_layers()).len()
    }

    /// Compiles the pipelines a composite of `layers` layers can draw with.
    pub(crate) fn warm_composite(&self, layers: usize) {
        if layers == 0 {
            return;
        }
        let cached = self.composite_pipeline();
        self.composite_program_pipeline(cached, CompositeProgram::OneLayer);
        if self.batches_small_layers() {
            self.composite_program_pipeline(cached, CompositeProgram::Loop);
        }
    }
}

/// The size class of a drawn layer, which alone decides both whether it may
/// share a pass ([`composite_batches`]) and which composite program draws it
/// ([`Renderer::composite_program`]), so the two cannot disagree.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum LayerTier {
    /// Bounds under half the target: may share a pass.
    Small,
    /// Bounds of half the target or more: always a pass of its own.
    Large,
}

impl LayerTier {
    /// The tier of a layer that can change the target pixels `bounds`.
    pub(crate) fn of(bounds: PixelRect, size: RenderSize) -> Self {
        if under_half(bounds, size) {
            Self::Small
        } else {
            Self::Large
        }
    }
}

/// Whether `bounds` covers less than half the target: a layer's tier, and
/// whether a pass draws only inside its layers' bounds.
fn under_half(bounds: PixelRect, size: RenderSize) -> bool {
    bounds.area() * 2 < PixelRect::whole(size).area()
}

/// The two composite programs, entry points of one shader that share its
/// composite function (composite.wesl `composite_layer`).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum CompositeProgram {
    /// `fs`: the pass's layers in a loop, their count a uniform.
    Loop,
    /// `fs_layer`: one layer, no loop.
    OneLayer,
}

/// Test-only record of the composite pipelines a renderer created and drew
/// with.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct CompositePipelineLog {
    /// The program of each composite pipeline created, in order.
    pub(crate) created: Vec<CompositeProgram>,
    /// Composite passes drawn.
    pub(crate) passes: usize,
    /// Every distinct program, pipeline, tier and whether the pass held
    /// several layers, of those passes.
    pub(crate) drawn:
        std::collections::HashSet<(CompositeProgram, wgpu::RenderPipeline, LayerTier, bool)>,
}

/// Groups drawn layers, in order, into the passes that composite them, each
/// with the union of its layers' bounds.
///
/// Only small layers ([`LayerTier::Small`], under half the target) share a
/// pass, up to `limit` of them. Such a pass saves a small layer what
/// dominates its cost, the pass itself, its barrier and its backdrop copy; a
/// large layer's pass costs its pixels, and in a batch every step runs over
/// the batch's whole area, so it keeps a pass of its own.
pub(crate) fn composite_batches<T>(
    drawn: &[(T, PixelRect, LayerTier)],
    limit: usize,
) -> Vec<(std::ops::Range<usize>, PixelRect)> {
    let mut batches: Vec<(std::ops::Range<usize>, PixelRect)> = Vec::new();
    for (index, (_, bounds, tier)) in drawn.iter().enumerate() {
        match batches.last_mut() {
            Some((range, union))
                if range.len() < limit
                    && *tier == LayerTier::Small
                    && drawn[range.start].2 == LayerTier::Small =>
            {
                range.end = index + 1;
                *union = union.union(*bounds);
            }
            _ => batches.push((index..index + 1, *bounds)),
        }
    }
    batches
}

/// How the intermediate texture stores a value, which the composite shader
/// rounds to itself (its `STORED_FORMAT`, fixed by the renderer's
/// intermediate format).
#[derive(Copy, Clone, Debug)]
enum StoredValue {
    /// Kept as computed.
    AsIs,
    /// Rounded to a half float.
    Half,
    /// Rounded to an 8-bit unorm code.
    Unorm8,
}

impl StoredValue {
    fn of(format: wgpu::TextureFormat) -> Self {
        match format {
            wgpu::TextureFormat::Rgba16Float => Self::Half,
            wgpu::TextureFormat::Rgba8Unorm => Self::Unorm8,
            _ => Self::AsIs,
        }
    }

    const fn shader_value(self) -> f64 {
        match self {
            Self::AsIs => 0.0,
            Self::Half => 1.0,
            Self::Unorm8 => 2.0,
        }
    }
}

struct BatchPreparationContext<'a> {
    device: &'a wgpu::Device,
    layout: &'a wgpu::BindGroupLayout,
    sampler: &'a wgpu::Sampler,
    /// Bound as the backdrop of the first pass, which composites over
    /// `background` and reads no texture.
    no_backdrop: &'a wgpu::TextureView,
    blend_space: crate::BlendSpace,
    max_filter_samples: u32,
    output_size: RenderSize,
    /// The first layer's backdrop, as [`stored_background`] gives it.
    background: [f32; 4],
}

/// Writes one batch's `BatchParams` and binds its layers' textures, the
/// unused texture slots repeating the first layer's. Without a `backdrop` the
/// batch composites over the background.
fn prepare_batch(
    context: &BatchPreparationContext<'_>,
    backdrop: Option<&wgpu::TextureView>,
    batch: &[(&CompositeLayer<'_>, PixelRect, LayerTier)],
    uniforms: &mut UniformArenaLease,
) -> wgpu::BindGroup {
    debug_assert!(!batch.is_empty() && batch.len() <= COMPOSITE_BATCH_LAYERS);
    let mut values = [0.0_f32; BATCH_UNIFORM_FLOATS];
    #[allow(clippy::cast_precision_loss)]
    let controls = [
        context.blend_space.shader_value(),
        context.max_filter_samples as f32,
        batch.len() as f32,
        if backdrop.is_some() { 0.0 } else { 1.0 },
        context.output_size.width as f32,
        context.output_size.height as f32,
        0.0,
        0.0,
        context.background[0],
        context.background[1],
        context.background[2],
        context.background[3],
    ];
    values[..controls.len()].copy_from_slice(&controls);
    for ((layer, bounds, _), slot) in batch
        .iter()
        .zip(values[controls.len()..].as_chunks_mut::<16>().0)
    {
        let visual = layer.visual;
        #[allow(clippy::cast_precision_loss)]
        slot.copy_from_slice(&[
            visual.position[0],
            visual.position[1],
            visual.anchor[0],
            visual.anchor[1],
            visual.scale[0],
            visual.scale[1],
            visual.rotation_degrees.to_radians(),
            visual.opacity,
            layer.blend_mode.shader_value(),
            0.0,
            0.0,
            0.0,
            bounds.x as f32,
            bounds.y as f32,
            (bounds.x + bounds.width) as f32,
            (bounds.y + bounds.height) as f32,
        ]);
    }
    let uniform = uniforms.write(&f32_bytes(values));
    let texture = |slot: usize| {
        let (layer, _, _) = batch.get(slot).unwrap_or(&batch[0]);
        layer.texture
    };
    let mut entries = (0..COMPOSITE_BATCH_LAYERS)
        .map(|slot| wgpu::BindGroupEntry {
            binding: u32::try_from(slot).expect("few layer slots"),
            resource: wgpu::BindingResource::TextureView(texture(slot)),
        })
        .collect::<Vec<_>>();
    entries.push(wgpu::BindGroupEntry {
        binding: SAMPLER_BINDING,
        resource: wgpu::BindingResource::Sampler(context.sampler),
    });
    entries.push(wgpu::BindGroupEntry {
        binding: BACKDROP_BINDING,
        resource: wgpu::BindingResource::TextureView(backdrop.unwrap_or(context.no_backdrop)),
    });
    entries.push(wgpu::BindGroupEntry {
        binding: UNIFORM_BINDING,
        resource: uniforms.binding(uniform),
    });
    context
        .device
        .create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("alpha2 layer batch bind group"),
            layout: context.layout,
            entries: &entries,
        })
}

/// Clears `target` to `stored`, a value the target's format holds exactly
/// ([`stored_background`]), so no device rounds it: the clear stores what a
/// layer pass writes for the same background.
fn clear_target(encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, stored: [f32; 4]) {
    let color = stored_color(stored);
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("alpha2 layer clear pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(color),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
}

/// How a composite pass begins and where it draws.
#[derive(Copy, Clone)]
struct PassTarget {
    /// What the target holds before the pass: kept, or cleared.
    load: wgpu::LoadOp<wgpu::Color>,
    /// Only these pixels are drawn; the whole target when `None`.
    scissor: Option<PixelRect>,
}

impl PassTarget {
    /// A pass that writes every pixel, so what the target held does not matter.
    const WHOLE: Self = Self {
        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        scissor: None,
    };
}

/// A stored value as a clear color: a value the target holds exactly, so the
/// device converts it unchanged.
fn stored_color(stored: [f32; 4]) -> wgpu::Color {
    let [r, g, b, a] = stored.map(f64::from);
    wgpu::Color { r, g, b, a }
}

/// Draws one batch into `target`, over the whole frame, or with a scissor
/// only inside it, the target loaded or cleared first.
fn encode_batch(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    PassTarget { load, scissor }: PassTarget,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("alpha2 layer composition pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    if let Some(scissor) = scissor {
        pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
    }
    pass.draw(0..3, 0..1);
}

/// Copies one region between two textures of the same size and format.
fn copy_region(
    encoder: &mut wgpu::CommandEncoder,
    source: &wgpu::Texture,
    destination: &wgpu::Texture,
    region: PixelRect,
) {
    let at = |texture| wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d {
            x: region.x,
            y: region.y,
            z: 0,
        },
        aspect: wgpu::TextureAspect::All,
    };
    encoder.copy_texture_to_texture(
        at(source),
        at(destination),
        wgpu::Extent3d {
            width: region.width,
            height: region.height,
            depth_or_array_layers: 1,
        },
    );
}

/// A rectangle of target pixels.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct PixelRect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl PixelRect {
    /// No pixels: the bounds of a layer that misses the target.
    const EMPTY: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    const fn whole(size: RenderSize) -> Self {
        Self {
            x: 0,
            y: 0,
            width: size.width,
            height: size.height,
        }
    }

    pub(crate) fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// The smallest rectangle holding both.
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

/// Pixels beyond a layer's transformed edges that its coverage can still
/// reach: the antialiasing footprint is at most about one and a half pixels,
/// so this is generous.
const COVERAGE_MARGIN: f64 = 4.0;

/// The target pixels a layer can change: its transformed source rectangle,
/// widened by [`COVERAGE_MARGIN`] and clamped to the target, or `None` when
/// that misses the target. Everywhere else the layer's coverage is zero and
/// compositing returns the backdrop unchanged.
///
/// Mirrors the composite shader's mapping: a source corner `(u, v)` lands at
/// `position * size + R^T ((u, v) - anchor) * scale * size`, with `R` the
/// shader's rotation.
pub(crate) fn layer_bounds(visual: LayerTransform, size: RenderSize) -> Option<PixelRect> {
    let (width, height) = (f64::from(size.width), f64::from(size.height));
    let (turn_sin, turn_cos) = f64::from(visual.rotation_degrees).to_radians().sin_cos();
    let corner = |u: f64, v: f64| {
        let x = (u - f64::from(visual.anchor[0])) * f64::from(visual.scale[0]) * width;
        let y = (v - f64::from(visual.anchor[1])) * f64::from(visual.scale[1]) * height;
        (
            f64::from(visual.position[0]) * width + turn_cos * x - turn_sin * y,
            f64::from(visual.position[1]) * height + turn_sin * x + turn_cos * y,
        )
    };
    let corners = [
        corner(0.0, 0.0),
        corner(1.0, 0.0),
        corner(0.0, 1.0),
        corner(1.0, 1.0),
    ];
    let fold = |pick: fn(&(f64, f64)) -> f64, extreme: fn(f64, f64) -> f64, start: f64| {
        corners.iter().map(pick).fold(start, extreme)
    };
    let left = (fold(|c| c.0, f64::min, f64::INFINITY) - COVERAGE_MARGIN)
        .floor()
        .max(0.0);
    let top = (fold(|c| c.1, f64::min, f64::INFINITY) - COVERAGE_MARGIN)
        .floor()
        .max(0.0);
    let right = (fold(|c| c.0, f64::max, f64::NEG_INFINITY) + COVERAGE_MARGIN)
        .ceil()
        .min(width);
    let bottom = (fold(|c| c.1, f64::max, f64::NEG_INFINITY) + COVERAGE_MARGIN)
        .ceil()
        .min(height);
    if !(right > left && bottom > top) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(PixelRect {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// The canvas background as the intermediate texture stores it: 8-bit or
/// half-float precision, rounded to nearest, transparent when absent.
///
/// Both ways a canvas gets its background take this value: the clear of a
/// canvas with nothing drawn on it, and the first layer's pass, which writes
/// it wherever the layer does not reach. A device converting the original
/// value itself may round a tie, or a product `f32` makes a tie, the other
/// way (0.7 is 178.5 in `f32` and 178.49999 exactly; a half-float clear of
/// 0.3 read back as 76 and the layer pass's 77 on every Vulkan driver
/// tested), so an empty frame and the frame beside it would differ by a
/// code.
pub(crate) fn stored_background(
    background: Option<Color>,
    format: wgpu::TextureFormat,
) -> [f32; 4] {
    let Some(color) = background else {
        return [0.0; 4];
    };
    // A clear value reaches the device as `f32` and is converted from there.
    #[allow(clippy::cast_possible_truncation)]
    let channel = |value: f64| {
        let value = value as f32;
        match format {
            wgpu::TextureFormat::Rgba8Unorm => (value.clamp(0.0, 1.0) * 255.0).round() / 255.0,
            wgpu::TextureFormat::Rgba16Float => half::f16::from_f32(value).to_f32(),
            _ => value,
        }
    };
    [
        channel(color.red),
        channel(color.green),
        channel(color.blue),
        channel(color.alpha),
    ]
}

const SAMPLER_BINDING: u32 = 8;
const BACKDROP_BINDING: u32 = 9;
const UNIFORM_BINDING: u32 = 10;

/// The bind group layout of a composite pass: a texture per layer slot, the
/// sampler, the backdrop, and the batch uniform.
fn batch_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let mut entries = (0..COMPOSITE_BATCH_LAYERS)
        .map(|slot| texture_entry(u32::try_from(slot).expect("few layer slots"), true))
        .collect::<Vec<_>>();
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: SAMPLER_BINDING,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    });
    entries.push(texture_entry(BACKDROP_BINDING, false));
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: UNIFORM_BINDING,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: batch_uniform_binding(),
        count: None,
    });
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("alpha2 layer batch bind group layout"),
        entries: &entries,
    })
}

/// The batch uniform binding, at the size the graph reserves and each batch
/// writes: a `BatchParams` that grew past it fails pipeline creation instead
/// of overrunning the uniform arena on a device with a small alignment.
fn batch_uniform_binding() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: false,
        min_binding_size: std::num::NonZeroU64::new(float_uniform_bytes(BATCH_UNIFORM_FLOATS)),
    }
}

fn texture_entry(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn composite_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    fragment_entry: &str,
    label: &str,
) -> wgpu::RenderPipeline {
    let label = Some(label);
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label,
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let constants = [("STORED_FORMAT", StoredValue::of(format).shader_value())];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label,
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            targets: &[Some(format.into())],
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &constants,
                ..wgpu::PipelineCompilationOptions::default()
            },
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

pub(crate) fn validate_visual(visual: LayerTransform) -> Result<(), RenderError> {
    let values = [
        visual.position[0],
        visual.position[1],
        visual.anchor[0],
        visual.anchor[1],
        visual.scale[0],
        visual.scale[1],
        visual.rotation_degrees,
        visual.opacity,
    ];
    if values.into_iter().any(|value| !value.is_finite()) || !(0.0..=1.0).contains(&visual.opacity)
    {
        return Err(RenderError::InvalidVisual);
    }
    Ok(())
}

/// Whether a layer collapses to a line or a point and covers no pixel.
///
/// A scale that passes through zero (an animation from 1 to -1, or an eased
/// curve that overshoots) is a legitimate instant, not an error: the layer is
/// not visible, and its transform has no inverse to sample with.
pub(crate) fn is_degenerate(visual: LayerTransform) -> bool {
    visual.scale[0].abs() <= f32::EPSILON || visual.scale[1].abs() <= f32::EPSILON
}

/// Whether a layer has no opacity, so it adds nothing to any pixel.
///
/// Every blend mode weighs its result by the source's alpha, which opacity
/// scales, so the layer leaves the backdrop as it is. Drawing it anyway would
/// cost a pass of the layer's size and change the backdrop only by rounding
/// it through the blend space. An inactive matte hides its clip this way, and
/// a fade or hold reaches it.
pub(crate) fn is_invisible(visual: LayerTransform) -> bool {
    visual.opacity <= 0.0
}
