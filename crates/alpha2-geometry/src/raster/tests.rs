use super::dense::{coverage_bytes, rasterize_dense};
use super::{RasterScratch, TILE, TileState, rasterize, tile_aligned, winding_at};
use crate::parity::{SCENES, SIZES, clip_of, fill_of, scene};
use crate::{IRect, project_far};
use alpha2_model::FillRule;
use kurbo::{BezPath, Point};

fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> BezPath {
    let mut path = BezPath::new();
    path.move_to((x0, y0));
    path.line_to((x1, y0));
    path.line_to((x1, y1));
    path.line_to((x0, y1));
    path.close_path();
    path
}

fn total(path: &BezPath, rule: FillRule, clip: IRect) -> f64 {
    let (bytes, _, _) = coverage_bytes(path, rule, clip);
    bytes.iter().map(|byte| f64::from(*byte) / 255.0).sum()
}

fn canvas(w: i32, h: i32) -> IRect {
    IRect::new(0, 0, w, h)
}

#[test]
fn sparse_accumulation_gives_the_dense_bytes_and_tiles_for_every_scene() {
    let mut scratch = RasterScratch::default();
    for (id, name) in SCENES.iter().enumerate() {
        for (w, h) in SIZES {
            for draw in scene(id, w, h) {
                let (path, rule) = fill_of(&draw);
                let clip = clip_of(&path, w, h);
                let sparse = rasterize(&path, rule, clip, &mut scratch);
                let dense = rasterize_dense(&path, rule, clip);
                assert_eq!(sparse, dense, "{name} at {w}x{h}");
            }
        }
    }
}

#[test]
fn a_rectangle_covers_its_area() {
    for (x0, y0, x1, y1) in [
        (10.0, 10.0, 50.0, 30.0),
        (10.25, 3.5, 47.75, 29.125),
        (0.5, 0.5, 1.5, 1.5),
        (33.3, 17.7, 33.9, 60.1),
    ] {
        let path = rect_path(x0, y0, x1, y1);
        let area = (x1 - x0) * (y1 - y0);
        for rule in [FillRule::Nonzero, FillRule::Evenodd] {
            let covered = total(&path, rule, canvas(64, 64));
            // Each byte rounds by at most half a level.
            let perimeter_pixels = 2.0 * ((x1 - x0) + (y1 - y0)) + 4.0;
            assert!(
                (covered - area).abs() <= perimeter_pixels * 0.5 / 255.0 + 1e-9,
                "{x0},{y0},{x1},{y1}: covered {covered}, area {area}"
            );
        }
    }
}

#[test]
fn a_half_pixel_edge_covers_half_the_pixel() {
    let path = rect_path(2.5, 0.0, 8.0, 16.0);
    let (bytes, w, _) = coverage_bytes(&path, FillRule::Nonzero, canvas(16, 16));
    for y in 0..16 {
        assert_eq!(bytes[y * w + 1], 0);
        assert_eq!(bytes[y * w + 2], 128, "row {y}");
        assert_eq!(bytes[y * w + 3], 255);
        assert_eq!(bytes[y * w + 8], 0);
    }
}

#[test]
fn horizontal_and_zero_length_lines_add_nothing() {
    let mut path = rect_path(4.0, 4.0, 12.0, 12.0);
    // A zero-length subpath, and one that only runs back and forth horizontally.
    path.move_to((20.0, 20.0));
    path.line_to((20.0, 20.0));
    path.close_path();
    path.move_to((1.0, 30.0));
    path.line_to((40.0, 30.0));
    path.close_path();
    let alone = coverage_bytes(
        &rect_path(4.0, 4.0, 12.0, 12.0),
        FillRule::Nonzero,
        canvas(48, 48),
    );
    assert_eq!(
        coverage_bytes(&path, FillRule::Nonzero, canvas(48, 48)),
        alone
    );
}

#[test]
fn vertical_edges_on_pixel_boundaries_are_exact() {
    let (bytes, w, h) = coverage_bytes(
        &rect_path(3.0, 0.0, 9.0, 16.0),
        FillRule::Nonzero,
        canvas(16, 16),
    );
    for y in 0..h {
        for x in 0..w {
            let expected = if (3..9).contains(&x) { 255 } else { 0 };
            assert_eq!(bytes[y * w + x], expected, "({x}, {y})");
        }
    }
}

