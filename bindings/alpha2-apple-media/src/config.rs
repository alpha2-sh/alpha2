//! Codec configuration records read and written for the frameworks.

/// The parameter sets of an `avcC` record (SPS then PPS) and its NAL length size.
pub(crate) fn avc_parameter_sets(record: &[u8]) -> Option<(Vec<&[u8]>, usize)> {
    let length_size = usize::from(record.get(4)? & 3) + 1;
    let mut sets = Vec::new();
    let mut at = 5;
    for mask in [0x1f, 0xff] {
        let count = record.get(at)? & mask;
        at += 1;
        for _ in 0..count {
            let (unit, next) = length_prefixed(record, at)?;
            sets.push(unit);
            at = next;
        }
    }
    Some((sets, length_size))
}

/// The VPS, SPS and PPS of an `hvcC` record and its NAL length size. Other arrays (SEI) are
/// skipped: a description built from them fails.
pub(crate) fn hevc_parameter_sets(record: &[u8]) -> Option<(Vec<&[u8]>, usize)> {
    let length_size = usize::from(record.get(21)? & 3) + 1;
    let arrays = *record.get(22)?;
    let mut sets = Vec::new();
    let mut at = 23;
    for _ in 0..arrays {
        let count = u16::from_be_bytes([*record.get(at + 1)?, *record.get(at + 2)?]);
        at += 3;
        for _ in 0..count {
            let (unit, next) = length_prefixed(record, at)?;
            if (32..=34).contains(&((unit.first()? >> 1) & 0x3f)) {
                sets.push(unit);
            }
            at = next;
        }
    }
    Some((sets, length_size))
}

fn length_prefixed(record: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let length = usize::from(u16::from_be_bytes([*record.get(at)?, *record.get(at + 1)?]));
    Some((record.get(at + 2..at + 2 + length)?, at + 2 + length))
}

/// Bits per sample and `chroma_format_idc` (1 for 4:2:0) of an H.264 SPS.
pub(crate) fn avc_sps_format(sps: &[u8]) -> Option<(u8, u8)> {
    let mut bits = Bits::new(sps.get(1..)?);
    let profile = bits.read(8)?;
    bits.read(16)?; // constraint flags, level_idc
    bits.ue()?; // seq_parameter_set_id
    if !matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        return Some((8, 1));
    }
    let chroma = bits.ue()?;
    if chroma == 3 {
        bits.read(1)?; // separate_colour_plane_flag
    }
    let depth = bits.ue()? + 8;
    Some((u8::try_from(depth).ok()?, u8::try_from(chroma).ok()?))
}

/// Reads an RBSP, skipping emulation prevention bytes.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    bit: u8,
    zeros: u8,
}

impl<'a> Bits<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            at: 0,
            bit: 0,
            zeros: 0,
        }
    }

    fn flag(&mut self) -> Option<bool> {
        if self.bit == 0 {
            let byte = *self.data.get(self.at)?;
            if self.zeros >= 2 && byte == 3 {
                self.at += 1;
                self.zeros = 0;
                return self.flag();
            }
            self.zeros = if byte == 0 { self.zeros + 1 } else { 0 };
        }
        let value = self.data.get(self.at)? >> (7 - self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.at += 1;
        }
        Some(value == 1)
    }

    fn read(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some(value << 1 | u32::from(self.flag()?)))
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1 << zeros) - 1 + self.read(zeros)?)
    }
}

/// The channel configuration of an `AudioSpecificConfig`.
pub(crate) fn aac_channel_configuration(config: &[u8]) -> Option<u32> {
    let mut bits = config
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |bit| u32::from(byte >> bit & 1)));
    let mut read = |count: u32| -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some(value << 1 | bits.next()?))
    };
    if read(5)? == 31 {
        read(6)?; // escaped object type
    }
    if read(4)? == 15 {
        read(24)?; // explicit sampling frequency
    }
    read(4)
}

/// The decoder-specific info (`DecoderSpecificInfo`, tag 5) inside an `ES_Descriptor`, which may
/// follow an `esds` box's version and flags.
pub(crate) fn es_specific_info(descriptor: &[u8]) -> Option<&[u8]> {
    // A descriptor: tag, a size in 7-bit groups, then the body.
    fn split(bytes: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, mut rest) = bytes.split_first()?;
        let mut size = 0_usize;
        for _ in 0..4 {
            let (&byte, tail) = rest.split_first()?;
            rest = tail;
            size = size << 7 | usize::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        let (body, after) = rest.split_at_checked(size)?;
        Some((tag, body, after))
    }
    let start = if descriptor.first() == Some(&3) { 0 } else { 4 };
    let (tag, es, _) = split(descriptor.get(start..)?)?;
    if tag != 3 {
        return None;
    }
    // ES_ID, then flags naming optional fields: a dependency (2 bytes), a URL, an OCR id (2).
    let flags = *es.get(2)?;
    let mut at = 3;
    if flags & 0x80 != 0 {
        at += 2;
    }
    if flags & 0x40 != 0 {
        at += 1 + usize::from(*es.get(at)?);
    }
    if flags & 0x20 != 0 {
        at += 2;
    }
    let (tag, config, _) = split(es.get(at..)?)?;
    if tag != 4 {
        return None;
    }
    let (tag, specific, _) = split(config.get(13..)?)?;
    (tag == 5).then_some(specific)
}

