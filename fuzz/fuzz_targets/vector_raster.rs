#![no_main]

use alpha2_geometry::kurbo::{BezPath, Point};
use alpha2_geometry::raster::{RasterScratch, TileState, dense, rasterize};
use alpha2_geometry::{IRect, project_far};
use alpha2_model::FillRule;
use libfuzzer_sys::fuzz_target;

/// Reads `f64`s from fuzz bytes: each a scaled 16-bit value, so coordinates land on and around
/// a small canvas and, scaled by a picked power of ten, far past it.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&first, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(first)
    }

    fn coordinate(&mut self, scale: f64) -> Option<f64> {
        let value = u16::from_le_bytes([self.byte()?, self.byte()?]);
        Some((f64::from(value) / 65_535.0 * 160.0 - 48.0) * scale)
    }

    fn point(&mut self, scale: f64) -> Option<Point> {
        Some(Point::new(self.coordinate(scale)?, self.coordinate(scale)?))
    }
}

/// A path from fuzz bytes: moves, lines, quadratics, cubics, and closes.
fn path(reader: &mut Reader<'_>, scale: f64) -> BezPath {
    let mut path = BezPath::new();
    let mut started = false;
    while let Some(kind) = reader.byte() {
        let Some(point) = reader.point(scale) else {
            break;
        };
        match kind % 5 {
            0 => {
                path.move_to(point);
                started = true;
                continue;
            }
            _ if !started => {
                path.move_to(point);
                started = true;
                continue;
            }
            1 => path.line_to(point),
            2 => {
                let Some(end) = reader.point(scale) else {
                    break;
                };
                path.quad_to(point, end);
            }
            3 => {
                let (Some(second), Some(end)) = (reader.point(scale), reader.point(scale)) else {
                    break;
                };
                path.curve_to(point, second, end);
            }
            _ => path.close_path(),
        }
    }
    path
}

// Any path rasterizes without a panic, to coverage in 0..=255 in whole tiles; the sparse
// rasterizer gives the dense reference's bytes; and an axis-aligned rectangle covers its area.
fuzz_target!(|data: &[u8]| {
    let mut reader = Reader(data);
    let Some(flags) = reader.byte() else { return };
    let rule = if flags & 1 == 1 {
        FillRule::Evenodd
    } else {
        FillRule::Nonzero
    };
    let scale = [1.0, 10.0, 1.0e3, 1.0e6][usize::from((flags >> 1) & 3)];
    let clip = IRect::new(0, 0, 64, 48);
    let path = project_far(&path(&mut reader, scale), 64, 48);
    let mut scratch = RasterScratch::default();
    let coverage = rasterize(&path, rule, clip, &mut scratch);
    assert_eq!(
        coverage.tiles.len(),
        (coverage.size.0 * coverage.size.1) as usize
    );
    for tile in &coverage.tiles {
        if let TileState::Partial(index) = tile {
            assert!((*index as usize) < coverage.partial.len());
        }
    }
    assert_eq!(coverage, dense::rasterize_dense(&path, rule, clip));

    // A rectangle from the first two points covers its area within rounding.
    let mut corners = Reader(data);
    corners.byte();
    if let (Some(a), Some(b)) = (corners.point(1.0), corners.point(1.0)) {
        let (x0, x1) = (a.x.min(b.x).clamp(0.0, 64.0), a.x.max(b.x).clamp(0.0, 64.0));
        let (y0, y1) = (a.y.min(b.y).clamp(0.0, 48.0), a.y.max(b.y).clamp(0.0, 48.0));
        let mut rectangle = BezPath::new();
        rectangle.move_to((x0, y0));
        rectangle.line_to((x1, y0));
        rectangle.line_to((x1, y1));
        rectangle.line_to((x0, y1));
        rectangle.close_path();
        let (bytes, _, _) = dense::coverage_bytes(&rectangle, FillRule::Nonzero, clip);
        let covered: f64 = bytes.iter().map(|byte| f64::from(*byte) / 255.0).sum();
        let area = (x1 - x0) * (y1 - y0);
        let edge_pixels = 2.0 * ((x1 - x0) + (y1 - y0)) + 4.0;
        assert!(
            (covered - area).abs() <= edge_pixels * 0.5 / 255.0 + 1e-6,
            "rectangle {x0},{y0},{x1},{y1}: covered {covered}, area {area}"
        );
    }
});
