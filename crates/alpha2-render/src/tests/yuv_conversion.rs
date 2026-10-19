//! YUV sources: plane upload, conversion to RGB for every matrix, range, and subsampling, coding round trips, and malformed planes.

use super::*;

#[test]
fn gpu_yuv_conversion_covers_every_sdr_matrix_and_range() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 2).expect("size");
    for matrix in [
        ColorMatrix::Bt601,
        ColorMatrix::Bt709,
        ColorMatrix::Bt2020Ncl,
    ] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let (black, white) = if range == ColorRange::Limited {
                (16, 235)
            } else {
                (0, 255)
            };
            let (target, diagnostics) = renderer
                .render_yuv420p(
                    Yuv420Frame {
                        y: &[black, white, black, white],
                        u: &[128],
                        v: &[128],
                        y_stride: 2,
                        chroma_stride: 1,
                        size,
                        alpha: 1.0,
                        color: Some(ColorMetadata {
                            primaries: ColorPrimaries::Bt709,
                            transfer: TransferFunction::Bt709,
                            matrix,
                            range,
                            dynamic_range: DynamicRange::Sdr,
                        }),
                    },
                    crate::ColorPolicy::RejectHdr,
                )
                .expect("supported YUV conversion");
            assert_eq!(diagnostics, [] as [crate::ColorDiagnostic; 0]);
            let pixels = read_rgba8(&renderer, &target);
            assert!(pixels[0..3].iter().all(|value| *value <= 1));
            assert!(pixels[4..7].iter().all(|value| *value >= 254));
        }
    }
}

/// Full-range BT.709 YCbCr codes as the 8-bit sRGB-encoded RGB the
/// presentation stores: the matrix, clamped, as the shader's EOTF and the
/// intermediate's encode cancel.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn full_range_bt709_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let (kr, kb) = (0.2126_f32, 0.0722_f32);
    let kg = 1.0 - kr - kb;
    let luma = f32::from(y) / 255.0;
    let cb = (f32::from(u) - 128.0) / 255.0;
    let cr = (f32::from(v) - 128.0) / 255.0;
    [
        luma + 2.0 * (1.0 - kr) * cr,
        luma - 2.0 * kb * (1.0 - kb) / kg * cb - 2.0 * kr * (1.0 - kr) / kg * cr,
        luma + 2.0 * (1.0 - kb) * cb,
    ]
    .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Presents `frame` unscaled with the given chroma siting and reads it back,
/// failing on any validation error the device reports on the way.
fn present_yuv(renderer: &Renderer, frame: crate::YuvFrame<'_>, siting: ChromaLocation) -> Vec<u8> {
    let presentation = SourcePresentation {
        chroma_siting: siting,
        ..SourcePresentation::identity(frame.size)
    };
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let (prepared, diagnostics) = renderer
        .prepare_yuv_presented(frame, presentation, ColorPolicy::RejectHdr)
        .expect("supported YUV format");
    assert_eq!(diagnostics, [] as [crate::ColorDiagnostic; 0]);
    let (texture, pending) = prepared.into_parts();
    assert!(renderer.submit_pending_source(&pending.expect("pending conversion")));
    let pixels = read_rgba8(renderer, &texture);
    if let Some(error) = pollster::block_on(scope.pop()) {
        panic!("{:?}: {error}", frame.format);
    }
    pixels
}

#[test]
fn upload_textures_ask_only_for_usages_their_format_allows() {
    // R16Unorm is sampled but not renderable, and WebGL2 without EXT_color_buffer_float cannot
    // render half floats: ask only for usages every device allows, or 10-bit uploads fail.
    for format in [
        wgpu::TextureFormat::R8Unorm,
        wgpu::TextureFormat::Rg8Unorm,
        wgpu::TextureFormat::R16Unorm,
        wgpu::TextureFormat::Rg16Unorm,
        wgpu::TextureFormat::R16Float,
        wgpu::TextureFormat::Rg16Float,
        wgpu::TextureFormat::Rgba16Float,
    ] {
        assert_eq!(
            crate::pool::upload_texture_usage(format),
            wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            "{format:?}"
        );
    }
    // The browser copies VideoFrames into RGBA8 uploads, which needs it.
    assert_eq!(
        crate::pool::upload_texture_usage(wgpu::TextureFormat::Rgba8Unorm),
        wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
    );
}

