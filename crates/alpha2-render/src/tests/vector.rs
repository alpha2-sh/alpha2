//! The vector pass: fills drawn from tile coverage, painted, layered, and
//! stored as the engine stores pictures.

use super::*;
use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedTransform, VectorScene, build_scene,
};
use alpha2_model::{FillRule, GradientStop, Paint, Rgba, Spread};

pub(super) fn solid(red: f32, green: f32, blue: f32, alpha: f32) -> Paint {
    Paint::Solid {
        color: Rgba::new(red, green, blue, alpha),
    }
}

pub(super) fn fill(paint: Paint) -> EvaluatedItem {
    EvaluatedItem::Fill {
        paint,
        opacity: 1.0,
        rule: FillRule::Nonzero,
    }
}

pub(super) fn rectangle(cx: f64, cy: f64, w: f64, h: f64) -> EvaluatedItem {
    EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
        center: Point::new(cx, cy),
        size: Vec2::new(w, h),
        corner_radius: 0.0,
        rotation_degrees: 0.0,
    })
}

/// The scene `items` draw at `size`, rendered by `renderer` in `space` and
/// read back.
pub(super) fn draw(
    renderer: &Renderer,
    size: RenderSize,
    items: &[EvaluatedItem],
    space: BlendSpace,
) -> Vec<u8> {
    let scene = build_scene(items, CanvasSize::new(size.width, size.height));
    draw_scene(renderer, size, scene, space)
}

pub(super) fn draw_scene(
    renderer: &Renderer,
    size: RenderSize,
    scene: VectorScene,
    space: BlendSpace,
) -> Vec<u8> {
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(space);
    let node = graph.add_vector(Arc::new(scene));
    graph.set_output(node).expect("known output");
    let output = renderer.execute_graph(&graph).expect("the scene renders");
    read_rgba8(renderer, &output)
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * width + x) * 4) as usize;
    [rgba[at], rgba[at + 1], rgba[at + 2], rgba[at + 3]]
}

#[test]
fn a_filled_rectangle_covers_its_pixels_and_nothing_else() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let size = RenderSize::new(64, 48).expect("size");
    for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
        // 32 x 24 pixels from (16, 12): half a pixel past the edge on the right.
        let items = [
            rectangle(0.5 + 0.25 / 64.0, 0.5, 32.5 / 64.0, 0.5),
            fill(solid(1.0, 0.5, 0.0, 1.0)),
        ];
        let rgba = draw(&renderer, size, &items, space);
        // Half of 255 is a tie the linear-light round trip may round either way.
        let inside = pixel(&rgba, 64, 30, 20);
        assert!(
            inside[0] == 255 && inside[1].abs_diff(128) <= 1 && inside[2] == 0 && inside[3] == 255,
            "{space:?}: {inside:?}"
        );
        assert_eq!(pixel(&rgba, 64, 2, 2), [0; 4], "{space:?}");
        assert_eq!(pixel(&rgba, 64, 15, 20), [0; 4], "{space:?}");
        // The half-covered column right of the edge.
        let edge = pixel(&rgba, 64, 48, 20);
        assert!((126..=129).contains(&edge[3]), "{space:?}: {edge:?}");
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

fn stroke(paint: Paint, width: f64) -> EvaluatedItem {
    EvaluatedItem::Stroke {
        paint,
        width,
        opacity: 1.0,
        join: alpha2_model::LineJoin::Miter,
        miter_limit: 4.0,
        cap: alpha2_model::LineCap::Butt,
        dash: Vec::new(),
        dash_offset: 0.0,
    }
}

fn group(items: Vec<EvaluatedItem>, transform: EvaluatedTransform) -> EvaluatedItem {
    EvaluatedItem::Group {
        items: Arc::from(items),
        transform,
    }
}

