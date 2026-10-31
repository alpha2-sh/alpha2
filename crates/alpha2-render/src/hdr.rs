//! HDR sources brought into the SDR working space.
//!
//! The working space is display-referred linear light with BT.709 primaries,
//! where `1.0` is SDR white. An HDR source is linearized to
//! display light in nits (PQ by its EOTF, HLG by its inverse OETF and the
//! BT.2100 OOTF at its nominal peak), converted to BT.709 primaries, and
//! tone-mapped from its peak to SDR white by the ITU-R BT.2390 EETF applied to
//! the largest component, which keeps each pixel's hue. Components the
//! narrower gamut cannot hold are clipped at zero.
//!
//! These functions are the reference the YUV conversion shader implements;
//! GPU tests compare the two.

// The curves are evaluated in f64 and handed back as the f32 the renderer uses; every value
// is a bounded luminance or code.
#![allow(clippy::cast_possible_truncation)]

use alpha2_media::{ColorPrimaries, TransferFunction};

/// The luminance of SDR white, and so of the working space's `1.0`, in nits.
pub const SDR_WHITE_NITS: f32 = 100.0;
/// The peak an HDR source is assumed to reach when a project does not say.
pub const DEFAULT_HDR_PEAK_NITS: u32 = 1_000;
/// The luminance PQ's code value `1.0` stands for.
const PQ_PEAK_NITS: f64 = 10_000.0;

// SMPTE ST 2084 constants.
const PQ_M1: f64 = 2_610.0 / 16_384.0;
const PQ_M2: f64 = 2_523.0 / 4_096.0 * 128.0;
const PQ_C1: f64 = 3_424.0 / 4_096.0;
const PQ_C2: f64 = 2_413.0 / 4_096.0 * 32.0;
const PQ_C3: f64 = 2_392.0 / 4_096.0 * 32.0;

// ITU-R BT.2100 HLG constants.
const HLG_A: f64 = 0.178_832_77;
const HLG_B: f64 = 0.284_668_92;
const HLG_C: f64 = 0.559_910_73;
/// BT.2020 luma weights, which the HLG OOTF weighs scene light by.
const BT2020_LUMA: [f64; 3] = [0.2627, 0.6780, 0.0593];
/// The scene luma the HLG OOTF floors its gain at, as the GPU shader does.
const HLG_MIN_LUMA: f64 = 1.0e-10;

/// How an HDR source is tone-mapped: from `source_peak_nits` to SDR white.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HdrToneMap {
    /// The brightest luminance the source is taken to reach. PQ sources above
    /// it are clipped to it; HLG sources are displayed at it (their nominal
    /// peak).
    pub source_peak_nits: u32,
}

impl Default for HdrToneMap {
    fn default() -> Self {
        Self {
            source_peak_nits: DEFAULT_HDR_PEAK_NITS,
        }
    }
}

/// Display luminance, in nits, of a PQ code value in `0..=1`.
#[must_use]
pub fn pq_eotf(code: f32) -> f32 {
    let e = libm::pow(f64::from(code.clamp(0.0, 1.0)), 1.0 / PQ_M2);
    let y = libm::pow((e - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * e), 1.0 / PQ_M1);
    (y * PQ_PEAK_NITS) as f32
}

/// The PQ code value of a display luminance in nits.
#[must_use]
pub fn pq_inverse_eotf(nits: f32) -> f32 {
    let y = libm::pow((f64::from(nits) / PQ_PEAK_NITS).clamp(0.0, 1.0), PQ_M1);
    libm::pow((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), PQ_M2) as f32
}

/// Scene light in `0..=1` of an HLG code value (the inverse OETF).
#[must_use]
pub fn hlg_inverse_oetf(code: f32) -> f32 {
    let e = f64::from(code.clamp(0.0, 1.0));
    let light = if e <= 0.5 {
        e * e / 3.0
    } else {
        (libm::exp((e - HLG_C) / HLG_A) + HLG_B) / 12.0
    };
    light as f32
}

