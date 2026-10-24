//! Composition graphs resolved into the steps that draw their output.
//!
//! A composition is compiled once, when the index is built: the nodes its
//! output reaches are put in dependency order, bypassed nodes are replaced by
//! the input they pass through, and every input is resolved to the step that
//! produces it. A group node is replaced by the steps of the composition it
//! draws, its group inputs resolved to the group node's inputs and its
//! controls set on the nodes they expose; a media node that reads a
//! composition source is replaced by that composition's steps, shifted in
//! time by the node's start. The compiled steps are what the flat graph
//! would be: a group draws exactly what its nodes would inline. Evaluating a
//! frame then only samples settings at the clip's composition time, plus
//! each step's offset.
//!
//! Inlining copies: each step holds its own node's settings and path, so a
//! composition drawn many times holds its settings many times. Validation
//! bounds the steps ([`MAX_INLINED_NODES`]), and the planner bounds what
//! they hold: the plans of one project hold at most [`MAX_PLAN_BYTES`].

use alpha2_media::SourceId;
use alpha2_model::{
    Animated, AnimatedPoint, ClipBlendMode, ClipEffect, Composition, CompositionId,
    CompositionReads, EffectValue, Generator, Keyframe, MAX_COMPOSITION_DEPTH, MAX_INLINED_NODES,
    MatteMode, MediaKind, MediaSource, NodeId, NodeOp, Paint, ShapeMask, VisualProperties,
};
use alpha2_time::MediaTime;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::GeneratorPlan;

/// Most bytes the compiled composition plans of one project hold together,
/// as [`Step`]s estimate them: each step, its node path, and the heap its
/// settings use (keyframes, parameter and resource names, masks, layers,
/// gradient stops). A project's plans are those of every composition a clip
/// or composition source shows; past this, none of them is drawn
/// ([`PlanError::TooLarge`]). The steps are what validation counts, at most
/// [`MAX_INLINED_NODES`]; this bounds how large each may be, which inlining
/// multiplies.
pub const MAX_PLAN_BYTES: u64 = 256 * 1024 * 1024;

/// A composition's output, drawn by steps in dependency order.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositionPlan {
    /// The composition.
    pub id: CompositionId,
    /// Steps in dependency order; each step's inputs are earlier steps.
    pub steps: Vec<Step>,
    /// The step whose image is the composition's picture.
    pub output: usize,
}

impl CompositionPlan {
    /// The effect and transition invocations the steps run, in step order.
    pub fn effects(&self) -> impl Iterator<Item = &ClipEffect> {
        self.steps.iter().filter_map(|step| match &step.op {
            StepOp::Effect { effect, .. } | StepOp::Transition { effect, .. } => Some(effect),
            _ => None,
        })
    }

    /// Whether a step draws the picture of the clip showing the plan.
    #[must_use]
    pub fn uses_clip(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step.op, StepOp::Clip))
    }
}

/// One node of a composition, with its inputs resolved to earlier steps.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// The node this step draws: its id, after the ids of the group and
    /// media nodes it was inlined through, joined by `/`.
    pub node: NodeId,
    /// What it draws.
    pub op: StepOp,
    /// Added to the plan's time to sample this step: the starts of the media
    /// nodes whose composition sources it was inlined through.
    pub offset: MediaTime,
}

impl Step {
    /// The time this step's settings are sampled at, when the plan is drawn
    /// at `time`.
    #[must_use]
    pub fn time(&self, time: MediaTime) -> MediaTime {
        MediaTime(time.ticks().saturating_add(self.offset.ticks()))
    }
}

