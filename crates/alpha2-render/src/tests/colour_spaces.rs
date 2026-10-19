//! Wide-gamut and HDR sources converted into the SDR working space.

use super::*;

#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn gpu_wide_gamut_sdr_sources_convert_into_bt709_as_the_reference_does() {
    // BT.2020 primaries with an SDR transfer reach the shader's SDR branch with the gamut row of
    // the HDR uniform. 4:4:4, so each pixel is one sample.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 2).expect("size");
    // The gray encodes to 126.9, clear of a half step: 502 lands on 127.5, which GPUs round
    // either way.
    let y: [u16; 8] = [64, 940, 500, 600, 420, 300, 700, 520];
    let u: [u16; 8] = [512, 512, 512, 350, 680, 400, 300, 600];
    let v: [u16; 8] = [512, 512, 512, 300, 700, 750, 450, 380];
    let bytes = |codes: &[u16]| {
        codes
            .iter()
            .flat_map(|code| code.to_le_bytes())
            .collect::<Vec<u8>>()
    };
    let (y10, u10, v10) = (bytes(&y), bytes(&u), bytes(&v));
    let render = |primaries: ColorPrimaries| {
        let color = ColorMetadata {
            primaries,
            transfer: TransferFunction::Bt709,
            matrix: ColorMatrix::Bt2020Ncl,
            range: ColorRange::Limited,
            dynamic_range: DynamicRange::Sdr,
        };
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let (prepared, diagnostics) = renderer
            .prepare_yuv_presented(
                crate::YuvFrame {
                    format: alpha2_media::YuvFormat::planar(0, 0, 10),
                    planes: [plane(&y10, 8), plane(&u10, 8), plane(&v10, 8)],
                    alpha_plane: None,
                    size,
                    alpha: 1.0,
                    color: Some(color),
                },
                SourcePresentation::identity(size),
                ColorPolicy::RejectHdr,
            )
            .expect("wide-gamut SDR is not HDR");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending conversion")));
        let pixels = read_rgba8(&renderer, &texture);
        assert!(pollster::block_on(scope.pop()).is_none());
        let expected = (0..8)
            .map(|index| {
                let (linear, _) = crate::convert_yuv_to_linear_rgba(
                    crate::YuvSample {
                        y: f32::from(y[index]) / 1023.0,
                        u: f32::from(u[index]) / 1023.0,
                        v: f32::from(v[index]) / 1023.0,
                        alpha: 1.0,
                    },
                    10,
                    Some(color),
                    ColorPolicy::RejectHdr,
                )
                .expect("reference");
                crate::encode_linear_rgb(
                    [linear[0], linear[1], linear[2]],
                    crate::SdrOutputTransfer::Srgb,
                )
                .map(|value| (value * 255.0).round() as u8)
            })
            .collect::<Vec<_>>();
        assert_rgb_close(&pixels, &expected, &format!("{primaries:?}"));
        pixels
    };
    let wide = render(ColorPrimaries::Bt2020);
    let narrow = render(ColorPrimaries::Bt709);
    // Gray, white and black are the same in both gamuts; the colors are not.
    for index in 0..3 {
        assert_eq!(
            wide[index * 4..index * 4 + 3],
            narrow[index * 4..index * 4 + 3]
        );
    }
    let moved = (3..8)
        .filter(|index| {
            (0..3)
                .any(|channel| wide[index * 4 + channel].abs_diff(narrow[index * 4 + channel]) > 8)
        })
        .count();
    assert_eq!(moved, 5, "every color converts: {wide:?} vs {narrow:?}");
}

