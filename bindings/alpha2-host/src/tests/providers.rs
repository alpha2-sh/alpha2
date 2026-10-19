//! Extension sources and their providers: export, limits, readers, and budgets.

use super::*;

fn extension_corpus(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus/extensions")
        .join(name)
}

/// A 96x64, 10 fps project: an image sequence for 0.4 s, then a GIF for
/// 0.4 s, under an SVG shown throughout on the track above.
fn extension_project() -> Value {
    json!({
        "schema_version": 1, "id": "extensions", "name": "Extensions",
        "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "seq", "locator": "sequence://frames", "kind": "video", "provider": {
                "format": "image_sequence",
                "frames": ["f7", "f8", "f9", "f10"],
                "frame_rate": {"num": 10, "den": 1},
                "start_number": 7
            }},
            {"id": "gif", "locator": "asset://gif", "kind": "video", "provider": {"format": "animated_image"}},
            {"id": "logo", "locator": "asset://logo", "kind": "image", "provider": {"format": "svg"}}
        ],
        "timeline": {"tracks": [
            {"id": "v1", "name": "V1", "kind": "video", "clips": [
                {"id": "a", "source_id": "seq",
                 "timeline_range": {"start": 0, "end": 48_000},
                 "source_range": {"start": 0, "end": 48_000}},
                {"id": "b", "source_id": "gif",
                 "timeline_range": {"start": 48_000, "end": 96_000},
                 "source_range": {"start": 0, "end": 48_000}}
            ]},
            {"id": "v2", "name": "V2", "kind": "video", "clips": [
                {"id": "c", "source_id": "logo",
                 "timeline_range": {"start": 0, "end": 96_000},
                 "source_range": {"start": 0, "end": 96_000},
                 "visual": {"scale": {"mode": "constant", "value": {"x": 0.25, "y": 0.25}},
                            "position": {"mode": "constant", "value": {"x": 0.125, "y": 0.125}},
                            "anchor": {"x": 0.5, "y": 0.5},
                            "rotation_degrees": {"mode": "constant", "value": 0},
                            "opacity": {"mode": "constant", "value": 1}}}
            ]}
        ]}
    })
}

fn sequence_paths() -> Value {
    json!(
        (7..=10)
            .map(|number| extension_corpus(&format!("sequence_{number:04}.png")))
            .collect::<Vec<_>>()
    )
}