/// The operation of a [`Step`]. Inputs are indices of earlier steps.
#[derive(Clone, Debug, PartialEq)]
pub enum StepOp {
    /// The picture of the clip showing the composition.
    Clip,
    /// A host-supplied source at `start` plus the composition time.
    Media {
        /// The source.
        source_id: SourceId,
        /// Source time at composition time zero.
        start: MediaTime,
    },
    /// A picture the engine draws: a generator node, or a media node naming
    /// a generated source.
    Generator {
        /// What it draws.
        generator: Generator,
        /// The picture's time at composition time zero: a media node's
        /// start, zero for a generator node. Only shapes and layouts animate
        /// with it.
        start: MediaTime,
        /// A shapes or layout generator's tree, its fixed parts evaluated.
        plan: Option<Arc<GeneratorPlan>>,
    },
    /// A registered effect.
    Effect {
        /// The processed image.
        input: usize,
        /// The images it samples besides, by name, in name order.
        inputs: Vec<(String, usize)>,
        /// The effect.
        effect: ClipEffect,
    },
    /// A registered transition at an explicit progress.
    Transition {
        /// The image at progress zero.
        from: usize,
        /// The image at progress one.
        to: usize,
        /// The transition.
        effect: ClipEffect,
        /// Progress in `0..=1`.
        progress: Animated<f32>,
    },
    /// The input placed by a transform.
    Transform {
        /// The placed image.
        input: usize,
        /// The transform.
        transform: VisualProperties,
    },
    /// Layers composited over transparency, bottom first.
    Merge {
        /// The layers.
        layers: Vec<StepLayer>,
    },
    /// The input kept where a second image says.
    Matte {
        /// The kept image.
        input: usize,
        /// The coverage image.
        matte: usize,
        /// Which property of the coverage image counts.
        mode: MatteMode,
    },
    /// The input kept inside shapes.
    ShapeMask {
        /// The masked image.
        input: usize,
        /// The shapes.
        masks: Vec<ShapeMask>,
    },
}

/// One layer of a [`StepOp::Merge`].
#[derive(Clone, Debug, PartialEq)]
pub struct StepLayer {
    /// The layer's image.
    pub input: usize,
    /// Its blend mode.
    pub blend_mode: ClipBlendMode,
    /// Its opacity.
    pub opacity: Animated<f32>,
}

/// Why a composition's output cannot be drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The composition selects no output.
    NoOutput,
    /// A node the output needs has an unconnected input.
    MissingInput {
        /// The node.
        node: NodeId,
        /// The input: `input`, `from`, `to`, `matte`, `layers[i]`,
        /// `inputs[i]`, or `inputs.<name>`.
        input: String,
    },
    /// A group input read where no group node feeds it: in a composition
    /// drawn as a clip's composition or as a source.
    UnboundGroupInput {
        /// The node.
        node: NodeId,
    },
    /// A clip node in a composition drawn as a source, which shows no clip.
    ClipWithoutClip {
        /// The node.
        node: NodeId,
    },
    /// The group or media node draws compositions nested deeper than
    /// [`alpha2_model::MAX_COMPOSITION_DEPTH`]. Validation refuses such a
    /// project, so only an unvalidated one reaches this.
    NestedTooDeep {
        /// The node.
        node: NodeId,
    },
    /// The output needs more than [`alpha2_model::MAX_INLINED_NODES`] steps,
    /// which validation refuses, or the plans of the project hold more than
    /// [`MAX_PLAN_BYTES`].
    TooLarge,
}

/// What compiled plans may still hold, in bytes: one for a plan compiled on
/// its own, shared by the plans of one project.
#[derive(Debug)]
pub(crate) struct PlanBudget {
    left: u64,
    exceeded: bool,
}

impl PlanBudget {
    pub(crate) const fn new(bytes: u64) -> Self {
        Self {
            left: bytes,
            exceeded: false,
        }
    }

    /// Whether a plan needed more than was left.
    pub(crate) const fn exceeded(&self) -> bool {
        self.exceeded
    }
}

/// What planning looks up: a project's compositions and sources by ID, and
/// what each composition reads. Built once for all the plans of a project.
pub(crate) struct Lookup<'a> {
    compositions: HashMap<&'a CompositionId, &'a Composition>,
    sources: HashMap<&'a SourceId, &'a MediaSource>,
    reads: CompositionReads<'a>,
}

