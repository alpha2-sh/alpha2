//! Conform: media resolution, fingerprints, proxies, and refusals before any output.

use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn media_is_resolved_hashed_and_selected_before_anything_renders() {
    use alpha2_assets::{MediaPurpose, OriginalState, Selection};
    let clip = corpus("parity-rgb.mov");
    let hash = sha256_hex(&clip);

    // A recorded hash is checked; the found fingerprint says what the demuxer saw.
    let resolved = resolve(
        &conform_project(&json!({"content_sha256": hash})),
        &[("video", clip.clone())],
        &[],
        MediaPurpose::ExportOriginal,
    );
    assert!(resolved.report.ready, "{:?}", resolved.report);
    assert_eq!(
        resolved.catalog.assets.len(),
        1,
        "unused sources are not probed"
    );
    let OriginalState::Online { fingerprint } = &resolved.catalog.assets[0].original else {
        panic!("online: {:?}", resolved.catalog.assets[0].original);
    };
    assert_eq!(fingerprint.content_sha256.as_deref(), Some(hash.as_str()));
    let video = fingerprint
        .streams
        .as_ref()
        .and_then(|streams| streams.video.as_ref())
        .expect("video stream");
    assert_eq!((video.width, video.height), (96, 64));
    assert_eq!(fingerprint.duration_ticks, Some(120_000));
    assert_eq!(resolved.paths.get("video"), Some(&clip));

    // Nothing recorded to compare: no full read of the file.
    let unhashed = resolve(
        &conform_project(&json!({})),
        &[("video", clip.clone())],
        &[],
        MediaPurpose::ExportOriginal,
    );
    let OriginalState::Online { fingerprint } = &unhashed.catalog.assets[0].original else {
        panic!("online");
    };
    assert_eq!(fingerprint.content_sha256, None);

    // Other bytes than the project recorded: a preview warns, an export refuses.
    let other = conform_project(&json!({"content_sha256": "0".repeat(64)}));
    assert!(
        resolve(
            &other,
            &[("video", clip.clone())],
            &[],
            MediaPurpose::Preview
        )
        .report
        .ready
    );
    let refused = resolve(
        &other,
        &[("video", clip.clone())],
        &[],
        MediaPurpose::ExportOriginal,
    );
    assert!(!refused.report.ready);
    assert!(
        crate::assets::blocking_summary(&refused.report).contains("video: fingerprint mismatch"),
        "{}",
        crate::assets::blocking_summary(&refused.report)
    );

    let gone = PathBuf::from("/nonexistent/alpha2/video.mov");
    let recorded = conform_project(&json!({}));
    let preview = resolve(
        &recorded,
        &[("video", gone.clone())],
        &[("video", clip.clone())],
        MediaPurpose::Preview,
    );
    assert!(preview.report.ready);
    assert_eq!(
        preview.report.sources[0].selection,
        Selection::Proxy {
            id: "video#proxy0".into()
        }
    );
    assert_eq!(preview.paths.get("video"), Some(&clip));
    let original = resolve(
        &recorded,
        &[("video", gone.clone())],
        &[("video", clip.clone())],
        MediaPurpose::ExportOriginal,
    );
    assert!(!original.report.ready);
    assert_eq!(
        original.report.sources[0].usable_proxy.as_deref(),
        Some("video#proxy0")
    );
    assert!(
        original.paths.is_empty(),
        "nothing is read in place of an original"
    );

    let unsupported = resolve(
        &recorded,
        &[("video", corpus("MANIFEST.md"))],
        &[],
        MediaPurpose::Preview,
    );
    assert!(matches!(
        unsupported.catalog.assets[0].original,
        OriginalState::Unsupported { .. }
    ));
    assert!(!unsupported.report.ready);
    let missing = resolve(&recorded, &[], &[], MediaPurpose::Preview);
    assert!(
        crate::assets::blocking_summary(&missing.report).ends_with("video: missing"),
        "{}",
        crate::assets::blocking_summary(&missing.report)
    );
}

