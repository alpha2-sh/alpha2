//! Export pipeline limits, stage timings and traces, and the GPU upload of decoded frames.

use super::*;

#[test]
#[allow(clippy::used_underscore_binding)]
fn rgba_frames_present_on_the_gpu_from_pooled_upload_textures() {
    let _gpu = gpu_test_lock();
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_product()).expect("a GPU device");
    let device = gpu.device().clone();
    let renderer = alpha2_render::Renderer::new(gpu);
    let frame = DecodedVideoFrame {
        pts: MediaTime::ZERO,
        dts: None,
        duration: None,
        visible_rect: Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        },
        sample_aspect_ratio: SampleAspectRatio::default(),
        orientation: Orientation::UPRIGHT,
        pixel_format: PixelFormat::Rgba8,
        chroma_location: alpha2_media::ChromaLocation::Unspecified,
        color: Some(ColorMetadata::default()),
        planes: vec![Plane {
            // Rows padded past their two pixels: a second row read at the wrong offset starts with
            // the padding.
            bytes: vec![
                255, 0, 0, 128, 0, 255, 0, 255, 9, 9, //
                0, 0, 255, 255, 255, 255, 255, 255, 9, 9,
            ]
            .into(),
            stride: 10,
            width: 2,
            height: 2,
        }],
    };
    let size = RenderSize::new(4, 4).expect("valid size");
    let upload = |renderer: &alpha2_render::Renderer| {
        let upload = crate::frames::upload_frame(
            renderer,
            size,
            alpha2_render::ColorPolicy::RejectHdr,
            &frame,
        )
        .expect("uploads");
        assert!(!upload.direct_yuv);
        let pending = upload
            .texture
            .pending
            .as_ref()
            .expect("presentation pending");
        assert!(renderer.submit_pending_source(pending));
        upload
    };
    // The presented texture holds the frame's pixels, premultiplied, each row from its own stride;
    // any validation error fails the test.
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let presented = upload(&renderer);
    let pixels = renderer
        .read_rgba8(&presented.texture._texture)
        .expect("reads the presented texture");
    if let Some(error) = pollster::block_on(scope.pop()) {
        panic!("{error}");
    }
    let at = |x: usize, y: usize| &pixels[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
    for ((x, y), expected) in [
        ((0, 0), [128, 0, 0, 128]),
        ((3, 0), [0, 255, 0, 255]),
        ((0, 3), [0, 0, 255, 255]),
        ((3, 3), [255, 255, 255, 255]),
    ] {
        assert!(
            at(x, y)
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "({x}, {y}) is {:?}, expected {expected:?}: {pixels:?}",
            at(x, y)
        );
    }
    drop(presented);
    let warm = renderer.diagnostics().upload_textures;
    for _ in 0..10 {
        upload(&renderer);
    }
    let after = renderer.diagnostics().upload_textures;
    assert_eq!(after.created, warm.created, "upload textures after warm-up");
    assert_eq!(after.reused - warm.reused, 10);
}

