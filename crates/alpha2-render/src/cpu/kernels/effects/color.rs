//! The "Color" group's kernels.

use super::{
    CpuKernel, KernelContext, decode, decode3, effect_row, encode, gray, luminance, output,
    pow_tables, premultiplied, saturate, straight, table_pow,
};
use crate::builtins::LUT_3D_ID;
use crate::cpu::curves;
use crate::cpu::image::PixelBox;
use crate::cpu::kernels::{
    CpuLut, builtin_hsv_to_rgb, builtin_rgb_to_hsv, builtin_smoothstep, fract, mix,
};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        LUT_3D_ID => &Lut3d,
        "alpha2.effect.color-controls" => &ColorControls,
        "alpha2.effect.invert" => &Invert,
        "alpha2.effect.black-and-white" => &BlackAndWhite,
        "alpha2.effect.sepia" => &Sepia,
        "alpha2.effect.duotone" => &Duotone,
        "alpha2.effect.gradient-map" => &GradientMap,
        "alpha2.effect.posterize" => &Posterize,
        "alpha2.effect.threshold" => &Threshold,
        "alpha2.effect.levels" => &Levels,
        "alpha2.effect.hue-saturation" => &HueSaturation,
        "alpha2.effect.vibrance" => &Vibrance,
        "alpha2.effect.channel-mixer" => &ChannelMixer,
        "alpha2.effect.lift-gamma-gain" => &LiftGammaGain,
        "alpha2.effect.color-isolate" => &ColorIsolate,
        "alpha2.effect.solarize" => &Solarize,
        "alpha2.effect.fill" => &Fill,
        _ => return None,
    })
}

/// `sample_<lut>(rgb)` as the registry generates it: the domain mapped to
/// texel centres (the half-texel inset), then a trilinear sample with the
/// edge repeated.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(in crate::cpu::kernels) fn sample_lut(lut: &CpuLut, rgb: [f32; 3]) -> [f32; 3] {
    let edge = lut.edge as f32;
    let last = lut.edge.saturating_sub(1) as usize;
    let mut base = [0_usize; 3];
    let mut next = [0_usize; 3];
    let mut fraction = [0.0_f32; 3];
    for axis in 0..3 {
        let normalized = saturate(
            (rgb[axis] - lut.domain_min[axis]) / (lut.domain_max[axis] - lut.domain_min[axis]),
        );
        let coord = (normalized * (edge - 1.0) + 0.5) / edge;
        let position = coord * edge - 0.5;
        let floor = position.floor();
        fraction[axis] = position - floor;
        let index = floor.max(0.0) as usize;
        base[axis] = index.min(last);
        next[axis] = (index + 1).min(last);
    }
    let size = lut.edge as usize;
    let at = |r: usize, g: usize, b: usize| lut.values[(b * size + g) * size + r];
    let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    let [r0, g0, b0] = base;
    let [r1, g1, b1] = next;
    let [fr, fg, fb] = fraction;
    let low = lerp(
        lerp(at(r0, g0, b0), at(r1, g0, b0), fr),
        lerp(at(r0, g1, b0), at(r1, g1, b0), fr),
        fg,
    );
    let high = lerp(
        lerp(at(r0, g0, b1), at(r1, g0, b1), fr),
        lerp(at(r0, g1, b1), at(r1, g1, b1), fr),
        fg,
    );
    lerp(low, high, fb)
}

/// lut-3d.wgsl.
struct Lut3d;

impl CpuKernel for Lut3d {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        // The executor refuses a pass whose table is not registered.
        let Some(lut) = context.lut("lut") else {
            out.fill([0.0; 4]);
            return;
        };
        let strength = context.float("strength");
        effect_row(context, y, x0, out, |_, _, src| {
            if src[3] <= 0.000_001 {
                return [0.0; 4];
            }
            let rgb = [src[0] / src[3], src[1] / src[3], src[2] / src[3]];
            let mapped = sample_lut(lut, rgb);
            premultiplied(
                [
                    mix(rgb[0], mapped[0], strength),
                    mix(rgb[1], mapped[1], strength),
                    mix(rgb[2], mapped[2], strength),
                ],
                src[3],
            )
        });
    }
}

/// color-controls.wgsl: grading in linear light.
struct ColorControls;

const MIDDLE_GREY: f32 = 0.18;

