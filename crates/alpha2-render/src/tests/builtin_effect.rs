//! What each built-in effect and transition does to pixels, at its defaults
//! and at the values that reach its edge cases.

use super::*;

/// A renderer with the whole built-in catalog registered.
fn builtin_renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
    renderer
}

/// `pixel` repeated over `size`.
fn solid(size: RenderSize, pixel: [u8; 4]) -> Vec<u8> {
    pixel.repeat((size.width * size.height) as usize)
}

/// Fails with how many pixels of two equally sized images differ, and the first of them.
fn assert_same_pixels(actual: &[u8], expected: &[u8], what: &str) {
    let differing: Vec<_> = actual
        .as_chunks::<4>()
        .0
        .iter()
        .zip(expected.as_chunks::<4>().0)
        .enumerate()
        .filter(|(_, (a, e))| a != e)
        .collect();
    assert!(
        differing.is_empty(),
        "{what}: {} of {} pixels differ, first {:?}",
        differing.len(),
        actual.len() / 4,
        differing.first()
    );
}

/// Renders built-in effect `id` over `pixels` (premultiplied RGBA8 over
/// `size`) and reads the result back.
fn render_builtin_effect(
    renderer: &Renderer,
    id: &str,
    size: RenderSize,
    pixels: &[u8],
    params: &EffectParams,
) -> Vec<u8> {
    let source = rgba_image_texture(renderer, size, pixels);
    let target = renderer
        .render_effect(
            &crate::EffectId::new(id.into(), 1),
            &source.create_view(&wgpu::TextureViewDescriptor::default()),
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

/// The alpha of every pixel in column `x`, top first.
fn column_alpha(pixels: &[u8], size: RenderSize, x: u32) -> Vec<u8> {
    (0..size.height)
        .map(|y| pixels[((y * size.width + x) * 4 + 3) as usize])
        .collect()
}

/// The alpha of every pixel in row `y`, left first.
fn row_alpha(pixels: &[u8], size: RenderSize, y: u32) -> Vec<u8> {
    (0..size.width)
        .map(|x| pixels[((y * size.width + x) * 4 + 3) as usize])
        .collect()
}

/// Renders built-in transition `id` from `from` to `to` (premultiplied
/// RGBA8 over `size`) at `progress`.
fn render_builtin_transition(
    renderer: &Renderer,
    id: &str,
    size: RenderSize,
    (from, to): (&[u8], &[u8]),
    params: &EffectParams,
    progress: f32,
) -> RenderedTexture {
    let from = rgba_image_texture(renderer, size, from);
    let to = rgba_image_texture(renderer, size, to);
    renderer
        .render_effect(
            &crate::EffectId::new(id.into(), 1),
            &from.create_view(&wgpu::TextureViewDescriptor::default()),
            Some(&to.create_view(&wgpu::TextureViewDescriptor::default())),
            size,
            params,
            EffectRuntime {
                progress,
                #[allow(clippy::cast_precision_loss)]
                ratio: size.width as f32 / size.height as f32,
                ..EffectRuntime::default()
            },
        )
        .expect("the transition renders")
}

#[test]
fn directional_without_a_direction_slides_like_its_default() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(16, 16).expect("non-zero size");
    let red = solid(size, [255, 0, 0, 255]);
    let blue = solid(size, [0, 0, 255, 255]);
    let render = |direction: [f32; 2]| {
        let out = render_builtin_transition(
            &renderer,
            "alpha2.transition.directional",
            size,
            (&red, &blue),
            &EffectParams::new().with("direction", ParamValue::Vec2(direction)),
            0.5,
        );
        read_rgba8(&renderer, &out)
    };
    let default = render([0.0, 1.0]);
    let blue_pixels = default
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[2] > pixel[0])
        .count();
    assert_eq!(
        blue_pixels, 128,
        "half way, the default slide shows half of each clip"
    );
    // The schema admits (0, 0); every transition frame still has to mix both clips.
    assert_same_pixels(
        &render([0.0, 0.0]),
        &default,
        "a zero direction takes the default",
    );
}

