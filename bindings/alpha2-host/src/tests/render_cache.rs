//! The render cache: baked frames, quota, eviction, and its writer and reader threads.

use super::*;

/// A frame that repeats the last is found whole in the result cache: a still
/// exported for a second is drawn once. Each export starts with an empty
/// cache, since its source keys mean nothing to another export.
#[test]
fn a_repeated_frame_is_drawn_once_per_export() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("result-cache");
    let end_ticks = alpha2_time::TICKS_PER_SECOND;
    let mut project = image_and_tone_project(end_ticks, 48_000);
    project["sources"] = json!([{"id": "image", "locator": "fixture://image", "kind": "image"}]);
    project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .truncate(1);
    let assets = HashMap::from([(
        "image".to_owned(),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/cross_topology_parity.png"),
    )]);
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let mut export = |name: &str| {
        host.export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end_ticks,
            RateRequest { num: 30, den: 1 },
            false,
            NativeVideoCodec::Mpeg4Part2,
            None,
            VideoQualityRequest::Level(QualityLevel::VeryHigh),
            NativeAudioCodec::Aac,
            AudioQualityRequest::Level(QualityLevel::VeryHigh),
            None,
            &directory.join(name),
            &mut |_| Ok(()),
        )
        .expect("export")
    };
    for name in ["first.mp4", "second.mp4"] {
        let cache = export(name)["resultCache"].clone();
        assert_eq!(cache["misses"], json!(1), "{cache}");
        assert_eq!(cache["hits"], json!(29), "{cache}");
        assert!(cache["budgetBytes"].as_u64().is_some_and(|bytes| bytes > 0));
    }
    assert_eq!(
        std::fs::read(directory.join("first.mp4")).expect("first"),
        std::fs::read(directory.join("second.mp4")).expect("second"),
    );
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_still_under_a_constant_effect_or_a_held_caption_is_drawn_once_per_export() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("result-cache-effects");
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts/Alpha2Fixture.ttf");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let look = |effect: &str, name: &str, value: f64| {
        json!([{"id": "look", "output": "effect", "nodes": [
            {"id": "clip", "op": {"type": "clip"}},
            {"id": "effect", "op": {"type": "effect", "input": "clip", "effect": {
                "id": effect, "version": 1,
                "params": {name: {"mode": "constant", "value": {"type": "float", "value": value}}}}}}]}])
    };
    let mut export = |name: &str, project: &Value| {
        let request = parse_request(
            &json!({
                "protocolVersion": 1,
                "operation": "export",
                "requestId": name,
                "project": project,
                "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/corpus/cross_topology_parity.png")},
                "fonts": [font],
                "startTicks": 0,
                "endTicks": alpha2_time::TICKS_PER_SECOND,
                "frameRate": {"num": 30, "den": 1},
                "includeAudio": false,
                "videoCodec": "mpeg4-part2",
                "output": directory.join(format!("{name}.mp4")),
            })
            .to_string(),
        )
        .expect("request");
        let result = host.handle(request, &mut |_| Ok(())).expect("export");
        let cache = &result["resultCache"];
        (
            [&cache["hits"], &cache["misses"], &cache["ineligible"]].map(Value::as_u64),
            result["performance"]["graphPasses"].as_u64(),
        )
    };
    let still = still_project([0.0, 0.0, 0.0], "auto");
    // Exposure has no time in it: the clip's picture and the frame are drawn once.
    let mut effect = still.clone();
    effect["compositions"] = look("alpha2.effect.color-controls", "exposure", 0.4);
    effect["timeline"]["tracks"][0]["clips"][0]["composition"] = json!({"id": "look"});
    assert_eq!(
        export("effect", &effect),
        ([Some(29), Some(2), Some(0)], Some(2))
    );
    // A blur is drawn as several passes; none of them reads the time either.
    let mut blur = effect.clone();
    blur["compositions"] = look("alpha2.effect.gaussian-blur", "radius", 0.01);
    let (cache, passes) = export("blur", &blur);
    assert_eq!(cache, [Some(29), Some(2), Some(0)]);
    assert!(passes.is_some_and(|passes| passes <= 5), "{passes:?}");
    // Film grain draws new grain in every frame.
    let mut grain = effect.clone();
    grain["compositions"] = look("alpha2.effect.film-grain", "amount", 0.2);
    assert_eq!(
        export("grain", &grain),
        ([Some(0), Some(60), Some(0)], Some(60))
    );
    let mut title = still;
    title["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
        .push(json!({
        "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {"cues": [
            {"timeline_range": {"start": 0, "end": alpha2_time::TICKS_PER_SECOND},
             "text": "Held title"}]}}));
    let (cache, _) = export("title", &title);
    assert_eq!(cache, [Some(29), Some(1), Some(0)]);
    std::fs::remove_dir_all(&directory).ok();
}

/// A source at a lower rate than the export shows each of its pictures over
/// several output frames, and an output frame showing the picture the last one
/// drew is found whole in the result cache, as a still is: a 12 fps clip
/// exported at 60 fps is drawn 12 times, not 60. The cache keys a video frame
/// by the picture it shows, not by the time it was asked for, which differs in
/// every output frame and would never hit although the upload is shared.
#[test]
fn a_picture_shown_over_several_output_frames_is_drawn_once() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("result-cache-pictures");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let request = json!({
        "protocolVersion": 1, "requestId": "pictures", "operation": "export",
        "project": conform_project(&json!({})),
        "assets": {"video": corpus("parity-rgb.mov")},
        "startTicks": 0, "endTicks": alpha2_time::TICKS_PER_SECOND,
        "frameRate": {"num": 60, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "output": directory.join("pictures.mp4"),
    });
    let result = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |_| Ok(()),
        )
        .expect("export");
    let cache = &result["resultCache"];
    let performance = &result["performance"];
    assert_eq!(
        [&cache["hits"], &cache["misses"], &cache["ineligible"]].map(Value::as_u64),
        [Some(48), Some(12), Some(0)],
        "{cache}"
    );
    assert_eq!(performance["graphPasses"], json!(12), "{performance}");
    // Uploads are shared by picture whatever the cache keys: each picture is uploaded once.
    assert_eq!(performance["sourceUploads"], json!(12), "{performance}");
    assert_eq!(
        performance["physicalSourceReuses"],
        json!(48),
        "{performance}"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// The synchronous comparison path keys a video frame by the picture it
/// shows too: two times that present one picture share a key, and the next
/// picture has another.
#[test]
fn the_synchronous_frame_supply_keys_a_frame_by_its_picture() {
    use alpha2_engine::FrameProvider as _;
    let _gpu = gpu_test_lock();
    let project = Project::from_json(&conform_project(&json!({})).to_string()).expect("project");
    let source = SourceId::new("video").expect("source ID");
    let assets = HashMap::from([(source.as_str().to_owned(), corpus("parity-rgb.mov"))]);
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_high_performance())
        .expect("a GPU device");
    let renderer = alpha2_render::Renderer::new(gpu);
    let size = RenderSize::new(96, 64).expect("size");
    let mut frames =
        crate::frames::NativeFrames::new(size, &project, &assets, std::sync::Arc::default())
            .expect("frames");
    let mut key_at = |ticks: i64| {
        let need = FrameNeed {
            reader: None,
            clip_id: ClipId::new("clip").expect("clip ID"),
            source_id: source.clone(),
            media_time: MediaTime::from_ticks(ticks),
        };
        frames
            .supply(&renderer, std::slice::from_ref(&need))
            .expect("supplies");
        frames
            .source_key(&source, need.media_time)
            .expect("a keyed frame")
    };
    // 12 fps: a picture every 10,000 ticks, shown at five 60 fps output times.
    let first = key_at(0);
    assert_eq!(key_at(2_000), first, "the first picture again");
    assert_eq!(key_at(8_000), first, "still the first picture");
    let second = key_at(10_000);
    assert_ne!(second, first, "the second picture");
    assert_eq!(key_at(12_000), second);
}

