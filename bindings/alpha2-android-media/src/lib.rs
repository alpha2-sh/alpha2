//! The Android media backend: `MediaCodec` decoder sessions behind the host-core traits.
//!
//! Every NDK function used exists at API 29, the supported minimum, so the library loads on every
//! supported device. On other targets the crate is empty.

#[cfg(target_os = "android")]
mod audio;
#[cfg(target_os = "android")]
mod codec;
#[cfg(target_os = "android")]
mod video;

#[cfg(target_os = "android")]
pub use audio::{AndroidAudio, MediaCodecAudio};
#[cfg(target_os = "android")]
pub use video::{AndroidVideo, MediaCodecVideo};

#[cfg(all(test, target_os = "android"))]
mod tests;
