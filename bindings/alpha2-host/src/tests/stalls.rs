//! Stalls: decoders, encoders, providers, and copies that stop answering.

use super::*;

/// A decode pool over the long-GOP fixture whose shards block forever where
/// `stall` says, as a stuck `FFmpeg` call does.
fn stalling_decode_worker(
    stall: crate::decode::InjectedStall,
    depth: usize,
    stall_timeout: Duration,
) -> crate::decode::DecodeWorker {
    faulty_decode_worker(stall, depth, stall_timeout, 1)
}

/// [`stalling_decode_worker`] with `concurrency` shards, each faulting where
/// `stall` says.
fn faulty_decode_worker(
    stall: crate::decode::InjectedStall,
    depth: usize,
    stall_timeout: Duration,
    concurrency: usize,
) -> crate::decode::DecodeWorker {
    configured_faulty_decode_worker(stall, depth, stall_timeout, concurrency, |_| {})
}

/// [`faulty_decode_worker`] with its configuration changed by `configure`.
fn configured_faulty_decode_worker(
    stall: crate::decode::InjectedStall,
    depth: usize,
    stall_timeout: Duration,
    concurrency: usize,
    configure: impl FnOnce(&mut crate::decode::DecodeWorkerConfig),
) -> crate::decode::DecodeWorker {
    decode_worker_over(corpus("long-gop.mp4"), concurrency, |config| {
        config.depth = depth;
        config.stall_timeout = stall_timeout;
        config.stall = Some(stall);
        configure(config);
    })
}

/// Blocks at the third decoder operation, before any file is opened, so the
/// short limit cannot misfire on real work.
const STALL_IN_FIRST_JOB: crate::decode::InjectedStall =
    crate::decode::InjectedStall::AfterOperations(2);

fn stalling_job(index: u64) -> DecodeJob {
    let time = MediaTime::from_ticks(i64::try_from(index).expect("index fits") * 4_000);
    DecodeJob {
        generation: 0,
        index,
        time,
        needs: vec![FrameNeed {
            reader: None,
            clip_id: ClipId::new("clip-a").expect("clip ID"),
            source_id: SourceId::new("video-source").expect("source ID"),
            media_time: time,
        }],
        reverse_runs: HashMap::new(),
    }
}

/// Waits for the frame of a job the worker's shard is stuck in, and returns
/// the error the wait ends with.
fn wait_for_stuck_frame(worker: &crate::decode::DecodeWorker, started: Instant) -> String {
    loop {
        match worker.result_timeout(Duration::from_millis(10)) {
            Ok(Some(_)) => panic!("a stuck shard delivered a frame"),
            Ok(None) => assert!(
                started.elapsed() < Duration::from_secs(10),
                "the stall was never reported"
            ),
            Err(error) => break error,
        }
    }
}

