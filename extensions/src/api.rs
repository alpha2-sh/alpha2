//! The contract every provider keeps: what probing a file reports, how a
//! source is asked for a frame, and what it answers.

use std::fmt;
use std::sync::Arc;

use alpha2_model::{LinearPrimaries, ProviderSource};
use alpha2_time::{FrameRate, MediaTime};
use serde::Serialize;

/// The longest delay not taken as meant: a frame declaring this or less
/// (zero included) is shown for [`NORMALIZED_DELAY_MS`] and reported. A GIF
/// counts delays in hundredths of a second, so its 0 and 1 are the delays
/// normalized; APNG and WebP declare frame times exactly, and a longer delay
/// (60 fps is 16 or 17 ms) plays as declared, in every format alike.
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
        /// Each frame's duration: longer than [`MAX_NORMALIZED_DELAY_MS`], or
        /// normalized to [`NORMALIZED_DELAY_MS`].
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

    /// The frame shown at `time` and the source time it covers: before the
    /// first frame the first answers, past the last the last, as a video's
    /// frames do. Repetition is the clip's, never the source's.
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

/// When frame `index` starts at `rate`: the engine's frame boundary,
/// [`FrameRate::frame_start`], the tick its exact start falls in. Output
/// frames are sampled there, so a source at the project's rate shows frame
/// `k` at output frame `k`.
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
    /// An image the document references is not registered, or declares a
    /// size larger than a provider opens; it is not drawn.
    MissingImage {
        /// The reference.
        href: String,
    },
    /// A feature the provider does not draw, where it first appears and how
    /// many times it does: it is left out, never approximated silently.
    UnsupportedFeature {
        /// What it is.
        feature: String,
        /// Where it first appears (a JSON pointer).
        path: String,
        /// How many places use it.
        count: usize,
    },
    /// An image the document references is registered but cannot be drawn
    /// (a format an SVG image is not drawn from, or an SVG that does not
    /// parse), and why; it is not drawn.
    UnreadableImage {
        /// The reference.
        href: String,
        /// Why it cannot be drawn.
        reason: String,
    },
    /// An item of a package that does not open, and why; the package's
    /// others can still be chosen.
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
    /// Finite 32-bit float samples beyond the largest half float; each is
    /// shown as the largest half float of its sign.
    OutOfHalfRange {
        /// How many.
        count: usize,
    },
    /// Channels stored as 32-bit floats are shown at half-float precision,
    /// about three decimal digits.
    ReducedPrecision,
}

/// What linear-light media's RGB is: the primaries it is in, and where that
/// was learned.
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
    /// How many times the file asks to be played, `0` for ever (a GIF's
    /// repetitions plus its first play; once without a loop extension);
    /// `None` when it does not say. The clip decides; this is a hint for
    /// import.
    pub loop_count: Option<u32>,
    /// The items a package holds, such as a dotLottie's animations. Empty
    /// for a file of one item.
    pub items: Vec<String>,
    /// The item of a package the report describes: the one a source opened
    /// shows, or for a probe the first that opens (the others that do not
    /// are reported). `None` for a file of one item.
    pub item: Option<String>,
    /// For linear-light media, what its RGB is; `None` for sRGB media.
    pub color: Option<LinearColor>,
    /// What the user should know.
    pub diagnostics: Vec<Diagnostic>,
}

/// A frame's pixels, rows packed without padding, in the precision its
/// source has. Shared, never copied, with what the source keeps: a frame a
/// source answers again (a still, a frame it holds) costs no copy.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FramePixels {
    /// Premultiplied, sRGB-encoded RGBA8.
    Rgba8(Arc<[u8]>),
    /// Premultiplied linear-light RGBA as IEEE 754 half floats (their bits),
    /// in BT.709 primaries with `1.0` at SDR white; values above `1.0` and
    /// below `0.0` are kept.
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
    /// The first source time it is shown at (`i64::MIN` ticks for the
    /// first frame, which also answers earlier times).
    pub start: MediaTime,
    /// The first source time it is no longer shown at, or `None` when it is
    /// the last.
    pub end: Option<MediaTime>,
    /// What producing this frame found that the user should know, such as
    /// non-finite samples in one frame of a sequence.
    pub diagnostics: Vec<Diagnostic>,
}

/// Why a provider could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtensionError {
    /// No registered provider reads these bytes, or the provider asked for
    /// is not in this build.
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

/// Bytes a source needs besides its own: an image sequence's frames, and the
/// fonts and images an SVG names. A provider never fetches anything else.
/// It may be asked from inside a parser (usvg asks for each image it
/// draws), which requires it to be shared across threads.
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

/// A resolver a source may keep: an image sequence reads each frame from it
/// when it shows that frame, never all of them when it opens.
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

    /// The frame shown at `time`. A vector source is rasterized to fit
    /// within `fit` (width, height), keeping its shape; a raster source
    /// answers at its own size.
    ///
    /// # Errors
    /// Why the frame cannot be produced.
    fn render(&mut self, time: MediaTime, fit: (u32, u32)) -> Result<VisualFrame, ExtensionError>;

    /// The resources rendering at `time` reads, by locator: those the source
    /// does not already hold. A host that registers resources only while
    /// they are used (a page's frames) registers these before the render
    /// and releases them after.
    fn needs(&self, _time: MediaTime) -> Vec<String> {
        Vec::new()
    }

    /// The bytes the source holds between renders: decoded frames and
    /// images it keeps, its last rendered frame, and its file's bytes when
    /// it keeps them. A host that bounds what its open sources hold counts
    /// this, and closes a source to free it.
    fn resident_bytes(&self) -> usize;

    /// Another reader of this open source, positioned on its own: what it
    /// decodes and where it is decoding are its own, and what the source
    /// decoded when it opened (its file, and for an animation its checked
    /// frames and the frames kept between requests, within one frame
    /// budget) is shared with this one and every other reader made from
    /// either. `None` for a source whose readers need nothing of their own,
    /// one that draws any time as cheaply: a host shares that source.
    fn reader(&self) -> Option<Box<dyn VisualSource>> {
        None
    }

    /// Of [`Self::resident_bytes`], what this source shares with the
    /// readers made by [`Self::reader`]: the same bytes for each of them,
    /// which a host counts once.
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
    /// Why bytes that look like this provider's format cannot be read.
    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>>;

    /// Opens `bytes` as the project's `source`, whose settings (such as
    /// which item of a package it shows) the provider applies. Bytes that
    /// are not this provider's, those [`Self::probe`] answers `None` for,
    /// are told apart here, from the same signature checks: opening never
    /// probes first, so a file is read once.
    ///
    /// # Errors
    /// [`ExtensionError::Mismatch`] for bytes that are not this provider's,
    /// or why they cannot be opened.
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
