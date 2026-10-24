//! The content tree of a shapes generator: geometry, paints, and modifiers in
//! the order an After Effects or Lottie shape layer lists them.
//!
//! Positions (vertices, tangents, centres, a group's position and anchor) and
//! sizes are normalized as the generator's picture is, `(0, 0)` top-left to
//! `(1, 1)` bottom-right; lengths (radii, stroke widths, dashes) are fractions
//! of the picture's shorter side. Rotation and scale act in pixel space, so a
//! rotated square stays square on a wide canvas.

use crate::animation::valid_keyframes;
use crate::checks::{
    ValueIssue, ValueProblem, any, non_negative, point, scalar, unit, validate_paint, vec2,
};
use crate::{Animated, AnimatedPoint, Paint, Vec2};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Most items one shapes generator holds, nested groups and their items
/// included.
pub const MAX_SHAPE_ITEMS: usize = 1_024;
/// Deepest group nesting in one shapes generator.
pub const MAX_SHAPE_DEPTH: usize = 32;
/// Most vertices one path holds, over all its subpaths.
pub const MAX_PATH_VERTICES: usize = 4_096;
/// Most subpaths one path holds.
pub const MAX_PATH_SUBPATHS: usize = 256;
/// Most entries in a stroke's dash pattern.
pub const MAX_DASH_ENTRIES: usize = 16;
/// Most points a polystar has. A fractional count is valid; its fraction is
/// dropped when drawn.
pub const MAX_POLYSTAR_POINTS: f32 = 100.0;
/// Fewest points a polystar has.
pub const MIN_POLYSTAR_POINTS: f32 = 3.0;

/// One entry of a shapes generator's content tree.
///
/// A group is read front to back. Geometry accumulates in its group; a fill
/// or a stroke paints the geometry listed before it in the same group,
/// nested groups before it included; a trim cuts that geometry and replaces
/// it. A paint listed earlier is drawn on top of one listed later, as in
/// After Effects and Lottie.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShapeItem {
    /// Items drawn together, through one transform and opacity. Their
    /// geometry also counts as the enclosing group's, for the paints and
    /// trims after the group.
    Group {
        /// The group's items, front first.
        items: Vec<ShapeItem>,
        /// Where the group lies, and how opaque it is.
        #[serde(default)]
        transform: ShapeTransform,
    },
    /// A rectangle, optionally with rounded corners.
    Rectangle(RectangleGeometry),
    /// An ellipse.
    Ellipse(EllipseGeometry),
    /// A regular polygon or star.
    Polystar(PolystarGeometry),
    /// A Bezier path.
    Path(PathGeometry),
    /// Paints the inside of the geometry listed before it in its group, all
    /// of it as one compound path under `rule`. An open path is closed for
    /// it.
    Fill {
        /// What covers it.
        paint: Animated<Paint>,
        /// Opacity in `0..=1`.
        #[serde(default = "opaque")]
        opacity: Animated<f32>,
        /// Which regions of the compound path are inside.
        #[serde(default)]
        rule: FillRule,
    },
    /// Strokes the geometry listed before it in its group.
    Stroke(StrokeItem),
    /// Keeps part of the geometry listed before it in its group, by length.
    Trim(TrimItem),
}

pub(crate) fn opaque() -> Animated<f32> {
    Animated::Constant { value: 1.0 }
}

fn zero() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
}

/// A geometry alone, as a mask's outline is.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShapeGeometry {
    /// A rectangle, optionally with rounded corners.
    Rectangle(RectangleGeometry),
    /// An ellipse.
    Ellipse(EllipseGeometry),
    /// A regular polygon or star.
    Polystar(PolystarGeometry),
    /// A Bezier path.
    Path(PathGeometry),
}

/// A rectangle about its centre, turned clockwise about it. Its outline
/// starts at the top-right corner and runs clockwise.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RectangleGeometry {
    /// Centre.
    pub center: AnimatedPoint,
    /// Full width and height, before rotation. Non-negative.
    pub size: Animated<Vec2>,
    /// Corner radius, as a fraction of the shorter side. Non-negative; a
    /// radius past half the rectangle is capped.
    #[serde(default = "zero")]
    pub corner_radius: Animated<f32>,
    /// Clockwise rotation about the centre, in degrees.
    #[serde(default = "zero")]
    pub rotation_degrees: Animated<f32>,
}

