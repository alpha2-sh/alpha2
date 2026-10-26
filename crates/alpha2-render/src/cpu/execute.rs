//! Running a render graph on the CPU: the GPU executor's walk
//! (`graph_execute.rs`), with each node drawn into a [`CpuImage`] by bands of
//! rows.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use alpha2_media::YuvEncoding;
use web_time::Instant;

use super::CpuRenderer;
use super::composite::{CompositeSettings, LayerInput, composite};
use super::half;
use super::image::{CpuImage, PixelBox, Pixels};
use super::kernels::{CpuLut, KernelContext};
use crate::graph::{RenderGraph, RenderNode, RenderNodeId};
use crate::graph_cache::renderer_node_key;
use crate::{
    CacheEligibility, CacheIntent, EffectId, EffectKind, EffectParams, EffectResources,
    EffectRuntime, GraphError, RenderCacheKey, RenderError, Yuv420pTargets,
};

/// What one node reads and how it is drawn, for the kernel nodes.
struct KernelNode<'a> {
    effect: &'a EffectId,
    kind: EffectKind,
    source_a: Option<RenderNodeId>,
    source_b: Option<RenderNodeId>,
    inputs: Vec<(&'a str, RenderNodeId)>,
    params: &'a EffectParams<'a>,
    resources: &'a EffectResources,
    runtime: EffectRuntime,
}