fn gradient(kind: u32, a: [f32; 2], b: [f32; 2], spread: Spread) -> Paint {
    let stops = super::generator::SPECTRUM
        .iter()
        .map(|(offset, color)| GradientStop {
            offset: *offset,
            color: Rgba::new(color[0], color[1], color[2], color[3]),
        })
        .collect();
    let start = alpha2_model::Vec2::new(a[0], a[1]);
    match kind {
        2 => Paint::LinearGradient {
            start,
            end: alpha2_model::Vec2::new(b[0], b[1]),
            stops,
            spread,
        },
        3 => Paint::RadialGradient {
            center: start,
            radius: b[0],
            stops,
            spread,
        },
        _ => Paint::ConicGradient {
            center: start,
            angle_degrees: b[0],
            stops,
        },
    }
}

fn test_paint(kind: u32, a: [f32; 2], b: [f32; 2], spread: u32) -> super::generator::TestPaint {
    let b = if kind == 4 {
        [b[0].to_radians(), b[1]]
    } else {
        b
    };
    super::generator::TestPaint::gradient(kind, a, b, spread, &super::generator::SPECTRUM)
}

#[test]
fn a_shape_is_filled_and_stroked_on_its_outline_and_transparent_outside() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 64).expect("size");
    let at = |pixels: &[u8], x: u32, y: u32| pixel(pixels, 64, x, y);
    let green = solid(0.0, 1.0, 0.0, 1.0);
    let white = solid(1.0, 1.0, 1.0, 1.0);
    // A 32-pixel square from 16 to 48, with a 4-pixel stroke centred on the outline (pixels 14
    // to 17 on the left edge) listed first, so on top of the fill.
    let square = || rectangle(0.5, 0.5, 0.5, 0.5);
    for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
        let pixels = draw(
            &renderer,
            size,
            &[
                square(),
                stroke(white.clone(), 4.0 / 64.0),
                fill(green.clone()),
            ],
            space,
        );
        assert_eq!(at(&pixels, 32, 32), [0, 255, 0, 255], "inside");
        assert_eq!(at(&pixels, 16, 32), [255, 255, 255, 255], "on the outline");
        assert_eq!(
            at(&pixels, 14, 32),
            [255, 255, 255, 255],
            "outer half of the stroke"
        );
        assert_eq!(at(&pixels, 12, 32), [0, 0, 0, 0], "outside");
        assert_eq!(at(&pixels, 20, 32), [0, 255, 0, 255], "inside the stroke");
        let ring = draw(
            &renderer,
            size,
            &[square(), stroke(white.clone(), 4.0 / 64.0)],
            space,
        );
        assert_eq!(at(&ring, 32, 32), [0, 0, 0, 0]);
        assert_eq!(at(&ring, 16, 32), [255, 255, 255, 255]);
    }
    // An ellipse turned a quarter turn: 48 wide becomes 48 tall.
    let ellipse = EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
        center: Point::new(0.5, 0.5),
        size: Vec2::new(0.75, 0.25),
        rotation_degrees: 90.0,
    });
    let pixels = draw(
        &renderer,
        size,
        &[ellipse, fill(green)],
        BlendSpace::LinearLight,
    );
    assert_eq!(at(&pixels, 32, 12)[3], 255, "the long axis is vertical");
    assert_eq!(at(&pixels, 12, 32)[3], 0, "the short axis is horizontal");
}

#[test]
fn a_fill_over_the_canvas_paints_what_the_generator_paints() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(crate::generator_definition()))
        .expect("the generator compiles");
    let size = RenderSize::new(96, 48).expect("size");
    for (kind, a, b, spread) in [
        (2, [0.2, 0.2], [0.6, 0.7], 2),
        (3, [0.4, 0.6], [0.25, 0.0], 1),
        (4, [0.5, 0.5], [30.0, 0.0], 0),
    ] {
        let spread_value = [Spread::Pad, Spread::Repeat, Spread::Reflect][spread as usize];
        let reference =
            super::generator::generated(&renderer, size, &test_paint(kind, a, b, spread));
        for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
            let drawn = draw(
                &renderer,
                size,
                &[
                    rectangle(0.5, 0.5, 1.0, 1.0),
                    fill(gradient(kind, a, b, spread_value)),
                ],
                space,
            );
            let worst = drawn
                .iter()
                .zip(&reference)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(worst <= 2, "kind {kind} in {space:?}: {worst}");
        }
    }
}

