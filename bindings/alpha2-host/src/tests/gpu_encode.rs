//! GPU encode: encoder capabilities, the GPU budget, and the input policy.

use super::*;

/// A capability with the shape the Vulkan backend reports, for the budget's
/// arithmetic alone.
#[cfg(feature = "hwencode")]
fn nv12_capability() -> alpha2_native::HardwareEncodeCapability {
    alpha2_native::HardwareEncodeCapability {
        backend: alpha2_native::HardwareEncodeBackend::Vulkan,
        encoder: "h264_vulkan",
        codec: NativeVideoCodec::H264,
        gpu_input_only: true,
        input_layout: alpha2_native::EncoderInputLayout::Nv12,
        handoff: alpha2_native::EncoderInputHandoff::GpuCopy,
        shares_renderer_device: true,
        minimum: (160, 64),
        maximum: (4096, 4096),
        granularity: (16, 16),
        max_p_picture_l0_references: Some(1),
        max_b_picture_l0_references: Some(0),
        max_l1_references: Some(0),
        rate_control_modes: vec!["cbr", "vbr"],
        plane_ring_slots: 4,
        handoff_retained_surfaces: 4,
        codec_retained_surfaces: 0,
    }
}

/// The start line names the backend and every encoder the GPU-resident path
/// takes on the renderer's adapter, or the reason it takes none.
#[cfg(feature = "hwencode")]
#[test]
fn the_gpu_encode_start_line_names_the_encoders_or_why_there_are_none() {
    let d3d12 = |codec: NativeVideoCodec, encoder| alpha2_native::HardwareEncodeCapability {
        backend: alpha2_native::HardwareEncodeBackend::D3D12Va,
        encoder,
        codec,
        ..nv12_capability()
    };
    assert_eq!(
        crate::gpu_encode_diagnostic(
            "Intel(R) Iris(R) Xe Graphics",
            &[
                d3d12(NativeVideoCodec::H264, "h264_d3d12va"),
                d3d12(NativeVideoCodec::Hevc, "hevc_d3d12va")
            ],
            None,
        ),
        "d3d12va h264_d3d12va, hevc_d3d12va on Intel(R) Iris(R) Xe Graphics"
    );
    assert_eq!(
        crate::gpu_encode_diagnostic(
            "NVIDIA GeForce MX450",
            &[],
            Some(
                "NVIDIA GeForce MX450 has no D3D12 device shared with a hardware encoder: \
                 AVERROR(-22)"
                    .to_owned()
            ),
        ),
        "unavailable: NVIDIA GeForce MX450 has no D3D12 device shared with a hardware encoder: \
         AVERROR(-22)"
    );
    assert_eq!(
        crate::gpu_encode_diagnostic("WARP", &[], None),
        "unavailable: no encoder takes GPU frames here"
    );
}

