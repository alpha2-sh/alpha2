//! Estimates resvg's drawing cost at a given size before it draws.
//!
//! resvg has no work limit: layers can be up to five canvases a side, filter regions are
//! unbounded, and filter cost grows with radius or kernel size. A few hundred bytes can ask for
//! hours of work or more memory than exists. This walks the tree as resvg does and refuses
//! weighted work past [`MAX_DRAWING`] canvases ([`weights`]), held pixels past
//! [`MAX_HELD_PIXELS`], and node draws past [`MAX_DRAWN`]. Areas are clipped bounds, never less
//! than resvg touches.

use resvg::tiny_skia::{PathSegment, Rect, Transform};
use resvg::usvg::{self, Group, Node, Paint, filter};

use crate::api::ExtensionError;
use crate::budget::{FLOOR_CANVAS, MAX_DRAWING};

/// Most intermediate pixels (layers, masks, tiles, filter results) held at once: 1 GiB of RGBA8.
pub(super) const MAX_HELD_PIXELS: f64 = 268_435_456.0;

/// Most node draws per render, counting repeats through patterns, masks, clips, and filters.
/// Even an empty node costs resvg about a microsecond.
pub(super) const MAX_DRAWN: u64 = 4_000_000;

/// How far, in device pixels, a path, stroke, or image may reach past its target. tiny-skia 0.12
/// clips paths in `f32` and walks them in fixed point; past about a million pixels it panics,
/// which aborts the process. Within twice this it never failed over millions of random paths.
/// Only segments that both reach this far and cross into the target count ([`Walk::segments`]).
pub(super) const MAX_REACH: f64 = 262_144.0;

/// How far an axis-aligned fill edge may reach; tiny-skia clips these exactly up to `i32`
/// rounding.
pub(super) const MAX_AXIS: f64 = 33_554_432.0;

/// Clearance beyond which tiny-skia skips a segment or draws it exactly along the edge (it rounds
/// bounds out by a pixel and a half).
const BESIDE: f64 = 16.0;

/// How far a layer's bounds or filter region may reach; resvg rounds them to `i32` rectangles
/// before clamping and panics past this.
pub(super) const MAX_EXTENT: f64 = 1_073_741_824.0;

/// Per-pixel cost relative to one blended fill, measured natively at 1080p (a canvas of blended
/// fill is 4 to 8 ms) and rounded up. Times below are per 1080p canvas.
mod weights {
    /// A fill or stroke (4.3 ms blended, 8 ms gradient). Opaque fills are not discounted.
    pub(super) const DRAW: f64 = 1.0;
    /// Allocated, drawn into, composited (11 ms).
    pub(super) const LAYER: f64 = 2.0;
    /// Own canvas, cleared, drawn, made a mask, applied (20 ms).
    pub(super) const CLIP: f64 = 3.0;
    /// Own canvas, drawn, made a mask, applied (28 ms).
    pub(super) const MASK: f64 = 4.0;
    /// A pattern tile or nested document canvas, per place drawn.
    pub(super) const CANVAS: f64 = 2.0;
    /// Every filter primitive's result plus sRGB/linear conversion of its input (up to 22 ms).
    pub(super) const PRIMITIVE: f64 = 3.0;
    /// feComponentTransfer (44 ms), feComposite (34 ms), feBlend (30 ms),
    /// feDisplacementMap (29 ms), feDiffuseLighting (25 ms).
    pub(super) const PIXELWISE: f64 = 3.0;
    /// feSpecularLighting (85 ms), and feDiffuseLighting with any light.
    pub(super) const LIGHTING: f64 = 9.0;
    /// feGaussianBlur as three box blurs, deviation of 2 pixels or more (81 to 84 ms).
    pub(super) const BOX_BLUR: f64 = 8.0;
    /// feGaussianBlur as a recursive filter, below 2 pixels (681 ms).
    pub(super) const IIR_BLUR: f64 = 82.0;
    /// feMerge, each input drawn (15 ms).
    pub(super) const MERGE_INPUT: f64 = 2.0;
    /// feTurbulence, its setup (67 ms) and each octave (70 ms).
    pub(super) const TURBULENCE: f64 = 6.0;
    pub(super) const OCTAVE: f64 = 9.0;
    /// feMorphology, each pixel of its window at each pixel (1.74 ms).
    pub(super) const MORPHOLOGY_CELL: f64 = 0.22;
    /// feConvolveMatrix, each kernel cell at each pixel (4.5 ms).
    pub(super) const CONVOLVE_CELL: f64 = 0.65;
}