#[test]
fn a_rotated_group_turns_its_gradient_with_it() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 64).expect("size");
    let linear = |turned: f64| {
        let items = vec![
            rectangle(0.0, 0.0, 2.0, 2.0),
            fill(gradient(2, [-0.25, 0.0], [0.25, 0.0], Spread::Pad)),
        ];
        draw(
            &renderer,
            size,
            &[group(
                items,
                EvaluatedTransform {
                    position: Point::new(0.5, 0.5),
                    rotation_degrees: turned,
                    ..EvaluatedTransform::default()
                },
            )],
            BlendSpace::SrgbEncoded,
        )
    };
    let (flat, turned) = (linear(0.0), linear(90.0));
    // A quarter turn clockwise maps a pixel left of centre to one above it.
    for offset in [4, 9, 14] {
        assert_eq!(
            pixel(&flat, 64, 32 - offset, 32),
            pixel(&turned, 64, 31, 32 - offset),
            "offset {offset}"
        );
    }
}

#[test]
fn a_faded_group_blends_overlapping_items_once() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 32).expect("size");
    // Two overlapping opaque squares in a half-opaque group: the overlap is as opaque as
    // either alone, not more.
    let items = vec![
        rectangle(0.4, 0.5, 0.4, 0.5),
        fill(solid(1.0, 0.0, 0.0, 1.0)),
        rectangle(0.6, 0.5, 0.4, 0.5),
        fill(solid(0.0, 0.0, 1.0, 1.0)),
    ];
    for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
        let faded = draw(
            &renderer,
            size,
            &[group(
                items.clone(),
                EvaluatedTransform {
                    opacity: 0.5,
                    ..EvaluatedTransform::default()
                },
            )],
            space,
        );
        let (left, overlap, right) = (
            pixel(&faded, 64, 18, 16),
            pixel(&faded, 64, 32, 16),
            pixel(&faded, 64, 46, 16),
        );
        for (name, value) in [("left", left), ("overlap", overlap), ("right", right)] {
            assert!(value[3].abs_diff(128) <= 1, "{space:?} {name}: {value:?}");
        }
        // The first fill is on top in the overlap, so it is red there.
        assert!(overlap[0] > 100 && overlap[2] < 4, "{space:?}: {overlap:?}");
    }
}

#[test]
fn linear_light_and_encoded_blending_differ_only_where_colors_mix() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(32, 32).expect("size");
    // Half-transparent white over opaque black: linear light mixes to a lighter grey.
    let items = [
        rectangle(0.5, 0.5, 1.0, 1.0),
        fill(solid(1.0, 1.0, 1.0, 0.5)),
        rectangle(0.5, 0.5, 1.0, 1.0),
        fill(solid(0.0, 0.0, 0.0, 1.0)),
    ];
    let linear = pixel(
        &draw(&renderer, size, &items, BlendSpace::LinearLight),
        32,
        16,
        16,
    );
    let encoded = pixel(
        &draw(&renderer, size, &items, BlendSpace::SrgbEncoded),
        32,
        16,
        16,
    );
    // An 8-bit intermediate blends in encoded space even for a linear-light graph.
    let mixed = if renderer.vector_linear(BlendSpace::LinearLight) {
        188
    } else {
        128
    };
    assert!(linear[0].abs_diff(mixed) <= 1, "{linear:?}");
    assert!(encoded[0].abs_diff(128) <= 1, "{encoded:?}");
    assert_eq!((linear[3], encoded[3]), (255, 255));
    // Where nothing mixes, both store the same.
    let alone = [
        rectangle(0.5, 0.5, 1.0, 1.0),
        fill(solid(0.2, 0.6, 0.9, 1.0)),
    ];
    let a = draw(&renderer, size, &alone, BlendSpace::LinearLight);
    let b = draw(&renderer, size, &alone, BlendSpace::SrgbEncoded);
    assert!(a.iter().zip(&b).all(|(a, b)| a.abs_diff(*b) <= 1));
}