#[cfg(feature = "hwencode")]
#[test]
fn the_gpu_budget_counts_the_ring_the_surfaces_and_what_the_handoff_retains() {
    use crate::metrics::gpu_encode_pipeline_limits;
    let capability = nv12_capability();
    let size = RenderSize::new(1920, 1080).expect("size");
    // 1080 codes as 1088: surfaces are sized at the coded extent, the ring at the output.
    let surface = 1920 * 1088 * 3 / 2;
    let ring = 4 * 1920 * 1080 * 3 / 2;
    let minimum = ring + surface * (2 + 4);

    let roomy = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(256 << 20),
        None,
    )
    .expect("roomy");
    let budget = roomy.gpu.expect("gpu budget");
    assert_eq!(
        roomy.encode_channel_depth,
        crate::metrics::GPU_DEFAULT_ENCODE_CHANNEL_DEPTH
    );
    assert_eq!(budget.surface_bytes, surface);
    assert_eq!(budget.ring_bytes, ring);
    assert_eq!(
        budget.surface_ceiling(),
        crate::metrics::GPU_DEFAULT_ENCODE_CHANNEL_DEPTH + 2 + 4
    );
    let deep = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(256 << 20),
        Some(64),
    )
    .expect("deep");
    assert_eq!(deep.encode_channel_depth, crate::MAX_ENCODE_CHANNEL_DEPTH);
    assert!(budget.reserved_bytes() <= 256 << 20);

    let exact =
        gpu_encode_pipeline_limits(&capability, size, InFlightBudget::requested(minimum), None)
            .expect("minimum");
    assert_eq!(exact.encode_channel_depth, 0);
    assert_eq!(exact.gpu.expect("gpu budget").reserved_bytes(), minimum);

    let short = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(minimum - 1),
        None,
    );
    assert!(
        short
            .as_ref()
            .is_err_and(|reason| reason.contains("minimum of 6 encoder surfaces")),
        "{short:?}",
    );
    // The refusal names the budget as it was set, never a variable nobody set.
    let scaled = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget {
            bytes: (minimum - 1) / 4,
            source: crate::memory::BudgetSource::Default,
            scale: 0.25,
            unscaled: minimum - 1,
        },
        None,
    )
    .expect_err("short");
    assert!(
        scaled.starts_with(&format!(
            "the {}-byte in-flight budget (the default; memory.scale 0.25 asked for {}) cannot hold",
            minimum - 1,
            (minimum - 1) / 4
        )),
        "{scaled}"
    );
    assert!(
        !scaled.contains("ALPHA2_EXPORT_IN_FLIGHT_BYTES"),
        "{scaled}"
    );

    let pinned = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(minimum + surface),
        Some(8),
    )
    .expect("pinned");
    assert_eq!(pinned.encode_channel_depth, 1);
    let unpinned = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(256 << 20),
        Some(3),
    )
    .expect("unpinned");
    assert_eq!(unpinned.encode_channel_depth, 3);
}

/// A capability with the shape the `VideoToolbox` backend reports: no plane
/// ring, the renderer draws into the encoder's own surfaces, which are the
/// output size, and the codec keeps six of them past each send.
#[cfg(feature = "hwencode")]
fn direct_target_capability() -> alpha2_native::HardwareEncodeCapability {
    alpha2_native::HardwareEncodeCapability {
        backend: alpha2_native::HardwareEncodeBackend::VideoToolbox,
        encoder: "h264_videotoolbox",
        codec: NativeVideoCodec::H264,
        gpu_input_only: false,
        input_layout: alpha2_native::EncoderInputLayout::Nv12,
        handoff: alpha2_native::EncoderInputHandoff::DirectTarget,
        shares_renderer_device: true,
        minimum: (2, 2),
        maximum: (4096, 4096),
        granularity: (1, 1),
        max_p_picture_l0_references: None,
        max_b_picture_l0_references: None,
        max_l1_references: None,
        rate_control_modes: vec!["vbr"],
        plane_ring_slots: 0,
        handoff_retained_surfaces: 0,
        codec_retained_surfaces: 6,
    }
}

/// The budget of a backend with no ring and codec-held surfaces,
/// which only a Mac reports, checked on any machine.
#[cfg(feature = "hwencode")]
#[test]
fn the_gpu_budget_of_a_direct_target_holds_the_surfaces_the_codec_keeps() {
    use crate::metrics::gpu_encode_pipeline_limits;
    let capability = direct_target_capability();
    let size = RenderSize::new(1920, 1080).expect("size");
    // The pool's buffers are the output size: no coded padding, and no ring.
    let surface = 1920 * 1080 * 3 / 2;
    let minimum = surface * (2 + 6);

    let roomy = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(256 << 20),
        None,
    )
    .expect("roomy");
    let budget = roomy.gpu.expect("gpu budget");
    assert_eq!(budget.surface_bytes, surface);
    assert_eq!(budget.ring_bytes, 0);
    assert_eq!(budget.retained_surfaces, 6);
    assert_eq!(
        roomy.encode_channel_depth,
        crate::metrics::GPU_DEFAULT_ENCODE_CHANNEL_DEPTH
    );
    assert_eq!(
        budget.surface_ceiling(),
        crate::metrics::GPU_DEFAULT_ENCODE_CHANNEL_DEPTH + 2 + 6
    );
    assert_eq!(roomy.staging_bytes_per_frame, 0);

    let exact =
        gpu_encode_pipeline_limits(&capability, size, InFlightBudget::requested(minimum), None)
            .expect("minimum");
    assert_eq!(exact.encode_channel_depth, 0);
    assert_eq!(exact.gpu.expect("gpu budget").reserved_bytes(), minimum);
    let short = gpu_encode_pipeline_limits(
        &capability,
        size,
        InFlightBudget::requested(minimum - 1),
        None,
    );
    assert!(
        short.as_ref().is_err_and(|reason| reason
            .contains("plane ring (0 slots, 0 B) and its minimum of 8 encoder surfaces")),
        "{short:?}",
    );

    for (width, height) in [(2, 2), (1918, 1080), (4096, 4096)] {
        assert_eq!(
            capability.accepts(width, height),
            Ok(()),
            "{width}x{height}"
        );
        assert_eq!(capability.coded(width, height), (width, height));
    }
    assert!(capability.accepts(1919, 1080).is_err());
    assert!(capability.accepts(4098, 2160).is_err());
}

