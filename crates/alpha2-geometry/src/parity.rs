//! Fixed scenes whose coverage and outlines are hashed the same way by the
//! native tests and the browser module, so a target that rasterizes or
//! strokes differently fails both (`tests/golden/vector_coverage.json`).
//! Trigonometry is `libm`'s, so every target builds the same geometry.

use std::fmt::Write as _;

use crate::raster::{RasterScratch, TILE_BYTES, TileCoverage, TileState, rasterize};
use crate::{IRect, StableHash, StrokeStyle, shapes, stroke_outline};
use alpha2_model::{FillRule, LineCap, LineJoin};
use kurbo::{Affine, BezPath, Point};

/// One thing a scene draws.
pub enum Draw {
    /// A fill.
    Fill(BezPath, FillRule),
    /// A stroke, expanded to its outline.
    Stroke(BezPath, StrokeStyle),
}

/// The scenes, by name.
pub const SCENES: [&str; 17] = [
    "circle",
    "pentagram_nonzero",
    "pentagram_evenodd",
    "overlap_same_nonzero",
    "overlap_same_evenodd",
    "overlap_opposite_nonzero",
    "bowtie",
    "loop_cubic_nonzero",
    "loop_cubic_evenodd",
    "bent_pentagon",
    "blob_stroke_round",
    "open_stroke_round_caps",
    "zigzag_miter_dash",
    "zigzag_bevel_square",
    "hairlines",
    "anim200",
    "items300",
];

/// The canvas sizes each scene is hashed at.
pub const SIZES: [(u32, u32); 2] = [(640, 360), (1080, 1920)];

fn circle(path: &mut BezPath, center: Point, radius: f64, clockwise: bool) {
    let k = shapes::KAPPA * radius;
    let s = if clockwise { 1.0 } else { -1.0 };
    let (cx, cy, r) = (center.x, center.y, radius);
    path.move_to((cx + r, cy));
    path.curve_to((cx + r, cy + s * k), (cx + k, cy + s * r), (cx, cy + s * r));
    path.curve_to((cx - k, cy + s * r), (cx - r, cy + s * k), (cx - r, cy));
    path.curve_to((cx - r, cy - s * k), (cx - k, cy - s * r), (cx, cy - s * r));
    path.curve_to((cx + k, cy - s * r), (cx + r, cy - s * k), (cx + r, cy));
    path.close_path();
}

fn polar(center: Point, radius: f64, angle: f64) -> Point {
    Point::new(
        center.x + radius * libm::cos(angle),
        center.y + radius * libm::sin(angle),
    )
}

