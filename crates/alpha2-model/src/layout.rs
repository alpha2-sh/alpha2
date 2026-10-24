//! The box tree of a layout generator: boxes laid out by CSS flexbox around
//! their text and shapes.
//!
//! Field names are CSS property names in `snake_case`, and values CSS keywords
//! in `snake_case`. Every length (sizes, padding, gap, margin, inset, radius,
//! border width, font size, translate) is a fraction of the layout picture's
//! shorter side; sizes and insets may instead be a percent of the parent's
//! content box, as in CSS. Boxes are `border-box`: a width includes padding
//! and border.

use crate::checks::{
    Checked, ValueIssue, ValueProblem, animated, any, non_negative, scalar, unit, validate_paint,
    vec2,
};
use crate::{
    Animated, Interpolation, LINE_HEIGHT_RANGE, MAX_FONT_FAMILY_BYTES, MAX_TEXT_CONTENT_BYTES,
    MAX_TEXT_OUTLINE_WIDTH, Paint, Rgba, ShapeIssue, ShapeItem, TextAlign, TextOutline, Vec2,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Most nodes in one layout tree, the root included.
pub const MAX_LAYOUT_NODES: usize = 256;
/// Deepest nesting of one layout tree; the root is depth 1.
pub const MAX_LAYOUT_DEPTH: usize = 16;
/// Most nodes with text in one layout tree.
pub const MAX_LAYOUT_TEXTS: usize = 64;
/// Most isolated nodes (with a visual field or hidden overflow) in one
/// layout tree.
pub const MAX_LAYOUT_ISOLATED: usize = 16;
/// Largest font size of a layout's text, as a fraction of the picture's
/// shorter side.
pub const MAX_LAYOUT_FONT_SIZE: f32 = 1.0;

/// One box of a layout tree, laid out by CSS flexbox.
///
/// A box is a flex container when it has children, a leaf when it has text
/// or shape content, and empty otherwise (a spacer or a decoration). Per
/// box, its background is drawn first, then its border, then its content.
/// Boxes are drawn in document order; a reversed direction moves them, not
/// their order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutNode {
    /// Boxes laid out in this one, in document (and paint) order. Not with
    /// `text` or `shape`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<LayoutNode>,
    /// The box's text, wrapped at its content width. Not with `children` or
    /// `shape`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<LayoutText>,
    /// Shapes drawn on a picture the size of the box's content box. Not with
    /// `children` or `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<LayoutShape>,

    /// The main axis of the box's children. Default `row`. Not on a box
    /// with text or shape content, as every flex container field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_direction: Option<FlexDirection>,
    /// Whether children wrap onto more lines. Default `nowrap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_wrap: Option<FlexWrap>,
    /// Space between children, on both axes. At least 0; default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Animated<f32>>,
    /// Where children lie along the main axis. Default `start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify_content: Option<JustifyContent>,
    /// Where children lie across the main axis. Default `stretch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_items: Option<AlignItems>,

    /// Space inside the border. Sides at least 0; default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<Animated<Insets>>,
    /// What covers the border box, rounded by `border_radius`. Positions
    /// are normalized to the border box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Animated<Paint>>,
    /// A border inside the border box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<LayoutBorder>,
    /// The corner radius of the border box. At least 0; default 0. A radius
    /// past half the box's shorter side is capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_radius: Option<Animated<f32>>,
    /// Whether content past the padding box is drawn. Default `visible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<Overflow>,

    /// Border-box width. `None` is auto. Not on the root, as every flex item
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<Animated<LayoutLength>>,
    /// Border-box height. `None` is auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<Animated<LayoutLength>>,
    /// Smallest width. `None` is auto, the CSS automatic minimum size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_width: Option<Animated<LayoutLength>>,
    /// Smallest height. `None` is auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_height: Option<Animated<LayoutLength>>,
    /// Largest width. `None` is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<Animated<LayoutLength>>,
    /// Largest height. `None` is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<Animated<LayoutLength>>,
    /// Width over height. Positive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<f32>,
    /// Space outside the border. Sides may be negative; there is no `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<Animated<Insets>>,
    /// How much of the free space the box takes. At least 0; default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_grow: Option<Animated<f32>>,
    /// How much the box gives up when space runs short. At least 0; default
    /// 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_shrink: Option<Animated<f32>>,
    /// The main size before growing and shrinking. `None` is auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_basis: Option<Animated<LayoutLength>>,
    /// Where the box lies across its parent's main axis. `None` follows the
    /// parent's `align_items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_self: Option<AlignItems>,
    /// Whether the box is in its parent's flow. Default `relative`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    /// Where an `absolute` box lies in its parent's padding box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<LayoutInset>,

    /// Opacity of the box and everything in it, as one group, in `0..=1`.
    /// Drawn after layout; it never moves a box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<Animated<f32>>,
    /// Offset of the box and everything in it, after layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<Animated<Vec2>>,
    /// Per-axis scale of the box and everything in it about its border-box
    /// centre, after layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Animated<Vec2>>,
    /// Clockwise rotation of the box and everything in it about its
    /// border-box centre, in degrees, after layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<Animated<f32>>,
}

