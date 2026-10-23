use crate::{ShapesPlan, sample_paint, sample_path, sample_point};
use alpha2_geometry::EvaluatedItem;
use alpha2_model::{
    Animated, AnimatedPoint, Interpolation, Keyframe, Paint, Path, PathVertex, PointKeyframe, Rgba,
    ShapeItem, Subpath, Vec2,
};
use alpha2_time::MediaTime;
use std::sync::Arc;

fn items(json: &str) -> Vec<ShapeItem> {
    serde_json::from_str(json).expect("items JSON")
}

const STATIC_GROUP: &str = r#"{"type":"group","items":[
    {"type":"rectangle","center":{"mode":"constant","value":{"x":0.5,"y":0.5}},
     "size":{"mode":"constant","value":{"x":0.2,"y":0.2}}},
    {"type":"fill","paint":{"mode":"constant","value":{"type":"solid","color":{"red":1,"green":0,"blue":0,"alpha":1}}}}]}"#;

const MOVING_ELLIPSE: &str = r#"{"type":"ellipse",
    "center":{"mode":"keyframes","keyframes":[
        {"time":0,"value":{"x":0,"y":0},"interpolation":{"type":"linear"}},
        {"time":1000,"value":{"x":1,"y":0},"interpolation":{"type":"hold"}}]},
    "size":{"mode":"constant","value":{"x":0.1,"y":0.1}}}"#;

#[test]
fn a_tree_without_keyframes_is_one_shared_value_at_every_time() {
    let plan = ShapesPlan::new(&items(&format!("[{STATIC_GROUP}]")));
    assert!(!plan.is_animated());
    let first = plan.evaluate(MediaTime::ZERO, 16.0 / 9.0);
    let later = plan.evaluate(MediaTime(5_000), 1.0);
    assert!(Arc::ptr_eq(&first, &later));
}

#[test]
fn a_static_subtree_beside_an_animated_item_is_shared_between_frames() {
    let plan = ShapesPlan::new(&items(&format!("[{STATIC_GROUP},{MOVING_ELLIPSE}]")));
    assert!(plan.is_animated());
    let group_items = |tree: &Arc<[EvaluatedItem]>| match &tree[0] {
        EvaluatedItem::Group { items, .. } => Arc::clone(items),
        _ => panic!("a group"),
    };
    let first = plan.evaluate(MediaTime::ZERO, 1.0);
    let later = plan.evaluate(MediaTime(500), 1.0);
    assert!(Arc::ptr_eq(&group_items(&first), &group_items(&later)));
    assert_ne!(first[1], later[1], "the ellipse moves");
    // A group whose transform animates keeps its unchanging items shared too.
    let moving_group = items(
        r#"[{"type":"group","transform":{"rotation_degrees":{"mode":"keyframes","keyframes":[
            {"time":0,"value":0,"interpolation":{"type":"linear"}},
            {"time":1000,"value":90,"interpolation":{"type":"hold"}}]}},
          "items":[{"type":"rectangle","center":{"mode":"constant","value":{"x":0.5,"y":0.5}},
                    "size":{"mode":"constant","value":{"x":0.2,"y":0.2}}}]}]"#,
    );
    let plan = ShapesPlan::new(&moving_group);
    let (a, b) = (
        plan.evaluate(MediaTime(100), 1.0),
        plan.evaluate(MediaTime(900), 1.0),
    );
    assert!(Arc::ptr_eq(&group_items(&a), &group_items(&b)));
    assert_ne!(a[0], b[0]);
}

fn point_keys(out_tangent: Vec2, in_tangent: Vec2, interpolation: Interpolation) -> AnimatedPoint {
    AnimatedPoint::Keyframes {
        keyframes: vec![
            PointKeyframe {
                time: MediaTime(0),
                value: Vec2::new(0.1, 0.5),
                interpolation,
                out_tangent,
                in_tangent: Vec2::default(),
            },
            PointKeyframe {
                time: MediaTime(1_000),
                value: Vec2::new(0.9, 0.5),
                interpolation: Interpolation::Hold,
                out_tangent: Vec2::default(),
                in_tangent,
            },
        ],
    }
}

