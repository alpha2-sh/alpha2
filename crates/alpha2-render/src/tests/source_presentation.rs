//! How a source is presented: external leases, crop, rotation, Exif orientation, sample aspect, alpha, and padded planes.

use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn gpu_nv12_presentation_retains_its_surface_until_queue_completion() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let (y, uv) = nv12_test_planes(&renderer, 2, 2, &[16; 4], &[128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let rejected_submitted = Arc::new(AtomicBool::new(false));
    let rejected = Arc::new(TestExternalLease::new(&rejected_submitted));
    let rejected_weak = Arc::downgrade(&rejected);
    let size = RenderSize::new(2, 2).expect("size");
    assert!(matches!(
        renderer.prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::I420,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
            rejected.clone(),
        ),
        Err(RenderError::InvalidImageData)
    ));
    drop(rejected);
    assert!(rejected_weak.upgrade().is_none());
    assert!(!rejected_submitted.load(Ordering::Acquire));

    let submitted = Arc::new(AtomicBool::new(false));
    let lease = Arc::new(TestExternalLease::new(&submitted));
    let weak = Arc::downgrade(&lease);
    let (prepared, diagnostics) = renderer
        .prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::NV12,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr,
            lease.clone(),
        )
        .expect("NV12 source prepares");
    assert_eq!(diagnostics, [] as [crate::ColorDiagnostic; 0]);
    drop(lease);
    let (texture, pending) = prepared.into_parts();
    let pending = pending.expect("conversion remains pending");
    assert_eq!(pending.retained_bytes(), 6);
    assert!(renderer.submit_pending_source(&pending));
    assert!(submitted.load(Ordering::Acquire));
    assert!(weak.upgrade().is_some());
    let pixels = read_rgba8(&renderer, &texture);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [0, 0, 0, 255])
    );
    // The callback only parks a completed lease; a bare device poll would release it only where
    // the conversion had finished by the readback's submit (Vulkan), not on WARP.
    renderer
        .reclaim_external_leases()
        .expect("queue completion");
    assert!(weak.upgrade().is_none());
}

#[test]
fn gpu_yuv_presentation_applies_contain_crop_rotation_sar_and_odd_chroma() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = RenderSize::new(3, 3).expect("source size");
    let output = RenderSize::new(6, 6).expect("output size");
    let frame = Yuv420Frame {
        y: &[16, 16, 16, 235, 235, 235, 16, 16, 16],
        u: &[128; 4],
        v: &[128; 4],
        y_stride: 3,
        chroma_stride: 2,
        size: source,
        alpha: 1.0,
        color: Some(ColorMetadata::default()),
    };
    assert_eq!(renderer.pipeline_cache_stats().builtins, 0);
    let (target, _) = renderer
        .render_yuv420p_presented(
            frame,
            SourcePresentation {
                output_size: output,
                visible_rect: Rect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 2,
                },
                orientation: Orientation::rotated(Rotation::Deg90),
                sample_aspect_ratio: (2, 1),
                chroma_siting: ChromaLocation::Left,
            },
            crate::ColorPolicy::RejectHdr,
        )
        .expect("presented YUV frame");
    let pixels = read_rgba8(&renderer, &target);
    let alpha = |x: usize, y: usize| pixels[(y * 6 + x) * 4 + 3];
    assert_eq!(alpha(0, 3), 0, "left contain bar remains transparent");
    assert_eq!(alpha(2, 3), 255, "rotated source occupies the center");
    assert_eq!(alpha(5, 3), 0, "right contain bar remains transparent");
    assert_eq!(
        renderer.pipeline_cache_stats().builtins,
        1 + rgba_readback_pipeline_count(&renderer)
    );
    assert_eq!(renderer.diagnostics().source_submissions, 1);
    let first_uploads = renderer.diagnostics().upload_textures;
    assert_eq!(first_uploads.created, 3);
    assert_eq!(first_uploads.reused, 0);
    assert_eq!(first_uploads.active, 0);
    assert_eq!(first_uploads.available, 3);
    assert_eq!(first_uploads.high_water_bytes, 17);
    let _ = renderer
        .render_yuv420p_presented(
            frame,
            SourcePresentation::identity(source),
            crate::ColorPolicy::RejectHdr,
        )
        .expect("cached YUV pipeline");
    assert_eq!(
        renderer.pipeline_cache_stats().builtins,
        1 + rgba_readback_pipeline_count(&renderer)
    );
    assert_eq!(renderer.diagnostics().source_submissions, 2);
    let second_uploads = renderer.diagnostics().upload_textures;
    assert_eq!(second_uploads.created, 3);
    assert_eq!(second_uploads.reused, 3);
    assert_eq!(second_uploads.discarded, 0);
    assert_eq!(second_uploads.active, 0);
    assert_eq!(second_uploads.available, 3);
}

