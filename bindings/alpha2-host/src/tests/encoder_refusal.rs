//! What an export reports when no video encoder opened, from the refusals the
//! candidates gave: built here by hand, the way the native layer classifies
//! them, so no machine needs a held session or a missing encoder.

use alpha2_native::{EncoderRefusal, NativeVideoCodec, OpenFailureKind};

use crate::encoder_refusal::encoders_refused_error;
use crate::protocol::HostError;

fn refusal(encoder: &str, hardware: bool, kind: OpenFailureKind, reason: &str) -> EncoderRefusal {
    EncoderRefusal {
        encoder: encoder.to_owned(),
        hardware_accelerated: hardware,
        kind,
        reason: reason.to_owned(),
    }
}

/// Ten exports hold every NVENC and Vulkan video session of a consumer GPU,
/// and the runtime has no software H.264 encoder: the eleventh can succeed
/// once one of them ends, so it is told to retry, not that the machine
/// cannot encode.
#[test]
fn a_busy_gpu_encoder_with_no_cpu_encoder_is_encoder_busy() {
    let gpu_busy = refusal(
        "h264_vulkan",
        true,
        OpenFailureKind::Busy,
        "Unable to create video session: VK_ERROR_OUT_OF_DEVICE_MEMORY",
    );
    let nvenc = refusal(
        "h264_nvenc",
        true,
        OpenFailureKind::Busy,
        "OpenEncodeSessionEx failed: out of memory (10): (no details)",
    );
    for (refusals, gpu) in [
        (vec![nvenc.clone()], Some(&gpu_busy)),
        (vec![nvenc.clone()], None),
        (Vec::new(), Some(&gpu_busy)),
    ] {
        let message = encoders_refused_error(
            NativeVideoCodec::H264,
            (1920, 1080),
            &refusals,
            gpu,
            Some(("h264_vulkan", (160, 64))),
        )
        .expect("a busy encoder decides");
        let error = HostError::classify(message);
        assert_eq!(error.code, "ENCODER_BUSY", "{}", error.message);
        assert!(error.message.contains("busy"), "{}", error.message);
        assert!(error.message.contains("retry"), "{}", error.message);
        let failure = crate::failure::startup_failure_json(error.code, &error.message, None);
        assert_eq!(failure["code"], "RESOURCE_BUSY");
        assert_eq!(failure["retryAfterMs"], 2_000);
    }
    // A missing encoder beside a busy one: still busy, the busy one can open later.
    let absent = refusal("h264_mf", false, OpenFailureKind::Absent, "no MFT");
    assert!(
        encoders_refused_error(
            NativeVideoCodec::H264,
            (1920, 1080),
            &[absent.clone(), nvenc],
            None,
            None,
        )
        .is_some_and(|message| HostError::classify(message).code == "ENCODER_BUSY")
    );
    // Nothing busy and nothing too small: the caller's own message stands.
    assert_eq!(
        encoders_refused_error(NativeVideoCodec::H264, (1920, 1080), &[absent], None, None),
        None
    );
}

/// 96x64 is below NVENC's 146x50 and below a discrete NVIDIA GPU's
/// `h264_vulkan` minimum: the same request fails the same way every time, so
/// it is invalid input that names the sizes that work.
#[test]
fn an_output_below_every_encoder_minimum_is_invalid_input() {
    let nvenc = refusal(
        "h264_nvenc",
        true,
        OpenFailureKind::Unsuitable,
        "encoder h264_nvenc requires at least 146x50, received 96x64",
    );
    let message = encoders_refused_error(
        NativeVideoCodec::H264,
        (96, 64),
        std::slice::from_ref(&nvenc),
        None,
        Some(("h264_vulkan", (160, 64))),
    )
    .expect("every encoder refused the size");
    let error = HostError::classify(message);
    assert_eq!(error.code, "INVALID_INPUT", "{}", error.message);
    assert!(error.message.contains("96x64"), "{}", error.message);
    assert!(
        error.message.contains("h264_nvenc 146x50"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("h264_vulkan 160x64"),
        "{}",
        error.message
    );
    assert!(error.message.contains("OpenH264"), "{}", error.message);
    // One encoder that refused for another reason: the size does not explain it.
    let other = refusal("h264_qsv", true, OpenFailureKind::Unknown, "no device");
    assert_eq!(
        encoders_refused_error(
            NativeVideoCodec::H264,
            (96, 64),
            &[nvenc, other],
            None,
            None,
        ),
        None
    );
}

/// The startup line says whether `OpenH264` is there and what to do when it
/// is not: on a machine without a hardware H.264 encoder it is what every
/// H.264 export needs.
#[test]
fn the_startup_line_names_openh264() {
    assert!(
        crate::openh264_startup_status(alpha2_native::openh264_status()).starts_with("openh264="),
    );
    assert_eq!(
        crate::openh264_startup_status(&alpha2_native::OpenH264Status::Disabled),
        "openh264=disabled (ALPHA2_OPENH264=0)"
    );
    let missing = crate::openh264_startup_status(&alpha2_native::OpenH264Status::NotInstalled(
        "looked in the cache".to_owned(),
    ));
    assert!(missing.starts_with("openh264=not installed ("), "{missing}");
    assert!(
        missing.contains(alpha2_native::OPENH264_INSTALL_COMMAND),
        "{missing}"
    );
    assert_eq!(
        crate::openh264_startup_status(&alpha2_native::OpenH264Status::Unusable(
            "bad checksum".to_owned()
        )),
        "openh264=unusable (bad checksum)"
    );
}

/// Each fallback says how the encoder ran and what its refusal is evidence of, so a client can
/// tell a busy GPU encoder (which may open later) from a missing one.
#[test]
fn fallback_entries_say_how_the_encoder_ran() {
    let rejected = [
        alpha2_native::NativeVideoEncoderFallback {
            encoder_name: "h264_nvenc".to_owned(),
            hardware_accelerated: true,
            reason: alpha2_native::NativeEncoderRejection::OpenFailed,
            kind: OpenFailureKind::Busy,
            detail: "OpenEncodeSessionEx failed: out of memory (10)".to_owned(),
        },
        alpha2_native::NativeVideoEncoderFallback {
            encoder_name: "h264_mf".to_owned(),
            hardware_accelerated: false,
            reason: alpha2_native::NativeEncoderRejection::Dimensions,
            kind: OpenFailureKind::Unsuitable,
            detail: "64x48 is below 128x96".to_owned(),
        },
    ];
    assert_eq!(
        crate::encoder_refusal::fallbacks_json(&rejected),
        serde_json::json!([
            {"encoder": "h264_nvenc", "hardwareAccelerated": true, "kind": "busy",
             "reason": "open-failed", "detail": "OpenEncodeSessionEx failed: out of memory (10)"},
            {"encoder": "h264_mf", "hardwareAccelerated": false, "kind": "unsuitable",
             "reason": "dimensions", "detail": "64x48 is below 128x96"},
        ])
    );
}
