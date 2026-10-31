//! Platform-neutral media I/O contracts.
//!
//! Adapters translate `FFmpeg`, `WebCodecs`, or other host objects at this boundary;
//! none of those platform types are part of these contracts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{error::Error, fmt};

use alpha2_time::{FrameRate, MediaTime};

use crate::{ColorMetadata, SourceId};

/// Rectangle inside a coded video frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Rect {
    /// Horizontal origin in coded pixels.
    pub x: u32,
    /// Vertical origin in coded pixels.
    pub y: u32,
    /// Visible width.
    pub width: u32,
    /// Visible height.
    pub height: u32,
}

/// Clockwise rotation to apply when displaying a frame.
///
/// This is the convention of the `WebCodecs` `VideoFrame.rotation`. A binding
/// whose source states rotation another way converts it: `FFmpeg`'s display
/// matrix angle is counter-clockwise, so a binding reports `(-angle) mod 360`.
#[derive(Copy, Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum Rotation {
    /// No rotation.
    #[default]
    Deg0,
    /// Clockwise quarter turn.
    Deg90,
    /// Half turn.
    Deg180,
    /// Clockwise three-quarter turn, i.e. a counter-clockwise quarter turn.
    Deg270,
}

impl Rotation {
    /// Clockwise quarter turns, `0..4`.
    #[must_use]
    pub const fn quarter_turns(self) -> u32 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 1,
            Self::Deg180 => 2,
            Self::Deg270 => 3,
        }
    }

    /// The rotation of `quarter_turns` clockwise quarter turns, of any sign.
    #[must_use]
    pub const fn from_quarter_turns(quarter_turns: i32) -> Self {
        match quarter_turns.rem_euclid(4) {
            0 => Self::Deg0,
            1 => Self::Deg90,
            2 => Self::Deg180,
            _ => Self::Deg270,
        }
    }

    /// Whether the rotation exchanges width and height.
    #[must_use]
    pub const fn swaps_axes(self) -> bool {
        matches!(self, Self::Deg90 | Self::Deg270)
    }
}

/// How to show a decoded picture upright: mirror it left to right when
/// `mirror` is set, then turn it clockwise by `rotation`.
///
/// The two together reach every orientation a container or an image can
/// declare: the eight of the Exif `Orientation` tag, every display matrix
/// whose angle is a multiple of 90 degrees, a Matroska projection's pose. A binding
/// converts its source's own statement to this one; `FFmpeg`'s display matrix
/// mirrors after it rotates, and counter-clockwise.
#[derive(Copy, Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Orientation {
    /// Clockwise rotation, applied after the mirror.
    pub rotation: Rotation,
    /// Whether the stored picture is mirrored left to right before it turns.
    pub mirror: bool,
}

impl Orientation {
    /// Shown as stored.
    pub const UPRIGHT: Self = Self {
        rotation: Rotation::Deg0,
        mirror: false,
    };

    /// A rotation without a mirror.
    #[must_use]
    pub const fn rotated(rotation: Rotation) -> Self {
        Self {
            rotation,
            mirror: false,
        }
    }

    /// The orientation an Exif `Orientation` value (1 to 8) declares; `None`
    /// for any other value. 2, 4, 5, and 7 mirror.
    #[must_use]
    pub const fn from_exif(value: u16) -> Option<Self> {
        let (rotation, mirror) = match value {
            1 => (Rotation::Deg0, false),
            2 => (Rotation::Deg0, true),
            3 => (Rotation::Deg180, false),
            4 => (Rotation::Deg180, true),
            5 => (Rotation::Deg270, true),
            6 => (Rotation::Deg90, false),
            7 => (Rotation::Deg90, true),
            8 => (Rotation::Deg270, false),
            _ => return None,
        };
        Some(Self { rotation, mirror })
    }

    /// Whether the picture is shown as stored.
    #[must_use]
    pub const fn is_upright(self) -> bool {
        matches!(self.rotation, Rotation::Deg0) && !self.mirror
    }

    /// The displayed size of a stored `width` x `height` picture.
    #[must_use]
    pub const fn displayed_size(self, width: u32, height: u32) -> (u32, u32) {
        if self.rotation.swaps_axes() {
            (height, width)
        } else {
            (width, height)
        }
    }
}

/// Reduced pixel-width to pixel-height ratio.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SampleAspectRatio {
    /// Ratio numerator.
    pub num: u32,
    /// Ratio denominator, never zero.
    pub den: u32,
}

impl Default for SampleAspectRatio {
    fn default() -> Self {
        Self { num: 1, den: 1 }
    }
}

impl SampleAspectRatio {
    /// The ratio `num`:`den` in lowest terms, or `None` for a ratio with a
    /// zero term, which declares nothing (`FFmpeg` writes an unknown ratio
    /// as 0/1) or does not fit.
    #[must_use]
    pub fn new(num: u64, den: u64) -> Option<Self> {
        if num == 0 || den == 0 {
            return None;
        }
        let divisor = greatest_common_divisor(num, den);
        Some(Self {
            num: u32::try_from(num / divisor).ok()?,
            den: u32::try_from(den / divisor).ok()?,
        })
    }

    /// The ratio of the pixels of a `visible` picture shown at `displayed`
    /// size (width, height), as a `WebCodecs` frame's display size says it.
    ///
    /// A display size is the stored size with one side stretched by the
    /// ratio and rounded to whole pixels, so it cannot say the ratio exactly
    /// (128x96 of 4:3 pixels shows 171x96, not 170 2/3). The ratio is the
    /// simplest fraction that rounds to that size, which is the declared one
    /// wherever a declared ratio's terms are small (4:3, 64:45, 10:11), and
    /// otherwise within half a displayed pixel of the size.
    #[must_use]
    pub fn of_display(visible: (u32, u32), displayed: (u32, u32)) -> Option<Self> {
        let [
            visible_width,
            visible_height,
            displayed_width,
            displayed_height,
        ] = [visible.0, visible.1, displayed.0, displayed.1].map(u128::from);
        if visible_width == 0
            || visible_height == 0
            || displayed_width == 0
            || displayed_height == 0
        {
            return None;
        }
        // A side the display stretched is known to half a pixel; one it kept is exact.
        let slack = |shown: u128, stored: u128| u128::from(shown != stored);
        let width_slack = slack(displayed_width, visible_width);
        let height_slack = slack(displayed_height, visible_height);
        // ratio = (displayed_width / visible_width) / (displayed_height / visible_height), in
        // halves of a pixel.
        let low = (
            (2 * displayed_width - width_slack) * visible_height,
            (2 * displayed_height + height_slack) * visible_width,
        );
        let high = (
            (2 * displayed_width + width_slack) * visible_height,
            (2 * displayed_height - height_slack) * visible_width,
        );
        let (num, den) = simplest_fraction_between(low, high)?;
        Self::new(u64::try_from(num).ok()?, u64::try_from(den).ok()?)
    }

    /// The size a stored `width` x `height` picture of these pixels is shown
    /// at in square pixels, as a browser demuxer reports it: the side the
    /// pixels are longer along stretched, rounded half up.
    #[must_use]
    pub fn square_pixel_size(self, width: u32, height: u32) -> (u32, u32) {
        let stretch = |side: u32, by: u32, of: u32| {
            let scaled =
                (u64::from(side) * u64::from(by) * 2 + u64::from(of)) / (2 * u64::from(of));
            u32::try_from(scaled).unwrap_or(u32::MAX)
        };
        if self.num == 0 || self.den == 0 {
            return (width, height);
        }
        match self.num.cmp(&self.den) {
            std::cmp::Ordering::Greater => (stretch(width, self.num, self.den), height),
            std::cmp::Ordering::Less => (width, stretch(height, self.den, self.num)),
            std::cmp::Ordering::Equal => (width, height),
        }
    }
}