/// The ellipse inscribed in a size, turned clockwise about its centre. Its
/// outline starts at the top and runs clockwise.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EllipseGeometry {
    /// Centre.
    pub center: AnimatedPoint,
    /// Full width and height, before rotation. Non-negative.
    pub size: Animated<Vec2>,
    /// Clockwise rotation about the centre, in degrees.
    #[serde(default = "zero")]
    pub rotation_degrees: Animated<f32>,
}

/// A regular polygon or a star, its first point straight up from the centre
/// before rotation, the rest clockwise.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PolystarGeometry {
    /// A polygon or a star.
    pub kind: PolystarKind,
    /// Centre.
    pub center: AnimatedPoint,
    /// Number of points (corners of a polygon, tips of a star), from
    /// [`MIN_POLYSTAR_POINTS`] to [`MAX_POLYSTAR_POINTS`]. A fraction is
    /// dropped when drawn, as Lottie drops it, so an animated count changes
    /// the figure one whole point at a time.
    pub points: Animated<f32>,
    /// Distance of the points from the centre, as a fraction of the shorter
    /// side. Non-negative.
    pub outer_radius: Animated<f32>,
    /// A star's distance of its inner corners from the centre. Non-negative.
    #[serde(default = "zero")]
    pub inner_radius: Animated<f32>,
    /// How round the outer corners are: 0 sharp, 1 a smooth curve through
    /// them. In `0..=1`.
    #[serde(default = "zero")]
    pub outer_roundness: Animated<f32>,
    /// How round a star's inner corners are, in `0..=1`.
    #[serde(default = "zero")]
    pub inner_roundness: Animated<f32>,
    /// Clockwise rotation about the centre, in degrees.
    #[serde(default = "zero")]
    pub rotation_degrees: Animated<f32>,
}

/// What kind of figure a polystar is.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolystarKind {
    /// Points joined by inner corners.
    #[default]
    Star,
    /// Points joined by straight sides.
    Polygon,
}

/// A path, animated as one value.
///
/// Keyframes must agree in structure: the same number of subpaths, each
/// closed or open alike and with the same number of vertices. Between two
/// keyframes every point and tangent moves along a straight line.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PathGeometry {
    /// The path.
    pub path: Animated<Path>,
}

/// One or more subpaths.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Path {
    /// The subpaths, at most [`MAX_PATH_SUBPATHS`], with at most
    /// [`MAX_PATH_VERTICES`] vertices among them.
    pub subpaths: Vec<Subpath>,
}

/// A run of vertices joined by cubic Bezier segments.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Subpath {
    /// Whether a segment joins the last vertex back to the first.
    #[serde(default)]
    pub closed: bool,
    /// The vertices, in order.
    pub vertices: Vec<PathVertex>,
}

/// A path vertex and its tangents. The segment from vertex `a` to vertex
/// `b` is the cubic Bezier through `a.point`, `a.point + a.out_tangent`,
/// `b.point + b.in_tangent`, and `b.point`; zero tangents make it straight.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PathVertex {
    /// Where the vertex lies.
    pub point: Vec2,
    /// The control point of the segment ending here, relative to `point`.
    #[serde(default)]
    pub in_tangent: Vec2,
    /// The control point of the segment starting here, relative to `point`.
    #[serde(default)]
    pub out_tangent: Vec2,
}

/// A group's placement: in pixel space, translate by `position`, rotate,
/// scale, and translate by minus `anchor`, so `anchor` lands on `position`.
/// The default leaves its items where they are.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ShapeTransform {
    /// The point of the items that is placed at `position`.
    #[serde(default = "origin")]
    pub anchor: Animated<Vec2>,
    /// Where `anchor` is placed.
    #[serde(default)]
    pub position: AnimatedPoint,
    /// Per-axis scale; 1 leaves the items unscaled. Stroke widths scale with
    /// it.
    #[serde(default = "unscaled")]
    pub scale: Animated<Vec2>,
    /// Clockwise rotation, in degrees.
    #[serde(default = "zero")]
    pub rotation_degrees: Animated<f32>,
    /// Opacity of the group as a whole, in `0..=1`: overlapping items are
    /// composited first and faded together.
    #[serde(default = "opaque")]
    pub opacity: Animated<f32>,
}