#[test]
fn crop_feathers_only_the_edges_it_cuts() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    // 1920 x 1080 / 8: the default feather spans several pixels at each edge.
    let size = RenderSize::new(240, 135).expect("non-zero size");
    let grey = solid(size, [118, 118, 118, 255]);
    let crop = |params: EffectParams| {
        render_builtin_effect(&renderer, "alpha2.effect.crop", size, &grey, &params)
    };

    assert_same_pixels(&crop(EffectParams::new()), &grey, "default crop");

    // Cutting the left tenth feathers that line and leaves the frame's other three edges whole.
    let left = crop(EffectParams::new().with("top_left", ParamValue::Vec2([0.1, 0.0])));
    assert_eq!(row_alpha(&left, size, 67)[0], 0, "the cut part is gone");
    assert!(
        column_alpha(&left, size, size.width - 1)
            .iter()
            .all(|a| *a == 255),
        "the uncut right edge: {:?}",
        column_alpha(&left, size, size.width - 1)
    );
    assert!(
        row_alpha(&left, size, 0)[30..].iter().all(|a| *a == 255),
        "the uncut top edge"
    );
    let ramp = &row_alpha(&left, size, 67)[20..28];
    assert!(
        ramp.windows(2).all(|pair| pair[0] <= pair[1]) && ramp.iter().any(|a| *a > 0 && *a < 255),
        "the cut line is feathered: {ramp:?}"
    );

    let crossed = crop(
        EffectParams::new()
            .with("top_left", ParamValue::Vec2([0.6, 0.0]))
            .with("bottom_right", ParamValue::Vec2([0.5, 0.0]))
            .with("feather", ParamValue::Float(0.2)),
    );
    assert!(
        crossed.iter().all(|c| *c == 0),
        "a crossed crop keeps nothing"
    );
}

#[test]
fn window_blinds_keeps_alpha_and_color_valid_while_it_overshoots() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(16, 64).expect("non-zero size");
    let opaque = solid(size, [128, 128, 128, 255]);
    let clear = solid(size, [0; 4]);
    // The covered stripes run ahead of progress by half (gl-transitions'
    // `t *= 1.5`) and their weight passes 1 between p = 2/3 and about 0.97.
    for progress in [0.7, 0.75, 0.8, 0.85, 0.9] {
        for (from, to, what) in [
            (&opaque, &clear, "opaque to transparent"),
            (&clear, &opaque, "transparent to opaque"),
        ] {
            let out = render_builtin_transition(
                &renderer,
                "alpha2.transition.window-blinds",
                size,
                (from, to),
                &EffectParams::new(),
                progress,
            );
            if out.format != wgpu::TextureFormat::Rgba16Float {
                // An 8-bit intermediate clamps every write; only half floats keep the overshoot.
                continue;
            }
            for pixel in read_rgba16f(&renderer, &out).as_chunks::<4>().0 {
                assert!(
                    (0.0..=1.0).contains(&pixel[3])
                        && pixel[..3].iter().all(|c| (0.0..=pixel[3]).contains(c)),
                    "{what} at p={progress}: {pixel:?} is not premultiplied color"
                );
            }
        }
    }
}

#[test]
fn sharpen_leaves_a_flat_color_alone_beside_transparency() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(12, 4).expect("non-zero size");
    // Transparent, then the same grey half covered, then opaque: the band a
    // contained clip leaves around itself, and a soft matte edge.
    let pixels: Vec<u8> = (0..size.height)
        .flat_map(|_| {
            (0..size.width).flat_map(|x| match x {
                0..4 => [0, 0, 0, 0],
                4 => [51, 51, 51, 128],
                _ => [102, 102, 102, 255],
            })
        })
        .collect();
    for amount in [0.6, 3.0] {
        let sharpened = render_builtin_effect(
            &renderer,
            "alpha2.effect.sharpen",
            size,
            &pixels,
            &EffectParams::new().with("amount", ParamValue::Float(amount)),
        );
        for (index, (out, source)) in sharpened
            .as_chunks::<4>()
            .0
            .iter()
            .zip(pixels.as_chunks::<4>().0)
            .enumerate()
        {
            assert!(
                out.iter().zip(source).all(|(o, s)| o.abs_diff(*s) <= 1),
                "amount {amount}, pixel {index}: {out:?} from {source:?}; a flat color has no detail to sharpen"
            );
        }
    }
}

