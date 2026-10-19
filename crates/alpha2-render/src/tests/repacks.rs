//! Repacking decoded pictures into NV12 encoder targets.

use super::*;

/// Reads back one repacked NV12 target pair as tight luma and UV rows.
fn read_nv12_targets(
    context: &GpuContext,
    luma: &wgpu::Texture,
    chroma: &wgpu::Texture,
    size: RenderSize,
) -> (Vec<u8>, Vec<u8>) {
    let row = size
        .width
        .next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let luma_buffer = nv12_test_readback(context, "repack luma readback", row, size.height);
    let chroma_buffer = nv12_test_readback(context, "repack chroma readback", row, size.height / 2);
    let mut encoder = context
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("repack readback encoder"),
        });
    for (texture, buffer) in [(luma, &luma_buffer), (chroma, &chroma_buffer)] {
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(texture.height()),
                },
            },
            texture.size(),
        );
    }
    context.submit([encoder.finish()]);
    (
        unpad_rows(context, &luma_buffer, row, size.width, size.height),
        unpad_rows(context, &chroma_buffer, row, size.width, size.height / 2),
    )
}

/// Coded planes of `width` x `height` whose every sample is distinct enough
/// that a shifted, swapped, or filtered read cannot match.
fn repack_test_codes(width: u32, height: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let code = |index: u32, salt: u32| u8::try_from((index * 37 + salt) % 251).expect("byte");
    let luma = (0..width * height).map(|index| code(index, 3)).collect();
    let chroma = width / 2 * (height / 2);
    let u = (0..chroma).map(|index| code(index, 101)).collect();
    let v = (0..chroma).map(|index| code(index, 199)).collect();
    (luma, u, v)
}