#[test]
fn gpu_ten_bit_planes_upload_as_16_bit_normalized_textures_where_supported() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    if !renderer
        .features
        .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
    {
        eprintln!("skipped: no 16-bit normalized textures on this device");
        return;
    }
    let size = RenderSize::new(4, 2).expect("size");
    let (y, u, v) = (
        [50_u8, 100, 150, 200, 80, 120, 160, 220],
        [60_u8; 8],
        [200_u8; 8],
    );
    let expected = y
        .iter()
        .map(|luma| full_range_bt709_rgb(*luma, 60, 200))
        .collect::<Vec<_>>();
    let (y10, u10, v10) = (widen(&y, 2), widen(&u, 2), widen(&v, 2));
    let pixels = present_yuv(
        &renderer,
        full_range_frame(
            alpha2_media::YuvFormat::planar(0, 0, 10),
            [plane(&y10, 8), plane(&u10, 8), plane(&v10, 8)],
            size,
        ),
        ChromaLocation::Unspecified,
    );
    assert_rgb_close(&pixels, &expected, "yuv444p10le as R16Unorm");
}

fn widen(codes: &[u8], shift: u32) -> Vec<u8> {
    codes
        .iter()
        .flat_map(|code| (u16::from(*code) << shift).to_le_bytes())
        .collect()
}

#[test]
fn gpu_yuv_444_and_high_depth_planes_convert_every_sample() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 2).expect("size");
    let y = [50_u8, 100, 150, 200, 80, 120, 160, 220];
    let u = [60_u8, 200, 90, 160, 128, 30, 220, 100];
    let v = [200_u8, 60, 128, 170, 40, 210, 110, 90];
    let expected = (0..8)
        .map(|index| full_range_bt709_rgb(y[index], u[index], v[index]))
        .collect::<Vec<_>>();
    let frame = |format, planes| full_range_frame(format, planes, size);
    let eight = present_yuv(
        &renderer,
        frame(
            alpha2_media::YuvFormat::planar(0, 0, 8),
            [plane(&y, 4), plane(&u, 4), plane(&v, 4)],
        ),
        ChromaLocation::Unspecified,
    );
    assert_rgb_close(&eight, &expected, "yuv444p");

    // The same codes as low-aligned 10-bit and high-aligned 16-bit containers, then without
    // 16-bit normalized textures.
    let (y10, u10, v10) = (widen(&y, 2), widen(&u, 2), widen(&v, 2));
    let ten = alpha2_media::YuvFormat::planar(0, 0, 10);
    let ten_frame = frame(ten, [plane(&y10, 8), plane(&u10, 8), plane(&v10, 8)]);
    let (y16, u16_, v16) = (widen(&y, 8), widen(&u, 8), widen(&v, 8));
    let sixteen = frame(
        alpha2_media::YuvFormat::planar(0, 0, 16),
        [plane(&y16, 8), plane(&u16_, 8), plane(&v16, 8)],
    );
    for (label, frame) in [("yuv444p10le", ten_frame), ("yuv444p16le", sixteen)] {
        let pixels = present_yuv(&renderer, frame, ChromaLocation::Unspecified);
        assert_rgb_close(&pixels, &expected, label);
    }
    renderer
        .features
        .remove(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
    let integers = present_yuv(&renderer, ten_frame, ChromaLocation::Unspecified);
    assert_rgb_close(&integers, &expected, "yuv444p10le as unsigned integers");
}

#[test]
fn gpu_yuv_444_honours_an_anamorphic_sample_aspect_ratio() {
    // Pixels twice as wide as tall: the 2x2 frame shows as 4x2, filling the middle rows of 4x4.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(2, 2).expect("size");
    let (y, chroma) = ([200_u8; 4], [128_u8; 4]);
    let presentation = SourcePresentation {
        output_size: RenderSize::new(4, 4).expect("size"),
        sample_aspect_ratio: (2, 1),
        ..SourcePresentation::identity(size)
    };
    let (prepared, _) = renderer
        .prepare_yuv_presented(
            full_range_frame(
                alpha2_media::YuvFormat::planar(0, 0, 8),
                [plane(&y, 2), plane(&chroma, 2), plane(&chroma, 2)],
                size,
            ),
            presentation,
            ColorPolicy::RejectHdr,
        )
        .expect("4:4:4 presents");
    let (texture, pending) = prepared.into_parts();
    assert!(renderer.submit_pending_source(&pending.expect("pending")));
    let pixels = read_rgba8(&renderer, &texture);
    let alpha = |row: usize| {
        (0..4)
            .map(|x| pixels[(row * 4 + x) * 4 + 3])
            .collect::<Vec<_>>()
    };
    assert_eq!(alpha(0), [0; 4]);
    assert_eq!(alpha(1), [255; 4]);
    assert_eq!(alpha(2), [255; 4]);
    assert_eq!(alpha(3), [0; 4]);
}

