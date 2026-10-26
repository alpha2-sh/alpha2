//! Paints: a port of `paint_wgsl.rs`, shared by the generator pass and the
//! vector pass as the GPU shares its WGSL, so a gradient is the same color
//! wherever it is drawn.

use super::curves::{exact_srgb_to_linear, linear_to_srgb};
use super::kernels::mix4;
use crate::MAX_GENERATOR_STOPS;

/// Paint kinds as `AlphaPaint.kind` numbers them.
pub(crate) const KIND_NONE: u32 = 0;
pub(crate) const KIND_SOLID: u32 = 1;
pub(crate) const KIND_LINEAR: u32 = 2;
pub(crate) const KIND_RADIAL: u32 = 3;
pub(crate) const KIND_CONIC: u32 = 4;

/// `AlphaPaint`, with its stop colors already premultiplied in linear
/// light: the shader converts them per pixel, always to the same values.
#[derive(Clone, Debug)]
pub(crate) struct Paint {
    kind: u32,
    a: [f32; 2],
    b: [f32; 2],
    spread: u32,
    count: usize,
    offsets: [f32; MAX_GENERATOR_STOPS],
    colors: [[f32; 4]; MAX_GENERATOR_STOPS],
}

impl Paint {
    /// A paint of `count` stops (at most eight are read) with straight sRGB
    /// `colors`, as authored.
    pub(crate) fn new(
        kind: u32,
        a: [f32; 2],
        b: [f32; 2],
        spread: u32,
        count: usize,
        offsets: [f32; MAX_GENERATOR_STOPS],
        colors: [[f32; 4]; MAX_GENERATOR_STOPS],
    ) -> Self {
        Self {
            kind,
            a,
            b,
            spread,
            count: count.min(MAX_GENERATOR_STOPS),
            offsets,
            colors: colors.map(stop_color),
        }
    }

    /// Whether it paints nothing anywhere.
    pub(crate) const fn is_none(&self) -> bool {
        self.kind == KIND_NONE || self.count == 0
    }

    /// Its color when that is the same everywhere: no paint, a solid, or a
    /// gradient of one stop.
    pub(crate) fn constant(&self) -> Option<[f32; 4]> {
        if self.is_none() {
            return Some([0.0; 4]);
        }
        (self.kind == KIND_SOLID || self.count == 1).then_some(self.colors[0])
    }

    /// `builtin_paint(paint, uv, res)`: the premultiplied linear-light
    /// color at `uv`, normalized in a picture of `res` pixels.
    #[inline]
    pub(crate) fn at(&self, uv: [f32; 2], res: [f32; 2]) -> [f32; 4] {
        if let Some(color) = self.constant() {
            return color;
        }
        let t = self.spread_position(self.position(uv, res));
        let offsets = &self.offsets;
        if t <= offsets[0] {
            return self.colors[0];
        }
        for stop in 1..self.count {
            if t <= offsets[stop] {
                let span = offsets[stop] - offsets[stop - 1];
                let f = if span <= 0.0 {
                    1.0
                } else {
                    (t - offsets[stop - 1]) / span
                };
                return mix4(self.colors[stop - 1], self.colors[stop], f);
            }
        }
        self.colors[self.count - 1]
    }

    /// `builtin_paint_position`.
    #[inline]
    fn position(&self, uv: [f32; 2], res: [f32; 2]) -> f32 {
        let p = [uv[0] * res[0], uv[1] * res[1]];
        let a = [self.a[0] * res[0], self.a[1] * res[1]];
        if self.kind == KIND_LINEAR {
            let direction = [self.b[0] * res[0] - a[0], self.b[1] * res[1] - a[1]];
            let along = (p[0] - a[0]) * direction[0] + (p[1] - a[1]) * direction[1];
            let squared = direction[0] * direction[0] + direction[1] * direction[1];
            return along / squared.max(0.000_001);
        }
        let offset = [p[0] - a[0], p[1] - a[1]];
        let squared = offset[0] * offset[0] + offset[1] * offset[1];
        if self.kind == KIND_RADIAL {
            return squared.sqrt() / (self.b[0] * res[0].min(res[1])).max(0.000_001);
        }
        // Conic: clockwise from straight up (-y), a whole turn from 0 to 1.
        if squared <= 0.0 {
            return 0.0;
        }
        let angle = libm::atan2f(offset[0], -offset[1]) - self.b[0];
        let turn = angle / std::f32::consts::TAU;
        turn - turn.floor()
    }

    /// `builtin_paint_spread`.
    #[inline]
    fn spread_position(&self, t: f32) -> f32 {
        if self.kind == KIND_CONIC {
            return t;
        }
        match self.spread {
            1 => t - t.floor(),
            2 => {
                let m = t - 2.0 * (t * 0.5).floor();
                if m > 1.0 { 2.0 - m } else { m }
            }
            _ => t.clamp(0.0, 1.0),
        }
    }
}

/// `builtin_stop_color`: straight sRGB to premultiplied linear light, 1
/// staying exactly 1.
pub(crate) fn stop_color(color: [f32; 4]) -> [f32; 4] {
    let alpha = color[3].clamp(0.0, 1.0);
    let linear = |value: f32| exact_srgb_to_linear(value.clamp(0.0, 1.0));
    [
        linear(color[0]) * alpha,
        linear(color[1]) * alpha,
        linear(color[2]) * alpha,
        alpha,
    ]
}

/// `builtin_generator_store`: premultiplied linear light to the
/// premultiplied, sRGB-encoded value the engine stores, 1 staying 1.
#[inline]
pub(crate) fn encode(color: [f32; 4]) -> [f32; 4] {
    let alpha = color[3];
    if alpha <= 0.0 {
        return [0.0; 4];
    }
    let encoded = |value: f32| linear_to_srgb((value / alpha).clamp(0.0, 1.0)) * alpha;
    [
        encoded(color[0]),
        encoded(color[1]),
        encoded(color[2]),
        alpha,
    ]
}
