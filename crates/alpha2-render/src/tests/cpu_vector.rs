//! The CPU vector pass and generator against the GPU's.

use std::sync::Arc;

use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedTransform, VectorScene, build_scene,
};
use alpha2_model::{FillRule, GradientStop, Paint, Rgba, Spread};

use super::cpu_parity::{Parity, Tolerance};
use crate::{
    BlendSpace, CpuRenderer, EffectId, EffectParams, EffectRuntime, GENERATOR_EFFECT_ID,
    ParamValue, RenderGraph, RenderSize,
};

/// Not square, and not whole tiles across or down.
fn size() -> RenderSize {
    RenderSize::new(100, 70).expect("size")
}

const SPACES: [BlendSpace; 2] = [BlendSpace::LinearLight, BlendSpace::SrgbEncoded];

const SPREADS: [Spread; 3] = [Spread::Pad, Spread::Repeat, Spread::Reflect];

/// Stop lists: two stops, the four-stop spectrum (one translucent), and
/// eight uneven stops with a hard step.
fn stop_lists() -> [Vec<(f32, [f32; 4])>; 3] {
    [
        vec![(0.0, [0.0, 0.0, 0.0, 1.0]), (1.0, [1.0, 1.0, 1.0, 1.0])],
        vec![
            (0.0, [0.95, 0.2, 0.2, 1.0]),
            (0.4, [0.95, 0.85, 0.2, 1.0]),
            (0.7, [0.2, 0.7, 0.95, 0.6]),
            (1.0, [0.5, 0.2, 0.9, 1.0]),
        ],
        vec![
            (0.05, [0.1, 0.3, 0.9, 1.0]),
            (0.15, [0.9, 0.9, 0.1, 0.8]),
            (0.3, [0.2, 0.8, 0.3, 1.0]),
            (0.3, [0.8, 0.1, 0.5, 1.0]),
            (0.5, [0.0, 0.0, 0.0, 0.3]),
            (0.62, [1.0, 1.0, 1.0, 1.0]),
            (0.8, [0.6, 0.4, 0.1, 0.9]),
            (0.95, [0.3, 0.9, 0.9, 1.0]),
        ],
    ]
}

fn stops(list: &[(f32, [f32; 4])]) -> Vec<GradientStop> {
    list.iter()
        .map(|(offset, color)| GradientStop {
            offset: *offset,
            color: Rgba::new(color[0], color[1], color[2], color[3]),
        })
        .collect()
}

fn solid(red: f32, green: f32, blue: f32, alpha: f32) -> Paint {
    Paint::Solid {
        color: Rgba::new(red, green, blue, alpha),
    }
}

/// A gradient of `kind` (2 linear, 3 radial, 4 conic). Its points are
/// off the pixel lattice: a pixel centre exactly on a repeat's wrap is a tie
/// that rounding settles either way, on two GPUs as well.
fn gradient(kind: u32, spread: Spread, list: &[(f32, [f32; 4])]) -> Paint {
    let stops = stops(list);
    match kind {
        2 => Paint::LinearGradient {
            start: alpha2_model::Vec2::new(0.213, 0.307),
            end: alpha2_model::Vec2::new(0.552, 0.618),
            stops,
            spread,
        },
        3 => Paint::RadialGradient {
            center: alpha2_model::Vec2::new(0.453, 0.551),
            radius: 0.29,
            stops,
            spread,
        },
        _ => Paint::ConicGradient {
            center: alpha2_model::Vec2::new(0.407, 0.503),
            angle_degrees: 30.0,
            stops,
        },
    }
}

fn fill(paint: Paint, opacity: f64) -> EvaluatedItem {
    EvaluatedItem::Fill {
        paint,
        opacity,
        rule: FillRule::Nonzero,
    }
}

fn rectangle(center: [f64; 2], size: [f64; 2], corner: f64, rotation: f64) -> EvaluatedItem {
    EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
        center: Point::new(center[0], center[1]),
        size: Vec2::new(size[0], size[1]),
        corner_radius: corner,
        rotation_degrees: rotation,
    })
}

