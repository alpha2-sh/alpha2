use super::*;
use alpha2_gpu::{GpuDiagnostic, GpuDiagnosticLog, GpuErrorKind};

#[test]
fn the_first_phase_a_message_received_is_kept() {
    let inner = tag_phase(ExportPhase::Encode, None, "encoder refused".to_owned());
    let outer = tag_phase(ExportPhase::Mux, Some(4), inner);
    assert_eq!(phase_tag(&outer), Some((ExportPhase::Encode, None)));
    assert_eq!(strip_phase_tags(outer), "encoder refused");
    let wrapped = format!(
        "wrapped: {}",
        tag_phase(ExportPhase::Readback, Some(12), "failed".to_owned())
    );
    assert_eq!(phase_tag(&wrapped), Some((ExportPhase::Readback, Some(12))));
    assert_eq!(strip_phase_tags(wrapped), "wrapped: failed");
}

#[test]
fn a_readback_failure_is_not_memory_exhaustion_without_device_evidence() {
    let message = "GPU texture readback failed: map-failed";
    let log = GpuDiagnosticLog::default();
    assert_eq!(
        classify_failure(
            "ALPHA2_ERROR",
            message,
            None,
            ExportPhase::Readback,
            &log.snapshot()
        ),
        FailureClass {
            code: "READBACK_FAILED",
            resource: "unknown"
        }
    );
    log.push(GpuDiagnostic::UncapturedError {
        kind: GpuErrorKind::OutOfMemory,
        message: "staging buffer".to_owned(),
    });
    assert_eq!(
        classify_failure(
            "ALPHA2_ERROR",
            message,
            None,
            ExportPhase::Readback,
            &log.snapshot()
        ),
        FailureClass {
            code: "OUT_OF_MEMORY",
            resource: "gpu"
        }
    );
    log.push(GpuDiagnostic::DeviceLost {
        reason: "Unknown".to_owned(),
        message: "reset".to_owned(),
    });
    assert_eq!(
        classify_failure(
            "ALPHA2_ERROR",
            message,
            None,
            ExportPhase::Readback,
            &log.snapshot()
        )
        .code,
        "DEVICE_LOST"
    );
}

#[test]
fn an_encoder_error_during_a_gpu_oom_is_not_blamed_on_the_gpu() {
    let log = GpuDiagnosticLog::default();
    log.push(GpuDiagnostic::UncapturedError {
        kind: GpuErrorKind::OutOfMemory,
        message: "texture".to_owned(),
    });
    assert_eq!(
        classify_failure(
            "ENCODE_ERROR",
            "native encode failed: invalid argument",
            None,
            ExportPhase::Encode,
            &log.snapshot()
        )
        .code,
        "UNKNOWN"
    );
}

#[test]
fn host_codes_and_runtime_markers_classify() {
    let none = GpuDiagnosticSnapshot::default();
    let code = |host, message, phase| classify_failure(host, message, None, phase, &none).code;
    assert_eq!(
        code("GPU_TIMEOUT", "", ExportPhase::Readback),
        "GPU_TIMEOUT"
    );
    assert_eq!(
        code("INVALID_INPUT", "", ExportPhase::Prepare),
        "INVALID_INPUT"
    );
    assert_eq!(code("CANCELLED", "", ExportPhase::Render), "CANCELLED");
    assert_eq!(
        code(
            "ENCODE_ERROR",
            "av_write_trailer: No space left on device",
            ExportPhase::Mux
        ),
        "STORAGE_QUOTA_EXCEEDED"
    );
    assert_eq!(
        code(
            "ALPHA2_ERROR",
            "Cannot allocate memory",
            ExportPhase::Decode
        ),
        "ALLOCATION_FAILED"
    );
    assert_eq!(
        code("IO_ERROR", "denied", ExportPhase::Mux),
        "OUTPUT_IO_FAILED"
    );
    assert_eq!(
        code("ALPHA2_ERROR", "who knows", ExportPhase::Render),
        "UNKNOWN"
    );
}

/// The class each protocol code is evidence of on its own is the one
/// `alpha2/export-failure` gives it: both read the same table.
#[test]
fn every_protocol_code_classifies_as_the_shared_table_says() {
    let table: Value =
        serde_json::from_str(include_str!("../../../tests/cases/failure-classes.json"))
            .expect("tests/cases/failure-classes.json");
    let protocol = table["protocol"].as_object().expect("protocol codes");
    let listed: std::collections::BTreeSet<&str> = protocol.keys().map(String::as_str).collect();
    let known: std::collections::BTreeSet<&str> =
        crate::protocol::PROTOCOL_CODES.into_iter().collect();
    assert_eq!(
        listed, known,
        "the table lists exactly the protocol's codes"
    );
    let none = GpuDiagnosticSnapshot::default();
    for (code, expected) in protocol {
        let class = classify_failure(code, "", None, ExportPhase::Prepare, &none);
        assert_eq!(class.code, expected["code"], "{code}");
        assert_eq!(class.resource, expected["resource"], "{code}");
    }
}

#[test]
fn a_start_that_failed_carries_a_failure_report() {
    let failure = startup_failure_json("HARDWARE_DECODE_UNAVAILABLE", "no decode device", None);
    assert_eq!(failure["code"], "INVALID_INPUT");
    assert_eq!(failure["hostCode"], "HARDWARE_DECODE_UNAVAILABLE");
    assert_eq!(failure["phase"], "prepare");
    assert_eq!(failure["origin"], "native-host");
    assert_eq!(
        failure["diagnostics"]["environment"]["hostVersion"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        failure["diagnostics"]["environment"]["osVersion"],
        alpha2_os::os_version()
    );
    assert_eq!(
        startup_failure_json("GPU_INIT_FAILED", "no adapter", None)["code"],
        "UNKNOWN"
    );
}

#[test]
fn the_failure_names_the_readback_stage() {
    let failure = failure_json(
        "ALPHA2_ERROR",
        "GPU texture readback failed: callback-dropped",
        None,
        (ExportPhase::Readback, Some(7)),
        &GpuDiagnosticSnapshot::default(),
        Map::new(),
    );
    assert_eq!(failure["stage"], "callback-dropped");
    assert_eq!(failure["frameIndex"], 7);
    assert_eq!(failure["phase"], "readback");
    assert_eq!(failure["code"], "READBACK_FAILED");
}

/// A busy encoder frees up when the exports holding it end, so a client
/// retries it, after the wait the failure names.
#[test]
fn encoder_busy_is_a_retryable_resource_class() {
    let failure = failure_json(
        "ENCODER_BUSY",
        "every hardware h264 encoder here is busy",
        None,
        (ExportPhase::Prepare, None),
        &GpuDiagnosticSnapshot::default(),
        Map::new(),
    );
    assert_eq!(failure["code"], "RESOURCE_BUSY");
    assert_eq!(failure["resource"], "gpu");
    assert_eq!(failure["hostCode"], "ENCODER_BUSY");
    assert_eq!(
        failure["retryAfterMs"],
        crate::encoder_refusal::ENCODER_BUSY_RETRY_AFTER_MS
    );
    let other = failure_json(
        "ENCODE_ERROR",
        "no",
        None,
        (ExportPhase::Prepare, None),
        &GpuDiagnosticSnapshot::default(),
        Map::new(),
    );
    assert!(other.get("retryAfterMs").is_none(), "{other}");
}
