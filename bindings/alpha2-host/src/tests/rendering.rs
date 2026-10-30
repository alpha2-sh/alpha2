//! What an export renders: reversed and looped clips, text, color, and runtime effects.

use super::*;

/// A reversed clip of a long-GOP source exports the forward clip's frames in
/// reverse order.
#[test]
fn a_reversed_clip_exports_its_frames_in_reverse_order() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("reverse");
    let end_ticks = alpha2_time::TICKS_PER_SECOND;
    let assets = HashMap::from([("video-source".to_owned(), corpus("h264-long-gop.mp4"))]);
    let export = |time_map: &str| {
        let mut project: Value = serde_json::from_str(include_str!(
            "../../../../tests/cases/host-video-export-project.json"
        ))
        .expect("video export project");
        project["settings"]["width"] = json!(128);
        project["settings"]["height"] = json!(96);
        let clip = &mut project["timeline"]["tracks"][0]["clips"][0];
        clip["timeline_range"]["end"] = json!(end_ticks);
        clip["source_range"]["end"] = json!(end_ticks);
        clip["time_map"] = json!({ "type": time_map });
        let output = directory.join(format!("{time_map}.mp4"));
        let mut host = create_host(VideoDecodePolicy::Software).expect("host");
        host.export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end_ticks,
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
        .expect("export");
        decoded_luma(&output)
    };
    let forward = export("forward");
    let reverse = export("reverse");
    assert_eq!((forward.len(), reverse.len()), (30, 30));
    let distance = |a: &[u8], b: &[u8]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum::<u64>()
    };
    // Encoding is lossy and neighbouring frames can be alike, so the mirrored forward frame must be
    // among the closest to each reversed one, and the forward order must not be.
    let is_closest = |frame: &[u8], candidate: usize| {
        let best = forward
            .iter()
            .map(|other| distance(frame, other))
            .min()
            .expect("frames");
        distance(frame, &forward[candidate]) == best
    };
    for (index, frame) in reverse.iter().enumerate() {
        assert!(is_closest(frame, 29 - index), "reversed frame {index}");
    }
    assert!(
        (0..30)
            .filter(|&index| !is_closest(&reverse[index], index))
            .count()
            > 20,
        "the reversed export is not the forward one"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// The left channel of a file's audio at `sample_rate`.
fn decoded_left(path: &Path, sample_rate: u32) -> Vec<f32> {
    let mut audio = alpha2_native::NativeAudioDecoder::open(path, sample_rate).expect("open audio");
    let cancel = CancelToken::default();
    let mut left = Vec::new();
    loop {
        match audio.decode_next(&cancel).expect("decode audio") {
            alpha2_media::DecodeEvent::Audio(block) => left.extend_from_slice(&block.channels[0]),
            alpha2_media::DecodeEvent::EndOfStream => return left,
            _ => {}
        }
    }
}

/// Two seconds of a white picture over the `swell` audio source, its clip
/// mapped by `time_map` from `0..source_end`.
fn swell_project(sample_rate: u32, time_map: &Value, source_end: i64) -> Value {
    let seconds = alpha2_time::TICKS_PER_SECOND;
    let white = json!({"red": 1, "green": 1, "blue": 1, "alpha": 1});
    json!({
        "schema_version": 1, "id": "reverse-audio", "name": "Reverse audio",
        "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": sample_rate},
        "sources": [
            {"id": "white", "kind": "generated",
             "generator": {"type": "fill", "paint": {"type": "solid", "color": white}}},
            {"id": "swell", "locator": "fixture://swell", "kind": "audio"}
        ],
        "timeline": {"tracks": [
            {"id": "video", "name": "Video", "kind": "video", "clips": [{
                "id": "picture", "source_id": "white",
                "timeline_range": {"start": 0, "end": 2 * seconds},
                "source_range": {"start": 0, "end": 2 * seconds}
            }]},
            {"id": "audio", "name": "Audio", "kind": "audio", "clips": [{
                "id": "sound", "source_id": "swell",
                "timeline_range": {"start": 0, "end": 2 * seconds},
                "source_range": {"start": 0, "end": source_end},
                "time_map": time_map
            }]}
        ]}
    })
}