/// The fraction with the smallest denominator in the closed interval
/// `low`..=`high` of positive fractions (numerator, denominator), by the
/// continued fractions of its ends.
fn simplest_fraction_between(low: (u128, u128), high: (u128, u128)) -> Option<(u128, u128)> {
    let ((low_num, low_den), (high_num, high_den)) = (low, high);
    if low_den == 0 || high_den == 0 || low_num * high_den > high_num * low_den {
        return None;
    }
    let whole = low_num / low_den;
    if whole * low_den == low_num {
        return Some((whole, 1));
    }
    if (whole + 1) * high_den <= high_num {
        return Some((whole + 1, 1));
    }
    // Both ends lie strictly between `whole` and `whole + 1`: x = whole + 1/y, with y between the
    // reciprocals of the fractional parts.
    let (num, den) = simplest_fraction_between(
        (high_den, high_num - whole * high_den),
        (low_den, low_num - whole * low_den),
    )?;
    Some((whole * num + den, num))
}

fn greatest_common_divisor(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Plane organization delivered by a decoder.
///
/// A [`Decoder`] delivers only the layouts the render stage consumes directly:
/// planar or semi-planar YCbCr of any chroma subsampling and bit depth that
/// [`YuvFormat`] describes, 8-bit RGBA, and linear-light half-float RGBA for
/// sources with more range than 8 bits hold. Every other decoded layout, such as
/// packed RGB orders, palettes, or YCbCr with alpha, must be converted by the
/// decoder before it is handed over, and is then reported as
/// [`PixelFormat::Rgba8`]. The bundled native decoder does this with
/// `libswscale`; a custom decoder is responsible for the same conversion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Planar or semi-planar YCbCr, one [`Plane`] per stored plane.
    Yuv(YuvFormat),
    /// Straight-alpha 8-bit red, green, blue, alpha in one plane.
    Rgba8,
    /// Premultiplied linear-light red, green, blue, alpha in one plane, each
    /// a little-endian IEEE 754 half float, in BT.709 primaries with `1.0` at
    /// SDR white. Values above `1.0` and below `0.0` are allowed; the frame's
    /// color metadata does not apply.
    LinearRgbaF16,
}

/// How a planar or semi-planar YCbCr frame stores its samples.
///
/// Luma is always the first plane. Chroma follows as separate U and V planes,
/// or as one plane of interleaved U, V pairs. Each chroma plane covers
/// `ceil(width / 2^chroma_shift_x)` by `ceil(height / 2^chroma_shift_y)`
/// samples. A sample of more than eight bits is a little-endian `u16`. A
/// format with [`alpha`](Self::alpha) stores one more plane after the
/// chroma: straight, full-range alpha at the luma's size and depth.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct YuvFormat {
    /// Log2 of the horizontal chroma subsampling: 0 for 4:4:4, 1 for 4:2:x.
    pub chroma_shift_x: u8,
    /// Log2 of the vertical chroma subsampling: 1 for 4:2:0, 0 otherwise.
    pub chroma_shift_y: u8,
    /// Significant bits per sample, 8 through 16.
    pub bit_depth: u8,
    /// Whether U and V interleave in one plane, as NV12 and P010 do.
    pub interleaved_chroma: bool,
    /// Whether a sample of fewer than 16 bits sits in the high bits of its
    /// `u16`, as P010's do, rather than the low bits, as `yuv420p10le`'s do.
    pub msb_aligned: bool,
    /// Whether an alpha plane follows the chroma, as `yuva444p16le`'s does.
    pub alpha: bool,
}

impl YuvFormat {
    /// 8-bit planar 4:2:0 (`yuv420p`).
    pub const I420: Self = Self::planar(1, 1, 8);
    /// 8-bit 4:2:0 with interleaved chroma (`nv12`).
    pub const NV12: Self = Self {
        interleaved_chroma: true,
        ..Self::I420
    };
    /// 10-bit 4:2:0 with interleaved, high-aligned chroma (`p010le`).
    pub const P010: Self = Self {
        bit_depth: 10,
        msb_aligned: true,
        ..Self::NV12
    };

    /// Planar YCbCr with low-aligned samples.
    #[must_use]
    pub const fn planar(chroma_shift_x: u8, chroma_shift_y: u8, bit_depth: u8) -> Self {
        Self {
            chroma_shift_x,
            chroma_shift_y,
            bit_depth,
            interleaved_chroma: false,
            msb_aligned: false,
            alpha: false,
        }
    }

    /// The same format with an alpha plane after the chroma.
    #[must_use]
    pub const fn with_alpha(self) -> Self {
        Self {
            alpha: true,
            ..self
        }
    }

    /// Whether the renderer can sample this format: 8 to 16 bits, chroma
    /// subsampled by at most four on either axis, and high alignment only
    /// where there are spare bits to align in.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.bit_depth >= 8
            && self.bit_depth <= 16
            && self.chroma_shift_x <= 2
            && self.chroma_shift_y <= 2
            && (!self.msb_aligned || self.bit_depth > 8)
    }

    /// Bytes each stored sample takes: one up to 8 bits, two above.
    #[must_use]
    pub const fn bytes_per_sample(self) -> u32 {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    /// Number of stored planes: luma and chroma (three, or two with
    /// interleaved chroma), then alpha when the format has it.
    #[must_use]
    pub const fn plane_count(self) -> usize {
        self.color_plane_count() + if self.alpha { 1 } else { 0 }
    }

    /// Number of planes that hold luma and chroma: three, or two with
    /// interleaved chroma. The alpha plane, if any, is the one after them.
    #[must_use]
    pub const fn color_plane_count(self) -> usize {
        if self.interleaved_chroma { 2 } else { 3 }
    }

    /// Chroma plane dimensions, in sample positions, for a coded frame size.
    #[must_use]
    pub const fn chroma_size(self, width: u32, height: u32) -> (u32, u32) {
        (
            width.div_ceil(1 << self.chroma_shift_x),
            height.div_ceil(1 << self.chroma_shift_y),
        )
    }
}

/// Chroma sample location for subsampled YUV.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ChromaLocation {
    /// Adapter could not determine a location.
    #[default]
    Unspecified,
    /// MPEG-2/most video: horizontally left, vertically centered.
    Left,
    /// JPEG-style centered chroma.
    Center,
    /// Top-left co-sited chroma.
    TopLeft,
}

/// One decoded image plane.
///
/// The bytes are shared rather than owned outright, so a decoder can hand
/// over its own reference-counted frame memory without copying it, and a
/// cloned frame shares its planes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plane {
    /// Plane bytes, including row padding: `stride * height` of them.
    pub bytes: bytes::Bytes,
    /// Bytes between adjacent rows.
    pub stride: u32,
    /// Plane width in sample positions; an interleaved chroma plane holds a
    /// U, V pair at each.
    pub width: u32,
    /// Plane height in samples.
    pub height: u32,
}