/// A fingerprint records the size a picture is shown at, as a browser
/// reports it: a JPEG's Exif `Orientation` and a display matrix that
/// mirrors and turns both swap a stored size's sides.
#[test]
fn a_fingerprint_records_the_size_a_picture_is_shown_at() {
    use crate::assets::{HardwareVideoDecode, fingerprint};
    for (name, still, shown) in [
        ("exif-orientation-1.jpg", true, (64, 48)),
        // Stored 48x64, turned a quarter to show 64x48.
        ("exif-orientation-6.jpg", true, (64, 48)),
        ("exif-orientation-7.jpg", true, (64, 48)),
        ("h264-bt709-hflip.mp4", false, (128, 96)),
        ("h264-bt709-transverse.mp4", false, (96, 128)),
        // 128x96 of 4:3 pixels, declared by `pasp` or only in the SPS VUI: widened as a browser
        // demuxer widens them.
        ("h264-bt709-pasp.mp4", false, (171, 96)),
        ("h264-bt709-vui-sar.ts", false, (171, 96)),
        ("h264-bt709-vui-sar.mp4", false, (171, 96)),
    ] {
        let found = fingerprint(&corpus(name), false, still, &HardwareVideoDecode::None)
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        let video = found.streams.and_then(|streams| streams.video);
        assert_eq!(
            video.map(|video| (video.width, video.height)),
            Some(shown),
            "{name}"
        );
    }
}

/// A fingerprint finds the streams a probe selects by their stream index, not
/// by their place among the audio and video streams, so a subtitle first does
/// not hide the audio at `streams[2]`. A track no decoder reads and a song's
/// cover art are not media the source has.
#[test]
fn a_fingerprint_records_the_streams_a_reader_plays() {
    use crate::assets::{HardwareVideoDecode, fingerprint};
    for (name, video, audio) in [
        ("subtitle-first.mkv", true, true),
        ("audio-undecodable.mp4", true, false),
        ("cover-art.m4a", false, true),
    ] {
        let found = fingerprint(&corpus(name), false, false, &HardwareVideoDecode::None)
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        let streams = found.streams.expect("streams");
        assert_eq!(
            (streams.video.is_some(), streams.audio.is_some()),
            (video, audio),
            "{name}"
        );
    }
}

/// A proxy stands in for an original only if it shows the same shape, its
/// pixel aspect ratio applied: a 4:3 picture of 4:3 pixels shows 16:9 and is
/// no proxy of a 4:3 picture of square pixels, and the same stream shown
/// 16:9 by its codec alone is one of it. A browser compares the same shapes.
#[test]
fn a_proxy_shows_the_shape_its_original_shows() {
    use alpha2_assets::{MediaPurpose, Selection};
    let proxied = |original: &str, proxy: &str| {
        let resolved = resolve(
            &conform_project(&json!({})),
            &[("video", corpus(original))],
            &[("video", corpus(proxy))],
            MediaPurpose::ExportProxy,
        );
        let source = &resolved.report.sources[0];
        (
            source.selection.clone(),
            serde_json::to_value(&source.issues).expect("issues"),
        )
    };
    // MP4s without audio, all from zero: a transport stream starts at 1.47 s, which no MP4 proxy
    // from zero stands in for.
    let (selection, issues) = proxied("h264-bt709-level.mp4", "h264-bt709-pasp.mp4");
    assert_eq!(selection, Selection::Original, "{issues}");
    let problem = issues
        .as_array()
        .and_then(|issues| issues.iter().find(|issue| issue["kind"] == "proxy"))
        .map(|issue| issue["problem"].clone());
    assert_eq!(
        problem,
        Some(json!({"kind": "invalid", "reasons": ["aspect"]})),
        "{issues}"
    );
    let (selection, issues) = proxied("h264-bt709-pasp.mp4", "h264-bt709-vui-sar.mp4");
    assert_eq!(
        selection,
        Selection::Proxy {
            id: "video#proxy0".into()
        },
        "{issues}"
    );
}