/// `seconds` of 440 Hz rising linearly from silence to -6 dBFS.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn swell(sample_rate: u32, seconds: usize) -> Vec<f32> {
    let length = sample_rate as usize * seconds;
    (0..length)
        .map(|index| {
            let progress = index as f64 / length as f64;
            (0.5 * progress
                * (std::f64::consts::TAU * 440.0 * index as f64 / f64::from(sample_rate)).sin())
                as f32
        })
        .collect()
}

/// Root-mean-square level of each of the first eight quarter seconds.
#[allow(clippy::cast_precision_loss)]
fn quarter_second_levels(samples: &[f32], sample_rate: u32) -> Vec<f64> {
    samples
        .chunks(sample_rate as usize / 4)
        .take(8)
        .map(|chunk| {
            (chunk
                .iter()
                .map(|value| f64::from(*value).powi(2))
                .sum::<f64>()
                / chunk.len() as f64)
                .sqrt()
        })
        .collect()
}

/// A reversed clip's sound plays backwards: a tone that swells plays
/// fading, and a looped one keeps its level through every pass.
#[test]
fn reversed_and_looped_sound_exports_as_mapped() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("reverse-audio");
    let sample_rate = 48_000_u32;
    let tone_path = directory.join("swell.wav");
    write_float_wav(&tone_path, sample_rate, &swell(sample_rate, 2));
    let seconds = alpha2_time::TICKS_PER_SECOND;
    let export = |time_map: Value, source_end: i64| {
        let project = swell_project(sample_rate, &time_map, source_end);
        let output = directory.join(format!(
            "{}-{source_end}.mp4",
            time_map["type"].as_str().expect("type")
        ));
        let mut host = create_host(VideoDecodePolicy::Software).expect("host");
        host.export(
            &project.to_string(),
            &HashMap::from([("swell".to_owned(), tone_path.clone())]),
            &HashMap::new(),
            Vec::new(),
            0,
            2 * seconds,
            RateRequest { num: 30, den: 1 },
            true,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::Mpeg4Part2,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::VeryHigh),
            None,
            &output,
            &mut |_| Ok(()),
        )
        .expect("export");
        decoded_left(&output, sample_rate)
    };
    let levels = |samples: &[f32]| quarter_second_levels(samples, sample_rate);
    let forward = levels(&export(json!({"type": "forward"}), 2 * seconds));
    let reverse = levels(&export(json!({"type": "reverse"}), 2 * seconds));
    for quarter in 0..8 {
        assert!(
            (reverse[quarter] - forward[7 - quarter]).abs() < forward[7 - quarter] * 0.05,
            "quarter {quarter}: reversed {reverse:?} against forward {forward:?}"
        );
    }
    assert!(reverse[0] > reverse[7] * 4.0, "{reverse:?}");
    let looped = levels(&export(
        json!({"type": "loop", "mode": "repeat"}),
        seconds / 2,
    ));
    for quarter in 0..8 {
        let expected = if quarter % 2 == 0 {
            forward[0]
        } else {
            forward[1]
        };
        assert!(
            (looped[quarter] - expected).abs() < expected * 0.05,
            "quarter {quarter}: looped {looped:?} against {forward:?}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// A clip four times faster than its source, reversed, exports from exactly
/// the media the mix's needs name: the windows hold no readahead, and the
/// first read of a newly opened source does not count as moving backward.
#[test]
fn a_fast_reversed_clip_exports_from_the_media_its_needs_name() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("fast-reverse-audio");
    let sample_rate = 48_000_u32;
    let tone_path = directory.join("swell.wav");
    write_float_wav(&tone_path, sample_rate, &swell(sample_rate, 8));
    let seconds = alpha2_time::TICKS_PER_SECOND;
    let project = swell_project(sample_rate, &json!({"type": "reverse"}), 8 * seconds);
    let output = directory.join("reverse-4x.mp4");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    host.export(
        &project.to_string(),
        &HashMap::from([("swell".to_owned(), tone_path)]),
        &HashMap::new(),
        Vec::new(),
        0,
        2 * seconds,
        RateRequest { num: 30, den: 1 },
        true,
        crate::protocol::ResolvedFormat {
            container: alpha2_media::Container::Mp4,
            video: NativeVideoCodec::Mpeg4Part2,
            audio: NativeAudioCodec::Aac,
            pixels: alpha2_media::PixelLayout::YUV420P,
        },
        None,
        VideoQualityRequest::Level(QualityLevel::High),
        AudioQualityRequest::Level(QualityLevel::VeryHigh),
        None,
        &output,
        &mut |_| Ok(()),
    )
    .expect("export");
    let levels = quarter_second_levels(&decoded_left(&output, sample_rate), sample_rate);
    assert!(levels[0] > levels[7] * 4.0, "{levels:?}");
    std::fs::remove_dir_all(&directory).ok();
}

/// Captions and text generators export with the fonts the request names,
/// and a project that shows text without any fails naming the missing fonts.
#[test]
fn text_exports_with_the_requested_fonts() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("captions");
    let second = alpha2_time::TICKS_PER_SECOND;
    let black = json!({"red": 0, "green": 0, "blue": 0, "alpha": 1});
    let style =
        json!({"font_size": 0.2, "position": {"x": 0.5, "y": 0.5}, "anchor": {"x": 0.5, "y": 0.5}});
    let range = json!({"start": 0, "end": second});
    let project = |text_track: Value| {
        json!({
            "schema_version": 1, "id": "captions", "name": "Captions",
            "settings": {"width": 160, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
            "sources": [
                {"id": "black", "kind": "generated",
                 "generator": {"type": "fill", "paint": {"type": "solid", "color": black}}},
                {"id": "title", "kind": "generated",
                 "generator": {"type": "text", "text": "Hello", "style": style}}
            ],
            "timeline": {"tracks": [
                {"id": "video", "name": "Video", "kind": "video", "clips": [{
                    "id": "clip", "source_id": "black", "timeline_range": range, "source_range": range
                }]},
                text_track
            ]}
        })
    };
    let captions = project(
        json!({"id": "subs", "name": "Subtitles", "kind": "caption", "captions": {
            "style": style,
            "cues": [{"timeline_range": range, "text": "Hello"}]
        }}),
    );
    let titles = project(
        json!({"id": "titles", "name": "Titles", "kind": "video", "clips": [{
            "id": "title-clip", "source_id": "title", "timeline_range": range, "source_range": range
        }]}),
    );
    let export = |host: &mut crate::host::Host, project: &Value, name: &str| {
        host.export(
            &project.to_string(),
            &HashMap::new(),
            &HashMap::new(),
            Vec::new(),
            0,
            second / 10,
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
            directory.join(name),
            &mut |_| Ok(()),
        )
    };
    for (name, project) in [("captions", &captions), ("titles", &titles)] {
        let mut host = create_host(VideoDecodePolicy::Software).expect("host");
        let error = export(&mut host, project, &format!("{name}-no-fonts.mp4"))
            .expect_err("text needs fonts");
        assert!(error.contains("no fonts"), "{name}: {error}");

        host.set_fonts(&[corpus("../fonts/Alpha2Fixture.ttf")])
            .expect("the fixture font");
        let output = directory.join(format!("{name}.mp4"));
        export(&mut host, project, &format!("{name}.mp4")).expect("export");
        let frames = decoded_luma(&output);
        let luma = &frames[0];
        // White text in the middle of a black picture: bright pixels there, and black (limited
        // range) at the corner.
        let at = |x: usize, y: usize| luma[y * 160 + x];
        assert!(
            (40..56).any(|y| (50..110).any(|x| at(x, y) > 180)),
            "{name}: no text"
        );
        assert!(at(2, 2) < 24, "{name}: {}", at(2, 2));
        assert!(
            host.set_fonts(&[corpus("long-gop.mp4")]).is_err(),
            "a video is not a font"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_font_collection_registers_every_face() {
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let files = host
        .set_fonts(&[
            corpus("../fonts/Alpha2Fixture.ttf"),
            corpus("../fonts/Alpha2FixturePair.ttc"),
        ])
        .expect("the fixture fonts");
    // One file read each, and its faces: the fixture's one and the pair's Regular and Bold.
    assert_eq!(files.len(), 2);
    assert_eq!(
        host.texts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .font_count(),
        3
    );
}

#[test]
fn audio_window_rejects_unreported_internal_gaps_and_overlaps() {
    use crate::audio::append_audio_block;
    use alpha2_media::AudioBlock;
    for start_sample in [2, 4] {
        let mut window = AudioBlock {
            start_sample: 0,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::MONO,
            channels: vec![vec![1.0; 3]],
        };
        let next = AudioBlock {
            start_sample,
            sample_rate: 48_000,
            speakers: alpha2_media::SpeakerLayout::MONO,
            channels: vec![vec![1.0; 3]],
        };
        assert!(append_audio_block(&mut window, next).is_err());
        assert_eq!(window.channels[0].len(), 3);
    }
}

#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::used_underscore_binding
)]
fn hdr_sources_decode_and_tone_map_or_are_refused_in_words() {
    // Real PQ and HLG files through the native decoder and upload: the neutral luma ramp on their
    // first row must land where the reference conversion puts it.
    let _gpu = gpu_test_lock();
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_high_performance())
        .expect("a GPU device");
    let renderer = alpha2_render::Renderer::new(gpu);
    let size = RenderSize::new(64, 48).expect("size");
    let tone = alpha2_render::ColorPolicy::ToneMapHdr(alpha2_render::hdr::HdrToneMap::default());
    for (name, transfer) in [
        ("hdr-pq.mp4", alpha2_media::TransferFunction::Pq),
        ("hdr-hlg.mp4", alpha2_media::TransferFunction::Hlg),
    ] {
        let mut decoder = alpha2_native::NativeDecoder::open(corpus(name))
            .unwrap_or_else(|error| panic!("{name} opens: {error:?}"));
        let cancel = CancelToken::default();
        let frame = loop {
            match decoder.decode_next(&cancel).expect("decode") {
                alpha2_media::DecodeEvent::Video(frame) => break frame,
                alpha2_media::DecodeEvent::EndOfStream => panic!("{name}: no frame"),
                _ => {}
            }
        };
        let color = frame.color.expect("tagged");
        assert_eq!(color.transfer, transfer, "{name}");
        assert_eq!(
            color.primaries,
            alpha2_media::ColorPrimaries::Bt2020,
            "{name}"
        );
        let refused = crate::frames::upload_frame(
            &renderer,
            size,
            alpha2_render::ColorPolicy::RejectHdr,
            &frame,
        )
        .err()
        .unwrap_or_else(|| panic!("{name}: refused"));
        assert!(refused.contains("refuses HDR"), "{refused}");
        assert_eq!(HostError::classify(refused).code, "INVALID_INPUT", "{name}");
        let upload = crate::frames::upload_frame(&renderer, size, tone, &frame)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(renderer.submit_pending_source(upload.texture.pending.as_ref().expect("pending")));
        let pixels = renderer
            .read_rgba8(&upload.texture._texture)
            .expect("readback");
        for x in [0_usize, 31, 63] {
            // Row 0 is Y = 64 + 876*x/63, Cb = Cr = 512.
            let code = 64.0 + (876.0 * x as f32 / 63.0).floor();
            let (linear, _) = alpha2_render::convert_yuv_to_linear_rgba(
                alpha2_render::YuvSample {
                    y: code / 1023.0,
                    u: 512.0 / 1023.0,
                    v: 512.0 / 1023.0,
                    alpha: 1.0,
                },
                10,
                Some(color),
                tone,
            )
            .expect("reference");
            let expected = alpha2_render::encode_linear_rgb(
                [linear[0], linear[1], linear[2]],
                alpha2_render::SdrOutputTransfer::Srgb,
            )
            .map(|value| (value * 255.0).round() as u8);
            let pixel = &pixels[x * 4..x * 4 + 3];
            for channel in 0..3 {
                assert!(
                    pixel[channel].abs_diff(expected[channel]) <= 2,
                    "{name} x {x} channel {channel}: {pixel:?} != {expected:?}"
                );
            }
        }
    }
}

/// A wide-gamut SDR file (BT.2020 primaries and matrix, the SDR transfer
/// `bt2020-10`) decodes as SDR, which a renderer refusing HDR still takes,
/// and is converted into BT.709 primaries without a tone map: each flat
/// block lands where the reference conversion of its codes puts it, a colour
/// moves from where the same codes read as BT.709 would, a gray stays gray,
/// and BT.2020's own green, outside BT.709's gamut, clips to green.
#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn a_bt2020_sdr_source_converts_into_bt709_primaries_without_a_tone_map() {
    let name = "bt2020-sdr.mkv";
    let mut decoder = alpha2_native::NativeDecoder::open(corpus(name))
        .unwrap_or_else(|error| panic!("{name} opens: {error:?}"));
    let cancel = CancelToken::default();
    let frame = loop {
        match decoder.decode_next(&cancel).expect("decode") {
            alpha2_media::DecodeEvent::Video(frame) => break frame,
            alpha2_media::DecodeEvent::EndOfStream => panic!("{name}: no frame"),
            _ => {}
        }
    };
    let color = frame.color.expect("tagged");
    assert_eq!(
        color,
        alpha2_media::ColorMetadata {
            primaries: alpha2_media::ColorPrimaries::Bt2020,
            transfer: alpha2_media::TransferFunction::Bt709,
            matrix: alpha2_media::ColorMatrix::Bt2020Ncl,
            range: alpha2_media::ColorRange::Limited,
            dynamic_range: alpha2_media::DynamicRange::Sdr,
        }
    );
    let _gpu = gpu_test_lock();
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_high_performance())
        .expect("a GPU device");
    let renderer = alpha2_render::Renderer::new(gpu);
    let sdr = alpha2_render::ColorPolicy::RejectHdr;
    let pixels = uploaded_first_frame(&renderer, name, sdr);
    let reference = |codes: [f32; 3], color| {
        let (linear, diagnostics) = alpha2_render::convert_yuv_to_linear_rgba(
            alpha2_render::YuvSample {
                y: codes[0] / 1023.0,
                u: codes[1] / 1023.0,
                v: codes[2] / 1023.0,
                alpha: 1.0,
            },
            10,
            Some(color),
            sdr,
        )
        .expect("SDR");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        alpha2_render::encode_linear_rgb(
            [linear[0], linear[1], linear[2]],
            alpha2_render::SdrOutputTransfer::Srgb,
        )
        .map(|value| (value * 255.0).round() as u8)
    };
    let as_bt709 = alpha2_media::ColorMetadata {
        primaries: alpha2_media::ColorPrimaries::Bt709,
        ..color
    };
    // The middle of each 32x24 block, and its Y, Cb, Cr codes (`tests/corpus/generate/media.sh`).
    for (label, (x, y), codes) in [
        ("gray", (16_usize, 12_usize), [470.0, 512.0, 512.0]),
        ("muted green", (48, 12), [520.0, 470.0, 440.0]),
        ("BT.2020 green", (16, 36), [658.0, 189.0, 100.0]),
        ("skin", (48, 36), [600.0, 430.0, 600.0]),
    ] {
        let pixel = &pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        let expected = reference(codes, color);
        for channel in 0..3 {
            assert!(
                pixel[channel].abs_diff(expected[channel]) <= 1,
                "{label} channel {channel}: {pixel:?} != {expected:?}"
            );
        }
        assert_eq!(pixel[3], 255, "{label}");
        let unconverted = reference(codes, as_bt709);
        match label {
            "gray" => {
                assert_eq!(expected, unconverted, "a gray is a gray in both gamuts");
                assert!(pixel[0] == pixel[1] && pixel[1] == pixel[2], "{pixel:?}");
            }
            "BT.2020 green" => assert_eq!(
                [pixel[0], pixel[2]],
                [0, 0],
                "{pixel:?}: outside BT.709, red and blue clip"
            ),
            _ => assert!(
                (0..3).any(|channel| pixel[channel].abs_diff(unconverted[channel]) > 8),
                "{label}: {pixel:?} is the BT.709 reading {unconverted:?}"
            ),
        }
    }
}