/// Samples at 48 kHz an Opus packet decodes to, from its TOC byte (RFC 6716 3.1).
pub(crate) fn opus_packet_frames(packet: &[u8]) -> Option<usize> {
    let toc = *packet.first()?;
    let config = usize::from(toc >> 3);
    let frame = match config {
        0..=11 => [480, 960, 1_920, 2_880][config % 4],
        12..=15 => [480, 960][config % 2],
        _ => [120, 240, 480, 960][config % 4],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(packet.get(1)? & 0x3f),
    };
    Some(frame * count)
}

/// Samples an MPEG audio Layer III frame decodes to: 1152 for MPEG-1, 576 for MPEG-2 and 2.5.
pub(crate) fn mp3_packet_frames(packet: &[u8]) -> Option<usize> {
    let header = packet.get(..2)?;
    if header[0] != 0xff || header[1] & 0xe0 != 0xe0 {
        return None;
    }
    Some(if header[1] >> 3 & 3 == 3 { 1_152 } else { 576 })
}

/// Samples an E-AC-3 packet decodes to: 256 per audio block of its first syncframe.
pub(crate) fn eac3_packet_frames(packet: &[u8]) -> Option<usize> {
    if packet.get(..2)? != [0x0b, 0x77] {
        return None;
    }
    let byte = *packet.get(4)?;
    let blocks = if byte >> 6 == 3 {
        6
    } else {
        [1, 2, 3, 6][usize::from(byte >> 4 & 3)]
    };
    Some(blocks * 256)
}

/// An MPEG-4 descriptor: its tag, a four-byte expandable size, then `body`.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let size = u32::try_from(body.len()).unwrap_or(u32::MAX);
    let mut out = vec![tag];
    for shift in [21, 14, 7] {
        out.push(0x80 | ((size >> shift) & 0x7f) as u8);
    }
    out.push((size & 0x7f) as u8);
    out.extend_from_slice(body);
    out
}

/// An `ES_Descriptor` carrying `specific` (the decoder-specific info) for `object_type` in
/// stream type `stream_type` (ISO/IEC 14496-1 7.2.6.5).
pub(crate) fn es_descriptor(object_type: u8, stream_type: u8, specific: &[u8]) -> Vec<u8> {
    let mut config = vec![object_type, stream_type << 2 | 1];
    config.extend_from_slice(&[0; 11]); // bufferSizeDB, maxBitrate, avgBitrate
    config.extend_from_slice(&descriptor(5, specific));
    let mut es = vec![0, 1, 0]; // ES_ID 1, no dependence, URL or OCR stream
    es.extend_from_slice(&descriptor(4, &config));
    es.extend_from_slice(&descriptor(6, &[2])); // SLConfigDescriptor, predefined MP4
    descriptor(3, &es)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_profile_sps_reports_its_depth_and_chroma() {
        // High 10 4:2:0: profile 110, level 30, sps_id 0, chroma_format_idc 1, depth minus 8 = 2.
        let sps = [0x67, 110, 0, 30, 0b1010_0110, 0b1000_0000];
        assert_eq!(avc_sps_format(&sps), Some((10, 1)));
        assert_eq!(avc_sps_format(&[0x67, 66, 0, 30, 0x80]), Some((8, 1)));
    }

    #[test]
    fn emulation_prevention_bytes_are_skipped() {
        let mut bits = Bits::new(&[0, 0, 3, 1]);
        assert_eq!(bits.read(24), Some(1));
    }

    #[test]
    fn packet_frame_counts_come_from_their_headers() {
        assert_eq!(opus_packet_frames(&[0xfc]), Some(960));
        assert_eq!(opus_packet_frames(&[0x7b, 0x03]), Some(2_880));
        assert_eq!(opus_packet_frames(&[0x09]), Some(1_920));
        assert_eq!(mp3_packet_frames(&[0xff, 0xfb]), Some(1_152));
        assert_eq!(mp3_packet_frames(&[0xff, 0xf3]), Some(576));
        assert_eq!(mp3_packet_frames(&[0xff, 0xe3]), Some(576));
        assert_eq!(mp3_packet_frames(&[0x49, 0x44]), None);
        assert_eq!(eac3_packet_frames(&[0x0b, 0x77, 0, 0, 0x3f]), Some(1_536));
        assert_eq!(eac3_packet_frames(&[0x0b, 0x77, 0, 0, 0x0f]), Some(256));
    }

    #[test]
    fn es_descriptor_sizes_nest() {
        let es = es_descriptor(0x40, 5, &[0x12, 0x10]);
        assert_eq!(es_specific_info(&es), Some(&[0x12, 0x10][..]));
        let mut boxed = vec![0; 4];
        boxed.extend_from_slice(&es);
        assert_eq!(es_specific_info(&boxed), Some(&[0x12, 0x10][..]));
        assert_eq!(es[0], 3);
        assert_eq!(usize::from(es[4]), es.len() - 5);
        assert_eq!(
            &es[es.len() - 13..es.len() - 6],
            &[5, 0x80, 0x80, 0x80, 2, 0x12, 0x10]
        );
    }
}
