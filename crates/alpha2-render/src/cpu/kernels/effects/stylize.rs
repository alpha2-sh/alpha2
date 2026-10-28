//! The "Stylize" group's kernels.

use std::f32::consts::TAU;

use super::{
    CpuKernel, KernelContext, aspect, effect_row, gray, grow, luminance, margin, output,
    premultiplied, sample_edge, saturate, seed, straight, uv_width,
};
use crate::cpu::image::PixelBox;
use crate::cpu::kernels::{
    builtin_hash, builtin_hash3, builtin_interval_coverage, builtin_smoothstep, builtin_unit,
    builtin_value_noise, fract, fwidth_fine, mix, mix4, pcg, smoothstep,
};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        "alpha2.effect.vignette" => &Vignette,
        "alpha2.effect.film-grain" => &FilmGrain,
        "alpha2.effect.rgb-split" => &RgbSplit,
        "alpha2.effect.glitch" => &Glitch,
        "alpha2.effect.crt" => &Crt,
        "alpha2.effect.vhs" => &Vhs,
        "alpha2.effect.halftone" => &Halftone,
        "alpha2.effect.emboss" => &Emboss,
        "alpha2.effect.edge-detect" => &EdgeDetect,
        "alpha2.effect.outline" => &Outline,
        "alpha2.effect.light-leak" => &LightLeak,
        "alpha2.effect.letterbox" => &Letterbox,
        "alpha2.effect.oil-paint" => &OilPaint,
        _ => return None,
    })
}

/// WGSL `u32(clamp(floor(value), 0.0, 4294967040.0))`: the largest f32
/// below 2^32, not `u32::MAX`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole(value: f32) -> u32 {
    value.floor().clamp(0.0, 4_294_967_040.0) as u32
}

/// vignette.wgsl.
struct Vignette;

impl CpuKernel for Vignette {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let radius = context.float("radius");
        let softness = context.float("softness");
        effect_row(context, y, x0, out, |_, uv, src| {
            let (dx, dy) = (uv[0] - 0.5, uv[1] - 0.5);
            // The shader's 1.41421356 is this f32.
            let distance = (dx * dx + dy * dy).sqrt() * std::f32::consts::SQRT_2;
            let shade = 1.0 - amount * smoothstep(radius - softness, radius + softness, distance);
            [src[0] * shade, src[1] * shade, src[2] * shade, src[3]]
        });
    }
}

/// film-grain.wgsl: PCG noise per cell and per exact time, in integers.
struct FilmGrain;

impl CpuKernel for FilmGrain {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        // WGSL `round` takes ties to even.
        let size = (context.float("size").max(0.5) * 256.0).round_ties_even() as u32;
        let ticks = context.time_ticks();
        let seed = pcg(ticks[0] ^ pcg(ticks[1]));
        let resolution = context.resolution();
        effect_row(context, y, x0, out, |_, uv, src| {
            if src[3] <= 0.0001 {
                return [0.0; 4];
            }
            let pixel = [
                (uv[0] * resolution[0]).floor().max(0.0) as u32,
                (uv[1] * resolution[1]).floor().max(0.0) as u32,
            ];
            let cell = pixel.map(|value| value.wrapping_mul(256).wrapping_add(128) / size);
            let hash = pcg(cell[0] ^ pcg(cell[1] ^ seed));
            let noise = (hash >> 8) as f32 / 16_777_216.0 - 0.5;
            let grain = noise * amount;
            let rgb = [0, 1, 2].map(|c| saturate(src[c] / src[3] + grain));
            premultiplied(rgb, src[3])
        });
    }
}

/// rgb-split.wgsl: red from one side, blue from the other.
struct RgbSplit;

impl RgbSplit {
    /// The linear mode's shift in uv.
    fn along(context: &KernelContext<'_>) -> [f32; 2] {
        let angle = context.float("angle").to_radians();
        let amount = context.float("amount");
        [
            libm::cosf(angle) * amount / aspect(context)[0],
            -libm::sinf(angle) * amount,
        ]
    }
}

