//! Removes the `user_data_unregistered` SEI `VideoToolbox` writes into its
//! H.264 output.
//!
//! The SEI (payload type 5, Apple's UUID `47564adc-5c4c-433f-94ef-c5113cd143a8`)
//! carries encoder state that differs between runs of the same encode, so two
//! exports of one project would differ in a few bytes and nothing else. No
//! decoder reads it, so dropping it changes no decoded picture and makes the
//! output repeatable.

use rsmpeg::avcodec::AVPacket;

/// Apple's `user_data_unregistered` UUID.
const APPLE_UUID: [u8; 16] = [
    0x47, 0x56, 0x4a, 0xdc, 0x5c, 0x4c, 0x43, 0x3f, 0x94, 0xef, 0xc5, 0x11, 0x3c, 0xd1, 0x43, 0xa8,
];

/// H.264's SEI NAL unit type.
const NAL_SEI: u8 = 6;

/// SEI payload type `user_data_unregistered`.
const USER_DATA_UNREGISTERED: u32 = 5;

/// Drops the SEI NAL units of `packet` whose every message is Apple's
/// `user_data_unregistered`, leaving every other byte as it was.
///
/// # Errors
/// When the packet's buffer cannot be made writable.
pub(crate) fn strip_vt_user_data_sei(
    packet: &mut AVPacket,
) -> Result<(), alpha2_media::EncodeError> {
    let bytes = alpha2_ffmpeg_bridge::packet_bytes(packet);
    match strip_bytes(&bytes) {
        Some(stripped) => alpha2_ffmpeg_bridge::shrink_packet_to(packet, &stripped)
            .map_err(|error| alpha2_media::EncodeError::Adapter(error.to_string())),
        None => Ok(()),
    }
}

/// `data` without Apple's SEI NAL units, or `None` when it has none.
///
/// `FFmpeg`'s `VideoToolbox` encoder writes Annex B (four-byte start codes);
/// a packet that does not begin with a start code is read as four-byte
/// length prefixes, the layout `VideoToolbox` itself produces.
pub(crate) fn strip_bytes(data: &[u8]) -> Option<Vec<u8>> {
    let units = if start_code_length(data, 0).is_some() {
        annex_b_units(data)
    } else {
        length_prefixed_units(data)?
    };
    if !units
        .iter()
        .any(|unit| is_apple_sei(&data[unit.payload.clone()]))
    {
        return None;
    }
    let mut kept = Vec::with_capacity(data.len());
    for unit in units {
        if !is_apple_sei(&data[unit.payload.clone()]) {
            kept.extend_from_slice(&data[unit.whole]);
        }
    }
    Some(kept)
}

/// One NAL unit: its bytes with the start code or length prefix that
/// introduces it, and the NAL unit alone.
struct Unit {
    whole: std::ops::Range<usize>,
    payload: std::ops::Range<usize>,
}

/// The length of the start code at `at` (3 or 4 bytes), if one is there.
fn start_code_length(data: &[u8], at: usize) -> Option<usize> {
    match data.get(at..) {
        Some([0, 0, 1, ..]) => Some(3),
        Some([0, 0, 0, 1, ..]) => Some(4),
        _ => None,
    }
}

/// The NAL units of an Annex B buffer that begins with a start code. Zero
/// bytes before the next start code belong to the unit before it
/// (`trailing_zero_8bits`), so a rebuilt buffer keeps them.
fn annex_b_units(data: &[u8]) -> Vec<Unit> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= data.len() {
        if let Some(length) = start_code_length(data, at) {
            starts.push((at, at + length));
            at += length;
        } else {
            at += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, &(start, payload))| {
            let end = starts.get(index + 1).map_or(data.len(), |next| next.0);
            Unit {
                whole: start..end,
                payload: payload..end,
            }
        })
        .collect()
}

/// The NAL units of a buffer of four-byte big-endian length prefixes, or
/// `None` when a length runs past the end.
fn length_prefixed_units(data: &[u8]) -> Option<Vec<Unit>> {
    let mut units = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let length = u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?);
        let end = (at + 4).checked_add(usize::try_from(length).ok()?)?;
        if end > data.len() {
            return None;
        }
        units.push(Unit {
            whole: at..end,
            payload: at + 4..end,
        });
        at = end;
    }
    Some(units)
}

/// Whether `unit` is an SEI NAL unit holding only Apple's
/// `user_data_unregistered` messages.
fn is_apple_sei(unit: &[u8]) -> bool {
    let Some((&header, rest)) = unit.split_first() else {
        return false;
    };
    if header & 0x1f != NAL_SEI {
        return false;
    }
    let payload = unescape(rest);
    let mut at = 0;
    let mut messages = 0;
    // A message starts while more than the RBSP trailing bits (0x80) remain.
    while at < payload.len() && payload[at..] != [0x80] {
        let Some((kind, next)) = read_ff_coded(&payload, at) else {
            return false;
        };
        let Some((size, next)) = read_ff_coded(&payload, next) else {
            return false;
        };
        let Some(end) = usize::try_from(size)
            .ok()
            .and_then(|size| next.checked_add(size))
        else {
            return false;
        };
        if kind != USER_DATA_UNREGISTERED
            || end > payload.len()
            || payload.get(next..next + 16) != Some(&APPLE_UUID[..])
        {
            return false;
        }
        messages += 1;
        at = end;
    }
    messages > 0
}

