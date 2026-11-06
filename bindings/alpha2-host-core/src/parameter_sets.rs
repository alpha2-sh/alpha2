//! What H.264 and HEVC sequence parameter sets declare (coded size, cropping, the VUI's aspect
//! ratio, color and chroma siting) and HEVC VPS layer declarations, read from a stream's sample
//! description, and the bit depth a VP9 frame header declares.

use alpha2_media::{ChromaLocation, ColorMetadata, SampleAspectRatio};

/// The codecs whose parameter sets this module reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterSetCodec {
    /// H.264 (`avcC` or Annex B).
    H264,
    /// HEVC (`hvcC` or Annex B).
    Hevc,
}

/// The coded picture size an SPS declares and the luma samples it crops off each edge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CropWindow {
    /// Coded width in luma samples.
    pub coded_width: u32,
    /// Coded height in luma samples.
    pub coded_height: u32,
    /// Columns off the left.
    pub left: u32,
    /// Columns off the right.
    pub right: u32,
    /// Rows off the top.
    pub top: u32,
    /// Rows off the bottom.
    pub bottom: u32,
}

impl CropWindow {
    /// The visible area: the coded picture less the crop, `None` when the crop leaves nothing.
    #[must_use]
    pub fn visible(&self) -> Option<alpha2_media::Rect> {
        let width = self
            .coded_width
            .checked_sub(self.left)?
            .checked_sub(self.right)?;
        let height = self
            .coded_height
            .checked_sub(self.top)?
            .checked_sub(self.bottom)?;
        (width > 0 && height > 0).then_some(alpha2_media::Rect {
            x: self.left,
            y: self.top,
            width,
            height,
        })
    }
}

/// What an SPS's video usability information declares.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Vui {
    /// The pixel aspect ratio, when declared and not 0:0.
    pub sample_aspect_ratio: Option<SampleAspectRatio>,
    /// The color the video signal type declares.
    pub color: Option<ColorMetadata>,
    /// The chroma siting of a 4:2:0 stream, when declared.
    pub chroma_location: Option<ChromaLocation>,
}

/// What one SPS declares.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SequenceFacts {
    /// The coded size and crop.
    pub crop: CropWindow,
    /// `chroma_format_idc`: 0 monochrome, 1 4:2:0, 2 4:2:2, 3 4:4:4.
    pub chroma_format: u32,
    /// Luma bits per sample.
    pub bit_depth_luma: u32,
    /// Chroma bits per sample.
    pub bit_depth_chroma: u32,
    /// The VUI; empty when absent or unreadable.
    pub vui: Vui,
}

impl SequenceFacts {
    /// The chroma siting frames have: the VUI's, else the 4:2:0 default both codecs infer
    /// (type 0, left).
    #[must_use]
    pub fn chroma_location(&self) -> Option<ChromaLocation> {
        self.vui
            .chroma_location
            .or((self.chroma_format == 1).then_some(ChromaLocation::Left))
    }
}

/// What the first SPS in `extradata` (`avcC`, `hvcC` or Annex B) declares; `None` when there is
/// none or its fields up to the crop do not parse.
#[must_use]
pub fn sequence_facts(codec: ParameterSetCodec, extradata: &[u8]) -> Option<SequenceFacts> {
    let hevc = codec == ParameterSetCodec::Hevc;
    let nal = *sequence_parameter_sets(hevc, extradata).first()?;
    if hevc { hevc_sps(nal) } else { h264_sps(nal) }
}