#[test]
fn overlapping_subpaths_follow_the_fill_rule() {
    let mut same = rect_path(0.0, 0.0, 10.0, 16.0);
    same.extend(rect_path(5.0, 0.0, 15.0, 16.0).elements().iter().copied());
    let clip = canvas(16, 16);
    let (nonzero, w, _) = coverage_bytes(&same, FillRule::Nonzero, clip);
    let (evenodd, _, _) = coverage_bytes(&same, FillRule::Evenodd, clip);
    // The overlap is wound twice: inside by nonzero, a hole by even-odd.
    assert_eq!(nonzero[w * 8 + 7], 255);
    assert_eq!(evenodd[w * 8 + 7], 0);
    assert_eq!(evenodd[w * 8 + 2], 255);
    assert_eq!(evenodd[w * 8 + 12], 255);
    // Wound the other way, the overlap cancels under both rules.
    let mut opposite = rect_path(0.0, 0.0, 10.0, 16.0);
    let mut reversed = BezPath::new();
    reversed.move_to((5.0, 0.0));
    reversed.line_to((5.0, 16.0));
    reversed.line_to((15.0, 16.0));
    reversed.line_to((15.0, 0.0));
    reversed.close_path();
    opposite.extend(reversed.elements().iter().copied());
    let (cancelled, _, _) = coverage_bytes(&opposite, FillRule::Nonzero, clip);
    assert_eq!(cancelled[w * 8 + 7], 0);
}

#[test]
fn a_self_intersecting_figure_eight_paints_both_lobes_under_either_rule() {
    let mut eight = BezPath::new();
    eight.move_to((2.0, 2.0));
    eight.line_to((30.0, 30.0));
    eight.line_to((30.0, 2.0));
    eight.line_to((2.0, 30.0));
    eight.close_path();
    for rule in [FillRule::Nonzero, FillRule::Evenodd] {
        let (bytes, w, _) = coverage_bytes(&eight, rule, canvas(32, 32));
        // Left and right lobes are each wound once, the top and bottom not at all.
        assert_eq!(bytes[16 * w + 6], 255, "{rule:?}");
        assert_eq!(bytes[16 * w + 26], 255, "{rule:?}");
        assert_eq!(bytes[5 * w + 16], 0, "{rule:?}");
        assert_eq!(bytes[27 * w + 16], 0, "{rule:?}");
    }
}

#[test]
fn tiles_are_classified_at_their_boundaries() {
    let mut scratch = RasterScratch::default();
    // Exactly the second tile column: full there, empty either side, no partial tile.
    let path = rect_path(16.0, 0.0, 32.0, 32.0);
    let coverage = rasterize(
        &path,
        FillRule::Nonzero,
        IRect::new(0, 0, 48, 32),
        &mut scratch,
    );
    assert_eq!(coverage.size, (3, 2));
    assert_eq!(
        coverage.tiles,
        [
            TileState::Empty,
            TileState::Full,
            TileState::Empty,
            TileState::Empty,
            TileState::Full,
            TileState::Empty
        ]
    );
    assert_eq!(coverage.partial.len(), 0);
    // A pixel past the edge makes the neighbours partial.
    let path = rect_path(15.0, 0.0, 33.0, 32.0);
    let coverage = rasterize(
        &path,
        FillRule::Nonzero,
        IRect::new(0, 0, 48, 32),
        &mut scratch,
    );
    assert_eq!(coverage.counts(), (0, 2, 4));
    assert_eq!(coverage.at(15, 3), 255);
    assert_eq!(coverage.at(14, 3), 0);
    assert_eq!(coverage.at(33, 3), 0);
}

#[test]
fn a_clip_is_grown_to_whole_tiles_and_coverage_lies_where_the_path_is() {
    assert_eq!(
        tile_aligned(IRect::new(5, 17, 40, 33)),
        IRect::new(0, 16, 48, 48)
    );
    let mut scratch = RasterScratch::default();
    let path = rect_path(20.0, 20.0, 40.0, 40.0);
    let coverage = rasterize(
        &path,
        FillRule::Nonzero,
        IRect::new(19, 19, 41, 41),
        &mut scratch,
    );
    assert_eq!(coverage.origin, (1, 1));
    assert_eq!(coverage.rect(), IRect::new(16, 16, 48, 48));
    for (x, y, expected) in [
        (19, 30, 0),
        (20, 30, 255),
        (39, 39, 255),
        (40, 30, 0),
        (30, 41, 0),
    ] {
        assert_eq!(coverage.at(x, y), expected, "({x}, {y})");
    }
}