/// Exports `project` (a still over `frames` frames at 30 fps) through the
/// protocol with a render cache in `cache`, returning the result.
fn export_with_render_cache(
    host: &mut Host,
    project: &Value,
    cache: &Path,
    output: &Path,
    frames: i64,
) -> Value {
    let request = parse_request(
        &json!({
            "protocolVersion": 1,
            "operation": "export",
            "requestId": "baked",
            "project": project,
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png")},
            "startTicks": 0,
            "endTicks": frames * 4_000,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "mpeg4-part2",
            "renderCache": {"directory": cache, "quotaBytes": 1 << 30, "backlog": "wait"},
            "output": output,
        })
        .to_string(),
    )
    .expect("request");
    host.handle(request, &mut |_| Ok(())).expect("export")
}

/// Baked frames are what the encoder received: a second export of the same
/// snapshot takes every frame from the cache and writes the same file; a
/// corrupt frame is rendered again; another snapshot, or a bypassed clip,
/// finds nothing.
#[test]
fn baked_frames_replace_rendering_bit_for_bit() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("baked");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project = still_project([0.0, 0.0, 0.0], "auto");
    let first = export_with_render_cache(&mut host, &project, &cache, &directory.join("a.mp4"), 6);
    assert_eq!(
        first["renderCache"]["writes"],
        json!(6),
        "{}",
        first["renderCache"]
    );
    assert_eq!(first["renderCache"]["hits"], json!(0));

    let second = export_with_render_cache(&mut host, &project, &cache, &directory.join("b.mp4"), 6);
    assert_eq!(
        second["renderCache"]["hits"],
        json!(6),
        "{}",
        second["renderCache"]
    );
    assert_eq!(second["renderCache"]["writes"], json!(0));
    let bytes = |name: &str| std::fs::read(directory.join(name)).expect("output");
    assert_eq!(bytes("a.mp4"), bytes("b.mp4"));

    let frames = walk(&cache);
    assert_eq!(frames.len(), 6);
    let mut damaged = std::fs::read(&frames[0]).expect("frame");
    let last = damaged.len() - 1;
    damaged[last] ^= 0xff;
    std::fs::write(&frames[0], damaged).expect("damage");
    let third = export_with_render_cache(&mut host, &project, &cache, &directory.join("c.mp4"), 6);
    assert_eq!(
        third["renderCache"]["corrupt"],
        json!(1),
        "{}",
        third["renderCache"]
    );
    assert_eq!(third["renderCache"]["hits"], json!(5));
    assert_eq!(third["renderCache"]["writes"], json!(1));
    assert_eq!(bytes("a.mp4"), bytes("c.mp4"));

    let other = still_project([0.5, 0.0, 0.0], "auto");
    let changed = export_with_render_cache(&mut host, &other, &cache, &directory.join("d.mp4"), 6);
    assert_eq!(changed["renderCache"]["hits"], json!(0));
    let bypassed = still_project([0.0, 0.0, 0.0], "bypass");
    let live = export_with_render_cache(&mut host, &bypassed, &cache, &directory.join("e.mp4"), 6);
    assert_eq!(
        live["renderCache"]["bypassedFrames"],
        json!(6),
        "{}",
        live["renderCache"]
    );
    assert_eq!(live["renderCache"]["hits"], json!(0));
    assert_eq!(live["renderCache"]["writes"], json!(0));
    std::fs::remove_dir_all(&directory).ok();
}

