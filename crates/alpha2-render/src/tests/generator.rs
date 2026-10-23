use super::*;

/// One paint as the generator's parameters carry it.
pub(super) struct TestPaint {
    kind: u32,
    a: [f32; 2],
    b: [f32; 2],
    spread: u32,
    stops: Vec<(f32, [f32; 4])>,
}

impl TestPaint {
    pub(super) fn gradient(
        kind: u32,
        a: [f32; 2],
        b: [f32; 2],
        spread: u32,
        stops: &[(f32, [f32; 4])],
    ) -> Self {
        Self {
            kind,
            a,
            b,
            spread,
            stops: stops.to_vec(),
        }
    }
}

fn with_paint(
    mut params: EffectParams<'static>,
    prefix: &str,
    paint: &TestPaint,
) -> EffectParams<'static> {
    let name = |field: &str| format!("{prefix}_{field}");
    let mut offsets = [0.0_f32; 8];
    for (index, (offset, color)) in paint.stops.iter().enumerate() {
        offsets[index] = *offset;
        params = params.with(name(&format!("c{index}")), ParamValue::Color(*color));
    }
    #[allow(clippy::cast_precision_loss)]
    let count = paint.stops.len() as f32;
    params
        .with(name("kind"), ParamValue::Enum(paint.kind))
        .with(name("a"), ParamValue::Vec2(paint.a))
        .with(name("b"), ParamValue::Vec2(paint.b))
        .with(name("spread"), ParamValue::Enum(paint.spread))
        .with(name("count"), ParamValue::Float(count))
        .with(
            name("offsets0"),
            ParamValue::Vec4([offsets[0], offsets[1], offsets[2], offsets[3]]),
        )
        .with(
            name("offsets1"),
            ParamValue::Vec4([offsets[4], offsets[5], offsets[6], offsets[7]]),
        )
}

/// The generator's fill of `paint` at `size`, read back.
pub(super) fn generated(renderer: &Renderer, size: RenderSize, paint: &TestPaint) -> Vec<u8> {
    let params = with_paint(EffectParams::new(), "fill", paint);
    let output = renderer
        .render_generator(
            &crate::EffectId::new(crate::GENERATOR_EFFECT_ID.to_owned(), 1),
            size,
            &params,
            EffectRuntime::default(),
        )
        .expect("the generator renders");
    read_rgba8(renderer, &output)
}

pub(super) fn generator_renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(crate::generator_definition()))
        .expect("the generator compiles");
    renderer
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f64) -> f64 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// A straight sRGB stop color, premultiplied in linear light.
fn premultiplied_linear(color: [f32; 4]) -> [f64; 4] {
    let alpha = f64::from(color[3]);
    [
        srgb_to_linear(f64::from(color[0])) * alpha,
        srgb_to_linear(f64::from(color[1])) * alpha,
        srgb_to_linear(f64::from(color[2])) * alpha,
        alpha,
    ]
}

/// The reference for one gradient: premultiplied linear-light
/// interpolation, stored premultiplied and sRGB-encoded.
fn reference_gradient(stops: &[(f32, [f32; 4])], t: f64) -> [f64; 4] {
    let color = if t <= f64::from(stops[0].0) {
        premultiplied_linear(stops[0].1)
    } else {
        stops
            .windows(2)
            .find(|pair| t <= f64::from(pair[1].0))
            .map_or_else(
                || premultiplied_linear(stops[stops.len() - 1].1),
                |pair| {
                    let (from, to) = (f64::from(pair[0].0), f64::from(pair[1].0));
                    let f = (t - from) / (to - from);
                    let (a, b) = (
                        premultiplied_linear(pair[0].1),
                        premultiplied_linear(pair[1].1),
                    );
                    std::array::from_fn(|channel| a[channel] + (b[channel] - a[channel]) * f)
                },
            )
    };
    if color[3] <= 0.0 {
        return [0.0; 4];
    }
    let store = |channel: f64| linear_to_srgb((channel / color[3]).clamp(0.0, 1.0)) * color[3];
    [store(color[0]), store(color[1]), store(color[2]), color[3]]
}

fn assert_near(actual: &[u8], expected: [f64; 4], context: &str) {
    for channel in 0..4 {
        let want = expected[channel] * 255.0;
        assert!(
            (f64::from(actual[channel]) - want).abs() <= 1.0,
            "{context}: {actual:?} against {:?}",
            expected.map(|value| value * 255.0)
        );
    }
}

