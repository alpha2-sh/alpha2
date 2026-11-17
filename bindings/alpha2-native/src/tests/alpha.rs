//! Sources with alpha: VP9's block additions through libvpx, HEVC's auxiliary layer reported
//! dropped, and no hardware session for either.

use super::*;

/// The alpha of `frame` at column `x` of its tenth row: an 8-bit RGBA frame's fourth byte, or a
/// YCbCr frame's 4:4:4 alpha plane.
fn alpha_at(frame: &DecodedVideoFrame, x: usize) -> u8 {
    match frame.pixel_format {
        PixelFormat::Rgba8 => {
            let plane = &frame.planes[0];
            plane.bytes[10 * plane.stride as usize + 4 * x + 3]
        }
        PixelFormat::Yuv(format) => {
            assert!(format.alpha, "{format:?} has no alpha plane");
            let alpha = &frame.planes[3];
            alpha.bytes[10 * alpha.stride as usize + x]
        }
        other => panic!("{other:?} has no alpha"),
    }
}

fn first_software_frame(name: &str) -> (DecodedVideoFrame, Option<crate::DroppedLayer>) {
    let mut decoder =
        NativeExportDecoder::open_with_threads(corpus(name), 1).expect("software decoder");
    let dropped = decoder.dropped_layer().cloned();
    let frame = decoder
        .decode_video_next(&CancelToken::default())
        .expect("decodes")
        .expect("a frame");
    let frame = frame.as_cpu().expect("a software frame").clone();
    (frame, dropped)
}

#[test]
fn vp9_alpha_decodes_its_alpha_plane() {
    crate::require_pinned_ffmpeg_for_tests();
    let probed = crate::probe(corpus("vp9-alpha.webm")).expect("probe");
    assert_eq!(
        probed
            .selected_video_stream()
            .and_then(|stream| stream.alpha),
        Some(alpha2_media::SourceAlpha::Vp9SideData)
    );
    let (frame, dropped) = first_software_frame("vp9-alpha.webm");
    assert_eq!(dropped, None);
    // The fixture's alpha is 0, 128 and 255 in three bands, losslessly coded.
    for (x, expected) in [(4, 0), (20, 128), (40, 255)] {
        let alpha = alpha_at(&frame, x);
        assert!(alpha.abs_diff(expected) <= 1, "alpha at {x}: {alpha}");
    }
}

#[test]
fn vp9_alpha_without_libvpx_reports_alpha_dropped() {
    crate::require_pinned_ffmpeg_for_tests();
    crate::decode::WITHOUT_LIBVPX_VP9.with(|without| without.set(true));
    let (frame, dropped) = first_software_frame("vp9-alpha.webm");
    crate::decode::WITHOUT_LIBVPX_VP9.with(|without| without.set(false));
    assert_eq!(
        dropped,
        Some(crate::DroppedLayer {
            code: "alpha-dropped",
            reason: "this FFmpeg runtime has no libvpx-vp9 decoder".to_owned(),
        })
    );
    // `FFmpeg`'s own `vp9` decodes it opaque.
    assert!(
        matches!(frame.pixel_format, PixelFormat::Yuv(format) if !format.alpha),
        "{:?}",
        frame.pixel_format
    );
}

#[test]
fn an_hevc_alpha_source_decodes_its_base_layer_and_reports_alpha_dropped() {
    crate::require_pinned_ffmpeg_for_tests();
    let probed = crate::probe(corpus("hevc-alpha.mov")).expect("probe");
    assert_eq!(
        probed
            .selected_video_stream()
            .and_then(|stream| stream.alpha),
        Some(alpha2_media::SourceAlpha::HevcAuxiliaryLayer)
    );
    let (frame, dropped) = first_software_frame("hevc-alpha.mov");
    assert_eq!(dropped.map(|dropped| dropped.code), Some("alpha-dropped"));
    // The base layer is BT.709 limited-range green (Y 173) in every column, transparent ones too.
    assert!(
        matches!(frame.pixel_format, PixelFormat::Yuv(format) if !format.alpha),
        "{:?}",
        frame.pixel_format
    );
    let luma = &frame.planes[0];
    for x in [8, 48, 100] {
        let sample = luma.bytes[10 * luma.stride as usize + x];
        assert!(sample.abs_diff(173) <= 3, "luma {sample} at {x}");
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn an_alpha_source_never_opens_a_hardware_session() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    for name in ["vp9-alpha.webm", "hevc-alpha.mov"] {
        let refused = open_hardware_decoder(corpus(name), &context)
            .err()
            .unwrap_or_else(|| panic!("{name} opened a hardware session"));
        assert_eq!(
            refused.reason,
            HardwareDecodeFallback::AlphaChannel,
            "{name}: {refused}"
        );
    }
}

/// Lossy WebP with an alpha chunk decodes to `yuva420p`; the fixtures' alpha is 200 everywhere.
#[test]
fn lossy_webp_with_alpha_keeps_alpha_in_every_column() {
    for name in [
        "webp-lossy-alpha-250x100.webp",
        "webp-lossy-alpha-37x20.webp",
    ] {
        let (frame, _) = first_software_frame(name);
        assert_eq!(frame.pixel_format, PixelFormat::Rgba8, "{name}");
        let plane = &frame.planes[0];
        let (w, h) = (plane.width as usize, plane.height as usize);
        assert_eq!(plane.bytes.len(), 4 * w * h, "{name}");
        for y in 0..h {
            for x in 0..w {
                let alpha = plane.bytes[y * plane.stride as usize + 4 * x + 3];
                assert_eq!(alpha, 200, "{name} at ({x}, {y})");
            }
        }
    }
}
