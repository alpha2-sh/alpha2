//! The Apple media backend: `VideoToolbox` and `AudioToolbox` sessions behind the host-core
//! traits.
//!
//! It builds on every Apple target, so macOS runs its tests too. Every framework function used
//! exists on iOS 17, the supported minimum, except the supplemental decoder registration, which
//! is looked up at run time. On other targets the crate is empty.

#[cfg(target_vendor = "apple")]
mod audio;
#[cfg(target_vendor = "apple")]
mod config;
#[cfg(target_vendor = "apple")]
mod sys;
#[cfg(target_vendor = "apple")]
mod video;

#[cfg(target_vendor = "apple")]
pub use audio::{AppleAudio, AudioToolboxAudio, VorbisAudio};
#[cfg(target_vendor = "apple")]
pub use video::{AppleVideo, VideoToolboxVideo};

#[cfg(all(test, target_vendor = "apple"))]
mod tests;
