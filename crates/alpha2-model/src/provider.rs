//! Sources whose media an extension provider supplies.

use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

use crate::MediaKind;

/// Most frames an image sequence lists.
pub const MAX_SEQUENCE_FRAMES: usize = 100_000;

/// Largest dotLottie package a provider reads, in bytes. A package is read
/// whole, and only the files a source shows are unpacked from it: its
/// animation, within [`MAX_LOTTIE_DOCUMENT_BYTES`], and that animation's
/// images. A larger file is refused before it is read.
pub const MAX_PROVIDER_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

/// Largest Lottie animation a provider reads, in bytes: a Lottie JSON file,
/// or the animation a dotLottie package unpacks to. A Lottie document is
/// parsed whole, to as much as about 45 times its size at its densest
/// (numbers alone), so a larger one is refused before it is parsed, and a
/// host refuses a larger file before reading it. Within it, a document
/// parses to at most about 750 MiB.
pub const MAX_LOTTIE_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Largest SVG document a provider reads, in bytes. An SVG document is
/// parsed whole, to as much as about 30 times its size at its densest
/// (elements alone), so a larger one is refused before it is parsed, and a
/// host refuses a larger file before reading it. Within it, a document
/// parses to at most about 960 MiB.
pub const MAX_SVG_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;

/// A source whose media an extension provider supplies, rather than the
/// host's video and image decoders.
///
/// A host picks the provider by probing the bytes at import, never by a file
/// name, and records it here with the settings the source needs, so every
/// host opens the source the same way. The engine does not read it: a
/// provider source needs frames like any video or image source.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "format", rename_all = "snake_case", from = "ProviderSourceWire")]
#[non_exhaustive]
pub enum ProviderSource {
    /// A static SVG document, rasterized at the size it is shown. The
    /// source's kind is `image`.
    Svg,
    /// A GIF, animated WebP, or APNG: one cycle of frames, each with its own
    /// duration. Repetition is the clip's. The source's kind is `video`.
    AnimatedImage,
    /// A Lottie animation, from a Lottie JSON file or a dotLottie package,
    /// evaluated at its own frames. The source's kind is `video`.
    Lottie {
        /// The dotLottie animation shown, by its ID; the package's first
        /// when absent, and absent for a Lottie JSON file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        animation: Option<String>,
    },
    /// Still images played in order at a fixed rate. The source's kind is
    /// `video`.
    ImageSequence {
        /// Each frame's locator, in play order. `null` is a number missing
        /// from the sequence, allowed only when `missing` says how to show
        /// it.
        frames: Vec<Option<String>>,
        /// The rate the frames play at.
        frame_rate: FrameRate,
        /// The number of the first frame, as its file was numbered.
        #[serde(default)]
        start_number: u64,
        /// How a missing frame is shown.
        #[serde(default)]
        missing: MissingFrames,
        /// The primaries OpenEXR frames' linear RGB is in, overriding what
        /// the first frame's chromaticities say. Only for OpenEXR frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
    /// A still OpenEXR image: linear-light RGB(A), read at half precision.
    /// The source's kind is `image`.
    Exr {
        /// The primaries its linear RGB is in, overriding what the file's
        /// chromaticities say (BT.709 when it says nothing, as OpenEXR
        /// defines).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
}

/// The wire form of [`ProviderSource`].
///
/// serde lets an internally tagged unit variant ignore extra fields even with
/// `deny_unknown_fields`, so `{"format": "svg", "frames": []}` would parse.
/// Empty struct variants reject them, as the published schema does.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "ProviderSource"))]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProviderSourceWire {
    /// A static SVG document, rasterized at the size it is shown. The
    /// source's kind is `image`.
    Svg {},
    /// A GIF, animated WebP, or APNG: one cycle of frames, each with its own
    /// duration. Repetition is the clip's. The source's kind is `video`.
    AnimatedImage {},
    /// A Lottie animation, from a Lottie JSON file or a dotLottie package,
    /// evaluated at its own frames. The source's kind is `video`.
    Lottie {
        /// The dotLottie animation shown, by its ID; the package's first
        /// when absent, and absent for a Lottie JSON file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        animation: Option<String>,
    },
    /// Still images played in order at a fixed rate. The source's kind is
    /// `video`.
    ImageSequence {
        /// Each frame's locator, in play order. `null` is a number missing
        /// from the sequence, allowed only when `missing` says how to show
        /// it.
        frames: Vec<Option<String>>,
        /// The rate the frames play at.
        frame_rate: FrameRate,
        /// The number of the first frame, as its file was numbered.
        #[serde(default)]
        start_number: u64,
        /// How a missing frame is shown.
        #[serde(default)]
        missing: MissingFrames,
        /// The primaries OpenEXR frames' linear RGB is in, overriding what
        /// the first frame's chromaticities say. Only for OpenEXR frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
    },
    /// A still OpenEXR image: linear-light RGB(A), read at half precision.
    /// The source's kind is `image`.
    Exr {
        /// The primaries its linear RGB is in, overriding what the file's
        /// chromaticities say (BT.709 when it says nothing, as OpenEXR
        /// defines).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        primaries: Option<LinearPrimaries>,
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
        }
    }
}

/// Named RGB primaries linear-light media may be in, each with its own white
/// point.
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
            Self::Svg | Self::Exr { .. } => MediaKind::Image,
            Self::AnimatedImage | Self::Lottie { .. } | Self::ImageSequence { .. } => {
                MediaKind::Video
            }
        }
    }

    /// Whether the settings are coherent: a Lottie animation ID is not empty,
    /// and an image sequence lists at most
    /// [`MAX_SEQUENCE_FRAMES`] frames, the first present, no locator empty or
    /// listed twice, and a missing frame only with a policy for it.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if let Self::Lottie {
            animation: Some(animation),
        } = self
        {
            return !animation.is_empty();
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