#[test]
fn export_pipeline_limits_include_staging_and_encode_frames() {
    let size = RenderSize::new(3_840, 2_160).expect("4K size");
    let limits = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        InFlightBudget::requested(DEFAULT_EXPORT_IN_FLIGHT_BYTES),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        None,
    )
    .expect("default budget holds a bounded 4K pipeline");
    assert_eq!(limits.readback_depth, 3);
    // 256 MiB holds 21 packed 4K frames. Three are readbacks, one is the frame the coordinator is
    // packing and one the frame the worker is encoding, so the channel takes the ceiling.
    assert_eq!(limits.encode_channel_depth, MAX_ENCODE_CHANNEL_DEPTH);
    assert_eq!(limits.packed_bytes_per_frame, 12_441_600);
    // The renderer's layout: 3840-byte luma rows, and chroma rows padded from 1920 to 2048 bytes,
    // each plane on its own placement boundary, then the readback's 16-byte canary.
    assert_eq!(limits.staging_bytes_per_frame, 12_718_080 + 16);
    let reserved = limits
        .staging_bytes_per_frame
        .saturating_mul(u64::try_from(limits.readback_depth).expect("depth fits"))
        .saturating_add(
            limits.packed_bytes_per_frame.saturating_mul(
                u64::try_from(crate::metrics::readback_encode_frame_bound(
                    limits.encode_channel_depth,
                ))
                .expect("depth fits"),
            ),
        );
    assert!(reserved <= limits.byte_budget);

    let pinned = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        InFlightBudget::requested(DEFAULT_EXPORT_IN_FLIGHT_BYTES),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        Some(2),
    )
    .expect("a pinned depth still fits the default budget");
    assert_eq!(pinned.encode_channel_depth, 2);

    // A budget that cannot pay for the derived depth degrades to one that fits rather than failing:
    // one readback and three packed frames hold the readback, the frame being packed, the frame
    // being encoded, and a channel of one.
    let tight = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        InFlightBudget::requested(12_718_080 + 16 + 12_441_600 * 3),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        None,
    )
    .expect("a four-frame budget still runs");
    assert_eq!(tight.readback_depth, 1);
    assert_eq!(tight.encode_channel_depth, 1);

    // The least a readback export runs with is a rendezvous channel; a budget for one readback and
    // one packed frame is refused.
    let least = 12_718_080 + 16 + 12_441_600 * 2;
    let rendezvous = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        InFlightBudget::requested(least),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        None,
    )
    .expect("one readback and two packed frames run");
    assert_eq!(rendezvous.readback_depth, 1);
    assert_eq!(rendezvous.encode_channel_depth, 0);
    let error = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        InFlightBudget::requested(least - 1),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        None,
    )
    .expect_err("one readback and less than two packed frames is refused");
    assert!(
        error.contains("one readback and two packed 3840x2160 YUV420P frames"),
        "{error}"
    );
    assert!(
        error.contains("in-flight budget (memory.inFlightBytes) cannot hold"),
        "{error}"
    );

    assert!(
        export_pipeline_limits(
            size,
            wgpu::TextureFormat::Rgba16Float,
            InFlightBudget::requested(1),
            1,
            Some(0)
        )
        .is_err()
    );
}

/// A retry's `memory.scale` does not shrink a budget below the least an export
/// runs with: the minimal profile (0.25) would otherwise refuse an 8K export's
/// one readback and two packed frames as `RESOURCE_LIMIT`, a permanent failure
/// in place of an out-of-memory retry. The browser keeps a retry at its least
/// too. The least is held up to the budget before the scale; a budget that
/// cannot hold it before any scale is still refused.
#[test]
fn a_retry_scale_holds_the_least_an_export_needs() {
    use crate::memory::{MemoryPolicy, MemoryRequest};
    let size = RenderSize::new(7_680, 4_320).expect("8K");
    let minimal = MemoryPolicy::resolve(MemoryRequest {
        in_flight_bytes: Some(DEFAULT_EXPORT_IN_FLIGHT_BYTES),
        scale: Some(0.25),
        ..MemoryRequest::default()
    })
    .expect("policy");
    let limits = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        minimal.in_flight_budget(),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        None,
    )
    .expect("the minimal retry profile still exports 8K");
    assert_eq!((limits.readback_depth, limits.encode_channel_depth), (1, 0));
    assert!(limits.byte_budget > DEFAULT_EXPORT_IN_FLIGHT_BYTES / 4);
    assert!(limits.byte_budget <= DEFAULT_EXPORT_IN_FLIGHT_BYTES);
    let small = MemoryPolicy::resolve(MemoryRequest {
        in_flight_bytes: Some(64 << 20),
        scale: Some(0.25),
        ..MemoryRequest::default()
    })
    .expect("policy");
    let refused = export_pipeline_limits(
        size,
        wgpu::TextureFormat::Rgba16Float,
        small.in_flight_budget(),
        MAX_IN_FLIGHT_VIDEO_FRAMES,
        None,
    )
    .expect_err("64 MiB holds no 8K readback");
    assert!(refused.contains("RESOURCE_LIMIT"), "{refused}");
}

/// A decoded frame set larger than the scaled decode queue budget
/// fits alone, up to the budget before the scale; two of them still wait
/// for each other, and one larger than the unscaled budget is refused.
#[test]
fn a_scaled_decode_queue_holds_one_frame_set_up_to_its_unscaled_budget() {
    let budget = DecodeBudget {
        limit: crate::memory::ByteBudget {
            bytes: 8,
            ceiling: 32,
        },
        state: Mutex::new(DecodeBudgetState::default()),
        available: Condvar::new(),
    };
    let cancel = CancelToken::default();
    budget.reserve(20, &cancel).expect("one set fits alone");
    budget.release(20);
    let error = budget
        .reserve(33, &cancel)
        .expect_err("over the unscaled budget");
    assert!(error.contains("32 byte decode queue budget"), "{error}");
    budget.reserve(6, &cancel).expect("fits the scaled budget");
    assert_eq!(budget.peak_bytes(), 20);
}

