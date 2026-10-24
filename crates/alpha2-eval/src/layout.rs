//! A layout generator's box tree evaluated at a time.
//!
//! A [`LayoutPlan`] is made once, when the index is built. A tree with no
//! keyframes is evaluated then and kept, shared, so every frame gets the
//! same value and the same key, and the engine resolves it once.

use crate::ShapesPlan;
use crate::animation::{
    sample_hold, sample_insets, sample_length, sample_paint, sample_scalar, sample_vec2,
};
use alpha2_geometry::StableHash;
use alpha2_model::{
    AlignItems, Animated, FlexDirection, FlexWrap, Insets, JustifyContent, LayoutLength,
    LayoutNode, Overflow, Paint, Position, Rgba, TextAlign, TextOutline, Vec2,
};
use alpha2_time::MediaTime;
use std::sync::Arc;

/// A layout tree, evaluated once when it does not animate.
#[derive(Clone, Debug)]
pub struct LayoutPlan {
    root: Box<LayoutNode>,
    /// Each shape node's plan, in preorder, with a digest of its items.
    shapes: Vec<(Arc<ShapesPlan>, u128)>,
    /// Whether a shape node animates, so the time is part of the key.
    shapes_animate: bool,
    /// The whole tree, when nothing in it animates.
    fixed: Option<Arc<EvaluatedLayout>>,
}

impl PartialEq for LayoutPlan {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl LayoutPlan {
    /// Plans `root`, evaluating it when nothing in it animates.
    #[must_use]
    pub fn new(root: &LayoutNode) -> Self {
        let mut shapes = Vec::new();
        root.walk(|_, node| {
            if let Some(shape) = &node.shape {
                let mut hash = StableHash::new();
                hash.bytes(format!("{:?}", shape.items).as_bytes());
                shapes.push((Arc::new(ShapesPlan::new(&shape.items)), hash.finish()));
            }
        });
        let shapes_animate = shapes.iter().any(|(plan, _)| plan.is_animated());
        let mut plan = Self {
            root: Box::new(root.clone()),
            shapes,
            shapes_animate,
            fixed: None,
        };
        if !root.is_animated() {
            plan.fixed = Some(Arc::new(plan.evaluate_now(MediaTime::ZERO)));
        }
        plan
    }

    /// The tree at picture time `time`; the same shared value at every time
    /// when nothing animates.
    #[must_use]
    pub fn evaluate(&self, time: MediaTime) -> Arc<EvaluatedLayout> {
        self.fixed
            .clone()
            .unwrap_or_else(|| Arc::new(self.evaluate_now(time)))
    }

    /// Whether anything in the tree animates.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        self.fixed.is_none()
    }

    /// The model tree the plan was made from.
    #[must_use]
    pub const fn root(&self) -> &LayoutNode {
        &self.root
    }

    fn evaluate_now(&self, time: MediaTime) -> EvaluatedLayout {
        let mut shapes = self.shapes.iter();
        let root = evaluate_node(&self.root, time, &mut shapes);
        let mut hash = StableHash::new();
        hash_node(&mut hash, &root, &self.shapes);
        if self.shapes_animate {
            hash.u64(time.ticks().cast_unsigned());
        }
        EvaluatedLayout {
            root,
            time,
            key: hash.finish(),
        }
    }
}

/// A layout tree with every value sampled.
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluatedLayout {
    /// The root box: the whole picture.
    pub root: EvaluatedNode,
    /// The picture time shape nodes are drawn at.
    pub time: MediaTime,
    /// Equal for trees that lay out and draw alike: every sampled value, and
    /// `time` when a shape node animates.
    pub key: u128,
}

/// One box, its values sampled. Lengths are fractions of the picture's
/// shorter side, as in the model.
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluatedNode {
    /// What flexbox reads.
    pub layout: NodeLayout,
    /// What is drawn under the content.
    pub paint: NodePaint,
    /// Present when the node has any visual field, with defaults filled.
    pub visual: Option<NodeVisual>,
    /// Whether content past the padding box is clipped.
    pub clip: bool,
    /// Children, text, a shape, or nothing.
    pub content: NodeContent,
}

impl EvaluatedNode {
    /// Whether the node is drawn into its own composite.
    #[must_use]
    pub const fn is_isolated(&self) -> bool {
        self.visual.is_some() || self.clip
    }
}