/// Where the stored pixel shown at displayed `(x, y)` sits, by the Exif
/// definition of each `Orientation` value, for a stored `width` x `height`
/// picture: the reference the presentation passes are held to.
fn exif_stored_pixel(value: u16, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
    match value {
        1 => (x, y),
        2 => (width - 1 - x, y),
        3 => (width - 1 - x, height - 1 - y),
        4 => (x, height - 1 - y),
        5 => (y, x),
        6 => (y, height - 1 - x),
        7 => (width - 1 - y, height - 1 - x),
        8 => (width - 1 - y, x),
        _ => unreachable!("Exif orientations are 1-8"),
    }
}

/// Both presentation passes show each of the eight Exif orientations, four
/// of them mirrored, the way the Exif definition places the stored pixels.
/// At the displayed size every output pixel samples one stored pixel's centre.
#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Small test values.
fn presentation_shows_every_exif_orientation_upright() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let (width, height) = (4_u32, 2_u32);
    let source = RenderSize::new(width, height).expect("source size");
    // Eight distinct opaque colours, one per stored pixel, and eight luma levels.
    let colour = |x: u32, y: u32| {
        [
            (x * 60) as u8,
            (y * 200) as u8,
            40 + (x + y * 4) as u8 * 20,
            255,
        ]
    };
    let rgba = (0..height)
        .flat_map(|y| (0..width).flat_map(move |x| colour(x, y)))
        .collect::<Vec<_>>();
    let luma = (0..height)
        .flat_map(|y| (0..width).map(move |x| 16 + 25 * (x + y * width) as u8))
        .collect::<Vec<_>>();
    let rgba_source = renderer.upload_rgba8(source, &rgba).expect("source upload");
    for value in 1..=8 {
        let orientation = Orientation::from_exif(value).expect("Exif orientation");
        let (shown_width, shown_height) = orientation.displayed_size(width, height);
        let output = RenderSize::new(shown_width, shown_height).expect("output size");
        let presentation = SourcePresentation {
            output_size: output,
            visible_rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            orientation,
            sample_aspect_ratio: (1, 1),
            chroma_siting: ChromaLocation::Unspecified,
        };
        let presented = read_rgba8(
            &renderer,
            &renderer
                .render_rgba_presented(
                    rgba_source.texture(),
                    source,
                    presentation,
                    crate::SourceAlpha::Straight,
                )
                .expect("RGBA presentation"),
        );
        let (yuv, _) = renderer
            .render_yuv420p_presented(
                Yuv420Frame {
                    y: &luma,
                    u: &[128; 2],
                    v: &[128; 2],
                    y_stride: width,
                    chroma_stride: 2,
                    size: source,
                    alpha: 1.0,
                    color: Some(ColorMetadata::default()),
                },
                presentation,
                crate::ColorPolicy::RejectHdr,
            )
            .expect("presented YUV frame");
        let yuv = read_rgba8(&renderer, &yuv);
        // Full-range-expanded luma of the limited-range level, before the sRGB round trip
        // that leaves it as it was.
        let gray = |level: u8| ((f32::from(level) - 16.0) * 255.0 / 219.0).round() as u8;
        for y in 0..shown_height {
            for x in 0..shown_width {
                let (stored_x, stored_y) = exif_stored_pixel(value, x, y, width, height);
                let at = ((y * shown_width + x) * 4) as usize;
                let expected = colour(stored_x, stored_y);
                for channel in 0..4 {
                    assert!(
                        presented[at + channel].abs_diff(expected[channel]) <= 1,
                        "Exif {value} RGBA ({x}, {y}): {:?}, stored ({stored_x}, {stored_y}) is {expected:?}",
                        &presented[at..at + 4],
                    );
                }
                let level = luma[(stored_y * width + stored_x) as usize];
                assert!(
                    yuv[at].abs_diff(gray(level)) <= 2,
                    "Exif {value} YUV ({x}, {y}): {}, stored ({stored_x}, {stored_y}) is {}",
                    yuv[at],
                    gray(level),
                );
            }
        }
    }
}

