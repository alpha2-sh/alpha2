use super::*;

use crate::builtins::{GAUSSIAN_BLUR_ID, GLOW_ID};

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
    pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
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
    pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
    // sigma = 0.05 / 2 * 1080 = 27 px, as a 1080p project with radius 0.05.
    let size = RenderSize::new(512, 1080).expect("non-zero size");
    let pixels = line_image(size, 32);
    let blurred = render_project_effect(
        &renderer,
        GAUSSIAN_BLUR_ID,
        size,
        &pixels,
        &EffectParams::new().with("radius", ParamValue::Float(0.05)),
    );
    let row = middle_row(&blurred, size);
    assert_falls_off(&row, 271, 54, "wide gaussian blur");
}

#[test]
fn glow_falls_off_smoothly_from_a_bright_line() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
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

#[test]
fn expanded_effects_check_their_parameters() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
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
    pollster::block_on(renderer.register_builtin_transitions()).expect("transitions compile");
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
        pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
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
