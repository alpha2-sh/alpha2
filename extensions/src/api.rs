//! The provider contract: probe reports, frame requests, and frames.

use std::fmt;
use std::sync::Arc;

use alpha2_model::{LinearPrimaries, ProviderSource};
use alpha2_time::{FrameRate, MediaTime};
use serde::Serialize;

/// Frame delays at or below this are shown for [`NORMALIZED_DELAY_MS`] and reported. This
/// catches GIF delays of 0 and 1 hundredths while 60 fps (16 or 17 ms) plays as declared.
pub const MAX_NORMALIZED_DELAY_MS: u32 = 10;
/// What a delay of [`MAX_NORMALIZED_DELAY_MS`] or less becomes.
pub const NORMALIZED_DELAY_MS: u32 = 100;

/// A format a provider reads.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Format {
    /// A static SVG document.
    Svg,
    /// A GIF.
    Gif,
    /// An APNG.
    Apng,
    /// An animated WebP.
    Webp,
    /// A numbered PNG sequence.
    PngSequence,
    /// A Lottie JSON animation.
    Lottie,
    /// A dotLottie package of one or more Lottie animations.
    DotLottie,
    /// A still OpenEXR image.
    Exr,
    /// A numbered OpenEXR sequence.
    ExrSequence,
    /// A LaTeX math formula.
    Latex,
    /// A page of a PDF document.
    Pdf,
}

/// When a source's frames are shown, in source time.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Timing {
    /// One picture, whatever the time.
    Static,
    /// Frames shown for their own durations, in ticks, one after another.
    Frames {
        /// Each frame's duration, already normalized.
        durations: Vec<i64>,
    },
    /// Frames at a fixed rate.
    Rate {
        /// The rate.
        rate: FrameRate,
        /// How many frames.
        frames: usize,
    },
}

impl Timing {
    /// How long one cycle lasts, or `None` for a static picture.
    #[must_use]
    pub fn duration(&self) -> Option<MediaTime> {
        match self {
            Self::Static => None,
            Self::Frames { durations } => Some(MediaTime::from_ticks(durations.iter().sum())),
            Self::Rate { rate, frames } => Some(frame_start(*rate, *frames)),
        }
    }

    /// How many frames there are.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        match self {
            Self::Static => 1,
            Self::Frames { durations } => durations.len(),
            Self::Rate { frames, .. } => *frames,
        }
    }

    /// The frame shown at `time` and the source time it covers. Times outside the source clamp to
    /// the first or last frame; looping is the clip's job.
    #[must_use]
    pub fn frame_at(&self, time: MediaTime) -> (usize, MediaTime, Option<MediaTime>) {
        let ticks = time.ticks();
        match self {
            Self::Static => (0, MediaTime::from_ticks(i64::MIN), None),
            Self::Frames { durations } => {
                let mut start = 0_i64;
                for (index, duration) in durations.iter().enumerate() {
                    let end = start + duration;
                    if ticks < end || index + 1 == durations.len() {
                        let first = if index == 0 { i64::MIN } else { start };
                        let last =
                            (index + 1 < durations.len()).then(|| MediaTime::from_ticks(end));
                        return (index, MediaTime::from_ticks(first), last);
                    }
                    start = end;
                }
                (0, MediaTime::from_ticks(i64::MIN), None)
            }
            Self::Rate { rate, frames } => {
                let index = rate
                    .frame_at_or_before(time)
                    .map_or(usize::MAX, |index| {
                        usize::try_from(index.max(0)).unwrap_or(usize::MAX)
                    })
                    .min(frames - 1);
                let first = if index == 0 {
                    MediaTime::from_ticks(i64::MIN)
                } else {
                    frame_start(*rate, index)
                };
                let last = (index + 1 < *frames).then(|| frame_start(*rate, index + 1));
                (index, first, last)
            }
        }
    }
}

/// Uses the engine's [`FrameRate::frame_start`] boundary, so a source at the project's rate shows
/// frame `k` at output frame `k`.
fn frame_start(rate: FrameRate, index: usize) -> MediaTime {
    i64::try_from(index)
        .ok()
        .and_then(|index| rate.frame_start(index))
        .unwrap_or(MediaTime::from_ticks(i64::MAX))
}

