//! Copy exports: where packets are copied instead of rendered, and how a copy reports.

use super::*;

/// Progress never goes back: the event that starts `finalizing` reports the
/// frames the encoder has completed, not all of them, and the frames it
/// completes while flushing raise it to the total. Reporting the total first
/// would send 91/91, then 90/91, then 91/91 when frames are still in the
/// encoder as the last one is submitted.
#[test]
fn progress_never_goes_back_when_the_export_finishes() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("finalizing-progress");
    // 91 frames: a report every 30 frames puts 90 just below the total. A slow encoder still holds
    // frames when the last is submitted.
    let end = 91 * alpha2_time::TICKS_PER_SECOND / 30;
    crate::encode::INJECT_SLOW_ENCODE.with(|slow| slow.set(Some(Duration::from_millis(15))));
    let project = json!({
        "schema_version": 1, "id": "progress", "name": "Progress",
        "settings": {"width": 640, "height": 360, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let mut events = Vec::new();
    host.export(
        &project.to_string(),
        &HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]),
        &HashMap::new(),
        Vec::new(),
        0,
        end,
        RateRequest { num: 30, den: 1 },
        false,
        crate::protocol::ResolvedFormat {
            container: alpha2_media::Container::Mp4,
            video: NativeVideoCodec::Mpeg4Part2,
            audio: NativeAudioCodec::Aac,
        },
        None,
        VideoQualityRequest::Level(QualityLevel::High),
        AudioQualityRequest::Level(QualityLevel::High),
        None,
        directory.join("progress.mp4"),
        &mut |event| {
            events.push((event.phase.as_str(), event.completed, event.total));
            Ok(())
        },
    )
    .expect("export");
    assert!(
        events.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&("finalizing", 91, 91)), "{events:?}");
    assert!(
        events.contains(&("finalizing", 90, 91)),
        "the encoder held no frame when the last was submitted: {events:?}"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// A packet copy that fails says it was a copy and where it failed: copying
/// packets (`encode`, at the packet it reached) or finishing the file
/// (`mux`), as a render's report does, never `prepare`.
#[test]
fn a_failed_copy_reports_its_step_and_its_output_path() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("copy-failure");
    let second = alpha2_time::TICKS_PER_SECOND;
    let request = |output: &Path| {
        parse_request(
            &json!({
                "protocolVersion": 1, "requestId": "copy-failure", "operation": "export",
                "project": {
                    "schema_version": 1, "id": "copy", "name": "Copy",
                    "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
                    "sources": [{"id": "video", "locator": "fixture://video", "kind": "video", "has_audio": true}],
                    "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                        "id": "clip", "source_id": "video",
                        "timeline_range": {"start": 0, "end": 2 * second},
                        "source_range": {"start": 0, "end": 2 * second}
                    }]}]},
                    "audio": {"limiter": null}
                },
                "assets": {"video": corpus("h264-bt709-copy.mp4")},
                "startTicks": 0, "endTicks": 2 * second,
                "frameRate": {"num": 30, "den": 1},
                "includeAudio": true,
                "videoCodec": "h264",
                "strategy": "require-copy",
                "output": output
            })
            .to_string(),
        )
        .expect("request")
    };
    let stop = || HostError::new("CANCELLED", "the client stopped the export");
    let copying = host
        .handle(request(&directory.join("a.mp4")), &mut |_| Err(stop()))
        .expect_err("stopped while copying");
    let failure = copying.failure.as_ref().expect("failure report");
    assert_eq!(failure["phase"], json!("encode"), "{failure}");
    assert_eq!(failure["frameIndex"], json!(30), "{failure}");
    assert_eq!(
        failure["diagnostics"]["videoPipeline"],
        json!("packet-copy"),
        "{failure}"
    );
    assert_eq!(failure["diagnostics"]["audioStrategy"], json!("copy"));
    let finishing = host
        .handle(request(&directory.join("b.mp4")), &mut |event| {
            if event.completed == event.total {
                Err(stop())
            } else {
                Ok(())
            }
        })
        .expect_err("stopped while finishing");
    let failure = finishing.failure.as_ref().expect("failure report");
    assert_eq!(failure["phase"], json!("mux"), "{failure}");
    assert_eq!(failure["frameIndex"], Value::Null, "{failure}");
    std::fs::remove_dir_all(&directory).ok();
}

