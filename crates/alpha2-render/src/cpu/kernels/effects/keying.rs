//! The "Keying" group's kernels.

use super::{
    CpuKernel, KernelContext, aspect, centre, decode3, effect_row, gray, grow, luminance, margin,
    output, pow_tables, premultiplied, saturate, straight, table_pow,
};
use crate::builtins::CHROMA_KEY_ID;
use crate::cpu::image::PixelBox;
use crate::cpu::kernels::{builtin_smoothstep, mix, smoothstep};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        CHROMA_KEY_ID => &ChromaKey,
        "alpha2.effect.luma-key" => &LumaKey,
        "alpha2.effect.spill-suppress" => &SpillSuppress,
        "alpha2.effect.alpha-adjust" => &AlphaAdjust,
        "alpha2.effect.matte-choker" => &MatteChoker,
        "alpha2.effect.set-matte" => &SetMatte,
        _ => return None,
    })
}

/// `builtin_chroma_plane`.
#[inline]
fn chroma_plane(rgb: [f32; 3]) -> [f32; 2] {
    let luma = luminance(rgb);
    [rgb[2] - luma, rgb[0] - luma]
}

/// chroma-key.wgsl.
struct ChromaKey;

impl CpuKernel for ChromaKey {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let key = context.vec4("key_color");
        let key = [key[0], key[1], key[2]];
        let key_plane = chroma_plane(key);
        let threshold = context.float("threshold");
        let softness = context.float("softness");
        let spill = context.float("spill_suppression");
        let chroma = context.choice("distance_mode") == 1;
        effect_row(context, y, x0, out, |_, _, src| {
            if src[3] <= 0.000_001 {
                return [0.0; 4];
            }
            let rgb = [src[0] / src[3], src[1] / src[3], src[2] / src[3]];
            let distance = if chroma {
                let plane = chroma_plane(rgb);
                let (a, b) = (plane[0] - key_plane[0], plane[1] - key_plane[1]);
                (a * a + b * b).sqrt()
            } else {
                let (r, g, b) = (rgb[0] - key[0], rgb[1] - key[1], rgb[2] - key[2]);
                (r * r + g * g + b * b).sqrt()
            };
            let keep = smoothstep(threshold, threshold + softness, distance);
            let luma = luminance(rgb);
            let amount = (1.0 - keep) * spill;
            let corrected = rgb.map(|value| mix(value, luma, amount));
            premultiplied(corrected, src[3] * keep)
        });
    }
}

/// luma-key.wgsl.
struct LumaKey;

impl CpuKernel for LumaKey {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let threshold = context.float("threshold");
        let softness = context.float("softness");
        let bright = context.choice("key_out") == 1;
        effect_row(context, y, x0, out, |_, _, src| {
            let rgb = straight(src, 0.0001);
            let value = gray(rgb);
            let keep = if bright {
                1.0 - builtin_smoothstep(1.0 - threshold - softness, 1.0 - threshold, value)
            } else {
                builtin_smoothstep(threshold, threshold + softness, value)
            };
            output(rgb, src[3] * keep)
        });
    }
}

/// spill-suppress.wgsl.
struct SpillSuppress;

impl CpuKernel for SpillSuppress {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    // The shader's sum and half, not a midpoint's.
    #[allow(clippy::manual_midpoint)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let blue = context.choice("screen") != 0;
        effect_row(context, y, x0, out, |_, _, src| {
            let mut rgb = straight(src, 0.0001);
            if blue {
                rgb[2] = mix(rgb[2], rgb[2].min((rgb[0] + rgb[1]) * 0.5), amount);
            } else {
                rgb[1] = mix(rgb[1], rgb[1].min((rgb[0] + rgb[2]) * 0.5), amount);
            }
            output(rgb, src[3])
        });
    }
}

/// alpha-adjust.wgsl.
struct AlphaAdjust;