#[test]
fn a_position_without_tangents_samples_bit_for_bit_as_a_vector_does() {
    let curve = alpha2_model::CubicBezier::new(0.3, -0.4, 0.7, 1.4);
    for interpolation in [Interpolation::Linear, Interpolation::CubicBezier(curve)] {
        let point = point_keys(Vec2::default(), Vec2::default(), interpolation);
        let AnimatedPoint::Keyframes { keyframes } = &point else {
            unreachable!()
        };
        let vector = Animated::Keyframes {
            keyframes: keyframes
                .iter()
                .map(|keyframe| Keyframe {
                    time: keyframe.time,
                    value: keyframe.value,
                    interpolation: keyframe.interpolation,
                })
                .collect(),
        };
        for ticks in [0, 1, 137, 500, 999, 1_000, 2_000] {
            let time = MediaTime(ticks);
            let a = sample_point(&point, time, 16.0 / 9.0);
            let b = crate::sample_vec2(&vector, time);
            assert_eq!(
                (a.x.to_bits(), a.y.to_bits()),
                (b.x.to_bits(), b.y.to_bits()),
                "{ticks}"
            );
        }
    }
}

#[test]
fn a_curved_position_moves_at_constant_speed_and_overshoots_along_its_tangent() {
    // Both tangents lift the path upward (toward smaller y): an arch.
    let point = point_keys(
        Vec2::new(0.0, -0.3),
        Vec2::new(0.0, -0.3),
        Interpolation::Linear,
    );
    let at = |ticks| sample_point(&point, MediaTime(ticks), 1.0);
    let middle = at(500);
    assert!((middle.x - 0.5).abs() < 1e-4, "{middle:?}");
    assert!(middle.y < 0.4, "{middle:?}");
    // Equal times, equal distances along the arch.
    let step = |a: Vec2, b: Vec2| {
        let (dx, dy) = (f64::from(b.x - a.x), f64::from(b.y - a.y));
        (dx * dx + dy * dy).sqrt()
    };
    let (d1, d2) = (step(at(100), at(200)), step(at(400), at(500)));
    assert!((d1 - d2).abs() < 2e-3, "{d1} {d2}");
    // An overshooting curve continues past the end along the end tangent: straight down.
    let curve = alpha2_model::CubicBezier::new(0.3, 0.0, 0.5, 1.6);
    let point = point_keys(
        Vec2::new(0.0, -0.3),
        Vec2::new(0.0, -0.3),
        Interpolation::CubicBezier(curve),
    );
    let past = (1..1_000)
        .map(|ticks| sample_point(&point, MediaTime(ticks), 1.0))
        .find(|value| value.y > 0.5 + 1e-3)
        .expect("overshoots past the end");
    assert!((past.x - 0.9).abs() < 1e-4, "{past:?}");
}

#[test]
fn paths_and_paints_interpolate_between_keyframes() {
    let square = |offset: f32| Path {
        subpaths: vec![Subpath {
            closed: true,
            vertices: [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]
                .iter()
                .map(|(x, y)| PathVertex {
                    point: Vec2::new(x + offset, *y),
                    ..PathVertex::default()
                })
                .collect(),
        }],
    };
    let path = Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime(0),
                value: square(0.0),
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                time: MediaTime(100),
                value: square(1.0),
                interpolation: Interpolation::Hold,
            },
        ],
    };
    let halfway = sample_path(&path, MediaTime(50));
    assert!((halfway.subpaths[0].vertices[1].point.x - 1.5).abs() < 1e-12);
    let paint = Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime(0),
                value: Paint::Solid {
                    color: Rgba::new(1.0, 0.0, 0.0, 0.0),
                },
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                time: MediaTime(100),
                value: Paint::Solid {
                    color: Rgba::new(0.0, 0.0, 1.0, 1.0),
                },
                interpolation: Interpolation::Hold,
            },
        ],
    };
    // Premultiplied: fading in from transparent red is blue all the way.
    let Paint::Solid { color } = sample_paint(&paint, MediaTime(50)) else {
        panic!("solid");
    };
    assert!(
        color.red.abs() < 1e-6
            && (color.blue - 1.0).abs() < 1e-6
            && (color.alpha - 0.5).abs() < 1e-6
    );
}
