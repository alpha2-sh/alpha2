//! SVG path data (`d`) read into the engine's cubic Bezier paths.

use alpha2_model::{Path, PathVertex, Subpath, Vec2};

/// Reads `d`. Every SVG path command is supported; arcs become cubic segments.
///
/// # Errors
/// Describes the first part of `d` that is not path data.
pub(crate) fn parse(d: &str) -> Result<Path, String> {
    let mut reader = Reader {
        bytes: d.as_bytes(),
        at: 0,
    };
    let mut builder = Builder::default();
    let mut command = None;
    loop {
        reader.skip_separators();
        if reader.at >= reader.bytes.len() {
            break;
        }
        let byte = reader.bytes[reader.at];
        if byte.is_ascii_alphabetic() {
            reader.at += 1;
            command = Some(byte);
            if matches!(byte, b'Z' | b'z') {
                builder.close();
                continue;
            }
        } else if command.is_none() {
            return Err(format!(
                "path data starts with a command, not {:?}",
                byte as char
            ));
        }
        let command = command.expect("a command was read");
        let relative = command.is_ascii_lowercase();
        let base = if relative {
            builder.current
        } else {
            Vec2::new(0.0, 0.0)
        };
        let point = |reader: &mut Reader| -> Result<Vec2, String> {
            let x = reader.number()?;
            let y = reader.number()?;
            Ok(Vec2::new(base.x + x, base.y + y))
        };
        match command.to_ascii_uppercase() {
            b'M' => {
                let to = point(&mut reader)?;
                builder.move_to(to);
                // Further pairs after a move are lines.
                return_lines(&mut reader, &mut builder, relative)?;
            }
            b'L' => {
                let to = point(&mut reader)?;
                builder.line_to(to);
            }
            b'H' => {
                let x = reader.number()?;
                let to = Vec2::new(
                    if relative { builder.current.x + x } else { x },
                    builder.current.y,
                );
                builder.line_to(to);
            }
            b'V' => {
                let y = reader.number()?;
                let to = Vec2::new(
                    builder.current.x,
                    if relative { builder.current.y + y } else { y },
                );
                builder.line_to(to);
            }
            b'C' => {
                let first = point(&mut reader)?;
                let second = point(&mut reader)?;
                let to = point(&mut reader)?;
                builder.cubic_to(first, second, to);
            }
            b'S' => {
                let first = builder.reflected_cubic();
                let second = point(&mut reader)?;
                let to = point(&mut reader)?;
                builder.cubic_to(first, second, to);
            }
            b'Q' => {
                let control = point(&mut reader)?;
                let to = point(&mut reader)?;
                builder.quad_to(control, to);
            }
            b'T' => {
                let control = builder.reflected_quad();
                let to = point(&mut reader)?;
                builder.quad_to(control, to);
            }
            b'A' => {
                let radii = Vec2::new(reader.number()?.abs(), reader.number()?.abs());
                let rotation = reader.number()?;
                let large = reader.flag()?;
                let sweep = reader.flag()?;
                let to = point(&mut reader)?;
                builder.arc_to(radii, rotation, large, sweep, to);
            }
            other => return Err(format!("unknown path command {:?}", other as char)),
        }
    }
    builder.finish()
}

/// Reads the line pairs that may follow a move's first pair.
fn return_lines(reader: &mut Reader, builder: &mut Builder, relative: bool) -> Result<(), String> {
    loop {
        reader.skip_separators();
        if !reader.starts_number() {
            return Ok(());
        }
        let base = if relative {
            builder.current
        } else {
            Vec2::new(0.0, 0.0)
        };
        let x = reader.number()?;
        let y = reader.number()?;
        builder.line_to(Vec2::new(base.x + x, base.y + y));
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip_separators(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b',')
        {
            self.at += 1;
        }
    }

    fn starts_number(&self) -> bool {
        self.bytes
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.'))
    }

    fn number(&mut self) -> Result<f32, String> {
        self.skip_separators();
        let start = self.at;
        let mut seen_dot = false;
        let mut seen_exponent = false;
        while let Some(&byte) = self.bytes.get(self.at) {
            let sign_here = matches!(byte, b'-' | b'+')
                && (self.at == start || matches!(self.bytes[self.at - 1], b'e' | b'E'));
            if byte.is_ascii_digit() || sign_here {
            } else if byte == b'.' && !seen_dot && !seen_exponent {
                seen_dot = true;
            } else if matches!(byte, b'e' | b'E') && !seen_exponent && self.at > start {
                seen_exponent = true;
            } else {
                break;
            }
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|text| text.parse::<f32>().ok())
            .filter(|value| value.is_finite())
            .ok_or_else(|| format!("expected a number at byte {start}"))
    }

    fn flag(&mut self) -> Result<bool, String> {
        self.skip_separators();
        match self.bytes.get(self.at) {
            Some(b'0') => {
                self.at += 1;
                Ok(false)
            }
            Some(b'1') => {
                self.at += 1;
                Ok(true)
            }
            _ => Err(format!("expected an arc flag, 0 or 1, at byte {}", self.at)),
        }
    }
}