impl<'a> Lookup<'a> {
    pub(crate) fn new(compositions: &'a [Composition], sources: &'a [MediaSource]) -> Self {
        let compositions = compositions
            .iter()
            .map(|composition| (&composition.id, composition))
            .collect();
        let reads = CompositionReads::new(&compositions);
        Self {
            compositions,
            sources: sources.iter().map(|source| (&source.id, source)).collect(),
            reads,
        }
    }

    /// What each composition reads.
    pub(crate) const fn reads(&self) -> &CompositionReads<'a> {
        &self.reads
    }
}

/// Compiles `composition`'s output into steps, as a clip's composition: its
/// clip nodes draw the clip's picture. `compositions` resolves group nodes
/// and `sources` media nodes, which may name generated or composition
/// sources.
///
/// The project must be valid ([`alpha2_model::Project::validate`]): every
/// input names a node, there is no cycle, and no composition uses itself.
///
/// The plan holds at most [`MAX_PLAN_BYTES`]; an index holds every plan of
/// a project within that together.
///
/// # Errors
/// Returns [`PlanError`] when there is no output, the output needs an input
/// that is not connected or a group input no group node feeds, or the plan
/// nests too deep or grows too large.
pub fn plan_composition(
    composition: &Composition,
    compositions: &[Composition],
    sources: &[MediaSource],
) -> Result<CompositionPlan, PlanError> {
    plan(
        composition,
        &Lookup::new(compositions, sources),
        true,
        &mut PlanBudget::new(MAX_PLAN_BYTES),
    )
}

/// [`plan_composition`] with what `lookup` holds, within what `budget` has
/// left, taking what the plan holds from it.
pub(crate) fn plan_composition_within(
    composition: &Composition,
    lookup: &Lookup<'_>,
    budget: &mut PlanBudget,
) -> Result<CompositionPlan, PlanError> {
    plan(composition, lookup, true, budget)
}

/// Compiles `composition` as a composition source draws it: like
/// [`plan_composition`], but a clip node or group input is an error, since
/// no clip or group node feeds it.
///
/// # Errors
/// Returns [`PlanError`] as [`plan_composition`] does, and for a clip node
/// or group input the output needs.
pub fn plan_source_composition(
    composition: &Composition,
    compositions: &[Composition],
    sources: &[MediaSource],
) -> Result<CompositionPlan, PlanError> {
    plan(
        composition,
        &Lookup::new(compositions, sources),
        false,
        &mut PlanBudget::new(MAX_PLAN_BYTES),
    )
}

/// [`plan_source_composition`] with what `lookup` holds, within what
/// `budget` has left, taking what the plan holds from it.
pub(crate) fn plan_source_composition_within(
    composition: &Composition,
    lookup: &Lookup<'_>,
    budget: &mut PlanBudget,
) -> Result<CompositionPlan, PlanError> {
    plan(composition, lookup, false, budget)
}

fn plan<'a>(
    composition: &'a Composition,
    lookup: &Lookup<'a>,
    clip_allowed: bool,
    budget: &mut PlanBudget,
) -> Result<CompositionPlan, PlanError> {
    let mut planner = Planner {
        lookup,
        steps: Vec::new(),
        budget,
    };
    let scope = Scope {
        composition,
        inputs: Vec::new(),
        controls: HashMap::new(),
        offset: MediaTime::ZERO,
        prefix: String::new(),
        clip_allowed,
        group_inputs_bound: false,
        depth: 1,
    };
    let output = planner.output(&scope)?;
    Ok(CompositionPlan {
        id: composition.id.clone(),
        steps: planner.steps,
        output,
    })
}

/// One composition as it is being drawn: inline in another, or on its own.
struct Scope<'a> {
    composition: &'a Composition,
    /// The step each group input reads: none for one its composition does
    /// not read.
    inputs: Vec<Option<usize>>,
    /// Values its group node sets, by node and parameter.
    controls: HashMap<(&'a NodeId, &'a str), &'a Animated<EffectValue>>,
    /// Added to the plan's time for its steps.
    offset: MediaTime,
    /// Prepended to its node ids in the steps.
    prefix: String,
    /// Whether a clip node draws the clip's picture here.
    clip_allowed: bool,
    /// Whether a group node feeds its group inputs.
    group_inputs_bound: bool,
    /// How deeply it is nested: one for the plan's own composition.
    depth: usize,
}