fn origin() -> Animated<Vec2> {
    Animated::Constant {
        value: Vec2::new(0.0, 0.0),
    }
}

fn unscaled() -> Animated<Vec2> {
    Animated::Constant {
        value: Vec2::new(1.0, 1.0),
    }
}

impl Default for ShapeTransform {
    fn default() -> Self {
        Self {
            anchor: origin(),
            position: AnimatedPoint::default(),
            scale: unscaled(),
            rotation_degrees: zero(),
            opacity: opaque(),
        }
    }
}

/// A stroke along geometry, centred on its outline.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StrokeItem {
    /// What covers the stroke.
    pub paint: Animated<Paint>,
    /// Full width, as a fraction of the shorter side. Non-negative; zero
    /// draws nothing.
    pub width: Animated<f32>,
    /// Opacity in `0..=1`.
    #[serde(default = "opaque")]
    pub opacity: Animated<f32>,
    /// How segments meet.
    #[serde(default)]
    pub join: LineJoin,
    /// Longest a miter join reaches, as a multiple of half the width, before
    /// it is beveled. At least 1.
    #[serde(default = "four")]
    pub miter_limit: f32,
    /// How open ends look.
    #[serde(default)]
    pub cap: LineCap,
    /// Dashes, or a solid stroke when absent.
    #[serde(default)]
    pub dash: Option<Dash>,
}

const fn four() -> f32 {
    4.0
}

/// How stroke segments meet at a vertex.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LineJoin {
    /// Extended edges meeting at a point, up to the miter limit.
    #[default]
    Miter,
    /// A circular arc.
    Round,
    /// A straight cut across the corner.
    Bevel,
}

/// How an open stroke ends.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LineCap {
    /// Flat at the end point.
    #[default]
    Butt,
    /// A half circle past the end point.
    Round,
    /// A half square past the end point.
    Square,
}

/// A dash pattern.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Dash {
    /// Alternating dash and gap lengths, as fractions of the shorter side:
    /// one to [`MAX_DASH_ENTRIES`] non-negative entries, not all zero. An
    /// odd count repeats once more to pair up.
    pub pattern: Vec<Animated<f32>>,
    /// How far into the pattern the stroke starts.
    #[serde(default = "zero")]
    pub offset: Animated<f32>,
}

/// Keeps a span of geometry's length.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TrimItem {
    /// Where the kept span starts, as a fraction of the length, in `0..=1`.
    pub start: Animated<f32>,
    /// Where it ends, in `0..=1`. Nothing is kept unless it is after
    /// `start`.
    pub end: Animated<f32>,
    /// Turns the span is shifted by; it wraps around a closed path.
    #[serde(default = "zero")]
    pub offset: Animated<f32>,
    /// Whether the span is of all the geometry together or of each part.
    #[serde(default)]
    pub mode: TrimMode,
}

/// What a trim measures.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TrimMode {
    /// One span over the length of every subpath end to end, in order.
    #[default]
    Together,
    /// A span of each subpath alone.
    Individually,
}

/// Which regions of a path are inside it.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FillRule {
    /// Where the outline winds around a point at all.
    #[default]
    Nonzero,
    /// Where it winds around a point an odd number of times.
    Evenodd,
}

/// Why a content tree or a mask outline is invalid.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ShapeProblem {
    /// More than [`MAX_SHAPE_ITEMS`] items.
    TooManyItems,
    /// Groups nested deeper than [`MAX_SHAPE_DEPTH`].
    TooDeep,
    /// A path with more than [`MAX_PATH_SUBPATHS`] subpaths or
    /// [`MAX_PATH_VERTICES`] vertices.
    PathTooLarge,
    /// A dash pattern with no entries, more than [`MAX_DASH_ENTRIES`], or
    /// only zeros.
    InvalidDash,
    /// Keyframes that are empty, out of order, or have an unusable curve.
    InvalidKeyframes,
    /// A value that is not finite or outside its range.
    OutOfRange,
    /// Path keyframes that differ in subpath count, closing, or vertex
    /// count.
    PathKeyframesDiffer,
    /// Paint keyframes that differ in kind, stop count, or spread.
    PaintKeyframesDiffer,
}

