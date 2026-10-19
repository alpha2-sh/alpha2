//! Packing renders into YUV: I420 and NV12 packs, caller buffers, and consumers of unwritten buffers.

use super::*;

#[test]
fn gpu_yuv420_readback_produces_limited_range_planes() {
    let _lock = gpu_test_lock();
    for gpu in gpus_by_intermediate() {
        yuv420_readback_on(gpu);
    }
}

fn yuv420_readback_on(gpu: GpuContext) {
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(8, 4).expect("even size");
    for (color, expected_y) in [
        (Color::rgba(0.0, 0.0, 0.0, 1.0), 16_u8),
        (Color::rgba(1.0, 1.0, 1.0, 1.0), 235_u8),
    ] {
        let target = renderer.render_solid(size, color).expect("solid target");
        let planes = renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("YUV readback submits")
            .wait()
            .expect("YUV readback maps");
        assert_eq!(planes.y.len(), 32);
        assert_eq!(planes.u.len(), 8);
        assert_eq!(planes.v.len(), 8);
        assert!(planes.y.iter().all(|value| value.abs_diff(expected_y) <= 1));
        assert!(planes.u.iter().all(|value| value.abs_diff(128) <= 1));
        assert!(planes.v.iter().all(|value| value.abs_diff(128) <= 1));
    }
    let target = renderer
        .render_solid(size, Color::rgba(0.0, 0.0, 0.0, 1.0))
        .expect("solid target");
    let readback = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("packed I420 readback submits");
    readback.wait_ready().expect("packed I420 maps");
    let packed = i420_by_plane_layout(readback);
    assert_eq!(packed.len(), 48);
    assert!(packed[..32].iter().all(|value| value.abs_diff(16) <= 1));
    assert!(packed[32..].iter().all(|value| value.abs_diff(128) <= 1));
    let diagnostics = renderer.diagnostics();
    assert_eq!(diagnostics.readback_submissions, 3);
    assert_eq!(diagnostics.render_passes, 6);
    assert_eq!(diagnostics.copy_operations, 9);
    assert_eq!(diagnostics.copy_bytes, 3 * 48);
}

#[test]
fn gpu_packing_into_caller_buffers_matches_the_allocating_pack() {
    // The native path packs straight into encoder plane memory and must land on the browser
    // path's bytes, strided rows included.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    let expected = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("readback submits")
        .wait()
        .expect("readback maps");

    // Strides wider than the plane, which is how FFmpeg aligns its rows.
    let (luma_stride, chroma_stride) = (11_usize, 7_usize);
    let mut y = vec![0xAA_u8; luma_stride * 3 + 8];
    let mut u = vec![0xAA_u8; chroma_stride + 4];
    let mut v = vec![0xAA_u8; chroma_stride + 4];
    let readback = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("second readback submits");
    readback.wait_ready().expect("second readback maps");
    readback
        .finish_into(&mut Yuv420pTargets {
            y: PlaneTarget::new(&mut y, luma_stride),
            u: PlaneTarget::new(&mut u, chroma_stride),
            v: PlaneTarget::new(&mut v, chroma_stride),
        })
        .expect("packs into caller buffers");

    let rows = |bytes: &[u8], stride: usize, width: usize, height: usize| {
        (0..height)
            .flat_map(|row| bytes[row * stride..row * stride + width].to_vec())
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&y, luma_stride, 8, 4), expected.y);
    assert_eq!(rows(&u, chroma_stride, 4, 2), expected.u);
    assert_eq!(rows(&v, chroma_stride, 4, 2), expected.v);
    // Padding between rows is the caller's, and must be left alone.
    assert_eq!(y[8..luma_stride], [0xAA; 3]);

    let mut small = vec![0_u8; 4];
    let short = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("third readback submits");
    short.wait_ready().expect("third readback maps");
    let mut spare_u = vec![0_u8; 8];
    let mut spare_v = vec![0_u8; 8];
    assert!(
        short
            .finish_into(&mut Yuv420pTargets {
                y: PlaneTarget::new(&mut small, 8),
                u: PlaneTarget::new(&mut spare_u, 4),
                v: PlaneTarget::new(&mut spare_v, 4),
            })
            .is_err()
    );
}

