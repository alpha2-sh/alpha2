use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::graph_cache::{
    composite_cache_key, effect_cache_key_with_resources, named_inputs_key, transient_source_key,
    vector_cache_key,
};
use crate::{
    BlendMode, Color, EffectId, EffectParams, EffectResources, EffectRuntime, LayerTransform,
    RenderError, RenderSize, SourceView,
};

/// Stable handle to a node within one render graph.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenderNodeId {
    pub(crate) graph: u32,
    pub(crate) index: usize,
}

impl RenderNodeId {
    /// Returns the node's insertion-order index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.index
    }
}

/// Public category of a render graph node.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RenderNodeKind {
    /// Externally owned input texture.
    Source,
    /// Registered effect: one source, and the named inputs its definition
    /// declares.
    Effect,
    /// Registered two-input transition.
    Transition,
    /// Registered zero-input generator.
    Generator,
    /// Ordered layer composition.
    Composite,
    /// A vector scene: fills drawn from tile coverage.
    Vector,
}

/// Named images one effect node samples besides its source, by the names its
/// definition declares. Kept sorted by name, one node per name; a name is
/// borrowed for `'a` or owned, as [`EffectParams`] names are.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectInputs<'a> {
    values: Vec<(Cow<'a, str>, RenderNodeId)>,
}

impl<'a> EffectInputs<'a> {
    /// Creates an empty set, without allocating.
    #[must_use]
    pub const fn new() -> Self {
        Self { values: Vec::new() }
    }

    /// Adds or replaces one named input.
    #[must_use]
    pub fn with(mut self, name: impl Into<Cow<'a, str>>, node: RenderNodeId) -> Self {
        self.insert(name, node);
        self
    }

    /// Adds or replaces one named input in place.
    pub fn insert(&mut self, name: impl Into<Cow<'a, str>>, node: RenderNodeId) {
        let name = name.into();
        match self
            .values
            .binary_search_by(|(held, _)| held.as_ref().cmp(name.as_ref()))
        {
            Ok(index) => self.values[index].1 = node,
            Err(index) => self.values.insert(index, (name, node)),
        }
    }

    /// Whether no input is named.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The number of named inputs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Every input by name, in name order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, RenderNodeId)> {
        self.values
            .iter()
            .map(|(name, node)| (name.as_ref(), *node))
    }
}

/// One owned layer reference in a graph composition node.
#[derive(Copy, Clone, Debug)]
pub struct GraphLayer {
    /// Node whose result is sampled as this layer.
    pub source: RenderNodeId,
    /// Layer transform and opacity.
    pub visual: LayerTransform,
    /// Artistic blend operation.
    pub blend_mode: BlendMode,
}

/// An insertion-ordered, acyclic render program.
#[derive(Debug)]
pub struct RenderGraph<'a> {
    pub(crate) id: u32,
    pub(crate) size: RenderSize,
    pub(crate) nodes: Vec<RenderNode<'a>>,
    pub(crate) cache_keys: Vec<RenderCacheKey>,
    /// Whether each node's key is complete: no source under it lacks a
    /// stable key.
    pub(crate) stable: Vec<bool>,
    /// Each node's cache intent; `None` for a node that is not a candidate.
    pub(crate) intents: Vec<Option<crate::CacheIntent>>,
    pub(crate) output: Option<RenderNodeId>,
    /// The space this graph's composites blend in; `None` takes the
    /// renderer's.
    pub(crate) blend_space: Option<crate::BlendSpace>,
}