#[test]
fn gpu_yuv_422_and_interleaved_chroma_follow_their_subsampling() {
    // With left-sited chroma, even columns sit exactly on a chroma sample.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 2).expect("size");
    let y = [50_u8, 100, 150, 200, 80, 120, 160, 220];
    let u = [60_u8, 200, 128, 30];
    let v = [200_u8, 60, 40, 210];
    let pixels = present_yuv(
        &renderer,
        crate::YuvFrame {
            format: alpha2_media::YuvFormat::planar(1, 0, 8),
            planes: [plane(&y, 4), plane(&u, 2), plane(&v, 2)],
            alpha_plane: None,
            size,
            alpha: 1.0,
            color: Some(full_range_bt709()),
        },
        ChromaLocation::Left,
    );
    let even = |row: usize, column: usize| {
        let pixel = (row * 4 + column) * 4;
        pixels[pixel..pixel + 4].to_vec()
    };
    for (row, column, chroma) in [(0, 0, 0), (0, 2, 1), (1, 0, 2), (1, 2, 3)] {
        assert_rgb_close(
            &even(row, column),
            &[full_range_bt709_rgb(
                y[row * 4 + column],
                u[chroma],
                v[chroma],
            )],
            &format!("yuv422p row {row} column {column}"),
        );
    }

    // NV12 and P010 from the CPU match the same codes as I420.
    let y = [50_u8, 100, 150, 200, 80, 120, 160, 220];
    let (u, v) = ([60_u8, 200], [200_u8, 60]);
    let i420 = present_yuv(
        &renderer,
        crate::YuvFrame {
            format: alpha2_media::YuvFormat::I420,
            planes: [plane(&y, 4), plane(&u, 2), plane(&v, 2)],
            alpha_plane: None,
            size,
            alpha: 1.0,
            color: Some(full_range_bt709()),
        },
        ChromaLocation::Left,
    );
    let uv = [u[0], v[0], u[1], v[1]];
    let nv12 = present_yuv(
        &renderer,
        crate::YuvFrame {
            format: alpha2_media::YuvFormat::NV12,
            planes: [plane(&y, 4), plane(&uv, 4), plane(&[], 0)],
            alpha_plane: None,
            size,
            alpha: 1.0,
            color: Some(full_range_bt709()),
        },
        ChromaLocation::Left,
    );
    assert_eq!(nv12, i420, "NV12");
    let (y10, uv10) = (widen(&y, 8), widen(&uv, 8));
    let p010 = present_yuv(
        &renderer,
        crate::YuvFrame {
            format: alpha2_media::YuvFormat::P010,
            planes: [plane(&y10, 8), plane(&uv10, 8), plane(&[], 0)],
            alpha_plane: None,
            size,
            alpha: 1.0,
            color: Some(full_range_bt709()),
        },
        ChromaLocation::Left,
    );
    for (index, (ten, eight)) in p010.iter().zip(&i420).enumerate() {
        assert!(
            ten.abs_diff(*eight) <= 1,
            "P010 byte {index}: {ten} != {eight}"
        );
    }
}