fn srgb_to_linear(code: u8) -> f64 {
    let encoded = f64::from(code) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn linear_to_srgb(linear: f64) -> u8 {
    let linear = linear.clamp(0.0, 1.0);
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round() as u8
}

#[test]
fn color_controls_grade_in_linear_light_around_middle_grey() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(2, 1).expect("non-zero size");
    // Middle grey (linear 0.18 is code 118) and pure red.
    let pixels = [118, 118, 118, 255, 255, 0, 0, 255];
    let grade = |params: EffectParams| {
        render_builtin_effect(
            &renderer,
            "alpha2.effect.color-controls",
            size,
            &pixels,
            &params,
        )
    };
    let grey = srgb_to_linear(118);
    let near = |actual: u8, expected: u8, what: &str| {
        assert!(
            actual.abs_diff(expected) <= 1,
            "{what}: {actual}, expected {expected}"
        );
    };

    let identity = grade(EffectParams::new());
    for (actual, expected) in identity.iter().zip(pixels) {
        near(*actual, expected, "the defaults");
    }
    for (stops, factor) in [(1.0, 2.0), (-1.0, 0.5)] {
        let out = grade(EffectParams::new().with("exposure", ParamValue::Float(stops)));
        near(
            out[0],
            linear_to_srgb(grey * factor),
            &format!("exposure {stops}"),
        );
    }
    for contrast in [0.5, 1.5, 2.0] {
        let out = grade(EffectParams::new().with("contrast", ParamValue::Float(contrast)));
        near(out[0], 118, &format!("contrast {contrast} at middle grey"));
    }
    // Without saturation, red keeps its luminance as light: 0.2126.
    let out = grade(EffectParams::new().with("saturation", ParamValue::Float(0.0)));
    let red_luminance = linear_to_srgb(0.2126);
    for channel in &out[4..7] {
        near(*channel, red_luminance, "red without saturation");
    }
}

#[test]
fn colour_distance_ramps_across_a_fixed_width_of_color_distance() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    // From black to grey codes 0..63 across the frame: the colour distance
    // grows by a code's worth of sqrt(3) per column, on every row alike.
    let size = RenderSize::new(64, 4).expect("non-zero size");
    let from = solid(size, [0, 0, 0, 255]);
    let to: Vec<u8> = (0..size.height)
        .flat_map(|_| (0..64_u8).flat_map(|code| [code, code, code, 255]))
        .collect();
    let (progress, power) = (0.2_f32, 5.0_f32);
    let out = render_builtin_transition(
        &renderer,
        "alpha2.transition.colour-distance",
        size,
        (&from, &to),
        &EffectParams::new().with("power", ParamValue::Float(power)),
        progress,
    );
    let out = read_rgba8(&renderer, &out);
    // The threshold lives in colour space: a pixel's switch depends on its colour distance alone,
    // not on screen-space derivatives, which GPUs measure differently.
    let smoothstep = |edge0: f32, edge1: f32, x: f32| {
        let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    for code in 0..64_u8 {
        let target = f32::from(code) / 255.0;
        let distance = target * 3.0_f32.sqrt();
        let mask = smoothstep(distance - 0.02, distance + 0.02, progress);
        let mixed = target * mask;
        let value = mixed + (target - mixed) * progress.powf(power);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected = (value * 255.0).round() as u8;
        for y in 0..size.height {
            let index = ((y * size.width + u32::from(code)) * 4) as usize;
            assert!(
                out[index].abs_diff(expected) <= 1,
                "code {code}, row {y}: {} where the ramp gives {expected}",
                out[index]
            );
        }
    }
}

#[test]
fn mirror_keeps_the_half_it_names_on_the_axis_it_names() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(4, 4).expect("non-zero size");
    // Every pixel distinct: red counts columns, green rows.
    let pixels: Vec<u8> = (0..4)
        .flat_map(|y| (0..4).flat_map(move |x| [40 + 60 * x, 40 + 60 * y, 128, 255]))
        .collect();
    let mirror = |axis: u32, keep_second_half: bool| {
        render_builtin_effect(
            &renderer,
            "alpha2.effect.mirror",
            size,
            &pixels,
            &EffectParams::new()
                .with("axis", ParamValue::Enum(axis))
                .with("keep_second_half", ParamValue::Bool(keep_second_half)),
        )
    };
    let red = |out: &[u8], x: u32| out[(x * 4) as usize];
    let green = |out: &[u8], y: u32| out[(y * 16 + 1) as usize];
    let first = mirror(0, false);
    assert_eq!(
        (0..4).map(|x| red(&first, x)).collect::<Vec<_>>(),
        [40, 100, 100, 40],
        "the left half, mirrored"
    );
    let second = mirror(0, true);
    assert_eq!(
        (0..4).map(|x| red(&second, x)).collect::<Vec<_>>(),
        [220, 160, 160, 220],
        "the right half, mirrored"
    );
    let top = mirror(1, false);
    assert_eq!(
        (0..4).map(|y| green(&top, y)).collect::<Vec<_>>(),
        [40, 100, 100, 40],
        "the top half, mirrored"
    );
    let bottom = mirror(1, true);
    assert_eq!(
        (0..4).map(|y| green(&bottom, y)).collect::<Vec<_>>(),
        [220, 160, 160, 220],
        "the bottom half, mirrored"
    );
}