static NEXT_GRAPH_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug)]
pub(crate) enum RenderNode<'a> {
    Source(&'a wgpu::TextureView),
    Effect {
        effect: EffectId,
        source: RenderNodeId,
        inputs: EffectInputs<'a>,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    },
    Transition {
        effect: EffectId,
        from: RenderNodeId,
        to: RenderNodeId,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    },
    Generator {
        effect: EffectId,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    },
    Composite {
        layers: Vec<GraphLayer>,
        background: Option<Color>,
    },
    Vector {
        scene: std::sync::Arc<alpha2_geometry::VectorScene>,
    },
}

/// Deterministic 128-bit key for a source or rendered graph result.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenderCacheKey(pub(crate) u128);

impl RenderCacheKey {
    /// Creates a stable source key supplied by a decode or asset cache.
    #[must_use]
    pub const fn source(value: u64) -> Self {
        Self(value as u128)
    }

    /// Creates a key from all 128 bits, as [`Self::value`] gives them back. A
    /// key a caller derives for something other than a decoded source sets
    /// high bits, so it never equals a [`Self::source`] key.
    #[must_use]
    pub const fn from_value(value: u128) -> Self {
        Self(value)
    }

    /// Returns the stable 128-bit representation.
    #[must_use]
    pub const fn value(self) -> u128 {
        self.0
    }
}

/// Structural or execution failure in a render graph.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GraphError {
    /// A node references an ID not already present in the graph.
    InvalidDependency,
    /// No final output node was selected.
    MissingOutput,
    /// An external source cannot be returned as an owned render result.
    SourceOutput,
    /// A registered shader kind does not match its graph node.
    EffectKindMismatch,
    /// A rendering operation failed.
    Render(RenderError),
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDependency => {
                formatter.write_str("render graph dependency must name an earlier node")
            }
            Self::MissingOutput => formatter.write_str("render graph has no selected output"),
            Self::SourceOutput => {
                formatter.write_str("render graph output must be a rendered node")
            }
            Self::EffectKindMismatch => {
                formatter.write_str("registered effect kind does not match its graph node")
            }
            Self::Render(error) => write!(formatter, "render graph execution failed: {error}"),
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Render(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RenderError> for GraphError {
    fn from(error: RenderError) -> Self {
        Self::Render(error)
    }
}

impl GraphError {
    /// Collapses a graph failure into the renderer's error type, reporting
    /// every structural fault as [`RenderError::InvalidGraph`].
    #[must_use]
    pub const fn into_render_error(self) -> RenderError {
        match self {
            Self::Render(error) => error,
            Self::InvalidDependency
            | Self::MissingOutput
            | Self::SourceOutput
            | Self::EffectKindMismatch => RenderError::InvalidGraph,
        }
    }
}

impl<'a> RenderGraph<'a> {
    /// Creates an empty graph whose rendered nodes share one output size.
    #[must_use]
    pub fn new(size: RenderSize) -> Self {
        Self {
            id: NEXT_GRAPH_ID.fetch_add(1, Ordering::Relaxed),
            size,
            nodes: Vec::new(),
            cache_keys: Vec::new(),
            stable: Vec::new(),
            intents: Vec::new(),
            output: None,
            blend_space: None,
        }
    }

    /// The size every rendered node of this graph has.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    /// Blends this graph's composites in `space` rather than in the
    /// renderer's default space. A timeline frame takes it from its project,
    /// so preview and export blend alike whatever renderer draws them.
    pub fn set_blend_space(&mut self, space: crate::BlendSpace) {
        self.blend_space = Some(space);
    }

    /// Adds an externally owned source texture.
    pub fn add_source(&mut self, texture: SourceView<'a>) -> RenderNodeId {
        let key = transient_source_key(self.id, self.nodes.len());
        self.push(RenderNode::Source(texture.wgpu_view()), key, false)
    }

    /// Adds an externally owned texture with a cross-frame stable cache key.
    ///
    /// The key is the caller's promise that the texture's pixels depend on
    /// nothing it leaves out: the source's content, the frame, and how it was
    /// decoded and converted. Results drawn from it can then be cached.
    pub fn add_source_with_key(
        &mut self,
        texture: SourceView<'a>,
        key: RenderCacheKey,
    ) -> RenderNodeId {
        self.push(RenderNode::Source(texture.wgpu_view()), key, true)
    }

    /// Adds a registered one-input effect node.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] unless `source` is earlier.
    pub fn add_effect(
        &mut self,
        effect: EffectId,
        source: RenderNodeId,
        params: EffectParams<'a>,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.add_effect_with_resources(effect, source, params, EffectResources::new(), runtime)
    }

    /// Adds a registered one-input effect node with external resources.
    ///
    /// The built-in Gaussian blur and glow are added as the internal passes
    /// that draw them, and the returned node is the last of those.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] unless `source` is earlier;
    /// for a built-in drawn as passes, also [`RenderError::InvalidEffectParams`]
    /// when its parameters are not ones it declares or are out of range.
    pub fn add_effect_with_resources(
        &mut self,
        effect: EffectId,
        source: RenderNodeId,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.add_effect_with_inputs(
            effect,
            source,
            EffectInputs::new(),
            params,
            resources,
            runtime,
        )
    }

    /// Adds a registered effect node that also samples `inputs`, bound by the
    /// names its definition declares.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] unless `source` and every
    /// input are earlier, [`RenderError::InvalidEffectParams`] for named
    /// inputs on a built-in drawn as passes, and otherwise the errors of
    /// [`Self::add_effect_with_resources`].
    pub fn add_effect_with_inputs(
        &mut self,
        effect: EffectId,
        source: RenderNodeId,
        inputs: EffectInputs<'a>,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.validate_dependency(source)?;
        for (_, input) in inputs.iter() {
            self.validate_dependency(input)?;
        }
        // Some built-ins are drawn as several internal passes; they bind no resources and
        // declare no inputs. Checked before they are expanded, so no node is left behind.
        if !inputs.is_empty() && crate::gaussian_blur::expanded_passes(effect.as_str()).is_some() {
            return Err(RenderError::InvalidEffectParams.into());
        }
        if let Some(expanded) = self.add_expanded_effect(&effect, source, &params, runtime) {
            if resources.iter().next().is_some() {
                return Err(RenderError::InvalidEffectParams.into());
            }
            return expanded;
        }
        let source_key = self.cache_keys[source.index];
        // Without named inputs the key is the one `add_effect_with_resources` gives.
        let input_keys: &[RenderCacheKey] = if inputs.is_empty() {
            &[source_key]
        } else {
            &[source_key, named_inputs_key(&inputs, &self.cache_keys)]
        };
        let key = effect_cache_key_with_resources(
            b"effect", self.size, &effect, input_keys, &params, &resources, runtime,
        );
        let stable =
            self.stable[source.index] && inputs.iter().all(|(_, input)| self.stable[input.index]);
        Ok(self.push(
            RenderNode::Effect {
                effect,
                source,
                inputs,
                params,
                resources,
                runtime,
            },
            key,
            stable,
        ))
    }

    /// Adds a registered generator node: a picture drawn from its
    /// parameters alone, at the graph's size.
    pub fn add_generator(
        &mut self,
        effect: EffectId,
        params: EffectParams<'a>,
        runtime: EffectRuntime,
    ) -> RenderNodeId {
        self.add_generator_with_resources(effect, params, EffectResources::new(), runtime)
    }

    /// Adds a registered generator node that samples external resources,
    /// bound by the names its definition declares, as an effect's are.
    pub fn add_generator_with_resources(
        &mut self,
        effect: EffectId,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    ) -> RenderNodeId {
        let key = effect_cache_key_with_resources(
            b"generator",
            self.size,
            &effect,
            &[],
            &params,
            &resources,
            runtime,
        );
        self.push(
            RenderNode::Generator {
                effect,
                params,
                resources,
                runtime,
            },
            key,
            true,
        )
    }

    /// Adds a vector scene: its fills drawn from tile coverage at the
    /// graph's size, over transparency (see [`alpha2_geometry::VectorScene`]).
    /// Its key is the scene's, so an unchanged scene is a cache hit; its
    /// picture is premultiplied and sRGB-encoded, as every node's.
    ///
    /// The scene's coordinates are this graph's pixels: one built for
    /// another size draws at that size's coordinates.
    pub fn add_vector(
        &mut self,
        scene: std::sync::Arc<alpha2_geometry::VectorScene>,
    ) -> RenderNodeId {
        let key = vector_cache_key(self.size, scene.key);
        self.push(RenderNode::Vector { scene }, key, true)
    }

    /// Adds a registered two-input transition node.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] unless both inputs are earlier.
    pub fn add_transition(
        &mut self,
        effect: EffectId,
        from: RenderNodeId,
        to: RenderNodeId,
        params: EffectParams<'a>,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.add_transition_with_resources(
            effect,
            from,
            to,
            params,
            EffectResources::new(),
            runtime,
        )
    }

    /// Adds a registered two-input transition node with external resources.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] unless both inputs are earlier.
    pub fn add_transition_with_resources(
        &mut self,
        effect: EffectId,
        from: RenderNodeId,
        to: RenderNodeId,
        params: EffectParams<'a>,
        resources: EffectResources,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.validate_dependency(from)?;
        self.validate_dependency(to)?;
        let key = effect_cache_key_with_resources(
            b"transition",
            self.size,
            &effect,
            &[self.cache_keys[from.index], self.cache_keys[to.index]],
            &params,
            &resources,
            runtime,
        );
        let stable = self.stable[from.index] && self.stable[to.index];
        Ok(self.push(
            RenderNode::Transition {
                effect,
                from,
                to,
                params,
                resources,
                runtime,
            },
            key,
            stable,
        ))
    }

    /// Adds an ordered layer-composition node over a transparent target.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] for a missing layer source.
    pub fn add_composite(&mut self, layers: Vec<GraphLayer>) -> Result<RenderNodeId, GraphError> {
        self.push_composite(layers, None)
    }

    /// Adds an ordered layer-composition node over a solid background.
    ///
    /// `background` is the bottom layer: the first layer blends against it as
    /// against any lower layer. Like [`crate::Renderer::render_solid`]'s
    /// color, it is a premultiplied, sRGB-encoded storage value.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] for a missing layer source,
    /// or [`RenderError::InvalidColor`] for a background out of range or not
    /// premultiplied.
    pub fn add_composite_over(
        &mut self,
        layers: Vec<GraphLayer>,
        background: Color,
    ) -> Result<RenderNodeId, GraphError> {
        if !background.is_valid() {
            return Err(GraphError::Render(RenderError::InvalidColor));
        }
        self.push_composite(layers, Some(background))
    }

    fn push_composite(
        &mut self,
        layers: Vec<GraphLayer>,
        background: Option<Color>,
    ) -> Result<RenderNodeId, GraphError> {
        for layer in &layers {
            self.validate_dependency(layer.source)?;
        }
        let key = composite_cache_key(self.size, &layers, background, &self.cache_keys);
        let stable = layers.iter().all(|layer| self.stable[layer.source.index]);
        Ok(self.push(RenderNode::Composite { layers, background }, key, stable))
    }

    /// Selects the graph result.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] for an unknown node.
    pub fn set_output(&mut self, output: RenderNodeId) -> Result<(), GraphError> {
        self.validate_dependency(output)?;
        self.output = Some(output);
        Ok(())
    }

    /// Returns the number of nodes in insertion order.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the public category of a node.
    #[must_use]
    pub fn node_kind(&self, id: RenderNodeId) -> Option<RenderNodeKind> {
        if id.graph == self.id {
            self.nodes.get(id.index).map(RenderNode::kind)
        } else {
            None
        }
    }

    /// Names `node` a cache candidate: its result is looked up before it is
    /// drawn and kept after, as `intent` says, when its key is complete.
    /// [`crate::CacheIntent::Bypass`] instead keeps it, everything it is drawn
    /// from, and everything drawn from it out of the cache.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] for an unknown node.
    pub fn set_cache_intent(
        &mut self,
        node: RenderNodeId,
        intent: crate::CacheIntent,
    ) -> Result<(), GraphError> {
        self.validate_dependency(node)?;
        self.intents[node.index] = Some(intent);
        Ok(())
    }

    /// Whether `node`'s result can be cached, and if not, why.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] for an unknown node.
    pub fn cache_eligibility(
        &self,
        node: RenderNodeId,
    ) -> Result<crate::CacheEligibility, GraphError> {
        self.validate_dependency(node)?;
        Ok(self.eligibility()[node.index])
    }

    /// Every node's eligibility. A bypass reaches down to what a node is
    /// drawn from and up to what is drawn from it; a transient source only up.
    pub(crate) fn eligibility(&self) -> Vec<crate::CacheEligibility> {
        let count = self.nodes.len();
        let mut bypassed: Vec<bool> = self
            .intents
            .iter()
            .map(|intent| *intent == Some(crate::CacheIntent::Bypass))
            .collect();
        // Up: nodes are in dependency order, so inputs come first.
        for index in 0..count {
            if self.nodes[index]
                .dependencies()
                .iter()
                .any(|input| bypassed[input.index])
            {
                bypassed[index] = true;
            }
        }
        // Down: from each explicitly bypassed node to its inputs.
        let mut down: Vec<bool> = self
            .intents
            .iter()
            .map(|intent| *intent == Some(crate::CacheIntent::Bypass))
            .collect();
        for index in (0..count).rev() {
            if down[index] {
                for input in self.nodes[index].dependencies() {
                    down[input.index] = true;
                }
            }
        }
        (0..count)
            .map(|index| {
                if bypassed[index] || down[index] {
                    crate::CacheEligibility::Bypassed
                } else if self.stable[index] {
                    crate::CacheEligibility::Eligible
                } else {
                    crate::CacheEligibility::TransientSource
                }
            })
            .collect()
    }

    fn push(
        &mut self,
        node: RenderNode<'a>,
        cache_key: RenderCacheKey,
        stable: bool,
    ) -> RenderNodeId {
        let id = RenderNodeId {
            graph: self.id,
            index: self.nodes.len(),
        };
        self.nodes.push(node);
        self.cache_keys.push(cache_key);
        self.stable.push(stable);
        self.intents.push(None);
        id
    }

    fn validate_dependency(&self, dependency: RenderNodeId) -> Result<(), GraphError> {
        if dependency.graph == self.id && dependency.index < self.nodes.len() {
            Ok(())
        } else {
            Err(GraphError::InvalidDependency)
        }
    }
}

impl RenderNodeKind {
    /// The label a pass of this kind is timed under (see
    /// [`crate::PassTiming::label`]).
    pub(crate) const fn pass_label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Effect => "effect",
            Self::Transition => "transition",
            Self::Generator => "generator",
            Self::Composite => "composite",
            Self::Vector => "vector",
        }
    }
}

