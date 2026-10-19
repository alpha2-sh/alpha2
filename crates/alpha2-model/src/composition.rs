//! Composition graphs: a clip's picture built from connected nodes.
//!
//! A [`Composition`] is an acyclic graph of image operations stored once in
//! the project. A clip that shows one refers to it with a
//! [`ClipComposition`], which also says how the clip's timeline time maps
//! onto the composition's own time. Every node outputs one image; a node
//! takes its inputs by naming the nodes that produce them, so a node's
//! output can feed any number of later nodes, and an input can be left
//! unconnected while the graph is being built.
//!
//! A composition can also be used inside another: a [`NodeOp::Group`] node
//! draws it, feeding its [`NodeOp::GroupInput`] nodes from the group node's
//! inputs and setting the node settings it exposes as
//! [`CompositionControl`]s, independently for each group node. A
//! [`crate::MediaKind::Composition`] source shows one on the timeline, or in
//! a media node. Compositions may not use themselves, however indirectly.
//! A composition is inlined wherever it is drawn, so its uses multiply:
//! nesting is bounded by [`MAX_COMPOSITION_DEPTH`] and what all compositions
//! draw by [`MAX_INLINED_NODES`].
//!
//! What a graph means is checked in two steps. Saving checks its structure:
//! that every name resolves, that nothing depends on itself, and that every
//! value is in range. Rendering then checks what the chosen output needs:
//! every input a node reachable from the output uses must be connected.
//! Nodes that do not reach the output are kept and ignored.

use crate::{
    Animated, ClipBlendMode, ClipEffect, CompositionId, EffectValue, Generator, MatteMode, NodeId,
    ShapeMask, VisualProperties,
};
use alpha2_media::SourceId;
use alpha2_time::MediaTime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};

/// Most compositions in one project.
pub const MAX_COMPOSITIONS: usize = 10_000;
/// Most nodes in one composition.
pub const MAX_COMPOSITION_NODES: usize = 256;
/// Most layers in one merge node.
pub const MAX_MERGE_LAYERS: usize = 32;
/// Most inputs a composition takes as a group.
pub const MAX_GROUP_INPUTS: usize = 16;
/// Most controls a composition exposes as a group.
pub const MAX_COMPOSITION_CONTROLS: usize = 64;
/// Deepest nesting of compositions: a composition that draws no other has
/// depth one, and one that draws others, through group nodes or
/// composition-source media nodes, is one deeper than the deepest of them.
/// Drawing a composition recurses once per level.
pub const MAX_COMPOSITION_DEPTH: usize = 64;
/// Most nodes the project's compositions draw, summed over every
/// composition: each node counts one, except a group node or a media node
/// reading a composition source, which counts the nodes of the composition
/// it draws. A composition is inlined wherever it is drawn, so this bounds
/// what compiling the compositions makes, however their uses multiply.
pub const MAX_INLINED_NODES: usize = 65_536;
/// Most composition nodes the clips on the timeline at one instant draw
/// together: each clip counts the nodes its composition and its composition
/// source draw once inlined, as [`MAX_INLINED_NODES`] counts them, and a
/// clip showing neither counts none. [`MAX_INLINED_NODES`] bounds what the
/// compositions compile to however often they are used; this bounds what
/// their uses draw in one frame, so a frame never draws more than one
/// composition of the largest size the project may hold.
pub const MAX_FRAME_INLINED_NODES: usize = 65_536;

/// An acyclic graph of image operations with one selected output.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Composition {
    /// Stable composition identity.
    pub id: CompositionId,
    /// The graph's nodes. Their order has no meaning: a node may name a node
    /// listed after it as its input.
    #[serde(default)]
    pub nodes: Vec<CompositionNode>,
    /// The node whose image is the composition's picture. A composition
    /// without one saves, but cannot be rendered.
    #[serde(default)]
    pub output: Option<NodeId>,
    /// The images a group node gives it, by name, in port order: its
    /// [`NodeOp::GroupInput`] nodes read them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// The node settings a group node may set, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<CompositionControl>,
}

/// A node setting a composition exposes to the group nodes that use it: an
/// effect or transition parameter of one of its nodes.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CompositionControl {
    /// The name a group node sets it by; unique in the composition.
    pub name: String,
    /// The effect or transition node.
    pub node: NodeId,
    /// The parameter of its effect.
    pub param: String,
}