#[test]
fn vignette_darkens_the_corners_and_leaves_the_centre() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(33, 33).expect("non-zero size");
    let grey = solid(size, [200, 200, 200, 255]);
    let out = render_builtin_effect(
        &renderer,
        "alpha2.effect.vignette",
        size,
        &grey,
        &EffectParams::new()
            .with("amount", ParamValue::Float(0.75))
            .with("radius", ParamValue::Float(0.4))
            .with("softness", ParamValue::Float(0.2)),
    );
    let at = |x: u32, y: u32| out[((y * size.width + x) * 4) as usize];
    assert_eq!(at(16, 16), 200, "the centre is untouched");
    // A corner is past radius + softness: shaded by the whole amount, alpha kept.
    for (x, y) in [(0, 0), (32, 0), (0, 32), (32, 32)] {
        assert!(
            at(x, y).abs_diff(50) <= 1,
            "corner ({x}, {y}): {}",
            at(x, y)
        );
        assert_eq!(out[((y * size.width + x) * 4 + 3) as usize], 255);
    }
    let diagonal: Vec<u8> = (0..=16).map(|d| at(16 + d, 16 + d)).collect();
    assert!(
        diagonal.windows(2).all(|pair| pair[1] <= pair[0]),
        "{diagonal:?}"
    );
}

#[test]
fn pixelate_fills_each_square_block_with_its_centre() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(16, 8).expect("non-zero size");
    let pixels: Vec<u8> = (0..8_u8)
        .flat_map(|y| (0..16_u8).flat_map(move |x| [x * 15, y * 30, 0, 255]))
        .collect();
    let out = render_builtin_effect(
        &renderer,
        "alpha2.effect.pixelate",
        size,
        &pixels,
        &EffectParams::new().with("blocks", ParamValue::Float(4.0)),
    );
    for y in 0..8_u32 {
        for x in 0..16_u32 {
            let index = ((y * 16 + x) * 4) as usize;
            // The block's centre falls between its middle two pixels; bilinear sampling
            // averages them.
            let (bx, by) = (x / 4 * 4, y / 4 * 4);
            let expected = [
                (bx * 15 + (bx + 1) * 15 + 30) / 2,
                (by * 30 + (by + 1) * 30 + 60) / 2,
            ];
            assert!(
                u32::from(out[index]).abs_diff(expected[0]) <= 1
                    && u32::from(out[index + 1]).abs_diff(expected[1]) <= 1,
                "pixel ({x}, {y}): {:?}, block centre {expected:?}",
                &out[index..index + 4]
            );
        }
    }
}

#[test]
fn glow_spreads_light_alike_across_and_down_a_wide_frame() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    // A frame three times wider than tall: the halo is round in pixels.
    let size = RenderSize::new(192, 64).expect("non-zero size");
    let mut pixels = solid(size, [0, 0, 0, 255]);
    for (dx, dy) in (0..3_usize).flat_map(|dx| (0..3).map(move |dy| (dx, dy))) {
        let index = ((31 + dy) * 192 + 95 + dx) * 4;
        pixels[index..index + 3].copy_from_slice(&[255, 255, 255]);
    }
    let dim = ((32 * 192 + 20) * 4) as usize;
    pixels[dim..dim + 3].copy_from_slice(&[100, 100, 100]);
    let out = render_builtin_effect(
        &renderer,
        "alpha2.effect.glow",
        size,
        &pixels,
        &EffectParams::new()
            .with("threshold", ParamValue::Float(0.6))
            .with("intensity", ParamValue::Float(3.0))
            .with("radius", ParamValue::Float(0.06)),
    );
    let at = |x: u32, y: u32| out[((y * 192 + x) * 4) as usize];
    // Out from the 3 x 3 square's centre, past its edge: an 8-bit intermediate rounds each
    // separable pass, a code or two.
    for distance in 2..=4 {
        let across = [at(96 + distance, 32), at(96 - distance, 32)];
        let down = [at(96, 32 + distance), at(96, 32 - distance)];
        assert!(across[0] > 0, "the halo reaches {distance} px");
        for value in across.into_iter().chain(down) {
            assert!(
                value.abs_diff(across[0]) <= 3,
                "{distance} px out: across {across:?}, down {down:?}"
            );
        }
    }
    assert_eq!(at(20, 32), 100, "below the threshold nothing glows");
    assert_eq!(at(19, 32), 0);
}