#[test]
fn gpu_joint_uv_pack_preserves_chroma_math_and_i420_layout() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 2).expect("even size");
    let blocks = [
        [[255, 0, 0, 255]; 4],
        [
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
            [0, 0, 0, 255],
        ],
        [
            [255, 0, 255, 255],
            [255, 0, 255, 128],
            [255, 0, 255, 64],
            [255, 255, 255, 0],
        ],
        [
            [255, 255, 0, 255],
            [0, 255, 255, 255],
            [255, 0, 255, 255],
            [255, 255, 255, 255],
        ],
    ];
    let pixels = (0..2)
        .flat_map(|row| {
            blocks
                .iter()
                .flat_map(move |block| [block[row * 2], block[row * 2 + 1]].into_iter().flatten())
        })
        .collect::<Vec<_>>();
    let source = rgba_image_texture(&renderer, size, &pixels);
    let target = renderer
        .render_rgba_presented(
            &source,
            size,
            SourcePresentation::identity(size),
            crate::SourceAlpha::Straight,
        )
        .expect("straight RGBA source presents");

    for matrix in [
        ColorMatrix::Bt601,
        ColorMatrix::Bt709,
        ColorMatrix::Bt2020Ncl,
        ColorMatrix::Rgb,
    ] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let metadata = ColorMetadata {
                matrix,
                range,
                ..ColorMetadata::default()
            };
            let expected = blocks
                .iter()
                .map(|block| expected_chroma(*block, metadata))
                .collect::<Vec<_>>();
            let planes = renderer
                .begin_yuv420p_readback(&target, centred(metadata))
                .expect("joint UV readback submits")
                .wait()
                .expect("joint UV readback maps");
            assert_eq!(planes.u.len(), blocks.len());
            assert_eq!(planes.v.len(), blocks.len());
            for (index, (expected_u, expected_v)) in expected.iter().copied().enumerate() {
                assert!(
                    planes.u[index].abs_diff(expected_u) <= 1,
                    "{matrix:?} {range:?} block {index} U: {} != {expected_u}",
                    planes.u[index]
                );
                assert!(
                    planes.v[index].abs_diff(expected_v) <= 1,
                    "{matrix:?} {range:?} block {index} V: {} != {expected_v}",
                    planes.v[index]
                );
            }

            let readback = renderer
                .begin_yuv420p_readback(&target, centred(metadata))
                .expect("packed I420 readback submits");
            readback.wait_ready().expect("packed I420 readback maps");
            let packed = i420_by_plane_layout(readback);
            let expected_packed = planes
                .y
                .iter()
                .chain(&planes.u)
                .chain(&planes.v)
                .copied()
                .collect::<Vec<_>>();
            assert_eq!(packed, expected_packed);
        }
    }
}

#[test]
fn readback_rejects_a_target_format_it_cannot_pack() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(4, 4).expect("non-zero size");
    let texture = renderer
        .upload_rgba8(size, &[0_u8; 4 * 4 * 4])
        .expect("uploaded source");
    let unsupported = crate::RenderedTexture::owned(
        size,
        wgpu::TextureFormat::Rg8Unorm,
        texture.texture().clone(),
    );
    assert!(matches!(
        renderer.begin_rgba8_readback(&unsupported),
        Err(crate::RenderError::TextureReadback(
            crate::ReadbackFailure::UnsupportedTarget
        ))
    ));
}

#[test]
fn the_nv12_pack_separates_into_exactly_the_i420_pack_samples() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(16, 8).expect("even size");
    for color in [
        Color::rgba(0.0, 0.0, 0.0, 1.0),
        Color::rgba(1.0, 1.0, 1.0, 1.0),
        Color::rgba(0.25, 0.5, 0.75, 1.0),
        Color::rgba(0.9, 0.1, 0.4, 1.0),
    ] {
        let target = renderer.render_solid(size, color).expect("solid target");
        // The NV12 pack runs first: after the I420 readback its pooled uniforms could inherit an
        // uploaded arena and pass while uploading nothing.
        let (luma, chroma) = nv12_planes(&renderer, &target, size);
        let expected = renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("I420 readback submits")
            .wait()
            .expect("I420 readback maps");

        assert_eq!(luma, expected.y, "luma plane must match the I420 pack");
        let (u, v): (Vec<u8>, Vec<u8>) = chroma
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0], pair[1]))
            .unzip();
        assert_eq!(u, expected.u, "deinterleaved U must match the I420 pack");
        assert_eq!(v, expected.v, "deinterleaved V must match the I420 pack");
    }
}

