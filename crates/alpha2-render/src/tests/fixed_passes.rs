//! Film grain, Gaussian blur, chroma key and 3D LUT effects: goldens, parameter checks, and references.

use super::*;

/// Film grain is integer-hashed, so every GPU draws the same grain: one
/// golden holds sizes whose cell edges fall on pixel centers (0.5, 1.5) and
/// a coarse one, and another time draws other grain.
#[test]
fn film_grain_is_the_same_on_every_gpu_and_changes_with_time() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("built-ins compile");
    let id = crate::EffectId::new("alpha2.effect.film-grain".into(), 1);
    let size = RenderSize::new(8, 8).expect("non-zero size");
    let gray = rgba_image_texture(&renderer, size, &[128, 128, 128, 255].repeat(64));
    let gray_view = gray.create_view(&wgpu::TextureViewDescriptor::default());
    let grain = |grain_size: f32, ticks: i64| {
        let params = EffectParams::new()
            .with("amount", ParamValue::Float(0.5))
            .with("size", ParamValue::Float(grain_size));
        let target = renderer
            .render_effect(
                &id,
                &gray_view,
                None,
                size,
                &params,
                EffectRuntime {
                    time: alpha2_time::MediaTime::from_ticks(ticks),
                    ..EffectRuntime::default()
                },
            )
            .expect("film grain renders");
        read_rgba8(&renderer, &target)
    };
    let sizes = [0.5, 1.5, 4.0];
    let frames = sizes.map(|grain_size| grain(grain_size, 150_000));
    for frame in &frames {
        let levels = frame
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[0])
            .collect::<std::collections::BTreeSet<_>>();
        assert!(levels.len() > 1, "grain varies across the frame");
    }
    let row_bytes = 8 * 4;
    let sheet = (0..8)
        .flat_map(|row| {
            frames
                .iter()
                .flat_map(move |frame| frame[row * row_bytes..(row + 1) * row_bytes].to_vec())
        })
        .collect::<Vec<_>>();
    assert_rgba8_golden(
        "film_grain",
        RenderSize::new(8 * 3, 8).expect("non-zero size"),
        &sheet,
        1,
    );
    assert_ne!(
        grain(1.5, 300_000),
        frames[1],
        "another time draws other grain"
    );
}