/// `prefer-copy` copies a range that is one source untouched and whose packets
/// decode alone into its frames, and renders anything else with the reason;
/// `require-copy` refuses instead.
#[test]
#[allow(clippy::too_many_lines)]
fn a_copy_is_taken_only_where_the_packets_are_the_rendered_frames() {
    use crate::config::ExportStrategy;
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("packet-copy");
    let second = alpha2_time::TICKS_PER_SECOND;
    let project = |source_start: i64, duration: i64, limiter: bool| {
        let mut value = json!({
            "schema_version": 1, "id": "copy", "name": "Copy",
            "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video", "has_audio": true}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": duration},
                "source_range": {"start": source_start, "end": source_start + duration}
            }]}]}
        });
        if !limiter {
            value["audio"] = json!({"limiter": null});
        }
        value
    };
    let mut export = |name: &str,
                      source: &str,
                      project: Value,
                      duration: i64,
                      audio: bool,
                      strategy: ExportStrategy| {
        host.strategy = strategy;
        let result = host.export(
            &project.to_string(),
            &HashMap::from([("video".to_owned(), corpus(source))]),
            &HashMap::new(),
            Vec::new(),
            0,
            duration,
            RateRequest { num: 30, den: 1 },
            audio,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join(name),
            &mut |_| Ok(()),
        );
        host.strategy = ExportStrategy::Render;
        result
    };
    let decoded_frames = |name: &str| {
        let mut decoder =
            alpha2_native::NativeDecoder::open(directory.join(name)).expect("open copy");
        let cancel = CancelToken::default();
        let mut times = Vec::new();
        loop {
            match decoder.decode_next(&cancel).expect("decode copy") {
                alpha2_media::DecodeEvent::Video(frame) => times.push(frame.pts.ticks()),
                alpha2_media::DecodeEvent::EndOfStream => break times,
                _ => {}
            }
        }
    };

    let whole = export(
        "whole.mp4",
        "h264-bt709-copy.mp4",
        project(0, 2 * second, false),
        2 * second,
        true,
        ExportStrategy::PreferCopy,
    )
    .expect("whole copy");
    assert_eq!(whole["videoPipeline"], json!("packet-copy"), "{whole}");
    assert_eq!(whole["strategy"], json!("copy"));
    assert_eq!(whole.get("renderCache"), Some(&Value::Null), "{whole}");
    assert_eq!(whole["frames"], json!(60));
    assert_eq!(whole["audio"], json!(true));
    assert!(whole["performance"]["copiedAudioPackets"].as_u64() > Some(0));
    let times = decoded_frames("whole.mp4");
    assert_eq!(times.len(), 60);
    assert_eq!(times.first(), Some(&0));

    let gop = export(
        "gop.mp4",
        "h264-bt709-copy.mp4",
        project(second, second, false),
        second,
        false,
        ExportStrategy::RequireCopy,
    )
    .expect("GOP copy");
    assert_eq!(gop["videoPipeline"], json!("packet-copy"), "{gop}");
    assert_eq!(gop["frames"], json!(30));
    let times = decoded_frames("gop.mp4");
    assert_eq!(times.len(), 30);
    assert_eq!(times.first(), Some(&0));

    // Starting inside a GOP could show pictures that refer to ones before it.
    let refused = export(
        "mid-gop.mp4",
        "h264-bt709-copy.mp4",
        project(second / 2, second, false),
        second,
        false,
        ExportStrategy::RequireCopy,
    )
    .expect_err("mid-GOP start");
    let error = HostError::classify(refused);
    assert_eq!(error.code, "COPY_INELIGIBLE", "{}", error.message);
    assert!(error.message.contains("IDR"), "{}", error.message);
    assert!(!directory.join("mid-gop.mp4").exists());

    // A trim with audio copies the video and encodes the mix: cutting the boundary audio packets
    // would not be a copy.
    let trimmed = export(
        "trim-audio.mp4",
        "h264-bt709-copy.mp4",
        project(second, second, false),
        second,
        true,
        ExportStrategy::RequireCopy,
    )
    .expect("trimmed audio");
    assert_eq!(trimmed["videoPipeline"], json!("packet-copy"), "{trimmed}");
    assert_eq!(trimmed["audioStrategy"], json!("encode"));
    assert!(
        trimmed["audioCopyIneligible"]
            .as_str()
            .is_some_and(|reason| reason.contains("whole of its video")),
        "{trimmed}"
    );
    assert_eq!(trimmed["performance"]["encodedAudioFrames"], json!(48_000));
    assert_eq!(trimmed["performance"]["copiedAudioPackets"], json!(0));
    let (video, audio) = stream_spans(&directory.join("trim-audio.mp4"));
    assert_eq!(
        video,
        (0, second),
        "the copied video starts at zero and lasts the range"
    );
    // AAC frames are 1024 samples, and the encoder's priming is behind zero: the audio covers the
    // range to within one frame.
    assert!(audio.0 <= 0 && audio.1 >= second, "{audio:?}");
    assert!(audio.1 - second < 1024 * second / 48_000 + 1, "{audio:?}");

    // The default master limiter processes the mix, so it is encoded and the video still copied.
    let limited = export(
        "limited.mp4",
        "h264-bt709-copy.mp4",
        project(0, 2 * second, true),
        2 * second,
        true,
        ExportStrategy::RequireCopy,
    )
    .expect("limited");
    assert_eq!(limited["videoPipeline"], json!("packet-copy"), "{limited}");
    assert_eq!(limited["audioStrategy"], json!("encode"));
    assert!(
        limited["audioCopyIneligible"]
            .as_str()
            .is_some_and(|reason| reason.contains("limiter")),
        "{limited}"
    );

    // An untagged source would come out untagged where a render tags BT.709.
    let refused = export(
        "untagged.mp4",
        "h264-long-gop.mp4",
        project(0, second, false),
        second,
        false,
        ExportStrategy::RequireCopy,
    )
    .expect_err("untagged");
    assert!(refused.contains("tagged"), "{refused}");
    let _ = std::fs::remove_dir_all(directory);
}