/// Largest frame edge, in pixels, anything in alpha2 decodes, renders, or
/// composes a project at.
pub const MAX_FRAME_DIMENSION: u32 = 16_384;
/// Largest frame area, in pixels, anything in alpha2 decodes, renders, or
/// composes a project at: a decoder refuses a larger frame before it
/// allocates one.
pub const MAX_FRAME_PIXELS: u64 = 67_108_864;

/// Whether a `width` x `height` frame is within [`MAX_FRAME_DIMENSION`] and
/// [`MAX_FRAME_PIXELS`].
#[must_use]
pub const fn frame_size_supported(width: u32, height: u32) -> bool {
    width <= MAX_FRAME_DIMENSION
        && height <= MAX_FRAME_DIMENSION
        && (width as u64) * (height as u64) <= MAX_FRAME_PIXELS
}

/// Host-independent decoded video frame.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedVideoFrame {
    /// Presentation timestamp.
    pub pts: MediaTime,
    /// Optional decode timestamp.
    pub dts: Option<MediaTime>,
    /// Nominal frame duration when known.
    pub duration: Option<MediaTime>,
    /// The visible crop within the coded picture, in coded pixels.
    pub visible_rect: Rect,
    /// Pixel aspect ratio.
    pub sample_aspect_ratio: SampleAspectRatio,
    /// How the picture is shown upright.
    pub orientation: Orientation,
    /// Plane layout.
    pub pixel_format: PixelFormat,
    /// Chroma siting.
    pub chroma_location: ChromaLocation,
    /// Declared color metadata: the project's declaration, else the frame's
    /// or stream's tags. `None` when nothing declares one: the renderer then
    /// applies [`crate::ColorMetadata::UNDECLARED`] and reports it.
    pub color: Option<ColorMetadata>,
    /// Image planes in the order [`Self::pixel_format`] stores them.
    pub planes: Vec<Plane>,
}

/// Most planes an [`AudioBlock`] carries: a 22.2 layout's 24, the most of
/// any layout `FFmpeg` names.
///
/// Neither binding downmixes a source with more: both refuse it, so no
/// source is mixed one way natively and another in the browser.
pub const MAX_AUDIO_CHANNELS: usize = 24;

/// The loudspeaker one plane of an [`AudioBlock`] feeds.
///
/// The positions are `FFmpeg`'s named channels (`AVChannel`), which take in
/// the WAVE / `WAVEFORMATEXTENSIBLE` mask positions, Apple's channel labels,
/// and ISO/IEC 23001-8's. [`Speaker::name`] is `FFmpeg`'s abbreviation.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Speaker {
    /// Front left (`FL`).
    FrontLeft,
    /// Front right (`FR`).
    FrontRight,
    /// Front center (`FC`): a mono source's one channel, and most films' dialogue.
    FrontCenter,
    /// Low-frequency effects (`LFE`).
    LowFrequency,
    /// Back left (`BL`).
    BackLeft,
    /// Back right (`BR`).
    BackRight,
    /// Front left of center (`FLC`).
    FrontLeftOfCenter,
    /// Front right of center (`FRC`).
    FrontRightOfCenter,
    /// Back center (`BC`).
    BackCenter,
    /// Side left (`SL`).
    SideLeft,
    /// Side right (`SR`).
    SideRight,
    /// Top center (`TC`).
    TopCenter,
    /// Top front left (`TFL`).
    TopFrontLeft,
    /// Top front center (`TFC`).
    TopFrontCenter,
    /// Top front right (`TFR`).
    TopFrontRight,
    /// Top back left (`TBL`).
    TopBackLeft,
    /// Top back center (`TBC`).
    TopBackCenter,
    /// Top back right (`TBR`).
    TopBackRight,
    /// Left of a matrix-encoded stereo downmix (`DL`, Lt).
    DownmixLeft,
    /// Right of a matrix-encoded stereo downmix (`DR`, Rt).
    DownmixRight,
    /// Wide left (`WL`).
    WideLeft,
    /// Wide right (`WR`).
    WideRight,
    /// Surround direct left (`SDL`).
    SurroundDirectLeft,
    /// Surround direct right (`SDR`).
    SurroundDirectRight,
    /// Second low-frequency effects (`LFE2`).
    LowFrequency2,
    /// Top side left (`TSL`).
    TopSideLeft,
    /// Top side right (`TSR`).
    TopSideRight,
    /// Bottom front center (`BFC`).
    BottomFrontCenter,
    /// Bottom front left (`BFL`).
    BottomFrontLeft,
    /// Bottom front right (`BFR`).
    BottomFrontRight,
    /// Side surround left (`SSL`).
    SideSurroundLeft,
    /// Side surround right (`SSR`).
    SideSurroundRight,
    /// Top surround left (`TTL`).
    TopSurroundLeft,
    /// Top surround right (`TTR`).
    TopSurroundRight,
    /// Binaural left (`BIL`).
    BinauralLeft,
    /// Binaural right (`BIR`).
    BinauralRight,
    /// A channel no loudspeaker position names (`UNK`): unknown, unused, or
    /// one ambisonic component. The fold leaves it out.
    Unnamed,
}

impl Speaker {
    /// Every speaker with its `FFmpeg` name and channel number (`AVChannel`);
    /// [`Speaker::Unnamed`] is `FFmpeg`'s `AV_CHAN_UNKNOWN`.
    const TABLE: [(Self, &'static str, u32); 37] = [
        (Self::FrontLeft, "FL", 0),
        (Self::FrontRight, "FR", 1),
        (Self::FrontCenter, "FC", 2),
        (Self::LowFrequency, "LFE", 3),
        (Self::BackLeft, "BL", 4),
        (Self::BackRight, "BR", 5),
        (Self::FrontLeftOfCenter, "FLC", 6),
        (Self::FrontRightOfCenter, "FRC", 7),
        (Self::BackCenter, "BC", 8),
        (Self::SideLeft, "SL", 9),
        (Self::SideRight, "SR", 10),
        (Self::TopCenter, "TC", 11),
        (Self::TopFrontLeft, "TFL", 12),
        (Self::TopFrontCenter, "TFC", 13),
        (Self::TopFrontRight, "TFR", 14),
        (Self::TopBackLeft, "TBL", 15),
        (Self::TopBackCenter, "TBC", 16),
        (Self::TopBackRight, "TBR", 17),
        (Self::DownmixLeft, "DL", 29),
        (Self::DownmixRight, "DR", 30),
        (Self::WideLeft, "WL", 31),
        (Self::WideRight, "WR", 32),
        (Self::SurroundDirectLeft, "SDL", 33),
        (Self::SurroundDirectRight, "SDR", 34),
        (Self::LowFrequency2, "LFE2", 35),
        (Self::TopSideLeft, "TSL", 36),
        (Self::TopSideRight, "TSR", 37),
        (Self::BottomFrontCenter, "BFC", 38),
        (Self::BottomFrontLeft, "BFL", 39),
        (Self::BottomFrontRight, "BFR", 40),
        (Self::SideSurroundLeft, "SSL", 41),
        (Self::SideSurroundRight, "SSR", 42),
        (Self::TopSurroundLeft, "TTL", 43),
        (Self::TopSurroundRight, "TTR", 44),
        (Self::BinauralLeft, "BIL", 61),
        (Self::BinauralRight, "BIR", 62),
        (Self::Unnamed, "UNK", 0x300),
    ];

    /// `FFmpeg`'s abbreviation (`FL`, `FR`, `FC`, `LFE`, ...); `UNK` for
    /// [`Speaker::Unnamed`].
    #[must_use]
    pub fn name(self) -> &'static str {
        Self::TABLE
            .iter()
            .find(|(speaker, _, _)| *speaker == self)
            .map_or("UNK", |(_, name, _)| name)
    }

