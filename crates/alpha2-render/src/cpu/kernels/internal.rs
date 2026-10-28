//! The internal passes: the blur and glow passes, shape-mask combines and the track matte.

use super::effects::{
    centre, effect_row, grow, luminance, margin, output, premultiplied, saturate, straight,
};
use super::{CpuKernel, KernelContext, builtin_smoothstep, mix4, transition_row};
use crate::builtins::{
    FOCUS_COMBINE_ID, GAUSSIAN_BLUR_PASS_ID, GLOW_COMBINE_ID, GLOW_THRESHOLD_ID, SHADOW_COMBINE_ID,
    SHADOW_OFFSET_ID, UNSHARP_COMBINE_ID,
};
use crate::cpu::curves;
use crate::cpu::half;
use crate::cpu::image::{CpuImage, PixelBox, Pixels};
use crate::masking::{
    MASK_COMBINE_EFFECT_IDS, MASK_COMBINE_NAMES, MAX_SHAPE_MASKS, TRACK_MATTE_EFFECT_ID,
};

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        GAUSSIAN_BLUR_PASS_ID => &BlurPass,
        GLOW_THRESHOLD_ID => &GlowThreshold,
        GLOW_COMBINE_ID => &GlowCombine,
        UNSHARP_COMBINE_ID => &UnsharpCombine,
        FOCUS_COMBINE_ID => &FocusCombine,
        SHADOW_OFFSET_ID => &ShadowOffset,
        SHADOW_COMBINE_ID => &ShadowCombine,
        TRACK_MATTE_EFFECT_ID => &TrackMatte,
        _ if MASK_COMBINE_EFFECT_IDS.contains(&id) => &MaskCombine,
        _ => return None,
    })
}

/// Taps a blur pass reads on either side at most (gaussian-blur.wgsl).
const PASS_REACH: i32 = 12;

/// One blur pass's taps: its reach and the Gaussian's weights, by tap
/// index plus [`PASS_REACH`].
struct Taps {
    reach: i32,
    weights: [f32; 2 * PASS_REACH as usize + 1],
    sum: f32,
}

impl Taps {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn new(context: &KernelContext<'_>) -> Self {
        let sigma = context.float("sigma").max(0.0001);
        let budget = ((context.max_filter_samples() * 0.5).floor() as i32).max(1);
        let reach = ((sigma * 3.0).ceil() as i32).min(PASS_REACH.min(budget));
        let mut weights = [0.0; 2 * PASS_REACH as usize + 1];
        let mut sum = 0.0;
        // Summed in the shader's order, from -12 up.
        for index in -reach..=reach {
            let position = index as f32 / sigma;
            let weight = libm::expf(-0.5 * position * position);
            weights[(index + PASS_REACH) as usize] = weight;
            sum += weight;
        }
        Self {
            reach,
            weights,
            sum,
        }
    }

    fn weight(&self, index: i32) -> f32 {
        #[allow(clippy::cast_sign_loss)]
        self.weights[(index + PASS_REACH) as usize]
    }
}

