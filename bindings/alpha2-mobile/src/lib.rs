//! The alpha2 API for iOS and Android apps.
//!
//! Swift and Kotlin bindings are generated from this crate with uniffi. The shape follows the
//! Node client: [`Alpha2::capabilities`], [`Alpha2::conform`], [`Alpha2::export`] and typed
//! [`Alpha2Error`]s carrying the engine's error codes. Projects and results cross as JSON. A
//! conform result is this library's own shape, whether each source's file opens, not the desktop
//! host's conform report.
//!
//! Long operations run on threads this library owns. uniffi futures cannot be cancelled, so an
//! export returns an [`ExportHandle`] whose [`ExportHandle::cancel`] stops it and whose
//! [`ExportHandle::outcome`] resolves with its outcome. Outputs are written inside the app's sandbox
//! and published atomically; the app moves or shares them.
//!
//! On Android the app calls `Alpha2Android.init(context)` once before anything else; audio output
//! and codec listing need the app context.

// The scaffolding uniffi generates is the crate's FFI boundary, and exported functions receive
// owned values from it.
#![allow(unsafe_code, clippy::needless_pass_by_value)]

uniffi::setup_scaffolding!();

#[cfg(feature = "compose")]
mod compose;
mod edit;
mod error;
// The media pipeline is built everywhere so desktop tests compile it; only platforms with a media
// backend run it.
#[cfg_attr(
    not(any(target_os = "android", target_vendor = "apple")),
    allow(dead_code)
)]
mod export;
mod extract;
mod platform;
mod player;
#[cfg(feature = "plugins")]
mod plugin;
mod session;
mod stills;
mod surface;
mod zero_copy;

#[cfg(target_os = "android")]
pub mod android;

#[cfg(all(test, any(target_os = "android", target_os = "ios")))]
mod device_tests;

pub use edit::{EditedProject, apply_edit};
pub use error::{Alpha2Error, EncoderRefusalRecord};
pub use extract::{
    ExtractAudioBlock, ExtractRequest, ExtractVideoFrame, ExtractionHandle, Extractor,
    ExtractorFailure,
};
pub use player::{Player, PlayerRequest};
#[cfg(feature = "plugins")]
pub use plugin::Plugin;
pub use session::{
    Alpha2, ConformRequest, EngineConfig, ExportHandle, ExportOutcome, ExportProgress,
    ExportProgressEvent, ExportRequest, FontFile, RendererMode, StillEntry, StillsRequest,
    VideoDecodeMode, VideoFrameMode,
};

/// The library's version.
#[uniffi::export]
#[must_use]
pub fn alpha2_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Why `project_json` is not a valid alpha2 project, one line per problem; empty when it is.
#[uniffi::export]
#[must_use]
pub fn validate_project(project_json: String) -> Vec<String> {
    session::project_problems(&project_json)
}