/// Something a provider did that the user should know about.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Diagnostic {
    /// A frame's delay was [`MAX_NORMALIZED_DELAY_MS`] or less and is shown for
    /// [`NORMALIZED_DELAY_MS`].
    NormalizedDelay {
        /// The frame's index.
        frame: usize,
        /// Its declared delay, in milliseconds.
        declared_ms: u32,
    },
    /// Text names a font no registered font provides; it is not drawn.
    MissingFont {
        /// The font families the text asks for.
        families: String,
    },
    /// Fonts a document names but does not embed. Their text is drawn with the nearest standard
    /// font, so its glyphs may differ from the author's.
    SubstitutedFont {
        /// The fonts substituted.
        families: String,
    },
    /// A referenced image is not registered or is too large; it is not drawn.
    MissingImage {
        /// The reference.
        href: String,
    },
    /// A feature the provider does not draw; it is left out, never silently approximated.
    UnsupportedFeature {
        /// What it is.
        feature: String,
        /// Where it first appears (a JSON pointer).
        path: String,
        /// How many places use it.
        count: usize,
    },
    /// A referenced image is registered but cannot be decoded; it is not drawn.
    UnreadableImage {
        /// The reference.
        href: String,
        /// Why it cannot be drawn.
        reason: String,
    },
    /// A package item that does not open; the other items can still be chosen.
    UnreadableItem {
        /// The item.
        item: String,
        /// Why it does not open.
        reason: String,
    },
    /// Samples that were NaN or infinite; each is shown as `0`.
    NonFiniteSamples {
        /// How many.
        count: usize,
    },
    /// Finite 32-bit float samples beyond the half-float range; each is clamped to it.
    OutOfHalfRange {
        /// How many.
        count: usize,
    },
    /// Channels stored as 32-bit floats are shown at half-float precision.
    ReducedPrecision,
}

/// The primaries of linear-light media and where they came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct LinearColor {
    /// The named primaries the chromaticities are, if any.
    pub primaries: Option<LinearPrimaries>,
    /// The red, green, blue, and white chromaticities, CIE 1931 `[x, y]`.
    pub chromaticities: [[f32; 2]; 4],
    /// Where they come from.
    pub origin: ColorOrigin,
}

/// Where a source's color description comes from.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorOrigin {
    /// The file declares it.
    File,
    /// The file does not say and the format defines a default.
    FormatDefault,
    /// The project says so, overriding the file.
    Project,
}

/// What probing a file finds.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ProbeReport {
    /// The format.
    pub format: Format,
    /// The canvas width, in pixels.
    pub width: u32,
    /// The canvas height, in pixels.
    pub height: u32,
    /// When its frames are shown.
    pub timing: Timing,
    /// Plays the file asks for (`0` for forever), or `None` if unstated. Only a hint for import.
    pub loop_count: Option<u32>,
    /// The items a package holds, such as a dotLottie's animations; empty for a single item.
    pub items: Vec<String>,
    /// The package item described: the one opened, or for a probe the first that opens. `None`
    /// for a single item.
    pub item: Option<String>,
    /// The color of linear-light media; `None` for sRGB media.
    pub color: Option<LinearColor>,
    /// What the user should know.
    pub diagnostics: Vec<Diagnostic>,
}

/// A frame's tightly packed pixels, in the source's precision. Shared with the source's cache,
/// so repeated frames cost no copy.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FramePixels {
    /// Premultiplied, sRGB-encoded RGBA8.
    Rgba8(Arc<[u8]>),
    /// Premultiplied linear-light RGBA as IEEE 754 half-float bits, BT.709 primaries, `1.0` at
    /// SDR white; values outside `0.0..=1.0` are kept.
    LinearF16(Arc<[u16]>),
}

impl FramePixels {
    /// The bytes the pixels take.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Rgba8(pixels) => pixels.len(),
            Self::LinearF16(halves) => halves.len() * 2,
        }
    }

    /// The RGBA8 pixels, or `None` for another precision.
    #[must_use]
    pub fn rgba8(&self) -> Option<&[u8]> {
        match self {
            Self::Rgba8(pixels) => Some(pixels),
            Self::LinearF16(_) => None,
        }
    }

    /// The linear half-float pixels, or `None` for another precision.
    #[must_use]
    pub fn linear_f16(&self) -> Option<&[u16]> {
        match self {
            Self::LinearF16(halves) => Some(halves),
            Self::Rgba8(_) => None,
        }
    }
}