/// A box's text: one paragraph style, wrapped at the box's content width.
///
/// The text fills the content width, its lines aligned in it by
/// `text_align`; its first line starts at the content box's top.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutText {
    /// The text; `\n` starts a line. At most [`MAX_TEXT_CONTENT_BYTES`]
    /// bytes per value; may be empty. Its keyframes all `hold`.
    pub content: Animated<String>,
    /// The family the text draws in, as a text style's `font_family`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size as a fraction of the picture's shorter side: positive, at
    /// most [`MAX_LAYOUT_FONT_SIZE`].
    #[serde(default = "default_font_size")]
    pub font_size: Animated<f32>,
    /// Text color.
    #[serde(default = "white")]
    pub color: Rgba,
    /// Baseline-to-baseline distance in font sizes, in `[0.5, 3]`.
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// How lines align within the content width. Default `start`.
    #[serde(default = "start")]
    pub text_align: TextAlign,
    /// A band around the glyphs; its width a fraction of the font size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<TextOutline>,
}

fn default_font_size() -> Animated<f32> {
    Animated::Constant { value: 0.05 }
}

const fn white() -> Rgba {
    Rgba::new(1.0, 1.0, 1.0, 1.0)
}

const fn default_line_height() -> f32 {
    1.2
}

const fn start() -> TextAlign {
    TextAlign::Start
}

/// A box's shapes, drawn as a shapes generator draws its items on a picture
/// the size of the box's content box: positions normalized to that box,
/// lengths fractions of its shorter side. A shape box has no size of its
/// own.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutShape {
    /// The content tree, front first.
    pub items: Vec<ShapeItem>,
}

/// A length on each side of a box. Missing sides are 0.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Insets {
    /// Top side.
    #[serde(default)]
    pub top: f32,
    /// Right side.
    #[serde(default)]
    pub right: f32,
    /// Bottom side.
    #[serde(default)]
    pub bottom: f32,
    /// Left side.
    #[serde(default)]
    pub left: f32,
}

impl Insets {
    fn sides(self) -> [f32; 4] {
        [self.top, self.right, self.bottom, self.left]
    }
}

/// A border inside the border box, as wide on every side.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutBorder {
    /// Width. At least 0.
    pub width: Animated<f32>,
    /// What covers the border. Positions are normalized to the border box.
    pub paint: Animated<Paint>,
}

/// Where an absolute box lies in its parent's padding box. A missing side is
/// auto: with neither `left` nor `right`, the box keeps its place in the
/// flow on that axis.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutInset {
    /// Distance from the parent's left padding edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<Animated<LayoutLength>>,
    /// Distance from the parent's top padding edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<Animated<LayoutLength>>,
    /// Distance from the parent's right padding edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<Animated<LayoutLength>>,
    /// Distance from the parent's bottom padding edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom: Option<Animated<LayoutLength>>,
}

/// A size or an inset.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutLength {
    /// A fraction of the layout picture's shorter side.
    Length {
        /// The fraction.
        value: f32,
    },
    /// Percent (0 to 100 is 0% to 100%) of the parent's content box, along
    /// the field's axis.
    Percent {
        /// The percent.
        value: f32,
    },
}

impl LayoutLength {
    /// The number, whatever its kind.
    #[must_use]
    pub const fn value(self) -> f32 {
        match self {
            Self::Length { value } | Self::Percent { value } => value,
        }
    }