    /// The speaker `FFmpeg` names `name`, or `None` for a name it does not
    /// give a loudspeaker position (`UNK` is [`Speaker::Unnamed`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::TABLE
            .iter()
            .find(|(_, known, _)| *known == name)
            .map(|(speaker, _, _)| *speaker)
    }

    /// `FFmpeg`'s channel number (`enum AVChannel`) for this speaker.
    #[must_use]
    pub fn ffmpeg_channel(self) -> u32 {
        Self::TABLE
            .iter()
            .find(|(speaker, _, _)| *speaker == self)
            .map_or(0x300, |(_, _, channel)| *channel)
    }

    /// The speaker of `FFmpeg` channel number `channel`; any channel without
    /// a loudspeaker position (unknown, unused, ambisonic, user) is
    /// [`Speaker::Unnamed`].
    #[must_use]
    pub fn from_ffmpeg_channel(channel: i64) -> Self {
        Self::TABLE
            .iter()
            .find(|(_, _, number)| i64::from(*number) == channel)
            .map_or(Self::Unnamed, |(speaker, _, _)| *speaker)
    }
}

/// The speakers an [`AudioBlock`]'s planes feed, in plane order: 1 to
/// [`MAX_AUDIO_CHANNELS`] of them.
///
/// A provider reports the layout its source declares (a WAVE channel mask, a
/// `QuickTime` `chan` box, a codec's channel configuration), in the order its
/// planes arrive; neither binding rematrixes a source into another layout.
/// A source that declares none, or only channels no speaker names, has
/// [`SpeakerLayout::default_for`] its channel count.
#[derive(Copy, Clone, Eq, Hash, PartialEq)]
pub struct SpeakerLayout {
    speakers: [Speaker; MAX_AUDIO_CHANNELS],
    count: u8,
}

impl std::fmt::Debug for SpeakerLayout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_list()
            .entries(self.speakers().iter().map(|speaker| speaker.name()))
            .finish()
    }
}

impl SpeakerLayout {
    /// One front-center plane: a mono source, and a mono project's mix.
    pub const MONO: Self = Self::fixed(&[Speaker::FrontCenter]);
    /// Front left and right: a stereo source, and a stereo project's mix.
    pub const STEREO: Self = Self::fixed(&[Speaker::FrontLeft, Speaker::FrontRight]);

    const fn fixed(speakers: &[Speaker]) -> Self {
        let mut layout = [Speaker::Unnamed; MAX_AUDIO_CHANNELS];
        let mut index = 0;
        while index < speakers.len() {
            layout[index] = speakers[index];
            index += 1;
        }
        #[allow(clippy::cast_possible_truncation)]
        Self {
            speakers: layout,
            count: speakers.len() as u8,
        }
    }

    /// The layout of these speakers in this plane order, or `None` for none
    /// or more than [`MAX_AUDIO_CHANNELS`].
    #[must_use]
    pub fn new(speakers: &[Speaker]) -> Option<Self> {
        if speakers.is_empty() || speakers.len() > MAX_AUDIO_CHANNELS {
            return None;
        }
        let mut layout = Self {
            speakers: [Speaker::Unnamed; MAX_AUDIO_CHANNELS],
            count: u8::try_from(speakers.len()).ok()?,
        };
        layout.speakers[..speakers.len()].copy_from_slice(speakers);
        Some(layout)
    }

    /// The layout a source of `channels` planes that declares none has:
    /// `FFmpeg`'s default for the count (`av_channel_layout_default`), in the
    /// WAVE order. Mono, stereo, 2.1, 4.0, 5.0, 5.1, 6.1 and 7.1 up to 8;
    /// 5.1.4, 7.1.4, 9.1.4, 9.1.6 and 22.2 for 10, 12, 14, 16 and 24. A count
    /// `FFmpeg` names no layout for is a 7.1 with its further channels
    /// unnamed. `None` for no channels or more than [`MAX_AUDIO_CHANNELS`].
    #[must_use]
    pub fn default_for(channels: usize) -> Option<Self> {
        /// `FFmpeg`'s native-order channel masks (`AV_CH_LAYOUT_*`).
        const SEVEN_ONE: u64 = 0x63f;
        let mask: u64 = match channels {
            1 => 0x4,
            2 => 0x3,
            3 => 0xb,
            4 => 0x107,
            5 => 0x37,
            6 => 0x3f,
            7 => 0x70f,
            10 => 0x2_d60f,
            12 => 0x2_d63f,
            14 => 0x2_d6ff,
            16 => 0x30_0002_d6ff,
            24 => 0x1f8_0003_ffff,
            8..=MAX_AUDIO_CHANNELS => SEVEN_ONE,
            _ => return None,
        };
        let mut layout = Self::of_mask(mask)?;
        // A count without a layout of its own: the 7.1's further channels name no speaker.
        layout.count = u8::try_from(channels).ok()?;
        Some(layout)
    }

    /// The speakers of a native-order channel mask, in bit order.
    fn of_mask(mask: u64) -> Option<Self> {
        let speakers = (0..64_i64)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(Speaker::from_ffmpeg_channel)
            .collect::<Vec<_>>();
        Self::new(&speakers)
    }

    /// The layout a source declares, or [`SpeakerLayout::default_for`] its
    /// channel count when it names no speaker at all. The flag says the
    /// default was assumed.
    #[must_use]
    pub fn declared_or_default(speakers: &[Speaker]) -> Option<(Self, bool)> {
        if speakers.iter().all(|speaker| *speaker == Speaker::Unnamed) {
            return Self::default_for(speakers.len()).map(|layout| (layout, true));
        }
        Self::new(speakers).map(|layout| (layout, false))
    }

    /// The speakers, in plane order.
    #[must_use]
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers[..usize::from(self.count)]
    }

    /// How many planes the layout has.
    #[must_use]
    pub fn channels(&self) -> usize {
        usize::from(self.count)
    }

    /// The speakers' `FFmpeg` names, joined by `+` (`FL+FR+FC`), as
    /// `av_channel_layout_from_string` reads them.
    #[must_use]
    pub fn describe(&self) -> String {
        self.speakers()
            .iter()
            .map(|speaker| speaker.name())
            .collect::<Vec<_>>()
            .join("+")
    }
}

/// Planar floating-point audio at the project sample rate.
///
/// Positions are sample indices at `sample_rate`, never ticks: a tick is not a
/// whole number of samples at 44.1 kHz, so a start in ticks cannot say which
/// sample a block begins with, and every conversion between the two would need
/// a rounding rule both sides agree on. Sample `n` is at `n / sample_rate`
/// seconds on the block's timeline, the source's for a decoder and the
/// project's for a mixer.
///
/// A block from a decoder or an [`AudioBlockProvider`] keeps its source's
/// channels, 1 to [`MAX_AUDIO_CHANNELS`] planes, and says which speaker each
/// feeds; the mixer folds them into the project's layout by those speakers.
/// A mixer's own blocks have the project's channels.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioBlock {
    /// Index of the first sample, at `sample_rate`.
    pub start_sample: i64,
    /// Project sample rate used by every returned block.
    pub sample_rate: u32,
    /// The speaker each of `channels` feeds, one per plane.
    pub speakers: SpeakerLayout,
    /// One vector per channel, all with equal lengths, in `speakers`' order.
    pub channels: Vec<Vec<f32>>,
}