#[test]
fn gradients_interpolate_premultiplied_in_linear_light_and_spread_as_asked() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let size = RenderSize::new(64, 4).expect("size");
    // Opaque red to transparent blue: premultiplied, the middle is half-transparent red,
    // never purple.
    let stops = [
        (0.0, [1.0, 0.0, 0.0, 1.0]),
        (0.5, [0.0, 0.0, 1.0, 0.0]),
        (1.0, [1.0, 1.0, 1.0, 1.0]),
    ];
    for spread in 0..3 {
        let paint = TestPaint::gradient(2, [0.25, 0.5], [0.75, 0.5], spread, &stops);
        let pixels = generated(&renderer, size, &paint);
        for x in 0..64_usize {
            #[allow(clippy::cast_precision_loss)]
            let u = (x as f64 + 0.5) / 64.0;
            let t = (u - 0.25) / 0.5;
            let t = match spread {
                0 => t.clamp(0.0, 1.0),
                1 => t - t.floor(),
                _ => {
                    let m = t - 2.0 * (t * 0.5).floor();
                    if m > 1.0 { 2.0 - m } else { m }
                }
            };
            assert_near(
                &pixels[x * 4..x * 4 + 4],
                reference_gradient(&stops, t),
                &format!("spread {spread} at x {x}"),
            );
        }
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn radial_and_conic_gradients_are_circular_and_turn_clockwise_from_up() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    // The circle is round in pixels, its radius a fraction of the shorter side.
    let size = RenderSize::new(96, 48).expect("size");
    let at =
        |pixels: &[u8], x: usize, y: usize| pixels[(y * 96 + x) * 4..(y * 96 + x) * 4 + 4].to_vec();
    let black_to_white = [(0.0, [0.0, 0.0, 0.0, 1.0]), (1.0, [1.0, 1.0, 1.0, 1.0])];
    let radial = TestPaint::gradient(3, [0.5, 0.5], [0.5, 0.0], 0, &black_to_white);
    let pixels = generated(&renderer, size, &radial);
    // Pixel (x, 24) is |x + 0.5 - 48| pixels from the centre of a 24-pixel radius, the same
    // distance horizontally and vertically.
    for (x, y) in [(60, 24), (48, 36), (36, 24), (70, 24), (90, 24)] {
        #[allow(clippy::cast_precision_loss)]
        let distance = ((x as f64 + 0.5 - 48.0).powi(2) + (y as f64 + 0.5 - 24.0).powi(2)).sqrt();
        assert_near(
            &at(&pixels, x, y),
            reference_gradient(&black_to_white, (distance / 24.0).min(1.0)),
            &format!("radial at ({x}, {y})"),
        );
    }

    // Red at 0, green a quarter turn on, blue half, white three quarters.
    let quarters = [
        (0.0, [1.0, 0.0, 0.0, 1.0]),
        (0.25, [0.0, 1.0, 0.0, 1.0]),
        (0.5, [0.0, 0.0, 1.0, 1.0]),
        (0.75, [1.0, 1.0, 1.0, 1.0]),
        (1.0, [1.0, 1.0, 1.0, 1.0]),
    ];
    let conic = TestPaint::gradient(4, [0.5, 0.5], [0.0, 0.0], 0, &quarters);
    let pixels = generated(&renderer, size, &conic);
    // Every pixel against the reference at its angle, away from the centre and from the
    // seam at straight up, where neighbouring pixels jump from the last stop to the first.
    for y in 0..48_usize {
        for x in 0..96_usize {
            #[allow(clippy::cast_precision_loss)]
            let (dx, dy) = (x as f64 + 0.5 - 48.0, y as f64 + 0.5 - 24.0);
            let turn = dx.atan2(-dy) / std::f64::consts::TAU;
            let t = turn - turn.floor();
            if dx.hypot(dy) < 3.0 || !(0.01..=0.99).contains(&t) {
                continue;
            }
            let pixel = at(&pixels, x, y);
            let want = reference_gradient(&quarters, t).map(|value| value * 255.0);
            assert!(
                (0..4).all(|channel| (f64::from(pixel[channel]) - want[channel]).abs() <= 2.0),
                "conic at ({x}, {y}), t {t}: {pixel:?} against {want:?}"
            );
        }
    }
    // A quarter turn of offset moves the colors a quarter turn clockwise.
    let turned = TestPaint::gradient(
        4,
        [0.5, 0.5],
        [std::f32::consts::FRAC_PI_2, 0.0],
        0,
        &quarters,
    );
    let pixels = generated(&renderer, size, &turned);
    let turn = 45.5_f64.atan2(-0.5) / std::f64::consts::TAU - 0.25;
    assert_near(
        &at(&pixels, 93, 24),
        reference_gradient(&quarters, turn),
        "a quarter turn on, right is where up was",
    );
}

#[test]
fn generated_pictures_match_their_golden() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    let tile = RenderSize::new(48, 48).expect("size");
    let tiles = [
        generated(
            &renderer,
            tile,
            &TestPaint::gradient(2, [0.2, 0.2], [0.6, 0.7], 2, &SPECTRUM),
        ),
        generated(
            &renderer,
            tile,
            &TestPaint::gradient(3, [0.4, 0.6], [0.25, 0.0], 1, &SPECTRUM),
        ),
        generated(
            &renderer,
            tile,
            &TestPaint::gradient(4, [0.5, 0.5], [0.3, 0.0], 0, &SPECTRUM),
        ),
    ];
    let size = RenderSize::new(48 * 3, 48).expect("size");
    let mut pixels = Vec::with_capacity(48 * 3 * 48 * 4);
    for y in 0..48 {
        for tile in &tiles {
            pixels.extend_from_slice(&tile[y * 48 * 4..(y + 1) * 48 * 4]);
        }
    }
    assert_rgba8_golden("generated_sources", size, &pixels, 2);
}

/// Stops a golden paints with.
pub(super) const SPECTRUM: [(f32, [f32; 4]); 4] = [
    (0.0, [0.95, 0.2, 0.2, 1.0]),
    (0.4, [0.95, 0.85, 0.2, 1.0]),
    (0.7, [0.2, 0.7, 0.95, 0.6]),
    (1.0, [0.5, 0.2, 0.9, 1.0]),
];