impl fmt::Display for ShapeProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyItems => "has more items than a shapes generator may hold",
            Self::TooDeep => "nests groups too deeply",
            Self::PathTooLarge => "has more subpaths or vertices than a path may hold",
            Self::InvalidDash => "has an invalid dash pattern",
            Self::InvalidKeyframes => "has invalid keyframes",
            Self::OutOfRange => "has a value out of range",
            Self::PathKeyframesDiffer => "has path keyframes that differ in structure",
            Self::PaintKeyframesDiffer => "has paint keyframes that differ in kind or stops",
        })
    }
}

/// Where in a content tree a problem lies, and what it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShapeIssue {
    /// The item's path from the tree's root, like `items[2].items[0].path`,
    /// or a mask's field, like `shape.size`.
    pub at: String,
    /// What is wrong there.
    pub problem: ShapeProblem,
}

impl From<ValueIssue> for ShapeIssue {
    fn from(issue: ValueIssue) -> Self {
        Self::new(
            issue.at,
            match issue.problem {
                ValueProblem::InvalidKeyframes => ShapeProblem::InvalidKeyframes,
                ValueProblem::OutOfRange => ShapeProblem::OutOfRange,
                ValueProblem::PaintKeyframesDiffer => ShapeProblem::PaintKeyframesDiffer,
            },
        )
    }
}

impl ShapeIssue {
    fn new(at: impl Into<String>, problem: ShapeProblem) -> Self {
        Self {
            at: at.into(),
            problem,
        }
    }
}

type Checked = Result<(), ShapeIssue>;

/// Checks a content tree: its limits, every value's range, and that
/// animated paths and paints agree in structure between keyframes.
///
/// # Errors
/// Returns the first problem in document order.
pub fn validate_shape_items(items: &[ShapeItem]) -> Result<(), ShapeIssue> {
    let mut count = 0;
    validate_items(items, "items", 1, &mut count)
}

pub(crate) fn validate_items(
    items: &[ShapeItem],
    at: &str,
    depth: usize,
    count: &mut usize,
) -> Checked {
    if depth > MAX_SHAPE_DEPTH {
        return Err(ShapeIssue::new(at, ShapeProblem::TooDeep));
    }
    for (index, item) in items.iter().enumerate() {
        let at = format!("{at}[{index}]");
        *count += 1;
        if *count > MAX_SHAPE_ITEMS {
            return Err(ShapeIssue::new(at, ShapeProblem::TooManyItems));
        }
        match item {
            ShapeItem::Group { items, transform } => {
                validate_transform(transform, &format!("{at}.transform"))?;
                validate_items(items, &format!("{at}.items"), depth + 1, count)?;
            }
            ShapeItem::Rectangle(geometry) => validate_rectangle(geometry, &at)?,
            ShapeItem::Ellipse(geometry) => validate_ellipse(geometry, &at)?,
            ShapeItem::Polystar(geometry) => validate_polystar(geometry, &at)?,
            ShapeItem::Path(geometry) => validate_path(&geometry.path, &format!("{at}.path"))?,
            ShapeItem::Fill { paint, opacity, .. } => {
                validate_paint(paint, &format!("{at}.paint"))?;
                scalar(opacity, &format!("{at}.opacity"), unit)?;
            }
            ShapeItem::Stroke(stroke) => validate_stroke(stroke, &at)?,
            ShapeItem::Trim(trim) => {
                scalar(&trim.start, &format!("{at}.start"), unit)?;
                scalar(&trim.end, &format!("{at}.end"), unit)?;
                scalar(&trim.offset, &format!("{at}.offset"), any)?;
            }
        }
    }
    Ok(())
}

impl ShapeGeometry {
    /// Checks the outline as a content tree's geometry is checked; `at`
    /// names it in the issue.
    ///
    /// # Errors
    /// Returns the first problem in document order.
    pub fn validate(&self, at: &str) -> Result<(), ShapeIssue> {
        match self {
            Self::Rectangle(geometry) => validate_rectangle(geometry, at),
            Self::Ellipse(geometry) => validate_ellipse(geometry, at),
            Self::Polystar(geometry) => validate_polystar(geometry, at),
            Self::Path(geometry) => validate_path(&geometry.path, &format!("{at}.path")),
        }
    }
}

