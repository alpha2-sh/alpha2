//! What an export's pictures are made of: their bit depth, whether they keep
//! alpha, and the YCbCr layout each codec writes them in.
//!
//! One policy for every host, like the container table: the native sinks,
//! the host protocol and the browser exporter resolve a request through
//! [`resolve_pixels`], and the TypeScript mirror keeps to the same table
//! (`tests/cases/export-container-corpus.json` pins both).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Container, VideoCodec};

/// Bits per component of an output picture.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum BitDepth {
    /// 8 bits: 256 codes.
    #[default]
    #[serde(rename = "8")]
    Eight,
    /// 10 bits: 1024 codes.
    #[serde(rename = "10")]
    Ten,
}

impl BitDepth {
    /// The number of bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Self::Eight => 8,
            Self::Ten => 10,
        }
    }

    /// The depth of `bits`, `None` for any other number.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            8 => Some(Self::Eight),
            10 => Some(Self::Ten),
            _ => None,
        }
    }
}

/// What an export asks its pictures to be.
#[derive(Copy, Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct OutputPixels {
    /// The bit depth, or `None` for the codec's own: 10 for `ProRes`, 8 for
    /// every other codec.
    pub bit_depth: Option<BitDepth>,
    /// Whether the pictures keep their alpha. An output with alpha is drawn
    /// without the project background.
    pub alpha: bool,
}

/// How chroma is subsampled.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Chroma {
    /// Half width, half height.
    Yuv420,
    /// Half width, full height.
    Yuv422,
    /// Full width and height.
    Yuv444,
}

impl Chroma {
    /// How many luma columns and rows one chroma sample covers.
    #[must_use]
    pub const fn subsampling(self) -> (u32, u32) {
        match self {
            Self::Yuv420 => (2, 2),
            Self::Yuv422 => (2, 1),
            Self::Yuv444 => (1, 1),
        }
    }
}

/// How the planes of a picture are stored.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum PlaneLayout {
    /// One plane per component, each code in its own sample (a byte at 8
    /// bits, a little-endian 16-bit word holding the code at 10): `FFmpeg`'s
    /// `yuv420p`, `yuva444p10le` and so on.
    Planar,
    /// 10-bit 4:2:0 as `P010`: a luma plane and one plane of interleaved
    /// Cb and Cr, each code shifted into the top of a little-endian 16-bit
    /// word. What hardware encoders take.
    P010,
}

/// The pictures one codec writes: chroma subsampling, bit depth, alpha, and
/// how the planes are stored.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct PixelLayout {
    /// How chroma is subsampled.
    pub chroma: Chroma,
    /// Bits per component, alpha included.
    pub bit_depth: BitDepth,
    /// Whether a full-resolution alpha plane follows the color planes.
    pub alpha: bool,
    /// How the planes are stored.
    pub planes: PlaneLayout,
}

impl PixelLayout {
    /// What every export wrote before alpha and 10-bit output: 8-bit 4:2:0
    /// with no alpha, in three planes.
    pub const YUV420P: Self = Self {
        chroma: Chroma::Yuv420,
        bit_depth: BitDepth::Eight,
        alpha: false,
        planes: PlaneLayout::Planar,
    };

    /// Whether this is [`Self::YUV420P`], which every path packs as it
    /// always has.
    #[must_use]
    pub fn is_yuv420p(self) -> bool {
        self == Self::YUV420P
    }

    /// The same pictures stored as `P010` where they are 10-bit 4:2:0 with
    /// no alpha, for an encoder that takes only that; otherwise unchanged.
    #[must_use]
    pub const fn as_p010(self) -> Self {
        match (self.chroma, self.bit_depth, self.alpha) {
            (Chroma::Yuv420, BitDepth::Ten, false) => Self {
                planes: PlaneLayout::P010,
                ..self
            },
            _ => self,
        }
    }