#[test]
fn gpu_yuv_import_then_export_is_the_identity_for_every_sdr_transfer_tag() {
    // SDR input is linearized with the sRGB EOTF and export encodes with the sRGB OETF, so a
    // project without effects hands back the codes it was given.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(256, 2).expect("size");
    for transfer in [TransferFunction::Bt709, TransferFunction::Srgb] {
        for matrix in [
            ColorMatrix::Bt601,
            ColorMatrix::Bt709,
            ColorMatrix::Bt2020Ncl,
        ] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                let (black, white) = if range == ColorRange::Limited {
                    (16_u16, 235_u16)
                } else {
                    (0, 255)
                };
                let ramp = (0..256_u16)
                    .map(|column| {
                        u8::try_from(black + (white - black) * column / 255).expect("8-bit code")
                    })
                    .collect::<Vec<_>>();
                let y = [ramp.as_slice(), ramp.as_slice()].concat();
                let color = ColorMetadata {
                    primaries: ColorPrimaries::Bt709,
                    transfer,
                    matrix,
                    range,
                    dynamic_range: DynamicRange::Sdr,
                };
                let (target, _) = renderer
                    .render_yuv420p(
                        Yuv420Frame {
                            y: &y,
                            u: &[128; 128],
                            v: &[128; 128],
                            y_stride: 256,
                            chroma_stride: 128,
                            size,
                            alpha: 1.0,
                            color: Some(color),
                        },
                        ColorPolicy::RejectHdr,
                    )
                    .expect("supported YUV conversion");
                let planes = renderer
                    .begin_yuv420p_readback(&target, centred(color))
                    .expect("YUV readback submits")
                    .wait()
                    .expect("YUV readback maps");
                let worst = planes
                    .y
                    .iter()
                    .zip(&y)
                    .map(|(exported, imported)| exported.abs_diff(*imported))
                    .max()
                    .expect("luma samples");
                assert!(
                    worst <= 1,
                    "{transfer:?} {matrix:?} {range:?}: luma drifted by {worst} codes"
                );
                assert!(
                    planes
                        .u
                        .iter()
                        .chain(&planes.v)
                        .all(|value| value.abs_diff(128) <= 1),
                    "{transfer:?} {matrix:?} {range:?}: neutral chroma drifted"
                );
            }
        }
    }
}

/// 8-bit YCbCr codes of `color`'s coding whose RGB lies inside the cube
/// with a margin, so neither direction clamps: the ITU formulas in `f64`,
/// independent of the renderer's constants.
fn in_gamut_codes(color: ColorMetadata) -> Vec<[u8; 3]> {
    let (kr, kb) = match color.matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
        ColorMatrix::Bt2020Ncl => (0.2627, 0.0593),
        ColorMatrix::Rgb => unreachable!("YCbCr codings only"),
    };
    let kg = 1.0 - kr - kb;
    let (luma, chroma): (Vec<u16>, Vec<u16>) = if color.range == ColorRange::Limited {
        (
            (16..=235).step_by(9).collect(),
            (16..=240).step_by(14).collect(),
        )
    } else {
        (
            (0..=255).step_by(10).collect(),
            (0..=255).step_by(15).collect(),
        )
    };
    let normalize = |y: u16, c: u16| -> (f64, f64) {
        if color.range == ColorRange::Limited {
            (
                (f64::from(y) - 16.0) / 219.0,
                (f64::from(c) - 128.0) / 224.0,
            )
        } else {
            (f64::from(y) / 255.0, (f64::from(c) - 128.0) / 255.0)
        }
    };
    let mut codes = Vec::new();
    for &y in &luma {
        for &cb in &chroma {
            for &cr in &chroma {
                let (luma, blue) = normalize(y, cb);
                let (_, red) = normalize(y, cr);
                let rgb = [
                    luma + 2.0 * (1.0 - kr) * red,
                    luma - 2.0 * kb * (1.0 - kb) / kg * blue - 2.0 * kr * (1.0 - kr) / kg * red,
                    luma + 2.0 * (1.0 - kb) * blue,
                ];
                if rgb.iter().all(|value| (0.02..=0.98).contains(value)) {
                    codes.push([y, cb, cr].map(|code| u8::try_from(code).expect("8-bit")));
                }
            }
        }
    }
    codes
}

/// Side of the square each colour of [`coding_image`] fills.
const CODING_BLOCK: usize = 8;

/// Colours per row of [`coding_image`].
const CODING_COLUMNS: usize = 32;

