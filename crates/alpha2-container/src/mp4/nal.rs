//! Bit-level reads of codec configuration, for AAC `AudioSpecificConfig` and HEVC VPS layers.

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