/// Refuses drawing `tree` at `width` x `height` if it exceeds a budget.
pub(super) fn check(
    tree: &usvg::Tree,
    ts: Transform,
    width: u32,
    height: u32,
) -> Result<(), ExtensionError> {
    let (w, h) = (f64::from(width), f64::from(height));
    let mut walk = Walk {
        canvas: (w * h).max(FLOOR_CANVAS),
        // resvg keeps a layer within five canvases a side, two outside each edge.
        bounds: Bounds::new(-2.0 * w, -2.0 * h, 3.0 * w, 3.0 * h),
        work: 0.0,
        held: w * h,
        most_held: w * h,
        drawn: 0,
        heaviest: (0.0, String::new()),
        largest: (0.0, String::new()),
        far: None,
        over: false,
    };
    walk.nodes(tree.root(), ts, Bounds::new(0.0, 0.0, w, h));
    if let Some((far, limit)) = walk.far {
        return Err(ExtensionError::Budget(format!(
            "drawing the SVG at {width}x{height} places {far} more than {limit} pixels past what it is drawn on"
        )));
    }
    if walk.drawn > MAX_DRAWN {
        return Err(ExtensionError::Budget(format!(
            "the SVG draws more than {MAX_DRAWN} things"
        )));
    }
    if walk.most_held > MAX_HELD_PIXELS {
        return Err(ExtensionError::Budget(format!(
            "drawing the SVG at {width}x{height} holds more than {} pixels of layers and filter results at once (the most at {})",
            MAX_HELD_PIXELS, walk.largest.1
        )));
    }
    if walk.work > MAX_DRAWING * walk.canvas {
        return Err(ExtensionError::Budget(format!(
            "drawing the SVG at {width}x{height} costs more than {MAX_DRAWING} times its canvas, counted as at least 1920x1080 (the most at {})",
            walk.heaviest.1
        )));
    }
    Ok(())
}

/// A rectangle in device pixels.
#[derive(Clone, Copy)]
struct Bounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl Bounds {
    fn new(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    fn of(rect: Rect) -> Self {
        Self::new(
            f64::from(rect.left()),
            f64::from(rect.top()),
            f64::from(rect.right()),
            f64::from(rect.bottom()),
        )
    }

    /// `rect` in device pixels through `ts`.
    fn through(rect: Rect, ts: Transform) -> Option<Self> {
        rect.transform(ts).map(Self::of)
    }

    fn within(self, other: Self) -> Self {
        Self::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        )
    }

    /// Rounded outward to whole pixels, then grown by `by` on each side.
    fn whole(self, by: f64) -> Self {
        Self::new(
            self.left.floor() - by,
            self.top.floor() - by,
            self.right.ceil() + by,
            self.bottom.ceil() + by,
        )
    }

    fn width(self) -> f64 {
        (self.right - self.left).max(0.0)
    }

    fn height(self) -> f64 {
        (self.bottom - self.top).max(0.0)
    }

    fn area(self) -> f64 {
        self.width() * self.height()
    }
}