/// I420 planes that paint each code over a `CODING_BLOCK` square, and the
/// image's width and height.
fn coding_image(codes: &[[u8; 3]]) -> ([Vec<u8>; 3], usize, usize) {
    let rows = codes.len().div_ceil(CODING_COLUMNS);
    let (width, height) = (CODING_COLUMNS * CODING_BLOCK, rows * CODING_BLOCK);
    let chroma_width = width / 2;
    let mut planes = [
        vec![0_u8; width * height],
        vec![128_u8; chroma_width * (height / 2)],
        vec![128_u8; chroma_width * (height / 2)],
    ];
    let half = CODING_BLOCK / 2;
    for (index, code) in codes.iter().enumerate() {
        let (left, top) = (
            (index % CODING_COLUMNS) * CODING_BLOCK,
            (index / CODING_COLUMNS) * CODING_BLOCK,
        );
        for row in top..top + CODING_BLOCK {
            planes[0][row * width + left..row * width + left + CODING_BLOCK].fill(code[0]);
        }
        for row in top / 2..top / 2 + half {
            let start = row * chroma_width + left / 2;
            planes[1][start..start + half].fill(code[1]);
            planes[2][start..start + half].fill(code[2]);
        }
    }
    (planes, width, height)
}

/// Imports `planes` as `depth`-bit 4:2:0 of `color` and exports them as
/// 8-bit I420 with the same tags: the largest Y, Cb, and Cr difference from
/// `codes` inside each colour's square, away from its chroma edges.
fn coding_round_trip(
    renderer: &Renderer,
    color: ColorMetadata,
    codes: &[[u8; 3]],
    depth: u8,
) -> [u8; 3] {
    let (planes, width, height) = coding_image(codes);
    let chroma_width = width / 2;
    let (format, stride, planes) = if depth == 8 {
        (alpha2_media::YuvFormat::I420, 1, planes)
    } else {
        (
            alpha2_media::YuvFormat::planar(1, 1, depth),
            2,
            planes.map(|plane| widen(&plane, u32::from(depth - 8))),
        )
    };
    let size = RenderSize::new(
        u32::try_from(width).expect("width"),
        u32::try_from(height).expect("height"),
    )
    .expect("size");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let (prepared, _) = renderer
        .prepare_yuv_presented(
            crate::YuvFrame {
                format,
                planes: [
                    plane(&planes[0], width * stride),
                    plane(&planes[1], chroma_width * stride),
                    plane(&planes[2], chroma_width * stride),
                ],
                alpha_plane: None,
                size,
                alpha: 1.0,
                color: Some(color),
            },
            SourcePresentation {
                chroma_siting: ChromaLocation::Left,
                ..SourcePresentation::identity(size)
            },
            ColorPolicy::RejectHdr,
        )
        .expect("supported coding");
    let (texture, pending) = prepared.into_parts();
    assert!(renderer.submit_pending_source(&pending.expect("pending conversion")));
    let exported = renderer
        .begin_yuv420p_readback(&texture, centred(color))
        .expect("YUV readback submits")
        .wait()
        .expect("YUV readback maps");
    if let Some(error) = pollster::block_on(scope.pop()) {
        panic!("{color:?} {depth}-bit: {error}");
    }
    let half = CODING_BLOCK / 2;
    let mut worst = [0_u8; 3];
    for (index, code) in codes.iter().enumerate() {
        let (left, top) = (
            (index % CODING_COLUMNS) * CODING_BLOCK,
            (index / CODING_COLUMNS) * CODING_BLOCK,
        );
        for row in top + 2..top + CODING_BLOCK - 2 {
            for column in left + 2..left + CODING_BLOCK - 2 {
                worst[0] = worst[0].max(exported.y[row * width + column].abs_diff(code[0]));
            }
        }
        for row in top / 2 + 1..top / 2 + half - 1 {
            for column in left / 2 + 1..left / 2 + half - 1 {
                let at = row * chroma_width + column;
                worst[1] = worst[1].max(exported.u[at].abs_diff(code[1]));
                worst[2] = worst[2].max(exported.v[at].abs_diff(code[2]));
            }
        }
    }
    worst
}

