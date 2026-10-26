//! The public built-in effects (builtins.rs `builtin_effect_definitions`),
//! ported from their bodies in `crates/alpha2-shaders/shaders/runtime/effects`.

use super::{CpuKernel, CpuLut, KernelContext, mix, pixel_uv, smoothstep};
use crate::RenderSize;
use crate::builtins::{CHROMA_KEY_ID, GAUSSIAN_BLUR_ID, GLOW_ID, LUT_3D_ID};
use crate::cpu::curves;
use crate::cpu::image::{CpuImage, PixelBox};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        CHROMA_KEY_ID => &ChromaKey,
        LUT_3D_ID => &Lut3d,
        GAUSSIAN_BLUR_ID | GLOW_ID => &Expanded,
        "alpha2.effect.color-controls" => &ColorControls,
        "alpha2.effect.vignette" => &Vignette,
        "alpha2.effect.sharpen" => &Sharpen,
        "alpha2.effect.pixelate" => &Pixelate,
        "alpha2.effect.film-grain" => &FilmGrain,
        "alpha2.effect.crop" => &Crop,
        "alpha2.effect.mirror" => &Mirror,
        _ => return None,
    })
}

/// `sample_src(uv)` at the centre of pixel `(x, y)` of `image`: the texel
/// itself when the picture is the output's size, as a GPU sampler reads it
/// there (its fixed-point coordinates land on the centre exactly).
#[inline]
pub(super) fn centre(image: &CpuImage, size: RenderSize, x: u32, y: u32) -> [f32; 4] {
    if image.size() == size {
        image.texel(x, y)
    } else {
        let uv = pixel_uv(size, x, y);
        image.sample(uv[0], uv[1])
    }
}

/// Fills `out` with `effect(x, uv, src)` for the pixels `x0..` of row `y`,
/// `src` the source at the pixel, as the effect wrapper passes it.
#[inline]
pub(super) fn effect_row(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
    effect: impl Fn(u32, [f32; 2], [f32; 4]) -> [f32; 4],
) {
    for (offset, pixel) in out.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let x = x0 + offset as u32;
        let src = centre(context.source_a, context.size, x, y);
        *pixel = effect(x, pixel_uv(context.size, x, y), src);
    }
}

/// `box` grown by `x` columns and `y` rows on each side, kept inside `size`.
pub(super) fn grow(region: PixelBox, x: u32, y: u32, size: RenderSize) -> PixelBox {
    if region.is_empty() {
        return region;
    }
    let left = region.x.saturating_sub(x);
    let top = region.y.saturating_sub(y);
    let right = region.right().saturating_add(x).min(size.width);
    let bottom = region.bottom().saturating_add(y).min(size.height);
    PixelBox {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
}

/// A margin of `pixels` (non-negative) rounded up, one more for the
/// bilinear neighbour and one for rounding.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn margin(pixels: f32) -> u32 {
    if pixels.is_finite() {
        (pixels.abs().ceil().min(1.0e9) as u32).saturating_add(2)
    } else {
        u32::MAX
    }
}

/// WGSL `clamp(value, 0.0, 1.0)` as GPUs evaluate it, `min(max(..))`: NaN
/// becomes 0.
#[inline]
#[allow(clippy::manual_clamp)]
pub(super) fn saturate(value: f32) -> f32 {
    value.max(0.0).min(1.0)
}

/// `dot(rgb, vec3f(0.2126, 0.7152, 0.0722))`.
#[inline]
pub(super) fn luminance(rgb: [f32; 3]) -> f32 {
    rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722
}

/// The straight color of `color`, black below `floor` of alpha (each body
/// has its own floor).
#[inline]
pub(super) fn straight(color: [f32; 4], floor: f32) -> [f32; 3] {
    if color[3] <= floor {
        return [0.0; 3];
    }
    [
        color[0] / color[3],
        color[1] / color[3],
        color[2] / color[3],
    ]
}

/// `rgb * alpha, alpha`.
#[inline]
pub(super) fn premultiplied(rgb: [f32; 3], alpha: f32) -> [f32; 4] {
    [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
}

/// The sRGB EOTF (`builtin_decode`), through the shared table on `0..1`;
/// a straight color past one (half-float storage rounds alpha down) takes
/// the curve itself.
#[inline]
pub(super) fn decode(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        return encoded / 12.92;
    }
    if encoded < 1.0 {
        return curves::srgb_to_linear(encoded);
    }
    libm::powf((encoded + 0.055) / 1.055, 2.4)
}

