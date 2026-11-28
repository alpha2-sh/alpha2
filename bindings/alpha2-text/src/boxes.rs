//! Text background boxes: padded line boxes with a radius and a solid or dashed border.

use super::{TextBackground, TextBoxDash, over, premultiplied};

/// One box in raster pixels, right and bottom exclusive, its radius capped at half its shorter
/// side.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct LineBox {
    pub(crate) left: f32,
    pub(crate) right: f32,
    pub(crate) top: f32,
    pub(crate) bottom: f32,
    radius: f32,
}

impl LineBox {
    /// The box over `[left, right]` by `[top, bottom]` grown by `background`'s padding.
    pub(crate) fn padded(
        background: &TextBackground,
        [left, right, top, bottom]: [f32; 4],
    ) -> Self {
        let [pad_top, pad_right, pad_bottom, pad_left] = background.padding;
        let (left, right) = (left - pad_left, right + pad_right);
        let (top, bottom) = (top - pad_top, bottom + pad_bottom);
        let half = ((right - left).min(bottom - top) * 0.5).max(0.0);
        Self {
            left,
            right,
            top,
            bottom,
            radius: background.radius.min(half),
        }
    }

    /// Coverage of pixel `[x, y]` (its top-left corner) by the box inset by `inset`.
    fn cover(&self, [x, y]: [f32; 2], inset: f32) -> f32 {
        let (left, right) = (self.left + inset, self.right - inset);
        let (top, bottom) = (self.top + inset, self.bottom - inset);
        if right <= left || bottom <= top {
            return 0.0;
        }
        let radius = (self.radius - inset).max(0.0);
        if radius <= 0.0 {
            let across = (right.min(x + 1.0) - left.max(x)).clamp(0.0, 1.0);
            let down = (bottom.min(y + 1.0) - top.max(y)).clamp(0.0, 1.0);
            return across * down;
        }
        let centre = [f32::midpoint(left, right), f32::midpoint(top, bottom)];
        let half = [(right - left) * 0.5, (bottom - top) * 0.5];
        let q = [
            (x + 0.5 - centre[0]).abs() - half[0] + radius,
            (y + 0.5 - centre[1]).abs() - half[1] + radius,
        ];
        let outside = libm::hypotf(q[0].max(0.0), q[1].max(0.0));
        let distance = outside + q[0].max(q[1]).min(0.0) - radius;
        (0.5 - distance).clamp(0.0, 1.0)
    }

    /// Coverage of pixel `[x, y]` by a border `width` wide inside the box, dashed by `dash`.
    fn border(&self, point: [f32; 2], width: f32, dash: Option<&TextBoxDash>) -> f32 {
        let band = (self.cover(point, 0.0) - self.cover(point, width)).max(0.0);
        match dash {
            Some(dash) if band > 0.0 => band * self.dash_factor(point, width, dash),
            _ => band,
        }
    }

