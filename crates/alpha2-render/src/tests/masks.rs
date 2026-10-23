//! Mask coverage combined in order, feathered by a blur, and luma mattes.

use super::*;
use crate::tests::vector::{fill, rectangle, solid};
use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{CanvasSize, EvaluatedGeometry, EvaluatedItem, build_scene};

/// One mask: its outline, whether it is inverted, and its mode (0 add, 1
/// subtract, 2 intersect).
type Mask = (EvaluatedItem, bool, u32);

fn ellipse(cx: f64, cy: f64, w: f64, h: f64) -> EvaluatedItem {
    EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
        center: Point::new(cx, cy),
        size: Vec2::new(w, h),
        rotation_degrees: 0.0,
    })
}

fn mask_renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    for definition in crate::mask_combine_definitions() {
        pollster::block_on(renderer.register_effect(definition)).expect("the masks compile");
    }
    pollster::block_on(renderer.register_effect(crate::builtins::gaussian_blur_pass_definition()))
        .expect("the blur compiles");
    renderer
}

/// White kept by `masks` at `size`, each feathered by a blur of `sigma`
/// pixels; returns each pixel's alpha.
fn masked_alpha(renderer: &Renderer, size: RenderSize, masks: &[Mask], sigma: f32) -> Vec<u8> {
    let mut graph = RenderGraph::new(size);
    let white = graph.add_vector(Arc::new(build_scene(
        &[
            rectangle(0.5, 0.5, 1.0, 1.0),
            fill(solid(1.0, 1.0, 1.0, 1.0)),
        ],
        CanvasSize::new(size.width, size.height),
    )));
    let mut inputs = crate::EffectInputs::new();
    let mut params = EffectParams::new();
    for (index, (outline, invert, mode)) in masks.iter().enumerate() {
        let names = crate::MASK_COMBINE_NAMES[index];
        let coverage = graph.add_vector(Arc::new(build_scene(
            &[outline.clone(), fill(solid(1.0, 1.0, 1.0, 1.0))],
            CanvasSize::new(size.width, size.height),
        )));
        let coverage = graph
            .add_gaussian_pixels(coverage, sigma, EffectRuntime::default())
            .expect("an earlier node");
        inputs.insert(names.input, coverage);
        params.insert(names.invert, ParamValue::Bool(*invert));
        params.insert(names.mode, ParamValue::Enum(*mode));
    }
    let output = graph
        .add_effect_with_inputs(
            crate::EffectId::from_static(crate::MASK_COMBINE_EFFECT_IDS[masks.len() - 1], 1),
            white,
            inputs,
            params,
            crate::EffectResources::new(),
            EffectRuntime::default(),
        )
        .expect("earlier nodes");
    graph.set_output(output).expect("known output");
    let output = renderer.execute_graph(&graph).expect("the masks render");
    read_rgba8(renderer, &output)
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| pixel[3])
        .collect()
}

