use alpha2_time::FrameRate;

#[test]
fn video_rate_table_matches_the_pinned_cases() {
    let table: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/cases/video-rate-table.json"))
            .expect("rate table is JSON");
    for case in table["cases"].as_array().expect("cases") {
        let codec: crate::VideoCodec = case["codec"]
            .as_str()
            .expect("codec")
            .parse()
            .expect("codec");
        let level: crate::QualityLevel = case["level"]
            .as_str()
            .expect("level")
            .parse()
            .expect("level");
        let rate = FrameRate::new(
            u32::try_from(case["frameRate"]["num"].as_u64().expect("num")).expect("num"),
            u32::try_from(case["frameRate"]["den"].as_u64().expect("den")).expect("den"),
        )
        .expect("rate");
        let resolved = crate::resolve_video_rate_control(
            codec,
            u32::try_from(case["width"].as_u64().expect("width")).expect("width"),
            u32::try_from(case["height"].as_u64().expect("height")).expect("height"),
            rate,
            level,
        )
        .expect("resolves");
        assert_eq!(
            serde_json::to_value(resolved).expect("serializes"),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn the_pinned_table_has_a_case_for_every_codec_a_sink_can_reach() {
    // Every codec parses, so every codec reaches a sink: the browser exporter offers each but
    // MPEG-4 Part 2, and the native host takes each its FFmpeg encodes. A codec's numbers are
    // pinned before they ship, measured or not.
    let table: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/cases/video-rate-table.json"))
            .expect("rate table is JSON");
    let cases = table["cases"].as_array().expect("cases");
    for codec in crate::VideoCodec::ALL {
        assert!(
            cases
                .iter()
                .any(|case| case["codec"].as_str() == Some(codec.as_str())),
            "tests/cases/video-rate-table.json pins no {} case",
            codec.as_str()
        );
    }
}

#[test]
fn every_level_is_strictly_above_the_one_below_it() {
    let rate = FrameRate::new(30, 1).expect("rate");
    for codec in crate::VideoCodec::ALL {
        for (width, height) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            let rates = crate::QualityLevel::ALL.map(|level| {
                crate::resolve_video_rate_control(codec, width, height, rate, level)
                    .expect("resolves")
                    .bitrate
            });
            assert!(
                rates.windows(2).all(|pair| pair[0] < pair[1]),
                "{codec:?} {width}x{height}: {rates:?}"
            );
        }
    }
}

#[test]
fn an_unknown_codec_names_its_kind_and_the_names_that_kind_takes() {
    // A video codec name handed to the audio parser is not an audio codec, and the error must
    // not suggest it is one to pass.
    for name in ["mp3", "h264", "AAC", "mp4a.40.2"] {
        let error = name
            .parse::<crate::AudioCodec>()
            .expect_err("not an audio codec")
            .to_string();
        assert_eq!(
            error,
            format!("unknown audio codec \"{name}\"; expected aac or opus")
        );
    }
    for name in ["aac", "avc", "H264"] {
        let error = name
            .parse::<crate::VideoCodec>()
            .expect_err("not a video codec")
            .to_string();
        assert_eq!(
            error,
            format!(
                "unknown video codec \"{name}\"; expected one of h264, hevc, vp9, av1, mpeg4-part2"
            )
        );
    }
}

#[test]
fn a_zero_dimension_does_not_resolve() {
    let rate = FrameRate::new(30, 1).expect("rate");
    assert_eq!(
        crate::resolve_video_rate_control(
            crate::VideoCodec::H264,
            0,
            1080,
            rate,
            crate::QualityLevel::High
        ),
        Err(crate::RateControlError::EmptyFrame)
    );
}
