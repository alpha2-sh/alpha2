//! Sources whose media an extension provider supplies.

use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

use crate::{MAX_TEXT_CONTENT_BYTES, MediaKind, Rgba};

/// Most frames an image sequence lists.
pub const MAX_SEQUENCE_FRAMES: usize = 100_000;

/// Largest dotLottie package a provider reads, in bytes, refusing larger files unread. Only the
/// shown animation (within [`MAX_LOTTIE_DOCUMENT_BYTES`]) and its images are unpacked.
pub const MAX_PROVIDER_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

/// Largest Lottie animation a provider reads, in bytes (a JSON file or an unpacked dotLottie
/// animation). Parsing can take about 45 times the document size, so this caps it near
/// 750 MiB; larger documents are refused before parsing.
pub const MAX_LOTTIE_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Largest SVG document a provider reads, in bytes. Parsing can take about 30 times the
/// document size, so this caps it near 960 MiB; larger documents are refused before parsing.
pub const MAX_SVG_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;

/// Longest LaTeX formula a source holds, in bytes.
pub const MAX_LATEX_BYTES: usize = MAX_TEXT_CONTENT_BYTES;

/// Largest LaTeX font size, in pixels per em.
pub const MAX_LATEX_FONT_SIZE: f32 = 1_024.0;

/// Largest PDF document a provider reads, in bytes. Larger files are refused before reading.
pub const MAX_PDF_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

/// Most pages a PDF document a provider reads may have.
pub const MAX_PDF_PAGES: u32 = 10_000;

