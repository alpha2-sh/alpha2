//! Host logic shared by every native alpha2 host, with no `FFmpeg` and no platform media API.
//!
//! The desktop host drives it through `FFmpeg`; the iOS and Android hosts through
//! `VideoToolbox` and `MediaCodec`. Project audio rendering needs the `engine` feature, on by
//! default.

mod audio_host;
pub mod audio_timeline;
pub mod recovery;
pub mod tools;

#[cfg(feature = "cpal-host")]
pub use audio_host::{CpalHostError, CpalOutputHost, RealtimeAudioSource};
pub use audio_host::{
    DecodedAudioSources, HostAudioConfig, HostSampleFormat, OfflineAudioError,
    negotiate_output_config, render_offline,
};
#[cfg(feature = "engine")]
pub use audio_host::{ProjectAudioRequest, render_project_audio};
pub use recovery::{
    ConcealedAudioGap, CorruptPacket, DECODE_WORK_LIMIT, DecodeDiagnostic, DecodeDiagnosticCounts,
    DecodeDiagnostics, DecodeFailure, DecodePolicy, DecodeProgressWatch, DroppedAudioOverlap,
    Recovery, VideoRecoveryGap, watch_decode_progress,
};
pub use tools::{
    FrameScore, THUMBNAIL_LUMA_RANGE, choose_thumbnail, downscale_frame, resize_rgba, score_frame,
};
