use std::collections::HashSet;
use web_time::Instant;

use crate::composite::CompositeTarget;
use crate::frame_gpu_time::FrameSample;
use crate::graph::{GraphError, RenderCacheKey, RenderGraph, RenderNode, RenderNodeId};
use crate::graph_cache::{renderer_cache_key, renderer_node_key};
use crate::prepared_frame::{FramePack, PreparedNode};
use crate::runtime_effect::{EffectPass, PendingEffectTiming};
use crate::submission_pilot::PassKind;
use crate::uniform::UniformArenaLease;
use crate::yuv::SourceWorkTicket;
use crate::{
    CompositeLayer, EffectId, EffectKind, RenderedTexture, Renderer, ShaderDiagnostics,
    WarmupReport,
};

pub(crate) struct FrameStart {
    plan: crate::RenderPlan,
    prepared: crate::prepared_frame::PreparedFrame,
    uniforms: UniformArenaLease,
    cached: CachedResults,
    submission_plan: Option<crate::submission_planner::SubmissionPlan>,
}

pub(crate) struct EncodedGraph {
    texture: RenderedTexture,
    /// Results to keep after the submission is accepted; keeping them earlier could leave
    /// unwritten textures after a failure.
    cache_inserts: Vec<(RenderCacheKey, RenderedTexture, bool)>,
    /// The result cache's clear count at lookup; a later clear drops `cache_inserts`.
    cache_clears: u64,
    uniforms: UniformArenaLease,
    pending_timings: Vec<PendingEffectTiming>,
    encoded_nodes: u64,
    encoded_render_passes: u64,
}

impl EncodedGraph {
    pub(crate) fn flush_uniforms(&self, queue: &wgpu::Queue) {
        self.uniforms.flush(queue);
    }

    pub(crate) fn output_and_uniforms(&mut self) -> (&RenderedTexture, &mut UniformArenaLease) {
        (&self.texture, &mut self.uniforms)
    }
}

impl Renderer {
    /// A deterministic key for the graph and the settings that affect its pixels (blend space,
    /// applied quality controls, intermediate format). Adapters that store pictures alike give
    /// equal keys.
    ///
    /// # Errors
    /// Returns [`GraphError::MissingOutput`] when no output was selected.
    pub fn graph_cache_key(&self, graph: &RenderGraph<'_>) -> Result<RenderCacheKey, GraphError> {
        let plan = graph.compile()?;
        Ok(renderer_cache_key(
            plan.cache_key,
            graph.blend_space.unwrap_or(self.blend_space).shader_value(),
            self.quality_controls(),
            self.intermediate_format(),
        ))
    }

    /// Initializes all pipelines referenced by one graph.
    ///
    /// # Errors
    /// Returns graph validation errors, or diagnostics for an unknown effect or a failed pipeline.
    pub fn warmup_graph(&self, graph: &RenderGraph<'_>) -> Result<WarmupReport, ShaderDiagnostics> {
        self.warmup_graphs(&[graph])
    }

    /// Initializes the deduplicated pipeline set referenced by project graphs.
    ///
    /// # Errors
    /// Returns diagnostics for a malformed graph, an unknown effect or a failed pipeline.
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
        let failed = |error: crate::RenderError| ShaderDiagnostics::error(error.to_string());
        for layers in composite_layers {
            self.warm_composite(layers).map_err(failed)?;
        }
        for space in &vector_spaces {
            self.warm_vector(*space).map_err(failed)?;
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
        let _frame = self.frame_lock.hold();
        let start = self.prepare_graph(graph, &[], FramePack::Nothing)?;
        let mut command_encoder = self.command_encoder("alpha2 render graph encoder");
        let graph_work = self.encode_graph(start, graph, &mut command_encoder, &[], None)?;
        graph_work.flush_uniforms(self.gpu.queue());
        self.submit([command_encoder.finish()])
            .map_err(GraphError::Render)?;
        Ok(self.complete_graph_work(graph_work, true))
    }