#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn gpu_hdr_sources_tone_map_into_the_sdr_working_space_as_the_reference_does() {
    // BT.2020 limited-range 10-bit 4:4:4, so each pixel has exactly one sample.
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(4, 2).expect("size");
    let y: [u16; 8] = [64, 200, 400, 520, 600, 700, 820, 940];
    let u: [u16; 8] = [512, 512, 512, 400, 700, 512, 300, 512];
    let v: [u16; 8] = [512, 512, 512, 650, 420, 800, 512, 512];
    let bytes = |codes: &[u16]| {
        codes
            .iter()
            .flat_map(|code| code.to_le_bytes())
            .collect::<Vec<u8>>()
    };
    let (y10, u10, v10) = (bytes(&y), bytes(&u), bytes(&v));
    // The default peak, and one under about 334 nits where the HLG system gamma is below 1
    // and black meets a negative power.
    let tones = [
        crate::hdr::HdrToneMap::default(),
        crate::hdr::HdrToneMap {
            source_peak_nits: 200,
        },
    ];
    for (tone, transfer) in tones.into_iter().flat_map(|tone| {
        [TransferFunction::Pq, TransferFunction::Hlg].map(|transfer| (tone, transfer))
    }) {
        let color = ColorMetadata {
            primaries: ColorPrimaries::Bt2020,
            transfer,
            matrix: ColorMatrix::Bt2020Ncl,
            range: ColorRange::Limited,
            dynamic_range: DynamicRange::Hdr,
        };
        let presentation = SourcePresentation::identity(size);
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let (prepared, diagnostics) = renderer
            .prepare_yuv_presented(
                crate::YuvFrame {
                    format: alpha2_media::YuvFormat::planar(0, 0, 10),
                    planes: [plane(&y10, 8), plane(&u10, 8), plane(&v10, 8)],
                    alpha_plane: None,
                    size,
                    alpha: 1.0,
                    color: Some(color),
                },
                presentation,
                ColorPolicy::ToneMapHdr(tone),
            )
            .expect("an HDR source tone-maps");
        assert_eq!(diagnostics, [crate::ColorDiagnostic::ToneMappedHdr]);
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending conversion")));
        let pixels = read_rgba8(&renderer, &texture);
        assert!(pollster::block_on(scope.pop()).is_none());
        let expected = (0..8)
            .map(|index| {
                let (linear, _) = crate::convert_yuv_to_linear_rgba(
                    crate::YuvSample {
                        y: f32::from(y[index]) / 1023.0,
                        u: f32::from(u[index]) / 1023.0,
                        v: f32::from(v[index]) / 1023.0,
                        alpha: 1.0,
                    },
                    10,
                    Some(color),
                    ColorPolicy::ToneMapHdr(tone),
                )
                .expect("reference");
                assert!(
                    linear.iter().all(|component| component.is_finite()),
                    "{transfer:?} at {} nits, code {index}: {linear:?}",
                    tone.source_peak_nits
                );
                crate::encode_linear_rgb(
                    [linear[0], linear[1], linear[2]],
                    crate::SdrOutputTransfer::Srgb,
                )
                .map(|value| (value * 255.0).round() as u8)
            })
            .collect::<Vec<_>>();
        assert_rgb_close(
            &pixels,
            &expected,
            &format!("{transfer:?} at {} nits", tone.source_peak_nits),
        );
        // Brighter codes never come out darker, and the brightest is near SDR white.
        let lumas = pixels
            .chunks(4)
            .take(3)
            .map(|p| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]))
            .collect::<Vec<_>>();
        assert!(lumas.windows(2).all(|pair| pair[0] <= pair[1]), "{lumas:?}");
        assert!(
            pixels[7 * 4] > 200,
            "{transfer:?} peak: {:?}",
            &pixels[28..32]
        );
    }
    let hdr = ColorMetadata {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferFunction::Pq,
        matrix: ColorMatrix::Bt2020Ncl,
        range: ColorRange::Limited,
        dynamic_range: DynamicRange::Hdr,
    };
    let frame = || crate::YuvFrame {
        format: alpha2_media::YuvFormat::planar(0, 0, 10),
        planes: [plane(&y10, 8), plane(&u10, 8), plane(&v10, 8)],
        alpha_plane: None,
        size,
        alpha: 1.0,
        color: Some(hdr),
    };
    // A refusal of its own, not an effect parameter's.
    assert!(matches!(
        renderer.prepare_yuv_presented(
            frame(),
            SourcePresentation::identity(size),
            ColorPolicy::RejectHdr
        ),
        Err(RenderError::UnsupportedHdr)
    ));
    let (_, skipped) = renderer
        .prepare_yuv_presented(
            frame(),
            SourcePresentation::identity(size),
            ColorPolicy::WarnHdr,
        )
        .expect("skipped");
    assert_eq!(skipped, [crate::ColorDiagnostic::UnsupportedHdr]);
}
