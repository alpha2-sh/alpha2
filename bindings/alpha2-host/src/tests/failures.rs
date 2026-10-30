//! How a failure is classified, which failure an export reports first, and the damage its decoders skipped.

use super::*;

/// A native interop helper's fence wait that timed out.
#[cfg(feature = "hwdevice")]
const fn bridge_timeout() -> alpha2_ffmpeg_bridge::BridgeError {
    alpha2_ffmpeg_bridge::BridgeError::NativeGpu(alpha2_ffmpeg_bridge::NativeGpuFailure {
        backend: alpha2_ffmpeg_bridge::NativeGpuBackend::Vulkan,
        operation: "encode copy slot",
        step: "fence wait",
        status: 2,
        fault: Some(alpha2_media::GpuFault::Timeout),
    })
}

#[test]
fn a_gpu_wait_timeout_is_its_own_error_code() {
    // The renderer's and the interop helpers' timeouts carry one message, so a caller can tell a
    // hung GPU from a generic failure and restart it.
    assert_eq!(
        HostError::classify(error_message(alpha2_render::RenderError::GpuTimeout)).code,
        "GPU_TIMEOUT"
    );
    #[cfg(feature = "hwdevice")]
    assert_eq!(
        HostError::classify(format!(
            "native export failed: {}",
            error_message(bridge_timeout())
        ))
        .code,
        "GPU_TIMEOUT"
    );
}

