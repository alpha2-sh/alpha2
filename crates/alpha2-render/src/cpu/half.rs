//! Half floats as the intermediate stores them.
//!
//! GPU `Rgba16Float` targets round finite values toward zero, keep subnormals, saturate finite
//! overflow to the largest half and keep infinity (measured across Vulkan, DX12 and Metal on
//! several vendors and software rasterizers); [`store`] is that conversion. Effect and composite
//! passes round their results to the nearest half first ([`store_nearest`]), so the conversion
//! only sees halves.

use std::sync::OnceLock;

/// `value` rounded toward zero to a half float, as bits.
///
/// Subnormals are kept (step 2^-24), finite overflow becomes the largest finite half, and
/// infinity and NaN are preserved.
#[must_use]
#[inline]
pub(crate) fn store(value: f32) -> u16 {
    let bits = value.to_bits();
    // Fast path for positive normal halves: rebiased exponent and top ten fraction bits.
    if (0x3880_0000..0x477f_e000).contains(&bits) {
        #[allow(clippy::cast_possible_truncation)]
        return ((bits >> 13) - (112 << 10)) as u16;
    }
    if bits == 0 {
        return 0;
    }
    store_slow(bits)
}

#[cold]
fn store_slow(bits: u32) -> u16 {
    let sign = ((bits >> 16) & 0x8000) as u16;
    #[allow(clippy::cast_possible_wrap)]
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        return if mantissa == 0 {
            sign | 0x7c00
        } else {
            sign | 0x7e00
        };
    }
    let unbiased = exponent - 127;
    if unbiased > 15 {
        return sign | 0x7bff;
    }
    if unbiased >= -14 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let biased = ((unbiased + 15) as u16) << 10;
        #[allow(clippy::cast_possible_truncation)]
        return sign | biased | (mantissa >> 13) as u16;
    }
    if unbiased >= -24 {
        // A subnormal half counts 2^-24 steps: 1.m times 2^(unbiased + 24), truncated.
        #[allow(clippy::cast_sign_loss)]
        let shift = (-1 - unbiased) as u32;
        #[allow(clippy::cast_possible_truncation)]
        return sign | ((mantissa | 0x0080_0000) >> shift) as u16;
    }
    sign
}

const SMALLEST_NORMAL: f32 = 1.0 / 16_384.0;

const SUBNORMAL_STEPS: f32 = 16_777_216.0;

/// Mirrors composite.wesl's `stored_half`: `value` rounded to the nearest half, ties to even.
#[must_use]
#[inline]
pub(crate) fn nearest(value: f32) -> f32 {
    if value.abs() >= SMALLEST_NORMAL {
        let bits = value.to_bits();
        f32::from_bits(bits.wrapping_add(0x0fff + ((bits >> 13) & 1)) & 0xffff_e000)
    } else {
        (value * SUBNORMAL_STEPS).round_ties_even() * (1.0 / SUBNORMAL_STEPS)
    }
}

#[must_use]
#[inline]
pub(crate) fn store_nearest(value: f32) -> u16 {
    store(nearest(value))
}

#[must_use]
#[inline]
pub(crate) fn store4_nearest(value: [f32; 4]) -> [u16; 4] {
    value.map(store_nearest)
}

/// The four components of a pixel, stored.
#[must_use]
#[inline]
pub(crate) fn store4(value: [f32; 4]) -> [u16; 4] {
    [
        store(value[0]),
        store(value[1]),
        store(value[2]),
        store(value[3]),
    ]
}

/// The value half float `bits` holds, exactly.
#[must_use]
#[inline]
pub(crate) fn load(bits: u16) -> f32 {
    table()[usize::from(bits)]
}

/// The four components of a stored pixel.
#[must_use]
#[inline]
pub(crate) fn load4(bits: [u16; 4]) -> [f32; 4] {
    let table = table();
    bits.map(|bits| table[usize::from(bits)])
}