    const fn is_percent(self) -> bool {
        matches!(self, Self::Percent { .. })
    }
}

/// The main axis of a flex container.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FlexDirection {
    /// Left to right.
    #[default]
    Row,
    /// Top to bottom.
    Column,
    /// Right to left.
    RowReverse,
    /// Bottom to top.
    ColumnReverse,
}

/// Whether a flex container's children wrap.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FlexWrap {
    /// One line.
    #[default]
    #[serde(rename = "nowrap")]
    NoWrap,
    /// As many lines as the children need.
    #[serde(rename = "wrap")]
    Wrap,
}

/// Where children lie along a flex container's main axis.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum JustifyContent {
    /// Packed at the start.
    #[default]
    Start,
    /// Packed in the middle.
    Center,
    /// Packed at the end.
    End,
    /// The first at the start, the last at the end, equal space between.
    SpaceBetween,
    /// Equal space around each.
    SpaceAround,
    /// Equal space between each and at both ends.
    SpaceEvenly,
}

/// Where children lie across a flex container's main axis.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AlignItems {
    /// At the start.
    Start,
    /// In the middle.
    Center,
    /// At the end.
    End,
    /// Across the whole line.
    #[default]
    Stretch,
}

/// Whether a box is in its parent's flow.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Position {
    /// In the flow.
    #[default]
    Relative,
    /// Out of the flow, placed by `inset`.
    Absolute,
}

/// Whether content past a box's padding box is drawn.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    /// Drawn.
    #[default]
    Visible,
    /// Clipped to the padding box.
    Hidden,
}

/// Why a layout tree is invalid.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LayoutProblem {
    /// More than [`MAX_LAYOUT_NODES`] nodes.
    TooManyNodes,
    /// Nodes nested deeper than [`MAX_LAYOUT_DEPTH`].
    TooDeep,
    /// More than [`MAX_LAYOUT_TEXTS`] nodes with text.
    TooManyTexts,
    /// More than [`MAX_LAYOUT_ISOLATED`] nodes with a visual field or hidden
    /// overflow.
    TooManyIsolated,
    /// A node with more than one of children, text, and shape.
    MixedContent,
    /// A flex container field on a node with text or shape content.
    ContainerFieldOnLeaf,
    /// A size, margin, position, or flex item field on the root.
    RootItemField,
    /// Keyframes that are empty, out of order, or have an unusable curve.
    InvalidKeyframes,
    /// A value that is not finite or outside its range.
    OutOfRange,
    /// Size keyframes that mix `length` and `percent`.
    LengthKeyframesDiffer,
    /// Paint keyframes that differ in kind, stop count, or spread.
    PaintKeyframesDiffer,
    /// A text keyframe whose interpolation is not `hold`.
    TextKeyframesInterpolate,
}

impl fmt::Display for LayoutProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyNodes => "has more nodes than a layout may hold",
            Self::TooDeep => "nests nodes too deeply",
            Self::TooManyTexts => "has more text nodes than a layout may hold",
            Self::TooManyIsolated => {
                "has more nodes with opacity, transforms, or hidden overflow than a layout may hold"
            }
            Self::MixedContent => "has more than one of children, text, and shape",
            Self::ContainerFieldOnLeaf => {
                "sets a flex container field on a node with text or shape content"
            }
            Self::RootItemField => "sets a size, margin, position, or flex item field on the root",
            Self::InvalidKeyframes => "has invalid keyframes",
            Self::OutOfRange => "has a value out of range",
            Self::LengthKeyframesDiffer => "has size keyframes that differ in kind",
            Self::PaintKeyframesDiffer => "has paint keyframes that differ in kind or stops",
            Self::TextKeyframesInterpolate => "has text keyframes that do not hold",
        })
    }
}

/// Where in a layout tree a problem lies, and what it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutIssue {
    /// The field's path from the root, like `root.children[2].padding`.
    pub at: String,
    /// What is wrong there.
    pub problem: LayoutProblem,
}

impl LayoutIssue {
    fn new(at: impl Into<String>, problem: LayoutProblem) -> Self {
        Self {
            at: at.into(),
            problem,
        }
    }
}

