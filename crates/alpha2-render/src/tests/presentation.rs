//! Source presentation, the pass every decoded picture takes before the
//! composite: orientation, the visible rect's chroma, and minification.

use super::*;

/// A source `width` x `height` whose every pixel is a distinct opaque color.
fn distinct_pixels(width: u32, height: u32) -> Vec<[u8; 4]> {
    (0..width * height)
        .map(|index| {
            let i = u8::try_from(index).expect("small source");
            [40 * i + 10, 250 - 40 * i, 37 * i + 5, 255]
        })
        .collect()
}

/// Where output pixel `(x, y)` of a clockwise `rotation` of a `width` x
/// `height` picture comes from in the stored picture.
fn rotated_from(rotation: Rotation, width: u32, height: u32, x: u32, y: u32) -> (u32, u32) {
    match rotation {
        Rotation::Deg0 => (x, y),
        // A clockwise quarter turn: the stored bottom-left corner ends up top left.
        Rotation::Deg90 => (y, height - 1 - x),
        Rotation::Deg180 => (width - 1 - x, height - 1 - y),
        // The stored top-right corner ends up top left.
        Rotation::Deg270 => (width - 1 - y, x),
    }
}

/// Presents a 3 x 2 picture at each quarter turn into exactly its turned
/// size and checks that every output pixel is the stored pixel the clockwise
/// turn puts there, through `present` (one of the two presentation shaders).
fn assert_turns_clockwise(
    what: &str,
    stored: &[[u8; 4]],
    present: &dyn Fn(Rotation, RenderSize) -> Vec<u8>,
) {
    let (width, height) = (3, 2);
    for rotation in [
        Rotation::Deg0,
        Rotation::Deg90,
        Rotation::Deg180,
        Rotation::Deg270,
    ] {
        let output = if rotation.swaps_axes() {
            RenderSize::new(height, width)
        } else {
            RenderSize::new(width, height)
        }
        .expect("non-zero size");
        let pixels = present(rotation, output);
        for y in 0..output.height {
            for x in 0..output.width {
                let (sx, sy) = rotated_from(rotation, width, height, x, y);
                let expected = stored[(sy * width + sx) as usize];
                let index = ((y * output.width + x) * 4) as usize;
                let actual = &pixels[index..index + 4];
                assert!(
                    actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 2),
                    "{what} {rotation:?}: output ({x}, {y}) is {actual:?}, stored ({sx}, {sy}) is {expected:?}"
                );
            }
        }
    }
}

fn turned(size: RenderSize, output: RenderSize, rotation: Rotation) -> SourcePresentation {
    SourcePresentation {
        output_size: output,
        orientation: Orientation::rotated(rotation),
        ..SourcePresentation::identity(size)
    }
}

#[test]
fn both_presentation_shaders_turn_each_quarter_clockwise() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(3, 2).expect("non-zero size");

    let stored = distinct_pixels(3, 2);
    let bytes: Vec<u8> = stored.iter().flatten().copied().collect();
    let source = renderer.upload_rgba8(size, &bytes).expect("source upload");
    assert_turns_clockwise("RGBA", &stored, &|rotation, output| {
        let target = renderer
            .render_rgba_presented(
                source.texture(),
                size,
                turned(size, output, rotation),
                crate::SourceAlpha::Straight,
            )
            .expect("RGBA presentation");
        read_rgba8(&renderer, &target)
    });

    // Full-range 4:4:4 grays: each output pixel's luma names the stored pixel.
    let luma: Vec<u8> = (0..6).map(|i| 20 + 40 * i).collect();
    let neutral = [128_u8; 6];
    let grays: Vec<[u8; 4]> = luma.iter().map(|y| [*y, *y, *y, 255]).collect();
    assert_turns_clockwise("YUV", &grays, &|rotation, output| {
        let frame = full_range_frame(
            alpha2_media::YuvFormat::planar(0, 0, 8),
            [plane(&luma, 3), plane(&neutral, 3), plane(&neutral, 3)],
            size,
        );
        let (prepared, _) = renderer
            .prepare_yuv_presented(
                frame,
                turned(size, output, rotation),
                ColorPolicy::RejectHdr,
            )
            .expect("4:4:4 presents");
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending")));
        read_rgba8(&renderer, &texture)
    });
}