impl CpuKernel for RgbSplit {
    // A channel comes from at most the shift away: in the radial mode half
    // the picture times four times the amount.
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let size = context.size;
        let extent = [size.width as f32, size.height as f32];
        let reach = if context.choice("mode") == 1 {
            let amount = context.float("amount") * 2.0;
            [amount, amount]
        } else {
            Self::along(context)
        };
        grow(
            context.source_a.bounds(),
            margin(reach[0] * extent[0]),
            margin(reach[1] * extent[1]),
            size,
        )
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let radial = context.choice("mode") == 1;
        let amount = context.float("amount");
        let along = Self::along(context);
        effect_row(context, y, x0, out, |_, uv, src| {
            let shift = if radial {
                [(uv[0] - 0.5) * amount * 4.0, (uv[1] - 0.5) * amount * 4.0]
            } else {
                along
            };
            let red = context.sample_src([uv[0] + shift[0], uv[1] + shift[1]]);
            let blue = context.sample_src([uv[0] - shift[0], uv[1] - shift[1]]);
            let alpha = src[3].max(red[3].max(blue[3]));
            [red[0], src[1], blue[2], alpha]
        });
    }
}

/// glitch.wgsl: rows of blocks jumping sideways, a new pattern `speed`
/// times a second.
struct Glitch;

impl CpuKernel for Glitch {
    // Rows move at most a tenth of the width times the amount, and the
    // channels split a further `color_split`.
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let size = context.size;
        let reach = 0.1 * context.float("amount").abs() * size.width as f32
            + context.float("color_split").abs() * size.height as f32;
        grow(context.source_a.bounds(), margin(reach), margin(0.0), size)
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let block_size = context.float("block_size");
        let color_split = context.float("color_split");
        let frame = whole(context.time() * context.float("speed"));
        let seed = seed(context.float("seed"));
        let width = aspect(context)[0];
        effect_row(context, y, x0, out, |_, uv, _| {
            let row = whole(uv[1] / block_size);
            let h = builtin_hash3(row, frame, seed);
            let jumping = if builtin_unit(h) < amount * 0.5 {
                1.0
            } else {
                0.0
            };
            let shift = (builtin_unit(builtin_hash(h)) - 0.5) * 0.2 * amount * jumping;
            let p = [uv[0] + shift, uv[1]];
            let split = color_split * jumping / width;
            let base = context.sample_src(p);
            let red = context.sample_src([p[0] + split, p[1]]);
            let blue = context.sample_src([p[0] - split, p[1]]);
            [red[0], base[1], blue[2], base[3].max(red[3].max(blue[3]))]
        });
    }
}

/// crt.wgsl: a curved screen with scanlines, an aperture mask one output
/// pixel wide and darkened corners.
struct Crt;

/// crt.wgsl's `p`: `uv` on the curved screen.
fn crt_bend(uv: [f32; 2], curvature: f32) -> [f32; 2] {
    let centered = [uv[0] * 2.0 - 1.0, uv[1] * 2.0 - 1.0];
    let r2 = centered[0] * centered[0] + centered[1] * centered[1];
    let bend = 1.0 + curvature * r2 * 0.25;
    [
        centered[0] * bend * 0.5 + 0.5,
        centered[1] * bend * 0.5 + 0.5,
    ]
}