fn ellipse(center: [f64; 2], size: [f64; 2]) -> EvaluatedItem {
    EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
        center: Point::new(center[0], center[1]),
        size: Vec2::new(size[0], size[1]),
        rotation_degrees: 0.0,
    })
}

/// Past every edge of the canvas: no antialiased edge inside it.
fn everywhere() -> EvaluatedItem {
    rectangle([0.5, 0.5], [1.2, 1.2], 0.0, 0.0)
}

fn group(items: Vec<EvaluatedItem>, transform: EvaluatedTransform) -> EvaluatedItem {
    EvaluatedItem::Group {
        items: Arc::from(items),
        transform,
    }
}

fn scene(items: &[EvaluatedItem]) -> Arc<VectorScene> {
    let size = size();
    Arc::new(build_scene(items, CanvasSize::new(size.width, size.height)))
}

/// Checks `items` in both blend spaces.
fn check_items(parity: &Parity, name: &str, tolerance: Tolerance, items: &[EvaluatedItem]) {
    let scene = scene(items);
    for space in SPACES {
        parity.check(
            &format!("{name}-{space:?}"),
            size(),
            tolerance,
            |graph, _| {
                graph.set_blend_space(space);
                Ok(graph.add_vector(Arc::clone(&scene)))
            },
        );
    }
}

#[test]
fn solid_fills_match() {
    let parity = Parity::new();
    check_items(
        &parity,
        "vector-solid-interior",
        Tolerance::Close,
        &[everywhere(), fill(solid(0.9, 0.45, 0.1, 1.0), 1.0)],
    );
    check_items(
        &parity,
        "vector-solid-translucent",
        Tolerance::Close,
        &[everywhere(), fill(solid(0.2, 0.6, 0.85, 0.55), 0.7)],
    );
    check_items(
        &parity,
        "vector-solid-shapes",
        Tolerance::Edges,
        &[
            rectangle([0.3, 0.4], [0.37, 0.5], 0.2, 0.0),
            fill(solid(0.9, 0.3, 0.2, 1.0), 1.0),
            ellipse([0.7, 0.55], [0.4, 0.6]),
            fill(solid(0.1, 0.8, 0.4, 1.0), 0.8),
            rectangle([0.52, 0.5], [0.3, 0.7], 0.0, 23.0),
            fill(solid(0.3, 0.2, 0.9, 0.6), 1.0),
        ],
    );
}

#[test]
fn overlapping_translucent_fills_match() {
    let parity = Parity::new();
    let mut items = vec![everywhere(), fill(solid(0.05, 0.05, 0.1, 1.0), 1.0)];
    for (index, color) in [
        [1.0, 0.2, 0.2, 0.5],
        [0.2, 1.0, 0.3, 0.6],
        [0.2, 0.3, 1.0, 0.45],
        [1.0, 1.0, 1.0, 0.3],
    ]
    .into_iter()
    .enumerate()
    {
        #[allow(clippy::cast_precision_loss)]
        let offset = index as f64 * 0.12;
        items.push(ellipse([0.3 + offset, 0.35 + offset * 0.5], [0.45, 0.6]));
        items.push(fill(solid(color[0], color[1], color[2], color[3]), 0.85));
    }
    check_items(&parity, "vector-overlap", Tolerance::Edges, &items);
}

#[test]
fn gradients_match_for_every_kind_spread_and_stop_count() {
    let parity = Parity::new();
    for (count, list) in stop_lists().iter().enumerate() {
        for kind in [2, 3, 4] {
            // A conic gradient has no spread.
            let spreads: &[Spread] = if kind == 4 { &SPREADS[..1] } else { &SPREADS };
            for spread in spreads {
                check_items(
                    &parity,
                    &format!("vector-gradient-{kind}-{spread:?}-{count}"),
                    Tolerance::Close,
                    &[everywhere(), fill(gradient(kind, *spread, list), 1.0)],
                );
            }
        }
    }
}