/// A 4:2:0 picture of `visible` gray (Y 126, neutral chroma) stored in a
/// surface padded to `coded`, whose padding holds zero chroma, the green a
/// decoder's uninitialized padding shows.
fn padded_gray_420(coded: RenderSize, visible: RenderSize) -> (Vec<u8>, Vec<u8>) {
    let luma = vec![126_u8; (coded.width * coded.height) as usize];
    let (chroma_width, chroma_height) = (coded.width.div_ceil(2), coded.height.div_ceil(2));
    let chroma = (0..chroma_height)
        .flat_map(|y| {
            (0..chroma_width).map(move |x| {
                if x < visible.width.div_ceil(2) && y < visible.height.div_ceil(2) {
                    128
                } else {
                    0
                }
            })
        })
        .collect();
    (luma, chroma)
}

#[test]
fn a_padded_surface_shows_none_of_its_padding_chroma() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let visible = RenderSize::new(8, 4).expect("non-zero size");
    let present = |coded: RenderSize, siting: ChromaLocation| {
        let (luma, chroma) = padded_gray_420(coded, visible);
        let chroma_stride = coded.width.div_ceil(2) as usize;
        let frame = crate::YuvFrame {
            format: alpha2_media::YuvFormat::planar(1, 1, 8),
            planes: [
                plane(&luma, coded.width as usize),
                plane(&chroma, chroma_stride),
                plane(&chroma, chroma_stride),
            ],
            alpha_plane: None,
            size: coded,
            alpha: 1.0,
            color: Some(full_range_bt709()),
        };
        let presentation = SourcePresentation {
            output_size: visible,
            visible_rect: Rect {
                x: 0,
                y: 0,
                width: visible.width,
                height: visible.height,
            },
            chroma_siting: siting,
            ..SourcePresentation::identity(coded)
        };
        let (prepared, _) = renderer
            .prepare_yuv_presented(frame, presentation, ColorPolicy::RejectHdr)
            .expect("4:2:0 presents");
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending")));
        read_rgba8(&renderer, &texture)
    };
    // Padded as a hardware decoder pads: two more rows and two more columns.
    let coded = RenderSize::new(10, 6).expect("non-zero size");
    for siting in [
        ChromaLocation::Center,
        ChromaLocation::Left,
        ChromaLocation::TopLeft,
    ] {
        let unpadded = present(visible, siting);
        assert!(
            unpadded
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] == p[1] && p[1] == p[2]),
            "{siting:?}: the unpadded picture is gray"
        );
        let padded = present(coded, siting);
        for (index, (a, b)) in padded
            .as_chunks::<4>()
            .0
            .iter()
            .zip(unpadded.as_chunks::<4>().0)
            .enumerate()
        {
            assert!(
                a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= 1),
                "{siting:?}: pixel ({}, {}) is {a:?} padded, {b:?} unpadded",
                index % 8,
                index / 8
            );
        }
    }
}

/// 96 x 24 with a one-pixel white line in every third column (1, 4, 7, ...),
/// black between: as `(rgba, luma)`.
fn thin_lines() -> (Vec<u8>, Vec<u8>) {
    let luma: Vec<u8> = (0..24)
        .flat_map(|_| (0..96).map(|x| if x % 3 == 1 { 255 } else { 0 }))
        .collect();
    let rgba = luma.iter().flat_map(|y| [*y, *y, *y, 255]).collect();
    (rgba, luma)
}