impl CpuKernel for ColorControls {
    // A transparent pixel grades to black times its alpha: transparent.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let exposure = context.float("exposure");
        let temperature = context.float("temperature");
        let tint = context.float("tint");
        let contrast = context.float("contrast");
        let saturation = context.float("saturation");
        // The gain is the same for every pixel.
        let gain = [
            libm::exp2f(exposure + temperature * 0.5),
            libm::exp2f(exposure + tint * 0.5),
            libm::exp2f(exposure + -temperature * 0.5),
        ];
        // `pow(x, 1)` is `x`; skipping it keeps the common case exact.
        #[allow(clippy::float_cmp)]
        let unit_contrast = contrast == 1.0;
        let tables = pow_tables();
        effect_row(context, y, x0, out, |_, _, src| {
            let encoded = straight(src, 0.0001);
            let mut rgb = [0.0_f32; 3];
            for channel in 0..3 {
                let linear = decode(encoded[channel]) * gain[channel];
                let ratio = (linear / MIDDLE_GREY).max(1.0e-10);
                rgb[channel] = MIDDLE_GREY
                    * if unit_contrast {
                        ratio
                    } else {
                        table_pow(tables, ratio, contrast)
                    };
            }
            let luma = luminance(rgb);
            let graded =
                rgb.map(|value| curves::linear_to_srgb(saturate(mix(luma, value, saturation))));
            premultiplied(graded, src[3])
        });
    }
}

/// Fills `out` with `builtin_output(grade(builtin_straight(src)), src.a)`,
/// the shape of every per-pixel color body.
#[inline]
fn grade_row(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
    grade: impl Fn([f32; 3]) -> [f32; 3],
) {
    effect_row(context, y, x0, out, |_, _, src| {
        output(grade(straight(src, 0.0001)), src[3])
    });
}

/// `mix(a, b, t)` on each channel.
#[inline]
fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [mix(a[0], b[0], t), mix(a[1], b[1], t), mix(a[2], b[2], t)]
}

/// `dot(a, b)`.
#[inline]
fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The rgb of a `vec4` or color parameter.
#[inline]
fn rgb_of(value: [f32; 4]) -> [f32; 3] {
    [value[0], value[1], value[2]]
}

/// invert.wgsl.
struct Invert;

impl CpuKernel for Invert {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        grade_row(context, y, x0, out, |rgb| {
            rgb.map(|value| mix(value, 1.0 - value, amount))
        });
    }
}

/// black-and-white.wgsl.
struct BlackAndWhite;

impl CpuKernel for BlackAndWhite {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let weights = [
            context.float("red"),
            context.float("green"),
            context.float("blue"),
        ];
        grade_row(context, y, x0, out, |rgb| {
            let gray = encode(saturate(dot3(decode3(rgb), weights)));
            rgb.map(|value| mix(value, gray, amount))
        });
    }
}

/// sepia.wgsl.
struct Sepia;

impl CpuKernel for Sepia {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        grade_row(context, y, x0, out, |rgb| {
            let toned = [
                dot3(rgb, [0.393, 0.769, 0.189]),
                dot3(rgb, [0.349, 0.686, 0.168]),
                dot3(rgb, [0.272, 0.534, 0.131]),
            ];
            mix3(rgb, toned, amount)
        });
    }
}

/// duotone.wgsl.
struct Duotone;

impl CpuKernel for Duotone {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let shadows = rgb_of(context.vec4("shadows"));
        let highlights = rgb_of(context.vec4("highlights"));
        let amount = context.float("amount");
        grade_row(context, y, x0, out, |rgb| {
            let toned = mix3(shadows, highlights, gray(rgb));
            mix3(rgb, toned, amount)
        });
    }
}

/// gradient-map.wgsl.
struct GradientMap;

impl CpuKernel for GradientMap {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let shadows = rgb_of(context.vec4("shadows"));
        let midtones = rgb_of(context.vec4("midtones"));
        let highlights = rgb_of(context.vec4("highlights"));
        let m = context.float("midpoint");
        let amount = context.float("amount");
        grade_row(context, y, x0, out, |rgb| {
            let t = gray(rgb);
            let mapped = if t < m {
                mix3(shadows, midtones, t / m)
            } else {
                mix3(midtones, highlights, (t - m) / (1.0 - m))
            };
            mix3(rgb, mapped, amount)
        });
    }
}

/// posterize.wgsl.
struct Posterize;

impl CpuKernel for Posterize {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        // WGSL `round` takes ties to even.
        let steps = context.float("levels").round_ties_even().max(2.0) - 1.0;
        grade_row(context, y, x0, out, |rgb| {
            rgb.map(|value| (value * steps + 0.5).floor() / steps)
        });
    }
}

/// threshold.wgsl.
struct Threshold;

impl CpuKernel for Threshold {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let level = context.float("level");
        let half_soft = context.float("softness") * 0.5;
        grade_row(context, y, x0, out, |rgb| {
            let value = builtin_smoothstep(level - half_soft, level + half_soft, gray(rgb));
            [value; 3]
        });
    }
}

/// levels.wgsl.
struct Levels;