fn sizes(value: Vec2) -> bool {
    value.x >= 0.0 && value.y >= 0.0
}

fn validate_rectangle(geometry: &RectangleGeometry, at: &str) -> Checked {
    point(&geometry.center, &format!("{at}.center"))?;
    vec2(&geometry.size, &format!("{at}.size"), sizes)?;
    scalar(
        &geometry.corner_radius,
        &format!("{at}.corner_radius"),
        non_negative,
    )?;
    scalar(
        &geometry.rotation_degrees,
        &format!("{at}.rotation_degrees"),
        any,
    )?;
    Ok(())
}

fn validate_ellipse(geometry: &EllipseGeometry, at: &str) -> Checked {
    point(&geometry.center, &format!("{at}.center"))?;
    vec2(&geometry.size, &format!("{at}.size"), sizes)?;
    scalar(
        &geometry.rotation_degrees,
        &format!("{at}.rotation_degrees"),
        any,
    )?;
    Ok(())
}

fn validate_polystar(geometry: &PolystarGeometry, at: &str) -> Checked {
    point(&geometry.center, &format!("{at}.center"))?;
    scalar(&geometry.points, &format!("{at}.points"), |points| {
        (MIN_POLYSTAR_POINTS..=MAX_POLYSTAR_POINTS).contains(&points)
    })?;
    scalar(
        &geometry.outer_radius,
        &format!("{at}.outer_radius"),
        non_negative,
    )?;
    scalar(
        &geometry.inner_radius,
        &format!("{at}.inner_radius"),
        non_negative,
    )?;
    scalar(
        &geometry.outer_roundness,
        &format!("{at}.outer_roundness"),
        unit,
    )?;
    scalar(
        &geometry.inner_roundness,
        &format!("{at}.inner_roundness"),
        unit,
    )?;
    scalar(
        &geometry.rotation_degrees,
        &format!("{at}.rotation_degrees"),
        any,
    )?;
    Ok(())
}

fn validate_transform(transform: &ShapeTransform, at: &str) -> Checked {
    vec2(&transform.anchor, &format!("{at}.anchor"), |_| true)?;
    point(&transform.position, &format!("{at}.position"))?;
    vec2(&transform.scale, &format!("{at}.scale"), |_| true)?;
    scalar(
        &transform.rotation_degrees,
        &format!("{at}.rotation_degrees"),
        any,
    )?;
    scalar(&transform.opacity, &format!("{at}.opacity"), unit)?;
    Ok(())
}

fn validate_stroke(stroke: &StrokeItem, at: &str) -> Checked {
    validate_paint(&stroke.paint, &format!("{at}.paint"))?;
    scalar(&stroke.width, &format!("{at}.width"), non_negative)?;
    scalar(&stroke.opacity, &format!("{at}.opacity"), unit)?;
    if !stroke.miter_limit.is_finite() || stroke.miter_limit < 1.0 {
        return Err(ShapeIssue::new(
            format!("{at}.miter_limit"),
            ShapeProblem::OutOfRange,
        ));
    }
    if let Some(dash) = &stroke.dash {
        let at = format!("{at}.dash");
        if dash.pattern.is_empty() || dash.pattern.len() > MAX_DASH_ENTRIES {
            return Err(ShapeIssue::new(
                format!("{at}.pattern"),
                ShapeProblem::InvalidDash,
            ));
        }
        for (index, entry) in dash.pattern.iter().enumerate() {
            scalar(entry, &format!("{at}.pattern[{index}]"), non_negative)?;
        }
        // A pattern of only zeros has no length to repeat over. Animated entries are judged
        // by their keyframes: some keyframe must be positive.
        let positive = dash.pattern.iter().any(|entry| match entry {
            Animated::Constant { value } => *value > 0.0,
            Animated::Keyframes { keyframes } => keyframes.iter().any(|key| key.value > 0.0),
        });
        if !positive {
            return Err(ShapeIssue::new(
                format!("{at}.pattern"),
                ShapeProblem::InvalidDash,
            ));
        }
        scalar(&dash.offset, &format!("{at}.offset"), any)?;
    }
    Ok(())
}

