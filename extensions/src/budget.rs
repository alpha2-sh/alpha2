//! Limits every provider checks before it reads, decodes, or draws a source. A source past one
//! is refused with [`ExtensionError::Budget`], natively and in the browser alike.
//!
//! The limits cover document size ([`document_limit`]), canvas size ([`check_canvas`]), decoded
//! pixels ([`MAX_ANIMATION_PIXELS`], [`MAX_SOURCE_IMAGE_PIXELS`], [`DEFAULT_FRAME_BUDGET`]) and
//! drawing cost ([`MAX_DRAWING`]). Format-specific counts (SVG elements, Lottie draws, dotLottie
//! files) live next to each parser.

use alpha2_model::{
    MAX_LOTTIE_DOCUMENT_BYTES, MAX_PROVIDER_DOCUMENT_BYTES, MAX_SVG_DOCUMENT_BYTES, ProviderSource,
};

use crate::api::ExtensionError;

/// Largest canvas side a provider opens, in pixels.
pub const MAX_CANVAS_SIDE: u32 = 16_384;
/// Largest canvas a provider opens, in pixels (8K UHD).
pub const MAX_CANVAS_PIXELS: u64 = 7_680 * 4_320;
/// Most frames an animated image may have.
pub const MAX_ANIMATION_FRAMES: usize = 10_000;
/// Most pixels all of an animated image's frames may cover together. About a thousand 1080p
/// frames, roughly ten seconds of native decoding.
pub const MAX_ANIMATION_PIXELS: u64 = 1 << 31;
/// Decoded bytes an animated source or an image sequence keeps between requests.
pub const DEFAULT_FRAME_BUDGET: usize = 256 * 1024 * 1024;
/// Most pixels the raster images inside one source may decode to together. An SVG counts every
/// image draw per render (an image reused through `<use>` counts twice); Lottie counts its assets
/// once at open.
pub const MAX_SOURCE_IMAGE_PIXELS: u64 = 4 * MAX_CANVAS_PIXELS;

/// Most canvases of blended fill one SVG render or Lottie frame may draw. A 1080p canvas of
/// blended fill costs about 4 to 10 ms natively, so this is about 2 s. Opaque fills are not
/// discounted, since whether something above fades is unknown before drawing.
#[cfg(any(feature = "svg", feature = "lottie"))]
pub(crate) const MAX_DRAWING: f64 = 256.0;

/// Minimum canvas an SVG counts as for [`MAX_DRAWING`], since a small icon can still draw many
/// blurs. Lottie uses its own canvas because all its layers are frame-sized.
#[cfg(feature = "svg")]
pub(crate) const FLOOR_CANVAS: f64 = 1920.0 * 1080.0;

/// Refuses a canvas larger than a provider opens.
///
/// # Errors
/// [`ExtensionError::Budget`] or, for an empty canvas,
/// [`ExtensionError::Malformed`].
pub fn check_canvas(width: u32, height: u32) -> Result<(), ExtensionError> {
    if width == 0 || height == 0 {
        return Err(ExtensionError::Malformed("the canvas is empty".into()));
    }
    if width > MAX_CANVAS_SIDE
        || height > MAX_CANVAS_SIDE
        || u64::from(width) * u64::from(height) > MAX_CANVAS_PIXELS
    {
        return Err(ExtensionError::Budget(format!(
            "a {width}x{height} canvas is larger than {MAX_CANVAS_PIXELS} pixels"
        )));
    }
    Ok(())
}

/// The most bytes a provider reads of a whole-file format (SVG, Lottie JSON, dotLottie), with a
/// name for the file kind; `None` for streamed formats. `head` is the file's start, used to tell
/// a dotLottie ZIP from Lottie JSON.
#[must_use]
pub fn document_limit(source: &ProviderSource, head: &[u8]) -> Option<(usize, &'static str)> {
    match source {
        ProviderSource::Svg => Some((MAX_SVG_DOCUMENT_BYTES, "SVG document")),
        ProviderSource::Lottie { .. } if head.starts_with(b"PK\x03\x04") => {
            Some((MAX_PROVIDER_DOCUMENT_BYTES, "dotLottie package"))
        }
        ProviderSource::Lottie { .. } => Some((MAX_LOTTIE_DOCUMENT_BYTES, "Lottie file")),
        _ => None,
    }
}

/// Refuses a source file of `length` bytes larger than its [`document_limit`].
///
/// # Errors
/// [`ExtensionError::Budget`].
pub fn check_document_size(
    source: &ProviderSource,
    head: &[u8],
    length: u64,
) -> Result<(), ExtensionError> {
    match document_limit(source, head) {
        Some((limit, what)) if length > limit as u64 => Err(ExtensionError::Budget(format!(
            "the {what} is {length} bytes, more than the {limit} a provider reads"
        ))),
        _ => Ok(()),
    }
}

/// Refuses images that together decode to more than [`MAX_SOURCE_IMAGE_PIXELS`].
///
/// # Errors
/// [`ExtensionError::Budget`].
pub fn check_image_pixels(pixels: u64) -> Result<(), ExtensionError> {
    if pixels > MAX_SOURCE_IMAGE_PIXELS {
        return Err(ExtensionError::Budget(format!(
            "its images decode to {pixels} pixels, more than the {MAX_SOURCE_IMAGE_PIXELS} one source may"
        )));
    }
    Ok(())
}
