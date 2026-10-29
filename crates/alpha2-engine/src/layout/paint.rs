//! A resolved layout's paint list: what its picture is drawn from, in
//! paint order. Backgrounds, borders and shapes are vector fills, batched
//! into as few scenes as the order allows; texts are their textures, placed
//! 1:1 at whole pixels; isolated nodes are groups.

use super::unit_of;
use alpha2_eval::{EvaluatedLayout, EvaluatedNode, NodeContent};
use alpha2_geometry::kurbo::{Affine, BezPath, Point, Vec2};
use alpha2_geometry::{CanvasSize, IRect, VectorOp, VectorScene, build_scene_in, scene_paint};
use alpha2_layout::{NodeRects, ResolvedLayout};
use alpha2_model::{FillRule, Paint, Rgba};
use alpha2_render::RenderSize;
use std::sync::Arc;

/// How an item's picture is placed on the layout's picture, in pixels: the
/// item's point `anchor` lands on `position`, scaled per axis about it, then
/// turned clockwise. An item's own pixels are the picture's, 1:1, before
/// this.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Placement {
    pub(crate) anchor: [f64; 2],
    pub(crate) position: [f64; 2],
    pub(crate) scale: [f64; 2],
    pub(crate) rotation_degrees: f64,
    pub(crate) opacity: f32,
    /// Whether it may turn: a node's visual placement. A group does not
    /// collapse into an item that may turn, whatever the angle, so the graph
    /// keeps its shape while the angle animates.
    pub(crate) rotates: bool,
}

impl Placement {
    pub(crate) const IDENTITY: Self = Self {
        anchor: [0.0; 2],
        position: [0.0; 2],
        scale: [1.0; 2],
        rotation_degrees: 0.0,
        opacity: 1.0,
        rotates: false,
    };

    /// This placement applied after `inner`, which does not rotate: the
    /// same geometry as the two in turn, since both scales are diagonal.
    fn then(self, inner: Self) -> Self {
        let offset = [
            self.scale[0] * (inner.position[0] - self.anchor[0]),
            self.scale[1] * (inner.position[1] - self.anchor[1]),
        ];
        let (sine, cosine) = libm::sincos(self.rotation_degrees.to_radians());
        Self {
            anchor: inner.anchor,
            position: [
                self.position[0] + cosine * offset[0] - sine * offset[1],
                self.position[1] + sine * offset[0] + cosine * offset[1],
            ],
            scale: [
                self.scale[0] * inner.scale[0],
                self.scale[1] * inner.scale[1],
            ],
            rotation_degrees: self.rotation_degrees,
            opacity: self.opacity * inner.opacity,
            rotates: self.rotates,
        }
    }
}

/// One entry of a layout's paint list.
#[derive(Clone, Debug)]
pub(crate) enum PaintItem {
    /// Vector fills drawn together over the picture.
    Vector {
        scene: Arc<VectorScene>,
        place: Placement,
    },
    /// The text with this index in the layout's texts, its texture's pixel
    /// `(0, 0)` at `place`.
    Text { index: usize, place: Placement },
    /// An isolated node: `items` composited, kept inside `clip`, placed by
    /// `place`.
    Group {
        items: Vec<PaintItem>,
        clip: Option<Arc<VectorScene>>,
        place: Placement,
    },
}

/// A text node's texture as the paint list sees it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextBox {
    /// Where its layout box's top-left corner lies in its texture.
    pub(crate) corner: [u32; 2],
    /// The texture's size.
    pub(crate) size: [u32; 2],
}

/// The list while it is built: vector ops not yet made scenes.
enum Building {
    Vector {
        ops: Vec<VectorOp>,
        bounds: IRect,
    },
    Text {
        index: usize,
        origin: [i32; 2],
        bounds: IRect,
    },
    Group {
        items: Vec<Building>,
        clip: Option<Vec<VectorOp>>,
        place: Placement,
    },
}

struct Builder<'a> {
    canvas: CanvasSize,
    unit: f64,
    rects: &'a [NodeRects],
    texts: &'a [Option<TextBox>],
    /// The next node's index in preorder, and the next text node's.
    node: usize,
    text: usize,
    time: alpha2_time::MediaTime,
}