/// A box's sampled box-model and flex values.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct NodeLayout {
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub gap: f32,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub padding: Insets,
    /// The border's width; zero without one.
    pub border_width: f32,
    pub overflow: Overflow,
    pub width: Option<LayoutLength>,
    pub height: Option<LayoutLength>,
    pub min_width: Option<LayoutLength>,
    pub min_height: Option<LayoutLength>,
    pub max_width: Option<LayoutLength>,
    pub max_height: Option<LayoutLength>,
    pub aspect_ratio: Option<f32>,
    pub margin: Insets,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Option<LayoutLength>,
    pub align_self: Option<AlignItems>,
    pub position: Position,
    /// Left, top, right, bottom; `None` is auto.
    pub inset: [Option<LayoutLength>; 4],
}

/// What a box draws under its content.
#[derive(Clone, Debug, PartialEq)]
pub struct NodePaint {
    /// What covers the border box.
    pub background: Option<Paint>,
    /// The border's width and paint.
    pub border: Option<(f32, Paint)>,
    /// The border box's corner radius.
    pub border_radius: f32,
}

/// How a box and its subtree are drawn after layout.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct NodeVisual {
    /// Group opacity, in `0..=1`.
    pub opacity: f32,
    /// Offset, a fraction of the picture's shorter side.
    pub translate: Vec2,
    /// Per-axis scale about the border-box centre.
    pub scale: Vec2,
    /// Clockwise rotation about the border-box centre, in degrees.
    pub rotation_degrees: f32,
}

/// A box's content.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeContent {
    /// Nothing: a spacer or a decoration.
    Empty,
    /// Boxes laid out in this one, in document order.
    Children(Vec<EvaluatedNode>),
    /// One paragraph of text.
    Text(EvaluatedText),
    /// Shapes, evaluated at draw time: spatial curves need the node's
    /// aspect, known after layout.
    Shape(Arc<ShapesPlan>),
}

/// A text node's sampled text and style.
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluatedText {
    /// The text; may be empty.
    pub content: String,
    /// The family it draws in.
    pub font_family: Option<String>,
    /// Font size, a fraction of the picture's shorter side; positive.
    pub font_size: f32,
    /// Text color.
    pub color: Rgba,
    /// Baseline-to-baseline distance in font sizes.
    pub line_height: f32,
    /// How lines align in the content width.
    pub text_align: TextAlign,
    /// A band around the glyphs.
    pub outline: Option<TextOutline>,
}

fn non_negative(animation: Option<&Animated<f32>>, time: MediaTime, default: f32) -> f32 {
    animation.map_or(default, |value| sample_scalar(value, time).max(0.0))
}

fn length(animation: Option<&Animated<LayoutLength>>, time: MediaTime) -> Option<LayoutLength> {
    animation.map(|value| sample_length(value, time))
}

fn size(animation: Option<&Animated<LayoutLength>>, time: MediaTime) -> Option<LayoutLength> {
    length(animation, time).map(|length| match length {
        LayoutLength::Length { value } => LayoutLength::Length {
            value: value.max(0.0),
        },
        LayoutLength::Percent { value } => LayoutLength::Percent {
            value: value.max(0.0),
        },
    })
}