/// One row of a picture, for reading texels by column.
#[derive(Copy, Clone)]
enum Row<'a> {
    /// Outside the content: transparent.
    Empty,
    Half(u32, &'a [[u16; 4]]),
    Unorm8(u32, &'a [[u8; 4]]),
}

impl<'a> Row<'a> {
    /// Row `y` (inside the picture) of `image`.
    fn of(image: &'a CpuImage, y: u32) -> Self {
        let bounds = image.bounds();
        if y < bounds.y || y >= bounds.bottom() {
            return Self::Empty;
        }
        let width = bounds.width as usize;
        let start = (y - bounds.y) as usize * width;
        match image.pixels() {
            Pixels::Half(pixels) => Self::Half(bounds.x, &pixels[start..start + width]),
            Pixels::Unorm8(pixels) => Self::Unorm8(bounds.x, &pixels[start..start + width]),
        }
    }

    /// The texels of columns `column..column + out.len()`, transparent
    /// outside the content.
    fn decode(self, column: u32, out: &mut [[f32; 4]]) {
        out.fill([0.0; 4]);
        let (first, length) = match self {
            Self::Empty => return,
            Self::Half(first, pixels) => (first, pixels.len()),
            Self::Unorm8(first, pixels) => (first, pixels.len()),
        };
        // The content's columns within the requested ones.
        let from = first.max(column) as usize;
        let to = (first as usize + length).min(column as usize + out.len());
        if from >= to {
            return;
        }
        let out = &mut out[from - column as usize..to - column as usize];
        let start = from - first as usize;
        match self {
            Self::Empty => {}
            Self::Half(_, pixels) => {
                // The `half` crate converts a slice with F16C where the CPU
                // has it; the conversion is exact either way.
                use ::half::slice::{HalfBitsSliceExt as _, HalfFloatSliceExt as _};
                let pixels: &[::half::f16] = pixels[start..start + out.len()]
                    .as_flattened()
                    .reinterpret_cast();
                pixels.convert_to_f32_slice(out.as_flattened_mut());
            }
            Self::Unorm8(_, pixels) => {
                for (out, pixel) in out.iter_mut().zip(&pixels[start..]) {
                    *out = pixel.map(half::unorm8_value);
                }
            }
        }
    }
}

/// Where tap `index` lands, `index * spacing` texels from the pixel's
/// centre: the whole texels before it and the weight of the next texel.
/// A tap more than `extent` texels away reads the edge as one `extent + 1`
/// away does, so the offset is kept within that.
#[allow(clippy::cast_possible_truncation)]
fn tap_offset(spacing: f32, index: i32, extent: u32) -> (i64, f32) {
    let limit = i64::from(extent) + 1;
    let offset = f64::from(spacing) * f64::from(index);
    let whole = offset.floor();
    ((whole as i64).clamp(-limit, limit), (offset - whole) as f32)
}

#[inline]
fn add_weighted(sum: &mut [f32; 4], sample: [f32; 4], weight: f32) {
    for channel in 0..4 {
        sum[channel] += sample[channel] * weight;
    }
}

#[inline]
fn add_between(sum: &mut [f32; 4], a: [f32; 4], b: [f32; 4], t: f32, weight: f32) {
    for channel in 0..4 {
        sum[channel] += (a[channel] + (b[channel] - a[channel]) * t) * weight;
    }
}

/// gaussian-blur.wgsl: one axis of a Gaussian, taps `step` apart in uv.
///
/// The taps are the shader's, read bilinearly with the edge repeated and
/// summed in the shader's order. A tap lands `index * step` from the pixel
/// centre exactly, as the sampler's fixed-point coordinates place it, not
/// after a round trip through `uv` in f32 (a ten-thousandth of a texel
/// apart). So every pixel of a row reads the same two texels around its
/// own column (or row) with the same weights, and a tap is one pass over a
/// decoded row.
struct BlurPass;

impl CpuKernel for BlurPass {
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let taps = Taps::new(context);
        let step = context.vec2("step");
        let reach = taps.reach as f32;
        let size = context.size;
        grow(
            context.source_a.bounds(),
            margin(reach * step[0] * size.width as f32),
            margin(reach * step[1] * size.height as f32),
            size,
        )
    }

    #[allow(clippy::cast_precision_loss)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let taps = Taps::new(context);
        let step = context.vec2("step");
        let size = context.size;
        // The passes along one axis read the source's rows as the output's, which they are
        // only at the output's size; another size is sampled in uv, as the shader samples it.
        let texels = context.source_a.size() == size;
        if texels && step[1] == 0.0 {
            horizontal(context, &taps, step[0] * size.width as f32, y, x0, out);
        } else if texels && step[0] == 0.0 {
            vertical(context, &taps, step[1] * size.height as f32, y, x0, out);
        } else {
            super::shade_row(context, y, x0, out, |_, uv| {
                let mut result = [0.0_f32; 4];
                for index in -taps.reach..=taps.reach {
                    let offset = index as f32;
                    let sample =
                        context.sample_src([uv[0] + step[0] * offset, uv[1] + step[1] * offset]);
                    add_weighted(&mut result, sample, taps.weight(index));
                }
                result.map(|value| value / taps.sum)
            });
        }
    }
}