#[test]
fn conform_reads_av1_without_a_hardware_session() {
    // The pinned SDK decodes AV1 in software with libdav1d, so no host needs a hardware session to
    // read it.
    use crate::assets::{HardwareVideoDecode, fingerprint};
    use alpha2_assets::{MediaPurpose, OriginalState};
    let av1 = corpus("av1.mp4");
    for hardware in [
        HardwareVideoDecode::None,
        HardwareVideoDecode::Codecs(vec!["h264", "hevc"]),
        HardwareVideoDecode::Codecs(vec!["h264", "av1"]),
        HardwareVideoDecode::PerStream,
    ] {
        let found = fingerprint(&av1, false, false, &hardware).expect("AV1 decodes in software");
        let video = found.streams.and_then(|streams| streams.video);
        assert_eq!(
            video.map(|video| (video.width, video.height)),
            Some((64, 48))
        );
    }
    let software = resolve(
        &conform_project(&Value::Null),
        &[("video", av1)],
        &[],
        MediaPurpose::ExportOriginal,
    );
    assert!(
        !matches!(
            &software.catalog.assets[0].original,
            OriginalState::Unsupported { .. }
        ),
        "{:?}",
        software.catalog.assets[0].original
    );
}

#[test]
fn a_panic_handling_a_request_fails_that_request_and_the_host_answers_the_next() {
    // A provider or demuxer panic fails only that request: unwinding out of the serve loop would
    // end a resident host without an answer.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("panic");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let clip = corpus("parity-rgb.mov");
    let conform = || {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "conform", "operation": "conform",
                "project": conform_project(&json!({})),
                "assets": {"video": clip}
            })
            .to_string(),
        )
        .expect("conform request")
    };
    let export = |output: &Path| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "export", "operation": "export",
                "project": conform_project(&json!({})),
                "assets": {"video": clip},
                "startTicks": 0, "endTicks": 30_000,
                "frameRate": {"num": 12, "den": 1},
                "videoCodec": "mpeg4-part2",
                "output": output
            })
            .to_string(),
        )
        .expect("export request")
    };
    crate::host::PANIC_IN_CONFORM.set(true);
    let error = host
        .handle(conform(), &mut |_| Ok(()))
        .expect_err("the conform panics");
    assert_eq!(error.code, "INTERNAL");
    assert!(
        error.message.contains("injected conform panic"),
        "{}",
        error.message
    );
    assert!(error.failure.is_none(), "a conform is not an export");
    host.handle(conform(), &mut |_| Ok(()))
        .expect("the next conform is answered");

    let refused = directory.join("refused.mp4");
    crate::host::PANIC_IN_CONFORM.set(true);
    let error = host
        .handle(export(&refused), &mut |_| Ok(()))
        .expect_err("the export's conform panics");
    assert_eq!(error.code, "INTERNAL");
    let failure = error.failure.expect("an export failure report");
    assert_eq!(failure["code"], "UNKNOWN");
    assert_eq!(failure["hostCode"], "INTERNAL");
    assert_eq!(failure["phase"], "prepare");
    assert!(!refused.exists());
    let output = directory.join("after.mp4");
    host.handle(export(&output), &mut |_| Ok(()))
        .expect("the next export runs");
    assert!(output.exists());
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn an_encode_worker_that_cannot_start_fails_the_export_and_leaves_no_partial() {
    // At a process or cgroup thread limit, thread creation fails, and the host reports the export's
    // failure instead of panicking.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("encode-thread");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let clip = corpus("parity-rgb.mov");
    let export = |output: &Path| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "export", "operation": "export",
                "project": conform_project(&json!({})),
                "assets": {"video": clip},
                "startTicks": 0, "endTicks": 30_000,
                "frameRate": {"num": 12, "den": 1},
                "videoCodec": "mpeg4-part2",
                "output": output
            })
            .to_string(),
        )
        .expect("export request")
    };
    let output = directory.join("out.mp4");
    crate::encode::REFUSE_ENCODE_THREAD.set(true);
    let error = host
        .handle(export(&output), &mut |_| Ok(()))
        .expect_err("the export fails");
    assert_eq!(error.code, "ENCODE_ERROR", "{}", error.message);
    assert!(
        error
            .message
            .contains("failed to start native encode worker"),
        "{}",
        error.message
    );
    assert!(error.failure.is_some());
    // The sink was dropped with the thread's closure, removing its partial, so the same output is
    // not refused as stale.
    let left = std::fs::read_dir(&directory)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    assert!(left.is_empty(), "{left:?}");
    host.handle(export(&output), &mut |_| Ok(()))
        .expect("the retry exports");
    assert!(output.exists());
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn an_export_refuses_unready_media_before_any_output_and_says_what_it_read() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("conform");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let clip = corpus("parity-rgb.mov");
    let request = |media: &str, assets: Value, proxies: Value, output: &Path| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "conform", "operation": "export",
                "project": conform_project(&json!({})),
                "assets": assets, "proxies": proxies, "media": media,
                "startTicks": 0, "endTicks": 120_000,
                "frameRate": {"num": 12, "den": 1},
                "videoCodec": "mpeg4-part2",
                "output": output
            })
            .to_string(),
        )
        .expect("request")
    };
    let gone = directory.join("gone.mov");
    let refused_output = directory.join("refused.mp4");
    let error = host
        .handle(
            request(
                "original",
                json!({"video": gone}),
                json!({"video": [{"path": clip}]}),
                &refused_output,
            ),
            &mut |_| Ok(()),
        )
        .expect_err("an offline original refuses an original export");
    assert_eq!(error.code, "CONFORM_FAILED");
    let details = error.details.expect("the report");
    assert_eq!(details["ready"], json!(false));
    assert_eq!(details["sources"][0]["usable_proxy"], json!("video#proxy0"));
    assert!(!refused_output.exists(), "no output is started");

    let proxied_output = directory.join("proxied.mp4");
    let result = host
        .handle(
            request(
                "proxies",
                json!({"video": gone}),
                json!({"video": [{"path": clip}]}),
                &proxied_output,
            ),
            &mut |_| Ok(()),
        )
        .expect("a proxy export reads the proxy");
    assert_eq!(
        result["media"]["video"],
        json!({"media": "proxy", "id": "video#proxy0"})
    );
    assert!(proxied_output.exists());
    let original = host
        .handle(
            request(
                "original",
                json!({"video": clip}),
                json!({}),
                &directory.join("original.mp4"),
            ),
            &mut |_| Ok(()),
        )
        .expect("an original export");
    assert_eq!(result["frames"], original["frames"]);
    assert_eq!(original["media"]["video"], json!({"media": "original"}));
    std::fs::remove_dir_all(&directory).ok();
}