#[test]
fn sharpen_steepens_an_edge() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(8, 2).expect("non-zero size");
    let pixels: Vec<u8> = (0..2)
        .flat_map(|_| {
            (0..8).flat_map(|x| {
                if x < 4 {
                    [80, 80, 80, 255]
                } else {
                    [160, 160, 160, 255]
                }
            })
        })
        .collect();
    let out = render_builtin_effect(
        &renderer,
        "alpha2.effect.sharpen",
        size,
        &pixels,
        &EffectParams::new().with("amount", ParamValue::Float(1.0)),
    );
    let row: Vec<u8> = (0..8).map(|x| out[x * 4]).collect();
    // Each side of the step moves a quarter of the step away from the other: 80 - 20, 160 + 20.
    assert_eq!(row, [80, 80, 80, 60, 180, 160, 160, 160]);
}

#[test]
fn chroma_key_softens_its_edge_suppresses_spill_and_can_ignore_luma() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(3, 1).expect("non-zero size");
    let key = |pixels: &[u8], params: EffectParams| {
        render_builtin_effect(&renderer, "alpha2.effect.chroma-key", size, pixels, &params)
    };
    // Halfway through the soft edge: 0.2 from the key, threshold 0.1, softness 0.2.
    let edge = key(
        &[0, 255, 51, 255, 0, 255, 0, 255, 255, 0, 0, 255],
        EffectParams::new()
            .with("key_color", ParamValue::Color([0.0, 1.0, 0.0, 1.0]))
            .with("threshold", ParamValue::Float(0.1))
            .with("softness", ParamValue::Float(0.2))
            .with("spill_suppression", ParamValue::Float(1.0)),
    );
    // Half kept, and half way to its luminance (0.7296): (0.365, 0.865, 0.465) at alpha 0.5.
    for (actual, expected) in edge[..4].iter().zip([47_u8, 110, 59, 128]) {
        assert!(
            actual.abs_diff(expected) <= 1,
            "the soft edge: {:?}",
            &edge[..4]
        );
    }
    assert_eq!(&edge[4..8], [0; 4], "the key itself is gone");
    assert_eq!(
        &edge[8..],
        [255, 0, 0, 255],
        "red is far from green and stays"
    );

    // A lit and a shadowed screen differ by a gray: the same chroma.
    let screen = [77, 255, 77, 255, 0, 179, 0, 255, 255, 0, 0, 255];
    let params = |mode| {
        EffectParams::new()
            .with("key_color", ParamValue::Color([0.0, 0.7, 0.0, 1.0]))
            .with("threshold", ParamValue::Float(0.35))
            .with("softness", ParamValue::Float(0.1))
            .with("distance_mode", ParamValue::Enum(mode))
    };
    let rgb = key(&screen, params(0));
    assert_eq!(
        rgb[3], 255,
        "in RGB the lit screen is 0.52 away and stays: {rgb:?}"
    );
    assert_eq!(rgb[7], 0, "the shadowed screen is the key");
    let chroma = key(&screen, params(1));
    assert_eq!(
        chroma[3], 0,
        "by chroma alone the lit screen is the key too: {chroma:?}"
    );
    assert_eq!(chroma[7], 0);
    assert_eq!(&chroma[8..], [255, 0, 0, 255]);
}

