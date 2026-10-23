use crate::query::{hit_test, nearest_point, path_lengths, resample_path, shape_bounds, trim_path};
use crate::trim::{Measured, TrimSpan, subpaths, trim_together};
use crate::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedPath, EvaluatedSubpath,
    EvaluatedTransform, EvaluatedVertex, VectorOp, affine, arclen, build_scene, shapes, spatial,
};
use alpha2_model::{FillRule, LineCap, LineJoin, Paint, PolystarKind, Rgba, TrimMode};
use kurbo::{Affine, BezPath, ParamCurve, PathEl, Point, Vec2};
use std::sync::Arc;

const SQUARE: CanvasSize = CanvasSize::new(100, 100);

fn red() -> Paint {
    Paint::Solid {
        color: Rgba::new(1.0, 0.0, 0.0, 1.0),
    }
}

fn fill() -> EvaluatedItem {
    EvaluatedItem::Fill {
        paint: red(),
        opacity: 1.0,
        rule: FillRule::Nonzero,
    }
}

fn stroke(width: f64) -> EvaluatedItem {
    EvaluatedItem::Stroke {
        paint: red(),
        width,
        opacity: 1.0,
        join: LineJoin::Miter,
        miter_limit: 4.0,
        cap: LineCap::Butt,
        dash: Vec::new(),
        dash_offset: 0.0,
    }
}

fn rectangle(cx: f64, cy: f64, w: f64, h: f64) -> EvaluatedItem {
    EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
        center: Point::new(cx, cy),
        size: Vec2::new(w, h),
        corner_radius: 0.0,
        rotation_degrees: 0.0,
    })
}

fn points(path: &BezPath) -> Vec<Point> {
    path.elements()
        .iter()
        .filter_map(|element| match *element {
            PathEl::MoveTo(p)
            | PathEl::LineTo(p)
            | PathEl::CurveTo(_, _, p)
            | PathEl::QuadTo(_, p) => Some(p),
            PathEl::ClosePath => None,
        })
        .collect()
}

#[test]
fn a_rectangle_starts_top_right_and_runs_clockwise() {
    let path = shapes::rectangle(Point::new(50.0, 40.0), Vec2::new(20.0, 10.0), 0.0, 0.0);
    assert_eq!(
        points(&path),
        [
            Point::new(60.0, 35.0),
            Point::new(60.0, 45.0),
            Point::new(40.0, 45.0),
            Point::new(40.0, 35.0)
        ]
    );
    // Rounded: below the top-right arc, with the radius capped at half the shorter side.
    let rounded = shapes::rectangle(Point::new(50.0, 40.0), Vec2::new(20.0, 10.0), 9.0, 0.0);
    assert_eq!(points(&rounded)[0], Point::new(60.0, 40.0));
}

#[test]
fn an_ellipse_starts_at_its_top() {
    let path = shapes::ellipse(Point::new(50.0, 50.0), Vec2::new(40.0, 20.0), 0.0);
    assert_eq!(
        points(&path),
        [
            Point::new(50.0, 40.0),
            Point::new(70.0, 50.0),
            Point::new(50.0, 60.0),
            Point::new(30.0, 50.0),
            Point::new(50.0, 40.0)
        ]
    );
}

#[test]
fn rotation_is_clockwise_in_pixel_space() {
    let turned = affine::rotation(90.0) * Point::new(10.0, 0.0);
    assert!((turned.x - 0.0).abs() < 1e-12 && (turned.y - 10.0).abs() < 1e-12);
    // A group placement puts the anchor on the position and scales about it.
    let placed = affine::placement(
        Point::new(50.0, 50.0),
        Point::new(10.0, 10.0),
        Vec2::new(2.0, 2.0),
        0.0,
    );
    assert_eq!(placed * Point::new(10.0, 10.0), Point::new(50.0, 50.0));
    assert_eq!(placed * Point::new(11.0, 10.0), Point::new(52.0, 50.0));
    assert!((affine::max_scale(Affine::scale_non_uniform(3.0, 0.5)) - 3.0).abs() < 1e-12);
}

#[test]
fn a_polystar_has_its_first_point_straight_up() {
    let star = shapes::polystar(&shapes::Polystar {
        star: true,
        center: Point::new(50.0, 50.0),
        points: 5.9,
        outer_radius: 40.0,
        inner_radius: 20.0,
        outer_roundness: 0.0,
        inner_roundness: 0.0,
        degrees: 0.0,
    });
    let corners = points(&star);
    // Five points (the fraction dropped), ten corners and the closing one.
    assert_eq!(corners.len(), 11);
    assert!((corners[0].x - 50.0).abs() < 1e-9 && (corners[0].y - 10.0).abs() < 1e-9);
    let inner = corners[1];
    assert!((affine::distance(inner, Point::new(50.0, 50.0)) - 20.0).abs() < 1e-9);
}