#[test]
fn gpu_every_sdr_coding_round_trips_in_gamut_colour_exactly_at_eight_bits() {
    // Within one code at 10 bits: full-range 10-bit 4c is not 8-bit c (1023 is not 4 x 255).
    // Full-range chroma is centred on 128/255, not one half. Only block middles are compared,
    // since chroma filtering at block edges is each resampler's own.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    // An 8-bit intermediate stores the converted RGB in 8 bits, which cannot hold every
    // 8-bit YUV code apart: there, one code.
    let bound_8 = u8::from(renderer.intermediate_format() == wgpu::TextureFormat::Rgba8Unorm);
    for matrix in [
        ColorMatrix::Bt601,
        ColorMatrix::Bt709,
        ColorMatrix::Bt2020Ncl,
    ] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let color = ColorMetadata {
                primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Bt709,
                matrix,
                range,
                dynamic_range: DynamicRange::Sdr,
            };
            let codes = in_gamut_codes(color);
            assert!(
                codes.len() > 100,
                "{matrix:?} {range:?}: {} colours",
                codes.len()
            );
            for (depth, bound) in [(8, bound_8), (10, 1)] {
                let worst = coding_round_trip(&renderer, color, &codes, depth);
                assert!(
                    worst.iter().all(|delta| *delta <= bound),
                    "{matrix:?} {range:?} {depth}-bit: worst Y/Cb/Cr {worst:?}"
                );
            }
        }
    }
}