impl CpuKernel for Levels {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let mask = match context.choice("channel") {
            1 => [1.0, 0.0, 0.0],
            2 => [0.0, 1.0, 0.0],
            3 => [0.0, 0.0, 1.0],
            _ => [1.0; 3],
        };
        let input_black = context.float("input_black");
        let range = (context.float("input_white") - input_black).max(0.0001);
        let exponent = 1.0 / context.float("gamma");
        let output_black = context.float("output_black");
        let output_white = context.float("output_white");
        // `pow(x, 1)` is `x`; skipping it keeps the default exact.
        #[allow(clippy::float_cmp)]
        let unit_gamma = exponent == 1.0;
        let tables = pow_tables();
        grade_row(context, y, x0, out, |rgb| {
            let mut leveled = [0.0_f32; 3];
            for channel in 0..3 {
                let x = saturate((rgb[channel] - input_black) / range).max(1.0e-10);
                let curved = if unit_gamma {
                    x
                } else {
                    table_pow(tables, x, exponent)
                };
                leveled[channel] = mix(
                    rgb[channel],
                    mix(output_black, output_white, curved),
                    mask[channel],
                );
            }
            leveled
        });
    }
}

/// hue-saturation.wgsl.
struct HueSaturation;

impl CpuKernel for HueSaturation {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let hue = context.float("hue") / 360.0;
        let saturation = 1.0 + context.float("saturation");
        let lightness = context.float("lightness");
        let toward = if lightness > 0.0 { 1.0 } else { 0.0 };
        grade_row(context, y, x0, out, |rgb| {
            let mut hsv = builtin_rgb_to_hsv(rgb);
            hsv[0] += hue;
            hsv[1] = saturate(hsv[1] * saturation);
            builtin_hsv_to_rgb(hsv).map(|value| mix(value, toward, lightness.abs()))
        });
    }
}

/// vibrance.wgsl.
struct Vibrance;

impl CpuKernel for Vibrance {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        grade_row(context, y, x0, out, |rgb| {
            let saturation = rgb[0].max(rgb[1].max(rgb[2])) - rgb[0].min(rgb[1].min(rgb[2]));
            let gray = luminance(rgb);
            let scale = 1.0 + amount * (1.0 - saturation);
            rgb.map(|value| mix(gray, value, scale))
        });
    }
}

/// channel-mixer.wgsl.
struct ChannelMixer;

impl CpuKernel for ChannelMixer {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let rows = [
            context.vec4("red"),
            context.vec4("green"),
            context.vec4("blue"),
        ];
        let monochrome = context.boolean("monochrome");
        grade_row(context, y, x0, out, |rgb| {
            let mixed = rows.map(|row| dot3(rgb, rgb_of(row)) + row[3]);
            if monochrome { [mixed[0]; 3] } else { mixed }
        });
    }
}

/// lift-gamma-gain.wgsl.
struct LiftGammaGain;

impl CpuKernel for LiftGammaGain {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let [lift, gamma, gain] = ["lift", "gamma", "gain"].map(|name| context.vec4(name));
        let lift = [0, 1, 2].map(|channel| lift[channel] + lift[3]);
        let gain = [0, 1, 2].map(|channel| 1.0 + gain[channel] + gain[3]);
        let exponent = [0, 1, 2].map(|channel| 1.0 / (1.0 + gamma[channel] + gamma[3]).max(0.01));
        // `pow(x, 1)` is `x`; skipping it keeps the default exact.
        #[allow(clippy::float_cmp)]
        let unit_gamma = exponent.map(|value| value == 1.0);
        let tables = pow_tables();
        grade_row(context, y, x0, out, |rgb| {
            let mut curved = [0.0_f32; 3];
            for channel in 0..3 {
                let graded = (rgb[channel] * gain[channel] + lift[channel] * (1.0 - rgb[channel]))
                    .max(1.0e-10);
                curved[channel] = if unit_gamma[channel] {
                    graded
                } else {
                    table_pow(tables, graded, exponent[channel])
                };
            }
            curved
        });
    }
}

/// color-isolate.wgsl.
struct ColorIsolate;

impl CpuKernel for ColorIsolate {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let hue = context.float("hue") / 360.0;
        let range = context.float("range");
        let softness = context.float("softness");
        let amount = context.float("amount");
        grade_row(context, y, x0, out, |rgb| {
            let hsv = builtin_rgb_to_hsv(rgb);
            let hue_gap = (fract(hsv[0] - hue + 0.5) - 0.5).abs() * 360.0;
            let in_hue = 1.0 - builtin_smoothstep(range, range + softness, hue_gap);
            let keep = in_hue * builtin_smoothstep(0.05, 0.2, hsv[1]);
            let gray = gray(rgb);
            rgb.map(|value| mix(value, gray, amount * (1.0 - keep)))
        });
    }
}

/// solarize.wgsl.
struct Solarize;

impl CpuKernel for Solarize {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let threshold = context.float("threshold");
        grade_row(context, y, x0, out, |rgb| {
            rgb.map(|value| {
                if value > threshold {
                    1.0 - value
                } else {
                    value
                }
            })
        });
    }
}

/// fill.wgsl.
struct Fill;

impl CpuKernel for Fill {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let color = context.vec4("color");
        let strength = context.float("amount") * color[3];
        grade_row(context, y, x0, out, |rgb| {
            mix3(rgb, rgb_of(color), strength)
        });
    }
}
