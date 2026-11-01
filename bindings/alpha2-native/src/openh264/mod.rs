//! Software H.264 through Cisco's `OpenH264` binary, loaded at run time.
//!
//! The bundled LGPL `FFmpeg` has no CPU H.264 encoder on Linux or macOS. This module loads the
//! user-installed binary ([`library`]), drives it ([`ffi`]) and produces packets and a sample
//! description ([`encoder`]). Its encoder name is `openh264` (distinct from `FFmpeg`'s
//! `libopenh264`) and it ranks after every hardware encoder and `libx264`.

mod encoder;
mod ffi;
mod library;

pub(crate) use encoder::{
    OpenH264Encoder, OpenH264Rate, OpenH264Settings, slice_count, thread_count,
};
pub use library::{
    OPENH264_INSTALL_COMMAND, OPENH264_NOTICE, OPENH264_VERSION, OpenH264Library, OpenH264Provider,
    OpenH264Status, openh264_status,
};

/// The encoder name `--video-encoder` takes for `OpenH264`.
pub const OPENH264_ENCODER_NAME: &str = "openh264";

#[cfg(test)]
mod tests;