#[test]
fn input_policies_parse_and_classify() {
    use crate::config::VideoInputPolicy;
    for policy in [
        VideoInputPolicy::Auto,
        VideoInputPolicy::GpuRequired,
        VideoInputPolicy::CpuReadback,
    ] {
        assert_eq!(VideoInputPolicy::parse(policy.as_str()), Ok(policy));
    }
    assert!(VideoInputPolicy::parse("gpu").is_err());
    let error = HostError::classify(format!(
        "{}GPU-resident output is required but unavailable: no device",
        crate::protocol::gpu_output_unavailable_tag()
    ));
    assert_eq!(error.code, "GPU_OUTPUT_UNAVAILABLE");
    assert!(!error.message.contains("[alpha2:"), "{}", error.message);
}

/// What `inputPolicy` and a pinned encoder decide, on a device that can
/// encode from the GPU.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn the_input_policy_and_a_pinned_encoder_choose_the_output_path() {
    use crate::config::VideoInputPolicy;
    let _gpu = gpu_test_lock();
    let Some((mut host, capability, _)) = gpu_output_host(VideoDecodePolicy::Auto) else {
        return;
    };
    let encoder = capability.encoder;
    let directory = scratch_directory("input-policy");
    let end = alpha2_time::TICKS_PER_SECOND / 3;
    let project = json!({
        "schema_version": 1, "id": "input-policy", "name": "Input policy",
        "settings": {"width": 640, "height": 360, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let assets = HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]);
    let export = |host: &mut Host,
                  name: &str,
                  codec: NativeVideoCodec,
                  pinned: Option<&str>,
                  policy: VideoInputPolicy| {
        host.input_policy = policy;
        let result = host.export(
            &project.to_string(),
            &assets,
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            false,
            crate::protocol::ResolvedFormat {
                container: alpha2_media::Container::Mp4,
                video: codec,
                audio: NativeAudioCodec::Aac,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
            pinned,
            VideoQualityRequest::Level(QualityLevel::High),
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            directory.join(name),
            &mut |_| Ok(()),
        );
        host.input_policy = VideoInputPolicy::Auto;
        result
    };
    let required_error = |result: Result<Value, String>| {
        let error = HostError::classify(result.expect_err("the export must refuse"));
        assert_eq!(error.code, "GPU_OUTPUT_UNAVAILABLE", "{}", error.message);
        error.message
    };

    for (name, policy) in [
        ("pinned-auto.mp4", VideoInputPolicy::Auto),
        ("pinned-required.mp4", VideoInputPolicy::GpuRequired),
    ] {
        let result = export(
            &mut host,
            name,
            NativeVideoCodec::H264,
            Some(encoder),
            policy,
        )
        .expect("pinned export");
        assert_eq!(result["videoPipeline"], json!("gpu-resident"), "{result}");
        assert_eq!(result["videoEncoder"], json!(encoder));
        assert_eq!(result["inputPolicy"], json!(policy.as_str()));
    }

    let pinned_readback = export(
        &mut host,
        "pinned-readback.mp4",
        NativeVideoCodec::H264,
        Some(encoder),
        VideoInputPolicy::CpuReadback,
    );
    if capability.gpu_input_only {
        let message = required_error(pinned_readback);
        assert!(message.contains("takes GPU frames only"), "{message}");
    } else {
        let result = pinned_readback.expect("the pinned encoder on the readback path");
        assert_eq!(result["videoPipeline"], json!("readback"), "{result}");
        assert_eq!(result["videoEncoder"], json!(encoder), "{result}");
        assert!(
            result["gpuOutputPathUnavailable"]
                .as_str()
                .is_some_and(|reason| reason == "inputPolicy is cpu-readback"),
            "{result}"
        );
    }

    let message = required_error(export(
        &mut host,
        "mpeg4-required.mp4",
        NativeVideoCodec::Mpeg4Part2,
        None,
        VideoInputPolicy::GpuRequired,
    ));
    assert!(message.contains("not mpeg4-part2"), "{message}");
    let readback = export(
        &mut host,
        "mpeg4-auto.mp4",
        NativeVideoCodec::Mpeg4Part2,
        None,
        VideoInputPolicy::Auto,
    )
    .expect("readback export");
    assert_eq!(readback["videoPipeline"], json!("readback"));
    assert!(
        readback["gpuOutputPathUnavailable"]
            .as_str()
            .is_some_and(|reason| reason.contains("not mpeg4-part2")),
        "{readback}"
    );

    // The pool's real allocation and the host's leases are reported apart; the handoff keeps up to
    // one surface per ring slot on top.
    let result = export(
        &mut host,
        "counted.mp4",
        NativeVideoCodec::H264,
        None,
        VideoInputPolicy::Auto,
    )
    .expect("counted export");
    let performance = &result["performance"];
    let budget = &performance["gpuEncodeBudget"];
    let leases = performance["encoderSurfaceLeasePeak"]
        .as_u64()
        .expect("lease peak");
    let allocated = performance["encoderFramesAllocated"]
        .as_u64()
        .expect("allocated");
    assert!(leases >= 1 && leases <= budget["leaseBound"].as_u64().expect("bound"));
    assert!(allocated >= leases, "{performance}");
    assert!(allocated <= budget["surfaceCeiling"].as_u64().expect("ceiling"));
    assert_eq!(performance["encoderSurfaceAcquires"], json!(10));
    let _ = std::fs::remove_dir_all(directory);
}

/// `capabilities.encode.gpuOutputPath.codecs` lists each codec the path
/// takes, the first being the one the report leads with, and each one an
/// export of it would be offered; an unavailable path lists none.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[test]
fn the_capability_lists_each_codec_the_path_takes() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Auto).expect("host");
    let path = host.capability_report().encode["gpuOutputPath"].clone();
    let codecs = path["codecs"].as_array().expect("codecs").clone();
    if path["available"] == json!(false) {
        assert_eq!(codecs, Vec::<Value>::new(), "{path}");
        assert!(
            path["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "{path}"
        );
        eprintln!(
            "the GPU-resident path is unavailable here: {}",
            path["reason"]
        );
    } else {
        assert!(!codecs.is_empty(), "{path}");
        assert_eq!(codecs[0]["videoCodec"], path["videoCodec"], "{path}");
        assert_eq!(codecs[0]["encoder"], path["encoder"], "{path}");
        assert_eq!(codecs[0]["minimumWidth"], path["minimumWidth"], "{path}");
        assert_eq!(
            codecs[0]["rateControlModes"], path["rateControlModes"],
            "{path}"
        );
        if !cfg!(target_os = "windows") {
            assert_eq!(codecs.len(), 1, "{path}");
        }
    }
    // Listed exactly when an export of the codec would be offered the path, as an export decides
    // it.
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
    ] {
        let listed = codecs
            .iter()
            .find(|entry| entry["videoCodec"] == json!(codec.as_str()));
        let offered = host
            .gpu_output_candidate(codec, None)
            .and_then(|candidate| {
                host.gpu_output_capability()
                    .map(|_| candidate)
                    .map_err(crate::gpu_output::GpuOutputRefusal::unavailable)
            });
        match offered {
            Ok((_, capability)) => {
                let listed =
                    listed.unwrap_or_else(|| panic!("{} is not listed: {path}", codec.as_str()));
                assert_eq!(listed["encoder"], json!(capability.encoder));
                assert_eq!(listed["maximumWidth"], json!(capability.maximum.0));
                assert_eq!(listed["widthAlignment"], json!(capability.granularity.0));
            }
            Err(refusal) => {
                assert!(
                    listed.is_none(),
                    "{} is listed but refused: {}",
                    codec.as_str(),
                    refusal.reason
                );
            }
        }
    }
}