impl Composition {
    /// Creates an empty composition with no output.
    #[must_use]
    pub const fn new(id: CompositionId) -> Self {
        Self {
            id,
            nodes: Vec::new(),
            output: None,
            inputs: Vec::new(),
            controls: Vec::new(),
        }
    }

    /// The node named `id`.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&CompositionNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }
}

/// One image operation in a [`Composition`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CompositionNode {
    /// Identity, unique within its composition.
    pub id: NodeId,
    /// Whether the node passes its [`NodeOp::bypass_input`] through
    /// unchanged instead of operating. A node without inputs cannot be
    /// bypassed.
    #[serde(default)]
    pub bypass: bool,
    /// What the node does, with its inputs and settings.
    pub op: NodeOp,
}

impl CompositionNode {
    /// Creates a node that is not bypassed.
    #[must_use]
    pub const fn new(id: NodeId, op: NodeOp) -> Self {
        Self {
            id,
            bypass: false,
            op,
        }
    }

    /// The inputs drawing the node reads, in input order: only its
    /// [`NodeOp::bypass_input`] when it is bypassed (none for a merge without
    /// layers), and every input otherwise. The renderer plans these, and
    /// [`crate::Project::used_sources`] follows them.
    #[must_use]
    pub fn needed_inputs(&self) -> Vec<NodeInput<'_>> {
        if self.bypass {
            return self.op.bypass_input().into_iter().collect();
        }
        self.op.inputs()
    }
}

/// The operation of a [`CompositionNode`].
///
/// Animated settings are keyed in composition time (see
/// [`ClipComposition`]), not timeline time.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeOp {
    /// The picture of the clip showing the composition: its source at the
    /// source time the clip's time map gives, or what its generated source
    /// draws.
    Clip {},
    /// Another visual source, read at `start` plus the composition time.
    /// A media node adds no sound to the mix.
    Media {
        /// The source; a video, image, or generated source.
        source_id: SourceId,
        /// Source time shown at composition time zero. Non-negative.
        #[serde(default)]
        start: MediaTime,
    },
    /// A picture the engine draws.
    Generator {
        /// What is drawn.
        generator: Generator,
    },
    /// A registered one-input effect.
    Effect {
        /// The image the effect processes.
        #[serde(default)]
        input: Option<NodeId>,
        /// The versioned effect and its parameters.
        effect: ClipEffect,
    },
    /// A registered two-input transition at an explicit progress.
    Transition {
        /// The image at progress zero.
        #[serde(default)]
        from: Option<NodeId>,
        /// The image at progress one.
        #[serde(default)]
        to: Option<NodeId>,
        /// The versioned transition and its parameters.
        effect: ClipEffect,
        /// Progress through the transition, in `0..=1`.
        progress: Animated<f32>,
    },
    /// The input placed on the canvas by a transform and opacity.
    Transform {
        /// The image placed.
        #[serde(default)]
        input: Option<NodeId>,
        /// Position, anchor, scale, rotation, and opacity.
        #[serde(default)]
        transform: VisualProperties,
    },
    /// Ordered layers composited over transparency.
    Merge {
        /// Layers from bottom to top.
        #[serde(default)]
        layers: Vec<MergeLayer>,
    },
    /// The input kept where a second image says.
    Matte {
        /// The image kept.
        #[serde(default)]
        input: Option<NodeId>,
        /// The image whose alpha or luminance is coverage.
        #[serde(default)]
        matte: Option<NodeId>,
        /// Which property of the matte image is coverage.
        mode: MatteMode,
    },
    /// The input kept inside shapes.
    ShapeMask {
        /// The image masked.
        #[serde(default)]
        input: Option<NodeId>,
        /// The shapes, combined in order; at most [`crate::MAX_CLIP_MASKS`].
        /// Geometry is in the input image's normalized space.
        masks: Vec<ShapeMask>,
    },
    /// Another composition drawn in this one's time, as a group: its
    /// output, with its group inputs fed from `inputs` and its controls set
    /// from `controls`.
    Group {
        /// The composition drawn.
        composition: CompositionId,
        /// The image for each of its inputs, in its port order.
        #[serde(default)]
        inputs: Vec<Option<NodeId>>,
        /// Values for its controls, by name; a control not set keeps the
        /// composition's own value. Keyed in composition time.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        controls: BTreeMap<String, Animated<EffectValue>>,
    },
    /// In a composition used as a group, the image the group node gives its
    /// input `index`.
    GroupInput {
        /// The input, in the composition's port order.
        index: u32,
    },
}