#[derive(Default)]
struct Builder {
    subpaths: Vec<Subpath>,
    vertices: Vec<PathVertex>,
    current: Vec2,
    start: Vec2,
    /// The last segment's second control point, absolute, for `S`.
    last_cubic: Option<Vec2>,
    /// The last segment's control point, absolute, for `T`.
    last_quad: Option<Vec2>,
}

fn vertex(point: Vec2) -> PathVertex {
    PathVertex {
        point,
        in_tangent: Vec2::new(0.0, 0.0),
        out_tangent: Vec2::new(0.0, 0.0),
    }
}

fn minus(a: Vec2, b: Vec2) -> Vec2 {
    Vec2::new(a.x - b.x, a.y - b.y)
}

impl Builder {
    fn flush(&mut self, closed: bool) {
        if self.vertices.len() > 1 || (closed && !self.vertices.is_empty()) {
            self.subpaths.push(Subpath {
                closed,
                vertices: std::mem::take(&mut self.vertices),
            });
        }
        self.vertices.clear();
    }

    fn ensure_started(&mut self) {
        if self.vertices.is_empty() {
            self.vertices.push(vertex(self.current));
        }
    }

    fn move_to(&mut self, to: Vec2) {
        self.flush(false);
        self.current = to;
        self.start = to;
        self.vertices.push(vertex(to));
        self.last_cubic = None;
        self.last_quad = None;
    }

    fn line_to(&mut self, to: Vec2) {
        self.ensure_started();
        self.vertices.push(vertex(to));
        self.current = to;
        self.last_cubic = None;
        self.last_quad = None;
    }

    fn cubic_to(&mut self, first: Vec2, second: Vec2, to: Vec2) {
        self.ensure_started();
        let last = self.vertices.last_mut().expect("a started subpath");
        last.out_tangent = minus(first, last.point);
        let mut end = vertex(to);
        end.in_tangent = minus(second, to);
        self.vertices.push(end);
        self.current = to;
        self.last_cubic = Some(second);
        self.last_quad = None;
    }

    fn quad_to(&mut self, control: Vec2, to: Vec2) {
        let from = self.current;
        let first = Vec2::new(
            from.x + 2.0 / 3.0 * (control.x - from.x),
            from.y + 2.0 / 3.0 * (control.y - from.y),
        );
        let second = Vec2::new(
            to.x + 2.0 / 3.0 * (control.x - to.x),
            to.y + 2.0 / 3.0 * (control.y - to.y),
        );
        self.cubic_to(first, second, to);
        self.last_cubic = None;
        self.last_quad = Some(control);
    }

    fn reflected_cubic(&self) -> Vec2 {
        self.last_cubic.map_or(self.current, |control| {
            Vec2::new(
                2.0 * self.current.x - control.x,
                2.0 * self.current.y - control.y,
            )
        })
    }

    fn reflected_quad(&self) -> Vec2 {
        self.last_quad.map_or(self.current, |control| {
            Vec2::new(
                2.0 * self.current.x - control.x,
                2.0 * self.current.y - control.y,
            )
        })
    }

