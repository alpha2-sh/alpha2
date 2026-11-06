//! Bit-level reads of codec configuration: AAC `AudioSpecificConfig`, HEVC VPS layers, and the
//! reorder depths H.264, HEVC and MPEG-4 Part 2 headers declare.

/// A most-significant-bit-first reader.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    pub fn flag(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(bit == 1)
    }

    pub fn read(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value: u32, _| {
            Some((value << 1) | u32::from(self.flag()?))
        })
    }

    pub fn skip(&mut self, count: usize) -> Option<()> {
        let end = self.at.checked_add(count)?;
        (end <= self.data.len() * 8).then(|| self.at = end)
    }

    /// An unsigned Exp-Golomb value.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1_u32 << zeros) - 1 + self.read(zeros)?)
    }
}

/// The NAL units of an `hvcC` body's parameter set arrays.
fn hvcc_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let Some(&arrays) = data.get(22) else {
        return units;
    };
    let mut at = 23;
    for _ in 0..arrays {
        let Some(count) = data.get(at + 1..at + 3) else {
            break;
        };
        let count = u16::from_be_bytes([count[0], count[1]]);
        at += 3;
        for _ in 0..count {
            let Some(length) = data.get(at..at + 2) else {
                return units;
            };
            let end = at + 2 + usize::from(u16::from_be_bytes([length[0], length[1]]));
            let Some(unit) = data.get(at + 2..end) else {
                return units;
            };
            units.push(unit);
            at = end;
        }
    }
    units
}

fn unescape(escaped: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(escaped.len());
    let mut zeros = 0;
    for &byte in escaped {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        payload.push(byte);
    }
    payload
}

/// Whether the first VPS of an `hvcC` body declares an auxiliary picture layer
/// (`scalability_mask_flag[3]` in its extension, ITU-T H.265 F.7.3.2.1.1); `None` when there is
/// no VPS or its extension cannot be reached.
pub(crate) fn hevc_alpha_layer(hvcc: &[u8]) -> Option<bool> {
    let vps = hvcc_units(hvcc)
        .into_iter()
        .find(|nal| matches!(nal, [first, ..] if (first >> 1) & 0x3f == 32))?;
    let payload = unescape(vps.get(2..)?);
    let mut bits = Bits::new(&payload);
    bits.skip(4)?;
    let base_internal = bits.flag()?;
    bits.skip(1)?;
    if bits.read(6)? == 0 {
        return Some(false);
    }
    let sub_layers = bits.read(3)?;
    bits.skip(1 + 16)?;
    skip_profile_tier_level(&mut bits, true, sub_layers)?;
    let ordering_for_all = bits.flag()?;
    for _ in if ordering_for_all { 0 } else { sub_layers }..=sub_layers {
        bits.ue()?;
        bits.ue()?;
        bits.ue()?;
    }
    let max_layer_id = bits.read(6)?;
    let layer_sets = bits.ue()?;
    bits.skip(
        usize::try_from(layer_sets)
            .ok()?
            .checked_mul(max_layer_id as usize + 1)?,
    )?;
    if bits.flag()? {
        // Timing and HRD parameters come before the extension; they are not read.
        return None;
    }
    if !bits.flag()? {
        return Some(false);
    }
    while !bits.at.is_multiple_of(8) {
        bits.skip(1)?;
    }
    if base_internal {
        skip_profile_tier_level(&mut bits, false, sub_layers)?;
    }
    bits.skip(1)?;
    let mask = bits.read(16)?;
    Some(mask & (1 << (15 - 3)) != 0)
}

/// Skips `profile_tier_level(profile_present, max_sub_layers_minus1)` (ITU-T H.265 7.3.3).
fn skip_profile_tier_level(
    bits: &mut Bits<'_>,
    profile_present: bool,
    sub_layers: u32,
) -> Option<()> {
    bits.skip(if profile_present { 88 } else { 0 } + 8)?;
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((bits.flag()?, bits.flag()?));
    }
    if sub_layers > 0 {
        bits.skip(2 * (8 - sub_layers as usize))?;
    }
    for (profile, level) in present {
        bits.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
    }
    Some(())
}

impl Bits<'_> {
    /// A signed Exp-Golomb value.
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        })
    }
}

/// The reorder depth `FFmpeg`'s H.264 decoder starts from (`has_b_frames`): the first SPS's
/// `max_num_reorder_frames` when its VUI has bitstream restrictions, else 0.
pub(crate) fn avc_reorder_depth(avcc: &[u8]) -> Option<usize> {
    avc_sequence(avcc).map(|sequence| sequence.declared.unwrap_or(0))
}