struct Planner<'a, 'b> {
    lookup: &'b Lookup<'a>,
    steps: Vec<Step>,
    budget: &'b mut PlanBudget,
}

impl<'a> Planner<'a, '_> {
    /// The step drawing `scope`'s output.
    fn output(&mut self, scope: &Scope<'a>) -> Result<usize, PlanError> {
        let output = scope
            .composition
            .output
            .as_ref()
            .ok_or(PlanError::NoOutput)?;
        let nodes: HashMap<&NodeId, &'a alpha2_model::CompositionNode> = scope
            .composition
            .nodes
            .iter()
            .map(|node| (&node.id, node))
            .collect();
        self.resolve(scope, &nodes, output)
    }

    /// The step drawing node `id` of `scope`, planning it and what it needs
    /// first.
    ///
    /// Validation has ruled out cycles, so the walk ends; it is iterative so
    /// that a long chain cannot exhaust the stack. Nesting recurses once per
    /// group or composition source, at most
    /// [`alpha2_model::MAX_COMPOSITION_DEPTH`] deep, and the steps number at
    /// most [`alpha2_model::MAX_INLINED_NODES`]: validation bounds both, and
    /// the planner refuses to go further for a project that skipped it.
    fn resolve(
        &mut self,
        scope: &Scope<'a>,
        nodes: &HashMap<&NodeId, &'a alpha2_model::CompositionNode>,
        id: &'a NodeId,
    ) -> Result<usize, PlanError> {
        let mut done: HashMap<&NodeId, usize> = HashMap::new();
        // Each entry is a node whose needed inputs are pushed before it is revisited.
        let mut pending = vec![(id, false)];
        while let Some((current, expanded)) = pending.pop() {
            if done.contains_key(current) {
                continue;
            }
            let node = nodes[current];
            let needed = needed_inputs(self.lookup.reads(), &scope.prefix, node)?;
            if !expanded {
                pending.push((current, true));
                pending.extend(
                    needed
                        .iter()
                        .rev()
                        .filter(|input| !done.contains_key(**input))
                        .map(|input| (*input, false)),
                );
                continue;
            }
            let step = if node.bypass {
                done[needed[0]]
            } else {
                self.node_step(scope, node, &done)?
            };
            done.insert(current, step);
        }
        Ok(done[id])
    }

    /// The step (or inlined steps) drawing `node`, its inputs planned.
    fn node_step(
        &mut self,
        scope: &Scope<'a>,
        node: &'a alpha2_model::CompositionNode,
        done: &HashMap<&NodeId, usize>,
    ) -> Result<usize, PlanError> {
        let at = |input: &Option<NodeId>| {
            input
                .as_ref()
                .map(|id| done[id])
                .expect("needed inputs are connected and planned")
        };
        let id = || scoped(&scope.prefix, &node.id);
        let op = match &node.op {
            NodeOp::Clip {} => {
                if !scope.clip_allowed {
                    return Err(PlanError::ClipWithoutClip { node: id() });
                }
                StepOp::Clip
            }
            NodeOp::GroupInput { index } => {
                // Validation keeps the index within the composition's ports.
                return usize::try_from(*index)
                    .ok()
                    .and_then(|index| scope.inputs.get(index).copied().flatten())
                    .filter(|_| scope.group_inputs_bound)
                    .ok_or_else(|| PlanError::UnboundGroupInput { node: id() });
            }
            NodeOp::Group {
                composition,
                inputs,
                controls,
            } => {
                // Only the ports its composition reads are needed, and planned.
                let reads = self.lookup.reads();
                let inputs = inputs
                    .iter()
                    .enumerate()
                    .map(|(port, input)| reads.reads_port(composition, port).then(|| at(input)))
                    .collect();
                return self.group(scope, &id(), composition, inputs, controls);
            }
            NodeOp::Media { source_id, start } => {
                let source = self.lookup.sources.get(source_id).copied();
                if let Some(composition) = source.and_then(|source| source.composition.as_ref()) {
                    return self.composition_source(scope, &id(), composition, *start);
                }
                let generator = source
                    .filter(|source| source.kind == MediaKind::Generated)
                    .and_then(|source| source.generator.clone());
                generator.map_or_else(
                    || StepOp::Media {
                        source_id: source_id.clone(),
                        start: *start,
                    },
                    |generator| generator_step(generator, *start),
                )
            }
            NodeOp::Generator { generator } => generator_step(generator.clone(), MediaTime::ZERO),
            NodeOp::Effect {
                input,
                inputs,
                effect,
            } => StepOp::Effect {
                input: at(input),
                inputs: named_steps(inputs, &at),
                effect: controlled(scope, &node.id, effect),
            },
            NodeOp::Transition {
                from,
                to,
                effect,
                progress,
            } => StepOp::Transition {
                from: at(from),
                to: at(to),
                effect: controlled(scope, &node.id, effect),
                progress: progress.clone(),
            },
            NodeOp::Transform { input, transform } => StepOp::Transform {
                input: at(input),
                transform: transform.clone(),
            },
            NodeOp::Merge { layers } => StepOp::Merge {
                layers: layers
                    .iter()
                    .map(|layer| StepLayer {
                        input: at(&layer.input),
                        blend_mode: layer.blend_mode,
                        opacity: layer.opacity.clone(),
                    })
                    .collect(),
            },
            NodeOp::Matte { input, matte, mode } => StepOp::Matte {
                input: at(input),
                matte: at(matte),
                mode: *mode,
            },
            NodeOp::ShapeMask { input, masks } => StepOp::ShapeMask {
                input: at(input),
                masks: masks.clone(),
            },
        };
        self.push(Step {
            node: id(),
            op,
            offset: scope.offset,
        })
    }

    /// Adds `step` to the plan, within [`MAX_INLINED_NODES`] steps and what
    /// the budget has left.
    fn push(&mut self, step: Step) -> Result<usize, PlanError> {
        if self.steps.len() >= MAX_INLINED_NODES {
            return Err(PlanError::TooLarge);
        }
        let bytes = step_bytes(&step);
        if bytes > self.budget.left {
            self.budget.exceeded = true;
            return Err(PlanError::TooLarge);
        }
        self.budget.left -= bytes;
        self.steps.push(step);
        Ok(self.steps.len() - 1)
    }

    /// The steps of the composition a group node draws: its group inputs
    /// read `inputs`, and the controls it sets change its nodes' settings.
    fn group(
        &mut self,
        scope: &Scope<'a>,
        node: &NodeId,
        composition: &CompositionId,
        inputs: Vec<Option<usize>>,
        controls: &'a std::collections::BTreeMap<String, Animated<EffectValue>>,
    ) -> Result<usize, PlanError> {
        let target = self.composition(composition);
        let depth = nested(scope, node)?;
        let controls = target
            .controls
            .iter()
            .filter_map(|control| {
                controls
                    .get(&control.name)
                    .map(|value| ((&control.node, control.param.as_str()), value))
            })
            .collect();
        self.output(&Scope {
            composition: target,
            inputs,
            controls,
            offset: scope.offset,
            prefix: format!("{node}/"),
            clip_allowed: scope.clip_allowed,
            group_inputs_bound: true,
            depth,
        })
    }

    /// The steps of a composition a media node reads through a composition
    /// source, `start` later in time.
    fn composition_source(
        &mut self,
        scope: &Scope<'a>,
        node: &NodeId,
        composition: &CompositionId,
        start: MediaTime,
    ) -> Result<usize, PlanError> {
        let depth = nested(scope, node)?;
        self.output(&Scope {
            composition: self.composition(composition),
            inputs: Vec::new(),
            controls: HashMap::new(),
            offset: MediaTime(scope.offset.ticks().saturating_add(start.ticks())),
            prefix: format!("{node}/"),
            clip_allowed: false,
            group_inputs_bound: false,
            depth,
        })
    }

    fn composition(&self, id: &CompositionId) -> &'a Composition {
        self.lookup
            .compositions
            .get(id)
            .copied()
            .expect("validation guarantees referenced compositions exist")
    }
}