/// What flexbox reads of `node` at `time`; `border_width` is the sampled border's.
fn node_layout(node: &LayoutNode, time: MediaTime, border_width: f32) -> NodeLayout {
    let insets = |value: Option<&Animated<Insets>>| {
        value.map_or_else(Insets::default, |value| sample_insets(value, time))
    };
    let padding = insets(node.padding.as_ref());
    let padding = Insets {
        top: padding.top.max(0.0),
        right: padding.right.max(0.0),
        bottom: padding.bottom.max(0.0),
        left: padding.left.max(0.0),
    };
    let inset = node.inset.as_ref().map_or([None; 4], |inset| {
        [
            length(inset.left.as_ref(), time),
            length(inset.top.as_ref(), time),
            length(inset.right.as_ref(), time),
            length(inset.bottom.as_ref(), time),
        ]
    });
    NodeLayout {
        flex_direction: node.flex_direction.unwrap_or_default(),
        flex_wrap: node.flex_wrap.unwrap_or_default(),
        gap: non_negative(node.gap.as_ref(), time, 0.0),
        justify_content: node.justify_content.unwrap_or_default(),
        align_items: node.align_items.unwrap_or_default(),
        padding,
        border_width,
        overflow: node.overflow.unwrap_or_default(),
        width: size(node.width.as_ref(), time),
        height: size(node.height.as_ref(), time),
        min_width: size(node.min_width.as_ref(), time),
        min_height: size(node.min_height.as_ref(), time),
        max_width: size(node.max_width.as_ref(), time),
        max_height: size(node.max_height.as_ref(), time),
        aspect_ratio: node.aspect_ratio,
        margin: insets(node.margin.as_ref()),
        flex_grow: non_negative(node.flex_grow.as_ref(), time, 0.0),
        flex_shrink: non_negative(node.flex_shrink.as_ref(), time, 1.0),
        flex_basis: size(node.flex_basis.as_ref(), time),
        align_self: node.align_self,
        position: node.position.unwrap_or_default(),
        inset,
    }
}

fn evaluate_node<'a>(
    node: &LayoutNode,
    time: MediaTime,
    shapes: &mut impl Iterator<Item = &'a (Arc<ShapesPlan>, u128)>,
) -> EvaluatedNode {
    let border = node.border.as_ref().map(|border| {
        (
            sample_scalar(&border.width, time).max(0.0),
            sample_paint(&border.paint, time),
        )
    });
    let layout = node_layout(node, time, border.as_ref().map_or(0.0, |(width, _)| *width));
    let paint = NodePaint {
        background: node
            .background
            .as_ref()
            .map(|paint| sample_paint(paint, time)),
        border,
        border_radius: non_negative(node.border_radius.as_ref(), time, 0.0),
    };
    let visual = node.has_visual().then(|| NodeVisual {
        opacity: node
            .opacity
            .as_ref()
            .map_or(1.0, |value| sample_scalar(value, time).clamp(0.0, 1.0)),
        translate: node
            .translate
            .as_ref()
            .map_or_else(Vec2::default, |value| sample_vec2(value, time)),
        scale: node
            .scale
            .as_ref()
            .map_or(Vec2::new(1.0, 1.0), |value| sample_vec2(value, time)),
        rotation_degrees: node
            .rotation_degrees
            .as_ref()
            .map_or(0.0, |value| sample_scalar(value, time)),
    });
    let content = if let Some(text) = &node.text {
        NodeContent::Text(EvaluatedText {
            content: sample_hold(&text.content, time),
            font_family: text.font_family.clone(),
            font_size: sample_scalar(&text.font_size, time).max(f32::MIN_POSITIVE),
            color: text.color,
            line_height: text.line_height,
            text_align: text.text_align,
            outline: text.outline,
        })
    } else if node.shape.is_some() {
        let (plan, _) = shapes.next().expect("one plan per shape node");
        NodeContent::Shape(Arc::clone(plan))
    } else if node.children.is_empty() {
        NodeContent::Empty
    } else {
        NodeContent::Children(
            node.children
                .iter()
                .map(|child| evaluate_node(child, time, shapes))
                .collect(),
        )
    };
    EvaluatedNode {
        layout,
        paint,
        visual,
        clip: matches!(node.overflow, Some(Overflow::Hidden)),
        content,
    }
}

fn hash_length(hash: &mut StableHash, length: Option<LayoutLength>) {
    match length {
        None => hash.u32(0),
        Some(LayoutLength::Length { value }) => {
            hash.u32(1);
            hash.f32(value);
        }
        Some(LayoutLength::Percent { value }) => {
            hash.u32(2);
            hash.f32(value);
        }
    }
}

fn hash_insets(hash: &mut StableHash, insets: Insets) {
    for side in [insets.top, insets.right, insets.bottom, insets.left] {
        hash.f32(side);
    }
}

fn hash_color(hash: &mut StableHash, color: Rgba) {
    for component in [color.red, color.green, color.blue, color.alpha] {
        hash.f32(component);
    }
}