/// The first SPS's `max_num_reorder_frames`, declared or inferred as ITU-T H.264 E.2.1 infers
/// it: 0 for the intra profiles with `constraint_set3_flag`, else `MaxDpbFrames` (A.3.1).
fn avc_declared_reorder_depth(avcc: &[u8]) -> Option<usize> {
    let sequence = avc_sequence(avcc)?;
    if let Some(declared) = sequence.declared {
        return Some(declared);
    }
    if matches!(sequence.profile, 44 | 86 | 100 | 110 | 122 | 244) && sequence.constraint_set3 {
        return Some(0);
    }
    // Level 1b is `level_idc` 9, or 11 with `constraint_set3_flag` in the Baseline, Main and
    // Extended profiles.
    let level_1b = sequence.level == 9
        || sequence.level == 11
            && sequence.constraint_set3
            && matches!(sequence.profile, 66 | 77 | 88);
    // MaxDpbMbs of Table A-1.
    let dpb_macroblocks: u64 = match sequence.level {
        _ if level_1b => 396,
        10 => 396,
        11 => 900,
        12 | 13 | 20 => 2_376,
        21 => 4_752,
        22 | 30 => 8_100,
        31 => 18_000,
        32 => 20_480,
        40 | 41 => 32_768,
        42 => 34_816,
        50 => 110_400,
        51 | 52 => 184_320,
        60..=62 => 696_320,
        _ => return None,
    };
    let frame = sequence.frame_macroblocks.max(1);
    Some(usize::try_from((dpb_macroblocks / frame).min(16)).unwrap_or(16))
}

/// What the first SPS of an `avcC` record says about reordering.
struct AvcSequence {
    profile: u32,
    constraint_set3: bool,
    level: u32,
    /// `PicWidthInMbs * FrameHeightInMbs`.
    frame_macroblocks: u64,
    /// `max_num_reorder_frames`, when the VUI has bitstream restrictions.
    declared: Option<usize>,
}

