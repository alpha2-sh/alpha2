//! The "Blur & Sharpen" group's kernels.

use super::keying::tap_count;
use super::{CpuKernel, KernelContext, aspect, effect_row, grow, margin, premultiplied, saturate};
use crate::cpu::image::PixelBox;
use crate::cpu::kernels::{fract, shade_row};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        "alpha2.effect.sharpen" => &Sharpen,
        "alpha2.effect.radial-blur" => &RadialBlur,
        "alpha2.effect.lens-blur" => &LensBlur,
        "alpha2.effect.surface-blur" => &SurfaceBlur,
        _ => return None,
    })
}

/// sharpen.wgsl: an unsharp mask against the coverage-weighted ring of
/// four neighbours. The neighbours sit exactly one texel away, where the
/// sampler reads that texel (the edge repeated).
struct Sharpen;

impl CpuKernel for Sharpen {
    // A transparent pixel is returned as it is.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let resolution = context.resolution();
        let texel = [1.0 / resolution[0], 1.0 / resolution[1]];
        let image = context.source_a;
        let exact = image.size() == context.size;
        effect_row(context, y, x0, out, |x, uv, src| {
            if src[3] <= 0.0001 {
                return src;
            }
            let [right, left, below, above] = if exact {
                let (x, y) = (i64::from(x), i64::from(y));
                [
                    image.texel_clamped(x + 1, y),
                    image.texel_clamped(x - 1, y),
                    image.texel_clamped(x, y + 1),
                    image.texel_clamped(x, y - 1),
                ]
            } else {
                [
                    context.sample_src([uv[0] + texel[0], uv[1]]),
                    context.sample_src([uv[0] - texel[0], uv[1]]),
                    context.sample_src([uv[0], uv[1] + texel[1]]),
                    context.sample_src([uv[0], uv[1] - texel[1]]),
                ]
            };
            let mut ring_sum = [0.0_f32; 4];
            for channel in 0..4 {
                ring_sum[channel] =
                    right[channel] + left[channel] + below[channel] + above[channel];
            }
            let centre = [src[0] / src[3], src[1] / src[3], src[2] / src[3]];
            let ring = if ring_sum[3] > 0.0001 {
                [
                    ring_sum[0] / ring_sum[3],
                    ring_sum[1] / ring_sum[3],
                    ring_sum[2] / ring_sum[3],
                ]
            } else {
                centre
            };
            let rgb = [0, 1, 2].map(|c| saturate(centre[c] + (centre[c] - ring[c]) * amount));
            premultiplied(rgb, src[3])
        });
    }
}

/// The source's bounds grown by the `radius` parameter, in output heights.
#[allow(clippy::cast_precision_loss)]
fn grown_by_radius(context: &KernelContext<'_>) -> PixelBox {
    let reach = margin(context.float("radius") * context.size.height as f32);
    grow(context.source_a.bounds(), reach, reach, context.size)
}

/// radial-blur.wgsl: the taps' scale (zoom) or turn (spin) is the same for
/// every pixel. Zooming pulls in what lies outside the source.
struct RadialBlur;

impl CpuKernel for RadialBlur {
    #[allow(clippy::cast_precision_loss)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let taps = tap_count(context.max_filter_samples(), 1.0);
        let aspect = aspect(context);
        let center = context.vec2("center");
        let amount = context.float("amount");
        let spin = context.float("angle").to_radians();
        let zoom = context.choice("mode") != 1;
        // Each tap as the matrix taking the pixel's offset to the tap's.
        let matrices = (0..taps)
            .map(|i| {
                let t = (i as f32 + 0.5) / taps as f32 - 0.5;
                if zoom {
                    let scale = 1.0 + amount * t;
                    [scale, 0.0, 0.0, scale]
                } else {
                    let (c, s) = (libm::cosf(spin * t), libm::sinf(spin * t));
                    [c, -s, s, c]
                }
            })
            .collect::<Vec<_>>();
        shade_row(context, y, x0, out, |_, uv| {
            let offset = [
                (uv[0] - center[0]) * aspect[0],
                (uv[1] - center[1]) * aspect[1],
            ];
            let mut sum = [0.0_f32; 4];
            for m in &matrices {
                let p = if zoom {
                    [offset[0] * m[0], offset[1] * m[3]]
                } else {
                    [
                        m[0] * offset[0] + m[1] * offset[1],
                        m[2] * offset[0] + m[3] * offset[1],
                    ]
                };
                let sample = context
                    .sample_src([center[0] + p[0] / aspect[0], center[1] + p[1] / aspect[1]]);
                for channel in 0..4 {
                    sum[channel] += sample[channel];
                }
            }
            sum.map(|value| value / taps as f32)
        });
    }
}