/// A project of stills shown one after another, two frames each, as
/// `(source, cache intent)`; `pictures` maps each source to its picture
/// under `tests/`.
fn slideshow_request(
    shows: &[(&str, &str)],
    pictures: &[(&str, &str)],
    render_cache: Option<&Path>,
    output: &Path,
) -> Value {
    let frame = 4_000_i64;
    let clips = shows
        .iter()
        .enumerate()
        .map(|(slot, (source, cache))| {
            let start = i64::try_from(slot).expect("slot") * 2 * frame;
            json!({
                "id": format!("clip-{slot}"), "source_id": source, "cache": cache,
                "timeline_range": {"start": start, "end": start + 2 * frame},
                "source_range": {"start": 0, "end": 2 * frame}
            })
        })
        .collect::<Vec<_>>();
    let sources = pictures
        .iter()
        .map(|(id, _)| json!({"id": id, "locator": format!("fixture://{id}"), "kind": "image"}))
        .collect::<Vec<_>>();
    let assets = pictures
        .iter()
        .map(|(id, picture)| {
            (
                (*id).to_owned(),
                json!(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../tests")
                        .join(picture)
                ),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let mut request = json!({
        "protocolVersion": 1, "operation": "export", "requestId": "slideshow",
        "project": {
            "schema_version": 1, "id": "slideshow", "name": "Slideshow",
            "settings": {"width": 64, "height": 48,
                         "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
            "sources": sources,
            "timeline": {"tracks": [
                {"id": "video", "name": "Video", "kind": "video", "clips": clips}
            ]}
        },
        "assets": assets,
        "startTicks": 0,
        "endTicks": i64::try_from(shows.len()).expect("shows") * 2 * frame,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false, "videoCodec": "mpeg4-part2",
        "output": output,
    });
    if let Some(cache) = render_cache {
        request["renderCache"] =
            json!({"directory": cache, "quotaBytes": 1 << 30, "backlog": "wait"});
    }
    request
}

/// An export holds on the GPU the stills the frame it renders
/// shows, not every still it has shown. A still that comes back is decoded
/// again and draws the same picture as a fresh source of the same file.
#[test]
fn an_export_holds_only_the_stills_its_frame_shows() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("slideshow");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let pictures = [
        ("a", "golden/blend_modes.png"),
        ("b", "golden/chroma_key.png"),
        ("c", "golden/film_grain.png"),
        ("d", "corpus/cross_topology_parity.png"),
        ("e", "golden/blend_modes.png"),
    ];
    let mut export = |shows: &[(&str, &str)], name: &str| {
        let request = slideshow_request(shows, &pictures, None, &directory.join(name));
        let request = parse_request(&request.to_string()).expect("request");
        host.handle(request, &mut |_| Ok(())).expect("export")
    };
    let auto = "auto";
    let returning = export(
        &[
            ("a", auto),
            ("b", auto),
            ("c", auto),
            ("a", auto),
            ("d", auto),
        ],
        "returning.mp4",
    );
    assert_eq!(
        returning["performance"]["peakResidentStills"],
        json!(1),
        "{}",
        returning["performance"]
    );
    export(
        &[
            ("a", auto),
            ("b", auto),
            ("c", auto),
            ("e", auto),
            ("d", auto),
        ],
        "fresh.mp4",
    );
    let bytes = |name: &str| std::fs::read(directory.join(name)).expect("output");
    assert!(
        bytes("returning.mp4") == bytes("fresh.mp4"),
        "a still that comes back draws its own picture"
    );

    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwencode"
    ))]
    // A Windows device may encode HEVC or AV1 and not this export's H.264; its report then leads
    // with another codec.
    if host
        .gpu_output_capability()
        .is_ok_and(|capability| capability.codec == NativeVideoCodec::H264)
    {
        let shows = [
            ("a", auto),
            ("b", auto),
            ("c", auto),
            ("a", auto),
            ("d", auto),
        ];
        let mut request = slideshow_request(&shows, &pictures, None, &directory.join("gpu.mp4"));
        request["project"]["settings"]["width"] = json!(640);
        request["project"]["settings"]["height"] = json!(360);
        request["videoCodec"] = json!("h264");
        let request = parse_request(&request.to_string()).expect("request");
        let gpu = host.handle(request, &mut |_| Ok(())).expect("export");
        assert_eq!(gpu["videoOutputPath"], json!("gpu-resident"), "{gpu}");
        assert_eq!(
            gpu["performance"]["peakResidentStills"],
            json!(1),
            "{}",
            gpu["performance"]
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// A packet copy renders nothing and the GPU-resident path reads no frame
/// back, so neither bakes or takes frames: an export on either that asked
/// for a render cache reports the cache `unavailable` with the reason, not
/// a run that found nothing.
#[test]
fn a_copy_and_a_gpu_resident_export_report_the_render_cache_unavailable() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("render-cache-unavailable");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let second = alpha2_time::TICKS_PER_SECOND;
    let render_cache = json!({"directory": cache, "quotaBytes": 1 << 30, "backlog": "wait"});
    let copy = parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "copy-cache", "operation": "export",
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
            "renderCache": render_cache,
            "output": directory.join("copy.mp4"),
        })
        .to_string(),
    )
    .expect("request");
    let copied = host.handle(copy, &mut |_| Ok(())).expect("copy");
    assert_eq!(copied["videoOutputPath"], json!("packet-copy"), "{copied}");
    assert_eq!(
        copied["renderCache"],
        json!({"status": "unavailable", "reason": "a copy renders nothing to bake"}),
        "{copied}"
    );

    #[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwencode"))]
    {
        let mut host = create_host(VideoDecodePolicy::Auto).expect("host");
        match host.gpu_output_capability() {
            Ok(_) => {
                let shows = [("a", "auto"), ("a", "auto")];
                let mut request = slideshow_request(
                    &shows,
                    &[("a", "golden/blend_modes.png")],
                    Some(&cache),
                    &directory.join("gpu.mp4"),
                );
                request["project"]["settings"]["width"] = json!(640);
                request["project"]["settings"]["height"] = json!(360);
                request["videoCodec"] = json!("h264");
                let request = parse_request(&request.to_string()).expect("request");
                let gpu = host.handle(request, &mut |_| Ok(())).expect("export");
                assert_eq!(gpu["videoOutputPath"], json!("gpu-resident"), "{gpu}");
                assert_eq!(
                    gpu["renderCache"],
                    json!({
                        "status": "unavailable",
                        "reason": "the GPU-resident path does not read frames back"
                    }),
                    "{gpu}"
                );
            }
            Err(reason) => eprintln!("note: only the copy is checked: {reason}"),
        }
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// A frame taken from the render cache is not rendered, so none of
/// its sources is uploaded: an upload's staging lives until a submission,
/// and a run of cached frames submits nothing. A still whose picture
/// arrived with a cached frame is uploaded by the first rendered frame that
/// shows it.
#[test]
fn a_frame_the_render_cache_holds_uploads_no_source() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cached-uploads");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project: Value = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let mut export_video = |name: &str| {
        let request = parse_request(
            &json!({
                "protocolVersion": 1, "operation": "export", "requestId": "cached",
                "project": project,
                "assets": {"video-source": corpus("h264-long-gop.mp4")},
                "startTicks": 0, "endTicks": 120_000,
                "frameRate": {"num": 30, "den": 1},
                "includeAudio": false, "videoCodec": "mpeg4-part2",
                "renderCache": {"directory": cache, "quotaBytes": 1 << 30, "backlog": "wait"},
                "output": directory.join(name),
            })
            .to_string(),
        )
        .expect("request");
        host.handle(request, &mut |_| Ok(())).expect("export")
    };
    let first = export_video("first.mp4");
    assert_eq!(first["renderCache"]["writes"], json!(30));
    assert!(first["performance"]["sourceUploadBytes"].as_u64() > Some(0));
    let second = export_video("second.mp4");
    assert_eq!(
        second["renderCache"]["hits"],
        json!(30),
        "{}",
        second["renderCache"]
    );
    assert_eq!(
        second["performance"]["sourceUploadBytes"],
        json!(0),
        "{}",
        second["performance"]
    );

    let pictures = [("a", "golden/blend_modes.png")];
    let shows = [("a", "auto"), ("a", "auto"), ("a", "bypass")];
    let mut export_still = |name: &str| {
        let request = slideshow_request(&shows, &pictures, Some(&cache), &directory.join(name));
        let request = parse_request(&request.to_string()).expect("request");
        host.handle(request, &mut |_| Ok(())).expect("export")
    };
    let rendered = export_still("still-first.mp4");
    assert_eq!(rendered["renderCache"]["writes"], json!(4));
    let taken = export_still("still-second.mp4");
    assert_eq!(
        taken["renderCache"]["hits"],
        json!(4),
        "{}",
        taken["renderCache"]
    );
    let bytes = |name: &str| std::fs::read(directory.join(name)).expect("output");
    assert!(
        bytes("still-first.mp4") == bytes("still-second.mp4"),
        "a still that waited through cached frames draws the same picture"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Over its quota, the cache drops the least recently used frames, unpinned
/// before pinned, and never one the export just used.
#[test]
fn the_render_cache_evicts_old_unpinned_frames_first() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("evict");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let pinned = still_project([0.0, 0.0, 0.0], "pin");
    export_with_render_cache(&mut host, &pinned, &cache, &directory.join("a.mp4"), 3);
    let unpinned = still_project([0.2, 0.0, 0.0], "auto");
    export_with_render_cache(&mut host, &unpinned, &cache, &directory.join("b.mp4"), 3);
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3_600);
    for frame in walk(&cache) {
        std::fs::File::options()
            .write(true)
            .open(&frame)
            .and_then(|file| file.set_modified(hour_ago))
            .expect("age");
    }
    let frame_bytes = std::fs::metadata(&walk(&cache)[0]).expect("frame").len();
    let request = parse_request(
        &json!({
            "protocolVersion": 1, "operation": "export", "requestId": "quota",
            "project": still_project([0.4, 0.0, 0.0], "auto"),
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png")},
            "startTicks": 0, "endTicks": 12_000, "frameRate": {"num": 30, "den": 1},
            "includeAudio": false, "videoCodec": "mpeg4-part2",
            "renderCache": {
                "directory": cache, "quotaBytes": frame_bytes * 6 + frame_bytes / 2,
                "backlog": "wait",
            },
            "output": directory.join("c.mp4"),
        })
        .to_string(),
    )
    .expect("request");
    let result = host.handle(request, &mut |_| Ok(())).expect("export");
    assert_eq!(
        result["renderCache"]["evictions"],
        json!(3),
        "{}",
        result["renderCache"]
    );
    let left = walk(&cache);
    assert_eq!(left.len(), 6);
    assert_eq!(
        left.iter()
            .filter(|path| path.to_string_lossy().ends_with(".pinned.frame"))
            .count(),
        3,
        "the pinned frames outlived the unpinned ones"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Every baked frame file under `cache`, at any depth, in a stable order.
fn walk(cache: &Path) -> Vec<PathBuf> {
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                visit(&path, files);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "frame")
            {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    visit(cache, &mut files);
    files.sort();
    files
}

/// The quota holds while an export writes, not only when it ends: a frame
/// that does not fit, after earlier exports' frames have made room, is
/// skipped rather than written past it.
#[test]
fn the_render_cache_quota_holds_while_frames_are_written() {
    use crate::render_cache::{Backlog, BakedFrame, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-quota");
    // Noise does not compress, so each frame costs about its raw size.
    let frame = |seed: u8| {
        let mut state = u32::from(seed) + 1;
        let mut noise = |length: usize| {
            (0..length)
                .map(|_| {
                    state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    state.to_le_bytes()[2]
                })
                .collect::<Vec<_>>()
        };
        BakedFrame {
            y: noise(64 * 64),
            u: noise(32 * 32),
            v: noise(32 * 32),
        }
    };
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 20_000,
        backlog: Backlog::Wait,
    };
    let mut cache = RenderCache::open(
        &request,
        [1; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    for index in 0..6_u8 {
        let key = cache.key(MediaTime::from_ticks(i64::from(index) * 4_000), rate);
        cache.write(key, frame(index), false);
    }
    let report = cache.finish();
    assert_eq!(report["writes"], json!(3), "{report}");
    assert_eq!(report["quotaSkips"], json!(3), "{report}");
    let stored: u64 = walk(&directory)
        .iter()
        .map(|path| std::fs::metadata(path).expect("frame").len())
        .sum();
    assert!(stored <= 20_000, "{stored} bytes stored");

    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3_600);
    for path in walk(&directory) {
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(hour_ago))
            .expect("age");
    }
    let mut later = RenderCache::open(
        &request,
        [2; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    let key = later.key(MediaTime::from_ticks(0), rate);
    later.write(key, frame(9), false);
    let report = later.finish();
    assert_eq!(report["writes"], json!(1), "{report}");
    assert!(report["evictions"].as_u64().unwrap_or(0) >= 1, "{report}");
    std::fs::remove_dir_all(&directory).ok();
}

/// The format of an H.264 export's baked frames of `width` x `height`.
fn h264_frames(width: u32, height: u32) -> crate::render_cache::FrameFormat {
    crate::render_cache::FrameFormat {
        width,
        height,
        encoding: alpha2_media::YuvEncoding::export(alpha2_media::VideoCodec::H264),
    }
}

/// A frame of `width` x `height` noise, the same for the same `seed`. Noise
/// does not compress, so a frame costs about its raw size on disk.
fn noise_frame(seed: u32, width: usize, height: usize) -> crate::render_cache::BakedFrame {
    let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let mut noise = |length: usize| {
        (0..length)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                state.to_le_bytes()[2]
            })
            .collect::<Vec<_>>()
    };
    crate::render_cache::BakedFrame {
        y: noise(width * height),
        u: noise(width.div_ceil(2) * height.div_ceil(2)),
        v: noise(width.div_ceil(2) * height.div_ceil(2)),
    }
}

/// Sets `path`'s modification time to `seconds` before now.
fn age(path: &Path, seconds: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| {
            file.set_modified(std::time::SystemTime::now() - Duration::from_secs(seconds))
        })
        .expect("age");
}