#[test]
fn a_path_far_past_the_canvas_fills_it_as_it_would_nearby() {
    // A triangle a million pixels across whose edge crosses the canvas: projection keeps what
    // the canvas shows. Its edge passes through (0, 32) and (64, 0).
    let mut far = BezPath::new();
    far.move_to((-1.0e6, 32.0 + 1.0e6 / 2.0));
    far.line_to((64.0 + 1.0e6, -1.0e6 / 2.0));
    far.line_to((-1.0e6, -1.0e6));
    far.close_path();
    let projected = project_far(&far, 64, 64);
    let clip = canvas(64, 64);
    let (bytes, w, _) = coverage_bytes(&projected, FillRule::Nonzero, clip);
    let mut near = BezPath::new();
    near.move_to((0.0, 32.0));
    near.line_to((64.0, 0.0));
    near.line_to((0.0, 0.0));
    near.close_path();
    let (expected, _, _) = coverage_bytes(&near, FillRule::Nonzero, clip);
    let differing = bytes
        .iter()
        .zip(&expected)
        .filter(|(a, b)| a.abs_diff(**b) > 1)
        .count();
    assert_eq!(differing, 0, "{w}");
}

/// Coverage from 64 x 64 winding samples per pixel: the reference the
/// area accumulation approximates.
fn sampled(path: &BezPath, rule: FillRule, x: usize, y: usize) -> f64 {
    let mut inside = 0;
    for sy in 0..64 {
        for sx in 0..64 {
            #[allow(clippy::cast_precision_loss)]
            let point = Point::new(
                x as f64 + (f64::from(sx) + 0.5) / 64.0,
                y as f64 + (f64::from(sy) + 0.5) / 64.0,
            );
            let winding = winding_at(path, point);
            let covered = match rule {
                FillRule::Nonzero => winding != 0,
                FillRule::Evenodd => winding % 2 != 0,
            };
            inside += i32::from(covered);
        }
    }
    f64::from(inside) / 4096.0
}

#[test]
fn coverage_is_close_to_sampling_each_pixel_finely() {
    let mut circle = BezPath::new();
    let k = crate::shapes::KAPPA * 10.0;
    circle.move_to((26.0, 16.0));
    circle.curve_to((26.0, 16.0 + k), (16.0 + k, 26.0), (16.0, 26.0));
    circle.curve_to((16.0 - k, 26.0), (6.0, 16.0 + k), (6.0, 16.0));
    circle.curve_to((6.0, 16.0 - k), (16.0 - k, 6.0), (16.0, 6.0));
    circle.curve_to((16.0 + k, 6.0), (26.0, 16.0 - k), (26.0, 16.0));
    circle.close_path();
    let clip = canvas(32, 32);
    let (bytes, w, _) = coverage_bytes(&circle, FillRule::Nonzero, clip);
    let mut sum = 0.0;
    let mut worst: f64 = 0.0;
    for y in 4..28 {
        for x in 4..28 {
            let error = (f64::from(bytes[y * w + x]) / 255.0
                - sampled(&circle, FillRule::Nonzero, x, y))
            .abs();
            sum += error;
            worst = worst.max(error);
        }
    }
    let mean = sum / (24.0 * 24.0);
    assert!(mean * 255.0 < 1.0, "mean error {} levels", mean * 255.0);
    assert!(worst * 255.0 < 12.0, "worst error {} levels", worst * 255.0);
}

#[test]
fn every_tile_lies_within_the_aligned_clip() {
    let mut scratch = RasterScratch::default();
    let path = rect_path(3.0, 5.0, 70.0, 37.0);
    let coverage = rasterize(
        &path,
        FillRule::Nonzero,
        IRect::new(2, 4, 71, 38),
        &mut scratch,
    );
    let tile = i32::try_from(TILE).expect("small");
    assert_eq!(coverage.origin, (0, 0));
    assert_eq!(coverage.size, (5, 3));
    assert_eq!(coverage.rect(), IRect::new(0, 0, 5 * tile, 3 * tile));
}