/// The effects drawn as internal passes (blur, glow): the graph never draws
/// their own body, which returns the source. The kernel is here so the CPU
/// catalog lists them as project effects.
struct Expanded;

impl CpuKernel for Expanded {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        effect_row(context, y, x0, out, |_, _, src| src);
    }
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

/// `sample_<lut>(rgb)` as the registry generates it: the domain mapped to
/// texel centres (the half-texel inset), then a trilinear sample with the
/// edge repeated.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(super) fn sample_lut(lut: &CpuLut, rgb: [f32; 3]) -> [f32; 3] {
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

/// Entries per octave of the `pow` tables, and the fraction bits of an f32
/// mantissa below one entry.
const POW_STEPS: usize = 1024;
const POW_SHIFT: u32 = 13;

/// `log2` over one octave's mantissa and `exp2` over `[0, 1]`, built once
/// with `libm` and read with linear interpolation: a `pow` that is the same
/// on every target and needs no library call per pixel. Its error, a few
/// millionths relative, is far under a half float's step.
struct PowTables {
    log2: Vec<f32>,
    exp2: Vec<f32>,
}

#[allow(clippy::cast_precision_loss)]
fn pow_tables() -> &'static PowTables {
    static TABLES: std::sync::OnceLock<PowTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| PowTables {
        log2: (0..=POW_STEPS)
            .map(|step| libm::log2f(1.0 + step as f32 / POW_STEPS as f32))
            .collect(),
        exp2: (0..=POW_STEPS)
            .map(|step| libm::exp2f(step as f32 / POW_STEPS as f32))
            .collect(),
    })
}

/// `pow(base, exponent)` for a normal, positive `base`, as
/// `exp2(exponent * log2(base))`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn table_pow(tables: &PowTables, base: f32, exponent: f32) -> f32 {
    let bits = base.to_bits();
    let octave = ((bits >> 23) & 0xff) as i32 - 127;
    let mantissa = bits & 0x007f_ffff;
    let index = (mantissa >> POW_SHIFT) as usize;
    let fraction = (mantissa & ((1 << POW_SHIFT) - 1)) as f32 * (1.0 / (1 << POW_SHIFT) as f32);
    let low = tables.log2[index];
    let log2 = octave as f32 + (low + (tables.log2[index + 1] - low) * fraction);
    let power = exponent * log2;
    if power.is_nan() || power <= -126.0 {
        return 0.0;
    }
    if power >= 128.0 {
        return f32::INFINITY;
    }
    let whole = power.floor();
    let position = (power - whole) * POW_STEPS as f32;
    let index = (position as usize).min(POW_STEPS - 1);
    let fraction = position - index as f32;
    let low = tables.exp2[index];
    let scale = f32::from_bits(((whole as i32 + 127) as u32) << 23);
    (low + (tables.exp2[index + 1] - low) * fraction) * scale
}

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

/// pixelate.wgsl: each cell shows the sample at its centre.
struct Pixelate;

impl Pixelate {
    fn cells(context: &KernelContext<'_>) -> [f32; 2] {
        let resolution = context.resolution();
        let aspect = resolution[0] / resolution[1];
        let blocks = context.float("blocks");
        [blocks.max(2.0), (blocks / aspect).max(2.0)]
    }
}