/// A pass along x, taps `spacing` texels apart: every tap reads row `y`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn horizontal(
    context: &KernelContext<'_>,
    taps: &Taps,
    spacing: f32,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
) {
    let width = context.size.width as usize;
    let offsets = (-taps.reach..=taps.reach)
        .map(|index| tap_offset(spacing, index, context.size.width))
        .collect::<Vec<_>>();
    // The row with its edge texels repeated past both ends, as far as a tap
    // reads: no tap needs clamping.
    let pad = offsets
        .iter()
        .map(|&(whole, _)| whole.unsigned_abs() as usize + 1)
        .max()
        .unwrap_or(1);
    let mut line = vec![[0.0_f32; 4]; width + 2 * pad];
    Row::of(context.source_a, y).decode(0, &mut line[pad..pad + width]);
    let (left, right) = (line[pad], line[pad + width - 1]);
    line[..pad].fill(left);
    line[pad + width..].fill(right);
    out.fill([0.0; 4]);
    for (index, &(whole, fraction)) in (-taps.reach..=taps.reach).zip(&offsets) {
        let weight = taps.weight(index);
        let first = (i64::from(x0) + whole + pad as i64) as usize;
        let texels = &line[first..=first + out.len()];
        if fraction == 0.0 {
            for (sum, texel) in out.iter_mut().zip(texels) {
                add_weighted(sum, *texel, weight);
            }
        } else {
            for (sum, pair) in out.iter_mut().zip(texels.windows(2)) {
                add_between(sum, pair[0], pair[1], fraction, weight);
            }
        }
    }
    for pixel in out.iter_mut() {
        *pixel = pixel.map(|value| value / taps.sum);
    }
}

/// A pass along y, taps `spacing` rows apart: each tap reads one or two
/// rows at the pixels' own columns.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn vertical(
    context: &KernelContext<'_>,
    taps: &Taps,
    spacing: f32,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
) {
    let image = context.source_a;
    let last = i64::from(context.size.height) - 1;
    let mut rows = vec![[0.0_f32; 4]; out.len() * 2];
    let (upper, lower) = rows.split_at_mut(out.len());
    out.fill([0.0; 4]);
    for index in -taps.reach..=taps.reach {
        let (whole, fraction) = tap_offset(spacing, index, context.size.height);
        let top = i64::from(y) + whole;
        let above = Row::of(image, top.clamp(0, last) as u32);
        let below = Row::of(image, (top + 1).clamp(0, last) as u32);
        let weight = taps.weight(index);
        if fraction == 0.0 {
            if matches!(above, Row::Empty) {
                continue;
            }
            above.decode(x0, upper);
            for (sum, texel) in out.as_flattened_mut().iter_mut().zip(upper.as_flattened()) {
                *sum += texel * weight;
            }
        } else {
            if matches!((above, below), (Row::Empty, Row::Empty)) {
                continue;
            }
            above.decode(x0, upper);
            below.decode(x0, lower);
            for ((sum, a), b) in out.iter_mut().zip(upper.iter()).zip(lower.iter()) {
                add_between(sum, *a, *b, fraction, weight);
            }
        }
    }
    for pixel in out.iter_mut() {
        *pixel = pixel.map(|value| value / taps.sum);
    }
}

/// glow-threshold.wgsl: what of each pixel glows, opaque. It draws every
/// pixel, transparent ones as opaque black.
struct GlowThreshold;

impl CpuKernel for GlowThreshold {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let threshold = context.float("threshold");
        effect_row(context, y, x0, out, |_, _, src| {
            let rgb = straight(src, 0.0001).map(|value| (value - threshold).max(0.0));
            [rgb[0], rgb[1], rgb[2], 1.0]
        });
    }
}

/// The pixels a two-input pass whose body keeps `from`'s alpha can draw,
/// through the transition wrapper.
fn wrapped_bounds(context: &KernelContext<'_>, body: PixelBox) -> PixelBox {
    if context.progress() <= 0.0 {
        context.source_a.bounds()
    } else if context.progress() >= 1.0 {
        context.source_b.bounds()
    } else {
        body
    }
}

/// glow-combine.wgsl: the source with its bloom added.
struct GlowCombine;

impl CpuKernel for GlowCombine {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        wrapped_bounds(context, context.source_a.bounds())
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let intensity = context.float("intensity");
        let size = context.size;
        transition_row(context, y, x0, out, |x, _| {
            let src = centre(context.source_a, size, x, y);
            let bloom = centre(context.source_b, size, x, y);
            let base = straight(src, 0.0001);
            let rgb = [0, 1, 2].map(|c| saturate(base[c] + bloom[c] * intensity));
            premultiplied(rgb, src[3])
        });
    }
}

/// unsharp-combine.wgsl: the source pushed away from its blur.
struct UnsharpCombine;

