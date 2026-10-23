use std::collections::HashSet;
use web_time::Instant;

use crate::composite::CompositeTarget;
use crate::graph::{GraphError, RenderCacheKey, RenderGraph, RenderNode, RenderNodeId};
use crate::graph_cache::{renderer_cache_key, renderer_node_key};
use crate::runtime_effect::{EffectPass, PendingEffectTiming};
use crate::uniform::{
    BATCH_UNIFORM_FLOATS, RUNTIME_UNIFORM_WORDS, UniformArenaLease, aligned_uniform_end,
    float_uniform_bytes, param_uniform_bytes,
};
use crate::{
    CompositeLayer, EffectId, EffectKind, RenderedTexture, Renderer, ShaderDiagnostics,
    WarmupReport,
};

pub(crate) struct EncodedGraph {
    texture: RenderedTexture,
    /// Results to keep once the work that draws them is submitted and the
    /// GPU accepted it: kept any earlier, a failed encode or a rejected
    /// submission would leave unwritten textures behind.
    cache_inserts: Vec<(RenderCacheKey, RenderedTexture, bool)>,
    uniforms: UniformArenaLease,
    pending_timings: Vec<PendingEffectTiming>,
    encoded_nodes: u64,
    encoded_render_passes: u64,
}

impl EncodedGraph {
    pub(crate) fn texture(&self) -> &RenderedTexture {
        &self.texture
    }

    pub(crate) fn flush_uniforms(&self, queue: &wgpu::Queue) {
        self.uniforms.flush(queue);
    }
}

impl Renderer {
    /// Returns a deterministic key for the graph and the renderer
    /// configuration that changes its pixels: the blend space, the quality
    /// controls (as passes take them), and the intermediate format. Two
    /// renderers on adapters that store pictures alike give equal keys.
    ///
    /// # Errors
    /// Returns [`GraphError::MissingOutput`] when no output was selected.
    pub fn graph_cache_key(&self, graph: &RenderGraph<'_>) -> Result<RenderCacheKey, GraphError> {
        let quality = self.quality_controls();
        let plan = graph.compile_with_quality(quality)?;
        Ok(renderer_cache_key(
            plan.cache_key,
            graph.blend_space.unwrap_or(self.blend_space).shader_value(),
            quality,
            self.intermediate_format(),
        ))
    }

    /// Initializes all pipelines referenced by one graph.
    ///
    /// # Errors
    /// Returns graph validation errors or diagnostics for an unknown effect.
    pub fn warmup_graph(&self, graph: &RenderGraph<'_>) -> Result<WarmupReport, ShaderDiagnostics> {
        self.warmup_graphs(&[graph])
    }