/// A source whose media an extension provider supplies, not the host's video and image decoders.
///
/// A host picks the provider by probing the bytes at import, never by file name, and records
/// it here so every host opens the source the same way. The engine does not read it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "format", rename_all = "snake_case", from = "ProviderSourceWire")]
#[non_exhaustive]
pub enum ProviderSource {
    /// A static SVG document, rasterized at its shown size. The source's kind is `image`.
    Svg,
    /// A GIF, animated WebP, or APNG: one cycle of frames, each with its own duration.
    /// Repetition is the clip's. The source's kind is `video`.
    AnimatedImage,
    /// A Lottie animation (JSON file or dotLottie package), evaluated at its own frames. The
    /// source's kind is `video`.
    Lottie {
        /// The dotLottie animation shown, by ID. Absent means the package's first; always absent
        /// for a Lottie JSON file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        animation: Option<String>,
    },
    /// Still images played in order at a fixed rate. The source's kind is `video`.
    ImageSequence {
        /// Each frame's locator, in play order. `null` marks a missing number, allowed only when
        /// `missing` says how to show it.
        frames: Vec<Option<String>>,
        /// The rate the frames play at.
        frame_rate: FrameRate,
        /// The number of the first frame, as its file was numbered.
        #[serde(default)]
        start_number: u64,
        /// How a missing frame is shown.
        #[serde(default)]
        missing: MissingFrames,
        /// The primaries of OpenEXR frames' linear RGB, overriding the first frame's
        /// chromaticities. Only for OpenEXR frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
    /// A still OpenEXR image: linear-light RGB(A), read at half precision. The source's kind is
    /// `image`.
    Exr {
        /// The primaries of its linear RGB, overriding the file's chromaticities (BT.709 when the
        /// file has none, as OpenEXR defines).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
    /// A LaTeX math formula typeset with the bundled STIX Two Math font. The source's kind is
    /// `image`, and it has no locator or fingerprint since it reads no file. A formula that uses
    /// a character the font lacks, such as Hangul in `\text`, does not open.
    Latex {
        /// Math-mode LaTeX without `$` delimiters. Non-empty, at most [`MAX_LATEX_BYTES`] bytes.
        latex: String,
        /// Pixels per em, above `0` and at most [`MAX_LATEX_FONT_SIZE`].
        font_size: f32,
        /// The ink where the formula sets no `\color` of its own. White when absent.
        #[serde(default = "white")]
        color: Rgba,
        /// Display style, with large operators and limits above and below, or text style when
        /// `false`. Display when absent.
        #[serde(default = "display_style")]
        display: bool,
    },
    /// One page of a PDF document. The source's kind is `image`.
    Pdf {
        /// The page shown, counted from `1`, at most [`MAX_PDF_PAGES`]. The first when absent.
        #[serde(default = "first_page")]
        page: u32,
        /// Painted under the page, which is itself transparent. White paper when absent.
        #[serde(default = "white")]
        background: Rgba,
    },
}

/// The wire form of [`ProviderSource`].
///
/// serde lets an internally tagged unit variant ignore extra fields even with
/// `deny_unknown_fields`; empty struct variants reject them, as the published schema does.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "ProviderSource"))]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProviderSourceWire {
    /// A static SVG document, rasterized at its shown size. The source's kind is `image`.
    Svg {},
    /// A GIF, animated WebP, or APNG: one cycle of frames, each with its own duration.
    /// Repetition is the clip's. The source's kind is `video`.
    AnimatedImage {},
    /// A Lottie animation (JSON file or dotLottie package), evaluated at its own frames. The
    /// source's kind is `video`.
    Lottie {
        /// The dotLottie animation shown, by ID. Absent means the package's first; always absent
        /// for a Lottie JSON file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        animation: Option<String>,
    },
    /// Still images played in order at a fixed rate. The source's kind is `video`.
    ImageSequence {
        /// Each frame's locator, in play order. `null` marks a missing number, allowed only when
        /// `missing` says how to show it.
        frames: Vec<Option<String>>,
        /// The rate the frames play at.
        frame_rate: FrameRate,
        /// The number of the first frame, as its file was numbered.
        #[serde(default)]
        start_number: u64,
        /// How a missing frame is shown.
        #[serde(default)]
        missing: MissingFrames,
        /// The primaries of OpenEXR frames' linear RGB, overriding the first frame's
        /// chromaticities. Only for OpenEXR frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
    /// A still OpenEXR image: linear-light RGB(A), read at half precision. The source's kind is
    /// `image`.
    Exr {
        /// The primaries of its linear RGB, overriding the file's chromaticities (BT.709 when the
        /// file has none, as OpenEXR defines).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
    /// A LaTeX math formula typeset with the bundled STIX Two Math font. The source's kind is
    /// `image`, and it has no locator or fingerprint since it reads no file. A formula that uses
    /// a character the font lacks, such as Hangul in `\text`, does not open.
    Latex {
        /// Math-mode LaTeX without `$` delimiters. Non-empty, at most [`MAX_LATEX_BYTES`] bytes.
        latex: String,
        /// Pixels per em, above `0` and at most [`MAX_LATEX_FONT_SIZE`].
        font_size: f32,
        /// The ink where the formula sets no `\color` of its own. White when absent.
        #[serde(default = "white")]
        color: Rgba,
        /// Display style, with large operators and limits above and below, or text style when
        /// `false`. Display when absent.
        #[serde(default = "display_style")]
        display: bool,
    },
    /// One page of a PDF document. The source's kind is `image`.
    Pdf {
        /// The page shown, counted from `1`, at most [`MAX_PDF_PAGES`]. The first when absent.
        #[serde(default = "first_page")]
        page: u32,
        /// Painted under the page, which is itself transparent. White paper when absent.
        #[serde(default = "white")]
        background: Rgba,
    },
}

impl From<ProviderSourceWire> for ProviderSource {
    fn from(wire: ProviderSourceWire) -> Self {
        match wire {
            ProviderSourceWire::Svg {} => Self::Svg,
            ProviderSourceWire::AnimatedImage {} => Self::AnimatedImage,
            ProviderSourceWire::Lottie { animation } => Self::Lottie { animation },
            ProviderSourceWire::ImageSequence {
                frames,
                frame_rate,
                start_number,
                missing,
                primaries,
            } => Self::ImageSequence {
                frames,
                frame_rate,
                start_number,
                missing,
                primaries,
            },
            ProviderSourceWire::Exr { primaries } => Self::Exr { primaries },
            ProviderSourceWire::Latex {
                latex,
                font_size,
                color,
                display,
            } => Self::Latex {
                latex,
                font_size,
                color,
                display,
            },
            ProviderSourceWire::Pdf { page, background } => Self::Pdf { page, background },
        }
    }
}

const fn white() -> Rgba {
    Rgba::new(1.0, 1.0, 1.0, 1.0)
}

const fn display_style() -> bool {
    true
}

const fn first_page() -> u32 {
    1
}

/// Named RGB primaries linear-light media may be in, each with its own white point.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LinearPrimaries {
    /// ITU-R BT.709 and sRGB, D65 white.
    Bt709,
    /// ITU-R BT.2020, D65 white.
    Bt2020,
    /// DCI-P3 primaries with a D65 white, as Display P3.
    P3D65,
    /// ACES AP0 (SMPTE ST 2065-1), the ACES white.
    AcesAp0,
    /// ACES AP1 (ACEScg), the ACES white.
    AcesAp1,
}

/// How an image sequence shows a frame missing from its numbering.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MissingFrames {
    /// A sequence with a missing frame is refused.
    #[default]
    Reject,
    /// The frame before it is shown.
    Hold,
    /// Nothing is shown: a transparent frame.
    Blank,
}

impl ProviderSource {
    /// The kind of source the provider's media is.
    #[must_use]
    pub const fn kind(&self) -> MediaKind {
        match self {
            Self::Svg | Self::Exr { .. } | Self::Latex { .. } | Self::Pdf { .. } => {
                MediaKind::Image
            }
            Self::AnimatedImage | Self::Lottie { .. } | Self::ImageSequence { .. } => {
                MediaKind::Video
            }
        }
    }

    /// Whether the provider reads a file of the source's own. One that does not, such as LaTeX,
    /// holds its content in these settings, so its source has no locator or fingerprint.
    #[must_use]
    pub const fn reads_file(&self) -> bool {
        !matches!(self, Self::Latex { .. })
    }

    /// Whether the settings are coherent and within the limits each variant's fields state.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Lottie {
                animation: Some(animation),
            } => return !animation.is_empty(),
            Self::Latex {
                latex,
                font_size,
                color,
                ..
            } => {
                return !latex.is_empty()
                    && latex.len() <= MAX_LATEX_BYTES
                    && *font_size > 0.0
                    && *font_size <= MAX_LATEX_FONT_SIZE
                    && color.is_valid();
            }
            Self::Pdf { page, background } => {
                return (1..=MAX_PDF_PAGES).contains(page) && background.is_valid();
            }
            _ => {}
        }
        let Self::ImageSequence {
            frames, missing, ..
        } = self
        else {
            return true;
        };
        let present = frames.iter().flatten().collect::<Vec<_>>();
        let unique = present
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == present.len();
        !frames.is_empty()
            && frames.len() <= MAX_SEQUENCE_FRAMES
            && frames[0].is_some()
            && unique
            && present.iter().all(|locator| !locator.is_empty())
            && (present.len() == frames.len() || *missing != MissingFrames::Reject)
    }
}
