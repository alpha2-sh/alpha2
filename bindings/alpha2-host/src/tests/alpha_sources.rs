//! Sources with alpha: VP9 alpha keeps its transparency through an export, HEVC's auxiliary alpha
//! layer is reported dropped, and neither takes a hardware session.

use super::*;

/// A 128x96 project: a magenta fill under the video `video`, both for 0.1 s.
fn over_magenta() -> Value {
    let span = json!({"start": 0, "end": 12_000});
    json!({
        "schema_version": 1, "id": "alpha-over-magenta", "name": "Alpha over magenta",
        "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1},
                     "sample_rate": 48000},
        "sources": [
            {"id": "fill", "kind": "generated", "generator": {"type": "fill", "paint": {
                "type": "solid", "color": {"red": 1, "green": 0, "blue": 1, "alpha": 1}}}},
            {"id": "video", "locator": "fixture://video", "kind": "video"}
        ],
        "timeline": {"tracks": [
            {"id": "under", "name": "Under", "kind": "video", "clips": [{
                "id": "fill-clip", "source_id": "fill", "timeline_range": span, "source_range": span
            }]},
            {"id": "over", "name": "Over", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video", "timeline_range": span, "source_range": span
            }]}
        ]}
    })
}

/// Exports [`over_magenta`] with `name` as its video, to MPEG-4 Part 2 (which every runtime
/// encodes), and returns the result, what its decoders reported (its `decodeDiagnostics`) and
/// the output's first luma plane.
fn export_over_magenta(
    name: &str,
    policy: VideoDecodePolicy,
    directory: &Path,
) -> (Value, Value, Vec<u8>) {
    let mut host = create_host(policy).expect("host");
    let output = directory.join(format!("{name}-{}.mp4", policy.as_str()));
    let result = host
        .export(
            &over_magenta().to_string(),
            &HashMap::from([("video".to_owned(), corpus(name))]),
            &HashMap::new(),
            Vec::new(),
            0,
            12_000,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::Mpeg4Part2,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::VeryHigh),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            &output,
            &mut |_| Ok(()),
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    let diagnostics = host.failure.decode_diagnostics();
    // A hardware decoder would have dropped the alpha, so none was opened.
    let reasons = &result["performance"]["hardwareFallbackReasons"];
    if host.hardware_decode_ready() {
        assert_eq!(reasons["alphaChannel"], json!(1), "{name}: {result}");
    }
    assert_eq!(
        result["performance"]["hardwareDecodedFrames"],
        json!(0),
        "{result}"
    );
    let luma = decoded_planes(&output)
        .into_iter()
        .next()
        .expect("a frame")
        .swap_remove(0);
    (result, diagnostics, luma)
}

/// The luma of row 48 at the middle of the picture's columns `from..to`.
fn luma_at(luma: &[u8], from: usize, to: usize) -> u8 {
    luma[48 * 128 + usize::midpoint(from, to)]
}

#[test]
fn a_vp9_alpha_export_keeps_transparency_over_magenta() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("vp9-alpha-over-magenta");
    let (_, diagnostics, luma) =
        export_over_magenta("vp9-alpha.webm", VideoDecodePolicy::Auto, &directory);
    assert_eq!(diagnostics, json!([]));
    // The 64-wide fixture is scaled to the 128-wide frame: its alpha 0, 128 and 255 bands cover
    // columns 0-31, 32-63 and 64-127. Magenta is luma 78 and the fixture's green 145; the half
    // band blends the two in linear light, which reads neither.
    let (clear, half, opaque) = (
        luma_at(&luma, 0, 32),
        luma_at(&luma, 32, 64),
        luma_at(&luma, 64, 128),
    );
    assert!(clear.abs_diff(78) <= 6, "transparent band: {clear}");
    assert!(opaque.abs_diff(145) <= 6, "opaque band: {opaque}");
    assert!(
        half.abs_diff(clear) > 20 && half.abs_diff(opaque) > 6,
        "half-transparent band: {half} between {clear} and {opaque}"
    );
}

#[test]
fn an_hevc_alpha_source_reports_alpha_dropped() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("hevc-alpha-over-magenta");
    let (_, diagnostics, luma) =
        export_over_magenta("hevc-alpha.mov", VideoDecodePolicy::Auto, &directory);
    assert_eq!(
        diagnostics,
        json!([{
            "type": "layerDropped",
            "code": "alpha-dropped",
            "source": "video",
            "reason": "alpha in an HEVC auxiliary layer is not decoded",
        }])
    );
    // Opaque: the base layer's green (luma 173) covers the magenta, transparent columns too.
    for (from, to) in [(0, 32), (32, 64), (64, 128)] {
        let sample = luma_at(&luma, from, to);
        assert!(sample.abs_diff(173) <= 6, "columns {from}-{to}: {sample}");
    }
    // The base layer decodes in software under every decode mode, so no hardware decoder's
    // converted copy of it (VideoToolbox's is about 15 codes off) reaches the picture.
    let (_, _, software) =
        export_over_magenta("hevc-alpha.mov", VideoDecodePolicy::Software, &directory);
    let worst = luma
        .iter()
        .zip(&software)
        .map(|(auto, software)| auto.abs_diff(*software))
        .max();
    assert!(
        worst <= Some(2),
        "auto and software differ by up to {worst:?}"
    );
}

/// A conform says before any export that an original's alpha will not be decoded, as a warning
/// that blocks nothing; VP9 alpha, which this runtime decodes, gets no such entry.
#[test]
fn a_conform_warns_of_alpha_it_will_not_decode() {
    use alpha2_assets::{ConformIssue, MediaPurpose};
    for (name, dropped) in [("hevc-alpha.mov", true), ("vp9-alpha.webm", false)] {
        let resolved = resolve(
            &conform_project(&json!({})),
            &[("video", corpus(name))],
            &[],
            MediaPurpose::ExportOriginal,
        );
        assert!(resolved.report.ready, "{name}: {:?}", resolved.report);
        let warned = resolved
            .report
            .sources
            .iter()
            .flat_map(|source| &source.issues)
            .find_map(|entry| match &entry.issue {
                ConformIssue::AlphaDropped { reason } => Some((reason.clone(), entry.blocking)),
                _ => None,
            });
        if dropped {
            assert_eq!(
                warned,
                Some((
                    "alpha in an HEVC auxiliary layer is not decoded".to_owned(),
                    false
                )),
                "{name}"
            );
        } else {
            assert_eq!(warned, None, "{name}");
        }
    }
}

/// Capabilities list the source codecs whose alpha decodes: `ProRes` 4444, and VP9 on the pinned
/// runtime, which has libvpx's VP9 decoder.
#[test]
fn capabilities_list_the_codecs_whose_alpha_decodes() {
    alpha2_native::require_pinned_ffmpeg_for_tests();
    let decode = crate::protocol::decode_capabilities_json("software", false, false, None);
    assert_eq!(decode["alpha"], json!(["prores", "vp9"]));
}