impl CpuKernel for Crt {
    // The curve pulls the picture towards the centre, out of the source's
    // pixels: the whole picture.

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let curvature = context.float("curvature");
        let scanlines = context.float("scanlines");
        let mask = context.float("mask");
        let vignette = context.float("vignette");
        let resolution = context.resolution();
        let lines = context.float("line_count").min(resolution[1] * 0.5);
        let size = context.size;
        effect_row(context, y, x0, out, |x, uv, _| {
            let centered = [uv[0] * 2.0 - 1.0, uv[1] * 2.0 - 1.0];
            let r2 = centered[0] * centered[0] + centered[1] * centered[1];
            let p = crt_bend(uv, curvature);
            let width = uv_width(size, x, y, |uv| crt_bend(uv, curvature));
            let picture = sample_edge(context, p, width, 0);
            let scan = 1.0 - scanlines * (0.5 - 0.5 * libm::cosf(p[1] * lines * TAU));
            let column = (uv[0] * resolution[0]).floor() as u32 % 3;
            let dim = 1.0 - mask;
            let shade = 1.0 - vignette * r2 * 0.5;
            let rgb = straight(picture, 0.0001);
            let rgb = [0, 1, 2].map(|c| {
                let phosphor = if column == c { 1.0 } else { dim };
                rgb[c as usize] * scan * phosphor * shade
            });
            output(rgb, picture[3])
        });
    }
}

/// vhs.wgsl: wobbling rows, a rolling tracking band, chroma bleeding to
/// the right and grain, the bleed and the grain in output pixels.
struct Vhs;

impl CpuKernel for Vhs {
    // Rows move at most `(0.002 + 0.015) * amount` of the width; the alpha
    // is the moved pixel's.
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let size = context.size;
        let reach = 0.017 * context.float("amount").abs() * size.width as f32;
        grow(context.source_a.bounds(), margin(reach), margin(0.0), size)
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let noise = context.float("noise");
        let bleed = context.float("bleed");
        let seed = seed(context.float("seed"));
        let t = context.time() * context.float("speed");
        let resolution = context.resolution();
        let band_center = fract(t * 0.1);
        let texel = 1.0 / resolution[0];
        let frame = builtin_hash(whole(t * 30.0)) ^ seed;
        effect_row(context, y, x0, out, |_, uv, _| {
            let wobble =
                (builtin_value_noise([uv[1] * 40.0, t * 3.0], seed) - 0.5) * 0.004 * amount;
            let band = 1.0 - builtin_smoothstep(0.0, 0.04, (uv[1] - band_center).abs());
            let tear = (builtin_value_noise([uv[1] * 400.0, t * 30.0], seed.wrapping_add(1)) - 0.5)
                * 0.03
                * band
                * amount;
            let p = [uv[0] + wobble + tear, uv[1]];
            let c0 = context.sample_src(p);
            let s2 = context.sample_src([p[0] - texel * 2.0, p[1]]);
            let s4 = context.sample_src([p[0] - texel * 4.0, p[1]]);
            let s6 = context.sample_src([p[0] - texel * 6.0, p[1]]);
            let smear: [f32; 4] = std::array::from_fn(|c| (s2[c] + s4[c] + s6[c]) / 3.0);
            let rgb0 = straight(c0, 0.0001);
            let rgb1 = straight(smear, 0.0001);
            let luma = luminance(rgb0);
            let luma1 = luminance(rgb1);
            let pixel = [whole(uv[0] * resolution[0]), whole(uv[1] * resolution[1])];
            let grain = builtin_unit(builtin_hash3(pixel[0], pixel[1], frame)) - 0.5;
            let lift = grain * noise + band * 0.15 * amount;
            let rgb = [0, 1, 2].map(|c| {
                let chroma = mix(rgb0[c] - luma, rgb1[c] - luma1, bleed);
                luma + chroma + lift
            });
            output(rgb, c0[3])
        });
    }
}

/// halftone.wgsl: a turned grid of dots, each sized by its cell's centre.
struct Halftone;

/// Halftone's grid: the cell side in heights and the turn's cosine and
/// sine.
struct Grid {
    aspect: [f32; 2],
    size: f32,
    cos: f32,
    sin: f32,
}

impl Grid {
    fn new(context: &KernelContext<'_>) -> Self {
        let aspect = aspect(context);
        let angle = context.float("angle").to_radians();
        Self {
            aspect,
            size: aspect[0] / context.float("cells"),
            cos: libm::cosf(angle),
            sin: libm::sinf(angle),
        }
    }