#[test]
fn transformed_paints_match() {
    let parity = Parity::new();
    let list = &stop_lists()[1];
    for kind in [2, 3, 4] {
        for (name, transform) in [
            (
                "scaled",
                EvaluatedTransform {
                    anchor: Point::new(0.5, 0.5),
                    position: Point::new(0.45, 0.5),
                    scale: Vec2::new(1.7, 0.6),
                    ..EvaluatedTransform::default()
                },
            ),
            (
                "rotated",
                EvaluatedTransform {
                    anchor: Point::new(0.5, 0.5),
                    position: Point::new(0.5, 0.5),
                    scale: Vec2::new(0.8, 1.1),
                    rotation_degrees: 37.0,
                    ..EvaluatedTransform::default()
                },
            ),
        ] {
            // Large enough to cover the canvas however it turns.
            let items = [group(
                vec![
                    rectangle([0.5, 0.5], [4.0, 4.0], 0.0, 0.0),
                    fill(gradient(kind, Spread::Reflect, list), 1.0),
                ],
                transform,
            )];
            check_items(
                &parity,
                &format!("vector-transformed-{kind}-{name}"),
                Tolerance::Close,
                &items,
            );
        }
    }
    // Turned edges, with a gradient on them.
    check_items(
        &parity,
        "vector-transformed-edges",
        Tolerance::Edges,
        &[group(
            vec![
                rectangle([0.5, 0.5], [0.5, 0.4], 0.1, 0.0),
                fill(gradient(3, Spread::Repeat, list), 0.9),
            ],
            EvaluatedTransform {
                anchor: Point::new(0.5, 0.5),
                position: Point::new(0.55, 0.45),
                scale: Vec2::new(1.3, 0.9),
                rotation_degrees: -21.0,
                ..EvaluatedTransform::default()
            },
        )],
    );
}

#[test]
fn group_layers_match() {
    let parity = Parity::new();
    let inner = vec![
        rectangle([0.4, 0.5], [0.45, 0.6], 0.0, 0.0),
        fill(solid(1.0, 0.1, 0.1, 1.0), 1.0),
        ellipse([0.6, 0.5], [0.45, 0.7]),
        fill(gradient(2, Spread::Pad, &stop_lists()[1]), 0.8),
    ];
    let faded = |opacity: f64, items: Vec<EvaluatedItem>| {
        group(
            items,
            EvaluatedTransform {
                opacity,
                ..EvaluatedTransform::default()
            },
        )
    };
    check_items(
        &parity,
        "vector-group",
        Tolerance::Edges,
        &[
            everywhere(),
            fill(solid(0.1, 0.2, 0.3, 1.0), 1.0),
            faded(0.55, inner.clone()),
        ],
    );
    // A layer within a layer, over transparency.
    let mut nested = inner;
    nested.push(faded(
        0.4,
        vec![
            rectangle([0.5, 0.3], [0.8, 0.25], 0.0, 0.0),
            fill(solid(0.2, 0.9, 1.0, 1.0), 1.0),
        ],
    ));
    check_items(
        &parity,
        "vector-group-nested",
        Tolerance::Edges,
        &[faded(0.7, nested)],
    );
}

/// The generator's parameters for one paint.
fn paint_params(
    kind: u32,
    a: [f32; 2],
    b: [f32; 2],
    spread: u32,
    list: &[(f32, [f32; 4])],
) -> EffectParams<'static> {
    let mut params = EffectParams::new();
    let mut offsets = [0.0_f32; 8];
    for (index, (offset, color)) in list.iter().enumerate() {
        offsets[index] = *offset;
        params = params.with(format!("fill_c{index}"), ParamValue::Color(*color));
    }
    #[allow(clippy::cast_precision_loss)]
    let count = list.len() as f32;
    params
        .with("fill_kind", ParamValue::Enum(kind))
        .with("fill_a", ParamValue::Vec2(a))
        .with("fill_b", ParamValue::Vec2(b))
        .with("fill_spread", ParamValue::Enum(spread))
        .with("fill_count", ParamValue::Float(count))
        .with(
            "fill_offsets0",
            ParamValue::Vec4([offsets[0], offsets[1], offsets[2], offsets[3]]),
        )
        .with(
            "fill_offsets1",
            ParamValue::Vec4([offsets[4], offsets[5], offsets[6], offsets[7]]),
        )
}

