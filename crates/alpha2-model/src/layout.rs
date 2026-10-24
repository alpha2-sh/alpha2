//! The box tree of a layout generator: boxes laid out by CSS flexbox or grid
//! around their text and shapes.
//!
//! Field names are CSS property names in `snake_case`, and keyword values CSS
//! keywords in `snake_case`. A length is a number (a fraction of the layout
//! picture's shorter side), `"<n>px"` (pixels of the project canvas), or,
//! for sizes and insets, `"<n>%"` of the parent's content box, as in CSS
//! (see [`crate::layout_values`]). Boxes are `border-box`: a width includes
//! padding and border. Text style (`font_family`, `font_size`, `color`,
//! `line_height`, `text_align`, `outline`) may be set on any box and is
//! inherited by the boxes in it, as CSS inherits it.

use crate::checks::{
    Checked, ValueIssue, ValueProblem, animated, any, non_negative, scalar, unit, validate_paint,
    vec2,
};
use crate::layout_values::{
    GridEdge, GridPlacement, Insets, LayoutLength, LayoutOffset, LayoutSize, RepeatCount,
    TrackComponent, TrackList,
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
/// Most explicit tracks on one axis of a grid, `repeat()` counts expanded.
pub const MAX_GRID_TRACKS: usize = 64;
/// The farthest grid line a placement names, from either end.
const MAX_GRID_LINE: i16 = 65;

/// The initial text style: what the root inherits.
pub const LAYOUT_TEXT_DEFAULTS: LayoutTextStyle = LayoutTextStyle {
    font_family: None,
    font_size: 0.05,
    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
    line_height: 1.2,
    text_align: TextAlign::Start,
    outline: None,
};

/// A box's text style once inherited: what its text draws with. Lengths are
/// fractions of the picture's shorter side.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutTextStyle {
    /// The family the text draws in, as a text style's `font_family`.
    pub font_family: Option<String>,
    /// Font size as a fraction of the picture's shorter side.
    pub font_size: f32,
    /// Text color.
    pub color: Rgba,
    /// Baseline-to-baseline distance in font sizes.
    pub line_height: f32,
    /// How lines align within the content width.
    pub text_align: TextAlign,
    /// A band around the glyphs; its width a fraction of the font size.
    pub outline: Option<TextOutline>,
}