/// The depth of a composition `node` draws inside `scope`, if within
/// [`MAX_COMPOSITION_DEPTH`].
fn nested(scope: &Scope<'_>, node: &NodeId) -> Result<usize, PlanError> {
    let depth = scope.depth + 1;
    if depth > MAX_COMPOSITION_DEPTH {
        return Err(PlanError::NestedTooDeep { node: node.clone() });
    }
    Ok(depth)
}

/// `id` after `prefix`.
fn scoped(prefix: &str, id: &NodeId) -> NodeId {
    if prefix.is_empty() {
        return id.clone();
    }
    NodeId::new(format!("{prefix}{id}")).expect("a prefixed id is not empty")
}

/// An effect node's named inputs, each with the step drawing it, in name
/// order.
fn named_steps(
    inputs: &BTreeMap<String, Option<NodeId>>,
    at: &dyn Fn(&Option<NodeId>) -> usize,
) -> Vec<(String, usize)> {
    inputs
        .iter()
        .map(|(name, node)| (name.clone(), at(node)))
        .collect()
}

/// `effect` with the parameters its group node sets.
fn controlled(scope: &Scope<'_>, node: &NodeId, effect: &ClipEffect) -> ClipEffect {
    let mut effect = effect.clone();
    for ((target, param), value) in &scope.controls {
        if *target == node {
            effect.params.insert((*param).to_owned(), (*value).clone());
        }
    }
    effect
}

