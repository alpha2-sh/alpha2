//! The backend conformance suite on `MediaCodec`, run on a device or emulator with the corpus
//! copied to `ALPHA2_TESTS_DIR`.

use std::sync::Arc;

use alpha2_backend_conformance::{ContainerBackend, conformance_tests};
use alpha2_host_core::audio_stream::WithPcm;

use crate::codec::Codec;
use crate::{AndroidAudio, AndroidVideo};

fn refuses(name: &str) -> Option<&'static str> {
    if matches!(name, "hdr-pq.mp4" | "hdr-pq-topleft.mp4") {
        return Some("Android decodes H.264 up to High profile; these are 10-bit 4:4:4");
    }
    match name.rsplit('.').next().unwrap_or_default() {
        "mkv" | "webm" => return Some("Matroska is not read yet"),
        "ts" | "avi" => return Some("the container layer reads no MPEG-TS or AVI"),
        _ => {}
    }
    // What a device decodes depends on it: the API 29 emulator has no AV1 or AC-3 decoder and
    // only the software MPEG-4 decoder, which reads Simple Profile.
    let (mime, hardware) = match name {
        "av1.mp4" | "av1-colocated.mp4" => ("video/av01", false),
        "audio-ac3-5.1.mp4" => ("audio/ac3", false),
        "audio-eac3-5.0.mp4" => ("audio/eac3", false),
        "cfr-bt709-limited.mp4" | "long-gop.mp4" => ("video/mp4v-es", true),
        _ => return None,
    };
    let decodes = Codec::decoder_for(mime).is_ok_and(|codec| !hardware || !codec.is_software());
    (!decodes).then_some("this device has no decoder for it")
}

fn mediacodec() -> ContainerBackend {
    ContainerBackend::new(
        "mediacodec",
        Arc::new(AndroidVideo),
        Arc::new(WithPcm(AndroidAudio)),
        refuses,
    )
}

conformance_tests!(mediacodec());