/// The first frame of corpus file `name` decoded natively and uploaded with
/// `policy` at its own 64x48, read back as stored: sRGB-encoded and
/// premultiplied RGBA.
#[allow(clippy::used_underscore_binding)]
fn uploaded_first_frame(
    renderer: &alpha2_render::Renderer,
    name: &str,
    policy: alpha2_render::ColorPolicy,
) -> Vec<u8> {
    let mut decoder = alpha2_native::NativeDecoder::open(corpus(name))
        .unwrap_or_else(|error| panic!("{name} opens: {error:?}"));
    let cancel = CancelToken::default();
    let frame = loop {
        match decoder.decode_next(&cancel).expect("decode") {
            alpha2_media::DecodeEvent::Video(frame) => break frame,
            alpha2_media::DecodeEvent::EndOfStream => panic!("{name}: no frame"),
            _ => {}
        }
    };
    let upload = crate::frames::upload_frame(
        renderer,
        RenderSize::new(64, 48).expect("size"),
        policy,
        &frame,
    )
    .unwrap_or_else(|error| panic!("{name}: {error}"));
    assert!(renderer.submit_pending_source(upload.texture.pending.as_ref().expect("pending")));
    renderer
        .read_rgba8(&upload.texture._texture)
        .expect("readback")
}