/// One layer of a [`NodeOp::Merge`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MergeLayer {
    /// The layer's image.
    #[serde(default)]
    pub input: Option<NodeId>,
    /// How the layer blends with the layers under it.
    #[serde(default)]
    pub blend_mode: ClipBlendMode,
    /// The layer's opacity, in `0..=1`.
    #[serde(default = "opaque")]
    pub opacity: Animated<f32>,
}

impl MergeLayer {
    /// An opaque, normally blended layer of `input`.
    #[must_use]
    pub const fn new(input: Option<NodeId>) -> Self {
        Self {
            input,
            blend_mode: ClipBlendMode::Normal,
            opacity: Animated::Constant { value: 1.0 },
        }
    }
}

const fn opaque() -> Animated<f32> {
    Animated::Constant { value: 1.0 }
}

/// What each composition's output reads: the group inputs and clip nodes a
/// walk from its output reaches, along the inputs drawing each node reads,
/// into the compositions its group nodes draw. A group draws what its nodes
/// would inline, so a group node needs only the ports its composition reads:
/// a port it never reads may be left unconnected, and what a connected one
/// reads is not drawn.
///
/// Built once for a project's compositions, in dependency order, without
/// recursion. A composition that uses itself (which validation refuses) is
/// taken to read every port and draw the clip.
#[derive(Clone, Debug, Default)]
pub struct CompositionReads<'a> {
    read: HashMap<&'a CompositionId, Reads>,
}

/// What one composition's output reads.
#[derive(Clone, Debug)]
struct Reads {
    /// By group input index.
    ports: Vec<bool>,
    /// Whether it draws the picture of the clip showing it.
    clip: bool,
}

impl<'a> CompositionReads<'a> {
    /// What each of `compositions` reads; it resolves group nodes by ID.
    #[must_use]
    pub fn new(compositions: &HashMap<&'a CompositionId, &'a Composition>) -> Self {
        let mut reads = Self::default();
        let mut walking = HashSet::new();
        let group = |node: &'a CompositionNode| match &node.op {
            NodeOp::Group { composition, .. } => compositions.get(composition).copied(),
            _ => None,
        };
        for root in compositions.values() {
            let mut stack = vec![(*root, false)];
            while let Some((composition, expanded)) = stack.pop() {
                if reads.read.contains_key(&composition.id) {
                    continue;
                }
                if !expanded {
                    if !walking.insert(&composition.id) {
                        continue;
                    }
                    stack.push((composition, true));
                    stack.extend(
                        composition
                            .nodes
                            .iter()
                            .filter_map(group)
                            .filter(|inner| {
                                !reads.read.contains_key(&inner.id) && !walking.contains(&inner.id)
                            })
                            .map(|inner| (inner, false)),
                    );
                    continue;
                }
                walking.remove(&composition.id);
                let read = reads.walk(composition);
                reads.read.insert(&composition.id, read);
            }
        }
        reads
    }

    /// Whether `composition`'s output reads its group input `port`; true for
    /// a composition these reads do not know.
    #[must_use]
    pub fn reads_port(&self, composition: &CompositionId, port: usize) -> bool {
        self.read
            .get(composition)
            .is_none_or(|read| read.ports.get(port).copied().unwrap_or(false))
    }

    /// Whether `composition`, shown by a clip, draws the clip's picture: its
    /// output reaches a clip node, directly or in a group. True for a
    /// composition these reads do not know.
    #[must_use]
    pub fn draws_clip(&self, composition: &CompositionId) -> bool {
        self.read.get(composition).is_none_or(|read| read.clip)
    }

    /// The inputs drawing `node` reads, in input order: its
    /// [`CompositionNode::needed_inputs`], of a group node only the ports its
    /// composition reads.
    #[must_use]
    pub fn needed_inputs<'n>(&self, node: &'n CompositionNode) -> Vec<NodeInput<'n>> {
        let mut needed = node.needed_inputs();
        if let (false, NodeOp::Group { composition, .. }) = (node.bypass, &node.op) {
            needed.retain(|input| match input {
                NodeInput::Port(port, _) => self.reads_port(composition, *port),
                _ => true,
            });
        }
        needed
    }

    /// What `composition`'s output reaches, with the compositions its group
    /// nodes draw already known.
    fn walk(&self, composition: &'a Composition) -> Reads {
        let nodes: HashMap<&NodeId, &CompositionNode> = composition
            .nodes
            .iter()
            .map(|node| (&node.id, node))
            .collect();
        let mut read = Reads {
            ports: vec![false; composition.inputs.len()],
            clip: false,
        };
        let mut reached = HashSet::new();
        let mut pending: Vec<&NodeId> = composition.output.iter().collect();
        while let Some(id) = pending.pop() {
            let Some(node) = nodes.get(id).filter(|_| reached.insert(id)) else {
                continue;
            };
            match (node.bypass, &node.op) {
                (false, NodeOp::GroupInput { index }) => {
                    if let Some(port) = usize::try_from(*index)
                        .ok()
                        .and_then(|index| read.ports.get_mut(index))
                    {
                        *port = true;
                    }
                }
                (false, NodeOp::Clip {}) => read.clip = true,
                (false, NodeOp::Group { composition, .. }) => {
                    read.clip |= self.draws_clip(composition);
                }
                _ => {}
            }
            pending.extend(
                self.needed_inputs(node)
                    .into_iter()
                    .filter_map(NodeInput::node),
            );
        }
        read
    }
}

