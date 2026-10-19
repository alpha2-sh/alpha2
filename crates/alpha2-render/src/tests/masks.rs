//! Shape masks and luma mattes.

use super::*;

/// Parameters of a shape-mask pass: `(center, size, rotation_degrees, radius,
/// feather, ellipse, invert, mode)` per mask.
type MaskSpec = ([f32; 2], [f32; 2], f32, f32, f32, bool, bool, u32);

fn mask_params(masks: &[MaskSpec]) -> EffectParams<'static> {
    #[allow(clippy::cast_precision_loss)]
    let mut params = EffectParams::new().with("count", ParamValue::Float(masks.len() as f32));
    for (index, &(center, size, rotation, radius, feather, ellipse, invert, mode)) in
        masks.iter().enumerate()
    {
        let name = |field: &str| format!("m{index}_{field}");
        params = params
            .with(name("center"), ParamValue::Vec2(center))
            .with(name("size"), ParamValue::Vec2(size))
            .with(name("rotation"), ParamValue::Float(rotation.to_radians()))
            .with(name("radius"), ParamValue::Float(radius))
            .with(name("feather"), ParamValue::Float(feather))
            .with(name("ellipse"), ParamValue::Bool(ellipse))
            .with(name("invert"), ParamValue::Bool(invert))
            .with(name("mode"), ParamValue::Enum(mode));
    }
    params
}

/// White through the shape-mask pass at `size`; returns each pixel's alpha.
fn masked_alpha(renderer: &Renderer, size: RenderSize, masks: &[MaskSpec]) -> Vec<u8> {
    let white = rgba_image_texture(
        renderer,
        size,
        &vec![255; (size.width * size.height * 4) as usize],
    );
    let view = white.create_view(&wgpu::TextureViewDescriptor::default());
    let output = renderer
        .render_effect(
            &crate::EffectId::new(crate::SHAPE_MASK_EFFECT_ID.to_owned(), 1),
            &view,
            None,
            size,
            &mask_params(masks),
            EffectRuntime::default(),
        )
        .expect("the mask renders");
    read_rgba8(renderer, &output)
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| pixel[3])
        .collect()
}