#[test]
fn the_suite_runs_on_the_adapter_a_native_host_exports_on() {
    // The first adapter listed may not be the one the host renders on. Hold the lock: a second
    // instance loads and unloads the Vulkan layers and drivers.
    let _lock = gpu_test_lock();
    let host = pollster::block_on(GpuContext::request_high_performance())
        .expect("a native host's GPU adapter");
    let (suite, host) = (gpu(), host.capabilities().clone());
    assert_eq!(
        (
            &suite.capabilities().adapter_name,
            &suite.capabilities().backend
        ),
        (&host.adapter_name, &host.backend)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn malformed_planes_and_presentations_are_refused_before_they_reach_the_gpu() {
    // Bad plane, stride or presentation input would otherwise show as a stale pooled texture
    // or a NaN draw rect rather than as an error.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let size = RenderSize::new(4, 2).expect("size");
    let refused = |what: &str, result: Result<(), RenderError>| {
        assert_eq!(result, Err(RenderError::InvalidImageData), "{what}");
    };
    let plane = |bytes, stride| crate::SourcePlane { bytes, stride };

    let rgba = [90_u8; 4 * 4 * 2];
    let padded = [90_u8; 20 * 2];
    renderer.upload_rgba8(size, &rgba).expect("a whole picture");
    for (what, bytes) in [
        ("a short picture", &rgba[..31]),
        ("a long one", &padded[..33]),
    ] {
        refused(what, renderer.upload_rgba8(size, bytes).map(drop));
    }
    let presented = |pixels| {
        renderer
            .prepare_rgba8_presented(pixels, size, SourcePresentation::identity(size))
            .map(drop)
    };
    presented(plane(&padded, 20)).expect("padded rows");
    refused("a stride under a row", presented(plane(&rgba, 15)));
    refused(
        "rows longer than the bytes hold",
        presented(plane(&rgba, 20)),
    );
    refused(
        "rows shorter than the bytes hold",
        presented(plane(&padded, 16)),
    );

    renderer
        .upload_linear_rgba16f(size, &[0; 4 * 4 * 2])
        .expect("a whole picture");
    refused(
        "short halves",
        renderer.upload_linear_rgba16f(size, &[0; 31]).map(drop),
    );
    let halves = [0_u8; 32 * 2];
    refused(
        "a half-float stride under a row",
        renderer
            .write_linear_rgba16f(size, plane(&halves, 31))
            .map(drop),
    );

    let (y, u, v) = ([16_u8; 8], [128_u8; 2], [128_u8; 2]);
    let frame = i420;
    let good = || [plane(&y, 4), plane(&u, 2), plane(&v, 2)];
    let present_yuv = |frame, presentation| {
        renderer
            .prepare_yuv_presented(frame, presentation, ColorPolicy::RejectHdr)
            .map(drop)
    };
    present_yuv(frame(good(), 1.0), SourcePresentation::identity(size)).expect("good planes");
    let long_u = [128_u8; 3];
    for (what, planes) in [
        (
            "a short luma plane",
            [plane(&y[..7], 4), plane(&u, 2), plane(&v, 2)],
        ),
        (
            "a luma stride under a row",
            [plane(&y, 3), plane(&u, 2), plane(&v, 2)],
        ),
        (
            "a long chroma plane",
            [plane(&y, 4), plane(&long_u, 2), plane(&v, 2)],
        ),
        (
            "a short chroma plane",
            [plane(&y, 4), plane(&u, 2), plane(&v[..1], 2)],
        ),
    ] {
        refused(
            what,
            present_yuv(frame(planes, 1.0), SourcePresentation::identity(size)),
        );
    }
    for alpha in [f32::NAN, f32::INFINITY, -0.01, 1.01] {
        refused(
            &format!("alpha {alpha}"),
            present_yuv(frame(good(), alpha), SourcePresentation::identity(size)),
        );
    }

    let presentation = |x, y, width, height, sample_aspect_ratio| SourcePresentation {
        visible_rect: Rect {
            x,
            y,
            width,
            height,
        },
        sample_aspect_ratio,
        ..SourcePresentation::identity(size)
    };
    let (luma, chroma) = nv12_test_planes(&renderer, 4, 2, &[16; 8], &[128; 4]);
    let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
    let chroma_view = chroma.create_view(&wgpu::TextureViewDescriptor::default());
    let present_gpu_with_alpha = |presentation, alpha| {
        let submitted = Arc::new(AtomicBool::new(false));
        renderer
            .prepare_gpu_yuv_presented(
                GpuVideoPlanes {
                    y: &luma_view,
                    chroma: &chroma_view,
                    v: None,
                    format: alpha2_media::YuvFormat::NV12,
                    size,
                    alpha,
                    color: Some(ColorMetadata::default()),
                },
                presentation,
                ColorPolicy::RejectHdr,
                Arc::new(TestExternalLease::new(&submitted)),
            )
            .map(drop)
    };
    let present_gpu = |presentation| present_gpu_with_alpha(presentation, 1.0);
    present_gpu(presentation(1, 0, 2, 2, (2, 1))).expect("a crop with wide pixels");
    for alpha in [f32::NAN, -0.01, 1.01] {
        refused(
            &format!("GPU YUV, alpha {alpha}"),
            present_gpu_with_alpha(SourcePresentation::identity(size), alpha),
        );
    }
    for (what, geometry) in [
        ("no visible width", presentation(0, 0, 0, 2, (1, 1))),
        ("no visible height", presentation(0, 0, 4, 0, (1, 1))),
        (
            "a zero sample aspect ratio",
            presentation(0, 0, 4, 2, (0, 1)),
        ),
        (
            "a sample aspect ratio over zero",
            presentation(0, 0, 4, 2, (1, 0)),
        ),
        (
            "a rect past the right edge",
            presentation(1, 0, 4, 2, (1, 1)),
        ),
        (
            "a rect past the bottom edge",
            presentation(0, 1, 4, 2, (1, 1)),
        ),
        (
            "a rect whose right edge overflows",
            presentation(u32::MAX, 0, 2, 2, (1, 1)),
        ),
        (
            "a rect whose bottom edge overflows",
            presentation(0, u32::MAX, 2, 2, (1, 1)),
        ),
    ] {
        refused(
            &format!("YUV, {what}"),
            present_yuv(frame(good(), 1.0), geometry),
        );
        refused(&format!("GPU YUV, {what}"), present_gpu(geometry));
        refused(
            &format!("RGBA, {what}"),
            renderer
                .prepare_rgba8_presented(plane(&rgba, 16), size, geometry)
                .map(drop),
        );
    }

    // A repack uploads its planes through the same check.
    let luma_target = nv12_test_plane(&renderer.gpu, "luma", size, wgpu::TextureFormat::R8Unorm);
    let chroma_target = nv12_test_plane(
        &renderer.gpu,
        "chroma",
        RenderSize::new(2, 1).expect("size"),
        wgpu::TextureFormat::Rg8Unorm,
    );
    let repack = |planes| {
        renderer.repack_nv12_timed(
            crate::RepackSource::Cpu(frame(planes, 1.0)),
            presentation(0, 0, 4, 2, (1, 1)).visible_rect,
            crate::Nv12PlaneTargets {
                luma: &luma_target,
                chroma: &chroma_target,
            },
            &mut crate::FrameSubmitTimings::default(),
        )
    };
    repack(good()).expect("good planes repack");
    refused(
        "a repack of a short luma plane",
        repack([plane(&y[..7], 4), plane(&u, 2), plane(&v, 2)]),
    );
    refused(
        "a repack with a stride under a row",
        repack([plane(&y, 4), plane(&u, 1), plane(&v, 2)]),
    );
    renderer
        .confirm_gpu_work()
        .expect("the repacks the GPU ran check out");
    assert!(pollster::block_on(scope.pop()).is_none());
}
