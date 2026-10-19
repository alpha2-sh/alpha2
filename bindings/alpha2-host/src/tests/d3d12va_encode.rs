//! D3D12VA encode on Windows: the encode device, capabilities, and refusals.

use super::*;

/// A solid colour at 256x144 for `frames` frames: an export that needs no
/// fixture and decodes nothing.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn fill_project(frames: i64) -> Value {
    let end = frames * alpha2_time::TICKS_PER_SECOND / 30;
    json!({
        "schema_version": 1, "id": "fill", "name": "Fill",
        "settings": {"width": 256, "height": 144, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "fill", "kind": "generated",
                     "generator": {"type": "fill", "paint": {"type": "solid",
                         "color": {"red": 0.2, "green": 0.4, "blue": 0.6, "alpha": 1}}}}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "fill",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    })
}

/// Exports [`fill_project`] in `codec`, perhaps pinned to `pinned`, under
/// the input policy `policy`, to `output`.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
fn export_fill(
    host: &mut Host,
    codec: NativeVideoCodec,
    pinned: Option<&str>,
    policy: crate::config::VideoInputPolicy,
    output: &Path,
) -> Result<Value, String> {
    let frames = 6;
    host.input_policy = policy;
    let result = host.export(
        &fill_project(frames).to_string(),
        &HashMap::new(),
        &HashMap::new(),
        Vec::new(),
        0,
        frames * alpha2_time::TICKS_PER_SECOND / 30,
        RateRequest { num: 30, den: 1 },
        false,
        codec,
        pinned,
        VideoQualityRequest::Level(QualityLevel::High),
        NativeAudioCodec::Aac,
        AudioQualityRequest::Level(QualityLevel::High),
        None,
        output,
        &mut |_| Ok(()),
    );
    host.input_policy = crate::config::VideoInputPolicy::Auto;
    result
}

/// What the host's D3D12VA test-encode runner starts in a test binary, which
/// has no `probe-d3d12va-encode` subcommand: the subcommand, with the
/// arguments in [`crate::d3d12va_probe::TEST_CHILD_ARGUMENTS`].
#[cfg(all(target_os = "windows", feature = "hwencode"))]
#[test]
#[ignore = "the D3D12VA test encode's child process, which the host's runner starts"]
fn d3d12va_test_encode_child() {
    let Ok(arguments) = std::env::var(crate::d3d12va_probe::TEST_CHILD_ARGUMENTS) else {
        return;
    };
    crate::d3d12va_probe::child_test_main(&arguments);
}

/// Under `software` the shared D3D12 device still exists, for the
/// GPU-resident export, and nothing decodes on it: not the readiness the
/// report and the failure notes give, not the frame supply, not the decode
/// pool.
#[cfg(all(target_os = "windows", feature = "hwencode", feature = "hwdecode"))]
#[test]
fn software_decode_does_not_decode_on_the_encode_device() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    if host.shared_d3d12.is_none() {
        // The adapter wgpu prefers makes no shared device here (an NVIDIA MX450 does not), and
        // `software` keeps that adapter. The device `auto` decodes on is the same kind of device,
        // and switching the host's mode is what makes it one only encode wants.
        eprintln!(
            "the software-decode host has no shared device ({}); using the auto host's",
            host.encode_adapters.no_device.clone().unwrap_or_default()
        );
        host = create_host(VideoDecodePolicy::Auto).expect("host");
        host.decode_policy = VideoDecodePolicy::Software;
    }
    let Some(shared) = host.shared_d3d12.clone() else {
        eprintln!("skipped: no adapter here makes a shared D3D12 device");
        return;
    };
    assert!(host.hardware_encode_device().is_some());
    assert!(host.decode_d3d12().is_none());
    assert!(!host.hardware_decode_ready());
    let report = host.capability_report();
    assert_eq!(
        report.decode["deviceReady"],
        json!(false),
        "{}",
        report.decode
    );
    assert_eq!(report.decode["mode"], json!("software"));
    let directory = scratch_directory("software-encode-device");
    let end = alpha2_time::TICKS_PER_SECOND / 3;
    let project = json!({
        "schema_version": 1, "id": "software-device", "name": "Software decode on the encode device",
        "settings": {"width": 256, "height": 144, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "fixture://video", "kind": "video"}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": 0, "end": end},
            "source_range": {"start": 0, "end": end}
        }]}]}
    });
    let result = host
        .export(
            &project.to_string(),
            &HashMap::from([("video".to_owned(), corpus("h264-long-gop.mp4"))]),
            &HashMap::new(),
            Vec::new(),
            0,
            end,
            RateRequest { num: 30, den: 1 },
            false,
            NativeVideoCodec::H264,
            None,
            VideoQualityRequest::Level(QualityLevel::High),
            NativeAudioCodec::Aac,
            AudioQualityRequest::Level(QualityLevel::High),
            None,
            &directory.join("out.mp4"),
            &mut |_| Ok(()),
        )
        .expect("export");
    let performance = &result["performance"];
    assert_eq!(result["hardwareDecodeReady"], json!(false), "{result}");
    assert_eq!(
        performance["hardwareDecodedFrames"],
        json!(0),
        "{performance}"
    );
    assert!(
        performance["softwareDecodedFrames"].as_u64() > Some(0),
        "{performance}"
    );
    host.decode_policy = VideoDecodePolicy::Auto;
    assert_eq!(host.decode_d3d12().is_some(), shared.decode_planes());
}

