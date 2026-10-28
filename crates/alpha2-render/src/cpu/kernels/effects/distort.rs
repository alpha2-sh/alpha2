//! The "Distort" group's kernels.

use super::{
    CpuKernel, KernelContext, aspect, centre, effect_row, grow, luminance, margin, rotate,
    sample_edge, saturate, seed, straight, uv_width, wrap,
};
use crate::cpu::image::PixelBox;
use crate::cpu::kernels::{builtin_value_noise, fract, mix, shade_row, smoothstep};
use libm::{atan2f, cosf, expf, sinf};
use std::f32::consts::TAU;

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    Some(match id {
        "alpha2.effect.pixelate" => &Pixelate,
        "alpha2.effect.crop" => &Crop,
        "alpha2.effect.mirror" => &Mirror,
        "alpha2.effect.swirl" => &Swirl,
        "alpha2.effect.bulge" => &Bulge,
        "alpha2.effect.wave-warp" => &WaveWarp,
        "alpha2.effect.ripple" => &Ripple,
        "alpha2.effect.lens-distortion" => &LensDistortion,
        "alpha2.effect.polar-coordinates" => &PolarCoordinates,
        "alpha2.effect.offset" => &Offset,
        "alpha2.effect.corner-pin" => &CornerPin,
        "alpha2.effect.shake" => &Shake,
        "alpha2.effect.turbulent-displace" => &TurbulentDisplace,
        "alpha2.effect.displacement-map" => &DisplacementMap,
        "alpha2.effect.kaleidoscope" => &Kaleidoscope,
        "alpha2.effect.tile" => &Tile,
        _ => return None,
    })
}

/// pixelate.wgsl: each cell shows the sample at its centre.
struct Pixelate;

/// The hexagon shape's `1 / sqrt(3)` and `sqrt(3)`, as the shader writes them.
const HEX_ROW: f32 = 0.577_350_27;
const SQRT_3: f32 = 1.732_050_8;

impl Pixelate {
    fn cells(context: &KernelContext<'_>) -> [f32; 2] {
        let resolution = context.resolution();
        let aspect = resolution[0] / resolution[1];
        let blocks = context.float("blocks");
        [blocks.max(2.0), (blocks / aspect).max(2.0)]
    }

    /// The hexagon's size, centre to corner, in output heights.
    fn hexagon(context: &KernelContext<'_>) -> f32 {
        let resolution = context.resolution();
        let aspect = resolution[0] / resolution[1];
        aspect / context.float("blocks").max(2.0) / SQRT_3
    }
}

/// The centre, in uv, of the pointy-top hexagon of `size` output heights
/// that holds `uv`: cube coordinates rounded to the nearest centre.
#[allow(clippy::many_single_char_names)]
fn hexagon_centre(uv: [f32; 2], aspect: f32, size: f32) -> [f32; 2] {
    let p = [(uv[0] - 0.5) * aspect / size, (uv[1] - 0.5) * 1.0 / size];
    let q = [p[0] * HEX_ROW - p[1] / 3.0, p[1] * 0.666_666_7];
    let cube = [q[0], q[1], -q[0] - q[1]];
    // WGSL `round` takes ties to even.
    let mut r = cube.map(f32::round_ties_even);
    let d = [0, 1, 2].map(|axis| (r[axis] - cube[axis]).abs());
    if d[0] > d[1] && d[0] > d[2] {
        r[0] = -r[1] - r[2];
    } else if d[1] > d[2] {
        r[1] = -r[0] - r[2];
    }
    let centre = [
        (SQRT_3 * r[0] + 0.866_025_4 * r[1]) * size,
        (1.5 * r[1]) * size,
    ];
    [centre[0] / aspect + 0.5, centre[1] / 1.0 + 0.5]
}