#[test]
fn a_circle_measures_its_circumference() {
    let circle = shapes::ellipse(Point::new(0.0, 0.0), Vec2::new(200.0, 200.0), 0.0);
    let length: f64 = subpaths(&circle)[0]
        .segments
        .iter()
        .map(arclen::length)
        .sum();
    // The four-cubic circle is within 0.03 percent of the true one.
    let exact = std::f64::consts::TAU * 100.0;
    assert!((length - exact).abs() / exact < 3e-4, "{length}");
}

#[test]
fn an_arc_length_inverts() {
    let circle = shapes::ellipse(Point::new(0.0, 0.0), Vec2::new(200.0, 200.0), 0.0);
    let segment = subpaths(&circle)[0].segments[0];
    let total = arclen::length(&segment);
    for fraction in [0.1, 0.5, 0.9] {
        let t = arclen::parameter_at(&segment, total * fraction, total);
        let run = arclen::length_between(&segment, 0.0, t);
        assert!(
            (run - total * fraction).abs() <= arclen::ACCURACY,
            "{fraction}"
        );
    }
}

fn measured(path: &BezPath) -> Vec<Measured> {
    subpaths(path)
        .into_iter()
        .map(|subpath| Measured {
            subpath,
            measure: Affine::IDENTITY,
        })
        .collect()
}

fn line(x0: f64, x1: f64) -> BezPath {
    let mut path = BezPath::new();
    path.move_to((x0, 0.0));
    path.line_to((x1, 0.0));
    path
}

#[test]
fn trimming_together_spans_every_subpath_in_order() {
    let mut path = line(0.0, 100.0);
    path.extend(line(200.0, 300.0).elements().iter().copied());
    let kept = trim_together(
        &measured(&path),
        TrimSpan {
            start: 0.25,
            end: 0.75,
            offset: 0.0,
        },
    );
    // Half of 200 total, from 50 to 150: the end of the first line, the start of the second.
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].0, 0);
    assert_eq!(kept[0].1.segments[0].start(), Point::new(50.0, 0.0));
    assert_eq!(kept[1].0, 1);
    assert_eq!(kept[1].1.segments[0].end(), Point::new(250.0, 0.0));
}

#[test]
fn crossing_ends_trim_away_everything() {
    let kept = trim_together(
        &measured(&line(0.0, 100.0)),
        TrimSpan {
            start: 0.6,
            end: 0.4,
            offset: 0.0,
        },
    );
    assert_eq!(kept.len(), 0);
}

#[test]
fn an_offset_wraps_around_a_closed_path_as_one_piece() {
    let square = shapes::rectangle(Point::new(50.0, 50.0), Vec2::new(40.0, 40.0), 0.0, 0.0);
    let kept = trim_together(
        &measured(&square),
        TrimSpan {
            start: 0.0,
            end: 0.5,
            offset: 0.875,
        },
    );
    // From 7/8 of the way round, past the start, to 3/8: one piece through the top-right
    // corner where the square starts.
    assert_eq!(kept.len(), 1);
    let piece = &kept[0].1;
    let start = piece.segments[0].start();
    let end = piece.segments.last().expect("segments").end();
    assert!(
        (start.x - 50.0).abs() < 1e-6 && (start.y - 30.0).abs() < 1e-6,
        "{start:?}"
    );
    assert!(
        (end.x - 50.0).abs() < 1e-6 && (end.y - 70.0).abs() < 1e-6,
        "{end:?}"
    );
    let length: f64 = piece.segments.iter().map(arclen::length).sum();
    assert!((length - 80.0).abs() < 1e-6);
}

#[test]
fn trimming_individually_cuts_each_subpath_alone() {
    let mut path = line(0.0, 100.0);
    path.extend(line(200.0, 220.0).elements().iter().copied());
    let kept = crate::trim::trim_individually(
        &measured(&path),
        TrimSpan {
            start: 0.0,
            end: 0.5,
            offset: 0.0,
        },
    );
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].1.segments[0].end(), Point::new(50.0, 0.0));
    assert_eq!(kept[1].1.segments[0].end(), Point::new(210.0, 0.0));
}