/// What `step` holds, as [`MAX_PLAN_BYTES`] counts it.
fn step_bytes(step: &Step) -> u64 {
    let heap = match &step.op {
        StepOp::Clip | StepOp::Matte { .. } => 0,
        StepOp::Media { source_id, .. } => source_id.as_str().len(),
        StepOp::Generator { generator, .. } => generator_bytes(generator),
        StepOp::Effect { effect, inputs, .. } => {
            effect_bytes(effect)
                + inputs
                    .iter()
                    .map(|(name, _)| name.len() + size_of::<(String, usize)>())
                    .sum::<usize>()
        }
        StepOp::Transition {
            effect, progress, ..
        } => effect_bytes(effect) + keyframe_bytes(progress),
        StepOp::Transform { transform, .. } => transform_bytes(transform),
        StepOp::Merge { layers } => layers
            .iter()
            .map(|layer| size_of::<StepLayer>() + keyframe_bytes(&layer.opacity))
            .sum(),
        StepOp::ShapeMask { masks, .. } => masks.iter().map(mask_bytes).sum(),
    };
    u64::try_from(size_of::<Step>() + step.node.as_str().len() + heap).unwrap_or(u64::MAX)
}

/// The heap an animated value's keyframes take; none for a constant.
fn keyframe_bytes<T>(value: &Animated<T>) -> usize {
    match value {
        Animated::Constant { .. } => 0,
        Animated::Keyframes { keyframes } => keyframes.len() * size_of::<Keyframe<T>>(),
    }
}

fn effect_bytes(effect: &ClipEffect) -> usize {
    effect.id.len()
        + effect
            .params
            .iter()
            .map(|(name, value)| {
                name.len() + size_of::<Animated<EffectValue>>() + keyframe_bytes(value)
            })
            .sum::<usize>()
        + effect
            .resources
            .iter()
            .map(|(name, id)| name.len() + id.as_str().len())
            .sum::<usize>()
}

fn transform_bytes(transform: &VisualProperties) -> usize {
    point_bytes(&transform.position)
        + keyframe_bytes(&transform.scale)
        + keyframe_bytes(&transform.rotation_degrees)
        + keyframe_bytes(&transform.opacity)
}

fn mask_bytes(mask: &ShapeMask) -> usize {
    size_of::<ShapeMask>() + geometry_bytes(&mask.shape) + keyframe_bytes(&mask.feather)
}