struct Walk {
    canvas: f64,
    /// Where resvg clamps a layer.
    bounds: Bounds,
    /// Weighted pixels touched.
    work: f64,
    held: f64,
    most_held: f64,
    drawn: u64,
    /// The costliest and largest single items, for the error message.
    heaviest: (f64, String),
    largest: (f64, String),
    /// The first item past [`MAX_REACH`] or [`MAX_EXTENT`], and that limit.
    far: Option<(String, f64)>,
    /// Set once any budget is spent; the walk stops counting.
    over: bool,
}

impl Walk {
    fn spend(&mut self, pixels: f64, weight: f64, what: impl FnOnce() -> String) {
        let cost = pixels * weight;
        self.work += cost;
        if cost > self.heaviest.0 {
            self.heaviest = (cost, what());
        }
        self.check();
    }

    fn hold(&mut self, pixels: f64, what: impl FnOnce() -> String) {
        if pixels > self.largest.0 {
            self.largest = (pixels, what());
        }
        self.held += pixels;
        self.most_held = self.most_held.max(self.held);
        self.check();
    }

    fn release(&mut self, pixels: f64) {
        self.held -= pixels;
    }

    /// Flags `bounds` if it reaches more than `limit` device pixels past `target`.
    fn reach(
        &mut self,
        bounds: Option<Bounds>,
        target: Bounds,
        limit: f64,
        what: impl FnOnce() -> String,
    ) {
        let Some(bounds) = bounds else {
            return;
        };
        let within = bounds.left >= target.left - limit
            && bounds.top >= target.top - limit
            && bounds.right <= target.right + limit
            && bounds.bottom <= target.bottom + limit;
        if !within && self.far.is_none() {
            self.far = Some((what(), limit));
            self.over = true;
        }
    }

    /// Flags an offset that would overflow resvg's `i32` rectangle past [`MAX_EXTENT`].
    fn offset(&mut self, (dx, dy): (f32, f32), (sx, sy): (f64, f64), region: Bounds, what: &str) {
        let (dx, dy) = (f64::from(dx) * sx, f64::from(dy) * sy);
        let moved = Bounds::new(
            region.left + dx,
            region.top + dy,
            region.right + dx,
            region.bottom + dy,
        );
        self.reach(Some(moved), region, MAX_EXTENT, || what.to_owned());
    }

    fn check(&mut self) {
        self.over |= self.far.is_some()
            || self.work > MAX_DRAWING * self.canvas
            || self.most_held > MAX_HELD_PIXELS
            || self.drawn > MAX_DRAWN;
    }

    fn visit(&mut self) -> bool {
        self.drawn += 1;
        self.check();
        !self.over
    }

    fn nodes(&mut self, group: &Group, ts: Transform, target: Bounds) {
        for node in group.children() {
            if !self.visit() {
                return;
            }
            match node {
                Node::Group(group) => self.group(group, ts, target),
                Node::Path(path) => self.path(path, ts, target),
                Node::Image(image) => self.image(image, ts, target),
                Node::Text(text) => self.group(text.flattened(), ts, target),
            }
        }
    }

    fn group(&mut self, group: &Group, ts: Transform, target: Bounds) {
        let ts = ts.pre_concat(group.transform());
        if !group.should_isolate() {
            self.nodes(group, ts, target);
            return;
        }
        let Some(bounds) = Bounds::through(group.layer_bounding_box().to_rect(), ts) else {
            return;
        };
        self.reach(Some(bounds), target, MAX_EXTENT, || {
            named("group", group.id())
        });
        // A filter's layer is its region; another's is its bounds and 2 pixels.
        let grow = if group.filters().is_empty() { 2.0 } else { 0.0 };
        let layer = bounds.whole(grow).within(self.bounds);
        if layer.area() == 0.0 {
            return;
        }
        let pixels = layer.area();
        let name = || named("group", group.id());
        self.hold(pixels, name);
        self.spend(pixels, weights::LAYER, name);
        self.nodes(group, ts, layer);
        for filter in group.filters() {
            self.filter(filter, ts, layer);
        }
        if let Some(clip) = group.clip_path() {
            self.clip(clip, ts, layer);
        }
        if let Some(mask) = group.mask() {
            self.mask(mask, ts, layer);
        }
        self.release(pixels);
    }