impl CpuKernel for AlphaAdjust {
    // Inverted, transparent pixels become opaque black.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        if context.boolean("invert") {
            PixelBox::whole(context.size)
        } else {
            context.source_a.bounds()
        }
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let black = context.float("black");
        let range = (context.float("white") - black).max(0.0001);
        let exponent = 1.0 / context.float("gamma");
        let invert = context.boolean("invert");
        let tables = pow_tables();
        effect_row(context, y, x0, out, |_, _, src| {
            let rgb = straight(src, 0.0001);
            let x = saturate((src[3] - black) / range);
            let curved = if x > 0.0 {
                table_pow(tables, x.max(1.0e-10), exponent)
            } else {
                0.0
            };
            output(rgb, if invert { 1.0 - curved } else { curved })
        });
    }
}

/// matte-choker.wgsl: the ring of taps, alternately at the radius and at
/// half of it, is the same for every pixel.
struct MatteChoker;

impl CpuKernel for MatteChoker {
    // Dilating spreads the matte by the radius.
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let source = context.source_a.bounds();
        if context.float("choke") >= 0.0 {
            return source;
        }
        let reach = margin(context.float("radius") * context.size.height as f32);
        grow(source, reach, reach, context.size)
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let taps = tap_count(context.max_filter_samples(), 8.0);
        let choke = context.float("choke");
        let radius = context.float("radius");
        let aspect = aspect(context);
        let reach = [radius / aspect[0], radius / aspect[1]];
        let turn = std::f32::consts::TAU / taps as f32;
        // The ring is the same for every pixel.
        let ring = (0..taps)
            .map(|i| {
                let angle = i as f32 * turn;
                let scale = if i & 1 == 1 { 0.5 } else { 1.0 };
                [
                    libm::cosf(angle) * reach[0] * scale,
                    libm::sinf(angle) * reach[1] * scale,
                ]
            })
            .collect::<Vec<_>>();
        effect_row(context, y, x0, out, |_, uv, src| {
            let mut low = src[3];
            let mut high = src[3];
            let mut spread = [0.0_f32; 4];
            for offset in &ring {
                let s = context.sample_src([uv[0] + offset[0], uv[1] + offset[1]]);
                low = low.min(s[3]);
                high = high.max(s[3]);
                let gain = (s[3] - src[3]).max(0.0);
                for channel in 0..4 {
                    spread[channel] += s[channel] * gain;
                }
            }
            let picked = if choke >= 0.0 {
                src.map(|value| value * (low / src[3].max(0.0001)))
            } else {
                let floor = spread[3].max(1.0e-12);
                let gained = high - src[3];
                [
                    src[0] + saturate(spread[0] / floor) * gained,
                    src[1] + saturate(spread[1] / floor) * gained,
                    src[2] + saturate(spread[2] / floor) * gained,
                    high,
                ]
            };
            let t = choke.abs();
            [0, 1, 2, 3].map(|c| mix(src[c], picked[c], t))
        });
    }
}

/// `u32(clamp(max_filter_samples(), low, 64.0))`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn tap_count(samples: f32, low: f32) -> u32 {
    samples.max(low).min(64.0) as u32
}

/// set-matte.wgsl: an unbound matte covers nothing.
struct SetMatte;

impl CpuKernel for SetMatte {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let matte = context
            .inputs
            .iter()
            .find(|(name, _)| *name == "matte")
            .map(|(_, image)| *image);
        let luma = context.choice("mode") == 1;
        let invert = context.boolean("invert");
        let size = context.size;
        effect_row(context, y, x0, out, |x, _, src| {
            let m = matte.map_or([0.0; 4], |image| centre(image, size, x, y));
            let coverage = if luma {
                let linear = decode3(straight(m, 0.0001).map(saturate));
                luminance(linear) * m[3]
            } else {
                m[3]
            };
            let kept = saturate(if invert { 1.0 - coverage } else { coverage });
            src.map(|value| value * kept)
        });
    }
}
