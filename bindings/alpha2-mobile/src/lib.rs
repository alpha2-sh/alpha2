//! The alpha2 API for iOS and Android apps.
//!
//! Swift and Kotlin bindings are generated from this crate with uniffi. The shape follows the
//! Node client: [`Alpha2::capabilities`], [`Alpha2::conform`], [`Alpha2::export`] and typed
//! [`Alpha2Error`]s carrying the engine's error codes. Projects, conform reports, capabilities and
//! export reports cross as JSON in the same schemas the desktop host uses, so schema fields pass
//! through unchanged.
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

mod error;
// The media pipeline is built everywhere so desktop tests compile it; only platforms with a media
// backend run it.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod export;
mod platform;
mod player;
mod session;
mod surface;

#[cfg(target_os = "android")]
pub mod android;

#[cfg(all(test, target_os = "android"))]
mod device_tests;

pub use error::{Alpha2Error, EncoderRefusalRecord};
pub use player::{Player, PlayerRequest};
pub use session::{
    Alpha2, ConformRequest, EngineConfig, ExportHandle, ExportOutcome, ExportProgress,
    ExportProgressEvent, ExportRequest, FontFile, RendererMode, VideoDecodeMode,
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