impl CpuKernel for Pixelate {
    // A pixel shows its cell's centre, at most half a cell away, or a
    // hexagon's, at most its size away.
    #[allow(clippy::cast_precision_loss)]
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        let size = context.size;
        if context.choice("shape") != 0 {
            let reach = margin(Self::hexagon(context) * size.height as f32);
            return grow(context.source_a.bounds(), reach, reach, size);
        }
        let cells = Self::cells(context);
        grow(
            context.source_a.bounds(),
            margin(size.width as f32 / cells[0] * 0.5),
            margin(size.height as f32 / cells[1] * 0.5),
            size,
        )
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        if context.choice("shape") != 0 {
            let aspect = aspect(context)[0];
            let size = Self::hexagon(context);
            shade_row(context, y, x0, out, |_, uv| {
                context.sample_src(hexagon_centre(uv, aspect, size))
            });
            return;
        }
        let cells = Self::cells(context);
        shade_row(context, y, x0, out, |_, uv| {
            context.sample_src([
                ((uv[0] * cells[0]).floor() + 0.5) / cells[0],
                ((uv[1] * cells[1]).floor() + 0.5) / cells[1],
            ])
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
        shade_row(context, y, x0, out, |_, uv| {
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

/// Fills `out` with the source at `map(uv)` for the pixels `x0..` of row
/// `y`, its edge per `mode`: the shared shape of the distortions, which
/// take `fwidthFine` of the source position for the edge's coverage.
fn warp_row(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
    mode: u32,
    map: impl Fn([f32; 2]) -> [f32; 2],
) {
    shade_row(context, y, x0, out, |x, uv| {
        let width = uv_width(context.size, x, y, &map);
        sample_edge(context, map(uv), width, mode)
    });
}

/// `length(v)`.
#[inline]
fn length(v: [f32; 2]) -> f32 {
    (v[0] * v[0] + v[1] * v[1]).sqrt()
}

/// swirl.wgsl: a turn about the centre that fades to none at the radius.
struct Swirl;

impl CpuKernel for Swirl {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let aspect = aspect(context);
        let center = context.vec2("center");
        let radius = context.float("radius").max(0.0001);
        let angle = context.float("angle").to_radians();
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [
                (uv[0] - center[0]) * aspect[0],
                (uv[1] - center[1]) * aspect[1],
            ];
            let t = saturate(1.0 - length(p) / radius);
            let q = rotate(p, angle * t * t);
            [center[0] + q[0] / aspect[0], center[1] + q[1] / aspect[1]]
        });
    }
}

/// bulge.wgsl: a radial magnification or pinch inside the radius.
struct Bulge;

impl CpuKernel for Bulge {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let aspect = aspect(context);
        let center = context.vec2("center");
        let radius = context.float("radius").max(0.0001);
        let strength = context.float("strength");
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [
                (uv[0] - center[0]) * aspect[0],
                (uv[1] - center[1]) * aspect[1],
            ];
            let x = length(p) / radius;
            let f = (1.0 - x * x).max(0.0);
            let scale = 1.0 - strength * 0.75 * f * f;
            [
                center[0] + p[0] * scale / aspect[0],
                center[1] + p[1] * scale / aspect[1],
            ]
        });
    }
}

/// wave-warp.wgsl: a travelling sine wave displacing across its direction.
struct WaveWarp;

impl CpuKernel for WaveWarp {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let aspect = aspect(context);
        let turn = context.float("angle").to_radians();
        let direction = [cosf(turn), -sinf(turn)];
        let normal = [-direction[1], direction[0]];
        let amplitude = context.float("amplitude");
        let wavelength = context.float("wavelength");
        let cycles = fract(context.float("speed") * context.time());
        let phase_offset = context.float("phase").to_radians();
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [(uv[0] - 0.5) * aspect[0], (uv[1] - 0.5) * aspect[1]];
            let along = p[0] * direction[0] + p[1] * direction[1];
            let phase = TAU * (along / wavelength - cycles) + phase_offset;
            let wave = sinf(phase);
            let q = [
                p[0] + normal[0] * amplitude * wave,
                p[1] + normal[1] * amplitude * wave,
            ];
            [q[0] / aspect[0] + 0.5, q[1] / aspect[1] + 0.5]
        });
    }
}

/// ripple.wgsl: rings travelling out of the centre, fading with distance.
struct Ripple;

impl CpuKernel for Ripple {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let aspect = aspect(context);
        let center = context.vec2("center");
        let amplitude = context.float("amplitude");
        let wavelength = context.float("wavelength");
        let decay = context.float("decay");
        let cycles = fract(context.float("speed") * context.time());
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [
                (uv[0] - center[0]) * aspect[0],
                (uv[1] - center[1]) * aspect[1],
            ];
            let r = length(p);
            let wave = sinf(TAU * (r / wavelength - cycles)) * expf(-decay * r);
            let away = r.max(0.000_001);
            let q = [
                p[0] + p[0] / away * amplitude * wave,
                p[1] + p[1] / away * amplitude * wave,
            ];
            [center[0] + q[0] / aspect[0], center[1] + q[1] / aspect[1]]
        });
    }
}