impl CpuKernel for UnsharpCombine {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        wrapped_bounds(context, context.source_a.bounds())
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let amount = context.float("amount");
        let threshold = context.float("threshold");
        let size = context.size;
        transition_row(context, y, x0, out, |x, _| {
            let src = centre(context.source_a, size, x, y);
            if src[3] <= 0.0001 {
                return src;
            }
            let blurred = centre(context.source_b, size, x, y);
            let center = [src[0] / src[3], src[1] / src[3], src[2] / src[3]];
            let around = if blurred[3] > 0.0001 {
                [0, 1, 2].map(|c| blurred[c] / blurred[3])
            } else {
                center
            };
            let detail = [0, 1, 2].map(|c| center[c] - around[c]);
            let weight = builtin_smoothstep(0.0, threshold, luminance(detail).abs());
            output(
                [0, 1, 2].map(|c| center[c] + detail[c] * amount * weight),
                src[3],
            )
        });
    }
}

/// focus-combine.wgsl: the source and its two blurs by the distance from
/// the band.
struct FocusCombine;

impl FocusCombine {
    fn input<'a>(context: &KernelContext<'a>, name: &str) -> Option<&'a CpuImage> {
        context
            .inputs
            .iter()
            .find(|(input, _)| *input == name)
            .map(|(_, image)| *image)
    }
}

impl CpuKernel for FocusCombine {
    // The blurs reach past the source.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        ["soft", "blurred"]
            .into_iter()
            .filter_map(|name| Self::input(context, name))
            .fold(context.source_a.bounds(), |covered, image| {
                covered.union(image.bounds())
            })
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let resolution = context.resolution();
        let aspect = resolution[0] / resolution[1];
        let position = context.float("position");
        let angle = context.float("angle").to_radians();
        let normal = [libm::sinf(angle), libm::cosf(angle)];
        let focus = context.float("focus");
        let falloff = context.float("falloff");
        let (soft, blurred) = (
            Self::input(context, "soft"),
            Self::input(context, "blurred"),
        );
        let size = context.size;
        effect_row(context, y, x0, out, |x, uv, src| {
            let offset = [(uv[0] - 0.5) * aspect, uv[1] - position];
            let off_focus = (offset[0] * normal[0] + offset[1] * normal[1]).abs();
            let w = builtin_smoothstep(focus * 0.5, focus * 0.5 + falloff, off_focus);
            let soft = soft.map_or([0.0; 4], |image| centre(image, size, x, y));
            let blurred = blurred.map_or([0.0; 4], |image| centre(image, size, x, y));
            if w < 0.5 {
                mix4(src, soft, w * 2.0)
            } else {
                mix4(soft, blurred, w * 2.0 - 1.0)
            }
        });
    }
}

/// The track matte (masking.rs `track_matte_definition`).
struct TrackMatte;

impl CpuKernel for TrackMatte {
    // Without inversion the matte's coverage is zero where it is
    // transparent.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let picture = context.source_a.bounds();
        let body = if context.boolean("inverted") {
            picture
        } else {
            picture.intersect(context.source_b.bounds())
        };
        wrapped_bounds(context, body)
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let luma = context.boolean("luma");
        let inverted = context.boolean("inverted");
        let size = context.size;
        transition_row(context, y, x0, out, |x, _| {
            let picture = centre(context.source_a, size, x, y);
            let matte = centre(context.source_b, size, x, y);
            let mut coverage = matte[3];
            if luma {
                let alpha = matte[3].max(0.000_001);
                let linear = [0, 1, 2].map(|c| curves::srgb_to_linear(saturate(matte[c] / alpha)));
                coverage = luminance(linear) * matte[3];
            }
            if inverted {
                coverage = 1.0 - coverage;
            }
            let coverage = saturate(coverage);
            picture.map(|value| value * coverage)
        });
    }
}

/// The shape-mask combine for one to eight masks (masking.rs
/// `mask_combine_definition`): one kernel, the mask count read from the
/// definition.
struct MaskCombine;

/// One mask of a combine: its coverage picture (unbound samples
/// transparent), inversion and mode.
#[derive(Copy, Clone)]
struct Mask<'a> {
    image: Option<&'a CpuImage>,
    invert: bool,
    mode: u32,
}

impl MaskCombine {
    fn masks<'a>(context: &KernelContext<'a>) -> ([Mask<'a>; MAX_SHAPE_MASKS], usize) {
        let count = context.definition.inputs.len().min(MAX_SHAPE_MASKS);
        let mut masks = [Mask {
            image: None,
            invert: false,
            mode: 0,
        }; MAX_SHAPE_MASKS];
        for (mask, names) in masks.iter_mut().zip(&MASK_COMBINE_NAMES[..count]) {
            *mask = Mask {
                image: context
                    .inputs
                    .iter()
                    .find(|(name, _)| *name == names.input)
                    .map(|(_, image)| *image),
                invert: context.boolean(names.invert),
                mode: context.choice(names.mode),
            };
        }
        (masks, count)
    }
}