#[test]
#[cfg(feature = "extensions")]
#[allow(clippy::too_many_lines)]
fn extension_sources_export_through_their_providers() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("extensions");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = |frames: Value, output: &Path| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "extensions", "operation": "export",
                "project": extension_project(),
                "assets": {
                    "gif": extension_corpus("animation.gif"),
                    "logo": extension_corpus("shapes.svg")
                },
                "frames": {"seq": frames},
                "startTicks": 0, "endTicks": 96_000,
                "frameRate": {"num": 10, "den": 1},
                "videoCodec": "mpeg4-part2",
                "videoQuality": {"bitrate": 4_000_000},
                "output": output
            })
            .to_string(),
        )
        .expect("request")
    };
    let output = directory.join("extensions.mp4");
    let mut events = Vec::new();
    let result = host
        .handle(request(sequence_paths(), &output), &mut |progress| {
            events.push(progress);
            Ok(())
        })
        .expect("export");
    assert_eq!(result["frames"], json!(8), "{result}");
    let first_finalizing = events
        .iter()
        .position(|event| event.phase == crate::protocol::ProgressPhase::Finalizing)
        .expect("a finalizing event");
    assert!(
        events[first_finalizing..]
            .iter()
            .all(|event| event.phase == crate::protocol::ProgressPhase::Finalizing)
    );
    assert_eq!(events.last().map(|event| event.completed), Some(8));
    for source in ["seq", "gif", "logo"] {
        assert_eq!(
            result["media"][source],
            json!({"media": "original"}),
            "{source}"
        );
    }

    // Frame 0 is the sequence's first picture, contained in the 96x64 canvas at twice its size;
    // frame 4 the GIF's first. Pixels are compared after an MPEG-4 round trip, so loosely.
    let mut decoder = alpha2_native::NativeDecoder::open(&output).expect("open output");
    let cancel = CancelToken::default();
    let mut frames = Vec::new();
    while frames.len() < 5 {
        match decoder.decode_next(&cancel).expect("decode") {
            alpha2_media::DecodeEvent::Video(frame) => frames.push(frame),
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    // Windows refuses to delete a file FFmpeg still holds open.
    drop(decoder);
    let luma = |frame: &alpha2_media::DecodedVideoFrame, x: usize, y: usize| {
        let plane = &frame.planes[0];
        f64::from(plane.bytes[y * plane.stride as usize + x])
    };
    // BT.709 limited luma of the background (20, 40, 90) is about 50, of the yellow block about
    // 185. The GIF's transparent background shows the black project background.
    for (frame, x, y, expected) in [
        (0, 80, 48, 50.0),
        (0, 20, 20, 185.0),
        (4, 80, 48, 16.0),
        (4, 20, 20, 185.0),
    ] {
        let got = luma(&frames[frame], x, y);
        assert!(
            (got - expected).abs() < 12.0,
            "frame {frame} ({x}, {y}): luma {got}, expected {expected}"
        );
    }
    let green = luma(&frames[0], 4, 4);
    assert!((green - 124.0).abs() < 12.0, "SVG green luma {green}");

    let mut broken = sequence_paths();
    broken[2] = json!(directory.join("gone.png"));
    let refused = directory.join("refused.mp4");
    let error = host
        .handle(request(broken, &refused), &mut |_| Ok(()))
        .expect_err("a missing sequence frame");
    assert_eq!(error.code, "CONFORM_FAILED");
    let failure = error.failure.clone().expect("a failure report");
    assert_eq!(failure["code"], "INVALID_INPUT");
    assert_eq!(failure["phase"], "prepare");
    assert_eq!(failure["origin"], "native-host");
    assert_eq!(failure["diagnostics"]["request"]["frameRate"]["num"], 10);
    assert!(failure["diagnostics"]["gpu"]["events"].is_array());
    assert!(failure["diagnostics"]["memory"]["gpu"]["textures"]["highWaterBytes"].is_u64());
    // A failed export's report is not the previous success's: the output path is not recorded
    // before the encoder opens.
    assert!(failure["diagnostics"].get("videoOutputPath").is_none());
    let details = error.details.expect("report");
    let seq = details["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .find(|source| source["source_id"] == json!("seq"))
        .expect("seq reported");
    assert_eq!(seq["issues"][0]["kind"], json!("offline"));
    assert!(!refused.exists());

    // A budget that cannot hold one frame is refused before anything renders or any output opens,
    // as the budget's, not as an engine failure.
    assert_eq!(result["memoryPolicy"]["inFlightBytes"]["source"], "default");
    let mut starved = parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "starved", "operation": "export",
            "project": extension_project(),
            "assets": {
                "gif": extension_corpus("animation.gif"),
                "logo": extension_corpus("shapes.svg")
            },
            "frames": {"seq": sequence_paths()},
            "startTicks": 0, "endTicks": 96_000,
            "frameRate": {"num": 10, "den": 1},
            "videoCodec": "mpeg4-part2",
            "memory": {"inFlightBytes": 1024},
            "output": directory.join("starved.mp4")
        })
        .to_string(),
    )
    .expect("request");
    let error = host
        .handle(starved, &mut |_| Ok(()))
        .expect_err("a budget below one frame");
    assert_eq!(error.code, "RESOURCE_LIMIT", "{}", error.message);
    let failure = error.failure.expect("a failure report");
    assert_eq!(failure["code"], "RESOURCE_LIMIT");
    assert_eq!(
        failure["diagnostics"]["memoryPolicy"]["inFlightBytes"]["bytes"],
        1024
    );
    assert!(!directory.join("starved.mp4").exists());
    starved = parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "zero", "operation": "export",
            "project": extension_project(), "assets": {}, "startTicks": 0, "endTicks": 1,
            "frameRate": {"num": 10, "den": 1}, "memory": {"decodeQueueBytes": 0},
            "output": directory.join("zero.mp4")
        })
        .to_string(),
    )
    .expect("request");
    let error = host
        .handle(starved, &mut |_| Ok(()))
        .expect_err("a zero budget");
    assert_eq!(error.code, "INVALID_INPUT");
    assert!(
        error.message.contains("memory.decodeQueueBytes"),
        "{}",
        error.message
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// A Lottie source shows the animation it names, or its package's first:
/// conform opens that one, so a package whose first animation is refused
/// conforms only for a source that names another.
#[test]
#[cfg(feature = "extensions")]
fn a_lottie_source_conforms_for_the_animation_it_shows() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("lottie-item");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let mut export = |provider: Value, name: &str| {
        let output = directory.join(name);
        let request = parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "lottie-item", "operation": "export",
                "project": {
                    "schema_version": 1, "id": "lottie", "name": "Lottie",
                    "settings": {"width": 64, "height": 48, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
                    "sources": [{"id": "anim", "locator": "asset://anim", "kind": "video", "provider": provider}],
                    "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
                        {"id": "a", "source_id": "anim",
                         "timeline_range": {"start": 0, "end": 24_000},
                         "source_range": {"start": 0, "end": 24_000}}
                    ]}]}
                },
                "assets": {"anim": extension_corpus("first-refused.lottie")},
                "startTicks": 0, "endTicks": 24_000,
                "frameRate": {"num": 10, "den": 1},
                "videoCodec": "mpeg4-part2",
                "output": output
            })
            .to_string(),
        )
        .expect("request");
        host.handle(request, &mut |_| Ok(())).map(|_| output)
    };
    let error = export(json!({"format": "lottie"}), "first.mp4")
        .expect_err("the package's first animation is refused");
    assert_eq!(error.code, "CONFORM_FAILED", "{}", error.message);
    assert!(
        error.message.contains("anim: unsupported"),
        "{}",
        error.message
    );
    let output = export(
        json!({"format": "lottie", "animation": "moving"}),
        "moving.mp4",
    )
    .expect("the package's second animation exports");
    assert!(output.is_file());
}

/// A Lottie, dotLottie or SVG file larger than its provider reads is refused
/// from its size and first bytes, before the rest of it is read: a file of a
/// tebibyte, sparse on disk, is refused at once rather than allocated for.
/// A file within its limit is read whole.
#[test]
#[cfg(feature = "extensions")]
fn a_document_file_past_its_limit_is_refused_before_it_is_read() {
    let directory = scratch_directory("document-limit");
    let lottie = alpha2_model::ProviderSource::Lottie { animation: None };
    for (name, head, spec, what) in [
        ("huge.json", &b"{\"w\":"[..], &lottie, "Lottie file"),
        (
            "huge.lottie",
            &b"PK\x03\x04"[..],
            &lottie,
            "dotLottie package",
        ),
        (
            "huge.svg",
            &b"<svg"[..],
            &alpha2_model::ProviderSource::Svg,
            "SVG document",
        ),
    ] {
        let path = directory.join(name);
        sparse_tebibyte(&path, head);
        let error = crate::providers::read_source(&path, spec).expect_err(name);
        assert!(
            matches!(&error, alpha2_extensions::ExtensionError::Budget(message)
                if message.contains(what) && message.contains("1099511627776 bytes")),
            "{name}: {error}"
        );
    }
    let path = directory.join("small.svg");
    std::fs::write(&path, b"<svg/>").expect("file");
    assert_eq!(
        crate::providers::read_source(&path, &alpha2_model::ProviderSource::Svg).expect("read"),
        b"<svg/>"
    );
}

