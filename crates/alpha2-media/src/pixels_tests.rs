use serde_json::Value;

use super::*;

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/cases/export-container-corpus.json"
    ))
    .expect("container corpus is JSON")
}

fn error_name(error: OutputPixelsError) -> &'static str {
    match error {
        OutputPixelsError::AlphaUnsupported { .. } => "alpha-unsupported",
        OutputPixelsError::AlphaUnsupportedInContainer { .. } => "alpha-unsupported-in-container",
        OutputPixelsError::BitDepthUnsupported { .. } => "bit-depth-unsupported",
    }
}

#[test]
fn the_pixel_policy_matches_the_shared_corpus() {
    let corpus = corpus();
    let cases = corpus["pixels"].as_array().expect("pixel cases");
    // Every codec a container offers, at every depth, with and without alpha.
    let offered: usize = Container::ALL
        .iter()
        .map(|container| container.video_codecs().len())
        .sum();
    assert_eq!(cases.len(), offered * 3 * 2);
    for case in cases {
        let codec: VideoCodec = case["codec"]
            .as_str()
            .expect("codec")
            .parse()
            .expect("known");
        let container: Container = case["container"]
            .as_str()
            .expect("container")
            .parse()
            .expect("known");
        let bit_depth = case["bitDepth"]
            .as_u64()
            .map(|bits| BitDepth::from_bits(u8::try_from(bits).expect("small")).expect("8 or 10"));
        let pixels = OutputPixels {
            bit_depth,
            alpha: case["alpha"].as_bool().expect("alpha"),
        };
        let resolved = resolve_pixels(codec, container, pixels);
        match (&case["ok"], &case["error"]) {
            (Value::String(format), Value::Null) => {
                assert_eq!(
                    resolved.map(PixelLayout::ffmpeg_name),
                    Ok(format.as_str()),
                    "{case}"
                );
            }
            (Value::Null, Value::String(error)) => {
                assert_eq!(resolved.map_err(error_name), Err(error.as_str()), "{case}");
            }
            _ => panic!("a case is ok or an error: {case}"),
        }
    }
}

#[test]
fn prores_is_mov_only() {
    for container in Container::ALL {
        assert_eq!(
            container.offers_video(VideoCodec::ProRes),
            container == Container::Mov,
            "{container}"
        );
    }
}

#[test]
fn vp9_alpha_is_refused_in_mp4() {
    let alpha = OutputPixels {
        bit_depth: None,
        alpha: true,
    };
    assert_eq!(
        resolve_pixels(VideoCodec::Vp9, Container::Mp4, alpha),
        Err(OutputPixelsError::AlphaUnsupportedInContainer {
            codec: VideoCodec::Vp9,
            container: Container::Mp4,
        })
    );
    assert_eq!(
        resolve_pixels(VideoCodec::Vp9, Container::Webm, alpha).map(PixelLayout::ffmpeg_name),
        Ok("yuva420p")
    );
}

#[test]
fn h264_refuses_ten_bit_and_alpha() {
    let ten = OutputPixels {
        bit_depth: Some(BitDepth::Ten),
        alpha: false,
    };
    assert!(matches!(
        resolve_pixels(VideoCodec::H264, Container::Mp4, ten),
        Err(OutputPixelsError::BitDepthUnsupported { .. })
    ));
    let alpha = OutputPixels {
        bit_depth: None,
        alpha: true,
    };
    assert!(matches!(
        resolve_pixels(VideoCodec::H264, Container::Mp4, alpha),
        Err(OutputPixelsError::AlphaUnsupported { .. })
    ));
}

#[test]
fn the_default_pixels_are_the_old_output() {
    for container in Container::ALL {
        for &codec in container.video_codecs() {
            if codec == VideoCodec::ProRes {
                continue;
            }
            assert_eq!(
                resolve_pixels(codec, container, OutputPixels::default()),
                Ok(PixelLayout::YUV420P),
                "{codec:?} in {container}"
            );
        }
    }
}

#[test]
fn planes_follow_the_layout() {
    let ten = PixelLayout {
        chroma: Chroma::Yuv420,
        bit_depth: BitDepth::Ten,
        alpha: false,
        planes: PlaneLayout::Planar,
    };
    assert_eq!(ten.planes(4, 2), [(4, 2, 2), (2, 1, 2), (2, 1, 2)].to_vec());
    assert_eq!(ten.as_p010().planes(4, 2), [(4, 2, 2), (4, 1, 2)].to_vec());
    assert_eq!(ten.as_p010().ffmpeg_name(), "p010le");
    let prores_alpha = PixelLayout {
        chroma: Chroma::Yuv444,
        bit_depth: BitDepth::Ten,
        alpha: true,
        planes: PlaneLayout::Planar,
    };
    assert_eq!(prores_alpha.planes(4, 2).len(), 4);
    // Alpha has no P010 form.
    assert_eq!(prores_alpha.as_p010(), prores_alpha);
}

#[test]
fn a_layout_is_found_by_its_ffmpeg_name() {
    for name in [
        "yuv420p",
        "yuva420p",
        "yuv420p10le",
        "yuva444p10le",
        "p010le",
        "yuv422p10le",
    ] {
        assert_eq!(
            PixelLayout::from_ffmpeg_name(name).map(PixelLayout::ffmpeg_name),
            Some(name)
        );
    }
    assert_eq!(PixelLayout::from_ffmpeg_name("rgba"), None);
}