/// PQ BT.2020 with alpha is a layout the renderer does not sample. It is
/// tone-mapped like the same picture without alpha, and its alpha is kept:
/// opaque pixels match, and half alpha halves them. Converted to RGBA instead,
/// it would show as SDR, PQ codes read as sRGB.
#[allow(clippy::used_underscore_binding)]
#[test]
fn a_frame_the_renderer_cannot_sample_is_tone_mapped_and_keeps_its_alpha() {
    let _gpu = gpu_test_lock();
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_high_performance())
        .expect("a GPU device");
    let renderer = alpha2_render::Renderer::new(gpu);
    let tone = alpha2_render::ColorPolicy::ToneMapHdr(alpha2_render::hdr::HdrToneMap::default());
    let alpha = uploaded_first_frame(&renderer, "prores-pq-alpha.mov", tone);
    let opaque = uploaded_first_frame(&renderer, "prores-pq-opaque.mov", tone);
    for (index, (pixel, reference)) in alpha
        .as_chunks::<4>()
        .0
        .iter()
        .zip(opaque.as_chunks::<4>().0)
        .enumerate()
    {
        let (x, y) = (index % 64, index / 64);
        // Columns 31 and 32 straddle the half-alpha edge, which the bilinear filter blends.
        if y >= 32 && x < 31 {
            assert!(
                pixel[3].abs_diff(128) <= 1,
                "half alpha at ({x}, {y}): {pixel:?}"
            );
            for channel in 0..3 {
                let halved = (f32::from(reference[channel]) * 512.0 / 1023.0).round();
                assert!(
                    (f32::from(pixel[channel]) - halved).abs() <= 2.0,
                    "({x}, {y}) channel {channel}: {pixel:?} against half of {reference:?}"
                );
            }
        } else if y < 31 || x > 32 {
            for channel in 0..4 {
                assert!(
                    pixel[channel].abs_diff(reference[channel]) <= 1,
                    "({x}, {y}) channel {channel}: {pixel:?} against {reference:?}"
                );
            }
        }
    }
}