/// One named input of a node, as a node's inputs are listed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeInput<'a> {
    /// A named input: `input`, `from`, `to`, or `matte`.
    Named(&'static str, Option<&'a NodeId>),
    /// The input of a merge layer, by layer index.
    Layer(usize, Option<&'a NodeId>),
    /// An input of a group node, by port index.
    Port(usize, Option<&'a NodeId>),
}

impl<'a> NodeInput<'a> {
    /// The connected node, if any.
    #[must_use]
    pub const fn node(self) -> Option<&'a NodeId> {
        match self {
            Self::Named(_, node) | Self::Layer(_, node) | Self::Port(_, node) => node,
        }
    }

    /// The input's name as a diagnostic shows it: `input`, `from`, `to`,
    /// `matte`, `layers[i]`, or `inputs[i]`.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Named(name, _) => name.to_owned(),
            Self::Layer(index, _) => format!("layers[{index}]"),
            Self::Port(index, _) => format!("inputs[{index}]"),
        }
    }
}

impl NodeOp {
    /// Every input of the operation, connected or not, in a fixed order.
    #[must_use]
    pub fn inputs(&self) -> Vec<NodeInput<'_>> {
        match self {
            Self::Clip {}
            | Self::Media { .. }
            | Self::Generator { .. }
            | Self::GroupInput { .. } => Vec::new(),
            Self::Group { inputs, .. } => inputs
                .iter()
                .enumerate()
                .map(|(index, input)| NodeInput::Port(index, input.as_ref()))
                .collect(),
            Self::Effect { input, .. }
            | Self::Transform { input, .. }
            | Self::ShapeMask { input, .. } => vec![NodeInput::Named("input", input.as_ref())],
            Self::Transition { from, to, .. } => vec![
                NodeInput::Named("from", from.as_ref()),
                NodeInput::Named("to", to.as_ref()),
            ],
            Self::Matte { input, matte, .. } => vec![
                NodeInput::Named("input", input.as_ref()),
                NodeInput::Named("matte", matte.as_ref()),
            ],
            Self::Merge { layers } => layers
                .iter()
                .enumerate()
                .map(|(index, layer)| NodeInput::Layer(index, layer.input.as_ref()))
                .collect(),
        }
    }

    /// The input a bypassed node passes through: `input` for one-input
    /// operations and mattes, `from` for a transition, the bottom layer of
    /// a merge, and a group's first input. `None` for an operation without
    /// inputs, which cannot be bypassed, and for a merge without layers.
    #[must_use]
    pub fn bypass_input(&self) -> Option<NodeInput<'_>> {
        match self {
            Self::Clip {}
            | Self::Media { .. }
            | Self::Generator { .. }
            | Self::GroupInput { .. } => None,
            Self::Merge { layers } => layers
                .first()
                .map(|layer| NodeInput::Layer(0, layer.input.as_ref())),
            _ => self.inputs().into_iter().next(),
        }
    }

    /// Whether the operation takes inputs, and so can be bypassed.
    #[must_use]
    pub const fn has_inputs(&self) -> bool {
        match self {
            Self::Clip {}
            | Self::Media { .. }
            | Self::Generator { .. }
            | Self::GroupInput { .. } => false,
            Self::Group { inputs, .. } => !inputs.is_empty(),
            _ => true,
        }
    }

    /// The effect or transition invocation the operation runs, if any.
    #[must_use]
    pub const fn effect(&self) -> Option<&ClipEffect> {
        match self {
            Self::Effect { effect, .. } | Self::Transition { effect, .. } => Some(effect),
            _ => None,
        }
    }
}