fn star(path: &mut BezPath, center: Point, outer: f64, inner: f64, points: u32) {
    let count = points * 2;
    for i in 0..count {
        let angle = -core::f64::consts::FRAC_PI_2
            + core::f64::consts::PI * f64::from(i) / f64::from(points);
        let radius = if i % 2 == 0 { outer } else { inner };
        let p = polar(center, radius, angle);
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    path.close_path();
}

/// A smooth closed curve through `points` (Catmull-Rom as cubics).
fn smooth_closed(path: &mut BezPath, points: &[Point]) {
    let n = points.len();
    path.move_to(points[0]);
    for i in 0..n {
        let (p0, p1, p2, p3) = (
            points[(i + n - 1) % n],
            points[i],
            points[(i + 1) % n],
            points[(i + 2) % n],
        );
        let c1 = Point::new(p1.x + (p2.x - p0.x) / 6.0, p1.y + (p2.y - p0.y) / 6.0);
        let c2 = Point::new(p2.x - (p3.x - p1.x) / 6.0, p2.y - (p3.y - p1.y) / 6.0);
        path.curve_to(c1, c2, p2);
    }
    path.close_path();
}

fn smooth_open(path: &mut BezPath, points: &[Point]) {
    let n = points.len();
    path.move_to(points[0]);
    for i in 0..n - 1 {
        let p0 = points[i.saturating_sub(1)];
        let (p1, p2) = (points[i], points[i + 1]);
        let p3 = points[(i + 2).min(n - 1)];
        let c1 = Point::new(p1.x + (p2.x - p0.x) / 6.0, p1.y + (p2.y - p0.y) / 6.0);
        let c2 = Point::new(p2.x - (p3.x - p1.x) / 6.0, p2.y - (p3.y - p1.y) / 6.0);
        path.curve_to(c1, c2, p2);
    }
}

struct Lcg(u64);

impl Lcg {
    #[allow(clippy::cast_precision_loss)]
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn style(width: f64, join: LineJoin, cap: LineCap) -> StrokeStyle {
    StrokeStyle {
        width,
        join,
        miter_limit: 4.0,
        cap,
        dash: Vec::new(),
        dash_offset: 0.0,
    }
}

fn rule_of(name: &str) -> FillRule {
    if name.ends_with("evenodd") {
        FillRule::Evenodd
    } else {
        FillRule::Nonzero
    }
}

/// What scene `id` draws on a `w` by `h` canvas.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn scene(id: usize, w: u32, h: u32) -> Vec<Draw> {
    let (wf, hf) = (f64::from(w), f64::from(h));
    let s = wf.min(hf);
    let center = Point::new(wf * 0.5, hf * 0.5);
    let (cx, cy) = (center.x, center.y);
    let mut path = BezPath::new();
    let name = SCENES[id];
    match name {
        "circle" => {
            circle(&mut path, center, 0.4 * s, true);
            vec![Draw::Fill(path, FillRule::Nonzero)]
        }
        "pentagram_nonzero" | "pentagram_evenodd" => {
            for i in 0..5u32 {
                let angle = -core::f64::consts::FRAC_PI_2
                    + 2.0 * core::f64::consts::TAU / 5.0 * f64::from(i);
                let p = polar(center, 0.42 * s, angle);
                if i == 0 {
                    path.move_to(p);
                } else {
                    path.line_to(p);
                }
            }
            path.close_path();
            vec![Draw::Fill(path, rule_of(name))]
        }
        "overlap_same_nonzero" | "overlap_same_evenodd" | "overlap_opposite_nonzero" => {
            let opposite = name.contains("opposite");
            circle(&mut path, Point::new(cx - 0.15 * s, cy), 0.3 * s, true);
            circle(&mut path, Point::new(cx + 0.15 * s, cy), 0.3 * s, !opposite);
            vec![Draw::Fill(path, rule_of(name))]
        }
        "bowtie" => {
            path.move_to((cx - 0.4 * s, cy - 0.3 * s));
            path.line_to((cx + 0.4 * s, cy + 0.3 * s));
            path.line_to((cx + 0.4 * s, cy - 0.3 * s));
            path.line_to((cx - 0.4 * s, cy + 0.3 * s));
            path.close_path();
            vec![Draw::Fill(path, FillRule::Nonzero)]
        }
        "loop_cubic_nonzero" | "loop_cubic_evenodd" => {
            path.move_to((cx - 0.4 * s, cy + 0.3 * s));
            path.curve_to(
                (cx + 0.6 * s, cy - 0.5 * s),
                (cx - 0.6 * s, cy - 0.5 * s),
                (cx + 0.4 * s, cy + 0.3 * s),
            );
            path.close_path();
            vec![Draw::Fill(path, rule_of(name))]
        }
        "bent_pentagon" => {
            let points: Vec<Point> = (0..5u32)
                .map(|i| {
                    polar(
                        center,
                        0.35 * s,
                        -core::f64::consts::FRAC_PI_2 + core::f64::consts::TAU / 5.0 * f64::from(i),
                    )
                })
                .collect();
            path.move_to(points[0]);
            // The edge from vertex 0 to 1 bows outward; the rest are straight.
            let bow = Point::new(cx + 0.45 * s, cy - 0.45 * s);
            path.curve_to(
                Point::new(f64::midpoint(points[0].x, bow.x), bow.y),
                bow,
                points[1],
            );
            for p in &points[2..] {
                path.line_to(*p);
            }
            path.close_path();
            vec![
                Draw::Fill(path.clone(), FillRule::Nonzero),
                Draw::Stroke(path, style(0.012 * s, LineJoin::Round, LineCap::Butt)),
            ]
        }
        "blob_stroke_round" => {
            let points: Vec<Point> = (0..12u32)
                .map(|i| {
                    let angle = core::f64::consts::TAU / 12.0 * f64::from(i);
                    let radius = 0.3 * s * (1.0 + 0.25 * libm::sin(3.0 * angle));
                    polar(center, radius, angle)
                })
                .collect();
            smooth_closed(&mut path, &points);
            vec![Draw::Stroke(
                path,
                style(0.02 * s, LineJoin::Miter, LineCap::Butt),
            )]
        }
        "open_stroke_round_caps" => {
            let points: Vec<Point> = (0..9u32)
                .map(|i| {
                    Point::new(
                        wf * (0.1 + 0.1 * f64::from(i)),
                        cy + 0.25 * s * libm::sin(f64::from(i)),
                    )
                })
                .collect();
            smooth_open(&mut path, &points);
            vec![Draw::Stroke(
                path,
                style(0.03 * s, LineJoin::Miter, LineCap::Round),
            )]
        }
        "zigzag_miter_dash" | "zigzag_bevel_square" => {
            path.move_to((wf * 0.08, cy));
            for i in 1..=10u32 {
                let y = if i % 2 == 0 {
                    cy + 0.2 * s
                } else {
                    cy - 0.2 * s
                };
                path.line_to((wf * (0.08 + 0.084 * f64::from(i)), y));
            }
            let stroke = if name == "zigzag_miter_dash" {
                StrokeStyle {
                    dash: vec![0.06 * s, 0.03 * s],
                    dash_offset: 0.01 * s,
                    ..style(0.015 * s, LineJoin::Miter, LineCap::Butt)
                }
            } else {
                style(0.02 * s, LineJoin::Bevel, LineCap::Square)
            };
            vec![Draw::Stroke(path, stroke)]
        }
        "hairlines" => (0..20u32)
            .map(|i| {
                let mut line = BezPath::new();
                let angle = core::f64::consts::PI / 20.0 * f64::from(i);
                line.move_to(polar(center, 0.05 * s, angle));
                line.line_to(polar(center, 0.45 * s, angle));
                Draw::Stroke(
                    line,
                    style(0.25 + 0.15 * f64::from(i), LineJoin::Miter, LineCap::Butt),
                )
            })
            .collect(),
        "anim200" => {
            let points: Vec<Point> = (0..200u32)
                .map(|i| {
                    let angle = core::f64::consts::TAU / 200.0 * f64::from(i);
                    let radius = 0.35 * s * (1.0 + 0.1 * libm::sin(7.0 * angle));
                    polar(center, radius, angle)
                })
                .collect();
            smooth_closed(&mut path, &points);
            vec![
                Draw::Fill(path.clone(), FillRule::Nonzero),
                Draw::Stroke(path, style(0.01 * s, LineJoin::Miter, LineCap::Butt)),
            ]
        }
        "items300" => {
            let mut random = Lcg(42);
            let mut draws = Vec::new();
            for i in 0..300u32 {
                let at = Point::new(wf * random.next(), hf * random.next());
                let radius = 0.05 * s;
                let mut item = BezPath::new();
                match i % 3 {
                    0 => circle(&mut item, at, radius, true),
                    1 => star(&mut item, at, radius, 0.45 * radius, 5),
                    _ => smooth_closed(
                        &mut item,
                        &[
                            Point::new(at.x - radius, at.y - radius),
                            Point::new(at.x + radius, at.y - radius),
                            Point::new(at.x + radius, at.y + radius),
                            Point::new(at.x - radius, at.y + radius),
                        ],
                    ),
                }
                if i % 3 == 0 {
                    draws.push(Draw::Stroke(
                        item.clone(),
                        style(0.004 * s, LineJoin::Miter, LineCap::Butt),
                    ));
                }
                draws.push(Draw::Fill(item, FillRule::Nonzero));
            }
            draws
        }
        _ => Vec::new(),
    }
}

/// What a draw fills: its path, or its stroke's outline, and the rule.
#[must_use]
pub fn fill_of(draw: &Draw) -> (BezPath, FillRule) {
    match draw {
        Draw::Fill(path, rule) => (path.clone(), *rule),
        Draw::Stroke(path, style) => (
            stroke_outline(path, style, Affine::IDENTITY),
            FillRule::Nonzero,
        ),
    }
}

/// The pixels a draw's coverage is computed in on a `w` by `h` canvas.
#[must_use]
pub fn clip_of(path: &BezPath, w: u32, h: u32) -> IRect {
    let canvas = IRect::new(
        0,
        0,
        i32::try_from(w).unwrap_or(i32::MAX),
        i32::try_from(h).unwrap_or(i32::MAX),
    );
    IRect::covering(path, canvas)
}

/// Adds a coverage's tiles to `hash`.
pub fn hash_coverage(hash: &mut StableHash, coverage: &TileCoverage) {
    hash.u32(coverage.origin.0.cast_unsigned());
    hash.u32(coverage.origin.1.cast_unsigned());
    hash.u32(coverage.size.0);
    hash.u32(coverage.size.1);
    for tile in &coverage.tiles {
        match tile {
            TileState::Empty => hash.u32(0),
            TileState::Full => hash.u32(1),
            TileState::Partial(index) => {
                hash.u32(2);
                let bytes: &[u8; TILE_BYTES] = &coverage.partial[*index as usize];
                hash.bytes(bytes);
            }
        }
    }
}

/// The coverage hash and the stroke-outline hash of scene `id`, as hex.
#[must_use]
pub fn scene_hashes(id: usize, w: u32, h: u32) -> (String, String) {
    let mut coverage = StableHash::new();
    let mut outlines = StableHash::new();
    let mut scratch = RasterScratch::default();
    for draw in scene(id, w, h) {
        let (path, rule) = fill_of(&draw);
        if let Draw::Stroke(..) = draw {
            outlines.path(&path);
        }
        let tiles = rasterize(&path, rule, clip_of(&path, w, h), &mut scratch);
        hash_coverage(&mut coverage, &tiles);
    }
    (
        format!("{:032x}", coverage.finish()),
        format!("{:032x}", outlines.finish()),
    )
}

/// Every scene's hashes at every size, as the golden file holds them:
/// `(scene, width, height, coverage, outline)`.
#[must_use]
pub fn all_scene_hashes() -> Vec<(&'static str, u32, u32, String, String)> {
    let mut all = Vec::new();
    for (id, name) in SCENES.iter().enumerate() {
        for (w, h) in SIZES {
            let (coverage, outline) = scene_hashes(id, w, h);
            all.push((*name, w, h, coverage, outline));
        }
    }
    all
}

/// Content trees whose scene keys pin the whole build: shape paths,
/// placements, trims, strokes, dashes, layers, and paints.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn tree_scenes() -> Vec<(&'static str, Vec<crate::EvaluatedItem>)> {
    use crate::{
        EvaluatedGeometry, EvaluatedItem, EvaluatedPath, EvaluatedSubpath, EvaluatedTransform,
        EvaluatedVertex,
    };
    use alpha2_model::{
        GradientStop, Paint, PolystarKind, Rgba, Spread, TrimMode, Vec2 as ModelVec2,
    };
    use kurbo::Vec2;
    use std::sync::Arc;

    let solid = |red: f32, alpha: f32| Paint::Solid {
        color: Rgba::new(red, 0.25, 0.5, alpha),
    };
    let gradient = Paint::LinearGradient {
        start: ModelVec2::new(0.1, 0.1),
        end: ModelVec2::new(0.9, 0.7),
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: Rgba::new(1.0, 0.0, 0.0, 1.0),
            },
            GradientStop {
                offset: 1.0,
                color: Rgba::new(0.0, 0.0, 1.0, 0.5),
            },
        ],
        spread: Spread::Reflect,
    };
    let fill = |paint: Paint, rule: FillRule| EvaluatedItem::Fill {
        paint,
        opacity: 1.0,
        rule,
    };
    let stroke = |width: f64, join: LineJoin, cap: LineCap, dash: Vec<f64>| EvaluatedItem::Stroke {
        paint: solid(0.1, 1.0),
        width,
        opacity: 0.75,
        join,
        miter_limit: 4.0,
        cap,
        dash,
        dash_offset: 0.01,
    };
    let pentagon = EvaluatedPath {
        subpaths: vec![EvaluatedSubpath {
            closed: true,
            vertices: [
                (0.0, -0.2),
                (0.19, -0.06),
                (0.12, 0.16),
                (-0.12, 0.16),
                (-0.19, -0.06),
            ]
            .iter()
            .enumerate()
            .map(|(index, (x, y))| EvaluatedVertex {
                point: Point::new(*x, *y),
                in_tangent: if index == 1 {
                    Vec2::new(-0.05, -0.08)
                } else {
                    Vec2::ZERO
                },
                out_tangent: Vec2::ZERO,
            })
            .collect(),
        }],
    };
    vec![
        (
            "bent_pentagon_group",
            vec![EvaluatedItem::Group {
                items: Arc::from(vec![
                    EvaluatedItem::Geometry(EvaluatedGeometry::Path(Arc::new(pentagon))),
                    stroke(0.01, LineJoin::Round, LineCap::Butt, Vec::new()),
                    fill(gradient.clone(), FillRule::Nonzero),
                ]),
                transform: EvaluatedTransform {
                    position: Point::new(0.5, 0.5),
                    rotation_degrees: 12.5,
                    scale: Vec2::new(1.5, 0.8),
                    ..EvaluatedTransform::default()
                },
            }],
        ),
        (
            "trimmed_star_and_rounded_rectangle",
            vec![
                EvaluatedItem::Geometry(EvaluatedGeometry::Polystar {
                    kind: PolystarKind::Star,
                    center: Point::new(0.3, 0.5),
                    points: 6.0,
                    outer_radius: 0.25,
                    inner_radius: 0.1,
                    outer_roundness: 0.3,
                    inner_roundness: 0.0,
                    rotation_degrees: 7.0,
                }),
                EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
                    center: Point::new(0.7, 0.5),
                    size: Vec2::new(0.3, 0.4),
                    corner_radius: 0.05,
                    rotation_degrees: -20.0,
                }),
                EvaluatedItem::Trim {
                    start: 0.1,
                    end: 0.8,
                    offset: 0.3,
                    mode: TrimMode::Together,
                },
                stroke(
                    0.02,
                    LineJoin::Miter,
                    LineCap::Round,
                    vec![0.03, 0.02, 0.01],
                ),
            ],
        ),
        (
            "faded_layer_of_ellipses",
            vec![EvaluatedItem::Group {
                items: Arc::from(vec![
                    EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                        center: Point::new(0.4, 0.5),
                        size: Vec2::new(0.4, 0.3),
                        rotation_degrees: 30.0,
                    }),
                    EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                        center: Point::new(0.6, 0.5),
                        size: Vec2::new(0.4, 0.3),
                        rotation_degrees: -30.0,
                    }),
                    fill(solid(0.9, 0.8), FillRule::Evenodd),
                    EvaluatedItem::Trim {
                        start: 0.0,
                        end: 0.5,
                        offset: 0.0,
                        mode: TrimMode::Individually,
                    },
                    stroke(0.015, LineJoin::Bevel, LineCap::Square, Vec::new()),
                ]),
                transform: EvaluatedTransform {
                    opacity: 0.6,
                    ..EvaluatedTransform::default()
                },
            }],
        ),
    ]
}