    /// How much of the dash pattern is on at pixel `[x, y]`'s nearest point of the border's
    /// centre line, measured from the end of the top-left corner clockwise (SVG 2 `rect`).
    fn dash_factor(&self, [x, y]: [f32; 2], width: f32, dash: &TextBoxDash) -> f32 {
        let inset = width * 0.5;
        let (left, right) = (self.left + inset, self.right - inset);
        let (top, bottom) = (self.top + inset, self.bottom - inset);
        let radius = (self.radius - inset).max(0.0);
        let (inner_left, inner_right) = (left + radius, right - radius);
        let (inner_top, inner_bottom) = (top + radius, bottom - radius);
        let (across, down) = (
            (inner_right - inner_left).max(0.0),
            (inner_bottom - inner_top).max(0.0),
        );
        let arc = std::f32::consts::FRAC_PI_2 * radius;
        let point = [x + 0.5, y + 0.5];
        let nearest = [
            point[0].clamp(inner_left, inner_right),
            point[1].clamp(inner_top, inner_bottom),
        ];
        let offset = [point[0] - nearest[0], point[1] - nearest[1]];
        let corner = |from: f32, start: f32| {
            let angle = (libm::atan2f(offset[1], offset[0]) - from)
                .rem_euclid(std::f32::consts::TAU)
                .min(std::f32::consts::FRAC_PI_2);
            start + radius * angle
        };
        let top_edge = 0.0;
        let right_edge = across + arc;
        let bottom_edge = right_edge + down + arc;
        let left_edge = bottom_edge + across + arc;
        let position = if offset[0] == 0.0 && offset[1] == 0.0 {
            let distances = [
                point[1] - top,
                right - point[0],
                bottom - point[1],
                point[0] - left,
            ];
            let side = (0..4)
                .min_by(|a, b| distances[*a].total_cmp(&distances[*b]))
                .unwrap_or(0);
            match side {
                0 => top_edge + (point[0] - inner_left),
                1 => right_edge + (point[1] - inner_top),
                2 => bottom_edge + (inner_right - point[0]),
                _ => left_edge + (inner_bottom - point[1]),
            }
        } else if offset[0] == 0.0 {
            if offset[1] < 0.0 {
                top_edge + (point[0] - inner_left)
            } else {
                bottom_edge + (inner_right - point[0])
            }
        } else if offset[1] == 0.0 {
            if offset[0] > 0.0 {
                right_edge + (point[1] - inner_top)
            } else {
                left_edge + (inner_bottom - point[1])
            }
        } else {
            match (offset[0] > 0.0, offset[1] > 0.0) {
                (true, false) => corner(-std::f32::consts::FRAC_PI_2, across),
                (true, true) => corner(0.0, right_edge + down),
                (false, true) => corner(std::f32::consts::FRAC_PI_2, bottom_edge + across),
                (false, false) => corner(std::f32::consts::PI, left_edge + down),
            }
        };
        dash.on_at(position + dash.offset)
    }
}

impl TextBoxDash {
    /// How much of the pixel at `position` along the line lies on a dash, with a one-pixel ramp
    /// at each dash end.
    fn on_at(&self, position: f32) -> f32 {
        let repeats = if self.pattern.len() % 2 == 1 { 2 } else { 1 };
        let period = self.pattern.iter().sum::<f32>() * if repeats == 2 { 2.0 } else { 1.0 };
        if period <= 0.0 {
            return 1.0;
        }
        let at = position.rem_euclid(period);
        let mut start = 0.0_f32;
        let mut best = 0.0_f32;
        for (index, length) in self
            .pattern
            .iter()
            .cycle()
            .take(self.pattern.len() * repeats)
            .enumerate()
        {
            let end = start + length;
            if index % 2 == 0 {
                for shift in [-period, 0.0, period] {
                    let inside = (at - (start + shift)).min(end + shift - at);
                    best = best.max((inside + 0.5).clamp(0.0, 1.0));
                }
            }
            start = end;
        }
        best
    }
}

/// The premultiplied box color over pixel `[x, y]`: the fills by their largest coverage, then
/// each border, hidden where another box's fill covers it, over them.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "coverage in [0, 1] scales to a byte"
)]
pub(crate) fn paint(background: &TextBackground, boxes: &[LineBox], point: [f32; 2]) -> [u32; 4] {
    if background.is_plain() {
        let [x, y] = point;
        let covered = boxes
            .iter()
            .map(|line| {
                let across = (line.right.min(x + 1.0) - line.left.max(x)).clamp(0.0, 1.0);
                let down = (line.bottom.min(y + 1.0) - line.top.max(y)).clamp(0.0, 1.0);
                across * down
            })
            .fold(0.0_f32, f32::max);
        return premultiplied(background.color, (covered * 255.0).round() as u8);
    }
    let fills = boxes
        .iter()
        .map(|line| line.cover(point, 0.0))
        .collect::<Vec<_>>();
    let fill = fills.iter().copied().fold(0.0_f32, f32::max);
    let mut pixel = premultiplied(background.color, (fill * 255.0).round() as u8);
    if let Some(border) = &background.border {
        let mut line = 0.0_f32;
        for (index, shape) in boxes.iter().enumerate() {
            let others = fills
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, cover)| *cover)
                .fold(0.0_f32, f32::max);
            line =
                line.max(shape.border(point, border.width, border.dash.as_ref()) * (1.0 - others));
        }
        if line > 0.0 {
            pixel = over(
                premultiplied(border.color, (line * 255.0).round() as u8),
                pixel,
            );
        }
    }
    pixel
}