#[test]
fn separable_gaussian_blur_matches_golden_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let mut renderer = Renderer::new(gpu);
    renderer
        .register_builtin_gaussian_blur()
        .expect("built-in blur compiles");
    let size = RenderSize::new(8, 8).expect("non-zero size");
    let pixels = (0..size.height)
        .flat_map(|y| {
            (0..size.width).flat_map(move |x| {
                if (x + y) % 2 == 0 {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
        })
        .collect::<Vec<_>>();
    let texture = rgba_image_texture(&renderer, size, &pixels);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let target = renderer
        .render_gaussian_blur(&view, size, 0.25)
        .expect("normalized radius is valid");
    let blurred = read_rgba8(&renderer, &target);
    assert_rgba8_golden("gaussian_blur", size, &blurred, 1);
}

#[test]
fn gaussian_blur_rejects_invalid_normalized_radius() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer
        .register_builtin_gaussian_blur()
        .expect("built-in blur compiles");
    let texture = rgba_texture(&renderer, [255, 255, 255, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    assert!(matches!(
        renderer.render_gaussian_blur(&view, size, f32::NAN),
        Err(RenderError::InvalidEffectParams)
    ));
    assert!(matches!(
        renderer.render_gaussian_blur(&view, size, 1.1),
        Err(RenderError::InvalidEffectParams)
    ));
}

#[test]
fn normalized_blur_matches_between_proxy_and_full_resolution() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer
        .register_builtin_gaussian_blur()
        .expect("built-in blur compiles");
    let proxy_size = RenderSize::new(16, 16).expect("non-zero size");
    let full_size = RenderSize::new(32, 32).expect("non-zero size");
    let proxy = centered_square(proxy_size);
    let full = centered_square(full_size);
    let proxy_texture = rgba_image_texture(&renderer, proxy_size, &proxy);
    let full_texture = rgba_image_texture(&renderer, full_size, &full);
    let proxy_view = proxy_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let full_view = full_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let proxy_target = renderer
        .render_gaussian_blur(&proxy_view, proxy_size, 0.15)
        .expect("normalized radius is valid");
    let full_target = renderer
        .render_gaussian_blur(&full_view, full_size, 0.15)
        .expect("normalized radius is valid");
    let proxy_pixels = read_rgba8(&renderer, &proxy_target);
    let full_pixels = read_rgba8(&renderer, &full_target);
    let resampled = downsample_rgba_2x(&full_pixels, full_size);
    let maximum_error = proxy_pixels
        .iter()
        .zip(resampled)
        .map(|(proxy, full)| proxy.abs_diff(full))
        .max()
        .expect("comparison contains pixels");
    assert!(
        maximum_error <= 8,
        "proxy/full normalized blur diverged by {maximum_error} levels"
    );
}

#[test]
fn chroma_key_removes_green_and_preserves_red_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let id = renderer
        .register_builtin_chroma_key()
        .expect("built-in chroma key compiles");
    let size = RenderSize::new(2, 1).expect("non-zero size");
    let texture = rgba_image_texture(&renderer, size, &[0, 255, 0, 255, 255, 0, 0, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let target = renderer
        .render_effect(
            &id,
            &view,
            None,
            size,
            &chroma_key_params([0.0, 1.0, 0.0], 0.1, 0.1, 1.0),
            EffectRuntime::default(),
        )
        .expect("valid chroma-key controls");
    let keyed = read_rgba8(&renderer, &target);
    assert_rgba8_golden("chroma_key", size, &keyed, 1);
}

#[test]
fn builtin_chroma_key_effect_keys_the_srgb_encoded_color_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    // A screen keyed with its straight sRGB-encoded color (51, 179, 77), as `key_color` is;
    // beside it, the color whose linear light is that triple (124, 218, 149) must stay.
    let size = RenderSize::new(3, 1).expect("non-zero size");
    let texture = rgba_image_texture(
        &renderer,
        size,
        &[51, 179, 77, 255, 124, 218, 149, 255, 255, 0, 0, 255],
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let id = renderer
        .register_builtin_chroma_key()
        .expect("built-in chroma key compiles");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let keyed = read_rgba8(
        &renderer,
        &renderer
            .render_effect(
                &id,
                &view,
                None,
                size,
                &chroma_key_params([51.0 / 255.0, 179.0 / 255.0, 77.0 / 255.0], 0.1, 0.1, 1.0),
                EffectRuntime::default(),
            )
            .expect("built-in chroma key renders"),
    );
    assert!(pollster::block_on(scope.pop()).is_none());
    assert_eq!(keyed[3], 0, "the screen is keyed out: {keyed:?}");
    assert_eq!(
        &keyed[4..],
        &[124, 218, 149, 255, 255, 0, 0, 255],
        "the rest stays"
    );
}

#[test]
fn chroma_key_rejects_zero_softness() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let id = renderer
        .register_builtin_chroma_key()
        .expect("built-in chroma key compiles");
    let texture = rgba_texture(&renderer, [0, 255, 0, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let result = renderer.render_effect(
        &id,
        &view,
        None,
        RenderSize::new(1, 1).expect("non-zero size"),
        &chroma_key_params([0.0, 1.0, 0.0], 0.1, 0.0, 1.0),
        EffectRuntime::default(),
    );
    assert!(matches!(result, Err(RenderError::InvalidEffectParams)));
}

#[test]
fn three_dimensional_lut_maps_straight_rgb_and_preserves_alpha() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 1).expect("non-zero size");
    let source = rgba_image_texture(&renderer, size, &[64, 128, 192, 255, 32, 64, 96, 128]);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let mapped = render_inverting_lut(&mut renderer, &source_view, size);
    assert_rgba8_golden("lut_3d", size, &mapped, 1);
}

#[test]
fn builtin_lut_effect_inverts_through_an_inverting_cube_when_a_gpu_is_available() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    // The second pixel carries premultiplied alpha, so the effect must recover
    // straight color before the lookup and premultiply after it.
    let size = RenderSize::new(2, 1).expect("non-zero size");
    let pixels = [64, 128, 192, 255, 32, 64, 96, 128];
    let source = rgba_image_texture(&renderer, size, &pixels);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let mapped = render_inverting_lut(&mut renderer, &source_view, size);
    assert!(pollster::block_on(scope.pop()).is_none());

    // A two-point cube interpolates linearly: each straight channel c maps to 1 - c.
    let expected = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| {
            let alpha = f64::from(pixel[3]) / 255.0;
            let mut out = [0_u8; 4];
            for channel in 0..3 {
                let straight = f64::from(pixel[channel]) / 255.0 / alpha;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let code = ((1.0 - straight) * alpha * 255.0).round() as u8;
                out[channel] = code;
            }
            out[3] = pixel[3];
            out
        })
        .collect::<Vec<_>>();
    let maximum_error = expected
        .iter()
        .zip(&mapped)
        .map(|(left, right)| left.abs_diff(*right))
        .max()
        .expect("comparison contains pixels");
    assert!(
        maximum_error <= 1,
        "LUT effect diverged from the inverted color by {maximum_error} levels: {mapped:?} against {expected:?}"
    );
}