/// A clip's use of a [`Composition`], and how its time maps onto it.
///
/// Composition time is `offset + (t - clip start) * scale` for timeline time
/// `t`, rounded down to a tick. Moving a clip keeps its offset, so its
/// animation moves with it; trimming its start by `d` adds `d * scale` to the
/// offset, so what remains of the animation stays where it was. Extending the
/// start before composition time zero makes the offset negative: settings
/// hold their first keyframe there, and media nodes show their first frame.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ClipComposition {
    /// The composition shown.
    pub id: CompositionId,
    /// Composition time at the clip's start.
    #[serde(default)]
    pub offset: MediaTime,
    /// Composition time per timeline time.
    #[serde(default)]
    pub scale: TimeScale,
}

impl ClipComposition {
    /// Shows `id` from its time zero at the timeline's rate.
    #[must_use]
    pub const fn new(id: CompositionId) -> Self {
        Self {
            id,
            offset: MediaTime::ZERO,
            scale: TimeScale::ONE,
        }
    }

    /// The composition time at timeline time `time` of a clip starting at
    /// `clip_start`, or `None` when it is not representable or the scale has
    /// a zero denominator (no rate; validation refuses it).
    #[must_use]
    pub fn local_time(&self, clip_start: MediaTime, time: MediaTime) -> Option<MediaTime> {
        let elapsed = i128::from(time.ticks()) - i128::from(clip_start.ticks());
        let scaled = (elapsed * i128::from(self.scale.num))
            .checked_div_euclid(i128::from(self.scale.den))?;
        let local = i128::from(self.offset.ticks()).checked_add(scaled)?;
        i64::try_from(local).ok().map(MediaTime)
    }
}

/// A positive rational rate of one time against another.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TimeScale {
    /// Numerator; positive.
    pub num: u32,
    /// Denominator; positive.
    pub den: u32,
}

impl TimeScale {
    /// The same rate.
    pub const ONE: Self = Self { num: 1, den: 1 };

    /// Whether both terms are positive.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.num > 0 && self.den > 0
    }
}

impl Default for TimeScale {
    fn default() -> Self {
        Self::ONE
    }
}