fn hash_paint(hash: &mut StableHash, paint: Option<&Paint>) {
    let stops = |hash: &mut StableHash, stops: &[alpha2_model::GradientStop]| {
        hash.u64(stops.len() as u64);
        for stop in stops {
            hash.f32(stop.offset);
            hash_color(hash, stop.color);
        }
    };
    match paint {
        None => hash.u32(0),
        Some(Paint::Solid { color }) => {
            hash.u32(1);
            hash_color(hash, *color);
        }
        Some(Paint::LinearGradient {
            start,
            end,
            stops: list,
            spread,
        }) => {
            hash.u32(2);
            for value in [start.x, start.y, end.x, end.y] {
                hash.f32(value);
            }
            stops(hash, list);
            hash.u32(*spread as u32);
        }
        Some(Paint::RadialGradient {
            center,
            radius,
            stops: list,
            spread,
        }) => {
            hash.u32(3);
            for value in [center.x, center.y, *radius] {
                hash.f32(value);
            }
            stops(hash, list);
            hash.u32(*spread as u32);
        }
        Some(Paint::ConicGradient {
            center,
            angle_degrees,
            stops: list,
        }) => {
            hash.u32(4);
            for value in [center.x, center.y, *angle_degrees] {
                hash.f32(value);
            }
            stops(hash, list);
        }
    }
}

fn hash_node(hash: &mut StableHash, node: &EvaluatedNode, shapes: &[(Arc<ShapesPlan>, u128)]) {
    let mut shape_index = 0;
    hash_node_in(hash, node, shapes, &mut shape_index);
}

fn hash_layout(hash: &mut StableHash, layout: &NodeLayout) {
    for value in [
        layout.flex_direction as u32,
        layout.flex_wrap as u32,
        layout.justify_content as u32,
        layout.align_items as u32,
        layout.overflow as u32,
        layout.position as u32,
        layout.align_self.map_or(u32::MAX, |align| align as u32),
    ] {
        hash.u32(value);
    }
    for value in [
        layout.gap,
        layout.border_width,
        layout.flex_grow,
        layout.flex_shrink,
        layout.aspect_ratio.unwrap_or(-1.0),
    ] {
        hash.f32(value);
    }
    hash_insets(hash, layout.padding);
    hash_insets(hash, layout.margin);
    for length in [
        layout.width,
        layout.height,
        layout.min_width,
        layout.min_height,
        layout.max_width,
        layout.max_height,
        layout.flex_basis,
    ]
    .into_iter()
    .chain(layout.inset)
    {
        hash_length(hash, length);
    }
}

fn hash_node_in(
    hash: &mut StableHash,
    node: &EvaluatedNode,
    shapes: &[(Arc<ShapesPlan>, u128)],
    shape_index: &mut usize,
) {
    hash_layout(hash, &node.layout);
    hash_paint(hash, node.paint.background.as_ref());
    match &node.paint.border {
        None => hash.u32(0),
        Some((width, paint)) => {
            hash.u32(1);
            hash.f32(*width);
            hash_paint(hash, Some(paint));
        }
    }
    hash.f32(node.paint.border_radius);
    match node.visual {
        None => hash.u32(0),
        Some(visual) => {
            hash.u32(1);
            for value in [
                visual.opacity,
                visual.translate.x,
                visual.translate.y,
                visual.scale.x,
                visual.scale.y,
                visual.rotation_degrees,
            ] {
                hash.f32(value);
            }
        }
    }
    hash.u32(u32::from(node.clip));
    match &node.content {
        NodeContent::Empty => hash.u32(0),
        NodeContent::Children(children) => {
            hash.u32(1);
            hash.u64(children.len() as u64);
            for child in children {
                hash_node_in(hash, child, shapes, shape_index);
            }
        }
        NodeContent::Text(text) => {
            hash.u32(2);
            hash.bytes(text.content.as_bytes());
            match &text.font_family {
                None => hash.u32(0),
                Some(family) => {
                    hash.u32(1);
                    hash.bytes(family.as_bytes());
                }
            }
            hash.f32(text.font_size);
            hash_color(hash, text.color);
            hash.f32(text.line_height);
            hash.u32(text.text_align as u32);
            match text.outline {
                None => hash.u32(0),
                Some(outline) => {
                    hash.u32(1);
                    hash_color(hash, outline.color);
                    hash.f32(outline.width);
                }
            }
        }
        NodeContent::Shape(_) => {
            hash.u32(3);
            let digest = shapes.get(*shape_index).map_or(0, |(_, digest)| *digest);
            *shape_index += 1;
            hash.bytes(&digest.to_le_bytes());
        }
    }
}