    fn path(&mut self, path: &usvg::Path, ts: Transform, target: Bounds) {
        if !path.is_visible() {
            return;
        }
        let area = |b: Option<Bounds>| b.map_or(0.0, |b| b.whole(1.0).within(target).area());
        if let Some(fill) = path.fill() {
            self.paint(fill.paint(), ts);
            let bounds = Bounds::through(path.bounding_box(), ts);
            self.segments(path, bounds, ts, target, 0.0);
            self.spend(area(bounds), weights::DRAW, || named("path", path.id()));
        }
        if let Some(stroke) = path.stroke() {
            self.paint(stroke.paint(), ts);
            let bounds = Bounds::through(path.stroke_bounding_box(), ts);
            // The outline lies within half the width of the path, times the miter limit.
            let (sx, sy) = ts.get_scale();
            let reach = f64::from(stroke.width().get() / 2.0 * stroke.miterlimit().get().max(1.5))
                * f64::from(sx.abs().max(sy.abs()));
            self.segments(path, bounds, ts, target, reach);
            self.spend(area(bounds), weights::DRAW, || named("path", path.id()));
        }
    }

    /// Flags a path if one of its segments (with outline `grow`) reaches past [`MAX_REACH`] and
    /// crosses into `target`, where tiny-skia's `f32` clipping fails. Far segments wholly beside
    /// the target are drawn exactly and are allowed.
    fn segments(
        &mut self,
        path: &usvg::Path,
        bounds: Option<Bounds>,
        ts: Transform,
        target: Bounds,
        grow: f64,
    ) {
        let Some(bounds) = bounds else {
            return;
        };
        let past = |b: Bounds, limit: f64| {
            b.left < target.left - limit
                || b.top < target.top - limit
                || b.right > target.right + limit
                || b.bottom > target.bottom + limit
        };
        let far = |b: Bounds| past(b, MAX_REACH);
        if !far(bounds) || self.far.is_some() {
            return;
        }
        let device = |p: resvg::tiny_skia::Point| {
            let mut p = p;
            ts.map_point(&mut p);
            (f64::from(p.x), f64::from(p.y))
        };
        // Whether the segment through `points` reaches far and into the target.
        let crosses = |points: &[(f64, f64)]| {
            let (mut left, mut top) = (f64::INFINITY, f64::INFINITY);
            let (mut right, mut bottom) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
            for (x, y) in points {
                left = left.min(*x);
                top = top.min(*y);
                right = right.max(*x);
                bottom = bottom.max(*y);
            }
            let hull = Bounds::new(left - grow, top - grow, right + grow, bottom + grow);
            // Wholly beside the target and within `MAX_EXTENT`, tiny-skia skips the segment or
            // draws it exactly along the edge.
            let beside = (hull.right < target.left - BESIDE
                || hull.left > target.right + BESIDE
                || hull.bottom < target.top - BESIDE
                || hull.top > target.bottom + BESIDE)
                && !past(hull, MAX_EXTENT);
            // Axis-aligned fill edges clip exactly within `MAX_AXIS`; stroke outlines do not.
            #[allow(clippy::float_cmp)]
            let axis = grow == 0.0
                && points.len() == 2
                && (points[0].0 == points[1].0 || points[0].1 == points[1].1)
                && !past(hull, MAX_AXIS);
            // Not a number: tiny-skia draws none of it.
            far(hull) && !beside && !axis
        };
        let (mut at, mut start) = ((0.0, 0.0), (0.0, 0.0));
        let mut reached = false;
        for segment in path.data().segments() {
            let to = match segment {
                PathSegment::MoveTo(p) => {
                    // The fill closes the previous subpath.
                    reached |= crosses(&[at, start]);
                    let p = device(p);
                    start = p;
                    p
                }
                PathSegment::LineTo(p) => {
                    let p = device(p);
                    reached |= crosses(&[at, p]);
                    p
                }
                PathSegment::QuadTo(p1, p) => {
                    let (p1, p) = (device(p1), device(p));
                    reached |= crosses(&[at, p1, p]);
                    p
                }
                PathSegment::CubicTo(p1, p2, p) => {
                    let (p1, p2, p) = (device(p1), device(p2), device(p));
                    reached |= crosses(&[at, p1, p2, p]);
                    p
                }
                PathSegment::Close => {
                    reached |= crosses(&[at, start]);
                    start
                }
            };
            at = to;
            if reached {
                break;
            }
        }
        reached |= crosses(&[at, start]);
        if reached {
            self.far = Some((named("path", path.id()), MAX_REACH));
            self.over = true;
        }
    }

