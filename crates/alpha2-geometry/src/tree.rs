//! A content tree evaluated at one time, and the scene it draws.
//!
//! The rules are After Effects' and Lottie's. A group is read front to back.
//! Geometry accumulates in its group; a fill or stroke paints the geometry
//! listed before it there, nested groups' included, and a trim cuts that
//! geometry and replaces it. Paints listed earlier are drawn on top, so a
//! group's drawing is emitted from its last item back to its first.
//!
//! Positions are normalized and lengths fractions of the shorter side, as
//! the model has them; [`build_scene`] takes them to device pixels. A
//! group's transform acts in pixel space, and the paths, strokes, and paint
//! coordinates under it go through it: a stroke is expanded in the space of
//! the group its stroke item is in, then transformed, so a non-uniform scale
//! makes its pen an ellipse.

use crate::scene::{IRect, PaintKind, ScenePaint, SceneStop, VectorOp, VectorScene};
use crate::trim::{Measured, TrimSpan};
use crate::{CanvasSize, StrokeStyle, affine, project_far, shapes, stroke_outline, trim};
use alpha2_model::{FillRule, LineCap, LineJoin, Paint, PolystarKind, TrimMode};
use kurbo::{Affine, BezPath, Point, Vec2};
use std::sync::Arc;

/// A path vertex evaluated at one time, normalized.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct EvaluatedVertex {
    /// Where it lies.
    pub point: Point,
    /// The control point of the segment ending here, relative to `point`.
    pub in_tangent: Vec2,
    /// The control point of the segment starting here, relative to `point`.
    pub out_tangent: Vec2,
}

/// A subpath evaluated at one time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvaluatedSubpath {
    /// Whether a segment joins the last vertex back to the first.
    pub closed: bool,
    /// The vertices.
    pub vertices: Vec<EvaluatedVertex>,
}

/// A path evaluated at one time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvaluatedPath {
    /// The subpaths.
    pub subpaths: Vec<EvaluatedSubpath>,
}