/// Checks every composition's identity, node settings, and connections, and
/// returns the nodes each draws once inlined.
///
/// `by_source` is `sources` by ID, which each media node is looked up in.
pub(crate) fn validate_compositions<'a>(
    compositions: &'a [Composition],
    sources: &'a [crate::MediaSource],
    by_source: &std::collections::HashMap<&SourceId, &crate::MediaSource>,
    effect_resources: &std::collections::HashSet<&crate::EffectResourceId>,
) -> Result<InlinedNodes<'a>, crate::ModelError> {
    use crate::ModelError;
    use std::collections::{HashMap, HashSet};

    if compositions.len() > MAX_COMPOSITIONS {
        return Err(ModelError::TooManyCompositions {
            found: compositions.len(),
            maximum: MAX_COMPOSITIONS,
        });
    }
    let mut ids = HashSet::with_capacity(compositions.len());
    for composition in compositions {
        if !ids.insert(&composition.id) {
            return Err(ModelError::DuplicateCompositionId(composition.id.clone()));
        }
    }
    let by_id: HashMap<&CompositionId, &Composition> = compositions
        .iter()
        .map(|composition| (&composition.id, composition))
        .collect();
    if let Some(source) = sources.iter().find(|source| {
        source
            .composition
            .as_ref()
            .is_some_and(|id| !by_id.contains_key(id))
    }) {
        return Err(ModelError::InvalidCompositionSource {
            source: source.id.clone(),
        });
    }
    for composition in compositions {
        if composition.nodes.len() > MAX_COMPOSITION_NODES {
            return Err(ModelError::TooManyCompositionNodes {
                composition: composition.id.clone(),
                found: composition.nodes.len(),
                maximum: MAX_COMPOSITION_NODES,
            });
        }
        let mut positions = HashMap::with_capacity(composition.nodes.len());
        for (position, node) in composition.nodes.iter().enumerate() {
            if node.id.as_str().contains('/') {
                return Err(ModelError::InvalidNodeId {
                    composition: composition.id.clone(),
                    node: node.id.clone(),
                });
            }
            if positions.insert(&node.id, position).is_some() {
                return Err(ModelError::DuplicateNodeId {
                    composition: composition.id.clone(),
                    node: node.id.clone(),
                });
            }
        }
        validate_ports(composition, &positions)?;
        for node in &composition.nodes {
            validate_node(composition, node, by_source, &by_id, effect_resources)?;
            for input in node.op.inputs() {
                if let Some(target) = input.node()
                    && !positions.contains_key(target)
                {
                    return Err(ModelError::UnknownNodeInput {
                        composition: composition.id.clone(),
                        node: node.id.clone(),
                        input: input.name(),
                        target: target.clone(),
                    });
                }
            }
        }
        if let Some(output) = &composition.output
            && !positions.contains_key(output)
        {
            return Err(ModelError::UnknownCompositionOutput {
                composition: composition.id.clone(),
                node: output.clone(),
            });
        }
        if let Some(node) = cycle_member(composition, &positions) {
            return Err(ModelError::CompositionCycle {
                composition: composition.id.clone(),
                node: node.clone(),
            });
        }
    }
    if let Some(composition) = reference_cycle_member(compositions, &by_id, by_source) {
        return Err(ModelError::CompositionUsesItself {
            composition: composition.clone(),
        });
    }
    validate_expansion(compositions, &by_id, sources)
}

/// The nodes each composition draws once inlined, as [`MAX_INLINED_NODES`]
/// counts them.
pub(crate) type InlinedNodes<'a> = std::collections::HashMap<&'a CompositionId, u64>;

/// The compositions nest at most [`MAX_COMPOSITION_DEPTH`] deep and draw at
/// most [`MAX_INLINED_NODES`] nodes; returns the nodes each draws. The
/// references are acyclic, so one walk in dependency order counts each
/// composition once; it is iterative so that a long chain cannot exhaust
/// the stack.
fn validate_expansion<'a>(
    compositions: &'a [Composition],
    by_id: &std::collections::HashMap<&'a CompositionId, &'a Composition>,
    sources: &'a [crate::MediaSource],
) -> Result<InlinedNodes<'a>, crate::ModelError> {
    use std::collections::HashMap;
    let composition_sources: HashMap<&SourceId, &CompositionId> = sources
        .iter()
        .filter_map(|source| Some((&source.id, source.composition.as_ref()?)))
        .collect();
    // The composition a node draws in its place, if any.
    let draws = |node: &'a CompositionNode| -> Option<&'a CompositionId> {
        match &node.op {
            NodeOp::Group { composition, .. } => Some(composition),
            NodeOp::Media { source_id, .. } => composition_sources.get(source_id).copied(),
            _ => None,
        }
        .filter(|id| by_id.contains_key(id))
    };
    // Depth and inlined node count of each composition walked.
    let mut walked: HashMap<&CompositionId, (usize, u64)> = HashMap::new();
    let mut total = 0_u64;
    for root in compositions {
        let mut stack = vec![(root, false)];
        while let Some((composition, expanded)) = stack.pop() {
            if walked.contains_key(&composition.id) {
                continue;
            }
            if !expanded {
                stack.push((composition, true));
                stack.extend(
                    composition
                        .nodes
                        .iter()
                        .filter_map(draws)
                        .filter(|id| !walked.contains_key(id))
                        .map(|id| (by_id[id], false)),
                );
                continue;
            }
            let mut depth = 1;
            let mut nodes = 0_u64;
            for node in &composition.nodes {
                let (inner, count) = draws(node).map_or((0, 1), |id| walked[id]);
                depth = depth.max(inner + 1);
                nodes = nodes.saturating_add(count);
            }
            walked.insert(&composition.id, (depth, nodes));
        }
        let (depth, nodes) = walked[&root.id];
        if depth > MAX_COMPOSITION_DEPTH {
            return Err(crate::ModelError::CompositionNestedTooDeep {
                composition: root.id.clone(),
                maximum: MAX_COMPOSITION_DEPTH,
            });
        }
        total = total.saturating_add(nodes);
        if total > MAX_INLINED_NODES as u64 {
            return Err(crate::ModelError::TooManyInlinedNodes {
                maximum: MAX_INLINED_NODES,
            });
        }
    }
    Ok(walked
        .into_iter()
        .map(|(id, (_, nodes))| (id, nodes))
        .collect())
}