/// One frame of a source.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VisualFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The pixels.
    pub pixels: FramePixels,
    /// Which of the source's frames this is.
    pub index: usize,
    /// The first source time it is shown at (`i64::MIN` ticks for the first frame).
    pub start: MediaTime,
    /// The first source time it is no longer shown at, or `None` for the last frame.
    pub end: Option<MediaTime>,
    /// Per-frame diagnostics, such as non-finite samples in one frame of a sequence.
    pub diagnostics: Vec<Diagnostic>,
}

/// Why a provider could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtensionError {
    /// No provider reads these bytes, or the requested provider is not in this build.
    Unsupported(String),
    /// The bytes are not a well-formed file of the format.
    Malformed(String),
    /// The file is larger than a provider opens.
    Budget(String),
    /// A resource the source needs is not registered.
    MissingResource(String),
    /// The media is not what the project says it is.
    Mismatch(String),
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            Self::Malformed(message) => write!(formatter, "malformed: {message}"),
            Self::Budget(message) => write!(formatter, "too large: {message}"),
            Self::MissingResource(locator) => write!(formatter, "missing resource {locator}"),
            Self::Mismatch(message) => write!(formatter, "mismatch: {message}"),
        }
    }
}

impl std::error::Error for ExtensionError {}

/// Supplies bytes a source needs besides its own (sequence frames, SVG fonts and images).
/// Parsers may call it from other threads, hence `Send + Sync`.
pub trait ResourceResolver: Send + Sync {
    /// The bytes registered under `locator`.
    ///
    /// # Errors
    /// [`ExtensionError::MissingResource`] when none are registered.
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError>;

    /// Every registered font file, for documents that draw text.
    fn fonts(&self) -> Vec<Arc<[u8]>> {
        Vec::new()
    }
}

/// A resolver a source may keep, such as an image sequence that reads frames on demand.
pub type SharedResolver = Arc<dyn ResourceResolver>;

/// A resolver with nothing registered.
pub struct NoResources;

impl ResourceResolver for NoResources {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        Err(ExtensionError::MissingResource(locator.to_owned()))
    }
}

/// A source a provider opened: asked for frames by source time.
pub trait VisualSource: Send {
    /// What the file is.
    fn report(&self) -> &ProbeReport;

    /// The frame shown at `time`. Vector sources rasterize to fit within `fit` (width, height),
    /// keeping aspect; raster sources use their own size.
    ///
    /// # Errors
    /// If the frame cannot be produced.
    fn render(&mut self, time: MediaTime, fit: (u32, u32)) -> Result<VisualFrame, ExtensionError>;

    /// Locators rendering at `time` will read that the source does not already hold.
    fn needs(&self, _time: MediaTime) -> Vec<String> {
        Vec::new()
    }

    /// Bytes the source holds between renders (cached frames, images, and file bytes).
    fn resident_bytes(&self) -> usize;

    /// Another independently positioned reader sharing this source's decoded data and frame
    /// budget. `None` when any time is equally cheap to draw, so the host can share the source.
    fn reader(&self) -> Option<Box<dyn VisualSource>> {
        None
    }

    /// The part of [`Self::resident_bytes`] shared with [`Self::reader`] readers, counted once.
    fn shared_bytes(&self) -> usize {
        0
    }
}

/// Reads one family of formats.
pub trait VisualProvider: Send + Sync {
    /// The provider's name, for diagnostics.
    fn name(&self) -> &'static str;

    /// What `bytes` are, or `None` when they are not this provider's.
    ///
    /// # Errors
    /// If bytes that look like this provider's format cannot be read.
    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>>;

    /// Opens `bytes` as the project's `source`, applying its settings. Open runs the same
    /// signature checks as [`Self::probe`], so callers need not probe first.
    ///
    /// # Errors
    /// [`ExtensionError::Mismatch`] for another provider's bytes, or the reason they do not open.
    fn open(
        &self,
        bytes: Arc<[u8]>,
        source: &ProviderSource,
        resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError>;
}

/// Premultiplies straight RGBA8 in place, rounding to nearest.
pub fn premultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = u8::try_from((u16::from(*channel) * alpha + 127) / 255).unwrap_or(255);
        }
    }
}