/// What a layout tree holds, for the plan budget: each node, its keyframes,
/// its text, and its shape items. Its plan holds about as much again.
#[must_use]
pub(crate) fn layout_bytes(
    node: &LayoutNode,
    shape_items_bytes: &impl Fn(&[alpha2_model::ShapeItem]) -> usize,
) -> usize {
    fn keys<T>(animation: Option<&Animated<T>>) -> usize {
        match animation {
            Some(Animated::Keyframes { keyframes }) => {
                keyframes.len() * size_of::<alpha2_model::Keyframe<T>>()
            }
            _ => 0,
        }
    }
    let text = node.text.as_ref().map_or(0, |text| {
        let content = match &text.content {
            Animated::Constant { value } => value.len(),
            Animated::Keyframes { keyframes } => keyframes
                .iter()
                .map(|keyframe| size_of::<alpha2_model::Keyframe<String>>() + keyframe.value.len())
                .sum(),
        };
        content + text.font_family.as_ref().map_or(0, String::len) + keys(Some(&text.font_size))
    });
    let paints = |paint: Option<&Animated<Paint>>| match paint {
        Some(Animated::Keyframes { keyframes }) => {
            keyframes.len() * size_of::<alpha2_model::Keyframe<Paint>>()
        }
        Some(Animated::Constant { .. }) | None => 0,
    };
    let inset = node.inset.as_ref().map_or(0, |inset| {
        [&inset.left, &inset.top, &inset.right, &inset.bottom]
            .into_iter()
            .map(|side| keys(side.as_ref()))
            .sum()
    });
    size_of::<LayoutNode>()
        + text
        + node
            .shape
            .as_ref()
            .map_or(0, |shape| shape_items_bytes(&shape.items))
        + keys(node.gap.as_ref())
        + keys(node.padding.as_ref())
        + paints(node.background.as_ref())
        + node.border.as_ref().map_or(0, |border| {
            keys(Some(&border.width)) + paints(Some(&border.paint))
        })
        + keys(node.border_radius.as_ref())
        + [
            &node.width,
            &node.height,
            &node.min_width,
            &node.min_height,
            &node.max_width,
            &node.max_height,
            &node.flex_basis,
        ]
        .into_iter()
        .map(|length| keys(length.as_ref()))
        .sum::<usize>()
        + keys(node.margin.as_ref())
        + keys(node.flex_grow.as_ref())
        + keys(node.flex_shrink.as_ref())
        + inset
        + keys(node.opacity.as_ref())
        + keys(node.translate.as_ref())
        + keys(node.scale.as_ref())
        + keys(node.rotation_degrees.as_ref())
        + node
            .children
            .iter()
            .map(|child| layout_bytes(child, shape_items_bytes))
            .sum::<usize>()
}

/// A generator's tree, planned once: what a shapes or layout generator
/// evaluates each frame from. Other generators need none.
#[derive(Clone, Debug, PartialEq)]
pub enum GeneratorPlan {
    /// A shapes generator's content tree.
    Shapes(ShapesPlan),
    /// A layout generator's box tree.
    Layout(LayoutPlan),
}

impl GeneratorPlan {
    /// The plan `generator` draws from, if it needs one.
    #[must_use]
    pub fn new(generator: &alpha2_model::Generator) -> Option<Self> {
        match generator {
            alpha2_model::Generator::Shapes { items } => Some(Self::Shapes(ShapesPlan::new(items))),
            alpha2_model::Generator::Layout { root } => Some(Self::Layout(LayoutPlan::new(root))),
            alpha2_model::Generator::Fill { .. } | alpha2_model::Generator::Text { .. } => None,
        }
    }

    /// The shapes plan, for a shapes generator.
    #[must_use]
    pub const fn shapes(&self) -> Option<&ShapesPlan> {
        match self {
            Self::Shapes(plan) => Some(plan),
            Self::Layout(_) => None,
        }
    }

    /// The layout plan, for a layout generator.
    #[must_use]
    pub const fn layout(&self) -> Option<&LayoutPlan> {
        match self {
            Self::Layout(plan) => Some(plan),
            Self::Shapes(_) => None,
        }
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