#[test]
fn gpu_yuv_upload_pool_retains_observed_plane_concurrency() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 4).expect("source size");
    let y = [16_u8; 16];
    let u = [128_u8; 4];
    let v = [128_u8; 4];
    let frame = Yuv420Frame {
        y: &y,
        u: &u,
        v: &v,
        y_stride: 4,
        chroma_stride: 2,
        size,
        alpha: 1.0,
        color: Some(ColorMetadata::default()),
    };
    let prepare_batch = || {
        (0..5)
            .map(|_| {
                let (prepared, diagnostics) = renderer
                    .prepare_yuv420p_presented(
                        frame,
                        SourcePresentation::identity(size),
                        ColorPolicy::RejectHdr,
                    )
                    .expect("prepared YUV source");
                assert_eq!(diagnostics, [] as [crate::ColorDiagnostic; 0]);
                prepared.into_parts()
            })
            .collect::<Vec<_>>()
    };
    let submit_batch = |batch: Vec<(RenderedTexture, Option<crate::PendingSourceWork>)>| {
        for (texture, pending) in batch {
            assert!(
                renderer.submit_pending_source(&pending.expect("YUV presentation remains pending"))
            );
            drop(texture);
        }
    };

    let first = prepare_batch();
    let active = renderer.diagnostics().upload_textures;
    assert_eq!(active.created, 15);
    assert_eq!(active.active, 15);
    submit_batch(first);
    let idle = renderer.diagnostics().upload_textures;
    assert_eq!(idle.available, 15);
    assert_eq!(idle.discarded, 0);

    let second = prepare_batch();
    let reused = renderer.diagnostics().upload_textures;
    assert_eq!(reused.created, 15);
    assert_eq!(reused.reused, 15);
    submit_batch(second);
    let final_stats = renderer.diagnostics().upload_textures;
    assert_eq!(final_stats.available, 15);
    assert_eq!(final_stats.discarded, 0);
}

#[test]
fn uniform_arenas_are_reused_at_any_source_count_and_graph_size() {
    // Each source holds its arena until submission; composite arenas grow with layer count.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 4).expect("source size");
    let y = [16_u8; 16];
    let u = [128_u8; 4];
    let v = [128_u8; 4];
    let frame = Yuv420Frame {
        y: &y,
        u: &u,
        v: &v,
        y_stride: 4,
        chroma_stride: 2,
        size,
        alpha: 1.0,
        color: Some(ColorMetadata::default()),
    };
    let render_frame = |layers: usize| {
        let batch = (0..5)
            .map(|_| {
                renderer
                    .prepare_yuv420p_presented(
                        frame,
                        SourcePresentation::identity(size),
                        ColorPolicy::RejectHdr,
                    )
                    .expect("prepared YUV source")
                    .0
                    .into_parts()
            })
            .collect::<Vec<_>>();
        for (_, pending) in &batch {
            assert!(renderer.submit_pending_source(pending.as_ref().expect("pending")));
        }
        let views = batch
            .iter()
            .map(|(texture, _)| {
                texture
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default())
            })
            .collect::<Vec<_>>();
        let composite = views
            .iter()
            .take(layers)
            .map(|view| CompositeLayer {
                texture: view,
                visual: visual([0.5, 0.5], 0.8),
                blend_mode: BlendMode::Normal,
            })
            .collect::<Vec<_>>();
        drop(renderer.render_layers(size, &composite).expect("composite"));
    };
    for layers in 1..=5 {
        render_frame(layers);
    }
    let warm = renderer.diagnostics().uniform_buffers;
    for frame in 0..20 {
        render_frame(frame % 5 + 1);
    }
    let after = renderer.diagnostics().uniform_buffers;
    assert_eq!(
        after.created - warm.created,
        0,
        "arenas created after warm-up ({} discarded)",
        after.discarded - warm.discarded
    );
}

