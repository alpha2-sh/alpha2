use super::*;

/// One paint as the generator's parameters carry it.
struct TestPaint {
    kind: u32,
    a: [f32; 2],
    b: [f32; 2],
    spread: u32,
    stops: Vec<(f32, [f32; 4])>,
}

impl TestPaint {
    fn solid(color: [f32; 4]) -> Self {
        Self {
            kind: 1,
            a: [0.0; 2],
            b: [0.0; 2],
            spread: 0,
            stops: vec![(0.0, color)],
        }
    }

    fn gradient(
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

/// A shape: kind (1 rectangle, 2 ellipse), centre, size, clockwise rotation
/// in degrees, corner radius, and stroke width.
type TestShape = (u32, [f32; 2], [f32; 2], f32, f32, f32);

const FILL: TestShape = (0, [0.5, 0.5], [1.0, 1.0], 0.0, 0.0, 0.0);

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

fn generated(
    renderer: &Renderer,
    size: RenderSize,
    shape: TestShape,
    fill: Option<&TestPaint>,
    stroke: Option<&TestPaint>,
) -> Vec<u8> {
    let (kind, center, extent, rotation, radius, stroke_width) = shape;
    let mut params = EffectParams::new()
        .with("shape", ParamValue::Enum(kind))
        .with("center", ParamValue::Vec2(center))
        .with("size", ParamValue::Vec2(extent))
        .with("rotation", ParamValue::Float(rotation.to_radians()))
        .with("radius", ParamValue::Float(radius))
        .with("stroke_width", ParamValue::Float(stroke_width));
    if let Some(fill) = fill {
        params = with_paint(params, "fill", fill);
    }
    if let Some(stroke) = stroke {
        params = with_paint(params, "stroke", stroke);
    }
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

fn generator_renderer() -> Renderer {
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
        let pixels = generated(&renderer, size, FILL, Some(&paint), None);
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
    let pixels = generated(&renderer, size, FILL, Some(&radial), None);
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
    let pixels = generated(&renderer, size, FILL, Some(&conic), None);
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
    let pixels = generated(&renderer, size, FILL, Some(&turned), None);
    let turn = 45.5_f64.atan2(-0.5) / std::f64::consts::TAU - 0.25;
    assert_near(
        &at(&pixels, 93, 24),
        reference_gradient(&quarters, turn),
        "a quarter turn on, right is where up was",
    );
}

#[test]
fn a_shape_is_filled_and_stroked_on_its_outline_and_transparent_outside() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    let size = RenderSize::new(64, 64).expect("size");
    let at =
        |pixels: &[u8], x: usize, y: usize| pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4].to_vec();
    let green = TestPaint::solid([0.0, 1.0, 0.0, 1.0]);
    let white = TestPaint::solid([1.0, 1.0, 1.0, 1.0]);
    // A 32-pixel square from 16 to 48, with a 4-pixel stroke centred on the outline: pixels
    // 14 to 17 on the left edge.
    let square: TestShape = (1, [0.5, 0.5], [0.5, 0.5], 0.0, 0.0, 4.0 / 64.0);
    let pixels = generated(&renderer, size, square, Some(&green), Some(&white));
    assert_eq!(at(&pixels, 32, 32), [0, 255, 0, 255], "inside");
    assert_eq!(at(&pixels, 16, 32), [255, 255, 255, 255], "on the outline");
    assert_eq!(
        at(&pixels, 14, 32),
        [255, 255, 255, 255],
        "outer half of the stroke"
    );
    assert_eq!(at(&pixels, 12, 32), [0, 0, 0, 0], "outside");
    assert_eq!(at(&pixels, 20, 32), [0, 255, 0, 255], "inside the stroke");

    let ring = generated(&renderer, size, square, None, Some(&white));
    assert_eq!(at(&ring, 32, 32), [0, 0, 0, 0]);
    assert_eq!(at(&ring, 16, 32), [255, 255, 255, 255]);

    // An ellipse turned a quarter turn: 48 wide becomes 48 tall.
    let ellipse: TestShape = (2, [0.5, 0.5], [0.75, 0.25], 90.0, 0.0, 0.0);
    let pixels = generated(&renderer, size, ellipse, Some(&green), None);
    assert_eq!(at(&pixels, 32, 12)[3], 255, "the long axis is vertical");
    assert_eq!(at(&pixels, 12, 32)[3], 0, "the short axis is horizontal");
}

#[test]
fn a_generated_picture_is_the_same_at_any_resolution() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    let stops = [(0.0, [0.9, 0.2, 0.1, 1.0]), (1.0, [0.1, 0.3, 0.9, 1.0])];
    let paint = TestPaint::gradient(3, [0.4, 0.5], [0.6, 0.0], 0, &stops);
    let white = TestPaint::solid([1.0, 1.0, 1.0, 1.0]);
    let shape: TestShape = (1, [0.5, 0.5], [0.6, 0.5], 0.0, 0.1, 0.05);
    let render = |edge: u32| {
        let size = RenderSize::new(edge * 2, edge).expect("size");
        generated(&renderer, size, shape, Some(&paint), Some(&white))
    };
    let (proxy, full) = (render(24), render(96));
    // Each proxy pixel centre is the corner shared by the middle 2x2 of the 4x4 it covers;
    // compare against their mean, away from edges. Gradient pad kinks can be a few codes off.
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
fn generated_pictures_match_their_golden() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    let tile = RenderSize::new(48, 48).expect("size");
    let spectrum = [
        (0.0, [0.95, 0.2, 0.2, 1.0]),
        (0.4, [0.95, 0.85, 0.2, 1.0]),
        (0.7, [0.2, 0.7, 0.95, 0.6]),
        (1.0, [0.5, 0.2, 0.9, 1.0]),
    ];
    let white = TestPaint::solid([1.0, 1.0, 1.0, 1.0]);
    let tiles = [
        generated(
            &renderer,
            tile,
            FILL,
            Some(&TestPaint::gradient(
                2,
                [0.2, 0.2],
                [0.6, 0.7],
                2,
                &spectrum,
            )),
            None,
        ),
        generated(
            &renderer,
            tile,
            FILL,
            Some(&TestPaint::gradient(
                3,
                [0.4, 0.6],
                [0.25, 0.0],
                1,
                &spectrum,
            )),
            None,
        ),
        generated(
            &renderer,
            tile,
            FILL,
            Some(&TestPaint::gradient(
                4,
                [0.5, 0.5],
                [0.3, 0.0],
                0,
                &spectrum,
            )),
            None,
        ),
        generated(
            &renderer,
            tile,
            (1, [0.5, 0.5], [0.7, 0.5], 15.0, 0.12, 0.06),
            Some(&TestPaint::gradient(
                2,
                [0.2, 0.5],
                [0.8, 0.5],
                0,
                &spectrum,
            )),
            Some(&white),
        ),
        generated(
            &renderer,
            tile,
            (2, [0.5, 0.5], [0.8, 0.45], -30.0, 0.0, 0.08),
            None,
            Some(&TestPaint::gradient(
                4,
                [0.5, 0.5],
                [0.0, 0.0],
                0,
                &spectrum,
            )),
        ),
    ];
    let size = RenderSize::new(48 * 5, 48).expect("size");
    let mut pixels = Vec::with_capacity(48 * 5 * 48 * 4);
    for y in 0..48 {
        for tile in &tiles {
            pixels.extend_from_slice(&tile[y * 48 * 4..(y + 1) * 48 * 4]);
        }
    }
    assert_rgba8_golden("generated_sources", size, &pixels, 2);
}