#[test]
fn the_eight_bit_fallback_blends_in_encoded_space() {
    let _lock = gpu_test_lock();
    let [_, eight_bit] = gpus_by_intermediate();
    let renderer = Renderer::new(eight_bit);
    assert!(!renderer.vector_linear(BlendSpace::LinearLight));
    let size = RenderSize::new(32, 32).expect("size");
    let items = [
        rectangle(0.5, 0.5, 1.0, 1.0),
        fill(solid(1.0, 1.0, 1.0, 0.5)),
        rectangle(0.5, 0.5, 1.0, 1.0),
        fill(solid(0.0, 0.0, 0.0, 1.0)),
    ];
    let value = pixel(
        &draw(&renderer, size, &items, BlendSpace::LinearLight),
        32,
        16,
        16,
    );
    assert!(value[0].abs_diff(128) <= 1, "{value:?}");
}

#[test]
fn partial_tiles_past_one_atlas_page_draw_from_the_next() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(768, 768).expect("size");
    // A circle whose edge crosses many tiles, drawn with pages of one tile row (128 slots) and
    // with the default pages: the same picture.
    let items = [
        EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
            center: Point::new(0.5, 0.5),
            size: Vec2::new(0.9, 0.9),
            rotation_degrees: 0.0,
        }),
        fill(solid(0.3, 0.9, 0.4, 1.0)),
        EvaluatedItem::Geometry(EvaluatedGeometry::Polystar {
            kind: alpha2_model::PolystarKind::Star,
            center: Point::new(0.5, 0.5),
            points: 40.0,
            outer_radius: 0.45,
            inner_radius: 0.2,
            outer_roundness: 0.0,
            inner_roundness: 0.0,
            rotation_degrees: 0.0,
        }),
        fill(solid(0.9, 0.2, 0.4, 1.0)),
    ];
    let default = draw(&renderer, size, &items, BlendSpace::LinearLight);
    renderer
        .vector_page_rows
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let paged = draw(&renderer, size, &items, BlendSpace::LinearLight);
    renderer
        .vector_page_rows
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let scene = build_scene(&items, CanvasSize::new(768, 768));
    let partial: usize = scene
        .ops
        .iter()
        .filter_map(|op| match op {
            alpha2_geometry::VectorOp::Fill {
                path, rule, bounds, ..
            } => Some(
                alpha2_geometry::raster::rasterize(
                    path,
                    *rule,
                    *bounds,
                    &mut alpha2_geometry::raster::RasterScratch::default(),
                )
                .partial
                .len(),
            ),
            _ => None,
        })
        .sum();
    assert!(
        partial > 2 * 128,
        "{partial} partial tiles fill more than two pages"
    );
    assert_eq!(default, paged);
}

#[test]
fn an_unchanged_scene_is_a_cache_hit_and_a_changed_one_is_not() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    renderer.set_result_cache_budget(64 * 1024 * 1024);
    let size = RenderSize::new(32, 32).expect("size");
    let render = |center: f64| {
        let mut graph = RenderGraph::new(size);
        let scene = build_scene(
            &[
                rectangle(center, 0.5, 0.5, 0.5),
                fill(solid(1.0, 0.0, 0.0, 1.0)),
            ],
            CanvasSize::new(32, 32),
        );
        let node = graph.add_vector(Arc::new(scene));
        let key = graph.cache_keys[node.index];
        graph
            .set_cache_intent(node, crate::CacheIntent::Auto)
            .expect("known node");
        let output = graph
            .add_composite(vec![GraphLayer {
                source: node,
                visual: visual([1.0, 1.0], 1.0),
                blend_mode: BlendMode::Normal,
            }])
            .expect("an earlier node");
        graph.set_output(output).expect("known output");
        renderer.execute_graph(&graph).expect("renders");
        key
    };
    let first = render(0.5);
    let hits = renderer.result_cache_stats().hits;
    assert_eq!(render(0.5), first);
    assert_eq!(renderer.result_cache_stats().hits, hits + 1);
    assert_ne!(render(0.6), first);
    assert_eq!(renderer.result_cache_stats().hits, hits + 1);
}