#[test]
fn the_frame_submit_split_accounts_for_the_whole_stage() {
    // A stage split is only useful if it adds up.
    let stages = ExportStepTimings {
        evaluate: Duration::from_millis(3),
        prepare: Duration::from_millis(5),
        submit: FrameSubmitTimings {
            graph_build: Duration::from_millis(7),
            command_encode: Duration::from_millis(11),
            source_acquire: Duration::from_millis(13),
            queue_lock_wait: Duration::from_millis(17),
            queue_submit: Duration::from_millis(19),
            lease_retire: Duration::from_millis(23),
            complete: Duration::from_millis(29),
        },
    };
    let metrics = ExportMetrics {
        frame_submit: Duration::from_millis(3 + 5 + 7 + 11 + 13 + 17 + 19 + 23 + 29),
        frame_submit_stages: stages.merged(ExportStepTimings::default()),
        encode_worker_idle: Duration::from_millis(41),
        ..ExportMetrics::default()
    };
    let mut value = serde_json::json!({
        "frameSubmitMs": metrics.frame_submit.as_secs_f64() * 1_000.0,
    });
    add_stage_split_metrics(&mut value, &metrics);
    let part = |key: &str| {
        value[key]
            .as_f64()
            .unwrap_or_else(|| panic!("{key} is reported"))
    };
    let split = [
        "frameEvaluateMs",
        "framePrepareMs",
        "frameGraphBuildMs",
        "frameCommandEncodeMs",
        "frameSourceAcquireMs",
        "frameQueueLockWaitMs",
        "frameQueueSubmitMs",
        "frameLeaseRetireMs",
        "frameCompleteMs",
    ]
    .into_iter()
    .map(part)
    .sum::<f64>();
    assert!((split - part("frameSubmitMs")).abs() < 1e-6);
    assert!((part("encodeWorkerIdleMs") - 41.0).abs() < 1e-6);
    assert!(value.get("readbackBlockingMs").is_none());
    assert!(value.get("pipelineOverlapFrames").is_none());
    assert!(value.get("gpuCopyBytes").is_none());
}

#[test]
fn the_export_trace_pairs_encode_spans_with_the_rows_that_queued_them() {
    // An audio block is reported by nothing at all, so the audio pairing is positional.
    let epoch = Instant::now();
    let mut trace = ExportTrace::from_env(epoch)
        .unwrap_or_else(|| ExportTrace::for_path(PathBuf::from("/dev/null"), epoch));
    trace.push_video(VideoFrameTrace {
        index: 0,
        packed: Duration::from_millis(1),
        ..VideoFrameTrace::default()
    });
    trace.push_audio(AudioBlockTrace {
        queued: Duration::from_millis(2),
        ..AudioBlockTrace::default()
    });
    trace.push_audio(AudioBlockTrace {
        queued: Duration::from_millis(3),
        ..AudioBlockTrace::default()
    });
    trace.record_encode(
        Some(0),
        Duration::from_millis(10),
        Duration::from_millis(18),
        Duration::from_millis(20),
    );
    trace.record_encode(
        None,
        Duration::from_millis(21),
        Duration::from_millis(21),
        Duration::from_millis(22),
    );
    trace.record_encode(
        None,
        Duration::from_millis(23),
        Duration::from_millis(24),
        Duration::from_millis(25),
    );
    // An index the export never produced must be dropped, not mis-assigned.
    trace.record_encode(
        Some(7),
        Duration::from_millis(99),
        Duration::from_millis(99),
        Duration::from_millis(99),
    );

    // A pipeline created while frame 0 was drawn, 4 ms into the export, taking 1.5 ms.
    trace.push_pipelines(
        0,
        &[alpha2_render::PipelineCreation {
            label: "alpha2.effect.glow".to_owned(),
            started: epoch + Duration::from_millis(4),
            finished: epoch + Duration::from_micros(5_500),
        }],
    );

    let csv = trace.to_csv();
    let rows = csv.lines().collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        5,
        "header plus one video, two audio and one pipeline row"
    );
    assert!(rows[0].starts_with("kind,index,"));
    assert!(rows[0].ends_with(",planesReadyMs,label,startedMs,finishedMs"));
    assert!(
        rows[1].ends_with("10.000,18.000,20.000,0.000,0.000,,,"),
        "video row carries its own encode span and send boundary: {}",
        rows[1]
    );
    assert_eq!(rows[2], "audio,0,,,,,,,,2.000,21.000,21.000,22.000,,,,,");
    assert_eq!(rows[3], "audio,1,,,,,,,,3.000,23.000,24.000,25.000,,,,,");
    assert_eq!(
        rows[4],
        "pipeline,0,,,,,,,,,,,,,,alpha2.effect.glow,4.000,5.500"
    );
    let columns = |row: &str| row.split(',').count();
    assert!(
        rows.iter().all(|row| columns(row) == columns(rows[0])),
        "{rows:?}"
    );
}