#[test]
fn a_span_ending_on_a_segment_boundary_leaves_no_sliver_to_stroke() {
    // Two of the turned ellipse's four quarters sum to a hair under half its length, which
    // once left a 3e-14 sliver of the third whose stroke put NaN in the outline.
    let ellipse = shapes::ellipse(Point::new(256.0, 180.0), Vec2::new(256.0, 108.0), 30.0);
    let kept = trim_together(
        &measured(&ellipse),
        TrimSpan {
            start: 0.0,
            end: 0.5,
            offset: 0.0,
        },
    );
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].1.segments.len(), 2);
    let mut piece = BezPath::new();
    crate::trim::append(&mut piece, &kept[0].1);
    let style = crate::StrokeStyle {
        width: 5.4,
        join: LineJoin::Bevel,
        miter_limit: 4.0,
        cap: LineCap::Square,
        dash: Vec::new(),
        dash_offset: 0.0,
    };
    let outline = crate::stroke_outline(&piece, &style, Affine::IDENTITY);
    assert!(
        outline
            .elements()
            .iter()
            .all(|element| element.end_point().is_none_or(Point::is_finite)),
        "{outline:?}"
    );
}

#[test]
fn spatial_motion_is_constant_speed_along_the_curve() {
    let (from, to) = (Point::new(0.0, 0.0), Point::new(100.0, 0.0));
    let (out_tangent, in_tangent) = (Vec2::new(0.0, -60.0), Vec2::new(0.0, -60.0));
    let quarter = spatial::point_along(from, out_tangent, in_tangent, to, 0.25);
    let half = spatial::point_along(from, out_tangent, in_tangent, to, 0.5);
    // Symmetric, so halfway along the length is the curve's middle.
    assert!((half.x - 50.0).abs() < 1e-3, "{half:?}");
    assert!(half.y < -40.0);
    assert!(quarter.x < half.x);
    // Past the end it continues along the end tangent: straight down here.
    let past = spatial::point_along(from, out_tangent, in_tangent, to, 1.1);
    assert!((past.x - 100.0).abs() < 1e-9 && past.y > 0.0, "{past:?}");
    // Without tangents it is a straight line.
    let straight = spatial::point_along(from, Vec2::ZERO, Vec2::ZERO, to, 0.3);
    assert!((straight.x - 30.0).abs() < 1e-3 && straight.y.abs() < 1e-9);
}

#[test]
fn a_paint_paints_only_the_geometry_before_it_and_earlier_paints_are_on_top() {
    let items = [
        rectangle(0.25, 0.5, 0.2, 0.2),
        stroke(0.01),
        rectangle(0.75, 0.5, 0.2, 0.2),
        fill(),
    ];
    let scene = build_scene(&items, SQUARE);
    // Back first: the fill (listed last, of both rectangles), then the stroke (of the first).
    assert_eq!(scene.ops.len(), 2);
    let VectorOp::Fill {
        bounds: fill_bounds,
        ..
    } = &scene.ops[0]
    else {
        panic!("fill first");
    };
    let VectorOp::Fill {
        bounds: stroke_bounds,
        ..
    } = &scene.ops[1]
    else {
        panic!("stroke second");
    };
    assert!(fill_bounds.x1 > 80, "fills both: {fill_bounds:?}");
    assert!(
        stroke_bounds.x1 < 50,
        "strokes the first only: {stroke_bounds:?}"
    );
}

#[test]
fn a_nested_group_lends_its_geometry_to_later_paints_and_keeps_its_transform() {
    let group = EvaluatedItem::Group {
        items: Arc::from(vec![rectangle(0.0, 0.0, 0.2, 0.2)]),
        transform: EvaluatedTransform {
            position: Point::new(0.5, 0.5),
            ..EvaluatedTransform::default()
        },
    };
    let scene = build_scene(&[group, fill()], SQUARE);
    let VectorOp::Fill { bounds, .. } = &scene.ops[0] else {
        panic!("one fill");
    };
    assert_eq!(*bounds, crate::IRect::new(39, 39, 61, 61));
}