/// lens-distortion.wgsl: the Brown-Conrady radial model.
struct LensDistortion;

impl CpuKernel for LensDistortion {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let aspect = aspect(context);
        let corner = 0.5 * length(aspect);
        let k1 = context.float("k1");
        let k2 = context.float("k2");
        let zoom = context.float("zoom");
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [
                (uv[0] - 0.5) * aspect[0] / corner,
                (uv[1] - 0.5) * aspect[1] / corner,
            ];
            let r2 = p[0] * p[0] + p[1] * p[1];
            let scale = 1.0 + k1 * r2 + k2 * r2 * r2;
            let q = [p[0] * scale / zoom, p[1] * scale / zoom];
            [
                q[0] * corner / aspect[0] + 0.5,
                q[1] * corner / aspect[1] + 0.5,
            ]
        });
    }
}

/// polar-coordinates.wgsl: the picture wrapped around the centre, or
/// unwrapped.
struct PolarCoordinates;

impl PolarCoordinates {
    /// The source position, and the same with the angle cut at the top.
    fn positions(uv: [f32; 2], unrolled: bool, amount: f32) -> ([f32; 2], [f32; 2]) {
        let c = [uv[0] - 0.5, uv[1] - 0.5];
        let radius = length(c) * 2.0;
        let around = [fract(atan2f(c[0], -c[1]) / TAU + 0.5), radius];
        let turned = [atan2f(-c[0], c[1]) / TAU, radius];
        let angle = (uv[0] - 0.5) * TAU;
        let unwrapped = [
            0.5 + sinf(angle) * uv[1] * 0.5,
            0.5 + -cosf(angle) * uv[1] * 0.5,
        ];
        let blend =
            |target: [f32; 2]| [mix(uv[0], target[0], amount), mix(uv[1], target[1], amount)];
        if unrolled {
            (blend(unwrapped), blend(unwrapped))
        } else {
            (blend(around), blend(turned))
        }
    }
}

impl CpuKernel for PolarCoordinates {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let unrolled = context.choice("mode") == 1;
        let amount = context.float("amount");
        let mode = context.choice("edge");
        let size = context.size;
        shade_row(context, y, x0, out, |x, uv| {
            let width = uv_width(size, x, y, |uv| Self::positions(uv, unrolled, amount).0);
            let other = uv_width(size, x, y, |uv| Self::positions(uv, unrolled, amount).1);
            let source = Self::positions(uv, unrolled, amount).0;
            sample_edge(context, source, [width[0].min(other[0]), width[1]], mode)
        });
    }
}

/// offset.wgsl: the picture shifted, wrapped per the edge.
struct Offset;

impl CpuKernel for Offset {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let shift = context.vec2("shift");
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            [uv[0] - shift[0], uv[1] - shift[1]]
        });
    }
}

/// corner-pin.wgsl: the inverse of the unit square's projective map onto
/// the quad (Heckbert 1989).
struct CornerPin;

/// The adjugate of the square-to-quad map, row by row, the map's
/// determinant, and whether the quad's sides are parallel to nothing.
struct Projection {
    rows: [[f32; 3]; 3],
    weight: f32,
    degenerate: bool,
}