/// lens-blur.wgsl: Vogel's disc, the same taps for every pixel.
struct LensBlur;

impl CpuKernel for LensBlur {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        grown_by_radius(context)
    }

    #[allow(clippy::cast_precision_loss)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let taps = tap_count(context.max_filter_samples(), 1.0);
        let radius = context.float("radius");
        let aspect = aspect(context);
        let reach = [radius / aspect[0], radius / aspect[1]];
        let highlights = context.float("highlights");
        let offsets = (0..taps)
            .map(|i| {
                let r = ((i as f32 + 0.5) / taps as f32).sqrt();
                let a = fract(i as f32 * 0.381_966_01) * std::f32::consts::TAU;
                [libm::cosf(a) * r * reach[0], libm::sinf(a) * r * reach[1]]
            })
            .collect::<Vec<_>>();
        shade_row(context, y, x0, out, |_, uv| {
            let mut sum = [0.0_f32; 4];
            let mut weight = 0.0_f32;
            for offset in &offsets {
                let s = context.sample_src([uv[0] + offset[0], uv[1] + offset[1]]);
                let l = s[0] * 0.2126 + s[1] * 0.7152 + s[2] * 0.0722;
                let w = 1.0 + highlights * l * l * l * l;
                for channel in 0..4 {
                    sum[channel] += s[channel] * w;
                }
                weight += w;
            }
            sum.map(|value| value / weight)
        });
    }
}

/// surface-blur.wgsl: the odd grid of taps and their distance weights are
/// the same for every pixel; only the color weight is not.
struct SurfaceBlur;

impl CpuKernel for SurfaceBlur {
    // Next to the content a transparent pixel's taps differ from it, but
    // are averaged in.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        grown_by_radius(context)
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let samples = context.max_filter_samples().clamp(1.0, 64.0);
        let full = (samples.sqrt() + 0.0001).floor();
        let side = 2.0 * ((full - 1.0) * 0.5).floor() + 1.0;
        let count = side as u32;
        let radius = context.float("radius");
        let aspect = aspect(context);
        let reach = [radius / aspect[0], radius / aspect[1]];
        let threshold = context.float("threshold");
        let inverse_range = 1.0 / (2.0 * threshold * threshold);
        // Row by row, as the shader's loops run.
        let mut taps = Vec::with_capacity((count * count) as usize);
        for ty in 0..count {
            for tx in 0..count {
                let o = [tx, ty].map(|value| (value as f32 + 0.5) / side * 2.0 - 1.0);
                let spatial = libm::expf(-(o[0] * o[0] + o[1] * o[1]) * 2.0);
                taps.push(([o[0] * reach[0], o[1] * reach[1]], spatial));
            }
        }
        effect_row(context, y, x0, out, |_, uv, src| {
            let mut sum = [0.0_f32; 4];
            let mut weight = 0.0_f32;
            for &(offset, spatial) in &taps {
                let s = context.sample_src([uv[0] + offset[0], uv[1] + offset[1]]);
                let d = [s[0] - src[0], s[1] - src[1], s[2] - src[2], s[3] - src[3]];
                let distance = d[0] * d[0] + d[1] * d[1] + d[2] * d[2] + d[3] * d[3];
                let w = spatial * libm::expf(-distance * inverse_range);
                for channel in 0..4 {
                    sum[channel] += s[channel] * w;
                }
                weight += w;
            }
            let weight = weight.max(0.000_001);
            sum.map(|value| value / weight)
        });
    }
}
