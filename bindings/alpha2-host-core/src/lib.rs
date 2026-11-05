//! Host logic shared by every native alpha2 host, with no `FFmpeg` and no platform media API.
//!
//! The desktop host drives it through `FFmpeg`; the iOS and Android hosts through
//! `VideoToolbox` and `MediaCodec`. Project audio rendering needs the `engine` feature, on by
//! default.

mod audio_host;
pub mod audio_timeline;
pub mod codecs;
pub mod encode_policy;
pub mod encode_watch;
pub mod parameter_sets;
pub mod partial;
pub mod probe_cache;
pub mod rate_control;
pub mod recovery;
pub mod tools;
pub mod vt_sei;

#[cfg(feature = "cpal-host")]
pub use audio_host::{CpalHostError, CpalOutputHost, RealtimeAudioSource};
pub use audio_host::{
    DecodedAudioSources, HostAudioConfig, HostSampleFormat, OfflineAudioError,
    negotiate_output_config, render_offline,
};
#[cfg(feature = "engine")]
pub use audio_host::{ProjectAudioRequest, render_project_audio};
pub use encode_watch::{EncodeCallWatch, watch_encode_calls};
pub use partial::{
    ExistingOutput, OutputTarget, PARTIAL_OUTPUT_IN_USE, PublishReport,
    remove_stale_partial_output, write_png, write_whole,
};
pub use recovery::{
    ConcealedAudioGap, CorruptPacket, DECODE_WORK_LIMIT, DecodeDiagnostic, DecodeDiagnosticCounts,
    DecodeDiagnostics, DecodeFailure, DecodePolicy, DecodeProgressWatch, DroppedAudioOverlap,
    Recovery, VideoRecoveryGap, watch_decode_progress,
};
pub use tools::{
    FrameScore, THUMBNAIL_LUMA_RANGE, choose_thumbnail, downscale_frame, resize_rgba, score_frame,
};
