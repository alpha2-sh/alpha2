//! The built-in transitions (builtins `transitions`), one module per catalog
//! group, as the definitions are.
//!
//! Each kernel is a literal port of its shader body in
//! `alpha2-shaders/shaders/runtime/transitions`: the same operations in the
//! same order, in gl-transitions' y-up coordinates (`gl_uv`). A
//! `fwidthFine` is the same expression evaluated at the pixel's 2 x 2 quad
//! partners, written as a function of the shader's `uv`.

// The shaders' own conversions between f32, u32 and the pixel grid.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use core::f32::consts::{FRAC_PI_2, PI, SQRT_2, TAU};

use libm::{atan2f, cosf, powf, sinf};

use super::{
    CpuKernel, KernelContext, builtin_aa_pulse, builtin_aa_ramp, builtin_aa_smoothstep,
    builtin_aa_step, builtin_angle_width, builtin_interval_coverage, builtin_premultiply,
    builtin_smoothstep, builtin_smoothstep4, builtin_straight, fract, fwidth_fine, gl_from_color,
    gl_to_color, gl_uv, mix, mix4, pcg, smoothstep, step, transition_row,
};

mod classic;
mod glitch;
mod shapes;
mod stylized;
mod three_d;
mod wipes;
mod zoom_blur;

type Vec2 = [f32; 2];
type Rgba = [f32; 4];
type Row = fn(&KernelContext<'_>, u32, u32, &mut [Rgba]);

/// A transition kernel: its row function.
struct Transition(Row);

impl CpuKernel for Transition {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
        (self.0)(context, y, x0, out);
    }
}

/// Each group's kernels, by the names after `alpha2.transition.`.
static GROUPS: [&[(&str, Transition)]; 7] = [
    classic::KERNELS,
    wipes::KERNELS,
    stylized::KERNELS,
    shapes::KERNELS,
    three_d::KERNELS,
    zoom_blur::KERNELS,
    glitch::KERNELS,
];

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    let name = id.strip_prefix("alpha2.transition.")?;
    GROUPS
        .iter()
        .flat_map(|kernels| kernels.iter())
        .find(|(kernel, _)| *kernel == name)
        .map(|(_, kernel)| kernel as &dyn CpuKernel)
}

#[inline]
fn add(a: Vec2, b: Vec2) -> Vec2 {
    [a[0] + b[0], a[1] + b[1]]
}

#[inline]
fn sub(a: Vec2, b: Vec2) -> Vec2 {
    [a[0] - b[0], a[1] - b[1]]
}

#[inline]
fn scale(a: Vec2, s: f32) -> Vec2 {
    [a[0] * s, a[1] * s]
}

#[inline]
fn dot(a: Vec2, b: Vec2) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

#[inline]
fn length(a: Vec2) -> f32 {
    dot(a, a).sqrt()
}

#[inline]
fn distance(a: Vec2, b: Vec2) -> f32 {
    length(sub(a, b))
}

#[inline]
fn normalize(a: Vec2) -> Vec2 {
    let length = length(a);
    [a[0] / length, a[1] / length]
}

/// WGSL `sign`: zero for zero, unlike `f32::signum`.
#[inline]
fn sign(x: f32) -> f32 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

#[inline]
fn add4(a: Rgba, b: Rgba) -> Rgba {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}

#[inline]
const fn select(rejected: f32, accepted: f32, condition: bool) -> f32 {
    if condition { accepted } else { rejected }
}

/// `normalize(direction)` scaled to unit L1 length, for the directional
/// wipe and warp; `fallback` for a direction of (almost) zero length.
fn l1_direction(direction: Vec2, fallback: Vec2) -> Vec2 {
    let direction = if length(direction) < 0.000_001 {
        fallback
    } else {
        direction
    };
    let vector = normalize(direction);
    let l1 = vector[0].abs() + vector[1].abs();
    [vector[0] / l1, vector[1] / l1]
}

/// The `sin` of `crazy-parametric-fun.wgsl` and `directional-scaled.wgsl`:
/// reduced to a quarter turn and summed as its Taylor series to x^11, the
/// shaders' operations in their order, from their literals.
#[allow(
    clippy::excessive_precision,
    clippy::many_single_char_names,
    clippy::unreadable_literal
)]
fn polynomial_sin(x: f32) -> f32 {
    let turns = x * 0.159154943091895;
    let r = (turns - turns.floor()) * TAU;
    let upper = r > PI;
    let h = if upper { r - PI } else { r };
    let t = if h > FRAC_PI_2 { PI - h } else { h };
    let t2 = t * t;
    let s = t
        * (1.0
            + t2 * (-0.166666666666667
                + t2 * (0.00833333333333333
                    + t2 * (-0.000198412698412698
                        + t2 * (0.00000275573192239859 - t2 * 0.0000000250521083854417)))));
    if upper { -s } else { s }
}