#[test]
fn a_faded_group_draws_in_a_layer_of_its_items() {
    let group = EvaluatedItem::Group {
        items: Arc::from(vec![rectangle(0.5, 0.5, 0.2, 0.2), fill(), stroke(0.02)]),
        transform: EvaluatedTransform {
            opacity: 0.5,
            ..EvaluatedTransform::default()
        },
    };
    let scene = build_scene(&[group], SQUARE);
    assert!(
        matches!(scene.ops[0], VectorOp::PushLayer { opacity, .. } if (opacity - 0.5).abs() < f32::EPSILON)
    );
    assert!(matches!(scene.ops[3], VectorOp::PopLayer));
    assert_eq!(scene.fill_count(), 2);
    // An invisible group draws nothing; an opaque one draws without a layer.
    let hidden = EvaluatedItem::Group {
        items: Arc::from(vec![rectangle(0.5, 0.5, 0.2, 0.2), fill()]),
        transform: EvaluatedTransform {
            opacity: 0.0,
            ..EvaluatedTransform::default()
        },
    };
    assert_eq!(build_scene(&[hidden], SQUARE).ops.len(), 0);
}

#[test]
fn a_scene_key_changes_with_what_it_draws_and_its_size() {
    let items = [rectangle(0.5, 0.5, 0.2, 0.2), fill()];
    let key = build_scene(&items, SQUARE).key;
    assert_eq!(key, build_scene(&items, SQUARE).key);
    assert_ne!(key, build_scene(&items, CanvasSize::new(100, 101)).key);
    let moved = [rectangle(0.5, 0.51, 0.2, 0.2), fill()];
    assert_ne!(key, build_scene(&moved, SQUARE).key);
}

#[test]
fn a_scaled_group_scales_its_stroke_width() {
    let group = |scale: f64| EvaluatedItem::Group {
        items: Arc::from(vec![rectangle(0.0, 0.0, 0.2, 0.2), stroke(0.02)]),
        transform: EvaluatedTransform {
            position: Point::new(0.5, 0.5),
            scale: Vec2::new(scale, scale),
            ..EvaluatedTransform::default()
        },
    };
    let bounds = |scale| match &build_scene(&[group(scale)], SQUARE).ops[0] {
        VectorOp::Fill { path, .. } => kurbo::Shape::bounding_box(path.as_ref()),
        _ => panic!("a fill"),
    };
    // Unscaled: 20 px square, 2 px stroke; doubled: 40 px, 4 px.
    assert!((bounds(1.0).width() - 22.0).abs() < 0.01);
    assert!((bounds(2.0).width() - 44.0).abs() < 0.01);
}

fn line_path(points: &[(f64, f64)], closed: bool) -> EvaluatedPath {
    EvaluatedPath {
        subpaths: vec![EvaluatedSubpath {
            closed,
            vertices: points
                .iter()
                .map(|(x, y)| EvaluatedVertex {
                    point: Point::new(*x, *y),
                    ..EvaluatedVertex::default()
                })
                .collect(),
        }],
    }
}

#[test]
fn queries_answer_in_normalized_coordinates_and_pixels() {
    let path = line_path(&[(0.1, 0.5), (0.9, 0.5)], false);
    assert_eq!(path_lengths(&path, SQUARE), [80.0]);
    let nearest = nearest_point(&path, SQUARE, Point::new(0.3, 0.4)).expect("a point");
    assert_eq!((nearest.subpath, nearest.segment), (0, 0));
    assert!((nearest.point.x - 0.3).abs() < 1e-6 && (nearest.distance - 10.0).abs() < 1e-6);
    let trimmed = trim_path(&path, SQUARE, 0.25, 0.75, 0.0);
    let vertices = &trimmed.subpaths[0].vertices;
    assert!((vertices[0].point.x - 0.3).abs() < 1e-9 && (vertices[1].point.x - 0.7).abs() < 1e-9);
    let resampled = resample_path(&path, SQUARE, &[5]);
    let xs: Vec<f64> = resampled.subpaths[0]
        .vertices
        .iter()
        .map(|vertex| (vertex.point.x * 1e6).round() / 1e6)
        .collect();
    assert_eq!(xs, [0.1, 0.3, 0.5, 0.7, 0.9]);
}