/// Display light in nits of HLG scene light shown at `peak_nits`: the BT.2100
/// OOTF, with the system gamma for that peak.
#[must_use]
pub fn hlg_ootf(scene: [f32; 3], peak_nits: f32) -> [f32; 3] {
    let gamma = 1.2 + 0.42 * libm::log10(f64::from(peak_nits) / 1_000.0);
    let luma: f64 = scene
        .iter()
        .zip(BT2020_LUMA)
        .map(|(component, weight)| f64::from(*component) * weight)
        .sum();
    // Below about 334 nits `gamma - 1` is negative and zero luma would give NaN: floor the luma
    // as the shader does (yuv-to-linear.wesl).
    let gain = f64::from(peak_nits) * libm::pow(luma.max(HLG_MIN_LUMA), gamma - 1.0);
    scene.map(|component| (gain * f64::from(component)) as f32)
}

/// The ITU-R BT.2390 EETF: `nits` from a source reaching `source_peak` mapped
/// onto a display reaching `target_peak`, both with black at zero. Below the
/// knee the curve is the identity; above it a Hermite spline rolls highlights
/// off to exactly `target_peak`. A source no brighter than the target passes
/// unchanged.
#[must_use]
pub fn bt2390_eetf(nits: f32, source_peak: f32, target_peak: f32) -> f32 {
    let source = f64::from(pq_inverse_eotf(source_peak));
    if source <= 0.0 {
        return 0.0;
    }
    let value = f64::from(pq_inverse_eotf(nits.min(source_peak))) / source;
    let max_lum = f64::from(pq_inverse_eotf(target_peak)) / source;
    let knee = 1.5 * max_lum - 0.5;
    let mapped = if value < knee || knee >= 1.0 {
        value
    } else {
        let t = (value - knee) / (1.0 - knee);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    pq_eotf((mapped * source) as f32)
}

/// The linear-light matrix from `from`'s primaries to `to`'s, both with a
/// D65 white point, as rows. BT.601 is taken as BT.709, as the SDR decode
/// path takes it.
#[must_use]
pub fn gamut_matrix(from: ColorPrimaries, to: ColorPrimaries) -> [[f32; 3]; 3] {
    let from = rgb_to_xyz(chromaticities(from));
    let to = invert(rgb_to_xyz(chromaticities(to)));
    let product = multiply(to, from);
    product.map(|row| row.map(|value| value as f32))
}

/// Linear working-space RGB, `1.0` at SDR white, of an HDR pixel's
/// nonlinear components in its own primaries.
#[must_use]
pub fn hdr_to_working(
    encoded: [f32; 3],
    transfer: TransferFunction,
    primaries: ColorPrimaries,
    tone: HdrToneMap,
) -> [f32; 3] {
    #[allow(clippy::cast_precision_loss)]
    let peak = tone.source_peak_nits as f32;
    let nits = match transfer {
        TransferFunction::Hlg => hlg_ootf(encoded.map(hlg_inverse_oetf), peak),
        _ => encoded.map(pq_eotf),
    };
    let gamut = gamut_matrix(primaries, ColorPrimaries::Bt709);
    let converted = gamut.map(|row| {
        row.iter()
            .zip(nits)
            .map(|(weight, component)| weight * component)
            .sum::<f32>()
    });
    let largest = converted[0].max(converted[1]).max(converted[2]);
    let scale = if largest > 0.0 {
        bt2390_eetf(largest, peak, SDR_WHITE_NITS) / largest
    } else {
        0.0
    };
    converted.map(|component| (component * scale / SDR_WHITE_NITS).clamp(0.0, 1.0))
}

/// CIE xy of red, green, blue and white.
fn chromaticities(primaries: ColorPrimaries) -> [[f64; 2]; 4] {
    const D65: [f64; 2] = [0.3127, 0.3290];
    match primaries {
        ColorPrimaries::Bt601 | ColorPrimaries::Bt709 => {
            [[0.640, 0.330], [0.300, 0.600], [0.150, 0.060], D65]
        }
        ColorPrimaries::Bt2020 => [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046], D65],
    }
}