#[test]
fn masks_combine_in_order_and_match_their_golden() {
    let _lock = gpu_test_lock();
    let renderer = mask_renderer();
    let size = RenderSize::new(64, 64).expect("size");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let at = |alpha: &[u8], x: u32, y: u32| alpha[(y * 64 + x) as usize];
    let circle = || ellipse(0.5, 0.5, 0.5, 0.5);
    let alpha = masked_alpha(&renderer, size, &[(circle(), false, 0)], 0.0);
    assert_eq!(at(&alpha, 32, 32), 255, "inside the ellipse");
    assert_eq!(at(&alpha, 2, 2), 0, "outside it");
    // The 16 px horizontal radius puts the edge between pixels 47 and 48.
    assert!(at(&alpha, 46, 32) >= 250 && at(&alpha, 48, 32) <= 5);

    let inverted = masked_alpha(&renderer, size, &[(circle(), true, 0)], 0.0);
    assert_eq!((at(&inverted, 32, 32), at(&inverted, 2, 2)), (0, 255));

    // A subtracting first mask starts from full coverage.
    let hole = masked_alpha(&renderer, size, &[(circle(), false, 1)], 0.0);
    assert_eq!((at(&hole, 32, 32), at(&hole, 2, 2)), (0, 255));

    // Two rectangles added, then the ellipse intersected.
    let combined = masked_alpha(
        &renderer,
        size,
        &[
            (rectangle(0.25, 0.5, 0.5, 1.0), false, 0),
            (rectangle(0.5, 0.25, 1.0, 0.5), false, 0),
            (circle(), false, 2),
        ],
        0.0,
    );
    assert_eq!(
        at(&combined, 26, 26),
        255,
        "left, top, and inside the ellipse"
    );
    assert_eq!(at(&combined, 40, 40), 0, "in neither rectangle");
    assert_eq!(
        at(&combined, 4, 32),
        0,
        "in a rectangle, outside the ellipse"
    );

    // Eight masks, every one taking part.
    let eight: Vec<Mask> = (0..8)
        .map(|index| {
            let x = 0.0625 + 0.125 * f64::from(index);
            (rectangle(x, 0.5, 0.0625, 1.0), false, 0)
        })
        .collect();
    let stripes = masked_alpha(&renderer, size, &eight, 0.0);
    for index in 0..8 {
        assert_eq!(at(&stripes, 4 + 8 * index, 10), 255, "stripe {index}");
        assert_eq!(at(&stripes, 8 * index, 10), 0, "gap {index}");
    }

    // A rotated, rounded rectangle feathered, and the rest; the golden holds the picture.
    let rotated = EvaluatedItem::Geometry(EvaluatedGeometry::Rectangle {
        center: Point::new(0.5, 0.5),
        size: Vec2::new(0.6, 0.3),
        corner_radius: 0.05,
        rotation_degrees: 30.0,
    });
    let golden = [
        masked_alpha(&renderer, size, &[(rotated, false, 0)], 2.0),
        combined,
        inverted,
        hole,
    ]
    .concat()
    .into_iter()
    .flat_map(|alpha| [alpha, alpha, alpha, 255])
    .collect::<Vec<_>>();
    assert_rgba8_golden(
        "mask_combine",
        RenderSize::new(64, 256).expect("size"),
        &golden,
        1,
    );
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_mask_shrinking_to_nothing_leaves_no_dot_and_its_inverse_no_pinhole() {
    let _lock = gpu_test_lock();
    let renderer = mask_renderer();
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let size = RenderSize::new(33, 33).expect("size");
    for square in [false, true] {
        let outline = |pixels: f64| {
            let extent = pixels / 33.0;
            if square {
                rectangle(0.5, 0.5, extent, extent)
            } else {
                ellipse(0.5, 0.5, extent, extent)
            }
        };
        for sigma in [0.0, 2.0] {
            let empty = masked_alpha(&renderer, size, &[(outline(0.0), false, 0)], sigma);
            assert!(empty.iter().all(|&alpha| alpha == 0), "{square} {sigma}");
            let full = masked_alpha(&renderer, size, &[(outline(0.0), true, 0)], sigma);
            assert!(full.iter().all(|&alpha| alpha == 255), "{square} {sigma}");
            // Coverage grows with the area from there.
            let totals = [0.0, 0.5, 1.0, 2.0].map(|pixels| {
                masked_alpha(&renderer, size, &[(outline(pixels), false, 0)], sigma)
                    .iter()
                    .map(|&alpha| u32::from(alpha))
                    .sum::<u32>()
            });
            assert!(
                totals.windows(2).all(|pair| pair[0] < pair[1]),
                "{square} {sigma}: {totals:?}"
            );
        }
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_feather_blur_spreads_the_edge_over_its_ramp_width() {
    let _lock = gpu_test_lock();
    let renderer = mask_renderer();
    // A half-plane edge at x = 64 of 128, blurred by a deviation of 8 px: the 10 to 90 percent
    // ramp of a Gaussian edge is 2.563 deviations wide, 20.5 px.
    let size = RenderSize::new(128, 32).expect("size");
    let alpha = masked_alpha(
        &renderer,
        size,
        &[(rectangle(0.25, 0.5, 0.5, 2.0), false, 0)],
        8.0,
    );
    let row: Vec<f32> = (0..128)
        .map(|x| f32::from(alpha[16 * 128 + x]) / 255.0)
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let crossing = |level: f32| {
        (1..128).find(|x| row[*x] < level).map_or(128.0, |x| {
            let (a, b) = (row[x - 1], row[x]);
            (x - 1) as f32 + (a - level) / (a - b)
        })
    };
    let width = crossing(0.1) - crossing(0.9);
    assert!((width - 20.5).abs() < 1.5, "ramp {width} px: {row:?}");
    assert!((row[63] - 0.5).abs() < 0.08 || (row[64] - 0.5).abs() < 0.08);
}

#[test]
fn a_luma_matte_weighs_linear_light_by_bt709_and_by_its_alpha() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(crate::track_matte_definition()))
        .expect("the matte compiles");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let white = rgba_texture(&renderer, [255; 4]);
    let white_view = white.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("size");
    let id = crate::EffectId::new(crate::TRACK_MATTE_EFFECT_ID.to_owned(), 1);
    // Premultiplied mattes: the primaries, whose luminance is their BT.709 weight, and
    // white at half alpha.
    for (matte, luma, inverted) in [
        ([255, 0, 0, 255], 0.2126, false),
        ([0, 255, 0, 255], 0.7152, false),
        ([0, 0, 255, 255], 0.0722, false),
        ([128, 128, 128, 128], 128.0 / 255.0, false),
        ([0, 255, 0, 255], 1.0 - 0.7152, true),
    ] {
        let matte_texture = rgba_texture(&renderer, matte);
        let matte_view = matte_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let output = renderer
            .render_effect(
                &id,
                &white_view,
                Some(&matte_view),
                size,
                &EffectParams::new()
                    .with("luma", ParamValue::Bool(true))
                    .with("inverted", ParamValue::Bool(inverted)),
                EffectRuntime {
                    progress: 0.5,
                    ..EffectRuntime::default()
                },
            )
            .expect("the matte renders");
        let alpha = f32::from(read_rgba8(&renderer, &output)[3]);
        assert!(
            (alpha - luma * 255.0).abs() <= 1.0,
            "{matte:?}: alpha {alpha}, luminance {luma}"
        );
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_mask_feather_is_the_same_fraction_of_the_picture_at_any_resolution() {
    let _lock = gpu_test_lock();
    let renderer = mask_renderer();
    // A quarter-picture feather: sigma 0.3125 of it.
    let row = |edge: u32| {
        let size = RenderSize::new(edge, edge).expect("size");
        #[allow(clippy::cast_precision_loss)]
        let sigma = 0.3125 * 0.25 * edge as f32;
        let alpha = masked_alpha(
            &renderer,
            size,
            &[(rectangle(0.5, 0.5, 0.5, 0.5), false, 0)],
            sigma,
        );
        let y = edge / 2;
        (0..edge)
            .map(|x| f32::from(alpha[(y * edge + x) as usize]) / 255.0)
            .collect::<Vec<_>>()
    };
    let (full, proxy) = (row(128), row(32));
    for (x, value) in proxy.iter().enumerate() {
        let full_value = f32::midpoint(full[x * 4 + 1], full[x * 4 + 2]);
        assert!(
            (value - full_value).abs() < 0.04,
            "at {x}/32: proxy {value}, full {full_value}"
        );
    }
}
