//! Gradient fills of glyphs, matching shape paints (`alpha2-render`'s `paint_wgsl.rs`): sRGB
//! stops interpolated premultiplied in linear light. Uses `f32` with `libm` so native and wasm
//! give the same bytes. COLR font gradients follow their own rules in `color.rs`.

use std::f32::consts::TAU;

/// A gradient over a text's layout box. Points are fractions of the box (`(0, 0)` top-left) and
/// a radius is a fraction of its shorter side.
#[derive(Clone, Debug, PartialEq)]
pub struct TextGradient {
    /// Geometry.
    pub kind: TextGradientKind,
    /// Two or more stops with non-decreasing offsets in `0..=1`.
    pub stops: Vec<TextGradientStop>,
    /// Behavior past the end stops; ignored by conic gradients.
    pub spread: TextSpread,
}

/// Where a gradient's offsets lie.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TextGradientKind {
    /// Offset 0 at `start`, 1 at `end`, constant across the line.
    Linear {
        /// Where offset 0 lies.
        start: [f32; 2],
        /// Where offset 1 lies; distinct from `start`.
        end: [f32; 2],
    },
    /// Offset 1 at `radius` from `center`.
    Radial {
        /// Where offset 0 lies.
        center: [f32; 2],
        /// Positive, a fraction of the box's shorter side.
        radius: f32,
    },
    /// One clockwise turn about `center`, starting `angle` radians clockwise from straight up.
    Conic {
        /// Center of rotation.
        center: [f32; 2],
        /// Where offset 0 lies.
        angle: f32,
    },
}

/// One color of a [`TextGradient`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextGradientStop {
    /// Position along the gradient, in `0..=1`.
    pub offset: f32,
    /// Straight, sRGB-encoded RGBA in `0..=1`.
    pub color: [f32; 4],
}

/// What a linear or radial gradient shows past its first and last stops.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TextSpread {
    /// The end colors continue.
    #[default]
    Pad,
    /// The gradient starts again.
    Repeat,
    /// The gradient runs back and forth.
    Reflect,
}

/// Same limit as shape paints.
pub(crate) const MAX_STOPS: usize = 16;

impl TextGradient {
    // Coincident linear end points are compared exactly, as the model does.
    #[allow(clippy::float_cmp)]
    pub(crate) fn is_valid(&self) -> bool {
        let finite = |point: [f32; 2]| point.iter().all(|value| value.is_finite());
        let geometry = match self.kind {
            TextGradientKind::Linear { start, end } => finite(start) && finite(end) && start != end,
            TextGradientKind::Radial { center, radius } => {
                finite(center) && radius.is_finite() && radius > 0.0
            }
            TextGradientKind::Conic { center, angle } => finite(center) && angle.is_finite(),
        };
        geometry
            && (2..=MAX_STOPS).contains(&self.stops.len())
            && self.stops.iter().all(|stop| {
                (0.0..=1.0).contains(&stop.offset)
                    && stop.color.iter().all(|value| (0.0..=1.0).contains(value))
            })
            && self
                .stops
                .windows(2)
                .all(|pair| pair[0].offset <= pair[1].offset)
    }

    /// Converts to pixel geometry and premultiplied linear stops, once per raster.
    pub(crate) fn prepared(&self, size: [f32; 2]) -> PreparedGradient<'_> {
        let at = |point: [f32; 2]| [point[0] * size[0], point[1] * size[1]];
        let kind = match self.kind {
            TextGradientKind::Linear { start, end } => TextGradientKind::Linear {
                start: at(start),
                end: at(end),
            },
            TextGradientKind::Radial { center, radius } => TextGradientKind::Radial {
                center: at(center),
                radius: radius * size[0].min(size[1]),
            },
            TextGradientKind::Conic { center, angle } => TextGradientKind::Conic {
                center: at(center),
                angle,
            },
        };
        PreparedGradient {
            gradient: self,
            kind,
            colors: self
                .stops
                .iter()
                .map(|stop| premultiplied_linear(stop.color))
                .collect(),
        }
    }
}

