//! EBML element headers, typed payload reads, and element writing.

use bytes::{BufMut, BytesMut};

/// Element IDs, with their length markers, as Matroska and EBML define them.
pub(crate) mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;
    pub const CRC32: u32 = 0xBF;

    pub const SEGMENT: u32 = 0x1853_8067;
    pub const SEEK_HEAD: u32 = 0x114D_9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    pub const DURATION: u32 = 0x4489;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const WRITING_APP: u32 = 0x5741;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const CUES: u32 = 0x1C53_BB6B;
    pub const TAGS: u32 = 0x1254_C367;
    pub const CHAPTERS: u32 = 0x1043_A770;
    pub const ATTACHMENTS: u32 = 0x1941_A469;
    pub const CLUSTER: u32 = 0x1F43_B675;

    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_ENABLED: u32 = 0xB9;
    pub const FLAG_DEFAULT: u32 = 0x88;
    pub const FLAG_LACING: u32 = 0x9C;
    pub const DEFAULT_DURATION: u32 = 0x23_E383;
    pub const TRACK_TIMESTAMP_SCALE: u32 = 0x23_314F;
    pub const NAME: u32 = 0x536E;
    pub const LANGUAGE: u32 = 0x22_B59C;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const CODEC_DELAY: u32 = 0x56AA;
    pub const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub const CONTENT_ENCODINGS: u32 = 0x6D80;
    pub const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
    pub const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;

    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const PIXEL_CROP_BOTTOM: u32 = 0x54AA;
    pub const PIXEL_CROP_TOP: u32 = 0x54BB;
    pub const PIXEL_CROP_LEFT: u32 = 0x54CC;
    pub const PIXEL_CROP_RIGHT: u32 = 0x54DD;
    pub const DISPLAY_WIDTH: u32 = 0x54B0;
    pub const DISPLAY_HEIGHT: u32 = 0x54BA;
    pub const DISPLAY_UNIT: u32 = 0x54B2;
    pub const STEREO_MODE: u32 = 0x53B8;
    pub const ALPHA_MODE: u32 = 0x53C0;
    pub const FRAME_RATE: u32 = 0x23_83E3;
    pub const COLOUR: u32 = 0x55B0;
    pub const MATRIX_COEFFICIENTS: u32 = 0x55B1;
    pub const CHROMA_SITING_HORZ: u32 = 0x55B7;
    pub const CHROMA_SITING_VERT: u32 = 0x55B8;
    pub const RANGE: u32 = 0x55B9;
    pub const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
    pub const PRIMARIES: u32 = 0x55BB;
    pub const PROJECTION: u32 = 0x7670;
    pub const PROJECTION_TYPE: u32 = 0x7671;
    pub const PROJECTION_PRIVATE: u32 = 0x7672;
    pub const PROJECTION_POSE_YAW: u32 = 0x7673;
    pub const PROJECTION_POSE_PITCH: u32 = 0x7674;
    pub const PROJECTION_POSE_ROLL: u32 = 0x7675;

    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const OUTPUT_SAMPLING_FREQUENCY: u32 = 0x78B5;
    pub const CHANNELS: u32 = 0x9F;
    pub const BIT_DEPTH: u32 = 0x6264;

    pub const CLUSTER_TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const BLOCK_DURATION: u32 = 0x9B;
    pub const REFERENCE_BLOCK: u32 = 0xFB;
    pub const DISCARD_PADDING: u32 = 0x75A2;

    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const CUE_RELATIVE_POSITION: u32 = 0xF0;
}

/// The level-1 elements a Segment holds; in an unknown-sized Cluster one of these ends it.
pub(crate) fn is_level1(element: u32) -> bool {
    matches!(
        element,
        id::SEEK_HEAD
            | id::INFO
            | id::TRACKS
            | id::CUES
            | id::TAGS
            | id::CHAPTERS
            | id::ATTACHMENTS
            | id::CLUSTER
            | id::SEGMENT
            | id::EBML
    )
}

/// An element header: its ID, its payload size (`None` when unknown) and the header's length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Header {
    pub id: u32,
    pub size: Option<u64>,
    pub len: usize,
}

/// The variable-length number at the start of `data` and its length, its marker bit kept for IDs
/// and cleared for sizes.
fn vint(data: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    let bytes = data.get(..len)?;
    let marker = if keep_marker {
        0xff
    } else {
        0xff_u8.checked_shr(u32::try_from(len).ok()?).unwrap_or(0)
    };
    let value = bytes[1..]
        .iter()
        .fold(u64::from(first & marker), |value, byte| {
            (value << 8) | u64::from(*byte)
        });
    Some((value, len))
}

/// A number with its marker cleared, without an unknown value (block track numbers, lace sizes).
pub(crate) fn read_number(data: &[u8]) -> Option<(u64, usize)> {
    vint(data, false)
}

/// A size field: `None` inside for the all-ones value that marks an unknown size.
pub(crate) fn read_size(data: &[u8]) -> Option<(Option<u64>, usize)> {
    let (value, len) = vint(data, false)?;
    let unknown = value == (1_u64 << (7 * len)) - 1;
    Some(((!unknown).then_some(value), len))
}