/// One box of a layout tree.
///
/// A box lays its children out by flexbox (`display: flex`, the default) or
/// grid (`display: grid`). It is a container when it has children, a leaf
/// when it has text or shape content, and empty otherwise (a spacer or a
/// decoration). Per box, its background is drawn first, then its border,
/// then its content. Boxes are drawn in document order; a reversed
/// direction or a grid placement moves them, not their order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutNode {
    /// Boxes laid out in this one, in document (and paint) order. Not with
    /// `text` or `shape`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<LayoutNode>,
    /// The box's text, wrapped at its content width and drawn in the style
    /// it inherits; `\n` starts a line. At most [`MAX_TEXT_CONTENT_BYTES`]
    /// bytes per value; may be empty. Its keyframes all `hold`. Not with
    /// `children` or `shape`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Animated<String>>,
    /// Shapes drawn on a picture the size of the box's content box. Not with
    /// `children` or `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<LayoutShape>,

    /// How the box lays its children out. Default `flex`. Not on a box with
    /// text or shape content, as every container field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    /// The main axis of a flex container's children. Default `row`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_direction: Option<FlexDirection>,
    /// Whether a flex container's children wrap onto more lines. Default
    /// `nowrap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_wrap: Option<FlexWrap>,
    /// A grid's explicit columns, as CSS writes them (`"1fr 2fr"`,
    /// `"repeat(3, minmax(100px, 1fr))"`). Default none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_template_columns: Option<TrackList>,
    /// A grid's explicit rows. Default none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_template_rows: Option<TrackList>,
    /// The size of a grid's implicit columns. Default `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_auto_columns: Option<TrackList>,
    /// The size of a grid's implicit rows. Default `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_auto_rows: Option<TrackList>,
    /// How a grid places items without a placement. Default `row`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_auto_flow: Option<GridAutoFlow>,
    /// Space between children, on both axes. At least 0; default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Animated<LayoutSize>>,
    /// Space between rows (between lines of a wrapping row flex
    /// container), in place of `gap`. At least 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_gap: Option<Animated<LayoutSize>>,
    /// Space between columns, in place of `gap`. At least 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_gap: Option<Animated<LayoutSize>>,
    /// Where children lie along the main (inline, for a grid) axis. Default
    /// `start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify_content: Option<JustifyContent>,
    /// Where children lie across the main axis, or in their grid areas'
    /// rows. Default `stretch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_items: Option<AlignItems>,
    /// Where lines of a wrapping flex container, or a grid's rows, lie
    /// across the container. Default `stretch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_content: Option<AlignContent>,
    /// Where a grid's children lie in their grid areas' columns. Default
    /// `stretch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify_items: Option<AlignItems>,

    /// Space inside the border: a length, the CSS shorthand
    /// (`"8px 16px"`), or the sides object. Sides at least 0; default 0.
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
    pub border_radius: Option<Animated<LayoutSize>>,
    /// Whether content past the padding box is drawn. Default `visible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<Overflow>,

    /// Border-box width. `None` is auto. Not on the root, as every item
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
    /// Space outside the border, written as `padding` is. Sides may be
    /// negative; there is no `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<Animated<Insets>>,
    /// How much of a flex container's free space the box takes. At least
    /// 0; default 0. Not in a grid, as every flex item field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_grow: Option<Animated<f32>>,
    /// How much the box gives up when a flex container's space runs short.
    /// At least 0; default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_shrink: Option<Animated<f32>>,
    /// The main size before growing and shrinking. `None` is auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_basis: Option<Animated<LayoutLength>>,
    /// Where the box lies across its flex container's main axis, or in its
    /// grid area's row. `None` follows the parent's `align_items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_self: Option<AlignItems>,
    /// Where the box lies in its grid area's column. `None` follows the
    /// parent's `justify_items`. Not in a flex container, as every grid
    /// item field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify_self: Option<AlignItems>,
    /// The box's columns in its grid, as CSS writes them (`2`, `"1 / 3"`,
    /// `"span 2"`). Default `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_column: Option<GridPlacement>,
    /// The box's rows in its grid. Default `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_row: Option<GridPlacement>,
    /// Whether the box is in its parent's flow. Default `relative`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    /// Where an `absolute` box lies in its parent's padding box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<LayoutInset>,

    /// The family the box's text draws in, as a text style's
    /// `font_family`. Inherited; the root's is the host's first font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size: positive, at most [`MAX_LAYOUT_FONT_SIZE`] of the
    /// picture's shorter side. Inherited; the root's is 0.05.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<Animated<LayoutSize>>,
    /// Text color. Inherited; the root's is white.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// Baseline-to-baseline distance in font sizes, in `[0.5, 3]`.
    /// Inherited; the root's is 1.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f32>,
    /// How lines align within the content width. Inherited; the root's is
    /// `start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_align: Option<TextAlign>,
    /// A band around the glyphs; its width a fraction of the font size.
    /// Inherited; the root has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<TextOutline>,

    /// Opacity of the box and everything in it, as one group, in `0..=1`.
    /// Drawn after layout; it never moves a box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<Animated<f32>>,
    /// Offset of the box and everything in it, after layout: the object, or
    /// one or two lengths as CSS `translate` writes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<Animated<LayoutOffset>>,
    /// Per-axis scale of the box and everything in it about its border-box
    /// centre, after layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Animated<Vec2>>,
    /// Clockwise rotation of the box and everything in it about its
    /// border-box centre, in degrees, after layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<Animated<f32>>,
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

/// A border inside the border box, as wide on every side.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LayoutBorder {
    /// Width. At least 0.
    pub width: Animated<LayoutSize>,
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