/// A color the project declares wins over the stream's tags on both native
/// frame paths: the export decode pool's frames carry it, and the render
/// path's upload of a stream that only the declaration calls PQ is the
/// upload of the same stream tagged PQ.
#[allow(clippy::used_underscore_binding)]
#[test]
fn a_declared_color_wins_on_the_native_frame_paths() {
    let declared = alpha2_media::ColorMetadata {
        primaries: alpha2_media::ColorPrimaries::Bt2020,
        transfer: alpha2_media::TransferFunction::Pq,
        matrix: alpha2_media::ColorMatrix::Bt2020Ncl,
        range: alpha2_media::ColorRange::Limited,
        dynamic_range: alpha2_media::DynamicRange::Hdr,
    };
    let mut project: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    project["settings"]["color"] = json!({"hdr": "tone_map", "hdr_peak_nits": 1000});
    project["sources"][0]["color_metadata"] = serde_json::to_value(declared).expect("color");
    let project = Project::from_json(&project.to_string()).expect("project");
    let source = SourceId::new("video-source").expect("source ID");
    let need = FrameNeed {
        reader: None,
        clip_id: ClipId::new("video-clip").expect("clip ID"),
        source_id: source.clone(),
        media_time: MediaTime::ZERO,
    };
    let assets = HashMap::from([(source.as_str().to_owned(), corpus("hdr-pq-untagged.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    let decoded = state
        .decode_need(&need, &CancelToken::default())
        .expect("decodes")
        .expect("a video frame");
    assert_eq!(decoded.frame.metadata().color, Some(declared));

    let _gpu = gpu_test_lock();
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_high_performance())
        .expect("a GPU device");
    let renderer = alpha2_render::Renderer::new(gpu);
    let size = RenderSize::new(64, 48).expect("size");
    let mut frames =
        crate::frames::NativeFrames::new(size, &project, &assets, std::sync::Arc::default())
            .expect("frames");
    frames
        .supply(&renderer, std::slice::from_ref(&need))
        .expect("supplies");
    let uploaded = Arc::clone(
        &frames
            .frames
            .get(&source, MediaTime::ZERO)
            .expect("installed")
            .texture,
    );
    assert!(renderer.submit_pending_source(uploaded.pending.as_ref().expect("pending")));
    let pixels = renderer.read_rgba8(&uploaded._texture).expect("readback");
    let tone = alpha2_render::ColorPolicy::ToneMapHdr(alpha2_render::hdr::HdrToneMap::default());
    assert_eq!(
        pixels,
        uploaded_first_frame(&renderer, "hdr-pq.mp4", tone),
        "the untagged stream declared PQ is shown as the tagged one"
    );
}

/// A still shown through the runtime effect `test.tint@1`.
fn tinted_still_project() -> Value {
    let mut project = still_project([0.0, 0.0, 0.0], "auto");
    project["compositions"] = json!([{
        "id": "look",
        "nodes": [
            {"id": "clip", "op": {"type": "clip"}},
            {"id": "effect", "op": {"type": "effect", "input": "clip",
                "effect": {"id": "test.tint", "version": 1}}}
        ],
        "output": "effect"
    }]);
    project["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({"id": "look"});
    project
}

/// Exports [`tinted_still_project`] through the protocol with `effects` as
/// the request's runtime effect definitions.
fn export_tinted(host: &mut Host, effects: &Value, output: &Path) -> Result<Value, HostError> {
    let request = parse_request(
        &json!({
            "protocolVersion": 1,
            "operation": "export",
            "requestId": "tinted",
            "project": tinted_still_project(),
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png")},
            "effects": effects,
            "startTicks": 0,
            "endTicks": 3 * 4_000,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "mpeg4-part2",
            "output": output,
        })
        .to_string(),
    )
    .expect("request");
    host.handle(request, &mut |_| Ok(()))
}

fn tint(body: &str) -> Value {
    json!({"id": "test.tint", "version": 1, "kind": "effect", "wgslBody": body})
}

const RED_ONLY: &str =
    "fn effect(uv: vec2f, src: vec4f) -> vec4f { return vec4f(src.r, 0.0, 0.0, src.a); }";

const GREEN_ONLY: &str =
    "fn effect(uv: vec2f, src: vec4f) -> vec4f { return vec4f(0.0, src.g, 0.0, src.a); }";

/// Exports a still of `nodes` (composition `look`, output `output`) over a white generated clip,
/// with the image asset as source `image` and `effects` as the request's definitions.
fn export_look(
    host: &mut Host,
    nodes: &Value,
    output: &str,
    effects: &Value,
    path: &Path,
) -> Result<Value, HostError> {
    let mut project = still_project([0.0, 0.0, 0.0], "auto");
    project["sources"] = json!([
        {"id": "image", "locator": "fixture://image", "kind": "image"},
        {"id": "white", "kind": "generated", "generator": {"type": "fill", "paint": {"type": "solid",
            "color": {"red": 1, "green": 1, "blue": 1, "alpha": 1}}}}
    ]);
    project["timeline"]["tracks"][0]["clips"][0]["source_id"] = json!("white");
    project["compositions"] = json!([{"id": "look", "nodes": nodes, "output": output}]);
    project["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({"id": "look"});
    let request = parse_request(
        &json!({
            "protocolVersion": 1,
            "operation": "export",
            "requestId": "look",
            "project": project,
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/parity-image.png")},
            "effects": effects,
            "startTicks": 0,
            "endTicks": 3 * 4_000,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "mpeg4-part2",
            "output": path,
        })
        .to_string(),
    )
    .expect("request");
    host.handle(request, &mut |_| Ok(()))
}

/// An image asset reaches an effect as a named input through a media node, decoded and
/// converted like any media node: a white clip multiplied by the image exports the same frames
/// as the image passed through an effect that returns its source.
#[test]
fn an_export_draws_an_image_through_a_named_input() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("named-input");
    let bytes = |name: &str| sha256_hex(&directory.join(name));
    let mut host = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    let effects = json!([
        {"id": "test.multiply-map", "version": 1, "kind": "effect", "inputs": [{"name": "map"}],
         "wgslBody": "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src * sample_map(uv); }"},
        {"id": "test.same", "version": 1, "kind": "effect",
         "wgslBody": "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }"}
    ]);
    export_look(
        &mut host,
        &json!([
            {"id": "clip", "op": {"type": "clip"}},
            {"id": "map", "op": {"type": "media", "source_id": "image"}},
            {"id": "fx", "op": {"type": "effect", "input": "clip", "inputs": {"map": "map"},
                "effect": {"id": "test.multiply-map", "version": 1}}}
        ]),
        "fx",
        &effects,
        &directory.join("named.mp4"),
    )
    .expect("an image through a named input exports");
    export_look(
        &mut host,
        &json!([
            {"id": "image", "op": {"type": "media", "source_id": "image"}},
            {"id": "fx", "op": {"type": "effect", "input": "image",
                "effect": {"id": "test.same", "version": 1}}}
        ]),
        "fx",
        &effects,
        &directory.join("direct.mp4"),
    )
    .expect("the image through an effect exports");
    export_look(
        &mut host,
        &json!([{"id": "clip", "op": {"type": "clip"}}]),
        "clip",
        &effects,
        &directory.join("white.mp4"),
    )
    .expect("the white clip exports");
    assert_eq!(bytes("named.mp4"), bytes("direct.mp4"));
    assert_ne!(bytes("named.mp4"), bytes("white.mp4"));
}

/// A resident host registers the runtime effect definitions each request
/// carries, so a warm host renders what a fresh one does. Registered once per
/// (id, version) for the host's whole life, a later request's definition would
/// be ignored: an edited body would render as the first one, a fixed body stay
/// rejected, and an effect only an earlier request defined still draw.
#[test]
fn each_export_renders_the_runtime_effects_its_own_request_defines() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("runtime-effects");
    let bytes = |name: &str| sha256_hex(&directory.join(name));
    let mut fresh = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    export_tinted(
        &mut fresh,
        &json!([tint(GREEN_ONLY)]),
        &directory.join("fresh.mp4"),
    )
    .expect("fresh export");
    drop(fresh);

    let mut warm = create_gpu_test_host(VideoDecodePolicy::Software).expect("host");
    export_tinted(
        &mut warm,
        &json!([tint(RED_ONLY)]),
        &directory.join("red.mp4"),
    )
    .expect("first body");
    export_tinted(
        &mut warm,
        &json!([tint(GREEN_ONLY)]),
        &directory.join("green.mp4"),
    )
    .expect("edited body");
    assert_ne!(bytes("red.mp4"), bytes("green.mp4"));
    assert_eq!(
        bytes("fresh.mp4"),
        bytes("green.mp4"),
        "a warm host rendered another request's definition"
    );

    let error = export_tinted(&mut warm, &json!([]), &directory.join("none.mp4"))
        .expect_err("an effect this request does not define");
    assert!(
        error.message.contains("unregistered effect test.tint@1"),
        "{}",
        error.message
    );

    let error = export_tinted(
        &mut warm,
        &json!([tint(
            "fn effect(uv: vec2f, src: vec4f) -> vec4f { return nonsense; }"
        )]),
        &directory.join("broken.mp4"),
    )
    .expect_err("a body that does not compile");
    assert!(
        error.message.contains("registration failed"),
        "{}",
        error.message
    );
    export_tinted(
        &mut warm,
        &json!([tint(GREEN_ONLY)]),
        &directory.join("fixed.mp4"),
    )
    .expect("the fixed body registers");
    assert_eq!(bytes("fresh.mp4"), bytes("fixed.mp4"));

    // A definition that differs only in its named inputs is another definition: this one
    // declares an input the tinted node leaves out, so its frames fail.
    let mut with_input = tint(GREEN_ONLY);
    with_input["inputs"] = json!([{"name": "map"}]);
    let error = export_tinted(
        &mut warm,
        &json!([with_input]),
        &directory.join("input.mp4"),
    )
    .expect_err("a declared input the node leaves out");
    assert_eq!(error.code, "EFFECT_ERROR", "{}", error.message);
    export_tinted(
        &mut warm,
        &json!([tint(GREEN_ONLY)]),
        &directory.join("again.mp4"),
    )
    .expect("the definition without the input registers again");
    assert_eq!(bytes("fresh.mp4"), bytes("again.mp4"));

    let error = export_tinted(
        &mut warm,
        &json!([tint(RED_ONLY), tint(GREEN_ONLY)]),
        &directory.join("twice.mp4"),
    )
    .expect_err("two definitions of one id@version");
    assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
    std::fs::remove_dir_all(&directory).ok();
}
