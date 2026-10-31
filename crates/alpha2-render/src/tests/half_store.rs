//! What the device stores in a half-float target against the CPU renderer's model of it
//! (`cpu/half.rs` for a store, `cpu/vector.rs` for the premultiplied `over` blend).
//!
//! Only half-float targets are held to the model: an 8-bit target's tie rounding differs between
//! GPUs by design, and every pass that writes one rounds its value to a code first.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::unreadable_literal
)]

use super::device_precision::{Probe, TEXELS, VALUES};
use super::{adapter_gpu, gpu_test_lock};
use crate::cpu::half;

/// A seeded generator, so every machine draws the same values.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }

    /// A float in [2^(low - 127), 2^(high - 126)) with all 23 fraction bits random, so rounding
    /// toward zero and to nearest differ for about half of them.
    fn float(&mut self, low: u32, high: u32) -> f32 {
        let exponent = low + self.next() % (high - low + 1);
        f32::from_bits((exponent << 23) | (self.next() & 0x007f_ffff))
    }

    /// A float in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / 16_777_216.0
    }
}

/// Inputs where rounding modes and range handling differ, with the half rounding toward zero
/// keeps where it differs from rounding to nearest.
const SPECIALS: [(&str, u32); 33] = [
    ("one", 0x3f80_0000),
    ("half", 0x3f00_0000),
    ("largest finite", 0x477f_e000),
    ("smallest normal", 0x3880_0000),
    ("smallest subnormal", 0x3380_0000),
    ("tie to even", 0x3f80_1000),
    ("tie to odd", 0x3f80_3000),              // 0x3c01; to nearest 0x3c02
    ("past a tie", 0x3f80_1001),              // 0x3c00; to nearest 0x3c01
    ("short of a tie", 0x3f80_0fff),          // 0x3c00
    ("negative past a tie", 0xbf80_1001),     // 0xbc00; to nearest 0xbc01
    ("negative tie to odd", 0xbf80_3000),     // 0xbc01; to nearest 0xbc02
    ("0.7", 0x3f33_3333),                     // 0x3999; to nearest 0x399a
    ("0.3", 0x3e99_999a),                     // 0x34cc; to nearest 0x34cd
    ("0.2", 0x3e4c_cccd),                     // 0x3266
    ("0.35", 0x3eb3_3333),                    // 0x3599; to nearest 0x359a
    ("0.6", 0x3f19_999a),                     // 0x38cc; to nearest 0x38cd
    ("1.5 subnormal steps", 0x33c0_0000),     // 0x0001; to nearest 0x0002
    ("0.75 subnormal steps", 0x3340_0000),    // 0x0000; to nearest 0x0001
    ("17.5 subnormal steps", 0x358c_0000),    // 0x0011; to nearest 0x0012
    ("17.75 subnormal steps", 0x358e_0000),   // 0x0011; to nearest 0x0012
    ("largest subnormal range", 0x387b_a882), // 0x03ee; to nearest 0x03ef
    ("negative 1.5 subnormal steps", 0xb3c0_0000), // 0x8001; to nearest 0x8002
    ("65519", 0x477f_ef00),                   // 0x7bff
    ("65520", 0x477f_f000),                   // 0x7bff; to nearest infinity
    ("1e5", 0x47c3_5000),                     // 0x7bff; to nearest infinity
    ("-1e5", 0xc7c3_5000),                    // 0xfbff; to nearest -infinity
    ("infinity", 0x7f80_0000),                // 0x7c00
    ("-infinity", 0xff80_0000),               // 0xfc00
    ("quiet NaN", 0x7fc0_0000),
    ("signalling NaN", 0x7f80_0001),
    ("f32 subnormal", 0x0001_16c2), // 0x0000
    ("-0", 0x8000_0000),
    ("0", 0x0000_0000),
];

fn is_nan(bits: u16) -> bool {
    bits & 0x7c00 == 0x7c00 && bits & 0x03ff != 0
}

fn halves(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect()
}

