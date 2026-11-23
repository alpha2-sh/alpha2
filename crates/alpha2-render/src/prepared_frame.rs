//! One frame's graph resolved once, before anything is encoded.

use crate::composite::{LayerTier, PixelRect};
use crate::graph::{RenderGraph, RenderNode, RenderNodeId};
use crate::registry::RegisteredEffect;
use crate::uniform::{
    BATCH_UNIFORM_FLOATS, RUNTIME_UNIFORM_WORDS, aligned_uniform_end, float_uniform_bytes,
    param_uniform_bytes,
};
use crate::yuv::SourceWorkTicket;
use crate::{RenderSize, Renderer, Shared};

pub(crate) struct PreparedFrame {
    nodes: Vec<PreparedNode>,
}

pub(crate) enum PreparedNode {
    Nothing,
    Effect(Option<Shared<RegisteredEffect>>),
    Composite(PreparedComposite),
    Vector {
        uniform_slices: usize,
        scene: Option<Result<Box<crate::vector::PreparedVector>, crate::RenderError>>,
    },
}

pub(crate) struct PreparedComposite {
    pub(crate) drawn: Vec<(usize, PixelRect, LayerTier)>,
    pub(crate) batches: Vec<PreparedBatch>,
    pub(crate) pass: alpha2_gpu::PassKey,
}

pub(crate) struct PreparedBatch {
    pub(crate) layers: std::ops::Range<usize>,
    pub(crate) bounds: PixelRect,
    pub(crate) region: PixelRect,
    pub(crate) work: crate::FrameCost,
}

impl PreparedComposite {
    pub(crate) fn work(&self) -> crate::FrameCost {
        self.batches
            .iter()
            .fold(crate::FrameCost::default(), |mut total, batch| {
                total.add_cost(batch.work);
                total
            })
    }
}

impl PreparedFrame {
    pub(crate) fn node(&self, node: RenderNodeId) -> &PreparedNode {
        &self.nodes[node.index]
    }