#[cfg(feature = "hwdevice")]
#[test]
fn a_native_gpu_failure_is_classified_by_what_its_status_said() {
    // A helper's VkResult, HRESULT, or CVReturn is classified by the fault it names, not reported
    // as "FFmpeg error code N".
    use crate::failure::{ExportPhase, classify_failure, tag_phase};
    use alpha2_ffmpeg_bridge::{BridgeError, NativeGpuBackend, NativeGpuFailure};
    use alpha2_media::GpuFault;
    let failure = |backend, status, fault| NativeGpuFailure {
        backend,
        operation: "encode copy",
        step: "queue submit",
        status,
        fault,
    };
    let report = |message: String, phase| {
        let error = HostError::classify(message);
        let class = classify_failure(
            error.code,
            &error.message,
            error.evidence,
            phase,
            &alpha2_gpu::GpuDiagnosticSnapshot::default(),
        );
        assert!(!error.message.contains("[alpha2:"), "{}", error.message);
        assert!(!error.message.contains("FFmpeg"), "{}", error.message);
        (error.code, class.code, class.resource)
    };
    let lost = failure(NativeGpuBackend::Vulkan, -4, Some(GpuFault::DeviceLost));
    let device_memory = failure(
        NativeGpuBackend::D3D12,
        0x8007_000E_u32.cast_signed(),
        Some(GpuFault::DeviceMemory),
    );
    let host_memory = failure(NativeGpuBackend::Vulkan, -1, Some(GpuFault::HostMemory));
    let timeout = failure(NativeGpuBackend::Metal, 0, Some(GpuFault::Timeout));
    let unnamed = failure(NativeGpuBackend::Vulkan, -3, None);
    let encode = |failure: NativeGpuFailure| alpha2_media::EncodeError::Gpu {
        fault: failure.fault.expect("a fault"),
        message: failure.to_string(),
    };

    assert_eq!(
        report(
            error_message(BridgeError::NativeGpu(lost)),
            ExportPhase::Upload
        ),
        ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")
    );
    for (failure, expected) in [
        (lost, ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")),
        (device_memory, ("ALPHA2_ERROR", "OUT_OF_MEMORY", "gpu")),
        (host_memory, ("ALPHA2_ERROR", "ALLOCATION_FAILED", "cpu")),
        (timeout, ("GPU_TIMEOUT", "GPU_TIMEOUT", "gpu")),
    ] {
        let message = tag_phase(
            ExportPhase::Encode,
            Some(3),
            format!("native encode failed: {}", error_message(encode(failure))),
        );
        assert_eq!(report(message, ExportPhase::Encode), expected, "{failure}");
    }
    let decode = alpha2_media::DecodeError::Gpu {
        fault: GpuFault::DeviceMemory,
        message: device_memory.to_string(),
    };
    assert_eq!(
        report(error_message(decode), ExportPhase::Decode),
        ("ALPHA2_ERROR", "OUT_OF_MEMORY", "gpu")
    );
    let render = alpha2_render::RenderError::ExternalTextureSync(Some(GpuFault::DeviceLost));
    assert_eq!(
        report(
            error_message(alpha2_engine::EngineError::Render(render)),
            ExportPhase::Render
        ),
        ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")
    );
    let render = alpha2_render::RenderError::ExternalTextureSync(Some(GpuFault::DeviceMemory));
    assert_eq!(
        report(error_message(render), ExportPhase::Render),
        ("ALPHA2_ERROR", "OUT_OF_MEMORY", "gpu")
    );
    // A status that names no fault is a failure, not evidence.
    assert_eq!(
        report(
            error_message(BridgeError::NativeGpu(unnamed)),
            ExportPhase::Upload
        ),
        ("ALPHA2_ERROR", "UNKNOWN", "unknown")
    );
    let first = crate::failure::evidence_tag(
        "ALLOCATION_FAILED",
        crate::failure::evidence_tag("OUT_OF_MEMORY", "x".to_owned()),
    );
    assert_eq!(HostError::classify(first).evidence, Some("OUT_OF_MEMORY"));
}

#[test]
fn dropped_gpu_work_is_a_lost_device() {
    // The renderer found a readback the GPU completed without running; the caller must restart the
    // host, as for a lost device.
    let render = alpha2_render::RenderError::GpuWorkDropped;
    for message in [
        error_message(render),
        error_message(alpha2_engine::EngineError::Render(render)),
        format!("native export failed: {}", error_message(render)),
    ] {
        assert_eq!(HostError::classify(message).code, "GPU_DEVICE_LOST");
    }
}

#[test]
fn a_frame_the_gpu_rejected_is_classified_by_what_the_gpu_said_and_not_as_a_lost_device() {
    // A missing canary must not read as a lost device ahead of the device's own OOM and validation
    // reports, which would retry a permanent validation failure on a "new device".
    use crate::failure::{ExportPhase, classify_failure, tag_phase};
    use alpha2_gpu::{GpuDiagnostic, GpuDiagnosticLog, GpuErrorKind};
    let report = |message: String, phase, log: &GpuDiagnosticLog| {
        let error = HostError::classify(message);
        assert!(!error.message.contains("[alpha2:"), "{}", error.message);
        let class = classify_failure(
            error.code,
            &error.message,
            error.evidence,
            phase,
            &log.snapshot(),
        );
        (error.code, class.code, class.resource)
    };
    let empty = GpuDiagnosticLog::default();
    // The frame's own scope caught the error, so its evidence travels with it, not via the device
    // log.
    for (kind, expected) in [
        (
            GpuErrorKind::OutOfMemory,
            ("ALPHA2_ERROR", "OUT_OF_MEMORY", "gpu"),
        ),
        (
            GpuErrorKind::Validation,
            ("ALPHA2_ERROR", "VALIDATION_FAILED", "gpu"),
        ),
        (
            GpuErrorKind::Internal,
            ("ALPHA2_ERROR", "UNKNOWN", "unknown"),
        ),
    ] {
        let rejected = alpha2_render::RenderError::Gpu { kind };
        for (message, phase) in [
            (error_message(rejected), ExportPhase::Render),
            (
                error_message(alpha2_engine::EngineError::Render(rejected)),
                ExportPhase::Readback,
            ),
            (
                tag_phase(
                    ExportPhase::Encode,
                    Some(7),
                    format!("native export failed: {}", error_message(rejected)),
                ),
                ExportPhase::Encode,
            ),
        ] {
            assert_eq!(report(message, phase, &empty), expected, "{kind:?}");
        }
    }

    // A canary missing where the device reported why: the device's report is the class.
    let dropped = || error_message(alpha2_render::RenderError::GpuWorkDropped);
    for (kind, class) in [
        (GpuErrorKind::Validation, "VALIDATION_FAILED"),
        (GpuErrorKind::OutOfMemory, "OUT_OF_MEMORY"),
    ] {
        let log = GpuDiagnosticLog::default();
        log.push(GpuDiagnostic::UncapturedError {
            kind,
            message: "rejected submission".to_owned(),
        });
        for phase in [ExportPhase::Readback, ExportPhase::Encode] {
            assert_eq!(
                report(dropped(), phase, &log),
                ("GPU_DEVICE_LOST", class, "gpu"),
                "{kind:?} {phase:?}"
            );
        }
        log.push(GpuDiagnostic::DeviceLost {
            reason: "Unknown".to_owned(),
            message: "reset".to_owned(),
        });
        assert_eq!(
            report(dropped(), ExportPhase::Readback, &log),
            ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")
        );
    }
    assert_eq!(
        report(dropped(), ExportPhase::Readback, &empty),
        ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")
    );
    let lost = GpuDiagnosticLog::default();
    lost.push(GpuDiagnostic::UncapturedError {
        kind: GpuErrorKind::Validation,
        message: "earlier".to_owned(),
    });
    assert_eq!(
        report(
            error_message(alpha2_render::RenderError::GpuDeviceLost),
            ExportPhase::Render,
            &lost
        ),
        ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")
    );
}

/// The wait for the render that packs an encoder surface is a typed timeout,
/// and the encode worker boundary keeps every typed GPU fault's code: the stall
/// is `GPU_TIMEOUT` on macOS as on Linux, not an adapter failure in its own
/// words passed on as text (`ENCODE_ERROR`, failure `UNKNOWN`).
#[test]
fn an_encode_input_wait_that_timed_out_is_a_gpu_timeout_on_every_backend() {
    use crate::encode::encode_failure;
    use crate::failure::{ExportPhase, classify_failure, tag_phase};
    use alpha2_media::{EncodeError, GpuFault};
    let report = |error: String| {
        let error = HostError::classify(tag_phase(ExportPhase::Encode, Some(3), error));
        let class = classify_failure(
            error.code,
            &error.message,
            error.evidence,
            ExportPhase::Encode,
            &alpha2_gpu::GpuDiagnosticSnapshot::default(),
        );
        assert!(!error.message.contains("[alpha2:"), "{}", error.message);
        (error.code, class.code, class.resource)
    };
    let surface_wait = EncodeError::gpu_timeout("the render packing an encoder surface, after 30s");
    assert!(
        surface_wait
            .to_string()
            .contains(alpha2_render::GPU_TIMEOUT_MESSAGE)
    );
    assert_eq!(
        report(encode_failure("writing media", surface_wait)),
        ("GPU_TIMEOUT", "GPU_TIMEOUT", "gpu")
    );
    // A typed fault keeps its code across the worker boundary whatever its words say.
    let lost = EncodeError::Gpu {
        fault: GpuFault::DeviceLost,
        message: "status -4".to_owned(),
    };
    assert_eq!(
        report(encode_failure("finalizing output", lost)),
        ("GPU_DEVICE_LOST", "DEVICE_LOST", "gpu")
    );
    assert_eq!(
        HostError::classify(encode_failure(
            "writing media",
            EncodeError::Adapter("external error".to_owned())
        ))
        .code,
        "ENCODE_ERROR"
    );
}

#[test]
fn gpu_timeouts_survive_the_production_error_conversion_and_context() {
    let render = alpha2_render::RenderError::GpuTimeout;
    for message in [
        error_message(render),
        error_message(alpha2_engine::EngineError::Render(render)),
        format!("native export failed: {}", error_message(render)),
    ] {
        let error = HostError::classify(message);
        assert_eq!(error.code, "GPU_TIMEOUT");
        assert!(error.message.contains(alpha2_render::GPU_TIMEOUT_MESSAGE));
    }
    #[cfg(feature = "hwdevice")]
    assert_eq!(
        HostError::classify(error_message(bridge_timeout())).code,
        "GPU_TIMEOUT"
    );
    assert_eq!(
        HostError::classify(error_message(std::io::Error::other("GpuTimeout"))).code,
        "ALPHA2_ERROR"
    );
}

#[test]
fn effect_errors_take_priority_over_generic_missing_input_errors() {
    assert_eq!(
        HostError::classify(error_message(
            alpha2_render::RenderError::MissingEffectInput
        ))
        .code,
        "EFFECT_ERROR"
    );
    assert_eq!(
        HostError::classify(crate::protocol::missing_input("missing uploaded asset")).code,
        "MISSING_INPUT"
    );
}

#[test]
fn contextualized_native_encoder_errors_are_classified_as_encode_errors() {
    assert_eq!(
        HostError::classify(crate::encode::encode_failure(
            "writing media",
            alpha2_media::EncodeError::Adapter("external error".to_owned()),
        ))
        .code,
        "ENCODE_ERROR"
    );
}

/// Text a caller chose (a path, a source ID) that spells the words of another
/// code's message: an engine message, a cancel, another export's output, a GPU
/// wait.
const WORDS_OF_OTHER_CODES: [&str; 13] = [
    "GPU device lost",
    "shader",
    "MissingEffectInput",
    "native encode failed",
    "mux",
    "missing uploaded asset",
    "invalid",
    "out of range",
    "must",
    "exceeds",
    "native export was cancelled",
    alpha2_native::PARTIAL_OUTPUT_IN_USE,
    alpha2_render::GPU_TIMEOUT_MESSAGE,
];

#[test]
fn a_code_is_never_read_from_the_words_of_a_message() {
    // The host reads no code from an untagged message's words, wherever a caller's path puts them.
    for words in WORDS_OF_OTHER_CODES {
        for message in [
            words.to_owned(),
            format!("{words}.mov: No such file or directory (os error 2)"),
        ] {
            let error = HostError::classify(message.clone());
            assert_eq!(error.code, "ALPHA2_ERROR", "{message}");
            assert_eq!(error.message, message);
            let tagged = crate::protocol::code_tag("IO_ERROR", message.clone());
            assert_eq!(HostError::classify(tagged).code, "IO_ERROR", "{message}");
        }
    }
}

#[test]
fn a_kept_render_reaches_the_error_details_whatever_its_path_spells() {
    let directory = scratch_directory("kept-path");
    // A `] ` would end a tag that carried the path as it is.
    for name in ["out.a2kept.mp4", "a] b.a2kept.mp4"] {
        let kept = directory.join(name);
        std::fs::write(&kept, b"render").expect("kept file");
        let message = crate::protocol::output_publish_failed(
            Some(&kept),
            format!("cannot publish the output: denied; the finished render is kept at {name}"),
        );
        let error = HostError::classify(crate::encode::encode_error(format!(
            "native encode failed while finalizing output: {message}"
        )));
        assert_eq!(error.code, "OUTPUT_PUBLISH_FAILED", "{name}");
        let named = error.details.as_ref().expect("details")["keptPath"]
            .as_str()
            .expect("keptPath")
            .to_owned();
        assert!(names_written_file(Path::new(&named), &kept), "{named}");
        assert!(
            !error.message.contains(crate::tag::open()),
            "{}",
            error.message
        );
        assert!(error.message.ends_with(name), "{}", error.message);
    }
    let unkept = HostError::classify(crate::protocol::output_publish_failed(None, "denied"));
    assert_eq!(unkept.code, "OUTPUT_PUBLISH_FAILED");
    assert!(unkept.details.is_none());
}

#[test]
fn a_stall_resource_is_never_read_from_the_words_of_its_message() {
    // A software decoder's stall stays software even when the source ID spells a hardware stall's
    // words.
    for marker in [
        crate::protocol::HARDWARE_DECODE_STALL_MARKER,
        crate::protocol::HARDWARE_ENCODE_STALL_MARKER,
    ] {
        for tag in [
            crate::protocol::decode_stalled_tag(),
            crate::protocol::encode_stalled_tag(),
        ] {
            let message = format!(
                "{tag}decode shard 0 reading source {marker} (clip c) at 0.000 s has been inside \
                 one decoder operation for 50 ms, with only software sessions open"
            );
            let (_, class) = stall_report(message.clone());
            assert_eq!(
                (class.code, class.resource),
                ("GPU_TIMEOUT", "unknown"),
                "{message}"
            );
        }
    }
}

#[test]
fn an_engine_error_carries_the_code_its_variant_names() {
    // The code is the variant's, not a keyword's in the text.
    for (error, code) in [
        (
            alpha2_engine::EngineError::ExportTooLarge {
                frames: 20_000_000,
                maximum: 10_000_000,
            },
            "INVALID_INPUT",
        ),
        (
            alpha2_engine::EngineError::Text(alpha2_engine::TextError::Raster(
                alpha2_text::TextRasterError::NoFonts,
            )),
            "MISSING_INPUT",
        ),
        (
            alpha2_engine::EngineError::OddExportDimensions {
                width: 65,
                height: 49,
                suggested: (64, 48),
            },
            "INVALID_INPUT",
        ),
        (
            alpha2_engine::EngineError::Render(alpha2_render::RenderError::MissingEffectResource),
            "MISSING_INPUT",
        ),
    ] {
        let message = error.to_string();
        let classified = HostError::classify(error_message(error));
        assert_eq!(classified.code, code, "{message}");
        assert_eq!(classified.message, message, "the tag is not shown");
    }
}

#[test]
fn a_storage_failure_is_known_by_its_errno_and_by_either_spelling() {
    // macOS spells EDQUOT "Disc quota exceeded", glibc "Disk quota exceeded".
    let gpu = alpha2_gpu::GpuDiagnosticSnapshot::default();
    let classify = |message: &str, phase| {
        crate::failure::classify_failure("ENCODE_ERROR", message, None, phase, &gpu).code
    };
    for phase in [
        crate::failure::ExportPhase::Encode,
        crate::failure::ExportPhase::Mux,
    ] {
        for message in [
            "native encode failed while writing media: encoder adapter failed: AVERROR(-69): `Disc quota exceeded`",
            "native encode failed while finalizing output: encoder adapter failed: AVERROR(-122): `Disk quota exceeded`",
            "native encode failed while writing media: encoder adapter failed: AVERROR(-28): `No space left on device`",
        ] {
            assert_eq!(
                classify(message, phase),
                "STORAGE_QUOTA_EXCEEDED",
                "{message}"
            );
        }
        for errno in crate::failure::STORAGE_ERRNOS {
            let message = format!(
                "native encode failed while finalizing output: encoder adapter failed: AVERROR(-{errno}): `Plus de place`"
            );
            assert_eq!(
                classify(&message, phase),
                "STORAGE_QUOTA_EXCEEDED",
                "{message}"
            );
        }
    }
    assert_eq!(
        classify(
            "native encode failed while writing media: encoder adapter failed: AVERROR(-5): `Input/output error`",
            crate::failure::ExportPhase::Encode
        ),
        "UNKNOWN"
    );
}

#[test]
fn every_engine_error_code_is_a_protocol_code() {
    for code in alpha2_engine::ErrorCode::ALL {
        assert!(
            crate::protocol::PROTOCOL_CODES.contains(&code.as_str()),
            "{code:?}"
        );
    }
}

#[test]
fn an_export_error_code_names_the_failure_and_not_the_words_in_its_paths_and_ids() {
    // Missing fonts and assets are MISSING_INPUT, unknown encoders INVALID_INPUT and refused
    // overwrites IO_ERROR, not ENCODE_ERROR/UNKNOWN, which the retry policy runs again.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("error-codes");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let clip = corpus("parity-rgb.mov");
    let export = |output: &Path, extra: Value, resource: Option<&str>| {
        let mut project = conform_project(&json!({}));
        if let Some(id) = resource {
            project["effect_resources"] =
                json!([{"id": id, "kind": "lut3d", "locator": "asset://grade"}]);
        }
        let mut request = json!({
            "protocolVersion": 1, "requestId": "export", "operation": "export",
            "project": project,
            "assets": {"video": clip},
            "startTicks": 0, "endTicks": 30_000,
            "frameRate": {"num": 12, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": output
        });
        for (key, value) in extra.as_object().expect("fields") {
            request[key] = value.clone();
        }
        parse_request(&request.to_string()).expect("export request")
    };
    let mut refused = |request, code: &str, class: &str| {
        let error = host
            .handle(request, &mut |_| Ok(()))
            .expect_err("the export is refused");
        assert_eq!(error.code, code, "{}", error.message);
        assert!(!error.message.contains("[alpha2:"), "{}", error.message);
        let failure = error.failure.expect("an export failure report");
        assert_eq!(failure["code"], class, "{}: {failure}", error.message);
        error.message
    };
    let output = directory.join("out.mp4");
    for folder in [
        "fonts",
        "Effects",
        "Encodes",
        "device-lost",
        "missing",
        "invalid",
    ] {
        let font = directory.join(folder).join("Inter.ttf");
        let message = refused(
            export(&output, json!({"fonts": [font]}), None),
            "MISSING_INPUT",
            "INVALID_INPUT",
        );
        assert!(message.contains("Inter.ttf"), "{message}");
    }
    let not_a_font = directory.join("Encodes-lost-device.ttf");
    std::fs::write(&not_a_font, b"not a font").expect("write");
    refused(
        export(&output, json!({"fonts": [not_a_font]}), None),
        "INVALID_INPUT",
        "INVALID_INPUT",
    );
    for id in ["grade", "device-lost", "encode-shader"] {
        refused(
            export(&output, json!({}), Some(id)),
            "MISSING_INPUT",
            "INVALID_INPUT",
        );
        let lut = directory.join("Encodes").join("grade.cube");
        refused(
            export(&output, json!({"effectAssets": {id: lut}}), Some(id)),
            "MISSING_INPUT",
            "INVALID_INPUT",
        );
    }
    refused(
        export(&output, json!({"videoEncoder": "no-such-encoder"}), None),
        "INVALID_INPUT",
        "INVALID_INPUT",
    );
    let existing = directory.join("existing.mp4");
    std::fs::write(&existing, b"keep").expect("write");
    refused(
        export(&existing, json!({}), None),
        "IO_ERROR",
        "OUTPUT_IO_FAILED",
    );
    assert_eq!(std::fs::read(&existing).expect("read"), b"keep");
    assert!(!output.exists(), "nothing was written");
    host.handle(export(&output, json!({}), None), &mut |_| Ok(()))
        .expect("the export runs");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn an_export_error_is_not_classified_by_tags_its_paths_spell() {
    // A caption font path holding a host tag is only a path; the host's own tags open with a token
    // no request can know.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("forged-tags");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let clip = corpus("parity-rgb.mov");
    let export = |output: &Path, extra: Value| {
        let mut request = json!({
            "protocolVersion": 1, "requestId": "export", "operation": "export",
            "project": conform_project(&json!({})),
            "assets": {"video": clip},
            "startTicks": 0, "endTicks": 30_000,
            "frameRate": {"num": 12, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": output
        });
        for (key, value) in extra.as_object().expect("fields") {
            request[key] = value.clone();
        }
        parse_request(&request.to_string()).expect("export request")
    };
    let mut refused = |request| {
        let error: HostError = host
            .handle(request, &mut |_| Ok(()))
            .expect_err("the export is refused");
        let failure = error.failure.clone().expect("an export failure report");
        (error, failure)
    };
    let output = directory.join("out.mp4");
    let (plain, plain_failure) = refused(export(
        &output,
        json!({"fonts": [directory.join("Inter.ttf")]}),
    ));
    assert_eq!(plain.code, "MISSING_INPUT", "{}", plain.message);
    let (plain_output, plain_output_failure) = refused(export(
        &directory.join("no-folder").join("out.mp4"),
        json!({}),
    ));
    assert_eq!(plain_output.code, "IO_ERROR", "{}", plain_output.message);
    for forged in [
        "[alpha2:code=GPU_DEVICE_LOST] ",
        "[alpha2:GPU_TIMEOUT] ",
        "[alpha2:INTERNAL] ",
        "[alpha2:RESOURCE_LIMIT] ",
        "[alpha2:DECODE_STALLED] ",
        "[alpha2:ENCODE_STALLED] ",
        "[alpha2:COPY_INELIGIBLE] ",
        "[alpha2:GPU_OUTPUT_UNAVAILABLE] ",
        "[alpha2:evidence=OUT_OF_MEMORY] ",
        "[alpha2:phase=render frame=3] ",
        "[alpha2:phase=encode frame=3] [alpha2:code=CANCELLED] ",
    ] {
        let font = directory.join(format!("{forged}Inter.ttf"));
        let (error, failure) = refused(export(&output, json!({"fonts": [font]})));
        assert_eq!(error.code, plain.code, "{forged}: {}", error.message);
        assert!(
            error.message.contains(&font.display().to_string()),
            "{forged}: the message lost the path as given: {}",
            error.message
        );
        for field in ["code", "phase", "resource", "frame"] {
            assert_eq!(failure[field], plain_failure[field], "{forged}: {failure}");
        }
        assert_eq!(error.evidence, None, "{forged}");

        let folder = directory.join(format!("{forged}no-folder"));
        let (error, failure) = refused(export(&folder.join("out.mp4"), json!({})));
        assert_eq!(error.code, plain_output.code, "{forged}: {}", error.message);
        assert!(
            error.message.contains(&folder.display().to_string()),
            "{forged}: {}",
            error.message
        );
        for field in ["code", "phase", "resource", "frame"] {
            assert_eq!(
                failure[field], plain_output_failure[field],
                "{forged}: {failure}"
            );
        }
    }
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_stopped_encode_worker_reports_the_original_pipeline_failure() {
    // A worker that has given up shows as a channel disconnect; the reported error must still be
    // what actually failed.
    let (worker, command_receiver) = EncodeWorker::detached(1);
    worker
        .failure
        .record("native encode failed: VideoToolbox rejected the frame");
    worker.cancel.cancel();
    drop(command_receiver);

    assert_eq!(
        worker.send(EncodeCommand::Finish),
        Err("native encode failed: VideoToolbox rejected the frame".to_owned())
    );
}

/// The encode channel's depth counts a command before the channel holds it, so
/// the worker, which may take it at once, never finds the count short: counted
/// after the send, about a quarter of the takes of this replica (a channel of
/// two, two million commands) find it at zero and wrap it through `u64::MAX`,
/// recording a depth of zero. A take that finds the count short fails the
/// worker here (a debug assertion).
#[test]
fn the_encode_channel_depth_counts_a_command_before_the_worker_can_take_it() {
    const COMMANDS: u64 = 2_000_000;
    let depth = EncodeChannelDepth::new(2);
    let (sender, receiver) = mpsc::sync_channel::<u64>(2);
    let worker_depth = Arc::clone(&depth);
    let worker = std::thread::spawn(move || {
        let mut taken = 0_u64;
        for _ in receiver {
            worker_depth.taken();
            taken += 1;
        }
        taken
    });
    for command in 0..COMMANDS {
        let mut pending = command;
        loop {
            match depth.try_send(&sender, pending) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(back)) => {
                    pending = back;
                    std::thread::yield_now();
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    panic!("the worker stopped taking commands")
                }
            }
        }
    }
    drop(sender);
    let taken = worker.join().expect("a take found the depth count short");
    assert_eq!(taken, COMMANDS);
    let samples = depth.samples();
    assert_eq!(samples.iter().sum::<u64>(), COMMANDS, "{samples:?}");
}

#[test]
fn a_full_encode_channel_blocks_until_the_worker_drains_it() {
    // An empty channel takes the command without reporting backpressure. A full one must block on
    // the worker rather than fail, and release as soon as the worker takes one command.
    let (worker, command_receiver) = EncodeWorker::detached(1);

    assert_eq!(worker.send(EncodeCommand::Finish), Ok(false));
    let drain = std::thread::spawn(move || {
        let _taken = command_receiver.recv().expect("queued command");
        command_receiver
    });
    assert!(worker.send(EncodeCommand::Finish).is_ok());
    let _receiver = drain.join().expect("drain thread");
}

/// A fake encode worker on `worker`'s channel: takes `commands` commands, each after `delay`, the
/// way the real worker records its takes. Its handle goes in `worker.join`.
fn fake_encode_worker(
    worker: &mut EncodeWorker,
    receiver: mpsc::Receiver<EncodeCommand>,
    commands: usize,
    delay: Duration,
) {
    let depth = std::sync::Arc::clone(&worker.depth);
    let progress = std::sync::Arc::clone(&worker.progress);
    worker.join = Some(std::thread::spawn(move || {
        let _exit = crate::heartbeat::WorkerExit(std::sync::Arc::clone(&progress));
        for _ in 0..commands {
            std::thread::sleep(delay);
            if crate::encode::take_command(&receiver, &depth, &progress).is_none() {
                return;
            }
        }
    }));
}

#[test]
fn a_rendezvous_send_returns_when_the_worker_takes_the_command() {
    // With a poll of 10 s, a send that waited for its poll instead of the take could not finish
    // in the 5 s each send is given.
    let (mut worker, receiver) = EncodeWorker::detached(0);
    worker.poll = Duration::from_secs(10);
    assert!(worker.rendezvous);
    fake_encode_worker(&mut worker, receiver, 20, Duration::from_millis(20));
    for command in 0..20 {
        let started = std::time::Instant::now();
        assert_eq!(
            worker.send(EncodeCommand::Finish),
            Ok(true),
            "command {command}: the worker was busy, so the hand-off waited"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "command {command} took {:?}",
            started.elapsed()
        );
    }
    assert_eq!(worker.progress.current(), 20);
}

#[test]
fn a_rendezvous_send_to_an_idle_worker_reports_no_backpressure() {
    let (mut worker, receiver) = EncodeWorker::detached(0);
    worker.poll = Duration::from_secs(10);
    fake_encode_worker(&mut worker, receiver, 1, Duration::ZERO);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !worker.progress.waiting() {
        assert!(
            std::time::Instant::now() < deadline,
            "the fake worker never waited"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(worker.send(EncodeCommand::Finish), Ok(false));
    assert_eq!(worker.progress.current(), 1);
}

#[test]
fn a_rendezvous_send_to_a_stopped_worker_reports_its_failure() {
    let (mut worker, receiver) = EncodeWorker::detached(0);
    worker.poll = Duration::from_secs(10);
    let failure = worker.failure.clone();
    let progress = std::sync::Arc::clone(&worker.progress);
    worker.join = Some(std::thread::spawn(move || {
        let _exit = crate::heartbeat::WorkerExit(progress);
        // Stops a moment after the send, which then waits for a take that never comes; a send
        // that finds the channel already closed must report the same failure.
        std::thread::sleep(Duration::from_millis(50));
        failure.record("native encode failed: the muxer refused the header");
        drop(receiver);
    }));
    let started = std::time::Instant::now();
    assert_eq!(
        worker.send(EncodeCommand::Finish),
        Err("native encode failed: the muxer refused the header".to_owned())
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_cancellation_notice_is_not_recorded_as_the_first_failure() {
    // Once another part cancelled the export, a worker's "cancelled" is not a cause.
    let failure = PipelineFailure::default();
    let cancel = CancelToken::default();
    let encode = |frame, message: &str| {
        crate::failure::tag_phase(
            crate::failure::ExportPhase::Encode,
            Some(frame),
            message.to_owned(),
        )
    };
    failure.record_unless_cancelled(&cancel, &encode(3, "disk full"));
    failure.record_unless_cancelled(&cancel, &encode(4, "second"));
    assert_eq!(failure.cause_or("x"), encode(3, "disk full"));

    let failure = PipelineFailure::default();
    cancel.cancel();
    failure.record_unless_cancelled(&cancel, "native encode was cancelled");
    assert_eq!(
        failure.cause_or("the coordinator's error"),
        "the coordinator's error"
    );
    assert_eq!(
        failure.report("the coordinator's error".to_owned()),
        "the coordinator's error"
    );
    assert_eq!(
        failure.report("a later error".to_owned()),
        "the coordinator's error"
    );
}

/// A decode worker whose pool already reported `status`, as a pool that
/// stopped does.
fn decode_worker_reporting(
    status: crate::decode::DecodeWorkerStatus,
    failure: PipelineFailure,
) -> crate::decode::DecodeWorker {
    let (command_sender, _command_receiver) = mpsc::sync_channel(1);
    let (_frame_sender, frame_receiver) = mpsc::sync_channel(1);
    let (status_sender, status_receiver) = mpsc::channel();
    status_sender.send(status).expect("status");
    crate::decode::DecodeWorker {
        commands: Some(command_sender),
        frames: Some(frame_receiver),
        status: status_receiver,
        join: None,
        cancel: CancelToken::default(),
        budget: Arc::new(DecodeBudget {
            limit: crate::memory::ByteBudget::exact(1 << 20),
            state: Mutex::new(DecodeBudgetState::default()),
            available: Condvar::new(),
        }),
        waits: Arc::default(),
        failure,
        heartbeats: Vec::new(),
        progress: Arc::default(),
        stall_timeout: Duration::from_secs(60),
        stalled: std::sync::atomic::AtomicBool::new(false),
    }
}

#[test]
fn a_worker_that_failed_after_the_first_failure_reports_the_first_one() {
    // A notice the first failure caused is not the export's failure: the coordinator gets the first
    // failure recorded.
    use crate::failure::{ExportPhase, tag_phase};
    let first = &tag_phase(
        ExportPhase::Encode,
        Some(812),
        "native encode failed while writing media: No space left on device".to_owned(),
    );
    let notice = &tag_phase(
        ExportPhase::Decode,
        None,
        "decode worker was cancelled".to_owned(),
    );
    let failure = PipelineFailure::default();
    failure.record(first);
    let worker = decode_worker_reporting(
        crate::decode::DecodeWorkerStatus::Failed(notice.to_owned()),
        failure.clone(),
    );
    assert_eq!(worker.check_status(), Err(first.to_owned()));
    let mut worker = decode_worker_reporting(
        crate::decode::DecodeWorkerStatus::Failed(notice.to_owned()),
        failure.clone(),
    );
    assert_eq!(worker.finish(), Err(first.to_owned()));

    let encode_worker = |failure: PipelineFailure| {
        let (mut worker, command_receiver) = EncodeWorker::detached(1);
        let (result_sender, result_receiver) = mpsc::channel();
        result_sender
            .send(crate::encode::EncodeWorkerEvent::Failed(tag_phase(
                ExportPhase::Encode,
                Some(813),
                "native encode was cancelled".to_owned(),
            )))
            .expect("event");
        worker.results = result_receiver;
        worker.failure = failure;
        (worker, command_receiver, result_sender)
    };
    let decode_first = &tag_phase(
        ExportPhase::Decode,
        None,
        crate::memory::resource_limit("one decoded frame set requires 9 bytes"),
    );
    let failure = PipelineFailure::default();
    failure.record(decode_first);
    let (worker, _commands, _results) = encode_worker(failure.clone());
    assert!(matches!(worker.try_result(), Err(error) if error == *decode_first));
    let (worker, _commands, _results) = encode_worker(failure);
    assert!(matches!(worker.result(), Err(error) if error == *decode_first));

    let worker = decode_worker_reporting(
        crate::decode::DecodeWorkerStatus::Failed(first.to_owned()),
        PipelineFailure::default(),
    );
    assert_eq!(worker.check_status(), Err(first.to_owned()));
}

#[test]
fn pipeline_failure_preserves_the_first_worker_error() {
    let failure = PipelineFailure::default();
    failure.record("original encode error");
    failure.record("secondary decode cancellation");
    assert_eq!(
        failure.cause_or("generic cancellation"),
        "original encode error"
    );
}

/// An export of a one-second 64x48 video clip with 0.4 s of audio beside
/// it, on the readback path: the decode pool and the audio window both run
/// while the encode worker writes.
fn video_and_audio_request(output: &Path) -> Request {
    let project = json!({
        "schema_version": 1,
        "id": "first-failure",
        "name": "First failure",
        "settings": {
            "width": 64,
            "height": 48,
            "frame_rate": {"num": 30, "den": 1},
            "sample_rate": 48000
        },
        "sources": [
            {"id": "video", "locator": "fixture://video", "kind": "video"},
            {"id": "audio", "locator": "fixture://audio", "kind": "audio"}
        ],
        "timeline": {"tracks": [
            {"id": "v", "name": "V", "kind": "video", "clips": [{
                "id": "video-clip",
                "source_id": "video",
                "timeline_range": {"start": 0, "end": 120_000},
                "source_range": {"start": 0, "end": 120_000}
            }]},
            {"id": "a", "name": "A", "kind": "audio", "clips": [{
                "id": "audio-clip",
                "source_id": "audio",
                "timeline_range": {"start": 0, "end": 48_000},
                "source_range": {"start": 0, "end": 48_000}
            }]}
        ]}
    });
    parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "first-failure", "operation": "export",
            "project": project,
            "assets": {
                "video": corpus("long-gop.mp4"),
                "audio": corpus("audio-only-aac-priming.m4a")
            },
            "startTicks": 0, "endTicks": 120_000,
            "frameRate": {"num": 30, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": output
        })
        .to_string(),
    )
    .expect("export request")
}

/// An export of `video` and `audio`, two seconds of each, written to `output`.
fn damaged_media_request(video: &str, audio: &str, output: &Path) -> Request {
    parse_request(
        &json!({
            "protocolVersion": 1, "requestId": "damaged", "operation": "export",
            "project": {
                "schema_version": 1, "id": "damaged", "name": "Damaged",
                "settings": {"width": 64, "height": 48, "frame_rate": {"num": 10, "den": 1}, "sample_rate": 48_000},
                "sources": [
                    {"id": "picture", "locator": "fixture://picture", "kind": "video"},
                    {"id": "sound", "locator": "fixture://sound", "kind": "audio"}
                ],
                "timeline": {"tracks": [
                    {"id": "v", "name": "V", "kind": "video", "clips": [{
                        "id": "picture-clip", "source_id": "picture",
                        "timeline_range": {"start": 0, "end": 240_000},
                        "source_range": {"start": 0, "end": 240_000}
                    }]},
                    {"id": "a", "name": "A", "kind": "audio", "clips": [{
                        "id": "sound-clip", "source_id": "sound",
                        "timeline_range": {"start": 0, "end": 240_000},
                        "source_range": {"start": 0, "end": 240_000}
                    }]}
                ]}
            },
            "assets": {"picture": corpus(video), "sound": corpus(audio)},
            "startTicks": 0, "endTicks": 240_000,
            "frameRate": {"num": 10, "den": 1},
            "videoCodec": "mpeg4-part2",
            "output": output
        })
        .to_string(),
    )
    .expect("export request")
}

/// The damage a Tolerant export decoded past is in the result, not only on
/// stderr as a JSON line per observation: `decodeDiagnostics` counts, per
/// source, what its decoders reported, `[]` for clean media, and a failed
/// export's report carries it too.
#[test]
fn an_export_reports_the_damage_its_decoders_skipped() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("decode-diagnostics");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let export = |host: &mut Host, policy, video: &str, audio: &str, name: &str| {
        crate::recovery::TEST_DECODE_POLICY.set(Some(policy));
        let result = host.handle(
            damaged_media_request(video, audio, &directory.join(name)),
            &mut |_| Ok(()),
        );
        crate::recovery::TEST_DECODE_POLICY.set(None);
        result
    };
    let clean = export(
        &mut host,
        alpha2_native::DecodePolicy::Strict,
        "recovery-h264.mp4",
        "recovery-aac.m4a",
        "clean.mp4",
    )
    .expect("a clean export");
    assert_eq!(clean["decodeDiagnostics"], json!([]), "{clean}");

    let tolerant = export(
        &mut host,
        alpha2_native::DecodePolicy::Tolerant,
        "recovery-h264-damaged.mp4",
        "recovery-aac-damaged.m4a",
        "tolerant.mp4",
    )
    .expect("Tolerant decodes past the damage");
    let reports = tolerant["decodeDiagnostics"].as_array().expect("a list");
    assert_eq!(reports.len(), 2, "{tolerant}");
    // In source ID order: the video's damaged packet left one gap, and the audio's 1024 samples
    // were concealed as silence at the project's rate.
    let (picture, sound) = (&reports[0], &reports[1]);
    assert_eq!(picture["source"], "picture", "{tolerant}");
    assert!(picture["corruptPackets"].as_u64() >= Some(1), "{tolerant}");
    assert!(picture["videoGaps"].as_u64() >= Some(1), "{tolerant}");
    assert_eq!(picture["concealedAudioSamples"], 0, "{tolerant}");
    assert_eq!(sound["source"], "sound", "{tolerant}");
    assert!(sound["corruptPackets"].as_u64() >= Some(1), "{tolerant}");
    assert_eq!(sound["concealedAudioSamples"], 1024, "{tolerant}");
    assert_eq!(sound["videoGaps"], 0, "{tolerant}");

    let strict = export(
        &mut host,
        alpha2_native::DecodePolicy::Strict,
        "recovery-h264-damaged.mp4",
        "recovery-aac.m4a",
        "strict.mp4",
    )
    .expect_err("Strict fails on the damage");
    assert!(
        strict.message.contains("strict decode policy"),
        "{}",
        strict.message
    );
    let failure = strict.failure.as_ref().expect("a failure report");
    let reported = &failure["diagnostics"]["decodeDiagnostics"];
    assert_eq!(reported[0]["source"], "picture", "{failure}");
    assert!(
        reported[0]["corruptPackets"].as_u64() >= Some(1),
        "{failure}"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// The child half of `an_export_reads_its_decode_policy_from_the_environment`:
/// a damaged-video export under the `ALPHA2_DECODE_POLICY` its parent set,
/// answered on stdout as the protocol answers it.
#[test]
#[ignore = "run by an_export_reads_its_decode_policy_from_the_environment with ALPHA2_DECODE_POLICY set"]
fn export_under_the_environment_decode_policy() {
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("environment-decode-policy");
    let output = directory.join("out.mp4");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let answer = match host.handle(
        damaged_media_request("recovery-h264-damaged.mp4", "recovery-aac.m4a", &output),
        &mut |_| Ok(()),
    ) {
        Ok(result) => json!({"ok": true, "result": result, "written": output.exists()}),
        Err(error) => json!({"ok": false, "error": error, "written": output.exists()}),
    };
    println!("{answer}");
    std::fs::remove_dir_all(&directory).ok();
}

/// An export reads `ALPHA2_DECODE_POLICY` as it starts. The other host
/// tests set the policy per thread, since tests running in parallel share
/// the environment; this one runs a child process under each value. A value
/// other than `strict` or `tolerant` fails the export with `INVALID_INPUT`
/// before it writes anything, rather than falling back to a policy.
#[test]
fn an_export_reads_its_decode_policy_from_the_environment() {
    let answer = |policy: &str| -> Value {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::failures::export_under_the_environment_decode_policy",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ALPHA2_DECODE_POLICY", policy)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("1 passed"), "{policy}: {stdout}");
        stdout
            .lines()
            // libtest writes the test's name on the same line first.
            .find_map(|line| line.find("{\"").map(|at| &line[at..]))
            .map_or_else(
                || panic!("{policy}: no answer in {stdout}"),
                |line| serde_json::from_str(line).expect("JSON"),
            )
    };
    let tolerant = answer("tolerant");
    assert_eq!(tolerant["ok"], true, "{tolerant}");
    assert!(
        tolerant["result"]["decodeDiagnostics"][0]["videoGaps"].as_u64() >= Some(1),
        "{tolerant}"
    );
    let strict = answer("strict");
    assert_eq!(strict["ok"], false, "{strict}");
    assert!(
        strict["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("strict decode policy")),
        "{strict}"
    );
    for policy in ["lenient", ""] {
        let refused = answer(policy);
        let error = &refused["error"];
        assert_eq!(refused["ok"], false, "{policy}: {refused}");
        assert_eq!(error["code"], "INVALID_INPUT", "{policy}: {refused}");
        assert!(
            error["message"].as_str().is_some_and(
                |message| message.contains("ALPHA2_DECODE_POLICY must be strict or tolerant")
            ),
            "{policy}: {refused}"
        );
        assert_eq!(
            error["failure"]["code"], "INVALID_INPUT",
            "{policy}: {refused}"
        );
        assert_eq!(refused["written"], false, "{policy}: {refused}");
    }
}

#[test]
fn an_export_whose_encoder_fails_reports_that_failure_and_not_the_cancellations_it_caused() {
    // A full-disk write failure cancels the export, and a worker's "cancelled" may reach the
    // coordinator first; the export must still name the write. Which notice wins depends on timing,
    // so the export runs several times.
    let _gpu = gpu_test_lock();
    let directory = scratch_directory("first-failure-readback");
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    for attempt in 0..6 {
        crate::encode::INJECT_ENCODE_FAILURE.set(Some(0));
        crate::decode::SLOW_DECODE_JOBS.set(Some(Duration::from_millis(40)));
        let error = within(
            Duration::from_secs(60),
            "an export whose write fails",
            || {
                host.handle(
                    video_and_audio_request(&directory.join(format!("out-{attempt}.mp4"))),
                    &mut |_| Ok(()),
                )
                .expect_err("an export whose write fails fails")
            },
        );
        assert_eq!(
            error.code, "ENCODE_ERROR",
            "attempt {attempt}: {}",
            error.message
        );
        assert!(
            error.message.contains("No space left on device"),
            "attempt {attempt}: {}",
            error.message
        );
        let failure = error.failure.expect("an export failure report");
        assert_eq!(
            failure["code"], "STORAGE_QUOTA_EXCEEDED",
            "attempt {attempt}: {failure}"
        );
        assert_eq!(failure["phase"], "encode", "attempt {attempt}: {failure}");
        assert_eq!(failure["frameIndex"], 0, "attempt {attempt}: {failure}");
    }
    std::fs::remove_dir_all(&directory).ok();
}