#[test]
fn a_picture_shrunk_threefold_averages_its_detail_instead_of_dropping_it() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(96, 24).expect("non-zero size");
    let (rgba, luma) = thin_lines();
    let neutral = vec![128_u8; luma.len()];
    let source = renderer.upload_rgba8(size, &rgba).expect("source upload");
    // Upright into 32 wide, and turned a quarter into 32 tall: three source
    // columns for every output pixel either way.
    for (rotation, output) in [
        (Rotation::Deg0, RenderSize::new(32, 8)),
        (Rotation::Deg90, RenderSize::new(8, 32)),
    ] {
        let output = output.expect("non-zero size");
        let presentation = SourcePresentation {
            output_size: output,
            orientation: Orientation::rotated(rotation),
            ..SourcePresentation::identity(size)
        };
        let target = renderer
            .render_rgba_presented(
                source.texture(),
                size,
                presentation,
                crate::SourceAlpha::Straight,
            )
            .expect("RGBA presentation");
        let presented = read_rgba8(&renderer, &target);
        let (prepared, _) = renderer
            .prepare_yuv_presented(
                full_range_frame(
                    alpha2_media::YuvFormat::planar(0, 0, 8),
                    [plane(&luma, 96), plane(&neutral, 96), plane(&neutral, 96)],
                    size,
                ),
                presentation,
                ColorPolicy::RejectHdr,
            )
            .expect("4:4:4 presents");
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending")));
        let converted = read_rgba8(&renderer, &texture);
        // Both average the stored, sRGB-encoded values, as the composite's
        // filter does: a third of 255.
        for (what, pixels, expected) in [("RGBA", &presented, 85_u8), ("YUV", &converted, 85)] {
            for (index, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
                assert!(
                    pixel[0].abs_diff(expected) <= 2 && pixel[3] == 255,
                    "{what} {rotation:?}: output pixel {index} is {pixel:?}, the average is {expected}"
                );
            }
        }
    }
}

#[test]
fn sixteen_bit_containers_filter_alike_as_integers_and_as_normalized_textures() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    if !renderer
        .features
        .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
    {
        eprintln!("skipped: no 16-bit normalized textures to compare with on this device");
        return;
    }
    // Enlarged 4x so every output pixel filters between samples: the integer build filters and
    // normalizes in the shader what the sampler does for R16Unorm.
    let size = RenderSize::new(4, 4).expect("non-zero size");
    let luma: Vec<u16> = (0..16).map(|i| 64 + i * 55).collect();
    let (u, v) = ([100_u16, 700, 300, 900], [800_u16, 200, 600, 400]);
    let bytes = |codes: &[u16], shift: u32| -> Vec<u8> {
        codes
            .iter()
            .flat_map(|c| (c << shift).to_le_bytes())
            .collect()
    };
    let planar = (bytes(&luma, 0), bytes(&u, 0), bytes(&v, 0));
    let interleaved: Vec<u16> = u.iter().zip(&v).flat_map(|(u, v)| [*u, *v]).collect();
    let p010 = (bytes(&luma, 6), bytes(&interleaved, 6));
    let presentation = SourcePresentation {
        output_size: RenderSize::new(16, 16).expect("non-zero size"),
        chroma_siting: ChromaLocation::Left,
        ..SourcePresentation::identity(size)
    };
    let color = Some(ColorMetadata {
        primaries: ColorPrimaries::Bt709,
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        ..ColorMetadata::default()
    });
    let present = |renderer: &Renderer, frame: crate::YuvFrame<'_>| {
        let (prepared, _) = renderer
            .prepare_yuv_presented(frame, presentation, ColorPolicy::RejectHdr)
            .expect("10-bit presents");
        let (texture, pending) = prepared.into_parts();
        assert!(renderer.submit_pending_source(&pending.expect("pending")));
        read_rgba8(renderer, &texture)
    };
    let frames = [
        (
            "yuv420p10le",
            crate::YuvFrame {
                format: alpha2_media::YuvFormat::planar(1, 1, 10),
                planes: [
                    plane(&planar.0, 8),
                    plane(&planar.1, 4),
                    plane(&planar.2, 4),
                ],
                alpha_plane: None,
                size,
                alpha: 1.0,
                color,
            },
        ),
        (
            "p010",
            crate::YuvFrame {
                format: alpha2_media::YuvFormat::P010,
                planes: [plane(&p010.0, 8), plane(&p010.1, 8), plane(&[], 0)],
                alpha_plane: None,
                size,
                alpha: 1.0,
                color,
            },
        ),
    ];
    let normalized = frames.map(|(_, frame)| present(&renderer, frame));
    renderer
        .features
        .remove(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
    for ((label, frame), normalized) in frames.into_iter().zip(normalized) {
        let integers = present(&renderer, frame);
        for (index, (a, b)) in integers
            .as_chunks::<4>()
            .0
            .iter()
            .zip(normalized.as_chunks::<4>().0)
            .enumerate()
        {
            assert!(
                a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= 1),
                "{label}: pixel {index} is {a:?} from integers, {b:?} from R16Unorm"
            );
        }
    }
}