/// A 16 x 16 picture for the contact sheet: gradients, a transparent band
/// down the left, and half-covered rows along the bottom.
fn effect_test_image(size: RenderSize) -> Vec<u8> {
    let mut pixels = transition_test_image(size, false);
    let width = size.width as usize;
    let height = size.height as usize;
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (index % width, index / width);
        if x < 2 {
            *pixel = [0; 4];
        } else if y >= height - 4 {
            *pixel = [pixel[0] / 2, pixel[1] / 2, pixel[2] / 2, 128];
        }
    }
    pixels
}

/// Every public effect but the LUT (its own golden, `lut_3d`), away from its
/// defaults, as the contact sheet draws them.
fn contact_sheet_effects() -> [(&'static str, EffectParams<'static>); 11] {
    [
        (
            "alpha2.effect.gaussian-blur",
            EffectParams::new().with("radius", ParamValue::Float(0.1)),
        ),
        (
            "alpha2.effect.chroma-key",
            EffectParams::new()
                .with("key_color", ParamValue::Color([0.5, 0.5, 0.5, 1.0]))
                .with("threshold", ParamValue::Float(0.3))
                .with("softness", ParamValue::Float(0.2))
                .with("spill_suppression", ParamValue::Float(0.5)),
        ),
        (
            "alpha2.effect.color-controls",
            EffectParams::new()
                .with("exposure", ParamValue::Float(0.5))
                .with("contrast", ParamValue::Float(1.3))
                .with("saturation", ParamValue::Float(1.5))
                .with("temperature", ParamValue::Float(0.3))
                .with("tint", ParamValue::Float(-0.2)),
        ),
        (
            "alpha2.effect.vignette",
            EffectParams::new()
                .with("amount", ParamValue::Float(0.8))
                .with("radius", ParamValue::Float(0.5))
                .with("softness", ParamValue::Float(0.3)),
        ),
        (
            "alpha2.effect.sharpen",
            EffectParams::new().with("amount", ParamValue::Float(1.5)),
        ),
        (
            "alpha2.effect.pixelate",
            EffectParams::new().with("blocks", ParamValue::Float(4.0)),
        ),
        (
            "alpha2.effect.glow",
            EffectParams::new()
                .with("threshold", ParamValue::Float(0.4))
                .with("intensity", ParamValue::Float(1.5))
                .with("radius", ParamValue::Float(0.05)),
        ),
        (
            "alpha2.effect.film-grain",
            EffectParams::new()
                .with("amount", ParamValue::Float(0.5))
                .with("size", ParamValue::Float(1.0)),
        ),
        (
            "alpha2.effect.crop",
            EffectParams::new()
                .with("top_left", ParamValue::Vec2([0.1, 0.2]))
                .with("bottom_right", ParamValue::Vec2([0.25, 0.05]))
                .with("feather", ParamValue::Float(0.05)),
        ),
        (
            "alpha2.effect.mirror",
            EffectParams::new()
                .with("axis", ParamValue::Enum(1))
                .with("keep_second_half", ParamValue::Bool(true)),
        ),
        (
            "alpha2.effect.mirror",
            EffectParams::new().with("axis", ParamValue::Enum(0)),
        ),
    ]
}

#[test]
fn every_builtin_effect_draws_its_reviewed_picture() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(16, 16).expect("non-zero size");
    let pixels = effect_test_image(size);
    let effects = contact_sheet_effects();
    let catalog: Vec<String> = crate::builtin_effect_definitions()
        .into_iter()
        .map(|definition| definition.id)
        .filter(|id| id != crate::LUT_3D_ID)
        .collect();
    for id in &catalog {
        assert!(
            effects.iter().any(|(effect, _)| effect == id),
            "{id} is missing from the contact sheet"
        );
    }
    let frames: Vec<Vec<u8>> = effects
        .iter()
        .map(|(id, params)| render_builtin_effect(&renderer, id, size, &pixels, params))
        .collect();
    let row_bytes = size.width as usize * 4;
    let mut sheet = Vec::with_capacity(row_bytes * frames.len() * size.height as usize);
    for row in 0..size.height as usize {
        for frame in &frames {
            sheet.extend_from_slice(&frame[row * row_bytes..(row + 1) * row_bytes]);
        }
    }
    let labels: Vec<&str> = effects.iter().map(|(id, _)| *id).collect();
    assert_rgba8_golden_tiles(
        &["builtin_effects"],
        RenderSize::new(
            size.width * u32::try_from(effects.len()).expect("a few tiles"),
            size.height,
        )
        .expect("non-zero size"),
        &labels,
        &sheet,
        2,
    );
}