/// Conform finds a Lottie source whose file is larger than the provider reads
/// unsupported from the file's size, saying so, without reading it: allocating
/// for a sparse tebibyte file whole would fail as out of memory.
#[test]
#[cfg(feature = "extensions")]
fn a_lottie_source_past_its_limit_fails_conform_unread() {
    let directory = scratch_directory("lottie-limit");
    let asset = directory.join("huge.json");
    sparse_tebibyte(&asset, br#"{"v":"5.7.0","w":64,"h":48"#);
    let project = json!({
        "schema_version": 1, "id": "lottie", "name": "Lottie",
        "settings": {"width": 64, "height": 48, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
        "sources": [{"id": "anim", "locator": "asset://anim", "kind": "video", "provider": {"format": "lottie"}}],
        "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
            {"id": "a", "source_id": "anim",
             "timeline_range": {"start": 0, "end": 24_000},
             "source_range": {"start": 0, "end": 24_000}}
        ]}]}
    });
    let resolved = resolve(
        &project,
        &[("anim", asset)],
        &[],
        alpha2_assets::MediaPurpose::ExportOriginal,
    );
    assert!(!resolved.report.ready);
    let alpha2_assets::OriginalState::Unsupported { reason } = &resolved.catalog.assets[0].original
    else {
        panic!("{:?}", resolved.catalog.assets[0].original);
    };
    assert!(
        reason.contains("too large: the Lottie file is 1099511627776 bytes"),
        "{reason}"
    );
}

/// A build without `extensions` reads no provider format: its capability
/// report says so, and an export of such sources is refused at conform,
/// naming the reason, rather than rendering them blank.
#[test]
#[cfg(not(feature = "extensions"))]
fn a_build_without_extensions_refuses_their_sources() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("no-extensions");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "no-extensions", "operation": "export",
            "project": extension_project(),
            "assets": {
                "gif": extension_corpus("animation.gif"),
                "logo": extension_corpus("shapes.svg")
            },
            "frames": {"seq": sequence_paths()},
            "startTicks": 0, "endTicks": 96_000,
            "frameRate": {"num": 10, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": directory.join("refused.mp4")
        })
        .to_string(),
    )
    .expect("request");
    let error = host
        .handle(request, &mut |_| Ok(()))
        .expect_err("no provider reads these sources");
    assert_eq!(error.code, "CONFORM_FAILED", "{}", error.message);
    for source in ["seq", "gif", "logo"] {
        assert!(
            error.message.contains(&format!("{source}: unsupported")),
            "{}",
            error.message
        );
    }
    let details = error.details.expect("the conform report").to_string();
    assert!(
        details.contains("this build has no provider for the source's format"),
        "{details}"
    );
    assert!(!directory.join("refused.mp4").exists());
}

#[test]
#[cfg(feature = "extensions")]
fn exr_sources_export_as_linear_light() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("exr");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let output = directory.join("exr.mp4");
    let project = json!({
        "schema_version": 1, "id": "exr", "name": "OpenEXR",
        "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "seq", "locator": "sequence://exr", "kind": "video", "provider": {
                "format": "image_sequence", "frames": ["f1", "f2"],
                "frame_rate": {"num": 10, "den": 1}, "start_number": 1
            }},
            {"id": "still", "locator": "asset://half", "kind": "image", "provider": {"format": "exr"}}
        ],
        "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
            {"id": "a", "source_id": "seq",
             "timeline_range": {"start": 0, "end": 24_000},
             "source_range": {"start": 0, "end": 24_000}},
            {"id": "b", "source_id": "still",
             "timeline_range": {"start": 24_000, "end": 36_000},
             "source_range": {"start": 0, "end": 12_000}}
        ]}]}
    });
    let request = parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "exr", "operation": "export",
            "project": project,
            "assets": {"still": extension_corpus("half.exr")},
            "frames": {"seq": [extension_corpus("sequence_0001.exr"), extension_corpus("sequence_0002.exr")]},
            "startTicks": 0, "endTicks": 36_000,
            "frameRate": {"num": 10, "den": 1},
            "videoCodec": "mpeg4-part2",
            "videoQuality": {"bitrate": 4_000_000},
            "output": output
        })
        .to_string(),
    )
    .expect("request");
    let result = host.handle(request, &mut |_| Ok(())).expect("export");
    assert_eq!(result["frames"], json!(3), "{result}");

    let mut decoder = alpha2_native::NativeDecoder::open(&output).expect("open output");
    let cancel = CancelToken::default();
    let mut frames = Vec::new();
    while frames.len() < 3 {
        match decoder.decode_next(&cancel).expect("decode") {
            alpha2_media::DecodeEvent::Video(frame) => frames.push(frame),
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    // Windows refuses to delete a file FFmpeg still holds open.
    drop(decoder);
    let luma = |frame: &alpha2_media::DecodedVideoFrame, x: usize, y: usize| {
        let plane = &frame.planes[0];
        f64::from(plane.bytes[y * plane.stride as usize + x])
    };
    // Linear 0.25 and 0.75 are limited luma 134 and 209. The still's first cell (2.0, 0.5, -0.25)
    // clips to luma 178; halfway to its transparent second cell the linear-light mix gives luma
    // 134, where mixing sRGB codes would give 97.
    for (frame, x, y, expected) in [
        (0, 48, 32, 134.0),
        (1, 48, 32, 209.0),
        (2, 12, 20, 178.0),
        (2, 24, 20, 134.0),
    ] {
        let got = luma(&frames[frame], x, y);
        assert!(
            (got - expected).abs() < 6.0,
            "frame {frame} ({x}, {y}): luma {got}, expected {expected}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// An OpenEXR frame reaches the upload as the halves its source decoded:
/// two frames of a still share the still's pixels instead of each being a
/// frame-sized copy of them as bytes (66 MB a 4K frame).
#[test]
#[cfg(all(feature = "extensions", target_endian = "little"))]
fn an_exr_frame_is_handed_over_without_copying_its_halves() {
    let project = Project::from_json(
        &json!({
            "schema_version": 1, "id": "exr", "name": "OpenEXR",
            "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
            "sources": [
                {"id": "still", "locator": "asset://half", "kind": "image", "provider": {"format": "exr"}}
            ],
            "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
                {"id": "a", "source_id": "still",
                 "timeline_range": {"start": 0, "end": 24_000},
                 "source_range": {"start": 0, "end": 24_000}}
            ]}]}
        })
        .to_string(),
    )
    .expect("project");
    let media = crate::providers::ProviderMedia::new(
        &project,
        crate::providers::ProviderFiles {
            originals: HashMap::from([("still".to_owned(), extension_corpus("half.exr"))]),
            ..Default::default()
        },
        (96, 64),
        u64::MAX,
    );
    let still = provider_reader("still", "a");
    let first = media.frame(&still, MediaTime::ZERO, 0).expect("frame 0");
    let second = media
        .frame(&still, MediaTime::from_ticks(12_000), 1)
        .expect("frame 1");
    assert_eq!(first.pixel_format, PixelFormat::LinearRgbaF16);
    let (plane, again) = (&first.planes[0], &second.planes[0]);
    assert_eq!(
        plane.bytes.len(),
        plane.stride as usize * plane.height as usize
    );
    // Both frames are alive, so equal addresses are one buffer: the source's.
    assert_eq!(
        plane.bytes.as_ptr(),
        again.bytes.as_ptr(),
        "each frame copied the still's halves"
    );
    assert_eq!(plane.bytes, again.bytes);
}