fn point_bytes(point: &AnimatedPoint) -> usize {
    match point {
        AnimatedPoint::Constant { .. } => 0,
        AnimatedPoint::Keyframes { keyframes } => {
            keyframes.len() * size_of::<alpha2_model::PointKeyframe>()
        }
    }
}

fn path_bytes(path: &Animated<alpha2_model::Path>) -> usize {
    let one = |path: &alpha2_model::Path| {
        path.subpaths.len() * size_of::<alpha2_model::Subpath>()
            + path.vertex_count() * size_of::<alpha2_model::PathVertex>()
    };
    match path {
        Animated::Constant { value } => one(value),
        Animated::Keyframes { keyframes } => keyframes
            .iter()
            .map(|keyframe| size_of::<Keyframe<alpha2_model::Path>>() + one(&keyframe.value))
            .sum(),
    }
}

fn geometry_bytes(geometry: &alpha2_model::ShapeGeometry) -> usize {
    use alpha2_model::ShapeGeometry;
    match geometry {
        ShapeGeometry::Rectangle(rectangle) => {
            point_bytes(&rectangle.center)
                + keyframe_bytes(&rectangle.size)
                + keyframe_bytes(&rectangle.corner_radius)
                + keyframe_bytes(&rectangle.rotation_degrees)
        }
        ShapeGeometry::Ellipse(ellipse) => {
            point_bytes(&ellipse.center)
                + keyframe_bytes(&ellipse.size)
                + keyframe_bytes(&ellipse.rotation_degrees)
        }
        ShapeGeometry::Polystar(star) => {
            point_bytes(&star.center)
                + keyframe_bytes(&star.points)
                + keyframe_bytes(&star.outer_radius)
                + keyframe_bytes(&star.inner_radius)
                + keyframe_bytes(&star.outer_roundness)
                + keyframe_bytes(&star.inner_roundness)
                + keyframe_bytes(&star.rotation_degrees)
        }
        ShapeGeometry::Path(path) => path_bytes(&path.path),
    }
}

fn paint_bytes(paint: &Paint) -> usize {
    match paint {
        Paint::Solid { .. } => 0,
        Paint::LinearGradient { stops, .. }
        | Paint::RadialGradient { stops, .. }
        | Paint::ConicGradient { stops, .. } => {
            stops.len() * size_of::<alpha2_model::GradientStop>()
        }
    }
}

fn animated_paint_bytes(paint: &Animated<Paint>) -> usize {
    match paint {
        Animated::Constant { value } => paint_bytes(value),
        Animated::Keyframes { keyframes } => keyframes
            .iter()
            .map(|keyframe| size_of::<Keyframe<Paint>>() + paint_bytes(&keyframe.value))
            .sum(),
    }
}

/// What a content tree holds: each item, its keyframes, vertices, and
/// stops. Its plan holds about as much again.
fn shape_items_bytes(items: &[alpha2_model::ShapeItem]) -> usize {
    use alpha2_model::{ShapeGeometry, ShapeItem};
    items
        .iter()
        .map(|item| {
            size_of::<ShapeItem>()
                + match item {
                    ShapeItem::Group { items, transform } => {
                        shape_items_bytes(items)
                            + keyframe_bytes(&transform.anchor)
                            + point_bytes(&transform.position)
                            + keyframe_bytes(&transform.scale)
                            + keyframe_bytes(&transform.rotation_degrees)
                            + keyframe_bytes(&transform.opacity)
                    }
                    ShapeItem::Rectangle(geometry) => {
                        geometry_bytes(&ShapeGeometry::Rectangle(geometry.clone()))
                    }
                    ShapeItem::Ellipse(geometry) => {
                        geometry_bytes(&ShapeGeometry::Ellipse(geometry.clone()))
                    }
                    ShapeItem::Polystar(geometry) => {
                        geometry_bytes(&ShapeGeometry::Polystar(geometry.clone()))
                    }
                    ShapeItem::Path(geometry) => path_bytes(&geometry.path),
                    ShapeItem::Fill { paint, opacity, .. } => {
                        animated_paint_bytes(paint) + keyframe_bytes(opacity)
                    }
                    ShapeItem::Stroke(stroke) => {
                        animated_paint_bytes(&stroke.paint)
                            + keyframe_bytes(&stroke.width)
                            + keyframe_bytes(&stroke.opacity)
                            + stroke.dash.as_ref().map_or(0, |dash| {
                                dash.pattern
                                    .iter()
                                    .map(|entry| size_of::<Animated<f32>>() + keyframe_bytes(entry))
                                    .sum::<usize>()
                                    + keyframe_bytes(&dash.offset)
                            })
                    }
                    ShapeItem::Trim(trim) => {
                        keyframe_bytes(&trim.start)
                            + keyframe_bytes(&trim.end)
                            + keyframe_bytes(&trim.offset)
                    }
                }
        })
        .sum()
}