#[test]
fn an_nv12_pack_keeps_its_uniforms_until_the_caller_submits() {
    // The pack is recorded into the caller's encoder; returned to the pool at record time, its
    // uniforms could be overwritten by the next renderer call before that submission.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    let solo = nv12_planes(&renderer, &target, size);
    let interleaved = nv12_planes_with(&renderer, &target, size, || {
        let texture = target
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        renderer
            .render_layers(
                size,
                &[CompositeLayer {
                    texture: &texture,
                    visual: visual([0.5, 0.5], 0.5),
                    blend_mode: BlendMode::Normal,
                }],
            )
            .expect("layers render");
        renderer
            .begin_yuv420p_readback(
                &target,
                centred(ColorMetadata {
                    matrix: ColorMatrix::Bt601,
                    range: ColorRange::Full,
                    ..ColorMetadata::default()
                }),
            )
            .expect("readback submits")
            .wait()
            .expect("readback maps");
    });
    assert_eq!(interleaved, solo);
}

#[test]
fn nv12_plane_targets_that_cannot_be_packed_into_are_refused_before_recording() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let target = renderer
        .render_solid(size, Color::rgba(0.25, 0.5, 0.75, 1.0))
        .expect("solid target");
    let plane = |width, height, format, usage| {
        renderer
            .gpu
            .device()
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("nv12 refused plane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
    };
    let attachment = wgpu::TextureUsages::RENDER_ATTACHMENT;
    let luma = plane(8, 4, wgpu::TextureFormat::R8Unorm, attachment);
    let chroma = plane(4, 2, wgpu::TextureFormat::Rg8Unorm, attachment);
    for (label, luma, chroma) in [
        (
            "sampling-only luma",
            &plane(
                8,
                4,
                wgpu::TextureFormat::R8Unorm,
                wgpu::TextureUsages::TEXTURE_BINDING,
            ),
            &chroma,
        ),
        (
            "full-size chroma",
            &luma,
            &plane(8, 4, wgpu::TextureFormat::Rg8Unorm, attachment),
        ),
        (
            "16-bit luma",
            &plane(8, 4, wgpu::TextureFormat::Rg8Unorm, attachment),
            &chroma,
        ),
    ] {
        let mut encoder = renderer
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let packed = renderer.encode_nv12_pack(
            &target,
            crate::Nv12PlaneTargets { luma, chroma },
            centred(ColorMetadata::default()),
            &mut encoder,
        );
        assert!(
            matches!(packed, Err(RenderError::InvalidPlaneTarget)),
            "{label}: {:?}",
            packed.err()
        );
        drop(encoder);
        assert!(pollster::block_on(scope.pop()).is_none(), "{label}");
    }
}

#[test]
fn every_i420_unpacking_agrees_with_the_nv12_pack_across_many_chroma_rows() {
    // Several chroma rows put V off a 512-byte boundary (200 wide: 100 bytes in); every
    // unpacking must agree with the others and with the NV12 pack.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(200, 16).expect("even size");
    let (width, height) = (200_usize, 16_usize);
    let code = |index: usize, step: usize, base: usize| {
        u8::try_from(base + index * step % 200).expect("in range")
    };
    let y = (0..width * height)
        .map(|index| code(index, 7, 20))
        .collect::<Vec<_>>();
    let u = (0..width * height / 4)
        .map(|index| code(index, 13, 30))
        .collect::<Vec<_>>();
    let v = (0..width * height / 4)
        .map(|index| code(index, 29, 25))
        .collect::<Vec<_>>();
    let (target, _) = renderer
        .render_yuv420p(
            Yuv420Frame {
                y: &y,
                u: &u,
                v: &v,
                y_stride: 200,
                chroma_stride: 100,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            crate::ColorPolicy::RejectHdr,
        )
        .expect("varying source renders");
    let readback = || {
        renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("readback submits")
    };
    let planes = readback().wait().expect("planes");
    let i420 = {
        let pending = readback();
        pending.wait_ready().expect("maps");
        i420_by_plane_layout(pending)
    };
    let mut into = (
        vec![0_u8; 200 * 16],
        vec![0_u8; 100 * 8],
        vec![0_u8; 100 * 8],
    );
    {
        let pending = readback();
        pending.wait_ready().expect("maps");
        pending
            .finish_into(&mut Yuv420pTargets {
                y: PlaneTarget::new(&mut into.0, 200),
                u: PlaneTarget::new(&mut into.1, 100),
                v: PlaneTarget::new(&mut into.2, 100),
            })
            .expect("packs into targets");
    }
    assert!(
        planes
            .u
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 8
    );
    assert_eq!(
        i420,
        [planes.y.clone(), planes.u.clone(), planes.v.clone()].concat()
    );
    assert_eq!(
        (into.0, into.1, into.2),
        (planes.y.clone(), planes.u.clone(), planes.v.clone())
    );
    let (luma, chroma) = nv12_planes(&renderer, &target, size);
    assert_eq!(luma, planes.y);
    let (nv12_u, nv12_v): (Vec<u8>, Vec<u8>) = chroma
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0], pair[1]))
        .unzip();
    assert_eq!(nv12_u, planes.u);
    assert_eq!(nv12_v, planes.v);
}

