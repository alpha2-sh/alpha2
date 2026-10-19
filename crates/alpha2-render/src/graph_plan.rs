use crate::RenderQualityControls;
use crate::graph::{GraphError, RenderCacheKey, RenderGraph, RenderNode, RenderNodeId};

/// Compiled execution and lifetime schedule for a render graph.
#[derive(Clone, Debug)]
pub struct RenderPlan {
    pub(crate) graph: u32,
    pub(crate) order: Vec<RenderNodeId>,
    pub(crate) last_use: Vec<Option<usize>>,
    pub(crate) output: RenderNodeId,
    pub(crate) cache_key: RenderCacheKey,
    pub(crate) filter_sample_limits: Vec<Option<u32>>,
    pub(crate) temporal_offsets: Vec<f32>,
}

impl RenderPlan {
    /// Reachable nodes in deterministic insertion order.
    #[must_use]
    pub fn execution_order(&self) -> &[RenderNodeId] {
        &self.order
    }

    /// Returns the execution position after which a node can be released.
    #[must_use]
    pub fn last_use(&self, node: RenderNodeId) -> Option<usize> {
        (node.graph == self.graph)
            .then(|| self.last_use.get(node.index).copied().flatten())
            .flatten()
    }

    /// Returns the structural cache key of the selected output.
    #[must_use]
    pub const fn cache_key(&self) -> RenderCacheKey {
        self.cache_key
    }

    /// Quality-controlled filter sample limit for one node.
    #[must_use]
    pub fn filter_sample_limit(&self, node: RenderNodeId) -> Option<u32> {
        (node.graph == self.graph)
            .then(|| self.filter_sample_limits.get(node.index).copied().flatten())
            .flatten()
    }

    /// Centered, normalized temporal sampling offsets for motion-aware runners.
    #[must_use]
    pub fn temporal_offsets(&self) -> &[f32] {
        &self.temporal_offsets
    }
}

impl RenderGraph<'_> {
    /// Compiles reachable nodes and their final-use positions.
    ///
    /// # Errors
    /// Returns [`GraphError::MissingOutput`] when no output was selected.
    pub fn compile(&self) -> Result<RenderPlan, GraphError> {
        self.compile_with_quality(RenderQualityControls::default())
    }

    /// Compiles with explicit filter and temporal sampling controls.
    ///
    /// # Errors
    /// Returns [`GraphError::MissingOutput`] when no output was selected.
    pub fn compile_with_quality(
        &self,
        quality: RenderQualityControls,
    ) -> Result<RenderPlan, GraphError> {
        let output = self.output.ok_or(GraphError::MissingOutput)?;
        let mut reachable = vec![false; self.nodes.len()];
        mark_reachable(output, &self.nodes, &mut reachable);
        let order = reachable
            .iter()
            .enumerate()
            .filter(|(_, is_reachable)| **is_reachable)
            .map(|(index, _)| RenderNodeId {
                graph: self.id,
                index,
            })
            .collect::<Vec<_>>();
        let mut last_use = vec![None; self.nodes.len()];
        for (position, node) in order.iter().enumerate() {
            for dependency in self.nodes[node.index].dependencies() {
                last_use[dependency.index] = Some(position);
            }
        }
        last_use[output.index] = Some(order.len());
        let filter_sample_limits = self
            .nodes
            .iter()
            .map(|node| match node {
                RenderNode::Effect { .. }
                | RenderNode::Transition { .. }
                | RenderNode::Generator { .. } => Some(quality.max_filter_samples.max(1)),
                RenderNode::Source(_) | RenderNode::Composite { .. } => None,
            })
            .collect();
        let count = quality.motion_blur_subframes.max(1);
        #[allow(clippy::cast_precision_loss)]
        let temporal_offsets = (0..count)
            .map(|index| (index as f32 + 0.5) / count as f32 - 0.5)
            .collect();
        Ok(RenderPlan {
            graph: self.id,
            order,
            last_use,
            output,
            cache_key: self.cache_keys[output.index],
            filter_sample_limits,
            temporal_offsets,
        })
    }
}

fn mark_reachable(node: RenderNodeId, nodes: &[RenderNode<'_>], reachable: &mut [bool]) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if reachable[node.index] {
            continue;
        }
        reachable[node.index] = true;
        pending.extend(nodes[node.index].dependencies());
    }
}