/// A composition's group inputs are named, distinct, and few enough, and its
/// controls name distinct settings of its effect and transition nodes.
fn validate_ports(
    composition: &Composition,
    positions: &std::collections::HashMap<&NodeId, usize>,
) -> Result<(), crate::ModelError> {
    use std::collections::HashSet;
    let invalid = || crate::ModelError::InvalidCompositionPorts {
        composition: composition.id.clone(),
    };
    let mut names = HashSet::new();
    if composition.inputs.len() > MAX_GROUP_INPUTS
        || composition
            .inputs
            .iter()
            .any(|name| name.is_empty() || !names.insert(name))
    {
        return Err(invalid());
    }
    let mut controls = HashSet::new();
    let mut settings = HashSet::new();
    if composition.controls.len() > MAX_COMPOSITION_CONTROLS {
        return Err(invalid());
    }
    for control in &composition.controls {
        let node = positions
            .get(&control.node)
            .map(|position| &composition.nodes[*position]);
        if control.name.is_empty()
            || control.param.is_empty()
            || !controls.insert(&control.name)
            || !settings.insert((&control.node, &control.param))
            || node.is_none_or(|node| node.op.effect().is_none())
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// A composition that uses itself, through group nodes or composition
/// sources, however indirectly: the first, in document order, that a
/// depth-first walk finds on its own path.
fn reference_cycle_member<'a>(
    compositions: &'a [Composition],
    by_id: &std::collections::HashMap<&'a CompositionId, &'a Composition>,
    sources: &std::collections::HashMap<&SourceId, &'a crate::MediaSource>,
) -> Option<&'a CompositionId> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let uses = |composition: &'a Composition| -> Vec<&'a CompositionId> {
        composition
            .nodes
            .iter()
            .filter_map(|node| match &node.op {
                NodeOp::Group { composition, .. } => Some(composition),
                NodeOp::Media { source_id, .. } => sources
                    .get(source_id)
                    .and_then(|source| source.composition.as_ref()),
                _ => None,
            })
            .filter(|id| by_id.contains_key(id))
            .collect()
    };
    let mut marks: std::collections::HashMap<&CompositionId, Mark> = compositions
        .iter()
        .map(|composition| (&composition.id, Mark::New))
        .collect();
    for root in compositions {
        if marks[&root.id] != Mark::New {
            continue;
        }
        let mut stack = vec![(&root.id, uses(root), 0_usize)];
        marks.insert(&root.id, Mark::Open);
        while let Some((current, next, at)) = stack.last_mut() {
            let Some(target) = next.get(*at).copied() else {
                marks.insert(current, Mark::Done);
                stack.pop();
                continue;
            };
            *at += 1;
            match marks[target] {
                Mark::Open => return Some(target),
                Mark::New => {
                    marks.insert(target, Mark::Open);
                    stack.push((target, uses(by_id[target]), 0));
                }
                Mark::Done => {}
            }
        }
    }
    None
}