    /// A pattern paint draws its tile at its shown scale on every fill or stroke.
    fn paint(&mut self, paint: &Paint, ts: Transform) {
        let Paint::Pattern(pattern) = paint else {
            return;
        };
        let (sx, sy) = ts.pre_concat(pattern.transform()).get_scale();
        let rect = pattern.rect();
        let tile = Bounds::new(
            0.0,
            0.0,
            f64::from((rect.width() * sx).round()),
            f64::from((rect.height() * sy).round()),
        );
        let pixels = tile.area();
        let name = || named("pattern", pattern.id());
        self.hold(pixels, name);
        self.spend(pixels, weights::CANVAS, name);
        self.nodes(pattern.root(), Transform::from_scale(sx, sy), tile);
        self.release(pixels);
    }

    fn image(&mut self, image: &usvg::Image, ts: Transform, target: Bounds) {
        if !image.is_visible() {
            return;
        }
        let name = || named("image", image.id());
        if let usvg::ImageKind::SVG(tree) = image.kind() {
            // Drawn onto a canvas the size of its target.
            let pixels = target.area();
            self.hold(pixels, name);
            self.spend(pixels, weights::CANVAS, name);
            self.nodes(tree.root(), ts, target);
            self.release(pixels);
        } else {
            let bounds = Bounds::through(image.bounding_box(), ts);
            self.reach(bounds, target, MAX_REACH, name);
            let pixels = bounds.map_or(0.0, |b| b.whole(1.0).within(target).area());
            self.spend(pixels, weights::DRAW, name);
        }
    }

    fn clip(&mut self, clip: &usvg::ClipPath, ts: Transform, layer: Bounds) {
        let pixels = layer.area();
        let name = || named("clip path", clip.id());
        self.hold(pixels, name);
        self.spend(pixels, weights::CLIP, name);
        self.clip_children(clip.root(), ts.pre_concat(clip.transform()), layer);
        if let Some(inner) = clip.clip_path() {
            self.clip(inner, ts, layer);
        }
        self.release(pixels);
    }

    /// A clip's content: paths are filled, groups count only for their own clips.
    fn clip_children(&mut self, group: &Group, ts: Transform, target: Bounds) {
        for node in group.children() {
            if !self.visit() {
                return;
            }
            match node {
                Node::Path(path) => {
                    if path.is_visible() {
                        let bounds = Bounds::through(path.bounding_box(), ts);
                        self.segments(path, bounds, ts, target, 0.0);
                        let pixels = bounds.map_or(0.0, |b| b.whole(1.0).within(target).area());
                        self.spend(pixels, weights::DRAW, || named("path", path.id()));
                    }
                }
                Node::Text(text) => self.clip_children(text.flattened(), ts, target),
                Node::Group(group) => {
                    let ts = ts.pre_concat(group.transform());
                    if let Some(clip) = group.clip_path() {
                        // Drawn on a canvas of its own, clipped, then drawn in.
                        let pixels = target.area();
                        let name = || named("group", group.id());
                        self.hold(pixels, name);
                        self.spend(pixels, weights::CLIP, name);
                        self.clip_children(group, ts, target);
                        self.clip(clip, ts, target);
                        self.release(pixels);
                    } else {
                        self.clip_children(group, ts, target);
                    }
                }
                Node::Image(_) => {}
            }
        }
    }

