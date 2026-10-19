//! What drawing a converted SVG at a size costs resvg, counted before it
//! draws.
//!
//! resvg draws without a work limit: every fill and stroke over the area it
//! covers, every group that needs its own layer (opacity, a clip, a mask, a
//! filter, a blend mode) into a pixmap as large as the group's bounds (up to
//! five canvases a side), every filter primitive over the filter's region
//! (which is not limited at all) at a cost per pixel that grows with its
//! radius or kernel at the size it is drawn, and every pattern tile and nested
//! SVG once each place it is drawn. A filter's results are all held until the
//! filter ends. So a few hundred bytes can ask for hours of work (a
//! morphology of radius 400 over 1080p) or more memory than there is (one
//! flood over a region twenty canvases wide).
//!
//! This walks the tree as resvg's renderer does and counts, in pixels:
//! - **work**, each pixel weighted by what touching it costs relative to a
//!   blended fill ([`weights`]); refused past [`MAX_DRAWING`] canvases;
//! - **memory**, the layers, masks, clips, tiles, and filter results held at
//!   once; refused past [`MAX_HELD_PIXELS`];
//! - **things drawn**, each node each time it is drawn; refused past
//!   [`MAX_DRAWN`].
//!
//! Areas are bounds, clipped to what they are drawn onto: never less than
//! resvg touches.

use resvg::tiny_skia::{PathSegment, Rect, Transform};
use resvg::usvg::{self, Group, Node, Paint, filter};

use crate::api::ExtensionError;
use crate::budget::{FLOOR_CANVAS, MAX_DRAWING};

/// Most pixels of layers, masks, clips, pattern tiles, nested documents, and
/// filter results a render holds at once: 1 GiB at four bytes a pixel.
pub(super) const MAX_HELD_PIXELS: f64 = 268_435_456.0;

/// Most nodes one render draws, each time it draws them (through every
/// pattern, mask, clip, and filter image that draws them again). Drawing
/// even an empty one costs resvg about a microsecond.
pub(super) const MAX_DRAWN: u64 = 4_000_000;

/// How far past what it is drawn onto (the canvas, a layer, a tile), in
/// device pixels, a path, a stroke, or an image may reach. tiny-skia 0.12
/// clips paths with `f32` math and walks them in fixed point: past about a
/// million pixels its clipped edges land outside the pixmap and it panics,
/// which aborts the browser module and, unwinding through its own drawing,
/// the native process. Within twice this (and a layer's
/// offset) it never did, over millions of random paths. resvg draws what the
/// document holds as it is, so a document that reaches farther at the size
/// asked is refused: a path one of whose segments reaches this far and into
/// what it is drawn onto (see [`Walk::segments`]), or an image or a mask's
/// region that does. Of 24,290 system SVGs, three icons drawn at 1080p reach
/// farther than a quarter of this and two terminal recordings (film strips
/// of frames side by side, hundreds of canvases wide) much farther, all in
/// small segments.
pub(super) const MAX_REACH: f64 = 262_144.0;

/// How far a horizontal or vertical line of a fill may reach into what it is
/// drawn onto: tiny-skia clips it exactly, short of `i32` pixel rounding (it
/// failed on rectangles a billion pixels out, never at a hundred million).
pub(super) const MAX_AXIS: f64 = 33_554_432.0;

/// How far clear of what it is drawn onto a path's segment must lie for
/// tiny-skia to pass it over, or draw it as a line along the edge, exactly
/// (its bounds are rounded out by a pixel and a half).
const BESIDE: f64 = 16.0;

/// How far a layer's bounds or a filter's region may reach: resvg makes
/// whole-pixel rectangles of them in `i32` first (and panics past it),
/// before it limits a layer to five canvases a side.
pub(super) const MAX_EXTENT: f64 = 1_073_741_824.0;