#[test]
fn coverage_of_an_unchanged_path_is_kept_between_frames() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(32, 32).expect("size");
    let items = [
        rectangle(0.5, 0.5, 0.5, 0.5),
        fill(solid(1.0, 0.0, 0.0, 1.0)),
    ];
    let counts = || {
        renderer
            .vector_state
            .lock()
            .expect("unpoisoned")
            .cache
            .hit_counts()
    };
    draw(&renderer, size, &items, BlendSpace::LinearLight);
    let (hits, misses) = counts();
    draw(&renderer, size, &items, BlendSpace::LinearLight);
    assert_eq!(counts(), (hits + 1, misses));
    renderer.release_idle_memory();
    assert!(
        renderer
            .vector_state
            .lock()
            .expect("unpoisoned")
            .cache
            .is_empty()
    );
}

#[test]
fn many_fills_each_take_their_own_uniform_slice() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let size = RenderSize::new(64, 8).expect("size");
    // Sixteen 2-pixel columns 4 pixels apart, each its own color: each fill reads its own slice.
    let items: Vec<EvaluatedItem> = (0..16_u8)
        .flat_map(|index| {
            let x = (f64::from(index) * 4.0 + 2.0) / 64.0;
            [
                rectangle(x, 0.5, 2.0 / 64.0, 1.0),
                fill(solid(f32::from(index) / 15.0, 0.0, 1.0, 1.0)),
            ]
        })
        .collect();
    let pixels = draw(&renderer, size, &items, BlendSpace::SrgbEncoded);
    for index in 0..16_u32 {
        let red = pixel(&pixels, 64, index * 4 + 2, 4)[0];
        assert_eq!(u32::from(red), (index * 255 + 7) / 15, "column {index}");
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_vector_picture_is_the_same_at_any_resolution() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let items = [
        EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
            center: Point::new(0.5, 0.5),
            size: Vec2::new(0.6, 0.5),
            corner_radius: 0.1,
            rotation_degrees: 0.0,
        }),
        stroke(solid(1.0, 1.0, 1.0, 1.0), 0.05),
        fill(gradient(3, [0.4, 0.5], [0.6, 0.0], Spread::Pad)),
    ];
    let render = |edge: u32| {
        let size = RenderSize::new(edge * 2, edge).expect("size");
        draw(&renderer, size, &items, BlendSpace::LinearLight)
    };
    let (proxy, full) = (render(24), render(96));
    let mut compared = 0;
    for y in 0..24_usize {
        for x in 0..48_usize {
            let full_pixel =
                |fx: usize, fy: usize| &full[(fy * 192 + fx) * 4..(fy * 192 + fx) * 4 + 4];
            let block = (0..16)
                .map(|index| full_pixel(x * 4 + index % 4, y * 4 + index / 4))
                .collect::<Vec<_>>();
            let uniform = block.iter().all(|pixel| {
                (0..4).all(|channel| pixel[channel].abs_diff(block[0][channel]) <= 24)
            });
            if !uniform {
                continue;
            }
            let pixel = &proxy[(y * 48 + x) * 4..(y * 48 + x) * 4 + 4];
            for channel in 0..4 {
                let mean = [(1, 1), (2, 1), (1, 2), (2, 2)]
                    .iter()
                    .map(|(dx, dy)| f64::from(full_pixel(x * 4 + dx, y * 4 + dy)[channel]))
                    .sum::<f64>()
                    / 4.0;
                assert!(
                    (f64::from(pixel[channel]) - mean).abs() <= 4.0,
                    "at ({x}, {y}): proxy {pixel:?}, full mean channel {channel} {mean}"
                );
            }
            compared += 1;
        }
    }
    assert!(compared > 600, "only {compared} pixels compared");
}