    fn mask(&mut self, mask: &usvg::Mask, ts: Transform, layer: Bounds) {
        // Its region is filled as a path.
        self.reach(
            Bounds::through(mask.rect().to_rect(), ts),
            layer,
            MAX_REACH,
            || named("mask", mask.id()),
        );
        let pixels = layer.area();
        let name = || named("mask", mask.id());
        self.hold(pixels, name);
        self.spend(pixels, weights::MASK, name);
        self.nodes(mask.root(), ts, layer);
        if let Some(inner) = mask.mask() {
            self.mask(inner, ts, layer);
        }
        self.release(pixels);
    }

    /// A filter's primitives, each result held until the filter ends. Generating and combining
    /// primitives cover the whole (unbounded) filter region; transforming ones cover their input.
    fn filter(&mut self, filter: &filter::Filter, ts: Transform, layer: Bounds) {
        use filter::{Input, Kind};
        let Some(region) = Bounds::through(filter.rect().to_rect(), ts) else {
            return;
        };
        self.reach(Some(region), layer, MAX_EXTENT, || {
            named("filter", filter.id())
        });
        for primitive in filter.primitives() {
            self.reach(
                Bounds::through(primitive.rect().to_rect(), ts),
                layer,
                MAX_EXTENT,
                || {
                    format!(
                        "{} in {}",
                        kind_name(primitive.kind()),
                        named("filter", filter.id())
                    )
                },
            );
        }
        let region = region.whole(0.0);
        let region_size = (region.width(), region.height());
        let source = (layer.width(), layer.height());
        let (sx, sy) = ts.get_scale();
        let scale = (f64::from(sx), f64::from(sy));
        // Result sizes by name; a later result replaces an earlier one of the same name.
        let mut results: Vec<(&str, (f64, f64))> = Vec::new();
        let size_of = |results: &[(&str, (f64, f64))], input: &Input| match input {
            Input::Reference(name) => results
                .iter()
                .rev()
                .find(|(result, _)| *result == name.as_str())
                .map_or(source, |(_, size)| *size),
            Input::SourceGraphic | Input::SourceAlpha => source,
        };
        let mut held = 0.0;
        for primitive in filter.primitives() {
            if !self.visit() {
                break;
            }
            let kind = primitive.kind();
            let input = match kind {
                Kind::ColorMatrix(fe) => Some(fe.input()),
                Kind::ComponentTransfer(fe) => Some(fe.input()),
                Kind::ConvolveMatrix(fe) => Some(fe.input()),
                Kind::DropShadow(fe) => Some(fe.input()),
                Kind::GaussianBlur(fe) => Some(fe.input()),
                Kind::Morphology(fe) => Some(fe.input()),
                Kind::Offset(fe) => Some(fe.input()),
                _ => None,
            };
            let size = input.map_or(region_size, |input| size_of(&results, input));
            let weight = weights::PRIMITIVE + self.primitive(kind, scale, size, ts, region);
            let pixels = size.0 * size.1;
            let what = || format!("{} in {}", kind_name(kind), named("filter", filter.id()));
            self.hold(pixels, what);
            held += pixels;
            self.spend(pixels, weight, what);
            results.push((primitive.result(), size));
        }
        self.release(held);
    }