    /// The `FFmpeg` pixel format name of this layout.
    #[must_use]
    pub const fn ffmpeg_name(self) -> &'static str {
        match (self.planes, self.chroma, self.bit_depth, self.alpha) {
            (PlaneLayout::P010, ..) => "p010le",
            (PlaneLayout::Planar, Chroma::Yuv420, BitDepth::Eight, false) => "yuv420p",
            (PlaneLayout::Planar, Chroma::Yuv420, BitDepth::Eight, true) => "yuva420p",
            (PlaneLayout::Planar, Chroma::Yuv420, BitDepth::Ten, false) => "yuv420p10le",
            (PlaneLayout::Planar, Chroma::Yuv420, BitDepth::Ten, true) => "yuva420p10le",
            (PlaneLayout::Planar, Chroma::Yuv422, BitDepth::Eight, false) => "yuv422p",
            (PlaneLayout::Planar, Chroma::Yuv422, BitDepth::Eight, true) => "yuva422p",
            (PlaneLayout::Planar, Chroma::Yuv422, BitDepth::Ten, false) => "yuv422p10le",
            (PlaneLayout::Planar, Chroma::Yuv422, BitDepth::Ten, true) => "yuva422p10le",
            (PlaneLayout::Planar, Chroma::Yuv444, BitDepth::Eight, false) => "yuv444p",
            (PlaneLayout::Planar, Chroma::Yuv444, BitDepth::Eight, true) => "yuva444p",
            (PlaneLayout::Planar, Chroma::Yuv444, BitDepth::Ten, false) => "yuv444p10le",
            (PlaneLayout::Planar, Chroma::Yuv444, BitDepth::Ten, true) => "yuva444p10le",
        }
    }

    /// The layout whose [`Self::ffmpeg_name`] is `name`, `None` for any other
    /// name.
    #[must_use]
    pub fn from_ffmpeg_name(name: &str) -> Option<Self> {
        let chromas = [Chroma::Yuv420, Chroma::Yuv422, Chroma::Yuv444];
        let depths = [BitDepth::Eight, BitDepth::Ten];
        chromas
            .into_iter()
            .flat_map(|chroma| depths.into_iter().map(move |bit_depth| (chroma, bit_depth)))
            .flat_map(|(chroma, bit_depth)| {
                [false, true].map(|alpha| Self {
                    chroma,
                    bit_depth,
                    alpha,
                    planes: PlaneLayout::Planar,
                })
            })
            .flat_map(|layout| [layout, layout.as_p010()])
            .find(|layout| layout.ffmpeg_name() == name)
    }

    /// The width and height in samples, and the bytes per sample, of each
    /// plane of a `width` x `height` picture, in storage order: Y, then Cb
    /// and Cr (one interleaved plane for `P010`), then alpha.
    #[must_use]
    pub fn planes(self, width: u32, height: u32) -> Vec<(u32, u32, u32)> {
        let (x, y) = self.chroma.subsampling();
        let (chroma_width, chroma_height) = (width.div_ceil(x), height.div_ceil(y));
        let bytes = match self.bit_depth {
            BitDepth::Eight => 1,
            BitDepth::Ten => 2,
        };
        let mut planes = vec![(width, height, bytes)];
        match self.planes {
            PlaneLayout::Planar => {
                planes.push((chroma_width, chroma_height, bytes));
                planes.push((chroma_width, chroma_height, bytes));
            }
            PlaneLayout::P010 => planes.push((chroma_width * 2, chroma_height, 2)),
        }
        if self.alpha {
            planes.push((width, height, bytes));
        }
        planes
    }
}