impl From<ValueIssue> for LayoutIssue {
    fn from(issue: ValueIssue) -> Self {
        Self::new(
            issue.at,
            match issue.problem {
                ValueProblem::InvalidKeyframes => LayoutProblem::InvalidKeyframes,
                ValueProblem::OutOfRange => LayoutProblem::OutOfRange,
                ValueProblem::PaintKeyframesDiffer => LayoutProblem::PaintKeyframesDiffer,
            },
        )
    }
}

/// Why a generator is invalid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GeneratorIssue {
    /// A fill's or a text's field is out of range.
    Invalid,
    /// A problem in a shapes generator's content tree, or in a layout's
    /// shape node (its path from the layout's root).
    Shape(ShapeIssue),
    /// A problem in a layout tree.
    Layout(LayoutIssue),
}

impl From<LayoutIssue> for GeneratorIssue {
    fn from(issue: LayoutIssue) -> Self {
        Self::Layout(issue)
    }
}

impl From<ValueIssue> for GeneratorIssue {
    fn from(issue: ValueIssue) -> Self {
        Self::Layout(issue.into())
    }
}

/// What the whole tree has used up of its limits so far.
#[derive(Default)]
struct Counts {
    nodes: usize,
    texts: usize,
    isolated: usize,
    shape_items: usize,
}

type LayoutChecked = Result<(), GeneratorIssue>;

/// Checks a layout tree: its limits, its structure, every value's range, and
/// that animated values agree in kind between keyframes. Shape nodes' items
/// are checked as a shapes generator's are, counted together.
///
/// # Errors
/// Returns the first problem in preorder.
pub(crate) fn validate_layout(root: &LayoutNode) -> LayoutChecked {
    let mut counts = Counts::default();
    if root.has_item_field() {
        return Err(LayoutIssue::new("root", LayoutProblem::RootItemField).into());
    }
    validate_node(root, "root", 1, &mut counts)
}

fn validate_node(node: &LayoutNode, at: &str, depth: usize, counts: &mut Counts) -> LayoutChecked {
    let issue = |problem| Err(LayoutIssue::new(at, problem).into());
    counts.nodes += 1;
    if counts.nodes > MAX_LAYOUT_NODES {
        return issue(LayoutProblem::TooManyNodes);
    }
    if depth > MAX_LAYOUT_DEPTH {
        return issue(LayoutProblem::TooDeep);
    }
    let contents = usize::from(!node.children.is_empty())
        + usize::from(node.text.is_some())
        + usize::from(node.shape.is_some());
    if contents > 1 {
        return issue(LayoutProblem::MixedContent);
    }
    if (node.text.is_some() || node.shape.is_some()) && node.has_container_field() {
        return issue(LayoutProblem::ContainerFieldOnLeaf);
    }
    if node.text.is_some() {
        counts.texts += 1;
        if counts.texts > MAX_LAYOUT_TEXTS {
            return issue(LayoutProblem::TooManyTexts);
        }
    }
    if node.is_isolated() {
        counts.isolated += 1;
        if counts.isolated > MAX_LAYOUT_ISOLATED {
            return issue(LayoutProblem::TooManyIsolated);
        }
    }
    validate_fields(node, at)?;
    if let Some(text) = &node.text {
        validate_text(text, &format!("{at}.text"))?;
    }
    if let Some(shape) = &node.shape {
        crate::shapes::validate_items(
            &shape.items,
            &format!("{at}.shape.items"),
            1,
            &mut counts.shape_items,
        )
        .map_err(GeneratorIssue::Shape)?;
    }
    for (index, child) in node.children.iter().enumerate() {
        validate_node(child, &format!("{at}.children[{index}]"), depth + 1, counts)?;
    }
    Ok(())
}

fn optional<T, E>(value: Option<&T>, check: impl FnOnce(&T) -> Result<(), E>) -> LayoutChecked
where
    GeneratorIssue: From<E>,
{
    Ok(value.map_or(Ok(()), check)?)
}