#[test]
fn an_effect_body_reads_the_time_only_through_the_names_that_reach_it() {
    use crate::registry::body_reads_time;
    for body in [
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * time(); }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * runtime.resolution_time.z; }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { let r = &runtime; return src * (*r).resolution_time.w; }",
        // A comment, a parameter, or a local naming it counts: the answer may only err towards
        // reading it.
        "// time\nfn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * params.time; }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { let time = 2.0; return src * time; }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src*time(); }",
    ] {
        assert!(body_reads_time(body), "{body}");
    }
    for body in [
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * ratio() * progress(); }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { let time_scale = 2.0; return src * time_scale; }",
        "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * f32(textureDimensions(source_a).x) / resolution().x; }",
        "fn runtime_like(x: f32) -> f32 { return x; } fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * runtime_like(1.0); }",
    ] {
        assert!(!body_reads_time(body), "{body}");
    }
}

#[test]
fn only_film_grain_of_the_builtins_reads_the_time() {
    // A body that changes whether it reads the time changes this list: the result cache keys an
    // effect by the frame time only when it does.
    let reading: Vec<String> = crate::builtin_effect_definitions()
        .into_iter()
        .chain(crate::builtin_transition_definitions())
        .chain(crate::builtins::internal_effect_definitions())
        .chain([crate::chroma_key_definition(), crate::lut_3d_definition()])
        .filter(|definition| crate::registry::body_reads_time(&definition.wgsl_body))
        .map(|definition| definition.id)
        .collect();
    assert_eq!(reading, ["alpha2.effect.film-grain"]);
}

#[test]
fn builtins_that_do_not_read_the_time_draw_alike_at_any_time() {
    let _lock = gpu_test_lock();
    let renderer = builtin_renderer();
    let size = RenderSize::new(16, 16).expect("non-zero size");
    let pixels = effect_test_image(size);
    let source = rgba_image_texture(&renderer, size, &pixels);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let at = |id: &str, params: &EffectParams, ticks: i64| {
        let target = renderer
            .render_effect(
                &crate::EffectId::new(id.into(), 1),
                &view,
                None,
                size,
                params,
                EffectRuntime {
                    time: alpha2_time::MediaTime::from_ticks(ticks),
                    ratio: 1.0,
                    ..EffectRuntime::default()
                },
            )
            .expect("the effect renders");
        read_rgba8(&renderer, &target)
    };
    for (id, params) in contact_sheet_effects() {
        let reads = renderer.effect_reads_time(&crate::EffectId::new(id.into(), 1));
        assert_eq!(reads, id == "alpha2.effect.film-grain", "{id}");
        let (first, later) = (at(id, &params, 0), at(id, &params, 870_000));
        if reads {
            assert_ne!(first, later, "{id} draws new grain at a new time");
        } else {
            assert_same_pixels(&later, &first, id);
        }
    }
    let other = rgba_image_texture(&renderer, size, &solid(size, [0, 0, 255, 255]));
    let other = other.create_view(&wgpu::TextureViewDescriptor::default());
    for definition in crate::builtin_transition_definitions() {
        let id = crate::EffectId::new(definition.id.clone(), 1);
        assert!(!renderer.effect_reads_time(&id), "{}", definition.id);
        let at = |ticks: i64| {
            let target = renderer
                .render_effect(
                    &id,
                    &view,
                    Some(&other),
                    size,
                    &EffectParams::new(),
                    EffectRuntime {
                        time: alpha2_time::MediaTime::from_ticks(ticks),
                        progress: 0.4,
                        ratio: 1.0,
                    },
                )
                .expect("the transition renders");
            read_rgba8(&renderer, &target)
        };
        assert_same_pixels(&at(870_000), &at(0), &definition.id);
    }
    // The passes a blur or a glow is drawn as are asked, not its placeholder body; an effect
    // this renderer does not know may read anything.
    assert!(!renderer.effect_reads_time(&crate::EffectId::new(crate::GLOW_ID.into(), 1)));
    assert!(!renderer.effect_reads_time(&crate::EffectId::new(crate::GAUSSIAN_BLUR_ID.into(), 1)));
    assert!(renderer.effect_reads_time(&crate::EffectId::new("test.unknown".into(), 1)));
}