    /// `p`: the position in cells, and its cell's centre.
    fn cell(&self, uv: [f32; 2]) -> ([f32; 2], [f32; 2]) {
        let v = [
            (uv[0] - 0.5) * self.aspect[0],
            (uv[1] - 0.5) * self.aspect[1],
        ];
        let p = [
            (self.cos * v[0] - self.sin * v[1]) / self.size,
            (self.sin * v[0] + self.cos * v[1]) / self.size,
        ];
        (p, [p[0].floor() + 0.5, p[1].floor() + 0.5])
    }

    fn distance(&self, uv: [f32; 2]) -> f32 {
        let (p, cell) = self.cell(uv);
        let (dx, dy) = (p[0] - cell[0], p[1] - cell[1]);
        (dx * dx + dy * dy).sqrt()
    }
}

impl CpuKernel for Halftone {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    // The shader's 0.7071 literal, not FRAC_1_SQRT_2.
    #[allow(clippy::approx_constant)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let grid = Grid::new(context);
        let color = context.choice("mode") == 1;
        let size = context.size;
        effect_row(context, y, x0, out, |x, uv, src| {
            let (p, cell) = grid.cell(uv);
            // `builtin_rotate(cell * size, -angle)`: sin(-a) is -sin(a).
            let v = [cell[0] * grid.size, cell[1] * grid.size];
            let center_uv = [
                (grid.cos * v[0] - -grid.sin * v[1]) / grid.aspect[0] + 0.5,
                (-grid.sin * v[0] + grid.cos * v[1]) / grid.aspect[1] + 0.5,
            ];
            let (dx, dy) = (p[0] - cell[0], p[1] - cell[1]);
            let from_center = (dx * dx + dy * dy).sqrt();
            let width = fwidth_fine(size, x, y, |uv| grid.distance(uv)).max(0.000_000_1);
            let rgb = straight(context.sample_src(center_uv), 0.0001);
            let dots = if color {
                rgb.map(|value| saturate(0.5 - (from_center - value.sqrt() * 0.7071) / width))
            } else {
                let radius = (1.0 - gray(rgb)).sqrt() * 0.7071;
                let ink = 1.0 - saturate(0.5 + (from_center - radius) / width);
                [1.0 - ink; 3]
            };
            output(dots, src[3])
        });
    }
}

/// emboss.wgsl: relief from the luminance ahead minus behind.
struct Emboss;

impl CpuKernel for Emboss {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let angle = context.float("angle").to_radians();
        let distance = context.float("distance");
        let strength = context.float("strength");
        let keep_color = context.boolean("keep_color");
        let offset = [
            libm::cosf(angle) * distance / aspect(context)[0],
            -libm::sinf(angle) * distance,
        ];
        effect_row(context, y, x0, out, |_, uv, src| {
            let ahead = context.sample_src([uv[0] + offset[0], uv[1] + offset[1]]);
            let behind = context.sample_src([uv[0] - offset[0], uv[1] - offset[1]]);
            let relief = strength
                * (luminance([ahead[0], ahead[1], ahead[2]])
                    - luminance([behind[0], behind[1], behind[2]]));
            let rgb = if keep_color {
                straight(src, 0.0001).map(|value| value + relief)
            } else {
                [0.5 + relief; 3]
            };
            output(rgb, src[3])
        });
    }
}

/// edge-detect.wgsl: Sobel gradients of the premultiplied color.
struct EdgeDetect;