/// The visible part of a coded plane, rows `stride` samples apart.
fn crop_plane(plane: &[u8], stride: u32, rect: Rect, samples: u32) -> Vec<u8> {
    (rect.y..rect.y + rect.height)
        .flat_map(|row| {
            let start = ((row * stride + rect.x) * samples) as usize;
            plane[start..start + (rect.width * samples) as usize].to_vec()
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_repack_writes_every_decoded_code_into_the_nv12_targets() {
    let _lock = gpu_test_lock();
    let context = gpu();
    let renderer = Renderer::new(context.clone());
    // Uncaptured validation errors do not fail a test here, so the whole run is scoped.
    let scope = context
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let coded = RenderSize::new(12, 8).expect("coded size");
    let visible = Rect {
        x: 2,
        y: 2,
        width: 8,
        height: 4,
    };
    let output = RenderSize::new(visible.width, visible.height).expect("output size");
    let chroma_visible = Rect {
        x: 1,
        y: 1,
        width: 4,
        height: 2,
    };
    let (luma, u, v) = repack_test_codes(coded.width, coded.height);
    let expected_luma = crop_plane(&luma, coded.width, visible, 1);
    let expected_chroma = crop_plane(&u, coded.width / 2, chroma_visible, 1)
        .into_iter()
        .zip(crop_plane(&v, coded.width / 2, chroma_visible, 1))
        .flat_map(|(u, v)| [u, v])
        .collect::<Vec<_>>();
    let interleaved = u
        .iter()
        .zip(&v)
        .flat_map(|(u, v)| [*u, *v])
        .collect::<Vec<_>>();
    let targets = || {
        (
            nv12_test_plane(
                &context,
                "repack luma",
                output,
                wgpu::TextureFormat::R8Unorm,
            ),
            nv12_test_plane(
                &context,
                "repack chroma",
                RenderSize::new(output.width / 2, output.height / 2).expect("chroma size"),
                wgpu::TextureFormat::Rg8Unorm,
            ),
        )
    };
    let before = renderer.diagnostics();
    let canaries_before = renderer.canaries.issued();
    let plane = |bytes, stride| crate::SourcePlane { bytes, stride };
    let cpu_frames = [
        (
            "I420",
            alpha2_media::YuvFormat::I420,
            [
                plane(&luma, coded.width),
                plane(&u, coded.width / 2),
                plane(&v, coded.width / 2),
            ],
        ),
        (
            "NV12",
            alpha2_media::YuvFormat::NV12,
            [
                plane(&luma, coded.width),
                plane(&interleaved, coded.width),
                plane(&[], 0),
            ],
        ),
    ];
    for (label, format, planes) in cpu_frames {
        let (luma_target, chroma_target) = targets();
        renderer
            .repack_nv12_timed(
                crate::RepackSource::Cpu(crate::YuvFrame {
                    format,
                    planes,
                    alpha_plane: None,
                    size: coded,
                    alpha: 1.0,
                    color: Some(ColorMetadata::default()),
                }),
                visible,
                crate::Nv12PlaneTargets {
                    luma: &luma_target,
                    chroma: &chroma_target,
                },
                &mut crate::FrameSubmitTimings::default(),
            )
            .unwrap_or_else(|error| panic!("{label} repacks: {error:?}"));
        let (read_luma, read_chroma) =
            read_nv12_targets(&context, &luma_target, &chroma_target, output);
        assert_eq!(read_luma, expected_luma, "{label} luma");
        assert_eq!(read_chroma, expected_chroma, "{label} chroma");
    }

    // A decoder surface: the lease is released only after the repack's submission.
    let (y, uv) = nv12_test_planes(&renderer, coded.width, coded.height, &luma, &interleaved);
    let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
    let submitted = Arc::new(AtomicBool::new(false));
    let lease = Arc::new(TestExternalLease::new(&submitted));
    let calls = Arc::clone(&lease.calls);
    let weak = Arc::downgrade(&lease);
    let (luma_target, chroma_target) = targets();
    renderer
        .repack_nv12_timed(
            crate::RepackSource::Gpu {
                planes: GpuVideoPlanes {
                    y: &y_view,
                    chroma: &uv_view,
                    v: None,
                    format: alpha2_media::YuvFormat::NV12,
                    size: coded,
                    alpha: 1.0,
                    color: Some(ColorMetadata::default()),
                },
                lease,
            },
            visible,
            crate::Nv12PlaneTargets {
                luma: &luma_target,
                chroma: &chroma_target,
            },
            &mut crate::FrameSubmitTimings::default(),
        )
        .expect("GPU planes repack");
    assert!(submitted.load(Ordering::Acquire));
    assert_eq!(calls.prepared.load(Ordering::Acquire), 1);
    assert_eq!(calls.flushed.load(Ordering::Acquire), 1);
    let (read_luma, read_chroma) =
        read_nv12_targets(&context, &luma_target, &chroma_target, output);
    assert_eq!(read_luma, expected_luma, "GPU luma");
    assert_eq!(read_chroma, expected_chroma, "GPU chroma");
    renderer.reclaim_external_leases().expect("lease reclaim");
    assert!(
        weak.upgrade().is_none(),
        "the lease outlived its submission"
    );

    let after = renderer.diagnostics();
    assert_eq!(after.yuv_repacks - before.yuv_repacks, 3);
    assert_eq!(after.source_submissions - before.source_submissions, 3);
    assert_eq!(after.source_conversions, before.source_conversions);
    assert_eq!(after.graph_passes, before.graph_passes);
    assert_eq!(after.graph_nodes, before.graph_nodes);
    // Each repack carried a canary, and each one the GPU ran checks out.
    assert_eq!(renderer.canaries.issued() - canaries_before, 3);
    renderer
        .confirm_gpu_work()
        .expect("the GPU ran every repack");
    assert_eq!(renderer.submission_canaries.outstanding(), 0);
    assert_eq!(renderer.gpu_lost(), None);
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_repack_refuses_pictures_and_targets_it_cannot_copy_exactly() {
    let _lock = gpu_test_lock();
    let context = gpu();
    let renderer = Renderer::new(context.clone());
    let coded = RenderSize::new(8, 4).expect("coded size");
    let (luma, u, v) = repack_test_codes(coded.width, coded.height);
    let full = Rect {
        x: 0,
        y: 0,
        width: 8,
        height: 4,
    };
    let luma_target = nv12_test_plane(
        &context,
        "refused luma",
        coded,
        wgpu::TextureFormat::R8Unorm,
    );
    let chroma_target = nv12_test_plane(
        &context,
        "refused chroma",
        RenderSize::new(4, 2).expect("chroma size"),
        wgpu::TextureFormat::Rg8Unorm,
    );
    let small_luma = nv12_test_plane(
        &context,
        "small luma",
        RenderSize::new(4, 2).expect("small size"),
        wgpu::TextureFormat::R8Unorm,
    );
    let frame = |format| crate::YuvFrame {
        format,
        planes: [
            crate::SourcePlane {
                bytes: &luma,
                stride: coded.width,
            },
            crate::SourcePlane {
                bytes: &u,
                stride: coded.width / 2,
            },
            crate::SourcePlane {
                bytes: &v,
                stride: coded.width / 2,
            },
        ],
        alpha_plane: None,
        size: coded,
        alpha: 1.0,
        color: None,
    };
    let cases = [
        (
            "4:4:4",
            frame(alpha2_media::YuvFormat::planar(0, 0, 8)),
            full,
            &luma_target,
            RenderError::InvalidImageData,
        ),
        (
            "odd origin",
            frame(alpha2_media::YuvFormat::I420),
            Rect {
                x: 1,
                y: 0,
                width: 6,
                height: 4,
            },
            &luma_target,
            RenderError::InvalidImageData,
        ),
        (
            "outside the coded picture",
            frame(alpha2_media::YuvFormat::I420),
            Rect {
                x: 2,
                y: 0,
                width: 8,
                height: 4,
            },
            &luma_target,
            RenderError::InvalidImageData,
        ),
        (
            "targets of another size",
            frame(alpha2_media::YuvFormat::I420),
            full,
            &small_luma,
            RenderError::InvalidPlaneTarget,
        ),
    ];
    let before = renderer.diagnostics();
    for (label, frame, visible, luma, expected) in cases {
        let scope = context
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let result = renderer.repack_nv12_timed(
            crate::RepackSource::Cpu(frame),
            visible,
            crate::Nv12PlaneTargets {
                luma,
                chroma: &chroma_target,
            },
            &mut crate::FrameSubmitTimings::default(),
        );
        assert_eq!(result.err(), Some(expected), "{label}");
        assert!(pollster::block_on(scope.pop()).is_none(), "{label}");
    }
    assert_eq!(renderer.diagnostics().yuv_repacks, before.yuv_repacks);
}