/// A gradient with its stop colors converted.
pub(crate) struct PreparedGradient<'a> {
    gradient: &'a TextGradient,
    /// Geometry in pixels.
    kind: TextGradientKind,
    colors: Vec<[f32; 4]>,
}

impl PreparedGradient<'_> {
    /// Straight sRGB bytes at `point`, matching the stored shape paint value.
    pub(crate) fn rgba8_at(&self, point: [f32; 2]) -> [u8; 4] {
        stored_rgba8(self.linear_at(point))
    }

    pub(crate) fn linear_at(&self, point: [f32; 2]) -> [f32; 4] {
        let gradient = self.gradient;
        let t = spread(self.kind, gradient.spread, position(self.kind, point));
        let stops = &gradient.stops;
        if t <= stops[0].offset {
            return self.colors[0];
        }
        for index in 1..stops.len() {
            if t <= stops[index].offset {
                let span = stops[index].offset - stops[index - 1].offset;
                let weight = if span <= 0.0 {
                    1.0
                } else {
                    (t - stops[index - 1].offset) / span
                };
                let (from, to) = (self.colors[index - 1], self.colors[index]);
                // WGSL's `mix`: from * (1 - w) + to * w.
                return std::array::from_fn(|channel| {
                    from[channel] * (1.0 - weight) + to[channel] * weight
                });
            }
        }
        self.colors[stops.len() - 1]
    }
}

fn position(kind: TextGradientKind, point: [f32; 2]) -> f32 {
    match kind {
        TextGradientKind::Linear { start, end } => {
            let direction = [end[0] - start[0], end[1] - start[1]];
            let from = [point[0] - start[0], point[1] - start[1]];
            dot(from, direction) / dot(direction, direction).max(0.000_001)
        }
        TextGradientKind::Radial { center, radius } => {
            let from = [point[0] - center[0], point[1] - center[1]];
            dot(from, from).sqrt() / radius.max(0.000_001)
        }
        TextGradientKind::Conic { center, angle } => {
            let from = [point[0] - center[0], point[1] - center[1]];
            if dot(from, from) <= 0.0 {
                return 0.0;
            }
            // Clockwise from straight up (-y), a whole turn from 0 to 1.
            let turn = (libm::atan2f(from[0], -from[1]) - angle) / TAU;
            turn - turn.floor()
        }
    }
}

fn spread(kind: TextGradientKind, spread: TextSpread, t: f32) -> f32 {
    if matches!(kind, TextGradientKind::Conic { .. }) {
        return t;
    }
    match spread {
        TextSpread::Pad => t.clamp(0.0, 1.0),
        TextSpread::Repeat => t - t.floor(),
        TextSpread::Reflect => {
            let turn = t - 2.0 * (t * 0.5).floor();
            if turn > 1.0 { 2.0 - turn } else { turn }
        }
    }
}

fn dot(left: [f32; 2], right: [f32; 2]) -> f32 {
    left[0] * right[0] + left[1] * right[1]
}

/// Straight sRGB to premultiplied linear light; 1 stays exactly 1.
pub(crate) fn premultiplied_linear(color: [f32; 4]) -> [f32; 4] {
    let alpha = color[3].clamp(0.0, 1.0);
    let linear = |value: f32| {
        let value = value.clamp(0.0, 1.0);
        if value >= 1.0 {
            1.0
        } else if value <= 0.040_45 {
            value / 12.92
        } else {
            libm::powf((value + 0.055) / 1.055, 2.4)
        }
    };
    [
        linear(color[0]) * alpha,
        linear(color[1]) * alpha,
        linear(color[2]) * alpha,
        alpha,
    ]
}

/// Premultiplied linear light to straight sRGB bytes; 1 stays exactly 1.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn stored_rgba8(color: [f32; 4]) -> [u8; 4] {
    let alpha = color[3].clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return [0; 4];
    }
    let encode = |value: f32| {
        let value = (value / alpha).clamp(0.0, 1.0);
        if value >= 1.0 {
            1.0
        } else if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * libm::powf(value, 1.0 / 2.4) - 0.055
        }
    };
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        byte(encode(color[0])),
        byte(encode(color[1])),
        byte(encode(color[2])),
        byte(alpha),
    ]
}