/// The D3D12VA test encode runs in a child process of the host, and its
/// answer is the one the capability gives, for every codec the renderer's
/// device says it encodes, on whatever adapter this machine renders on.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
#[test]
fn the_d3d12va_test_encode_runs_in_a_child_process() {
    let _gpu = gpu_test_lock();
    let host = create_host(VideoDecodePolicy::Auto).expect("host");
    let Some(shared) = host.shared_d3d12.clone() else {
        eprintln!(
            "skipped: no shared D3D12 device here ({})",
            host.encode_adapters.no_device.clone().unwrap_or_default()
        );
        return;
    };
    let luid = shared.adapter().expect("a DXGI adapter").luid;
    for codec in crate::adapter_choice::ENCODE_CODECS {
        if let Err(reason) = crate::d3d12va_probe::device_encodes(&shared, codec) {
            assert!(
                crate::d3d12va_probe::answer(luid, codec).is_none(),
                "{reason}"
            );
            continue;
        }
        let answer = crate::d3d12va_probe::answer(luid, codec)
            .unwrap_or_else(|| panic!("no child answered for {}", codec.as_str()));
        let capability = host.gpu_output_candidate(codec, None);
        match &answer {
            Ok(probe) => {
                assert_eq!(probe.codec, codec);
                match probe.first_packet_decodes() {
                    Ok(()) => assert!(capability.is_ok(), "{probe:?}"),
                    Err(reason) => {
                        let refusal = capability.expect_err("an undecodable first packet");
                        assert!(refusal.reason.contains(&reason), "{}", refusal.reason);
                    }
                }
            }
            Err(reason) => {
                let refusal = capability.expect_err("a test encode that did not answer");
                assert!(
                    refusal.reason.contains(reason.as_str()),
                    "{}",
                    refusal.reason
                );
            }
        }
        eprintln!("{}: {answer:?}", codec.as_str());
    }
}