#[test]
fn an_odd_export_size_is_refused_before_an_encoder_opens() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("odd-export");
    let mut project: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/cases/host-parity-project.json"),
        )
        .expect("fixture"),
    )
    .expect("fixture JSON");
    project["settings"]["width"] = json!(65);
    project["settings"]["height"] = json!(49);
    let assets = HashMap::from([(
        "image-source".to_owned(),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/cross_topology_parity.png"),
    )]);
    let output = directory.join("odd.mp4");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let error = host
        .export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            5_000,
            RateRequest { num: 24, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: NativeVideoCodec::Mpeg4Part2,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            &output,
            &mut |_| Ok(()),
        )
        .expect_err("65x49 is not a 4:2:0 size");
    assert!(
        error.contains("65x49") && error.contains("64x48"),
        "{error}"
    );
    assert_eq!(HostError::classify(error).code, "INVALID_INPUT");
    assert!(!output.exists(), "nothing was written");
    assert!(
        std::fs::read_dir(&directory)
            .expect("scratch")
            .next()
            .is_none(),
        "no partial output either"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// The AAC encoder runs on a thread of its own and adds its time to the sink's
/// totals whenever it finishes a frame. A command's `sent` counts the video
/// encoder's sends only: the difference of those totals across the command
/// would charge a video frame's trace row with the audio thread's work, and
/// could end its send after the command did.
#[test]
fn a_commands_send_time_is_its_own_threads() {
    let call = alpha2_native::EncodeStageTimings {
        video_send_frame: Duration::from_micros(100),
        audio_send_frame: Duration::from_micros(600),
        ..alpha2_native::EncodeStageTimings::default()
    };
    assert_eq!(
        crate::encode::span_send_time(call),
        Duration::from_micros(100)
    );
}

/// At the least budget an export runs with, the encode channel is a rendezvous, and a hand-off to
/// a busy encoder waits for the encoder's take, not for the stall-check interval.
#[test]
fn a_rendezvous_export_waits_only_for_the_encoder() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("rendezvous-export");
    let size = RenderSize::new(640, 360).expect("size");
    let least = crate::frames::estimated_staging_bytes(size, wgpu::TextureFormat::Rgba16Float)
        + crate::frames::packed_yuv420_bytes(size)
            * u64::try_from(crate::metrics::readback_encode_frame_bound(0)).expect("bound");
    let end = 2 * alpha2_time::TICKS_PER_SECOND;
    // The encoder is the bottleneck, so most hand-offs find it busy.
    crate::encode::INJECT_SLOW_ENCODE.with(|slow| slow.set(Some(Duration::from_millis(15))));
    let request = json!({
        "protocolVersion": 1, "operation": "export", "requestId": "rendezvous",
        "project": {
            "schema_version": 1, "id": "rendezvous", "name": "Rendezvous",
            "settings": {"width": 640, "height": 360, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": end},
                "source_range": {"start": 0, "end": end}
            }]}]}
        },
        "assets": {"video": corpus("h264-long-gop.mp4")},
        "startTicks": 0, "endTicks": end,
        "frameRate": {"num": 30, "den": 1},
        "includeAudio": false,
        "videoCodec": "mpeg4-part2",
        "inputPolicy": "cpu-readback",
        "memory": {"inFlightBytes": least},
        "output": directory.join("rendezvous.mp4")
    });
    let result = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |_| Ok(()),
        )
        .expect("export");
    let performance = &result["performance"];
    assert_eq!(performance["encodeChannelDepth"], 0, "{performance}");
    let waits = performance["encodeBackpressureCount"]
        .as_u64()
        .expect("backpressure count");
    assert!(
        waits > 0,
        "the slow encoder never made a hand-off wait: {performance}"
    );
    let waited = performance["encodeQueueWaitMs"]
        .as_f64()
        .expect("queue wait");
    #[allow(clippy::cast_precision_loss)]
    let average = waited / waits as f64;
    assert!(
        average < 125.0,
        "a hand-off waited {average:.1} ms on average ({waited:.1} ms over {waits})"
    );
    std::fs::remove_dir_all(&directory).ok();
}