#[test]
fn a_decoder_stuck_in_an_operation_fails_the_wait_for_its_frame() {
    // Vulkan decode waits on its execution fences with no timeout; an export waiting for the frame
    // would wait with it forever.
    let started = Instant::now();
    let worker = stalling_decode_worker(STALL_IN_FIRST_JOB, 2, Duration::from_millis(200));
    worker
        .send(stalling_job(0))
        .expect("the pool takes the job");
    let error = wait_for_stuck_frame(&worker, started);
    assert!(
        error.contains(crate::protocol::decode_stalled_tag()),
        "{error}"
    );
    // This shard opened no hardware session, so nothing says software decode would get further.
    let (code, class) = stall_report(error.clone());
    assert_eq!(code, "DECODE_STALLED", "{error}");
    assert_eq!((class.code, class.resource), ("GPU_TIMEOUT", "unknown"));
    drop(worker);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_shard_that_panics_fails_the_export_at_once_as_a_defect() {
    // A panic unwinds the shard without a result while its sibling holds the channel open: unless
    // the pool sees it, the export fails 40 s later as a stall.
    let started = Instant::now();
    let worker = faulty_decode_worker(
        crate::decode::InjectedStall::PanicAfterOperations(2),
        2,
        Duration::from_secs(20),
        2,
    );
    worker
        .send(stalling_job(0))
        .expect("the pool takes the job");
    let error = wait_for_stuck_frame(&worker, started);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the panic was reported after {:?}",
        started.elapsed()
    );
    assert!(
        error.contains("panicked decoding a frame: injected decoder panic"),
        "{error}"
    );
    assert_eq!(
        crate::failure::phase_tag(&error).map(|(phase, _)| phase),
        Some(crate::failure::ExportPhase::Decode)
    );
    let (code, class) = stall_report(error.clone());
    assert_eq!(code, "INTERNAL", "{error}");
    assert_eq!((class.code, class.resource), ("UNKNOWN", "unknown"));
    assert!(
        worker
            .heartbeats
            .iter()
            .all(|beat| beat.stalled_for().is_none())
    );
    let dropping = Instant::now();
    drop(worker);
    assert!(dropping.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_stall_in_a_shard_with_a_hardware_session_is_reported_on_the_gpu() {
    // A client retries a stall in software only where the GPU was involved, and reads that from the
    // failure's resource.
    let started = Instant::now();
    let worker = stalling_decode_worker(STALL_IN_FIRST_JOB, 2, Duration::from_millis(200));
    worker.heartbeats[0].opened_hardware();
    worker
        .send(stalling_job(0))
        .expect("the pool takes the job");
    let error = wait_for_stuck_frame(&worker, started);
    let (code, class) = stall_report(error.clone());
    assert_eq!(code, "DECODE_STALLED", "{error}");
    assert_eq!((class.code, class.resource), ("GPU_TIMEOUT", "gpu"));
}

/// A shard whose only hardware open failed holds no hardware session: it pinned
/// the source to software, and a stall there is not the GPU's. Reported on the
/// GPU, it would make a client retry the whole export in software to meet the
/// same stall.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
#[test]
fn a_stall_after_a_failed_hardware_open_is_not_reported_on_the_gpu() {
    let _gpu = gpu_test_lock();
    let Some(context) = vulkan_video_context() else {
        return;
    };
    let started = Instant::now();
    // The third operation is the software open after the hardware one refused MPEG-4 Part 2.
    let worker = configured_faulty_decode_worker(
        STALL_IN_FIRST_JOB,
        2,
        Duration::from_millis(200),
        1,
        |config| config.shared_vulkan = Some(context),
    );
    worker
        .send(stalling_job(0))
        .expect("the pool takes the job");
    let error = wait_for_stuck_frame(&worker, started);
    assert!(!worker.heartbeats[0].holds_hardware(), "{error}");
    let (code, class) = stall_report(error.clone());
    assert_eq!(code, "DECODE_STALLED", "{error}");
    assert_eq!((class.code, class.resource), ("GPU_TIMEOUT", "unknown"));
}

/// A decode that fails says which source, clip, and source time it was reading,
/// and the failure is placed at the output frame it was decoding for: the
/// decoder's own words name none of them. Its code is still the decoder's
/// words' alone.
#[test]
fn a_failed_decode_names_what_it_read_and_the_frame_it_was_for() {
    let scratch = scratch_directory("undecodable-video");
    let asset = scratch.join("not-a-video.mp4");
    std::fs::write(&asset, b"this file has no video stream at all").expect("asset");
    let worker = decode_worker_over(asset, 1, |_| {});
    let mut job = stalling_job(7);
    job.needs[0].media_time = MediaTime::from_ticks(60_000);
    job.needs[0].clip_id = ClipId::new("missing-effect").expect("clip ID");
    worker.send(job).expect("the pool takes the job");
    let started = Instant::now();
    let error = loop {
        match worker.result_timeout(Duration::from_millis(10)) {
            Ok(Some(_)) => panic!("an unreadable source decoded"),
            Ok(None) => assert!(started.elapsed() < Duration::from_secs(20)),
            Err(error) => break error,
        }
    };
    assert_eq!(
        crate::failure::phase_tag(&error),
        Some((crate::failure::ExportPhase::Decode, Some(7))),
        "{error}"
    );
    let reported = HostError::classify(error.clone());
    let attribution = "decoding source video-source (clip missing-effect) at 0.500 s: ";
    let (_, own) = reported
        .message
        .split_once(attribution)
        .unwrap_or_else(|| panic!("{error}"));
    assert_eq!(
        reported.code,
        HostError::classify(own.to_owned()).code,
        "{error}"
    );
}

#[cfg(feature = "extensions")]
/// Writes an APNG of `frames` noise frames of `side`x`side` pixels, which
/// takes a while to decode through.
fn write_noise_apng(path: &Path, side: u32, frames: u32) {
    let file = std::fs::File::create(path).expect("animation file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), side, side);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.set_animated(frames, 0).expect("animated");
    let mut writer = encoder.write_header().expect("header");
    let mut state = 0x2545_f491_u32;
    let mut pixels = vec![0_u8; (side * side * 4) as usize];
    for _ in 0..frames {
        for byte in &mut pixels {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state.to_le_bytes()[0];
        }
        writer.write_image_data(&pixels).expect("frame");
    }
    writer.finish().expect("animation");
}

/// An extension provider's open that decodes a long animation through is work
/// that keeps going, not a decoder stuck in `FFmpeg`; and a shard that waits
/// for another shard's provider call is not stuck either. Neither is reported
/// as `DECODE_STALLED` once it runs past the stall limit, which would end the
/// host process. Each decoded frame is held for a fixed step, so the opens
/// outlast the limit however fast the build decodes. The limit is many times a
/// step and a frame's decode, so a busy machine that decodes a frame slowly is
/// not taken for a stall.
#[cfg(feature = "extensions")]
#[test]
fn a_long_provider_open_that_keeps_decoding_is_not_a_stall() {
    const LIMIT: Duration = Duration::from_secs(1);
    const FRAMES: u32 = 60;
    const STEP: Duration = Duration::from_millis(50);
    let scratch = scratch_directory("long-animation");
    let mut originals = HashMap::new();
    for name in ["one", "two"] {
        let path = scratch.join(format!("{name}.png"));
        write_noise_apng(&path, 256, FRAMES);
        originals.insert(name.to_owned(), path);
    }
    let clip = |id: &str, source: &str| {
        json!({"id": id, "source_id": source,
               "timeline_range": {"start": 0, "end": 120_000},
               "source_range": {"start": 0, "end": 120_000}})
    };
    let project: Project = serde_json::from_value(json!({
        "schema_version": 1, "id": "long-animations", "name": "Long animations",
        "settings": {"width": 256, "height": 256, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "one", "locator": "asset://one", "kind": "video", "provider": {"format": "animated_image"}},
            {"id": "two", "locator": "asset://two", "kind": "video", "provider": {"format": "animated_image"}}
        ],
        "timeline": {"tracks": [
            {"id": "v1", "name": "V1", "kind": "video", "clips": [clip("a", "one")]},
            {"id": "v2", "name": "V2", "kind": "video", "clips": [clip("b", "two")]}
        ]}
    }))
    .expect("project");
    let providers = Arc::new(crate::providers::ProviderMedia::new(
        &project,
        crate::providers::ProviderFiles {
            originals,
            ..crate::providers::ProviderFiles::default()
        },
        (256, 256),
        u64::MAX,
    ));
    let config = crate::decode::DecodeWorkerConfig {
        providers,
        decoding: std::sync::Arc::default(),
        depth: 2,
        byte_budget: crate::memory::ByteBudget::exact(256 << 20),
        concurrency: 2,
        decoder_threads: 1,
        hardware_decoder_threads: 1,
        max_sessions: 2,
        reverse_cache_bytes: 0,
        reverse_hardware_surfaces: 0,
        stall_timeout: LIMIT,
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        shared_vulkan: None,
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        shared_metal: None,
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        shared_d3d12: None,
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        hardware_required: false,
        stall: Some(crate::decode::InjectedStall::SlowProgress(STEP)),
    };
    let worker = crate::decode::DecodeWorker::spawn(
        &project,
        &HashMap::new(),
        &config,
        CancelToken::default(),
        PipelineFailure::default(),
    )
    .expect("decode worker");
    let need = |clip: &str, source: &str| FrameNeed {
        reader: None,
        clip_id: ClipId::new(clip).expect("clip ID"),
        source_id: SourceId::new(source).expect("source ID"),
        media_time: MediaTime::ZERO,
    };
    let started = Instant::now();
    worker
        .send(DecodeJob {
            generation: 0,
            index: 0,
            time: MediaTime::ZERO,
            needs: vec![need("a", "one"), need("b", "two")],
            reverse_runs: HashMap::new(),
        })
        .expect("the pool takes the job");
    let decoded = loop {
        match worker.result_timeout(Duration::from_millis(10)) {
            Ok(Some(decoded)) => break decoded,
            Ok(None) => assert!(started.elapsed() < Duration::from_secs(120)),
            Err(error) => panic!("after {:?}: {error}", started.elapsed()),
        }
    };
    assert_eq!(decoded.frames.len(), 2);
    // Otherwise nothing ran past the limit, and this tested nothing.
    assert!(
        started.elapsed() > LIMIT * 2,
        "the opens took only {:?}",
        started.elapsed()
    );
}

/// A shard inside an extension provider's call past the stall limit is reported
/// as inside the provider, which is still drawing or opening its file or
/// blocked reading it: a provider calls no `FFmpeg`. A Lottie animation that
/// takes longer to draw than the limit is not a decoder "blocked, most likely
/// reading a file that stopped answering", left running as stuck inside
/// `FFmpeg`.
#[cfg(feature = "extensions")]
#[test]
fn a_shard_stuck_inside_an_extension_provider_is_reported_there() {
    let scratch = scratch_directory("provider-stall");
    let path = scratch.join("animation.png");
    write_noise_apng(&path, 16, 2);
    let project: Project = serde_json::from_value(json!({
        "schema_version": 1, "id": "provider-stall", "name": "Provider stall",
        "settings": {"width": 16, "height": 16, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "animation", "locator": "asset://animation", "kind": "video", "provider": {"format": "animated_image"}}
        ],
        "timeline": {"tracks": [
            {"id": "v1", "name": "V1", "kind": "video", "clips": [
                {"id": "a", "source_id": "animation",
                 "timeline_range": {"start": 0, "end": 120_000},
                 "source_range": {"start": 0, "end": 120_000}}
            ]}
        ]}
    }))
    .expect("project");
    let providers = Arc::new(crate::providers::ProviderMedia::new(
        &project,
        crate::providers::ProviderFiles {
            originals: HashMap::from([("animation".to_owned(), path)]),
            ..crate::providers::ProviderFiles::default()
        },
        (16, 16),
        u64::MAX,
    ));
    let config = crate::decode::DecodeWorkerConfig {
        providers,
        decoding: std::sync::Arc::default(),
        depth: 1,
        byte_budget: crate::memory::ByteBudget::exact(64 << 20),
        concurrency: 1,
        decoder_threads: 1,
        hardware_decoder_threads: 1,
        max_sessions: 1,
        reverse_cache_bytes: 0,
        reverse_hardware_surfaces: 0,
        stall_timeout: Duration::from_millis(200),
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        shared_vulkan: None,
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        shared_metal: None,
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        shared_d3d12: None,
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        hardware_required: false,
        stall: Some(crate::decode::InjectedStall::AfterOperations(1)),
    };
    let worker = crate::decode::DecodeWorker::spawn(
        &project,
        &HashMap::new(),
        &config,
        CancelToken::default(),
        PipelineFailure::default(),
    )
    .expect("decode worker");
    worker
        .send(DecodeJob {
            generation: 0,
            index: 0,
            time: MediaTime::ZERO,
            needs: vec![FrameNeed {
                reader: None,
                clip_id: ClipId::new("a").expect("clip ID"),
                source_id: SourceId::new("animation").expect("source ID"),
                media_time: MediaTime::ZERO,
            }],
            reverse_runs: HashMap::new(),
        })
        .expect("the pool takes the job");
    let started = Instant::now();
    let error = loop {
        match worker.result_timeout(Duration::from_millis(10)) {
            Ok(Some(_)) => panic!("the stalled provider returned a frame"),
            Ok(None) => assert!(
                started.elapsed() < Duration::from_secs(60),
                "no stall reported"
            ),
            Err(error) => break error,
        }
    };
    assert!(
        error.contains(crate::protocol::decode_stalled_tag()),
        "{error}"
    );
    assert!(
        error.contains("inside an extension provider, which is still drawing or opening"),
        "{error}"
    );
    assert!(!error.contains("FFmpeg"), "{error}");
    drop(worker);
}

/// The heartbeat holds hardware while a hardware open is in progress, and
/// after one succeeded, never after one that failed.
#[test]
fn a_hardware_open_holds_hardware_only_while_it_runs_or_once_it_succeeded() {
    let beat = crate::heartbeat::WorkerHeartbeat::default();
    let failing = beat.begin_hardware_open();
    assert!(beat.holds_hardware());
    drop(failing);
    assert!(!beat.holds_hardware());
    beat.begin_hardware_open().opened();
    assert!(beat.holds_hardware());
}

#[test]
fn a_decoder_stuck_in_an_operation_fails_a_send_to_its_full_queue() {
    // With the pool stuck, its queue fills, and the scheduler's next send would block before it
    // ever waits for a frame.
    let started = Instant::now();
    let worker = stalling_decode_worker(STALL_IN_FIRST_JOB, 1, Duration::from_millis(200));
    let mut index = 0;
    let error = loop {
        match worker.send(stalling_job(index)) {
            Ok(_) => index += 1,
            Err(error) => break error,
        }
        assert!(index < 16, "the queue never filled");
    };
    assert!(
        error.contains(crate::protocol::decode_stalled_tag()),
        "{error}"
    );
    drop(worker);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_decoder_stuck_closing_its_sessions_fails_the_finish() {
    // A shard closes its decoders after the last job, which the pool joins before it answers, so
    // a stuck close would hold an export that had every frame.
    let started = Instant::now();
    // The job really decodes, so the limit leaves room for that: a debug
    // build opening the file cold, under an antivirus scan, can take seconds.
    let mut worker = stalling_decode_worker(
        crate::decode::InjectedStall::OnClose,
        2,
        Duration::from_secs(5),
    );
    worker
        .send(stalling_job(0))
        .expect("the pool takes the job");
    loop {
        match worker
            .result_timeout(Duration::from_millis(10))
            .expect("the job decodes")
        {
            Some(_) => break,
            None => assert!(
                started.elapsed() < Duration::from_secs(30),
                "the job never decoded"
            ),
        }
    }
    let error = worker
        .finish()
        .expect_err("a shard stuck closing its decoders fails the finish");
    assert!(
        error.contains(crate::protocol::decode_stalled_tag()),
        "{error}"
    );
    let dropping = Instant::now();
    drop(worker);
    assert!(dropping.elapsed() < Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[test]
fn dropping_a_worker_with_a_stuck_shard_returns_when_no_stall_was_reported() {
    // An export failing for another reason drops its decode worker, whose join must not wait on the
    // stuck shard.
    let worker = stalling_decode_worker(STALL_IN_FIRST_JOB, 2, Duration::from_millis(200));
    worker
        .send(stalling_job(0))
        .expect("the pool takes the job");
    let waiting = Instant::now();
    while !worker.heartbeats[0].is_stalled() {
        assert!(
            waiting.elapsed() < Duration::from_secs(30),
            "the shard never reached its stall"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let heartbeats = worker.heartbeats.clone();
    assert!(!heartbeats.iter().any(|beat| beat.abandoned()));
    let started = Instant::now();
    drop(worker);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "dropping took {:?}",
        started.elapsed()
    );
    // This drop left the stuck shard running and said so: the process-wide flag alone is set by
    // every earlier stall test too.
    assert!(
        heartbeats.iter().any(|beat| beat.abandoned()),
        "the dropped worker's shards"
    );
    assert!(crate::heartbeat::worker_threads_abandoned());
}

/// An export request for the conform project's one-second clip, 12 frames,
/// in `codec`.
fn encode_stall_request(output: &Path) -> Request {
    parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "encode-stall", "operation": "export",
            "project": conform_project(&json!({})),
            "assets": {"video": corpus("parity-rgb.mov")},
            "startTicks": 0, "endTicks": 120_000,
            "frameRate": {"num": 12, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": output
        })
        .to_string(),
    )
    .expect("export request")
}

/// Runs `request` with its encode worker blocking forever where `stall`
/// says, watched with a short limit, and returns how it failed.
fn export_with_a_stuck_encoder(
    host: &mut Host,
    request: Request,
    stall: crate::heartbeat::InjectedStall,
) -> HostError {
    let started = Instant::now();
    crate::encode::INJECT_ENCODE_STALL.set(Some((stall, Duration::from_millis(300))));
    let error = within(
        Duration::from_secs(120),
        "an export with a stuck encoder",
        || {
            host.handle(request, &mut |_| Ok(()))
                .expect_err("an export whose encoder is stuck fails")
        },
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the stall was reported after {:?}",
        started.elapsed()
    );
    assert!(crate::heartbeat::worker_threads_abandoned());
    error
}

/// An output in place is a finished export: a GPU wait that fails after the
/// file is published is reported, not answered as the export's failure, on the
/// readback path and on the GPU-resident one. Answered as a failure, a retry to
/// the same path would be refused for the file already there.
#[test]
fn a_cleanup_failure_after_the_output_is_written_does_not_fail_the_export() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("cleanup-failure");
    let export = |host: &mut Host, request: Request, output: &Path| {
        crate::host::INJECT_CLEANUP_FAILURE.set(true);
        let result = host.handle(request, &mut |_| Ok(()));
        let injected = crate::host::INJECT_CLEANUP_FAILURE.replace(false);
        let result = result.expect("an export whose output is written succeeds");
        assert!(!injected, "the cleanup ran");
        assert!(output.is_file(), "{result}");
        assert_eq!(
            result["performance"]["retiredLeasesAtExit"],
            json!(0),
            "{result}"
        );
        result
    };
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let output = directory.join("readback.mp4");
    let result = export(&mut host, encode_stall_request(&output), &output);
    assert_eq!(result["videoOutputPath"], json!("readback"), "{result}");

    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwencode"
    ))]
    if let Some((mut host, _)) = gpu_output_host(VideoDecodePolicy::Software) {
        let output = directory.join("gpu.mp4");
        let mut request = json!({
            "protocolVersion": 1, "requestId": "cleanup-gpu", "operation": "export",
            "project": conform_project(&json!({})),
            "assets": {"video": corpus("parity-rgb.mov")},
            "startTicks": 0, "endTicks": 120_000,
            "frameRate": {"num": 12, "den": 1},
            "videoCodec": "h264",
            "inputPolicy": "gpu-required",
            "output": output
        });
        request["project"]["settings"]["width"] = json!(640);
        request["project"]["settings"]["height"] = json!(360);
        let result = export(
            &mut host,
            parse_request(&request.to_string()).expect("request"),
            &output,
        );
        assert_eq!(result["videoOutputPath"], json!("gpu-resident"), "{result}");
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// Run by [`the_synchronous_comparison_switch_is_not_ignored_on_the_gpu`]
/// in a child process whose `ALPHA2_EXPORT_PIPELINE` is off.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
#[ignore = "run by the_synchronous_comparison_switch_is_not_ignored_on_the_gpu with ALPHA2_EXPORT_PIPELINE=off"]
fn a_gpu_required_export_with_the_pipeline_off() {
    assert!(!crate::config::configured_pipeline_enabled());
    let Some((mut host, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("pipeline-off-gpu");
    let mut request = json!({
        "protocolVersion": 1, "requestId": "pipeline-off", "operation": "export",
        "project": conform_project(&json!({})),
        "assets": {"video": corpus("parity-rgb.mov")},
        "startTicks": 0, "endTicks": 120_000,
        "frameRate": {"num": 12, "den": 1},
        "videoCodec": "h264",
        "inputPolicy": "gpu-required",
        "output": directory.join("out.mp4")
    });
    request["project"]["settings"]["width"] = json!(640);
    request["project"]["settings"]["height"] = json!(360);
    let error = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |_| Ok(()),
        )
        .expect_err("the GPU-resident path is not the synchronous comparison path");
    assert_eq!(error.code, "GPU_OUTPUT_UNAVAILABLE", "{}", error.message);
    assert!(
        error.message.contains("ALPHA2_EXPORT_PIPELINE is off"),
        "{}",
        error.message
    );
    std::fs::remove_dir_all(&directory).ok();
    println!("checked: the GPU-resident path refused");
}

/// `ALPHA2_EXPORT_PIPELINE=off` runs the synchronous comparison path, which is
/// the readback path. The GPU-resident path declines the export under it,
/// saying why; running its worker pipeline and reporting `pipelineEnabled:
/// true`, it would leave the switch doing nothing to a default H.264 export
/// where that path exists. The switch is read from the environment, so the
/// export runs in a child of this test binary.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
fn the_synchronous_comparison_switch_is_not_ignored_on_the_gpu() {
    let _gpu = gpu_test_lock();
    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env("ALPHA2_EXPORT_PIPELINE", "off")
        .args([
            "--exact",
            "tests::stalls::a_gpu_required_export_with_the_pipeline_off",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    let stdout = String::from_utf8_lossy(&child.stdout);
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        child.status.success() && stdout.contains("1 passed"),
        "{}\n{stdout}\n{stderr}",
        child.status
    );
    if !stdout.contains("checked: the GPU-resident path refused") {
        eprintln!("skipped: {}", stderr.lines().last().unwrap_or_default());
    }
}

#[test]
fn an_encoder_stuck_in_a_call_fails_the_export_instead_of_hanging() {
    // An encode worker blocked inside FFmpeg (a dead hard-mounted share) fills its channel; a
    // blocking send would wait forever.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("encode-stall-send");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let error = export_with_a_stuck_encoder(
        &mut host,
        encode_stall_request(&directory.join("out.mp4")),
        crate::heartbeat::InjectedStall::AfterOperations(0),
    );
    assert_eq!(error.code, "ENCODE_STALLED", "{}", error.message);
    assert!(
        error.message.contains("in a software encoder"),
        "{}",
        error.message
    );
    let failure = error.failure.expect("an export failure report");
    assert_eq!(failure["hostCode"], "ENCODE_STALLED");
    assert_eq!(failure["code"], "GPU_TIMEOUT");
    assert_eq!(failure["resource"], "unknown");
    assert_eq!(failure["phase"], "encode");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn an_encoder_stuck_finishing_its_output_fails_the_export() {
    // After the last frame the finish comes from a worker that may be stuck flushing or writing the
    // trailer.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("encode-stall-finish");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let error = export_with_a_stuck_encoder(
        &mut host,
        encode_stall_request(&directory.join("out.mp4")),
        crate::heartbeat::InjectedStall::OnClose,
    );
    assert_eq!(error.code, "ENCODE_STALLED", "{}", error.message);
    let failure = error.failure.expect("an export failure report");
    assert_eq!(failure["code"], "GPU_TIMEOUT");
    assert_eq!(failure["phase"], "mux");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn a_gpu_resident_encoder_stuck_on_the_gpu_fails_the_export_on_the_gpu() {
    // `h264_vulkan` waits on its fences with UINT64_MAX; the stall is injected in the worker, so it
    // is the same on any backend.
    let _gpu = gpu_test_lock();
    let Some((mut host, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("encode-stall-gpu");
    let mut request = json!({
        "protocolVersion": 1, "requestId": "encode-stall-gpu", "operation": "export",
        "project": conform_project(&json!({})),
        "assets": {"video": corpus("parity-rgb.mov")},
        "startTicks": 0, "endTicks": 120_000,
        "frameRate": {"num": 12, "den": 1},
        "videoCodec": "h264",
        "inputPolicy": "gpu-required",
        "output": directory.join("out.mp4")
    });
    request["project"]["settings"]["width"] = json!(640);
    request["project"]["settings"]["height"] = json!(360);
    let error = export_with_a_stuck_encoder(
        &mut host,
        parse_request(&request.to_string()).expect("request"),
        crate::heartbeat::InjectedStall::AfterOperations(0),
    );
    assert_eq!(error.code, "ENCODE_STALLED", "{}", error.message);
    assert!(
        error
            .message
            .contains(crate::protocol::HARDWARE_ENCODE_STALL_MARKER),
        "{}",
        error.message
    );
    let failure = error.failure.expect("an export failure report");
    assert_eq!(failure["code"], "GPU_TIMEOUT");
    assert_eq!(failure["resource"], "gpu");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn dropping_an_encode_worker_stuck_in_a_call_returns_when_no_stall_was_reported() {
    // An export failing for another reason drops its encode worker, whose join must not wait.
    let directory = scratch_directory("encode-stall-drop");
    let sink = alpha2_native::NativeVideoEncodeSink::create(
        directory.join("out.mp4"),
        alpha2_native::VideoEncodeConfig {
            width: 16,
            height: 16,
            frame_rate: alpha2_time::FrameRate::new(12, 1).expect("rate"),
            color: alpha2_media::ColorMetadata::default(),
            rate_control: alpha2_media::VideoRateControl::bitrate(
                200_000,
                alpha2_media::BitrateMode::Variable,
            )
            .expect("rate control"),
            codec: NativeVideoCodec::Mpeg4Part2,
            display: alpha2_native::VideoDisplay::default(),
            container: alpha2_media::Container::Mp4,
        },
    )
    .expect("sink");
    crate::encode::INJECT_ENCODE_STALL.set(Some((
        crate::heartbeat::InjectedStall::AfterOperations(0),
        Duration::from_millis(300),
    )));
    let worker = EncodeWorker::spawn(
        crate::encode::NativeExportSink::Video(sink),
        CancelToken::default(),
        PipelineFailure::default(),
        1,
        Instant::now(),
        Duration::from_secs(60),
    )
    .expect("encode worker");
    worker
        .send(EncodeCommand::Finish)
        .expect("the worker takes the command");
    let waiting = Instant::now();
    while !worker.heartbeat.is_stalled() {
        assert!(
            waiting.elapsed() < Duration::from_secs(30),
            "the worker never reached its stall"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let heartbeat = Arc::clone(&worker.heartbeat);
    assert!(!heartbeat.abandoned());
    let started = Instant::now();
    within(
        Duration::from_secs(60),
        "dropping a stuck encode worker",
        || {
            drop(worker);
        },
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "dropping took {:?}",
        started.elapsed()
    );
    assert!(heartbeat.abandoned(), "the dropped encode worker");
    assert!(crate::heartbeat::worker_threads_abandoned());
    std::fs::remove_dir_all(&directory).ok();
}

/// A copy export of `h264-bt709-copy.mp4`'s two seconds into `output`, video
/// only, with `strategy`.
fn stalled_copy_request(output: &Path, strategy: &str) -> Request {
    let second = alpha2_time::TICKS_PER_SECOND;
    parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "copy-stall", "operation": "export",
            "project": {
                "schema_version": 1, "id": "copy", "name": "Copy",
                "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
                "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
                "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                    "id": "clip", "source_id": "video",
                    "timeline_range": {"start": 0, "end": 2 * second},
                    "source_range": {"start": 0, "end": 2 * second}
                }]}]}
            },
            "assets": {"video": corpus("h264-bt709-copy.mp4")},
            "startTicks": 0, "endTicks": 2 * second,
            "frameRate": {"num": 30, "den": 1},
            "includeAudio": false,
            "videoCodec": "h264",
            "strategy": strategy,
            "output": output
        })
        .to_string(),
    )
    .expect("request")
}