/// Each step that can refuse the D3D12VA path, forced on whatever adapter
/// this machine renders on: an `auto` export falls back to the readback path
/// with that step's reason, which the capability report gives too, while
/// `gpu-required` and a pinned `*_d3d12va` encoder refuse with
/// `GPU_OUTPUT_UNAVAILABLE` before any output exists, and the host's start
/// is bounded by the test encode's timeout.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
#[test]
#[allow(clippy::too_many_lines)]
fn every_d3d12va_refusal_falls_back_with_its_reason() {
    use crate::config::VideoInputPolicy;
    use crate::d3d12va_probe::InjectedRefusal;
    let _gpu = gpu_test_lock();
    let started = Instant::now();
    let host = create_host(VideoDecodePolicy::Auto).expect("host");
    let unforced_start = started.elapsed();
    let Some(shared) = host.shared_d3d12.clone() else {
        eprintln!(
            "skipped: no shared D3D12 device here ({})",
            host.encode_adapters.no_device.clone().unwrap_or_default()
        );
        return;
    };
    drop(host);
    let adapter = shared.gpu().capabilities().adapter_name.clone();
    drop(shared);
    let timeout = Duration::from_secs(4);
    let directory = scratch_directory("d3d12va-refusals");
    for (refusal, reason) in [
        (
            InjectedRefusal::NoEncoder,
            "this FFmpeg runtime has no h264_d3d12va encoder".to_owned(),
        ),
        (
            InjectedRefusal::NoDeviceEncoder,
            format!("{adapter} reports no D3D12 video encoder for h264"),
        ),
        (
            InjectedRefusal::NoVideoDevice,
            format!("{adapter}: ID3D12VideoDevice3 unavailable"),
        ),
        (
            InjectedRefusal::OpenFails,
            format!("h264_d3d12va did not open and encode on {adapter}: Encoding failed"),
        ),
        (
            InjectedRefusal::Timeout,
            format!(
                "h264_d3d12va did not open and encode on {adapter}: the D3D12VA test encode did \
                 not finish within {timeout:?} and was killed"
            ),
        ),
        (
            InjectedRefusal::Crash,
            format!(
                "h264_d3d12va did not open and encode on {adapter}: the D3D12VA test encode \
                 crashed (exit 0xC0000409)"
            ),
        ),
    ] {
        let case = format!("{refusal:?}");
        let _forced = crate::d3d12va_probe::inject(refusal, timeout);
        let started = Instant::now();
        let mut host = create_host(VideoDecodePolicy::Auto).expect("host");
        let start = started.elapsed();
        assert!(
            start <= unforced_start + timeout + Duration::from_secs(5),
            "{case}: the start took {start:?}, against {unforced_start:?} unforced"
        );

        let path = host.capability_report().encode["gpuOutputPath"].clone();
        assert_eq!(path["available"], json!(false), "{case}: {path}");
        assert_eq!(path["codecs"], json!([]), "{case}: {path}");
        assert!(
            path["reason"]
                .as_str()
                .is_some_and(|text| text.contains(&reason)),
            "{case}: {path}"
        );

        let output = directory.join(format!("{case}-auto.mp4"));
        let result = export_fill(
            &mut host,
            NativeVideoCodec::H264,
            None,
            VideoInputPolicy::Auto,
            &output,
        )
        .unwrap_or_else(|error| panic!("{case}: the auto export failed: {error}"));
        assert_eq!(
            result["videoOutputPath"],
            json!("readback"),
            "{case}: {result}"
        );
        assert!(
            result["gpuOutputPathUnavailable"]
                .as_str()
                .is_some_and(|text| text.contains(&reason)),
            "{case}: {result}"
        );
        assert!(output.exists(), "{case}");

        for (name, pinned, policy) in [
            ("required", None, VideoInputPolicy::GpuRequired),
            ("pinned", Some("h264_d3d12va"), VideoInputPolicy::Auto),
        ] {
            let output = directory.join(format!("{case}-{name}.mp4"));
            let error = HostError::classify(
                export_fill(&mut host, NativeVideoCodec::H264, pinned, policy, &output)
                    .expect_err("the GPU path is required"),
            );
            assert_eq!(
                error.code, "GPU_OUTPUT_UNAVAILABLE",
                "{case} {name}: {}",
                error.message
            );
            assert!(
                error.message.contains(&reason),
                "{case} {name}: {}",
                error.message
            );
            let prefix = format!("{case}-{name}");
            let written: Vec<_> = std::fs::read_dir(&directory)
                .expect("directory")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|written| written.starts_with(&prefix))
                .collect();
            assert!(written.is_empty(), "{case} {name}: {written:?}");
        }
    }
}

/// Each codec an export asks for gets the D3D12VA encoder for that codec or
/// a refusal that names it: a device decides per codec, and a
/// `gpu-required` export of one codec is never served by another's encoder.
#[cfg(all(target_os = "windows", feature = "hwencode"))]
#[test]
fn each_codec_takes_the_gpu_path_or_says_why_not() {
    use crate::config::VideoInputPolicy;
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Auto).expect("host");
    let directory = scratch_directory("d3d12va-per-codec");
    for codec in crate::adapter_choice::ENCODE_CODECS {
        let output = directory.join(format!("{}.mp4", codec.as_str()));
        let encoder = format!("{}_d3d12va", codec.as_str());
        match export_fill(
            &mut host,
            codec,
            None,
            VideoInputPolicy::GpuRequired,
            &output,
        ) {
            Ok(result) => {
                assert_eq!(result["videoOutputPath"], json!("gpu-resident"), "{result}");
                assert_eq!(result["videoEncoder"], json!(encoder), "{result}");
                assert_eq!(result["videoCodec"], json!(codec.as_str()), "{result}");
            }
            Err(error) => {
                let error = HostError::classify(error);
                assert_eq!(error.code, "GPU_OUTPUT_UNAVAILABLE", "{}", error.message);
                assert!(
                    error.message.contains(&encoder)
                        || error.message.contains(&format!("for {}", codec.as_str()))
                        || error.message.contains("shared with a hardware encoder")
                        || error.message.contains("ALPHA2_GPU_ENCODE is off"),
                    "{}",
                    error.message
                );
                assert!(!output.exists());
                eprintln!("{}: {}", codec.as_str(), error.message);
            }
        }
    }
}