    /// Initializes the deduplicated pipeline set referenced by project graphs.
    ///
    /// # Errors
    /// Returns diagnostics for a malformed graph or an unknown effect.
    pub fn warmup_graphs(
        &self,
        graphs: &[&RenderGraph<'_>],
    ) -> Result<WarmupReport, ShaderDiagnostics> {
        let mut effects = Vec::<EffectId>::new();
        let mut seen = HashSet::<EffectId>::new();
        let mut needs_composite = false;
        let mut composite_layers = Vec::<usize>::new();
        let mut vector_spaces = Vec::<crate::BlendSpace>::new();
        for graph in graphs {
            let plan = graph
                .compile()
                .map_err(|error| ShaderDiagnostics::error(error.to_string()))?;
            for node in plan.order {
                match &graph.nodes[node.index] {
                    RenderNode::Effect { effect, .. }
                    | RenderNode::Transition { effect, .. }
                    | RenderNode::Generator { effect, .. } => {
                        if seen.insert(effect.clone()) {
                            effects.push(effect.clone());
                        }
                    }
                    RenderNode::Composite { layers, .. } => {
                        needs_composite = true;
                        composite_layers.push(layers.len());
                    }
                    RenderNode::Vector { .. } => {
                        let space = graph.blend_space.unwrap_or(self.blend_space);
                        if !vector_spaces.contains(&space) {
                            vector_spaces.push(space);
                        }
                    }
                    RenderNode::Source(_) => {}
                }
            }
        }
        self.warmup(&effects)?;
        for layers in composite_layers {
            self.warm_composite(layers);
        }
        for space in &vector_spaces {
            self.warm_vector(*space);
        }
        Ok(WarmupReport {
            runtime_effects: effects.len(),
            builtins: usize::from(needs_composite) + usize::from(!vector_spaces.is_empty()),
        })
    }

    /// Executes the reachable portion of a graph and returns its owned output.
    ///
    /// # Errors
    /// Returns a structural error or the first rendering failure.
    #[allow(clippy::too_many_lines)]
    pub fn execute_graph(&self, graph: &RenderGraph<'_>) -> Result<RenderedTexture, GraphError> {
        let mut command_encoder = self.command_encoder("alpha2 render graph encoder");
        let graph_work = self.encode_graph(graph, &mut command_encoder)?;
        graph_work.flush_uniforms(self.gpu.queue());
        self.submit([command_encoder.finish()])
            .map_err(GraphError::Render)?;
        Ok(self.complete_graph_work(graph_work, true))
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn encode_graph(
        &self,
        graph: &RenderGraph<'_>,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<EncodedGraph, GraphError> {
        self.validate_size(graph.size)?;
        let plan = graph.compile()?;
        if matches!(graph.nodes[plan.output.index], RenderNode::Source(_)) {
            return Err(GraphError::SourceOutput);
        }
        let format = self.intermediate_format();
        let uniform_alignment = u64::from(self.limits.min_uniform_buffer_offset_alignment.max(1));
        let uniform_capacity = graph_uniform_capacity(self, graph, &plan.order, uniform_alignment);
        let mut uniforms = self
            .uniform_buffers
            .acquire(self.gpu.device(), uniform_capacity);
        let mut outputs: Vec<Option<RenderedTexture>> =
            (0..graph.nodes.len()).map(|_| None).collect();
        let blend_space = graph.blend_space.unwrap_or(self.blend_space).shader_value();
        let cached = self.cached_results(graph, &plan.order, plan.output, blend_space);
        let mut pending_timings = Vec::<PendingEffectTiming>::new();
        let mut encoded_nodes = 0_u64;
        let mut encoded_render_passes = 0_u64;
        let mut cache_inserts = Vec::new();
        // Reading a clock per pass is profiling cost, so it is opt-in.
        let timing = self.pass_timing_epoch();
        for (position, node_id) in plan.order.iter().copied().enumerate() {
            let started = timing.map(|epoch| (epoch, Instant::now()));
            if let Some(hit) = cached.hits[node_id.index].clone() {
                outputs[node_id.index] = Some(hit);
                release_inputs(graph, &plan.last_use, &mut outputs, node_id, position);
                continue;
            }
            if !cached.needed[node_id.index] {
                // Only results found in the cache consume it: release its inputs where they
                // would have been.
                release_inputs(graph, &plan.last_use, &mut outputs, node_id, position);
                continue;
            }
            let output = match &graph.nodes[node_id.index] {
                RenderNode::Source(_) => None,
                RenderNode::Effect {
                    effect,
                    source,
                    inputs,
                    params,
                    resources,
                    runtime,
                } => {
                    if self
                        .effect_definition(effect)
                        .is_some_and(|definition| definition.kind != EffectKind::Effect)
                    {
                        return Err(GraphError::EffectKindMismatch);
                    }
                    let source_view = graph_view(graph, &outputs, *source);
                    // Empty, and so not allocated, for an effect without named inputs.
                    let input_views: Vec<(&str, &wgpu::TextureView)> = inputs
                        .iter()
                        .map(|(name, node)| (name, graph_view(graph, &outputs, node)))
                        .collect();
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph effect target",
                    );
                    if let Some(pending) = self.encode_effect_pass(
                        encoder,
                        EffectPass {
                            id: effect,
                            kind: crate::RenderNodeKind::Effect,
                            source_a: source_view,
                            source_b: None,
                            inputs: &input_views,
                            size: graph.size,
                            params,
                            resources,
                            runtime: *runtime,
                            output: target.view(),
                        },
                        &mut uniforms,
                    )? {
                        pending_timings.push(pending);
                    }
                    encoded_render_passes = encoded_render_passes.saturating_add(1);
                    Some(RenderedTexture::pooled(graph.size, format, target))
                }
                RenderNode::Transition {
                    effect,
                    from,
                    to,
                    params,
                    resources,
                    runtime,
                } => {
                    if self
                        .effect_definition(effect)
                        .is_some_and(|definition| definition.kind != EffectKind::Transition)
                    {
                        return Err(GraphError::EffectKindMismatch);
                    }
                    let from_view = graph_view(graph, &outputs, *from);
                    let to_view = graph_view(graph, &outputs, *to);
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph transition target",
                    );
                    if let Some(pending) = self.encode_effect_pass(
                        encoder,
                        EffectPass {
                            id: effect,
                            kind: crate::RenderNodeKind::Transition,
                            source_a: from_view,
                            source_b: Some(to_view),
                            inputs: &[],
                            size: graph.size,
                            params,
                            resources,
                            runtime: *runtime,
                            output: target.view(),
                        },
                        &mut uniforms,
                    )? {
                        pending_timings.push(pending);
                    }
                    encoded_render_passes = encoded_render_passes.saturating_add(1);
                    Some(RenderedTexture::pooled(graph.size, format, target))
                }
                RenderNode::Generator {
                    effect,
                    params,
                    resources,
                    runtime,
                } => {
                    if self
                        .effect_definition(effect)
                        .is_some_and(|definition| definition.kind != EffectKind::Generator)
                    {
                        return Err(GraphError::EffectKindMismatch);
                    }
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph generator target",
                    );
                    // A generator samples nothing; its bindings still need a texture.
                    if let Some(pending) = self.encode_effect_pass(
                        encoder,
                        EffectPass {
                            id: effect,
                            kind: crate::RenderNodeKind::Generator,
                            source_a: &self.empty_input,
                            source_b: None,
                            inputs: &[],
                            size: graph.size,
                            params,
                            resources,
                            runtime: *runtime,
                            output: target.view(),
                        },
                        &mut uniforms,
                    )? {
                        pending_timings.push(pending);
                    }
                    encoded_render_passes = encoded_render_passes.saturating_add(1);
                    Some(RenderedTexture::pooled(graph.size, format, target))
                }
                RenderNode::Vector { scene } => {
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph vector target",
                    );
                    encoded_render_passes =
                        encoded_render_passes.saturating_add(self.encode_vector(
                            encoder,
                            scene,
                            graph.size,
                            graph.blend_space.unwrap_or(self.blend_space),
                            target.view(),
                            &mut uniforms,
                        )?);
                    Some(RenderedTexture::pooled(graph.size, format, target))
                }
                RenderNode::Composite { layers, background } => {
                    let views = layers
                        .iter()
                        .map(|layer| graph_view(graph, &outputs, layer.source))
                        .collect::<Vec<_>>();
                    let composite_layers = layers
                        .iter()
                        .zip(&views)
                        .map(|(layer, view)| CompositeLayer {
                            texture: view,
                            visual: layer.visual,
                            blend_mode: layer.blend_mode,
                        })
                        .collect::<Vec<_>>();
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph composite target",
                    );
                    encoded_render_passes =
                        encoded_render_passes.saturating_add(self.encode_layers_pass(
                            encoder,
                            graph.size,
                            &composite_layers,
                            CompositeTarget {
                                background: *background,
                                blend_space: graph.blend_space.unwrap_or(self.blend_space),
                                output: target.texture(),
                                output_view: target.view(),
                            },
                            &mut uniforms,
                        )?);
                    Some(RenderedTexture::pooled(graph.size, format, target))
                }
            };
            if let Some((epoch, started)) = started
                && !matches!(graph.nodes[node_id.index], RenderNode::Source(_))
            {
                let label = graph.nodes[node_id.index].kind().pass_label();
                crate::diagnostics::record_pass(
                    &self.diagnostics,
                    epoch,
                    label,
                    started.elapsed(),
                    crate::TimingSource::CpuEncoding,
                );
            }
            if let (Some(output), Some((key, pinned))) = (&output, cached.store[node_id.index]) {
                cache_inserts.push((key, output.clone(), pinned));
            }
            outputs[node_id.index] = output;
            if !matches!(graph.nodes[node_id.index], RenderNode::Source(_)) {
                encoded_nodes = encoded_nodes.saturating_add(1);
            }
            release_inputs(graph, &plan.last_use, &mut outputs, node_id, position);
        }

        let texture = outputs[plan.output.index]
            .take()
            .ok_or(GraphError::SourceOutput)?;
        Ok(EncodedGraph {
            texture,
            cache_inserts,
            uniforms,
            pending_timings,
            encoded_nodes,
            encoded_render_passes,
        })
    }

    pub(crate) fn complete_graph_work(
        &self,
        encoded: EncodedGraph,
        standalone_submission: bool,
    ) -> RenderedTexture {
        let EncodedGraph {
            texture,
            cache_inserts,
            uniforms,
            pending_timings,
            encoded_nodes,
            encoded_render_passes,
        } = encoded;
        uniforms.recycle();
        if !cache_inserts.is_empty() {
            let mut cache = self.lock_result_cache();
            for (key, result, pinned) in &cache_inserts {
                cache.insert(*key, result, *pinned);
            }
        }
        crate::diagnostics::record_graph_work(
            &self.diagnostics,
            encoded_nodes,
            encoded_render_passes,
            standalone_submission,
        );
        for timing in pending_timings {
            timing.map_after_submit();
        }
        texture
    }
}