#[test]
fn a_shape_shrinking_to_nothing_fades_out_instead_of_leaving_a_dot() {
    let _lock = gpu_test_lock();
    let renderer = generator_renderer();
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    // 33 pixels, so the centre is pixel 16's centre.
    let size = RenderSize::new(33, 33).expect("size");
    let white = TestPaint::solid([1.0, 1.0, 1.0, 1.0]);
    let coverage = |shape: TestShape, fill: bool| {
        let (fill, stroke) = if fill {
            (Some(&white), None)
        } else {
            (None, Some(&white))
        };
        let alpha = generated(&renderer, size, shape, fill, stroke)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[3])
            .collect::<Vec<_>>();
        let total = alpha.iter().map(|&value| u32::from(value)).sum::<u32>();
        (alpha[16 * 33 + 16], total)
    };
    for kind in [1, 2] {
        let shape = |pixels: f32| (kind, [0.5, 0.5], [pixels / 33.0; 2], 0.0, 0.0, 0.0);
        let sizes = [0.0, 0.25, 0.5, 1.0];
        let measured = sizes.map(|pixels| coverage(shape(pixels), true));
        // Nothing at all at size 0, where the edge ramp alone would leave alpha 127 or
        // 128 at the centre.
        assert_eq!(measured[0], (0, 0), "kind {kind} at size 0");
        for (pair, size_pair) in measured.windows(2).zip(sizes.windows(2)) {
            assert!(
                pair[0].1 < pair[1].1,
                "kind {kind}: {size_pair:?} px gave {pair:?}"
            );
        }
        assert!(
            measured[1].0 <= 32,
            "kind {kind} at 0.25 px: {:?}",
            measured[1]
        );
        assert!(
            measured[2].0 <= 96,
            "kind {kind} at 0.5 px: {:?}",
            measured[2]
        );
    }
    // A stroke thinner than a pixel fades with its width the same way, down to none.
    let square = |stroke: f32| (1, [0.5, 0.5], [0.5, 0.5], 0.0, 0.0, stroke / 33.0);
    let strokes = [0.001, 0.25, 0.5, 1.0].map(|width| coverage(square(width), false).1);
    assert!(strokes[0] < 33 * 4, "a 0.001 px stroke: {strokes:?}");
    assert!(
        strokes.windows(2).all(|pair| pair[0] < pair[1]),
        "{strokes:?}"
    );
    assert!(pollster::block_on(scope.pop()).is_none());
}