/// `annex_b`'s NAL units with 4-byte big-endian lengths in place of start codes, as MP4 stores
/// H.264 and HEVC samples.
#[must_use]
pub fn length_prefixed_units(annex_b: &[u8]) -> Vec<u8> {
    let units = annex_b_units(annex_b);
    let mut out = Vec::with_capacity(annex_b.len() + units.len());
    for unit in units {
        out.extend_from_slice(&u32::try_from(unit.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

/// The `avcC` record (ISO/IEC 14496-15 5.3.3.1) of the SPS and PPS units in `annex_b`, with
/// 4-byte NAL lengths; `None` without a readable SPS and a PPS.
#[must_use]
pub fn avc_decoder_config(annex_b: &[u8]) -> Option<Vec<u8>> {
    let units = annex_b_units(annex_b);
    let of_type = |kind: u8| -> Vec<&[u8]> {
        units
            .iter()
            .copied()
            .filter(|unit| unit[0] & 0x1f == kind)
            .collect()
    };
    let (sps, pps) = (of_type(7), of_type(8));
    let first = *sps.first()?;
    if pps.is_empty() || first.len() < 4 {
        return None;
    }
    let facts = h264_sps(first)?;
    let mut record = vec![1, first[1], first[2], first[3], 0xff];
    record.push(0xe0 | u8::try_from(sps.len()).ok().filter(|count| *count < 32)?);
    for unit in &sps {
        record.extend_from_slice(&u16::try_from(unit.len()).ok()?.to_be_bytes());
        record.extend_from_slice(unit);
    }
    record.push(u8::try_from(pps.len()).ok()?);
    for unit in &pps {
        record.extend_from_slice(&u16::try_from(unit.len()).ok()?.to_be_bytes());
        record.extend_from_slice(unit);
    }
    // The High profiles' extension: chroma format and bit depths, no SPS extensions.
    if matches!(first[1], 100 | 110 | 122 | 144) {
        record.push(0xfc | u8::try_from(facts.chroma_format & 3).ok()?);
        record.push(0xf8 | u8::try_from(facts.bit_depth_luma.checked_sub(8)? & 7).ok()?);
        record.push(0xf8 | u8::try_from(facts.bit_depth_chroma.checked_sub(8)? & 7).ok()?);
        record.push(0);
    }
    Some(record)
}

/// The `hvcC` record (ISO/IEC 14496-15 8.3.3.1) of the VPS, SPS and PPS units in `annex_b`, with
/// 4-byte NAL lengths; `None` without a readable SPS.
#[must_use]
pub fn hevc_decoder_config(annex_b: &[u8]) -> Option<Vec<u8>> {
    let units = annex_b_units(annex_b);
    let kind = |unit: &[u8]| (unit[0] >> 1) & 0x3f;
    let sps = *units
        .iter()
        .find(|unit| unit.len() > 2 && kind(unit) == 33)?;
    let facts = hevc_sps(sps)?;
    // After the NAL header: VPS id, sub-layer count and nesting, then the general profile, tier
    // and level that the record repeats (12 bytes).
    let payload = unescape(sps.get(2..)?);
    let general = payload.get(1..13)?;
    let sub_layers = (payload[0] >> 1) & 7;
    let nested = payload[0] & 1;
    let mut record = vec![1];
    record.extend_from_slice(general);
    record.extend_from_slice(&[0xf0, 0x00, 0xfc]); // no segmentation or parallelism declared
    record.push(0xfc | u8::try_from(facts.chroma_format & 3).ok()?);
    record.push(0xf8 | u8::try_from(facts.bit_depth_luma.checked_sub(8)? & 7).ok()?);
    record.push(0xf8 | u8::try_from(facts.bit_depth_chroma.checked_sub(8)? & 7).ok()?);
    record.extend_from_slice(&[0, 0]); // average frame rate unknown
    record.push(((sub_layers + 1) << 3) | (nested << 2) | 3);
    let arrays = [32, 33, 34]
        .into_iter()
        .map(|wanted| {
            units
                .iter()
                .copied()
                .filter(|unit| unit.len() > 2 && kind(unit) == wanted)
                .collect::<Vec<_>>()
        })
        .filter(|array| !array.is_empty())
        .collect::<Vec<_>>();
    record.push(u8::try_from(arrays.len()).ok()?);
    for array in arrays {
        record.push(0x80 | kind(array[0]));
        record.extend_from_slice(&u16::try_from(array.len()).ok()?.to_be_bytes());
        for unit in array {
            record.extend_from_slice(&u16::try_from(unit.len()).ok()?.to_be_bytes());
            record.extend_from_slice(unit);
        }
    }
    Some(record)
}

/// The crop windows of every SPS in `extradata` (`avcC`, `hvcC` or Annex B); `None` when one does
/// not parse.
fn crop_windows(codec: ParameterSetCodec, extradata: &[u8]) -> Option<Vec<CropWindow>> {
    let hevc = codec == ParameterSetCodec::Hevc;
    sequence_parameter_sets(hevc, extradata)
        .into_iter()
        .map(|nal| if hevc { hevc_sps(nal) } else { h264_sps(nal) }.map(|facts| facts.crop))
        .collect()
}

/// The crop window of the first SPS in `extradata`; `None` when there is none or it does not
/// parse.
#[must_use]
pub fn crop_window(codec: ParameterSetCodec, extradata: &[u8]) -> Option<CropWindow> {
    crop_windows(codec, extradata)?.first().copied()
}

/// Luma columns and rows the first SPS that crops a leading edge takes off the left and top.
///
/// `(0, 0)` when every SPS crops only right/bottom; `None` when `extradata` (`avcC`, `hvcC` or
/// Annex B) holds no readable parameter set.
#[must_use]
pub fn leading_crop(codec: ParameterSetCodec, extradata: &[u8]) -> Option<(u32, u32)> {
    let crops = crop_windows(codec, extradata)?;
    let first = *crops.first()?;
    let leading = crops
        .into_iter()
        .find(|crop| crop.left > 0 || crop.top > 0)
        .unwrap_or(first);
    Some((leading.left, leading.top))
}

/// The layers besides the base one that an HEVC VPS declares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HevcLayers {
    /// Only the base layer.
    Single,
    /// An auxiliary picture layer carrying alpha: `scalability_mask_flag[3]` in the VPS extension
    /// (ITU-T H.265 F.7.4.3.1.1, Table F.1).
    AuxiliaryAlpha,
    /// More layers, none declared auxiliary, or a VPS extension this cannot read.
    Extra,
}

/// The layers the first HEVC VPS in `extradata` declares, or `None` when there is none.
#[must_use]
pub fn hevc_layers(extradata: &[u8]) -> Option<HevcLayers> {
    let units = match extradata {
        [1, ..] => hvcc_units(extradata),
        _ => annex_b_units(extradata),
    };
    let vps = units
        .into_iter()
        .find(|nal| matches!(nal, [first, ..] if (first >> 1) & 0x3f == 32))?;
    let payload = unescape(vps.get(2..)?);
    let mut bits = Bits::new(&payload);
    bits.skip(4)?; // vps_video_parameter_set_id
    let base_internal = bits.flag()?;
    bits.skip(1)?; // vps_base_layer_available_flag
    let max_layers = bits.read(6)?; // vps_max_layers_minus1
    if max_layers == 0 {
        return Some(HevcLayers::Single);
    }
    Some(vps_auxiliary_layer(&mut bits, base_internal).unwrap_or(HevcLayers::Extra))
}

/// Reads the rest of a multi-layer VPS up to its extension's scalability mask (ITU-T H.265 7.3.2.1,
/// F.7.3.2.1.1). `None` when unread fields (timing, HRD) come first.
fn vps_auxiliary_layer(bits: &mut Bits<'_>, base_internal: bool) -> Option<HevcLayers> {
    let sub_layers = bits.read(3)?; // vps_max_sub_layers_minus1
    bits.skip(1 + 16)?; // vps_temporal_id_nesting_flag, vps_reserved_0xffff_16bits
    skip_profile_tier_level(bits, true, sub_layers)?;
    let ordering_for_all = bits.flag()?; // vps_sub_layer_ordering_info_present_flag
    for _ in if ordering_for_all { 0 } else { sub_layers }..=sub_layers {
        bits.ue()?; // vps_max_dec_pic_buffering_minus1
        bits.ue()?; // vps_max_num_reorder_pics
        bits.ue()?; // vps_max_latency_increase_plus1
    }
    let max_layer_id = bits.read(6)?; // vps_max_layer_id
    let layer_sets = bits.ue()?; // vps_num_layer_sets_minus1
    bits.skip(
        usize::try_from(layer_sets)
            .ok()?
            .checked_mul(max_layer_id as usize + 1)?,
    )?;
    if bits.flag()? {
        // vps_timing_info_present_flag: HRD parameters follow, which this does not read.
        return None;
    }
    if !bits.flag()? {
        // vps_extension_flag unset: more layers with no extension to describe them.
        return Some(HevcLayers::Extra);
    }
    while !bits.at.is_multiple_of(8) {
        bits.skip(1)?; // vps_extension_alignment_bit_equal_to_one
    }
    if base_internal {
        skip_profile_tier_level(bits, false, sub_layers)?;
    }
    bits.skip(1)?; // splitting_flag
    let mask = bits.read(16)?; // scalability_mask_flag[0..16]
    Some(if mask & (1 << (15 - 3)) != 0 {
        HevcLayers::AuxiliaryAlpha
    } else {
        HevcLayers::Extra
    })
}

/// Skips `profile_tier_level(profile_present, max_sub_layers_minus1)` (ITU-T H.265 7.3.3).
fn skip_profile_tier_level(
    bits: &mut Bits<'_>,
    profile_present: bool,
    sub_layers: u32,
) -> Option<()> {
    // The general profile (88 bits) when present, and the general level.
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

/// The SPS NAL units (header included, still escaped) of a sample description.
fn sequence_parameter_sets(hevc: bool, extradata: &[u8]) -> Vec<&[u8]> {
    let is_sps = |nal: &[u8]| match (hevc, nal) {
        (false, [header, ..]) => header & 0x1f == 7,
        // Type 33 at the base layer only: a higher layer's SPS is laid out differently.
        (true, [first, second, ..]) => {
            (first >> 1) & 0x3f == 33 && (first & 1) == 0 && second >> 3 == 0
        }
        _ => false,
    };
    let units = match (hevc, extradata) {
        (false, [1, ..]) => avcc_units(extradata),
        (true, [1, ..]) => hvcc_units(extradata),
        _ => annex_b_units(extradata),
    };
    units.into_iter().filter(|nal| is_sps(nal)).collect()
}

/// The parameter sets of an `avcC` box body: its SPS list, then its PPS list.
fn avcc_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let Some(&count) = data.get(5) else {
        return units;
    };
    let mut at = 6;
    for _ in 0..(count & 0x1f) {
        let Some((nal, next)) = length_prefixed(data, at) else {
            return units;
        };
        units.push(nal);
        at = next;
    }
    let Some(&count) = data.get(at) else {
        return units;
    };
    at += 1;
    for _ in 0..count {
        let Some((nal, next)) = length_prefixed(data, at) else {
            break;
        };
        units.push(nal);
        at = next;
    }
    units
}

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
            let Some((nal, next)) = length_prefixed(data, at) else {
                return units;
            };
            units.push(nal);
            at = next;
        }
    }
    units
}

