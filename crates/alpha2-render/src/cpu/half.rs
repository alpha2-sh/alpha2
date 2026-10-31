//! Half floats as the intermediate stores them.
//!
//! The GPU's `Rgba16Float` targets round a finite value toward zero when they
//! store it, keep subnormals, store a finite value past the largest half as
//! that half and keep infinity: measured on NVIDIA, RADV, lavapipe, llvmpipe
//! GL, Apple M4, Intel Iris Xe and NVIDIA MX450 over Vulkan and DX12. The
//! composite shader writes values that are already halves (`stored_half` in
//! composite.wesl). Both are [`store`] here, so every picture the CPU keeps
//! holds the bits a GPU keeps.

use std::sync::OnceLock;

/// `value` rounded toward zero to the nearest half float, as its bits.
///
/// Subnormal halves are kept (a step of 2^-24), finite magnitudes past the
/// largest finite half become it, infinity stays infinity and NaN stays NaN.
#[must_use]
#[inline]
pub(crate) fn store(value: f32) -> u16 {
    let bits = value.to_bits();
    // The common case first: a positive normal half, whose bits are the
    // float's exponent rebiased and its top ten fraction bits.
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
        // A subnormal half counts steps of 2^-24: the significand, 1.m, times
        // 2^(unbiased + 24), truncated.
        #[allow(clippy::cast_sign_loss)]
        let shift = (-1 - unbiased) as u32;
        #[allow(clippy::cast_possible_truncation)]
        return sign | ((mantissa | 0x0080_0000) >> shift) as u16;
    }
    sign
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

/// Every half float's value, indexed by its bits: a lookup is exact, as the
/// conversion it replaces is.
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

/// An 8-bit code as the GPU stores a value in an `Rgba8Unorm` target or
/// plane: clamped, scaled and rounded to nearest, ties to even.
#[must_use]
#[inline]
pub(crate) fn unorm8(value: f32) -> u8 {
    // Adding and taking away 2^23 rounds a value in 0..=255 to the nearest
    // whole number, ties to even, in IEEE arithmetic: what `round_ties_even`
    // gives, without its call into the math library.
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
            // Just past the value, toward the next half away from zero, it still truncates to it.
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
    fn the_composite_shaders_stored_half_agrees() {
        // composite.wesl: normals masked to 10 fraction bits, subnormals trunc(v * 2^24) / 2^24.
        let shader = |value: f32| {
            if value >= 1.0 / 16_384.0 {
                f32::from_bits(value.to_bits() & 0xffff_e000)
            } else {
                (value * 16_777_216.0).trunc() * (1.0 / 16_777_216.0)
            }
        };
        let mut value = 1.0e-8_f32;
        while value < 60_000.0 {
            assert_eq!(load(store(value)), shader(value), "{value}");
            value *= 1.000_37;
        }
    }

    #[test]
    fn unorm8_rounds_ties_to_even() {
        assert_eq!(unorm8(0.5), 128);
        assert_eq!(unorm8(-1.0), 0);
        assert_eq!(unorm8(2.0), 255);
        assert_eq!(unorm8(unorm8_value(77)), 77);
    }
}
