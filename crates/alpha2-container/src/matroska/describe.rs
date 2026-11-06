//! Track entries read into the vocabulary, by the rules of `FFmpeg`'s Matroska demuxer.

use alpha2_media::{
    ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, Orientation,
    SampleAspectRatio, SpeakerLayout, TransferFunction, frame_size_supported,
};
use alpha2_time::FrameRate;
use bytes::{Bytes, BytesMut};

use super::ebml::{Children, float, id, sint, string, uint};
use crate::audio::ogg::VorbisModes;
use crate::audio::{flac, speakers_from_mask, vorbis_order};
use crate::{
    AudioCodec, AudioTrack, DemuxError, FourCc, PcmFormat, TrackId, TrackInfo, TrackKind,
    VideoCodec, VideoTrack,
};

/// A `TrackEntry` as stored, with `FFmpeg`'s defaults for absent elements.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub number: u64,
    pub uid: u64,
    pub kind: u64,
    pub codec_id: String,
    pub private: Bytes,
    pub codec_delay: u64,
    pub language: String,
    pub name: Option<String>,
    pub default_duration: u64,
    pub track_scale: f64,
    pub flag_default: bool,
    pub flag_enabled: bool,
    pub encoding: Encoding,
    pub alpha_addition: bool,
    pub video: VideoFields,
    pub audio: AudioFields,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VideoFields {
    pub pixel_width: u64,
    pub pixel_height: u64,
    pub crop: [u64; 4],
    pub display_width: Option<u64>,
    pub display_height: Option<u64>,
    pub display_unit: u64,
    pub stereo_mode: Option<u64>,
    pub frame_rate: f64,
    pub alpha_mode: u64,
    pub colour: Option<Colour>,
    pub projection: Option<Projection>,
}

/// What a track's first `ContentEncoding` does to its frames.
#[derive(Clone, Debug)]
pub(crate) enum Encoding {
    None,
    /// Header stripping (`ContentCompAlgo` 3): these bytes start every frame.
    Stripped(Bytes),
    /// Another compression, or an encryption: the frames cannot be read.
    Unreadable,
}

