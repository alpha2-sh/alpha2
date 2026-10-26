//! Native `FFmpeg` 8 media adapter.
//!
//! All rsmpeg and `FFmpeg` types remain private to this binding. Public callers
//! only observe the platform-neutral `alpha2-media` contracts.
//!
//! Mixing a timeline project's audio needs the `timeline` feature, which is
//! the only part of this crate that links the project model and the engine.

// A closed standard error must not panic the process, so diagnostics go through
// `alpha2_os::diagnostic!` instead of `eprintln!`.
#![cfg_attr(not(test), warn(clippy::print_stderr))]

mod audio_host;
mod audio_input;
pub use audio_input::AudioStreamConfig;
mod audio_timeline;
mod bitstream;
mod capabilities;
mod convert;
mod decode;
mod recovery;
pub use recovery::{
    ConcealedAudioGap, CorruptPacket, DECODE_WORK_LIMIT, DecodeDiagnostic, DecodeDiagnosticCounts,
    DecodeDiagnostics, DecodePolicy, DecodeProgressWatch, DroppedAudioOverlap, VideoRecoveryGap,
    watch_decode_progress,
};
mod encode;
mod mux;
mod partial;
pub use partial::{PARTIAL_OUTPUT_IN_USE, remove_stale_partial_output, write_replacing};
#[cfg(any(
    test,
    all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    )
))]
mod parameter_sets;
mod passthrough;
pub use passthrough::{
    AudioBlockFeed, CopyAudio, PacketCopy, PacketCopyAudio, PacketCopyPlan, PacketCopyPlanner,
    PacketCopyReport, PacketCopyRequest, check_yuv_transcode, copy_packets, plan_packet_copy,
    same_chroma_siting,
};
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
mod hwdecode;
#[cfg(feature = "hwencode")]
mod hwencode;
mod openh264;
pub use openh264::{
    OPENH264_ENCODER_NAME, OPENH264_INSTALL_COMMAND, OPENH264_NOTICE, OPENH264_VERSION,
    OpenH264Library, OpenH264Provider, OpenH264Status, openh264_status,
};
mod rate_control;
pub use rate_control::{AppliedRateControl, RateControlMechanism};
#[cfg(test)]
mod tests;
mod tools;
pub use tools::{
    FrameScore, THUMBNAIL_LUMA_RANGE, choose_thumbnail, downscale_frame, resize_rgba, score_frame,
};
mod video_backend;
mod video_encode;
mod video_frame;
pub use alpha2_ffmpeg_bridge::Yuv420pFrameBuffer;
#[cfg(feature = "cpal-host")]
pub use audio_host::{CpalHostError, CpalOutputHost, RealtimeAudioSource};
pub use audio_host::{
    DecodedAudioSources, HostAudioConfig, HostSampleFormat, OfflineAudioError,
    negotiate_output_config, render_offline,
};
#[cfg(feature = "timeline")]
pub use audio_host::{ProjectAudioRequest, render_project_audio};
pub use capabilities::{
    AvEncodeConfig, NativeAudioCodec, NativeAudioEncoderCapability, NativeCodecCapabilities,
    NativeEncoderClass, NativeEncoderHardwareConfig, NativeEncoderLibrary, NativeEncoderRejection,
    NativeHardwareDecodeCapabilities, NativeHardwareDecodeCapability, NativeRateControlProbe,
    NativeVideoCodec, NativeVideoEncoderCapability, NativeVideoEncoderFallback,
    NativeVideoEncoderProbe, NativeVideoEncoderSelection, audio_encoder_capabilities,
    codec_capabilities, h264_encoder_remedy, hardware_decode_backend, hardware_decode_capabilities,
    hybrid_encoders_use_hardware, no_video_encoder_message, probe_video_encoders,
    video_encoder_capabilities, video_encoders_without_probing,
};
pub use convert::{
    CodecThreadAllocation, CodecThreadRequest, codec_thread_allocation, codec_thread_budget,
    codec_thread_override, configured_codec_thread_override,
};
pub use decode::{
    DecodeStageTimings, NativeAudioDecoder, NativeDecoder, ProbeResult, SOURCE_SAMPLE_RATES, probe,
    software_decode_unsupported, visit_packet_times,
};
pub use encode::{
    EncodeCallWatch, EncodeStageTimings, NativeAvEncodeSink, NativeWavEncodeSink,
    watch_encode_calls,
};
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
pub use hwdecode::{VulkanDecodeProbe, probe_vulkan_decode};
#[cfg(all(target_os = "windows", feature = "hwencode"))]
pub use hwencode::{
    D3D12EncodeProbe, D3D12RateControlOpen, D3D12RateControlRunner, D3D12TestEncodeRunner,
    D3D12VA_TEST_ENCODE_SIZE, d3d12va_encoder, open_d3d12va_rate_control, probe_d3d12va_encode,
    set_d3d12va_rate_control_runner, set_d3d12va_test_encode_runner,
};
#[cfg(feature = "hwencode")]
pub use hwencode::{
    EncoderInputHandoff, EncoderInputLayout, EncoderPoolStats, EncoderSurface,
    EncoderSurfaceDescriptor, HardwareEncodeBackend, HardwareEncodeCapability,
    HardwareEncodeDevice, HardwareEncodeInput, HardwareEncodeOpenError, HardwareEncodeRequest,
    HardwareEncodeSession, InputFrame,
};
#[cfg(all(target_os = "macos", feature = "hwencode"))]
pub use hwencode::{VideoToolboxEncodeProbe, probe_videotoolbox_encode};
#[cfg(all(target_os = "linux", feature = "hwencode"))]
pub use hwencode::{VulkanEncodeProbe, probe_vulkan_encode};
pub use video_encode::{NativeVideoEncodeSink, VideoDisplay, VideoEncodeConfig};
pub use video_frame::{
    HardwareDecodeFallback, HardwareFrameLease, HardwareOpenError, NativeExportDecoder,
    NativePlaneLayout, NativeVideoFrame, NativeVideoFrameMetadata,
};