fn validate_fields(node: &LayoutNode, at: &str) -> LayoutChecked {
    let field = |name: &str| format!("{at}.{name}");
    optional(node.gap.as_ref(), |gap| {
        scalar(gap, &field("gap"), non_negative)
    })?;
    optional(node.padding.as_ref(), |padding| {
        insets(padding, &field("padding"), non_negative)
    })?;
    optional(node.background.as_ref(), |paint| {
        validate_paint(paint, &field("background"))
    })?;
    if let Some(border) = &node.border {
        scalar(&border.width, &field("border.width"), non_negative)?;
        validate_paint(&border.paint, &field("border.paint"))?;
    }
    optional(node.border_radius.as_ref(), |radius| {
        scalar(radius, &field("border_radius"), non_negative)
    })?;
    for (name, size) in [
        ("width", &node.width),
        ("height", &node.height),
        ("min_width", &node.min_width),
        ("min_height", &node.min_height),
        ("max_width", &node.max_width),
        ("max_height", &node.max_height),
    ] {
        optional(size.as_ref(), |size| {
            length(size, &field(name), non_negative)
        })?;
    }
    if node
        .aspect_ratio
        .is_some_and(|ratio| !ratio.is_finite() || ratio <= 0.0)
    {
        return Err(ValueIssue::new(field("aspect_ratio"), ValueProblem::OutOfRange).into());
    }
    optional(node.margin.as_ref(), |margin| {
        insets(margin, &field("margin"), any)
    })?;
    optional(node.flex_grow.as_ref(), |grow| {
        scalar(grow, &field("flex_grow"), non_negative)
    })?;
    optional(node.flex_shrink.as_ref(), |shrink| {
        scalar(shrink, &field("flex_shrink"), non_negative)
    })?;
    optional(node.flex_basis.as_ref(), |basis| {
        length(basis, &field("flex_basis"), non_negative)
    })?;
    if let Some(inset) = &node.inset {
        for (name, side) in [
            ("left", &inset.left),
            ("top", &inset.top),
            ("right", &inset.right),
            ("bottom", &inset.bottom),
        ] {
            optional(side.as_ref(), |side| {
                length(side, &field(&format!("inset.{name}")), any)
            })?;
        }
    }
    optional(node.opacity.as_ref(), |opacity| {
        scalar(opacity, &field("opacity"), unit)
    })?;
    optional(node.translate.as_ref(), |translate| {
        vec2(translate, &field("translate"), |_| true)
    })?;
    optional(node.scale.as_ref(), |scale| {
        vec2(scale, &field("scale"), |_| true)
    })?;
    optional(node.rotation_degrees.as_ref(), |rotation| {
        scalar(rotation, &field("rotation_degrees"), any)
    })
}

fn insets(animation: &Animated<Insets>, at: &str, check: fn(f32) -> bool) -> Checked {
    animated(animation, at, |value| {
        value
            .sides()
            .iter()
            .all(|side| side.is_finite() && check(*side))
    })
}

/// A length's values, and that its keyframes share a kind.
fn length(
    animation: &Animated<LayoutLength>,
    at: &str,
    check: fn(f32) -> bool,
) -> Result<(), GeneratorIssue> {
    animated(animation, at, |value| {
        value.value().is_finite() && check(value.value())
    })?;
    if let Animated::Keyframes { keyframes } = animation {
        let kind = keyframes[0].value.is_percent();
        if keyframes.iter().any(|key| key.value.is_percent() != kind) {
            return Err(LayoutIssue::new(at, LayoutProblem::LengthKeyframesDiffer).into());
        }
    }
    Ok(())
}

fn validate_text(text: &LayoutText, at: &str) -> LayoutChecked {
    let content_at = format!("{at}.content");
    animated(&text.content, &content_at, |_| true)?;
    if let Animated::Keyframes { keyframes } = &text.content
        && keyframes
            .iter()
            .any(|key| key.interpolation != Interpolation::Hold)
    {
        return Err(LayoutIssue::new(content_at, LayoutProblem::TextKeyframesInterpolate).into());
    }
    animated(&text.content, &content_at, |content| {
        content.len() <= MAX_TEXT_CONTENT_BYTES
    })?;
    let out_of_range = |name: &str| {
        Err(LayoutIssue::new(format!("{at}.{name}"), LayoutProblem::OutOfRange).into())
    };
    if text
        .font_family
        .as_ref()
        .is_some_and(|family| family.is_empty() || family.len() > MAX_FONT_FAMILY_BYTES)
    {
        return out_of_range("font_family");
    }
    scalar(&text.font_size, &format!("{at}.font_size"), |size| {
        size > 0.0 && size <= MAX_LAYOUT_FONT_SIZE
    })?;
    if !text.color.is_valid() {
        return out_of_range("color");
    }
    if !LINE_HEIGHT_RANGE.contains(&text.line_height) {
        return out_of_range("line_height");
    }
    let valid_outline = |outline: TextOutline| {
        outline.color.is_valid() && outline.width > 0.0 && outline.width <= MAX_TEXT_OUTLINE_WIDTH
    };
    if text.outline.is_some_and(|outline| !valid_outline(outline)) {
        return out_of_range("outline");
    }
    Ok(())
}