#[test]
fn shape_masks_cut_shapes_combine_in_order_and_match_their_golden() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(crate::shape_mask_definition()))
        .expect("the mask compiles");
    let size = RenderSize::new(64, 64).expect("size");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let at = |alpha: &[u8], x: u32, y: u32| alpha[(y * 64 + x) as usize];
    let ellipse: MaskSpec = ([0.5, 0.5], [0.5, 0.5], 0.0, 0.0, 0.0, true, false, 0);
    let alpha = masked_alpha(&renderer, size, &[ellipse]);
    assert_eq!(at(&alpha, 32, 32), 255, "inside the ellipse");
    assert_eq!(at(&alpha, 2, 2), 0, "outside it");
    // The 16 px horizontal radius puts the edge between pixels 47 and 48 (the ellipse distance
    // is approximate within a fraction of a pixel).
    assert!(at(&alpha, 47, 32) >= 250 && at(&alpha, 48, 32) <= 5);

    let inverted = masked_alpha(
        &renderer,
        size,
        &[(ellipse.0, ellipse.1, 0.0, 0.0, 0.0, true, true, 0)],
    );
    assert_eq!((at(&inverted, 32, 32), at(&inverted, 2, 2)), (0, 255));

    // A subtracting first mask starts from full coverage.
    let hole = masked_alpha(
        &renderer,
        size,
        &[(ellipse.0, ellipse.1, 0.0, 0.0, 0.0, true, false, 1)],
    );
    assert_eq!((at(&hole, 32, 32), at(&hole, 2, 2)), (0, 255));

    // Two rectangles added, then the ellipse intersected.
    let left: MaskSpec = ([0.25, 0.5], [0.5, 1.0], 0.0, 0.0, 0.0, false, false, 0);
    let top: MaskSpec = ([0.5, 0.25], [1.0, 0.5], 0.0, 0.0, 0.0, false, false, 0);
    let combined = masked_alpha(
        &renderer,
        size,
        &[
            left,
            top,
            (ellipse.0, ellipse.1, 0.0, 0.0, 0.0, true, false, 2),
        ],
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

    // A rotated, rounded, feathered rectangle in every mode; the golden holds the picture.
    let rotated: MaskSpec = ([0.5, 0.5], [0.6, 0.3], 30.0, 0.05, 0.1, false, false, 0);
    let golden = [
        masked_alpha(&renderer, size, &[rotated]),
        combined,
        inverted,
        hole,
    ]
    .concat()
    .into_iter()
    .flat_map(|alpha| [alpha, alpha, alpha, 255])
    .collect::<Vec<_>>();
    assert_rgba8_golden(
        "shape_masks",
        RenderSize::new(64, 256).expect("size"),
        &golden,
        1,
    );
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_mask_shrinking_to_nothing_leaves_no_dot_and_its_inverse_no_pinhole() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(crate::shape_mask_definition()))
        .expect("the mask compiles");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    // 33 pixels, so the centre is pixel 16's centre.
    let size = RenderSize::new(33, 33).expect("size");
    let centre = 16 * 33 + 16;
    let mask = |pixels: f32, feather: f32, ellipse: bool, invert: bool| -> MaskSpec {
        (
            [0.5, 0.5],
            [pixels / 33.0; 2],
            0.0,
            0.0,
            feather,
            ellipse,
            invert,
            0,
        )
    };
    for ellipse in [false, true] {
        for feather in [0.0, 0.2] {
            let empty = masked_alpha(&renderer, size, &[mask(0.0, feather, ellipse, false)]);
            assert!(
                empty.iter().all(|&alpha| alpha == 0),
                "ellipse {ellipse}, feather {feather}: centre {}",
                empty[centre]
            );
            let full = masked_alpha(&renderer, size, &[mask(0.0, feather, ellipse, true)]);
            assert!(
                full.iter().all(|&alpha| alpha == 255),
                "inverted, ellipse {ellipse}, feather {feather}: centre {}",
                full[centre]
            );
            // Coverage grows with the size from there, continuously, while the mask is
            // narrower than its edge: a pixel, or the feather (6.6 px here).
            let scale = if feather > 0.0 { 6.0 } else { 1.0 };
            let totals = [0.0, 0.25, 0.5, 1.0].map(|pixels| {
                let pixels = pixels * scale;
                masked_alpha(&renderer, size, &[mask(pixels, feather, ellipse, false)])
                    .iter()
                    .map(|&alpha| u32::from(alpha))
                    .sum::<u32>()
            });
            assert!(
                totals.windows(2).all(|pair| pair[0] < pair[1]),
                "ellipse {ellipse}, feather {feather}: {totals:?}"
            );
        }
        let quarter = masked_alpha(&renderer, size, &[mask(0.25, 0.0, ellipse, false)]);
        assert!(quarter[centre] <= 32, "a 0.25 px mask: {}", quarter[centre]);
    }
    assert!(pollster::block_on(scope.pop()).is_none());
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
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(crate::shape_mask_definition()))
        .expect("the mask compiles");
    let feathered: MaskSpec = ([0.5, 0.5], [0.5, 0.5], 0.0, 0.0, 0.25, false, false, 0);
    let row = |edge: u32| {
        let size = RenderSize::new(edge, edge).expect("size");
        let alpha = masked_alpha(&renderer, size, &[feathered]);
        let y = edge / 2;
        (0..edge)
            .map(|x| f32::from(alpha[(y * edge + x) as usize]) / 255.0)
            .collect::<Vec<_>>()
    };
    let (full, proxy) = (row(128), row(32));
    // Each proxy pixel centre against the full-resolution pixels at the same normalized
    // position.
    for (x, value) in proxy.iter().enumerate() {
        let full_value = f32::midpoint(full[x * 4 + 1], full[x * 4 + 2]);
        assert!(
            (value - full_value).abs() < 0.04,
            "at {x}/32: proxy {value}, full {full_value}"
        );
    }
    // The ramp is a quarter of the picture wide: from 0.125 to 0.375.
    assert!(full[8] < 0.02 && full[56] > 0.98, "{:?}", &full[..64]);
}
