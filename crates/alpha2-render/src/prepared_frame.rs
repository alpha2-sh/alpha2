//! One frame's graph resolved once, before anything is encoded.

use crate::composite::{LayerTier, PixelRect};
use crate::graph::{RenderGraph, RenderNode, RenderNodeId};
use crate::registry::RegisteredEffect;
use crate::uniform::{
    BATCH_UNIFORM_FLOATS, RUNTIME_UNIFORM_WORDS, aligned_uniform_end, float_uniform_bytes,
    param_uniform_bytes,
};
use crate::{RenderSize, Renderer, Shared};

pub(crate) struct PreparedFrame {
    nodes: Vec<PreparedNode>,
}

pub(crate) enum PreparedNode {
    Nothing,
    Effect(Option<Shared<RegisteredEffect>>),
    Composite(PreparedComposite),
    Vector { uniform_slices: usize },
}

pub(crate) struct PreparedComposite {
    pub(crate) drawn: Vec<(usize, PixelRect, LayerTier)>,
    pub(crate) batches: Vec<(std::ops::Range<usize>, PixelRect)>,
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

    pub(crate) fn composite(&self, node: RenderNodeId) -> Option<&PreparedComposite> {
        match self.node(node) {
            PreparedNode::Composite(composite) => Some(composite),
            _ => None,
        }
    }

    pub(crate) fn uniform_capacity(&self, order: &[RenderNodeId], alignment: u64) -> u64 {
        order.iter().fold(0, |cursor, node| match self.node(*node) {
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
            PreparedNode::Vector { uniform_slices } => {
                (0..*uniform_slices).fold(cursor, |cursor, _| {
                    aligned_uniform_end(cursor, crate::vector::VECTOR_UNIFORM_BYTES, alignment)
                })
            }
        })
    }
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
                    self.prepare_composite(layers.iter().map(|layer| layer.visual), graph.size),
                ),
                RenderNode::Vector { scene } => PreparedNode::Vector {
                    uniform_slices: scene
                        .ops
                        .iter()
                        .filter(|op| !matches!(op, alpha2_geometry::VectorOp::PushLayer { .. }))
                        .count()
                        + 1,
                },
            };
        }
        PreparedFrame { nodes }
    }

    pub(crate) fn prepare_composite(
        &self,
        visuals: impl Iterator<Item = crate::LayerTransform>,
        size: RenderSize,
    ) -> PreparedComposite {
        let drawn = self.drawn_layers(visuals, size);
        let batches = crate::composite::composite_batches(&drawn, self.composite_batch_layers());
        PreparedComposite { drawn, batches }
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