impl LayoutNode {
    /// Whether any flex container field is present.
    fn has_container_field(&self) -> bool {
        self.flex_direction.is_some()
            || self.flex_wrap.is_some()
            || self.gap.is_some()
            || self.justify_content.is_some()
            || self.align_items.is_some()
    }

    /// Whether any field the root may not have is present.
    fn has_item_field(&self) -> bool {
        self.width.is_some()
            || self.height.is_some()
            || self.min_width.is_some()
            || self.min_height.is_some()
            || self.max_width.is_some()
            || self.max_height.is_some()
            || self.aspect_ratio.is_some()
            || self.margin.is_some()
            || self.flex_grow.is_some()
            || self.flex_shrink.is_some()
            || self.flex_basis.is_some()
            || self.align_self.is_some()
            || self.position.is_some()
            || self.inset.is_some()
    }

    /// Whether the node has a visual field: opacity, translate, scale, or
    /// rotation.
    #[must_use]
    pub const fn has_visual(&self) -> bool {
        self.opacity.is_some()
            || self.translate.is_some()
            || self.scale.is_some()
            || self.rotation_degrees.is_some()
    }

    /// Whether the node is drawn into its own composite: it has a visual
    /// field or hidden overflow. Presence decides, not values.
    #[must_use]
    pub const fn is_isolated(&self) -> bool {
        self.has_visual() || matches!(self.overflow, Some(Overflow::Hidden))
    }

    /// Whether the node, its subtree, or a shape in it changes over time.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        let inset = self.inset.as_ref().is_some_and(|inset| {
            [&inset.left, &inset.top, &inset.right, &inset.bottom]
                .into_iter()
                .any(|value| keyed_option(value.as_ref()))
        });
        let text = self
            .text
            .as_ref()
            .is_some_and(|text| keyed(&text.content) || keyed(&text.font_size));
        let shape = self
            .shape
            .as_ref()
            .is_some_and(|shape| shape.items.iter().any(ShapeItem::is_animated));
        let border = self
            .border
            .as_ref()
            .is_some_and(|border| keyed(&border.width) || keyed(&border.paint));
        keyed_option(self.gap.as_ref())
            || keyed_option(self.padding.as_ref())
            || keyed_option(self.background.as_ref())
            || border
            || keyed_option(self.border_radius.as_ref())
            || [
                &self.width,
                &self.height,
                &self.min_width,
                &self.min_height,
                &self.max_width,
                &self.max_height,
                &self.flex_basis,
            ]
            .into_iter()
            .any(|value| keyed_option(value.as_ref()))
            || keyed_option(self.margin.as_ref())
            || keyed_option(self.flex_grow.as_ref())
            || keyed_option(self.flex_shrink.as_ref())
            || inset
            || keyed_option(self.opacity.as_ref())
            || keyed_option(self.translate.as_ref())
            || keyed_option(self.scale.as_ref())
            || keyed_option(self.rotation_degrees.as_ref())
            || text
            || shape
            || self.children.iter().any(Self::is_animated)
    }

    /// Every node of the tree in preorder, with its path from the root.
    pub fn walk(&self, mut visit: impl FnMut(&str, &Self)) {
        fn go(node: &LayoutNode, at: &str, visit: &mut impl FnMut(&str, &LayoutNode)) {
            visit(at, node);
            for (index, child) in node.children.iter().enumerate() {
                go(child, &format!("{at}.children[{index}]"), visit);
            }
        }
        go(self, "root", &mut visit);
    }
}

const fn keyed<T>(animation: &Animated<T>) -> bool {
    matches!(animation, Animated::Keyframes { .. })
}

const fn keyed_option<T>(animation: Option<&Animated<T>>) -> bool {
    matches!(animation, Some(Animated::Keyframes { .. }))
}