#[test]
fn a_copy_whose_output_stops_answering_fails_as_an_encode_stall() {
    // A copy whose muxer write never returns is watched, so the export is still answered and a
    // termination's cancel seen.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("copy-stall");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    crate::packet_copy::INJECT_COPY_STALL.with(|stall| {
        stall.set(Some((
            crate::packet_copy::CopyStage::Copy,
            crate::heartbeat::InjectedStall::AfterOperations(4),
            Duration::from_millis(300),
        )));
    });
    let mut reported = Vec::new();
    let error = within(Duration::from_secs(60), "the stalled copy", || {
        host.handle(
            stalled_copy_request(&directory.join("out.mp4"), "require-copy"),
            &mut |event| {
                reported.push(event.completed);
                Ok(())
            },
        )
        .expect_err("a stalled copy")
    });
    assert_eq!(error.code, "ENCODE_STALLED", "{}", error.message);
    assert!(
        error
            .message
            .contains("the packet copy has been inside one muxer call"),
        "{}",
        error.message
    );
    let failure = error.failure.expect("an export failure report");
    assert_eq!(failure["hostCode"], "ENCODE_STALLED");
    assert_eq!(failure["phase"], "encode", "{failure}");
    assert_eq!(
        failure["diagnostics"]["videoOutputPath"],
        json!("packet-copy"),
        "{failure}"
    );
    assert!(reported.is_empty(), "{reported:?}");
    assert!(crate::heartbeat::worker_threads_abandoned());
    assert!(!directory.join("out.mp4").exists());
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_copy_plan_whose_source_stops_answering_fails_as_a_decode_stall() {
    // A stalled plan fails the export even where the copy was only preferred: a render would read
    // the same source.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("copy-plan-stall");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    for strategy in ["require-copy", "prefer-copy"] {
        crate::packet_copy::INJECT_COPY_STALL.with(|stall| {
            stall.set(Some((
                crate::packet_copy::CopyStage::Plan,
                crate::heartbeat::InjectedStall::AfterOperations(0),
                Duration::from_millis(300),
            )));
        });
        let error = within(Duration::from_secs(60), "the stalled plan", || {
            host.handle(
                stalled_copy_request(&directory.join(format!("{strategy}.mp4")), strategy),
                &mut |_| Ok(()),
            )
            .expect_err("a stalled plan")
        });
        assert_eq!(
            error.code, "DECODE_STALLED",
            "{strategy}: {}",
            error.message
        );
        assert!(
            error
                .message
                .contains("the packet copy's plan has been inside one read"),
            "{}",
            error.message
        );
        let failure = error.failure.expect("an export failure report");
        assert_eq!(failure["phase"], "decode", "{failure}");
    }
    std::fs::remove_dir_all(&directory).ok();
}