/// A request for a short H.264 export of the parity clip into `directory`, under `policy`.
#[cfg(feature = "hwencode")]
fn rejection_request(directory: &std::path::Path, policy: &str) -> Value {
    let mut request = json!({
        "protocolVersion": 1, "requestId": "rejected", "operation": "export",
        "project": conform_project(&json!({})),
        "assets": {"video": corpus("parity-rgb.mov")},
        "startTicks": 0, "endTicks": 120_000,
        "frameRate": {"num": 12, "den": 1},
        "videoCodec": "h264",
        "inputPolicy": policy,
        "output": directory.join("out.mp4")
    });
    request["project"]["settings"]["width"] = json!(640);
    request["project"]["settings"]["height"] = json!(360);
    request
}

/// A frame the GPU-resident encoder rejects on a device still usable starts the export over on
/// the readback path, once, and the result says where and why.
#[cfg(feature = "hwencode")]
#[test]
fn a_rejected_gpu_frame_restarts_the_export_on_the_readback_path() {
    let _gpu = gpu_test_lock();
    let Some((mut host, _, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("rejected-restart");
    let request = rejection_request(&directory, "auto");
    let _rejecting = crate::encode::reject_gpu_frame(5);
    let mut events = Vec::new();
    let result = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |progress| {
                events.push(progress);
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("{}: {}", error.code, error.message));
    assert_eq!(result["videoPipeline"], "readback", "{result}");
    let fallback = &result["gpuOutputPathFallback"];
    assert_eq!(fallback["frame"], 5, "{result}");
    assert!(
        fallback["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("injected rejected frame 5")),
        "{result}"
    );
    assert!(
        result["gpuOutputPathUnavailable"]
            .as_str()
            .is_some_and(|reason| reason.contains("restarted on the readback path")),
        "{result}"
    );
    let restarts = events.iter().filter(|event| event.restarted).count();
    assert_eq!(restarts, 1, "{events:?}");
    let last = events.last().expect("progress");
    assert_eq!(last.completed, last.total, "{events:?}");
    assert_eq!(decoded_luma(&directory.join("out.mp4")).len(), 12);
    std::fs::remove_dir_all(&directory).ok();
}

/// `inputPolicy: gpu-required` takes no other path: a rejected frame fails the export, naming the
/// encoder's reason, and leaves no output.
#[cfg(feature = "hwencode")]
#[test]
fn gpu_required_does_not_restart_after_a_rejected_frame() {
    let _gpu = gpu_test_lock();
    let Some((mut host, _, _)) = gpu_output_host(VideoDecodePolicy::Software) else {
        return;
    };
    let directory = scratch_directory("rejected-required");
    let request = rejection_request(&directory, "gpu-required");
    let _rejecting = crate::encode::reject_gpu_frame(5);
    let error = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |progress| {
                assert!(!progress.restarted);
                Ok(())
            },
        )
        .expect_err("a rejected frame fails a gpu-required export");
    assert_eq!(error.code, "ENCODE_ERROR", "{}", error.message);
    assert!(
        error.message.contains("injected rejected frame 5")
            && error
                .message
                .contains("was not restarted on the readback path"),
        "{}",
        error.message
    );
    assert!(!directory.join("out.mp4").exists());
    std::fs::remove_dir_all(&directory).ok();
}