    pub(crate) fn registration(&self, node: RenderNodeId) -> Option<&Shared<RegisteredEffect>> {
        match self.node(node) {
            PreparedNode::Effect(registered) => registered.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn vector(
        &self,
        node: RenderNodeId,
    ) -> Option<&Result<Box<crate::vector::PreparedVector>, crate::RenderError>> {
        match self.node(node) {
            PreparedNode::Vector { scene, .. } => scene.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn prepare_vectors<'a>(
        &mut self,
        renderer: &Renderer,
        graph: &RenderGraph<'_>,
        drawn: impl Iterator<Item = &'a RenderNodeId>,
    ) {
        let blend_space = graph.blend_space.unwrap_or(renderer.blend_space);
        for node in drawn {
            if let (
                RenderNode::Vector { scene },
                PreparedNode::Vector {
                    scene: prepared, ..
                },
            ) = (&graph.nodes[node.index], &mut self.nodes[node.index])
            {
                *prepared = Some(
                    renderer
                        .prepare_vector(scene, graph.size, blend_space)
                        .map(Box::new),
                );
            }
        }
    }

    pub(crate) fn composite(&self, node: RenderNodeId) -> Option<&PreparedComposite> {
        match self.node(node) {
            PreparedNode::Composite(composite) => Some(composite),
            _ => None,
        }
    }

    pub(crate) fn uniform_capacity(
        &self,
        sources: &[SourceWorkTicket<'_>],
        order: &[RenderNodeId],
        pack: FramePack,
        alignment: u64,
    ) -> u64 {
        let cursor = source_uniform_end(0, sources, alignment);
        let cursor = order
            .iter()
            .fold(cursor, |cursor, node| match self.node(*node) {
                PreparedNode::Nothing => cursor,
                PreparedNode::Effect(registered) => effect_uniform_end(
                    cursor,
                    registered
                        .as_ref()
                        .map_or(0, |registered| registered.definition.params.len()),
                    alignment,
                ),
                PreparedNode::Composite(composite) => {
                    (0..composite.batches.len()).fold(cursor, |cursor, _| {
                        aligned_uniform_end(
                            cursor,
                            float_uniform_bytes(BATCH_UNIFORM_FLOATS),
                            alignment,
                        )
                    })
                }
                PreparedNode::Vector { uniform_slices, .. } => {
                    (0..*uniform_slices).fold(cursor, |cursor, _| {
                        aligned_uniform_end(cursor, crate::vector::VECTOR_UNIFORM_BYTES, alignment)
                    })
                }
            });
        match pack {
            FramePack::Nothing | FramePack::Texels => cursor,
            FramePack::Yuv420p | FramePack::Nv12 => {
                aligned_uniform_end(cursor, crate::readback::YUV_PACK_UNIFORM_BYTES, alignment)
            }
        }
    }
}

impl FramePack {
    pub(crate) fn work(self, size: RenderSize) -> crate::FrameCost {
        match self {
            Self::Nothing => crate::FrameCost::default(),
            Self::Yuv420p | Self::Nv12 => {
                let mut work = crate::readback::pack_luma_work(size);
                work.add_cost(crate::readback::pack_chroma_work(size));
                work
            }
            Self::Texels => crate::readback::texel_copy_work(size),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum FramePack {
    Nothing,
    Yuv420p,
    Nv12,
    Texels,
}

pub(crate) fn composite_uniform_capacity(composite: &PreparedComposite, alignment: u64) -> u64 {
    (0..composite.batches.len()).fold(0, |cursor, _| {
        aligned_uniform_end(cursor, float_uniform_bytes(BATCH_UNIFORM_FLOATS), alignment)
    })
}

pub(crate) fn source_uniform_capacity(sources: &[SourceWorkTicket<'_>], alignment: u64) -> u64 {
    source_uniform_end(0, sources, alignment)
}

fn source_uniform_end(cursor: u64, sources: &[SourceWorkTicket<'_>], alignment: u64) -> u64 {
    sources.iter().fold(cursor, |cursor, work| {
        aligned_uniform_end(cursor, work.uniform_bytes(), alignment)
    })
}

impl Renderer {
    pub(crate) fn prepare_frame(
        &self,
        graph: &RenderGraph<'_>,
        order: &[RenderNodeId],
    ) -> PreparedFrame {
        let mut nodes = (0..graph.nodes.len())
            .map(|_| PreparedNode::Nothing)
            .collect::<Vec<_>>();
        for node in order {
            nodes[node.index] = match &graph.nodes[node.index] {
                RenderNode::Source(_) => PreparedNode::Nothing,
                RenderNode::Effect { effect, .. }
                | RenderNode::Transition { effect, .. }
                | RenderNode::Generator { effect, .. } => {
                    PreparedNode::Effect(self.effects().get(effect))
                }
                RenderNode::Composite { layers, .. } => PreparedNode::Composite(
                    self.prepare_composite(
                        layers
                            .iter()
                            .map(|layer| (layer.visual, layer_texture_size(graph, layer.source))),
                        graph.size,
                    ),
                ),
                RenderNode::Vector { scene } => PreparedNode::Vector {
                    uniform_slices: scene
                        .ops
                        .iter()
                        .filter(|op| !matches!(op, alpha2_geometry::VectorOp::PushLayer { .. }))
                        .count()
                        + 1,
                    scene: None,
                },
            };
        }
        PreparedFrame { nodes }
    }

    pub(crate) fn prepare_composite(
        &self,
        layers: impl Iterator<Item = (crate::LayerTransform, RenderSize)> + Clone,
        size: RenderSize,
    ) -> PreparedComposite {
        let drawn = self.drawn_layers(layers.clone().map(|(visual, _)| visual), size);
        let textures = layers.collect::<Vec<_>>();
        let max_filter_samples = self.quality_controls().max_filter_samples;
        let batches = crate::composite::composite_batches(&drawn, self.composite_batch_layers())
            .into_iter()
            .map(|(range, bounds)| {
                let region = crate::composite::batch_region(bounds, size);
                let mut work =
                    crate::FrameCost::of(crate::composite::COMPOSITE_PASS_COST, region.area());
                for (index, layer_bounds, _) in &drawn[range.clone()] {
                    let (visual, texture) = textures[*index];
                    let samples =
                        crate::composite::layer_samples(visual, texture, size, max_filter_samples);
                    work.add_cost(crate::FrameCost::of(
                        crate::composite::COMPOSITE_SAMPLE_COST
                            .times(samples)
                            .plus(crate::composite::COMPOSITE_BLEND_COST),
                        layer_bounds.intersection(region).area(),
                    ));
                }
                PreparedBatch {
                    layers: range,
                    bounds,
                    region,
                    work,
                }
            })
            .collect();
        PreparedComposite {
            drawn,
            batches,
            pass: crate::submission_pilot::PassKind::Composite.key(size.pixels()),
        }
    }
}

fn layer_texture_size(graph: &RenderGraph<'_>, node: RenderNodeId) -> RenderSize {
    match &graph.nodes[node.index] {
        RenderNode::Source(view) => view.wgpu_view().map_or(graph.size, |view| RenderSize {
            width: view.texture().width(),
            height: view.texture().height(),
        }),
        _ => graph.size,
    }
}

fn effect_uniform_end(cursor: u64, params: usize, alignment: u64) -> u64 {
    let parameter_bytes = u64::try_from(param_uniform_bytes(params)).unwrap_or(u64::MAX);
    let cursor = aligned_uniform_end(cursor, parameter_bytes, alignment);
    aligned_uniform_end(
        cursor,
        float_uniform_bytes(RUNTIME_UNIFORM_WORDS),
        alignment,
    )
}

pub(crate) fn effect_uniform_capacity(params: usize, alignment: u64) -> u64 {
    effect_uniform_end(0, params, alignment)
}