fn validate_node(
    composition: &Composition,
    node: &CompositionNode,
    sources: &std::collections::HashMap<&SourceId, &crate::MediaSource>,
    compositions: &std::collections::HashMap<&CompositionId, &Composition>,
    effect_resources: &std::collections::HashSet<&crate::EffectResourceId>,
) -> Result<(), crate::ModelError> {
    use crate::ModelError;

    let invalid = || ModelError::InvalidNode {
        composition: composition.id.clone(),
        node: node.id.clone(),
    };
    let unit = |animation: &Animated<f32>| {
        crate::animation::valid_scalar_values(animation, |value| (0.0..=1.0).contains(&value))
    };
    if node.bypass && !node.op.has_inputs() {
        return Err(invalid());
    }
    let valid = match &node.op {
        NodeOp::Clip {} | NodeOp::Matte { .. } => true,
        NodeOp::Media { source_id, start } => {
            let visual = sources.get(source_id).is_some_and(|source| {
                matches!(
                    source.kind,
                    crate::MediaKind::Video
                        | crate::MediaKind::Image
                        | crate::MediaKind::Generated
                        | crate::MediaKind::Composition
                )
            });
            if !visual {
                return Err(ModelError::InvalidNodeSource {
                    composition: composition.id.clone(),
                    node: node.id.clone(),
                    source: source_id.clone(),
                });
            }
            start.ticks() >= 0
        }
        NodeOp::Generator { generator } => generator.is_valid(),
        NodeOp::Effect { effect, .. } => effect.is_valid(),
        NodeOp::Transition {
            effect, progress, ..
        } => effect.is_valid() && unit(progress),
        NodeOp::Transform { transform, .. } => transform.validate(),
        NodeOp::Merge { layers } => {
            layers.len() <= MAX_MERGE_LAYERS && layers.iter().all(|layer| unit(&layer.opacity))
        }
        NodeOp::ShapeMask { masks, .. } => {
            masks.len() <= crate::MAX_CLIP_MASKS && masks.iter().all(ShapeMask::is_valid)
        }
        NodeOp::Group {
            composition: target,
            inputs,
            controls,
        } => {
            let Some(target) = compositions.get(target) else {
                return Err(ModelError::UnknownGroupComposition {
                    composition: composition.id.clone(),
                    node: node.id.clone(),
                    target: target.clone(),
                });
            };
            inputs.len() == target.inputs.len()
                && controls.iter().all(|(name, value)| {
                    target.controls.iter().any(|control| &control.name == name)
                        && crate::effect::valid_param(value)
                })
        }
        NodeOp::GroupInput { index } => {
            usize::try_from(*index).is_ok_and(|index| index < composition.inputs.len())
        }
    };
    if !valid {
        return Err(invalid());
    }
    if let Some(effect) = node.op.effect()
        && let Some(resource) = effect
            .resources
            .values()
            .find(|resource| !effect_resources.contains(resource))
    {
        return Err(ModelError::UnknownNodeResource {
            composition: composition.id.clone(),
            node: node.id.clone(),
            resource: resource.clone(),
        });
    }
    Ok(())
}

/// A node on a cycle, if the graph has one: the first node, in document
/// order, that a depth-first walk finds on its own path.
fn cycle_member<'a>(
    composition: &'a Composition,
    positions: &std::collections::HashMap<&'a NodeId, usize>,
) -> Option<&'a NodeId> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let nodes = &composition.nodes;
    let mut marks = vec![Mark::New; nodes.len()];
    for root in 0..nodes.len() {
        if marks[root] != Mark::New {
            continue;
        }
        // Each entry is a node and how many of its inputs have been walked.
        let mut stack = vec![(root, 0_usize)];
        marks[root] = Mark::Open;
        while let Some((current, next)) = stack.last_mut() {
            let inputs = nodes[*current].op.inputs();
            let Some(input) = inputs.get(*next) else {
                marks[*current] = Mark::Done;
                stack.pop();
                continue;
            };
            *next += 1;
            let Some(target) = input.node().and_then(|id| positions.get(id)) else {
                continue;
            };
            match marks[*target] {
                Mark::Open => return Some(&nodes[*target].id),
                Mark::New => {
                    marks[*target] = Mark::Open;
                    stack.push((*target, 0));
                }
                Mark::Done => {}
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