/// The golden file's contents: every scene's coverage and outline hashes,
/// and every tree scene's key, as JSON text.
#[must_use]
pub fn golden_json() -> String {
    let mut out = String::from("{\n  \"coverage\": [\n");
    let hashes = all_scene_hashes();
    for (index, (name, w, h, coverage, outline)) in hashes.iter().enumerate() {
        let comma = if index + 1 == hashes.len() { "" } else { "," };
        // Writing to a String cannot fail.
        let _ = writeln!(
            out,
            "    {{ \"scene\": \"{name}\", \"width\": {w}, \"height\": {h}, \"coverage\": \"{coverage}\", \"outline\": \"{outline}\" }}{comma}"
        );
    }
    out.push_str("  ],\n  \"trees\": [\n");
    let trees = tree_scenes();
    let mut rows = Vec::new();
    for (name, items) in &trees {
        for (w, h) in SIZES {
            let scene = crate::build_scene(items, crate::CanvasSize::new(w, h));
            let mut coverage = StableHash::new();
            let mut scratch = RasterScratch::default();
            for op in &scene.ops {
                if let crate::VectorOp::Fill {
                    path, rule, bounds, ..
                } = op
                {
                    hash_coverage(
                        &mut coverage,
                        &rasterize(path, *rule, *bounds, &mut scratch),
                    );
                }
            }
            rows.push(format!(
                "    {{ \"tree\": \"{name}\", \"width\": {w}, \"height\": {h}, \"key\": \"{:032x}\", \"coverage\": \"{:032x}\" }}",
                scene.key,
                coverage.finish()
            ));
        }
    }
    out.push_str(&rows.join(",\n"));
    out.push_str("\n  ]\n}\n");
    out
}