#[test]
fn the_nv12_pack_matches_the_i420_pack_on_varying_content() {
    // Varying luma and chroma, so a transposed, offset or half-resolution sample shows; a solid
    // colour carries the same value at every coordinate.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(8, 4).expect("even size");
    let luma_samples: Vec<u8> = (0..32_u32)
        .map(|index| 16 + u8::try_from(index * 6 % 219).expect("in range"))
        .collect();
    let u_samples: Vec<u8> = (0..8_u32)
        .map(|index| 16 + u8::try_from(index * 27 % 219).expect("in range"))
        .collect();
    let v_samples: Vec<u8> = (0..8_u32)
        .map(|index| 235 - u8::try_from(index * 19 % 219).expect("in range"))
        .collect();
    let (target, _) = renderer
        .render_yuv420p(
            Yuv420Frame {
                y: &luma_samples,
                u: &u_samples,
                v: &v_samples,
                y_stride: 8,
                chroma_stride: 4,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            crate::ColorPolicy::RejectHdr,
        )
        .expect("varying source renders");

    let (luma, chroma) = nv12_planes(&renderer, &target, size);
    let expected = renderer
        .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
        .expect("I420 readback submits")
        .wait()
        .expect("I420 readback maps");
    assert!(
        expected
            .y
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1,
        "the source must actually vary or this proves nothing"
    );
    assert_eq!(luma, expected.y, "luma plane must match the I420 pack");
    let (u, v): (Vec<u8>, Vec<u8>) = chroma
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0], pair[1]))
        .unzip();
    assert_eq!(u, expected.u, "deinterleaved U must match the I420 pack");
    assert_eq!(v, expected.v, "deinterleaved V must match the I420 pack");
}

/// Packs `target` through the NV12 path and reads both planes back.
///
/// The readback here belongs to the test; the pack itself writes only
/// textures, which is the purpose of the entry point it exercises.
fn nv12_planes(
    renderer: &Renderer,
    target: &RenderedTexture,
    size: RenderSize,
) -> (Vec<u8>, Vec<u8>) {
    nv12_planes_with(renderer, target, size, || {})
}