fn stored_bytes(cache: &Path) -> u64 {
    walk(cache)
        .iter()
        .map(|path| std::fs::metadata(path).expect("frame").len())
        .sum()
}

/// The cache counts and deletes only the frames it wrote, in the directories
/// it named. A user's files under the cache directory, even under its version
/// directories, named like a frame in a directory the cache did not name, or
/// behind a symbolic link, are neither counted against the quota nor deleted
/// to make room for it.
#[test]
fn the_render_cache_deletes_only_what_it_wrote() {
    use crate::render_cache::{Backlog, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-foreign");
    let cache = directory.join("cache");
    let outside = directory.join("outside");
    let snapshot = "a".repeat(64);
    let frame_name = format!("ab{}.frame", "c".repeat(62));
    let foreign = [
        cache.join("v1/footage/clip.mov"),
        cache.join("v2/footage/clip.mov"),
        cache.join("v2/ab/clip.mov"),
        cache.join(format!("v2/{snapshot}/ab/notes.txt")),
        cache.join(format!("v2/{snapshot}/footage/{frame_name}")),
        cache.join(format!("v2/{snapshot}/ab/{frame_name}.bak")),
        outside.join(&frame_name),
        outside.join("clip.mov"),
    ];
    for path in &foreign {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("folder");
        std::fs::write(path, vec![7_u8; 50_000]).expect("foreign file");
        age(path, 3_600);
    }
    #[cfg(unix)]
    {
        for link in [
            cache.join("v1/ab"),
            cache.join(format!("v2/{snapshot}/cd")),
            cache.join(format!("v2/{}", "b".repeat(64))),
        ] {
            std::os::unix::fs::symlink(&outside, &link).expect("link");
        }
    }
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let request = RenderCacheRequest {
        directory: cache.clone(),
        quota_bytes: 20_000,
        backlog: Backlog::Wait,
    };
    let mut writer = RenderCache::open(
        &request,
        [1; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    for index in 0..3_u32 {
        let key = writer.key(MediaTime::from_ticks(i64::from(index) * 4_000), rate);
        writer.write(key, noise_frame(index, 64, 64), false);
        writer.settle();
    }
    let report = writer.finish();
    let missing = foreign
        .iter()
        .filter(|path| !path.is_file())
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "the cache deleted {missing:?}: {report}"
    );
    assert_eq!(report["writes"], json!(3), "{report}");
    assert_eq!(report["evictions"], json!(0), "{report}");
    std::fs::remove_dir_all(&directory).ok();
}

/// A frame is keyed by the format it was packed in as well as by the
/// snapshot: an H.264 export sites chroma left and an AV1 or VP9 one at the
/// center, so a frame baked for one is not the other's live render. Every
/// field of the encoding and the size changes the key; codecs that pack
/// alike (AV1 and VP9) share it.
#[test]
fn a_render_cache_key_covers_the_size_and_encoding_of_its_frames() {
    use crate::render_cache::{Backlog, FrameFormat, RenderCache, RenderCacheRequest};
    use alpha2_media::{
        ChromaLocation, ColorMatrix, ColorPrimaries, ColorRange, TransferFunction, VideoCodec,
        YuvEncoding,
    };
    let directory = scratch_directory("cache-format-keys");
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 1 << 20,
        backlog: Backlog::Wait,
    };
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let h264 = h264_frames(64, 64);
    let with = |change: &dyn Fn(&mut YuvEncoding)| {
        let mut format = h264;
        change(&mut format.encoding);
        format
    };
    let key = |format: FrameFormat| {
        let cache =
            RenderCache::open(&request, [9; 32], format, &CancelToken::default()).expect("cache");
        let key = cache.key(MediaTime::from_ticks(0), rate);
        assert_eq!(cache.finish()["status"], json!("used"));
        key
    };
    let av1 = FrameFormat {
        encoding: YuvEncoding::export(VideoCodec::Av1),
        ..h264
    };
    let vp9 = FrameFormat {
        encoding: YuvEncoding::export(VideoCodec::Vp9),
        ..h264
    };
    let distinct = [
        ("h264", h264),
        ("av1", av1),
        ("wider", h264_frames(66, 64)),
        ("taller", h264_frames(64, 66)),
        (
            "full range",
            with(&|encoding| encoding.color.range = ColorRange::Full),
        ),
        (
            "bt601 matrix",
            with(&|encoding| encoding.color.matrix = ColorMatrix::Bt601),
        ),
        (
            "bt2020 primaries",
            with(&|encoding| encoding.color.primaries = ColorPrimaries::Bt2020),
        ),
        (
            "srgb transfer",
            with(&|encoding| encoding.color.transfer = TransferFunction::Srgb),
        ),
        (
            "top-left chroma",
            with(&|encoding| encoding.chroma = ChromaLocation::TopLeft),
        ),
    ]
    .map(|(name, format)| (name, key(format)));
    for (index, (name, key)) in distinct.iter().enumerate() {
        for (other, other_key) in &distinct[index + 1..] {
            assert_ne!(key, other_key, "{name} and {other} share a key");
        }
    }
    assert_eq!(key(vp9), key(av1), "AV1 and VP9 pack alike");
    assert_eq!(key(h264), distinct[0].1, "a key is stable");
}

/// An AV1 export into a cache an H.264 export of the same snapshot filled
/// takes none of its frames, and a frame file whose header names another
/// encoding than the export reading it is not taken even under that
/// export's key: it is counted corrupt, deleted, and rendered again.
#[test]
fn a_render_cache_frame_baked_for_one_encoding_is_not_taken_for_another() {
    use crate::render_cache::{Backlog, FrameFormat, RenderCache, RenderCacheRequest};
    use alpha2_media::{VideoCodec, YuvEncoding};
    let directory = scratch_directory("cache-format-reuse");
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 1 << 20,
        backlog: Backlog::Wait,
    };
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let time = MediaTime::from_ticks(0);
    let h264 = h264_frames(64, 64);
    let av1 = FrameFormat {
        encoding: YuvEncoding::export(VideoCodec::Av1),
        ..h264
    };
    let mut baking =
        RenderCache::open(&request, [9; 32], h264, &CancelToken::default()).expect("cache");
    let h264_key = baking.key(time, rate);
    baking.write(h264_key.clone(), noise_frame(1, 64, 64), false);
    assert_eq!(baking.finish()["writes"], json!(1));
    let h264_frame = walk(&directory).pop().expect("the H.264 frame");

    let mut other =
        RenderCache::open(&request, [9; 32], av1, &CancelToken::default()).expect("cache");
    let av1_key = other.key(time, rate);
    assert_ne!(av1_key, h264_key);
    assert!(other.read(&av1_key).is_none(), "AV1 took an H.264 frame");
    assert!(other.read(&h264_key).is_none(), "AV1 took an H.264 frame");
    // The H.264 frame's file, under the AV1 key: only its header tells them apart.
    let av1_snapshot = std::fs::read_dir(directory.join("v2"))
        .expect("snapshots")
        .map(|entry| entry.expect("snapshot").path())
        .find(|snapshot| !h264_frame.starts_with(snapshot))
        .expect("the AV1 snapshot");
    let forged = av1_snapshot
        .join(&av1_key[..2])
        .join(format!("{av1_key}.frame"));
    std::fs::create_dir_all(forged.parent().expect("shard")).expect("shard");
    std::fs::copy(&h264_frame, &forged).expect("copy");
    assert!(other.read(&av1_key).is_none(), "AV1 took an H.264 frame");
    assert!(!forged.exists(), "the mismatched frame was kept");
    let report = other.finish();
    assert_eq!(report["hits"], json!(0), "{report}");
    assert_eq!(report["corrupt"], json!(1), "{report}");

    let mut again =
        RenderCache::open(&request, [9; 32], h264, &CancelToken::default()).expect("cache");
    let frame = again.read(&h264_key).expect("the H.264 frame");
    assert_eq!(frame.y, noise_frame(1, 64, 64).y);
    assert_eq!(again.finish()["hits"], json!(1));
}