impl From<&alpha2_model::Path> for EvaluatedPath {
    fn from(path: &alpha2_model::Path) -> Self {
        let point = |value: alpha2_model::Vec2| Point::new(f64::from(value.x), f64::from(value.y));
        let vector = |value: alpha2_model::Vec2| Vec2::new(f64::from(value.x), f64::from(value.y));
        Self {
            subpaths: path
                .subpaths
                .iter()
                .map(|subpath| EvaluatedSubpath {
                    closed: subpath.closed,
                    vertices: subpath
                        .vertices
                        .iter()
                        .map(|vertex| EvaluatedVertex {
                            point: point(vertex.point),
                            in_tangent: vector(vertex.in_tangent),
                            out_tangent: vector(vertex.out_tangent),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// A geometry item evaluated at one time.
#[derive(Clone, Debug, PartialEq)]
pub enum EvaluatedGeometry {
    /// A rectangle.
    Rectangle {
        /// Centre.
        center: Point,
        /// Full size.
        size: Vec2,
        /// Corner radius, a fraction of the shorter side.
        corner_radius: f64,
        /// Clockwise rotation in degrees.
        rotation_degrees: f64,
    },
    /// An ellipse.
    Ellipse {
        /// Centre.
        center: Point,
        /// Full size.
        size: Vec2,
        /// Clockwise rotation in degrees.
        rotation_degrees: f64,
    },
    /// A star or polygon.
    Polystar {
        /// Star or polygon.
        kind: PolystarKind,
        /// Centre.
        center: Point,
        /// Number of points; a fraction is dropped.
        points: f64,
        /// Points' radius, a fraction of the shorter side.
        outer_radius: f64,
        /// A star's inner radius, a fraction of the shorter side.
        inner_radius: f64,
        /// Roundness of the points.
        outer_roundness: f64,
        /// Roundness of a star's inner corners.
        inner_roundness: f64,
        /// Clockwise rotation in degrees.
        rotation_degrees: f64,
    },
    /// A Bezier path.
    Path(Arc<EvaluatedPath>),
}

impl EvaluatedGeometry {
    /// The geometry in the pixels of a picture of `size`.
    #[must_use]
    pub fn to_path(&self, size: CanvasSize) -> BezPath {
        let scale = size.scale();
        let pixel = |point: Point| Point::new(point.x * scale.x, point.y * scale.y);
        let extent = |vector: Vec2| Vec2::new(vector.x * scale.x, vector.y * scale.y);
        let unit = size.unit();
        match self {
            Self::Rectangle {
                center,
                size: extent_value,
                corner_radius,
                rotation_degrees,
            } => shapes::rectangle(
                pixel(*center),
                extent(*extent_value),
                corner_radius * unit,
                *rotation_degrees,
            ),
            Self::Ellipse {
                center,
                size: extent_value,
                rotation_degrees,
            } => shapes::ellipse(pixel(*center), extent(*extent_value), *rotation_degrees),
            Self::Polystar {
                kind,
                center,
                points,
                outer_radius,
                inner_radius,
                outer_roundness,
                inner_roundness,
                rotation_degrees,
            } => shapes::polystar(&shapes::Polystar {
                star: *kind == PolystarKind::Star,
                center: pixel(*center),
                points: *points,
                outer_radius: outer_radius * unit,
                inner_radius: inner_radius * unit,
                outer_roundness: *outer_roundness,
                inner_roundness: *inner_roundness,
                degrees: *rotation_degrees,
            }),
            Self::Path(path) => {
                let mut bez = BezPath::new();
                let mut vertices = Vec::new();
                for subpath in &path.subpaths {
                    vertices.clear();
                    vertices.extend(subpath.vertices.iter().map(|vertex| shapes::Vertex {
                        point: pixel(vertex.point),
                        in_tangent: extent(vertex.in_tangent),
                        out_tangent: extent(vertex.out_tangent),
                    }));
                    shapes::append_subpath(&mut bez, &vertices, subpath.closed);
                }
                bez
            }
        }
    }
}

/// A group's transform evaluated at one time, normalized.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EvaluatedTransform {
    /// The point of the items placed at `position`.
    pub anchor: Point,
    /// Where `anchor` is placed.
    pub position: Point,
    /// Per-axis scale.
    pub scale: Vec2,
    /// Clockwise rotation in degrees.
    pub rotation_degrees: f64,
    /// Group opacity, in `0..=1`.
    pub opacity: f64,
}

impl Default for EvaluatedTransform {
    fn default() -> Self {
        Self {
            anchor: Point::ZERO,
            position: Point::ZERO,
            scale: Vec2::new(1.0, 1.0),
            rotation_degrees: 0.0,
            opacity: 1.0,
        }
    }
}

impl EvaluatedTransform {
    /// The transform in the pixels of a picture of `size`.
    #[must_use]
    pub fn affine(&self, size: CanvasSize) -> Affine {
        affine::placement(
            size.point(self.position.x, self.position.y),
            size.point(self.anchor.x, self.anchor.y),
            self.scale,
            self.rotation_degrees,
        )
    }
}

/// One content-tree item evaluated at one time.
#[derive(Clone, Debug, PartialEq)]
pub enum EvaluatedItem {
    /// Items drawn through one transform and opacity.
    Group {
        /// The items, front first. Shared, so a subtree that does not change
        /// over time is evaluated once.
        items: Arc<[EvaluatedItem]>,
        /// The group's transform.
        transform: EvaluatedTransform,
    },
    /// Geometry.
    Geometry(EvaluatedGeometry),
    /// A fill of the geometry before it.
    Fill {
        /// What covers it.
        paint: Paint,
        /// Opacity in `0..=1`.
        opacity: f64,
        /// Which regions are inside.
        rule: FillRule,
    },
    /// A stroke of the geometry before it.
    Stroke {
        /// What covers it.
        paint: Paint,
        /// Full width, a fraction of the shorter side.
        width: f64,
        /// Opacity in `0..=1`.
        opacity: f64,
        /// How segments meet.
        join: LineJoin,
        /// Miter limit.
        miter_limit: f64,
        /// How ends look.
        cap: LineCap,
        /// Dash and gap lengths, fractions of the shorter side; solid when
        /// empty.
        dash: Vec<f64>,
        /// Dash offset, a fraction of the shorter side.
        dash_offset: f64,
    },
    /// A trim of the geometry before it.
    Trim {
        /// Start fraction.
        start: f64,
        /// End fraction.
        end: f64,
        /// Offset in turns.
        offset: f64,
        /// Together or individually.
        mode: TrimMode,
    },
}

/// Geometry accumulated in a group: a path in the space of the item that
/// made it, and the transform from there to the group's space.
#[derive(Clone)]
struct Entry {
    path: BezPath,
    relative: Affine,
}

/// What [`item_paths`] reports of one item.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemPath {
    /// The item's place in the tree, like `items[0].items[2]`.
    pub at: String,
    /// What it covers in device pixels: geometry's outline, a fill's area,
    /// a stroke's outline; empty for a group. Trims are not recorded.
    pub path: BezPath,
    /// How `path` is filled.
    pub rule: FillRule,
    /// Whether it is paint (a fill or a stroke) rather than geometry.
    pub paint: bool,
    /// The device pixels it reaches; a group's is its items'.
    pub bounds: kurbo::Rect,
}

struct Builder {
    size: CanvasSize,
    canvas: IRect,
    /// Whether items are named by their place in the tree: only queries
    /// read the names, so a scene drawn for a frame formats none.
    named: bool,
    /// Every item's outline or area, when queries need them.
    records: Option<Vec<ItemPath>>,
}

/// A group's drawing, in item order: the scene emits it back to front.
type Chunks = Vec<Vec<(VectorOp, String)>>;

/// The scene `items` draw on a picture of `size`.
#[must_use]
pub fn build_scene(items: &[EvaluatedItem], size: CanvasSize) -> VectorScene {
    let mut builder = Builder::new(size, false, false);
    let (_, chunks) = builder.group(items, Affine::IDENTITY, "");
    let ops = back_to_front(chunks)
        .into_iter()
        .map(|(op, _)| op)
        .collect();
    VectorScene::new(ops, size.width, size.height)
}

/// The scene `items` draw, with the item each of its operations was drawn
/// for (empty for a layer's push and pop).
#[must_use]
pub fn build_scene_with_items(
    items: &[EvaluatedItem],
    size: CanvasSize,
) -> (VectorScene, Vec<String>) {
    let mut builder = Builder::new(size, true, false);
    let (_, chunks) = builder.group(items, Affine::IDENTITY, "items");
    let (ops, owners): (Vec<_>, Vec<_>) = back_to_front(chunks).into_iter().unzip();
    (VectorScene::new(ops, size.width, size.height), owners)
}

/// Every item's outline or painted area, in document order, for the
/// editor's queries.
#[must_use]
pub fn item_paths(items: &[EvaluatedItem], size: CanvasSize) -> Vec<ItemPath> {
    let mut builder = Builder::new(size, true, true);
    builder.group(items, Affine::IDENTITY, "items");
    builder.records.unwrap_or_default()
}

fn back_to_front(chunks: Chunks) -> Vec<(VectorOp, String)> {
    chunks.into_iter().rev().flatten().collect()
}

impl Builder {
    fn new(size: CanvasSize, named: bool, records: bool) -> Self {
        let canvas = IRect::new(
            0,
            0,
            i32::try_from(size.width).unwrap_or(i32::MAX),
            i32::try_from(size.height).unwrap_or(i32::MAX),
        );
        Self {
            size,
            canvas,
            named,
            records: records.then(Vec::new),
        }
    }

    fn record(&mut self, at: &str, path: BezPath, rule: FillRule, paint: bool) {
        if let Some(records) = &mut self.records {
            let bounds = if path.elements().is_empty() {
                kurbo::Rect::ZERO
            } else {
                kurbo::Shape::bounding_box(&path)
            };
            records.push(ItemPath {
                at: at.to_owned(),
                path,
                rule,
                paint,
                bounds,
            });
        }
    }

    /// Reads a group's items: the geometry it leaves for the enclosing group
    /// (in this group's space) and its drawing.
    #[allow(clippy::too_many_lines)]
    fn group(
        &mut self,
        items: &[EvaluatedItem],
        to_device: Affine,
        at: &str,
    ) -> (Vec<Entry>, Chunks) {
        let mut entries: Vec<Entry> = Vec::new();
        let mut chunks: Chunks = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let at = if self.named {
                format!("{at}[{index}]")
            } else {
                String::new()
            };
            match item {
                EvaluatedItem::Group { items, transform } => {
                    let local = transform.affine(self.size);
                    let record_index = self.records.as_ref().map(Vec::len);
                    let inner_at = if self.named {
                        format!("{at}.items")
                    } else {
                        String::new()
                    };
                    let (inner, drawing) = self.group(items, to_device * local, &inner_at);
                    if let (Some(records), Some(first)) = (&mut self.records, record_index) {
                        let bounds = records[first..]
                            .iter()
                            .map(|record| record.bounds)
                            .filter(|bounds| {
                                bounds.area() > 0.0 || bounds.width() > 0.0 || bounds.height() > 0.0
                            })
                            .reduce(|a, b| a.union(b))
                            .unwrap_or(kurbo::Rect::ZERO);
                        records.insert(
                            first,
                            ItemPath {
                                at: at.clone(),
                                path: BezPath::new(),
                                rule: FillRule::Nonzero,
                                paint: false,
                                bounds,
                            },
                        );
                    }
                    entries.extend(inner.into_iter().map(|entry| Entry {
                        path: entry.path,
                        relative: local * entry.relative,
                    }));
                    let ops = back_to_front(drawing);
                    #[allow(clippy::cast_possible_truncation)]
                    let opacity = transform.opacity.clamp(0.0, 1.0) as f32;
                    if ops.is_empty() || opacity <= 0.0 {
                        continue;
                    }
                    if opacity < 1.0 {
                        let bounds = ops
                            .iter()
                            .filter_map(|(op, _)| match op {
                                VectorOp::Fill { bounds, .. } => Some(*bounds),
                                _ => None,
                            })
                            .fold(IRect::default(), IRect::union);
                        let mut layer = Vec::with_capacity(ops.len() + 2);
                        layer.push((VectorOp::PushLayer { opacity, bounds }, String::new()));
                        layer.extend(ops);
                        layer.push((VectorOp::PopLayer, String::new()));
                        chunks.push(layer);
                    } else {
                        chunks.push(ops);
                    }
                }
                EvaluatedItem::Geometry(geometry) => {
                    let path = geometry.to_path(self.size);
                    if self.records.is_some() {
                        let device = to_device * path.clone();
                        self.record(&at, device, FillRule::Nonzero, false);
                    }
                    entries.push(Entry {
                        path,
                        relative: Affine::IDENTITY,
                    });
                }
                EvaluatedItem::Fill {
                    paint,
                    opacity,
                    rule,
                } => {
                    let mut device = BezPath::new();
                    for entry in &entries {
                        device.extend(
                            (to_device * entry.relative * entry.path.clone())
                                .elements()
                                .iter()
                                .copied(),
                        );
                    }
                    let device = project_far(&device, self.size.width, self.size.height);
                    self.record(&at, device.clone(), *rule, true);
                    if let Some(op) = self.fill_op(device, *rule, paint, *opacity, to_device) {
                        chunks.push(vec![(op, at)]);
                    }
                }
                EvaluatedItem::Stroke {
                    paint,
                    width,
                    opacity,
                    join,
                    miter_limit,
                    cap,
                    dash,
                    dash_offset,
                } => {
                    let unit = self.size.unit();
                    let style = StrokeStyle {
                        width: width * unit,
                        join: *join,
                        miter_limit: *miter_limit,
                        cap: *cap,
                        dash: dash.iter().map(|length| length.max(0.0) * unit).collect(),
                        dash_offset: dash_offset * unit,
                    };
                    let mut local = BezPath::new();
                    for entry in &entries {
                        local.extend(
                            (entry.relative * entry.path.clone())
                                .elements()
                                .iter()
                                .copied(),
                        );
                    }
                    let outline = stroke_outline(&local, &style, to_device);
                    let outline = project_far(&outline, self.size.width, self.size.height);
                    self.record(&at, outline.clone(), FillRule::Nonzero, true);
                    if let Some(op) =
                        self.fill_op(outline, FillRule::Nonzero, paint, *opacity, to_device)
                    {
                        chunks.push(vec![(op, at)]);
                    }
                }
                EvaluatedItem::Trim {
                    start,
                    end,
                    offset,
                    mode,
                } => {
                    entries = trim_entries(&entries, to_device, *start, *end, *offset, *mode);
                }
            }
        }
        (entries, chunks)
    }

    fn fill_op(
        &self,
        path: BezPath,
        rule: FillRule,
        paint: &Paint,
        opacity: f64,
        to_device: Affine,
    ) -> Option<VectorOp> {
        #[allow(clippy::cast_possible_truncation)]
        let opacity = opacity.clamp(0.0, 1.0) as f32;
        if opacity <= 0.0 || path.elements().is_empty() || !affine::is_invertible(to_device) {
            return None;
        }
        let bounds = IRect::covering(&path, self.canvas);
        if bounds.is_empty() {
            return None;
        }
        Some(VectorOp::Fill {
            path: Arc::new(path),
            rule,
            paint: scene_paint(paint, to_device.inverse()),
            opacity,
            bounds,
        })
    }
}

/// Trims `entries`, measured in device pixels, keeping each entry's
/// transform for what remains of it.
fn trim_entries(
    entries: &[Entry],
    to_device: Affine,
    start: f64,
    end: f64,
    offset: f64,
    mode: TrimMode,
) -> Vec<Entry> {
    let span = TrimSpan { start, end, offset };
    // Every subpath, with the entry it came from.
    let mut owners = Vec::new();
    let mut parts = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        for subpath in trim::subpaths(&entry.path) {
            owners.push(index);
            parts.push(Measured {
                subpath,
                measure: to_device * entry.relative,
            });
        }
    }
    let kept = match mode {
        TrimMode::Together => trim::trim_together(&parts, span),
        TrimMode::Individually => trim::trim_individually(&parts, span),
    };
    let mut result: Vec<Entry> = Vec::new();
    for (part, subpath) in kept {
        let relative = entries[owners[part]].relative;
        match result.last_mut() {
            Some(last) if last.relative == relative => trim::append(&mut last.path, &subpath),
            _ => {
                let mut path = BezPath::new();
                trim::append(&mut path, &subpath);
                result.push(Entry { path, relative });
            }
        }
    }
    result
}

/// `paint` as the scene paints it, through `inverse` from device pixels.
#[must_use]
pub fn scene_paint(paint: &Paint, inverse: Affine) -> ScenePaint {
    let color = |value: alpha2_model::Rgba| [value.red, value.green, value.blue, value.alpha];
    let stops = |list: &[alpha2_model::GradientStop]| {
        list.iter()
            .map(|stop| SceneStop {
                offset: stop.offset,
                color: color(stop.color),
            })
            .collect::<Vec<_>>()
    };
    let spread = |value: alpha2_model::Spread| match value {
        alpha2_model::Spread::Pad => 0,
        alpha2_model::Spread::Repeat => 1,
        alpha2_model::Spread::Reflect => 2,
    };
    match paint {
        Paint::Solid { color: value } => ScenePaint {
            kind: PaintKind::Solid,
            a: [0.0; 2],
            b: [0.0; 2],
            spread: 0,
            stops: vec![SceneStop {
                offset: 0.0,
                color: color(*value),
            }],
            inverse,
        },
        Paint::LinearGradient {
            start,
            end,
            stops: list,
            spread: mode,
        } => ScenePaint {
            kind: PaintKind::Linear,
            a: [start.x, start.y],
            b: [end.x, end.y],
            spread: spread(*mode),
            stops: stops(list),
            inverse,
        },
        Paint::RadialGradient {
            center,
            radius,
            stops: list,
            spread: mode,
        } => ScenePaint {
            kind: PaintKind::Radial,
            a: [center.x, center.y],
            b: [*radius, 0.0],
            spread: spread(*mode),
            stops: stops(list),
            inverse,
        },
        Paint::ConicGradient {
            center,
            angle_degrees,
            stops: list,
        } => ScenePaint {
            kind: PaintKind::Conic,
            a: [center.x, center.y],
            b: [angle_degrees.to_radians(), 0.0],
            spread: 0,
            stops: stops(list),
            inverse,
        },
    }
}