impl Projection {
    #[allow(clippy::many_single_char_names, clippy::similar_names)]
    fn new(context: &KernelContext<'_>) -> Self {
        let p0 = context.vec2("top_left");
        let p1 = context.vec2("top_right");
        let p2 = context.vec2("bottom_right");
        let p3 = context.vec2("bottom_left");
        let d1 = [p1[0] - p2[0], p1[1] - p2[1]];
        let d2 = [p3[0] - p2[0], p3[1] - p2[1]];
        let d3 = [p0[0] - p1[0] + p2[0] - p3[0], p0[1] - p1[1] + p2[1] - p3[1]];
        let det = d1[0] * d2[1] - d2[0] * d1[1];
        let degenerate = det.abs() < 1.0e-8;
        let safe = if degenerate { 1.0 } else { det };
        let g = (d3[0] * d2[1] - d2[0] * d3[1]) / safe;
        let h = (d1[0] * d3[1] - d3[0] * d1[1]) / safe;
        let a = p1[0] - p0[0] + g * p1[0];
        let b = p3[0] - p0[0] + h * p3[0];
        let c = p0[0];
        let d = p1[1] - p0[1] + g * p1[1];
        let e = p3[1] - p0[1] + h * p3[1];
        let f = p0[1];
        let ia = e - f * h;
        let ib = c * h - b;
        let ic = b * f - c * e;
        let id = f * g - d;
        let ie = a - c * g;
        let if_ = c * d - a * f;
        let ig = d * h - e * g;
        let ih = b * g - a * h;
        let ii = a * e - b * d;
        Self {
            rows: [[ia, ib, ic], [id, ie, if_], [ig, ih, ii]],
            weight: a * ia + b * id + c * ig,
            degenerate,
        }
    }

    /// The source position of `uv`, and whether the map reaches it.
    fn source(&self, uv: [f32; 2]) -> ([f32; 2], bool) {
        let [x, y, w] = self.rows;
        let w = w[0] * uv[0] + w[1] * uv[1] + w[2];
        let front = w * self.weight > 0.0 && !self.degenerate;
        let divisor = if front { w } else { 1.0 };
        (
            [
                (x[0] * uv[0] + x[1] * uv[1] + x[2]) / divisor,
                (y[0] * uv[0] + y[1] * uv[1] + y[2]) / divisor,
            ],
            front,
        )
    }
}

impl CpuKernel for CornerPin {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let projection = Projection::new(context);
        shade_row(context, y, x0, out, |x, uv| {
            let width = uv_width(context.size, x, y, |uv| projection.source(uv).0);
            let (source, front) = projection.source(uv);
            if front {
                sample_edge(context, source, width, 0)
            } else {
                [0.0; 4]
            }
        });
    }
}

/// shake.wgsl: the whole frame moved, turned and zoomed by noise in time.
struct Shake;

impl CpuKernel for Shake {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let seed = seed(context.float("seed"));
        let t = context.time() * context.float("frequency");
        let shift = [
            builtin_value_noise([t, 0.5], seed) * 2.0 - 1.0,
            builtin_value_noise([t, 3.5], seed) * 2.0 - 1.0,
        ];
        let turn = (builtin_value_noise([t, 6.5], seed) * 2.0 - 1.0)
            * context.float("rotation").to_radians();
        let aspect = aspect(context);
        let zoom = context.float("zoom");
        let amplitude = context.float("amplitude");
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let turned = rotate(
                [
                    (uv[0] - 0.5) * aspect[0] / zoom,
                    (uv[1] - 0.5) * aspect[1] / zoom,
                ],
                -turn,
            );
            let p = [
                turned[0] - shift[0] * amplitude,
                turned[1] - shift[1] * amplitude,
            ];
            [p[0] / aspect[0] + 0.5, p[1] / aspect[1] + 0.5]
        });
    }
}

/// turbulent-displace.wgsl: two fractal noise fields displacing the
/// picture.
struct TurbulentDisplace;

/// `turbulent_fbm`, given the octave weights past the first.
fn turbulent_fbm(p: [f32; 2], seed: u32, weights: [f32; 3]) -> f32 {
    let octave = |scale: f32, shift: [f32; 2]| {
        builtin_value_noise([p[0] * scale + shift[0], p[1] * scale + shift[1]], seed)
    };
    let mut sum = builtin_value_noise(p, seed);
    sum += weights[0] * octave(2.03, [17.0, 3.0]);
    sum += weights[1] * octave(4.01, [5.0, 29.0]);
    sum += weights[2] * octave(7.97, [41.0, 11.0]);
    sum / (1.0 + weights[0] + weights[1] + weights[2])
}