/// Writes `head` to a new file at `path` and extends it to a tebibyte that
/// takes no space on disk. NTFS reserves the clusters of a file extended past
/// its end unless the file is marked sparse first, so there it is.
#[cfg(feature = "extensions")]
fn sparse_tebibyte(path: &Path, head: &[u8]) {
    std::fs::write(path, head).expect("head");
    #[cfg(windows)]
    {
        let status = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("fsutil");
        assert!(status.success(), "fsutil sparse setflag: {status}");
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("file")
        .set_len(1 << 40)
        .expect("sparse");
}

#[test]
#[cfg(feature = "extensions")]
fn a_sequence_reads_each_frame_file_when_the_export_shows_it() {
    // Opening a sequence reads no frame file: reading every one into memory, the first frame would
    // wait for the whole sequence, and the export would hold its size.
    let directory = scratch_directory("sequence-reads");
    let paths = (7..=10)
        .map(|number| {
            let name = format!("sequence_{number:04}.png");
            let path = directory.join(&name);
            std::fs::copy(extension_corpus(&name), &path).expect("copy frame");
            Some(path)
        })
        .collect::<Vec<_>>();
    let project = Project::from_json(&extension_project().to_string()).expect("project");
    let media = crate::providers::ProviderMedia::new(
        &project,
        crate::providers::ProviderFiles {
            frames: HashMap::from([("seq".to_owned(), paths.clone())]),
            ..Default::default()
        },
        (96, 64),
        u64::MAX,
    );
    let seq = provider_reader("seq", "seq-clip");
    std::fs::remove_file(paths[3].as_ref().expect("path")).expect("remove");
    let first = media.frame(&seq, MediaTime::ZERO, 0).expect("frame 0");
    assert_eq!(
        (first.visible_rect.width, first.visible_rect.height),
        (48, 32)
    );
    media
        .frame(&seq, MediaTime::from_ticks(24_000), 2)
        .expect("frame 2");
    let error = media
        .frame(&seq, MediaTime::from_ticks(36_000), 3)
        .expect_err("frame 3 is gone");
    assert!(error.contains("sequence_0010.png"), "{error}");
    std::fs::remove_dir_all(&directory).ok();
}

/// An export's caption fonts are read once, when it starts: an SVG opened
/// later in the export draws its text with those bytes, never reading the
/// font files again, so one gone by then still draws.
#[cfg(feature = "extensions")]
#[test]
fn an_svg_draws_with_the_fonts_its_export_read_at_its_start() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("svg-fonts");
    let font = directory.join("caption.ttf");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    // Nine seconds at 10 fps; the label shows only in the last second, long after the first
    // progress event.
    let project = json!({
        "schema_version": 1, "id": "label", "name": "Label",
        "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
        "sources": [{"id": "label", "locator": "asset://label", "kind": "image", "provider": {"format": "svg"}}],
        "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
            {"id": "a", "source_id": "label",
             "timeline_range": {"start": 960_000, "end": 1_080_000},
             "source_range": {"start": 0, "end": 120_000}}
        ]}]}
    });
    let mut export = |output: &Path, remove_font: bool| {
        std::fs::copy(corpus("../fonts/Alpha2Fixture.ttf"), &font).expect("font");
        let request = parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "svg-fonts", "operation": "export",
                "project": project,
                "assets": {"label": extension_corpus("label.svg")},
                "fonts": [font],
                "startTicks": 0, "endTicks": 1_080_000,
                "frameRate": {"num": 10, "den": 1},
                "includeAudio": false,
                "videoCodec": "mpeg4-part2",
                "videoQuality": {"bitrate": 4_000_000},
                "output": output
            })
            .to_string(),
        )
        .expect("request");
        let result = host
            .handle(request, &mut |_| {
                if remove_font {
                    let _ = std::fs::remove_file(&font);
                }
                Ok(())
            })
            .expect("export");
        assert!(!remove_font || !font.exists());
        result
    };
    let kept = directory.join("kept.mp4");
    let removed = directory.join("removed.mp4");
    export(&kept, false);
    export(&removed, true);
    assert!(
        std::fs::read(&kept).expect("kept") == std::fs::read(&removed).expect("removed"),
        "the label lost its text once the font file was gone"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// What an extension provider could not draw as asked is reported with the
/// export, not left out silently: an SVG's text with no font for it is in
/// the result's `providerDiagnostics`, once however many frames show it,
/// and in a failed export's report.
#[cfg(feature = "extensions")]
#[test]
fn provider_diagnostics_are_reported_with_the_export_and_its_failure() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("provider-diagnostics");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = |output: &Path| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "provider-diagnostics", "operation": "export",
                "project": {
                    "schema_version": 1, "id": "label", "name": "Label",
                    "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
                    "sources": [{"id": "label", "locator": "asset://label", "kind": "image", "provider": {"format": "svg"}}],
                    "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
                        {"id": "a", "source_id": "label",
                         "timeline_range": {"start": 0, "end": 480_000},
                         "source_range": {"start": 0, "end": 480_000}}
                    ]}]}
                },
                "assets": {"label": extension_corpus("label.svg")},
                "startTicks": 0, "endTicks": 480_000,
                "frameRate": {"num": 10, "den": 1},
                "includeAudio": false,
                "videoCodec": "mpeg4-part2",
                "output": output
            })
            .to_string(),
        )
        .expect("request")
    };
    let expected = json!([{"source": "label", "kind": "missing_font", "families": "DejaVu Sans"}]);
    let result = host
        .handle(request(&directory.join("a.mp4")), &mut |_| Ok(()))
        .expect("export");
    assert_eq!(result["providerDiagnostics"], expected, "{result}");
    assert_eq!(result["providerDiagnosticsOmitted"], json!(0));
    // A client that stops the export after its first progress event fails it; the report still says
    // what the SVG could not draw.
    let failed = host
        .handle(request(&directory.join("b.mp4")), &mut |_| {
            Err(HostError::new("CANCELLED", "the client stopped the export"))
        })
        .expect_err("stopped");
    let failure = failed.failure.as_ref().expect("failure report");
    assert_eq!(
        failure["diagnostics"]["providerDiagnostics"], expected,
        "{failure}"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// An SVG's `<image href>` to a file beside it arrives by the request's
/// `resources`, which give a source its files by locator as a browser page
/// registers the same locator: drawn, and no `missing_image`. Resources for a
/// source that reads none are refused.
#[cfg(feature = "extensions")]
#[test]
fn an_svg_draws_the_image_files_its_resources_give_it() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("provider-resources");
    let svg = directory.join("badge.svg");
    std::fs::write(
        &svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="64"><image href="art/badge.png" x="0" y="0" width="96" height="64"/></svg>"#,
    )
    .expect("svg");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = |output: &Path, resources: Value| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "provider-resources", "operation": "export",
                "project": {
                    "schema_version": 1, "id": "badge", "name": "Badge",
                    "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
                    "sources": [{"id": "badge", "locator": "asset://badge", "kind": "image", "provider": {"format": "svg"}}],
                    "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [
                        {"id": "a", "source_id": "badge",
                         "timeline_range": {"start": 0, "end": 120_000},
                         "source_range": {"start": 0, "end": 120_000}}
                    ]}]}
                },
                "assets": {"badge": svg},
                "resources": resources,
                "startTicks": 0, "endTicks": 120_000,
                "frameRate": {"num": 10, "den": 1},
                "includeAudio": false,
                "videoCodec": "mpeg4-part2",
                "output": output
            })
            .to_string(),
        )
        .expect("request")
    };
    let without = directory.join("without.mp4");
    let missing = host
        .handle(request(&without, json!({})), &mut |_| Ok(()))
        .expect("export");
    assert_eq!(
        missing["providerDiagnostics"],
        json!([{"source": "badge", "kind": "missing_image", "href": "art/badge.png"}]),
        "{missing}"
    );
    let with = directory.join("with.mp4");
    let drawn = host
        .handle(
            request(
                &with,
                json!({"badge": {"art/badge.png": corpus("parity-image.png")}}),
            ),
            &mut |_| Ok(()),
        )
        .expect("export");
    assert_eq!(drawn["providerDiagnostics"], json!([]), "{drawn}");
    assert_ne!(
        std::fs::read(&without).expect("without"),
        std::fs::read(&with).expect("with"),
        "the image was not drawn"
    );
    let refused = host
        .handle(
            request(
                &directory.join("refused.mp4"),
                json!({"elsewhere": {"art/badge.png": corpus("parity-image.png")}}),
            ),
            &mut |_| Ok(()),
        )
        .expect_err("a source that reads no resources");
    assert_eq!(refused.code, "INVALID_INPUT", "{}", refused.message);
    assert!(refused.message.contains("elsewhere"), "{}", refused.message);
    std::fs::remove_dir_all(&directory).ok();
}