/// What touching one pixel costs, against blending a fill over it (one).
/// Measured natively at 1080p (this machine: one canvas of blended fill,
/// semi-transparent or gradient, 4 to 8 ms), rounded up, each filter primitive
/// with the input it may convert to linear RGB first (about 3).
mod weights {
    /// A fill or a stroke: a blended canvas is 4.3 ms, a gradient 8 ms; an
    /// opaque solid fill is cheaper, but whether something above it fades
    /// is not known here.
    pub(super) const DRAW: f64 = 1.0;
    /// A layer: allocated, drawn into, composited (11 ms a canvas).
    pub(super) const LAYER: f64 = 2.0;
    /// A clip: its own canvas, cleared, drawn, made a mask, applied (20 ms).
    pub(super) const CLIP: f64 = 3.0;
    /// A mask: its own canvas, drawn, made a mask, applied (28 ms).
    pub(super) const MASK: f64 = 4.0;
    /// A pattern tile or a nested document's canvas: allocated and drawn
    /// onto (each place it is drawn).
    pub(super) const CANVAS: f64 = 2.0;
    /// What every filter primitive costs: its result, and its input copied
    /// and converted between sRGB and linear RGB (up to 22 ms a canvas).
    pub(super) const PRIMITIVE: f64 = 3.0;
    /// feComponentTransfer (44 ms), feComposite (34 ms), feBlend (30 ms),
    /// feDisplacementMap (29 ms), feDiffuseLighting (25 ms).
    pub(super) const PIXELWISE: f64 = 3.0;
    /// feSpecularLighting (85 ms), and feDiffuseLighting with any light.
    pub(super) const LIGHTING: f64 = 9.0;
    /// feGaussianBlur as three box blurs, a deviation of 2 pixels or more on
    /// an axis, whatever it is (81 to 84 ms).
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

/// Counts what resvg's drawing of `tree` at `width` x `height` costs and
/// refuses it past a budget.
pub(super) fn check(tree: &usvg::Tree, width: u32, height: u32) -> Result<(), ExtensionError> {
    let (w, h) = (f64::from(width), f64::from(height));
    let size = tree.size();
    // Sizes are within MAX_CANVAS_SIDE, which f32 holds exactly.
    #[allow(clippy::cast_precision_loss)]
    let ts = Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
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

    /// `rect` in device pixels through `ts`, or `None` where it has none.
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

    /// Grown to whole pixels outward, `by` more each side.
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
    /// The canvas's pixels.
    canvas: f64,
    /// Where resvg keeps a layer.
    bounds: Bounds,
    /// Pixels touched, weighted.
    work: f64,
    /// Pixels held now, and the most at once.
    held: f64,
    most_held: f64,
    drawn: u64,
    /// The single thing that costs the most work, and the one that holds
    /// the most pixels, with what they are.
    heaviest: (f64, String),
    largest: (f64, String),
    /// The first thing that reaches farther than it may ([`MAX_REACH`],
    /// [`MAX_EXTENT`]), and how far that is.
    far: Option<(String, f64)>,
    /// Whether a budget is already spent: the walk stops counting.
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

    /// Notes `bounds` (device pixels, `None` where it has none) as drawn
    /// onto `target`, past which it may reach `limit`.
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

    /// Notes an offset of (`dx`, `dy`) units at a scale of (`sx`, `sy`):
    /// resvg moves an image by it in whole pixels (`i32`) and makes a
    /// rectangle of it there, which past [`MAX_EXTENT`] overflows.
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
            // Its outline lies within half its width of the path, a miter's
            // as far as the miter limit takes it.
            let (sx, sy) = ts.get_scale();
            let reach = f64::from(stroke.width().get() / 2.0 * stroke.miterlimit().get().max(1.5))
                * f64::from(sx.abs().max(sy.abs()));
            self.segments(path, bounds, ts, target, reach);
            self.spend(area(bounds), weights::DRAW, || named("path", path.id()));
        }
    }

    /// Notes a path drawn onto `target` through `ts` (its outline `grow`
    /// device pixels around it) whose bounds reach past [`MAX_REACH`] when
    /// one of its segments does and crosses into `target`: tiny-skia clips
    /// such an edge with `f32` math, where it fails; one wholly beside the
    /// pixmap it passes over or draws as a line along its edge, exactly, as
    /// a path far out but drawn in small pieces (a strip of recorded
    /// terminal frames hundreds of canvases wide) is.
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
            // Wholly beside what it is drawn onto, clear of it by more than the bounds tiny-skia
            // rounds out, and nearer than `MAX_EXTENT`: passed over, or drawn as a line along its
            // edge, exactly (a hairline failed at 1e20 pixels, never at 1e12).
            let beside = (hull.right < target.left - BESIDE
                || hull.left > target.right + BESIDE
                || hull.bottom < target.top - BESIDE
                || hull.top > target.bottom + BESIDE)
                && !past(hull, MAX_EXTENT);
            // A horizontal or vertical line of a fill is clipped exactly, nearer than
            // `MAX_AXIS`; a stroke's outline around one has corners.
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
                    // The fill closes the subpath before.
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

    /// A pattern paint draws its tile, at the scale it is shown, each time
    /// a path is filled or stroked with it.
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
            // Drawn onto a canvas the size of the one it is drawn onto.
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

    /// A clip's content: paths filled, groups only for their own clips.
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

    /// A filter's primitives, each result held until the filter ends. A
    /// primitive that makes an image (a flood, a turbulence, an image, a
    /// tile, lighting, a displacement) or combines two (a blend, a
    /// composite, a merge) works over the filter's whole region, which
    /// resvg does not limit; one that transforms its input (a blur, a
    /// morphology, a convolution, a color matrix, a transfer, an offset, a
    /// drop shadow) over its input, the layer or an earlier result.
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
        // Each result's size, by name; the latest of a name is the one used.
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

    /// What a primitive costs a pixel beyond [`weights::PRIMITIVE`], at a
    /// scale of (`sx`, `sy`) device pixels a unit, over an image of `width`
    /// by `height`.
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