impl CpuKernel for EdgeDetect {
    // Inverted, it covers the whole picture; else the source's alpha, and
    // so its pixels.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        if context.boolean("invert") {
            PixelBox::whole(context.size)
        } else {
            context.source_a.bounds()
        }
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let reach = context.float("width");
        let d = [reach / aspect(context)[0], reach];
        let strength = context.float("strength");
        let invert = context.boolean("invert");
        let colored = context.boolean("colored");
        effect_row(context, y, x0, out, |_, uv, src| {
            let at = |dx: f32, dy: f32| context.sample_src([uv[0] + dx, uv[1] + dy]);
            let tl = at(-d[0], -d[1]);
            let tc = at(0.0, -d[1]);
            let tr = at(d[0], -d[1]);
            let ml = at(-d[0], 0.0);
            let mr = at(d[0], 0.0);
            let bl = at(-d[0], d[1]);
            let bc = at(0.0, d[1]);
            let br = at(d[0], d[1]);
            let magnitude = [0, 1, 2].map(|c| {
                let gx = (tr[c] + 2.0 * mr[c] + br[c]) - (tl[c] + 2.0 * ml[c] + bl[c]);
                let gy = (bl[c] + 2.0 * bc[c] + br[c]) - (tl[c] + 2.0 * tc[c] + tr[c]);
                (gx * gx + gy * gy).sqrt() * strength
            });
            let edge = if colored {
                magnitude
            } else {
                [luminance(magnitude); 3]
            };
            if invert {
                output(edge.map(|value| 1.0 - value), 1.0)
            } else {
                output(edge, src[3])
            }
        });
    }
}

/// outline.wgsl: an outer stroke from two rings of taps.
struct Outline;

impl CpuKernel for Outline {
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let reach = margin(context.float("width") * context.size.height as f32);
        grow(context.source_a.bounds(), reach, reach, context.size)
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let taps = context.max_filter_samples().clamp(8.0, 64.0) as u32;
        let width = context.float("width");
        let reach = [width / aspect(context)[0], width];
        let step = TAU / taps as f32;
        // Each tap's offset in uv, the same for every pixel.
        let offsets: Vec<[f32; 2]> = (0..taps)
            .map(|i| {
                let angle = i as f32 * step;
                let ring = if i & 1 == 1 { 0.5 } else { 1.0 };
                [
                    libm::cosf(angle) * reach[0] * ring,
                    libm::sinf(angle) * reach[1] * ring,
                ]
            })
            .collect();
        let color = context.vec4("color");
        let stroke = [
            color[0] * color[3],
            color[1] * color[3],
            color[2] * color[3],
            color[3],
        ];
        effect_row(context, y, x0, out, |_, uv, src| {
            let mut cover = src[3];
            for offset in &offsets {
                cover = cover.max(context.sample_src([uv[0] + offset[0], uv[1] + offset[1]])[3]);
            }
            let rest = 1.0 - src[3];
            std::array::from_fn(|c| src[c] + stroke[c] * cover * rest)
        });
    }
}

/// light-leak.wgsl: two drifting blobs of light screened over the picture.
struct LightLeak;

impl CpuKernel for LightLeak {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let seed = seed(context.float("seed"));
        let t = context.time() * context.float("speed");
        let aspect = aspect(context);
        let color = context.vec4("color");
        let intensity = context.float("intensity");
        let position = context.vec2("position");
        let size = context.float("size");
        let drift_a = [
            builtin_value_noise([t, 0.0], seed) - 0.5,
            builtin_value_noise([t, 7.0], seed) - 0.5,
        ];
        let drift_b = [
            builtin_value_noise([t, 13.0], seed) - 0.5,
            builtin_value_noise([t, 19.0], seed) - 0.5,
        ];
        let s2 = size * size;
        effect_row(context, y, x0, out, |_, uv, src| {
            let a = [
                (uv[0] - position[0] - drift_a[0] * 0.3) * aspect[0],
                (uv[1] - position[1] - drift_a[1] * 0.3) * aspect[1],
            ];
            let b = [
                (uv[0] - position[0] - 0.25 - drift_b[0] * 0.4) * aspect[0],
                (uv[1] - position[1] - 0.1 - drift_b[1] * 0.4) * aspect[1],
            ];
            let glow = libm::expf(-(a[0] * a[0] + a[1] * a[1]) / s2)
                + 0.6 * libm::expf(-(b[0] * b[0] + b[1] * b[1]) / (s2 * 0.5));
            let rgb = straight(src, 0.0001);
            let screened = [0, 1, 2].map(|c| {
                let leak = saturate(color[c] * glow * intensity * color[3]);
                1.0 - (1.0 - rgb[c]) * (1.0 - leak)
            });
            output(screened, src[3])
        });
    }
}