/// Draws the built-in LUT effect at full strength over `source` through a
/// two-point cube that inverts every channel.
fn render_inverting_lut(
    renderer: &mut Renderer,
    source: &wgpu::TextureView,
    size: RenderSize,
) -> Vec<u8> {
    let id = renderer
        .register_builtin_lut_3d()
        .expect("built-in LUT effect compiles");
    // .cube order, red varying fastest.
    let data = crate::Lut3dData::from_cube(
        "LUT_3D_SIZE 2\n1 1 1\n0 1 1\n1 0 1\n0 0 1\n1 1 0\n0 1 0\n1 0 0\n0 0 0\n",
    )
    .expect("inverted LUT parses");
    renderer
        .register_lut3d("inverted", &data)
        .expect("inverted LUT uploads");
    let target = renderer
        .render_effect_with_resources(
            &id,
            source,
            None,
            size,
            &EffectParams::new().with("strength", ParamValue::Float(1.0)),
            &crate::EffectResources::new().with("lut", "inverted"),
            EffectRuntime::default(),
        )
        .expect("built-in LUT renders");
    read_rgba8(renderer, &target)
}

fn centered_square(size: RenderSize) -> Vec<u8> {
    let left = size.width / 4;
    let right = size.width - left;
    let top = size.height / 4;
    let bottom = size.height - top;
    (0..size.height)
        .flat_map(|y| {
            (0..size.width).flat_map(move |x| {
                if (left..right).contains(&x) && (top..bottom).contains(&y) {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
        })
        .collect()
}

fn downsample_rgba_2x(full: &[u8], size: RenderSize) -> Vec<u8> {
    let width = size.width as usize;
    let half_width = width / 2;
    let half_height = size.height as usize / 2;
    let mut output = Vec::with_capacity(half_width * half_height * 4);
    for y in 0..half_height {
        for x in 0..half_width {
            for channel in 0..4 {
                let offsets = [
                    ((y * 2) * width + x * 2) * 4 + channel,
                    ((y * 2) * width + x * 2 + 1) * 4 + channel,
                    (((y * 2) + 1) * width + x * 2) * 4 + channel,
                    (((y * 2) + 1) * width + x * 2 + 1) * 4 + channel,
                ];
                let sum = offsets
                    .into_iter()
                    .map(|offset| u16::from(full[offset]))
                    .sum::<u16>();
                output.push(u8::try_from((sum + 2) / 4).expect("RGBA average fits in u8"));
            }
        }
    }
    output
}