/// `FFmpeg` runtime and pinned SDK identity used by native diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FfmpegRuntimeIdentity {
    /// Version reported by the loaded `FFmpeg` shared library.
    pub version: String,
    /// Configure flags reported by the loaded libavcodec.
    pub configuration: String,
    /// SDK release used by alpha2 release builds.
    pub pinned_sdk_release: &'static str,
}

impl FfmpegRuntimeIdentity {
    /// Why the loaded runtime is not the pinned SDK, or `None` when it is.
    ///
    /// Cargo links whichever `FFmpeg` `pkg-config` finds, so a shell without
    /// the SDK's `PKG_CONFIG_PATH` builds against a system copy (on macOS,
    /// Homebrew's GPL build) and nothing else says so. Such a runtime offers
    /// different encoders (`libx264` ahead of `h264_videotoolbox`), so its
    /// exports are not the ones alpha2 ships.
    ///
    /// Besides the version, the configuration must be one the SDK's
    /// `manifest.toml` allows: no GPL, nonfree or version 3 code, no
    /// external `--enable-lib*` codec but the ones the SDK requires, which
    /// must be there, and `--disable-autodetect`, without which a build takes
    /// whatever libraries its machine happens to have. That catches a system
    /// build of the same version, and an earlier SDK build, too.
    #[must_use]
    pub fn pinned_sdk_mismatch(&self) -> Option<String> {
        let mut reasons = Vec::new();
        if self.version != PINNED_SDK_SOURCE_TAG {
            reasons.push(format!(
                "version {} is not {PINNED_SDK_SOURCE_TAG}",
                self.version
            ));
        }
        let flags = || self.configuration.split_whitespace();
        for flag in flags().filter(|flag| {
            ["--enable-gpl", "--enable-nonfree", "--enable-version3"].contains(flag)
                || (flag.starts_with("--enable-lib")
                    && !PINNED_SDK_EXTERNAL_LIBRARIES.contains(flag))
        }) {
            reasons.push(format!("configured with {flag}"));
        }
        for library in PINNED_SDK_EXTERNAL_LIBRARIES {
            if !flags().any(|flag| flag == *library) {
                reasons.push(format!("not configured with {library}"));
            }
        }
        if !flags().any(|flag| flag == "--disable-autodetect") {
            reasons.push("not configured with --disable-autodetect".to_owned());
        }
        (!reasons.is_empty()).then(|| reasons.join(", "))
    }
}

/// Panics unless the loaded `FFmpeg` is the pinned SDK, for tests to call
/// before they use it.
///
/// A test binary built without `FFMPEG_PKG_CONFIG_PATH` links a system
/// `FFmpeg` and passes or fails on that runtime's encoders, which is not
/// what alpha2 ships: its results look like the release's and are not. Setting
/// `ALPHA2_ALLOW_UNPINNED_FFMPEG=1` runs them anyway. The runtime is looked
/// at once per process.
///
/// # Panics
/// When the runtime is not the pinned SDK and the variable is not `1`.
pub fn require_pinned_ffmpeg_for_tests() {
    static MISMATCH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let mismatch = MISMATCH.get_or_init(|| {
        let identity = ffmpeg_runtime_identity();
        let reason = identity.pinned_sdk_mismatch()?;
        if std::env::var_os("ALPHA2_ALLOW_UNPINNED_FFMPEG").is_some_and(|value| value == "1") {
            alpha2_os::diagnostic!(
                "alpha2: testing against FFmpeg {} although it is not the pinned SDK ({reason}): \
                 ALPHA2_ALLOW_UNPINNED_FFMPEG=1",
                identity.version
            );
            return None;
        }
        Some(format!(
            "these tests loaded FFmpeg {}, not the pinned SDK {}: {reason}. The build \
             linked whatever FFmpeg pkg-config found. Rebuild with \
             FFMPEG_PKG_CONFIG_PATH=\"$(scripts/fetch-ffmpeg-sdk.sh)/lib/pkgconfig\" \
             (on Windows, FFMPEG_INCLUDE_DIR and FFMPEG_LIBS_DIR). \
             ALPHA2_ALLOW_UNPINNED_FFMPEG=1 runs them against this FFmpeg anyway.",
            identity.version, identity.pinned_sdk_release,
        ))
    });
    if let Some(message) = mismatch {
        panic!("{message}");
    }
}

/// `source_tag` of `release/ffmpeg-lgpl/manifest.toml`: what the pinned SDK's
/// `av_version_info` reports.
const PINNED_SDK_SOURCE_TAG: &str = "n8.1.3";

/// The `--enable-lib*` flags in `required_configure` of
/// `release/ffmpeg-lgpl/manifest.toml`: the external libraries the pinned SDK
/// links.
const PINNED_SDK_EXTERNAL_LIBRARIES: &[&str] = &[
    "--enable-libdav1d",
    "--enable-libopus",
    "--enable-libsvtav1",
    "--enable-libvpx",
];

/// Returns the loaded `FFmpeg` identity together with the release SDK pin.
#[must_use]
pub fn ffmpeg_runtime_identity() -> FfmpegRuntimeIdentity {
    FfmpegRuntimeIdentity {
        version: alpha2_ffmpeg_bridge::runtime_version(),
        configuration: alpha2_ffmpeg_bridge::runtime_configuration(),
        pinned_sdk_release: "ffmpeg-8.1.3-shared-build4",
    }
}