/// The decode cursor of `clip` reading provider source `source` directly.
#[cfg(feature = "extensions")]
fn provider_reader(source: &str, clip: &str) -> crate::decode::DecodeCursorKey {
    (
        SourceId::new(source).expect("source ID"),
        ClipId::new(clip).expect("clip ID"),
        None,
    )
}

/// Writes an APNG of `frames` noise frames of `side`x`side` pixels, each
/// shown for 0.1 s.
#[cfg(feature = "extensions")]
fn write_paced_apng(path: &Path, side: u32, frames: u32) {
    let file = std::fs::File::create(path).expect("animation file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), side, side);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_animated(frames, 0).expect("animated");
    encoder.set_frame_delay(1, 10).expect("delay");
    let mut writer = encoder.write_header().expect("header");
    for frame in 0..frames {
        let value = u8::try_from(frame % 256).expect("byte");
        let pixels = [value, value, value, 255].repeat((side * side) as usize);
        writer.write_image_data(&pixels).expect("frame");
    }
    writer.finish().expect("animation");
}

/// A 96x64, 10 fps project of an animated image `anim` and an SVG `logo`,
/// with the animation shown by clips `a` and `b` on two tracks, `b` 3 s
/// further into it.
#[cfg(feature = "extensions")]
fn two_reader_project() -> Project {
    let clip = |id: &str, source: &str, offset: i64| {
        json!({"id": id, "source_id": source,
               "timeline_range": {"start": 0, "end": 360_000},
               "source_range": {"start": offset, "end": offset + 360_000}})
    };
    Project::from_json(
        &json!({
            "schema_version": 1, "id": "readers", "name": "Readers",
            "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
            "sources": [
                {"id": "anim", "locator": "asset://anim", "kind": "video", "provider": {"format": "animated_image"}},
                {"id": "logo", "locator": "asset://logo", "kind": "image", "provider": {"format": "svg"}}
            ],
            "timeline": {"tracks": [
                {"id": "v1", "name": "V1", "kind": "video", "clips": [clip("a", "anim", 0)]},
                {"id": "v2", "name": "V2", "kind": "video", "clips": [clip("b", "anim", 360_000)]},
                {"id": "v3", "name": "V3", "kind": "video", "clips": [clip("l", "logo", 0)]}
            ]}
        })
        .to_string(),
    )
    .expect("project")
}

/// Two clips showing one animated image at unrelated times each have their own
/// open animation, as two readers of one video file each have their own
/// decoder. An animation's decoder is positioned on its next frame, so a reader
/// behind it that shared it would restart it from the first frame, and past the
/// frames it keeps every output frame would decode it again up to both readers.
/// An SVG draws any time as cheaply, and its readers share one.
#[cfg(feature = "extensions")]
#[test]
fn readers_of_one_animated_image_keep_their_own_positions() {
    let directory = scratch_directory("animation-readers");
    let path = directory.join("long.png");
    write_paced_apng(&path, 8, 60);
    let media = crate::providers::ProviderMedia::new(
        &two_reader_project(),
        crate::providers::ProviderFiles {
            originals: HashMap::from([
                ("anim".to_owned(), path),
                ("logo".to_owned(), extension_corpus("shapes.svg")),
            ]),
            ..Default::default()
        },
        (96, 64),
        u64::MAX,
    );
    let at = |index: u64| MediaTime::from_ticks(i64::try_from(index).expect("index") * 12_000);
    for frame in 0..30_u64 {
        for (clip, index) in [("a", frame), ("b", frame + 30)] {
            let shown = media
                .frame(&provider_reader("anim", clip), at(index), frame)
                .expect("frame");
            assert_eq!(
                shown.planes[0].bytes[0],
                u8::try_from(index).expect("byte"),
                "clip {clip} at frame {frame}"
            );
        }
    }
    assert_eq!(media.open_sources(), 2, "one animation per reader");
    for clip in ["l", "m"] {
        media
            .frame(&provider_reader("logo", clip), MediaTime::ZERO, 30)
            .expect("logo");
    }
    assert_eq!(media.open_sources(), 3, "one SVG for its readers");
    std::fs::remove_dir_all(&directory).ok();
}

/// The readers of one animated image share what opening it decoded: the
/// first reader reads the file and decodes it whole once, to check it; the
/// readers after it, made while one is open, decode none of it again, and
/// the frames kept between requests are one frame budget for all of them.
/// Each still has its own decoder, and closing one for the budget frees
/// only what was its own.
#[cfg(feature = "extensions")]
#[test]
fn readers_of_one_animated_image_share_its_decode_and_kept_frames() {
    const READERS: u64 = 8;
    const FRAMES: u32 = 60;
    let directory = scratch_directory("animation-shared");
    let path = directory.join("long.png");
    write_paced_apng(&path, 32, FRAMES);
    let files = || crate::providers::ProviderFiles {
        originals: HashMap::from([
            ("anim".to_owned(), path.clone()),
            ("logo".to_owned(), extension_corpus("shapes.svg")),
        ]),
        ..Default::default()
    };
    let decoded = std::rc::Rc::new(std::cell::Cell::new(0_u64));
    let counter = std::rc::Rc::clone(&decoded);
    let _watch = alpha2_extensions::watch_progress(move || counter.set(counter.get() + 1));
    let at = |index: u64| MediaTime::from_ticks(i64::try_from(index).expect("index") * 12_000);
    let reader = |index: u64| provider_reader("anim", &format!("r{index}"));
    let shown = |media: &crate::providers::ProviderMedia, reader_index: u64, index: u64, frame| {
        let shown = media
            .frame(&reader(reader_index), at(index), frame)
            .expect("frame");
        assert_eq!(
            shown.planes[0].bytes[0],
            u8::try_from(index).expect("byte"),
            "reader {reader_index} at frame {frame}"
        );
    };

    let one =
        crate::providers::ProviderMedia::new(&two_reader_project(), files(), (96, 64), u64::MAX);
    shown(&one, 0, 0, 0);
    let one_reader = one.resident_bytes();
    assert_eq!(
        decoded.get(),
        u64::from(FRAMES),
        "the first reader checks the file once"
    );

    decoded.set(0);
    let media =
        crate::providers::ProviderMedia::new(&two_reader_project(), files(), (96, 64), u64::MAX);
    for index in 0..READERS {
        shown(&media, index, index * 5, 0);
    }
    assert_eq!(
        decoded.get(),
        u64::from(FRAMES),
        "{READERS} readers decoded {} frames of a {FRAMES}-frame file",
        decoded.get()
    );
    assert_eq!(
        media.open_sources(),
        usize::try_from(READERS).expect("readers")
    );
    assert_eq!(
        media.resident_bytes(),
        one_reader,
        "what {READERS} readers hold, every frame they show kept"
    );
    for frame in 1..10 {
        for index in 0..READERS {
            shown(&media, index, index * 5 + frame, frame);
        }
    }
    assert_eq!(decoded.get(), u64::from(FRAMES));

    // A budget below one reader closes every reader the last two frames did not show; closing
    // them frees nothing the readers still open share.
    let tight = crate::providers::ProviderMedia::new(&two_reader_project(), files(), (96, 64), 1);
    for index in 0..READERS {
        shown(&tight, index, index, index);
    }
    assert_eq!(tight.open_sources(), 2);
    assert_eq!(tight.resident_bytes(), one_reader);
    assert_eq!(tight.peak_bytes(), one_reader);
    // Four readers shown long ago all close: each that closes leaves what they share held by the
    // reader still shown, so the budget is still passed after every one.
    for index in 0..4 {
        shown(&tight, index, index, 20);
    }
    shown(&tight, 4, 4, 30);
    assert_eq!(tight.open_sources(), 1);
    assert_eq!(tight.resident_bytes(), one_reader);
    std::fs::remove_dir_all(&directory).ok();
}

/// Two shards opening readers of one animated image at once decode it
/// once: the second waits for the first reader's open and is made from it.
#[cfg(feature = "extensions")]
#[test]
fn readers_opening_at_once_decode_an_animated_image_once() {
    const FRAMES: u32 = 12;
    let directory = scratch_directory("animation-opening");
    let path = directory.join("long.png");
    write_paced_apng(&path, 8, FRAMES);
    let media = Arc::new(crate::providers::ProviderMedia::new(
        &two_reader_project(),
        crate::providers::ProviderFiles {
            originals: HashMap::from([
                ("anim".to_owned(), path),
                ("logo".to_owned(), extension_corpus("shapes.svg")),
            ]),
            ..Default::default()
        },
        (96, 64),
        u64::MAX,
    ));
    let decoded = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (entered, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let first = {
        let media = Arc::clone(&media);
        let decoded = Arc::clone(&decoded);
        std::thread::spawn(move || {
            let hold = std::sync::Mutex::new(Some((entered, released)));
            let _watch = alpha2_extensions::watch_progress(move || {
                decoded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some((entered, released)) = hold
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    entered.send(()).ok();
                    released.recv().ok();
                }
            });
            media.frame(&provider_reader("anim", "a"), MediaTime::ZERO, 0)
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the first reader started decoding");
    let second = {
        let media = Arc::clone(&media);
        let decoded = Arc::clone(&decoded);
        std::thread::spawn(move || {
            let _watch = alpha2_extensions::watch_progress(move || {
                decoded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            media.frame(
                &provider_reader("anim", "b"),
                MediaTime::from_ticks(12_000),
                0,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let waited = !second.is_finished();
    release.send(()).ok();
    let first = first.join().expect("first").expect("first frame");
    let second = second.join().expect("second").expect("second frame");
    assert!(
        waited,
        "the second reader did not wait for the first's open"
    );
    assert_eq!(first.planes[0].bytes[0], 0);
    assert_eq!(second.planes[0].bytes[0], 1);
    assert_eq!(
        decoded.load(std::sync::atomic::Ordering::Relaxed),
        FRAMES,
        "two readers opening at once decoded the file once"
    );
    assert_eq!(media.open_sources(), 2);
    std::fs::remove_dir_all(&directory).ok();
}

/// A shard drawing one provider source never waits for another shard
/// drawing a different one: each source has its own lock, so an SVG is drawn
/// while an animation is still decoding.
#[cfg(feature = "extensions")]
#[test]
fn different_provider_sources_are_drawn_at_once() {
    let directory = scratch_directory("provider-locks");
    let path = directory.join("long.png");
    write_paced_apng(&path, 8, 4);
    let media = Arc::new(crate::providers::ProviderMedia::new(
        &two_reader_project(),
        crate::providers::ProviderFiles {
            originals: HashMap::from([
                ("anim".to_owned(), path),
                ("logo".to_owned(), extension_corpus("shapes.svg")),
            ]),
            ..Default::default()
        },
        (96, 64),
        u64::MAX,
    ));
    let (entered, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let animation = {
        let media = Arc::clone(&media);
        std::thread::spawn(move || {
            let hold = std::sync::Mutex::new(Some((entered, released)));
            let _watch = alpha2_extensions::watch_progress(move || {
                if let Some((entered, released)) = hold
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    entered.send(()).ok();
                    released.recv().ok();
                }
            });
            media.frame(&provider_reader("anim", "a"), MediaTime::ZERO, 0)
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the animation started decoding");
    let logo = {
        let media = Arc::clone(&media);
        std::thread::spawn(move || media.frame(&provider_reader("logo", "l"), MediaTime::ZERO, 0))
    };
    let started = Instant::now();
    while !logo.is_finished() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let drawn_meanwhile = logo.is_finished();
    release.send(()).ok();
    animation
        .join()
        .expect("animation")
        .expect("animation frame");
    logo.join().expect("logo").expect("logo frame");
    assert!(drawn_meanwhile, "the SVG waited for the animation's render");
    std::fs::remove_dir_all(&directory).ok();
}

/// A 96x64, 10 fps project of provider sources one after another, each for
/// 0.2 s: two GIFs of one file, the first again, then an SVG; the SVG also
/// shows beside the first GIF at the start, and both GIFs show at once,
/// side by side, for 0.2 s.
#[cfg(feature = "extensions")]
fn provider_succession_project() -> Value {
    let clip = |id: &str, source: &str, start: i64, x: f64| {
        json!({"id": id, "source_id": source,
               "timeline_range": {"start": start, "end": start + 24_000},
               "source_range": {"start": 0, "end": 24_000},
               "visual": {"scale": {"mode": "constant", "value": {"x": 0.5, "y": 0.5}},
                          "position": {"mode": "constant", "value": {"x": x, "y": 0.5}},
                          "anchor": {"x": 0.5, "y": 0.5},
                          "rotation_degrees": {"mode": "constant", "value": 0},
                          "opacity": {"mode": "constant", "value": 1}}})
    };
    json!({
        "schema_version": 1, "id": "succession", "name": "Succession",
        "settings": {"width": 96, "height": 64, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "gif", "locator": "asset://gif", "kind": "video", "provider": {"format": "animated_image"}},
            {"id": "gif2", "locator": "asset://gif2", "kind": "video", "provider": {"format": "animated_image"}},
            {"id": "logo", "locator": "asset://logo", "kind": "image", "provider": {"format": "svg"}}
        ],
        "timeline": {"tracks": [
            {"id": "v1", "name": "V1", "kind": "video", "clips": [
                clip("a", "gif", 0, 0.5),
                clip("b", "gif2", 24_000, 0.5),
                clip("c", "gif", 48_000, 0.25),
                clip("e", "logo", 72_000, 0.5)
            ]},
            {"id": "v2", "name": "V2", "kind": "video", "clips": [
                clip("z", "logo", 0, 0.75),
                clip("d", "gif2", 48_000, 0.75)
            ]}
        ]}
    })
}

#[test]
#[cfg(feature = "extensions")]
fn the_provider_sources_an_export_keeps_open_hold_at_most_their_budget() {
    // Provider sources close under the budget: kept open to the end, each with its own
    // decoded-frame budget, what they hold would grow with their number.
    let project = Project::from_json(&provider_succession_project().to_string()).expect("project");
    let files = || crate::providers::ProviderFiles {
        originals: HashMap::from([
            ("gif".to_owned(), extension_corpus("animation.gif")),
            ("gif2".to_owned(), extension_corpus("animation.gif")),
            ("logo".to_owned(), extension_corpus("shapes.svg")),
        ]),
        ..Default::default()
    };
    let source = |id: &str| provider_reader(id, &format!("{id}-clip"));
    let at = |ms: i64| MediaTime::from_ticks(ms * 120);
    let unbounded = crate::providers::ProviderMedia::new(&project, files(), (96, 64), u64::MAX);
    unbounded.frame(&source("gif"), at(0), 0).expect("gif");
    let one_gif = unbounded.resident_bytes();
    assert!(one_gif > 0);
    unbounded.frame(&source("logo"), at(0), 0).expect("logo");
    let logo = unbounded.resident_bytes() - one_gif;
    assert!(logo > 0 && logo < one_gif);
    let media = crate::providers::ProviderMedia::new(&project, files(), (96, 64), one_gif);
    let reference = crate::providers::ProviderMedia::new(&project, files(), (96, 64), u64::MAX);
    // (output frame, the sources it shows at a source time, what stays open after it)
    let frames: [(u64, &[&str], i64, u64); 9] = [
        (0, &["gif"], 0, one_gif),
        (1, &["gif"], 100, one_gif),
        (2, &["gif2"], 0, 2 * one_gif),
        (3, &["gif2"], 100, one_gif),
        // Both GIFs in one frame stay open past the budget: the frame's least working set.
        (4, &["gif", "gif2"], 150, 2 * one_gif),
        (5, &["gif"], 50, 2 * one_gif),
        (6, &["gif"], 100, one_gif),
        (7, &["logo"], 0, one_gif + logo),
        (8, &["logo"], 0, logo),
    ];
    for (frame, shown, time, open) in frames {
        for id in shown {
            let got = media.frame(&source(id), at(time), frame).expect("frame");
            let expected = reference
                .frame(&source(id), at(time), frame)
                .expect("reference");
            assert_eq!(
                got.planes[0].bytes, expected.planes[0].bytes,
                "{id} at frame {frame}"
            );
        }
        assert_eq!(media.resident_bytes(), open, "after frame {frame}");
    }
    assert_eq!(media.reopens(), 1);
    assert_eq!(media.peak_bytes(), 2 * one_gif);
    assert_eq!(media.open_sources(), 1);
    assert_eq!(reference.open_sources(), 3);
}

#[test]
#[cfg(feature = "extensions")]
fn an_export_closes_provider_sources_past_its_budget_and_writes_the_same_file() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("provider-budget");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let mut export = |memory: Value, output: &Path| {
        host.handle(
            parse_request(
                &json!({
                    "protocolVersion": 1, "requestId": "provider-budget", "operation": "export",
                    "project": provider_succession_project(),
                    "assets": {
                        "gif": extension_corpus("animation.gif"),
                        "gif2": extension_corpus("animation.gif"),
                        "logo": extension_corpus("shapes.svg")
                    },
                    "startTicks": 0, "endTicks": 96_000,
                    "frameRate": {"num": 10, "den": 1},
                    "videoCodec": "mpeg4-part2",
                    "videoQuality": {"bitrate": 4_000_000},
                    "memory": memory,
                    "output": output
                })
                .to_string(),
            )
            .expect("request"),
            &mut |_| Ok(()),
        )
        .expect("export")
    };
    let open = directory.join("open.mp4");
    let kept = export(json!({}), &open);
    assert_eq!(
        kept["memoryPolicy"]["providerSourceBytes"]["source"],
        "default"
    );
    assert_eq!(kept["performance"]["providerSourceReopens"], 0, "{kept}");
    let held = kept["performance"]["peakProviderSourceBytes"]
        .as_u64()
        .expect("peak");
    // A one-byte budget keeps only what this frame and the one before show. An animation is
    // opened per clip, so a later clip opens it anew rather than again.
    let closed = directory.join("closed.mp4");
    let bounded = export(json!({"providerSourceBytes": 1}), &closed);
    assert_eq!(
        bounded["memoryPolicy"]["providerSourceBytes"],
        json!({"bytes": 1, "source": "request"})
    );
    assert!(
        bounded["performance"]["providerSourceReopens"]
            .as_u64()
            .expect("reopens")
            >= 1,
        "{bounded}"
    );
    let peak = bounded["performance"]["peakProviderSourceBytes"]
        .as_u64()
        .expect("peak");
    assert!(peak <= held, "bounded {peak}, unbounded {held}");
    assert_eq!(
        std::fs::read(&open).expect("open"),
        std::fs::read(&closed).expect("closed"),
        "closing and opening sources again changed the output"
    );
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
#[cfg(feature = "extensions")]
fn conform_refuses_a_sequence_whose_files_miss_a_frame_the_project_lists() {
    // A null file where the project lists a frame fails conform, which checks each listed frame,
    // not only the count of files; the export would fail when the sequence first rendered.
    use alpha2_assets::MediaPurpose;
    let project = Project::from_json(&extension_project().to_string()).expect("project");
    let resolve = |frames: Vec<Option<PathBuf>>| {
        let originals = HashMap::from([
            ("gif".to_owned(), extension_corpus("animation.gif")),
            ("logo".to_owned(), extension_corpus("shapes.svg")),
        ]);
        let proxies = HashMap::new();
        let frames = HashMap::from([("seq".to_owned(), frames)]);
        let media = crate::assets::RequestMedia {
            originals: &originals,
            proxies: &proxies,
            frames: &frames,
        };
        crate::assets::resolve_media(
            &project,
            &media,
            &media,
            MediaPurpose::ExportOriginal,
            &crate::assets::HardwareVideoDecode::None,
        )
    };
    let all = (7..=10)
        .map(|number| Some(extension_corpus(&format!("sequence_{number:04}.png"))))
        .collect::<Vec<_>>();
    assert!(resolve(all.clone()).report.ready);
    let mut holed = all;
    holed[1] = None;
    let refused = resolve(holed);
    assert!(!refused.report.ready, "a listed frame without a file");
    let summary = crate::assets::blocking_summary(&refused.report);
    assert!(summary.contains("seq"), "{summary}");
}