/// letterbox.wgsl: opaque bars around a window of `aspect`.
struct Letterbox;

impl Letterbox {
    /// The window's low corner and size in uv.
    fn window(context: &KernelContext<'_>) -> ([f32; 2], [f32; 2]) {
        let resolution = context.resolution();
        let canvas = resolution[0] / resolution[1];
        let aspect = context.float("aspect");
        let window = if aspect < canvas {
            [aspect / canvas, 1.0]
        } else {
            [1.0, canvas / aspect]
        };
        ([(1.0 - window[0]) * 0.5, (1.0 - window[1]) * 0.5], window)
    }
}

impl CpuKernel for Letterbox {
    // The bars are opaque over transparency too: the whole picture.

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let (low, window) = Self::window(context);
        let color = context.vec4("color");
        let bar = [
            color[0] * color[3],
            color[1] * color[3],
            color[2] * color[3],
            color[3],
        ];
        let size = context.size;
        let place = |uv: [f32; 2]| [(uv[0] - low[0]) / window[0], (uv[1] - low[1]) / window[1]];
        effect_row(context, y, x0, out, |x, uv, src| {
            let p = place(uv);
            let width = uv_width(size, x, y, place);
            let inside = builtin_interval_coverage(p[0], width[0])
                * builtin_interval_coverage(p[1], width[1]);
            mix4(bar, src, inside)
        });
    }
}

/// oil-paint.wgsl: Kuwahara's filter, the quadrants' means weighted by the
/// inverse fourth power of their variance.
struct OilPaint;

impl CpuKernel for OilPaint {
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let reach = margin(context.float("radius") * context.size.height as f32);
        grow(context.source_a.bounds(), reach, reach, context.size)
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let side = (context.max_filter_samples().clamp(4.0, 64.0) * 0.25)
            .sqrt()
            .floor()
            .max(1.0);
        let radius = context.float("radius");
        let reach = [radius / aspect(context)[0], radius];
        let count = side as u32;
        // Each quadrant's tap offsets in uv, the same for every pixel.
        let quadrants: Vec<Vec<[f32; 2]>> = (0..4_u32)
            .map(|q| {
                let sign = [
                    if q & 1 == 1 { 1.0 } else { -1.0 },
                    if q & 2 == 2 { 1.0 } else { -1.0 },
                ];
                (0..count)
                    .flat_map(|y| (0..count).map(move |x| (x, y)))
                    .map(|(x, y)| {
                        let o = [
                            (x as f32 + 0.5) / side * sign[0],
                            (y as f32 + 0.5) / side * sign[1],
                        ];
                        [o[0] * reach[0], o[1] * reach[1]]
                    })
                    .collect()
            })
            .collect();
        let n = side * side;
        effect_row(context, y, x0, out, |_, uv, _| {
            let mut weighted = [0.0_f32; 4];
            let mut total = 0.0_f32;
            for taps in &quadrants {
                let mut sum = [0.0_f32; 4];
                let mut squares = [0.0_f32; 3];
                for offset in taps {
                    let s = context.sample_src([uv[0] + offset[0], uv[1] + offset[1]]);
                    for c in 0..4 {
                        sum[c] += s[c];
                    }
                    for c in 0..3 {
                        squares[c] += s[c] * s[c];
                    }
                }
                let mean = sum.map(|value| value / n);
                let spread: [f32; 3] = std::array::from_fn(|c| squares[c] / n - mean[c] * mean[c]);
                let variance = (spread[0] + spread[1] + spread[2]).max(0.0) + 1.0e-3;
                let quartic = (variance * variance) * (variance * variance);
                let weight = 1.0 / quartic;
                for c in 0..4 {
                    weighted[c] += mean[c] * weight;
                }
                total += weight;
            }
            weighted.map(|value| value / total)
        });
    }
}