fn validate_path(path: &Animated<Path>, at: &str) -> Checked {
    let check_one = |path: &Path| -> Checked {
        let vertices: usize = path
            .subpaths
            .iter()
            .map(|subpath| subpath.vertices.len())
            .sum();
        if path.subpaths.len() > MAX_PATH_SUBPATHS || vertices > MAX_PATH_VERTICES {
            return Err(ShapeIssue::new(at, ShapeProblem::PathTooLarge));
        }
        let finite = |value: Vec2| value.x.is_finite() && value.y.is_finite();
        let all_finite = path.subpaths.iter().all(|subpath| {
            subpath.vertices.iter().all(|vertex| {
                finite(vertex.point) && finite(vertex.in_tangent) && finite(vertex.out_tangent)
            })
        });
        all_finite
            .then_some(())
            .ok_or_else(|| ShapeIssue::new(at, ShapeProblem::OutOfRange))
    };
    match path {
        Animated::Constant { value } => check_one(value),
        Animated::Keyframes { keyframes } => {
            if !valid_keyframes(keyframes) {
                return Err(ShapeIssue::new(at, ShapeProblem::InvalidKeyframes));
            }
            for keyframe in keyframes {
                check_one(&keyframe.value)?;
            }
            let structure = |path: &Path| {
                path.subpaths
                    .iter()
                    .map(|subpath| (subpath.closed, subpath.vertices.len()))
                    .collect::<Vec<_>>()
            };
            let first = structure(&keyframes[0].value);
            if keyframes[1..]
                .iter()
                .any(|keyframe| structure(&keyframe.value) != first)
            {
                return Err(ShapeIssue::new(at, ShapeProblem::PathKeyframesDiffer));
            }
            Ok(())
        }
    }
}

impl Path {
    /// Number of vertices over all subpaths.
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.subpaths
            .iter()
            .map(|subpath| subpath.vertices.len())
            .sum()
    }
}

impl ShapeItem {
    /// Whether the item or anything in it changes over time.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        match self {
            Self::Group { items, transform } => {
                transform.is_animated() || items.iter().any(Self::is_animated)
            }
            Self::Rectangle(geometry) => geometry.is_animated(),
            Self::Ellipse(geometry) => geometry.is_animated(),
            Self::Polystar(geometry) => geometry.is_animated(),
            Self::Path(geometry) => keyed(&geometry.path),
            Self::Fill { paint, opacity, .. } => keyed(paint) || keyed(opacity),
            Self::Stroke(stroke) => {
                keyed(&stroke.paint)
                    || keyed(&stroke.width)
                    || keyed(&stroke.opacity)
                    || stroke
                        .dash
                        .as_ref()
                        .is_some_and(|dash| dash.pattern.iter().any(keyed) || keyed(&dash.offset))
            }
            Self::Trim(trim) => keyed(&trim.start) || keyed(&trim.end) || keyed(&trim.offset),
        }
    }
}

impl ShapeGeometry {
    /// Whether the outline changes over time.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        match self {
            Self::Rectangle(geometry) => geometry.is_animated(),
            Self::Ellipse(geometry) => geometry.is_animated(),
            Self::Polystar(geometry) => geometry.is_animated(),
            Self::Path(geometry) => keyed(&geometry.path),
        }
    }
}

impl RectangleGeometry {
    fn is_animated(&self) -> bool {
        self.center.is_animated()
            || keyed(&self.size)
            || keyed(&self.corner_radius)
            || keyed(&self.rotation_degrees)
    }
}

impl EllipseGeometry {
    fn is_animated(&self) -> bool {
        self.center.is_animated() || keyed(&self.size) || keyed(&self.rotation_degrees)
    }
}

impl PolystarGeometry {
    fn is_animated(&self) -> bool {
        self.center.is_animated()
            || keyed(&self.points)
            || keyed(&self.outer_radius)
            || keyed(&self.inner_radius)
            || keyed(&self.outer_roundness)
            || keyed(&self.inner_roundness)
            || keyed(&self.rotation_degrees)
    }
}

impl ShapeTransform {
    fn is_animated(&self) -> bool {
        keyed(&self.anchor)
            || self.position.is_animated()
            || keyed(&self.scale)
            || keyed(&self.rotation_degrees)
            || keyed(&self.opacity)
    }
}

const fn keyed<T>(animation: &Animated<T>) -> bool {
    matches!(animation, Animated::Keyframes { .. })
}