/// [`nv12_planes`], running `between` after the pack is recorded and before
/// the caller submits it.
fn nv12_planes_with(
    renderer: &Renderer,
    target: &RenderedTexture,
    size: RenderSize,
    between: impl FnOnce(),
) -> (Vec<u8>, Vec<u8>) {
    let context = gpu();
    let chroma_size = RenderSize {
        width: size.width / 2,
        height: size.height / 2,
    };
    let luma = nv12_test_plane(
        &context,
        "nv12 test luma",
        size,
        wgpu::TextureFormat::R8Unorm,
    );
    let chroma = nv12_test_plane(
        &context,
        "nv12 test chroma",
        chroma_size,
        wgpu::TextureFormat::Rg8Unorm,
    );
    // Both planes carry the same bytes per row: luma is full width at one byte per texel,
    // chroma is half width at two.
    let row = size
        .width
        .next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let luma_buffer = nv12_test_readback(&context, "nv12 luma readback", row, size.height);
    let chroma_buffer =
        nv12_test_readback(&context, "nv12 chroma readback", row, chroma_size.height);
    let mut encoder = context
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("nv12 test encoder"),
        });
    let pack = renderer
        .encode_nv12_pack(
            target,
            crate::Nv12PlaneTargets {
                luma: &luma,
                chroma: &chroma,
            },
            centred(ColorMetadata::default()),
            &mut encoder,
        )
        .expect("NV12 pack encodes");
    for (texture, buffer, extent) in [
        (&luma, &luma_buffer, size),
        (&chroma, &chroma_buffer, chroma_size),
    ] {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(extent.height),
                },
            },
            wgpu::Extent3d {
                width: extent.width,
                height: extent.height,
                depth_or_array_layers: 1,
            },
        );
    }
    between();
    context.submit([encoder.finish()]);
    drop(pack);
    (
        unpad_rows(&context, &luma_buffer, row, size.width, size.height),
        unpad_rows(
            &context,
            &chroma_buffer,
            row,
            size.width,
            chroma_size.height,
        ),
    )
}

/// One way of finishing a mapped YUV readback.
type YuvConsumer = fn(crate::Yuv420pReadback) -> Result<(), RenderError>;

#[test]
fn every_yuv_readback_consumer_refuses_a_buffer_the_gpu_did_not_write() {
    // A readback the GPU ran always carries a canary, so a consumer that skipped the check would
    // pass on that alone; behind macOS's ignored submissions it would encode stale bytes.
    let _lock = gpu_test_lock();
    let consumers: [(&str, YuvConsumer); 3] = [
        ("finish", |readback| readback.finish().map(|_| ())),
        ("finish_into", |readback| {
            let (mut y, mut u, mut v) = (vec![0; 32], vec![0; 8], vec![0; 8]);
            readback.finish_into(&mut Yuv420pTargets {
                y: PlaneTarget::new(&mut y, 8),
                u: PlaneTarget::new(&mut u, 4),
                v: PlaneTarget::new(&mut v, 4),
            })
        }),
        ("finish_mapped", |readback| readback.finish_mapped(|_| ())),
    ];
    for (name, consume) in consumers {
        let renderer = Renderer::new(gpu());
        let size = RenderSize::new(8, 4).expect("even size");
        let target = renderer
            .render_solid(size, Color::rgba(0.25, 0.6, 0.75, 1.0))
            .expect("solid target");
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        // A readback the GPU ran, through the same consumer: it passes, and its staging
        // buffer, frame and canary included, goes back to the pool.
        let ran = renderer
            .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
            .expect("readback submits");
        ran.wait_ready().expect("maps");
        consume(ran).unwrap_or_else(|error| panic!("{name}: a readback the GPU ran: {error}"));
        let pooled = renderer.readback_buffers.stats();
        assert_eq!(pooled.available, 1, "{name}");
        // Encoded and never submitted, as behind an ignored submission: the pooled buffer
        // maps on time still holding the earlier frame and its canary.
        let mut encoder = renderer
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let unsubmitted = renderer
            .encode_yuv420p_readback(&target, centred(ColorMetadata::default()), &mut encoder)
            .expect("encodes");
        drop(encoder);
        let submission = renderer.gpu.submit([]);
        let dropped = renderer.complete_yuv420p_readback(unsubmitted, submission, true);
        dropped.wait_ready().expect("the stale buffer maps");
        assert_eq!(
            consume(dropped),
            Err(RenderError::GpuWorkDropped),
            "{name}: a buffer holding another readback's canary"
        );
        assert_eq!(
            renderer.gpu_lost(),
            Some(RenderError::GpuWorkDropped),
            "{name}"
        );
        assert_eq!(
            renderer.readback_buffers.stats().available,
            pooled.available - 1,
            "{name}: a buffer holding no frame of its readback is not pooled again"
        );
        assert_eq!(
            renderer
                .begin_yuv420p_readback(&target, centred(ColorMetadata::default()))
                .err(),
            Some(RenderError::GpuWorkDropped),
            "{name}: later readbacks are refused"
        );
        assert!(pollster::block_on(scope.pop()).is_none(), "{name}");
    }
}