impl AudioBlock {
    /// Samples per channel.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    /// Index one past the last sample.
    #[must_use]
    pub fn end_sample(&self) -> i64 {
        self.start_sample
            .saturating_add(i64::try_from(self.frames()).unwrap_or(i64::MAX))
    }

    /// The first sample's time, at or before it on the tick grid.
    ///
    /// For display and for APIs that take a [`MediaTime`]; positions within
    /// audio stay in samples.
    #[must_use]
    pub fn start_time(&self) -> MediaTime {
        MediaTime::from_ticks(sample_ticks_floor(self.start_sample, self.sample_rate))
    }
}

/// The tick at or before sample `sample` at `sample_rate`.
#[must_use]
pub fn sample_ticks_floor(sample: i64, sample_rate: u32) -> i64 {
    let ticks = (i128::from(sample) * i128::from(alpha2_time::TICKS_PER_SECOND))
        .div_euclid(i128::from(sample_rate.max(1)));
    i64::try_from(ticks).unwrap_or(if ticks < 0 { i64::MIN } else { i64::MAX })
}

/// Generic color declaration for a host-rasterized texture.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RasterColorSpace {
    /// IEC sRGB encoded RGB.
    Srgb,
    /// Linear-light RGB with BT.709/sRGB primaries.
    LinearSrgb,
}

/// Alpha representation of host pixels.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Premultiplication {
    /// RGB has already been multiplied by alpha.
    Premultiplied,
    /// RGB is straight and must be premultiplied at upload.
    Straight,
    /// Pixels are opaque.
    Opaque,
}

/// Generic image/video/text texture descriptor owned by a host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostTexture {
    /// Host-defined cache identity.
    pub content_version: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Alpha representation.
    pub premultiplication: Premultiplication,
    /// RGB encoding.
    pub color_space: RasterColorSpace,
}

/// Horizontal alignment applied by a host text layout engine.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TextAlign {
    /// Align each line to its paragraph's leading edge: the left of a
    /// left-to-right paragraph, the right of a right-to-left one.
    #[default]
    Start,
    /// Center each line in the requested layout width.
    Center,
    /// Align each line to its paragraph's trailing edge.
    End,
}

/// How a text host draws emoji and other color glyphs.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum EmojiPolicy {
    /// Draw a glyph a font has in color (a COLR painting, or a CBDT or sbix
    /// bitmap) in its own colors; a grapheme shown as an emoji (as for
    /// [`Self::Replace`]) tries the stack's color fonts first, and one
    /// asking for text (U+FE0E) the others.
    #[default]
    Color,
    /// Rasterize a monochrome outline when the selected host stack provides
    /// one: every glyph is drawn as an outline in the text's color, and an
    /// emoji tries the fonts without color glyphs first.
    Monochrome,
    /// Draw each grapheme shown as an emoji (UTS #51: one with an
    /// `Emoji_Presentation` character, or U+FE0F with a pictograph or a
    /// keycap, and no U+FE0E), sequence and modifiers included, as one
    /// U+FFFD REPLACEMENT CHARACTER. A symbol shown as text by default,
    /// such as (c) or a sun, stays.
    Replace,
}

/// Layout facts attached to a host-rasterized text texture.
///
/// The compositor consumes [`Self::texture`] exactly like an image or decoded
/// video texture. These values are retained for placement, cache validation,
/// and cross-host contract tests; they never select a text-specific GPU path.
#[derive(Clone, Debug, PartialEq)]
pub struct TextTexture {
    /// Generic texture identity and pixel declaration.
    pub texture: HostTexture,
    /// Where the layout box's left edge lies in the texture, in pixels. The
    /// texture adds the same margin on both sides of the box for ink and
    /// decorations that reach past it.
    pub layout_x: f32,
    /// Where the layout box's top edge lies in the texture, in pixels, with
    /// the same margin below the box.
    pub layout_y: f32,
    /// Logical layout width before integer pixel rounding.
    pub layout_width: f32,
    /// Logical layout height before integer pixel rounding.
    pub layout_height: f32,
    /// Distance from the layout box's top edge to the first alphabetic
    /// baseline.
    pub first_baseline: f32,
    /// Highest any line's ink reaches above its baseline, and at least the
    /// nominal ascent the lines are spaced by.
    pub ascent: f32,
    /// Lowest any line's ink reaches below its baseline, and at least the
    /// nominal descent.
    pub descent: f32,
    /// Number of laid-out lines.
    pub line_count: u32,
}

impl TextTexture {
    /// Checks dimensions and finite layout metrics at the public boundary.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.texture.width > 0
            && self.texture.height > 0
            && self.line_count > 0
            && [
                self.layout_x,
                self.layout_y,
                self.layout_width,
                self.layout_height,
                self.first_baseline,
                self.ascent,
                self.descent,
            ]
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0)
            && self.first_baseline <= self.layout_height
            && f64::from(self.layout_x) + f64::from(self.layout_width)
                <= f64::from(self.texture.width)
            && f64::from(self.layout_y) + f64::from(self.layout_height)
                <= f64::from(self.texture.height)
    }
}

/// RGBA8 pixels returned by an image, video, or text rasterization host.
#[derive(Clone, Debug, PartialEq)]
pub struct HostTexturePixels {
    /// Shared texture descriptor. Text hosts additionally return [`TextTexture`].
    pub texture: HostTexture,
    /// Tightly packed top-to-bottom RGBA8 rows.
    pub rgba8: Vec<u8>,
}

impl HostTexturePixels {
    /// Checks that the owned bytes exactly cover the declared texture.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        usize::try_from(self.texture.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.texture.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            == Some(self.rgba8.len())
    }
}

/// Media stream domain.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StreamKind {
    /// Video stream.
    Video,
    /// Audio stream.
    Audio,
}

/// Platform-neutral probe result for one stream.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    /// Demuxer stream index.
    pub index: usize,
    /// Stream domain.
    pub kind: StreamKind,
    /// Codec short name when available.
    pub codec: String,
    /// Stream duration when known.
    pub duration: Option<MediaTime>,
    /// Video dimensions.
    pub dimensions: Option<(u32, u32)>,
    /// Video frame rate.
    pub frame_rate: Option<FrameRate>,
    /// Audio sample rate.
    pub sample_rate: Option<u32>,
    /// Audio channel count.
    pub channels: Option<u32>,
    /// How the stream's pictures are shown upright.
    pub orientation: Orientation,
    /// Sample aspect ratio.
    pub sample_aspect_ratio: SampleAspectRatio,
    /// Declared video color.
    pub color: Option<ColorMetadata>,
    /// Bytes one decoded picture of a video stream holds in the decoder's
    /// output format (`av_image_get_buffer_size` of its pixel format at the
    /// coded size): 1.5 bytes a pixel for 8-bit 4:2:0, twice that at 10 bits,
    /// four times at 10-bit 4:4:4. `None` when the format is not known.
    pub frame_bytes: Option<u64>,
    /// How a video stream carries alpha, when it does; `None` for an opaque stream.
    pub alpha: Option<SourceAlpha>,
}

/// How a video stream carries an alpha channel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SourceAlpha {
    /// VP9 in Matroska or `WebM` with `alpha_mode` 1: a second VP9 stream of the alpha plane in
    /// each block's additions, which only libvpx's VP9 decoder reads.
    Vp9SideData,
    /// HEVC whose video parameter set declares an auxiliary alpha layer (Apple's HEVC with alpha),
    /// which no decoder alpha2 opens reads: the base layer decodes opaque.
    HevcAuxiliaryLayer,
    /// A pixel format with an alpha plane (`ProRes` 4444, PNG, `QuickTime` Animation).
    Planar,
}