/// A name, a project edit, and the reason the edited project's mix is encoded.
type EncodedMix<'a> = (&'a str, &'a dyn Fn(&mut Value), &'a str);

/// A copy shows and sounds as a render of the range does: what the render
/// does not draw or hear is not copied, and an export that carries audio
/// carries it whatever the strategy, silence included.
#[test]
#[allow(clippy::too_many_lines)]
fn a_copy_shows_and_sounds_as_the_render_of_its_range() {
    use crate::config::ExportStrategy;
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("packet-copy-render");
    let second = alpha2_time::TICKS_PER_SECOND;
    // The whole two-second fixture, untouched and unlimited, so its own audio is copied unless
    // something says otherwise.
    let project = |edit: &dyn Fn(&mut Value)| {
        let mut value = json!({
            "schema_version": 1, "id": "copy", "name": "Copy",
            "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video", "has_audio": true}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": 2 * second},
                "source_range": {"start": 0, "end": 2 * second}
            }]}]},
            "audio": {"limiter": null}
        });
        edit(&mut value);
        value
    };
    let add_track = |value: &mut Value, track: Value| {
        value["timeline"]["tracks"]
            .as_array_mut()
            .expect("tracks")
            .push(track);
    };
    let mut export = |name: &str, source: &str, project: Value, end: i64| {
        host.strategy = ExportStrategy::RequireCopy;
        let result = host.export(
            &project.to_string(),
            &HashMap::from([("video".to_owned(), corpus(source))]),
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            true,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join(name),
            &mut |_| Ok(()),
        );
        host.strategy = ExportStrategy::Render;
        result
    };
    let mut wrong = Vec::new();

    let copied = export(
        "own.mp4",
        "h264-bt709-copy.mp4",
        project(&|_| {}),
        2 * second,
    )
    .expect("the source's own audio");
    assert_eq!(copied["audioStrategy"], json!("copy"), "{copied}");

    // Heard as silence, or through a bus the render processes: the render's mix is encoded beside
    // the copied video.
    let encoded: [EncodedMix<'_>; 3] = [
        (
            "muted",
            &|value| value["timeline"]["tracks"][0]["clips"][0]["audio"] = json!({"muted": true}),
            "nothing in the range is heard",
        ),
        (
            "solo-elsewhere",
            &|value| {
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio", "audio": {"solo": true}}),
                );
            },
            "nothing in the range is heard",
        ),
        (
            "ducked",
            &|value| {
                add_track(value, json!({"id": "a", "name": "A", "kind": "audio"}));
                value["timeline"]["tracks"][0]["audio"] = json!({"ducking": {
                    "sidechain_track": "a", "threshold": 0.01, "depth_db": -24.0,
                    "attack_ms": 5.0, "release_ms": 200.0
                }});
            },
            "audio bus processes",
        ),
    ];
    for (name, edit, reason) in encoded {
        let file = format!("{name}.mp4");
        match export(&file, "h264-bt709-copy.mp4", project(edit), 2 * second) {
            Ok(result) => {
                let carried = alpha2_native::probe(directory.join(&file))
                    .expect("probe")
                    .selected_audio
                    .is_some();
                let because = result["audioCopyIneligible"].as_str().unwrap_or("");
                if result["audioStrategy"] != json!("encode")
                    || result["audio"] != json!(true)
                    || !carried
                    || !because.contains(reason)
                {
                    wrong.push(format!(
                        "{name}: audioStrategy {}, audio {}, stream {carried}, because {because:?}",
                        result["audioStrategy"], result["audio"]
                    ));
                }
            }
            Err(error) => wrong.push(format!("{name}: {error}")),
        }
    }

    let matte_alone = project(&|value| {
        add_track(
            value,
            json!({"id": "v2", "name": "V2", "kind": "video", "clips": [{
                "id": "matted", "source_id": "video",
                "timeline_range": {"start": second, "end": 2 * second},
                "source_range": {"start": 0, "end": second},
                "matte": {"source": "clip", "mode": "luma"}
            }]}),
        );
    });
    let refused: [(&str, &str, Value, i64, &str); 3] = [
        ("matte", "h264-bt709-copy.mp4", matte_alone, second, "matte"),
        (
            "hflip",
            "h264-bt709-hflip.mp4",
            project(&|_| {}),
            2 * second,
            "mirrors",
        ),
        (
            "vui-sar",
            "h264-bt709-vui-sar.mp4",
            project(&|_| {}),
            2 * second,
            "4:3",
        ),
    ];
    for (name, source, project, end, reason) in refused {
        match export(&format!("{name}.mp4"), source, project, end) {
            Ok(result) => wrong.push(format!(
                "{name}: copied ({} segments)",
                result["copiedSegments"]
            )),
            Err(error) => {
                let error = HostError::classify(error);
                if error.code != "COPY_INELIGIBLE" || !error.message.contains(reason) {
                    wrong.push(format!("{name}: {} {}", error.code, error.message));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    let _ = std::fs::remove_dir_all(directory);
}

/// Untouched clips that tile the range, cut at IDR pictures, are copied as
/// runs back to back: the output decodes to exactly the source frames in
/// timeline order. A cut away from an IDR picture renders instead.
#[test]
fn clips_cut_at_idr_pictures_are_copied_back_to_back() {
    use crate::config::ExportStrategy;
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("packet-join");
    let second = alpha2_time::TICKS_PER_SECOND;
    // The second second of the fixture, then its first: each clip starts at one of its IDR pictures
    // unless `second_start` says otherwise.
    let project = |second_start: i64| {
        json!({
            "schema_version": 1, "id": "join", "name": "Join",
            "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [
                {"id": "later", "source_id": "video",
                 "timeline_range": {"start": 0, "end": second},
                 "source_range": {"start": second, "end": 2 * second}},
                {"id": "earlier", "source_id": "video",
                 "timeline_range": {"start": second, "end": 2 * second},
                 "source_range": {"start": second_start, "end": second_start + second}}
            ]}]}
        })
    };
    let assets = HashMap::from([("video".to_owned(), corpus("h264-bt709-copy.mp4"))]);
    let mut export = |name: &str, project: Value, strategy: ExportStrategy| {
        host.strategy = strategy;
        let result = host.export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            2 * second,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::H264,
                audio: NativeAudioCodec::Aac,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join(name),
            &mut |_| Ok(()),
        );
        host.strategy = ExportStrategy::Render;
        result
    };
    let luma = |path: &Path| {
        let mut decoder = alpha2_native::NativeDecoder::open(path).expect("open");
        let cancel = CancelToken::default();
        let mut frames = Vec::new();
        loop {
            match decoder.decode_next(&cancel).expect("decode") {
                alpha2_media::DecodeEvent::Video(frame) => {
                    frames.push((frame.pts.ticks(), frame.planes[0].bytes.clone()));
                }
                alpha2_media::DecodeEvent::EndOfStream => break frames,
                _ => {}
            }
        }
    };

    let joined = export("joined.mp4", project(0), ExportStrategy::RequireCopy).expect("join");
    assert_eq!(joined["videoPipeline"], json!("packet-copy"), "{joined}");
    assert_eq!(joined["copiedSegments"], json!(2));
    assert_eq!(joined["frames"], json!(60));
    let source = luma(&corpus("h264-bt709-copy.mp4"));
    let copied = luma(&directory.join("joined.mp4"));
    assert_eq!(copied.len(), 60);
    let frame = second / 30;
    for (index, (time, pixels)) in copied.iter().enumerate() {
        let index = i64::try_from(index).expect("index");
        assert_eq!(
            *time,
            index * frame,
            "frame {index} is on the output's grid"
        );
        // Frames 0-29 are the source's 30-59, then its 0-29.
        let source_index = usize::try_from((index + 30) % 60).expect("index");
        assert!(
            *pixels == source[source_index].1,
            "output frame {index} is not source frame {source_index}"
        );
    }

    let refused = export(
        "mid-gop.mp4",
        project(second / 2),
        ExportStrategy::RequireCopy,
    )
    .expect_err("a cut inside a GOP");
    assert!(
        refused.contains("clip earlier") && refused.contains("IDR"),
        "{refused}"
    );
    assert!(!directory.join("mid-gop.mp4").exists());
    let _ = std::fs::remove_dir_all(directory);
}