/// A 96x64, 12 fps project whose one clip reads `video` only through a
/// composition: shown as a composition source, or drawn by a group node.
fn nested_media_project(through: &str) -> Value {
    let span = json!({"start": 0, "end": 120_000});
    let vignette = json!({"id": "alpha2.effect.vignette", "version": 1});
    let (sources, compositions, clip) = match through {
        "composition-source" => (
            json!([
                {"id": "video", "locator": "asset://video", "kind": "video"},
                {"id": "card", "kind": "composition", "composition": "card-look"}
            ]),
            json!([{"id": "card-look", "nodes": [
                {"id": "video", "op": {"type": "media", "source_id": "video"}},
                {"id": "fx", "op": {"type": "effect", "input": "video", "effect": vignette}}
            ], "output": "fx"}]),
            json!({"id": "clip", "source_id": "card", "timeline_range": span, "source_range": span}),
        ),
        "group" => (
            json!([
                {"id": "still", "locator": "asset://still", "kind": "image"},
                {"id": "video", "locator": "asset://video", "kind": "video"}
            ]),
            json!([
                {"id": "look", "nodes": [
                    {"id": "clip", "op": {"type": "clip"}},
                    {"id": "inset", "op": {"type": "group", "composition": "inset", "inputs": ["clip"]}}
                ], "output": "inset"},
                {"id": "inset", "inputs": ["image"], "nodes": [
                    {"id": "in", "op": {"type": "group_input", "index": 0}},
                    {"id": "video", "op": {"type": "media", "source_id": "video"}},
                    {"id": "mix", "op": {"type": "merge", "layers": [
                        {"input": "in"},
                        {"input": "video", "opacity": {"mode": "constant", "value": 0.5}}
                    ]}}
                ], "output": "mix"}
            ]),
            json!({"id": "clip", "source_id": "still", "composition": {"id": "look"},
                "timeline_range": span, "source_range": span}),
        ),
        other => unreachable!("{other}"),
    };
    json!({
        "schema_version": 1, "id": "nested", "name": "Nested",
        "settings": {"width": 96, "height": 64, "frame_rate": {"num": 12, "den": 1}, "sample_rate": 48_000},
        "sources": sources,
        "compositions": compositions,
        "timeline": {"tracks": [{"id": "v1", "name": "V1", "kind": "video", "clips": [clip]}]}
    })
}