#[test]
fn gpu_rgba_presentation_contains_rotates_and_premultiplies() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source_size = RenderSize::new(1, 2).expect("source size");
    let output_size = RenderSize::new(4, 4).expect("output size");
    let source = renderer
        .upload_rgba8(source_size, &[255, 0, 0, 128, 255, 0, 0, 128])
        .expect("source upload");
    let target = renderer
        .render_rgba_presented(
            source.texture(),
            source_size,
            SourcePresentation {
                output_size,
                visible_rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 2,
                },
                orientation: Orientation::rotated(Rotation::Deg90),
                sample_aspect_ratio: (1, 1),
                chroma_siting: ChromaLocation::Unspecified,
            },
            crate::SourceAlpha::Straight,
        )
        .expect("RGBA presentation");
    let pixels = read_rgba8(&renderer, &target);
    let pixel = |x: usize, y: usize| &pixels[(y * 4 + x) * 4..(y * 4 + x + 1) * 4];
    assert_eq!(pixel(0, 0), [0, 0, 0, 0]);
    assert!(pixel(1, 2)[0].abs_diff(128) <= 1);
    assert_eq!(pixel(1, 2)[1], 0);
    assert!(pixel(1, 2)[3].abs_diff(128) <= 1);
    assert_eq!(pixel(3, 3), [0, 0, 0, 0]);
    assert_eq!(
        renderer.pipeline_cache_stats().builtins,
        1 + rgba_readback_pipeline_count(&renderer)
    );
}

/// A YCbCr frame with an alpha plane is presented premultiplied by it: a
/// white square on a transparent black surround, scaled up 4x, stays white
/// under its alpha at every edge (each luma sample converted and
/// premultiplied before it is filtered), a constant half alpha halves every
/// component, and the opaque middle is the opaque format's own conversion.
#[test]
fn gpu_yuv_with_an_alpha_plane_is_premultiplied_before_it_is_filtered() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 4).expect("source size");
    let output_size = RenderSize::new(16, 16).expect("output size");
    let inside = |index: usize| (1..3).contains(&(index % 4)) && (1..3).contains(&(index / 4));
    // 10-bit full range: white (Y 1023) inside, black (Y 0) around it; neutral
    // chroma.
    let luma = widen16(
        &(0..16)
            .map(|index| if inside(index) { 1023 } else { 0 })
            .collect::<Vec<_>>(),
    );
    let chroma = widen16(&[512; 16]);
    let present = |alpha: &[u8]| {
        let presentation = SourcePresentation {
            visible_rect: Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            ..SourcePresentation::identity(output_size)
        };
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let (prepared, _) = renderer
            .prepare_yuv_presented(
                crate::YuvFrame {
                    format: alpha2_media::YuvFormat::planar(0, 0, 10).with_alpha(),
                    planes: [plane(&luma, 8), plane(&chroma, 8), plane(&chroma, 8)],
                    alpha_plane: Some(plane(alpha, 8)),
                    size,
                    alpha: 1.0,
                    color: Some(full_range_bt709()),
                },
                presentation,
                ColorPolicy::RejectHdr,
            )
            .expect("YUV with alpha");
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending conversion")));
        let pixels = read_rgba8(&renderer, &texture);
        assert!(pollster::block_on(scope.pop()).is_none(), "validation");
        pixels
    };
    let square = present(&widen16(
        &(0..16)
            .map(|index| if inside(index) { 1023 } else { 0 })
            .collect::<Vec<_>>(),
    ));
    let mut edges = 0;
    for pixel in square.as_chunks::<4>().0 {
        if pixel[3] > 0 && pixel[3] < 255 {
            edges += 1;
        }
        for channel in 0..3 {
            assert!(
                pixel[channel].abs_diff(pixel[3]) <= 1,
                "premultiplied {pixel:?} is not white under its alpha"
            );
        }
    }
    assert!(edges > 0, "the scale left no partly covered edge");
    assert_eq!(
        square[(8 * 16 + 8) * 4..(8 * 16 + 8) * 4 + 4],
        [255; 4],
        "the opaque middle"
    );
    let half = present(&widen16(&[512; 16]));
    for (index, pixel) in half.as_chunks::<4>().0.iter().enumerate() {
        assert!(
            pixel[3].abs_diff(128) <= 1,
            "half alpha at {index}: {pixel:?}"
        );
    }
    assert_eq!(half[0..3], [0; 3], "black in the corner");
    let middle = &half[(8 * 16 + 8) * 4..(8 * 16 + 8) * 4 + 4];
    assert!(
        middle[..3].iter().all(|value| value.abs_diff(128) <= 1),
        "white at half alpha: {middle:?}"
    );
}

