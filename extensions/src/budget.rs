//! What one provider call may cost, in one place: the limits every provider
//! checks a source against before it reads, decodes, or draws it. A source
//! past one is refused with [`ExtensionError::Budget`], the same way natively
//! and in the browser module, as both run this code.
//!
//! The limits are on different axes and are not folded into one number:
//! - **document size**: the most bytes of a source's own file a provider
//!   reads, for the formats it reads whole ([`document_limit`]);
//! - **canvas**: the largest canvas a provider opens ([`check_canvas`]);
//! - **decoded pixels**: what an animated image's frames cover together
//!   ([`MAX_ANIMATION_PIXELS`]) and what the images inside one source decode
//!   to ([`MAX_SOURCE_IMAGE_PIXELS`]), and the decoded bytes a source keeps
//!   between requests ([`DEFAULT_FRAME_BUDGET`]);
//! - **drawing**: what drawing one SVG render or one Lottie frame costs, in
//!   canvases of blended fill ([`MAX_DRAWING`]).
//!
//! Each provider also counts what its own format can run away on (an SVG's
//! elements and path segments, a Lottie frame's draws and nesting, a
//! dotLottie package's files) next to its parser, and refuses past those
//! limits with the same error.

use alpha2_model::{
    MAX_LOTTIE_DOCUMENT_BYTES, MAX_PROVIDER_DOCUMENT_BYTES, MAX_SVG_DOCUMENT_BYTES, ProviderSource,
};

use crate::api::ExtensionError;

/// Largest canvas side a provider opens, in pixels.
pub const MAX_CANVAS_SIDE: u32 = 16_384;
/// Largest canvas a provider opens, in pixels: 8K UHD.
pub const MAX_CANVAS_PIXELS: u64 = 7_680 * 4_320;
/// Most frames an animated image may have.
pub const MAX_ANIMATION_FRAMES: usize = 10_000;
/// Most pixels an animated image's frames may cover, all frames together:
/// what one pass through it decodes and composites, whatever its canvas.
/// About a thousand whole 1080p frames, which one pass decodes in about ten
/// seconds natively.
pub const MAX_ANIMATION_PIXELS: u64 = 1 << 31;
/// Decoded bytes an animated source or an image sequence keeps between
/// requests.
pub const DEFAULT_FRAME_BUDGET: usize = 256 * 1024 * 1024;
/// Most pixels the raster images inside one source may decode to, all of
/// them together: four whole canvases of [`MAX_CANVAS_PIXELS`]. An SVG
/// decodes every image it draws on each render (an image drawn twice, as
/// through `<use>`, counts twice) and a Lottie animation decodes its image
/// assets once, when it opens, and holds them; each image is also within
/// [`check_canvas`] on its own. A source past it is refused.
pub const MAX_SOURCE_IMAGE_PIXELS: u64 = 4 * MAX_CANVAS_PIXELS;

/// Most canvases' worth of drawing one provider call may cost: each pixel of
/// every fill, stroke, layer, mask, clip, tile, and filter primitive an SVG
/// render draws, and each fill and stroke a Lottie frame paints, weighted by
/// what it costs against a fill blended over the canvas. A 1080p canvas of
/// blended fill (a gradient, or a solid fill made translucent) costs about
/// 4 to 10 ms natively, so this is about 2 s. An opaque solid fill is not
/// counted for less, as whether something above it fades is not known before
/// it is drawn.
#[cfg(any(feature = "svg", feature = "lottie"))]
pub(crate) const MAX_DRAWING: f64 = 256.0;

/// The least an SVG's canvas counts as for [`MAX_DRAWING`]: 1920x1080, as
/// what is bounded is time and a small icon may draw many blurs. A Lottie
/// frame counts in its own canvas, as everything it paints is drawn onto
/// layers the size of the frame.
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

/// The most bytes a provider reads of a source's own file, for the formats
/// it reads whole: a dotLottie package ([`MAX_PROVIDER_DOCUMENT_BYTES`]), a
/// Lottie JSON file ([`MAX_LOTTIE_DOCUMENT_BYTES`]), and an SVG document
/// ([`MAX_SVG_DOCUMENT_BYTES`]), with what the file is called; `None` for
/// the formats a provider decodes as it goes. `head` is the file's start: a
/// Lottie source's file is a package when it starts as a ZIP file does.
///
/// A host checks a file's size against it before reading it, and every
/// provider checks the bytes it is given before parsing them.
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

/// Refuses a source's file of `length` bytes larger than its
/// [`document_limit`], before it is read or parsed.
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

/// Refuses images that together decode to more than
/// [`MAX_SOURCE_IMAGE_PIXELS`].
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