#[test]
fn media_only_a_group_or_composition_source_reads_is_checked_and_decoded() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("nested-media");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let video = corpus("parity-rgb.mov");
    let still = corpus("parity-image.png");
    for (through, assets) in [
        ("composition-source", json!({"video": video})),
        ("group", json!({"still": still, "video": video})),
    ] {
        let request = |assets: &Value, output: &Path| {
            parse_request(
                &json!({
                    "protocolVersion": 1, "requestId": through, "operation": "export",
                    "project": nested_media_project(through),
                    "assets": assets,
                    "startTicks": 0, "endTicks": 120_000,
                    "frameRate": {"num": 12, "den": 1},
                    "videoCodec": "mpeg4-part2",
                    "output": output
                })
                .to_string(),
            )
            .expect("request")
        };
        let mut missing = assets.clone();
        missing.as_object_mut().expect("assets").remove("video");
        let refused_output = directory.join(format!("{through}-refused.mp4"));
        let error = host
            .handle(request(&missing, &refused_output), &mut |_| Ok(()))
            .expect_err("a missing video refuses the export");
        assert_eq!(error.code, "CONFORM_FAILED", "{through}: {}", error.message);
        let details = error.details.expect("the report");
        assert!(
            details["sources"]
                .as_array()
                .expect("sources")
                .iter()
                .any(|source| source["source_id"] == "video"
                    && source["issues"][0]["kind"] == "missing"),
            "{through}: {details}"
        );
        assert!(!refused_output.exists(), "{through}: no output is started");
        let output = directory.join(format!("{through}.mp4"));
        let result = host
            .handle(request(&assets, &output), &mut |_| Ok(()))
            .unwrap_or_else(|error| panic!("{through}: {}", error.message));
        assert_eq!(result["frames"], json!(12), "{through}");
        assert_eq!(
            result["media"]["video"],
            json!({"media": "original"}),
            "{through}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// The native host records the evidence a browser records of the same
/// bytes (`tests/cases/media-evidence-corpus.json`, which the browser editor's
/// tests hold to the same answers): the size a video is shown at, a
/// stretch only its parameter sets signal included, a variable-rate video's
/// measured rate, not `FFmpeg`'s guess of its base rate, and where a
/// transport stream that starts late ends, not its length.
#[test]
fn a_fingerprint_records_the_evidence_every_host_records() {
    use crate::assets::{HardwareVideoDecode, fingerprint};
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases/media-evidence-corpus.json");
    let corpus_cases: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("JSON");
    for case in corpus_cases["files"].as_array().expect("files") {
        let name = case["file"].as_str().expect("file");
        let found = fingerprint(&corpus(name), false, false, &HardwareVideoDecode::None)
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        let rate = found
            .streams
            .as_ref()
            .and_then(|streams| streams.video.as_ref()?.frame_rate);
        let expected = case["frame_rate"].as_object().map(|rate| {
            alpha2_time::FrameRate::new(
                u32::try_from(rate["num"].as_u64().expect("num")).expect("u32"),
                u32::try_from(rate["den"].as_u64().expect("den")).expect("u32"),
            )
            .expect("rate")
        });
        assert_eq!(rate, expected, "{name}: frame rate");
        assert_eq!(
            found.duration_ticks,
            case["duration_ticks"].as_i64(),
            "{name}: duration"
        );
        let shown = found
            .streams
            .as_ref()
            .and_then(|streams| streams.video.as_ref())
            .map(|video| (u64::from(video.width), u64::from(video.height)));
        let size = case["size"].as_object().map(|size| {
            (
                size["width"].as_u64().expect("width"),
                size["height"].as_u64().expect("height"),
            )
        });
        assert_eq!(shown, size, "{name}: size");
    }
}

/// A proxy a browser made keeps its original's timestamps, so a native host
/// whose only evidence of the offline original is what the browser recorded
/// reads it: a variable-rate video's, and a transport stream's that starts
/// late. A proxy that starts at zero instead is no proxy of the stream.
#[test]
fn a_native_host_reads_the_proxy_a_browser_made_from_the_evidence_it_recorded() {
    use crate::assets::{HardwareVideoDecode, fingerprint};
    use alpha2_assets::{MediaPurpose, Selection};
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases/media-evidence-corpus.json");
    let corpus_cases: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("JSON");
    // What a browser recorded of `name`: the corpus's rate and end, at the size it is shown.
    let recorded = |name: &str| {
        let case = corpus_cases["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|case| case["file"] == name)
            .expect("listed");
        let shown = fingerprint(&corpus(name), false, false, &HardwareVideoDecode::None)
            .expect("probe")
            .streams
            .and_then(|streams| streams.video)
            .expect("video");
        json!({
            "streams": {"video": {
                "width": shown.width, "height": shown.height, "frame_rate": case["frame_rate"]
            }},
            "duration_ticks": case["duration_ticks"]
        })
    };
    let proxied = |original: &str, proxy: &str| {
        let resolved = resolve(
            &conform_project(&recorded(original)),
            &[],
            &[("video", corpus(proxy))],
            MediaPurpose::ExportProxy,
        );
        let source = &resolved.report.sources[0];
        (
            source.selection.clone(),
            serde_json::to_value(&source.issues).expect("issues"),
        )
    };
    let proxy = Selection::Proxy {
        id: "video#proxy0".into(),
    };
    for name in ["vfr.mp4", "h264-bt709-copy.ts"] {
        let (selection, issues) = proxied(name, name);
        assert_eq!(selection, proxy, "{name}: {issues}");
    }
    // The same pictures from zero: two seconds where the stream ends at 3.47 s.
    let (selection, issues) = proxied("h264-bt709-copy.ts", "h264-bt709-copy.mp4");
    assert_eq!(selection, Selection::Unavailable, "{issues}");
    let problem = issues
        .as_array()
        .and_then(|issues| issues.iter().find(|issue| issue["kind"] == "proxy"))
        .map(|issue| issue["problem"].clone());
    assert_eq!(
        problem,
        Some(json!({"kind": "invalid", "reasons": ["duration"]})),
        "{issues}"
    );
}

/// A proxy's `madeFrom` that is not a well-formed fingerprint is refused as
/// input, naming where it is, rather than read as evidence of other media.
#[test]
fn a_malformed_made_from_is_refused_as_input() {
    let clip = corpus("parity-rgb.mov");
    let conform = json!({
        "protocolVersion": 1, "requestId": "conform", "operation": "conform",
        "project": conform_project(&json!({})),
        "assets": {"video": clip},
        "proxies": {"video": [{"path": clip, "madeFrom": {"content_sha256": "A".repeat(64)}}]}
    });
    let input = format!("{conform}\n");
    let mut serving = Serving::new(VideoDecodePolicy::Software);
    serving.create = no_gpu;
    let mut output = Vec::new();
    let _ = serve_requests(
        &mut serving,
        &mut std::io::BufReader::new(input.as_bytes()),
        &mut output,
        &mut |_| {},
    );
    let answers = answers_of(output);
    assert_eq!(answers[0]["ok"], false, "{}", answers[0]);
    assert_eq!(
        answers[0]["error"]["code"], "INVALID_INPUT",
        "{}",
        answers[0]
    );
    assert_eq!(
        answers[0]["error"]["message"],
        "proxy 0 of source video: madeFrom has a malformed content_sha256",
        "{}",
        answers[0]
    );
}