/// A retry of the same snapshot into a cache its first attempt filled takes
/// every frame that attempt baked. The frames the first attempt left unbaked
/// (busy skips) are rendered again and offered to the writer, which finds no
/// room: deleting a frame the retry is about to read would only make it
/// render that one too.
#[test]
fn a_retry_into_a_full_cache_takes_every_frame_the_first_attempt_baked() {
    use crate::render_cache::{Backlog, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-retry");
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let time = |index: u32| MediaTime::from_ticks(i64::from(index) * 4_000);
    let request = |quota_bytes| RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes,
        backlog: Backlog::Wait,
    };
    let mut first = RenderCache::open(
        &request(1 << 30),
        [2; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    for index in (0..10_u32).step_by(2) {
        let key = first.key(time(index), rate);
        first.write(key, noise_frame(index, 64, 64), false);
        first.settle();
    }
    let first = first.finish();
    assert_eq!(first["writes"], json!(5), "{first}");
    let baked = stored_bytes(&directory);
    for path in walk(&directory) {
        age(&path, 600);
    }
    let mut retry = RenderCache::open(
        &request(baked + baked / 10),
        [2; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    let mut hits = 0;
    for index in 0..10_u32 {
        let key = retry.key(time(index), rate);
        if retry.read(&key).is_some() {
            hits += 1;
        } else {
            retry.write(key, noise_frame(index, 64, 64), false);
            retry.settle();
        }
    }
    let report = retry.finish();
    assert_eq!(hits, 5, "{report}");
    assert_eq!(report["evictions"], json!(0), "{report}");
    std::fs::remove_dir_all(&directory).ok();
}

/// A frame an export wrote is never deleted by that export, even where the
/// file system keeps coarse modification times (FAT stores even seconds, a
/// network share the server's clock) and the frame looks older than the
/// export.
#[test]
fn an_export_never_deletes_its_own_frames_whatever_their_modification_time() {
    use crate::render_cache::{Backlog, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-coarse");
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 9_000,
        backlog: Backlog::Wait,
    };
    let mut cache = RenderCache::open(
        &request,
        [3; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    let key = cache.key(MediaTime::from_ticks(0), rate);
    cache.write(key, noise_frame(0, 64, 64), false);
    cache.settle();
    let written = walk(&directory);
    assert_eq!(written.len(), 1);
    age(&written[0], 3);
    let key = cache.key(MediaTime::from_ticks(4_000), rate);
    cache.write(key, noise_frame(1, 64, 64), false);
    cache.settle();
    let report = cache.finish();
    assert!(
        written[0].is_file(),
        "the export deleted its own frame: {report}"
    );
    assert_eq!(report["evictions"], json!(0), "{report}");
    assert_eq!(report["quotaSkips"], json!(1), "{report}");
    std::fs::remove_dir_all(&directory).ok();
}

/// Two exports of different snapshots writing into one cache at once leave
/// it within its quota: each walks the directory again as it writes, so it
/// counts what the other wrote, and deletes the other's frames to make room
/// as it would an earlier export's.
#[test]
fn concurrent_exports_into_one_cache_leave_it_within_its_quota() {
    use crate::render_cache::{Backlog, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-concurrent");
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let quota_bytes = 60_000;
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes,
        backlog: Backlog::Wait,
    };
    let mut one = RenderCache::open(
        &request,
        [4; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    let mut two = RenderCache::open(
        &request,
        [5; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    for index in 0..20_u32 {
        let time = MediaTime::from_ticks(i64::from(index) * 4_000);
        for (cache, seed) in [(&mut one, index), (&mut two, index + 100)] {
            let key = cache.key(time, rate);
            cache.write(key, noise_frame(seed, 64, 64), false);
            cache.settle();
        }
        assert!(
            stored_bytes(&directory) <= quota_bytes + 2 * (quota_bytes / 16 + 7_000),
            "frame {index}: {} bytes stored",
            stored_bytes(&directory)
        );
    }
    let (one, two) = (one.finish(), two.finish());
    let stored = stored_bytes(&directory);
    assert!(stored <= quota_bytes, "{stored} bytes stored: {one} {two}");
    std::fs::remove_dir_all(&directory).ok();
}

/// Under `wait` the writer holds at most [`BACKLOG_FRAMES`] frames, the one
/// it is storing included, as the protocol says: the export waits with the
/// next one in hand instead of queueing a third.
///
/// [`BACKLOG_FRAMES`]: crate::render_cache::BACKLOG_FRAMES
#[test]
fn a_waiting_export_hands_the_writer_at_most_its_backlog() {
    use crate::render_cache::{BACKLOG_FRAMES, Backlog, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-wait");
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 1 << 30,
        backlog: Backlog::Wait,
    };
    // Large noise frames take the writer long enough to compress that the export catches up with
    // it.
    let frames = (0..6_u32)
        .map(|seed| noise_frame(seed, 1024, 1024))
        .collect::<Vec<_>>();
    let mut cache = RenderCache::open(
        &request,
        [6; 32],
        h264_frames(1024, 1024),
        &CancelToken::default(),
    )
    .expect("cache");
    let mut most = 0;
    for (index, frame) in (0_i64..).zip(frames) {
        let key = cache.key(MediaTime::from_ticks(index * 4_000), rate);
        cache.write(key, frame, false);
        most = most.max(cache.held());
    }
    let report = cache.finish();
    assert_eq!(report["writes"], json!(6), "{report}");
    assert!(
        most <= BACKLOG_FRAMES,
        "the writer held {most} frames: {report}"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Under `skip` the export never waits for the writer: a frame it has no
/// room for is counted instead of baked. Once the quota is full nothing more
/// is handed to the writer, so no frame is compressed only to be dropped.
#[test]
fn the_render_cache_writer_never_holds_up_a_skipping_export() {
    use crate::render_cache::{Backlog, BakedFrame, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-backlog");
    let frame = || BakedFrame {
        y: vec![7; 256 * 256],
        u: vec![8; 128 * 128],
        v: vec![9; 128 * 128],
    };
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let request = |quota_bytes, backlog| RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes,
        backlog,
    };
    let mut cache = RenderCache::open(
        &request(1 << 30, Backlog::Skip),
        [3; 32],
        h264_frames(256, 256),
        &CancelToken::default(),
    )
    .expect("cache");
    for index in 0..40_i64 {
        if cache.accepts() {
            let key = cache.key(MediaTime::from_ticks(index * 4_000), rate);
            cache.write(key, frame(), false);
        }
    }
    let report = cache.finish();
    let (writes, busy, errors) = (
        report["writes"].as_u64().expect("writes"),
        report["busySkips"].as_u64().expect("busySkips"),
        report["writeErrors"].as_u64().expect("writeErrors"),
    );
    assert!(writes >= 1, "{report}");
    if errors == 0 {
        assert_eq!(writes + busy, 40, "{report}");
    } else {
        // A failed write stops the cache, so the frames no longer add up (seen as a rename refused
        // while a Windows scanner held the file); the export is still not held up.
        eprintln!("the render cache failed {errors} write(s) during the test: {report}");
        assert!(writes + busy + errors <= 40, "{report}");
    }
    assert_eq!(report["backlog"], json!("skip"));

    let mut full = RenderCache::open(
        &request(10, Backlog::Wait),
        [4; 32],
        h264_frames(256, 256),
        &CancelToken::default(),
    )
    .expect("cache");
    let key = full.key(MediaTime::from_ticks(0), rate);
    full.write(key, frame(), false);
    full.settle();
    assert!(!full.accepts());
    assert!(!full.accepts());
    let report = full.finish();
    assert_eq!(report["writes"], json!(0), "{report}");
    assert_eq!(report["quotaSkips"], json!(3), "{report}");
    std::fs::remove_dir_all(&directory).ok();
}

/// A cache directory that cannot be made (here under a file, as a missing
/// drive, a read-only share, or a full disk would refuse it) leaves the
/// readback export uncached, never failed: the result reports the cache
/// unavailable and why, as the GPU-resident and copy paths do.
#[test]
fn a_render_cache_that_cannot_be_opened_never_fails_the_export() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cache-unopenable");
    let blocker = directory.join("not-a-directory");
    std::fs::write(&blocker, b"a file where the cache would go").expect("blocker");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project = still_project([0.0, 0.0, 0.0], "auto");
    let result = export_with_render_cache(
        &mut host,
        &project,
        &blocker.join("cache"),
        &directory.join("a.mp4"),
        3,
    );
    let report = &result["renderCache"];
    assert_eq!(report["status"], json!("unavailable"), "{report}");
    assert!(
        report["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cannot be created")),
        "{report}"
    );
    assert_eq!(result["videoOutputPath"], json!("readback"), "{result}");
    assert!(directory.join("a.mp4").is_file());
    std::fs::remove_dir_all(&directory).ok();
}

/// The render cache keys a build by the digest of its executable. A build whose
/// executable cannot be read has no digest, and hashing nothing would give
/// every such build one key and each other's frames; without the digest the
/// export runs without the cache and says why.
#[test]
fn an_unreadable_executable_leaves_the_render_cache_unused() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("unreadable-binary");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project = still_project([0.0, 0.0, 0.0], "auto");
    crate::host::INJECT_UNREADABLE_BINARY.with(|inject| inject.set(true));
    let result = export_with_render_cache(&mut host, &project, &cache, &directory.join("a.mp4"), 3);
    crate::host::INJECT_UNREADABLE_BINARY.with(|inject| inject.set(false));
    let report = &result["renderCache"];
    assert_eq!(report["status"], json!("unavailable"), "{report}");
    assert!(
        report["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("executable")),
        "{report}"
    );
    assert!(
        walk(&cache).is_empty(),
        "frames were baked under no build's identity"
    );
    assert!(directory.join("a.mp4").is_file());
    std::fs::remove_dir_all(&directory).ok();
}

/// Exports `project` like [`export_with_render_cache`] with `backlog`, its
/// cache's writer blocking forever at its second operation (the first
/// frame it stores) and watched with a short limit.
fn export_with_a_stuck_cache_writer(
    host: &mut Host,
    project: &Value,
    cache: &Path,
    output: &Path,
    backlog: &str,
) -> Value {
    crate::render_cache::INJECT_WRITER_STALL.with(|stall| {
        stall.set(Some((
            crate::heartbeat::InjectedStall::AfterOperations(1),
            Duration::from_millis(300),
        )));
    });
    let request = parse_request(
        &json!({
            "protocolVersion": 1,
            "operation": "export",
            "requestId": "stuck-cache",
            "project": project,
            "assets": {"image": Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/corpus/cross_topology_parity.png")},
            "startTicks": 0,
            "endTicks": 12 * 4_000,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "mpeg4-part2",
            "renderCache": {"directory": cache, "quotaBytes": 1 << 30, "backlog": backlog},
            "output": output,
        })
        .to_string(),
    )
    .expect("request");
    within(
        Duration::from_secs(60),
        "the export with a stuck cache",
        || {
            host.handle(request, &mut |_| Ok(()))
                .expect("a stuck cache never fails the export")
        },
    )
}

#[test]
fn a_render_cache_writer_stuck_in_its_filesystem_is_left_and_the_export_succeeds() {
    // A cache writer stuck in a filesystem call (a dead hard-mounted share) does not hold the
    // export after its output is published.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("stuck-cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project = still_project([0.0, 0.0, 0.0], "auto");
    for backlog in ["skip", "wait"] {
        let cache = directory.join(format!("cache-{backlog}"));
        let output = directory.join(format!("{backlog}.mp4"));
        let result =
            export_with_a_stuck_cache_writer(&mut host, &project, &cache, &output, backlog);
        let report = &result["renderCache"];
        assert_eq!(report["status"], json!("used"), "{backlog}: {report}");
        assert_eq!(report["abandoned"], json!(true), "{backlog}: {report}");
        assert_eq!(report["writes"], json!(0), "{backlog}: {report}");
        assert_eq!(result["frames"], json!(12), "{backlog}: {result}");
        assert!(output.is_file(), "{backlog}: the output was published");

        // The directory is off for the rest of the process, so another export does not touch it.
        let again = export_with_render_cache(
            &mut host,
            &project,
            &cache,
            &directory.join(format!("{backlog}-again.mp4")),
            12,
        );
        let report = &again["renderCache"];
        assert_eq!(
            report["status"],
            json!("unavailable"),
            "{backlog}: {report}"
        );
        assert!(
            report["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("off for the rest of this process")),
            "{backlog}: {report}"
        );
    }
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_cancelled_export_stops_waiting_for_a_stuck_render_cache_writer() {
    // A `wait` backlog waits for room only while the export runs, and the writer it leaves is
    // joined only up to the stall limit.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cancelled-cache");
    let request = crate::render_cache::RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 1 << 30,
        backlog: crate::render_cache::Backlog::Wait,
    };
    crate::render_cache::INJECT_WRITER_STALL.with(|stall| {
        stall.set(Some((
            crate::heartbeat::InjectedStall::AfterOperations(1),
            Duration::from_secs(2),
        )));
    });
    let cancel = CancelToken::default();
    let mut cache =
        crate::render_cache::RenderCache::open(&request, [8; 32], h264_frames(64, 64), &cancel)
            .expect("cache");
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let started = Instant::now();
    within(Duration::from_secs(60), "a cancelled cache", || {
        for index in 0..2_u32 {
            let key = cache.key(MediaTime::from_ticks(i64::from(index) * 4_000), rate);
            cache.write(key, noise_frame(index, 64, 64), false);
        }
        cancel.cancel();
        let key = cache.key(MediaTime::from_ticks(8_000), rate);
        cache.write(key, noise_frame(2, 64, 64), false);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the cancelled write waited {:?}",
            started.elapsed()
        );
        drop(cache);
    });
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "dropping took {:?}",
        started.elapsed()
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// The frame files the render cache's readers read under `directory`, with
/// the names of the threads that read them.
fn frames_read_under(directory: &Path) -> Vec<(PathBuf, Option<String>)> {
    crate::render_cache::FRAMES_READ
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(path, _)| path.starts_with(directory))
        .cloned()
        .collect()
}

#[test]
fn a_baked_frame_is_read_and_checked_off_the_exports_own_thread() {
    // Cached frames are read, inflated and checksummed off the export's thread: inline that is 7
    // to 15 ms per 1080p frame.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cache-read-thread");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project = still_project([0.0, 0.0, 0.0], "auto");
    let first = export_with_render_cache(&mut host, &project, &cache, &directory.join("a.mp4"), 6);
    assert_eq!(first["renderCache"]["writes"], json!(6), "{first}");
    let second = export_with_render_cache(&mut host, &project, &cache, &directory.join("b.mp4"), 6);
    assert_eq!(second["renderCache"]["hits"], json!(6), "{second}");
    let read = frames_read_under(&cache);
    assert_eq!(read.len(), 6, "{read:?}");
    let export_thread = std::thread::current().name().map(str::to_owned);
    for (path, thread) in &read {
        assert_ne!(
            thread,
            &export_thread,
            "{} was read on the export's thread",
            path.display()
        );
        assert_eq!(
            thread.as_deref(),
            Some("alpha2-render-cache-read"),
            "{} was read on {thread:?}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read(directory.join("a.mp4")).expect("a"),
        std::fs::read(directory.join("b.mp4")).expect("b"),
        "the frames taken encode to the same file"
    );
}

/// The reader reads what it is asked, in order, ahead of the export, and
/// holds no more than `READ_AHEAD_FRAMES`. A frame asked for and not taken
/// is let go; one that fails its checks is counted corrupt whether or not it
/// was taken.
#[test]
fn the_render_cache_reads_ahead_in_order_and_within_its_bound() {
    use crate::render_cache::{Backlog, READ_AHEAD_FRAMES, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("cache-read-ahead");
    let rate = alpha2_time::FrameRate::new(30, 1).expect("rate");
    let time = |index: u32| MediaTime::from_ticks(i64::from(index) * 4_000);
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 1 << 30,
        backlog: Backlog::Wait,
    };
    let mut writer = RenderCache::open(
        &request,
        [5; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    // Every frame the readers may hold, one past them, and two more.
    let count = u32::try_from(READ_AHEAD_FRAMES).expect("small") + 3;
    let ahead = READ_AHEAD_FRAMES;
    let keys = (0..count)
        .map(|index| writer.key(time(index), rate))
        .collect::<Vec<_>>();
    for (index, key) in (0_u32..).zip(&keys) {
        writer.write(key.clone(), noise_frame(index, 64, 64), false);
        writer.settle();
    }
    assert_eq!(writer.finish()["writes"], json!(count));
    let last = keys.last().expect("frames");
    let torn = walk(&directory)
        .into_iter()
        .find(|path| path.to_string_lossy().contains(last.as_str()))
        .expect("the last frame");
    let bytes = std::fs::read(&torn).expect("frame");
    std::fs::write(&torn, &bytes[..bytes.len() / 2]).expect("tear");

    let mut cache = RenderCache::open(
        &request,
        [5; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    )
    .expect("cache");
    for key in &keys {
        cache.request(key);
    }
    assert_eq!(
        cache.asked(),
        ahead,
        "the readers hold at most their read-ahead"
    );
    let first = cache.read(&keys[0]).expect("frame 0");
    assert_eq!(first.y, noise_frame(0, 64, 64).y);
    assert_eq!(cache.asked(), ahead - 1);
    // The frames asked for before it are let go; it was not asked yet, and is asked now.
    let past = cache
        .read(&keys[ahead])
        .expect("the frame past the read-ahead");
    assert_eq!(past.y, noise_frame(count - 3, 64, 64).y);
    assert_eq!(cache.asked(), 0);
    let second = cache.read(&keys[1]).expect("frame 1");
    assert_eq!(second.y, noise_frame(1, 64, 64).y);
    // Asked for and never taken: the export ended first. The torn one is still found corrupt.
    cache.request(&keys[ahead + 1]);
    cache.request(last);
    let report = cache.finish();
    assert_eq!(report["hits"], json!(3), "{report}");
    assert_eq!(report["corrupt"], json!(1), "{report}");
    assert_eq!(report["abandoned"], json!(false), "{report}");
    assert!(!torn.exists(), "the torn frame was deleted");
}

#[test]
fn a_render_cache_reader_stuck_in_its_filesystem_is_left_and_the_export_renders() {
    // The export's own thread makes none of a lookup's filesystem calls, which could block forever
    // on a hard-mounted share.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("stuck-cache-reader");
    let cache = directory.join("cache");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let project = still_project([0.0, 0.0, 0.0], "auto");
    let baked = export_with_render_cache(&mut host, &project, &cache, &directory.join("a.mp4"), 12);
    assert_eq!(baked["renderCache"]["writes"], json!(12), "{baked}");
    // Each reader blocks forever on its first or second frame (the first after it made the
    // directory).
    crate::render_cache::INJECT_READER_STALL.with(|stall| {
        stall.set(Some((
            crate::heartbeat::InjectedStall::AfterOperations(1),
            Duration::from_millis(300),
        )));
    });
    let started = Instant::now();
    let result = within(
        Duration::from_secs(60),
        "the export with a stuck reader",
        || export_with_render_cache(&mut host, &project, &cache, &directory.join("b.mp4"), 12),
    );
    let report = &result["renderCache"];
    assert_eq!(report["status"], json!("used"), "{report}");
    assert_eq!(report["abandoned"], json!(true), "{report}");
    assert!(
        report["hits"].as_u64().is_some_and(|hits| hits < 12),
        "{report}"
    );
    assert_eq!(result["frames"], json!(12), "{result}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the export took {:?}",
        started.elapsed()
    );
    assert_eq!(
        std::fs::read(directory.join("a.mp4")).expect("a"),
        std::fs::read(directory.join("b.mp4")).expect("b"),
        "the frames rendered instead encode to the same file"
    );
    let again = export_with_render_cache(&mut host, &project, &cache, &directory.join("c.mp4"), 12);
    assert_eq!(
        again["renderCache"]["status"],
        json!("unavailable"),
        "{again}"
    );
}

#[test]
fn a_render_cache_directory_that_cannot_be_made_in_time_is_abandoned() {
    // The export's own thread does not make the snapshot's directory, for the same reason.
    use crate::render_cache::{Backlog, RenderCache, RenderCacheRequest};
    let directory = scratch_directory("stuck-cache-open");
    let request = RenderCacheRequest {
        directory: directory.to_path_buf(),
        quota_bytes: 1 << 30,
        backlog: Backlog::Skip,
    };
    crate::render_cache::INJECT_READER_STALL.with(|stall| {
        stall.set(Some((
            crate::heartbeat::InjectedStall::AfterOperations(0),
            Duration::from_millis(300),
        )));
    });
    let started = Instant::now();
    let opened = within(Duration::from_secs(60), "a stuck open", || {
        RenderCache::open(
            &request,
            [6; 32],
            h264_frames(64, 64),
            &CancelToken::default(),
        )
    });
    let Err(reason) = opened else {
        panic!("a directory that was never made was opened");
    };
    assert!(
        reason.contains("off for the rest of this process"),
        "{reason}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the open took {:?}",
        started.elapsed()
    );
    let Err(reason) = RenderCache::open(
        &request,
        [6; 32],
        h264_frames(64, 64),
        &CancelToken::default(),
    ) else {
        panic!("an abandoned directory was opened again");
    };
    assert!(reason.contains("earlier export"), "{reason}");
}
