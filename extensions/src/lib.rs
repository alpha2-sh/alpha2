//! Extension providers: frames of formats the host's video and image
//! decoders do not read, supplied through one contract.
//!
//! A provider probes bytes (never a file name) and opens a source that
//! answers frames by source time. The host registers the providers its build
//! includes, in priority order, and feeds their frames to the engine like any
//! other source's; the engine and the renderer know nothing of them. Each
//! format is a cargo feature: a build without one still renders everything
//! else, and reports that format's sources as unsupported.

#[cfg(feature = "animated-image")]
mod animated;
mod api;
mod budget;
#[cfg(feature = "exr")]
mod exr;
#[cfg(feature = "lottie")]
mod lottie;
mod progress;
#[cfg(any(
    feature = "animated-image",
    feature = "image-sequence",
    feature = "lottie"
))]
mod raster;
mod registry;
#[cfg(feature = "image-sequence")]
mod sequence;
#[cfg(feature = "svg")]
mod svg;
#[cfg(test)]
mod tests;

#[cfg(feature = "animated-image")]
pub use animated::AnimatedImageProvider;
pub use api::{
    ColorOrigin, Diagnostic, ExtensionError, Format, FramePixels, LinearColor,
    MAX_NORMALIZED_DELAY_MS, NORMALIZED_DELAY_MS, NoResources, ProbeReport, ResourceResolver,
    SharedResolver, Timing, VisualFrame, VisualProvider, VisualSource, premultiply,
};
pub use budget::{
    DEFAULT_FRAME_BUDGET, MAX_ANIMATION_FRAMES, MAX_ANIMATION_PIXELS, MAX_CANVAS_PIXELS,
    MAX_CANVAS_SIDE, MAX_SOURCE_IMAGE_PIXELS, check_canvas, check_document_size,
    check_image_pixels, document_limit,
};
#[cfg(feature = "exr")]
pub use exr::ExrProvider;
#[cfg(feature = "lottie")]
pub use lottie::LottieProvider;
pub use progress::{ProgressWatch, watch_progress};
pub use registry::Registry;
#[cfg(feature = "image-sequence")]
pub use sequence::open_sequence;
#[cfg(feature = "svg")]
pub use svg::SvgProvider;