impl LayoutInset {
    fn sides(&self) -> [(&'static str, Option<&Animated<LayoutLength>>); 4] {
        [
            ("left", self.left.as_ref()),
            ("top", self.top.as_ref()),
            ("right", self.right.as_ref()),
            ("bottom", self.bottom.as_ref()),
        ]
    }
}

/// How a box lays its children out.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Display {
    /// CSS flexbox.
    #[default]
    Flex,
    /// CSS grid.
    Grid,
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

/// How a grid places items that have no placement of their own.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GridAutoFlow {
    /// Filling each row, adding rows.
    #[default]
    Row,
    /// Filling each column, adding columns.
    Column,
    /// As `row`, filling earlier holes.
    RowDense,
    /// As `column`, filling earlier holes.
    ColumnDense,
}

/// Where children lie along a container's main axis, or a grid's columns in
/// it.
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

/// Where the lines of a wrapping flex container, or a grid's rows, lie
/// across the container.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AlignContent {
    /// Packed at the start.
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
    /// Grown to fill the container.
    #[default]
    Stretch,
}

/// Where children lie across a flex container's main axis, or in their grid
/// areas.
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
    /// Across the whole line or area.
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
    /// `display` on a node with text or shape content.
    DisplayOnLeaf,
    /// A container field on a node with text or shape content.
    ContainerFieldOnLeaf,
    /// A size, margin, position, or item field on the root.
    RootItemField,
    /// A grid container field (named) on a flex container.
    GridFieldOnFlex(&'static str),
    /// A flex container field (named) on a grid container.
    FlexFieldOnGrid(&'static str),
    /// A flex item field (named) on a grid's child.
    FlexItemFieldInGrid(&'static str),
    /// A grid item field (named) on a flex container's child.
    GridItemFieldInFlex(&'static str),
    /// More than [`MAX_GRID_TRACKS`] explicit tracks on one axis.
    TooManyTracks,
    /// A `repeat()` count outside 1 to 64.
    RepeatCount,
    /// `repeat()` in implicit track sizes.
    RepeatInAutoTracks,
    /// An `auto-fill` or `auto-fit` repeat beside another, or with a track
    /// (in it or beside it) without a fixed size.
    AutoRepeatNeedsFixedTracks,
    /// A grid line outside -65 to 65, line 0, or a span outside 1 to 64.
    GridLineOutOfRange,
    /// Keyframes that are empty, out of order, or have an unusable curve.
    InvalidKeyframes,
    /// A value that is not finite or outside its range.
    OutOfRange,
    /// Size keyframes that mix a percent with a length.
    LengthKeyframesDiffer,
    /// Paint keyframes that differ in kind, stop count, or spread.
    PaintKeyframesDiffer,
    /// A text keyframe whose interpolation is not `hold`.
    TextKeyframesInterpolate,
}

impl fmt::Display for LayoutProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GridFieldOnFlex(field) => {
                return write!(
                    formatter,
                    "sets {field}, a grid container field, without display grid"
                );
            }
            Self::FlexFieldOnGrid(field) => {
                return write!(
                    formatter,
                    "sets {field}, a flex container field, on a grid container"
                );
            }
            Self::FlexItemFieldInGrid(field) => {
                return write!(
                    formatter,
                    "sets {field}, a flex item field, in a grid container"
                );
            }
            Self::GridItemFieldInFlex(field) => {
                return write!(
                    formatter,
                    "sets {field}, a grid item field, in a flex container"
                );
            }
            _ => {}
        }
        formatter.write_str(match self {
            Self::TooManyNodes => "has more nodes than a layout may hold",
            Self::TooDeep => "nests nodes too deeply",
            Self::TooManyTexts => "has more text nodes than a layout may hold",
            Self::TooManyIsolated => {
                "has more nodes with opacity, transforms, or hidden overflow than a layout may hold"
            }
            Self::MixedContent => "has more than one of children, text, and shape",
            Self::DisplayOnLeaf => "sets display on a node with text or shape content",
            Self::ContainerFieldOnLeaf => {
                "sets a container field on a node with text or shape content"
            }
            Self::RootItemField => "sets a size, margin, position, or item field on the root",
            Self::TooManyTracks => "has more than 64 tracks",
            Self::RepeatCount => "repeats tracks a number of times outside 1 to 64",
            Self::RepeatInAutoTracks => "uses repeat() in implicit track sizes",
            Self::AutoRepeatNeedsFixedTracks => {
                "repeats auto-fill or auto-fit beside another, or with a track without a fixed size"
            }
            Self::GridLineOutOfRange => {
                "names a grid line outside -65 to 65 or line 0, or a span outside 1 to 64"
            }
            Self::InvalidKeyframes => "has invalid keyframes",
            Self::OutOfRange => "has a value out of range",
            Self::LengthKeyframesDiffer => "has size keyframes that differ in kind",
            Self::PaintKeyframesDiffer => "has paint keyframes that differ in kind or stops",
            Self::TextKeyframesInterpolate => "has text keyframes that do not hold",
            Self::GridFieldOnFlex(_)
            | Self::FlexFieldOnGrid(_)
            | Self::FlexItemFieldInGrid(_)
            | Self::GridItemFieldInFlex(_) => unreachable!("written above"),
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

/// What a node is checked against: the picture's shorter side in canvas
/// pixels, when known, and its parent's `display`.
#[derive(Copy, Clone)]
struct Context {
    shorter_side: Option<f32>,
    parent: Option<Display>,
}

/// Checks a layout tree: its limits, its structure, every value's range, and
/// that animated values agree in kind between keyframes. Shape nodes' items
/// are checked as a shapes generator's are, counted together. With the
/// canvas (`(width, height)`), a pixel font size is checked against the
/// largest a layout's text may be.
///
/// # Errors
/// Returns the first problem in preorder.
pub(crate) fn validate_layout(root: &LayoutNode, canvas: Option<(u32, u32)>) -> LayoutChecked {
    let mut counts = Counts::default();
    if root.has_item_field() {
        return Err(LayoutIssue::new("root", LayoutProblem::RootItemField).into());
    }
    #[allow(clippy::cast_precision_loss)]
    let shorter_side = canvas.map(|(width, height)| width.min(height) as f32);
    let context = Context {
        shorter_side,
        parent: None,
    };
    validate_node(root, "root", 1, context, &mut counts)
}

fn validate_node(
    node: &LayoutNode,
    at: &str,
    depth: usize,
    context: Context,
    counts: &mut Counts,
) -> LayoutChecked {
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
    let leaf = node.text.is_some() || node.shape.is_some();
    if leaf && node.display.is_some() {
        return issue(LayoutProblem::DisplayOnLeaf);
    }
    if leaf && node.has_container_field() {
        return issue(LayoutProblem::ContainerFieldOnLeaf);
    }
    let display = node.display.unwrap_or_default();
    let mismatch = match display {
        Display::Flex => node
            .grid_container_field()
            .map(LayoutProblem::GridFieldOnFlex),
        Display::Grid => node
            .flex_container_field()
            .map(LayoutProblem::FlexFieldOnGrid),
    };
    if let Some(problem) = mismatch {
        return issue(problem);
    }
    let item_mismatch = match context.parent {
        Some(Display::Flex) => node
            .grid_item_field()
            .map(LayoutProblem::GridItemFieldInFlex),
        Some(Display::Grid) => node
            .flex_item_field()
            .map(LayoutProblem::FlexItemFieldInGrid),
        None => None,
    };
    if let Some(problem) = item_mismatch {
        return issue(problem);
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
    validate_grid(node, at)?;
    validate_style(node, at, context.shorter_side)?;
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
    let inner = Context {
        shorter_side: context.shorter_side,
        parent: Some(display),
    };
    for (index, child) in node.children.iter().enumerate() {
        validate_node(
            child,
            &format!("{at}.children[{index}]"),
            depth + 1,
            inner,
            counts,
        )?;
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
    for (name, gap) in [
        ("gap", &node.gap),
        ("row_gap", &node.row_gap),
        ("column_gap", &node.column_gap),
    ] {
        optional(gap.as_ref(), |gap| size(gap, &field(name), non_negative))?;
    }
    optional(node.padding.as_ref(), |padding| {
        insets(padding, &field("padding"), non_negative)
    })?;
    optional(node.background.as_ref(), |paint| {
        validate_paint(paint, &field("background"))
    })?;
    if let Some(border) = &node.border {
        size(&border.width, &field("border.width"), non_negative)?;
        validate_paint(&border.paint, &field("border.paint"))?;
    }
    optional(node.border_radius.as_ref(), |radius| {
        size(radius, &field("border_radius"), non_negative)
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
        for (name, side) in inset.sides() {
            optional(side, |side| {
                length(side, &field(&format!("inset.{name}")), any)
            })?;
        }
    }
    optional(node.opacity.as_ref(), |opacity| {
        scalar(opacity, &field("opacity"), unit)
    })?;
    optional(node.translate.as_ref(), |translate| {
        animated(translate, &field("translate"), |offset| {
            offset.x.value().is_finite() && offset.y.value().is_finite()
        })
    })?;
    optional(node.scale.as_ref(), |scale| {
        vec2(scale, &field("scale"), |_| true)
    })?;
    optional(node.rotation_degrees.as_ref(), |rotation| {
        scalar(rotation, &field("rotation_degrees"), any)
    })
}

/// The grid fields: track counts and sizes, and placement lines.
fn validate_grid(node: &LayoutNode, at: &str) -> LayoutChecked {
    let issue = |name: &str, problem| Err(LayoutIssue::new(format!("{at}.{name}"), problem).into());
    for (name, tracks, explicit) in [
        ("grid_template_columns", &node.grid_template_columns, true),
        ("grid_template_rows", &node.grid_template_rows, true),
        ("grid_auto_columns", &node.grid_auto_columns, false),
        ("grid_auto_rows", &node.grid_auto_rows, false),
    ] {
        let Some(tracks) = tracks else { continue };
        if let Some(problem) = track_problem(tracks, explicit) {
            return issue(name, problem);
        }
    }
    for (name, placement) in [
        ("grid_column", node.grid_column),
        ("grid_row", node.grid_row),
    ] {
        let in_range = |edge: GridEdge| match edge {
            GridEdge::Auto => true,
            GridEdge::Line(line) => line != 0 && line.abs() <= MAX_GRID_LINE,
            GridEdge::Span(count) => (1..=64).contains(&count),
        };
        if placement.is_some_and(|placement| !placement.edges().into_iter().all(in_range)) {
            return issue(name, LayoutProblem::GridLineOutOfRange);
        }
    }
    Ok(())
}

fn track_problem(tracks: &TrackList, explicit: bool) -> Option<LayoutProblem> {
    let lengths_valid = tracks
        .lengths()
        .all(|length| length.value().is_finite() && length.value() >= 0.0);
    let shares_valid = tracks
        .shares()
        .all(|share| share.is_finite() && share > 0.0);
    if !(lengths_valid && shares_valid) {
        return Some(LayoutProblem::OutOfRange);
    }
    let repeats = tracks.0.iter().filter_map(|component| match component {
        TrackComponent::Repeat(count, _) => Some(*count),
        TrackComponent::Single(_) => None,
    });
    if !explicit && repeats.clone().next().is_some() {
        return Some(LayoutProblem::RepeatInAutoTracks);
    }
    if repeats
        .clone()
        .any(|count| matches!(count, RepeatCount::Count(count) if !(1..=64).contains(&count)))
    {
        return Some(LayoutProblem::RepeatCount);
    }
    if tracks.track_count() > MAX_GRID_TRACKS {
        return Some(LayoutProblem::TooManyTracks);
    }
    let automatic = repeats
        .filter(|count| !matches!(count, RepeatCount::Count(_)))
        .count();
    if automatic > 1
        || (automatic == 1
            && !tracks
                .sizes()
                .all(crate::layout_values::TrackSize::is_fixed))
    {
        return Some(LayoutProblem::AutoRepeatNeedsFixedTracks);
    }
    None
}

/// The inherited text style a node writes, where it writes it.
fn validate_style(node: &LayoutNode, at: &str, shorter_side: Option<f32>) -> LayoutChecked {
    let out_of_range = |name: &str| {
        Err(LayoutIssue::new(format!("{at}.{name}"), LayoutProblem::OutOfRange).into())
    };
    if node
        .font_family
        .as_ref()
        .is_some_and(|family| family.is_empty() || family.len() > MAX_FONT_FAMILY_BYTES)
    {
        return out_of_range("font_family");
    }
    optional(node.font_size.as_ref(), |font_size| {
        animated(font_size, &format!("{at}.font_size"), |value| {
            // Pixels compare with the limit on a known canvas only.
            let fraction = match (*value, shorter_side) {
                (LayoutSize::Px(_), None) => 0.0,
                (value, Some(side)) => value.fraction(side),
                (value, None) => value.value(),
            };
            value.value().is_finite() && value.value() > 0.0 && fraction <= MAX_LAYOUT_FONT_SIZE
        })
    })?;
    if node.color.is_some_and(|color| !color.is_valid()) {
        return out_of_range("color");
    }
    if node
        .line_height
        .is_some_and(|height| !LINE_HEIGHT_RANGE.contains(&height))
    {
        return out_of_range("line_height");
    }
    let valid_outline = |outline: TextOutline| {
        outline.color.is_valid() && outline.width > 0.0 && outline.width <= MAX_TEXT_OUTLINE_WIDTH
    };
    if node.outline.is_some_and(|outline| !valid_outline(outline)) {
        return out_of_range("outline");
    }
    Ok(())
}

fn insets(animation: &Animated<Insets>, at: &str, check: fn(f32) -> bool) -> Checked {
    animated(animation, at, |value| {
        value
            .sides()
            .iter()
            .all(|side| side.value().is_finite() && check(side.value()))
    })
}

fn size(animation: &Animated<LayoutSize>, at: &str, check: fn(f32) -> bool) -> Checked {
    animated(animation, at, |value| {
        value.value().is_finite() && check(value.value())
    })
}

/// A length's values, and that its keyframes share a kind: a percent, or a
/// length (a fraction or pixels, both lengths once planned).
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

fn validate_text(content: &Animated<String>, at: &str) -> LayoutChecked {
    animated(content, at, |_| true)?;
    if let Animated::Keyframes { keyframes } = content
        && keyframes
            .iter()
            .any(|key| key.interpolation != Interpolation::Hold)
    {
        return Err(LayoutIssue::new(at, LayoutProblem::TextKeyframesInterpolate).into());
    }
    animated(content, at, |content| {
        content.len() <= MAX_TEXT_CONTENT_BYTES
    })?;
    Ok(())
}

impl LayoutNode {
    /// Whether any container field is present.
    fn has_container_field(&self) -> bool {
        self.flex_container_field().is_some()
            || self.grid_container_field().is_some()
            || self.gap.is_some()
            || self.row_gap.is_some()
            || self.column_gap.is_some()
            || self.justify_content.is_some()
            || self.align_items.is_some()
            || self.align_content.is_some()
    }

    /// The first flex container field present.
    fn flex_container_field(&self) -> Option<&'static str> {
        [
            ("flex_direction", self.flex_direction.is_some()),
            ("flex_wrap", self.flex_wrap.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name))
    }

    /// The first grid container field present.
    fn grid_container_field(&self) -> Option<&'static str> {
        [
            (
                "grid_template_columns",
                self.grid_template_columns.is_some(),
            ),
            ("grid_template_rows", self.grid_template_rows.is_some()),
            ("grid_auto_columns", self.grid_auto_columns.is_some()),
            ("grid_auto_rows", self.grid_auto_rows.is_some()),
            ("grid_auto_flow", self.grid_auto_flow.is_some()),
            ("justify_items", self.justify_items.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name))
    }

    /// The first flex item field present.
    fn flex_item_field(&self) -> Option<&'static str> {
        [
            ("flex_grow", self.flex_grow.is_some()),
            ("flex_shrink", self.flex_shrink.is_some()),
            ("flex_basis", self.flex_basis.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name))
    }

    /// The first grid item field present.
    fn grid_item_field(&self) -> Option<&'static str> {
        [
            ("grid_column", self.grid_column.is_some()),
            ("grid_row", self.grid_row.is_some()),
            ("justify_self", self.justify_self.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name))
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
            || self.flex_item_field().is_some()
            || self.grid_item_field().is_some()
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
            inset
                .sides()
                .into_iter()
                .any(|(_, value)| keyed_option(value))
        });
        let shape = self
            .shape
            .as_ref()
            .is_some_and(|shape| shape.items.iter().any(ShapeItem::is_animated));
        let border = self
            .border
            .as_ref()
            .is_some_and(|border| keyed(&border.width) || keyed(&border.paint));
        [
            &self.gap,
            &self.row_gap,
            &self.column_gap,
            &self.border_radius,
            &self.font_size,
        ]
        .into_iter()
        .any(|value| keyed_option(value.as_ref()))
            || keyed_option(self.padding.as_ref())
            || keyed_option(self.background.as_ref())
            || border
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
            || keyed_option(self.text.as_ref())
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

    /// The tree with every pixel length turned into a fraction of a picture
    /// `shorter_side` pixels on its shorter side: what a plan evaluates, so
    /// that pixels and fractions interpolate as one kind.
    #[must_use]
    pub fn without_px(&self, shorter_side: f32) -> Self {
        let mut node = self.clone();
        node.convert_px(shorter_side);
        node
    }

    fn convert_px(&mut self, shorter_side: f32) {
        let size = |value: &mut Option<Animated<LayoutSize>>| {
            if let Some(value) = value {
                *value = value.map(|size| size.without_px(shorter_side));
            }
        };
        let length = |value: &mut Option<Animated<LayoutLength>>| {
            if let Some(value) = value {
                *value = value.map(|length| length.without_px(shorter_side));
            }
        };
        let insets = |value: &mut Option<Animated<Insets>>| {
            if let Some(value) = value {
                *value = value.map(|insets| insets.without_px(shorter_side));
            }
        };
        let tracks = |value: &mut Option<TrackList>| {
            if let Some(tracks) = value {
                *tracks = tracks.without_px(shorter_side);
            }
        };
        tracks(&mut self.grid_template_columns);
        tracks(&mut self.grid_template_rows);
        tracks(&mut self.grid_auto_columns);
        tracks(&mut self.grid_auto_rows);
        for value in [
            &mut self.gap,
            &mut self.row_gap,
            &mut self.column_gap,
            &mut self.border_radius,
            &mut self.font_size,
        ] {
            size(value);
        }
        insets(&mut self.padding);
        insets(&mut self.margin);
        if let Some(border) = &mut self.border {
            border.width = border.width.map(|width| width.without_px(shorter_side));
        }
        for value in [
            &mut self.width,
            &mut self.height,
            &mut self.min_width,
            &mut self.min_height,
            &mut self.max_width,
            &mut self.max_height,
            &mut self.flex_basis,
        ] {
            length(value);
        }
        if let Some(inset) = &mut self.inset {
            for side in [
                &mut inset.left,
                &mut inset.top,
                &mut inset.right,
                &mut inset.bottom,
            ] {
                length(side);
            }
        }
        if let Some(translate) = &mut self.translate {
            *translate = translate.map(|offset| offset.without_px(shorter_side));
        }
        for child in &mut self.children {
            child.convert_px(shorter_side);
        }
    }
}

const fn keyed<T>(animation: &Animated<T>) -> bool {
    matches!(animation, Animated::Keyframes { .. })
}

const fn keyed_option<T>(animation: Option<&Animated<T>>) -> bool {
    matches!(animation, Some(Animated::Keyframes { .. }))
}