/// Little-endian 16-bit containers of `codes`.
fn widen16(codes: &[u16]) -> Vec<u8> {
    codes.iter().flat_map(|code| code.to_le_bytes()).collect()
}

/// A white square on a transparent black surround, scaled up 4x: every
/// presented pixel must stay white under its alpha (premultiplied red equal to
/// alpha).
#[test]
fn gpu_rgba_presentation_scales_transparent_edges_without_a_dark_fringe() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source_size = RenderSize::new(4, 4).expect("source size");
    let output_size = RenderSize::new(16, 16).expect("output size");
    let pixels = (0..16)
        .flat_map(|index| {
            let (x, y) = (index % 4, index / 4);
            if (1..3).contains(&x) && (1..3).contains(&y) {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 0]
            }
        })
        .collect::<Vec<u8>>();
    let presentation = SourcePresentation::identity(output_size);
    let presentation = SourcePresentation {
        visible_rect: Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
        ..presentation
    };
    // A premultiplied source holds the same bytes here: white is opaque and the surround
    // is zero either way.
    for alpha in [
        crate::SourceAlpha::Straight,
        crate::SourceAlpha::Premultiplied,
    ] {
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let source = renderer
            .upload_rgba8(source_size, &pixels)
            .expect("source upload");
        let target = renderer
            .render_rgba_presented(source.texture(), source_size, presentation, alpha)
            .expect("RGBA presentation");
        let presented = read_rgba8(&renderer, &target);
        assert!(pollster::block_on(scope.pop()).is_none(), "{alpha:?}");
        let mut edges = 0;
        for pixel in presented.as_chunks::<4>().0 {
            if pixel[3] > 0 && pixel[3] < 255 {
                edges += 1;
            }
            assert!(
                pixel[0].abs_diff(pixel[3]) <= 1,
                "{alpha:?}: premultiplied {pixel:?} is not white under its alpha"
            );
        }
        assert!(
            edges > 0,
            "{alpha:?}: the scale left no partly covered edge"
        );
    }
}

/// A premultiplied source is filtered as it is; premultiplying it again would
/// square its alpha into its color.
#[test]
fn gpu_rgba_presentation_keeps_premultiplied_sources_as_they_are() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(1, 1).expect("size");
    let source = renderer
        .upload_rgba8(size, &[128, 0, 0, 128])
        .expect("source upload");
    let target = renderer
        .render_rgba_presented(
            source.texture(),
            size,
            SourcePresentation::identity(size),
            crate::SourceAlpha::Premultiplied,
        )
        .expect("RGBA presentation");
    let pixel = read_rgba8(&renderer, &target);
    assert!(pixel[0].abs_diff(128) <= 1, "{pixel:?}");
    assert!(pixel[3].abs_diff(128) <= 1, "{pixel:?}");
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn f32_to_half(value: f32) -> u16 {
    // Exact for the values these tests use: normal halves with short fractions.
    if value == 0.0 {
        return 0;
    }
    let sign = if value < 0.0 { 0x8000 } else { 0 };
    let magnitude = value.abs();
    let exponent = magnitude.log2().floor() as i32;
    let fraction = (magnitude / 2.0_f32.powi(exponent) - 1.0) * 1024.0;
    sign | (((exponent + 15) as u16) << 10) | fraction.round() as u16
}

fn srgb_encode(value: f32) -> f32 {
    let magnitude = value.abs();
    let encoded = if magnitude <= 0.003_130_8 {
        12.92 * magnitude
    } else {
        1.055 * magnitude.powf(1.0 / 2.4) - 0.055
    };
    encoded.copysign(value)
}