/// Every half float's exact value, indexed by bits.
pub(crate) fn table() -> &'static [f32; 65_536] {
    static TABLE: OnceLock<Box<[f32; 65_536]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut values: Box<[f32; 65_536]> = vec![0.0_f32; 65_536]
            .into_boxed_slice()
            .try_into()
            .expect("65536 values");
        for (bits, value) in values.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let bits = bits as u16;
            *value = half::f16::from_bits(bits).to_f32();
        }
        values
    })
}

/// An 8-bit code as the GPU stores a value in an `Rgba8Unorm` target: clamped, scaled, rounded
/// to nearest even.
#[must_use]
#[inline]
pub(crate) fn unorm8(value: f32) -> u8 {
    // Adding and subtracting 2^23 rounds 0..=255 to nearest even in IEEE arithmetic, without a
    // math library call.
    const ROUND: f32 = 8_388_608.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let code = ((value.clamp(0.0, 1.0) * 255.0 + ROUND) - ROUND) as u8;
    code
}

/// The value an 8-bit code samples as.
#[must_use]
#[inline]
pub(crate) fn unorm8_value(code: u8) -> f32 {
    UNORM8[usize::from(code)]
}

static UNORM8: [f32; 256] = {
    let mut values = [0.0_f32; 256];
    let mut code = 0;
    while code < 256 {
        #[allow(clippy::cast_precision_loss)]
        {
            values[code] = code as f32 / 255.0;
        }
        code += 1;
    }
    values
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storing_rounds_toward_zero_and_keeps_subnormals_and_infinity() {
        for bits in 0..=u16::MAX {
            let value = half::f16::from_bits(bits);
            if value.is_nan() {
                continue;
            }
            let exact = value.to_f32();
            assert_eq!(store(exact), bits, "{exact} keeps its bits");
            if exact.is_infinite() {
                continue;
            }
            // One ulp further from zero still truncates to the same half.
            let nudged = f32::from_bits(exact.to_bits() + 1);
            if exact != 0.0 && (nudged.abs() < 65_520.0) {
                assert_eq!(store(nudged), bits, "{nudged} truncates to {exact}");
            }
        }
        assert_eq!(store(1.0e-9), 0);
        assert_eq!(store(-1.0e-9), 0x8000);
        assert_eq!(store(1.0e9), 0x7bff);
        assert_eq!(store(f32::INFINITY), 0x7c00);
        assert_eq!(store(f32::NEG_INFINITY), 0xfc00);
        assert_eq!(store(f32::MAX), 0x7bff);
        assert!(half::f16::from_bits(store(f32::NAN)).is_nan());
    }

    #[test]
    fn rounding_to_nearest_matches_ieee_half_conversion_in_range() {
        for bits in 0..=u16::MAX {
            let value = half::f16::from_bits(bits);
            if !value.is_finite() {
                continue;
            }
            let exact = value.to_f32();
            assert_eq!(store_nearest(exact), bits, "{exact} keeps its bits");
            for neighbour in [exact.next_up(), exact.next_down()] {
                if neighbour.abs() < 65_504.0 {
                    assert_eq!(
                        store_nearest(neighbour),
                        half::f16::from_f32(neighbour).to_bits(),
                        "{neighbour}"
                    );
                }
            }
        }
        let mut value = 1.0e-9_f32;
        while value < 65_504.0 {
            for signed in [value, -value] {
                assert_eq!(
                    store_nearest(signed),
                    half::f16::from_f32(signed).to_bits(),
                    "{signed}"
                );
            }
            value *= 1.000_37;
        }
        assert_eq!(store_nearest(65_520.0), 0x7bff);
        assert_eq!(store_nearest(f32::INFINITY), 0x7c00);
        assert!(half::f16::from_bits(store_nearest(f32::NAN)).is_nan());
    }

    #[test]
    fn unorm8_rounds_ties_to_even() {
        assert_eq!(unorm8(0.5), 128);
        assert_eq!(unorm8(-1.0), 0);
        assert_eq!(unorm8(2.0), 255);
        assert_eq!(unorm8(unorm8_value(77)), 77);
    }
}