impl CpuKernel for Pixelate {
    // A pixel shows its cell's centre, at most half a cell away.
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let cells = Self::cells(context);
        let size = context.size;
        grow(
            context.source_a.bounds(),
            margin(size.width as f32 / cells[0] * 0.5),
            margin(size.height as f32 / cells[1] * 0.5),
            size,
        )
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let cells = Self::cells(context);
        super::shade_row(context, y, x0, out, |_, uv| {
            context.sample_src([
                ((uv[0] * cells[0]).floor() + 0.5) / cells[0],
                ((uv[1] * cells[1]).floor() + 0.5) / cells[1],
            ])
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
        use super::pcg;
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

/// crop.wgsl: each cut line feathered on both sides.
struct Crop;

impl Crop {
    /// `feather`, the low corner and the high corner, in uv.
    fn lines(context: &KernelContext<'_>) -> (f32, [f32; 2], [f32; 2]) {
        let feather = context.float("feather").max(0.0001);
        let low = context.vec2("top_left");
        let bottom_right = context.vec2("bottom_right");
        (feather, low, [1.0 - bottom_right[0], 1.0 - bottom_right[1]])
    }
}

/// `builtin_crop_edge`.
#[inline]
fn crop_edge(position: f32, line: f32, feather: f32, cut: bool) -> f32 {
    if !cut {
        return 1.0;
    }
    smoothstep(line - feather, line + feather, position)
}

impl CpuKernel for Crop {
    // The source's pixels, less what lies past a cut line's feather.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let (feather, low, high) = Self::lines(context);
        if high[0] <= low[0] || high[1] <= low[1] {
            return PixelBox::EMPTY;
        }
        let size = context.size;
        let extent = [size.width as f32, size.height as f32];
        let mut start = [0_u32; 2];
        let mut end = [size.width, size.height];
        for axis in 0..2 {
            // A pixel centre at or before `line - feather` keeps nothing; one
            // pixel of slack on either side covers rounding.
            if low[axis] > 0.0 {
                let first = ((low[axis] - feather) * extent[axis] - 0.5).floor() - 1.0;
                start[axis] = first.max(0.0).min(extent[axis]) as u32;
            }
            if high[axis] < 1.0 {
                let last = ((high[axis] + feather) * extent[axis] + 0.5).ceil() + 1.0;
                end[axis] = last.max(0.0).min(extent[axis]) as u32;
            }
        }
        if end[0] <= start[0] || end[1] <= start[1] {
            return PixelBox::EMPTY;
        }
        context.source_a.bounds().intersect(PixelBox {
            x: start[0],
            y: start[1],
            width: end[0] - start[0],
            height: end[1] - start[1],
        })
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let (feather, low, high) = Self::lines(context);
        if high[0] <= low[0] || high[1] <= low[1] {
            out.fill([0.0; 4]);
            return;
        }
        effect_row(context, y, x0, out, |_, uv, src| {
            let inside = crop_edge(uv[0], low[0], feather, low[0] > 0.0)
                * crop_edge(-uv[0], -high[0], feather, high[0] < 1.0)
                * crop_edge(uv[1], low[1], feather, low[1] > 0.0)
                * crop_edge(-uv[1], -high[1], feather, high[1] < 1.0);
            src.map(|value| value * inside)
        });
    }
}

/// mirror.wgsl: one half of the picture reflected onto the other.
struct Mirror;

impl CpuKernel for Mirror {
    // What it shows is the source or its reflection, read bilinearly.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let size = context.size;
        let source = context.source_a.bounds();
        if source.is_empty() {
            return source;
        }
        let mut reflected = source;
        if context.choice("axis") == 0 {
            reflected.x = size.width - source.right();
        } else {
            reflected.y = size.height - source.bottom();
        }
        source.union(reflected).expand(2, size)
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let keep_first = !context.boolean("keep_second_half");
        let axis = usize::from(context.choice("axis") != 0);
        super::shade_row(context, y, x0, out, |_, uv| {
            let mut position = uv;
            let value = position[axis];
            position[axis] = if keep_first {
                value.min(1.0 - value)
            } else {
                value.max(1.0 - value)
            };
            context.sample_src(position)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{pow_tables, table_pow};

    // Far under a half float's step (about 5e-4 relative); the bound is
    // the f32 rounding of `exponent * log2(base)` at the smallest bases.
    #[test]
    fn the_table_pow_stays_far_below_a_half_float_step() {
        let tables = pow_tables();
        let mut worst = 0.0_f64;
        for exponent in [0.0_f32, 0.05, 0.3, 0.77, 1.0, 1.5, 2.0] {
            let mut base = 1.0e-10_f32;
            while base < 64.0 {
                let expected = f64::from(base).powf(f64::from(exponent));
                let got = f64::from(table_pow(tables, base, exponent));
                worst = worst.max((got - expected).abs() / expected);
                base *= 1.000_37;
            }
        }
        assert!(worst < 1.0e-5, "relative error {worst}");
    }
}