/// An SEI payload type or size: `0xff` bytes, each adding 255, then the last
/// byte. Returns the value and the offset after it.
fn read_ff_coded(data: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let mut value = 0_u32;
    loop {
        let byte = *data.get(at)?;
        at += 1;
        value = value.checked_add(u32::from(byte))?;
        if byte != 0xff {
            return Some((value, at));
        }
    }
}

/// `data` with its emulation prevention bytes (the `0x03` of `00 00 03`)
/// removed.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SEI `VideoToolbox` on Apple silicon wrote at the head of an export's first
    /// access unit: Apple's UUID, then 29 bytes of encoder state (one of
    /// them differs between runs), escaped as it was in the stream.
    const APPLE_SEI: &[u8] = &[
        0x06, 0x05, 0x2d, 0x47, 0x56, 0x4a, 0xdc, 0x5c, 0x4c, 0x43, 0x3f, 0x94, 0xef, 0xc5, 0x11,
        0x3c, 0xd1, 0x43, 0xa8, 0x01, 0x00, 0x00, 0x03, 0x00, 0x01, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x01, 0x02, 0x00, 0x07, 0x43, 0x60, 0x0b, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x08,
        0xb0, 0xba, 0x0c, 0x03, 0x91, 0x2b, 0x01, 0x80,
    ];
    const IDR: &[u8] = &[0x25, 0xb8, 0x20, 0x01, 0x3f, 0x8d];
    const SLICE: &[u8] = &[0x21, 0xe1, 0x04, 0x42];
    /// A recovery-point SEI (payload type 6), which decoders read.
    const RECOVERY_POINT: &[u8] = &[0x06, 0x06, 0x01, 0xc4, 0x80];

    fn annex_b(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&[0_u8, 0, 0, 1][..], unit].concat())
            .collect()
    }

    fn length_prefixed(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&u32::try_from(unit.len()).unwrap().to_be_bytes()[..], unit].concat())
            .collect()
    }

    #[test]
    fn the_apple_user_data_sei_is_removed() {
        assert_eq!(
            strip_bytes(&annex_b(&[APPLE_SEI, IDR, SLICE])),
            Some(annex_b(&[IDR, SLICE]))
        );
        // A three-byte start code, and a unit that is the last one.
        let mut short = vec![0, 0, 1];
        short.extend_from_slice(APPLE_SEI);
        short.extend_from_slice(&annex_b(&[IDR]));
        assert_eq!(strip_bytes(&short), Some(annex_b(&[IDR])));
        assert_eq!(
            strip_bytes(&annex_b(&[IDR, APPLE_SEI])),
            Some(annex_b(&[IDR]))
        );
        // The layout `VideoToolbox` itself writes, should a packet arrive in it.
        assert_eq!(
            strip_bytes(&length_prefixed(&[APPLE_SEI, IDR])),
            Some(length_prefixed(&[IDR]))
        );
        // Two Apple messages in one unit.
        let mut two = APPLE_SEI[..APPLE_SEI.len() - 1].to_vec();
        two.extend_from_slice(&APPLE_SEI[1..]);
        assert!(is_apple_sei(&two));
    }

    #[test]
    fn other_sei_and_slices_are_untouched() {
        for packet in [
            annex_b(&[IDR, SLICE]),
            annex_b(&[RECOVERY_POINT, IDR]),
            length_prefixed(&[RECOVERY_POINT, SLICE]),
            Vec::new(),
        ] {
            assert_eq!(strip_bytes(&packet), None, "{packet:02x?}");
        }
        // Apple's message beside another in one unit is kept whole.
        let mut mixed = RECOVERY_POINT[..RECOVERY_POINT.len() - 1].to_vec();
        mixed.extend_from_slice(&APPLE_SEI[1..]);
        assert!(!is_apple_sei(&mixed));
        assert_eq!(
            strip_bytes(&annex_b(&[&mixed, APPLE_SEI, IDR])),
            Some(annex_b(&[&mixed, IDR]))
        );
        // Another UUID is someone else's data.
        let mut other = APPLE_SEI.to_vec();
        other[3] ^= 1;
        assert_eq!(strip_bytes(&annex_b(&[&other, IDR])), None);
        // Trailing zero bytes before the next start code stay with their unit.
        let mut padded = annex_b(&[APPLE_SEI, IDR]);
        padded.extend_from_slice(&[0, 0]);
        padded.extend_from_slice(&annex_b(&[SLICE]));
        let mut expected = annex_b(&[IDR]);
        expected.extend_from_slice(&[0, 0]);
        expected.extend_from_slice(&annex_b(&[SLICE]));
        assert_eq!(strip_bytes(&padded), Some(expected));
    }
}