    /// Per-pixel cost beyond [`weights::PRIMITIVE`], at (`sx`, `sy`) device pixels per unit.
    fn primitive(
        &mut self,
        kind: &filter::Kind,
        (sx, sy): (f64, f64),
        (width, height): (f64, f64),
        ts: Transform,
        region: Bounds,
    ) -> f64 {
        use filter::Kind;
        let blur = |x: f32, y: f32| {
            let (x, y) = (f64::from(x) * sx, f64::from(y) * sy);
            if x.abs() < 0.05 && y.abs() < 0.05 {
                0.0
            } else if x >= 2.0 || y >= 2.0 {
                weights::BOX_BLUR
            } else {
                weights::IIR_BLUR
            }
        };
        match kind {
            Kind::Offset(fe) => {
                self.offset((fe.dx(), fe.dy()), (sx, sy), region, "feOffset");
                0.0
            }
            Kind::Flood(_) | Kind::Tile(_) | Kind::ColorMatrix(_) => 0.0,
            Kind::Blend(_)
            | Kind::Composite(_)
            | Kind::ComponentTransfer(_)
            | Kind::DisplacementMap(_) => weights::PIXELWISE,
            Kind::DiffuseLighting(_) | Kind::SpecularLighting(_) => weights::LIGHTING,
            Kind::GaussianBlur(fe) => blur(fe.std_dev_x().get(), fe.std_dev_y().get()),
            Kind::DropShadow(fe) => {
                self.offset((fe.dx(), fe.dy()), (sx, sy), region, "feDropShadow");
                2.0 * weights::PIXELWISE + blur(fe.std_dev_x().get(), fe.std_dev_y().get())
            }
            Kind::Merge(fe) => {
                weights::MERGE_INPUT
                    * f64::from(u32::try_from(fe.inputs().len()).unwrap_or(u32::MAX))
            }
            Kind::Turbulence(fe) => {
                weights::TURBULENCE + weights::OCTAVE * f64::from(fe.num_octaves())
            }
            Kind::Morphology(fe) => {
                let (rx, ry) = (
                    f64::from(fe.radius_x().get()) * sx,
                    f64::from(fe.radius_y().get()) * sy,
                );
                if !(rx > 0.0 && ry > 0.0) {
                    return 0.0;
                }
                let columns = (2.0 * rx.ceil()).min(width);
                let rows = (2.0 * ry.ceil()).min(height);
                weights::MORPHOLOGY_CELL * columns * rows
            }
            Kind::ConvolveMatrix(fe) => {
                weights::CONVOLVE_CELL
                    * f64::from(fe.matrix().columns())
                    * f64::from(fe.matrix().rows())
            }
            Kind::Image(fe) => {
                // Its content, drawn onto a canvas of the region.
                self.nodes(fe.root(), ts, region);
                0.0
            }
        }
    }
}

fn named(what: &str, id: &str) -> String {
    if id.is_empty() {
        format!("a {what}")
    } else {
        format!("{what} \"{id}\"")
    }
}

fn kind_name(kind: &filter::Kind) -> &'static str {
    use filter::Kind;
    match kind {
        Kind::Blend(_) => "feBlend",
        Kind::ColorMatrix(_) => "feColorMatrix",
        Kind::ComponentTransfer(_) => "feComponentTransfer",
        Kind::Composite(_) => "feComposite",
        Kind::ConvolveMatrix(_) => "feConvolveMatrix",
        Kind::DiffuseLighting(_) => "feDiffuseLighting",
        Kind::DisplacementMap(_) => "feDisplacementMap",
        Kind::DropShadow(_) => "feDropShadow",
        Kind::Flood(_) => "feFlood",
        Kind::GaussianBlur(_) => "feGaussianBlur",
        Kind::Image(_) => "feImage",
        Kind::Merge(_) => "feMerge",
        Kind::Morphology(_) => "feMorphology",
        Kind::Offset(_) => "feOffset",
        Kind::SpecularLighting(_) => "feSpecularLighting",
        Kind::Tile(_) => "feTile",
        Kind::Turbulence(_) => "feTurbulence",
    }
}