#[test]
fn a_shape_shrinking_to_nothing_fades_out_with_its_area() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(33, 33).expect("size");
    let white = solid(1.0, 1.0, 1.0, 1.0);
    let total = |items: &[EvaluatedItem]| {
        draw(&renderer, size, items, BlendSpace::LinearLight)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| u32::from(pixel[3]))
            .sum::<u32>()
    };
    let pixels = [0.0, 0.25, 0.5, 1.0];
    let filled = pixels.map(|extent| {
        total(&[
            rectangle(0.5, 0.5, extent / 33.0, extent / 33.0),
            fill(white.clone()),
        ])
    });
    assert_eq!(filled[0], 0);
    assert!(
        filled.windows(2).all(|pair| pair[0] < pair[1]),
        "{filled:?}"
    );
    // About the area: a quarter-pixel square is a sixteenth of a pixel.
    assert!(filled[1].abs_diff(16) <= 2, "{filled:?}");
    let strokes = [0.001, 0.25, 0.5, 1.0].map(|width| {
        total(&[
            rectangle(0.5, 0.5, 0.5, 0.5),
            stroke(white.clone(), width / 33.0),
        ])
    });
    assert!(strokes[0] < 33 * 4, "a 0.001 px stroke: {strokes:?}");
    assert!(
        strokes.windows(2).all(|pair| pair[0] < pair[1]),
        "{strokes:?}"
    );
}

#[test]
fn vector_pictures_match_their_golden() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let tile = RenderSize::new(48, 48).expect("size");
    let white = solid(1.0, 1.0, 1.0, 1.0);
    let tiles = [
        draw(
            &renderer,
            tile,
            &[
                EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
                    center: Point::new(0.5, 0.5),
                    size: Vec2::new(0.7, 0.5),
                    corner_radius: 0.12,
                    rotation_degrees: 15.0,
                }),
                stroke(white.clone(), 0.06),
                fill(gradient(2, [0.2, 0.5], [0.8, 0.5], Spread::Pad)),
            ],
            BlendSpace::LinearLight,
        ),
        draw(
            &renderer,
            tile,
            &[
                EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                    center: Point::new(0.5, 0.5),
                    size: Vec2::new(0.8, 0.45),
                    rotation_degrees: -30.0,
                }),
                stroke(gradient(4, [0.5, 0.5], [0.0, 0.0], Spread::Pad), 0.08),
            ],
            BlendSpace::LinearLight,
        ),
        draw(
            &renderer,
            tile,
            &[
                EvaluatedItem::Geometry(EvaluatedGeometry::Polystar {
                    kind: alpha2_model::PolystarKind::Star,
                    center: Point::new(0.5, 0.5),
                    points: 5.0,
                    outer_radius: 0.45,
                    inner_radius: 0.2,
                    outer_roundness: 0.3,
                    inner_roundness: 0.0,
                    rotation_degrees: 0.0,
                }),
                EvaluatedItem::Fill {
                    paint: gradient(3, [0.5, 0.5], [0.4, 0.0], Spread::Repeat),
                    opacity: 0.8,
                    rule: FillRule::Evenodd,
                },
            ],
            BlendSpace::LinearLight,
        ),
        draw(
            &renderer,
            tile,
            &[group(
                vec![
                    rectangle(0.4, 0.45, 0.5, 0.5),
                    fill(solid(0.9, 0.2, 0.2, 1.0)),
                    rectangle(0.6, 0.55, 0.5, 0.5),
                    fill(solid(0.2, 0.3, 0.9, 1.0)),
                ],
                EvaluatedTransform {
                    opacity: 0.6,
                    ..EvaluatedTransform::default()
                },
            )],
            BlendSpace::SrgbEncoded,
        ),
    ];
    let size = RenderSize::new(48 * 4, 48).expect("size");
    let mut pixels = Vec::with_capacity(48 * 4 * 48 * 4);
    for y in 0..48 {
        for tile in &tiles {
            pixels.extend_from_slice(&tile[y * 48 * 4..(y + 1) * 48 * 4]);
        }
    }
    // An 8-bit intermediate blends where colors mix in encoded space: its own reviewed picture.
    let name = if renderer.vector_linear(BlendSpace::LinearLight) {
        "vector_shapes"
    } else {
        "vector_shapes_rgba8"
    };
    assert_rgba8_golden(name, size, &pixels, 2);
}