fn graph_uniform_capacity(
    renderer: &Renderer,
    graph: &RenderGraph<'_>,
    order: &[RenderNodeId],
    alignment: u64,
) -> u64 {
    let mut capacity = 0_u64;
    for node in order {
        match &graph.nodes[node.index] {
            RenderNode::Effect { effect, .. }
            | RenderNode::Transition { effect, .. }
            | RenderNode::Generator { effect, .. } => {
                let parameter_bytes = renderer
                    .effects
                    .get(effect)
                    .map_or(0, |registered| registered.definition.params.len());
                let parameter_bytes =
                    u64::try_from(param_uniform_bytes(parameter_bytes)).unwrap_or(u64::MAX);
                capacity = aligned_uniform_end(capacity, parameter_bytes, alignment);
                capacity = aligned_uniform_end(
                    capacity,
                    float_uniform_bytes(RUNTIME_UNIFORM_WORDS),
                    alignment,
                );
            }
            RenderNode::Composite { layers, .. } => {
                let passes = renderer
                    .composite_batch_count(layers.iter().map(|layer| layer.visual), graph.size);
                for _ in 0..passes {
                    capacity = aligned_uniform_end(
                        capacity,
                        float_uniform_bytes(BATCH_UNIFORM_FLOATS),
                        alignment,
                    );
                }
            }
            RenderNode::Vector { scene } => {
                // A slice per fill and per layer, and one for the encode pass.
                let slices = scene
                    .ops
                    .iter()
                    .filter(|op| !matches!(op, alpha2_geometry::VectorOp::PushLayer { .. }))
                    .count()
                    + 1;
                for _ in 0..slices {
                    capacity = aligned_uniform_end(
                        capacity,
                        crate::vector::VECTOR_UNIFORM_BYTES,
                        alignment,
                    );
                }
            }
            RenderNode::Source(_) => {}
        }
    }
    capacity
}