#[test]
fn a_hit_finds_the_front_paint_by_its_rule_and_a_stroke_by_its_width() {
    let ring = EvaluatedItem::Geometry(EvaluatedGeometry::Path(Arc::new(EvaluatedPath {
        subpaths: vec![
            line_path(&[(0.2, 0.2), (0.8, 0.2), (0.8, 0.8), (0.2, 0.8)], true).subpaths[0].clone(),
            line_path(&[(0.4, 0.4), (0.6, 0.4), (0.6, 0.6), (0.4, 0.6)], true).subpaths[0].clone(),
        ],
    })));
    let items = [
        ring,
        stroke(0.04),
        EvaluatedItem::Fill {
            paint: red(),
            opacity: 1.0,
            rule: FillRule::Evenodd,
        },
    ];
    // On the outer edge: the stroke, listed first, is in front.
    assert_eq!(
        hit_test(&items, SQUARE, Point::new(0.2, 0.5)).as_deref(),
        Some("items[1]")
    );
    assert_eq!(
        hit_test(&items, SQUARE, Point::new(0.3, 0.5)).as_deref(),
        Some("items[2]")
    );
    // The even-odd hole, inside the inner square but off its stroke.
    assert_eq!(hit_test(&items, SQUARE, Point::new(0.5, 0.5)), None);
    assert_eq!(hit_test(&items, SQUARE, Point::new(0.05, 0.05)), None);
    let bounds = shape_bounds(&items, SQUARE);
    assert_eq!(bounds[0].0, "items[0]");
    let rect = bounds[0].1.expect("geometry has bounds");
    assert!((rect.x0 - 0.2).abs() < 1e-9 && (rect.x1 - 0.8).abs() < 1e-9);
    let stroked = bounds[1].1.expect("a stroke has bounds");
    assert!(stroked.x0 < 0.2 && stroked.x1 > 0.8);
}

#[test]
fn a_trim_item_cuts_the_geometry_before_it() {
    let items = [
        EvaluatedItem::Geometry(EvaluatedGeometry::Path(Arc::new(line_path(
            &[(0.1, 0.5), (0.9, 0.5)],
            false,
        )))),
        EvaluatedItem::Trim {
            start: 0.0,
            end: 0.5,
            offset: 0.0,
            mode: TrimMode::Together,
        },
        stroke(0.02),
    ];
    let scene = build_scene(&items, SQUARE);
    let VectorOp::Fill { path, .. } = &scene.ops[0] else {
        panic!("a stroke");
    };
    let bounds = kurbo::Shape::bounding_box(path.as_ref());
    assert!(
        (bounds.x0 - 10.0).abs() < 1e-6 && (bounds.x1 - 50.0).abs() < 1e-6,
        "{bounds:?}"
    );
}

#[test]
fn polystar_kind_and_paths_build_without_panicking_at_the_limits() {
    for kind in [PolystarKind::Star, PolystarKind::Polygon] {
        let geometry = EvaluatedGeometry::Polystar {
            kind,
            center: Point::new(0.5, 0.5),
            points: f64::from(alpha2_model::MAX_POLYSTAR_POINTS),
            outer_radius: 0.4,
            inner_radius: 0.2,
            outer_roundness: 1.0,
            inner_roundness: 1.0,
            rotation_degrees: 33.0,
        };
        let scene = build_scene(&[EvaluatedItem::Geometry(geometry), fill()], SQUARE);
        assert_eq!(scene.fill_count(), 1);
    }
}

/// Whether kurbo was built with its `std` feature, which a workspace build
/// turns on through other crates: its math is then the platform's, not
/// libm's.
fn kurbo_has_std() -> bool {
    // `BezPath::to_svg` exists only with `std`, and an inherent method wins
    // over this trait's; without it, the trait's answers `()`.
    #[allow(dead_code)]
    trait WithoutStd {
        fn to_svg(&self) {}
    }
    impl WithoutStd for BezPath {}
    let svg: &dyn std::any::Any = &BezPath::new().to_svg();
    svg.is::<String>()
}

/// Drops the outline and scene-key hashes, which only libm's math pins.
fn without_float_bits(golden: &mut serde_json::Value) {
    for list in ["coverage", "trees"] {
        for row in golden[list].as_array_mut().expect("rows") {
            let row = row.as_object_mut().expect("a row");
            row.remove("outline");
            row.remove("key");
        }
    }
}

/// The pinned coverage and stroke-outline hashes of the parity scenes, which
/// the browser module must give too (`npm run test:wasm-parity`). Run with
/// `ALPHA2_UPDATE_GOLDENS=1 cargo test -p alpha2-geometry` to re-pin after
/// an intended change. With kurbo's `std` math, only coverage is compared.
#[test]
fn coverage_and_outlines_match_the_pinned_hashes() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/vector_coverage.json"
    );
    let text = crate::parity::golden_json();
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    if update {
        assert!(
            !kurbo_has_std(),
            "kurbo has `std` here, so its outlines are not libm's: re-pin with `cargo test -p alpha2-geometry`"
        );
        std::fs::write(path, &text).expect("write the golden");
        return;
    }
    let mut actual: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let mut pinned: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    if kurbo_has_std() {
        without_float_bits(&mut actual);
        without_float_bits(&mut pinned);
    }
    assert_eq!(actual, pinned);
}