/// The paint list of `layout`, resolved as `resolved`, on a picture of
/// `size`. `texts` gives each text node's texture, in preorder; `None` for
/// empty text, which draws nothing.
pub(crate) fn paint_list(
    layout: &EvaluatedLayout,
    resolved: &ResolvedLayout,
    size: RenderSize,
    texts: &[Option<TextBox>],
) -> Vec<PaintItem> {
    let mut builder = Builder {
        canvas: CanvasSize::new(size.width, size.height),
        unit: f64::from(unit_of(size)),
        rects: &resolved.nodes,
        texts,
        node: 0,
        text: 0,
        time: layout.time,
    };
    let mut items = Vec::new();
    builder.node(&layout.root, &mut items);
    items
        .into_iter()
        .filter_map(|item| finish(item, builder.canvas))
        .collect()
}

impl Builder<'_> {
    fn node(&mut self, node: &EvaluatedNode, out: &mut Vec<Building>) {
        let Some(&rects) = self.rects.get(self.node) else {
            return;
        };
        self.node += 1;
        let border = rects.border;
        let radius = f64::from(node.paint.border_radius) * self.unit;
        let placed = node.visual.map(|visual| {
            let center = [
                f64::from(border[0] + border[2]) * 0.5,
                f64::from(border[1] + border[3]) * 0.5,
            ];
            Placement {
                anchor: center,
                position: [
                    center[0] + f64::from(visual.translate.x) * self.unit,
                    center[1] + f64::from(visual.translate.y) * self.unit,
                ],
                scale: [f64::from(visual.scale.x), f64::from(visual.scale.y)],
                rotation_degrees: f64::from(visual.rotation_degrees),
                opacity: visual.opacity,
                // Any visual may turn: presence decides, so the graph keeps its shape.
                rotates: true,
            }
        });
        let mut own = Vec::new();
        let target = if placed.is_some() {
            &mut own
        } else {
            &mut *out
        };
        if let Some(paint) = &node.paint.background {
            let path = rounded(border, radius);
            self.fill(target, path, FillRule::Nonzero, paint, border);
        }
        // The padding box, rounded by what of the radius the border leaves.
        let inner_radius = (radius - f64::from(rects.padding[0] - border[0])).max(0.0);
        if let Some((width, paint)) = &node.paint.border
            && *width > 0.0
        {
            let mut path = rounded(border, radius);
            path.extend(rounded(rects.padding, inner_radius));
            self.fill(target, path, FillRule::Evenodd, paint, border);
        }
        let clip = node.clip.then(|| {
            let path = rounded(rects.padding, inner_radius);
            let inner = rects.padding;
            let white = Paint::Solid {
                color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            };
            fill_op(self.canvas, path, FillRule::Nonzero, &white, inner)
                .into_iter()
                .collect::<Vec<_>>()
        });
        let mut content = Vec::new();
        let content_target = if clip.is_some() {
            &mut content
        } else {
            &mut *target
        };
        self.content(node, rects, content_target);
        if let Some(clip) = clip {
            target.push(Building::Group {
                items: content,
                clip: Some(clip),
                place: Placement::IDENTITY,
            });
        }
        if let Some(place) = placed {
            out.push(Building::Group {
                items: own,
                clip: None,
                place,
            });
        }
    }

    fn content(&mut self, node: &EvaluatedNode, rects: NodeRects, out: &mut Vec<Building>) {
        match &node.content {
            NodeContent::Empty => {}
            NodeContent::Children(children) => {
                for child in children {
                    self.node(child, out);
                }
            }
            NodeContent::Text(_) => {
                let index = self.text;
                self.text += 1;
                if let Some(Some(text)) = self.texts.get(index) {
                    #[allow(clippy::cast_possible_truncation)]
                    let corner = [rects.content[0] as i32, rects.content[1] as i32];
                    let origin = [
                        corner[0].saturating_sub_unsigned(text.corner[0]),
                        corner[1].saturating_sub_unsigned(text.corner[1]),
                    ];
                    let bounds = IRect::new(
                        origin[0],
                        origin[1],
                        origin[0].saturating_add_unsigned(text.size[0]),
                        origin[1].saturating_add_unsigned(text.size[1]),
                    );
                    out.push(Building::Text {
                        index,
                        origin,
                        bounds,
                    });
                }
            }
            NodeContent::Shape(plan) => {
                let [left, top, right, bottom] = rects.content;
                let (width, height) = (right - left, bottom - top);
                if width < 1.0 || height < 1.0 {
                    return;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let space = CanvasSize::new(width as u32, height as u32);
                let items = plan.evaluate(self.time, f64::from(width) / f64::from(height));
                let ops = build_scene_in(
                    &items,
                    space,
                    Vec2::new(f64::from(left), f64::from(top)),
                    self.canvas,
                );
                let bounds = ops
                    .iter()
                    .filter_map(|op| match op {
                        VectorOp::Fill { bounds, .. } => Some(*bounds),
                        _ => None,
                    })
                    .fold(IRect::default(), IRect::union);
                add_vector(out, ops, bounds);
            }
        }
    }

    /// Adds a fill of `path` with `paint`, its points normalized to `space`.
    fn fill(
        &self,
        out: &mut Vec<Building>,
        path: BezPath,
        rule: FillRule,
        paint: &Paint,
        space: [f32; 4],
    ) {
        if let Some(op) = fill_op(self.canvas, path, rule, paint, space) {
            let VectorOp::Fill { bounds, .. } = op else {
                return;
            };
            add_vector(out, vec![op], bounds);
        }
    }
}

