// OpenEXR expectations are exact: every value is a half float.
#![allow(clippy::float_cmp, clippy::cast_precision_loss)]

#[cfg(any(feature = "svg", feature = "image-sequence"))]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alpha2_model::{MissingFrames, ProviderSource};
use alpha2_time::{FrameRate, MediaTime};

#[cfg(any(
    feature = "svg",
    feature = "animated-image",
    feature = "lottie",
    feature = "exr"
))]
use crate::Diagnostic;
#[cfg(any(feature = "image-sequence", feature = "exr"))]
use crate::FramePixels;
#[cfg(any(feature = "animated-image", feature = "image-sequence"))]
use crate::premultiply;
use crate::{ExtensionError, Format, NoResources, Registry, ResourceResolver, Timing};

mod animations;
#[cfg(feature = "exr")]
mod exr_images;
#[cfg(any(
    feature = "svg",
    feature = "animated-image",
    feature = "image-sequence",
    feature = "lottie"
))]
mod image_limits;
#[cfg(feature = "lottie")]
mod lottie_documents;
mod providers;
#[cfg(feature = "image-sequence")]
mod sequences;
#[cfg(feature = "svg")]
mod svg_documents;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/corpus/extensions")
        .join(name)
}

fn bytes(name: &str) -> Arc<[u8]> {
    std::fs::read(corpus(name)).expect("corpus file").into()
}

#[cfg(any(
    feature = "svg",
    feature = "animated-image",
    feature = "image-sequence",
    feature = "lottie",
    feature = "exr"
))]
fn ms(value: i64) -> MediaTime {
    MediaTime::from_ticks(value * 120)
}

#[cfg(any(feature = "svg", feature = "image-sequence"))]
/// Locators resolved to corpus files, and optionally fonts.
#[derive(Clone)]
struct Files {
    files: HashMap<String, Arc<[u8]>>,
    fonts: Vec<Arc<[u8]>>,
}

#[cfg(any(feature = "svg", feature = "image-sequence"))]
impl ResourceResolver for Files {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        self.files
            .get(locator)
            .cloned()
            .ok_or_else(|| ExtensionError::MissingResource(locator.to_owned()))
    }

    fn fonts(&self) -> Vec<Arc<[u8]>> {
        self.fonts.clone()
    }
}

#[cfg(feature = "image-sequence")]
fn sequence_files() -> Files {
    Files {
        files: (7..=10)
            .map(|number| {
                let name = format!("sequence_{number:04}.png");
                (name.clone(), bytes(&name))
            })
            .collect(),
        fonts: Vec::new(),
    }
}

#[cfg(feature = "image-sequence")]
fn sequence(frames: &[Option<&str>], missing: MissingFrames) -> ProviderSource {
    ProviderSource::ImageSequence {
        frames: frames
            .iter()
            .map(|frame| frame.map(str::to_owned))
            .collect(),
        frame_rate: FrameRate::new(10, 1).expect("rate"),
        start_number: 7,
        missing,
        primaries: None,
    }
}

#[cfg(any(feature = "svg", feature = "lottie"))]
fn pixel(frame: &crate::VisualFrame, x: usize, y: usize) -> [u8; 4] {
    let at = (y * frame.width as usize + x) * 4;
    <[u8; 4]>::try_from(&frame.pixels.rgba8().expect("rgba8")[at..at + 4]).expect("pixel")
}

/// The CRC-32 a PNG chunk ends with.
#[cfg(any(
    feature = "animated-image",
    feature = "image-sequence",
    feature = "lottie",
    feature = "svg"
))]
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A GIF (version 89a) of a `width` by `height` screen and `frames` frames, each a
/// `frame`-sized rectangle shown for 100 ms whose data is one pixel.
#[cfg(feature = "animated-image")]
fn declared_gif(width: u16, height: u16, frames: usize, frame: (u16, u16)) -> Vec<u8> {
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&width.to_le_bytes());
    gif.extend_from_slice(&height.to_le_bytes());
    // A global table of two colors: black and white.
    gif.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
    for _ in 0..frames {
        gif.extend_from_slice(&[0x21, 0xf9, 0x04, 0x00, 10, 0, 0, 0]);
        gif.push(0x2c);
        gif.extend_from_slice(&[0, 0, 0, 0]);
        gif.extend_from_slice(&frame.0.to_le_bytes());
        gif.extend_from_slice(&frame.1.to_le_bytes());
        // Clear, color 0, end: one pixel in 3-bit codes.
        gif.extend_from_slice(&[0x00, 0x02, 0x02, 0x44, 0x01, 0x00]);
    }
    gif.push(0x3b);
    gif
}

/// Where a frame's pixels are: two frames that share them share it.
#[cfg(all(
    feature = "svg",
    feature = "animated-image",
    feature = "image-sequence",
    feature = "lottie",
    feature = "exr"
))]
fn buffer(frame: &crate::VisualFrame) -> *const u8 {
    match &frame.pixels {
        FramePixels::Rgba8(pixels) => pixels.as_ptr(),
        FramePixels::LinearF16(halves) => halves.as_ptr().cast(),
    }
}
