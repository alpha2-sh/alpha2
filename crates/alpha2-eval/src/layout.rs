//! A layout generator's box tree evaluated at a time.
//!
//! A [`LayoutPlan`] is made once, when the index is built, for the project
//! canvas: pixel lengths become fractions of its shorter side then, so
//! everything after the plan sees fractions (and percents) only. A tree with
//! no keyframes is evaluated then and kept, shared, so every frame gets the
//! same value and the same key, and the engine resolves it once. Text style
//! is inherited during evaluation, so each text arrives complete.

use crate::ShapesPlan;
use crate::animation::{
    sample_hold, sample_insets, sample_length, sample_offset, sample_paint, sample_scalar,
    sample_size, sample_vec2,
};
use alpha2_geometry::StableHash;
use alpha2_model::{
    AlignContent, AlignItems, Animated, Display, FlexDirection, FlexWrap, GridAutoFlow,
    GridPlacement, JustifyContent, LAYOUT_TEXT_DEFAULTS, LayoutLength, LayoutNode, LayoutTextStyle,
    Overflow, Paint, Position, Rgba, TextAlign, TextShadow, TextStroke, TrackList, Vec2,
};
use alpha2_time::MediaTime;
use std::sync::Arc;

/// A layout tree, evaluated once when it does not animate.
#[derive(Clone, Debug)]
pub struct LayoutPlan {
    /// The tree with its pixels turned into fractions.
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
    /// Plans `root` for a canvas of `(width, height)` pixels, evaluating it
    /// when nothing in it animates.
    #[must_use]
    pub fn new(root: &LayoutNode, canvas: (u32, u32)) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let shorter_side = canvas.0.min(canvas.1).max(1) as f32;
        let root = root.without_px(shorter_side);
        let mut shapes = Vec::new();
        root.walk(|_, node| {
            if let Some(shape) = &node.shape {
                let mut hash = StableHash::new();
                hash.bytes(format!("{:?}", shape.items).as_bytes());
                shapes.push((Arc::new(ShapesPlan::new(&shape.items)), hash.finish()));
            }
        });
        let shapes_animate = shapes.iter().any(|(plan, _)| plan.is_animated());
        let animated = root.is_animated();
        let mut plan = Self {
            root: Box::new(root),
            shapes,
            shapes_animate,
            fixed: None,
        };
        if !animated {
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

    /// The tree the plan evaluates: the model tree with its pixels turned
    /// into fractions of the canvas's shorter side.
    #[must_use]
    pub const fn root(&self) -> &LayoutNode {
        &self.root
    }

    fn evaluate_now(&self, time: MediaTime) -> EvaluatedLayout {
        let mut shapes = self.shapes.iter();
        let root = evaluate_node(&self.root, time, &mut shapes, &LAYOUT_TEXT_DEFAULTS);
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
/// shorter side.
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluatedNode {
    /// What flexbox or grid reads.
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

/// A length on each side of a box, as a fraction of the picture's shorter
/// side.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
#[allow(missing_docs)]
pub struct Sides {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Sides {
    const fn from_array([top, right, bottom, left]: [f32; 4]) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    const fn to_array(self) -> [f32; 4] {
        [self.top, self.right, self.bottom, self.left]
    }
}

/// A box's sampled box-model, flex and grid values. Lengths are fractions
/// or percents, never pixels.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct NodeLayout {
    pub display: Display,
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub grid_template_columns: Option<TrackList>,
    pub grid_template_rows: Option<TrackList>,
    pub grid_auto_columns: Option<TrackList>,
    pub grid_auto_rows: Option<TrackList>,
    pub grid_auto_flow: GridAutoFlow,
    /// The gap between rows: `row_gap`, else `gap`.
    pub row_gap: f32,
    /// The gap between columns: `column_gap`, else `gap`.
    pub column_gap: f32,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub align_content: AlignContent,
    pub justify_items: AlignItems,
    pub padding: Sides,
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
    pub margin: Sides,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Option<LayoutLength>,
    pub align_self: Option<AlignItems>,
    pub justify_self: Option<AlignItems>,
    pub grid_column: GridPlacement,
    pub grid_row: GridPlacement,
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

/// A text node's sampled text and the style it inherits.
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluatedText {
    /// The text; may be empty.
    pub content: String,
    /// The family it draws in.
    pub font_family: Option<String>,
    /// Font size, a fraction of the picture's shorter side; positive.
    pub font_size: f32,
    /// What paints the glyphs, a gradient over the text's layout box.
    pub fill: Paint,
    /// Baseline-to-baseline distance in font sizes.
    pub line_height: f32,
    /// Space after each character, a fraction of the font size.
    pub letter_spacing: f32,
    /// The weight its faces are matched by.
    pub font_weight: u16,
    /// How lines align in the content width.
    pub text_align: TextAlign,
    /// Bands around the glyphs, innermost first.
    pub strokes: Vec<TextStroke>,
    /// Shadows of the glyphs and strokes, the first on top.
    pub shadows: Vec<TextShadow>,
}

fn non_negative(animation: Option<&Animated<f32>>, time: MediaTime, default: f32) -> f32 {
    animation.map_or(default, |value| sample_scalar(value, time).max(0.0))
}

fn size_or(
    animation: Option<&Animated<alpha2_model::LayoutSize>>,
    time: MediaTime,
    default: f32,
) -> f32 {
    animation.map_or(default, |value| sample_size(value, time).max(0.0))
}

fn length(animation: Option<&Animated<LayoutLength>>, time: MediaTime) -> Option<LayoutLength> {
    animation.map(|value| sample_length(value, time))
}

fn size(animation: Option<&Animated<LayoutLength>>, time: MediaTime) -> Option<LayoutLength> {
    length(animation, time).map(|length| length.with_value(length.value().max(0.0)))
}

/// What flexbox or grid reads of `node` at `time`; `border_width` is the
/// sampled border's.
fn node_layout(node: &LayoutNode, time: MediaTime, border_width: f32) -> NodeLayout {
    let insets = |value: Option<&Animated<alpha2_model::Insets>>| {
        value.map_or_else(Sides::default, |value| {
            Sides::from_array(sample_insets(value, time))
        })
    };
    let padding = Sides::from_array(
        insets(node.padding.as_ref())
            .to_array()
            .map(|side| side.max(0.0)),
    );
    let inset = node.inset.as_ref().map_or([None; 4], |inset| {
        [
            length(inset.left.as_ref(), time),
            length(inset.top.as_ref(), time),
            length(inset.right.as_ref(), time),
            length(inset.bottom.as_ref(), time),
        ]
    });
    let gap = size_or(node.gap.as_ref(), time, 0.0);
    NodeLayout {
        display: node.display.unwrap_or_default(),
        flex_direction: node.flex_direction.unwrap_or_default(),
        flex_wrap: node.flex_wrap.unwrap_or_default(),
        grid_template_columns: node.grid_template_columns.clone(),
        grid_template_rows: node.grid_template_rows.clone(),
        grid_auto_columns: node.grid_auto_columns.clone(),
        grid_auto_rows: node.grid_auto_rows.clone(),
        grid_auto_flow: node.grid_auto_flow.unwrap_or_default(),
        row_gap: size_or(node.row_gap.as_ref(), time, gap),
        column_gap: size_or(node.column_gap.as_ref(), time, gap),
        justify_content: node.justify_content.unwrap_or_default(),
        align_items: node.align_items.unwrap_or_default(),
        align_content: node.align_content.unwrap_or_default(),
        justify_items: node.justify_items.unwrap_or_default(),
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
        justify_self: node.justify_self,
        grid_column: node.grid_column.unwrap_or_default(),
        grid_row: node.grid_row.unwrap_or_default(),
        position: node.position.unwrap_or_default(),
        inset,
    }
}

/// The style `node` gives its text and its children: what it sets, and what
/// it inherits otherwise, sampled at `time`.
fn text_style(node: &LayoutNode, time: MediaTime, inherited: &LayoutTextStyle) -> LayoutTextStyle {
    LayoutTextStyle {
        font_family: node
            .font_family
            .clone()
            .or_else(|| inherited.font_family.clone()),
        font_size: node.font_size.as_ref().map_or(inherited.font_size, |size| {
            sample_size(size, time).max(f32::MIN_POSITIVE)
        }),
        fill: node.fill.clone().unwrap_or_else(|| inherited.fill.clone()),
        line_height: node.line_height.unwrap_or(inherited.line_height),
        letter_spacing: node.letter_spacing.unwrap_or(inherited.letter_spacing),
        font_weight: node.font_weight.unwrap_or(inherited.font_weight),
        text_align: node.text_align.unwrap_or(inherited.text_align),
        strokes: node
            .strokes
            .clone()
            .unwrap_or_else(|| inherited.strokes.clone()),
        shadows: node
            .shadows
            .clone()
            .unwrap_or_else(|| inherited.shadows.clone()),
    }
}

fn evaluate_node<'a>(
    node: &LayoutNode,
    time: MediaTime,
    shapes: &mut impl Iterator<Item = &'a (Arc<ShapesPlan>, u128)>,
    inherited: &LayoutTextStyle,
) -> EvaluatedNode {
    let border = node.border.as_ref().map(|border| {
        (
            sample_size(&border.width, time).max(0.0),
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
        border_radius: size_or(node.border_radius.as_ref(), time, 0.0),
    };
    let visual = node.has_visual().then(|| NodeVisual {
        opacity: node
            .opacity
            .as_ref()
            .map_or(1.0, |value| sample_scalar(value, time).clamp(0.0, 1.0)),
        translate: node
            .translate
            .as_ref()
            .map_or_else(Vec2::default, |value| sample_offset(value, time)),
        scale: node
            .scale
            .as_ref()
            .map_or(Vec2::new(1.0, 1.0), |value| sample_vec2(value, time)),
        rotation_degrees: node
            .rotation_degrees
            .as_ref()
            .map_or(0.0, |value| sample_scalar(value, time)),
    });
    let style = text_style(node, time, inherited);
    let content = if let Some(text) = &node.text {
        NodeContent::Text(EvaluatedText {
            content: sample_hold(text, time),
            font_family: style.font_family,
            font_size: style.font_size,
            fill: style.fill.clone(),
            line_height: style.line_height,
            letter_spacing: style.letter_spacing,
            font_weight: style.font_weight,
            text_align: style.text_align,
            strokes: style.strokes.clone(),
            shadows: style.shadows.clone(),
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
                .map(|child| evaluate_node(child, time, shapes, &style))
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
        Some(LayoutLength::Fraction(value)) => {
            hash.u32(1);
            hash.f32(value);
        }
        Some(LayoutLength::Percent(value)) => {
            hash.u32(2);
            hash.f32(value);
        }
        Some(LayoutLength::Px(value)) => {
            hash.u32(3);
            hash.f32(value);
        }
    }
}

fn hash_sides(hash: &mut StableHash, sides: Sides) {
    for side in sides.to_array() {
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
        layout.display as u32,
        layout.flex_direction as u32,
        layout.flex_wrap as u32,
        layout.grid_auto_flow as u32,
        layout.justify_content as u32,
        layout.align_items as u32,
        layout.align_content as u32,
        layout.justify_items as u32,
        layout.overflow as u32,
        layout.position as u32,
        layout.align_self.map_or(u32::MAX, |align| align as u32),
        layout.justify_self.map_or(u32::MAX, |align| align as u32),
    ] {
        hash.u32(value);
    }
    // A track list's text is all of it: its canonical form.
    for tracks in [
        &layout.grid_template_columns,
        &layout.grid_template_rows,
        &layout.grid_auto_columns,
        &layout.grid_auto_rows,
    ] {
        match tracks {
            None => hash.u32(0),
            Some(tracks) => {
                hash.u32(1);
                hash.bytes(tracks.to_string().as_bytes());
            }
        }
    }
    for placement in [layout.grid_column, layout.grid_row] {
        hash.bytes(placement.to_string().as_bytes());
    }
    for value in [
        layout.row_gap,
        layout.column_gap,
        layout.border_width,
        layout.flex_grow,
        layout.flex_shrink,
        layout.aspect_ratio.unwrap_or(-1.0),
    ] {
        hash.f32(value);
    }
    hash_sides(hash, layout.padding);
    hash_sides(hash, layout.margin);
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
            hash_paint(hash, Some(&text.fill));
            hash.f32(text.line_height);
            hash.f32(text.letter_spacing);
            hash.u32(u32::from(text.font_weight));
            hash.u32(text.text_align as u32);
            hash.u64(text.strokes.len() as u64);
            for stroke in &text.strokes {
                hash_color(hash, stroke.color);
                hash.f32(stroke.width);
            }
            hash.u64(text.shadows.len() as u64);
            for shadow in &text.shadows {
                hash_color(hash, shadow.color);
                hash.f32(shadow.offset.x);
                hash.f32(shadow.offset.y);
                hash.f32(shadow.blur);
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
/// its text, its track lists, and its shape items. Its plan holds about as
/// much again.
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
    let text = node.text.as_ref().map_or(0, |text| match text {
        Animated::Constant { value } => value.len(),
        Animated::Keyframes { keyframes } => keyframes
            .iter()
            .map(|keyframe| size_of::<alpha2_model::Keyframe<String>>() + keyframe.value.len())
            .sum(),
    });
    let style = node.font_family.as_ref().map_or(0, String::len) + keys(node.font_size.as_ref());
    let tracks = [
        &node.grid_template_columns,
        &node.grid_template_rows,
        &node.grid_auto_columns,
        &node.grid_auto_rows,
    ]
    .into_iter()
    .flatten()
    .map(|tracks| tracks.track_count() * size_of::<alpha2_model::TrackSize>())
    .sum::<usize>();
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
        + style
        + tracks
        + node
            .shape
            .as_ref()
            .map_or(0, |shape| shape_items_bytes(&shape.items))
        + [
            &node.gap,
            &node.row_gap,
            &node.column_gap,
            &node.border_radius,
        ]
        .into_iter()
        .map(|value| keys(value.as_ref()))
        .sum::<usize>()
        + keys(node.padding.as_ref())
        + paints(node.background.as_ref())
        + node.border.as_ref().map_or(0, |border| {
            keys(Some(&border.width)) + paints(Some(&border.paint))
        })
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
    /// The plan `generator` draws from on a project canvas of `canvas`
    /// pixels, if it needs one.
    #[must_use]
    pub fn new(generator: &alpha2_model::Generator, canvas: (u32, u32)) -> Option<Self> {
        match generator {
            alpha2_model::Generator::Shapes { items } => Some(Self::Shapes(ShapesPlan::new(items))),
            alpha2_model::Generator::Layout { root } => {
                Some(Self::Layout(LayoutPlan::new(root, canvas)))
            }
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