/// A fill op of `path` on `canvas`, its paint normalized to the box
/// `space`; `None` when it covers nothing.
fn fill_op(
    canvas: CanvasSize,
    path: BezPath,
    rule: FillRule,
    paint: &Paint,
    space: [f32; 4],
) -> Option<VectorOp> {
    let canvas_rect = IRect::new(
        0,
        0,
        i32::try_from(canvas.width).unwrap_or(i32::MAX),
        i32::try_from(canvas.height).unwrap_or(i32::MAX),
    );
    let bounds = IRect::covering(&path, canvas_rect);
    let (width, height) = (space[2] - space[0], space[3] - space[1]);
    if bounds.is_empty() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let inverse = Affine::translate((-f64::from(space[0]), -f64::from(space[1])));
    Some(VectorOp::Fill {
        path: Arc::new(path),
        rule,
        paint: scene_paint(paint, inverse, [width, height]),
        opacity: 1.0,
        bounds,
    })
}

/// The rounded rectangle `[left, top, right, bottom]`, its radius capped at
/// half its shorter side.
fn rounded(rect: [f32; 4], radius: f64) -> BezPath {
    let [left, top, right, bottom] = rect.map(f64::from);
    alpha2_geometry::shapes::rectangle(
        Point::new(f64::midpoint(left, right), f64::midpoint(top, bottom)),
        Vec2::new(right - left, bottom - top),
        radius,
        0.0,
    )
}

/// Adds vector ops covering `bounds`: to the last vector batch when nothing
/// drawn after it meets them, else as a new batch. Compositing is
/// associative and what is skipped is transparent where the ops draw, so the
/// picture is the same up to intermediate rounding.
fn add_vector(out: &mut Vec<Building>, ops: Vec<VectorOp>, bounds: IRect) {
    if ops.is_empty() {
        return;
    }
    for item in out.iter_mut().rev() {
        match item {
            Building::Vector {
                ops: batch,
                bounds: covered,
            } => {
                batch.extend(ops);
                *covered = covered.union(bounds);
                return;
            }
            Building::Text { bounds: text, .. } if text.intersect(bounds).is_empty() => {}
            Building::Text { .. } | Building::Group { .. } => break,
        }
    }
    out.push(Building::Vector { ops, bounds });
}

/// The built item as drawn: vector batches made scenes, and a group with no
/// clip and one item that does not rotate collapsed into that item.
fn finish(item: Building, canvas: CanvasSize) -> Option<PaintItem> {
    let scene = |ops| Arc::new(VectorScene::new(ops, canvas.width, canvas.height));
    Some(match item {
        Building::Vector { ops, .. } => PaintItem::Vector {
            scene: scene(ops),
            place: Placement::IDENTITY,
        },
        Building::Text { index, origin, .. } => PaintItem::Text {
            index,
            place: Placement {
                position: origin.map(f64::from),
                ..Placement::IDENTITY
            },
        },
        Building::Group { items, clip, place } => {
            let mut items = items
                .into_iter()
                .filter_map(|item| finish(item, canvas))
                .collect::<Vec<_>>();
            if items.is_empty() {
                return None;
            }
            if clip.is_none() && items.len() == 1 {
                match items.pop() {
                    Some(PaintItem::Vector {
                        scene,
                        place: inner,
                    }) if !inner.rotates => {
                        return Some(PaintItem::Vector {
                            scene,
                            place: place.then(inner),
                        });
                    }
                    Some(PaintItem::Text {
                        index,
                        place: inner,
                    }) if !inner.rotates => {
                        return Some(PaintItem::Text {
                            index,
                            place: place.then(inner),
                        });
                    }
                    Some(other) => items.push(other),
                    None => {}
                }
            }
            PaintItem::Group {
                items,
                clip: clip.map(scene),
                place,
            }
        }
    })
}