impl RenderNode<'_> {
    pub(crate) const fn kind(&self) -> RenderNodeKind {
        match self {
            Self::Source(_) => RenderNodeKind::Source,
            Self::Effect { .. } => RenderNodeKind::Effect,
            Self::Transition { .. } => RenderNodeKind::Transition,
            Self::Generator { .. } => RenderNodeKind::Generator,
            Self::Composite { .. } => RenderNodeKind::Composite,
            Self::Vector { .. } => RenderNodeKind::Vector,
        }
    }

    pub(crate) fn dependencies(&self) -> Vec<RenderNodeId> {
        match self {
            Self::Source(_) | Self::Generator { .. } | Self::Vector { .. } => Vec::new(),
            Self::Effect { source, inputs, .. } => std::iter::once(*source)
                .chain(inputs.iter().map(|(_, input)| input))
                .collect(),
            Self::Transition { from, to, .. } => vec![*from, *to],
            Self::Composite { layers, .. } => layers.iter().map(|layer| layer.source).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_and_setting_refuses_a_node_of_another_graph_or_one_not_yet_added() {
        // A node id carries its graph, so a dependency check that compared only indices
        // would wire one graph's node into another; every method taking a node refuses it.
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let effect = EffectId::new("test.effect".into(), 1);
        let mut other = RenderGraph::new(size);
        let foreign = other.add_generator(
            effect.clone(),
            EffectParams::new(),
            EffectRuntime::default(),
        );
        let mut graph = RenderGraph::new(size);
        let own = graph.add_generator(
            effect.clone(),
            EffectParams::new(),
            EffectRuntime::default(),
        );
        // The same index as a node of this graph, but another graph's.
        assert_eq!(foreign.index, own.index);
        let unknown = RenderNodeId {
            graph: graph.id,
            index: 1,
        };
        let layer = |source| GraphLayer {
            source,
            visual: crate::LayerTransform {
                position: [0.5, 0.5],
                anchor: [0.5, 0.5],
                scale: [1.0, 1.0],
                rotation_degrees: 0.0,
                opacity: 1.0,
            },
            blend_mode: crate::BlendMode::Normal,
        };
        for bad in [foreign, unknown] {
            let invalid = Err(GraphError::InvalidDependency);
            assert_eq!(
                graph.add_effect(
                    effect.clone(),
                    bad,
                    EffectParams::new(),
                    EffectRuntime::default()
                ),
                invalid
            );
            assert_eq!(
                graph.add_transition(
                    effect.clone(),
                    own,
                    bad,
                    EffectParams::new(),
                    EffectRuntime::default()
                ),
                invalid
            );
            assert_eq!(
                graph.add_transition(
                    effect.clone(),
                    bad,
                    own,
                    EffectParams::new(),
                    EffectRuntime::default()
                ),
                invalid
            );
            assert_eq!(graph.add_composite(vec![layer(own), layer(bad)]), invalid);
            assert_eq!(
                graph.add_composite_over(vec![layer(bad)], Color::BLACK),
                invalid
            );
            assert_eq!(graph.set_output(bad), Err(GraphError::InvalidDependency));
            assert_eq!(
                graph.set_cache_intent(bad, crate::CacheIntent::Pin),
                Err(GraphError::InvalidDependency)
            );
            assert_eq!(
                graph.cache_eligibility(bad),
                Err(GraphError::InvalidDependency)
            );
            assert_eq!(graph.node_kind(bad), None);
        }
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.node_kind(own), Some(RenderNodeKind::Generator));
        // An out-of-range background is refused before its layers are looked at.
        assert_eq!(
            graph.add_composite_over(vec![layer(foreign)], Color::rgba(1.5, 0.0, 0.0, 1.0)),
            Err(GraphError::Render(RenderError::InvalidColor))
        );
        assert_eq!(graph.node_count(), 1);
    }

    fn generator(graph: &mut RenderGraph<'_>, id: &'static str) -> RenderNodeId {
        graph.add_generator(
            EffectId::from_static(id, 1),
            EffectParams::new(),
            EffectRuntime::default(),
        )
    }

    fn effect_with_inputs(
        graph: &mut RenderGraph<'static>,
        source: RenderNodeId,
        inputs: EffectInputs<'static>,
    ) -> Result<RenderNodeId, GraphError> {
        graph.add_effect_with_inputs(
            EffectId::from_static("test.multi-input", 1),
            source,
            inputs,
            EffectParams::new(),
            EffectResources::new(),
            EffectRuntime::default(),
        )
    }

    #[test]
    fn named_inputs_are_dependencies() {
        // A node read only as a named input is drawn before the effect and kept until it.
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let mut graph = RenderGraph::new(size);
        let source = generator(&mut graph, "test.source");
        let map = generator(&mut graph, "test.map");
        let unused = generator(&mut graph, "test.unused");
        let effect = effect_with_inputs(&mut graph, source, EffectInputs::new().with("map", map))
            .expect("earlier nodes");
        graph.set_output(effect).expect("known output");
        let plan = graph.compile().expect("compiles");
        assert_eq!(plan.execution_order(), [source, map, effect]);
        assert_eq!(plan.last_use(map), Some(2));
        assert_eq!(plan.last_use(unused), None);
        // An input must be an earlier node of this graph, as a source must.
        let later = RenderNodeId {
            graph: graph.id,
            index: graph.node_count(),
        };
        assert_eq!(
            effect_with_inputs(&mut graph, source, EffectInputs::new().with("map", later)),
            Err(GraphError::InvalidDependency)
        );
    }

    #[test]
    fn named_inputs_on_a_built_in_drawn_as_passes_are_refused() {
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let mut graph = RenderGraph::new(size);
        let source = generator(&mut graph, "test.source");
        let map = generator(&mut graph, "test.map");
        let count = graph.node_count();
        for id in [crate::builtins::GAUSSIAN_BLUR_ID, crate::builtins::GLOW_ID] {
            assert_eq!(
                graph.add_effect_with_inputs(
                    EffectId::from_static(id, 1),
                    source,
                    EffectInputs::new().with("map", map),
                    EffectParams::new(),
                    EffectResources::new(),
                    EffectRuntime::default(),
                ),
                Err(GraphError::Render(RenderError::InvalidEffectParams)),
                "{id}"
            );
        }
        assert_eq!(graph.node_count(), count, "no pass is left behind");
    }

    #[test]
    fn a_named_input_is_part_of_the_cache_key() {
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let key = |build: &dyn Fn(&mut RenderGraph<'static>, [RenderNodeId; 3]) -> RenderNodeId| {
            let mut graph = RenderGraph::new(size);
            let nodes = [
                generator(&mut graph, "test.source"),
                generator(&mut graph, "test.map"),
                generator(&mut graph, "test.other-map"),
            ];
            let node = build(&mut graph, nodes);
            graph.cache_keys[node.index]
        };
        let named = |name: &'static str, input: usize| {
            key(&move |graph, nodes| {
                effect_with_inputs(
                    graph,
                    nodes[0],
                    EffectInputs::new().with(name, nodes[input]),
                )
                .expect("earlier nodes")
            })
        };
        // Equal inputs give equal keys in another graph; another node or name another key.
        assert_eq!(named("map", 1), named("map", 1));
        assert_ne!(named("map", 1), named("map", 2));
        assert_ne!(named("map", 1), named("mask", 1));
        let none = key(&|graph, nodes| {
            effect_with_inputs(graph, nodes[0], EffectInputs::new()).expect("earlier node")
        });
        assert_ne!(named("map", 1), none);
        // Without named inputs, the key is the one `add_effect_with_resources` gives.
        let plain = key(&|graph, nodes| {
            graph
                .add_effect_with_resources(
                    EffectId::from_static("test.multi-input", 1),
                    nodes[0],
                    EffectParams::new(),
                    EffectResources::new(),
                    EffectRuntime::default(),
                )
                .expect("earlier node")
        });
        assert_eq!(none, plain);
    }

    #[test]
    fn a_graph_compiles_only_once_an_output_is_chosen() {
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let mut graph = RenderGraph::new(size);
        let output = graph.add_composite(Vec::new()).expect("empty clear node");
        assert_eq!(graph.compile().err(), Some(GraphError::MissingOutput));
        assert_eq!(
            GraphError::MissingOutput.into_render_error(),
            RenderError::InvalidGraph
        );
        graph.set_output(output).expect("known output");
        assert_eq!(
            graph.compile().expect("compiles").execution_order(),
            [output]
        );
    }

    #[test]
    fn quality_compilation_exposes_centered_temporal_schedule() {
        let size = RenderSize::new(16, 9).expect("non-zero size");
        let mut graph = RenderGraph::new(size);
        let output = graph.add_composite(Vec::new()).expect("empty clear node");
        graph.set_output(output).expect("known output");
        let plan = graph
            .compile_with_quality(crate::RenderQualityControls {
                max_filter_samples: 12,
                motion_blur_subframes: 4,
            })
            .expect("compiled");
        assert_eq!(plan.temporal_offsets(), [-0.375, -0.125, 0.125, 0.375]);
        assert_eq!(plan.filter_sample_limit(output), None);
    }
}