    pub(crate) fn prepare_graph(
        &self,
        graph: &RenderGraph<'_>,
        sources: &[SourceWorkTicket<'_>],
        pack: FramePack,
    ) -> Result<FrameStart, GraphError> {
        self.forget_after_reported_errors();
        self.validate_size(graph.size)?;
        let plan = graph.compile()?;
        if matches!(graph.nodes[plan.output.index], RenderNode::Source(_)) {
            return Err(GraphError::SourceOutput);
        }
        let mut prepared = self.prepare_frame(graph, &plan.order);
        let blend_space = graph.blend_space.unwrap_or(self.blend_space).shader_value();
        let cached = self.cached_results(graph, &plan.order, plan.output, blend_space);
        let pixels = u64::from(graph.size.width) * u64::from(graph.size.height);
        let drawn = || {
            plan.order
                .iter()
                .filter(|node| cached.hits[node.index].is_none() && cached.needed[node.index])
        };
        prepared.prepare_vectors(self, graph, drawn());
        let uniforms = self.uniform_buffers.acquire(
            self.gpu.device(),
            prepared.uniform_capacity(sources, &plan.order, pack, self.uniform_alignment()),
        );
        let mut conversions = crate::FrameCost::default();
        for work in sources {
            conversions.add_cost(work.work());
        }
        let mut composites = crate::FrameCost::default();
        let mut predicted = pack.work(graph.size);
        predicted.add_cost(conversions);
        for node in drawn() {
            let work = node_work(&graph.nodes[node.index], prepared.node(*node), pixels);
            if prepared.composite(*node).is_some() {
                composites.add_cost(work);
            }
            predicted.add_cost(work);
        }
        let needed = self.texture_need(graph, &plan, &prepared, &cached, sources, pack);
        if let Some(budget) = self.gpu_memory_budget()
            && needed > budget
        {
            return Err(GraphError::Render(
                crate::RenderError::FrameExceedsGpuMemory { needed, budget },
            ));
        }
        let mut submission_plan = self.plan_submissions(predicted);
        if let Some(submission_plan) = &mut submission_plan {
            self.pilot_passes(
                drawn().map(|node| (&graph.nodes[node.index], prepared.registration(*node))),
                graph.size,
            );
            self.pilot_kinds(
                graph.size,
                &[
                    (PassKind::Conversion, conversions),
                    (PassKind::Composite, composites),
                    (PassKind::Pack, pack.work(graph.size)),
                ],
            );
            for node in drawn() {
                if let Some(Ok(scene)) = prepared.vector(*node) {
                    let _ = self.pilot_vector(scene, graph.size);
                }
            }
            submission_plan.predict(planned_passes(graph, drawn(), &prepared, sources, pack));
        }
        Ok(FrameStart {
            plan,
            prepared,
            uniforms,
            cached,
            submission_plan,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn encode_graph(
        &self,
        start: FrameStart,
        graph: &RenderGraph<'_>,
        encoder: &mut crate::renderer::RenderEncoder,
        sources: &[SourceWorkTicket<'_>],
        sample: Option<&FrameSample<'_>>,
    ) -> Result<EncodedGraph, GraphError> {
        let FrameStart {
            plan,
            prepared,
            mut uniforms,
            cached,
            submission_plan,
        } = start;
        encoder.planner().mark_frame();
        if let Some(submission_plan) = submission_plan {
            encoder.planner().plan(submission_plan);
        }
        let format = self.intermediate_format();
        let mut outputs: Vec<Option<RenderedTexture>> =
            (0..graph.nodes.len()).map(|_| None).collect();
        for work in sources {
            work.encode(self, encoder, &mut uniforms)?;
        }
        if let Some(sample) = sample {
            sample.graph_start(encoder);
        }
        let mut pending_timings = Vec::<PendingEffectTiming>::new();
        let mut encoded_nodes = 0_u64;
        let mut encoded_render_passes = 0_u64;
        let mut cache_inserts = Vec::new();
        let timing = self.pass_timing_epoch();
        for (position, node_id) in plan.order.iter().copied().enumerate() {
            let started = timing.map(|epoch| (epoch, Instant::now()));
            if let Some(hit) = cached.hits[node_id.index].clone() {
                outputs[node_id.index] = Some(hit);
                release_inputs(graph, &plan.last_use, &mut outputs, node_id, position);
                continue;
            }
            if !cached.needed[node_id.index] {
                // A cache hit still releases its inputs where drawing would have.
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
                    let registered = prepared.registration(node_id);
                    if registered
                        .is_some_and(|registered| registered.definition.kind != EffectKind::Effect)
                    {
                        return Err(GraphError::EffectKindMismatch);
                    }
                    let source_view = graph_view(graph, &outputs, *source)?;
                    let input_views: Vec<(&str, &wgpu::TextureView)> = inputs
                        .iter()
                        .map(|(name, node)| Ok((name, graph_view(graph, &outputs, node)?)))
                        .collect::<Result<_, GraphError>>()?;
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph effect target",
                    );
                    if let Some(pending) = self.encode_effect_pass(
                        encoder,
                        registered,
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
                    let registered = prepared.registration(node_id);
                    if registered.is_some_and(|registered| {
                        registered.definition.kind != EffectKind::Transition
                    }) {
                        return Err(GraphError::EffectKindMismatch);
                    }
                    let from_view = graph_view(graph, &outputs, *from)?;
                    let to_view = graph_view(graph, &outputs, *to)?;
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph transition target",
                    );
                    if let Some(pending) = self.encode_effect_pass(
                        encoder,
                        registered,
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
                    let registered = prepared.registration(node_id);
                    if registered.is_some_and(|registered| {
                        registered.definition.kind != EffectKind::Generator
                    }) {
                        return Err(GraphError::EffectKindMismatch);
                    }
                    let target = self.textures.acquire(
                        self.gpu.device(),
                        graph.size,
                        format,
                        "alpha2 graph generator target",
                    );
                    // A generator samples nothing, but its bindings still need a texture.
                    if let Some(pending) = self.encode_effect_pass(
                        encoder,
                        registered,
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
                RenderNode::Vector { .. } => {
                    let scene = prepared
                        .vector(node_id)
                        .ok_or(GraphError::Render(crate::RenderError::InvalidGraph))?
                        .as_ref()
                        .map_err(|error| GraphError::Render(*error))?;
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
                            target.view(),
                            &mut uniforms,
                        )?);
                    Some(RenderedTexture::pooled(graph.size, format, target))
                }
                RenderNode::Composite { layers, background } => {
                    let prepared_composite = prepared
                        .composite(node_id)
                        .ok_or(GraphError::Render(crate::RenderError::InvalidGraph))?;
                    let views = layers
                        .iter()
                        .map(|layer| graph_view(graph, &outputs, layer.source))
                        .collect::<Result<Vec<_>, _>>()?;
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
                            prepared_composite,
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
            cache_clears: cached.clears,
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
            cache_clears,
            uniforms,
            pending_timings,
            encoded_nodes,
            encoded_render_passes,
        } = encoded;
        uniforms.recycle();
        if !cache_inserts.is_empty() {
            let mut cache = self.lock_result_cache();
            // A clear since the lookup may have changed what these results should show.
            if cache.clears() == cache_clears {
                for (key, result, pinned) in &cache_inserts {
                    cache.insert(*key, result, *pinned);
                }
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

fn planned_passes<'a>(
    graph: &RenderGraph<'_>,
    drawn: impl Iterator<Item = &'a crate::graph::RenderNodeId>,
    prepared: &crate::prepared_frame::PreparedFrame,
    sources: &[SourceWorkTicket<'_>],
    pack: FramePack,
) -> Vec<(crate::FrameCost, Option<alpha2_gpu::PassKey>)> {
    let frame_pixels = graph.size.pixels();
    let mut passes = vec![(
        pack.work(graph.size),
        Some(PassKind::Pack.key(frame_pixels)),
    )];
    passes.extend(sources.iter().map(|work| {
        (
            work.work(),
            Some(PassKind::Conversion.key(work.output_size().pixels())),
        )
    }));
    passes.extend(drawn.filter_map(|node| match prepared.node(*node) {
        PreparedNode::Effect(Some(registered)) => Some((
            crate::FrameCost::of(registered.cost, frame_pixels),
            Some(alpha2_gpu::PassKey::new(registered.pass_id, frame_pixels)),
        )),
        PreparedNode::Composite(composite) => Some((composite.work(), Some(composite.pass))),
        PreparedNode::Vector {
            scene: Some(Ok(scene)),
        } => Some((scene.work(), Some(PassKind::Vector.key(frame_pixels)))),
        _ => None,
    }));
    passes
}

fn node_work(node: &RenderNode<'_>, prepared: &PreparedNode, pixels: u64) -> crate::FrameCost {
    match (node, prepared) {
        (_, PreparedNode::Effect(Some(registered))) => {
            crate::FrameCost::of(registered.cost, pixels)
        }
        (_, PreparedNode::Composite(composite)) => composite.work(),
        (
            _,
            PreparedNode::Vector {
                scene: Some(Ok(scene)),
            },
        ) => scene.work(),
        _ => crate::FrameCost::default(),
    }
}

impl Renderer {
    fn texture_need(
        &self,
        graph: &RenderGraph<'_>,
        plan: &crate::RenderPlan,
        prepared: &crate::prepared_frame::PreparedFrame,
        cached: &CachedResults,
        sources: &[SourceWorkTicket<'_>],
        pack: FramePack,
    ) -> u64 {
        let texel_bytes = match self.intermediate_format() {
            wgpu::TextureFormat::Rgba16Float => 8,
            _ => 4,
        };
        let frame = graph.size.pixels() * texel_bytes;
        let fixed = sources
            .iter()
            .map(|work| work.output_size().pixels() * texel_bytes)
            .sum::<u64>()
            + match pack {
                FramePack::Yuv420p | FramePack::Nv12 => graph.size.pixels() * 3 / 2,
                FramePack::Layout(layout) => {
                    layout.frame_bytes(graph.size.width, graph.size.height)
                }
                FramePack::Nothing => 0,
            };
        let mut held = vec![0_u64; graph.nodes.len()];
        let (mut alive, mut peak) = (0_u64, 0_u64);
        for (position, node) in plan.order.iter().enumerate() {
            let index = node.index;
            if matches!(graph.nodes[index], RenderNode::Source(_)) {
                continue;
            }
            if cached.hits[index].is_some() || cached.needed[index] {
                let scratch = match prepared.node(*node) {
                    PreparedNode::Composite(composite) if composite.batches.len() > 1 => frame,
                    PreparedNode::Vector {
                        scene: Some(Ok(scene)),
                    } => scene.texture_bytes(graph.size, texel_bytes),
                    _ => 0,
                };
                peak = peak.max(alive + frame + scratch);
                held[index] = frame;
                alive += frame;
            }
            for dependency in graph.nodes[index].dependencies() {
                if plan.last_use[dependency.index] == Some(position) {
                    alive -= std::mem::take(&mut held[dependency.index]);
                }
            }
        }
        fixed + peak
    }
}

/// The texture a dependency is drawn from; `None` for a CPU source.
fn graph_view<'a>(
    graph: &'a RenderGraph<'_>,
    outputs: &'a [Option<RenderedTexture>],
    node: RenderNodeId,
) -> Result<&'a wgpu::TextureView, GraphError> {
    match &graph.nodes[node.index] {
        RenderNode::Source(view) => view
            .wgpu_view()
            .ok_or(GraphError::Render(crate::RenderError::InvalidSource)),
        _ => Ok(outputs[node.index]
            .as_ref()
            .expect("compiled dependency has an output")
            .view()),
    }
}

/// Returns to the pool the results `node` was the last to read.
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

/// Cache hits, results to keep, and nodes still to draw for one graph.
struct CachedResults {
    hits: Vec<Option<RenderedTexture>>,
    store: Vec<Option<(RenderCacheKey, bool)>>,
    needed: Vec<bool>,
    /// The cache's clear count read with the lookups.
    clears: u64,
}

impl Renderer {
    /// Looks up cache candidates and marks which nodes hits make unnecessary. With the cache off or
    /// no candidates, everything is drawn and nothing kept.
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
            clears: 0,
        };
        if graph.intents.iter().all(Option::is_none) {
            return cached;
        }
        let mut cache = self.lock_result_cache();
        if !cache.enabled() {
            return cached;
        }
        cached.clears = cache.clears();
        let eligibility = graph.eligibility();
        let (mut hits, mut misses, mut ineligible) = (0, 0, 0);
        // From the output down: look a node up only if still needed; a hit's inputs are then needed
        // only by others.
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