#[test]
fn generator_paints_match() {
    let parity = Parity::new();
    let lists = stop_lists();
    let cases: Vec<(String, EffectParams<'static>)> = vec![
        ("none".into(), paint_params(0, [0.0; 2], [1.0, 0.0], 0, &[])),
        (
            "solid".into(),
            paint_params(1, [0.0; 2], [1.0, 0.0], 0, &[(0.0, [0.3, 0.7, 0.2, 0.8])]),
        ),
        (
            "linear-one-stop".into(),
            paint_params(2, [0.0; 2], [1.0, 0.0], 0, &lists[1][..1]),
        ),
        (
            "linear-pad".into(),
            paint_params(2, [0.2, 0.2], [0.6, 0.7], 0, &lists[0]),
        ),
        (
            "linear-reflect".into(),
            paint_params(2, [0.2, 0.2], [0.6, 0.7], 2, &lists[1]),
        ),
        (
            "radial-repeat".into(),
            paint_params(3, [0.4, 0.6], [0.25, 0.0], 1, &lists[2]),
        ),
        (
            "radial-pad".into(),
            paint_params(3, [0.5, 0.5], [0.4, 0.0], 0, &lists[1]),
        ),
        (
            "conic".into(),
            paint_params(4, [0.5, 0.5], [0.3, 0.0], 0, &lists[1]),
        ),
        (
            "conic-eight".into(),
            paint_params(4, [0.3, 0.6], [-1.2, 0.0], 0, &lists[2]),
        ),
    ];
    let id = EffectId::new(GENERATOR_EFFECT_ID.to_owned(), 1);
    for (name, params) in &cases {
        parity.check(
            &format!("generator-{name}"),
            size(),
            Tolerance::Close,
            |graph, _| {
                Ok(graph.add_generator(id.clone(), params.clone(), EffectRuntime::default()))
            },
        );
    }
}

/// The CPU picture of `scene` on `threads` threads, at `size`.
fn cpu_draw(
    threads: usize,
    size: RenderSize,
    scene: &Arc<VectorScene>,
    space: BlendSpace,
) -> Arc<crate::CpuImage> {
    let renderer = CpuRenderer::new(threads);
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(space);
    let node = graph.add_vector(Arc::clone(scene));
    graph.set_output(node).expect("output");
    renderer.execute_graph(&graph).expect("CPU draws")
}

#[test]
fn a_small_shape_keeps_only_its_tiles() {
    let scene = scene(&[
        ellipse([0.3, 0.4], [0.12, 0.1]),
        fill(gradient(3, Spread::Pad, &stop_lists()[1]), 1.0),
    ]);
    for space in SPACES {
        let bounds = cpu_draw(1, size(), &scene, space).bounds();
        // A 12 x 7 pixel ellipse at (30, 28) lies within two by two tiles.
        assert!(
            bounds.width <= 32 && bounds.height <= 32 && !bounds.is_empty(),
            "{bounds:?}"
        );
    }
}

#[test]
fn any_thread_count_draws_the_same() {
    let size = RenderSize::new(600, 300).expect("size");
    let items = [
        everywhere(),
        fill(gradient(4, Spread::Pad, &stop_lists()[2]), 1.0),
        group(
            vec![
                ellipse([0.4, 0.5], [0.5, 0.8]),
                fill(solid(0.9, 0.2, 0.3, 0.7), 1.0),
                rectangle([0.6, 0.5], [0.4, 0.5], 0.1, 15.0),
                fill(gradient(2, Spread::Reflect, &stop_lists()[1]), 0.9),
            ],
            EvaluatedTransform {
                opacity: 0.6,
                ..EvaluatedTransform::default()
            },
        ),
    ];
    let scene = Arc::new(build_scene(
        &items,
        CanvasSize::new(size.width, size.height),
    ));
    for space in SPACES {
        let pixels = cpu_draw(1, size, &scene, space).read_rgba8();
        for threads in [2, 3, 8] {
            assert_eq!(
                cpu_draw(threads, size, &scene, space).read_rgba8(),
                pixels,
                "{threads} threads in {space:?}"
            );
        }
    }
}