    /// An elliptical arc as cubic segments of at most a quarter turn (SVG 2, appendix B.2.4).
    #[allow(clippy::many_single_char_names, clippy::cast_possible_truncation)]
    fn arc_to(&mut self, radii: Vec2, rotation: f32, large: bool, sweep: bool, to: Vec2) {
        let from = self.current;
        let same = (from.x - to.x).abs() <= f32::EPSILON && (from.y - to.y).abs() <= f32::EPSILON;
        if radii.x <= 0.0 || radii.y <= 0.0 || same {
            self.line_to(to);
            return;
        }
        let (sin, cos) = libm::sincos(f64::from(rotation).to_radians());
        let (x1, y1) = (f64::from(from.x), f64::from(from.y));
        let (x2, y2) = (f64::from(to.x), f64::from(to.y));
        let (dx, dy) = ((x1 - x2) / 2.0, (y1 - y2) / 2.0);
        let (px, py) = (cos * dx + sin * dy, -sin * dx + cos * dy);
        let (mut rx, mut ry) = (f64::from(radii.x), f64::from(radii.y));
        let scale = (px * px) / (rx * rx) + (py * py) / (ry * ry);
        if scale > 1.0 {
            rx *= scale.sqrt();
            ry *= scale.sqrt();
        }
        let numerator = (rx * rx * ry * ry - rx * rx * py * py - ry * ry * px * px).max(0.0);
        let denominator = rx * rx * py * py + ry * ry * px * px;
        let mut factor = (numerator / denominator).sqrt();
        if large == sweep {
            factor = -factor;
        }
        let (cx_, cy_) = (factor * rx * py / ry, -factor * ry * px / rx);
        let (cx, cy) = (
            cos * cx_ - sin * cy_ + f64::midpoint(x1, x2),
            sin * cx_ + cos * cy_ + f64::midpoint(y1, y2),
        );
        let angle = |ux: f64, uy: f64, vx: f64, vy: f64| {
            let turn = if ux * vy - uy * vx < 0.0 { -1.0 } else { 1.0 };
            let dot =
                (ux * vx + uy * vy) / ((ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt());
            turn * libm::acos(dot.clamp(-1.0, 1.0))
        };
        let theta = angle(1.0, 0.0, (px - cx_) / rx, (py - cy_) / ry);
        let mut delta = angle(
            (px - cx_) / rx,
            (py - cy_) / ry,
            (-px - cx_) / rx,
            (-py - cy_) / ry,
        );
        if !sweep && delta > 0.0 {
            delta -= std::f64::consts::TAU;
        } else if sweep && delta < 0.0 {
            delta += std::f64::consts::TAU;
        }
        let pieces = (delta.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0);
        let step = delta / pieces;
        let kappa = 4.0 / 3.0 * libm::tan(step / 4.0);
        let point = |t: f64| {
            let (s, c) = libm::sincos(t);
            (
                cx + rx * c * cos - ry * s * sin,
                cy + rx * c * sin + ry * s * cos,
            )
        };
        let derivative = |t: f64| {
            let (s, c) = libm::sincos(t);
            (-rx * s * cos - ry * c * sin, -rx * s * sin + ry * c * cos)
        };
        #[allow(clippy::cast_sign_loss)]
        for piece in 0..pieces as u32 {
            let a = theta + step * f64::from(piece);
            let b = a + step;
            let (ax, ay) = point(a);
            let (bx, by) = point(b);
            let (dax, day) = derivative(a);
            let (dbx, dby) = derivative(b);
            let first = Vec2::new((ax + kappa * dax) as f32, (ay + kappa * day) as f32);
            let second = Vec2::new((bx - kappa * dbx) as f32, (by - kappa * dby) as f32);
            let end = if piece + 1 == pieces as u32 {
                to
            } else {
                Vec2::new(bx as f32, by as f32)
            };
            self.cubic_to(first, second, end);
        }
        self.last_cubic = None;
    }

    fn close(&mut self) {
        if self.vertices.len() > 1 {
            let last = self.vertices.last().expect("a vertex");
            let first = self.vertices[0].point;
            if (last.point.x - first.x).abs() < 1e-6 && (last.point.y - first.y).abs() < 1e-6 {
                // The closing segment is the one that ends here; the first vertex takes it.
                let last = self.vertices.pop().expect("a vertex");
                self.vertices[0].in_tangent = last.in_tangent;
            }
        }
        self.flush(true);
        self.current = self.start;
        self.last_cubic = None;
        self.last_quad = None;
    }

    fn finish(mut self) -> Result<Path, String> {
        self.flush(false);
        if self.subpaths.is_empty() {
            return Err("path data draws nothing".to_owned());
        }
        Ok(Path {
            subpaths: self.subpaths,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_curves_and_closes_read_as_vertices_with_tangents() {
        let path = parse("M0 0 L1 0 l0 1 H0 Z").expect("parses");
        assert_eq!(path.subpaths.len(), 1);
        assert!(path.subpaths[0].closed);
        assert_eq!(path.subpaths[0].vertices.len(), 4);
        let curve = parse("M0,0 C0.25,0 0.75,1 1,1 S1,2 2,2").expect("parses");
        let vertices = &curve.subpaths[0].vertices;
        assert_eq!(vertices.len(), 3);
        assert!((vertices[0].out_tangent.x - 0.25).abs() < 1e-6);
        assert!((vertices[1].in_tangent.x + 0.25).abs() < 1e-6);
        // The smooth curve's first control mirrors the last one about the shared point.
        assert!((vertices[1].out_tangent.x - 0.25).abs() < 1e-6);
    }

    #[test]
    fn an_arc_becomes_quarter_turn_cubics_ending_where_it_says() {
        let path = parse("M0 0.5 A0.5 0.5 0 1 1 1 0.5 A0.5 0.5 0 1 1 0 0.5 Z").expect("parses");
        let vertices = &path.subpaths[0].vertices;
        assert!(vertices.len() >= 4, "{}", vertices.len());
        assert!(path.subpaths[0].closed);
    }

    #[test]
    fn what_is_not_path_data_is_refused() {
        assert!(parse("0 0 L 1 1").is_err());
        assert!(parse("M0 0 L1").is_err());
        assert!(parse("M0 0 X1 1").is_err());
        assert!(parse("").is_err());
    }
}