/// The unit after a 16-bit length at `at`, and where the next length begins.
fn length_prefixed(data: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let length = data.get(at..at + 2)?;
    let end = at + 2 + usize::from(u16::from_be_bytes([length[0], length[1]]));
    Some((data.get(at + 2..end)?, end))
}

fn annex_b_units(data: &[u8]) -> Vec<&[u8]> {
    let starts = (0..data.len().saturating_sub(2))
        .filter(|&at| data[at..at + 3] == [0, 0, 1])
        .map(|at| at + 3)
        .collect::<Vec<_>>();
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).map_or(data.len(), |next| next - 3);
            // A four-byte start code leaves its leading zero on the unit before.
            let mut unit = &data[start..end.max(start)];
            while let [rest @ .., 0] = unit {
                unit = rest;
            }
            unit
        })
        .filter(|unit| !unit.is_empty())
        .collect()
}

/// An H.264 SPS's coded size, crop offsets in luma samples and VUI (ITU-T H.264 7.3.2.1.1).
fn h264_sps(nal: &[u8]) -> Option<SequenceFacts> {
    let payload = unescape(nal.get(1..)?);
    let mut bits = Bits::new(&payload);
    let profile = bits.read(8)?;
    bits.skip(16)?; // constraint flags and level_idc
    bits.ue()?; // seq_parameter_set_id
    let (mut chroma_format, mut bit_depth_luma, mut bit_depth_chroma) = (1, 8, 8);
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format = bits.ue()?;
        if chroma_format == 3 {
            bits.skip(1)?; // separate_colour_plane_flag
        }
        bit_depth_luma = bits.ue()?.saturating_add(8);
        bit_depth_chroma = bits.ue()?.saturating_add(8);
        bits.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.flag()? {
            for list in 0..if chroma_format == 3 { 12 } else { 8 } {
                if bits.flag()? {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.skip(1)?; // delta_pic_order_always_zero_flag
            bits.se()?; // offset_for_non_ref_pic
            bits.se()?; // offset_for_top_to_bottom_field
            for _ in 0..bits.ue()?.min(255) {
                bits.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_in_mbs = bits.ue()?.saturating_add(1);
    let height_in_map_units = bits.ue()?.saturating_add(1);
    let frame_mbs_only = bits.flag()?;
    let coded_width = width_in_mbs.saturating_mul(16);
    let coded_height = height_in_map_units
        .saturating_mul(16)
        .saturating_mul(if frame_mbs_only { 1 } else { 2 });
    if !frame_mbs_only {
        bits.skip(1)?; // mb_adaptive_frame_field_flag
    }
    bits.skip(1)?; // direct_8x8_inference_flag
    let mut crop = CropWindow {
        coded_width,
        coded_height,
        ..CropWindow::default()
    };
    if bits.flag()? {
        let (left, right, top, bottom) = (bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?);
        // Crop units (7-19, 7-20): chroma samples, and field pairs in a stream that may code
        // fields.
        let (unit_x, unit_y) = crop_units(chroma_format);
        let unit_y = unit_y * if frame_mbs_only { 1 } else { 2 };
        crop.left = left.saturating_mul(unit_x);
        crop.right = right.saturating_mul(unit_x);
        crop.top = top.saturating_mul(unit_y);
        crop.bottom = bottom.saturating_mul(unit_y);
    }
    let vui = bits
        .flag()
        .filter(|present| *present)
        .and_then(|_| vui(&mut bits, chroma_format))
        .unwrap_or_default();
    Some(SequenceFacts {
        crop,
        chroma_format,
        bit_depth_luma,
        bit_depth_chroma,
        vui,
    })
}

/// `SubWidthC` and `SubHeightC` (H.264 Table 6-1, H.265 Table 6-1).
const fn crop_units(chroma_format: u32) -> (u32, u32) {
    match chroma_format {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    }
}

/// The leading fields of `vui_parameters()`, which H.264 (E.1.1) and HEVC (E.2.1) share: aspect
/// ratio, overscan, video signal type and chroma location.
fn vui(bits: &mut Bits<'_>, chroma_format: u32) -> Option<Vui> {
    /// Table E-1: `aspect_ratio_idc` 1 to 16.
    const RATIOS: [(u32, u32); 16] = [
        (1, 1),
        (12, 11),
        (10, 11),
        (16, 11),
        (40, 33),
        (24, 11),
        (20, 11),
        (32, 11),
        (80, 33),
        (18, 11),
        (15, 11),
        (64, 33),
        (160, 99),
        (4, 3),
        (3, 2),
        (2, 1),
    ];
    let mut vui = Vui::default();
    if bits.flag()? {
        let ratio = match bits.read(8)? {
            255 => Some((bits.read(16)?, bits.read(16)?)),
            idc @ 1..=16 => RATIOS.get(idc as usize - 1).copied(),
            _ => None,
        };
        vui.sample_aspect_ratio = ratio
            .filter(|(num, den)| *num > 0 && *den > 0)
            .map(|(num, den)| SampleAspectRatio { num, den });
    }
    if bits.flag()? {
        bits.skip(1)?; // overscan_appropriate_flag
    }
    if bits.flag()? {
        bits.skip(3)?; // video_format
        let full = bits.flag()?;
        let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
        if bits.flag()? {
            primaries = bits.read(8)?;
            transfer = bits.read(8)?;
            matrix = bits.read(8)?;
        }
        let code = |value: u32| u8::try_from(value).unwrap_or(2);
        vui.color =
            ColorMetadata::from_h273(code(primaries), code(transfer), code(matrix), Some(full));
    }
    if bits.flag()? {
        let top = bits.ue()?;
        bits.ue()?; // chroma_sample_loc_type_bottom_field
        if chroma_format == 1 {
            vui.chroma_location = Some(match top {
                0 => ChromaLocation::Left,
                1 => ChromaLocation::Center,
                2 => ChromaLocation::TopLeft,
                _ => ChromaLocation::Unspecified,
            });
        }
    }
    Some(vui)
}

/// Skips one `scaling_list()` of `size` coefficients.
fn skip_scaling_list(bits: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8_i64, 8_i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + bits.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// An HEVC SPS's coded size, conformance window offsets in luma samples and VUI (ITU-T H.265
/// 7.3.2.2.1).
fn hevc_sps(nal: &[u8]) -> Option<SequenceFacts> {
    let payload = unescape(nal.get(2..)?);
    let mut bits = Bits::new(&payload);
    bits.skip(4)?; // sps_video_parameter_set_id
    let sub_layers = bits.read(3)?; // sps_max_sub_layers_minus1
    bits.skip(1)?; // sps_temporal_id_nesting_flag
    // profile_tier_level(1, sps_max_sub_layers_minus1): general profile and level, then sub-layers.
    bits.skip(96)?;
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
    bits.ue()?; // sps_seq_parameter_set_id
    let chroma_format = bits.ue()?;
    if chroma_format == 3 {
        bits.skip(1)?; // separate_colour_plane_flag
    }
    let coded_width = bits.ue()?;
    let coded_height = bits.ue()?;
    let mut crop = CropWindow {
        coded_width,
        coded_height,
        ..CropWindow::default()
    };
    if bits.flag()? {
        let (left, right, top, bottom) = (bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?);
        let (unit_x, unit_y) = crop_units(chroma_format);
        crop.left = left.saturating_mul(unit_x);
        crop.right = right.saturating_mul(unit_x);
        crop.top = top.saturating_mul(unit_y);
        crop.bottom = bottom.saturating_mul(unit_y);
    }
    let bit_depth_luma = bits.ue()?.saturating_add(8);
    let bit_depth_chroma = bits.ue()?.saturating_add(8);
    let vui = hevc_vui(&mut bits, sub_layers, chroma_format).unwrap_or_default();
    Some(SequenceFacts {
        crop,
        chroma_format,
        bit_depth_luma,
        bit_depth_chroma,
        vui,
    })
}

/// The HEVC SPS fields from `log2_max_pic_order_cnt_lsb_minus4` to the VUI, then the VUI.
fn hevc_vui(bits: &mut Bits<'_>, sub_layers: u32, chroma_format: u32) -> Option<Vui> {
    let poc_lsb_bits = bits.ue()?.checked_add(4)?;
    let first = if bits.flag()? { 0 } else { sub_layers };
    for _ in first..=sub_layers {
        bits.ue()?; // sps_max_dec_pic_buffering_minus1
        bits.ue()?; // sps_max_num_reorder_pics
        bits.ue()?; // sps_max_latency_increase_plus1
    }
    for _ in 0..6 {
        // Coding and transform block sizes, transform hierarchy depths.
        bits.ue()?;
    }
    if bits.flag()? && bits.flag()? {
        skip_hevc_scaling_lists(bits)?;
    }
    bits.skip(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if bits.flag()? {
        bits.skip(8)?; // PCM sample bit depths
        bits.ue()?; // log2_min_pcm_luma_coding_block_size_minus3
        bits.ue()?; // log2_diff_max_min_pcm_luma_coding_block_size
        bits.skip(1)?; // pcm_loop_filter_disabled_flag
    }
    skip_short_term_ref_pic_sets(bits)?;
    if bits.flag()? {
        for _ in 0..bits.ue()?.min(32) {
            bits.skip(poc_lsb_bits as usize)?; // lt_ref_pic_poc_lsb_sps
            bits.skip(1)?; // used_by_curr_pic_lt_sps_flag
        }
    }
    bits.skip(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    if !bits.flag()? {
        return Some(Vui::default());
    }
    vui(bits, chroma_format)
}

/// Skips `scaling_list_data()` (H.265 7.3.4).
fn skip_hevc_scaling_lists(bits: &mut Bits<'_>) -> Option<()> {
    for size in 0..4_u32 {
        let step = if size == 3 { 3 } else { 1 };
        for _ in (0..6).step_by(step) {
            if bits.flag()? {
                let coefficients = 64.min(1 << (4 + (size << 1)));
                if size > 1 {
                    bits.se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coefficients {
                    bits.se()?; // scaling_list_delta_coef
                }
            } else {
                bits.ue()?; // scaling_list_pred_matrix_id_delta
            }
        }
    }
    Some(())
}

/// Skips the SPS's `st_ref_pic_set()` list (H.265 7.3.7), which predicts sets from earlier ones.
fn skip_short_term_ref_pic_sets(bits: &mut Bits<'_>) -> Option<()> {
    let count = bits.ue()?;
    if count > 64 {
        return None;
    }
    let mut delta_pocs: Vec<u32> = Vec::with_capacity(count as usize);
    for index in 0..count {
        let predicted = index != 0 && bits.flag()?;
        let pictures = if predicted {
            bits.skip(1)?; // delta_rps_sign
            bits.ue()?; // abs_delta_rps_minus1
            let reference = *delta_pocs.last()?;
            let mut pictures = 0;
            for _ in 0..=reference {
                let used = bits.flag()?;
                if used || bits.flag()? {
                    pictures += 1;
                }
            }
            pictures
        } else {
            let negative = bits.ue()?;
            let positive = bits.ue()?;
            let pictures = negative
                .checked_add(positive)
                .filter(|total| *total <= 32)?;
            for _ in 0..pictures {
                bits.ue()?; // delta_poc_s0/s1_minus1
                bits.skip(1)?; // used_by_curr_pic_s0/s1_flag
            }
            pictures
        };
        delta_pocs.push(pictures);
    }
    Some(())
}

/// The bit depth a VP9 frame's uncompressed header declares (VP9 bitstream 6.2): 8 for profiles
/// 0 and 1, 10 or 12 for profiles 2 and 3. `None` for a frame without a color config (an inter
/// frame, or one that shows an existing frame) or a header that does not parse.
#[must_use]
pub fn vp9_bit_depth(frame: &[u8]) -> Option<u32> {
    const SYNC_CODE: u32 = 0x49_83_42;
    let mut bits = Bits::new(frame);
    if bits.read(2)? != 2 {
        return None;
    }
    let low = bits.read(1)?;
    let profile = (bits.read(1)? << 1) | low;
    if profile == 3 {
        bits.skip(1)?;
    }
    if bits.flag()? {
        return None;
    }
    let key_frame = !bits.flag()?;
    let show_frame = bits.flag()?;
    let error_resilient = bits.flag()?;
    if !key_frame {
        let intra_only = !show_frame && bits.flag()?;
        if !error_resilient {
            bits.skip(2)?;
        }
        if !intra_only || bits.read(24)? != SYNC_CODE {
            return None;
        }
        // Intra-only frames of profile 0 carry no color config and are 8-bit.
        if profile == 0 {
            return Some(8);
        }
    } else if bits.read(24)? != SYNC_CODE {
        return None;
    }
    if profile < 2 {
        return Some(8);
    }
    Some(if bits.flag()? { 12 } else { 10 })
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

/// A most-significant-bit-first reader over an unescaped payload.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn flag(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(bit == 1)
    }

    fn read(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some((value << 1) | u32::from(self.flag()?)))
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        let end = self.at.checked_add(count)?;
        (end <= self.data.len() * 8).then(|| self.at = end)
    }

    /// An unsigned Exp-Golomb value (9.1).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1_u32 << zeros) - 1 + self.read(zeros)?)
    }

    /// A signed Exp-Golomb value (9.1.1).
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HevcLayers, ParameterSetCodec, avc_decoder_config, crop_window, hevc_decoder_config,
        hevc_layers, leading_crop, length_prefixed_units, sequence_facts, vp9_bit_depth,
    };

    /// The VPS of `tests/corpus/hevc-alpha.mov` (`hevc_videotoolbox` with `-alpha_quality`): two
    /// layers, the second auxiliary.
    const ALPHA_VPS: [u8; 41] = [
        0x40, 0x01, 0x0c, 0x11, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x1e, 0x17, 0x06, 0xff, 0x1e, 0x08, 0x00, 0x08, 0x30, 0x28,
        0x53, 0x80, 0x50, 0x00, 0x30, 0x50, 0x0c, 0x23, 0xf1, 0x7a, 0x40,
    ];

    fn start_coded(nal: &[u8]) -> Vec<u8> {
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(nal);
        data
    }

    #[test]
    fn hevc_alpha_is_detected_from_its_vps() {
        assert_eq!(
            hevc_layers(&start_coded(&ALPHA_VPS)),
            Some(HevcLayers::AuxiliaryAlpha)
        );
        // The same VPS with one layer is the base alone.
        let mut single = ALPHA_VPS;
        single[3] &= 0x0f;
        assert_eq!(hevc_layers(&start_coded(&single)), Some(HevcLayers::Single));
        // Two layers with no auxiliary picture (MV-HEVC views): byte 25 holds
        // scalability_mask_flag[1] (0x20, multiview) and [3] (0x08, auxiliary).
        let mut views = ALPHA_VPS;
        views[25] = 0x20;
        assert_eq!(hevc_layers(&start_coded(&views)), Some(HevcLayers::Extra));
        assert_eq!(hevc_layers(&[1, 2, 3]), None);
    }

    /// The corpus's 128x96 H.264 SPS (High, 4:2:0, frames only) with
    /// `window` from the ninth byte on.
    fn h264_sps(window: &[u8]) -> Vec<u8> {
        let mut sps = vec![0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x42, 0x0d];
        sps.extend_from_slice(window);
        sps.extend_from_slice(&[
            0x16, 0xa0, 0x20, 0x20, 0x28, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x01,
            0xe0, 0x78, 0x91, 0x2c, 0xb0,
        ]);
        sps
    }

    /// The corpus's 64x48 HEVC SPS (Main) with `window` as its conformance window.
    fn hevc_sps(window: &[u8]) -> Vec<u8> {
        let mut sps = vec![
            0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x03, 0x00, 0x1e, 0xa0, 0x20, 0x83,
        ];
        sps.extend_from_slice(window);
        sps.extend_from_slice(&[
            0x59, 0x59, 0xae, 0x4c, 0xaf, 0x01, 0x69, 0x6c, 0x20, 0x00, 0x00, 0x03, 0x00, 0x20,
            0x00, 0x00, 0x03, 0x03, 0xc1,
        ]);
        sps
    }

    fn avcc(sps: &[u8]) -> Vec<u8> {
        let mut data = vec![1, 0x64, 0x00, 0x0a, 0xff, 0xe1];
        data.extend_from_slice(&u16::try_from(sps.len()).unwrap().to_be_bytes());
        data.extend_from_slice(sps);
        // One PPS after it, as every `avcC` carries.
        data.extend_from_slice(&[1, 0, 4, 0x68, 0xeb, 0xe3, 0xcb]);
        data
    }

    fn hvcc(sps: &[u8]) -> Vec<u8> {
        let mut data = vec![1];
        data.extend_from_slice(&[0; 21]);
        // Two arrays: a stand-in VPS, then the SPS.
        data.extend_from_slice(&[2, 32, 0, 1, 0, 2, 0x40, 0x01, 33, 0, 1]);
        data.extend_from_slice(&u16::try_from(sps.len()).unwrap().to_be_bytes());
        data.extend_from_slice(sps);
        data
    }

    fn annex_b(sps: &[u8]) -> Vec<u8> {
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(sps);
        data.extend_from_slice(&[0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb]);
        data
    }

    #[test]
    fn an_h264_sps_reports_the_columns_and_rows_it_crops_on_the_left_and_top() {
        for (window, expected) in [
            (&[0xb0][..], (0, 0)),
            (&[0xcb, 0xf0], (8, 0)),
            (&[0xf2, 0xf0], (0, 8)),
            (&[0xe5, 0xf0], (0, 0)),
            (&[0xf9, 0x30], (0, 0)),
        ] {
            let sps = h264_sps(window);
            for extradata in [avcc(&sps), annex_b(&sps)] {
                assert_eq!(
                    leading_crop(ParameterSetCodec::H264, &extradata),
                    Some(expected),
                    "{window:02x?}"
                );
            }
        }
    }

    #[test]
    fn an_hevc_sps_reports_the_columns_and_rows_its_window_takes_on_the_left_and_top() {
        for (window, expected) in [
            (&[0x16][..], (0, 0)),
            (&[0x19, 0x7e], (8, 0)),
            (&[0x1e, 0x5e], (0, 8)),
            (&[0x1c, 0xbe], (0, 0)),
            (&[0x1f, 0x2e], (0, 0)),
        ] {
            let sps = hevc_sps(window);
            for extradata in [hvcc(&sps), annex_b(&sps)] {
                assert_eq!(
                    leading_crop(ParameterSetCodec::Hevc, &extradata),
                    Some(expected),
                    "{window:02x?}"
                );
            }
        }
    }

    #[test]
    fn the_whole_crop_window_is_read_from_the_corpus() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
        for (name, expected) in [
            ("h264-bt709-crop-left.mp4", (8, 0, 0, 0)),
            ("h264-bt709-crop-bottom.mp4", (0, 0, 0, 6)),
            ("h264-padded.mp4", (0, 0, 0, 12)),
            ("hevc-padded.mp4", (0, 0, 0, 4)),
        ] {
            let file = std::fs::read(corpus.join(name)).expect(name);
            let hevc = name.starts_with("hevc");
            let tag: &[u8] = if hevc { b"hvcC" } else { b"avcC" };
            let at = file
                .windows(4)
                .position(|window| window == tag)
                .expect("decoder configuration");
            let size = u32::from_be_bytes(file[at - 4..at].try_into().expect("size")) as usize;
            let record = &file[at + 4..at - 4 + size];
            let codec = if hevc {
                ParameterSetCodec::Hevc
            } else {
                ParameterSetCodec::H264
            };
            let crop = crop_window(codec, record).expect(name);
            assert_eq!(
                (crop.left, crop.right, crop.top, crop.bottom),
                expected,
                "{name}"
            );
            let visible = crop.visible().expect("a visible area");
            let probed = match name {
                "h264-bt709-crop-left.mp4" => (120, 96),
                "h264-bt709-crop-bottom.mp4" => (128, 90),
                _ => (320, 180),
            };
            assert_eq!((visible.width, visible.height), probed, "{name}");
        }
    }

    #[test]
    fn the_vui_aspect_color_and_chroma_siting_are_read_from_the_corpus() {
        use alpha2_media::{ChromaLocation, ColorMetadata, SampleAspectRatio};

        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
        let hlg = ColorMetadata::from_h273(9, 18, 9, Some(false));
        let bt709 = ColorMetadata::from_h273(1, 1, 1, Some(false));
        let range_only = ColorMetadata::from_h273(2, 2, 2, Some(false));
        // As ffprobe reports them.
        for (name, ratio, color, chroma) in [
            (
                "h264-bt709-vui-sar.mp4",
                (4, 3),
                bt709,
                ChromaLocation::Left,
            ),
            (
                "hevc-topleft.mp4",
                (1, 1),
                range_only,
                ChromaLocation::TopLeft,
            ),
            ("hevc-hvc1.mp4", (1, 1), range_only, ChromaLocation::TopLeft),
            ("hdr-hlg.mp4", (1, 1), hlg, ChromaLocation::Left),
            ("hevc-padded.mp4", (1, 1), bt709, ChromaLocation::Left),
            ("h264-high10.mp4", (1, 1), None, ChromaLocation::Left),
        ] {
            let file = std::fs::read(corpus.join(name)).expect(name);
            let hevc = name.starts_with("hevc");
            let tag: &[u8] = if hevc { b"hvcC" } else { b"avcC" };
            let at = file
                .windows(4)
                .position(|window| window == tag)
                .expect("decoder configuration");
            let size = u32::from_be_bytes(file[at - 4..at].try_into().expect("size")) as usize;
            let codec = if hevc {
                ParameterSetCodec::Hevc
            } else {
                ParameterSetCodec::H264
            };
            let facts = sequence_facts(codec, &file[at + 4..at - 4 + size]).expect(name);
            let (num, den) = ratio;
            assert_eq!(
                facts
                    .vui
                    .sample_aspect_ratio
                    .unwrap_or(SampleAspectRatio { num: 1, den: 1 }),
                SampleAspectRatio { num, den },
                "{name}"
            );
            assert_eq!(facts.vui.color, color, "{name}");
            assert_eq!(facts.chroma_location(), Some(chroma), "{name}");
        }
    }

    /// The configuration record after `tag` in a corpus file.
    fn record(name: &str, tag: &[u8]) -> Vec<u8> {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
        let file = std::fs::read(corpus.join(name)).expect(name);
        let at = file
            .windows(4)
            .position(|window| window == tag)
            .expect("decoder configuration");
        let size = u32::from_be_bytes(file[at - 4..at].try_into().expect("size")) as usize;
        file[at + 4..at - 4 + size].to_vec()
    }

    /// Parameter sets with start codes, as an encoder emits them.
    fn annex_b_of(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&[0, 0, 0, 1][..], unit].concat())
            .collect()
    }

    #[test]
    fn decoder_configurations_are_rebuilt_from_annex_b_parameter_sets() {
        let avcc = record("h264-bt709-copy.mp4", b"avcC");
        let units = super::avcc_units(&avcc);
        let built = avc_decoder_config(&annex_b_of(&units)).expect("avcC");
        assert_eq!(built, avcc);
        let hvcc = record("hevc-hvc1.mp4", b"hvcC");
        let built = hevc_decoder_config(&annex_b_of(&super::hvcc_units(&hvcc))).expect("hvcC");
        // Equal but for what the encoder may declare beyond the parameter sets: spatial
        // segmentation, parallelism, frame rate and its SEI array.
        assert_eq!(built[..13], hvcc[..13]);
        assert_eq!(built[16..19], hvcc[16..19]);
        assert_eq!(built[21] & 0x3f, hvcc[21] & 0x3f);
        let parameter_sets = |record: &[u8]| {
            super::hvcc_units(record)
                .into_iter()
                .filter(|unit| matches!((unit[0] >> 1) & 0x3f, 32..=34))
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>()
        };
        assert_eq!(parameter_sets(&built), parameter_sets(&hvcc));
        assert_eq!(built[22], 3);
        assert_eq!(
            length_prefixed_units(&[0, 0, 0, 1, 0x65, 9, 0, 0, 1, 0x41, 7]),
            [0, 0, 0, 2, 0x65, 9, 0, 0, 0, 2, 0x41, 7]
        );
    }

    #[test]
    fn a_description_without_a_readable_sps_reports_nothing() {
        assert_eq!(leading_crop(ParameterSetCodec::H264, &[]), None);
        // Cut short inside the crop window.
        let mut truncated = h264_sps(&[0xcb, 0xf0]);
        truncated.truncate(9);
        assert_eq!(
            leading_crop(ParameterSetCodec::H264, &avcc(&truncated)),
            None
        );
    }

    /// MSB-first bytes of a string of `0` and `1`, zero-padded to whole bytes.
    fn packed(bits: &str) -> Vec<u8> {
        let bits = bits.bytes().filter(|bit| *bit != b' ').collect::<Vec<_>>();
        bits.chunks(8)
            .map(|chunk| {
                (0..8).fold(0_u8, |byte, at| {
                    (byte << 1) | u8::from(chunk.get(at) == Some(&b'1'))
                })
            })
            .collect()
    }

    const VP9_SYNC: &str = "01001001 10000011 01000010";

    #[test]
    fn vp9_keyframes_declare_their_bit_depth() {
        // The first bytes of libvpx keyframes: profile 0 (`tests/corpus/vp9.webm`), then
        // profile 2 at 10 and at 12 bits.
        assert_eq!(vp9_bit_depth(&[0x82, 0x49, 0x83, 0x42, 0x00]), Some(8));
        assert_eq!(vp9_bit_depth(&[0x92, 0x49, 0x83, 0x42, 0x00]), Some(10));
        assert_eq!(vp9_bit_depth(&[0x92, 0x49, 0x83, 0x42, 0x80]), Some(12));
        // Profile 3 has a reserved bit after the profile; profile 1 is 8-bit.
        assert_eq!(
            vp9_bit_depth(&packed(&format!("10 1 1 0 0 0 1 0 {VP9_SYNC} 1"))),
            Some(12)
        );
        assert_eq!(
            vp9_bit_depth(&packed(&format!("10 1 0 0 0 1 0 {VP9_SYNC}"))),
            Some(8)
        );
    }

    #[test]
    fn vp9_intra_only_frames_declare_their_bit_depth() {
        // Not shown, not error resilient, intra-only, two bits of reset_frame_context.
        assert_eq!(
            vp9_bit_depth(&packed(&format!("10 0 1 0 1 0 0 1 00 {VP9_SYNC} 0"))),
            Some(10)
        );
        assert_eq!(
            vp9_bit_depth(&packed(&format!("10 0 0 0 1 0 0 1 00 {VP9_SYNC}"))),
            Some(8)
        );
    }

    #[test]
    fn vp9_frames_without_a_color_config_declare_nothing() {
        // An inter frame, a frame showing an existing one, a broken sync code, a wrong marker,
        // a truncated header.
        assert_eq!(vp9_bit_depth(&[0x86, 0x00, 0x00, 0x00]), None);
        assert_eq!(vp9_bit_depth(&[0x88]), None);
        assert_eq!(vp9_bit_depth(&[0x82, 0x49, 0x83, 0x43, 0x00]), None);
        assert_eq!(vp9_bit_depth(&[0x42, 0x49, 0x83, 0x42, 0x00]), None);
        assert_eq!(vp9_bit_depth(&[0x92, 0x49]), None);
        assert_eq!(vp9_bit_depth(&[]), None);
    }
}