/// An element header at the start of `data`; IDs are at most four bytes long.
pub(crate) fn header(data: &[u8]) -> Option<Header> {
    let (element, id_len) = vint(data, true)?;
    if id_len > 4 {
        return None;
    }
    let (size, size_len) = read_size(data.get(id_len..)?)?;
    Some(Header {
        id: u32::try_from(element).ok()?,
        size,
        len: id_len + size_len,
    })
}

/// A signed lace size difference: the number less half its range (EBML lacing).
pub(crate) fn read_signed(data: &[u8]) -> Option<(i64, usize)> {
    let (value, len) = vint(data, false)?;
    let bias = (1_i64 << (7 * len - 1)) - 1;
    Some((i64::try_from(value).ok()? - bias, len))
}

/// The children of a master element's payload held in memory: each child's ID and payload. A
/// child that claims more than its parent holds is cut to what remains, and a header that cannot
/// be read ends the walk.
pub(crate) struct Children<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Children<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = (u32, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.data.get(self.at..)?;
        let found = header(rest)?;
        let body = &rest[found.len..];
        let size = found
            .size
            .map_or(body.len(), |size| {
                usize::try_from(size).unwrap_or(usize::MAX)
            })
            .min(body.len());
        self.at += found.len + size;
        Some((found.id, &body[..size]))
    }
}

/// An unsigned integer payload of at most eight bytes.
pub(crate) fn uint(payload: &[u8]) -> Option<u64> {
    (payload.len() <= 8).then(|| {
        payload
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
    })
}

/// A signed integer payload of at most eight bytes.
pub(crate) fn sint(payload: &[u8]) -> Option<i64> {
    let value = uint(payload)?;
    let bits = payload.len() * 8;
    #[allow(clippy::cast_possible_wrap)]
    Some(if bits == 0 || bits == 64 {
        value as i64
    } else {
        ((value << (64 - bits)) as i64) >> (64 - bits)
    })
}

/// A float payload of zero, four or eight bytes.
pub(crate) fn float(payload: &[u8]) -> Option<f64> {
    match payload.len() {
        0 => Some(0.0),
        4 => Some(f64::from(f32::from_be_bytes(payload.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(payload.try_into().ok()?)),
        _ => None,
    }
}

/// A string payload up to its first NUL.
pub(crate) fn string(payload: &[u8]) -> String {
    let end = payload
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}

/// Bytes an unsigned number needs, at least one.
pub(crate) fn uint_len(value: u64) -> usize {
    (((64 - value.leading_zeros()) as usize).div_ceil(8)).max(1)
}

/// Bytes a size field needs for `size`, the all-ones value of each length being reserved.
pub(crate) fn size_len(size: u64) -> usize {
    (1..=8)
        .find(|len| size < (1_u64 << (7 * len)) - 1)
        .unwrap_or(8)
}

pub(crate) fn put_id(out: &mut BytesMut, element: u32) {
    let len = uint_len(u64::from(element));
    out.put_uint(u64::from(element), len);
}

/// Writes `size` in a field of `len` bytes.
pub(crate) fn put_size(out: &mut BytesMut, size: u64, len: usize) {
    out.put_uint(size | (1_u64 << (7 * len)), len);
}

/// An element holding `payload`: binary data, a string, or the children of a master element.
pub(crate) fn put_master(out: &mut BytesMut, element: u32, payload: &[u8]) {
    put_id(out, element);
    put_size(out, payload.len() as u64, size_len(payload.len() as u64));
    out.put_slice(payload);
}

pub(crate) fn put_uint(out: &mut BytesMut, element: u32, value: u64) {
    put_id(out, element);
    let len = uint_len(value);
    put_size(out, len as u64, 1);
    out.put_uint(value, len);
}

pub(crate) fn put_sint(out: &mut BytesMut, element: u32, value: i64) {
    put_id(out, element);
    let len = (1..=8)
        .find(|len| {
            let bits = 8 * len - 1;
            bits >= 63 || (-(1_i64 << bits)..(1_i64 << bits)).contains(&value)
        })
        .unwrap_or(8);
    put_size(out, len as u64, 1);
    #[allow(clippy::cast_sign_loss)]
    out.put_uint(value as u64 & (u64::MAX >> (64 - 8 * len)), len);
}

pub(crate) fn put_float(out: &mut BytesMut, element: u32, value: f64) {
    put_id(out, element);
    put_size(out, 8, 1);
    out.put_f64(value);
}

/// A Void element of exactly `total` bytes, header included (`total` at least 2), sized with one
/// byte below 10 bytes and eight from there, as `FFmpeg` writes it.
pub(crate) fn put_void(out: &mut BytesMut, total: usize) {
    out.put_u8(0xEC);
    let len = if total < 10 { 1 } else { 8 };
    let payload = total - 1 - len;
    put_size(out, payload as u64, len);
    out.put_bytes(0, payload);
}