/// `builtin_mask_step`.
#[inline]
fn mask_step(coverage: f32, mask: f32, invert: bool, mode: u32) -> f32 {
    let mut m = saturate(mask);
    if invert {
        m = 1.0 - m;
    }
    match mode {
        1 => coverage * (1.0 - m),
        2 => coverage * m,
        _ => coverage + m - coverage * m,
    }
}

impl CpuKernel for MaskCombine {
    // Coverage that starts empty only grows where an added mask covers
    // something: subtracting and intersecting never raise it.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let source = context.source_a.bounds();
        if context.choice("mode0") != 0 {
            return source;
        }
        let (masks, count) = Self::masks(context);
        let mut covered = PixelBox::EMPTY;
        for mask in masks[..count].iter().filter(|mask| mask.mode == 0) {
            match mask.image {
                Some(image) if !mask.invert => covered = covered.union(image.bounds()),
                // An inverted added mask can cover everything.
                _ => return source,
            }
        }
        source.intersect(covered.expand(1, context.size))
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let (masks, count) = Self::masks(context);
        let masks = &masks[..count];
        let start = if context.choice("mode0") == 0 {
            0.0
        } else {
            1.0
        };
        let size = context.size;
        effect_row(context, y, x0, out, |x, _, src| {
            let mut coverage = start;
            for mask in masks {
                let value = mask.image.map_or(0.0, |image| centre(image, size, x, y)[3]);
                coverage = mask_step(coverage, value, mask.invert, mask.mode);
            }
            let coverage = saturate(coverage);
            src.map(|value| value * coverage)
        });
    }
}

/// shadow-offset.wgsl: the source's alpha moved by `offset`, in the shadow
/// color.
struct ShadowOffset;

impl CpuKernel for ShadowOffset {
    // The source's pixels moved by the offset, with room for the bilinear
    // neighbour and rounding; a side on the canvas's edge reaches that edge
    // still, as the sampler repeats it.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let source = context.source_a.bounds();
        let offset = context.vec2("offset");
        let size = context.size;
        if source.is_empty() {
            return source;
        }
        if context.source_a.size() != size || !offset.iter().all(|value| value.is_finite()) {
            return PixelBox::whole(size);
        }
        let extent = [size.width as f32, size.height as f32];
        let shift = [offset[0] * extent[0], offset[1] * extent[1]];
        let low = [
            source.x as f32 + shift[0] - 2.0,
            source.y as f32 + shift[1] - 2.0,
        ];
        let high = [
            source.right() as f32 + shift[0] + 2.0,
            source.bottom() as f32 + shift[1] + 2.0,
        ];
        let clamp = |value: f32, axis: usize| value.max(0.0).min(extent[axis]) as u32;
        let x = if source.x == 0 {
            0
        } else {
            clamp(low[0].floor(), 0)
        };
        let y = if source.y == 0 {
            0
        } else {
            clamp(low[1].floor(), 1)
        };
        let right = if source.right() == size.width {
            size.width
        } else {
            clamp(high[0].ceil(), 0)
        };
        let bottom = if source.bottom() == size.height {
            size.height
        } else {
            clamp(high[1].ceil(), 1)
        };
        if right <= x || bottom <= y {
            return PixelBox::EMPTY;
        }
        PixelBox {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let offset = context.vec2("offset");
        let color = context.vec4("color");
        let opacity = context.float("opacity");
        effect_row(context, y, x0, out, |_, uv, _| {
            let moved = context.sample_src([uv[0] - offset[0], uv[1] - offset[1]]);
            let coverage = moved[3] * opacity * color[3];
            [color[0], color[1], color[2], 1.0].map(|value| value * coverage)
        });
    }
}

/// shadow-combine.wgsl: the source over its softened shadow, or the shadow
/// alone.
struct ShadowCombine;

impl CpuKernel for ShadowCombine {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let shadow = context.source_b.bounds();
        let body = if context.boolean("shadow_only") {
            shadow
        } else {
            shadow.union(context.source_a.bounds())
        };
        wrapped_bounds(context, body)
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let shadow_only = context.boolean("shadow_only");
        let size = context.size;
        transition_row(context, y, x0, out, |x, _| {
            let shadow = centre(context.source_b, size, x, y);
            if shadow_only {
                return shadow;
            }
            let src = centre(context.source_a, size, x, y);
            let rest = 1.0 - src[3];
            [0, 1, 2, 3].map(|c| src[c] + shadow[c] * rest)
        });
    }
}
