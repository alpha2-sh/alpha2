use super::*;

use crate::builtins::{DIRECTIONAL_BLUR_ID, GAUSSIAN_BLUR_ID, GLOW_ID};

/// Opaque black with a white vertical line `width` pixels wide, centred.
fn line_image(size: RenderSize, width: u32) -> Vec<u8> {
    let left = (size.width - width) / 2;
    (0..size.height)
        .flat_map(|_| {
            (0..size.width).flat_map(move |x| {
                if (left..left + width).contains(&x) {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
        })
        .collect()
}

/// The red channel of the middle row.
fn middle_row(pixels: &[u8], size: RenderSize) -> Vec<u8> {
    let width = size.width as usize;
    let row = size.height as usize / 2;
    pixels[row * width * 4..(row + 1) * width * 4]
        .iter()
        .step_by(4)
        .copied()
        .collect()
}

fn render_project_effect(
    renderer: &Renderer,
    id: &str,
    size: RenderSize,
    pixels: &[u8],
    params: &EffectParams,
) -> Vec<u8> {
    let texture = rgba_image_texture(renderer, size, pixels);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let target = renderer
        .render_effect(
            &crate::EffectId::new(id.into(), 1),
            &view,
            None,
            size,
            params,
            EffectRuntime {
                #[allow(clippy::cast_precision_loss)]
                ratio: size.width as f32 / size.height as f32,
                ..EffectRuntime::default()
            },
        )
        .expect("the effect renders");
    read_rgba8(renderer, &target)
}

/// From the line's right edge outward, the light falls and never comes back:
/// each pixel within `reach` is lit, and none is brighter than the one before.
fn assert_falls_off(row: &[u8], from: usize, reach: usize, what: &str) {
    for x in from..from + reach {
        assert!(
            row[x + 1] <= row[x].saturating_add(1),
            "{what}: x={} is brighter than x={x}: {:?}",
            x + 1,
            &row[from..=from + reach]
        );
        assert!(
            row[x] > 0,
            "{what}: x={x}, {} px out, is dark: {:?}",
            x - from,
            &row[from..=from + reach]
        );
    }
}

#[test]
fn the_project_blur_is_one_gaussian_without_gaps_between_taps() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    // sigma = radius / 2 * 64 = 8 px: the 2D body's taps fall 8 px apart.
    let size = RenderSize::new(256, 64).expect("non-zero size");
    let pixels = line_image(size, 4);
    let blurred = render_project_effect(
        &renderer,
        GAUSSIAN_BLUR_ID,
        size,
        &pixels,
        &EffectParams::new().with("radius", ParamValue::Float(0.25)),
    );
    let row = middle_row(&blurred, size);
    // The line is x = 126..130; 2 sigma out is still lit.
    assert_falls_off(&row, 129, 16, "gaussian blur");
    let symmetric = (0..16).all(|d| row[129 + d].abs_diff(row[126 - d]) <= 1);
    assert!(symmetric, "the blur is lopsided: {row:?}");
    assert_rgba8_golden("gaussian_blur_project", size, &blurred, 2);

    // A project's effect and the direct call are one implementation.
    let texture = rgba_image_texture(&renderer, size, &pixels);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let direct = renderer
        .render_gaussian_blur(&view, size, 0.25)
        .expect("normalized radius is valid");
    assert_eq!(read_rgba8(&renderer, &direct), blurred);
}

#[test]
fn a_wide_project_blur_stays_smooth_at_any_size() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    // sigma = 0.05 / 2 * 1080 = 27 px, as a 1080p project with radius 0.05.
    let size = RenderSize::new(1280, 1080).expect("non-zero size");
    let pixels = line_image(size, 32);
    let blurred = render_project_effect(
        &renderer,
        GAUSSIAN_BLUR_ID,
        size,
        &pixels,
        &EffectParams::new().with("radius", ParamValue::Float(0.05)),
    );
    let row = middle_row(&blurred, size);
    assert_falls_off(&row, 655, 54, "wide gaussian blur");
}

/// Opaque black with a white `rect` (width, height) centred.
fn rect_image(size: RenderSize, rect: (u32, u32)) -> Vec<u8> {
    let (left, top) = ((size.width - rect.0) / 2, (size.height - rect.1) / 2);
    (0..size.height)
        .flat_map(|y| {
            (0..size.width).flat_map(move |x| {
                if (left..left + rect.0).contains(&x) && (top..top + rect.1).contains(&y) {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
        })
        .collect()
}

#[test]
fn portrait_blur_matches_landscape_transposed() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    // The radius is a fraction of the shorter side, 64 px both ways: sigma 3.2 px.
    let (portrait, landscape) = (
        RenderSize::new(64, 128).expect("non-zero size"),
        RenderSize::new(128, 64).expect("non-zero size"),
    );
    let blur = |size, rect| {
        render_project_effect(
            &renderer,
            GAUSSIAN_BLUR_ID,
            size,
            &rect_image(size, rect),
            &EffectParams::new().with("radius", ParamValue::Float(0.1)),
        )
    };
    let tall = blur(portrait, (16, 32));
    let wide = blur(landscape, (32, 16));
    let mut lit = 0;
    for y in 0..128_usize {
        for x in 0..64_usize {
            let (a, b) = (
                &tall[(y * 64 + x) * 4..][..4],
                &wide[(x * 128 + y) * 4..][..4],
            );
            for channel in 0..4 {
                assert!(
                    a[channel].abs_diff(b[channel]) <= 1,
                    "({x}, {y}): portrait {a:?}, landscape transposed {b:?}"
                );
            }
            lit += usize::from(a[0] > 0);
        }
    }
    // The blur reaches well past the rect's 512 pixels.
    assert!(lit > 1_000, "{lit} lit pixels");
}

#[test]
fn glow_falls_off_smoothly_from_a_bright_line() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    // radius 0.06 * 256 = 15 px: the ring of 12 taps sits 15 px out.
    let size = RenderSize::new(256, 256).expect("non-zero size");
    let pixels = line_image(size, 4);
    let glowing = render_project_effect(
        &renderer,
        GLOW_ID,
        size,
        &pixels,
        &EffectParams::new()
            .with("threshold", ParamValue::Float(0.5))
            .with("intensity", ParamValue::Float(3.0))
            .with("radius", ParamValue::Float(0.06)),
    );
    let row = middle_row(&glowing, size);
    assert_eq!(row[127], 255, "the line itself stays white");
    assert_falls_off(&row, 129, 12, "glow");
    // GL on the 8-bit intermediate is up to 3 levels off every other backend: it gets its own
    // reviewed variant rather than a looser tolerance for all.
    assert_rgba8_golden_tiles(&["glow", "glow_gl_rgba8"], size, &["glow"], &glowing, 2);
}

/// Opaque black with a white rectangle over columns `x` and rows `y`.
fn rectangle_image(size: RenderSize, x: std::ops::Range<u32>, y: std::ops::Range<u32>) -> Vec<u8> {
    (0..size.height)
        .flat_map(|row| {
            let inside_row = y.contains(&row);
            let x = x.clone();
            (0..size.width).flat_map(move |column| {
                if inside_row && x.contains(&column) {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
        })
        .collect()
}

/// The red channel at `(x, y)`.
fn red_at(pixels: &[u8], size: RenderSize, x: u32, y: u32) -> u8 {
    pixels[((y * size.width + x) * 4) as usize]
}

#[test]
fn a_directional_blur_at_zero_degrees_spreads_along_x_only() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    // sigma = length / 2 * 64 = 8 px.
    let size = RenderSize::new(256, 64).expect("non-zero size");
    let params = EffectParams::new()
        .with("length", ParamValue::Float(0.25))
        .with("angle", ParamValue::Float(0.0));
    let vertical = render_project_effect(
        &renderer,
        DIRECTIONAL_BLUR_ID,
        size,
        &line_image(size, 4),
        &params,
    );
    let row = middle_row(&vertical, size);
    assert_falls_off(&row, 129, 16, "directional blur");
    let symmetric = (0..16).all(|d| row[129 + d].abs_diff(row[126 - d]) <= 1);
    assert!(symmetric, "the blur is lopsided: {row:?}");
    // A horizontal line stays as sharp as it was across its rows.
    let horizontal = rectangle_image(size, 0..size.width, 30..34);
    let blurred = render_project_effect(&renderer, DIRECTIONAL_BLUR_ID, size, &horizontal, &params);
    for x in [0, 77, 128, 255] {
        for y in 0..size.height {
            assert!(
                red_at(&blurred, size, x, y).abs_diff(red_at(&horizontal, size, x, y)) <= 1,
                "({x}, {y}) moved across the line"
            );
        }
    }
}

#[test]
fn a_directional_blur_at_45_degrees_is_symmetric_about_its_line() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    // A square about the centre; sigma = 0.25 / 2 * 96 = 12 px along up-right.
    let size = RenderSize::new(96, 96).expect("non-zero size");
    let square = rectangle_image(size, 46..50, 46..50);
    let blurred = render_project_effect(
        &renderer,
        DIRECTIONAL_BLUR_ID,
        size,
        &square,
        &EffectParams::new()
            .with("length", ParamValue::Float(0.25))
            .with("angle", ParamValue::Float(45.0)),
    );
    let at = |x, y| red_at(&blurred, size, x, y);
    // Counter-clockwise: up and to the right is lit, down and to the right is not.
    assert!(at(55, 40) > 20, "along the line: {}", at(55, 40));
    assert!(at(55, 55) <= 1, "across the line: {}", at(55, 55));
    for y in 0..size.height {
        for x in 0..size.width {
            // Through the centre, and mirrored across the blur's line.
            let (turned, mirrored) = (at(95 - x, 95 - y), at(95 - y, 95 - x));
            assert!(
                at(x, y).abs_diff(turned) <= 2 && at(x, y).abs_diff(mirrored) <= 2,
                "({x}, {y}) is {} but {turned} turned and {mirrored} mirrored",
                at(x, y)
            );
        }
    }
}

#[test]
fn expanded_effects_check_their_parameters() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    let size = RenderSize::new(8, 8).expect("non-zero size");
    let texture = rgba_image_texture(&renderer, size, &line_image(size, 2));
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    for (id, params) in [
        (
            GAUSSIAN_BLUR_ID,
            EffectParams::new().with("radius", ParamValue::Float(1.5)),
        ),
        (
            GAUSSIAN_BLUR_ID,
            EffectParams::new().with("direction", ParamValue::Vec2([0.1, 0.0])),
        ),
        (
            GLOW_ID,
            EffectParams::new().with("radius", ParamValue::Vec2([0.01, 0.01])),
        ),
        (
            GLOW_ID,
            EffectParams::new().with("intensity", ParamValue::Float(f32::NAN)),
        ),
    ] {
        let result = renderer.render_effect(
            &crate::EffectId::new(id.into(), 1),
            &view,
            None,
            size,
            &params,
            EffectRuntime::default(),
        );
        assert!(
            matches!(result, Err(RenderError::InvalidEffectParams)),
            "{id} {params:?}: {result:?}"
        );
    }
}

/// Rows (top first) where `to` (blue) shows in column `x` of a transition
/// from red to blue.
fn blue_rows(pixels: &[u8], size: RenderSize, x: u32) -> Vec<u32> {
    (0..size.height)
        .filter(|&y| {
            let index = ((y * size.width + x) * 4) as usize;
            pixels[index + 2] > pixels[index]
        })
        .collect()
}

fn render_transition(renderer: &Renderer, id: &str, size: RenderSize, progress: f32) -> Vec<u8> {
    let red = rgba_image_texture(
        renderer,
        size,
        &[255, 0, 0, 255].repeat((size.width * size.height) as usize),
    );
    let blue = rgba_image_texture(
        renderer,
        size,
        &[0, 0, 255, 255].repeat((size.width * size.height) as usize),
    );
    let red_view = red.create_view(&wgpu::TextureViewDescriptor::default());
    let blue_view = blue.create_view(&wgpu::TextureViewDescriptor::default());
    let target = renderer
        .render_effect(
            &crate::EffectId::new(id.into(), 1),
            &red_view,
            Some(&blue_view),
            size,
            &EffectParams::new(),
            EffectRuntime {
                progress,
                ratio: 1.0,
                ..EffectRuntime::default()
            },
        )
        .expect("the transition renders");
    read_rgba8(renderer, &target)
}

#[test]
fn ported_transitions_keep_their_upward_direction() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer
        .register_builtin_transitions()
        .expect("transitions compile");
    let size = RenderSize::new(64, 64).expect("non-zero size");

    // A quarter of the way, wipe-up shows the next clip in the bottom quarter only.
    let up = blue_rows(
        &render_transition(&renderer, "alpha2.transition.wipe-up", size, 0.25),
        size,
        32,
    );
    assert_eq!(up, (48..64).collect::<Vec<_>>(), "wipe-up");
    let down = blue_rows(
        &render_transition(&renderer, "alpha2.transition.wipe-down", size, 0.25),
        size,
        32,
    );
    assert_eq!(down, (0..16).collect::<Vec<_>>(), "wipe-down");

    // An upright heart: its tip, in the centre column, is its lowest point;
    // its lobes, either side, reach higher than the dip between them.
    let heart = render_transition(&renderer, "alpha2.transition.heart", size, 0.4);
    let centre = blue_rows(&heart, size, 32);
    let lobe = blue_rows(&heart, size, 22);
    assert!(!centre.is_empty() && !lobe.is_empty(), "the heart is drawn");
    assert!(
        lobe.first() < centre.first(),
        "the lobes rise above the dip: lobe {lobe:?}, centre {centre:?}"
    );
    assert!(
        centre.last() > lobe.last(),
        "the tip points down: lobe {lobe:?}, centre {centre:?}"
    );
}

#[test]
fn a_tier_b_renderer_filters_with_the_tier_b_sample_budget_and_says_so() {
    // Tier B (WebGL2 / GL ES) clamps filters to 16 samples and reports the reduction. Tiers are
    // named, not taken from the adapter: every GL adapter is Tier B, so Tier A would compare
    // Tier B with itself.
    let _lock = gpu_test_lock();
    let renderers = |tier: CapabilityTier, budget: Option<u32>| {
        let mut renderer = Renderer::new(gpu().with_tier(tier));
        renderer.register_builtins().expect("built-ins compile");
        if let Some(budget) = budget {
            renderer.set_quality_controls(budget, 1);
        }
        renderer
    };
    let tier_b = renderers(CapabilityTier::TierB, None);
    let reduced = |requested| {
        vec![crate::CapabilityDiagnostic::FilterSamplesReduced {
            requested,
            applied: 16,
        }]
    };
    assert_eq!(tier_b.filter_sample_limit(), 16);
    // Its default budget, and an export's, are reduced and reported.
    assert_eq!(tier_b.quality_controls().max_filter_samples, 16);
    assert_eq!(tier_b.diagnostics().max_filter_samples, 16);
    assert_eq!(tier_b.diagnostics().degradations, reduced(64));
    tier_b.set_quality_controls(32, 1);
    assert_eq!(tier_b.quality_controls().max_filter_samples, 16);
    assert_eq!(tier_b.diagnostics().degradations, reduced(32));
    tier_b.set_quality_controls(8, 1);
    assert_eq!(tier_b.quality_controls().max_filter_samples, 8);
    assert_eq!(tier_b.diagnostics().degradations, Vec::new());
    tier_b.set_quality_controls(64, 1);

    // The passes take the reduced budget: a wide blur on Tier B is the one a 16-sample
    // budget draws, not the 64-sample one.
    let size = RenderSize::new(512, 1080).expect("non-zero size");
    let pixels = line_image(size, 32);
    let blur = |renderer: &Renderer| {
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let blurred = render_project_effect(
            renderer,
            GAUSSIAN_BLUR_ID,
            size,
            &pixels,
            &EffectParams::new().with("radius", ParamValue::Float(0.05)),
        );
        assert!(pollster::block_on(scope.pop()).is_none());
        blurred
    };
    let on_tier_b = blur(&tier_b);
    let (tier_a_16, tier_a_64) = (
        renderers(CapabilityTier::TierA, Some(16)),
        renderers(CapabilityTier::TierA, Some(64)),
    );
    // The comparison renderers filter with the budgets they are given, on any adapter.
    assert!(tier_a_64.filter_sample_limit() >= 64);
    assert_eq!(tier_a_64.quality_controls().max_filter_samples, 64);
    assert_eq!(tier_a_64.diagnostics().degradations, Vec::new());
    assert_eq!(tier_a_16.quality_controls().max_filter_samples, 16);
    assert_eq!(on_tier_b, blur(&tier_a_16));
    assert_ne!(on_tier_b, blur(&tier_a_64));
}
