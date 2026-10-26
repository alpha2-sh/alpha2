//! Software H.264 through Cisco's `OpenH264` binary, loaded at run time.
//!
//! The bundled `FFmpeg` runtime is LGPL and has no CPU H.264 encoder on Linux
//! or macOS. `OpenH264` fills that gap without changing the runtime: the user
//! installs Cisco's binary on request, and this module loads it
//! ([`library`]), drives it ([`ffi`]) and turns its output into the packets
//! and sample description the muxers take ([`encoder`]). Its encoder is named
//! `openh264`, apart from `FFmpeg`'s own `libopenh264` wrapper, and comes after
//! every hardware encoder and `libx264` in preference.

mod encoder;
mod ffi;
mod library;

pub(crate) use encoder::{OpenH264Encoder, OpenH264Rate, OpenH264Settings};
pub use library::{
    OPENH264_INSTALL_COMMAND, OPENH264_NOTICE, OPENH264_VERSION, OpenH264Library, OpenH264Provider,
    OpenH264Status, openh264_status,
};

/// The encoder name `--video-encoder` takes for `OpenH264`.
pub const OPENH264_ENCODER_NAME: &str = "openh264";

#[cfg(test)]
mod tests;