/// Why an output's pixels cannot be what was asked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputPixelsError {
    /// The codec cannot carry alpha.
    AlphaUnsupported {
        /// The requested codec.
        codec: VideoCodec,
    },
    /// The codec cannot carry alpha in this container.
    AlphaUnsupportedInContainer {
        /// The requested codec.
        codec: VideoCodec,
        /// The resolved container.
        container: Container,
    },
    /// The codec cannot be written at this depth, or not with alpha at it.
    BitDepthUnsupported {
        /// The requested codec.
        codec: VideoCodec,
        /// The requested depth.
        bit_depth: BitDepth,
        /// Whether alpha was asked for too.
        alpha: bool,
    },
}

impl fmt::Display for OutputPixelsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlphaUnsupported { codec } => write!(
                formatter,
                "video codec {} cannot keep alpha; use vp9 (webm, mkv) or prores (mov)",
                codec.as_str()
            ),
            Self::AlphaUnsupportedInContainer { codec, container } => write!(
                formatter,
                "{container} cannot carry {} alpha; use webm or mkv",
                codec.as_str()
            ),
            Self::BitDepthUnsupported {
                codec,
                bit_depth,
                alpha,
            } => write!(
                formatter,
                "video codec {} cannot be written at {} bits{}",
                codec.as_str(),
                bit_depth.bits(),
                if *alpha { " with alpha" } else { "" }
            ),
        }
    }
}

impl std::error::Error for OutputPixelsError {}

/// The layout `codec` writes `pixels` in, in `container`, which must offer
/// the codec ([`crate::resolve_export_format`] checks that first).
///
/// | Codec | 8-bit | 10-bit | Alpha |
/// | --- | --- | --- | --- |
/// | H.264, MPEG-4 Part 2 | 4:2:0 | no | no |
/// | HEVC, AV1 | 4:2:0 | 4:2:0 | no |
/// | VP9 | 4:2:0 | 4:2:0 (profile 2) | 8-bit only, not in MP4 |
/// | `ProRes` | no | 4:2:2 (422 HQ) | 4:4:4 (4444) |
///
/// VP9 alpha at 10 bits is refused: `libvpx` writes it only as an
/// experimental stream that players do not decode. `ProRes` is 10-bit by
/// definition, so it refuses 8.
///
/// # Errors
/// Returns the first [`OutputPixelsError`] the request runs into.
pub fn resolve_pixels(
    codec: VideoCodec,
    container: Container,
    pixels: OutputPixels,
) -> Result<PixelLayout, OutputPixelsError> {
    let default_depth = match codec {
        VideoCodec::ProRes => BitDepth::Ten,
        _ => BitDepth::Eight,
    };
    let bit_depth = pixels.bit_depth.unwrap_or(default_depth);
    let alpha = pixels.alpha;
    let refuse_depth = Err(OutputPixelsError::BitDepthUnsupported {
        codec,
        bit_depth,
        alpha,
    });
    let chroma = match codec {
        VideoCodec::H264 | VideoCodec::Mpeg4Part2 | VideoCodec::Hevc | VideoCodec::Av1 => {
            if alpha {
                return Err(OutputPixelsError::AlphaUnsupported { codec });
            }
            let ten_bits = matches!(codec, VideoCodec::Hevc | VideoCodec::Av1);
            if bit_depth == BitDepth::Ten && !ten_bits {
                return refuse_depth;
            }
            Chroma::Yuv420
        }
        VideoCodec::Vp9 => {
            if alpha && container == Container::Mp4 {
                return Err(OutputPixelsError::AlphaUnsupportedInContainer { codec, container });
            }
            if alpha && bit_depth == BitDepth::Ten {
                return refuse_depth;
            }
            Chroma::Yuv420
        }
        VideoCodec::ProRes => {
            if bit_depth == BitDepth::Eight {
                return refuse_depth;
            }
            if alpha {
                Chroma::Yuv444
            } else {
                Chroma::Yuv422
            }
        }
    };
    Ok(PixelLayout {
        chroma,
        bit_depth,
        alpha,
        planes: PlaneLayout::Planar,
    })
}

#[cfg(test)]
#[path = "pixels_tests.rs"]
mod tests;