impl CpuKernel for TurbulentDisplace {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let seed = seed(context.float("seed"));
        let aspect = aspect(context);
        // WGSL `round` takes ties to even.
        let octaves = context.float("complexity").round_ties_even();
        let weights = [(2.0, 0.5), (3.0, 0.25), (4.0, 0.125)]
            .map(|(least, weight)| if octaves >= least { weight } else { 0.0 });
        let drift = fract(context.time() * context.float("evolution") * 0.01) * 100.0;
        let size = context.float("size");
        let amount = context.float("amount");
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [
                (uv[0] - 0.5) * aspect[0] / size + drift,
                (uv[1] - 0.5) * aspect[1] / size + drift,
            ];
            let d = [
                turbulent_fbm(p, seed, weights) * 2.0 - 1.0,
                turbulent_fbm([p[0] + 31.7, p[1] + 47.3], seed.wrapping_add(1), weights) * 2.0
                    - 1.0,
            ];
            [
                uv[0] + d[0] * amount / aspect[0],
                uv[1] + d[1] * amount / aspect[1],
            ]
        });
    }
}

/// displacement-map.wgsl: the picture displaced by two channels of the
/// `map` input.
struct DisplacementMap;

/// `displacement_channel`: the channel's value, a color channel of a
/// transparent map neutral.
fn displacement_channel(c: [f32; 4], channel: u32) -> f32 {
    let rgb = straight(c, 0.0001);
    let color = match channel {
        0 => rgb[0],
        1 => rgb[1],
        2 => rgb[2],
        _ => luminance(rgb),
    };
    if channel == 3 {
        c[3]
    } else {
        mix(0.5, color, c[3])
    }
}

impl CpuKernel for DisplacementMap {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let map = context
            .inputs
            .iter()
            .find(|(name, _)| *name == "map")
            .map(|(_, image)| *image);
        let channels = [context.choice("channel_x"), context.choice("channel_y")];
        let amount = [context.float("amount_x"), context.float("amount_y")];
        let aspect = aspect(context);
        let size = context.size;
        let resolution = context.resolution();
        // The map's value is per pixel, so its derivative is between quad
        // neighbours; `source` takes the pixel whose centre it is given.
        let source = |uv: [f32; 2]| {
            let (px, py) = pixel_of(uv, resolution);
            let m = map.map_or([0.0; 4], |image| centre(image, size, px, py));
            let d = [
                displacement_channel(m, channels[0]) - 0.5,
                displacement_channel(m, channels[1]) - 0.5,
            ];
            [
                uv[0] + d[0] * 2.0 * amount[0] / aspect[0],
                uv[1] + d[1] * 2.0 * amount[1] / aspect[1],
            ]
        };
        let mode = context.choice("edge");
        shade_row(context, y, x0, out, |x, uv| {
            let width = uv_width(size, x, y, source);
            sample_edge(context, source(uv), width, mode)
        });
    }
}

/// The pixel whose centre is `uv` in a picture of `resolution`.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pixel_of(uv: [f32; 2], resolution: [f32; 2]) -> (u32, u32) {
    (
        (uv[0] * resolution[0]).floor() as u32,
        (uv[1] * resolution[1]).floor() as u32,
    )
}

/// kaleidoscope.wgsl: mirrored wedges about the centre.
struct Kaleidoscope;

impl CpuKernel for Kaleidoscope {
    #[allow(clippy::many_single_char_names)]
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let aspect = aspect(context);
        let center = context.vec2("center");
        // WGSL `round` takes ties to even.
        let segment = TAU / context.float("segments").round_ties_even().max(2.0);
        let turn = context.float("angle").to_radians();
        warp_row(context, y, x0, out, context.choice("edge"), |uv| {
            let p = [
                (uv[0] - center[0]) * aspect[0],
                (uv[1] - center[1]) * aspect[1],
            ];
            let mut a = atan2f(p[1], p[0]) - turn;
            a -= segment * (a / segment).floor();
            a = a.min(segment - a);
            let r = length(p);
            let q = rotate([cosf(a) * r, sinf(a) * r], turn);
            [center[0] + q[0] / aspect[0], center[1] + q[1] / aspect[1]]
        });
    }
}

/// tile.wgsl: the picture repeated, or mirrored, about the centre.
struct Tile;

impl CpuKernel for Tile {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let tiles = context.vec2("tiles");
        let offset = context.vec2("offset");
        let mode = if context.boolean("mirror") { 3 } else { 2 };
        shade_row(context, y, x0, out, |_, uv| {
            let p = [
                (uv[0] - 0.5 - offset[0]) * tiles[0] + 0.5,
                (uv[1] - 0.5 - offset[1]) * tiles[1] + 0.5,
            ];
            context.sample_src(wrap(p, mode))
        });
    }
}