/// The matrix taking linear RGB with these chromaticities to CIE XYZ.
fn rgb_to_xyz([red, green, blue, white]: [[f64; 2]; 4]) -> [[f64; 3]; 3] {
    let xyz = |[x, y]: [f64; 2]| [x / y, 1.0, (1.0 - x - y) / y];
    let columns = [xyz(red), xyz(green), xyz(blue)];
    let primaries = [
        [columns[0][0], columns[1][0], columns[2][0]],
        [columns[0][1], columns[1][1], columns[2][1]],
        [columns[0][2], columns[1][2], columns[2][2]],
    ];
    let white = xyz(white);
    let inverse = invert(primaries);
    let scale = inverse.map(|row| row.iter().zip(white).map(|(a, b)| a * b).sum::<f64>());
    primaries.map(|row| [row[0] * scale[0], row[1] * scale[1], row[2] * scale[2]])
}

fn multiply(a: [[f64; 3]; 3], b: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut product = [[0.0; 3]; 3];
    for (row, out) in product.iter_mut().enumerate() {
        for (column, cell) in out.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    product
}

fn invert(m: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let cofactor =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let adjugate = [
        [
            cofactor(1, 2, 1, 2),
            -cofactor(0, 2, 1, 2),
            cofactor(0, 1, 1, 2),
        ],
        [
            -cofactor(1, 2, 0, 2),
            cofactor(0, 2, 0, 2),
            -cofactor(0, 1, 0, 2),
        ],
        [
            cofactor(1, 2, 0, 1),
            -cofactor(0, 2, 0, 1),
            cofactor(0, 1, 0, 1),
        ],
    ];
    let determinant =
        m[0][0] * adjugate[0][0] + m[0][1] * adjugate[1][0] + m[0][2] * adjugate[2][0];
    adjugate.map(|row| row.map(|value| value / determinant))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_round_trips_and_hits_its_reference_points() {
        assert!((pq_eotf(1.0) - 10_000.0).abs() < 0.5);
        assert!(pq_eotf(0.0).abs() < 1.0e-6);
        // 100 nits sits at PQ code 0.5081 (BT.2100 table 4 region).
        assert!((pq_inverse_eotf(100.0) - 0.5081).abs() < 1.0e-3);
        for nits in [0.01_f32, 1.0, 100.0, 203.0, 1_000.0, 4_000.0] {
            let back = pq_eotf(pq_inverse_eotf(nits));
            assert!((back - nits).abs() / nits < 1.0e-4, "{nits} -> {back}");
        }
    }

    #[test]
    fn hlg_reference_white_is_203_nits_at_a_1000_nit_peak() {
        // BT.2408: 75 % HLG signal is reference white, 203 nits on a 1000-nit display.
        let scene = hlg_inverse_oetf(0.75);
        let [white, ..] = hlg_ootf([scene; 3], 1_000.0);
        assert!((white - 203.0).abs() < 1.5, "{white}");
        assert!((hlg_inverse_oetf(0.5) * 3.0 - 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn the_eetf_keeps_the_knee_reaches_the_target_and_never_rises_past_it() {
        assert!((bt2390_eetf(10.0, 1_000.0, 100.0) - 10.0).abs() < 0.01);
        // The source peak lands exactly on the target peak.
        assert!((bt2390_eetf(1_000.0, 1_000.0, 100.0) - 100.0).abs() < 0.05);
        let mut previous = 0.0;
        for step in 0_u16..=200 {
            let nits = f32::from(step) * 5.0;
            let mapped = bt2390_eetf(nits, 1_000.0, 100.0);
            assert!(mapped >= previous - 1.0e-4 && mapped <= 100.0 + 1.0e-3);
            previous = mapped;
        }
        // A source no brighter than the target is untouched.
        assert!((bt2390_eetf(80.0, 100.0, 100.0) - 80.0).abs() < 0.01);
    }

    #[test]
    fn gamut_matrices_convert_white_to_white_and_bt2020_red_out_of_gamut() {
        let m = gamut_matrix(ColorPrimaries::Bt2020, ColorPrimaries::Bt709);
        for row in m {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 1.0e-4);
        }
        // The published BT.2087 values.
        assert!((m[0][0] - 1.6605).abs() < 1.0e-3 && (m[2][2] - 1.1187).abs() < 1.0e-3);
        let identity = gamut_matrix(ColorPrimaries::Bt709, ColorPrimaries::Bt709);
        for (r, row) in identity.iter().enumerate() {
            for (c, value) in row.iter().enumerate() {
                assert!((value - f32::from(u8::from(r == c))).abs() < 1.0e-6);
            }
        }
    }

    #[test]
    fn hlg_black_is_black_at_every_peak_the_model_accepts() {
        // Below about 334 nits the system gamma is under 1, so zero luma meets a negative power.
        for peak in [100_u32, 200, 333, 334, 335, 1_000, 10_000] {
            #[allow(clippy::cast_precision_loss)]
            let display = hlg_ootf([0.0; 3], peak as f32);
            assert!(
                display.iter().all(|c| *c == 0.0),
                "{peak} nits: {display:?}"
            );
            let tone = HdrToneMap {
                source_peak_nits: peak,
            };
            // Code black, and a sub-black code that the inverse OETF clamps to zero light.
            for code in [0.0, -0.05] {
                let working = hdr_to_working(
                    [code; 3],
                    TransferFunction::Hlg,
                    ColorPrimaries::Bt2020,
                    tone,
                );
                assert!(
                    working.iter().all(|c| *c == 0.0),
                    "{peak} nits, code {code}: {working:?}"
                );
            }
            // A dim grey stays finite and above black.
            let grey = hdr_to_working(
                [0.1; 3],
                TransferFunction::Hlg,
                ColorPrimaries::Bt2020,
                tone,
            );
            assert!(
                grey.iter().all(|c| c.is_finite() && *c > 0.0),
                "{peak} nits: {grey:?}"
            );
        }
    }

    #[test]
    fn hdr_white_and_black_land_in_the_sdr_range_with_hue_kept() {
        let tone = HdrToneMap::default();
        let pq = |nits: f32| pq_inverse_eotf(nits);
        let black = hdr_to_working([0.0; 3], TransferFunction::Pq, ColorPrimaries::Bt2020, tone);
        assert!(black.iter().all(|component| *component == 0.0), "{black:?}");
        // 1000-nit PQ white is the brightest SDR white.
        let peak = hdr_to_working(
            [pq(1_000.0); 3],
            TransferFunction::Pq,
            ColorPrimaries::Bt2020,
            tone,
        );
        assert!(peak.iter().all(|c| (c - 1.0).abs() < 1.0e-3), "{peak:?}");
        // Dim grey below the knee keeps its level: 10 nits is 0.1 of SDR white.
        let grey = hdr_to_working(
            [pq(10.0); 3],
            TransferFunction::Pq,
            ColorPrimaries::Bt2020,
            tone,
        );
        assert!(grey.iter().all(|c| (c - 0.1).abs() < 2.0e-3), "{grey:?}");
        // A bright orange keeps its channel ratios.
        let orange = hdr_to_working(
            [pq(600.0), pq(300.0), pq(60.0)],
            TransferFunction::Pq,
            ColorPrimaries::Bt709,
            tone,
        );
        assert!((orange[1] / orange[0] - 0.5).abs() < 1.0e-3, "{orange:?}");
        assert!(orange[0] < 1.0 && orange[0] > 0.8);
        // HLG reference white is a bright, not clipped, SDR white.
        let hlg = hdr_to_working(
            [0.75; 3],
            TransferFunction::Hlg,
            ColorPrimaries::Bt2020,
            tone,
        );
        assert!(hlg.iter().all(|c| *c > 0.8 && *c < 1.0), "{hlg:?}");
    }
}