/// The `Colour` element's codes (ITU-T H.273), unspecified where absent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Colour {
    pub matrix: u64,
    pub range: u64,
    pub transfer: u64,
    pub primaries: u64,
    pub siting: (u64, u64),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Projection {
    pub kind: u64,
    pub private: Bytes,
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AudioFields {
    pub sampling: f64,
    pub output: f64,
    pub channels: u64,
    pub bit_depth: u64,
}

impl Default for AudioFields {
    fn default() -> Self {
        Self {
            sampling: 8000.0,
            output: 0.0,
            channels: 1,
            bit_depth: 0,
        }
    }
}

/// The track types `FFmpeg` turns into streams, with the first letter their codec IDs need.
const VIDEO: u64 = 1;
const AUDIO: u64 = 2;
const SUBTITLE: u64 = 17;
const METADATA: u64 = 33;

/// Reads a `TrackEntry` payload.
pub(crate) fn entry(payload: &[u8]) -> Entry {
    let mut entry = Entry {
        number: 0,
        uid: 0,
        kind: 0,
        codec_id: String::new(),
        private: Bytes::new(),
        codec_delay: 0,
        language: "eng".into(),
        name: None,
        default_duration: 0,
        track_scale: 1.0,
        flag_default: true,
        flag_enabled: true,
        encoding: Encoding::None,
        alpha_addition: false,
        video: VideoFields::default(),
        audio: AudioFields::default(),
    };
    let mut private_stripped = Bytes::new();
    for (element, body) in Children::new(payload) {
        let number = || uint(body).unwrap_or(0);
        match element {
            id::TRACK_NUMBER => entry.number = number(),
            id::TRACK_UID => entry.uid = number(),
            id::TRACK_TYPE => entry.kind = number(),
            id::CODEC_ID => entry.codec_id = string(body),
            id::CODEC_PRIVATE => entry.private = Bytes::copy_from_slice(body),
            id::CODEC_DELAY => entry.codec_delay = number(),
            id::LANGUAGE => entry.language = string(body),
            id::NAME => entry.name = Some(string(body)),
            id::DEFAULT_DURATION => entry.default_duration = number(),
            id::TRACK_TIMESTAMP_SCALE => entry.track_scale = float(body).unwrap_or(1.0),
            id::FLAG_DEFAULT => entry.flag_default = number() != 0,
            id::FLAG_ENABLED => entry.flag_enabled = number() != 0,
            id::VIDEO => entry.video = video_fields(body),
            id::AUDIO => entry.audio = audio_fields(body),
            id::CONTENT_ENCODINGS => (entry.encoding, private_stripped) = encodings(body),
            id::BLOCK_ADDITION_MAPPING => {
                // A mapping of type 0 or 1 marks codec-specific additions, such as VP9 alpha.
                entry.alpha_addition |= Children::new(body).all(|(element, value)| {
                    element != id::BLOCK_ADD_ID_TYPE || uint(value).unwrap_or(0) <= 1
                });
            }
            _ => {}
        }
    }
    if !private_stripped.is_empty() {
        let mut private = BytesMut::from(&private_stripped[..]);
        private.extend_from_slice(&entry.private);
        entry.private = private.freeze();
    }
    entry
}

fn video_fields(payload: &[u8]) -> VideoFields {
    let mut video = VideoFields::default();
    for (element, body) in Children::new(payload) {
        let number = || uint(body).unwrap_or(0);
        match element {
            id::PIXEL_WIDTH => video.pixel_width = number(),
            id::PIXEL_HEIGHT => video.pixel_height = number(),
            id::PIXEL_CROP_TOP => video.crop[0] = number(),
            id::PIXEL_CROP_BOTTOM => video.crop[1] = number(),
            id::PIXEL_CROP_LEFT => video.crop[2] = number(),
            id::PIXEL_CROP_RIGHT => video.crop[3] = number(),
            id::DISPLAY_WIDTH => video.display_width = Some(number()),
            id::DISPLAY_HEIGHT => video.display_height = Some(number()),
            id::DISPLAY_UNIT => video.display_unit = number(),
            id::STEREO_MODE => video.stereo_mode = Some(number()),
            id::FRAME_RATE => video.frame_rate = float(body).unwrap_or(0.0),
            id::ALPHA_MODE => video.alpha_mode = number(),
            id::COLOUR => video.colour = Some(colour(body)),
            id::PROJECTION => video.projection = Some(projection(body)),
            _ => {}
        }
    }
    video
}

fn colour(payload: &[u8]) -> Colour {
    let mut colour = Colour {
        matrix: 2,
        range: 0,
        transfer: 2,
        primaries: 2,
        siting: (0, 0),
    };
    for (element, body) in Children::new(payload) {
        let value = uint(body).unwrap_or(0);
        match element {
            id::MATRIX_COEFFICIENTS => colour.matrix = value,
            id::RANGE => colour.range = value,
            id::TRANSFER_CHARACTERISTICS => colour.transfer = value,
            id::PRIMARIES => colour.primaries = value,
            id::CHROMA_SITING_HORZ => colour.siting.0 = value,
            id::CHROMA_SITING_VERT => colour.siting.1 = value,
            _ => {}
        }
    }
    colour
}

fn projection(payload: &[u8]) -> Projection {
    let mut projection = Projection::default();
    for (element, body) in Children::new(payload) {
        match element {
            id::PROJECTION_TYPE => projection.kind = uint(body).unwrap_or(0),
            id::PROJECTION_PRIVATE => projection.private = Bytes::copy_from_slice(body),
            id::PROJECTION_POSE_YAW => projection.yaw = float(body).unwrap_or(0.0),
            id::PROJECTION_POSE_PITCH => projection.pitch = float(body).unwrap_or(0.0),
            id::PROJECTION_POSE_ROLL => projection.roll = float(body).unwrap_or(0.0),
            _ => {}
        }
    }
    projection
}

fn audio_fields(payload: &[u8]) -> AudioFields {
    let mut audio = AudioFields::default();
    for (element, body) in Children::new(payload) {
        match element {
            id::SAMPLING_FREQUENCY => audio.sampling = float(body).unwrap_or(8000.0),
            id::OUTPUT_SAMPLING_FREQUENCY => audio.output = float(body).unwrap_or(0.0),
            id::CHANNELS => audio.channels = uint(body).unwrap_or(1),
            id::BIT_DEPTH => audio.bit_depth = uint(body).unwrap_or(0),
            _ => {}
        }
    }
    audio
}

/// The first `ContentEncoding`: what it does to frames, and the bytes it strips from the codec
/// private data.
fn encodings(payload: &[u8]) -> (Encoding, Bytes) {
    const ENCODING: u32 = 0x6240;
    const SCOPE: u32 = 0x5032;
    const TYPE: u32 = 0x5033;
    const COMPRESSION: u32 = 0x5034;
    const ALGORITHM: u32 = 0x4254;
    const SETTINGS: u32 = 0x4255;
    let Some((_, encoding)) = Children::new(payload).find(|(element, _)| *element == ENCODING)
    else {
        return (Encoding::None, Bytes::new());
    };
    let mut scope = 1;
    let mut kind = 0;
    let mut algorithm = 0;
    let mut settings = Bytes::new();
    for (element, body) in Children::new(encoding) {
        match element {
            SCOPE => scope = uint(body).unwrap_or(1),
            TYPE => kind = uint(body).unwrap_or(0),
            COMPRESSION => {
                for (inner, value) in Children::new(body) {
                    match inner {
                        ALGORITHM => algorithm = uint(value).unwrap_or(0),
                        SETTINGS => settings = Bytes::copy_from_slice(value),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let frames = scope & 1 != 0;
    if kind != 0 || algorithm != 3 {
        let encoding = if frames {
            Encoding::Unreadable
        } else {
            Encoding::None
        };
        return (encoding, Bytes::new());
    }
    let private = if scope & 2 != 0 {
        settings.clone()
    } else {
        Bytes::new()
    };
    let encoding = if frames && !settings.is_empty() {
        Encoding::Stripped(settings)
    } else {
        Encoding::None
    };
    (encoding, private)
}

/// Whether `FFmpeg` turns `entry` into a stream: a known track type and a codec ID whose first
/// letter fits it.
pub(crate) fn listed(entry: &Entry) -> bool {
    let first = entry.codec_id.as_bytes().first().copied();
    match entry.kind {
        VIDEO => first == Some(b'V'),
        AUDIO => first == Some(b'A'),
        SUBTITLE | METADATA => matches!(first, Some(b'D' | b'S')),
        _ => false,
    }
}

/// How `FFmpeg`'s generic demuxing layer times and flags a track's packets.
#[derive(Clone, Debug)]
pub(crate) enum Timing {
    /// Decode times from presentation times reordered `depth` deep (H.264, HEVC, MPEG-4 Part 2
    /// with B-frames), a duration from the frame rate when the block has none.
    Video {
        depth: usize,
        frame: Option<(u64, u64)>,
    },
    /// Audio a codec parser times, in samples at `rate`.
    Parsed { parser: Parser, rate: u32 },
    /// AAC, whose decoder learns its frame length from the first packet.
    Aac { frame: i64, rate: u32 },
    /// PCM with `bytes` per sample frame of all channels.
    Pcm { bytes: u32, rate: u32 },
    /// Nothing filled in.
    Plain,
}

/// The codec parsers that time packets.
#[derive(Clone, Debug)]
pub(crate) enum Parser {
    Opus,
    Vorbis(Box<VorbisModes>),
    Flac,
    Ac3,
    Dts,
    Mpeg,
}

/// What a listed track entry becomes.
pub(crate) struct Described {
    pub info: TrackInfo,
    pub timing: Timing,
    /// Every packet decodes on its own (`AV_CODEC_PROP_INTRA_ONLY`, or a subtitle).
    pub intra: bool,
    /// Block timestamps are decode times (`V_MS/VFW/FOURCC`).
    pub decode_times: bool,
    /// `CodecDelay` in the track's units.
    pub delay: i64,
    /// `FFmpeg`'s time base for the track, `num / den` seconds. Packets carry times in its units
    /// times `num`, so the track's timescale is `den`.
    pub time_base: (u64, u64),
}

fn invalid(reason: &str) -> DemuxError {
    DemuxError::Invalid(reason.into())
}

fn greatest_common_divisor(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// `a * b / c` rounded to nearest, halves away from zero (`av_rescale`).
pub(crate) fn rescale(a: i64, b: i64, c: i64) -> i64 {
    crate::mp4::index::rescale(a, b, c)
}

/// The best rational approximation of `num / den` with terms at most `max` (`av_reduce`).
pub(crate) fn reduce(num: u64, den: u64, max: u64) -> (u64, u64) {
    let divisor = greatest_common_divisor(num, den).max(1);
    let (mut num, mut den) = (u128::from(num / divisor), u128::from(den / divisor));
    let max = u128::from(max);
    let (mut a0, mut a1) = ((0_u128, 1_u128), (1_u128, 0_u128));
    if num <= max && den <= max {
        a1 = (num, den);
        den = 0;
    }
    while den != 0 {
        let x = num / den;
        let next_den = num - den * x;
        let (a2n, a2d) = (x * a1.0 + a0.0, x * a1.1 + a0.1);
        if a2n > max || a2d > max {
            let mut x = x;
            if let Some(bound) = (max - a0.0).checked_div(a1.0) {
                x = bound;
            }
            if let Some(bound) = (max - a0.1).checked_div(a1.1) {
                x = x.min(bound);
            }
            if den * (2 * x * a1.1 + a0.1) > num * a1.1 {
                a1 = (x * a1.0 + a0.0, x * a1.1 + a0.1);
            }
            break;
        }
        a0 = a1;
        a1 = (a2n, a2d);
        num = den;
        den = next_den;
    }
    (
        u64::try_from(a1.0).unwrap_or(0),
        u64::try_from(a1.1).unwrap_or(0),
    )
}

/// Describes a listed track entry; `scale` is the segment's `TimestampScale`.
pub(crate) fn describe(entry: &Entry, scale: u64) -> Result<Described, DemuxError> {
    let track_scale = if entry.track_scale < 0.01 || entry.track_scale.is_nan() {
        1.0
    } else {
        entry.track_scale
    };
    #[allow(clippy::cast_precision_loss)]
    let product = scale as f64 * track_scale;
    if product.is_nan() || product > f64::from(u32::MAX) {
        return Err(invalid(
            "a Matroska track's timestamp scale is out of range",
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let num = (product as u64).max(1);
    let divisor = greatest_common_divisor(num, 1_000_000_000);
    let time_base = (num / divisor, 1_000_000_000 / divisor);
    let delay = rescale(
        i64::try_from(entry.codec_delay).unwrap_or(i64::MAX),
        i64::try_from(time_base.1).unwrap_or(1),
        i64::try_from(time_base.0.saturating_mul(1_000_000_000)).unwrap_or(i64::MAX),
    );
    let language = (entry.language != "und").then(|| entry.language.clone());
    let mut info = TrackInfo {
        id: TrackId(u32::try_from(entry.number).unwrap_or(u32::MAX)),
        kind: TrackKind::Other,
        timescale: u32::try_from(time_base.1).unwrap_or(u32::MAX),
        duration: None,
        language,
        title: entry.name.clone(),
        default: entry.flag_default,
        enabled: entry.flag_enabled,
        video: None,
        audio: None,
    };
    let mut described = Described {
        info: info.clone(),
        timing: Timing::Plain,
        intra: false,
        decode_times: false,
        delay,
        time_base,
    };
    if matches!(entry.encoding, Encoding::Unreadable) {
        return Ok(described);
    }
    match entry.kind {
        VIDEO => {
            let (video, timing, intra, decode_times) = video_track(entry)?;
            info.kind = TrackKind::Video;
            info.video = Some(video);
            described.timing = timing;
            described.intra = intra;
            described.decode_times = decode_times;
        }
        AUDIO => {
            let (audio, timing, intra) = audio_track(entry);
            info.kind = TrackKind::Audio;
            info.audio = Some(audio);
            described.timing = timing;
            described.intra = intra;
        }
        _ => {
            // Every subtitle codec `FFmpeg` knows decodes packet by packet.
            described.intra = SUBTITLE_CODECS
                .iter()
                .any(|prefix| entry.codec_id.starts_with(prefix));
        }
    }
    described.info = info;
    Ok(described)
}

const SUBTITLE_CODECS: [&str; 12] = [
    "D_WEBVTT/",
    "S_TEXT/UTF8",
    "S_TEXT/ASCII",
    "S_TEXT/ASS",
    "S_TEXT/SSA",
    "S_ASS",
    "S_SSA",
    "S_VOBSUB",
    "S_DVBSUB",
    "S_HDMV/PGS",
    "S_HDMV/TEXTST",
    "S_ARIBSUB",
];

fn fourcc_of(text: &str) -> FourCc {
    let mut code = [b' '; 4];
    for (slot, byte) in code.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
    FourCc(code)
}

type VideoParts = (VideoTrack, Timing, bool, bool);

fn video_track(entry: &Entry) -> Result<VideoParts, DemuxError> {
    let fields = &entry.video;
    let codec_id = entry.codec_id.as_str();
    let mut private = entry.private.clone();
    let mut decode_times = false;
    let (codec, intra) = if codec_id == "V_MS/VFW/FOURCC" && private.len() >= 40 {
        decode_times = true;
        let fourcc = FourCc([private[16], private[17], private[18], private[19]]);
        private = private.slice(40..);
        let intra = matches!(&fourcc.0, b"MJPG" | b"mjpa" | b"AVRn");
        (VideoCodec::Other(fourcc), intra)
    } else if codec_id.starts_with("V_MPEG4/ISO/AVC") {
        (VideoCodec::H264, false)
    } else if codec_id.starts_with("V_MPEGH/ISO/HEVC") {
        (VideoCodec::Hevc, false)
    } else if codec_id.starts_with("V_VP8") {
        (VideoCodec::Vp8, false)
    } else if codec_id.starts_with("V_VP9") {
        // `FFmpeg` ignores the VP9 feature list and exports no decoder configuration.
        private = Bytes::new();
        (VideoCodec::Vp9, false)
    } else if codec_id.starts_with("V_AV1") {
        (VideoCodec::Av1, false)
    } else if ["V_MPEG4/ISO/ASP", "V_MPEG4/ISO/AP", "V_MPEG4/ISO/SP"]
        .iter()
        .any(|prefix| codec_id.starts_with(prefix))
    {
        (VideoCodec::Mpeg4Part2, false)
    } else if codec_id.starts_with("V_PRORES") {
        let code = match private.get(..4) {
            Some(code) if private.len() == 4 => FourCc([code[0], code[1], code[2], code[3]]),
            _ => FourCc(*b"apcn"),
        };
        (VideoCodec::ProRes(code), true)
    } else {
        let intra = ["V_MJPEG", "V_UNCOMPRESSED"]
            .iter()
            .any(|prefix| codec_id.starts_with(prefix));
        (VideoCodec::Other(fourcc_of(codec_id)), intra)
    };
    let [top, bottom, left, right] = fields.crop;
    let (width, height) = (fields.pixel_width, fields.pixel_height);
    if left.saturating_add(right) >= width && (left | right) != 0
        || top.saturating_add(bottom) >= height && (top | bottom) != 0
    {
        return Err(invalid("a Matroska track crops away its whole picture"));
    }
    let width = u32::try_from(width).unwrap_or(u32::MAX);
    let height = u32::try_from(height).unwrap_or(u32::MAX);
    if !frame_size_supported(width, height) {
        return Err(DemuxError::Invalid(format!(
            "{width}x{height} pictures are larger than alpha2 decodes"
        )));
    }
    let depth = match codec {
        VideoCodec::H264 => crate::mp4::nal::avc_reorder_depth(&private).unwrap_or(0),
        VideoCodec::Hevc => crate::mp4::nal::hevc_reorder_depth(&private).unwrap_or(0),
        VideoCodec::Mpeg4Part2 => usize::from(crate::mp4::nal::mpeg4_has_b_frames(&private)),
        _ => 0,
    };
    let frame = (entry.default_duration > 0 || fields.frame_rate > 0.0)
        .then(|| frame_rate(entry))
        .flatten();
    let video = VideoTrack {
        codec,
        codec_private: private,
        in_band_parameter_sets: false,
        width,
        height,
        orientation: orientation(fields.projection.as_ref()),
        sample_aspect_ratio: aspect(fields),
        color: fields.colour.as_ref().and_then(color),
        clean_aperture: None,
        frame_rate: frame.and_then(|(num, den)| {
            FrameRate::new(u32::try_from(num).ok()?, u32::try_from(den).ok()?).ok()
        }),
        alpha: fields.alpha_mode != 0 || entry.alpha_addition && codec == VideoCodec::Vp9,
    };
    let fps = frame.filter(|(num, den)| *num < den * 1000 && *num > den * 5);
    Ok((
        video,
        Timing::Video { depth, frame: fps },
        intra,
        decode_times,
    ))
}

/// The average frame rate `FFmpeg` takes from `DefaultDuration` (or the old `FrameRate`).
fn frame_rate(entry: &Entry) -> Option<(u64, u64)> {
    let mut duration = entry.default_duration;
    if duration == 0 {
        let derived = 1e9 / entry.video.frame_rate;
        #[allow(clippy::cast_precision_loss)]
        let limit = u64::MAX as f64;
        if !(0.0..=limit).contains(&derived) {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let derived = derived as u64;
        duration = derived;
    }
    if duration == 0 {
        return None;
    }
    let halve = u64::from(duration > i64::MAX as u64) + 1;
    let (num, den) = reduce(1_000_000_000 / halve, duration / halve, 30_000);
    (num > 0 && den > 0).then_some((num, den))
}

/// The display matrix a rectangular projection's pose gives, read the way `FFmpeg` reads display
/// matrices.
fn orientation(projection: Option<&Projection>) -> Orientation {
    let Some(projection) = projection else {
        return Orientation::UPRIGHT;
    };
    if projection.private.first().is_some_and(|byte| *byte != 0) || projection.kind != 0 {
        return Orientation::UPRIGHT;
    }
    let (yaw, pitch, roll) = (projection.yaw, projection.pitch, projection.roll);
    if pitch == 0.0 && yaw == 0.0 && roll == 0.0 {
        return Orientation::UPRIGHT;
    }
    // `FFmpeg` compares the pose with these exactly representable values.
    #[allow(clippy::float_cmp)]
    let flat = pitch == 0.0 && (yaw == 0.0 || yaw.abs() == 180.0) && !roll.is_nan();
    if !flat {
        return Orientation::UPRIGHT;
    }
    let hflip = yaw != 0.0;
    let angle = roll * if hflip { 1.0 } else { -1.0 };
    let radians = -angle.to_radians();
    #[allow(clippy::cast_possible_truncation)]
    let fixed = |value: f64| (value * 65536.0) as i32;
    let (cos, sin) = (radians.cos(), radians.sin());
    let mut matrix = [
        [fixed(cos), fixed(-sin), 0],
        [fixed(sin), fixed(cos), 0],
        [0, 0, 1 << 30],
    ];
    if hflip {
        for row in &mut matrix {
            row[0] = -row[0];
        }
    }
    crate::mp4::describe::orientation(&matrix)
}

/// The pixel aspect ratio the display size declares, as `FFmpeg` derives it; `None` when the
/// display unit is unknown.
fn aspect(fields: &VideoFields) -> Option<SampleAspectRatio> {
    if fields.display_unit >= 4 {
        return None;
    }
    let [top, bottom, left, right] = fields.crop;
    let cropped_width = fields.pixel_width.checked_sub(left + right)?;
    let cropped_height = fields.pixel_height.checked_sub(top + bottom)?;
    let (mut display_width, mut display_height) = (fields.display_width, fields.display_height);
    if fields.display_unit == 0 {
        display_width = display_width.or(Some(cropped_width));
        display_height = display_height.or(Some(cropped_height));
    }
    let (display_width, display_height) = (display_width?, display_height?);
    let (mut width_mul, mut height_mul) = (1_u64, 1_u64);
    match fields.stereo_mode {
        Some(1 | 11 | 13 | 14) => width_mul = 2,
        Some(2 | 3 | 12) => height_mul = 2,
        _ => {}
    }
    if display_width == 0 || display_height == 0 {
        return None;
    }
    let num = u128::from(cropped_height) * u128::from(display_width) * u128::from(width_mul);
    let den = u128::from(cropped_width) * u128::from(display_height) * u128::from(height_mul);
    if num == 0 || den == 0 || num > u128::from(u64::MAX) || den > u128::from(u64::MAX) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let (num, den) = reduce(num as u64, den as u64, i32::MAX as u64);
    SampleAspectRatio::new(num, den)
}

/// The `Colour` codes `FFmpeg` keeps, mapped as alpha2's `FFmpeg` host maps codec parameters:
/// `None` when nothing is declared, unrecognized values as BT.709 limited range.
fn color(colour: &Colour) -> Option<ColorMetadata> {
    const UNSPECIFIED: u64 = 2;
    let matrix = if colour.matrix == 3 {
        UNSPECIFIED
    } else {
        colour.matrix
    };
    let primaries = if matches!(colour.primaries, 0 | 3) {
        UNSPECIFIED
    } else {
        colour.primaries
    };
    let transfer = if matches!(colour.transfer, 0 | 3) {
        UNSPECIFIED
    } else {
        colour.transfer
    };
    let range = matches!(colour.range, 1 | 2).then_some(colour.range);
    if matrix == UNSPECIFIED
        && primaries == UNSPECIFIED
        && transfer == UNSPECIFIED
        && range.is_none()
    {
        return None;
    }
    let transfer = match transfer {
        16 => TransferFunction::Pq,
        18 => TransferFunction::Hlg,
        13 => TransferFunction::Srgb,
        _ => TransferFunction::Bt709,
    };
    Some(ColorMetadata {
        primaries: match primaries {
            9 => ColorPrimaries::Bt2020,
            5 | 6 => ColorPrimaries::Bt601,
            _ => ColorPrimaries::Bt709,
        },
        transfer,
        matrix: match matrix {
            9 => ColorMatrix::Bt2020Ncl,
            5 | 6 => ColorMatrix::Bt601,
            0 => ColorMatrix::Rgb,
            _ => ColorMatrix::Bt709,
        },
        range: if range == Some(2) {
            ColorRange::Full
        } else {
            ColorRange::Limited
        },
        dynamic_range: if matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg) {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        },
    })
}

/// A sample rate field as `FFmpeg` takes it: invalid ones read as 8000.
fn rate_of(value: f64) -> u32 {
    if value.is_nan() || !(0.0..=f64::from(i32::MAX)).contains(&value) {
        return 8000;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value as u32
    }
}

fn pcm(bits: u16, float: bool, big_endian: bool, signed: bool) -> PcmFormat {
    PcmFormat {
        bits,
        float,
        big_endian,
        signed,
    }
}

/// The codec of an `A_MS/ACM` `WAVEFORMATEX`, for the PCM formats.
fn acm_codec(private: &[u8]) -> Option<(AudioCodec, u32, u32)> {
    let le16 = |at: usize| {
        Some(u16::from_le_bytes([
            *private.get(at)?,
            *private.get(at + 1)?,
        ]))
    };
    let mut tag = le16(0)?;
    let channels = u32::from(le16(2)?);
    let rate = u32::from_le_bytes(private.get(4..8)?.try_into().ok()?);
    let bits = le16(14)?;
    if tag == 0xfffe && private.len() >= 40 {
        tag = le16(24)?;
    }
    let format = match (tag, bits) {
        (1, 8) => pcm(8, false, false, false),
        (1, 16 | 24 | 32) => pcm(bits, false, false, true),
        (3, 32 | 64) => pcm(bits, true, false, true),
        _ => return None,
    };
    Some((AudioCodec::Pcm(format), rate, channels))
}

type AudioParts = (AudioTrack, Timing, bool);

#[allow(clippy::too_many_lines)]
fn audio_track(entry: &Entry) -> AudioParts {
    let fields = entry.audio;
    let codec_id = entry.codec_id.as_str();
    let mut sample_rate = rate_of(if fields.output > 0.0 {
        fields.output
    } else {
        fields.sampling
    });
    let mut channels = u32::try_from(fields.channels).unwrap_or(u32::MAX);
    let bit_depth = u16::try_from(fields.bit_depth).unwrap_or(0);
    let mut private = entry.private.clone();
    let mut speakers = None;
    let mut bits_per_sample = None;
    let starts = |prefix: &str| codec_id.starts_with(prefix);
    let codec = if codec_id == "A_MS/ACM" && private.len() >= 14 {
        let found = acm_codec(&private);
        private = private.slice(private.len().min(18)..);
        match found {
            Some((codec, rate, count)) => {
                sample_rate = rate;
                channels = count;
                codec
            }
            None => AudioCodec::Other(FourCc(*b"A_MS")),
        }
    } else if starts("A_AAC") {
        if private.is_empty() {
            private = synthesized_aac_config(codec_id, &fields);
        }
        if let Some((rate, count)) = crate::mp4::describe::audio_specific_config(&private) {
            sample_rate = rate;
            if count > 0 {
                channels = count;
            }
        }
        AudioCodec::Aac
    } else if starts("A_AC3") {
        AudioCodec::Ac3
    } else if starts("A_EAC3") {
        AudioCodec::Eac3
    } else if starts("A_DTS") {
        AudioCodec::Dts
    } else if starts("A_FLAC") {
        if let Some((info, mask)) = flac_header(&private) {
            sample_rate = info.0;
            channels = info.1;
            bits_per_sample = u16::try_from(info.2).ok();
            speakers = mask
                .and_then(|mask| speakers_from_mask(mask, channels))
                .or_else(|| {
                    let default = flac::CHANNEL_MASKS.get(channels.checked_sub(1)? as usize)?;
                    speakers_from_mask(*default, channels)
                });
        }
        AudioCodec::Flac
    } else if starts("A_ALAC") {
        if private.len() >= 24 {
            bits_per_sample = Some(u16::from(private[5]));
            channels = u32::from(private[9]);
            sample_rate = u32::from_be_bytes([private[20], private[21], private[22], private[23]]);
        }
        AudioCodec::Alac
    } else if starts("A_MPEG/L3") {
        AudioCodec::Mp3
    } else if starts("A_OPUS") {
        sample_rate = 48_000;
        if private.len() >= 19 && private.starts_with(b"OpusHead") {
            channels = u32::from(private[9]);
            speakers = match private[18] {
                0 if channels <= 2 => vorbis_order(channels),
                1 => vorbis_order(channels),
                _ => None,
            };
        }
        AudioCodec::Opus
    } else if starts("A_VORBIS") {
        if let Some(headers) = xiph_headers(&private)
            && let Some(identification) = headers.first()
            && identification.len() >= 16
            && identification.starts_with(b"\x01vorbis")
        {
            channels = u32::from(identification[11]);
            sample_rate = u32::from_le_bytes([
                identification[12],
                identification[13],
                identification[14],
                identification[15],
            ]);
            speakers = vorbis_order(channels);
        }
        AudioCodec::Vorbis
    } else if starts("A_PCM/FLOAT/IEEE") {
        AudioCodec::Pcm(pcm(
            if bit_depth == 64 { 64 } else { 32 },
            true,
            false,
            true,
        ))
    } else if starts("A_PCM/INT/BIG") || starts("A_PCM/INT/LIT") {
        let big_endian = starts("A_PCM/INT/BIG");
        AudioCodec::Pcm(match bit_depth {
            8 => pcm(8, false, false, false),
            24 | 32 => pcm(bit_depth, false, big_endian, true),
            _ => pcm(16, false, big_endian, true),
        })
    } else {
        AudioCodec::Other(fourcc_of(codec_id))
    };
    if let AudioCodec::Pcm(format) = codec {
        bits_per_sample = Some(format.bits);
    }
    let rate = if codec == AudioCodec::Opus {
        48_000
    } else {
        sample_rate
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let priming = rescale(
        i64::try_from(entry.codec_delay).unwrap_or(i64::MAX),
        i64::from(rate),
        1_000_000_000,
    )
    .clamp(0, i64::from(u32::MAX)) as u32;
    let timing = match codec {
        AudioCodec::Aac => Timing::Aac {
            frame: if aac_doubles(&private) { 2048 } else { 1024 },
            rate: sample_rate,
        },
        AudioCodec::Pcm(format) => Timing::Pcm {
            bytes: u32::from(format.bits / 8).saturating_mul(channels),
            rate: sample_rate,
        },
        AudioCodec::Opus => Timing::Parsed {
            parser: Parser::Opus,
            rate: 48_000,
        },
        AudioCodec::Vorbis => match vorbis_modes(&private) {
            Some(modes) => Timing::Parsed {
                parser: Parser::Vorbis(Box::new(modes)),
                rate: sample_rate,
            },
            None => Timing::Plain,
        },
        AudioCodec::Flac => Timing::Parsed {
            parser: Parser::Flac,
            rate: sample_rate,
        },
        AudioCodec::Ac3 | AudioCodec::Eac3 => Timing::Parsed {
            parser: Parser::Ac3,
            rate: sample_rate,
        },
        AudioCodec::Dts => Timing::Parsed {
            parser: Parser::Dts,
            rate: sample_rate,
        },
        AudioCodec::Mp3 => Timing::Parsed {
            parser: Parser::Mpeg,
            rate: sample_rate,
        },
        AudioCodec::Alac | AudioCodec::Other(_) => Timing::Plain,
    };
    let intra = !matches!(codec, AudioCodec::Other(_));
    (
        AudioTrack {
            codec,
            codec_private: private,
            sample_rate,
            channels,
            speakers: speakers
                .filter(|layout: &SpeakerLayout| u32::try_from(layout.channels()) == Ok(channels)),
            bits_per_sample,
            priming,
        },
        timing,
        intra,
    )
}

/// The `AudioSpecificConfig` `FFmpeg` builds for an `A_AAC/...` codec ID without private data.
fn synthesized_aac_config(codec_id: &str, fields: &AudioFields) -> Bytes {
    const RATES: [u32; 12] = [
        92_017, 75_132, 55_426, 46_009, 37_566, 27_713, 23_004, 18_783, 13_856, 11_502, 9_391, 0,
    ];
    let profile = ["/MAIN", "/LC", "/SSR"]
        .iter()
        .zip(1_u8..)
        .find(|(name, _)| codec_id.contains(*name))
        .map_or(2, |(_, profile)| profile);
    let index = |rate: f64| -> u8 {
        let rate = rate_of(rate);
        RATES
            .iter()
            .zip(0_u8..)
            .find(|(bound, _)| rate >= **bound)
            .map_or(11, |(_, index)| index)
    };
    let sri = index(fields.sampling);
    let channels = u8::try_from(fields.channels).unwrap_or(0);
    let mut config = vec![
        (profile << 3) | ((sri & 0x0e) >> 1),
        ((sri & 1) << 7) | (channels << 3),
    ];
    if codec_id.contains("SBR") {
        let output = if fields.output > 0.0 {
            fields.output
        } else {
            fields.sampling
        };
        config.extend_from_slice(&[0x56, 0xe5, 0x80 | (index(output) << 3)]);
    }
    Bytes::from(config)
}

/// Whether an `AudioSpecificConfig` signals SBR, whose decoder doubles the frame length.
fn aac_doubles(config: &[u8]) -> bool {
    let Some(&first) = config.first() else {
        return false;
    };
    let object_type = first >> 3;
    if object_type == 5 || object_type == 29 {
        return true;
    }
    // Explicit backward-compatible signalling: sync extension 0x2b7 then SBR object type 5.
    config.len() >= 5 && config[2] == 0x56 && config[3] == 0xe5 && config[4] & 0x80 != 0
}

/// The STREAMINFO facts (rate, channels, bits) and channel-mask comment of FLAC private data.
fn flac_header(private: &[u8]) -> Option<((u32, u32, u32), Option<u64>)> {
    let mut at = 4;
    if private.get(..4)? != b"fLaC" {
        return None;
    }
    let mut info = None;
    let mut mask = None;
    while let Some(header) = private.get(at..at + 4) {
        let kind = header[0] & 0x7f;
        let len = usize::try_from(u32::from_be_bytes([0, header[1], header[2], header[3]])).ok()?;
        let body = private.get(at + 4..at + 4 + len)?;
        match kind {
            0 => {
                let found = flac::stream_info(body).ok()?;
                info = Some((found.sample_rate, found.channels, found.bits));
            }
            4 => mask = flac::comment_channel_mask(body).or(mask),
            _ => {}
        }
        at += 4 + len;
        if header[0] & 0x80 != 0 {
            break;
        }
    }
    Some((info?, mask))
}

/// The packets of Xiph-laced private data (Vorbis headers).
pub(crate) fn xiph_headers(private: &[u8]) -> Option<Vec<&[u8]>> {
    let count = usize::from(*private.first()?) + 1;
    let mut at = 1;
    let mut sizes = Vec::with_capacity(count);
    for _ in 1..count {
        let mut size = 0_usize;
        loop {
            let byte = *private.get(at)?;
            at += 1;
            size += usize::from(byte);
            if byte != 255 {
                break;
            }
        }
        sizes.push(size);
    }
    let mut packets = Vec::with_capacity(count);
    for size in sizes {
        packets.push(private.get(at..at.checked_add(size)?)?);
        at += size;
    }
    packets.push(private.get(at..)?);
    Some(packets)
}

fn vorbis_modes(private: &[u8]) -> Option<VorbisModes> {
    let headers = xiph_headers(private)?;
    let (identification, setup) = (headers.first()?, headers.get(2)?);
    VorbisModes::parse(identification, setup)
}

/// A signed element payload, for `DiscardPadding` and `ReferenceBlock`.
pub(crate) fn signed(payload: &[u8]) -> i64 {
    sint(payload).unwrap_or(0)
}