impl SourceAlpha {
    /// The name a report gives it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vp9SideData => "vp9-side-data",
            Self::HevcAuxiliaryLayer => "hevc-auxiliary-layer",
            Self::Planar => "planar",
        }
    }
}

/// Kind of decoder cache/refill operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecodeRequestKind {
    /// Normal look-ahead fill.
    Prefetch,
    /// Exact seek: flush decoder state and begin at/before the requested time.
    Seek,
    /// Refill after cache eviction.
    Evicted,
    /// Refill every referenced input after GPU device recovery.
    DeviceRecovery,
}

/// One idempotent decoder request. Hosts discard results with an old generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodeRequest {
    /// Source identity.
    pub source: SourceId,
    /// Requested source timestamp.
    pub time: MediaTime,
    /// Why the engine needs the input.
    pub kind: DecodeRequestKind,
    /// Monotonically increasing seek/recovery generation.
    pub generation: u64,
}

/// Result correlated to a decoder request, safe for out-of-order completion.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeResult {
    /// Original request.
    pub request: DecodeRequest,
    /// Decoder outcome.
    pub event: DecodeEvent,
}

/// Decoder output and terminal states.
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeEvent {
    /// Decoded video became available.
    Video(DecodedVideoFrame),
    /// Decoded audio became available.
    Audio(AudioBlock),
    /// No exact frame is available yet; retry after feeding more input.
    Retry,
    /// The selected stream has ended.
    EndOfStream,
}

/// Cancellation flag shared across adapter operations.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Requests cancellation. The operation must stop at its next adapter boundary.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The sentence every wait on GPU work that timed out carries, wherever it
/// was waited on (the renderer, a native interop helper, an encoder input),
/// so a host that sees only error text tells a hung GPU from a generic
/// failure. Hosts classify it as `GPU_TIMEOUT`.
pub const GPU_TIMEOUT_MESSAGE: &str = "GPU work did not complete within the wait timeout";

/// What a failed GPU call says about the device it ran on, read from the
/// platform's own status for that call (a `VkResult`, `HRESULT`, or
/// `CVReturn`) where it failed, not from any message.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GpuFault {
    /// The device was lost or removed; nothing more runs on it.
    DeviceLost,
    /// The device was reset because work on it ran too long (Windows'
    /// `DXGI_ERROR_DEVICE_HUNG`): lost, and lost to the work it was given.
    DeviceHung,
    /// The device could not allocate memory of its own.
    DeviceMemory,
    /// The driver could not allocate host (CPU) memory for the call.
    HostMemory,
    /// GPU work the call waited on did not complete within the wait timeout.
    Timeout,
}

impl GpuFault {
    /// A stable name for logs and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeviceLost => "device-lost",
            Self::DeviceHung => "device-hung",
            Self::DeviceMemory => "device-memory",
            Self::HostMemory => "host-memory",
            Self::Timeout => "timeout",
        }
    }
}

/// Typed decoder failure.
///
/// Damaged input a decoder's policy lets it skip is not one: the decoder
/// goes on to the next frame and reports the damage through its adapter's
/// diagnostics, so a caller never retries around it. Damage it does not skip
/// is an adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The operation was cancelled.
    Cancelled,
    /// The requested stream or timestamp is outside the resource.
    OutOfRange,
    /// The adapter does not support this media representation.
    Unsupported(String),
    /// Adapter-specific failure, represented as stable text rather than a platform type.
    Adapter(String),
    /// A GPU call the adapter made failed, and its platform status says what
    /// that was evidence of. `message` names the call and the status.
    Gpu {
        /// What the status says about the device.
        fault: GpuFault,
        /// The failed call and its platform status.
        message: String,
    },
}

/// Why an encoder did not open, as far as its own status and log say.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum OpenFailureKind {
    /// This machine does not have what the encoder needs: no device, no
    /// driver, no transform. Asking again gives the same answer.
    Absent,
    /// The device is there and its sessions are taken (an NVENC session
    /// limit, a video session the driver could not allocate): asking again
    /// once another export ends can succeed.
    Busy,
    /// The encoder runs here but refuses this output: its size, its pixels,
    /// or an option the export needs.
    Unsuitable,
    /// Nothing typed says which.
    Unknown,
}

impl OpenFailureKind {
    /// A stable name for logs and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Busy => "busy",
            Self::Unsuitable => "unsuitable",
            Self::Unknown => "unknown",
        }
    }
}

/// One encoder that did not open, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderRefusal {
    /// The encoder, as `FFmpeg` names it (`h264_nvenc`).
    pub encoder: String,
    /// Whether it was tried on the GPU.
    pub hardware_accelerated: bool,
    /// What the refusal is evidence of.
    pub kind: OpenFailureKind,
    /// The encoder's own words, with the log lines it wrote.
    pub reason: String,
}

impl DecodeError {
    /// Returns whether the decoder must be discarded instead of retried.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Unsupported(_) | Self::Adapter(_) | Self::Gpu { .. }
        )
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("decode was cancelled"),
            Self::OutOfRange => formatter.write_str("decode request is out of range"),
            Self::Unsupported(message) => write!(formatter, "unsupported media: {message}"),
            Self::Adapter(message) | Self::Gpu { message, .. } => {
                write!(formatter, "decoder adapter failed: {message}")
            }
        }
    }
}

impl Error for DecodeError {}

/// Stateful media decoder implemented by a platform binding.
pub trait Decoder {
    /// Returns deterministic stream metadata and stream selection.
    fn streams(&self) -> &[StreamInfo];
    /// Flushes queued packets and frames without changing stream selection.
    ///
    /// # Errors
    /// Returns a platform-neutral adapter failure when flushing fails.
    fn flush(&mut self) -> Result<(), DecodeError>;
    /// Seeks and returns what is presented at the request's time.
    ///
    /// For video that is the frame whose presentation interval contains the
    /// time: the last frame whose timestamp is at or before the time's
    /// presentation deadline, the time plus the stream's presentation slack
    /// ([`alpha2_time::presentation_slack`]), which absorbs the rounding a
    /// stored timestamp went through. A seek starts from the keyframe at or
    /// before that deadline. Deciding so takes the frame after, which the
    /// next [`Self::decode_next`] returns. A stream whose first frame is later
    /// answers with that frame, and past the last frame the last one answers,
    /// however far past: a stream shows its last picture after its end as it
    /// shows its first before its start. A stream with no frame at all is
    /// [`DecodeError::OutOfRange`]. For audio it is the block holding the
    /// time's sample.
    ///
    /// # Errors
    /// Returns cancellation, range, unsupported-media, or adapter failures.
    fn seek(
        &mut self,
        request: DecodeRequest,
        cancel: &CancelToken,
    ) -> Result<DecodeResult, DecodeError>;
    /// Advances packet draining by one observable event.
    ///
    /// # Errors
    /// Returns cancellation, unsupported-media, or adapter failures.
    fn decode_next(&mut self, cancel: &CancelToken) -> Result<DecodeEvent, DecodeError>;
}