fn avc_sequence(avcc: &[u8]) -> Option<AvcSequence> {
    let parameter_sets = avcc.get(5).map_or(0, |byte| byte & 0x1f);
    if avcc.len() < 8 || avcc[0] != 1 || parameter_sets == 0 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([avcc[6], avcc[7]]));
    let sps = unescape(avcc.get(9..8 + length)?);
    let mut bits = Bits::new(&sps);
    let profile = bits.read(8)?;
    let constraint_set3 = bits.read(8)? & 0x10 != 0;
    let level = bits.read(8)?;
    bits.ue()?;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = bits.ue()?;
        if chroma == 3 {
            bits.skip(1)?;
        }
        bits.ue()?;
        bits.ue()?;
        bits.skip(1)?;
        if bits.flag()? {
            for list in 0..if chroma == 3 { 12 } else { 8 } {
                if bits.flag()? {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue()?;
    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.skip(1)?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue()?.min(255) {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue()?;
    bits.skip(1)?;
    let width = u64::from(bits.ue()?) + 1;
    let height = u64::from(bits.ue()?) + 1;
    let frame_mbs_only = bits.flag()?;
    if !frame_mbs_only {
        bits.skip(1)?;
    }
    bits.skip(1)?;
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    let declared = if bits.flag()? {
        let (restricted, depth) = vui_reorder_depth(&mut bits)?;
        restricted.then_some(depth)
    } else {
        None
    };
    Some(AvcSequence {
        profile,
        constraint_set3,
        level,
        frame_macroblocks: width * height * if frame_mbs_only { 1 } else { 2 },
        declared,
    })
}

/// Skips a `scaling_list()` of `size` entries (ITU-T H.264 7.3.2.1.1.1).
fn skip_scaling_list(bits: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8_i64, 8_i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + bits.se()?).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Whether `vui_parameters()` (ITU-T H.264 E.1.1) has bitstream restrictions, and its
/// `max_num_reorder_frames` (0 without).
fn vui_reorder_depth(bits: &mut Bits<'_>) -> Option<(bool, usize)> {
    if bits.flag()? && bits.read(8)? == 255 {
        bits.skip(32)?;
    }
    if bits.flag()? {
        bits.skip(1)?;
    }
    if bits.flag()? {
        bits.skip(4)?;
        if bits.flag()? {
            bits.skip(24)?;
        }
    }
    if bits.flag()? {
        bits.ue()?;
        bits.ue()?;
    }
    if bits.flag()? {
        bits.skip(65)?;
    }
    let nal_hrd = bits.flag()?;
    if nal_hrd {
        skip_hrd(bits)?;
    }
    let vcl_hrd = bits.flag()?;
    if vcl_hrd {
        skip_hrd(bits)?;
    }
    if nal_hrd || vcl_hrd {
        bits.skip(1)?;
    }
    bits.skip(1)?;
    if !bits.flag()? {
        return Some((false, 0));
    }
    bits.skip(1)?;
    for _ in 0..4 {
        bits.ue()?;
    }
    let reorder = bits.ue()?;
    bits.ue()?;
    Some((true, usize::try_from(reorder.min(16)).unwrap_or(16)))
}

/// Skips `hrd_parameters()` (ITU-T H.264 E.1.2).
fn skip_hrd(bits: &mut Bits<'_>) -> Option<()> {
    let count = bits.ue()?.min(31) + 1;
    bits.skip(8)?;
    for _ in 0..count {
        bits.ue()?;
        bits.ue()?;
        bits.skip(1)?;
    }
    bits.skip(20)
}

/// The reorder depth `FFmpeg`'s HEVC decoder exports from the first SPS of an `hvcC` body:
/// `sps_max_num_reorder_pics` of the highest sub-layer.
pub(crate) fn hevc_reorder_depth(hvcc: &[u8]) -> Option<usize> {
    let sps = hvcc_units(hvcc)
        .into_iter()
        .find(|nal| matches!(nal, [first, ..] if (first >> 1) & 0x3f == 33))?;
    let payload = unescape(sps.get(2..)?);
    let mut bits = Bits::new(&payload);
    bits.skip(4)?;
    let sub_layers = bits.read(3)?;
    bits.skip(1)?;
    skip_profile_tier_level(&mut bits, true, sub_layers)?;
    bits.ue()?;
    if bits.ue()? == 3 {
        bits.skip(1)?;
    }
    bits.ue()?;
    bits.ue()?;
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    bits.ue()?;
    bits.ue()?;
    bits.ue()?;
    let ordering_for_all = bits.flag()?;
    let mut reorder = 0;
    for _ in if ordering_for_all { 0 } else { sub_layers }..=sub_layers {
        bits.ue()?;
        reorder = bits.ue()?;
        bits.ue()?;
    }
    Some(usize::try_from(reorder.min(16)).unwrap_or(16))
}

/// Whether an MPEG-4 Part 2 video object layer header lets B-VOPs follow (`FFmpeg`'s
/// `has_b_frames = !low_delay` after reading it).
pub(crate) fn mpeg4_has_b_frames(config: &[u8]) -> bool {
    mpeg4_low_delay(config) == Some(false)
}

/// The `low_delay` of an MPEG-4 Part 2 video object layer header (ISO/IEC 14496-2 6.3.3), when
/// the header is found.
fn mpeg4_low_delay(config: &[u8]) -> Option<bool> {
    let start = config
        .windows(4)
        .position(|code| code[..3] == [0, 0, 1] && (0x20..=0x2f).contains(&code[3]))?;
    let mut bits = Bits::new(&config[start + 4..]);
    let mut low_delay = || -> Option<bool> {
        bits.skip(1)?;
        let object_type = bits.read(8)?;
        if bits.flag()? {
            bits.skip(7)?;
        }
        if bits.read(4)? == 15 {
            bits.skip(16)?;
        }
        if bits.flag()? {
            bits.skip(2)?;
            return bits.flag();
        }
        // Simple and Advanced Simple objects without control parameters have no B-VOPs.
        Some(matches!(object_type, 1 | 17))
    };
    low_delay()
}

/// The reorder depth a track of `codec` declares in `private` ([`crate::VideoTrack::reorder_depth`]).
pub(crate) fn declared_reorder_depth(codec: crate::VideoCodec, private: &[u8]) -> Option<usize> {
    use crate::VideoCodec;
    match codec {
        VideoCodec::H264 => avc_declared_reorder_depth(private),
        VideoCodec::Hevc => hevc_reorder_depth(private),
        VideoCodec::Mpeg4Part2 => mpeg4_low_delay(private).map(|low_delay| usize::from(!low_delay)),
        VideoCodec::Vp8 | VideoCodec::Vp9 | VideoCodec::Av1 | VideoCodec::ProRes(_) => Some(0),
        VideoCodec::Other(_) => None,
    }
}