impl CpuRenderer {
    /// Draws the reachable part of `graph` and returns its output.
    ///
    /// # Errors
    /// Returns a structural error or the first pass's failure:
    /// [`RenderError::UnknownEffect`] for an effect with no CPU kernel and
    /// [`RenderError::InvalidSource`] for a GPU source.
    #[allow(clippy::too_many_lines)]
    pub fn execute_graph(&self, graph: &RenderGraph<'_>) -> Result<Arc<CpuImage>, GraphError> {
        let started = Instant::now();
        let quality = self.quality_controls();
        let plan = graph.compile_with_quality(quality)?;
        if matches!(graph.nodes[plan.output.index], RenderNode::Source(_)) {
            return Err(GraphError::SourceOutput);
        }
        let blend_space = graph.blend_space.unwrap_or(self.blend_space);
        let cached =
            self.cached_results(graph, &plan.order, plan.output, blend_space.shader_value());
        let mut outputs: Vec<Option<Arc<CpuImage>>> = vec![None; graph.nodes.len()];
        let mut inserts = Vec::new();
        let mut drawn = 0_u64;
        let mut high_water = 0_u64;
        for (position, node) in plan.order.iter().copied().enumerate() {
            if let Some(hit) = cached.hits[node.index].clone() {
                outputs[node.index] = Some(hit);
                release_inputs(graph, &plan.last_use, &mut outputs, node, position);
                continue;
            }
            if !cached.needed[node.index] {
                release_inputs(graph, &plan.last_use, &mut outputs, node, position);
                continue;
            }
            let image = match &graph.nodes[node.index] {
                RenderNode::Source(view) => {
                    let source = view.cpu_source().ok_or(RenderError::InvalidSource)?;
                    source.presented(self)
                }
                RenderNode::Effect {
                    effect,
                    source,
                    inputs,
                    params,
                    resources,
                    runtime,
                } => Arc::new(self.draw_kernel(
                    graph,
                    &outputs,
                    &KernelNode {
                        effect,
                        kind: EffectKind::Effect,
                        source_a: Some(*source),
                        source_b: None,
                        inputs: inputs.iter().collect(),
                        params,
                        resources,
                        runtime: *runtime,
                    },
                )?),
                RenderNode::Transition {
                    effect,
                    from,
                    to,
                    params,
                    resources,
                    runtime,
                } => Arc::new(self.draw_kernel(
                    graph,
                    &outputs,
                    &KernelNode {
                        effect,
                        kind: EffectKind::Transition,
                        source_a: Some(*from),
                        source_b: Some(*to),
                        inputs: Vec::new(),
                        params,
                        resources,
                        runtime: *runtime,
                    },
                )?),
                RenderNode::Generator {
                    effect,
                    params,
                    resources,
                    runtime,
                } => Arc::new(self.draw_kernel(
                    graph,
                    &outputs,
                    &KernelNode {
                        effect,
                        kind: EffectKind::Generator,
                        source_a: None,
                        source_b: None,
                        inputs: Vec::new(),
                        params,
                        resources,
                        runtime: *runtime,
                    },
                )?),
                RenderNode::Vector { scene } => Arc::new(super::vector::draw(
                    &self.workers,
                    &self.coverage,
                    scene,
                    graph.size,
                    blend_space,
                )?),
                RenderNode::Composite { layers, background } => {
                    let inputs = layers
                        .iter()
                        .map(|layer| LayerInput {
                            image: output(&outputs, layer.source),
                            visual: layer.visual,
                            blend_mode: layer.blend_mode,
                        })
                        .collect::<Vec<_>>();
                    Arc::new(composite(
                        &self.workers,
                        &inputs,
                        CompositeSettings {
                            size: graph.size,
                            background: *background,
                            blend_space,
                            max_filter_samples: quality.max_filter_samples,
                        },
                    )?)
                }
            };
            if !matches!(graph.nodes[node.index], RenderNode::Source(_)) {
                drawn += 1;
            }
            if let Some((key, pinned)) = cached.store[node.index] {
                inserts.push((key, Arc::clone(&image), pinned));
            }
            outputs[node.index] = Some(image);
            high_water = high_water.max(
                outputs
                    .iter()
                    .flatten()
                    .map(|image| image.retained_bytes())
                    .sum(),
            );
            release_inputs(graph, &plan.last_use, &mut outputs, node, position);
        }
        let image = outputs[plan.output.index]
            .take()
            .ok_or(GraphError::SourceOutput)?;
        if !inserts.is_empty() {
            let mut cache = self.lock_cache();
            for (key, image, pinned) in &inserts {
                cache.insert(*key, image, *pinned);
            }
        }
        self.stats.graphs.fetch_add(1, Ordering::Relaxed);
        self.stats.nodes.fetch_add(drawn, Ordering::Relaxed);
        self.stats.render_micros.fetch_add(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.stats
            .image_high_water_bytes
            .fetch_max(high_water, Ordering::Relaxed);
        Ok(image)
    }

    /// Draws `graph` and packs its output into `targets` as YUV420 with
    /// `encoding`: the CPU counterpart of
    /// [`crate::Renderer::render_graph_yuv420p`], writing the caller's plane
    /// memory directly.
    ///
    /// # Errors
    /// Returns [`Self::execute_graph`]'s errors, and
    /// [`RenderError::TextureReadback`] for an odd size or a target too small.
    pub fn render_graph_into_yuv420p(
        &self,
        graph: &RenderGraph<'_>,
        encoding: YuvEncoding,
        targets: &mut Yuv420pTargets<'_>,
    ) -> Result<(), RenderError> {
        let image = self
            .execute_graph(graph)
            .map_err(GraphError::into_render_error)?;
        let started = Instant::now();
        super::pack::pack_yuv420p(&self.workers, &image, encoding, targets)?;
        self.stats.render_micros.fetch_add(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn draw_kernel(
        &self,
        graph: &RenderGraph<'_>,
        outputs: &[Option<Arc<CpuImage>>],
        node: &KernelNode<'_>,
    ) -> Result<CpuImage, GraphError> {
        let entry = self.entry(node.effect).ok_or(RenderError::UnknownEffect)?;
        let definition = &entry.definition;
        if definition.kind != node.kind {
            return Err(GraphError::EffectKindMismatch);
        }
        crate::runtime_effect::validate_runtime(node.runtime)?;
        crate::runtime_effect::encode_params(&definition.params, node.params)?;
        if node
            .resources
            .iter()
            .any(|(name, _)| !definition.resources.iter().any(|spec| spec.name == name))
        {
            return Err(RenderError::InvalidEffectParams.into());
        }
        let mut resources: Vec<(&str, &CpuLut)> = Vec::with_capacity(definition.resources.len());
        for spec in &definition.resources {
            let id = node
                .resources
                .get(&spec.name)
                .ok_or(RenderError::MissingEffectResource)?;
            let lut = self
                .resource(id)
                .ok_or(RenderError::MissingEffectResource)?;
            resources.push((spec.name.as_str(), lut));
        }
        if node
            .inputs
            .iter()
            .any(|(name, _)| !definition.inputs.iter().any(|spec| spec.name == *name))
        {
            return Err(RenderError::InvalidEffectParams.into());
        }
        let mut inputs = Vec::with_capacity(definition.inputs.len());
        for spec in &definition.inputs {
            let (_, input) = node
                .inputs
                .iter()
                .find(|(name, _)| *name == spec.name)
                .ok_or(RenderError::MissingEffectInput)?;
            inputs.push((spec.name.as_str(), output(outputs, *input)));
        }
        let transparent = CpuImage::transparent(graph.size);
        let source_a = node.source_a.map_or(&transparent, |id| output(outputs, id));
        let source_b = node.source_b.map_or(source_a, |id| output(outputs, id));
        let context = KernelContext {
            size: graph.size,
            definition,
            params: node.params,
            runtime: node.runtime,
            max_filter_samples: self.quality_controls().max_filter_samples,
            source_a,
            source_b,
            inputs: &inputs,
            resources: &resources,
        };
        let kernel = entry.kernel;
        // A kernel's bounds are in its inputs' pixels, which are the output's only when the
        // inputs are the output's size. A source presented at another size (a preview drawn
        // smaller than the project its frames were fitted to) is sampled in uv like any other
        // input, and can reach every pixel.
        let matching = [source_a, source_b]
            .into_iter()
            .chain(inputs.iter().map(|(_, image)| *image))
            .all(|image| image.size() == graph.size);
        let bounds = if matching {
            kernel.bounds(&context)
        } else {
            PixelBox::whole(graph.size)
        }
        .intersect(PixelBox::whole(graph.size));
        if bounds.is_empty() {
            return Ok(CpuImage::transparent(graph.size));
        }
        let width = bounds.width as usize;
        let mut pixels = vec![[0_u16; 4]; bounds.pixels()];
        self.workers.bands(&mut pixels, width, |first_row, band| {
            let mut row = vec![[0.0_f32; 4]; width];
            for (index, line) in band.chunks_mut(width).enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let y = bounds.y + (first_row + index) as u32;
                kernel.row(&context, y, bounds.x, &mut row);
                for (out, value) in line.iter_mut().zip(&row) {
                    *out = half::store4(*value);
                }
            }
        });
        Ok(CpuImage::new(graph.size, bounds, Pixels::Half(pixels)))
    }

    /// The graph's result-cache lookups, as the GPU executor's
    /// `cached_results`.
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
        let mut cache = self.lock_cache();
        if !cache.enabled() {
            return cached;
        }
        let eligibility = graph.eligibility();
        let (mut hits, mut misses, mut ineligible) = (0, 0, 0);
        cached.needed = vec![false; count];
        cached.needed[output.index] = true;
        for node in order.iter().rev() {
            if !cached.needed[node.index] {
                continue;
            }
            if let Some(intent) = graph.intents[node.index] {
                if eligibility[node.index] == CacheEligibility::Eligible {
                    let key = renderer_node_key(graph.cache_keys[node.index], blend_space);
                    let pinned = intent == CacheIntent::Pin;
                    if let Some(image) = cache.get(key, pinned) {
                        cached.hits[node.index] = Some(image);
                        hits += 1;
                        continue;
                    }
                    cached.store[node.index] = Some((key, pinned));
                    misses += 1;
                } else if intent != CacheIntent::Bypass {
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

struct CachedResults {
    hits: Vec<Option<Arc<CpuImage>>>,
    store: Vec<Option<(RenderCacheKey, bool)>>,
    needed: Vec<bool>,
}

fn output(outputs: &[Option<Arc<CpuImage>>], node: RenderNodeId) -> &CpuImage {
    outputs[node.index]
        .as_deref()
        .expect("compiled dependency has an output")
}

/// Drops the results `node` was the last to read.
fn release_inputs(
    graph: &RenderGraph<'_>,
    last_use: &[Option<usize>],
    outputs: &mut [Option<Arc<CpuImage>>],
    node: RenderNodeId,
    position: usize,
) {
    for dependency in graph.nodes[node.index].dependencies() {
        if last_use[dependency.index] == Some(position) {
            outputs[dependency.index] = None;
        }
    }
}