/// A provider's answer to one request: source samples, and what the source
/// holds on either side of them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioSpan<'a> {
    /// Source samples at the project rate, read by their own `start_sample`.
    pub block: &'a AudioBlock,
    /// Whether the source has no samples before `block.start_sample`: the
    /// block begins where the source's audio does, such as a stream that
    /// starts 50 ms into its container.
    pub starts_source: bool,
    /// Whether the source has no samples from `block.end_sample()` on: the
    /// block ends where the source's audio does.
    pub ends_source: bool,
}

impl<'a> AudioSpan<'a> {
    /// A block that holds its source's entire audio, as a source decoded
    /// once from end to end is.
    #[must_use]
    pub const fn whole(block: &'a AudioBlock) -> Self {
        Self {
            block,
            starts_source: true,
            ends_source: true,
        }
    }

    /// Whether the span accounts for every sample of `[start, end)`: each is
    /// in the block, or before it with [`Self::starts_source`], or after it
    /// with [`Self::ends_source`], where the source is silent.
    #[must_use]
    pub fn covers(&self, start: i64, end: i64) -> bool {
        (self.starts_source || self.block.start_sample <= start)
            && (self.ends_source || self.block.end_sample() >= end)
    }
}

/// Retrieves project-rate planar audio without exposing its backing cache.
pub trait AudioBlockProvider {
    /// Returns source samples `[start_sample, start_sample + frames)` at the
    /// project rate, as a span that [covers](AudioSpan::covers) all of them.
    ///
    /// The block may start before `start_sample` and end after the request.
    /// Where the source has no audio, before its first sample or after its
    /// last, the provider either holds zeros there or says so with
    /// [`AudioSpan::starts_source`] and [`AudioSpan::ends_source`], which
    /// spares allocating silence. A provider that cannot yet account for the
    /// whole request returns `None`, or a span that does not cover it: the
    /// mixer then reports the source missing instead of mixing the gap as
    /// silence and calling the block complete.
    fn block(&self, source: &SourceId, start_sample: i64, frames: usize) -> Option<AudioSpan<'_>>;
}

/// Receives project-rate planar audio from offline or realtime graph hosts.
pub trait AudioBlockSink {
    /// Writes one monotonic audio block.
    ///
    /// # Errors
    /// Returns a platform-neutral encode error on cancellation, invalid timing,
    /// or host failure.
    fn write_audio(&mut self, block: &AudioBlock, cancel: &CancelToken) -> Result<(), EncodeError>;

    /// Drains and commits the sink.
    ///
    /// # Errors
    /// Returns cancellation or host failures; success means all delayed
    /// samples were committed.
    fn finish_audio(&mut self, cancel: &CancelToken) -> Result<(), EncodeError>;

    /// Discards partial output.
    ///
    /// # Errors
    /// Returns an error when cleanup fails.
    fn abort_audio(&mut self) -> Result<(), EncodeError>;
}

/// One input accepted by an encoding/mux adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum EncodeEvent {
    /// Premultiplied rendered video.
    Video(DecodedVideoFrame),
    /// Project-rate planar audio.
    Audio(AudioBlock),
}

/// Typed encoding failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodeError {
    /// The operation was cancelled; partial output must be removed by policy.
    Cancelled,
    /// Input timestamps violated monotonic/interleave requirements.
    InvalidTimestamp,
    /// Adapter failure without leaking a platform error type.
    Adapter(String),
    /// The output file could not be created or claimed: it already exists,
    /// its path is unusable, or the file system refused. Nothing was encoded.
    Output(String),
    /// Another export, live in this process or another, is writing the same
    /// output: it holds the output's partial file. Nothing was encoded, and
    /// the other export's file is untouched.
    OutputInUse(String),
    /// The output was encoded in full, but it could not be moved to its
    /// final path. The finished file is never deleted for that: it is kept
    /// beside the target under another name.
    Publish {
        /// The target, the step that failed and the operating system's error.
        message: String,
        /// Where the finished file is now; `None` when even moving it there
        /// failed, which `message` then explains.
        kept: Option<std::path::PathBuf>,
    },
    /// The encoder refused one input frame on a device that is still usable:
    /// the same frames may encode on another path.
    EncoderRejected {
        /// The encoder that refused, as `FFmpeg` names it (`h264_d3d12va`).
        encoder: String,
        /// The frame it refused, counted from 0 in the order frames were sent.
        frame: u64,
        /// What it said, and the state of the session when it did.
        message: String,
    },
    /// A GPU call the adapter made failed, and its platform status says what
    /// that was evidence of. `message` names the call and the status.
    Gpu {
        /// What the status says about the device.
        fault: GpuFault,
        /// The failed call and its platform status.
        message: String,
    },
    /// No encoder candidate opened. Each candidate's refusal says whether it
    /// is missing here, busy, or unsuited to this output, so a caller can tell
    /// a retry that will succeed later from a request that never will.
    EncodersRefused {
        /// Every candidate's refusal, in the order they were tried.
        message: String,
        /// The refusals themselves, most preferred candidate first.
        refusals: Vec<EncoderRefusal>,
    },
}

impl EncodeError {
    /// A wait on GPU work an encode needs, for `what`, that timed out: a
    /// [`GpuFault::Timeout`] whose message carries [`GPU_TIMEOUT_MESSAGE`].
    #[must_use]
    pub fn gpu_timeout(what: impl fmt::Display) -> Self {
        Self::Gpu {
            fault: GpuFault::Timeout,
            message: format!("{what}: {GPU_TIMEOUT_MESSAGE}"),
        }
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("encode was cancelled"),
            Self::InvalidTimestamp => formatter.write_str("encode timestamp is invalid"),
            Self::Adapter(message)
            | Self::Gpu { message, .. }
            | Self::EncodersRefused { message, .. } => {
                write!(formatter, "encoder adapter failed: {message}")
            }
            Self::EncoderRejected { message, .. } => formatter.write_str(message),
            Self::Output(message) | Self::OutputInUse(message) => {
                write!(formatter, "cannot create the output: {message}")
            }
            Self::Publish {
                message,
                kept: Some(kept),
            } => write!(
                formatter,
                "cannot publish the output: {message}; the finished render is kept at {}",
                kept.display()
            ),
            Self::Publish {
                message,
                kept: None,
            } => write!(
                formatter,
                "cannot publish the output: {message}; the finished render could not be kept"
            ),
        }
    }
}

impl Error for EncodeError {}

/// Host encoding and muxing boundary.
pub trait EncodeSink {
    /// Queues one timestamped media item.
    ///
    /// # Errors
    /// Returns cancellation, invalid timestamp, or adapter failures.
    fn write(&mut self, event: EncodeEvent, cancel: &CancelToken) -> Result<(), EncodeError>;
    /// Drains encoder delay, interleaves remaining packets, and writes the trailer.
    ///
    /// # Errors
    /// Returns cancellation or adapter failures; success means the trailer exists.
    fn finish(&mut self, cancel: &CancelToken) -> Result<(), EncodeError>;
    /// Aborts and applies the adapter's partial-output cleanup policy.
    ///
    /// # Errors
    /// Returns an adapter failure if partial output could not be cleaned up.
    fn abort(&mut self) -> Result<(), EncodeError>;
}

#[cfg(test)]
mod tests {
    use super::SampleAspectRatio;