#[test]
fn rgba16float_stores_match_the_cpu_half_model() {
    let _lock = gpu_test_lock();
    let gpu = adapter_gpu();
    let probe = Probe::new(gpu);
    let mut random = Lcg(0x5c23_1630_0000_0001);
    let mut inputs = SPECIALS.iter().map(|(_, bits)| *bits).collect::<Vec<u32>>();
    while inputs.len() < VALUES {
        let magnitude = random.float(113, 127);
        let negative = random.next().is_multiple_of(8);
        inputs.push(if negative { -magnitude } else { magnitude }.to_bits());
    }
    let stored = halves(&probe.draw("store", wgpu::TextureFormat::Rgba16Float, &inputs, None));
    let mismatches = inputs
        .iter()
        .zip(&stored)
        .enumerate()
        .filter(|(_, (bits, device))| {
            let model = half::store(f32::from_bits(**bits));
            **device != model && !(is_nan(**device) && is_nan(model))
        })
        .map(|(index, (bits, device))| {
            let name = SPECIALS.get(index).map_or("random", |(name, _)| *name);
            format!(
                "{name} 0x{bits:08x}: device 0x{device:04x}, model 0x{:04x}",
                half::store(f32::from_bits(*bits))
            )
        })
        .collect::<Vec<_>>();
    assert!(
        mismatches.is_empty(),
        "{} of {VALUES} stored halves differ from the CPU model, first {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(8)]
    );
}

#[test]
fn premultiplied_over_stays_within_sixteen_half_ulps_of_the_cpu_model() {
    let _lock = gpu_test_lock();
    let gpu = adapter_gpu();
    if !gpu.capabilities().half_float_blendable {
        eprintln!("skipped: the adapter does not blend half floats");
        return;
    }
    let probe = Probe::new(gpu);
    let mut random = Lcg(0x5c23_1630_0000_0002);
    // A destination of stored halves (as the target holds it) and a full-precision source.
    let mut under = Vec::with_capacity(VALUES);
    let mut over = Vec::with_capacity(VALUES);
    let mut cases = Vec::with_capacity(VALUES);
    for _ in 0..TEXELS {
        let under_alpha = half::load(half::store(random.unit()));
        let over_alpha = random.unit().max(1.0 / 1024.0);
        let mut destination = [0.0_f32; 4];
        let mut source = [0.0_f32; 4];
        for channel in 0..3 {
            destination[channel] = half::load(half::store(under_alpha * random.unit()));
            source[channel] = over_alpha * random.unit();
        }
        destination[3] = under_alpha;
        source[3] = over_alpha;
        let mut model = destination;
        crate::cpu::premultiplied_over(&mut model, source);
        for channel in 0..4 {
            under.push(destination[channel].to_bits());
            over.push(source[channel].to_bits());
            cases.push(model[channel]);
        }
    }
    let blended = halves(&probe.draw(
        "store",
        wgpu::TextureFormat::Rgba16Float,
        &over,
        Some((&under, wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)),
    ));
    let mut matches = 0;
    let mut worst_ulps = 0_u16;
    let mut worst_codes = 0.0_f32;
    for (model, device) in cases.iter().zip(&blended) {
        let model_bits = half::store(*model);
        matches += usize::from(model_bits == *device);
        // Every value is in 0..=1, where a half's bits order as its value.
        worst_ulps = worst_ulps.max(model_bits.abs_diff(*device));
        worst_codes = worst_codes.max((half::load(*device) - model).abs() * 255.0);
    }
    eprintln!(
        "premultiplied over: {matches} of {VALUES} equal the CPU model, worst {worst_ulps} half \
         ULPs, {worst_codes:.4} codes"
    );
    // Measured: exact on lavapipe, llvmpipe, WARP and an Apple-silicon GPU; 1 half ULP on an
    // Intel integrated GPU; 16 on RADV and NVIDIA, whose worst is 2 half ULPs at the top of the
    // range, just under a quarter of a code.
    assert!(
        worst_ulps <= 16 && worst_codes <= 0.25,
        "the device's premultiplied over is {worst_ulps} half ULPs ({worst_codes} codes) from \
         the CPU model; at most 16 and 0.25 allowed"
    );
}
