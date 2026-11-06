//! The CRCs FLAC frames and Ogg pages carry, MSB first and unreflected.

#[allow(clippy::cast_possible_truncation)]
const fn table8(poly: u8) -> [u8; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ poly
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

#[allow(clippy::cast_possible_truncation)]
const fn table16(poly: u16) -> [u16; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = (index as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ poly
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

#[allow(clippy::cast_possible_truncation)]
const fn table32(poly: u32) -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ poly
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

const CRC8: [u8; 256] = table8(0x07);
const CRC16: [u16; 256] = table16(0x8005);
const CRC32: [u32; 256] = table32(0x04c1_1db7);

/// FLAC's frame header CRC-8 (polynomial 0x07, initial 0).
pub(crate) fn crc8(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0, |crc, byte| CRC8[usize::from(crc ^ byte)])
}

/// FLAC's frame CRC-16 (polynomial 0x8005, initial 0); 0 over a frame with its footer.
pub(crate) fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0, |crc, byte| {
        (crc << 8) ^ CRC16[usize::from((crc >> 8) as u8 ^ byte)]
    })
}

/// Ogg's page CRC-32 (polynomial 0x04c11db7, initial 0, no final xor), continued from `crc`.
pub(crate) fn crc32(crc: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(crc, |crc, byte| {
        (crc << 8) ^ CRC32[usize::from((crc >> 24) as u8 ^ byte)]
    })
}