    #[test]
    fn a_pixel_aspect_ratio_is_kept_in_lowest_terms_and_shows_as_a_browser_shows_it() {
        let ratio = |num, den| SampleAspectRatio { num, den };
        assert_eq!(SampleAspectRatio::new(8, 6), Some(ratio(4, 3)));
        assert_eq!(SampleAspectRatio::new(0, 1), None);
        assert_eq!(SampleAspectRatio::new(1, 0), None);
        assert_eq!(SampleAspectRatio::new(u64::from(u32::MAX) + 1, 1), None);
        // A display size rounds the stretched side: the declared ratio is the simplest that rounds
        // to it.
        let shown = |visible, displayed| SampleAspectRatio::of_display(visible, displayed);
        assert_eq!(shown((128, 96), (171, 96)), Some(ratio(4, 3)));
        assert_eq!(shown((96, 128), (96, 171)), Some(ratio(3, 4)));
        assert_eq!(shown((720, 576), (1024, 576)), Some(ratio(64, 45)));
        assert_eq!(shown((720, 480), (655, 480)), Some(ratio(10, 11)));
        assert_eq!(shown((720, 480), (720, 524)), Some(ratio(11, 12)));
        assert_eq!(shown((1440, 1080), (1920, 1080)), Some(ratio(4, 3)));
        assert_eq!(shown((128, 96), (128, 96)), Some(ratio(1, 1)));
        // Whatever it finds rounds to the size shown.
        for (visible, displayed) in [
            ((128, 96), (170, 96)),
            ((1000, 999), (1777, 999)),
            ((3, 2), (5, 2)),
        ] {
            let found = shown(visible, displayed).expect("a ratio");
            assert_eq!(
                found.square_pixel_size(visible.0, visible.1),
                displayed,
                "{visible:?} {displayed:?}"
            );
        }
        assert_eq!(shown((0, 96), (171, 96)), None);
        // mediabunny: Math.round(128 * 4 / 3) = 171, and a narrow pixel stretches the height.
        assert_eq!(ratio(4, 3).square_pixel_size(128, 96), (171, 96));
        assert_eq!(ratio(64, 45).square_pixel_size(720, 576), (1024, 576));
        assert_eq!(ratio(3, 4).square_pixel_size(96, 128), (96, 171));
        assert_eq!(ratio(1, 1).square_pixel_size(65, 49), (65, 49));
        // Half a pixel rounds up: 3 * 3 / 2 = 4.5.
        assert_eq!(ratio(3, 2).square_pixel_size(3, 7), (5, 7));
    }

    use super::*;

    #[test]
    fn a_frame_size_is_bounded_on_each_side_and_in_area() {
        assert!(frame_size_supported(16_384, 4_096));
        assert!(frame_size_supported(8_192, 8_192));
        assert!(!frame_size_supported(8_193, 8_192));
        assert!(!frame_size_supported(16_385, 1));
        assert!(!frame_size_supported(1, 16_385));
        assert!(!frame_size_supported(u32::MAX, u32::MAX));
    }

    #[test]
    fn cancellation_is_shared_and_monotonic() {
        let token = CancelToken::default();
        let other = token.clone();
        assert!(!other.is_cancelled());
        token.cancel();
        assert!(other.is_cancelled());
    }

    #[test]
    fn a_layout_names_each_plane_by_ffmpegs_channel_names() {
        let lcr = SpeakerLayout::new(&[
            Speaker::FrontCenter,
            Speaker::FrontLeft,
            Speaker::FrontRight,
        ])
        .expect("layout");
        assert_eq!(lcr.channels(), 3);
        assert_eq!(lcr.describe(), "FC+FL+FR");
        assert_eq!(format!("{lcr:?}"), r#"["FC", "FL", "FR"]"#);
        for (speaker, name, channel) in Speaker::TABLE {
            assert_eq!(speaker.name(), name);
            assert_eq!(Speaker::from_name(name), Some(speaker));
            assert_eq!(speaker.ffmpeg_channel(), channel);
            assert_eq!(Speaker::from_ffmpeg_channel(i64::from(channel)), speaker);
        }
        // Unused (0x200), ambisonic (0x400 and up) and user channels name no speaker.
        for channel in [-1, 18, 0x200, 0x400, 0x401, 0x7ff, 1_000] {
            assert_eq!(Speaker::from_ffmpeg_channel(channel), Speaker::Unnamed);
        }
        assert_eq!(Speaker::from_name("front left"), None);
        assert!(SpeakerLayout::new(&[]).is_none());
        assert!(SpeakerLayout::new(&[Speaker::FrontLeft; MAX_AUDIO_CHANNELS + 1]).is_none());
    }

    #[test]
    fn a_source_that_declares_no_speaker_has_ffmpegs_default_for_its_count() {
        let names = |channels| {
            SpeakerLayout::default_for(channels)
                .expect("a default")
                .describe()
        };
        assert_eq!(names(1), "FC");
        assert_eq!(names(3), "FL+FR+LFE");
        assert_eq!(names(4), "FL+FR+FC+BC");
        assert_eq!(names(8), "FL+FR+FC+LFE+BL+BR+SL+SR");
        assert_eq!(names(9), "FL+FR+FC+LFE+BL+BR+SL+SR+UNK");
        assert_eq!(names(10), "FL+FR+FC+LFE+SL+SR+TFL+TFR+TBL+TBR");
        assert_eq!(SpeakerLayout::default_for(24).expect("22.2").channels(), 24);
        assert!(SpeakerLayout::default_for(0).is_none());
        assert!(SpeakerLayout::default_for(MAX_AUDIO_CHANNELS + 1).is_none());
        let unnamed = [Speaker::Unnamed; 2];
        assert_eq!(
            SpeakerLayout::declared_or_default(&unnamed),
            Some((SpeakerLayout::default_for(2).expect("stereo"), true))
        );
        let partly = [Speaker::FrontCenter, Speaker::Unnamed];
        assert_eq!(
            SpeakerLayout::declared_or_default(&partly),
            Some((SpeakerLayout::new(&partly).expect("layout"), false))
        );
    }

    #[test]
    fn a_span_covers_a_request_only_where_it_holds_samples_or_knows_silence() {
        let block = AudioBlock {
            start_sample: 100,
            sample_rate: 48_000,
            speakers: SpeakerLayout::default_for(1).expect("mono"),
            channels: vec![vec![0.5; 50]],
        };
        let partial = AudioSpan {
            block: &block,
            starts_source: false,
            ends_source: false,
        };
        assert!(partial.covers(100, 150));
        assert!(partial.covers(120, 130));
        assert!(!partial.covers(99, 150), "a sample before the block");
        assert!(!partial.covers(100, 151), "a sample after the block");
        let starts = AudioSpan {
            starts_source: true,
            ..partial
        };
        assert!(starts.covers(0, 150));
        assert!(!starts.covers(0, 151));
        assert!(AudioSpan::whole(&block).covers(-5, 1_000));
    }

    #[test]
    fn text_descriptor_and_generic_pixels_validate_without_compositor_types() {
        let texture = HostTexture {
            content_version: 2,
            width: 4,
            height: 3,
            premultiplication: Premultiplication::Premultiplied,
            color_space: RasterColorSpace::Srgb,
        };
        let text = TextTexture {
            texture: texture.clone(),
            layout_x: 0.0,
            layout_y: 0.0,
            layout_width: 4.0,
            layout_height: 3.0,
            first_baseline: 2.0,
            ascent: 2.0,
            descent: 1.0,
            line_count: 1,
        };
        assert!(text.is_valid());
        assert!(
            HostTexturePixels {
                texture,
                rgba8: vec![0; 4 * 3 * 4],
            }
            .is_valid()
        );
    }
}