fn graph_view<'a>(
    graph: &'a RenderGraph<'_>,
    outputs: &'a [Option<RenderedTexture>],
    node: RenderNodeId,
) -> &'a wgpu::TextureView {
    match &graph.nodes[node.index] {
        RenderNode::Source(view) => view,
        _ => outputs[node.index]
            .as_ref()
            .expect("compiled dependency has an output")
            .view(),
    }
}

/// Drops the results `node` was the last to read, returning them to the pool.
fn release_inputs(
    graph: &RenderGraph<'_>,
    last_use: &[Option<usize>],
    outputs: &mut [Option<RenderedTexture>],
    node: RenderNodeId,
    position: usize,
) {
    for dependency in graph.nodes[node.index].dependencies() {
        if last_use[dependency.index] == Some(position) {
            outputs[dependency.index] = None;
        }
    }
}

/// What the result cache holds for one graph: results found, results to
/// keep, and which nodes must still be drawn.
struct CachedResults {
    hits: Vec<Option<RenderedTexture>>,
    store: Vec<Option<(RenderCacheKey, bool)>>,
    needed: Vec<bool>,
}

impl Renderer {
    /// Looks up the graph's cache candidates and works out which nodes a hit
    /// makes unnecessary. With the cache off or no candidates, every node is
    /// drawn and nothing is kept.
    fn cached_results(
        &self,
        graph: &RenderGraph<'_>,
        order: &[RenderNodeId],
        output: RenderNodeId,
        blend_space: f32,
    ) -> CachedResults {
        let count = graph.nodes.len();
        let mut cached = CachedResults {
            hits: vec![None; count],
            store: vec![None; count],
            needed: vec![true; count],
        };
        if graph.intents.iter().all(Option::is_none) {
            return cached;
        }
        let mut cache = self.lock_result_cache();
        if !cache.enabled() {
            return cached;
        }
        let eligibility = graph.eligibility();
        let (mut hits, mut misses, mut ineligible) = (0, 0, 0);
        // From the output down: a node is looked up only if something still
        // needs it, and a hit's inputs are needed only by others.
        cached.needed = vec![false; count];
        cached.needed[output.index] = true;
        for node in order.iter().rev() {
            if !cached.needed[node.index] {
                continue;
            }
            if let Some(intent) = graph.intents[node.index] {
                if eligibility[node.index] == crate::CacheEligibility::Eligible {
                    let key = renderer_node_key(graph.cache_keys[node.index], blend_space);
                    let pinned = intent == crate::CacheIntent::Pin;
                    if let Some(texture) = cache.get(key, pinned) {
                        cached.hits[node.index] = Some(texture);
                        hits += 1;
                        continue;
                    }
                    cached.store[node.index] = Some((key, pinned));
                    misses += 1;
                } else if intent != crate::CacheIntent::Bypass {
                    ineligible += 1;
                }
            }
            for input in graph.nodes[node.index].dependencies() {
                cached.needed[input.index] = true;
            }
        }
        let skipped = order
            .iter()
            .filter(|node| {
                !cached.needed[node.index]
                    && cached.hits[node.index].is_none()
                    && !matches!(graph.nodes[node.index], RenderNode::Source(_))
            })
            .count() as u64;
        cache.record(hits, misses, ineligible, skipped);
        cached
    }
}