/// Linear half-float sources are stored as every presented source is,
/// sRGB-encoded and premultiplied, but with light above SDR white and
/// negative components kept where the intermediate is half float.
#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn gpu_linear_sources_keep_values_beyond_sdr_through_presentation() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 1).expect("size");
    // Premultiplied linear: over-white, negative, half-transparent, and color with no
    // alpha.
    let pixels = [
        [2.0, 0.5, -0.25, 1.0],
        [4.0, 0.0, 0.0, 0.5],
        [0.25, 0.25, 0.25, 0.5],
        [1.0, 1.0, 1.0, 0.0],
    ];
    let halves = pixels
        .iter()
        .flatten()
        .map(|value| f32_to_half(*value))
        .collect::<Vec<_>>();
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let upload = renderer
        .upload_linear_rgba16f(size, &halves)
        .expect("upload");
    let (target, pending) = renderer
        .prepare_linear_presented(upload, size, SourcePresentation::identity(size))
        .expect("presentation")
        .into_parts();
    renderer.submit_pending_source(&pending.expect("pending work"));
    assert!(pollster::block_on(scope.pop()).is_none());
    let expected = pixels.map(|[red, green, blue, alpha]| {
        if alpha == 0.0 {
            return [0.0; 4];
        }
        let encode = |value: f32| srgb_encode(value / alpha) * alpha;
        [encode(red), encode(green), encode(blue), alpha]
    });
    if renderer.intermediate_format() == wgpu::TextureFormat::Rgba16Float {
        let stored = read_rgba16f(&renderer, &target);
        for (pixel, expected) in stored.as_chunks::<4>().0.iter().zip(&expected) {
            for (value, expected) in pixel.iter().zip(expected) {
                // Half floats hold about three decimal digits.
                assert!(
                    (value - expected).abs() <= expected.abs() * 2e-3 + 1e-3,
                    "{stored:?} is not {expected:?}"
                );
            }
        }
    }
    // Where it is shown or exported, SDR white is the most there is.
    let shown = read_rgba8(&renderer, &target);
    let expected8 = expected
        .iter()
        .flatten()
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as i32)
        .collect::<Vec<_>>();
    for (value, expected) in shown.iter().zip(expected8) {
        assert!((i32::from(*value) - expected).abs() <= 1, "{shown:?}");
    }
}

#[test]
fn gpu_plane_presentation_samples_only_the_visible_rect_of_a_padded_image() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    // Hardware decoders allocate macroblock-padded images: sampling must normalize against the
    // allocated extent, not the display size, or the picture is rescaled.
    let coded = RenderSize::new(2, 4).expect("even size");
    let visible = RenderSize::new(2, 2).expect("even size");
    let luma = [16, 16, 16, 16, 235, 235, 235, 235];
    let (y, uv) = nv12_test_planes(&renderer, 2, 4, &luma, &[128, 128, 128, 128]);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let submitted = Arc::new(AtomicBool::new(false));
    let presentation = SourcePresentation {
        output_size: visible,
        visible_rect: Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        },
        orientation: Orientation::UPRIGHT,
        sample_aspect_ratio: (1, 1),
        chroma_siting: ChromaLocation::Center,
    };
    let (prepared, diagnostics) = renderer
        .prepare_gpu_yuv_presented(
            GpuVideoPlanes {
                y: &y_view,
                chroma: &uv_view,
                v: None,
                format: alpha2_media::YuvFormat::NV12,
                size: coded,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            presentation,
            ColorPolicy::RejectHdr,
            Arc::new(TestExternalLease::new(&submitted)),
        )
        .expect("padded NV12 source prepares");
    assert_eq!(diagnostics, [] as [crate::ColorDiagnostic; 0]);
    let (texture, pending) = prepared.into_parts();
    assert!(renderer.submit_pending_source(&pending.expect("conversion remains pending")));
    let pixels = read_rgba8(&renderer, &texture);
    // Treating the padded rows as visible would blend the 235-code padding into the bottom.
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [0, 0, 0, 255]),
        "padding leaked into the presented image: {pixels:?}"
    );
}