/// A generator step, with a plan when the generator is a shapes or layout
/// generator.
fn generator_step(generator: Generator, start: MediaTime) -> StepOp {
    let plan = GeneratorPlan::new(&generator).map(Arc::new);
    StepOp::Generator {
        generator,
        start,
        plan,
    }
}

fn generator_bytes(generator: &Generator) -> usize {
    match generator {
        Generator::Fill { paint } => paint_bytes(paint),
        // Twice: the items, and their plan.
        Generator::Shapes { items } => 2 * shape_items_bytes(items),
        Generator::Text { text, style } => {
            text.len() + style.font_family.as_ref().map_or(0, String::len)
        }
        // Twice: the tree, and its plan.
        Generator::Layout { root } => 2 * crate::layout::layout_bytes(root, &shape_items_bytes),
    }
}

/// The nodes a node needs, in input order: its
/// [`CompositionReads::needed_inputs`], each connected. A missing one is
/// named as the steps name the node: after `prefix`, the group or media node
/// drawing its composition, like every plan error.
fn needed_inputs<'a>(
    reads: &CompositionReads<'_>,
    prefix: &str,
    node: &'a alpha2_model::CompositionNode,
) -> Result<Vec<&'a NodeId>, PlanError> {
    let needed = reads.needed_inputs(node);
    if node.bypass && needed.is_empty() {
        // A merge without layers has nothing to pass through.
        return Err(PlanError::MissingInput {
            node: scoped(prefix, &node.id),
            input: "layers[0]".into(),
        });
    }
    needed
        .into_iter()
        .map(|input| {
            input.node().ok_or_else(|| PlanError::MissingInput {
                node: scoped(prefix, &node.id),
                input: input.name(),
            })
        })
        .collect()
}

/// A clip's compiled composition and how its time maps onto it.
#[derive(Clone, Debug)]
pub(crate) struct IndexedComposition {
    pub(crate) plan: Arc<Result<CompositionPlan, PlanError>>,
    pub(crate) reference: alpha2_model::ClipComposition,
}

/// A clip's composition at one timeline timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveComposition<'a> {
    plan: &'a CompositionPlan,
    time: MediaTime,
}

impl<'a> ActiveComposition<'a> {
    /// `plan` at composition time `time`.
    #[must_use]
    pub const fn new(plan: &'a CompositionPlan, time: MediaTime) -> Self {
        Self { plan, time }
    }

    /// The compiled composition.
    #[must_use]
    pub const fn plan(&self) -> &'a CompositionPlan {
        self.plan
    }

    /// The composition time at which settings are sampled.
    #[must_use]
    pub const fn time(&self) -> MediaTime {
        self.time
    }

    /// Each host-supplied source a step reads, with the source time it
    /// reads, by step index.
    pub fn media(&self) -> impl Iterator<Item = (usize, &'a SourceId, MediaTime)> + 'a {
        let time = self.time;
        self.plan
            .steps
            .iter()
            .enumerate()
            .filter_map(move |(index, step)| match &step.op {
                StepOp::Media { source_id, start } => Some((
                    index,
                    source_id,
                    MediaTime(start.ticks().saturating_add(step.time(time).ticks())),
                )),
                _ => None,
            })
    }
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
